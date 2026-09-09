use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bytes::Bytes;
use crab_git::{
    incoming_pack::{self, BaseObject, IncomingPack, PreparedPack, ReceiveLimits},
    receive_plan::{
        self, GraphLimits, GraphSource, RefPolicy, RefUpdate, RefVisibility, ValidatedRefUpdates,
        VisibilitySource,
    },
};
use crab_metadata::{
    git_object_locator::GitObjectKind,
    git_visibility::{GitCatalogVisibilityIndex, GitVisibilityEdit},
};
use crab_remote_git::{OperationContext, OperationKind, RemoteGitRepository};
use gix_hash::ObjectId;
use gix_object::Kind;
use tokio_util::sync::CancellationToken;

/// Failure while validating and preparing incoming Git objects.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("publication preparation cancelled")]
    Cancelled,
    #[error("{0}")]
    Request(&'static str),
    #[error("prepared content rejected: {0}")]
    Content(String),
    #[error("prepared content is corrupt")]
    ContentFormat(#[source] crab_xet::error::XetError),
    #[error("incoming pack rejected")]
    Pack(#[from] incoming_pack::IncomingPackError),
    #[error("incoming graph rejected")]
    Graph(#[from] receive_plan::ReceivePlanError),
    #[error("pack preparation failed")]
    Prepare(#[from] incoming_pack::PreparePackError),
    #[error("preparation I/O failed")]
    Io(#[from] std::io::Error),
    #[error("publication content dependency rejected")]
    Dependency(#[source] Box<crab_read::dependency_proof::DependencyProofError>),
    #[error("publication commitment failed")]
    Write(#[from] crab_write::WriteError),
    #[error("publication artifact storage failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("publication artifact metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("preparation worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("remote lookup failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("preparation and reader cleanup failed")]
    Close {
        #[source]
        operation: Box<Error>,
        close: crab_remote_git::Error,
    },
}

/// Caller-selected receive bounds and per-ref policy.
pub struct Options<P> {
    pub layout: crab_storage::StoreLayout<crab_storage::Store>,
    pub graph: GraphLimits,
    pub pack: ReceiveLimits,
    pub policy: P,
}

/// Validated ref edits and immutable pack/visibility preparation.
pub struct Prepared {
    repository: RemoteGitRepository,
    pub(crate) layout: crab_storage::StoreLayout<crab_storage::Store>,
    plan: ValidatedRefUpdates,
    visibility: BTreeMap<String, GitVisibilityEdit>,
    pack: Option<PreparedPack>,
    pub(crate) content: Option<PreparedContent>,
    updates: Vec<RefUpdate>,
}

/// One immutable xorb or shard prepared on local scratch storage.
#[derive(Clone, Debug)]
pub struct ContentArtifact {
    pub(crate) protocol_hash: [u8; 32],
    pub(crate) body_hash: [u8; 32],
    size: u64,
    path: PathBuf,
}

impl ContentArtifact {
    /// Bind a local artifact to both its protocol identity and exact body.
    #[must_use]
    pub fn new(protocol_hash: [u8; 32], body_hash: [u8; 32], size: u64, path: PathBuf) -> Self {
        Self {
            protocol_hash,
            body_hash,
            size,
            path,
        }
    }

    #[must_use]
    pub const fn protocol_hash(&self) -> &[u8; 32] {
        &self.protocol_hash
    }

    #[must_use]
    pub const fn body_hash(&self) -> &[u8; 32] {
        &self.body_hash
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// One Crab pointer supplied by a prepared reconstruction shard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentFile {
    pub(crate) file_hash: [u8; 32],
    pub(crate) size: u64,
    shard_hash: [u8; 32],
}

impl ContentFile {
    #[must_use]
    pub const fn new(file_hash: [u8; 32], size: u64, shard_hash: [u8; 32]) -> Self {
        Self {
            file_hash,
            size,
            shard_hash,
        }
    }

    #[must_use]
    pub const fn file_hash(&self) -> &[u8; 32] {
        &self.file_hash
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub const fn shard_hash(&self) -> &[u8; 32] {
        &self.shard_hash
    }
}

/// Native Crab artifacts that must become durable before their Git pointers.
pub struct PreparedContent {
    pub(crate) xorbs: Vec<ContentArtifact>,
    pub(crate) shards: Vec<ContentArtifact>,
    pub(crate) files: Vec<ContentFile>,
}

impl PreparedContent {
    /// Validate local xorb and shard bodies and their complete pointer coverage.
    pub async fn new(
        xorbs: Vec<ContentArtifact>,
        shards: Vec<ContentArtifact>,
        files: Vec<ContentFile>,
        maximum_artifact_bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        if files.is_empty() || shards.is_empty() {
            return Err(Error::Content(
                "content artifact set is incomplete".to_owned(),
            ));
        }
        let total_artifact_bytes = xorbs
            .iter()
            .chain(&shards)
            .try_fold(0u64, |total, artifact| total.checked_add(artifact.size))
            .ok_or_else(|| Error::Content("content artifact size overflow".to_owned()))?;
        if total_artifact_bytes > maximum_artifact_bytes {
            return Err(Error::Content(
                "content artifacts exceed the admitted byte limit".to_owned(),
            ));
        }
        let inspection_cancel = cancel.clone();
        let inspection_xorbs = xorbs.clone();
        let inspected = tokio::task::spawn_blocking(move || {
            let mut seen = std::collections::BTreeSet::new();
            let mut inspected_by_hash = BTreeMap::new();
            for artifact in &inspection_xorbs {
                if artifact.size == 0 || !seen.insert(artifact.protocol_hash) {
                    return Err(Error::Content("xorb identity is invalid".to_owned()));
                }
                let inspected = crab_xet::xorb::parser::inspect_file(
                    &artifact.path,
                    maximum_artifact_bytes,
                    || inspection_cancel.is_cancelled(),
                )
                .map_err(|error| match error {
                    crab_xet::error::XetError::ShardReplayIo { source, .. } => Error::Io(source),
                    _ if inspection_cancel.is_cancelled() => Error::Cancelled,
                    error => Error::ContentFormat(error),
                })?;
                if <[u8; 32]>::from(inspected.hash) != artifact.protocol_hash
                    || inspected.body_digest != artifact.body_hash
                    || inspected.size != artifact.size
                {
                    return Err(Error::Content("xorb binding changed".to_owned()));
                }
                inspected_by_hash.insert(artifact.protocol_hash, inspected);
            }
            Ok(inspected_by_hash)
        })
        .await??;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let mut shard_bodies = BTreeMap::new();
        for artifact in &shards {
            if artifact.size == 0
                || artifact.size > maximum_artifact_bytes
                || artifact.size > crab_xet::shard::MAX_PUSH_SHARD_SIZE_BYTES
                || shard_bodies.contains_key(&artifact.protocol_hash)
            {
                return Err(Error::Content("shard identity is invalid".to_owned()));
            }
            let bytes = tokio::fs::read(&artifact.path).await?;
            if bytes.len() as u64 != artifact.size
                || blake3::hash(&bytes).as_bytes() != &artifact.body_hash
                || <[u8; 32]>::from(crab_xet::hash::compute_data_hash(&bytes))
                    != artifact.protocol_hash
            {
                return Err(Error::Content("shard binding changed".to_owned()));
            }
            shard_bodies.insert(artifact.protocol_hash, Bytes::from(bytes));
        }
        let mut seen_files = std::collections::BTreeSet::new();
        let mut used_shards = std::collections::BTreeSet::new();
        let mut used_xorbs = std::collections::BTreeSet::new();
        for file in &files {
            if !seen_files.insert(file.file_hash) {
                return Err(Error::Content("content file is duplicated".to_owned()));
            }
            let bytes = shard_bodies
                .get(&file.shard_hash)
                .ok_or_else(|| Error::Content("content shard is missing".to_owned()))?;
            used_shards.insert(file.shard_hash);
            let reader = crab_xet::shard::ShardReader::from_bytes(
                bytes.clone(),
                crab_xet::hash::MerkleHash::from(file.shard_hash),
            );
            let info = reader
                .get_file_info(&crab_xet::hash::MerkleHash::from(file.file_hash))
                .map_err(Error::ContentFormat)?
                .ok_or_else(|| Error::Content("shard does not contain its file".to_owned()))?;
            let covered = info.segments.iter().try_fold(0u64, |total, segment| {
                total.checked_add(u64::from(segment.unpacked_segment_bytes))
            });
            if covered != Some(file.size) {
                return Err(Error::Content(format!(
                    "shard file {} covers {covered:?} bytes, expected {}",
                    crab_xet::hash::MerkleHash::from(file.file_hash).hex(),
                    file.size
                )));
            }
            let mut dependencies = Vec::new();
            for segment in &info.segments {
                let hash: [u8; 32] = segment.xorb_hash.into();
                let expected = inspected
                    .get(&hash)
                    .ok_or_else(|| Error::Content("shard xorb is missing".to_owned()))?;
                used_xorbs.insert(hash);
                let dependency = reader
                    .get_xorb_info(&segment.xorb_hash)
                    .map_err(Error::ContentFormat)?
                    .ok_or_else(|| Error::Content("shard xorb metadata is missing".to_owned()))?;
                let mut offset = 0u32;
                if dependency.chunks.len() != expected.chunks.len()
                    || dependency
                        .chunks
                        .iter()
                        .zip(&expected.chunks)
                        .any(|(actual, expected)| {
                            let matches = actual.chunk_hash == expected.hash
                                && actual.unpacked_segment_bytes == expected.uncompressed_len
                                && actual.chunk_byte_range_start == offset;
                            offset = offset.saturating_add(expected.uncompressed_len);
                            !matches
                        })
                {
                    return Err(Error::Content("shard xorb metadata changed".to_owned()));
                }
                if !dependencies
                    .iter()
                    .any(|known: &crab_xet::shard::MDBXorbInfo| {
                        known.metadata.xorb_hash == dependency.metadata.xorb_hash
                    })
                {
                    dependencies.push(dependency);
                }
            }
            crab_xet::shard::validate_file_bundle(&info, &dependencies)
                .map_err(Error::ContentFormat)?;
        }
        if used_shards.len() != shards.len() || used_xorbs.len() != xorbs.len() {
            return Err(Error::Content(
                "content contains unused artifacts".to_owned(),
            ));
        }
        Ok(Self {
            xorbs,
            shards,
            files,
        })
    }

    #[must_use]
    pub fn xorbs(&self) -> &[ContentArtifact] {
        &self.xorbs
    }

    #[must_use]
    pub fn shards(&self) -> &[ContentArtifact] {
        &self.shards
    }

    #[must_use]
    pub fn files(&self) -> &[ContentFile] {
        &self.files
    }
}

/// Durable identity of a normalized pack staged before ref publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackBinding {
    pack_id: String,
    size: u64,
}

impl PackBinding {
    /// Return the Blake3 identity used by the canonical repository pack path.
    #[must_use]
    pub fn pack_id(&self) -> &str {
        &self.pack_id
    }

    /// Return the complete pack size, including its Git SHA-1 trailer.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// Uploaded immutable artifacts ready for journal publication.
pub struct Artifacts<'a> {
    pub(crate) prepared: &'a Prepared,
    pub(crate) snapshot: &'a crab_metadata::manifest_store::RepositorySnapshot,
    pub(crate) edits: Vec<crab_metadata::ref_journal::RefJournalEdit>,
    pub(crate) packs: Vec<crab_metadata::manifests::PackManifestEntry>,
    pub(crate) shards: Vec<String>,
}

impl Artifacts<'_> {
    /// Commit these uploaded artifacts against their admitted repository snapshot.
    ///
    /// Revalidate authorization and repository policy before calling. Keep the
    /// original ref leases and GC fences held and await completion: a marker
    /// error may require reconciliation, and cancellation cannot undo a commit.
    /// Planned commits also require the admitted operation lease; options retain
    /// that plan's identity through the journal intent and receipt boundary.
    pub async fn commit(
        self,
        head: Option<String>,
        options: crab_write::journal::CommitOptions<'_>,
    ) -> Result<crate::publication::CommitOutcome> {
        if !self
            .prepared
            .repository
            .is_current(options.cancellation())
            .await?
        {
            return Err(Error::Request(
                "Repository changed before publication; retry",
            ));
        }
        let layout = &self.prepared.layout;
        let result = crab_write::journal::commit_edits(
            layout.store(),
            layout,
            self.snapshot,
            self.edits,
            head,
            self.packs,
            self.shards,
            options,
        )
        .await;
        crate::publication::journal_outcome(result).map_err(Into::into)
    }
}

impl Prepared {
    /// Inspect the validated ref and content dependency plan.
    #[must_use]
    pub fn plan(&self) -> &ValidatedRefUpdates {
        &self.plan
    }

    /// Return the normalized pack identity, when this mutation introduces objects.
    #[must_use]
    pub fn pack_binding(&self) -> Option<PackBinding> {
        self.pack.as_ref().map(|pack| PackBinding {
            pack_id: pack.content_hash().to_hex().to_string(),
            size: pack.size(),
        })
    }

    /// Return conservative staged bytes and object count for admission.
    #[must_use]
    pub fn admission_estimate(&self) -> (u64, u64) {
        let mut bytes = 64_u64 * 1024;
        let mut objects = 8u64;
        if let Some(pack) = &self.pack {
            bytes = bytes.saturating_add(pack.size());
            objects = objects.saturating_add(u64::from(pack.object_count()));
        }
        if let Some(content) = &self.content {
            for artifact in content.xorbs.iter().chain(&content.shards) {
                bytes = bytes.saturating_add(artifact.size);
                objects = objects.saturating_add(1);
            }
            objects = objects.saturating_add(content.files.len() as u64);
        }
        (bytes.max(1), objects.max(1))
    }

    /// Return native content bound to this prepared publication, when present.
    #[must_use]
    pub fn content(&self) -> Option<&PreparedContent> {
        self.content.as_ref()
    }

    /// Bind locally validated Crab-native content to pointer dependencies.
    pub fn attach_content(mut self, content: PreparedContent) -> Result<Self> {
        let files = content
            .files
            .iter()
            .map(|file| (file.file_hash, file))
            .collect::<BTreeMap<_, _>>();
        let mut used = std::collections::BTreeSet::new();
        for dependency in self.plan.pointers() {
            let crab_git::pointer_detect::PointerKind::Crab(pointer) = &dependency.pointer else {
                continue;
            };
            let Some(file) = files.get(&pointer.file_hash) else {
                continue;
            };
            if pointer.size != file.size
                || pointer
                    .shard_hint
                    .is_some_and(|hint| hint != file.shard_hash)
            {
                return Err(Error::Request(
                    "prepared content does not match its Git pointer",
                ));
            }
            used.insert(file.file_hash);
        }
        if used.len() != files.len() {
            return Err(Error::Request(
                "prepared content is not reachable from the ref update",
            ));
        }
        self.content = Some(content);
        Ok(self)
    }

    /// Rebind writes to a protected staging store over the same validated reader.
    pub fn with_staging_layout(
        mut self,
        layout: crab_storage::StoreLayout<crab_storage::Store>,
    ) -> Result<Self> {
        if self.layout.repo_prefix() != layout.repo_prefix()
            || self.layout.global_prefix() != layout.global_prefix()
            || self.layout.store().target_identity() != layout.store().target_identity()
            || self.layout.store().bucket_identity() != layout.store().bucket_identity()
        {
            return Err(Error::Request(
                "protected staging layout differs from the validated repository",
            ));
        }
        if layout.store().staging_write_prefix().is_none() {
            return Err(Error::Request(
                "protected publication requires a staging-write store",
            ));
        }
        self.layout = layout;
        Ok(self)
    }

    /// Stage the normalized pack under one durable journal recovery root.
    ///
    /// This creates no ref, journal, visibility, or manifest entry. It makes a
    /// prepared mutation recoverable after its private scratch files disappear.
    pub async fn stage_recovery_artifacts(
        &self,
        plan_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<PackBinding>> {
        if plan_id.len() != 64
            || !plan_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(Error::Request("invalid recovery plan identity"));
        }
        if let Some(content) = &self.content {
            for xorb in &content.xorbs {
                self.layout
                    .store()
                    .put_multipart_file_retry(
                        &self.layout.ref_journal_recovery_xorb_path(
                            plan_id,
                            &crab_xet::hash::MerkleHash::from(xorb.protocol_hash),
                        ),
                        &xorb.path,
                        xorb.size,
                        xorb.body_hash,
                        8 * 1024 * 1024,
                        cancel,
                        None,
                    )
                    .await?;
            }
            for shard in &content.shards {
                self.layout
                    .store()
                    .put_multipart_file_retry(
                        &self.layout.ref_journal_recovery_shard_path(
                            plan_id,
                            &crab_xet::hash::MerkleHash::from(shard.protocol_hash),
                        ),
                        &shard.path,
                        shard.size,
                        shard.body_hash,
                        8 * 1024 * 1024,
                        cancel,
                        None,
                    )
                    .await?;
            }
        }
        let Some(pack) = &self.pack else {
            return Ok(None);
        };
        let binding = self
            .pack_binding()
            .ok_or(Error::Request("prepared pack identity disappeared"))?;
        self.layout
            .store()
            .put_multipart_file_retry(
                &self.layout.ref_journal_recovery_pack_path(plan_id),
                pack.pack_path(),
                binding.size(),
                *pack.content_hash().as_bytes(),
                8 * 1024 * 1024,
                cancel,
                None,
            )
            .await?;
        Ok(Some(binding))
    }

    /// Upload the validated pack and visibility evidence before journal commitment.
    ///
    /// The caller must hold ref leases and GC fences, supply the admitted snapshot,
    /// and revalidate authorization and that snapshot before committing. Content
    /// dependencies are verified before any artifact upload.
    /// Failure can leave unreferenced immutable objects; no refs are changed here.
    pub async fn upload<'a>(
        &'a self,
        snapshot: &'a crab_metadata::manifest_store::RepositorySnapshot,
        limits: crab_read::dependency_proof::DependencyProofLimits,
        holders: &BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<Artifacts<'a>> {
        use crab_metadata::{
            git_visibility, manifests::PackManifestEntry, ref_journal::RefJournalEdit,
        };
        if !self.repository.matches_snapshot(snapshot) {
            return Err(Error::Request(
                "Artifact snapshot differs from the prepared repository version",
            ));
        }
        let layout = &self.layout;
        let store = layout.store();
        let prepared_files = self
            .content
            .as_ref()
            .map(|content| {
                content
                    .files
                    .iter()
                    .map(|file| file.file_hash)
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_default();
        crab_read::dependency_proof::verify_dependencies_except_crab(
            layout,
            snapshot,
            self.plan.pointers(),
            &prepared_files,
            limits,
            cancel,
        )
        .await
        .map_err(|error| Error::Dependency(Box::new(error)))?;
        let mut shards = Vec::new();
        if let Some(content) = &self.content {
            for xorb in &content.xorbs {
                store
                    .put_multipart_file_retry(
                        &layout.xorb_path(&crab_xet::hash::MerkleHash::from(xorb.protocol_hash)),
                        &xorb.path,
                        xorb.size,
                        xorb.body_hash,
                        8 * 1024 * 1024,
                        cancel,
                        None,
                    )
                    .await?;
            }
            for shard in &content.shards {
                let hash = crab_xet::hash::MerkleHash::from(shard.protocol_hash);
                store
                    .put_multipart_file_retry(
                        &layout.shard_path(&hash),
                        &shard.path,
                        shard.size,
                        shard.body_hash,
                        8 * 1024 * 1024,
                        cancel,
                        None,
                    )
                    .await?;
                shards.push(hash.hex());
            }
        }
        let mut packs = Vec::new();
        if let Some(pack) = &self.pack {
            let binding = self
                .pack_binding()
                .ok_or(Error::Request("prepared pack identity disappeared"))?;
            let pack_id = binding.pack_id().to_owned();
            store
                .put_multipart_file_retry(
                    &layout.pack_path(&pack_id),
                    pack.pack_path(),
                    pack.size(),
                    *pack.content_hash().as_bytes(),
                    8 * 1024 * 1024,
                    cancel,
                    None,
                )
                .await?;
            let mut evidence = vec![(pack.kinds_path(), layout.pack_kind_metadata_path(&pack_id))];
            // Protected receive rebuilds indexes from the staged pack. Its plan
            // accepts only objects rooted by candidate metadata, so local index
            // helpers must remain client-side for a staging-write store.
            if store.staging_write_prefix().is_none() {
                evidence.extend([
                    (pack.index_path(), layout.pack_index_path(&pack_id)),
                    (
                        pack.reverse_path(),
                        layout.pack_reverse_index_path(&pack_id),
                    ),
                ]);
            }
            for (source, target) in evidence {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let bytes = tokio::fs::read(source).await?;
                store.put_exact(&target, bytes.into()).await?;
            }
            packs.push(PackManifestEntry {
                pack_id: pack_id.clone(),
                content_hash: pack_id,
                size: pack.size(),
                object_count: pack.object_count().into(),
                ref_tips: self
                    .updates
                    .iter()
                    .filter_map(|update| update.new.map(|oid| oid.to_string()))
                    .collect(),
            });
        }
        let mut edits = Vec::new();
        for update in &self.updates {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let evidence = match self.visibility.get(&update.name) {
                Some(proof) => Some(git_visibility::upload_edit(store, layout, proof).await?),
                None => None,
            };
            edits.push(RefJournalEdit {
                ref_name: update.name.clone(),
                old_oid: update.old.map(|oid| oid.to_string()),
                new_oid: update.new.map(|oid| oid.to_string()),
                peeled_oid: self
                    .plan
                    .peeled()
                    .get(&update.name)
                    .map(ToString::to_string),
                lock_holder: holders.get(&update.name).cloned(),
                visibility_evidence_hash: evidence,
            });
        }
        Ok(Artifacts {
            prepared: self,
            snapshot,
            edits,
            packs,
            shards,
        })
    }
}

impl Artifacts<'_> {
    /// Build and stage the server-verified protected publication plan.
    pub async fn stage_protected_plan(
        self,
        push_id: &str,
        upload_prefix: &str,
        ref_updates: Vec<crab_auth::PushRefUpdate>,
        cancel: &CancellationToken,
    ) -> Result<crate::protected::ProtectedPushPlan> {
        crate::protected::stage_plan(self, push_id, upload_prefix, ref_updates, cancel).await
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

struct Source<'a> {
    operation: &'a OperationContext,
    proof: Option<GitCatalogVisibilityIndex>,
    refs: Vec<String>,
    prior: Option<(String, ObjectId)>,
    handle: tokio::runtime::Handle,
}

type SourceResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

impl Source<'_> {
    fn ordinal(&self, oid: &ObjectId) -> SourceResult<Option<u32>> {
        if self.proof.is_none() {
            return Ok(None);
        }
        Ok(self
            .handle
            .block_on(self.operation.catalog_object_ordinals(&[*oid]))?
            .into_iter()
            .next()
            .flatten())
    }
    fn visible(&self, oid: &ObjectId) -> SourceResult<bool> {
        Ok(self.ordinal(oid)?.is_some_and(|ordinal| {
            self.proof.as_ref().is_some_and(|proof| {
                proof.contains_ordinal_for_refs(self.refs.iter().map(String::as_str), ordinal)
            })
        }))
    }
}

impl GraphSource for Source<'_> {
    fn trusted_kind(&mut self, oid: &ObjectId) -> SourceResult<Option<Kind>> {
        if !self.visible(oid)? {
            return Ok(None);
        }
        let bytes = oid.as_bytes().try_into()?;
        let kind = self
            .handle
            .block_on(self.operation.catalog_object_kinds(&[bytes]))?
            .into_iter()
            .next()
            .flatten()
            .map(|kind| match kind {
                GitObjectKind::Commit => Kind::Commit,
                GitObjectKind::Tree => Kind::Tree,
                GitObjectKind::Blob => Kind::Blob,
                GitObjectKind::Tag => Kind::Tag,
            });
        if kind.is_some() {
            return Ok(kind);
        }
        // Older imported catalogs may omit kind metadata. Reading this proven
        // visible object establishes its kind without expanding its closure.
        Ok(Some(
            self.handle.block_on(self.operation.read_object(*oid))?.kind,
        ))
    }
    fn read(&mut self, oid: &ObjectId) -> SourceResult<Option<BaseObject>> {
        // A locator hit alone cannot authorize a dangling object or thin base.
        if !self.visible(oid)? {
            return Ok(None);
        }
        let object = self.handle.block_on(self.operation.read_object(*oid))?;
        Ok(Some(BaseObject {
            kind: object.kind,
            data: object.data.to_vec(),
        }))
    }
}

impl VisibilitySource for Source<'_> {
    fn prior_tip(&self) -> Option<ObjectId> {
        self.prior.as_ref().map(|(_, oid)| *oid)
    }
    fn in_prior_closure(&mut self, oid: &ObjectId) -> SourceResult<bool> {
        let Some((name, _)) = &self.prior else {
            return Ok(false);
        };
        Ok(self.ordinal(oid)?.is_some_and(|ordinal| {
            self.proof
                .as_ref()
                .is_some_and(|proof| proof.contains_ordinal_in_ref(name, ordinal))
        }))
    }
}

/// Validate an incoming graph and prepare immutable publication artifacts.
///
/// The caller owns authorization, ref/GC leases and the temporary directory.
/// Await completion after cancellation so the reader and blocking work drain.
pub async fn prepare(
    repository: RemoteGitRepository,
    directory: std::path::PathBuf,
    input: Option<BufReader<File>>,
    updates: Vec<RefUpdate>,
    visibility_bases: BTreeMap<String, (String, ObjectId)>,
    cancel: &CancellationToken,
    options: Options<impl Fn(&str) -> RefPolicy + Send + 'static>,
) -> Result<Prepared> {
    if !repository.matches_store_layout(&options.layout) {
        return Err(Error::Request(
            "Preparation layout differs from the validated repository",
        ));
    }
    let base: BTreeMap<_, _> = repository
        .refs()
        .entries
        .iter()
        .map(|reference| (reference.name.clone(), reference.target))
        .collect();
    let proof = if base.is_empty() {
        None
    } else {
        Some(repository.catalog_visibility_index(cancel).await?)
    };
    let operation = repository
        .operation(OperationKind::Repository, cancel)
        .await?;
    let handle = tokio::runtime::Handle::current();
    let cancel = cancel.clone();
    let flag = Arc::new(AtomicBool::new(false));
    let watched = cancel.clone();
    let watched_flag = Arc::clone(&flag);
    let watcher = tokio::spawn(async move {
        watched.cancelled().await;
        watched_flag.store(true, Ordering::Release);
    });
    let work = tokio::task::spawn_blocking(move || {
        let mut source = Source {
            operation: &operation,
            proof,
            refs: base.keys().cloned().collect(),
            prior: None,
            handle: handle.clone(),
        };
        let result = (|| {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let incoming = match input {
                None => IncomingPack::empty(&directory)?,
                Some(mut input) => {
                    if input.fill_buf()?.is_empty()
                        && updates.iter().all(|update| update.new.is_none())
                    {
                        IncomingPack::empty(&directory)?
                    } else {
                        incoming_pack::quarantine(
                            input,
                            &directory,
                            options.pack,
                            || cancel.is_cancelled(),
                            |oid| source.read(oid),
                        )?
                    }
                }
            };
            let plan = receive_plan::validate(
                &incoming,
                &base,
                &updates,
                &options.policy,
                &mut source,
                options.graph,
                || cancel.is_cancelled(),
            )?;
            let mut visibility = BTreeMap::new();
            let mut count = 0usize;
            for update in &updates {
                let Some(new) = update.new else {
                    continue;
                };
                source.prior = match visibility_bases.get(&update.name) {
                    Some((name, tip)) if base.get(name) == Some(tip) => Some((name.clone(), *tip)),
                    Some(_) => {
                        return Err(Error::Request(
                            "Visibility base changed during receive admission; retry",
                        ));
                    }
                    None => update
                        .old
                        .map(|old| (update.name.clone(), old))
                        .or_else(|| {
                            // New refs at an existing tip can reuse that exact
                            // committed ref closure instead of walking the graph.
                            base.iter()
                                .find(|(name, tip)| {
                                    **tip == new
                                        && source
                                            .proof
                                            .as_ref()
                                            .is_some_and(|proof| proof.contains_ref(name))
                                })
                                .map(|(name, tip)| (name.clone(), *tip))
                        }),
                };
                let proof = receive_plan::plan_visibility(
                    &incoming,
                    new,
                    &mut source,
                    options.graph,
                    || cancel.is_cancelled(),
                )?;
                let evidence = match proof {
                    RefVisibility::Additive { base, added } => {
                        GitVisibilityEdit::from_delta_objects(
                            Some(base.to_string()),
                            new.to_string(),
                            added.into_iter().map(|oid| oid.to_string()).collect(),
                            vec![],
                        )
                    }
                    RefVisibility::Replacement { objects } => {
                        GitVisibilityEdit::from_replacement_objects(
                            update.old.map(|oid| oid.to_string()),
                            new.to_string(),
                            objects.into_iter().map(|oid| oid.to_string()).collect(),
                        )
                    }
                };
                count = count.saturating_add(evidence.added.len());
                if count > options.graph.max_graph_steps {
                    return Err(Error::Request(
                        "Combined ref visibility exceeds the graph object limit",
                    ));
                }
                visibility.insert(update.name.clone(), evidence);
            }
            let pack = incoming.prepare(&directory, options.pack.max_pack_bytes, &flag)?;
            Ok(Prepared {
                repository,
                layout: options.layout,
                plan,
                visibility,
                pack,
                content: None,
                updates,
            })
        })();
        drop(source);
        let close = handle.block_on(operation.finish(Ok(())));
        match (result, close) {
            (result, Ok(())) => result,
            (Ok(_), Err(error)) => Err(error.into()),
            (Err(operation), Err(close)) => Err(Error::Close {
                operation: Box::new(operation),
                close,
            }),
        }
    })
    .await;
    watcher.abort();
    // Drain the cancellation observer after the blocking worker closes its reader.
    let _ = watcher.await;
    work?
}
