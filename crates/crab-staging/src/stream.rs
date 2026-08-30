//! Bounded-memory local-file staging shared by `crab add` and `crab adopt`.
//!
//! The staging schema keys chunk rows by file hash. Local files are read once
//! into a provisional staging key, then the staged rows are adopted under the
//! finalized Blake3 file hash after the read completes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use tokio::io::AsyncReadExt;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::StagingArea;
use crate::index::{PreparedChunkClaim, RecordingAuthorityState};
use crate::push_plan::{ExistingChunkCandidate, ExistingChunkLookup, move_prepared_xorb};
use crate::{Result, StagingError as CrabError};
use crab_xet::chunker::GearChunker;
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::{RunId, SerializedPayloadPool, XorbBuilder, XorbResult};
use crab_xet::xorb::format::{Chunk, ChunkPlacement};

type PreparedXorbWriteHook = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }
    Ok(())
}

/// File read buffer size for both streaming passes.
pub const READ_BUF_SIZE: usize = 1024 * 1024;

/// Maximum CDC chunks held before flushing a staging batch.
///
/// Default chunks target 64 KiB and are capped at 128 KiB. The byte cap keeps
/// peak memory near the previous worst case, while the larger chunk cap lets
/// typical data batch closer to 64 MiB before touching the staging index.
pub const STAGE_BATCH_CHUNKS: usize = 1024;
pub const STAGE_BATCH_TARGET_BYTES: u64 = 64 * 1024 * 1024;
const DIRECT_XORB_STAGE_BATCH_TARGET_BYTES: u64 = 8 * 1024 * 1024;

/// Optional progress counters updated while streaming a file.
#[derive(Clone, Default)]
pub struct StreamStageProgress {
    /// Bytes read while hashing the file.
    pub bytes_done: Option<Arc<AtomicU64>>,
    /// Bytes read while CDC chunking and staging the file.
    pub chunk_bytes_done: Option<Arc<AtomicU64>>,
    /// CDC chunks emitted while streaming the file.
    pub chunks_done: Option<Arc<AtomicU64>>,
    /// Optional add-time xorb builder used by local `crab add` fast paths.
    pub xorb_builder: Option<StreamStageXorbBuilder>,
    /// Optional generation-pinned remote classifier shared by add workers.
    pub existing_lookup: Option<Arc<dyn ExistingChunkLookup>>,
}

/// Factory for per-file add-time xorb builders.
#[derive(Clone)]
pub struct StreamStageXorbBuilder {
    build: Arc<dyn Fn() -> XorbBuilder + Send + Sync>,
    admission: Arc<Semaphore>,
    materialization: Arc<Semaphore>,
    serialized_payload_pool: SerializedPayloadPool,
    coordination: Option<Arc<PreparedAuthorityCoordination>>,
}

struct PreparedAuthorityCoordination {
    preparation_id: crate::AddPreparationId,
    changed: Notify,
    failed: AtomicBool,
}

impl StreamStageXorbBuilder {
    pub fn new<F>(max_in_flight: usize, build: F) -> Self
    where
        F: Fn() -> XorbBuilder + Send + Sync + 'static,
    {
        let max_in_flight = max_in_flight.max(1);
        Self {
            build: Arc::new(build),
            admission: Arc::new(Semaphore::new(max_in_flight)),
            materialization: Arc::new(Semaphore::new(1)),
            serialized_payload_pool: SerializedPayloadPool::new(1),
            coordination: None,
        }
    }

    #[must_use]
    pub fn bind_preparation(mut self, preparation_id: crate::AddPreparationId) -> Self {
        self.coordination = Some(Arc::new(PreparedAuthorityCoordination {
            preparation_id,
            changed: Notify::new(),
            failed: AtomicBool::new(false),
        }));
        self
    }

    #[must_use]
    pub fn preparation_id(&self) -> Option<&crate::AddPreparationId> {
        self.coordination
            .as_ref()
            .map(|coordination| &coordination.preparation_id)
    }

    pub fn fail_preparation(&self) {
        if let Some(coordination) = &self.coordination {
            coordination.failed.store(true, Relaxed);
            coordination.changed.notify_waiters();
        }
    }

    fn authority_changed(&self) {
        if let Some(coordination) = &self.coordination {
            coordination.changed.notify_waiters();
        }
    }

    async fn wait_for_recording_authority(
        &self,
        staging: &StagingArea,
        batch_id: &crate::StagingBatchId,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let Some(coordination) = &self.coordination else {
            return Ok(());
        };
        loop {
            let changed = coordination.changed.notified();
            match staging.recording_authority_state(batch_id)? {
                RecordingAuthorityState::Complete => return Ok(()),
                RecordingAuthorityState::Missing => {
                    return Err(CrabError::StagingCorrupt(format!(
                        "direct add batch {} has no complete prepared, segment, or remote authority",
                        batch_id.as_str()
                    )));
                }
                RecordingAuthorityState::Pending => {}
            }
            if coordination.failed.load(Relaxed) {
                return Err(CrabError::Internal(
                    "another file failed while resolving preparation-wide chunk ownership"
                        .to_owned(),
                ));
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(CrabError::Cancelled),
                () = changed => {}
            }
        }
    }

    async fn build(
        &self,
        cancel: &CancellationToken,
    ) -> Result<(OwnedSemaphorePermit, XorbBuilder)> {
        let admission = Arc::clone(&self.admission);
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
            permit = admission.acquire_owned() => permit.map_err(|_| {
                CrabError::Internal("stream xorb builder admission closed".to_owned())
            })?,
        };
        let builder =
            (self.build)().with_serialized_payload_pool(self.serialized_payload_pool.clone());
        Ok((permit, builder))
    }

    async fn acquire_materialization(
        &self,
        cancel: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit> {
        let materialization = Arc::clone(&self.materialization);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(CrabError::Cancelled),
            permit = materialization.acquire_owned() => permit.map_err(|_| {
                CrabError::Internal("stream xorb materialization admission closed".to_owned())
            }),
        }
    }

    #[cfg(test)]
    fn available_permits(&self) -> usize {
        self.admission.available_permits()
    }

    #[cfg(test)]
    fn available_materialization_permits(&self) -> usize {
        self.materialization.available_permits()
    }
}

/// Prepared xorb produced from the verified streaming pass.
#[derive(Debug)]
pub struct StreamStagePreparedXorb {
    pub hash: MerkleHash,
    pub payload_hash: String,
    pub bytes: u64,
    pub payload_path: PathBuf,
    pub placements: Vec<ChunkPlacement>,
}

struct StreamPreparedXorbRequest {
    permit: OwnedSemaphorePermit,
    sequence: usize,
    result: XorbResult,
}

struct StreamPreparedXorbWriter {
    sender: Option<mpsc::Sender<StreamPreparedXorbRequest>>,
    task: tokio::task::JoinHandle<Result<PreparedXorbWriteResult>>,
    next_sequence: usize,
    factory: StreamStageXorbBuilder,
}

struct PreparedXorbWriteResult {
    xorbs: Vec<StreamStagePreparedXorb>,
    duration: Duration,
}

impl StreamPreparedXorbWriter {
    fn spawn(
        staging_root: PathBuf,
        file_hash: MerkleHash,
        factory: &StreamStageXorbBuilder,
        hooks: StreamStageHooks,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel::<StreamPreparedXorbRequest>(1);
        let serialized_payload_pool = factory.serialized_payload_pool.clone();
        let task = tokio::spawn(async move {
            let mut prepared_xorbs = Vec::new();
            let mut duration = Duration::ZERO;
            while let Some(request) = receiver.recv().await {
                let StreamPreparedXorbRequest {
                    permit,
                    sequence,
                    result,
                } = request;
                let write_start = Instant::now();
                let write_result = stream_prepared_xorb(
                    &staging_root,
                    &file_hash,
                    sequence,
                    result,
                    hooks.before_prepared_xorb_write.as_ref(),
                )
                .await;
                duration = duration.saturating_add(write_start.elapsed());
                // Keep admission through pool return so the next rollover reuses
                // this allocation instead of briefly overlapping it.
                match write_result {
                    Ok((prepared, serialized)) => {
                        serialized_payload_pool.recycle_serialized_bytes(serialized);
                        drop(permit);
                        prepared_xorbs.push(prepared);
                    }
                    Err(error) => {
                        drop(permit);
                        return Err(error);
                    }
                }
            }
            Ok(PreparedXorbWriteResult {
                xorbs: prepared_xorbs,
                duration,
            })
        });
        Self {
            sender: Some(sender),
            task,
            next_sequence: 0,
            factory: factory.clone(),
        }
    }

    async fn submit_reserved(
        &mut self,
        result: XorbResult,
        permit: OwnedSemaphorePermit,
    ) -> Result<()> {
        let request = Self::reserved_request(&mut self.next_sequence, result, permit)?;
        let sender = self.sender.as_ref().ok_or_else(|| {
            CrabError::Internal("stream prepared xorb writer already finished".to_owned())
        })?;
        sender.send(request).await.map_err(|_| {
            CrabError::Internal(
                "stream prepared xorb writer stopped before accepting work".to_owned(),
            )
        })
    }

    fn submit_reserved_blocking(
        sender: &mpsc::Sender<StreamPreparedXorbRequest>,
        next_sequence: &mut usize,
        result: XorbResult,
        permit: OwnedSemaphorePermit,
    ) -> Result<()> {
        let request = Self::reserved_request(next_sequence, result, permit)?;
        // This path is confined to the packer's spawn_blocking task; Tokio
        // rejects blocking_send from an asynchronous execution context.
        sender.blocking_send(request).map_err(|_| {
            CrabError::Internal(
                "stream prepared xorb writer stopped before accepting work".to_owned(),
            )
        })
    }

    fn reserved_request(
        next_sequence: &mut usize,
        result: XorbResult,
        permit: OwnedSemaphorePermit,
    ) -> Result<StreamPreparedXorbRequest> {
        let sequence = *next_sequence;
        *next_sequence = next_sequence.checked_add(1).ok_or_else(|| {
            CrabError::Internal("stream prepared xorb sequence overflow".to_owned())
        })?;
        Ok(StreamPreparedXorbRequest {
            permit,
            sequence,
            result,
        })
    }

    async fn finish(mut self) -> Result<PreparedXorbWriteResult> {
        self.sender.take();
        self.task
            .await
            .map_err(|error| CrabError::Internal(format!("prepared xorb writer failed: {error}")))?
    }
}

/// Result of streaming and staging one local file.
#[derive(Debug)]
pub struct StreamStageResult {
    pub batch_id: crate::StagingBatchId,
    pub abs_path: PathBuf,
    pub file_hash: [u8; 32],
    pub size: u64,
    pub chunks: usize,
    pub recipe: crate::recipe::FileRecipe,
    pub prepared_xorbs: Vec<StreamStagePreparedXorb>,
    pub index_stat: Option<VerifiedIndexStat>,
    pub timings: StreamStageTimings,
    pub duration_ms: u64,
}

/// Cumulative worker time spent in the expensive add preparation phases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStageTimings {
    /// Cumulative time spent finding CDC boundaries in worker tasks.
    pub chunking_duration_ms: u64,
    /// Cumulative time spent consulting the bucket-global remote index.
    pub remote_lookup_duration_ms: u64,
    /// Cumulative time spent packing and finalizing compressed xorbs.
    pub compression_duration_ms: u64,
    /// Cumulative time spent materializing and installing prepared payloads.
    pub payload_write_duration_ms: u64,
}

#[derive(Default)]
struct StreamStageTimingAccumulator {
    chunking: Duration,
    remote_lookup: Duration,
    compression: Duration,
    payload_write: Duration,
}

impl StreamStageTimingAccumulator {
    fn finish(self) -> StreamStageTimings {
        StreamStageTimings {
            chunking_duration_ms: duration_ms(self.chunking),
            remote_lookup_duration_ms: duration_ms(self.remote_lookup),
            compression_duration_ms: duration_ms(self.compression),
            payload_write_duration_ms: duration_ms(self.payload_write),
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Git-index stat fields captured while the staged bytes were verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedIndexStat {
    pub stat: gix_index::entry::Stat,
    pub len: u64,
}

impl VerifiedIndexStat {
    pub fn from_path_no_follow(path: &Path) -> Option<Self> {
        let metadata = gix_index::fs::Metadata::from_path_no_follow(path).ok()?;
        let stat = gix_index::entry::Stat::from_fs(&metadata).ok()?;
        Some(Self {
            stat,
            len: metadata.len(),
        })
    }

    pub fn from_file(file: &std::fs::File) -> Option<Self> {
        let metadata = gix_index::fs::Metadata::from_file(file).ok()?;
        let stat = gix_index::entry::Stat::from_fs(&metadata).ok()?;
        Some(Self {
            stat,
            len: metadata.len(),
        })
    }
}

async fn open_regular_file_no_follow(path: &Path) -> Result<std::fs::File> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || open_regular_file_no_follow_sync(&path))
        .await
        .map_err(|error| CrabError::Internal(format!("no-follow open task failed: {error}")))?
}

fn open_regular_file_no_follow_sync(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(any(unix, windows)))]
    if path.symlink_metadata()?.file_type().is_symlink() {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "staging refuses symbolic links".to_owned(),
        });
    }

    let file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return CrabError::Configuration {
                key: path.display().to_string(),
                origin: "staging refuses symbolic links".to_owned(),
            };
        }
        CrabError::Io(error)
    })?;
    if !file.metadata()?.file_type().is_file() {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "staging requires a regular file".to_owned(),
        });
    }
    Ok(file)
}

/// Stream a local file into the staging area with bounded memory.
pub async fn stage_file_streaming(
    abs_path: &Path,
    repo_root: &Path,
    staging: &StagingArea,
    progress: StreamStageProgress,
    cancel: &CancellationToken,
) -> Result<StreamStageResult> {
    stage_file_streaming_inner(
        abs_path,
        repo_root,
        None,
        staging,
        progress,
        cancel,
        StreamStageHooks::default(),
    )
    .await
}

/// Stream a local file into staging while recording a caller-provided repo path.
pub async fn stage_file_streaming_as(
    abs_path: &Path,
    repo_root: &Path,
    repo_path: &Path,
    staging: &StagingArea,
    progress: StreamStageProgress,
    cancel: &CancellationToken,
) -> Result<StreamStageResult> {
    stage_file_streaming_inner(
        abs_path,
        repo_root,
        Some(repo_path),
        staging,
        progress,
        cancel,
        StreamStageHooks::default(),
    )
    .await
}

#[derive(Clone, Default)]
struct StreamStageHooks {
    after_batch_flush: Option<Arc<dyn Fn() + Send + Sync>>,
    before_prepared_xorb_write: Option<PreparedXorbWriteHook>,
}

#[cfg(test)]
async fn stage_file_streaming_with_hooks(
    abs_path: &Path,
    repo_root: &Path,
    staging: &StagingArea,
    progress: StreamStageProgress,
    cancel: &CancellationToken,
    hooks: StreamStageHooks,
) -> Result<StreamStageResult> {
    stage_file_streaming_inner(abs_path, repo_root, None, staging, progress, cancel, hooks).await
}

async fn stage_file_streaming_inner(
    abs_path: &Path,
    repo_root: &Path,
    repo_path: Option<&Path>,
    staging: &StagingArea,
    progress: StreamStageProgress,
    cancel: &CancellationToken,
    hooks: StreamStageHooks,
) -> Result<StreamStageResult> {
    let start = Instant::now();
    let rel_path = repo_path.map_or_else(
        || {
            abs_path
                .strip_prefix(repo_root)
                .unwrap_or(abs_path)
                .to_path_buf()
        },
        Path::to_path_buf,
    );
    let descriptor = open_regular_file_no_follow(abs_path)
        .await
        .map_err(|error| with_path(abs_path, error))?;
    let before_stream_stat =
        VerifiedIndexStat::from_file(&descriptor).ok_or_else(|| CrabError::Configuration {
            key: abs_path.display().to_string(),
            origin: "staging could not capture descriptor identity".to_owned(),
        })?;
    if VerifiedIndexStat::from_path_no_follow(abs_path) != Some(before_stream_stat) {
        return Err(CrabError::FileChangedDuringStaging {
            path: abs_path.display().to_string(),
            first_hash: "unread".to_owned(),
            second_hash: "unread".to_owned(),
            first_size: before_stream_stat.len,
            second_size: VerifiedIndexStat::from_path_no_follow(abs_path).map_or(0, |s| s.len),
        });
    }
    let mut read_file = tokio::fs::File::from_std(descriptor.try_clone()?);
    let batch_id = staging.create_batch()?;
    let mut xorb_builder = progress.xorb_builder.clone();
    let owned_preparation = if let Some(builder) = xorb_builder.as_mut()
        && builder.preparation_id().is_none()
    {
        let preparation_id = staging.create_add_preparation()?;
        *builder = builder.clone().bind_preparation(preparation_id.clone());
        Some(preparation_id)
    } else {
        None
    };
    if let Some(preparation_id) = xorb_builder
        .as_ref()
        .and_then(StreamStageXorbBuilder::preparation_id)
        && let Err(error) = staging.attach_add_preparation_batch(preparation_id, &batch_id)
    {
        let _ = staging.rollback_batch(&batch_id);
        return Err(with_path(abs_path, error));
    }
    let provisional_merkle = provisional_file_hash(&rel_path, &batch_id);
    let retired = staging.retire_file(&provisional_merkle)?;
    if retired.rows_deleted > 0 {
        debug!(
            file_hash = %provisional_merkle.hex(),
            rows = retired.rows_deleted,
            segments_touched = retired.segments_touched.len(),
            "retired stale provisional staging rows before streaming add"
        );
    }
    staging.unregister_file(&provisional_merkle)?;

    if xorb_builder.is_some() {
        cleanup_stream_prepared_xorb_dir(staging.root(), &provisional_merkle, abs_path);
    }
    staging.pre_register_file_with_path(&provisional_merkle, 0, &rel_path.to_string_lossy())?;
    let stage_result = stream_chunk_and_stage(
        &mut read_file,
        &provisional_merkle,
        staging,
        &batch_id,
        progress.bytes_done.as_ref(),
        progress.chunk_bytes_done.as_ref(),
        progress.chunks_done.as_ref(),
        xorb_builder.as_ref(),
        progress.existing_lookup.as_deref(),
        cancel,
        &hooks,
    )
    .await;

    let mut stage_stats = match stage_result {
        Ok(stats) => stats,
        Err(err) => {
            if let Some(builder) = &xorb_builder {
                builder.fail_preparation();
            }
            if let Some(preparation_id) = &owned_preparation {
                let _ = staging.abort_add_preparation(preparation_id);
            }
            cleanup_staged_file(
                staging,
                &provisional_merkle,
                abs_path,
                "stream staging failed",
            );
            cleanup_stream_prepared_xorb_dir(staging.root(), &provisional_merkle, abs_path);
            let _ = staging.rollback_batch(&batch_id);
            return Err(with_path(abs_path, err));
        }
    };

    let file_merkle = MerkleHash::from(stage_stats.file_hash);
    let recipe = match stage_stats
        .recipe_recorder
        .seal(file_merkle, stage_stats.size)
    {
        Ok(recipe) => recipe,
        Err(error) => {
            fail_stream_preparation(staging, xorb_builder.as_ref(), owned_preparation.as_ref());
            let _ = staging.rollback_batch(&batch_id);
            return Err(with_path(abs_path, error));
        }
    };
    let after_descriptor_stat = VerifiedIndexStat::from_file(&descriptor);
    let after_path_stat = VerifiedIndexStat::from_path_no_follow(abs_path);
    if after_descriptor_stat != Some(before_stream_stat) || after_path_stat != after_descriptor_stat
    {
        fail_stream_preparation(staging, xorb_builder.as_ref(), owned_preparation.as_ref());
        cleanup_staged_file(
            staging,
            &provisional_merkle,
            abs_path,
            "file changed during stream staging",
        );
        cleanup_stream_prepared_xorb_dir(staging.root(), &provisional_merkle, abs_path);
        let _ = staging.rollback_batch(&batch_id);
        let (second_hash, second_size) = stream_hash_file(abs_path, None, cancel)
            .await
            .map_err(|e| with_path(abs_path, e))?;
        return Err(CrabError::FileChangedDuringStaging {
            path: abs_path.display().to_string(),
            first_hash: file_merkle.hex(),
            second_hash: MerkleHash::from(second_hash).hex(),
            first_size: stage_stats.size,
            second_size,
        });
    }

    if let Err(err) = staging.adopt_staged_file(&provisional_merkle, &file_merkle, stage_stats.size)
    {
        fail_stream_preparation(staging, xorb_builder.as_ref(), owned_preparation.as_ref());
        cleanup_staged_file(
            staging,
            &provisional_merkle,
            abs_path,
            "failed to adopt provisional stream staging",
        );
        cleanup_stream_prepared_xorb_dir(staging.root(), &provisional_merkle, abs_path);
        let _ = staging.rollback_batch(&batch_id);
        return Err(with_path(abs_path, err));
    }

    let authority_write_start = Instant::now();
    if xorb_builder.is_some()
        && let Err(err) = persist_stream_prepared_authority(
            staging,
            &batch_id,
            &mut stage_stats.prepared_xorbs,
            xorb_builder
                .as_ref()
                .and_then(StreamStageXorbBuilder::preparation_id),
        )
        .await
    {
        if let Some(builder) = &xorb_builder {
            builder.fail_preparation();
        }
        if let Some(preparation_id) = &owned_preparation {
            let _ = staging.abort_add_preparation(preparation_id);
        }
        let _ = staging.retire_file_if_unleased(&file_merkle);
        cleanup_stream_prepared_xorb_dir(staging.root(), &provisional_merkle, abs_path);
        let _ = staging.rollback_batch(&batch_id);
        return Err(with_path(abs_path, err));
    }
    if xorb_builder.is_some() {
        stage_stats.timings.payload_write = stage_stats
            .timings
            .payload_write
            .saturating_add(authority_write_start.elapsed());
    }
    if let Some(builder) = &xorb_builder {
        builder.authority_changed();
    }

    if let Some(builder) = &xorb_builder
        && let Err(err) = builder
            .wait_for_recording_authority(staging, &batch_id, cancel)
            .await
    {
        builder.fail_preparation();
        if let Some(preparation_id) = &owned_preparation {
            let _ = staging.abort_add_preparation(preparation_id);
        }
        let _ = staging.retire_file_if_unleased(&file_merkle);
        let _ = staging.rollback_batch(&batch_id);
        return Err(with_path(abs_path, err));
    }

    if let Err(err) = staging.record_verified_recipe_lease(&batch_id, &rel_path, &recipe) {
        if let Some(builder) = &xorb_builder {
            builder.fail_preparation();
        }
        if let Some(preparation_id) = &owned_preparation {
            let _ = staging.abort_add_preparation(preparation_id);
        }
        let _ = staging.retire_file_if_unleased(&file_merkle);
        cleanup_stream_prepared_xorb_dir(staging.root(), &provisional_merkle, abs_path);
        let _ = staging.rollback_batch(&batch_id);
        return Err(with_path(abs_path, err));
    }

    if let Err(err) = staging.record_file_path(&file_merkle, &rel_path.to_string_lossy()) {
        fail_stream_preparation(staging, xorb_builder.as_ref(), owned_preparation.as_ref());
        cleanup_stream_prepared_xorb_dir(staging.root(), &provisional_merkle, abs_path);
        let _ = staging.rollback_batch(&batch_id);
        return Err(with_path(abs_path, err));
    }

    if let Some(preparation_id) = &owned_preparation
        && let Err(err) = staging.finalize_add_preparation(preparation_id)
    {
        let _ = staging.abort_add_preparation(preparation_id);
        let _ = staging.rollback_batch(&batch_id);
        return Err(with_path(abs_path, err));
    }

    let index_stat = match after_descriptor_stat {
        Some(after) if after.len == stage_stats.size => Some(after),
        _ => {
            debug!(path = %rel_path.display(), "leaving Git stat cache unpopulated");
            None
        }
    };

    Ok(StreamStageResult {
        batch_id,
        abs_path: abs_path.to_path_buf(),
        file_hash: stage_stats.file_hash,
        size: stage_stats.size,
        chunks: stage_stats.chunks,
        recipe,
        prepared_xorbs: stage_stats.prepared_xorbs,
        index_stat,
        timings: stage_stats.timings.finish(),
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

fn fail_stream_preparation(
    staging: &StagingArea,
    builder: Option<&StreamStageXorbBuilder>,
    owned_preparation: Option<&crate::AddPreparationId>,
) {
    if let Some(builder) = builder {
        builder.fail_preparation();
    }
    if let Some(preparation_id) = owned_preparation {
        let _ = staging.abort_add_preparation(preparation_id);
    }
}

fn cleanup_staged_file(
    staging: &StagingArea,
    file_merkle: &MerkleHash,
    path: &Path,
    reason: &'static str,
) {
    match staging.retire_file(file_merkle) {
        Ok(retired) => {
            if retired.rows_deleted > 0 {
                debug!(
                    file_hash = %file_merkle.hex(),
                    path = %path.display(),
                    rows = retired.rows_deleted,
                    segments_touched = retired.segments_touched.len(),
                    reason,
                    "retired partial staging rows after stream-stage failure"
                );
            }
        }
        Err(cleanup_err) => {
            warn!(
                file_hash = %file_merkle.hex(),
                path = %path.display(),
                error = %cleanup_err,
                reason,
                "failed to retire partial staging rows after stream-stage failure"
            );
        }
    }
    if let Err(cleanup_err) = staging.unregister_file(file_merkle) {
        warn!(
            file_hash = %file_merkle.hex(),
            path = %path.display(),
            error = %cleanup_err,
            reason,
            "failed to unregister partial staged file after stream-stage failure"
        );
    }
}

pub async fn stream_hash_file(
    path: &Path,
    bytes_done: Option<&Arc<AtomicU64>>,
    cancel: &CancellationToken,
) -> Result<([u8; 32], u64)> {
    let descriptor = open_regular_file_no_follow(path).await?;
    let before = VerifiedIndexStat::from_file(&descriptor);
    if VerifiedIndexStat::from_path_no_follow(path) != before {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "file path changed during descriptor-safe open".to_owned(),
        });
    }
    let mut file = tokio::fs::File::from_std(descriptor.try_clone()?);
    let mut buf = vec![0u8; READ_BUF_SIZE];
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;

    loop {
        check_cancelled(cancel)?;
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        if let Some(counter) = bytes_done {
            counter.fetch_add(n as u64, Relaxed);
        }
    }

    let after = VerifiedIndexStat::from_file(&descriptor);
    if before != after || VerifiedIndexStat::from_path_no_follow(path) != after {
        return Err(CrabError::FileChangedDuringStaging {
            path: path.display().to_string(),
            first_hash: MerkleHash::from(*hasher.finalize().as_bytes()).hex(),
            second_hash: "changed".to_owned(),
            first_size: total,
            second_size: after.map_or(0, |stat| stat.len),
        });
    }
    Ok((*hasher.finalize().as_bytes(), total))
}

#[expect(
    clippy::too_many_arguments,
    reason = "streaming state is explicit to keep chunking and staging allocation-free"
)]
async fn stream_chunk_and_stage(
    file: &mut tokio::fs::File,
    file_hash: &MerkleHash,
    staging: &StagingArea,
    batch_id: &crate::StagingBatchId,
    bytes_done: Option<&Arc<AtomicU64>>,
    chunk_bytes_done: Option<&Arc<AtomicU64>>,
    chunks_done: Option<&Arc<AtomicU64>>,
    xorb_builder_factory: Option<&StreamStageXorbBuilder>,
    existing_lookup: Option<&dyn ExistingChunkLookup>,
    cancel: &CancellationToken,
    hooks: &StreamStageHooks,
) -> Result<ChunkStageStats> {
    let mut xorb_writer = xorb_builder_factory.map(|factory| {
        StreamPreparedXorbWriter::spawn(
            staging.root().to_path_buf(),
            *file_hash,
            factory,
            hooks.clone(),
        )
    });
    let stage_result = stream_chunk_and_stage_producer(
        file,
        file_hash,
        staging,
        batch_id,
        bytes_done,
        chunk_bytes_done,
        chunks_done,
        xorb_builder_factory,
        existing_lookup,
        xorb_writer.as_mut(),
        cancel,
        hooks,
    )
    .await;
    let writer_result = match xorb_writer {
        Some(writer) => writer.finish().await,
        None => Ok(PreparedXorbWriteResult {
            xorbs: Vec::new(),
            duration: Duration::ZERO,
        }),
    };

    match stage_result {
        Ok(mut stats) => {
            let writes = writer_result?;
            stats.timings.payload_write =
                stats.timings.payload_write.saturating_add(writes.duration);
            stats.prepared_xorbs = writes.xorbs;
            Ok(stats)
        }
        Err(error) => {
            if let Err(writer_error) = writer_result {
                warn!(
                    producer_error = %error,
                    "stream producer also failed after prepared xorb writer failure"
                );
                return Err(writer_error);
            }
            Err(error)
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "producer state stays explicit while writer lifecycle is owned by its caller"
)]
async fn stream_chunk_and_stage_producer(
    file: &mut tokio::fs::File,
    file_hash: &MerkleHash,
    staging: &StagingArea,
    batch_id: &crate::StagingBatchId,
    bytes_done: Option<&Arc<AtomicU64>>,
    chunk_bytes_done: Option<&Arc<AtomicU64>>,
    chunks_done: Option<&Arc<AtomicU64>>,
    xorb_builder_factory: Option<&StreamStageXorbBuilder>,
    existing_lookup: Option<&dyn ExistingChunkLookup>,
    mut xorb_writer: Option<&mut StreamPreparedXorbWriter>,
    cancel: &CancellationToken,
    hooks: &StreamStageHooks,
) -> Result<ChunkStageStats> {
    let mut chunker = GearChunker::new();
    let mut hasher = blake3::Hasher::new();
    let mut batch: Vec<(MerkleHash, Bytes)> = Vec::with_capacity(STAGE_BATCH_CHUNKS);
    let mut batch_payload_bytes = 0u64;
    let mut recipe_recorder =
        crate::recipe::RecipeRecorder::new(crate::recipe::ChunkingPolicyId::XetGearV1_64KiB);
    let (_xorb_builder_permit, mut xorb_builder) = match xorb_builder_factory {
        Some(factory) => {
            let (permit, builder) = factory.build(cancel).await?;
            (Some(permit), Some(builder))
        }
        None => (None, None),
    };
    let mut chunk_index_offset = 0u64;
    let mut recipe_byte_offset = 0u64;
    let mut total_chunks = 0usize;
    let mut remote_existing_chunks = 0u64;
    let mut total = 0u64;
    let mut timings = StreamStageTimingAccumulator::default();

    loop {
        check_cancelled(cancel)?;
        let mut buf = BytesMut::with_capacity(READ_BUF_SIZE);
        let n = file.read_buf(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf);
        total += n as u64;
        if let Some(counter) = bytes_done {
            counter.fetch_add(n as u64, Relaxed);
        }
        if let Some(counter) = chunk_bytes_done {
            counter.fetch_add(n as u64, Relaxed);
        }
        let chunking_start = Instant::now();
        let emitted = chunker.feed_bytes(&buf.freeze());
        timings.chunking = timings.chunking.saturating_add(chunking_start.elapsed());
        append_emitted_chunks(
            emitted,
            &mut batch,
            &mut batch_payload_bytes,
            &mut recipe_recorder,
            chunks_done,
            &mut total_chunks,
        )?;
        flush_full_batches(
            file_hash,
            staging,
            batch_id,
            &mut batch,
            &mut batch_payload_bytes,
            &mut chunk_index_offset,
            &mut recipe_byte_offset,
            &mut xorb_builder,
            &mut xorb_writer,
            existing_lookup,
            &mut remote_existing_chunks,
            &mut timings,
            cancel,
            hooks,
        )
        .await?;
    }

    let chunking_start = Instant::now();
    let final_chunk = chunker.finalize();
    timings.chunking = timings.chunking.saturating_add(chunking_start.elapsed());
    if let Some(last) = final_chunk {
        append_emitted_chunks(
            vec![last],
            &mut batch,
            &mut batch_payload_bytes,
            &mut recipe_recorder,
            chunks_done,
            &mut total_chunks,
        )?;
    }
    flush_batch(
        file_hash,
        staging,
        batch_id,
        &mut batch,
        &mut batch_payload_bytes,
        &mut chunk_index_offset,
        &mut recipe_byte_offset,
        &mut xorb_builder,
        &mut xorb_writer,
        existing_lookup,
        &mut remote_existing_chunks,
        &mut timings,
        cancel,
        hooks,
    )
    .await?;

    if let Some(builder) = xorb_builder {
        let writer = xorb_writer.as_mut().ok_or_else(|| {
            CrabError::Internal("direct xorb writer disappeared before finalization".to_owned())
        })?;
        let mut permit = if builder.staged_count() > 0 {
            Some(writer.factory.acquire_materialization(cancel).await?)
        } else {
            None
        };
        let compression_start = Instant::now();
        let finalized = tokio::task::spawn_blocking(move || builder.finalize())
            .await
            .map_err(|error| {
                CrabError::Internal(format!("direct xorb finalization task failed: {error}"))
            })??;
        timings.compression = timings
            .compression
            .saturating_add(compression_start.elapsed());
        if finalized.len() > 1 {
            return Err(CrabError::Internal(format!(
                "direct xorb finalization produced {} results after batch draining",
                finalized.len()
            )));
        }
        for result in finalized {
            let permit = permit.take().ok_or_else(|| {
                CrabError::Internal("final xorb was sealed without admission".to_owned())
            })?;
            writer.submit_reserved(result, permit).await?;
        }
    }

    Ok(ChunkStageStats {
        file_hash: *hasher.finalize().as_bytes(),
        size: total,
        chunks: total_chunks,
        recipe_recorder,
        prepared_xorbs: Vec::new(),
        timings,
    })
}

fn provisional_file_hash(path: &Path, batch_id: &crate::StagingBatchId) -> MerkleHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab provisional add staging v1\0");
    hasher.update(&super::staging_path_bytes(path));
    hasher.update(batch_id.as_str().as_bytes());
    MerkleHash::from(*hasher.finalize().as_bytes())
}

struct ChunkStageStats {
    file_hash: [u8; 32],
    size: u64,
    chunks: usize,
    recipe_recorder: crate::recipe::RecipeRecorder,
    prepared_xorbs: Vec<StreamStagePreparedXorb>,
    timings: StreamStageTimingAccumulator,
}

fn append_emitted_chunks(
    chunks: Vec<crab_xet::chunker::Chunk>,
    batch: &mut Vec<(MerkleHash, Bytes)>,
    batch_payload_bytes: &mut u64,
    recipe_recorder: &mut crate::recipe::RecipeRecorder,
    chunks_done: Option<&Arc<AtomicU64>>,
    total_chunks: &mut usize,
) -> Result<()> {
    let count = chunks.len();
    for chunk in chunks {
        let hash = MerkleHash::from(<[u8; 32]>::from(chunk.hash));
        let size = chunk.data.len() as u64;
        recipe_recorder.record(hash, size)?;
        *batch_payload_bytes = batch_payload_bytes.checked_add(size).ok_or_else(|| {
            CrabError::StagingCorrupt("stream staging batch byte count overflow".to_owned())
        })?;
        batch.push((hash, chunk.data));
    }
    *total_chunks += count;
    if count > 0
        && let Some(counter) = chunks_done
    {
        counter.fetch_add(count as u64, Relaxed);
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "batch flush owns several mutable staging buffers without packing them into a transient struct"
)]
async fn flush_full_batches(
    file_hash: &MerkleHash,
    staging: &StagingArea,
    batch_id: &crate::StagingBatchId,
    batch: &mut Vec<(MerkleHash, Bytes)>,
    batch_payload_bytes: &mut u64,
    chunk_index_offset: &mut u64,
    recipe_byte_offset: &mut u64,
    xorb_builder: &mut Option<XorbBuilder>,
    xorb_writer: &mut Option<&mut StreamPreparedXorbWriter>,
    existing_lookup: Option<&dyn ExistingChunkLookup>,
    remote_existing_chunks: &mut u64,
    timings: &mut StreamStageTimingAccumulator,
    cancel: &CancellationToken,
    hooks: &StreamStageHooks,
) -> Result<()> {
    let target_bytes = stage_batch_target_bytes(xorb_builder.is_some());
    while batch_needs_flush(batch, *batch_payload_bytes, target_bytes) {
        let (flush_len, flush_bytes) = batch_flush_prefix(batch, target_bytes);
        let tail = batch.split_off(flush_len);
        let mut full = std::mem::replace(batch, tail);
        let mut full_payload_bytes = flush_bytes;
        *batch_payload_bytes = batch_payload_bytes.saturating_sub(flush_bytes);
        flush_batch(
            file_hash,
            staging,
            batch_id,
            &mut full,
            &mut full_payload_bytes,
            chunk_index_offset,
            recipe_byte_offset,
            xorb_builder,
            xorb_writer,
            existing_lookup,
            remote_existing_chunks,
            timings,
            cancel,
            hooks,
        )
        .await?;
        check_cancelled(cancel)?;
    }
    Ok(())
}

fn stage_batch_target_bytes(building_xorbs: bool) -> u64 {
    if building_xorbs {
        DIRECT_XORB_STAGE_BATCH_TARGET_BYTES
    } else {
        STAGE_BATCH_TARGET_BYTES
    }
}

fn batch_needs_flush(
    batch: &[(MerkleHash, Bytes)],
    batch_payload_bytes: u64,
    target_bytes: u64,
) -> bool {
    batch.len() >= STAGE_BATCH_CHUNKS || batch_payload_bytes >= target_bytes
}

fn batch_flush_prefix(batch: &[(MerkleHash, Bytes)], target_bytes: u64) -> (usize, u64) {
    batch_flush_prefix_from_lengths(batch.iter().map(|(_, data)| data.len()), target_bytes)
}

fn batch_flush_prefix_from_lengths(
    lengths: impl IntoIterator<Item = usize>,
    target_bytes: u64,
) -> (usize, u64) {
    let mut bytes = 0u64;
    let mut count = 0usize;
    for (idx, len) in lengths.into_iter().enumerate() {
        count = idx + 1;
        bytes = bytes.saturating_add(len as u64);
        if idx + 1 >= STAGE_BATCH_CHUNKS || bytes >= target_bytes {
            return (idx + 1, bytes);
        }
    }
    (count, bytes)
}

#[expect(
    clippy::too_many_arguments,
    reason = "batch state and writer lifecycle stay explicit across the blocking pack boundary"
)]
async fn flush_batch(
    file_hash: &MerkleHash,
    staging: &StagingArea,
    batch_id: &crate::StagingBatchId,
    batch: &mut Vec<(MerkleHash, Bytes)>,
    batch_payload_bytes: &mut u64,
    chunk_index_offset: &mut u64,
    recipe_byte_offset: &mut u64,
    xorb_builder: &mut Option<XorbBuilder>,
    xorb_writer: &mut Option<&mut StreamPreparedXorbWriter>,
    existing_lookup: Option<&dyn ExistingChunkLookup>,
    remote_existing_chunks: &mut u64,
    timings: &mut StreamStageTimingAccumulator,
    cancel: &CancellationToken,
    hooks: &StreamStageHooks,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let batch_start = *chunk_index_offset;
    let lookup_start = Instant::now();
    let mut existing = classify_existing_batch(batch, existing_lookup, cancel).await?;
    timings.remote_lookup = timings.remote_lookup.saturating_add(lookup_start.elapsed());
    for (candidate, (_, data)) in existing.iter_mut().zip(batch.iter()) {
        if candidate.as_ref().is_some_and(|candidate| {
            u64::from(candidate.xorb_ref.uncompressed_size) != data.len() as u64
                || candidate.placement_id == [0; 32]
                || candidate.origin_proof_id == [0; 32]
        }) {
            *candidate = None;
        }
    }
    let remote_authority = batch
        .iter()
        .zip(existing.iter())
        .filter_map(|((chunk_hash, _), candidate)| {
            candidate.map(|candidate| (*chunk_hash, candidate))
        })
        .collect::<Vec<_>>();
    staging.append_recording_remote_chunks(batch_id, &remote_authority)?;
    *remote_existing_chunks = remote_existing_chunks
        .checked_add(remote_authority.len() as u64)
        .ok_or_else(|| {
            CrabError::StagingCorrupt("remote existing chunk count overflow".to_owned())
        })?;

    if xorb_builder.is_none() {
        let mut start = 0usize;
        while start < batch.len() {
            while start < batch.len() && existing[start].is_some() {
                start += 1;
            }
            if start == batch.len() {
                break;
            }
            let mut end = start + 1;
            while end < batch.len() && existing[end].is_none() {
                end += 1;
            }
            staging
                .stage_owned_chunks_batch_for_retired_file(
                    batch[start..end].to_vec(),
                    file_hash,
                    batch_start.checked_add(start as u64).ok_or_else(|| {
                        CrabError::StagingCorrupt(
                            "stream staging miss-run offset overflow".to_owned(),
                        )
                    })?,
                )
                .await?;
            start = end;
        }
    }
    let recipe_terms = batch
        .iter()
        .map(|(hash, data)| (*hash, data.len() as u64))
        .collect::<Vec<_>>();
    staging.append_recipe_recording_terms(
        batch_id,
        *chunk_index_offset,
        *recipe_byte_offset,
        &recipe_terms,
    )?;
    *recipe_byte_offset =
        recipe_terms
            .iter()
            .try_fold(*recipe_byte_offset, |offset, (_, size)| {
                offset.checked_add(*size).ok_or_else(|| {
                    CrabError::StagingCorrupt("stream recipe byte offset overflow".to_owned())
                })
            })?;
    let batch_len = u64::try_from(batch.len()).map_err(|_| {
        CrabError::StagingCorrupt(format!(
            "stream staging batch length {} cannot be represented",
            batch.len()
        ))
    })?;
    *chunk_index_offset = chunk_index_offset.checked_add(batch_len).ok_or_else(|| {
        CrabError::StagingCorrupt(format!(
            "stream staging chunk index overflow at offset {}",
            *chunk_index_offset
        ))
    })?;
    if xorb_builder.is_some() {
        let mut pack = vec![false; batch.len()];
        let coordination = xorb_writer
            .as_ref()
            .and_then(|writer| writer.factory.coordination.as_ref());
        if let Some(coordination) = coordination {
            let misses = batch
                .iter()
                .zip(existing.iter())
                .filter_map(|((hash, data), candidate)| {
                    candidate.is_none().then_some((*hash, data.len() as u64))
                })
                .collect::<Vec<_>>();
            let claims =
                staging.claim_prepared_chunks(&coordination.preparation_id, batch_id, &misses)?;
            let mut claims = claims.into_iter();
            for (index, candidate) in existing.iter().enumerate() {
                if candidate.is_some() {
                    continue;
                }
                let claim = claims.next().ok_or_else(|| {
                    CrabError::Internal(
                        "prepared ownership claim cardinality was truncated".to_owned(),
                    )
                })?;
                pack[index] = matches!(claim, PreparedChunkClaim::Claimed);
            }
            if claims.next().is_some() {
                return Err(CrabError::Internal(
                    "prepared ownership claim cardinality exceeded input".to_owned(),
                ));
            }
        } else {
            for (index, candidate) in existing.iter().enumerate() {
                pack[index] = candidate.is_none();
            }
        }
        let to_pack: Vec<_> = batch
            .iter()
            .zip(pack)
            .filter(|(_, should_pack)| *should_pack)
            .map(|((hash, data), _)| {
                (
                    Chunk {
                        hash: *hash,
                        data: data.clone(),
                    },
                    RunId(0),
                )
            })
            .collect();
        let mut builder = xorb_builder.take().ok_or_else(|| {
            CrabError::Internal("direct xorb builder disappeared before pack".to_owned())
        })?;
        let writer = xorb_writer.take().ok_or_else(|| {
            CrabError::Internal("direct xorb writer disappeared before pack".to_owned())
        })?;
        let writer_factory = writer.factory.clone();
        let sender = writer.sender.as_ref().cloned().ok_or_else(|| {
            CrabError::Internal("direct xorb writer already finished before pack".to_owned())
        })?;
        let mut next_sequence = writer.next_sequence;
        let runtime = tokio::runtime::Handle::current();
        let pack_cancel = cancel.clone();
        let compression_start = Instant::now();
        let (returned_builder, returned_sequence) = tokio::task::spawn_blocking(move || {
            builder.push_batch_with_rollover_admission(
                &to_pack,
                || runtime.block_on(writer_factory.acquire_materialization(&pack_cancel)),
                |result, permit| {
                    StreamPreparedXorbWriter::submit_reserved_blocking(
                        &sender,
                        &mut next_sequence,
                        result,
                        permit,
                    )
                },
            )?;
            Ok::<_, CrabError>((builder, next_sequence))
        })
        .await
        .map_err(|error| CrabError::Internal(format!("direct xorb pack task failed: {error}")))??;
        timings.compression = timings
            .compression
            .saturating_add(compression_start.elapsed());
        *xorb_builder = Some(returned_builder);
        writer.next_sequence = returned_sequence;
        *xorb_writer = Some(writer);
    }
    batch.clear();
    *batch_payload_bytes = 0;
    if let Some(hook) = &hooks.after_batch_flush {
        hook();
    }
    Ok(())
}

async fn classify_existing_batch(
    batch: &[(MerkleHash, Bytes)],
    lookup: Option<&dyn ExistingChunkLookup>,
    cancel: &CancellationToken,
) -> Result<Vec<Option<ExistingChunkCandidate>>> {
    let Some(lookup) = lookup else {
        return Ok(vec![None; batch.len()]);
    };
    let terms = batch
        .iter()
        .map(|(hash, data)| (*hash, data.len() as u64))
        .collect::<Vec<_>>();
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        result = lookup.lookup_existing_candidates(&terms) => result,
    };
    match result {
        Ok(candidates) if candidates.len() == batch.len() => Ok(candidates),
        Ok(candidates) => {
            warn!(
                returned = candidates.len(),
                requested = batch.len(),
                "remote chunk classifier returned malformed cardinality; packing batch locally"
            );
            Ok(vec![None; batch.len()])
        }
        Err(CrabError::Cancelled) => Err(CrabError::Cancelled),
        Err(error) => {
            warn!(
                error = %error,
                chunks = batch.len(),
                "remote chunk classifier unavailable; packing batch locally"
            );
            Ok(vec![None; batch.len()])
        }
    }
}

async fn persist_stream_prepared_authority(
    staging: &StagingArea,
    batch_id: &crate::StagingBatchId,
    prepared_xorbs: &mut [StreamStagePreparedXorb],
    preparation_id: Option<&crate::AddPreparationId>,
) -> Result<()> {
    let preparation_id = preparation_id.ok_or_else(|| {
        CrabError::Internal("direct prepared authority has no add preparation".to_owned())
    })?;
    for prepared in prepared_xorbs.iter_mut() {
        let written =
            move_prepared_xorb(staging.root(), &prepared.hash, &prepared.payload_path).await?;
        if written != prepared.bytes {
            return Err(CrabError::StagingCorrupt(format!(
                "stream-prepared xorb {} changed size while becoming authoritative: expected {} bytes, found {written}",
                prepared.hash.hex(),
                prepared.bytes
            )));
        }
    }
    staging.register_preparation_payloads(preparation_id, batch_id, prepared_xorbs)?;
    Ok(())
}

async fn stream_prepared_xorb(
    staging_root: &Path,
    file_hash: &MerkleHash,
    sequence: usize,
    result: XorbResult,
    before_write: Option<&PreparedXorbWriteHook>,
) -> Result<(StreamStagePreparedXorb, Bytes)> {
    let staging_root = staging_root.to_path_buf();
    let file_hash = *file_hash;
    let before_write = before_write.cloned();
    tokio::task::spawn_blocking(move || {
        stream_prepared_xorb_sync(
            &staging_root,
            &file_hash,
            sequence,
            result,
            before_write.as_ref(),
        )
    })
    .await
    .map_err(|error| CrabError::Internal(format!("prepared xorb write task failed: {error}")))?
}

fn stream_prepared_xorb_sync(
    staging_root: &Path,
    file_hash: &MerkleHash,
    sequence: usize,
    result: XorbResult,
    before_write: Option<&PreparedXorbWriteHook>,
) -> Result<(StreamStagePreparedXorb, Bytes)> {
    let XorbResult {
        bytes: serialized,
        hash,
        payload_digest: _,
        placements,
    } = result;
    let bytes = u64::try_from(serialized.len()).map_err(|_| {
        CrabError::Internal(format!(
            "prepared xorb {} length cannot be represented",
            hash.hex()
        ))
    })?;
    let payload_hash = blake3::hash(&serialized).to_hex().to_string();
    let payload_path = stream_prepared_xorb_path(staging_root, file_hash, &hash, sequence);
    if let Some(before_write) = before_write {
        before_write(&payload_path)?;
    }
    let parent = payload_path
        .parent()
        .ok_or_else(|| CrabError::Internal("stream prepared xorb path has no parent".to_owned()))?;
    std::fs::create_dir_all(parent)?;
    std::fs::write(&payload_path, &serialized)?;
    let prepared = StreamStagePreparedXorb {
        hash,
        payload_hash,
        bytes,
        payload_path,
        placements,
    };
    Ok((prepared, serialized))
}

fn stream_prepared_xorb_path(
    staging_root: &Path,
    file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    sequence: usize,
) -> PathBuf {
    staging_root
        .join("stream-prepared-xorbs")
        .join(file_hash.hex())
        .join(format!("{sequence:016x}-{}.xorb", xorb_hash.hex()))
}

pub fn cleanup_stream_prepared_xorbs(prepared_xorbs: &[StreamStagePreparedXorb]) {
    for prepared in prepared_xorbs {
        if !prepared
            .payload_path
            .components()
            .any(|component| component.as_os_str() == "stream-prepared-xorbs")
        {
            continue;
        }
        match std::fs::remove_file(&prepared.payload_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                path = %prepared.payload_path.display(),
                error = %e,
                "failed to remove stream-prepared xorb temp file"
            ),
        }
        if let Some(parent) = prepared.payload_path.parent() {
            match std::fs::remove_dir(parent) {
                Ok(()) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(e) => warn!(
                    path = %parent.display(),
                    error = %e,
                    "failed to remove stream-prepared xorb temp directory"
                ),
            }
        }
    }
}

fn cleanup_stream_prepared_xorb_dir(staging_root: &Path, file_hash: &MerkleHash, path: &Path) {
    let dir = staging_root
        .join("stream-prepared-xorbs")
        .join(file_hash.hex());
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(
            path = %path.display(),
            temp_dir = %dir.display(),
            error = %e,
            "failed to remove stream-prepared xorb temp directory"
        ),
    }
}

fn with_path(path: &Path, err: CrabError) -> CrabError {
    match err {
        CrabError::Io(e) => CrabError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        )),
        other => other,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::*;
    use crab_xet::xorb::builder::{CompressionPolicy, FixedCompression};
    use crab_xet::xorb::format::CompressionScheme;

    struct BatchRemoteLookup {
        first_only: bool,
    }

    struct MalformedRemoteLookup;

    #[async_trait::async_trait]
    impl ExistingChunkLookup for MalformedRemoteLookup {
        async fn lookup_existing_candidates(
            &self,
            _chunks: &[(MerkleHash, u64)],
        ) -> Result<Vec<Option<ExistingChunkCandidate>>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl ExistingChunkLookup for BatchRemoteLookup {
        async fn lookup_existing_candidates(
            &self,
            chunks: &[(MerkleHash, u64)],
        ) -> Result<Vec<Option<ExistingChunkCandidate>>> {
            Ok(chunks
                .iter()
                .enumerate()
                .map(|(index, (chunk_hash, size))| {
                    (!self.first_only || index == 0).then(|| {
                        let mut proof = <[u8; 32]>::from(*chunk_hash);
                        proof[0] ^= 0xA5;
                        ExistingChunkCandidate {
                            xorb_ref: crab_xet::xorb::format::XorbRef {
                                xorb_hash: MerkleHash::from(proof),
                                chunk_index: u32::from_le_bytes(proof[..4].try_into().unwrap()),
                                uncompressed_size: (*size).try_into().unwrap(),
                            },
                            placement_id: *blake3::hash(&proof).as_bytes(),
                            origin_proof_id: *blake3::hash(&proof).as_bytes(),
                        }
                    })
                })
                .collect())
        }
    }

    fn write_pattern_file(path: &Path, bytes: usize) {
        write_pattern_file_with_salt(path, bytes, 0);
    }

    fn write_pattern_file_with_salt(path: &Path, bytes: usize, salt: u64) {
        let mut file = std::fs::File::create(path).unwrap();
        let mut buf = vec![0u8; 1024 * 1024];
        let mut written = 0usize;
        while written < bytes {
            let n = (bytes - written).min(buf.len());
            for (i, byte) in buf[..n].iter_mut().enumerate() {
                let offset = (written + i) as u64;
                *byte = offset
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(salt.wrapping_mul(1_103_515_245))
                    .wrapping_shr(11) as u8;
            }
            file.write_all(&buf[..n]).unwrap();
            written += n;
        }
    }

    fn direct_hash_and_chunks(data: &[u8]) -> ([u8; 32], Vec<MerkleHash>) {
        let hash = *blake3::hash(data).as_bytes();
        let mut chunker = GearChunker::new();
        let mut chunks = Vec::new();
        for block in data.chunks(READ_BUF_SIZE) {
            chunks.extend(chunker.feed(block));
        }
        if let Some(last) = chunker.finalize() {
            chunks.push(last);
        }
        (
            hash,
            chunks
                .into_iter()
                .map(|c| MerkleHash::from(<[u8; 32]>::from(c.hash)))
                .collect(),
        )
    }

    fn multi_batch_fixture_bytes() -> usize {
        usize::try_from(STAGE_BATCH_TARGET_BYTES)
            .unwrap()
            .saturating_add(READ_BUF_SIZE * 2)
    }

    fn recipe_pairs(staging: &StagingArea, result: &StreamStageResult) -> Vec<(MerkleHash, u64)> {
        let mut pairs = Vec::new();
        let mut next = 0u64;
        while next < result.recipe.chunk_count() {
            let page = staging.recipe_page(&result.recipe, next).unwrap();
            pairs.extend(
                page.chunks
                    .iter()
                    .map(|chunk| (chunk.chunk_hash, chunk.len)),
            );
            next = page.next_occurrence();
        }
        pairs
    }

    fn recipe_remote_chunks(
        staging: &StagingArea,
        recipe: &crate::recipe::FileRecipe,
    ) -> Vec<(MerkleHash, crate::push_plan::ExistingChunkCandidate)> {
        let mut chunks = Vec::new();
        let mut next = 0u64;
        while next < recipe.chunk_count() {
            let page = staging
                .recipe_remote_chunk_page(recipe, next)
                .expect("remote authority page");
            assert!(page.len() <= crate::recipe::RECIPE_PAGE_ENTRIES);
            chunks.extend(page);
            next += crate::recipe::RECIPE_PAGE_ENTRIES as u64;
        }
        chunks
    }

    #[test]
    fn direct_xorb_staging_uses_smaller_compression_batches() {
        assert_eq!(stage_batch_target_bytes(false), STAGE_BATCH_TARGET_BYTES);
        assert_eq!(
            stage_batch_target_bytes(true),
            DIRECT_XORB_STAGE_BATCH_TARGET_BYTES
        );
    }

    #[test]
    fn phase_timings_convert_after_accumulating_submillisecond_work() {
        let mut timings = StreamStageTimingAccumulator::default();
        timings.chunking = timings.chunking.saturating_add(Duration::from_micros(600));
        timings.chunking = timings.chunking.saturating_add(Duration::from_micros(600));

        assert_eq!(timings.finish().chunking_duration_ms, 1);
    }

    #[tokio::test]
    async fn xorb_builder_admission_is_shared_and_cancel_safe() {
        let builders = StreamStageXorbBuilder::new(1, XorbBuilder::new);
        let cancel = CancellationToken::new();
        let (first_permit, _first_builder) = builders.build(&cancel).await.unwrap();

        assert_eq!(builders.available_permits(), 0);

        let waiting_cancel = CancellationToken::new();
        waiting_cancel.cancel();
        let error = match builders.build(&waiting_cancel).await {
            Ok(_) => panic!("cancelled admission unexpectedly built a xorb builder"),
            Err(error) => error,
        };
        assert!(matches!(error, CrabError::Cancelled));

        drop(first_permit);
        let (_second_permit, _second_builder) = builders.build(&cancel).await.unwrap();
    }

    #[tokio::test]
    async fn prepared_xorb_ownership_is_globally_bounded() {
        let builders = StreamStageXorbBuilder::new(2, XorbBuilder::new);
        let first = builders
            .acquire_materialization(&CancellationToken::new())
            .await
            .unwrap();
        let waiting_builders = builders.clone();
        let waiting = tokio::spawn(async move {
            waiting_builders
                .acquire_materialization(&CancellationToken::new())
                .await
        });

        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        assert_eq!(builders.available_materialization_permits(), 0);

        drop(first);
        let second = waiting.await.unwrap().unwrap();
        assert_eq!(builders.available_materialization_permits(), 0);
        drop(second);
        assert_eq!(builders.available_materialization_permits(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_stream_partial_overlap_has_one_canonical_prepared_placement() {
        let dir = tempfile::tempdir().unwrap();
        let shared_path = dir.path().join("shared.bin");
        let first_prefix_path = dir.path().join("first-prefix.bin");
        let second_prefix_path = dir.path().join("second-prefix.bin");
        write_pattern_file_with_salt(&shared_path, 3 * 1024 * 1024, 17);
        write_pattern_file_with_salt(&first_prefix_path, 1024 * 1024, 29);
        write_pattern_file_with_salt(&second_prefix_path, 1024 * 1024, 41);
        let shared = std::fs::read(shared_path).unwrap();
        let mut first_bytes = std::fs::read(first_prefix_path).unwrap();
        first_bytes.extend_from_slice(&shared);
        let mut second_bytes = std::fs::read(second_prefix_path).unwrap();
        second_bytes.extend_from_slice(&shared);
        let first_path = dir.path().join("first.bin");
        let second_path = dir.path().join("second.bin");
        std::fs::write(&first_path, &first_bytes).unwrap();
        std::fs::write(&second_path, &second_bytes).unwrap();

        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();
        let preparation = staging.create_add_preparation().unwrap();
        let builders =
            StreamStageXorbBuilder::new(2, XorbBuilder::new).bind_preparation(preparation.clone());
        let cancel = CancellationToken::new();
        let first = stage_file_streaming(
            &first_path,
            dir.path(),
            &staging,
            StreamStageProgress {
                xorb_builder: Some(builders.clone()),
                ..StreamStageProgress::default()
            },
            &cancel,
        );
        let second = stage_file_streaming(
            &second_path,
            dir.path(),
            &staging,
            StreamStageProgress {
                xorb_builder: Some(builders),
                ..StreamStageProgress::default()
            },
            &cancel,
        );
        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap();
        let second = second.unwrap();
        staging.finalize_add_preparation(&preparation).unwrap();
        staging.mark_batch_published(&first.batch_id).unwrap();
        staging.mark_batch_published(&second.batch_id).unwrap();

        let first_recipe = recipe_pairs(&staging, &first);
        let second_recipe = recipe_pairs(&staging, &second);
        let first_hashes = first_recipe
            .iter()
            .map(|(hash, _)| *hash)
            .collect::<std::collections::HashSet<_>>();
        let second_hashes = second_recipe
            .iter()
            .map(|(hash, _)| *hash)
            .collect::<std::collections::HashSet<_>>();
        let shared_hashes = first_hashes
            .intersection(&second_hashes)
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert!(
            shared_hashes.len() > 8,
            "fixture must share multiple CDC chunks"
        );

        let first_plan = staging
            .load_file_push_plan(&MerkleHash::from(first.file_hash))
            .await
            .unwrap()
            .unwrap();
        let second_plan = staging
            .load_file_push_plan(&MerkleHash::from(second.file_hash))
            .await
            .unwrap()
            .unwrap();
        let placement_map = |plan: &crate::push_plan::FilePushPlan| {
            plan.prepared_xorbs
                .iter()
                .flat_map(|xorb| {
                    xorb.placements.iter().map(|placement| {
                        (
                            MerkleHash::from_hex(&placement.chunk_hash).unwrap(),
                            MerkleHash::from_hex(&placement.xorb_hash).unwrap(),
                        )
                    })
                })
                .collect::<std::collections::HashMap<_, _>>()
        };
        let first_placements = placement_map(&first_plan);
        let second_placements = placement_map(&second_plan);
        for chunk_hash in shared_hashes {
            assert_eq!(
                first_placements.get(&chunk_hash),
                second_placements.get(&chunk_hash),
                "shared chunk must resolve to one canonical prepared xorb"
            );
        }
        for (result, expected) in [(&first, &first_bytes), (&second, &second_bytes)] {
            let hashes = recipe_pairs(&staging, result)
                .into_iter()
                .map(|(hash, _)| hash)
                .collect::<Vec<_>>();
            let reconstructed = staging
                .get_chunks_batch(&hashes)
                .await
                .unwrap()
                .into_iter()
                .flat_map(|(_, bytes)| bytes)
                .collect::<Vec<_>>();
            assert_eq!(&reconstructed, expected);
        }
    }

    #[tokio::test]
    async fn concurrent_prepared_xorb_writers_preserve_file_order() {
        fn results(salt: u8) -> Vec<XorbResult> {
            let policy = Arc::new(FixedCompression::new(CompressionScheme::None))
                as Arc<dyn CompressionPolicy>;
            let mut builder = XorbBuilder::with_policy(policy)
                .with_size_bounds(8, 8)
                .with_max_overshoot(0);
            for byte in 1_u8..=3 {
                let value = byte.wrapping_add(salt);
                builder
                    .push(
                        &Chunk {
                            hash: MerkleHash::from([value; 32]),
                            data: Bytes::from(vec![value; 8]),
                        },
                        RunId(0),
                    )
                    .unwrap();
            }
            builder.finalize().unwrap()
        }

        let dir = tempfile::tempdir().unwrap();
        let builders = StreamStageXorbBuilder::new(2, XorbBuilder::new);
        let first_results = results(0);
        let second_results = results(10);
        let first_hashes = first_results
            .iter()
            .map(|result| result.hash)
            .collect::<Vec<_>>();
        let second_hashes = second_results
            .iter()
            .map(|result| result.hash)
            .collect::<Vec<_>>();
        let mut first = StreamPreparedXorbWriter::spawn(
            dir.path().to_path_buf(),
            MerkleHash::from([40; 32]),
            &builders,
            StreamStageHooks::default(),
        );
        let mut second = StreamPreparedXorbWriter::spawn(
            dir.path().to_path_buf(),
            MerkleHash::from([41; 32]),
            &builders,
            StreamStageHooks::default(),
        );
        let cancel = CancellationToken::new();
        for (left, right) in first_results.into_iter().zip(second_results) {
            let left_permit = builders.acquire_materialization(&cancel).await.unwrap();
            first.submit_reserved(left, left_permit).await.unwrap();
            let right_permit = builders.acquire_materialization(&cancel).await.unwrap();
            second.submit_reserved(right, right_permit).await.unwrap();
        }
        let first = first.finish().await.unwrap();
        let second = second.finish().await.unwrap();

        assert_eq!(
            first
                .xorbs
                .iter()
                .map(|prepared| prepared.hash)
                .collect::<Vec<_>>(),
            first_hashes
        );
        assert_eq!(
            second
                .xorbs
                .iter()
                .map(|prepared| prepared.hash)
                .collect::<Vec<_>>(),
            second_hashes
        );
        for prepared in [&first, &second] {
            for (sequence, prepared) in prepared.xorbs.iter().enumerate() {
                assert!(prepared.payload_path.exists());
                assert!(
                    prepared
                        .payload_path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(&format!("{sequence:016x}-"))
                );
            }
        }
    }

    #[test]
    fn batch_flush_prefix_uses_byte_cap_before_chunk_cap() {
        let half_target = usize::try_from(STAGE_BATCH_TARGET_BYTES / 2).unwrap();
        let (count, bytes) = batch_flush_prefix_from_lengths(
            [half_target, half_target, READ_BUF_SIZE],
            STAGE_BATCH_TARGET_BYTES,
        );

        assert_eq!(count, 2);
        assert_eq!(bytes, STAGE_BATCH_TARGET_BYTES);
    }

    #[test]
    fn batch_flush_prefix_uses_chunk_cap_for_small_chunks() {
        let (count, bytes) = batch_flush_prefix_from_lengths(
            std::iter::repeat_n(1usize, STAGE_BATCH_CHUNKS + 1),
            STAGE_BATCH_TARGET_BYTES,
        );

        assert_eq!(count, STAGE_BATCH_CHUNKS);
        assert_eq!(bytes, STAGE_BATCH_CHUNKS as u64);
    }

    #[tokio::test]
    async fn empty_file_produces_empty_hash_and_zero_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, []).unwrap();
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.size, 0);
        assert_eq!(result.file_hash, *blake3::hash(b"").as_bytes());
        assert_eq!(result.chunks, 0);
        assert_eq!(result.recipe.chunk_count(), 0);
        let file_hash = MerkleHash::from(result.file_hash);
        assert!(staging.chunks_for_file(&file_hash).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn staging_rejects_symlink_without_recording_target_bytes() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.bin");
        let link = dir.path().join("link.bin");
        std::fs::write(&outside, b"outside-worktree-secret").unwrap();
        symlink(&outside, &link).unwrap();
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let err = stage_file_streaming(
            &link,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        let stats = tokio::task::block_in_place(|| staging.stats()).unwrap();
        assert_eq!(stats.file_count, 0);
        assert_eq!(stats.chunk_count, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn path_replacement_during_stream_never_publishes_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replace.bin");
        let displaced = dir.path().join("displaced.bin");
        let fixture_bytes = multi_batch_fixture_bytes();
        write_pattern_file(&path, fixture_bytes);
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let path_for_hook = path.clone();
        let displaced_for_hook = displaced.clone();
        let replaced = Arc::new(AtomicBool::new(false));
        let replaced_for_hook = Arc::clone(&replaced);
        let hooks = StreamStageHooks {
            after_batch_flush: Some(Arc::new(move || {
                if !replaced_for_hook.swap(true, Ordering::SeqCst) {
                    std::fs::rename(&path_for_hook, &displaced_for_hook).unwrap();
                    std::fs::write(&path_for_hook, b"replacement bytes").unwrap();
                }
            })),
            ..StreamStageHooks::default()
        };

        let err = stage_file_streaming_with_hooks(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
            hooks,
        )
        .await
        .unwrap_err();

        assert!(replaced.load(Ordering::SeqCst));
        assert!(matches!(err, CrabError::FileChangedDuringStaging { .. }));
        let stats = tokio::task::block_in_place(|| staging.stats()).unwrap();
        assert_eq!(stats.file_count, 0);
        assert_eq!(stats.chunk_count, 0);
    }

    #[tokio::test]
    async fn streaming_matches_direct_chunking_for_medium_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("medium.bin");
        write_pattern_file(&path, 2 * 1024 * 1024);
        let data = std::fs::read(&path).unwrap();
        let (expected_hash, expected_chunks) = direct_hash_and_chunks(&data);
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.file_hash, expected_hash);
        assert_eq!(result.size, data.len() as u64);
        assert_eq!(result.chunks, expected_chunks.len());
        let result_pairs = recipe_pairs(&staging, &result);
        let result_chunk_hashes: Vec<_> = result_pairs.iter().map(|(hash, _)| *hash).collect();
        assert_eq!(result_chunk_hashes, expected_chunks);
        let staged = staging
            .chunks_for_file(&MerkleHash::from(result.file_hash))
            .unwrap();
        assert_eq!(staged, expected_chunks);
        let staged_with_sizes = staging
            .chunks_for_file_with_sizes(&MerkleHash::from(result.file_hash))
            .unwrap();
        assert_eq!(result_pairs, staged_with_sizes);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rolling_back_one_shared_recipe_batch_preserves_other_lease() {
        let dir = tempfile::tempdir().unwrap();
        let first_path = dir.path().join("first.bin");
        let second_path = dir.path().join("second.bin");
        write_pattern_file(&first_path, 2 * 1024 * 1024);
        std::fs::copy(&first_path, &second_path).unwrap();
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let first = stage_file_streaming(
            &first_path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let second = stage_file_streaming(
            &second_path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(first.file_hash, second.file_hash);
        assert_ne!(first.batch_id, second.batch_id);

        staging.rollback_batch(&second.batch_id).unwrap();
        let file_hash = MerkleHash::from(first.file_hash);
        assert_eq!(
            staging.chunks_for_file(&file_hash).unwrap(),
            recipe_pairs(&staging, &first)
                .iter()
                .map(|(chunk_hash, _)| *chunk_hash)
                .collect::<Vec<_>>()
        );

        staging.rollback_batch(&first.batch_id).unwrap();
        assert!(staging.chunks_for_file(&file_hash).unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn committed_retirement_preserves_same_recipe_owned_by_open_batch() {
        let dir = tempfile::tempdir().unwrap();
        let first_path = dir.path().join("published.bin");
        let second_path = dir.path().join("open.bin");
        write_pattern_file(&first_path, 2 * 1024 * 1024);
        std::fs::copy(&first_path, &second_path).unwrap();
        let root = dir.path().join(".crab/staging");
        let staging = StagingArea::open(root.clone()).await.unwrap();
        let first = stage_file_streaming(
            &first_path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let second = stage_file_streaming(
            &second_path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        staging.mark_batch_published(&first.batch_id).unwrap();
        staging.close().await.unwrap();

        let reader = crate::StagingAreaReadOnly::open(root).await.unwrap();
        let file_hash = MerkleHash::from(first.file_hash);
        reader
            .create_push_snapshot("push-first", std::slice::from_ref(&first.recipe))
            .unwrap();
        reader.commit_push_snapshot("push-first").unwrap();
        let retained = reader.retire_push_snapshot("push-first").await.unwrap();
        assert!(retained.is_empty());
        assert!(!reader.chunks_for_file(&file_hash).unwrap().is_empty());
        reader.remove_push_snapshot("push-first").unwrap();

        reader.mark_batch_published(&second.batch_id).unwrap();
        reader
            .create_push_snapshot("push-second", std::slice::from_ref(&second.recipe))
            .unwrap();
        reader.commit_push_snapshot("push-second").unwrap();
        let retired = reader.retire_push_snapshot("push-second").await.unwrap();
        assert_eq!(retired.len(), 1);
        assert!(retired[0].rows_deleted > 0);
        assert!(reader.chunks_for_file(&file_hash).unwrap().is_empty());
        reader.remove_push_snapshot("push-second").unwrap();
    }

    #[tokio::test]
    async fn streaming_as_stages_source_outside_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let cache = dir.path().join("cache");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&cache).unwrap();
        let source = cache.join("overlay-backing.bin");
        write_pattern_file(&source, 2 * 1024 * 1024);
        let data = std::fs::read(&source).unwrap();
        let (expected_hash, expected_chunks) = direct_hash_and_chunks(&data);
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = stage_file_streaming_as(
            &source,
            &repo,
            Path::new("models/model.bin"),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.abs_path, source);
        assert_eq!(result.file_hash, expected_hash);
        assert_eq!(result.size, data.len() as u64);
        assert_eq!(result.chunks, expected_chunks.len());
        let staged = staging
            .chunks_for_file(&MerkleHash::from(result.file_hash))
            .unwrap();
        assert_eq!(staged, expected_chunks);
    }

    #[tokio::test]
    async fn streaming_prepares_xorbs_from_verified_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prepared.bin");
        write_pattern_file(&path, 2 * 1024 * 1024);
        let source = std::fs::read(&path).unwrap();
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
                ..StreamStageProgress::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(!result.prepared_xorbs.is_empty());
        let prepared_chunks: Vec<_> = result
            .prepared_xorbs
            .iter()
            .flat_map(|prepared| {
                prepared
                    .placements
                    .iter()
                    .map(|placement| (placement.chunk_hash, u64::from(placement.uncompressed_size)))
            })
            .collect();
        let result_pairs = recipe_pairs(&staging, &result);
        let unique_staged_chunks: std::collections::HashSet<_> =
            result_pairs.iter().copied().collect();
        let unique_prepared_chunks: std::collections::HashSet<_> =
            prepared_chunks.iter().copied().collect();

        assert_eq!(unique_prepared_chunks, unique_staged_chunks);
        let file = staging
            .list_files()
            .unwrap()
            .into_iter()
            .find(|file| file.file_hash == result.file_hash)
            .unwrap();
        assert_eq!(file.committed_chunks + file.pending_chunks, 0);
        staging
            .mark_batch_published(&result.batch_id)
            .expect("publish staged recipe");
        let plan = staging
            .load_file_push_plan(&MerkleHash::from(result.file_hash))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(plan.chunk_count, result.recipe.chunk_count());
        assert_eq!(plan.sequence_hash().unwrap(), result.recipe.sequence_hash());
        let hashes = result_pairs
            .iter()
            .map(|(hash, _)| *hash)
            .collect::<Vec<_>>();
        let reconstructed = staging
            .get_chunks_batch(&hashes)
            .await
            .unwrap()
            .into_iter()
            .flat_map(|(_, bytes)| bytes)
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, source);
        for prepared in &result.prepared_xorbs {
            assert!(!prepared.payload_path.exists());
            let payload = std::fs::read(crate::push_plan::prepared_xorb_path(
                staging.root(),
                &prepared.hash,
            ))
            .unwrap();
            assert_eq!(prepared.bytes, payload.len() as u64);
            assert_eq!(
                prepared.payload_hash,
                blake3::hash(&payload).to_hex().to_string()
            );
            assert!(
                prepared
                    .placements
                    .iter()
                    .all(|placement| placement.xorb_hash == prepared.hash)
            );
        }
        let staging_root = staging.root().to_path_buf();
        staging.close().await.unwrap();

        let reopened = crate::StagingAreaReadOnly::open(staging_root)
            .await
            .unwrap();
        assert_eq!(
            reopened
                .chunks_for_file_with_sizes(&MerkleHash::from(result.file_hash))
                .unwrap(),
            result_pairs
        );
        let reconstructed = reopened
            .get_chunks_batch(&hashes)
            .await
            .unwrap()
            .into_iter()
            .flat_map(|(_, bytes)| bytes)
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, source);
    }

    #[tokio::test]
    async fn proven_remote_chunks_create_no_local_payload_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote-only.bin");
        write_pattern_file(&path, 2 * 1024 * 1024);
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                existing_lookup: Some(Arc::new(BatchRemoteLookup { first_only: false })),
                ..StreamStageProgress::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(result.prepared_xorbs.is_empty());
        let file = staging
            .list_files()
            .unwrap()
            .into_iter()
            .find(|file| file.file_hash == result.file_hash)
            .unwrap();
        assert_eq!(file.committed_chunks + file.pending_chunks, 0);
        staging.mark_batch_published(&result.batch_id).unwrap();
        let plan = staging
            .load_file_push_plan(&MerkleHash::from(result.file_hash))
            .await
            .unwrap()
            .unwrap();
        let unique_recipe_chunks = recipe_pairs(&staging, &result)
            .into_iter()
            .map(|(hash, _)| hash)
            .collect::<std::collections::HashSet<_>>();
        assert!(plan.existing.is_empty());
        let indexed_remote_chunks = recipe_remote_chunks(&staging, &result.recipe)
            .into_iter()
            .map(|(hash, _)| hash)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(indexed_remote_chunks, unique_recipe_chunks);
        assert!(plan.prepared_xorbs.is_empty());
    }

    #[tokio::test]
    async fn mixed_remote_batch_packs_only_unknown_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.bin");
        write_pattern_file(&path, 4 * 1024 * 1024);
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
                existing_lookup: Some(Arc::new(BatchRemoteLookup { first_only: true })),
                ..StreamStageProgress::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        staging.mark_batch_published(&result.batch_id).unwrap();
        let plan = staging
            .load_file_push_plan(&MerkleHash::from(result.file_hash))
            .await
            .unwrap()
            .unwrap();
        assert!(plan.existing.is_empty());
        let existing = recipe_remote_chunks(&staging, &result.recipe)
            .into_iter()
            .map(|(hash, _candidate)| hash)
            .collect::<std::collections::HashSet<_>>();
        let prepared = plan
            .prepared_xorbs
            .iter()
            .flat_map(|xorb| xorb.placements.iter())
            .map(|placement| MerkleHash::from_hex(&placement.chunk_hash).unwrap())
            .collect::<std::collections::HashSet<_>>();
        let recipe = recipe_pairs(&staging, &result)
            .into_iter()
            .map(|(hash, _)| hash)
            .collect::<std::collections::HashSet<_>>();

        assert!(!existing.is_empty());
        assert!(!prepared.is_empty());
        assert!(existing.is_disjoint(&prepared));
        assert_eq!(
            existing
                .union(&prepared)
                .copied()
                .collect::<std::collections::HashSet<_>>(),
            recipe
        );
        let file = staging
            .list_files()
            .unwrap()
            .into_iter()
            .find(|file| file.file_hash == result.file_hash)
            .unwrap();
        assert_eq!(file.committed_chunks + file.pending_chunks, 0);
    }

    #[tokio::test]
    async fn malformed_remote_lookup_conservatively_packs_every_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("malformed-lookup.bin");
        write_pattern_file(&path, 2 * 1024 * 1024);
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
                existing_lookup: Some(Arc::new(MalformedRemoteLookup)),
                ..StreamStageProgress::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        staging.mark_batch_published(&result.batch_id).unwrap();
        let plan = staging
            .load_file_push_plan(&MerkleHash::from(result.file_hash))
            .await
            .unwrap()
            .unwrap();

        assert!(plan.existing.is_empty());
        let prepared = plan
            .prepared_xorbs
            .iter()
            .flat_map(|xorb| xorb.placements.iter())
            .map(|placement| placement.chunk_hash.as_str())
            .collect::<std::collections::HashSet<_>>();
        let recipe = recipe_pairs(&staging, &result)
            .into_iter()
            .map(|(hash, _)| hash.hex())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(prepared, recipe.iter().map(String::as_str).collect());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restaging_path_reclaims_prepared_payload_after_snapshot_release() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let first_source = dir.path().join("first.bin");
        let second_source = dir.path().join("second.bin");
        write_pattern_file(&first_source, 2 * 1024 * 1024);
        write_pattern_file(&second_source, 3 * 1024 * 1024);
        let staging_root = dir.path().join(".crab/staging");
        let logical_path = Path::new("models/model.bin");

        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let first = stage_file_streaming_as(
            &first_source,
            &repo,
            logical_path,
            &staging,
            StreamStageProgress {
                xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
                ..StreamStageProgress::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        staging.mark_batch_published(&first.batch_id).unwrap();
        let first_hash = MerkleHash::from(first.file_hash);
        let prepared_paths = first
            .prepared_xorbs
            .iter()
            .map(|prepared| crate::push_plan::prepared_xorb_path(staging.root(), &prepared.hash))
            .collect::<Vec<_>>();
        assert!(prepared_paths.iter().all(|path| path.is_file()));
        staging.close().await.unwrap();

        let reader = crate::StagingAreaReadOnly::open(staging_root.clone())
            .await
            .unwrap();
        reader
            .create_push_snapshot("push-first", std::slice::from_ref(&first.recipe))
            .unwrap();
        drop(reader);

        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let second = stage_file_streaming_as(
            &second_source,
            &repo,
            logical_path,
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        staging.mark_batch_published(&second.batch_id).unwrap();
        assert!(prepared_paths.iter().all(|path| path.is_file()));
        assert!(
            staging
                .published_recipe_for_file(&first_hash)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            staging
                .published_recipe_for_file(&MerkleHash::from(second.file_hash))
                .unwrap(),
            Some(second.recipe.clone())
        );
        let second_prepared = staging
            .load_file_push_plan(&MerkleHash::from(second.file_hash))
            .await
            .unwrap()
            .unwrap()
            .prepared_xorbs
            .into_iter()
            .map(|xorb| xorb.hash().unwrap())
            .collect::<std::collections::HashSet<_>>();
        staging.close().await.unwrap();

        let reader = crate::StagingAreaReadOnly::open(staging_root)
            .await
            .unwrap();
        let retired = reader
            .discard_open_push_snapshot("push-first")
            .await
            .unwrap();
        assert_eq!(retired.len(), 1);
        for prepared in &first.prepared_xorbs {
            let path = crate::push_plan::prepared_xorb_path(reader.root(), &prepared.hash);
            assert_eq!(
                path.exists(),
                second_prepared.contains(&prepared.hash),
                "restage must retain only content-addressed payloads leased by the new recipe"
            );
        }
        let second_chunks = reader
            .chunks_for_file(&MerkleHash::from(second.file_hash))
            .unwrap();
        assert_eq!(second_chunks.len(), second.chunks);
    }

    #[tokio::test]
    async fn prepared_xorb_writer_failure_publishes_no_authority() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("write-failure.bin");
        write_pattern_file(&path, multi_batch_fixture_bytes());
        let staging_root = dir.path().join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let hooks = StreamStageHooks {
            before_prepared_xorb_write: Some(Arc::new(|_| {
                Err(CrabError::Internal(
                    "injected prepared xorb failure".to_owned(),
                ))
            })),
            ..StreamStageHooks::default()
        };

        let error = stage_file_streaming_with_hooks(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
                ..StreamStageProgress::default()
            },
            &CancellationToken::new(),
            hooks,
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("injected prepared xorb failure"),
            "got {error:?}"
        );
        assert!(staging.list_files().unwrap().is_empty());
        assert!(
            std::fs::read_dir(staging_root.join("stream-prepared-xorbs"))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true)
        );
        assert!(!staging_root.join("push-plans/payloads").exists());
    }

    #[tokio::test]
    async fn progress_counts_streaming_and_chunking_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.bin");
        write_pattern_file(&path, 2 * 1024 * 1024);
        let data_len = std::fs::metadata(&path).unwrap().len();
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();
        let stream_bytes = Arc::new(AtomicU64::new(0));
        let chunk_bytes = Arc::new(AtomicU64::new(0));
        let chunks = Arc::new(AtomicU64::new(0));

        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                bytes_done: Some(Arc::clone(&stream_bytes)),
                chunk_bytes_done: Some(Arc::clone(&chunk_bytes)),
                chunks_done: Some(Arc::clone(&chunks)),
                xorb_builder: None,
                existing_lookup: None,
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(stream_bytes.load(Ordering::Relaxed), data_len);
        assert_eq!(chunk_bytes.load(Ordering::Relaxed), data_len);
        assert_eq!(chunks.load(Ordering::Relaxed), result.chunks as u64);
    }

    #[tokio::test]
    async fn multi_batch_file_stages_contiguous_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        write_pattern_file(&path, multi_batch_fixture_bytes());
        let data = std::fs::read(&path).unwrap();
        let (_, expected_chunks) = direct_hash_and_chunks(&data);
        assert!(
            expected_chunks.len() > STAGE_BATCH_CHUNKS
                || data.len() as u64 > STAGE_BATCH_TARGET_BYTES,
            "fixture must span multiple staging batches"
        );
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let staged = staging
            .chunks_for_file(&MerkleHash::from(result.file_hash))
            .unwrap();
        assert_eq!(staged.len(), expected_chunks.len());
        assert_eq!(staged, expected_chunks);
        let staged_with_sizes = staging
            .chunks_for_file_with_sizes(&MerkleHash::from(result.file_hash))
            .unwrap();
        assert_eq!(recipe_pairs(&staging, &result), staged_with_sizes);
    }

    #[tokio::test]
    async fn changed_file_during_stream_fails_and_retires_provisional_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("changing.bin");
        let fixture_bytes = multi_batch_fixture_bytes();
        write_pattern_file(&path, fixture_bytes);
        let original = std::fs::read(&path).unwrap();
        let original_hash = *blake3::hash(&original).as_bytes();
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let path_for_hook = path.clone();
        let did_mutate = Arc::new(AtomicBool::new(false));
        let did_mutate_for_hook = Arc::clone(&did_mutate);
        let hooks = StreamStageHooks {
            after_batch_flush: Some(Arc::new(move || {
                if !did_mutate_for_hook.swap(true, Ordering::SeqCst) {
                    write_pattern_file_with_salt(&path_for_hook, fixture_bytes, 7);
                }
            })),
            ..StreamStageHooks::default()
        };

        let err = stage_file_streaming_with_hooks(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
            hooks,
        )
        .await
        .unwrap_err();

        assert!(did_mutate.load(Ordering::SeqCst));
        let mutated = std::fs::read(&path).unwrap();
        let mutated_hash = *blake3::hash(&mutated).as_bytes();
        match err {
            CrabError::FileChangedDuringStaging {
                path: err_path,
                first_hash,
                second_hash,
                first_size,
                second_size,
            } => {
                assert!(err_path.ends_with("changing.bin"));
                assert_ne!(first_hash, MerkleHash::from(original_hash).hex());
                assert_eq!(second_hash, MerkleHash::from(mutated_hash).hex());
                assert_eq!(first_size, original.len() as u64);
                assert_eq!(second_size, mutated.len() as u64);
            }
            other => panic!("expected FileChangedDuringStaging, got {other:?}"),
        }

        assert!(
            staging.list_files().unwrap().is_empty(),
            "provisional rows and file records must be retired after validation fails"
        );
        assert!(
            staging
                .chunks_for_file(&MerkleHash::from(mutated_hash))
                .unwrap()
                .is_empty(),
            "changed file must not be adopted after validation fails"
        );
    }

    #[tokio::test]
    async fn writable_reopen_rolls_back_every_batch_in_unfinished_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unfinished.bin");
        write_pattern_file(&path, 2 * 1024 * 1024);
        let staging_root = dir.path().join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let preparation = staging.create_add_preparation().unwrap();
        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                xorb_builder: Some(
                    StreamStageXorbBuilder::new(1, XorbBuilder::new).bind_preparation(preparation),
                ),
                ..StreamStageProgress::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let payloads = result
            .prepared_xorbs
            .iter()
            .map(|prepared| crate::push_plan::prepared_xorb_path(&staging_root, &prepared.hash))
            .collect::<Vec<_>>();
        assert!(payloads.iter().all(|path| path.is_file()));
        staging.close().await.unwrap();

        let recovered = StagingArea::open(staging_root).await.unwrap();

        assert!(recovered.list_files().unwrap().is_empty());
        assert!(payloads.iter().all(|path| !path.exists()));
        assert_eq!(
            recovered
                .lifecycle_health()
                .unwrap()
                .open_batches_without_publication,
            0
        );
    }

    #[tokio::test]
    async fn cancellation_after_batch_flush_retires_partial_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cancel.bin");
        write_pattern_file(&path, multi_batch_fixture_bytes());
        let data = std::fs::read(&path).unwrap();
        let file_hash = *blake3::hash(&data).as_bytes();
        let (_, expected_chunks) = direct_hash_and_chunks(&data);
        assert!(
            expected_chunks.len() > STAGE_BATCH_CHUNKS
                || data.len() as u64 > STAGE_BATCH_TARGET_BYTES,
            "fixture must flush at least one full staging batch"
        );
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let cancel_for_hook = cancel.clone();
        let did_cancel = Arc::new(AtomicBool::new(false));
        let did_cancel_for_hook = Arc::clone(&did_cancel);
        let hooks = StreamStageHooks {
            after_batch_flush: Some(Arc::new(move || {
                if !did_cancel_for_hook.swap(true, Ordering::SeqCst) {
                    cancel_for_hook.cancel();
                }
            })),
            ..StreamStageHooks::default()
        };

        let err = stage_file_streaming_with_hooks(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &cancel,
            hooks,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CrabError::Cancelled), "got {err:?}");
        assert!(did_cancel.load(Ordering::SeqCst));
        assert!(
            staging
                .chunks_for_file(&MerkleHash::from(file_hash))
                .unwrap()
                .is_empty(),
            "partial rows staged before cancellation must be retired"
        );
    }

    #[tokio::test]
    async fn cancellation_discards_partial_direct_xorb_authority() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cancel-direct.bin");
        write_pattern_file(&path, multi_batch_fixture_bytes());
        let staging_root = dir.path().join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let cancel = CancellationToken::new();
        let cancel_for_hook = cancel.clone();
        let hooks = StreamStageHooks {
            after_batch_flush: Some(Arc::new(move || cancel_for_hook.cancel())),
            ..StreamStageHooks::default()
        };

        let error = stage_file_streaming_with_hooks(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
                ..StreamStageProgress::default()
            },
            &cancel,
            hooks,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CrabError::Cancelled), "got {error:?}");
        assert!(staging.list_files().unwrap().is_empty());
        assert!(
            std::fs::read_dir(staging_root.join("stream-prepared-xorbs"))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true)
        );
        assert!(!staging_root.join("push-plans/payloads").exists());
    }

    #[tokio::test]
    #[ignore = "manual: stages 128 MiB and measures RSS; run with --ignored"]
    async fn streaming_stage_bounded_rss_on_128mib_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rss.bin");
        write_pattern_file(&path, 128 * 1024 * 1024);
        let baseline = memory_stats::memory_stats()
            .map(|s| s.physical_mem)
            .unwrap_or(0);
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let peak = memory_stats::memory_stats()
            .map(|s| s.physical_mem)
            .unwrap_or(0);
        let delta = peak.saturating_sub(baseline);
        assert_eq!(result.size, 128 * 1024 * 1024);
        if baseline > 0 {
            assert!(
                delta < 96 * 1024 * 1024,
                "RSS delta {delta} exceeded bounded streaming budget"
            );
        }
    }
}
