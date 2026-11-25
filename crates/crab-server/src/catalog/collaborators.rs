use uuid::Uuid;

use super::{CatalogDocument, CatalogError, CatalogRecord, RepositoryApplicationState};

impl CatalogRecord {
    pub(crate) fn holds_organization_grants(&self, organization: Uuid) -> bool {
        self.owns_direct_grants(organization)
            && self.application == RepositoryApplicationState::CellReady
    }
}

impl CatalogDocument {
    pub(crate) fn organization_grant_records(
        &self,
        organization: Uuid,
    ) -> impl Iterator<Item = &CatalogRecord> {
        self.repositories
            .iter()
            .chain(
                self.deleted_repositories
                    .iter()
                    .map(|entry| &entry.repository),
            )
            .filter(move |record| record.holds_organization_grants(organization))
    }

    pub(super) fn organization_membership_digest(
        &self,
        organization: Uuid,
    ) -> Result<String, CatalogError> {
        let mut digest = blake3::Hasher::new();
        digest.update(organization.as_bytes());
        for record in self.organization_grant_records(organization) {
            digest.update(record.id.as_bytes());
            digest.update(record.membership_digest()?.as_bytes());
        }
        Ok(digest.finalize().to_hex().to_string())
    }
}
