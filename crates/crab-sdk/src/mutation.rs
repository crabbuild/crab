use serde::{Deserialize, Serialize};
use std::{fmt, pin::Pin};
use tokio::io::AsyncRead;

use crate::{EntryMode, Error, ErrorKind, GitPath, ObjectId, RepositoryLocator, Result};

const MAX_TOKEN_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_REF_UPDATES: usize = 1024;
pub(crate) const MAX_GIT_OBJECT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IDENTITY_COMPONENT_BYTES: usize = 1024;

/// Policy for updates with an exact expected old object identity.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WritePolicy {
    #[default]
    FastForward,
    ForceWithLease,
}

/// Exact identity and minute-resolution timestamp used to create a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitIdentity {
    name: String,
    email: String,
    seconds: i64,
    offset_minutes: i16,
}

impl CommitIdentity {
    /// Validate an identity without applying Git configuration or normalization.
    pub fn new(
        name: impl Into<String>,
        email: impl Into<String>,
        seconds: i64,
        offset_minutes: i16,
    ) -> Result<Self> {
        let identity = Self {
            name: name.into(),
            email: email.into(),
            seconds,
            offset_minutes,
        };
        if identity.name.is_empty()
            || identity.email.is_empty()
            || identity.name.len() > MAX_IDENTITY_COMPONENT_BYTES
            || identity.email.len() > MAX_IDENTITY_COMPONENT_BYTES
            || [&identity.name, &identity.email].iter().any(|value| {
                value
                    .bytes()
                    .any(|byte| matches!(byte, b'<' | b'>' | b'\n' | b'\r' | 0))
            })
            || offset_minutes.unsigned_abs() > 99 * 60 + 59
        {
            return Err(invalid("invalid Git commit identity"));
        }
        Ok(identity)
    }

    pub(crate) fn owner(&self) -> crab_remote::objects::Signature<'_> {
        crab_remote::objects::Signature {
            name: &self.name,
            email: &self.email,
            seconds: self.seconds,
            offset_minutes: self.offset_minutes,
        }
    }

    /// Return the exact author or committer name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the exact author or committer email.
    #[must_use]
    pub fn email(&self) -> &str {
        &self.email
    }

    #[cfg(feature = "local")]
    pub(crate) fn git_date(&self) -> String {
        let sign = if self.offset_minutes < 0 { '-' } else { '+' };
        let offset = self.offset_minutes.unsigned_abs();
        format!(
            "@{} {sign}{:02}{:02}",
            self.seconds,
            offset / 60,
            offset % 60
        )
    }
}

pub(crate) enum FileChange {
    Delete,
    Content {
        mode: EntryMode,
        size: u64,
        hydrated: bool,
        reader: Pin<Box<dyn AsyncRead + Send + 'static>>,
    },
}

/// One non-overlapping file replacement or deletion for a remote commit.
pub struct FileEdit {
    pub(crate) path: GitPath,
    pub(crate) change: FileChange,
}

impl fmt::Debug for FileEdit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let change = match &self.change {
            FileChange::Delete => "delete",
            FileChange::Content { hydrated: true, .. } => "hydrated",
            FileChange::Content { .. } => "git",
        };
        formatter
            .debug_struct("FileEdit")
            .field("path", &self.path)
            .field("change", &change)
            .finish()
    }
}

impl FileEdit {
    /// Delete an existing path from the base tree.
    pub fn delete(path: GitPath) -> Result<Self> {
        if path.is_root() {
            return Err(invalid("file edit path cannot be the repository root"));
        }
        path.owner()?;
        Ok(Self {
            path,
            change: FileChange::Delete,
        })
    }

    /// Store an exact-size stream directly as a Git blob.
    pub fn git(
        path: GitPath,
        mode: EntryMode,
        size: u64,
        reader: impl AsyncRead + Send + 'static,
    ) -> Result<Self> {
        Self::content(path, mode, size, false, reader)
    }

    /// Store an exact-size logical stream as native Crab content and commit its pointer.
    pub fn hydrated(
        path: GitPath,
        mode: EntryMode,
        size: u64,
        reader: impl AsyncRead + Send + 'static,
    ) -> Result<Self> {
        if matches!(mode, EntryMode::Symlink) {
            return Err(invalid("hydrated edits cannot use symlink mode"));
        }
        Self::content(path, mode, size, true, reader)
    }

    fn content(
        path: GitPath,
        mode: EntryMode,
        size: u64,
        hydrated: bool,
        reader: impl AsyncRead + Send + 'static,
    ) -> Result<Self> {
        if path.is_root() {
            return Err(invalid("file edit path cannot be the repository root"));
        }
        path.owner()?;
        mode.tree_mode()?;
        Ok(Self {
            path,
            change: FileChange::Content {
                mode,
                size,
                hydrated,
                reader: Box::pin(reader),
            },
        })
    }
}

/// Validated metadata and atomic branch condition for one remote commit.
#[derive(Clone, Debug)]
pub struct CommitOptions {
    base: Option<ObjectId>,
    branch: String,
    expected: Option<ObjectId>,
    author: CommitIdentity,
    committer: CommitIdentity,
    message: Vec<u8>,
    policy: WritePolicy,
}

impl CommitOptions {
    /// Create options for a branch update, or branch creation when expected is None.
    pub fn new(
        base: ObjectId,
        branch: &str,
        expected: Option<ObjectId>,
        author: CommitIdentity,
        committer: CommitIdentity,
        message: impl Into<Vec<u8>>,
    ) -> Result<Self> {
        validate_commit_branch(branch)?;
        let options = Self {
            base: Some(base),
            branch: branch.to_owned(),
            expected,
            author,
            committer,
            message: message.into(),
            policy: WritePolicy::FastForward,
        };
        options.validate_size(MAX_GIT_OBJECT_BYTES)?;
        Ok(options)
    }

    /// Create the first commit and branch in an initialized empty repository.
    pub fn initial(
        branch: &str,
        author: CommitIdentity,
        committer: CommitIdentity,
        message: impl Into<Vec<u8>>,
    ) -> Result<Self> {
        validate_commit_branch(branch)?;
        let options = Self {
            base: None,
            branch: branch.to_owned(),
            expected: None,
            author,
            committer,
            message: message.into(),
            policy: WritePolicy::FastForward,
        };
        options.validate_size(MAX_GIT_OBJECT_BYTES)?;
        Ok(options)
    }

    /// Allow a branch rewrite while retaining the exact expected old identity.
    pub fn with_policy(mut self, policy: WritePolicy) -> Result<Self> {
        if matches!(policy, WritePolicy::ForceWithLease) && self.expected.is_none() {
            return Err(invalid(
                "force-with-lease requires an expected old identity",
            ));
        }
        self.policy = policy;
        Ok(self)
    }

    pub(crate) fn branch(&self) -> &str {
        &self.branch
    }

    pub(crate) const fn base(&self) -> Option<ObjectId> {
        self.base
    }

    pub(crate) const fn expected(&self) -> Option<ObjectId> {
        self.expected
    }

    pub(crate) fn author(&self) -> &CommitIdentity {
        &self.author
    }

    pub(crate) fn committer(&self) -> &CommitIdentity {
        &self.committer
    }

    pub(crate) fn message(&self) -> &[u8] {
        &self.message
    }

    pub(crate) const fn policy(&self) -> WritePolicy {
        self.policy
    }

    pub(crate) fn validate_size(&self, maximum: u64) -> Result<()> {
        if self.encoded_size()? > maximum {
            return Err(Error::new(
                ErrorKind::LimitExceeded,
                "commit object exceeds operation limits",
            ));
        }
        Ok(())
    }

    pub(crate) fn encoded_size(&self) -> Result<u64> {
        let signature = |identity: &CommitIdentity| {
            identity
                .name
                .len()
                .checked_add(identity.email.len())
                .and_then(|size| size.checked_add(identity.seconds.to_string().len()))
                .and_then(|size| size.checked_add(11))
        };
        // SHA-1 tree and parent lines have fixed widths. Prefixes, the blank
        // separator and both signature bodies account for the remaining header.
        let fixed_header = if self.base.is_some() {
            112usize
        } else {
            64usize
        };
        let encoded = fixed_header
            .checked_add(signature(&self.author).ok_or_else(|| {
                Error::new(ErrorKind::LimitExceeded, "commit identity size overflow")
            })?)
            .and_then(|size| signature(&self.committer).and_then(|value| size.checked_add(value)))
            .and_then(|size| size.checked_add(self.message.len()))
            .ok_or_else(|| Error::new(ErrorKind::LimitExceeded, "commit size overflow"))?;
        u64::try_from(encoded).map_err(|source| {
            Error::with_source(ErrorKind::LimitExceeded, "commit size overflow", source)
        })
    }
}

fn validate_commit_branch(branch: &str) -> Result<()> {
    if !branch.starts_with("refs/heads/") {
        return Err(invalid(
            "remote commit target must be a fully qualified branch",
        ));
    }
    crab_git::refname::validate_push_refname(branch)
        .map(|_| ())
        .map_err(|source| {
            Error::with_source(ErrorKind::InvalidInput, "invalid branch name", source)
        })
}

/// A branch or tag edit with an explicit creation or expected old value.
#[derive(Clone, Debug, Serialize)]
pub struct RefUpdate {
    name: String,
    old: Option<String>,
    new: Option<String>,
}

impl<'de> Deserialize<'de> for RefUpdate {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            name: String,
            old: Option<String>,
            new: Option<String>,
        }
        let Wire { name, old, new } = Wire::deserialize(deserializer)?;
        let value = Self { name, old, new };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

impl RefUpdate {
    /// Create a ref only if it is absent at publication.
    pub fn create(name: &str, target: ObjectId) -> Result<Self> {
        Self::new(name, None, Some(target))
    }

    /// Update a ref only if its current value matches the expected identity.
    pub fn update(name: &str, expected: ObjectId, target: ObjectId) -> Result<Self> {
        Self::new(name, Some(expected), Some(target))
    }

    /// Delete a ref only if its current value matches the expected identity.
    pub fn delete(name: &str, expected: ObjectId) -> Result<Self> {
        Self::new(name, Some(expected), None)
    }

    fn new(name: &str, old: Option<ObjectId>, new: Option<ObjectId>) -> Result<Self> {
        let value = Self {
            name: name.to_owned(),
            old: old.map(|oid| oid.to_string()),
            new: new.map(|oid| oid.to_string()),
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<()> {
        if !(self.name.starts_with("refs/heads/") || self.name.starts_with("refs/tags/"))
            || self.old == self.new
        {
            return Err(invalid(
                "ref edit requires a branch or tag and a changed value",
            ));
        }
        crab_git::refname::validate_push_refname(&self.name).map_err(|source| {
            Error::with_source(ErrorKind::InvalidInput, "invalid ref name", source)
        })?;
        for value in self.old.iter().chain(self.new.iter()) {
            let oid = ObjectId::from_hex(value)?;
            if oid.as_bytes() == &[0; 20] || oid.to_string() != *value {
                return Err(invalid(
                    "ref edit requires canonical nonzero SHA-1 identities",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn owner(&self) -> Result<crab_git::receive_plan::RefUpdate> {
        let oid = |value: &String| {
            ObjectId::from_hex(value).map(|oid| gix_hash::ObjectId::from(*oid.as_bytes()))
        };
        Ok(crab_git::receive_plan::RefUpdate {
            name: self.name.clone(),
            old: self.old.as_ref().map(oid).transpose()?,
            new: self.new.as_ref().map(oid).transpose()?,
        })
    }

    /// Return the fully qualified destination name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[cfg(any(feature = "local", feature = "managed"))]
    pub(crate) fn expected(&self) -> Result<Option<ObjectId>> {
        self.old.as_deref().map(ObjectId::from_hex).transpose()
    }

    #[cfg(feature = "managed")]
    pub(crate) fn target(&self) -> Result<Option<ObjectId>> {
        self.new.as_deref().map(ObjectId::from_hex).transpose()
    }
}

/// A nonempty atomic ref batch, defaulting to fast-forward updates.
#[derive(Clone, Debug, Serialize)]
pub struct RefBatch {
    edits: Vec<RefUpdate>,
    policy: WritePolicy,
}

impl<'de> Deserialize<'de> for RefBatch {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            edits: Vec<RefUpdate>,
            policy: WritePolicy,
        }
        let Wire { edits, policy } = Wire::deserialize(deserializer)?;
        let value = Self { edits, policy };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

impl RefBatch {
    /// Validate the edit count, unique destinations and exact expected identities.
    pub fn new(edits: Vec<RefUpdate>) -> Result<Self> {
        let batch = Self {
            edits,
            policy: WritePolicy::FastForward,
        };
        batch.validate()?;
        Ok(batch)
    }

    /// Allow rewrites while preserving every update's expected old identity.
    #[must_use]
    pub fn with_policy(mut self, policy: WritePolicy) -> Self {
        self.policy = policy;
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.edits.is_empty() || self.edits.len() > MAX_REF_UPDATES {
            return Err(invalid("atomic ref batch must contain 1 to 1024 edits"));
        }
        let mut names = std::collections::BTreeSet::new();
        for edit in &self.edits {
            edit.validate()?;
            if !names.insert(&edit.name) {
                return Err(invalid("duplicate ref destination"));
            }
        }
        Ok(())
    }

    pub(crate) fn edits(&self) -> &[RefUpdate] {
        &self.edits
    }
    pub(crate) fn policy(&self) -> WritePolicy {
        self.policy
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBinding {
    placement: [u8; 32],
    repository: String,
    refs: RefBatch,
    pack: Option<PackBinding>,
    content: Option<ContentBinding>,
    commit: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackBinding {
    pack_id: String,
    size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContentArtifactBinding {
    pub(crate) protocol_hash: [u8; 32],
    pub(crate) body_hash: [u8; 32],
    pub(crate) size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContentFileBinding {
    pub(crate) file_hash: [u8; 32],
    pub(crate) size: u64,
    pub(crate) shard_hash: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContentBinding {
    pub(crate) xorbs: Vec<ContentArtifactBinding>,
    pub(crate) shards: Vec<ContentArtifactBinding>,
    pub(crate) files: Vec<ContentFileBinding>,
}

impl ContentBinding {
    fn from_owner(content: &crab_remote::prepare::PreparedContent) -> Self {
        let artifact = |artifact: &crab_remote::prepare::ContentArtifact| ContentArtifactBinding {
            protocol_hash: *artifact.protocol_hash(),
            body_hash: *artifact.body_hash(),
            size: artifact.size(),
        };
        Self {
            xorbs: content.xorbs().iter().map(artifact).collect(),
            shards: content.shards().iter().map(artifact).collect(),
            files: content
                .files()
                .iter()
                .map(|file| ContentFileBinding {
                    file_hash: *file.file_hash(),
                    size: file.size(),
                    shard_hash: *file.shard_hash(),
                })
                .collect(),
        }
    }

    fn validate(&self) -> Result<()> {
        let valid_artifacts = |artifacts: &[ContentArtifactBinding]| {
            let mut identities = std::collections::BTreeSet::new();
            artifacts
                .iter()
                .all(|artifact| artifact.size != 0 && identities.insert(artifact.protocol_hash))
        };
        let shards = self
            .shards
            .iter()
            .map(|artifact| artifact.protocol_hash)
            .collect::<std::collections::BTreeSet<_>>();
        let mut files = std::collections::BTreeSet::new();
        if (self.xorbs.is_empty() && self.files.iter().any(|file| file.size != 0))
            || self.shards.is_empty()
            || self.files.is_empty()
            || !valid_artifacts(&self.xorbs)
            || !valid_artifacts(&self.shards)
            || self
                .files
                .iter()
                .any(|file| !files.insert(file.file_hash) || !shards.contains(&file.shard_hash))
        {
            return Err(invalid("recovery token contains invalid prepared content"));
        }
        Ok(())
    }
}

impl RequestBinding {
    fn validate(&self, backend: &RecoveryBackend) -> Result<()> {
        match backend {
            RecoveryBackend::Direct => {
                RepositoryLocator::new(&self.repository)?;
            }
            #[cfg(feature = "managed")]
            RecoveryBackend::Managed { push_id, finalize } => {
                if push_id.is_nil() || managed_locator(&self.repository)?.managed_owner().is_none()
                {
                    return Err(invalid("managed recovery identity is invalid"));
                }
                if finalize.repository_id.is_nil()
                    || finalize.ref_updates.len() != self.refs.edits.len()
                    || finalize
                        .ref_updates
                        .iter()
                        .zip(&self.refs.edits)
                        .any(|(wire, edit)| {
                            wire.ref_name != edit.name
                                || wire.old_oid != edit.old
                                || Some(wire.new_oid.as_str()) != edit.new.as_deref()
                        })
                {
                    return Err(invalid("managed finalize request is incomplete"));
                }
            }
        }
        self.refs.validate()?;
        if self.pack.as_ref().is_some_and(|pack| {
            pack.size == 0
                || pack.pack_id.len() != 64
                || !pack
                    .pack_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        }) {
            return Err(invalid("recovery token contains an invalid prepared pack"));
        }
        if let Some(content) = &self.content {
            content.validate()?;
            if self.pack.is_none() {
                return Err(invalid("prepared content requires a Git pack"));
            }
        }
        let Some(commit) = &self.commit else {
            return Ok(());
        };
        let oid = ObjectId::from_hex(commit)?;
        if oid.to_string() != *commit
            || self.pack.is_none()
            || self.refs.edits.len() != 1
            || !self.refs.edits[0].name.starts_with("refs/heads/")
            || self.refs.edits[0].new.as_deref() != Some(commit)
        {
            return Err(invalid(
                "recovery token commit does not match its prepared branch update",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenBinding {
    version: u32,
    backend: RecoveryBackend,
    operation_nonce: [u8; 16],
    request: RequestBinding,
    request_digest: [u8; 32],
    plan_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RecoveryBackend {
    Direct,
    #[cfg(feature = "managed")]
    Managed {
        push_id: uuid::Uuid,
        finalize: crab_auth::managed::PushFinalizeRequest,
    },
}

/// Versioned recovery identity bound to placement and the exact atomic request.
///
/// Contains no credentials and grants no authority. Persist before execution.
#[derive(Clone, Debug)]
pub struct RecoveryToken(TokenBinding);

impl RecoveryToken {
    pub(crate) fn new(
        placement: [u8; 32],
        repository: &RepositoryLocator,
        refs: RefBatch,
        pack: Option<&crab_remote::prepare::PackBinding>,
        content: Option<&crab_remote::prepare::PreparedContent>,
        commit: Option<ObjectId>,
    ) -> Result<Self> {
        let backend = RecoveryBackend::Direct;
        let request = RequestBinding {
            placement,
            repository: repository.direct_prefix()?.to_owned(),
            refs,
            pack: pack.map(|pack| PackBinding {
                pack_id: pack.pack_id().to_owned(),
                size: pack.size(),
            }),
            content: content.map(ContentBinding::from_owner),
            commit: commit.map(|oid| oid.to_string()),
        };
        request.validate(&backend)?;
        let digest = crab_metadata::receipts::publication_request_digest(&(&backend, &request))
            .map_err(metadata_error)?;
        let nonce = *uuid::Uuid::now_v7().as_bytes();
        let plan = crab_metadata::receipts::publication_plan_id(&digest, &nonce);
        let token = Self(TokenBinding {
            version: 4,
            backend,
            operation_nonce: nonce,
            request,
            request_digest: digest,
            plan_id: blake3::Hash::from(plan).to_hex().to_string(),
        });
        if token.to_json()?.len() > MAX_TOKEN_BYTES {
            return Err(invalid("recovery token exceeds 1 MiB"));
        }
        Ok(token)
    }

    #[cfg(feature = "managed")]
    pub(crate) fn new_managed(
        placement: [u8; 32],
        repository: &RepositoryLocator,
        refs: RefBatch,
        pack: Option<&crab_remote::prepare::PackBinding>,
        content: Option<&crab_remote::prepare::PreparedContent>,
        commit: Option<ObjectId>,
        push_id: uuid::Uuid,
        finalize: crab_auth::managed::PushFinalizeRequest,
    ) -> Result<Self> {
        let repository = repository
            .managed_url()
            .ok_or_else(|| invalid("managed recovery requires a managed repository"))?;
        let backend = RecoveryBackend::Managed { push_id, finalize };
        let request = RequestBinding {
            placement,
            repository,
            refs,
            pack: pack.map(|pack| PackBinding {
                pack_id: pack.pack_id().to_owned(),
                size: pack.size(),
            }),
            content: content.map(ContentBinding::from_owner),
            commit: commit.map(|oid| oid.to_string()),
        };
        request.validate(&backend)?;
        let digest = crab_metadata::receipts::publication_request_digest(&(&backend, &request))
            .map_err(metadata_error)?;
        let nonce = *uuid::Uuid::now_v7().as_bytes();
        let plan = crab_metadata::receipts::publication_plan_id(&digest, &nonce);
        let token = Self(TokenBinding {
            version: 4,
            backend,
            operation_nonce: nonce,
            request,
            request_digest: digest,
            plan_id: blake3::Hash::from(plan).to_hex().to_string(),
        });
        if token.to_json()?.len() > MAX_TOKEN_BYTES {
            return Err(invalid("recovery token exceeds 1 MiB"));
        }
        Ok(token)
    }

    /// Encode the complete recovery binding for durable application storage.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(&self.0).map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot encode recovery token", source)
        })
    }

    /// Decode and verify a bounded token before using it for storage access.
    pub fn from_json(json: &str) -> Result<Self> {
        if json.len() > MAX_TOKEN_BYTES {
            return Err(invalid("recovery token exceeds 1 MiB"));
        }
        let binding: TokenBinding = serde_json::from_str(json).map_err(|source| {
            Error::with_source(ErrorKind::InvalidInput, "invalid recovery token", source)
        })?;
        if binding.version != 4 {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "unsupported recovery token version or backend",
            ));
        }
        let nonce = uuid::Uuid::from_bytes(binding.operation_nonce);
        if nonce.get_version_num() != 7 || nonce.get_variant() != uuid::Variant::RFC4122 {
            return Err(invalid(
                "recovery token requires its original UUIDv7 operation nonce",
            ));
        }
        binding.request.validate(&binding.backend)?;
        let digest = crab_metadata::receipts::publication_request_digest(&(
            &binding.backend,
            &binding.request,
        ))
        .map_err(metadata_error)?;
        let plan = crab_metadata::receipts::publication_plan_id(&digest, &binding.operation_nonce);
        if digest != binding.request_digest
            || binding.plan_id != blake3::Hash::from(plan).to_hex().as_str()
        {
            return Err(invalid("recovery token does not match its request binding"));
        }
        Ok(Self(binding))
    }

    /// Return the durable nonce-bound plan identity, distinct from diagnostic IDs.
    #[must_use]
    pub fn plan_id(&self) -> &str {
        &self.0.plan_id
    }

    #[cfg(feature = "local")]
    pub(crate) fn ref_batch(&self) -> &RefBatch {
        &self.0.request.refs
    }

    pub(crate) fn placement(&self) -> &[u8; 32] {
        &self.0.request.placement
    }
    pub(crate) fn batch(&self) -> &RefBatch {
        &self.0.request.refs
    }
    pub(crate) fn pack(&self) -> Option<(&str, u64)> {
        self.0
            .request
            .pack
            .as_ref()
            .map(|pack| (pack.pack_id.as_str(), pack.size))
    }
    pub(crate) fn content(&self) -> Option<&ContentBinding> {
        self.0.request.content.as_ref()
    }
    pub(crate) fn locator(&self) -> Result<RepositoryLocator> {
        match &self.0.backend {
            RecoveryBackend::Direct => RepositoryLocator::new(&self.0.request.repository),
            #[cfg(feature = "managed")]
            RecoveryBackend::Managed { .. } => managed_locator(&self.0.request.repository),
        }
    }
    #[cfg(feature = "managed")]
    pub(crate) fn managed_finalize(
        &self,
    ) -> Option<(uuid::Uuid, &crab_auth::managed::PushFinalizeRequest)> {
        match &self.0.backend {
            RecoveryBackend::Direct => None,
            RecoveryBackend::Managed { push_id, finalize } => Some((*push_id, finalize)),
        }
    }
}

#[cfg(feature = "managed")]
fn managed_locator(value: &str) -> Result<RepositoryLocator> {
    let rest = value
        .strip_prefix("crab://")
        .ok_or_else(|| invalid("managed recovery repository URL is invalid"))?;
    let mut parts = rest.split('/');
    let authority = parts.next().unwrap_or_default();
    let organization = parts.next().unwrap_or_default();
    let repository = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return Err(invalid("managed recovery repository URL is invalid"));
    }
    RepositoryLocator::managed(authority, organization, repository)
}

/// Durable attribution of an acknowledged or historically proven commit.
#[derive(Clone, Debug)]
pub struct CommitReceipt {
    pub(crate) recovery: RecoveryToken,
    pub(crate) transaction_id: String,
}

impl CommitReceipt {
    /// Return the token that can independently recheck this historical result.
    #[must_use]
    pub fn recovery_token(&self) -> &RecoveryToken {
        &self.recovery
    }
    /// Return the immutable direct journal transaction or managed push identity.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }
}

/// Read readiness after commitment; pending never implies rollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Readiness {
    Ready { generation: u64 },
    Pending,
}

/// A proven atomic request rejection before visibility publication.
#[derive(Debug)]
pub struct RefRejection {
    pub(crate) source: Error,
}

impl RefRejection {
    /// Return the typed failure that rejected the entire batch.
    #[must_use]
    pub fn error(&self) -> &Error {
        &self.source
    }
}

/// Terminal mutation evidence, retained even when cancellation follows commitment.
#[derive(Debug)]
#[must_use]
#[non_exhaustive]
pub enum MutationOutcome {
    Rejected {
        reasons: Vec<RefRejection>,
    },
    Committed {
        receipt: CommitReceipt,
        readiness: Readiness,
    },
    Indeterminate {
        recovery: RecoveryToken,
    },
}

pub(crate) fn metadata_error(source: crab_metadata::error::MetadataError) -> Error {
    Error::with_source(
        crate::remote_error::metadata_kind(&source),
        "publication metadata operation failed",
        source,
    )
}

fn invalid(message: &'static str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_size_limit_matches_canonical_encoding() {
        let base = ObjectId::from_hex(&"a".repeat(40)).unwrap();
        let tree =
            gix_hash::ObjectId::from_hex(b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let identity = CommitIdentity::new("author", "a@example.invalid", i64::MIN, -420).unwrap();
        for options in [
            CommitOptions::new(
                base,
                "refs/heads/main",
                Some(base),
                identity.clone(),
                identity.clone(),
                b"message\n".to_vec(),
            )
            .unwrap(),
            CommitOptions::initial(
                "refs/heads/main",
                identity.clone(),
                identity.clone(),
                b"message\n".to_vec(),
            )
            .unwrap(),
        ] {
            let parents = options
                .base()
                .map(|value| vec![value.owner()])
                .unwrap_or_default();
            let encoded = crab_remote::objects::encode_commit(
                tree,
                &parents,
                options.author().owner(),
                options.committer().owner(),
                options.message(),
            )
            .unwrap();
            options.validate_size(encoded.len() as u64).unwrap();
            assert_eq!(
                options
                    .validate_size(encoded.len() as u64 - 1)
                    .unwrap_err()
                    .kind(),
                ErrorKind::LimitExceeded
            );
        }
    }

    #[test]
    fn commit_identity_rejects_unbounded_header_components() {
        assert_eq!(
            CommitIdentity::new("x".repeat(1025), "a@example.invalid", 0, 0)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }

    fn token() -> RecoveryToken {
        let oid = ObjectId::from_hex(&"a".repeat(40)).unwrap();
        RecoveryToken::new(
            [7; 32],
            &RepositoryLocator::new("repository").unwrap(),
            RefBatch::new(vec![RefUpdate::create("refs/tags/v1", oid).unwrap()]).unwrap(),
            None,
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn recovery_binding_rejects_changed_placement_request_and_identity() {
        let token = token();
        let encoded = token.to_json().unwrap();
        assert_eq!(
            RecoveryToken::from_json(&encoded).unwrap().plan_id(),
            token.plan_id()
        );
        for pointer in [
            "/request/placement/0",
            "/operation_nonce/0",
            "/request_digest/0",
        ] {
            let mut changed: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            let value = changed.pointer_mut(pointer).unwrap();
            *value = ((value.as_u64().unwrap() + 1) % 256).into();
            assert!(
                RecoveryToken::from_json(&changed.to_string()).is_err(),
                "{pointer}"
            );
        }
        for (pointer, replacement) in [
            ("/request/repository", "another"),
            ("/request/refs/edits/0/name", "refs/tags/other"),
            (
                "/request/refs/edits/0/new",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
            ("/request/refs/policy", "force_with_lease"),
            (
                "/plan_id",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
        ] {
            let mut changed: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            *changed.pointer_mut(pointer).unwrap() = replacement.into();
            assert!(
                RecoveryToken::from_json(&changed.to_string()).is_err(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn deserialization_cannot_bypass_ref_input_validation() {
        for value in [
            serde_json::json!({"name":"main", "old":null, "new":"a".repeat(40)}),
            serde_json::json!({"name":"refs/heads/main", "old":null, "new":"0".repeat(40)}),
            serde_json::json!({"name":"refs/heads/main", "old":"a".repeat(40), "new":"a".repeat(40)}),
            serde_json::json!({"name":"refs/tags/v1", "old":null, "new":"a".repeat(64)}),
        ] {
            assert!(serde_json::from_value::<RefUpdate>(value).is_err());
        }
        let edit = serde_json::json!({"name":"refs/tags/v1", "old":null, "new":"a".repeat(40)});
        for edits in [vec![], vec![edit.clone(), edit]] {
            assert!(
                serde_json::from_value::<RefBatch>(
                    serde_json::json!({"edits":edits,"policy":"fast_forward"})
                )
                .is_err()
            );
        }
    }
}
