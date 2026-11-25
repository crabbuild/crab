use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    CatalogDocument, CatalogError, CatalogRecord, CatalogStore, MembershipActor, StorageError,
    audit::{AccessAudit, OutsideRemovalAudit, Position},
};

pub(crate) const MAX_COLLABORATORS: usize = 50;

impl CatalogRecord {
    pub(crate) fn owns_direct_grants(&self, organization: Uuid) -> bool {
        self.organization_id() == Some(organization) && self.forked_from.is_none()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "OutsideRemovalImpact"))]
pub(crate) struct Impact {
    pub collaborators: usize,
    pub repositories: usize,
    pub grants: usize,
}

pub(crate) struct Confirmed {
    pub position: Position,
    pub organization: Uuid,
    pub actor: MembershipActor,
    pub confirmation: String,
    pub subjects: BTreeSet<String>,
}

impl CatalogStore {
    pub(crate) async fn outside_removal_receipt(
        &self,
        snapshot: &CatalogDocument,
        organization: Uuid,
        position: Position,
        confirmation: &str,
    ) -> Result<Option<Impact>, CatalogError> {
        if position.revision > snapshot.version {
            return Ok(None);
        }
        match self.audit_record(snapshot, position).await {
            Ok(AccessAudit::OutsideRemoval(event))
                if event.organization == organization && event.confirmation == confirmation =>
            {
                Ok(Some(Impact {
                    collaborators: event.collaborators,
                    repositories: event.repositories.len(),
                    grants: event.grants,
                }))
            }
            Ok(_) => Err(CatalogError::Conflict),
            Err(CatalogError::Storage(StorageError::NotFound { .. })) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn remove_outside_grants(
        &self,
        input: Confirmed,
        cancel: &CancellationToken,
    ) -> Result<Impact, CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if document.version.checked_add(1) != Some(input.position.revision) {
            return Err(CatalogError::Conflict);
        }
        let mut before = BTreeMap::new();
        let mut grants = 0;
        for repo in document
            .repositories
            .iter_mut()
            .chain(
                document
                    .deleted_repositories
                    .iter_mut()
                    .map(|entry| &mut entry.repository),
            )
            .filter(|repo| repo.holds_organization_grants(input.organization))
        {
            if repo
                .members
                .iter()
                .any(|grant| input.subjects.contains(&grant.subject))
            {
                before.insert(repo.id, repo.membership_digest()?);
                let count = repo.members.len();
                repo.members
                    .retain(|grant| !input.subjects.contains(&grant.subject));
                grants += count - repo.members.len();
            }
        }
        if before.is_empty() || cancel.is_cancelled() {
            return Err(CatalogError::Conflict);
        }
        let repositories: Vec<_> = before.keys().copied().collect();
        let impact = Impact {
            collaborators: input.subjects.len(),
            repositories: repositories.len(),
            grants,
        };
        let after = audit_digest(&document, input.organization, &repositories)?;
        document.version = input.position.revision;
        document.queue_membership_audit(AccessAudit::OutsideRemoval(OutsideRemovalAudit {
            position: input.position,
            previous: document.membership_audit_head.clone(),
            organization: input.organization,
            previous_organization: None,
            actor: input.actor,
            occurred_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| CatalogError::Invalid("system clock is before epoch"))?
                .as_secs(),
            confirmation: input.confirmation,
            repositories,
            before: digest(before),
            after,
            collaborators: impact.collaborators,
            grants: impact.grants,
        }));
        document.validate(&self.root)?;
        // The exact revision and outbox entry fence retries even when the
        // object-store response or immutable audit flush is interrupted.
        if cancel.is_cancelled() || !self.write_document(&document, etag).await? {
            return Err(CatalogError::Conflict);
        }
        let _ = self.flush_membership_audit().await;
        Ok(impact)
    }
}

pub(super) fn audit_digest(
    document: &CatalogDocument,
    organization: Uuid,
    repositories: &[Uuid],
) -> Result<String, CatalogError> {
    let records: BTreeMap<_, _> = document
        .organization_grant_records(organization)
        .filter(|repo| repositories.binary_search(&repo.id).is_ok())
        .map(|repo| Ok((repo.id, repo.membership_digest()?)))
        .collect::<Result<_, CatalogError>>()?;
    if records.len() != repositories.len() {
        return Err(CatalogError::Invalid(
            "outside removal audit repositories are missing",
        ));
    }
    Ok(digest(records))
}

fn digest(records: BTreeMap<Uuid, String>) -> String {
    let mut digest = blake3::Hasher::new();
    digest.update(b"crab.catalog.outside-removal.v1");
    for (id, access) in records {
        digest.update(id.as_bytes());
        digest.update(access.as_bytes());
    }
    digest.finalize().to_hex().to_string()
}
