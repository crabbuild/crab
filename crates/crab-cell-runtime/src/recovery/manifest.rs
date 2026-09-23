use std::sync::Arc;

use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::StorageError;
use serde::{Deserialize, Serialize};

use crate::control::{RecoveryOverlayRef, RootRef};
use crate::identity::IncarnationId;
use crate::identity::{ApplicationId, CellId, Digest, SessionId};
use crate::node::log::RecoveredCellTail;
use crate::{Error, Result};

const MAX_MANIFEST_BYTES: u64 = 2 << 20;
const MULTIPART_BYTES: usize = 8 << 20;

/// One control-ready pointer returned after bundle and manifest publication.
pub struct PinnedRecoveryCell {
    pub application: ApplicationId,
    pub cell: CellId,
    pub incarnation: IncarnationId,
    pub cell_epoch: u64,
    pub recovery: RecoveryOverlayRef,
}

/// Bounded object publication counters for one recovery manifest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryPublicationSummary {
    pub bundle_bytes: u64,
    pub object_reads: u64,
    pub object_writes: u64,
}

/// Control-ready recovery pointers and their publication work summary.
pub struct PinnedRecoveryCells {
    pub cells: Vec<PinnedRecoveryCell>,
    pub summary: RecoveryPublicationSummary,
}

/// Exact immutable identity used for a local recovery-artifact cache entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecoveryArtifactKey {
    leader_session: [u8; 16],
    log_epoch: u64,
    application: [u8; 16],
    cell: [u8; 32],
    incarnation: [u8; 16],
    cell_epoch: u64,
    first_node_sequence: u64,
    last_node_sequence: u64,
    predecessor_digest: [u8; 32],
    predecessor_txid: u64,
    predecessor_checksum: u64,
    predecessor_commit_sequence: u64,
    final_txid: u64,
    final_checksum: u64,
    final_commit_sequence: u64,
    bundle_digest: [u8; 32],
}

impl RecoveryArtifactKey {
    /// Builds the exact scope and content identity used for cache admission.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "the persisted recovery scope is explicit"
    )]
    pub fn new(
        leader_session: SessionId,
        log_epoch: u64,
        application: ApplicationId,
        cell: CellId,
        incarnation: IncarnationId,
        cell_epoch: u64,
        first_node_sequence: u64,
        last_node_sequence: u64,
        predecessor: RootRef,
        final_position: crab_ltx::Position,
        final_commit_sequence: u64,
        bundle_digest: Digest,
    ) -> Self {
        Self {
            leader_session: *leader_session.as_bytes(),
            log_epoch,
            application: *application.as_bytes(),
            cell: *cell.as_bytes(),
            incarnation: *incarnation.as_bytes(),
            cell_epoch,
            first_node_sequence,
            last_node_sequence,
            predecessor_digest: *predecessor.digest.as_bytes(),
            predecessor_txid: predecessor.txid,
            predecessor_checksum: predecessor.checksum,
            predecessor_commit_sequence: predecessor.commit_sequence,
            final_txid: final_position.txid,
            final_checksum: final_position.checksum,
            final_commit_sequence,
            bundle_digest: *bundle_digest.as_bytes(),
        }
    }

    #[must_use]
    pub const fn bundle_digest(&self) -> [u8; 32] {
        self.bundle_digest
    }

    /// Derives a collision-resistant local filename identity from the full key.
    #[must_use]
    pub fn cache_digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab.recovery-artifact.v1\0");
        hasher.update(&self.leader_session);
        hasher.update(&self.log_epoch.to_be_bytes());
        hasher.update(&self.application);
        hasher.update(&self.cell);
        hasher.update(&self.incarnation);
        hasher.update(&self.cell_epoch.to_be_bytes());
        hasher.update(&self.first_node_sequence.to_be_bytes());
        hasher.update(&self.last_node_sequence.to_be_bytes());
        hasher.update(&self.predecessor_digest);
        hasher.update(&self.predecessor_txid.to_be_bytes());
        hasher.update(&self.predecessor_checksum.to_be_bytes());
        hasher.update(&self.predecessor_commit_sequence.to_be_bytes());
        hasher.update(&self.final_txid.to_be_bytes());
        hasher.update(&self.final_checksum.to_be_bytes());
        hasher.update(&self.final_commit_sequence.to_be_bytes());
        hasher.update(&self.bundle_digest);
        *hasher.finalize().as_bytes()
    }
}

/// Keeps a retained local artifact alive while a recovery overlay reads it.
pub struct RecoveryArtifact {
    bundle: crab_ltx::bundle::Bundle,
    lease: Arc<dyn crab_ltx::bundle::BundleLease>,
}

impl RecoveryArtifact {
    /// Wraps a verified bundle with the lease that keeps its backing artifact alive.
    #[must_use]
    pub fn new(
        bundle: crab_ltx::bundle::Bundle,
        lease: Arc<dyn crab_ltx::bundle::BundleLease>,
    ) -> Self {
        Self { bundle, lease }
    }

    fn into_parts(
        self,
    ) -> (
        crab_ltx::bundle::Bundle,
        Arc<dyn crab_ltx::bundle::BundleLease>,
    ) {
        (self.bundle, self.lease)
    }
}

/// Server-owned cache boundary for verified recovery bundles.
pub trait RecoveryArtifactStore: Send + Sync {
    /// Retains a bundle after its immutable object has been published.
    fn retain(&self, key: RecoveryArtifactKey, bundle: crab_ltx::bundle::Bundle) -> Result<()>;

    /// Loads and revalidates an exact retained artifact, or reports a miss.
    fn load(&self, key: &RecoveryArtifactKey) -> Result<Option<RecoveryArtifact>>;
}

/// Immutable object-store owner for recovered follower bundles and manifests.
#[derive(Clone)]
pub struct RecoveryManifestStore {
    layout: CellStorageLayout,
    limits: crab_ltx::Limits,
    recovery_disk: crab_ltx::DiskBudget,
    recovery_scratch: Option<std::path::PathBuf>,
    artifact_store: Option<Arc<dyn RecoveryArtifactStore>>,
}

impl RecoveryManifestStore {
    #[must_use]
    pub fn new(layout: CellStorageLayout, limits: crab_ltx::Limits) -> Self {
        let recovery_disk = crab_ltx::DiskBudget::new(limits.max_plan_bytes);
        Self {
            layout,
            limits,
            recovery_disk,
            recovery_scratch: None,
            artifact_store: None,
        }
    }

    /// Shares byte admission with node-wide follower-tail recovery work.
    #[must_use]
    pub fn with_recovery_disk(mut self, recovery_disk: crab_ltx::DiskBudget) -> Self {
        self.recovery_disk = recovery_disk;
        self
    }

    /// Places streamed recovery bundles on the runtime-owned session volume.
    ///
    /// The directory must already exist, be private to this runtime session,
    /// and remain on the same local volume accounted by its disk budget.
    /// Library callers that do not provide one use the operating-system
    /// temporary directory.
    #[must_use]
    pub fn with_recovery_scratch(mut self, directory: std::path::PathBuf) -> Self {
        self.recovery_scratch = Some(directory);
        self
    }

    /// Shares one server-owned verified artifact cache with pinning and load.
    /// Cache admission is opportunistic; object-store publication remains authoritative.
    #[must_use]
    pub fn with_recovery_artifacts(mut self, store: Arc<dyn RecoveryArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    pub(crate) fn recovery_scratch_directory(&self) -> std::path::PathBuf {
        self.recovery_scratch
            .clone()
            .unwrap_or_else(std::env::temp_dir)
    }

    /// Publishes every verified bundle before one content-addressed manifest.
    pub async fn pin(
        &self,
        leader_session: SessionId,
        log_epoch: u64,
        tails: Vec<RecoveredCellTail>,
    ) -> Result<Vec<PinnedRecoveryCell>> {
        Ok(self
            .pin_with_summary(leader_session, log_epoch, tails)
            .await?
            .cells)
    }

    pub async fn pin_with_summary(
        &self,
        leader_session: SessionId,
        log_epoch: u64,
        tails: Vec<RecoveredCellTail>,
    ) -> Result<PinnedRecoveryCells> {
        if leader_session.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || tails.is_empty()
        {
            return Err(Error::Node("invalid recovery manifest scope"));
        }
        let mut rows = Vec::with_capacity(tails.len());
        let mut artifacts = Vec::with_capacity(tails.len());
        let mut summary = RecoveryPublicationSummary::default();
        for tail in tails {
            let predecessor = tail.overlay.predecessor();
            let cell = CellId::from_bytes(predecessor.cell);
            let incarnation = IncarnationId::from_bytes(predecessor.incarnation);
            let runtime_predecessor = RootRef::from_ltx(cell, incarnation, predecessor)?;
            let final_position = tail.overlay.final_position();
            let final_commit_sequence = tail.overlay.final_commit_sequence();
            let bundle = tail.overlay.into_bundle()?;
            summary.bundle_bytes = summary
                .bundle_bytes
                .checked_add(bundle.len())
                .ok_or(Error::Capacity("recovery bundle byte count"))?;
            let bundle_digest = bundle.digest();
            let path = self.layout.node_log_bundle_path(
                leader_session.as_bytes(),
                log_epoch,
                &bundle_digest,
            );
            publish_bundle_immutable(&self.layout, &path, &bundle, &mut summary).await?;
            if self.artifact_store.is_some() {
                let key = RecoveryArtifactKey::new(
                    leader_session,
                    log_epoch,
                    ApplicationId::from_bytes(tail.application),
                    cell,
                    incarnation,
                    tail.cell_epoch,
                    tail.first_node_sequence,
                    tail.last_node_sequence,
                    runtime_predecessor,
                    final_position,
                    final_commit_sequence,
                    Digest::from_bytes(bundle_digest),
                );
                artifacts.push((key, bundle));
            }
            rows.push(ManifestCell {
                application: tail.application,
                cell: predecessor.cell,
                incarnation: predecessor.incarnation,
                cell_epoch: tail.cell_epoch,
                first_node_sequence: tail.first_node_sequence,
                last_node_sequence: tail.last_node_sequence,
                predecessor,
                final_position,
                final_commit_sequence,
                bundle_digest,
            });
        }
        rows.sort_unstable_by(|left, right| {
            (
                left.application,
                left.cell,
                left.incarnation,
                left.cell_epoch,
            )
                .cmp(&(
                    right.application,
                    right.cell,
                    right.incarnation,
                    right.cell_epoch,
                ))
        });
        if rows.windows(2).any(|pair| {
            pair[0].application == pair[1].application
                && pair[0].cell == pair[1].cell
                && pair[0].incarnation == pair[1].incarnation
                && pair[0].cell_epoch == pair[1].cell_epoch
        }) {
            return Err(Error::Node(
                "recovery manifest contains duplicate Cell scope",
            ));
        }
        let manifest = RecoveryManifest {
            leader_session,
            log_epoch,
            cells: rows,
        };
        let body = manifest.encode()?;
        let manifest_digest = *blake3::hash(&body).as_bytes();
        let path = self.layout.node_log_recovery_path(
            leader_session.as_bytes(),
            log_epoch,
            &manifest_digest,
        );
        publish_immutable(&self.layout, &path, &body, MAX_MANIFEST_BYTES, &mut summary).await?;
        if let Some(store) = &self.artifact_store {
            for (key, bundle) in artifacts {
                let store = Arc::clone(store);
                // Both immutable objects are the correctness boundary; a local
                // cache admission failure must not block control progress.
                let _ = tokio::task::spawn_blocking(move || store.retain(key, bundle)).await;
            }
        }
        Ok(PinnedRecoveryCells {
            cells: manifest
                .cells
                .into_iter()
                .map(|cell| PinnedRecoveryCell {
                    application: ApplicationId::from_bytes(cell.application),
                    cell: CellId::from_bytes(cell.cell),
                    incarnation: IncarnationId::from_bytes(cell.incarnation),
                    cell_epoch: cell.cell_epoch,
                    recovery: RecoveryOverlayRef {
                        leader_session,
                        log_epoch,
                        manifest_digest: Digest::from_bytes(manifest_digest),
                        first_node_sequence: cell.first_node_sequence,
                        last_node_sequence: cell.last_node_sequence,
                        predecessor: runtime_root(cell.predecessor),
                        final_txid: cell.final_position.txid,
                        final_checksum: cell.final_position.checksum,
                        final_commit_sequence: cell.final_commit_sequence,
                    },
                })
                .collect(),
            summary,
        })
    }

    /// Reopens the exact bundle named by a control-pinned recovery reference.
    pub async fn load_overlay(
        &self,
        cell: CellId,
        incarnation: IncarnationId,
        recovery: &RecoveryOverlayRef,
    ) -> Result<crab_ltx::RecoveryOverlay> {
        let path = self.layout.node_log_recovery_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            recovery.manifest_digest.as_bytes(),
        );
        let (body, _) = self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_MANIFEST_BYTES)
            .await?;
        if *blake3::hash(&body).as_bytes() != *recovery.manifest_digest.as_bytes() {
            return Err(Error::Node("recovery manifest digest differs"));
        }
        let manifest = RecoveryManifest::decode(&body)?;
        if manifest.leader_session != recovery.leader_session
            || manifest.log_epoch != recovery.log_epoch
        {
            return Err(Error::Node("recovery manifest path scope differs"));
        }
        let row = manifest
            .cells
            .into_iter()
            .find(|row| {
                row.application == *self.layout.application_id()
                    && row.cell == *cell.as_bytes()
                    && row.incarnation == *incarnation.as_bytes()
            })
            .ok_or(Error::Node("recovery manifest does not contain Cell"))?;
        let expected = RecoveryOverlayRef {
            leader_session: recovery.leader_session,
            log_epoch: recovery.log_epoch,
            manifest_digest: recovery.manifest_digest,
            first_node_sequence: row.first_node_sequence,
            last_node_sequence: row.last_node_sequence,
            predecessor: runtime_root(row.predecessor),
            final_txid: row.final_position.txid,
            final_checksum: row.final_position.checksum,
            final_commit_sequence: row.final_commit_sequence,
        };
        if &expected != recovery {
            return Err(Error::Node(
                "recovery control pointer differs from manifest",
            ));
        }
        if let Some(artifacts) = &self.artifact_store {
            let runtime_predecessor = RootRef::from_ltx(cell, incarnation, row.predecessor)?;
            let key = RecoveryArtifactKey::new(
                recovery.leader_session,
                recovery.log_epoch,
                ApplicationId::from_bytes(*self.layout.application_id()),
                cell,
                incarnation,
                row.cell_epoch,
                row.first_node_sequence,
                row.last_node_sequence,
                runtime_predecessor,
                row.final_position,
                row.final_commit_sequence,
                Digest::from_bytes(row.bundle_digest),
            );
            let artifacts = Arc::clone(artifacts);
            let cached = tokio::task::spawn_blocking(move || artifacts.load(&key))
                .await
                .map_err(|_| Error::Node("recovery artifact worker failed"))?;
            if let Ok(Some(artifact)) = cached {
                let (bundle, lease) = artifact.into_parts();
                return Ok(crab_ltx::RecoveryOverlay::new(
                    row.predecessor,
                    bundle,
                    row.final_position,
                    row.final_commit_sequence,
                )
                .with_bundle_lease(lease));
            }
        }
        let bundle_path = self.layout.node_log_bundle_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            &row.bundle_digest,
        );
        let metadata = self.layout.store().head(&bundle_path).await?;
        if metadata.size > self.limits.max_plan_bytes {
            return Err(Error::Node("recovery bundle exceeds limit"));
        }
        let disk_reservation = self.recovery_disk.try_reserve(metadata.size)?;
        let temporary = match &self.recovery_scratch {
            Some(directory) => tempfile::Builder::new()
                .prefix(".crab-recovery-")
                .tempfile_in(directory)?,
            None => tempfile::NamedTempFile::new()?,
        };
        let temporary = temporary.into_temp_path();
        self.layout
            .store()
            .download_to_path_bounded(&bundle_path, temporary.as_ref(), self.limits.max_plan_bytes)
            .await?;
        let limits = self.limits;
        let decoded = tokio::task::spawn_blocking(move || {
            crab_ltx::bundle::Bundle::decode_temp_file_with_digest(
                temporary,
                row.bundle_digest,
                limits,
            )
        })
        .await
        .map_err(crab_ltx::CrabError::from)?;
        let bundle = match decoded {
            Ok(bundle) => bundle,
            Err(crab_ltx::CrabError::ChecksumMismatch) => {
                return Err(Error::Node("recovery bundle digest differs"));
            }
            Err(error) => return Err(error.into()),
        };
        Ok(crab_ltx::RecoveryOverlay::new(
            row.predecessor,
            bundle,
            row.final_position,
            row.final_commit_sequence,
        )
        .with_disk_reservation(disk_reservation))
    }
}

struct RecoveryManifest {
    leader_session: SessionId,
    log_epoch: u64,
    cells: Vec<ManifestCell>,
}

struct ManifestCell {
    application: [u8; 16],
    cell: [u8; 32],
    incarnation: [u8; 16],
    cell_epoch: u64,
    first_node_sequence: u64,
    last_node_sequence: u64,
    predecessor: crab_ltx::RootRef,
    final_position: crab_ltx::Position,
    final_commit_sequence: u64,
    bundle_digest: [u8; 32],
}

impl RecoveryManifest {
    fn encode(&self) -> Result<Vec<u8>> {
        let body = serde_json::to_vec(&RawManifest::from(self))?;
        if body.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(Error::Node("recovery manifest exceeds limit"));
        }
        Ok(body)
    }

    fn decode(body: &[u8]) -> Result<Self> {
        if body.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(Error::Node("recovery manifest exceeds limit"));
        }
        let raw: RawManifest = serde_json::from_slice(body)?;
        let manifest = Self::try_from(raw)?;
        if manifest.encode()? != body {
            return Err(Error::Node("recovery manifest is not canonical"));
        }
        Ok(manifest)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    version: u32,
    leader_session: String,
    log_epoch: String,
    cells: Vec<RawManifestCell>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifestCell {
    application: String,
    cell: String,
    incarnation: String,
    cell_epoch: String,
    first_node_sequence: String,
    last_node_sequence: String,
    predecessor_digest: String,
    predecessor_txid: String,
    predecessor_checksum: String,
    predecessor_commit_sequence: String,
    final_txid: String,
    final_checksum: String,
    final_commit_sequence: String,
    bundle_digest: String,
}

impl From<&RecoveryManifest> for RawManifest {
    fn from(manifest: &RecoveryManifest) -> Self {
        Self {
            version: 1,
            leader_session: hex(manifest.leader_session.as_bytes()),
            log_epoch: manifest.log_epoch.to_string(),
            cells: manifest
                .cells
                .iter()
                .map(|cell| RawManifestCell {
                    application: hex(&cell.application),
                    cell: hex(&cell.cell),
                    incarnation: hex(&cell.incarnation),
                    cell_epoch: cell.cell_epoch.to_string(),
                    first_node_sequence: cell.first_node_sequence.to_string(),
                    last_node_sequence: cell.last_node_sequence.to_string(),
                    predecessor_digest: hex(&cell.predecessor.digest),
                    predecessor_txid: cell.predecessor.position.txid.to_string(),
                    predecessor_checksum: hex(&cell.predecessor.position.checksum.to_be_bytes()),
                    predecessor_commit_sequence: cell.predecessor.commit_sequence.to_string(),
                    final_txid: cell.final_position.txid.to_string(),
                    final_checksum: hex(&cell.final_position.checksum.to_be_bytes()),
                    final_commit_sequence: cell.final_commit_sequence.to_string(),
                    bundle_digest: hex(&cell.bundle_digest),
                })
                .collect(),
        }
    }
}

impl TryFrom<RawManifest> for RecoveryManifest {
    type Error = Error;

    fn try_from(raw: RawManifest) -> Result<Self> {
        if raw.version != 1 || raw.cells.is_empty() || raw.cells.len() > 1_024 {
            return Err(Error::Node("invalid recovery manifest shape"));
        }
        let leader_session = SessionId::from_bytes(unhex(&raw.leader_session)?);
        let log_epoch = decimal(&raw.log_epoch)?;
        let mut cells = Vec::with_capacity(raw.cells.len());
        for cell in raw.cells {
            let cell_id = unhex(&cell.cell)?;
            let incarnation = unhex(&cell.incarnation)?;
            cells.push(ManifestCell {
                application: unhex(&cell.application)?,
                cell: cell_id,
                incarnation,
                cell_epoch: decimal(&cell.cell_epoch)?,
                first_node_sequence: decimal(&cell.first_node_sequence)?,
                last_node_sequence: decimal(&cell.last_node_sequence)?,
                predecessor: crab_ltx::RootRef {
                    cell: cell_id,
                    incarnation,
                    digest: unhex(&cell.predecessor_digest)?,
                    position: crab_ltx::Position {
                        txid: decimal(&cell.predecessor_txid)?,
                        checksum: u64::from_be_bytes(unhex(&cell.predecessor_checksum)?),
                    },
                    commit_sequence: decimal(&cell.predecessor_commit_sequence)?,
                },
                final_position: crab_ltx::Position {
                    txid: decimal(&cell.final_txid)?,
                    checksum: u64::from_be_bytes(unhex(&cell.final_checksum)?),
                },
                final_commit_sequence: decimal(&cell.final_commit_sequence)?,
                bundle_digest: unhex(&cell.bundle_digest)?,
            });
        }
        let manifest = Self {
            leader_session,
            log_epoch,
            cells,
        };
        if leader_session.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || manifest.cells.iter().any(|cell| {
                cell.cell_epoch == 0
                    || cell.first_node_sequence == 0
                    || cell.first_node_sequence > cell.last_node_sequence
                    || cell.final_position.txid <= cell.predecessor.position.txid
                    || cell.final_commit_sequence <= cell.predecessor.commit_sequence
            })
        {
            return Err(Error::Node("invalid recovery manifest values"));
        }
        Ok(manifest)
    }
}

async fn publish_immutable(
    layout: &CellStorageLayout,
    path: &object_store::path::Path,
    body: &[u8],
    limit: u64,
    summary: &mut RecoveryPublicationSummary,
) -> Result<()> {
    if body.len() as u64 > limit {
        return Err(Error::Node("recovery object exceeds limit"));
    }
    summary.object_writes = summary
        .object_writes
        .checked_add(1)
        .ok_or(Error::Capacity("recovery object write count"))?;
    match layout
        .store()
        .create_strict(path, Bytes::copy_from_slice(body))
        .await
    {
        Ok(()) => Ok(()),
        Err(create_error) => {
            summary.object_reads = summary
                .object_reads
                .checked_add(1)
                .ok_or(Error::Capacity("recovery object read count"))?;
            match layout.store().get_with_etag_bounded(path, limit).await {
                Ok((existing, _)) if existing.as_ref() == body => Ok(()),
                Ok(_) => Err(Error::Node("recovery digest path contains different bytes")),
                Err(StorageError::NotFound { .. }) => Err(create_error.into()),
                Err(error) => Err(error.into()),
            }
        }
    }
}

async fn publish_bundle_immutable(
    layout: &CellStorageLayout,
    path: &object_store::path::Path,
    bundle: &crab_ltx::bundle::Bundle,
    summary: &mut RecoveryPublicationSummary,
) -> Result<()> {
    let store = layout.store();
    let digest = bundle.digest();
    let size = bundle.len();
    summary.object_reads = summary
        .object_reads
        .checked_add(1)
        .ok_or(Error::Capacity("recovery object read count"))?;
    match store.verify_size_and_hash(path, size, &digest).await {
        Ok(()) => return Ok(()),
        Err(StorageError::NotFound { .. }) => {}
        Err(StorageError::CorruptObject { .. }) => {
            return Err(Error::Node("recovery bundle path contains different bytes"));
        }
        Err(error) => return Err(error.into()),
    }

    let staged = object_store::path::Path::from(format!("{path}.staging"));
    let cancel = tokio_util::sync::CancellationToken::new();
    let upload = store
        .put_multipart_source_retry(
            &staged,
            bundle.upload_source(),
            size,
            digest,
            MULTIPART_BYTES,
            &cancel,
            None,
        )
        .await;
    summary.object_writes = summary
        .object_writes
        .checked_add(2)
        .ok_or(Error::Capacity("recovery object write count"))?;
    if let Err(error) = upload {
        return match cleanup_staged(store, &staged).await {
            Ok(()) => Err(error.into()),
            Err(cleanup_error) => Err(cleanup_error),
        };
    }
    let promotion = store
        .promote_staged_content_addressed_object(&staged, path, digest, size)
        .await;
    match cleanup_staged(store, &staged).await {
        Err(error) => Err(error),
        Ok(()) => promotion.map(|_| ()).map_err(Into::into),
    }
}

async fn cleanup_staged(
    store: &crab_storage::Store,
    path: &object_store::path::Path,
) -> Result<()> {
    match store.delete(path).await {
        Ok(()) | Err(StorageError::NotFound { .. }) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn runtime_root(root: crab_ltx::RootRef) -> RootRef {
    RootRef {
        digest: Digest::from_bytes(root.digest),
        txid: root.position.txid,
        checksum: root.position.checksum,
        commit_sequence: root.commit_sequence,
    }
}

fn decimal(value: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| Error::Node("invalid recovery manifest decimal"))?;
    if parsed.to_string() != value {
        return Err(Error::Node("noncanonical recovery manifest decimal"));
    }
    Ok(parsed)
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn unhex<const N: usize>(value: &str) -> Result<[u8; N]> {
    if value.len() != N * 2 {
        return Err(Error::Node("invalid recovery manifest hex length"));
    }
    let mut decoded = [0_u8; N];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        decoded[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(decoded)
}

fn nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(Error::Node("invalid recovery manifest hex")),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use object_store::{ObjectStoreExt, memory::InMemory, path::Path};

    use super::*;
    use crate::node::log::{RecoveryBase, build_recovery_overlays};

    struct RecoveryFixture {
        inner: Arc<InMemory>,
        layout: CellStorageLayout,
        replica: crab_ltx::CellReplica,
        manifests: RecoveryManifestStore,
        pinned: PinnedRecoveryCell,
        publication: RecoveryPublicationSummary,
        base: crab_ltx::RootRef,
        final_position: crab_ltx::Position,
    }

    async fn recovery_fixture() -> RecoveryFixture {
        recovery_fixture_with_store(None).await
    }

    struct MemoryArtifactStore {
        limits: crab_ltx::Limits,
        reject_retain: bool,
        bundles: Mutex<BTreeMap<RecoveryArtifactKey, Vec<u8>>>,
        loads: AtomicU64,
    }

    struct MemoryArtifactLease;

    impl crab_ltx::bundle::BundleLease for MemoryArtifactLease {}

    impl RecoveryArtifactStore for MemoryArtifactStore {
        fn retain(&self, key: RecoveryArtifactKey, bundle: crab_ltx::bundle::Bundle) -> Result<()> {
            if self.reject_retain {
                return Err(Error::Capacity("artifact test store"));
            }
            let bytes = bundle.read_all()?;
            self.bundles
                .lock()
                .map_err(|_| Error::Node("artifact test store lock poisoned"))?
                .insert(key, bytes.to_vec());
            Ok(())
        }

        fn load(&self, key: &RecoveryArtifactKey) -> Result<Option<RecoveryArtifact>> {
            let bytes = self
                .bundles
                .lock()
                .map_err(|_| Error::Node("artifact test store lock poisoned"))?
                .get(key)
                .cloned();
            let Some(bytes) = bytes else {
                return Ok(None);
            };
            self.loads.fetch_add(1, Ordering::Relaxed);
            let bundle = crab_ltx::bundle::Bundle::decode(bytes, self.limits)?;
            Ok(Some(RecoveryArtifact::new(
                bundle,
                Arc::new(MemoryArtifactLease),
            )))
        }
    }

    async fn recovery_fixture_with_store(
        artifacts: Option<Arc<dyn RecoveryArtifactStore>>,
    ) -> RecoveryFixture {
        let limits = crab_ltx::Limits::default();
        let directory = tempfile::TempDir::new().unwrap();
        let mut database =
            crab_ltx::Db::open(&directory.path().join("cell.sqlite"), limits).unwrap();
        database
            .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
            .unwrap();
        let first = database.capture().unwrap();
        let inner = Arc::new(InMemory::new());
        let store = crab_storage::Store::new(inner.clone());
        let application = [3; 16];
        let cell = [4; 32];
        let incarnation = [5; 16];
        let layout = CellStorageLayout::new(store, Path::from("root"), application);
        let replica =
            crab_ltx::CellReplica::new(layout.clone(), cell, incarnation, limits).unwrap();
        let base = replica.prepare(None, &first, 1, 1).await.unwrap().root();
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO values_ VALUES (1)", [])?;
                Ok(())
            })
            .unwrap();
        let tail = database.capture().unwrap();
        let segment = tail.segments.first().unwrap();
        let frame = crab_ltx::encode_node_frame(
            crab_ltx::NodeFrameScope {
                leader_session: [1; 16],
                log_epoch: 2,
                node_sequence: 1,
                application,
                cell,
                incarnation,
                cell_epoch: 6,
                commit_sequence: 2,
            },
            segment.info().clone(),
            Bytes::from(std::fs::read(segment.path()).unwrap()),
            limits,
        )
        .unwrap();
        let recovered = build_recovery_overlays(
            vec![frame],
            &[RecoveryBase {
                application,
                cell_epoch: 6,
                root: base,
            }],
            limits,
        )
        .unwrap();
        let manifests = RecoveryManifestStore::new(layout.clone(), limits);
        let manifests = artifacts.map_or(manifests.clone(), |store| {
            manifests.with_recovery_artifacts(store)
        });
        let pinned = manifests
            .pin_with_summary(SessionId::from_bytes([1; 16]), 2, recovered)
            .await
            .unwrap();
        let publication = pinned.summary;
        let mut pinned = pinned.cells;
        database.close().unwrap();
        RecoveryFixture {
            inner,
            layout,
            replica,
            manifests,
            pinned: pinned.pop().unwrap(),
            publication,
            base,
            final_position: tail.position,
        }
    }

    async fn load_error(fixture: &RecoveryFixture, recovery: &RecoveryOverlayRef) -> Error {
        match fixture
            .manifests
            .load_overlay(fixture.pinned.cell, fixture.pinned.incarnation, recovery)
            .await
        {
            Ok(_) => panic!("corrupt recovery input must not load"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn pinned_manifest_reopens_exact_overlay_and_prepares_successor() {
        let fixture = recovery_fixture().await;
        assert!(fixture.publication.bundle_bytes > 0);
        assert!(fixture.publication.object_reads > 0);
        assert!(fixture.publication.object_writes > 0);
        let overlay = fixture
            .manifests
            .load_overlay(
                fixture.pinned.cell,
                fixture.pinned.incarnation,
                &fixture.pinned.recovery,
            )
            .await
            .unwrap();
        let prepared = fixture
            .replica
            .prepare_recovered_overlay(&overlay, 1)
            .await
            .unwrap();
        assert_eq!(prepared.predecessor(), Some(fixture.base));
        assert_eq!(prepared.root().position, fixture.final_position);
        let mut control = crate::control::Control::initial(
            fixture.pinned.cell,
            fixture.pinned.incarnation,
            crate::control::Owner {
                session: SessionId::from_bytes([1; 16]),
                endpoint: "https://dead.internal:8081".into(),
            },
            Digest::from_bytes([12; 32]),
            1,
        )
        .unwrap();
        control.state = crate::control::ControlState::Serving;
        control.root = Some(runtime_root(fixture.base));
        let attached = control
            .attach_recovery(fixture.pinned.recovery.clone())
            .unwrap();
        let takeover = attached
            .takeover(crate::control::Owner {
                session: SessionId::from_bytes([13; 16]),
                endpoint: "https://successor.internal:8081".into(),
            })
            .unwrap();
        let published = takeover.publish_recovery(&prepared, None).unwrap();
        assert_eq!(published.state, crate::control::ControlState::Recovering);
        assert!(published.recovery.is_none());
        assert_eq!(published.root.unwrap().txid, fixture.final_position.txid);
    }

    #[tokio::test]
    async fn verified_artifact_store_hit_reuses_the_pinned_bundle() {
        let limits = crab_ltx::Limits::default();
        let artifacts = Arc::new(MemoryArtifactStore {
            limits,
            reject_retain: false,
            bundles: Mutex::new(BTreeMap::new()),
            loads: AtomicU64::new(0),
        });
        let fixture = recovery_fixture_with_store(Some(
            Arc::clone(&artifacts) as Arc<dyn RecoveryArtifactStore>
        ))
        .await;
        let overlay = fixture
            .manifests
            .load_overlay(
                fixture.pinned.cell,
                fixture.pinned.incarnation,
                &fixture.pinned.recovery,
            )
            .await
            .unwrap();
        assert_eq!(artifacts.loads.load(Ordering::Relaxed), 1);
        let prepared = fixture
            .replica
            .prepare_recovered_overlay(&overlay, 1)
            .await
            .unwrap();
        assert_eq!(prepared.root().position, fixture.final_position);
    }

    #[tokio::test]
    async fn artifact_cache_failure_keeps_object_store_recovery_available() {
        let artifacts = Arc::new(MemoryArtifactStore {
            limits: crab_ltx::Limits::default(),
            reject_retain: true,
            bundles: Mutex::new(BTreeMap::new()),
            loads: AtomicU64::new(0),
        });
        let fixture = recovery_fixture_with_store(Some(
            Arc::clone(&artifacts) as Arc<dyn RecoveryArtifactStore>
        ))
        .await;
        let overlay = fixture
            .manifests
            .load_overlay(
                fixture.pinned.cell,
                fixture.pinned.incarnation,
                &fixture.pinned.recovery,
            )
            .await
            .unwrap();
        assert_eq!(artifacts.loads.load(Ordering::Relaxed), 0);
        assert_eq!(overlay.final_position(), fixture.final_position);
    }

    #[tokio::test]
    async fn loaded_overlay_holds_bundle_disk_reservation_until_drop() {
        let fixture = recovery_fixture().await;
        let scratch = tempfile::TempDir::new().unwrap();
        let recovery = &fixture.pinned.recovery;
        let manifest_path = fixture.layout.node_log_recovery_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            recovery.manifest_digest.as_bytes(),
        );
        let (body, _) = fixture
            .layout
            .store()
            .get_with_etag_bounded(&manifest_path, MAX_MANIFEST_BYTES)
            .await
            .unwrap();
        let manifest = RecoveryManifest::decode(&body).unwrap();
        let bundle_path = fixture.layout.node_log_bundle_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            &manifest.cells[0].bundle_digest,
        );
        let size = fixture
            .layout
            .store()
            .head(&bundle_path)
            .await
            .unwrap()
            .size;
        let budget = crab_ltx::DiskBudget::new(size);
        let manifests =
            RecoveryManifestStore::new(fixture.layout.clone(), crab_ltx::Limits::default())
                .with_recovery_disk(budget.clone())
                .with_recovery_scratch(scratch.path().to_owned());
        let overlay = manifests
            .load_overlay(fixture.pinned.cell, fixture.pinned.incarnation, recovery)
            .await
            .unwrap();
        assert_eq!(overlay.bundle().len(), size);
        assert_eq!(budget.used(), size);
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 1);
        drop(overlay);
        assert_eq!(budget.used(), 0);
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn load_overlay_rejects_corrupt_manifest_bytes() {
        let fixture = recovery_fixture().await;
        let recovery = &fixture.pinned.recovery;
        let path = fixture.layout.node_log_recovery_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            recovery.manifest_digest.as_bytes(),
        );
        fixture
            .inner
            .put(&path, Bytes::from_static(b"corrupt manifest").into())
            .await
            .unwrap();

        let error = load_error(&fixture, recovery).await;

        assert!(matches!(
            error,
            Error::Node("recovery manifest digest differs")
        ));
    }

    #[tokio::test]
    async fn load_overlay_rejects_self_consistent_manifest_metadata_change() {
        let fixture = recovery_fixture().await;
        let mut recovery = fixture.pinned.recovery.clone();
        let original_path = fixture.layout.node_log_recovery_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            recovery.manifest_digest.as_bytes(),
        );
        let (body, _) = fixture
            .layout
            .store()
            .get_with_etag_bounded(&original_path, MAX_MANIFEST_BYTES)
            .await
            .unwrap();
        let mut raw: RawManifest = serde_json::from_slice(&body).unwrap();
        raw.cells[0].final_commit_sequence = "3".into();
        let changed = serde_json::to_vec(&raw).unwrap();
        let digest = *blake3::hash(&changed).as_bytes();
        let changed_path = fixture.layout.node_log_recovery_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            &digest,
        );
        fixture
            .layout
            .store()
            .put(&changed_path, Bytes::from(changed))
            .await
            .unwrap();
        recovery.manifest_digest = Digest::from_bytes(digest);

        let error = load_error(&fixture, &recovery).await;

        assert!(matches!(
            error,
            Error::Node("recovery control pointer differs from manifest")
        ));
    }

    #[tokio::test]
    async fn load_overlay_rejects_corrupt_bundle_bytes() {
        let fixture = recovery_fixture().await;
        let recovery = &fixture.pinned.recovery;
        let manifest_path = fixture.layout.node_log_recovery_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            recovery.manifest_digest.as_bytes(),
        );
        let (body, _) = fixture
            .layout
            .store()
            .get_with_etag_bounded(&manifest_path, MAX_MANIFEST_BYTES)
            .await
            .unwrap();
        let manifest = RecoveryManifest::decode(&body).unwrap();
        let bundle_path = fixture.layout.node_log_bundle_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            &manifest.cells[0].bundle_digest,
        );
        fixture
            .inner
            .put(&bundle_path, Bytes::from_static(b"corrupt bundle").into())
            .await
            .unwrap();

        let error = load_error(&fixture, recovery).await;

        assert!(matches!(
            error,
            Error::Node("recovery bundle digest differs")
        ));
    }
}
