mod read;
mod rename;
pub(crate) use rename::Rename;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

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
    #[error("repository destination is already in use")]
    NameUnavailable,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<Uuid>,
    pub name: String,
    pub prefix: String,
    pub placement_generation: u64,
    pub application: RepositoryApplicationState,
    pub description: String,
    pub members: Vec<RepositoryMember>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<RepositoryGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) organization: Option<OrganizationAccess>,
    pub protected_branches: Vec<BranchProtection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    naming: Option<rename::Naming>,
}

pub(crate) const MAX_TEAM_GRANTS: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct OrganizationAccess {
    pub id: Uuid,
    pub teams: Vec<TeamGrant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct TeamGrant {
    pub team: Uuid,
    pub access: crate::RepositoryAccess,
}

impl OrganizationAccess {
    pub(crate) fn validate(&self) -> Result<(), CatalogError> {
        let mut unique = HashSet::new();
        if self.id.is_nil()
            || self.teams.len() > MAX_TEAM_GRANTS
            || self.teams.iter().any(|grant| {
                grant.team.is_nil()
                    || grant.access == crate::RepositoryAccess::Admin
                    || !unique.insert(grant.team)
            })
        {
            return Err(CatalogError::Invalid(
                "team grants need a valid organization and at most 100 unique teams with read, triage, or write access",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryApplicationState {
    EmptyCellPending,
    GitImportPending,
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

/// Revision-bound replacement of direct members and OIDC group grants.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
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
    #[serde(skip)]
    pub(crate) renamed: bool,
    pub members: Vec<RepositoryMember>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<RepositoryGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) organization: Option<OrganizationAccess>,
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
    pub(crate) fn membership_snapshot(
        &self,
        record: &CatalogRecord,
    ) -> Result<MembershipSnapshot, CatalogError> {
        let source = record.access_record(
            record
                .forked_from
                .and_then(|id| self.repositories.iter().find(|source| source.id == id)),
        )?;
        Ok(record.access_snapshot(self.version, source))
    }

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
            if record.membership_digest()? != event.new_members_digest {
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
        let records: HashMap<_, _> = self
            .repositories
            .iter()
            .map(|record| (record.id, record))
            .collect();
        if records.len() != self.repositories.len() {
            return Err(CatalogError::Invalid("repository IDs must be unique"));
        }
        let mut names = HashSet::new();
        let mut prefixes = HashSet::new();
        for record in &self.repositories {
            if record.placement_generation == 0
                || !names.insert((record.owner.to_lowercase(), record.name.to_lowercase()))
                || !prefixes.insert(record.prefix.clone())
            {
                return Err(CatalogError::Invalid(
                    "repository IDs, names, prefixes, and placement generations must be valid and unique",
                ));
            }
            if let Some(upstream) = record.forked_from {
                record.access_record(records.get(&upstream).copied())?;
                if !record.members.is_empty()
                    || !record.groups.is_empty()
                    || record
                        .organization
                        .as_ref()
                        .is_some_and(|access| !access.teams.is_empty())
                {
                    return Err(CatalogError::Invalid(
                        "forks must inherit repository access",
                    ));
                }
            }
            let runtime = record.runtime_config(root, "main")?;
            if let Some(organization) = &record.organization {
                organization.validate()?;
            }
            crate::config::validate_repository(&runtime)
                .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
        }
        self.validate_renames(&mut names)?;
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

pub(crate) fn personal_namespace(issuer: &str, subject: &str) -> String {
    let identity = format!("{issuer}\0{subject}");
    let digest = blake3::derive_key("crab personal repository namespace v1", identity.as_bytes());
    let hex = blake3::Hash::from_bytes(digest).to_hex();
    format!("user-{}", &hex[..32])
}

pub(crate) fn is_personal_namespace(owner: &str) -> bool {
    owner
        .to_ascii_lowercase()
        .strip_prefix("user-")
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

pub(crate) fn organization_namespace(id: Uuid) -> String {
    format!("org-{}", id.simple())
}

pub(crate) fn is_organization_namespace(owner: &str) -> bool {
    owner
        .to_ascii_lowercase()
        .strip_prefix("org-")
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

pub(crate) enum BrowserNamespace {
    Personal(String),
    Organization(Uuid),
}

impl BrowserNamespace {
    fn catalog_owner(
        self,
        members: &[RepositoryMember],
    ) -> Result<(String, Option<OrganizationAccess>), CatalogError> {
        match self {
            Self::Personal(owner) => Ok((owner, None)),
            Self::Organization(id) if members.is_empty() => Ok((
                organization_namespace(id),
                Some(OrganizationAccess {
                    id,
                    teams: Vec::new(),
                }),
            )),
            Self::Organization(_) => Err(CatalogError::Invalid(
                "organization repositories inherit their initial administrators",
            )),
        }
    }
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
    organization: Option<&OrganizationAccess>,
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
    if let Some(organization) = organization {
        let mut canonical = organization.clone();
        canonical.teams.sort_by_key(|grant| grant.team);
        hasher.update(b"organization\0");
        hasher.update(&serde_json::to_vec(&canonical)?);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

impl CatalogRecord {
    pub fn organization_id(&self) -> Option<Uuid> {
        self.organization.as_ref().map(|access| access.id)
    }

    pub(crate) fn access_record<'a>(
        &'a self,
        source: Option<&'a Self>,
    ) -> Result<&'a Self, CatalogError> {
        let Some(upstream) = self.forked_from else {
            return Ok(self);
        };
        // A fork stays in its source organization and shares its live ACL. A
        // reused route or another fork cannot become its authority.
        source
            .filter(|source| {
                source.id == upstream
                    && source.forked_from.is_none()
                    && source.organization.is_some()
                    && source.organization_id() == self.organization_id()
                    && source.application == RepositoryApplicationState::CellReady
            })
            .ok_or(CatalogError::Invalid(
                "fork source is unavailable in its organization",
            ))
    }

    pub(crate) fn access_snapshot(&self, revision: u64, source: &Self) -> MembershipSnapshot {
        MembershipSnapshot {
            repository_id: self.id,
            revision,
            renamed: self.name_version() > 0,
            organization: source.organization.clone(),
            members: source.members.clone(),
            groups: source.groups.clone(),
        }
    }

    fn membership_digest(&self) -> Result<String, CatalogError> {
        membership_digest(&self.members, &self.groups, self.organization.as_ref())
    }

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
struct CachedCatalog {
    document: Arc<CatalogDocument>,
    etag: ETag,
    size: u64,
}

#[derive(Clone)]
pub struct CatalogStore {
    root: StorageRoot,
    path: object_store::path::Path,
    latest: Arc<RwLock<Option<CachedCatalog>>>,
}

enum InsertMode {
    ExistingPrefix,
    Reserved(MembershipActor),
}

impl CatalogStore {
    pub(crate) fn new(root: StorageRoot) -> Self {
        let path = root.path(CATALOG_RELATIVE_PATH);
        Self {
            root,
            path,
            latest: Arc::new(RwLock::new(None)),
        }
    }

    /// Open the catalog configured for this server deployment.
    pub fn from_config(config: &crate::Config) -> crate::Result<Self> {
        config.validate()?;
        Ok(Self::new(StorageRoot::build(&config.storage)?))
    }

    pub(crate) fn root(&self) -> &StorageRoot {
        &self.root
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
        if is_organization_namespace(&owner) {
            return Err(CatalogError::Invalid(
                "organization namespaces require organization authority",
            ));
        }
        let record = CatalogRecord {
            id: Uuid::now_v7(),
            owner,
            organization: None,
            forked_from: None,
            name,
            prefix,
            placement_generation: 1,
            application: RepositoryApplicationState::EmptyCellPending,
            description,
            members,
            groups: Vec::new(),
            protected_branches: Vec::new(),
            naming: None,
        };
        self.initialize_git(&record, &default_branch, false).await?;
        self.insert_with_mode(record, InsertMode::ExistingPrefix)
            .await
    }

    pub(crate) async fn reserve_browser_repository(
        &self,
        namespace: BrowserNamespace,
        name: String,
        description: String,
        request: Uuid,
        actor: MembershipActor,
        members: Vec<RepositoryMember>,
    ) -> Result<CatalogRecord, CatalogError> {
        let (owner, organization) = namespace.catalog_owner(&members)?;
        let mut hash = blake3::Hasher::new_derive_key("crab browser repository creation v1");
        hash.update(owner.as_bytes());
        hash.update(request.as_bytes());
        let mut id = [0; 16];
        id.copy_from_slice(&hash.finalize().as_bytes()[..16]);
        let id = uuid::Builder::from_custom_bytes(id).into_uuid();
        self.insert_with_mode(
            CatalogRecord {
                id,
                owner,
                organization,
                forked_from: None,
                name,
                prefix: format!("browser-repositories/{id}"),
                placement_generation: 1,
                application: RepositoryApplicationState::EmptyCellPending,
                description,
                members,
                groups: Vec::new(),
                protected_branches: Vec::new(),
                naming: None,
            },
            InsertMode::Reserved(actor),
        )
        .await
    }

    pub(crate) async fn reserve_git_import(
        &self,
        id: Uuid,
        namespace: BrowserNamespace,
        name: String,
        description: String,
        actor: MembershipActor,
        members: Vec<RepositoryMember>,
        forked_from: Option<Uuid>,
    ) -> Result<CatalogRecord, CatalogError> {
        let (owner, organization) = namespace.catalog_owner(&members)?;
        self.insert_with_mode(
            CatalogRecord {
                id,
                owner,
                organization,
                forked_from,
                name,
                prefix: format!("git-imports/{id}"),
                placement_generation: 1,
                application: RepositoryApplicationState::GitImportPending,
                description,
                members,
                groups: Vec::new(),
                protected_branches: Vec::new(),
                naming: None,
            },
            InsertMode::Reserved(actor),
        )
        .await
    }

    pub(crate) async fn initialize_git(
        &self,
        record: &CatalogRecord,
        default_branch: &str,
        require_identity: bool,
    ) -> Result<(), CatalogError> {
        let runtime = record.runtime_config(&self.root, default_branch)?;
        crate::config::validate_repository(&runtime)
            .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
        let layout = self.root.repository_layout(runtime.prefix.clone());
        let id = blake3::hash(record.id.as_bytes()).to_hex();
        let root = crab_write::capsule_protocol::initialize(
            &layout,
            id.as_ref(),
            &format!("refs/heads/{default_branch}"),
        )
        .await?;
        if require_identity && root.record().root().repository_id() != id.as_str() {
            return Err(CatalogError::Invalid(
                "reserved Git prefix contains another repository",
            ));
        }
        Ok(())
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
            organization: None,
            forked_from: None,
            name,
            prefix,
            placement_generation: 1,
            application: RepositoryApplicationState::EmptyCellPending,
            description,
            members,
            groups: Vec::new(),
            protected_branches: Vec::new(),
            naming: None,
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
        self.edit_access(owner, name, input.expected_revision, actor, |record| {
            record.members = input.members;
            record.groups = input.groups;
            crate::config::validate_repository(&record.runtime_config(&self.root, "main")?)
                .map_err(|_| CatalogError::Invalid("repository record failed validation"))?;
            if require_administrator
                && record.organization.is_none()
                && !record
                    .members
                    .iter()
                    .any(|member| member.access == crate::RepositoryAccess::Admin)
            {
                return Err(CatalogError::Invalid(
                    "at least one administrator is required",
                ));
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn set_organization_access(
        &self,
        owner: &str,
        name: &str,
        repository: Uuid,
        expected_revision: u64,
        mut organization: OrganizationAccess,
        actor: MembershipActor,
    ) -> Result<CatalogRecord, CatalogError> {
        organization.validate()?;
        organization.teams.sort_by_key(|grant| grant.team);
        self.edit_access(owner, name, expected_revision, actor, |record| {
            if record.id != repository {
                return Err(CatalogError::Conflict);
            }
            if record
                .organization
                .as_ref()
                .is_some_and(|current| current.id != organization.id)
            {
                return Err(CatalogError::Invalid(
                    "repository already belongs to another organization",
                ));
            }
            record.organization = Some(organization);
            Ok(())
        })
        .await
    }

    async fn edit_access(
        &self,
        owner: &str,
        name: &str,
        expected_revision: u64,
        actor: MembershipActor,
        edit: impl FnOnce(&mut CatalogRecord) -> Result<(), CatalogError>,
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
        if record.forked_from.is_some() {
            return Err(CatalogError::Invalid(
                "manage fork access on the source repository",
            ));
        }
        let previous = record.clone();
        edit(record)?;
        if record == &previous {
            return Ok(previous);
        }
        let previous_digest = previous.membership_digest()?;
        let new_digest = record.membership_digest()?;
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

    pub(crate) async fn needs_cell_initialization(
        &self,
        repository: Uuid,
        pending: RepositoryApplicationState,
    ) -> Result<bool, CatalogError> {
        let (document, _) = self.load().await?;
        let state = document
            .repositories
            .iter()
            .find(|record| record.id == repository)
            .ok_or(CatalogError::NotFound)?
            .application;
        if state == RepositoryApplicationState::CellReady {
            return Ok(false);
        }
        if state != pending {
            return Err(CatalogError::Invalid(
                "repository is reserved by another initialization workflow",
            ));
        }
        Ok(true)
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
        self.insert_with_mode(record, InsertMode::ExistingPrefix)
            .await
    }

    async fn insert_with_mode(
        &self,
        record: CatalogRecord,
        mode: InsertMode,
    ) -> Result<CatalogRecord, CatalogError> {
        for _ in 0..MAX_CAS_ATTEMPTS {
            let (mut document, etag) = self.load_for_mutation().await?;
            if let Some(existing) = document.repositories.iter().find(|existing| {
                existing.owner.eq_ignore_ascii_case(&record.owner)
                    && existing.name.eq_ignore_ascii_case(&record.name)
            }) {
                let same = existing.organization_id() == record.organization_id()
                    && existing.forked_from == record.forked_from
                    && match &mode {
                        InsertMode::ExistingPrefix => existing.prefix == record.prefix,
                        InsertMode::Reserved(_) => {
                            existing.id == record.id
                                && existing.prefix == record.prefix
                                && existing.owner == record.owner
                                && existing.name == record.name
                                && existing.description == record.description
                        }
                    };
                return if same {
                    Ok(existing.clone())
                } else {
                    Err(CatalogError::Conflict)
                };
            }
            if is_organization_namespace(&record.owner) {
                if record.organization.is_none() {
                    return Err(CatalogError::Invalid(
                        "organization namespaces require organization authority",
                    ));
                }
                // A lookalike catalog owner from before managed accounts keeps
                // its original access. It cannot share a managed namespace.
                if document.repositories.iter().any(|existing| {
                    existing.owner.eq_ignore_ascii_case(&record.owner)
                        && existing.organization_id() != record.organization_id()
                }) {
                    return Err(CatalogError::Conflict);
                }
            }
            if document
                .repositories
                .iter()
                .any(|existing| existing.id == record.id || existing.prefix == record.prefix)
            {
                return Err(CatalogError::Conflict);
            }
            document.version = document
                .version
                .checked_add(1)
                .ok_or(CatalogError::Invalid("catalog version overflowed"))?;
            document.release_alias(&record.owner, &record.name);
            document.repositories.push(record.clone());
            document.normalize();
            if !record.members.is_empty() || record.organization.is_some() {
                document.pending_membership_audit = Some(MembershipAuditEvent {
                    id: Uuid::now_v7(),
                    repository_id: record.id,
                    actor: match &mode {
                        InsertMode::Reserved(actor) => actor.clone(),
                        _ => MembershipActor {
                            issuer: "urn:crab:local".into(),
                            subject: "operator".into(),
                        },
                    },
                    previous_members_digest: membership_digest(&[], &[], None)?,
                    new_members_digest: record.membership_digest()?,
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
