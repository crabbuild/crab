//! Prepared payload production, shared resource admission and durable installation.

use super::PreparedXorbWriteHook;
use crate::index::RecordingAuthorityState;
use crate::push_plan::{ExistingChunkLookup, move_prepared_xorb};
use crate::{Result, StagingArea, StagingError as CrabError};
use bytes::Bytes;
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::{SerializedPayloadPool, XorbBuilder, XorbResult};
use crab_xet::xorb::format::ChunkPlacement;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

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
    pub(super) build: Arc<dyn Fn() -> XorbBuilder + Send + Sync>,
    pub(super) admission: Arc<Semaphore>,
    pub(super) materialization: Arc<Semaphore>,
    pub(super) promotion: Arc<Semaphore>,
    pub(super) serialized_payload_pool: SerializedPayloadPool,
    pub(super) coordination: Option<Arc<PreparedAuthorityCoordination>>,
}

pub(super) struct PreparedAuthorityCoordination {
    pub(super) preparation_id: crate::AddPreparationId,
    pub(super) changed: Notify,
    pub(super) failed: AtomicBool,
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
            promotion: Arc::new(Semaphore::new(1)),
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

    pub(super) fn authority_changed(&self) {
        if let Some(coordination) = &self.coordination {
            coordination.changed.notify_waiters();
        }
    }

    pub(super) async fn wait_for_recording_authority(
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

    pub(super) async fn build(
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

    pub(super) async fn acquire_materialization(
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

    pub(super) async fn acquire_promotion(&self) -> Result<OwnedSemaphorePermit> {
        Arc::clone(&self.promotion)
            .acquire_owned()
            .await
            .map_err(|_| CrabError::Internal("stream xorb promotion admission closed".to_owned()))
    }

    #[cfg(test)]
    pub(super) fn available_permits(&self) -> usize {
        self.admission.available_permits()
    }

    #[cfg(test)]
    pub(super) fn available_materialization_permits(&self) -> usize {
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

pub(super) struct StreamPreparedXorbRequest {
    permit: OwnedSemaphorePermit,
    sequence: usize,
    result: XorbResult,
}

pub(super) struct StreamPreparedXorbWriter {
    pub(super) sender: Option<mpsc::Sender<StreamPreparedXorbRequest>>,
    pub(super) task: tokio::task::JoinHandle<Result<PreparedXorbWriteResult>>,
    pub(super) next_sequence: usize,
    pub(super) factory: StreamStageXorbBuilder,
}

pub(super) struct PreparedXorbWriteResult {
    pub(super) xorbs: Vec<StreamStagePreparedXorb>,
    pub(super) duration: Duration,
}

impl StreamPreparedXorbWriter {
    pub(super) fn spawn(
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

    pub(super) async fn submit_reserved(
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

    pub(super) fn submit_reserved_blocking(
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

    pub(super) async fn finish(mut self) -> Result<PreparedXorbWriteResult> {
        self.sender.take();
        self.task
            .await
            .map_err(|error| CrabError::Io(std::io::Error::other(error)))?
    }
}

#[derive(Clone, Default)]
pub(crate) struct StreamStageHooks {
    #[cfg(test)]
    pub(crate) after_input_read: Option<Arc<dyn Fn() + Send + Sync>>,
    pub(crate) after_batch_flush: Option<Arc<dyn Fn() + Send + Sync>>,
    pub(crate) before_prepared_xorb_write: Option<PreparedXorbWriteHook>,
    pub(crate) before_prepared_xorb_promote: Option<PreparedXorbWriteHook>,
}

pub(super) async fn persist_stream_prepared_authority(
    staging: &StagingArea,
    batch_id: &crate::StagingBatchId,
    prepared_xorbs: &mut [StreamStagePreparedXorb],
    xorb_builder: Option<&StreamStageXorbBuilder>,
    hooks: &StreamStageHooks,
) -> Result<()> {
    let xorb_builder = xorb_builder.ok_or_else(|| {
        CrabError::Internal("direct prepared authority has no add preparation".to_owned())
    })?;
    let preparation_id = xorb_builder.preparation_id().ok_or_else(|| {
        CrabError::Internal("direct prepared authority has no add preparation".to_owned())
    })?;
    let _promotion_permit = if prepared_xorbs.is_empty() {
        None
    } else {
        // Concurrent fsync-backed promotion increases tail latency on the same disk.
        // Keep chunking parallel, then serialize this operation's authority boundary.
        Some(xorb_builder.acquire_promotion().await?)
    };
    for prepared in prepared_xorbs.iter_mut() {
        if let Some(before_promote) = &hooks.before_prepared_xorb_promote {
            before_promote(&prepared.payload_path)?;
        }
        let expected_payload_hash =
            blake3::Hash::from_hex(&prepared.payload_hash).map_err(|error| {
                CrabError::StagingCorrupt(format!(
                    "stream-prepared xorb {} has an invalid payload digest: {error}",
                    prepared.hash.hex()
                ))
            })?;
        let written = move_prepared_xorb(
            staging.root(),
            &prepared.hash,
            &prepared.payload_path,
            expected_payload_hash.as_bytes(),
        )
        .await?;
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
    .map_err(|error| CrabError::Io(std::io::Error::other(error)))?
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
    tracing::debug!(payload_bytes = bytes, "staging prepared payload written");
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

pub(super) fn cleanup_stream_prepared_xorb_dir(
    staging_root: &Path,
    file_hash: &MerkleHash,
    path: &Path,
) {
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
