use std::collections::HashSet;

use bytes::Bytes;
use crab_metadata::{layout_descriptor::read_canonical_layout, manifest_store::read_manifest};
use crab_storage::{ETag, StorageError, StoreLayout};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{BranchProtection, RepositoryConfig, RepositoryMember, storage_root::StorageRoot};

const SCHEMA_VERSION: u32 = 3;
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
    pub application: RepositoryApplicationState,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub members: Vec<RepositoryMember>,
    #[serde(default)]
    pub protected_branches: Vec<BranchProtection>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryApplicationState {
    EmptyCellPending,
    CellReady,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogDocument {
    pub schema_version: u32,
    pub version: u64,
    pub repositories: Vec<CatalogRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub membership_audit_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_membership_audit: Option<MembershipAuditEvent>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MembershipActor {
    pub issuer: String,
    pub subject: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MembershipSnapshot {
    pub revision: u64,
    pub members: Vec<RepositoryMember>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MembershipAuditEvent {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub actor: MembershipActor,
    pub previous_members_digest: String,
    pub new_members_digest: String,
    pub repository_version: u64,
    pub occurred_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

#[derive(Deserialize)]
struct CatalogSchema {
    schema_version: u32,
}

impl Default for CatalogDocument {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            version: 0,
            repositories: Vec::new(),
            membership_audit_head: None,
            pending_membership_audit: None,
        }
    }
}

impl CatalogDocument {
    fn validate(&self, root: &StorageRoot) -> Result<(), CatalogError> {
        if !matches!(self.schema_version, 2 | SCHEMA_VERSION)
            || self.repositories.len() > MAX_REPOSITORIES
        {
            return Err(CatalogError::Invalid(
                "unsupported schema or repository count",
            ));
        }
        if let Some(event) = &self.pending_membership_audit {
            if event.repository_version != self.version
                || !valid_actor(&event.actor)
                || event.occurred_at == 0
                || event.id.is_nil()
                || !self
                    .repositories
                    .iter()
                    .any(|record| record.id == event.repository_id)
                || event.previous != self.membership_audit_head
                || !valid_digest(&event.previous_members_digest)
                || !valid_digest(&event.new_members_digest)
            {
                return Err(CatalogError::Invalid("membership audit event is invalid"));
            }
            let record = self
                .repositories
                .iter()
                .find(|record| record.id == event.repository_id)
                .ok_or(CatalogError::Invalid("audit repository is missing"))?;
            if membership_digest(&record.members)? != event.new_members_digest {
                return Err(CatalogError::Invalid(
                    "membership audit digest does not match",
                ));
            }
        }
        if self
            .membership_audit_head
            .as_ref()
            .is_some_and(|path| !valid_audit_path(root, path))
            || (self.schema_version == 2
                && (self.membership_audit_head.is_some()
                    || self.pending_membership_audit.is_some()))
        {
            return Err(CatalogError::Invalid("membership audit chain is invalid"));
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

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_actor(actor: &MembershipActor) -> bool {
    [&actor.issuer, &actor.subject].iter().all(|value| {
        !value.is_empty() && value.chars().count() <= 512 && !value.chars().any(char::is_control)
    })
}

fn valid_audit_path(root: &StorageRoot, path: &str) -> bool {
    let prefix = format!("{}/", root.path(".crab/http-server/v1/audit/membership"));
    let Some(file) = path
        .strip_prefix(&prefix)
        .and_then(|file| file.strip_suffix(".json"))
    else {
        return false;
    };
    let Some((version, id)) = file.split_once('-') else {
        return false;
    };
    version.parse::<u64>().is_ok_and(|value| value > 0) && Uuid::parse_str(id).is_ok()
}

fn membership_digest(members: &[RepositoryMember]) -> Result<String, CatalogError> {
    let mut canonical = members.to_vec();
    canonical.sort_by(|a, b| (&a.subject, &a.name, a.access).cmp(&(&b.subject, &b.name, b.access)));
    let bytes = serde_json::to_vec(&canonical)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab membership digest v1");
    hasher.update(&bytes);
    Ok(hasher.finalize().to_hex().to_string())
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
        let schema: CatalogSchema = serde_json::from_slice(&body)?;
        if !matches!(schema.schema_version, 2 | SCHEMA_VERSION) {
            return Err(CatalogError::Invalid("unsupported catalog schema"));
        }
        let document: CatalogDocument = serde_json::from_slice(&body)?;
        // Version 2 had no audit fields. It remains readable until its next write.
        document.validate(&self.root)?;
        Ok((document, Some(etag)))
    }

    pub async fn membership(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<MembershipSnapshot, CatalogError> {
        let (document, _) = self.load().await?;
        let record = document
            .repositories
            .iter()
            .find(|record| {
                record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
            })
            .ok_or(CatalogError::NotFound)?;
        Ok(MembershipSnapshot {
            revision: document.version,
            members: record.members.clone(),
        })
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
        self.create_repository_with_mode(
            owner,
            name,
            prefix,
            default_branch,
            description,
            members,
            true,
        )
        .await
    }

    pub(crate) async fn create_repository_exclusive(
        &self,
        owner: String,
        name: String,
        prefix: String,
        default_branch: String,
        description: String,
        members: Vec<RepositoryMember>,
    ) -> Result<CatalogRecord, CatalogError> {
        self.create_repository_with_mode(
            owner,
            name,
            prefix,
            default_branch,
            description,
            members,
            false,
        )
        .await
    }

    async fn create_repository_with_mode(
        &self,
        owner: String,
        name: String,
        prefix: String,
        default_branch: String,
        description: String,
        members: Vec<RepositoryMember>,
        allow_existing: bool,
    ) -> Result<CatalogRecord, CatalogError> {
        let record = CatalogRecord {
            id: Uuid::now_v7(),
            owner,
            name,
            prefix,
            placement_generation: 1,
            application: RepositoryApplicationState::EmptyCellPending,
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
        self.insert_with_mode(record, allow_existing).await
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
            application: RepositoryApplicationState::EmptyCellPending,
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
        require_administrator: bool,
    ) -> Result<CatalogRecord, CatalogError> {
        let snapshot = self.membership(owner, name).await?;
        self.replace_members(
            owner,
            name,
            snapshot.revision,
            members,
            MembershipActor {
                issuer: "urn:crab:local".into(),
                subject: "operator".into(),
            },
            require_administrator,
        )
        .await
    }

    pub async fn replace_members(
        &self,
        owner: &str,
        name: &str,
        expected_revision: u64,
        members: Vec<RepositoryMember>,
        actor: MembershipActor,
        require_administrator: bool,
    ) -> Result<CatalogRecord, CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if document.version != expected_revision {
            return Err(CatalogError::Conflict);
        }
        let Some(record) = document.repositories.iter_mut().find(|record| {
            record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
        }) else {
            return Err(CatalogError::NotFound);
        };
        let mut candidate = record.clone();
        candidate.members = members.clone();
        crate::config::validate_repository(&candidate.runtime_config(&self.root, "main")?)
            .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
        if require_administrator
            && !members
                .iter()
                .any(|member| member.access == crate::RepositoryAccess::Admin)
        {
            return Err(CatalogError::Invalid(
                "at least one administrator is required",
            ));
        }
        if record.members == members {
            return Ok(record.clone());
        }
        let previous_digest = membership_digest(&record.members)?;
        let new_digest = membership_digest(&members)?;
        record.members = members;
        let updated = record.clone();
        document.version = document
            .version
            .checked_add(1)
            .ok_or(CatalogError::Invalid("catalog version overflowed"))?;
        document.schema_version = SCHEMA_VERSION;
        document.pending_membership_audit = Some(MembershipAuditEvent {
            id: Uuid::now_v7(),
            repository_id: updated.id,
            actor,
            previous_members_digest: previous_digest,
            new_members_digest: new_digest,
            repository_version: document.version,
            occurred_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| CatalogError::Invalid("system clock is before epoch"))?
                .as_secs(),
            previous: document.membership_audit_head.clone(),
        });
        document.validate(&self.root)?;
        if self.write_document(&document, etag).await? {
            let _ = self.flush_membership_audit().await;
            Ok(updated)
        } else {
            Err(CatalogError::Conflict)
        }
    }

    pub async fn flush_membership_audit(&self) -> Result<(), CatalogError> {
        let (mut document, etag) = self.load().await?;
        let Some(event) = document.pending_membership_audit.clone() else {
            return Ok(());
        };
        let path = self.root.path(&format!(
            ".crab/http-server/v1/audit/membership/{}-{}.json",
            event.repository_version, event.id
        ));
        match self
            .root
            .store
            .create_strict(&path, Bytes::from(serde_json::to_vec(&event)?))
            .await
        {
            Ok(()) => {}
            Err(StorageError::StateConflict { .. }) => {
                let (body, _) = self
                    .root
                    .store
                    .get_with_etag_bounded(&path, MAX_CATALOG_BYTES)
                    .await?;
                if serde_json::from_slice::<MembershipAuditEvent>(&body)? != event {
                    return Err(CatalogError::Invalid("immutable membership audit differs"));
                }
            }
            Err(error) => return Err(error.into()),
        }
        document.membership_audit_head = Some(path.to_string());
        document.pending_membership_audit = None;
        document.validate(&self.root)?;
        let _ = self.write_document(&document, etag).await?;
        Ok(())
    }

    async fn load_for_mutation(&self) -> Result<(CatalogDocument, Option<ETag>), CatalogError> {
        for _ in 0..MAX_CAS_ATTEMPTS {
            let loaded = self.load().await?;
            if loaded.0.pending_membership_audit.is_none() {
                return Ok(loaded);
            }
            // Never overwrite the single audit outbox: its event committed with the
            // previous membership and must become the chain head before another write.
            self.flush_membership_audit().await?;
        }
        Err(CatalogError::Conflict)
    }

    pub(crate) async fn mark_cell_ready(
        &self,
        repository: Uuid,
    ) -> Result<CatalogRecord, CatalogError> {
        for _ in 0..MAX_CAS_ATTEMPTS {
            let (mut document, etag) = self.load_for_mutation().await?;
            let record = document
                .repositories
                .iter_mut()
                .find(|record| record.id == repository)
                .ok_or(CatalogError::NotFound)?;
            if record.application == RepositoryApplicationState::CellReady {
                return Ok(record.clone());
            }
            record.application = RepositoryApplicationState::CellReady;
            let updated = record.clone();
            document.version = document
                .version
                .checked_add(1)
                .ok_or(CatalogError::Invalid("catalog version overflowed"))?;
            document.schema_version = SCHEMA_VERSION;
            document.validate(&self.root)?;
            if self.write_document(&document, etag).await? {
                return Ok(updated);
            }
        }
        Err(CatalogError::Conflict)
    }

    async fn insert(&self, record: CatalogRecord) -> Result<CatalogRecord, CatalogError> {
        self.insert_with_mode(record, true).await
    }

    async fn insert_with_mode(
        &self,
        record: CatalogRecord,
        allow_existing: bool,
    ) -> Result<CatalogRecord, CatalogError> {
        for _ in 0..MAX_CAS_ATTEMPTS {
            let (mut document, etag) = self.load_for_mutation().await?;
            if let Some(existing) = document.repositories.iter().find(|existing| {
                existing.owner.eq_ignore_ascii_case(&record.owner)
                    && existing.name.eq_ignore_ascii_case(&record.name)
            }) {
                return if allow_existing && existing.prefix == record.prefix {
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
            document.schema_version = SCHEMA_VERSION;
            document.repositories.push(record.clone());
            document.normalize();
            if !record.members.is_empty() {
                document.pending_membership_audit = Some(MembershipAuditEvent {
                    id: Uuid::now_v7(),
                    repository_id: record.id,
                    actor: MembershipActor {
                        issuer: "urn:crab:local".into(),
                        subject: "operator".into(),
                    },
                    previous_members_digest: membership_digest(&[])?,
                    new_members_digest: membership_digest(&record.members)?,
                    repository_version: document.version,
                    occurred_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(|_| CatalogError::Invalid("system clock is before epoch"))?
                        .as_secs(),
                    previous: document.membership_audit_head.clone(),
                });
            }
            document.validate(&self.root)?;
            if self.write_document(&document, etag).await? {
                let _ = self.flush_membership_audit().await;
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

    fn actor() -> MembershipActor {
        MembershipActor {
            issuer: "https://issuer.example".into(),
            subject: "alice-id".into(),
        }
    }

    fn admin() -> RepositoryMember {
        RepositoryMember {
            subject: "alice-id".into(),
            name: "Alice".into(),
            access: crate::RepositoryAccess::Admin,
        }
    }

    #[tokio::test]
    async fn v2_first_membership_write_migrates_and_preserves_repository() {
        let catalog = catalog();
        let original = catalog
            .create_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                "description".into(),
                vec![],
            )
            .await
            .unwrap();
        let (mut document, _) = catalog.load().await.unwrap();
        document.schema_version = 2;
        catalog
            .root
            .store
            .put_overwrite(
                &catalog.path,
                Bytes::from(serde_json::to_vec(&document).unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(catalog.load().await.unwrap().0, document);
        let changed = catalog
            .replace_members(
                "team",
                "project",
                document.version,
                vec![admin()],
                actor(),
                true,
            )
            .await
            .unwrap();
        let (after, _) = catalog.load().await.unwrap();
        assert_eq!(after.schema_version, 3);
        assert_eq!(
            changed,
            CatalogRecord {
                members: vec![admin()],
                ..original
            }
        );
    }

    #[tokio::test]
    async fn membership_requires_current_revision_and_final_admin_without_writes() {
        let catalog = catalog();
        catalog
            .create_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                String::new(),
                vec![admin()],
            )
            .await
            .unwrap();
        let (before, etag) = catalog.load().await.unwrap();
        assert!(matches!(
            catalog
                .replace_members(
                    "team",
                    "project",
                    before.version - 1,
                    vec![admin()],
                    actor(),
                    true
                )
                .await,
            Err(CatalogError::Conflict)
        ));
        assert!(matches!(
            catalog
                .replace_members("team", "project", before.version, vec![], actor(), true)
                .await,
            Err(CatalogError::Invalid(
                "at least one administrator is required"
            ))
        ));
        assert_eq!(catalog.load().await.unwrap(), (before, etag));
    }

    #[tokio::test]
    async fn pending_audit_survives_crash_and_later_mutation_keeps_chain() {
        let catalog = catalog();
        let record = catalog
            .create_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                String::new(),
                vec![admin()],
            )
            .await
            .unwrap();
        let (mut document, etag) = catalog.load().await.unwrap();
        let head = document.membership_audit_head.clone().unwrap();
        let (body, _) = catalog
            .root
            .store
            .get_with_etag_bounded(&head.clone().into(), 8192)
            .await
            .unwrap();
        let event: MembershipAuditEvent = serde_json::from_slice(&body).unwrap();
        // Recreate the committed outbox state immediately before its flush.
        catalog.root.store.delete(&head.into()).await.unwrap();
        document.membership_audit_head = None;
        document.pending_membership_audit = Some(event.clone());
        assert!(catalog.write_document(&document, etag).await.unwrap());
        let mut member = admin();
        member.name = "Alice renamed".into();
        catalog
            .replace_members(
                "team",
                "project",
                document.version,
                vec![member],
                actor(),
                true,
            )
            .await
            .unwrap();
        catalog.flush_membership_audit().await.unwrap();
        let (after, etag) = catalog.load().await.unwrap();
        catalog.flush_membership_audit().await.unwrap();
        assert_eq!(catalog.load().await.unwrap(), (after.clone(), etag));
        let (body, _) = catalog
            .root
            .store
            .get_with_etag_bounded(&after.membership_audit_head.unwrap().into(), 8192)
            .await
            .unwrap();
        let current: MembershipAuditEvent = serde_json::from_slice(&body).unwrap();
        assert_eq!(current.repository_id, record.id);
        let (body, _) = catalog
            .root
            .store
            .get_with_etag_bounded(&current.previous.unwrap().into(), 8192)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<MembershipAuditEvent>(&body).unwrap(),
            event
        );
    }

    #[test]
    fn membership_digest_is_independent_of_presentation_order() {
        let first = admin();
        let second = RepositoryMember {
            subject: "bob-id".into(),
            name: "Bob".into(),
            access: crate::RepositoryAccess::Read,
        };
        assert_eq!(
            membership_digest(&[first.clone(), second.clone()]).unwrap(),
            membership_digest(&[second, first]).unwrap()
        );
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
        assert_eq!(
            document.repositories[0].application,
            RepositoryApplicationState::EmptyCellPending
        );
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
    async fn adopted_repository_requires_empty_cell_and_ready_transition_is_idempotent() {
        let catalog = catalog();
        let layout = StoreLayout::new(
            catalog.root.store.clone(),
            catalog.root.repository_prefix("team/project").unwrap(),
        );
        crab_write::initialize::initialize_repository(
            &catalog.root.store,
            &layout,
            "refs/heads/main",
        )
        .await
        .unwrap();
        let runtime = catalog
            .adopt_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                String::new(),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.application,
            RepositoryApplicationState::EmptyCellPending
        );

        let ready = catalog.mark_cell_ready(runtime.id).await.unwrap();
        let repeated = catalog.mark_cell_ready(runtime.id).await.unwrap();
        let (document, _) = catalog.load().await.unwrap();

        assert_eq!(ready.application, RepositoryApplicationState::CellReady);
        assert_eq!(repeated, ready);
        assert_eq!(document.version, 2);
    }

    #[tokio::test]
    async fn legacy_catalog_is_rejected_at_the_hard_cut() {
        let catalog = catalog();
        let body = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "version": 4,
            "repositories": [{
                "id": "00000000-0000-0000-0000-000000000007",
                "owner": "team",
                "name": "project",
                "prefix": "team/project",
                "placement_generation": 1
            }]
        }))
        .unwrap();
        catalog
            .root
            .store
            .put_overwrite(&catalog.path, Bytes::from(body))
            .await
            .unwrap();

        assert!(matches!(
            catalog.load().await,
            Err(CatalogError::Invalid("unsupported catalog schema"))
        ));
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
    async fn exclusive_create_rejects_an_existing_repository() {
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
        let result = catalog
            .create_repository_exclusive(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                String::new(),
                vec![],
            )
            .await;
        assert!(matches!(result, Err(CatalogError::Conflict)));
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
            .set_members("TEAM", "PROJECT", members.clone(), true)
            .await
            .unwrap();
        let (after_update, _) = catalog.load().await.unwrap();
        let repeated = catalog
            .set_members("team", "project", members, true)
            .await
            .unwrap();
        let (after_repeat, _) = catalog.load().await.unwrap();

        assert_eq!(updated, repeated);
        assert_eq!(after_update.version, 2);
        assert_eq!(after_repeat, after_update);
    }

    #[tokio::test]
    async fn membership_replacement_requires_a_cataloged_repository() {
        let result = catalog()
            .set_members("team", "missing", vec![], false)
            .await;

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
                true,
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
