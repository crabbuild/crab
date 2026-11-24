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
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Scope {
    pub(super) organization: Uuid,
    action: Action,
    name: String,
    previous: Option<Position>,
}

impl Scope {
    pub(super) fn for_record(record: &CatalogRecord, action: Action) -> Option<Self> {
        Some(Self {
            organization: record.organization_id()?,
            action,
            name: record.name.clone(),
            previous: None,
        })
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
        self.position().valid(u64::MAX)
            && !self.repository_id.is_nil()
            && self.occurred_at > 0
            && self.occurred_at <= 9_007_199_254_740
            && valid_actor(&self.actor)
            && valid_digest(&self.previous_members_digest)
            && valid_digest(&self.new_members_digest)
            && self.organizations.len() <= 2
            && self.organizations.iter().enumerate().all(|(index, scope)| {
                !scope.organization.is_nil()
                    && crate::config::valid_repository_component(&scope.name)
                    && self.organizations[..index]
                        .iter()
                        .all(|other| other.organization != scope.organization)
                    && scope
                        .previous
                        .is_none_or(|position| position.valid(self.repository_version - 1))
            })
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
    pub(crate) name: String,
    pub(crate) repository: Uuid,
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
            name: scope.name,
            repository: event.repository_id,
            actor: event.actor,
            occurred_at_ms: event.occurred_at * 1000,
        })
    }
}
