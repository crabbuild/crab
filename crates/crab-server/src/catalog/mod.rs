use std::collections::HashSet;

use bytes::Bytes;
use crab_storage::{ETag, StorageError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BranchProtection, RepositoryConfig, RepositoryGroup, RepositoryMember,
    storage_root::StorageRoot,
};

const SCHEMA_VERSION: u32 = 3;
const MAX_CATALOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_REPOSITORIES: usize = 10_000;
const MAX_CAS_ATTEMPTS: usize = 8;
const CATALOG_RELATIVE_PATH: &str = ".crab/http-server/v1/catalog.json";
const MEMBERSHIP_AUDIT_RELATIVE_PREFIX: &str = ".crab/http-server/v1/audit/membership";

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
    #[error("repository content validation failed")]
    Read(#[from] crab_read::ReadError),
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
    pub description: String,
    pub members: Vec<RepositoryMember>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<RepositoryGroup>,
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

/// Revision-bound replacement of the complete repository access list.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessUpdate {
    pub expected_revision: u64,
    pub members: Vec<RepositoryMember>,
    pub groups: Vec<RepositoryGroup>,
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
    pub repository_id: Uuid,
    pub revision: u64,
    pub members: Vec<RepositoryMember>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<RepositoryGroup>,
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
        if self.schema_version != SCHEMA_VERSION || self.repositories.len() > MAX_REPOSITORIES {
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
            if membership_digest(&record.members, &record.groups)? != event.new_members_digest {
                return Err(CatalogError::Invalid(
                    "membership audit digest does not match",
                ));
            }
        }
        if self
            .membership_audit_head
            .as_ref()
            .is_some_and(|path| !valid_audit_path(path))
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

fn valid_audit_path(path: &str) -> bool {
    let prefix = format!("{MEMBERSHIP_AUDIT_RELATIVE_PREFIX}/");
    let Some(file) = path
        .strip_prefix(&prefix)
        .and_then(|path| path.strip_suffix(".json"))
    else {
        return false;
    };
    let Some((version, id)) = file.split_once('-') else {
        return false;
    };
    version.parse::<u64>().is_ok_and(|value| value > 0) && Uuid::parse_str(id).is_ok()
}

fn membership_digest(
    members: &[RepositoryMember],
    groups: &[RepositoryGroup],
) -> Result<String, CatalogError> {
    let mut canonical = members.to_vec();
    canonical.sort_by(|a, b| (&a.subject, &a.name, a.access).cmp(&(&b.subject, &b.name, b.access)));
    let bytes = serde_json::to_vec(&canonical)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab membership digest v1");
    hasher.update(&bytes);
    if !groups.is_empty() {
        let mut groups = groups.to_vec();
        groups.sort_by(|a, b| a.group.cmp(&b.group));
        hasher.update(b"oidc-groups\0");
        hasher.update(&serde_json::to_vec(&groups)?);
    }
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
            groups: self.groups.clone(),
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
        if schema.schema_version != SCHEMA_VERSION {
            return Err(CatalogError::Invalid("unsupported catalog schema"));
        }
        let document: CatalogDocument = serde_json::from_slice(&body)?;
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
            repository_id: record.id,
            revision: document.version,
            members: record.members.clone(),
            groups: record.groups.clone(),
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
            groups: Vec::new(),
            protected_branches: Vec::new(),
        };
        let runtime = record.runtime_config(&self.root, &default_branch)?;
        crate::config::validate_repository(&runtime)
            .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
        let layout = self.root.repository_layout(runtime.prefix.clone());
        crab_write::capsule_protocol::initialize(
            &layout,
            blake3::hash(record.id.as_bytes()).to_hex().as_ref(),
            &format!("refs/heads/{default_branch}"),
        )
        .await?;
        crab_metadata::capsule_protocol::load_root(&layout).await?;
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
            groups: Vec::new(),
            protected_branches: Vec::new(),
        };
        let runtime = record.runtime_config(&self.root, "main")?;
        crate::config::validate_repository(&runtime)
            .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
        let layout = self.root.repository_layout(runtime.prefix);
        let view = crab_read::capsule_protocol::open_view(
            &layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 2 * 1024 * 1024 * 1024,
                max_frontier_bytes: 2 * 1024 * 1024 * 1024,
            },
        )
        .await?;
        crab_read::capsule_protocol::verify_reachable_dependencies(
            &layout,
            &view,
            crab_read::capsule_protocol::CapsuleDependencyLimits {
                max_git_bytes: crate::integrity::MAX_DEPENDENCY_GIT_BYTES,
                pointer_scan: crab_git::walk::PointerScanLimits {
                    objects: 2_000_000,
                    lookups: 8_000_000,
                    allocation_bytes: 64 * 1024 * 1024,
                },
            },
            &tokio_util::sync::CancellationToken::new(),
        )
        .await?;
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
        // set-members edits people only. Keep groups under the same revision
        // so this command cannot erase an independently updated SSO access list.
        self.replace_access(
            owner,
            name,
            AccessUpdate {
                expected_revision: snapshot.revision,
                members,
                groups: snapshot.groups,
            },
            MembershipActor {
                issuer: "urn:crab:local".into(),
                subject: "operator".into(),
            },
            require_administrator,
        )
        .await
    }

    pub async fn replace_access(
        &self,
        owner: &str,
        name: &str,
        input: AccessUpdate,
        actor: MembershipActor,
        require_administrator: bool,
    ) -> Result<CatalogRecord, CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if document.version != input.expected_revision {
            return Err(CatalogError::Conflict);
        }
        let Some(record) = document.repositories.iter_mut().find(|record| {
            record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
        }) else {
            return Err(CatalogError::NotFound);
        };
        let mut candidate = record.clone();
        candidate.members = input.members.clone();
        candidate.groups = input.groups.clone();
        crate::config::validate_repository(&candidate.runtime_config(&self.root, "main")?)
            .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
        if require_administrator
            && !input
                .members
                .iter()
                .any(|member| member.access == crate::RepositoryAccess::Admin)
        {
            return Err(CatalogError::Invalid(
                "at least one administrator is required",
            ));
        }
        if record.members == input.members && record.groups == input.groups {
            return Ok(record.clone());
        }
        let previous_digest = membership_digest(&record.members, &record.groups)?;
        let new_digest = membership_digest(&input.members, &input.groups)?;
        record.members = input.members;
        record.groups = input.groups;
        let updated = record.clone();
        document.version = document
            .version
            .checked_add(1)
            .ok_or(CatalogError::Invalid("catalog version overflowed"))?;
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
        let relative_path = format!(
            "{MEMBERSHIP_AUDIT_RELATIVE_PREFIX}/{}-{}.json",
            event.repository_version, event.id
        );
        let path = self.root.path(&relative_path);
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
        // Catalog roots are portable backup units. Persisting a root-qualified
        // key here would make an otherwise byte-exact restore prefix-dependent.
        document.membership_audit_head = Some(relative_path);
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
                    previous_members_digest: membership_digest(&[], &[])?,
                    new_members_digest: membership_digest(&record.members, &record.groups)?,
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
mod tests;
