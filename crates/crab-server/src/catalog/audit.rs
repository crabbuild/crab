use super::{
    CatalogDocument, CatalogError, CatalogRecord, CatalogStore, MEMBERSHIP_AUDIT_RELATIVE_PREFIX,
    MembershipActor, MembershipAuditEvent, valid_actor, valid_digest,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Position {
    pub(crate) revision: u64,
    pub(crate) id: Uuid,
}

impl Position {
    pub(crate) fn valid(self, upper: u64) -> bool {
        self.revision > 0 && self.revision <= upper && !self.id.is_nil()
    }

    pub(super) fn path(self) -> String {
        format!(
            "{MEMBERSHIP_AUDIT_RELATIVE_PREFIX}/{}-{}.json",
            self.revision, self.id
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Registered,
    AccessChanged,
    TransferredIn,
    TransferredOut,
    Deleted,
    Restored,
    VisibilityChanged,
    OutsideCollaboratorsRemoved,
    CollaboratorGrantsRetained,
    MemberGrantsRevoked,
    MemberGrantsRestored,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct OutsideGrantChange {
    pub(crate) collaborators: usize,
    pub(crate) repositories: usize,
    pub(crate) grants: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Scope {
    pub(super) organization: Uuid,
    action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    previous: Option<Position>,
}

impl Scope {
    pub(super) fn for_record(record: &CatalogRecord, action: Action) -> Option<Self> {
        Some(Self {
            organization: record.organization_id()?,
            action,
            name: Some(record.name.clone()),
            previous: None,
        })
    }

    pub(super) fn outside_removal(organization: Uuid) -> Self {
        Self::membership_change(organization, Action::OutsideCollaboratorsRemoved)
    }

    pub(super) fn membership_change(organization: Uuid, action: Action) -> Self {
        Self {
            organization,
            action,
            name: None,
            previous: None,
        }
    }
}

impl MembershipAuditEvent {
    pub(super) fn position(&self) -> Position {
        Position {
            revision: self.repository_version,
            id: self.id,
        }
    }

    pub(super) fn valid(&self) -> bool {
        let target_valid = match (self.repository_id, self.outside_grants) {
            (Some(repository), None) => {
                !repository.is_nil()
                    && self.organizations.iter().all(|scope| {
                        !matches!(
                            scope.action,
                            Action::OutsideCollaboratorsRemoved
                                | Action::CollaboratorGrantsRetained
                                | Action::MemberGrantsRevoked
                                | Action::MemberGrantsRestored
                        ) && scope
                            .name
                            .as_deref()
                            .is_some_and(crate::config::valid_repository_component)
                    })
            }
            (None, Some(change)) => match self.organizations.as_slice() {
                [
                    Scope {
                        action: Action::OutsideCollaboratorsRemoved,
                        name: None,
                        ..
                    },
                ] => {
                    (1..=50).contains(&change.collaborators)
                        && change.repositories > 0
                        && change.grants >= change.collaborators.max(change.repositories)
                }
                [
                    Scope {
                        action: Action::CollaboratorGrantsRetained | Action::MemberGrantsRevoked,
                        name: None,
                        ..
                    },
                ] => {
                    (1..=crate::cells::organization::MAX_DEPARTURE_MEMBERS)
                        .contains(&change.collaborators)
                        && change.grants >= change.repositories
                }
                [
                    Scope {
                        action: Action::MemberGrantsRestored,
                        name: None,
                        ..
                    },
                ] => {
                    change.collaborators == 1
                        && change.repositories > 0
                        && change.grants == change.repositories
                }
                _ => false,
            },
            _ => false,
        };
        self.position().valid(u64::MAX)
            && target_valid
            && self.occurred_at > 0
            && self.occurred_at <= 9_007_199_254_740
            && valid_actor(&self.actor)
            && valid_digest(&self.previous_members_digest)
            && valid_digest(&self.new_members_digest)
            && self.organizations.len() <= 2
            && self.organizations.iter().enumerate().all(|(index, scope)| {
                !scope.organization.is_nil()
                    && self.organizations[..index]
                        .iter()
                        .all(|other| other.organization != scope.organization)
                    && scope
                        .previous
                        .is_none_or(|position| position.valid(self.repository_version - 1))
            })
    }

    pub(super) fn digest(&self, document: &CatalogDocument) -> Result<String, CatalogError> {
        if let Some(id) = self.repository_id {
            return document
                .repositories
                .iter()
                .find(|record| record.id == id)
                .or_else(|| {
                    document
                        .deleted_repository(id)
                        .map(|entry| &entry.repository)
                })
                .ok_or(CatalogError::Invalid("audit repository is missing"))?
                .membership_digest();
        }
        let [scope] = self.organizations.as_slice() else {
            return Err(CatalogError::Invalid("audit organization is missing"));
        };
        document.organization_membership_digest(scope.organization)
    }
}

impl CatalogDocument {
    pub(super) fn queue_membership_audit(&mut self, mut event: MembershipAuditEvent) {
        // The catalog CAS commits the event with the grant change. Each scoped
        // link uses its own head so a transfer cannot disclose another org's history.
        for scope in &mut event.organizations {
            scope.previous = self
                .organization_audit_heads
                .get(&scope.organization)
                .copied();
        }
        self.pending_membership_audit = Some(event);
    }

    pub(super) fn valid_pending_scopes(&self, event: &MembershipAuditEvent) -> bool {
        event.organizations.iter().all(|scope| {
            scope.previous
                == self
                    .organization_audit_heads
                    .get(&scope.organization)
                    .copied()
        })
    }

    pub(crate) fn organization_audit_start(&self, organization: Uuid) -> Option<Position> {
        self.pending_membership_audit
            .as_ref()
            .filter(|event| {
                event
                    .organizations
                    .iter()
                    .any(|scope| scope.organization == organization)
            })
            .map(MembershipAuditEvent::position)
            .or_else(|| self.organization_audit_heads.get(&organization).copied())
    }
}

pub(crate) struct Entry {
    pub(crate) position: Position,
    pub(crate) previous: Option<Position>,
    pub(crate) action: Action,
    pub(crate) repository: Option<(Uuid, String)>,
    pub(crate) outside_grants: Option<OutsideGrantChange>,
    pub(crate) actor: MembershipActor,
    pub(crate) occurred_at_ms: u64,
}

impl CatalogStore {
    pub(crate) async fn organization_audit_entry(
        &self,
        snapshot: &CatalogDocument,
        organization: Uuid,
        position: Position,
    ) -> Result<Entry, CatalogError> {
        if !position.valid(snapshot.version) {
            return Err(CatalogError::Invalid(
                "organization audit position is invalid",
            ));
        }
        let event = if let Some(pending) = snapshot
            .pending_membership_audit
            .as_ref()
            .filter(|event| event.position() == position)
        {
            pending.clone()
        } else {
            let (body, _) = self
                .root
                .store
                .get_with_etag_bounded(&self.root.path(&position.path()), 16 * 1024)
                .await?;
            serde_json::from_slice::<MembershipAuditEvent>(&body)?
        };
        if event.position() != position || !event.valid() {
            return Err(CatalogError::Invalid(
                "organization audit record is invalid",
            ));
        }
        let scope = event
            .organizations
            .into_iter()
            .find(|scope| scope.organization == organization)
            .ok_or(CatalogError::Invalid("organization audit scope is invalid"))?;
        Ok(Entry {
            position,
            previous: scope.previous,
            action: scope.action,
            repository: event.repository_id.zip(scope.name),
            outside_grants: event.outside_grants,
            actor: event.actor,
            occurred_at_ms: event.occurred_at * 1000,
        })
    }
}
