use std::collections::HashSet;

use bytes::Bytes;
use crab_metadata::{layout_descriptor::read_canonical_layout, manifest_store::read_manifest};
use crab_storage::{ETag, StorageError, StoreLayout};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{BranchProtection, RepositoryConfig, RepositoryMember, storage_root::StorageRoot};

const SCHEMA_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_REPOSITORIES: usize = 10_000;
const MAX_CAS_ATTEMPTS: usize = 8;
const CATALOG_RELATIVE_PATH: &str = ".crab/http-server/v1/catalog.json";

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("repository catalog storage failed")]
    Storage(#[from] StorageError),
    #[error("repository catalog encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("repository catalog is invalid: {0}")]
    Invalid(&'static str),
    #[error("repository catalog changed concurrently")]
    Conflict,
    #[error("repository is not present in the catalog")]
    NotFound,
    #[error("repository initialization failed")]
    Initialize(#[from] crab_write::WriteError),
    #[error("repository metadata validation failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogRecord {
    pub id: Uuid,
    pub owner: String,
    pub name: String,
    pub prefix: String,
    pub placement_generation: u64,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub members: Vec<RepositoryMember>,
    #[serde(default)]
    pub protected_branches: Vec<BranchProtection>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogDocument {
    pub schema_version: u32,
    pub version: u64,
    pub repositories: Vec<CatalogRecord>,
}

impl Default for CatalogDocument {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            version: 0,
            repositories: Vec::new(),
        }
    }
}

impl CatalogDocument {
    fn validate(&self, root: &StorageRoot) -> Result<(), CatalogError> {
        if self.schema_version != SCHEMA_VERSION || self.repositories.len() > MAX_REPOSITORIES {
            return Err(CatalogError::Invalid(
                "unsupported schema or repository count",
            ));
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        let mut prefixes = HashSet::new();
        for record in &self.repositories {
            if record.placement_generation == 0
                || !ids.insert(record.id)
                || !names.insert((record.owner.to_lowercase(), record.name.to_lowercase()))
                || !prefixes.insert(record.prefix.clone())
            {
                return Err(CatalogError::Invalid(
                    "repository IDs, names, prefixes, and placement generations must be valid and unique",
                ));
            }
            let runtime = record.runtime_config(root, "main")?;
            crate::config::validate_repository(&runtime)
                .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
        }
        Ok(())
    }

    fn normalize(&mut self) {
        self.repositories.sort_by(|left, right| {
            (left.owner.to_lowercase(), left.name.to_lowercase())
                .cmp(&(right.owner.to_lowercase(), right.name.to_lowercase()))
        });
    }
}

impl CatalogRecord {
    pub(crate) fn runtime_config(
        &self,
        root: &StorageRoot,
        default_branch: &str,
    ) -> Result<RepositoryConfig, CatalogError> {
        let normalized = crab_git::url::normalize_repository_prefix(&self.prefix)
            .map_err(|_| CatalogError::Invalid("repository prefix is invalid"))?;
        if normalized != self.prefix || normalized == ".crab" || normalized.starts_with(".crab/") {
            return Err(CatalogError::Invalid(
                "repository prefix must be canonical and outside the reserved .crab namespace",
            ));
        }
        Ok(RepositoryConfig {
            owner: self.owner.clone(),
            name: self.name.clone(),
            bucket: root.bucket_label.clone(),
            prefix: root
                .repository_prefix(&normalized)
                .map_err(|_| CatalogError::Invalid("repository prefix is invalid"))?,
            default_branch: default_branch.to_owned(),
            description: self.description.clone(),
            members: self.members.clone(),
            protected_branches: self.protected_branches.clone(),
        })
    }
}

#[derive(Clone)]
pub struct CatalogStore {
    root: StorageRoot,
    path: object_store::path::Path,
}

impl CatalogStore {
    pub(crate) fn new(root: StorageRoot) -> Self {
        let path = root.path(CATALOG_RELATIVE_PATH);
        Self { root, path }
    }

    /// Open the catalog configured for this server deployment.
    pub fn from_config(config: &crate::Config) -> crate::Result<Self> {
        config.validate()?;
        Ok(Self::new(StorageRoot::build(&config.storage)?))
    }

    pub(crate) fn root(&self) -> &StorageRoot {
        &self.root
    }

    pub async fn load(&self) -> Result<(CatalogDocument, Option<ETag>), CatalogError> {
        let (body, etag) = match self
            .root
            .store
            .get_with_etag_bounded(&self.path, MAX_CATALOG_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => return Ok((CatalogDocument::default(), None)),
            Err(error) => return Err(error.into()),
        };
        let document: CatalogDocument = serde_json::from_slice(&body)?;
        document.validate(&self.root)?;
        Ok((document, Some(etag)))
    }

    pub async fn create_repository(
        &self,
        owner: String,
        name: String,
        prefix: String,
        default_branch: String,
        description: String,
        members: Vec<RepositoryMember>,
    ) -> Result<CatalogRecord, CatalogError> {
        let record = CatalogRecord {
            id: Uuid::now_v7(),
            owner,
            name,
            prefix,
            placement_generation: 1,
            description,
            members,
            protected_branches: Vec::new(),
        };
        let runtime = record.runtime_config(&self.root, &default_branch)?;
        crate::config::validate_repository(&runtime)
            .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
        let layout = StoreLayout::new(self.root.store.clone(), runtime.prefix.clone());
        crab_write::initialize::initialize_repository(
            &self.root.store,
            &layout,
            &format!("refs/heads/{default_branch}"),
        )
        .await?;
        read_canonical_layout(&self.root.store, &layout).await?;
        read_manifest(&self.root.store, &layout).await?;
        self.insert(record).await
    }

    pub async fn adopt_repository(
        &self,
        owner: String,
        name: String,
        prefix: String,
        description: String,
        members: Vec<RepositoryMember>,
    ) -> Result<CatalogRecord, CatalogError> {
        let record = CatalogRecord {
            id: Uuid::now_v7(),
            owner,
            name,
            prefix,
            placement_generation: 1,
            description,
            members,
            protected_branches: Vec::new(),
        };
        let runtime = record.runtime_config(&self.root, "main")?;
        crate::config::validate_repository(&runtime)
            .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
        let layout = StoreLayout::new(self.root.store.clone(), runtime.prefix);
        read_canonical_layout(&self.root.store, &layout).await?;
        read_manifest(&self.root.store, &layout).await?;
        self.insert(record).await
    }

    /// Atomically replaces one repository's membership.
    ///
    /// A concurrent catalog writer causes this operation to fail instead of
    /// replaying a potentially stale administrative decision over newer state.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] for an unknown repository,
    /// [`CatalogError::Conflict`] after a concurrent catalog change, or the
    /// original validation, encoding, or storage error.
    pub async fn set_members(
        &self,
        owner: &str,
        name: &str,
        members: Vec<RepositoryMember>,
    ) -> Result<CatalogRecord, CatalogError> {
        let (mut document, etag) = self.load().await?;
        let Some(record) = document.repositories.iter_mut().find(|record| {
            record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
        }) else {
            return Err(CatalogError::NotFound);
        };
        if record.members == members {
            return Ok(record.clone());
        }
        record.members = members;
        let updated = record.clone();
        document.version = document
            .version
            .checked_add(1)
            .ok_or(CatalogError::Invalid("catalog version overflowed"))?;
        document.validate(&self.root)?;
        if self.write_document(&document, etag).await? {
            Ok(updated)
        } else {
            Err(CatalogError::Conflict)
        }
    }

    async fn insert(&self, record: CatalogRecord) -> Result<CatalogRecord, CatalogError> {
        for _ in 0..MAX_CAS_ATTEMPTS {
            let (mut document, etag) = self.load().await?;
            if let Some(existing) = document.repositories.iter().find(|existing| {
                existing.owner.eq_ignore_ascii_case(&record.owner)
                    && existing.name.eq_ignore_ascii_case(&record.name)
            }) {
                return if existing.prefix == record.prefix {
                    Ok(existing.clone())
                } else {
                    Err(CatalogError::Conflict)
                };
            }
            if document
                .repositories
                .iter()
                .any(|existing| existing.prefix == record.prefix)
            {
                return Err(CatalogError::Conflict);
            }
            document.version = document
                .version
                .checked_add(1)
                .ok_or(CatalogError::Invalid("catalog version overflowed"))?;
            document.repositories.push(record.clone());
            document.normalize();
            document.validate(&self.root)?;
            if self.write_document(&document, etag).await? {
                return Ok(record);
            }
        }
        Err(CatalogError::Conflict)
    }

    async fn write_document(
        &self,
        document: &CatalogDocument,
        etag: Option<ETag>,
    ) -> Result<bool, CatalogError> {
        let body = Bytes::from(serde_json::to_vec(document)?);
        if body.len() as u64 > MAX_CATALOG_BYTES {
            return Err(CatalogError::Invalid("catalog exceeds its byte limit"));
        }
        let result = match etag {
            Some(etag) => self.root.store.update(&self.path, body, etag).await,
            None => {
                self.root
                    .store
                    .create_strict_with_etag(&self.path, body)
                    .await
            }
        };
        match result {
            Ok(_) => Ok(true),
            Err(StorageError::StateConflict { .. }) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;

    fn catalog() -> CatalogStore {
        let store = crab_storage::Store::new(Arc::new(InMemory::new()));
        CatalogStore::new(StorageRoot::memory(store, "repositories"))
    }

    #[tokio::test]
    async fn create_initializes_and_publishes_one_repository() {
        let catalog = catalog();
        let record = catalog
            .create_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                "Project".into(),
                vec![],
            )
            .await
            .unwrap();
        let (document, _) = catalog.load().await.unwrap();
        assert_eq!(document.repositories, [record]);
        assert_eq!(document.version, 1);
    }

    #[tokio::test]
    async fn adopt_requires_canonical_repository_metadata() {
        let catalog = catalog();
        let result = catalog
            .adopt_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                String::new(),
                vec![],
            )
            .await;
        assert!(matches!(result, Err(CatalogError::Metadata(_))));
    }

    #[tokio::test]
    async fn retrying_create_returns_the_existing_record() {
        let catalog = catalog();
        let first = catalog
            .create_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                String::new(),
                vec![],
            )
            .await
            .unwrap();
        let second = catalog
            .create_repository(
                "TEAM".into(),
                "PROJECT".into(),
                "team/project".into(),
                "main".into(),
                String::new(),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn membership_replacement_is_atomic_and_idempotent() {
        let catalog = catalog();
        catalog
            .create_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                String::new(),
                vec![],
            )
            .await
            .unwrap();
        let members = vec![RepositoryMember {
            subject: "alice-sub".into(),
            name: "Alice".into(),
            access: crate::RepositoryAccess::Admin,
        }];

        let updated = catalog
            .set_members("TEAM", "PROJECT", members.clone())
            .await
            .unwrap();
        let (after_update, _) = catalog.load().await.unwrap();
        let repeated = catalog
            .set_members("team", "project", members)
            .await
            .unwrap();
        let (after_repeat, _) = catalog.load().await.unwrap();

        assert_eq!(updated, repeated);
        assert_eq!(after_update.version, 2);
        assert_eq!(after_repeat, after_update);
    }

    #[tokio::test]
    async fn membership_replacement_requires_a_cataloged_repository() {
        let result = catalog().set_members("team", "missing", vec![]).await;

        assert!(matches!(result, Err(CatalogError::NotFound)));
    }

    #[tokio::test]
    async fn invalid_membership_cannot_change_the_catalog() {
        let catalog = catalog();
        catalog
            .create_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                String::new(),
                vec![],
            )
            .await
            .unwrap();
        let (before, _) = catalog.load().await.unwrap();
        let member = RepositoryMember {
            subject: "duplicate-subject".into(),
            name: "Alice".into(),
            access: crate::RepositoryAccess::Admin,
        };

        let result = catalog
            .set_members(
                "team",
                "project",
                vec![
                    member.clone(),
                    RepositoryMember {
                        name: "Bob".into(),
                        ..member
                    },
                ],
            )
            .await;
        let (after, _) = catalog.load().await.unwrap();

        assert!(matches!(result, Err(CatalogError::Invalid(_))));
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn repository_prefixes_cannot_alias_or_enter_server_state() {
        let catalog = catalog();
        for prefix in ["/team/project/", ".crab/http-server/v1/auth"] {
            let result = catalog
                .adopt_repository(
                    "team".into(),
                    "project".into(),
                    prefix.into(),
                    String::new(),
                    vec![],
                )
                .await;
            assert!(matches!(result, Err(CatalogError::Invalid(_))), "{prefix}");
        }
    }
}
