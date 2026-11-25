use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CatalogDocument, CatalogError, CatalogStore, MembershipActor, MembershipAuditEvent,
    RepositoryApplicationState, RepositoryBaseline, audit,
};
use crate::app::MAX_NUMBER;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Visibility {
    #[default]
    Private,
    Public,
}

impl Visibility {
    pub(crate) fn is_public(self) -> bool {
        self == Self::Public
    }

    pub(super) fn is_private(&self) -> bool {
        *self == Self::Private
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Setting {
    pub(crate) version: u64,
    pub(crate) value: Visibility,
}

impl Setting {
    pub(super) fn is_initial(&self) -> bool {
        *self == Self::default()
    }

    pub(super) fn valid_saved(&self) -> bool {
        self.is_initial() || (self.version > 0 && self.version < MAX_NUMBER)
    }
}

impl CatalogStore {
    pub(crate) async fn change_visibility(
        &self,
        revision: u64,
        repository: Uuid,
        expected: Setting,
        actor: MembershipActor,
        occurred_at: u64,
    ) -> Result<CatalogDocument, CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if document.version != revision {
            return Err(CatalogError::Conflict);
        }
        let root = document
            .repository_by_id(repository)
            .ok_or(CatalogError::NotFound)?;
        if root.forked_from.is_some() {
            return Err(CatalogError::Invalid(
                "Change visibility on the upstream repository",
            ));
        }
        if root.visibility.value == expected.value {
            return Ok(document);
        }
        if root.visibility.version != expected.version || expected.version >= MAX_NUMBER - 1 {
            return Err(CatalogError::Conflict);
        }
        let root = root.clone();
        let now_ms = super::catalog_time_ms()?;
        if expected.value.is_public() {
            // Forks must stay private with their existing grants. In-flight jobs
            // still depend on lineage and must settle before an atomic detach.
            for fork in document
                .repositories
                .iter_mut()
                .filter(|fork| fork.forked_from == Some(repository))
            {
                if fork.application != RepositoryApplicationState::CellReady {
                    return Err(CatalogError::Invalid(
                        "Wait for pending forks before making this repository public",
                    ));
                }
                fork.fork_baseline = fork.baseline();
                fork.forked_from = None;
                fork.record_metadata_update(now_ms);
                if let Some(owner) = fork.fork_owner.take() {
                    fork.members = vec![crate::RepositoryMember {
                        subject: owner.subject,
                        name: "Fork owner".into(),
                        access: crate::RepositoryAccess::Admin,
                    }];
                    fork.groups.clear();
                    fork.organization = None;
                    fork.fork_baseline = RepositoryBaseline::Organization;
                } else {
                    fork.members = root.members.clone();
                    fork.groups = root.groups.clone();
                    fork.organization = root.organization.clone();
                }
            }
        }
        let record = document
            .repositories
            .iter_mut()
            .find(|record| record.id == repository)
            .ok_or(CatalogError::NotFound)?;
        record.visibility = Setting {
            version: expected.version + 1,
            value: expected.value,
        };
        record.record_metadata_update(now_ms);
        document.version = document
            .version
            .checked_add(1)
            .ok_or(CatalogError::Conflict)?;
        let digest = root.membership_digest()?;
        document.queue_membership_audit(MembershipAuditEvent {
            id: Uuid::now_v7(),
            outside_grants: None,
            repository_id: Some(repository),
            actor,
            previous_members_digest: digest.clone(),
            new_members_digest: digest,
            repository_version: document.version,
            occurred_at,
            previous: document.membership_audit_head.clone(),
            organizations: audit::Scope::for_record(&root, audit::Action::VisibilityChanged)
                .into_iter()
                .collect(),
        });
        document.validate(&self.root)?;
        if !self.write_document(&document, etag).await? {
            return Err(CatalogError::Conflict);
        }
        let _ = self.flush_membership_audit().await;
        Ok(document)
    }
}
