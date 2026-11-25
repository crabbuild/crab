use std::collections::BTreeSet;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    CatalogDocument, CatalogError, CatalogRecord, CatalogStore, MembershipActor,
    MembershipAuditEvent, RepositoryApplicationState,
    audit::{OutsideGrantChange, Scope},
};

pub(crate) struct Revoke {
    pub(crate) organization: Uuid,
    pub(crate) revision: u64,
    pub(crate) subjects: BTreeSet<String>,
    pub(crate) actor: MembershipActor,
}

impl CatalogRecord {
    pub(crate) fn holds_organization_grants(&self, organization: Uuid) -> bool {
        self.organization_id() == Some(organization)
            && self.forked_from.is_none()
            && self.application == RepositoryApplicationState::CellReady
    }
}

impl CatalogDocument {
    pub(super) fn organization_membership_digest(
        &self,
        organization: Uuid,
    ) -> Result<String, CatalogError> {
        let mut digest = blake3::Hasher::new();
        digest.update(organization.as_bytes());
        for record in self
            .repositories
            .iter()
            .chain(
                self.deleted_repositories
                    .iter()
                    .map(|entry| &entry.repository),
            )
            .filter(|record| record.holds_organization_grants(organization))
        {
            digest.update(record.id.as_bytes());
            digest.update(record.membership_digest()?.as_bytes());
        }
        Ok(digest.finalize().to_hex().to_string())
    }
}

impl CatalogStore {
    pub(crate) async fn revoke_outside_grants(
        &self,
        input: Revoke,
        cancel: &CancellationToken,
    ) -> Result<OutsideGrantChange, CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if document.version != input.revision || !(1..=50).contains(&input.subjects.len()) {
            return Err(CatalogError::Conflict);
        }
        let previous = document.organization_membership_digest(input.organization)?;
        let mut removed = OutsideGrantChange {
            collaborators: input.subjects.len(),
            repositories: 0,
            grants: 0,
        };
        let mut seen = BTreeSet::new();
        for record in &document.repositories {
            if record.holds_organization_grants(input.organization) {
                seen.extend(
                    record
                        .members
                        .iter()
                        .filter_map(|member| input.subjects.get(&member.subject)),
                );
            }
        }
        if seen.len() != input.subjects.len() {
            return Err(CatalogError::Conflict);
        }
        // Include deleted source records in the same CAS so a restore cannot
        // reactivate a grant that the owner removed across the organization.
        for record in document
            .repositories
            .iter_mut()
            .chain(
                document
                    .deleted_repositories
                    .iter_mut()
                    .map(|entry| &mut entry.repository),
            )
            .filter(|record| record.holds_organization_grants(input.organization))
        {
            let before = record.members.len();
            record
                .members
                .retain(|member| !input.subjects.contains(&member.subject));
            let revoked = before - record.members.len();
            removed.grants += revoked;
            removed.repositories += usize::from(revoked > 0);
        }
        let current = document.organization_membership_digest(input.organization)?;
        document.version = document
            .version
            .checked_add(1)
            .ok_or(CatalogError::Conflict)?;
        let occurred_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| CatalogError::Invalid("system clock is before epoch"))?
            .as_secs();
        document.queue_membership_audit(MembershipAuditEvent {
            id: Uuid::now_v7(),
            repository_id: None,
            actor: input.actor,
            previous_members_digest: previous,
            new_members_digest: current,
            repository_version: document.version,
            occurred_at,
            previous: document.membership_audit_head.clone(),
            organizations: vec![Scope::outside_removal(input.organization)],
            outside_grants: Some(removed),
        });
        document.validate(&self.root)?;
        if cancel.is_cancelled() || !self.write_document(&document, etag).await? {
            return Err(CatalogError::Conflict);
        }
        let _ = self.flush_membership_audit().await;
        Ok(removed)
    }
}
