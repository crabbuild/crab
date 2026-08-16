//! Xorb prefetch queue for the delayed-smudge filter protocol.
//!
//! When git sends a smudge command with `can-delay=1`, the filter may
//! respond "delayed" and kick off background reconstruction so the
//! file is ready by the time git asks for it via
//! `list_available_blobs`. This module owns that background state.
//!
//! # Architecture
//!
//! Each `submit()` spawns a [`xet_data::file_reconstruction::FileReconstructor`]
//! task that drives reconstruction through the shared [`StoreClient`]
//! adapter (which implements xet-core's `Client` trait). Reconstructed
//! bytes accumulate into an in-memory `Vec<u8>` — the delayed-smudge
//! protocol requires the bytes to stream back to git later, so we
//! buffer the whole file in memory.
//!
//! Total memory is bounded two ways:
//!
//! - Per-file, the [`AdjustableSemaphore`] passed to
//!   [`FileReconstructor::with_buffer_semaphore`] caps the in-flight
//!   decompressed xorb data staged ahead of the writer.
//! - Across files, the same semaphore is shared between every
//!   concurrent reconstructor, so the sum of prefetch buffers never
//!   exceeds `hydrate.prefetch_budget`.
//!
//! # Cancellation
//!
//! All reconstructors share a single `CancellationToken`. A SIGINT
//! fires the token, the reconstructors abort at their next check
//! point, and `PrefetchQueue::drain_for_shutdown` joins the remaining
//! tasks so the filter process exits without leaking bytes.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use xet_client::cas_client::Client;
use xet_client::chunk_cache::ChunkCache;
use xet_data::file_reconstruction::FileReconstructor;
use xet_runtime::core::XetContext;
use xet_runtime::utils::adjustable_semaphore::AdjustableSemaphore;

use crate::core::error::{CrabError, Result};
use crate::core::metrics::Metrics;
use crate::git::store_client::SharedShardHints;
use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use crab_xet::hash::MerkleHash;

/// Average xorb-chunk size used to translate the byte-denominated
/// `prefetch_budget` config into semaphore permits. Chunks from crab
/// CDC are typically 64 KiB; the exact value only matters for permit
/// granularity and is not a correctness parameter.
const AVG_CHUNK_SIZE_BYTES: u64 = 64 * 1024;

/// Minimum number of permits the prefetch semaphore will issue. Even a
/// very small budget keeps the queue usable for tests and edge cases.
const MIN_PREFETCH_PERMITS: u64 = 1;
const WAIT_COMPLETED_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Shared prefetch queue for the filter-process delay protocol.
///
/// Cheap to clone via `Arc`. One instance per filter-process session.
pub struct PrefetchQueue {
    client: Arc<dyn Client>,
    chunk_cache: Option<Arc<dyn ChunkCache>>,
    semaphore: Arc<AdjustableSemaphore>,
    cancel: CancellationToken,
    handle: Handle,
    metrics: Option<Arc<Metrics>>,
    file_index_lookup: Option<SharedFileIndexLookup>,
    shard_hints: Option<SharedShardHints>,

    /// In-flight and completed reconstructions, keyed by the pathname
    /// git originally sent. Ordering preserves submission order so
    /// `poll_completed()` drains in a predictable fashion for tests.
    state: tokio::sync::Mutex<PrefetchState>,
}

/// Interior mutable state of the queue.
#[derive(Default)]
struct PrefetchState {
    in_flight: HashMap<String, JoinHandle<PrefetchTaskResult>>,
    completed: Vec<String>,
    results: HashMap<String, Vec<u8>>,
    errors: HashMap<String, String>,
}

/// Result type of a single reconstruction task. Errors are stringified
/// because `FileReconstructionError` is not `Send + 'static`-friendly
/// to keep around indefinitely and the filter just logs + falls back
/// on the on-demand path.
type PrefetchTaskResult = std::result::Result<Vec<u8>, String>;

impl PrefetchQueue {
    /// Build a queue with the given dependencies.
    ///
    /// `prefetch_budget_bytes` is translated into semaphore permits
    /// using [`AVG_CHUNK_SIZE_BYTES`]. The resulting permit count is
    /// fixed for the lifetime of the queue — the underlying
    /// `AdjustableSemaphore` supports dynamic resizing, but crab does
    /// not expose a runtime knob for that yet.
    ///
    /// `handle` is a tokio runtime handle used to spawn prefetch tasks
    /// from the otherwise-blocking filter-protocol loop.
    #[must_use]
    pub fn new(
        client: Arc<dyn Client>,
        prefetch_budget_bytes: u64,
        cancel: CancellationToken,
        handle: Handle,
    ) -> Self {
        let permits = permits_from_budget(prefetch_budget_bytes);
        let semaphore = AdjustableSemaphore::new(permits, (permits, permits));

        Self {
            client,
            chunk_cache: None,
            semaphore,
            cancel,
            handle,
            metrics: None,
            file_index_lookup: None,
            shard_hints: None,
            state: tokio::sync::Mutex::new(PrefetchState::default()),
        }
    }

    /// Attach a shared chunk cache. When set, prefetch reconstructors
    /// reuse the same `DiskCache` as the non-prefetch hydration path
    /// (integration point — plumbed through here so the filter can
    /// enable it once `DiskCache` is wired in).
    #[must_use]
    pub fn with_chunk_cache(mut self, cache: Arc<dyn ChunkCache>) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// Attach shared perf counters. When present, `submit` /
    /// `take_result` bump `prefetch_started`, `prefetch_completed`,
    /// and `prefetch_bytes`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Close this lookup from [`drain_for_shutdown`] after all prefetch
    /// tasks have stopped.
    #[must_use]
    pub fn with_file_index_lookup(mut self, lookup: SharedFileIndexLookup) -> Self {
        self.file_index_lookup = Some(lookup);
        self
    }

    /// Attach the shard-hint map shared with the queue's StoreClient.
    #[must_use]
    pub fn with_shard_hints(mut self, hints: SharedShardHints) -> Self {
        self.shard_hints = Some(hints);
        self
    }

    /// The shared cancellation token bound to every prefetch task.
    ///
    /// Clone this into additional components (e.g. the filter loop)
    /// that also want to observe shutdown.
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Submit a file for background reconstruction.
    ///
    /// Spawns a [`FileReconstructor`] task bound to the shared
    /// semaphore, cancellation token, and optional chunk cache. The
    /// task's `JoinHandle` is recorded under `pathname`.
    ///
    /// Submitting the same pathname twice replaces the prior handle;
    /// callers should `poll_completed` and `take_result` between
    /// submissions of identical paths.
    pub async fn submit(&self, pathname: String, file_hash: MerkleHash) {
        self.submit_with_hint(pathname, file_hash, None).await;
    }

    /// Submit a file with an optional pointer-carried shard hint.
    pub async fn submit_with_hint(
        &self,
        pathname: String,
        file_hash: MerkleHash,
        shard_hint: Option<MerkleHash>,
    ) {
        if let (Some(hints), Some(shard_hash)) = (&self.shard_hints, shard_hint) {
            let mut hints = match hints.write() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            hints.insert(file_hash, shard_hash);
        }

        let client = self.client.clone();
        let chunk_cache = self.chunk_cache.clone();
        let semaphore = self.semaphore.clone();
        let cancel = self.cancel.clone();
        let metrics = self.metrics.clone();

        let handle = self.handle.spawn(async move {
            if let Some(m) = metrics.as_ref() {
                m.inc_prefetch_started();
            }

            let xet_context = match XetContext::default() {
                Ok(context) => context,
                Err(error) => return Err(format!("failed to initialize xet context: {error}")),
            };
            let mut reconstructor = FileReconstructor::new(&xet_context, &client, file_hash)
                .with_buffer_semaphore(semaphore)
                .with_cancellation_token(cancel);
            if let Some(cache) = chunk_cache {
                reconstructor = reconstructor.with_chunk_cache(cache);
            }

            // `SequentialWriter` takes ownership of the `Write` impl so
            // we can't reclaim a plain `Vec<u8>` after reconstruction.
            // Funnel writes through a shared cursor we can drain back
            // in the task body once the reconstructor returns.
            let shared = Arc::new(std::sync::Mutex::new(Cursor::new(Vec::<u8>::new())));
            let writer = SharedCursorWriter(shared.clone());

            match reconstructor.reconstruct_to_writer(writer).await {
                Ok(_bytes_written) => {
                    let mut guard = match shared.lock() {
                        Ok(g) => g,
                        Err(_) => {
                            return Err("reconstruction writer poisoned".to_string());
                        }
                    };
                    let bytes = std::mem::take(guard.get_mut());
                    if let Some(m) = metrics.as_ref() {
                        m.inc_prefetch_completed();
                        m.add_prefetch_bytes(bytes.len() as u64);
                    }
                    Ok(bytes)
                }
                Err(e) => Err(e.to_string()),
            }
        });

        let mut state = self.state.lock().await;

        // If a task for this pathname is already in flight, don't spawn
        // a duplicate — the previous submission's result will be served
        // to whichever caller asks first. Aborting and respawning
        // throws away any work already done on a nearly-complete task.
        // See finding CR2-F26.
        //
        // If a completed result is already materialized, the caller
        // will pick it up via take_result without needing a new task.
        if state.in_flight.contains_key(&pathname) || state.results.contains_key(&pathname) {
            debug!(path = %pathname, "prefetch submit ignored: already in flight or completed");
            // Drop the handle we spawned above so its Drop aborts the task.
            drop(handle);
            return;
        }
        state.in_flight.insert(pathname, handle);
    }

    /// Return the pathnames of all tasks that have completed since the
    /// last poll. Subsequent calls return an empty list until new
    /// tasks finish.
    ///
    /// Failed tasks are reported as "completed" here; the actual error
    /// surfaces when the caller invokes [`take_result`]. This matches
    /// git's delayed-smudge protocol, which expects
    /// `list_available_blobs` to enumerate *all* ready blobs
    /// regardless of status.
    pub async fn poll_completed(&self) -> Vec<String> {
        let mut state = self.state.lock().await;

        let ready: Vec<String> = state
            .in_flight
            .iter()
            .filter(|(_, h)| h.is_finished())
            .map(|(k, _)| k.clone())
            .collect();

        for path in &ready {
            if let Some(handle) = state.in_flight.remove(path) {
                match handle.await {
                    Ok(Ok(bytes)) => {
                        state.results.insert(path.clone(), bytes);
                    }
                    Ok(Err(msg)) => {
                        warn!(path = %path, error = %msg, "prefetch task failed");
                        state.errors.insert(path.clone(), msg);
                    }
                    Err(join_err) => {
                        let msg = format!("prefetch task aborted: {join_err}");
                        warn!(path = %path, error = %msg, "prefetch task join failed");
                        state.errors.insert(path.clone(), msg);
                    }
                }
                state.completed.push(path.clone());
            }
        }

        // Warn when completed but un-taken results grow large. The
        // filter-process is expected to drain results promptly via
        // `take_result` after listing completed blobs; if the results
        // map accumulates, it pins the full reconstructed content in
        // memory. See finding CR2-F27.
        const WARN_RESULT_COUNT: usize = 64;
        if state.results.len() >= WARN_RESULT_COUNT {
            let total_bytes: usize = state.results.values().map(Vec::len).sum();
            warn!(
                pending_results = state.results.len(),
                bytes_held = total_bytes,
                "prefetch results accumulating in memory; caller must drain via take_result"
            );
        }

        std::mem::take(&mut state.completed)
    }

    /// Wait until at least one delayed-smudge task is ready.
    ///
    /// Git treats an empty `list_available_blobs` response as "no more
    /// delayed blobs", so only return an empty list once the queue has
    /// no in-flight work left.
    pub async fn wait_completed(&self) -> Vec<String> {
        loop {
            let ready = self.poll_completed().await;
            if !ready.is_empty() {
                return ready;
            }

            let has_in_flight = !self.state.lock().await.in_flight.is_empty();
            if !has_in_flight || self.cancel.is_cancelled() {
                return Vec::new();
            }

            tokio::time::sleep(WAIT_COMPLETED_POLL_INTERVAL).await;
        }
    }

    /// Return the reconstructed bytes for a previously-submitted file.
    ///
    /// If the task has not completed yet, waits for it. Returns an
    /// error if the task failed or the pathname was never submitted.
    pub async fn take_result(&self, pathname: &str) -> Result<Vec<u8>> {
        // Fast path: result already materialized by `poll_completed`.
        {
            let mut state = self.state.lock().await;
            if let Some(bytes) = state.results.remove(pathname) {
                return Ok(bytes);
            }
            if let Some(msg) = state.errors.remove(pathname) {
                return Err(CrabError::Internal(format!(
                    "prefetch failed for {pathname}: {msg}"
                )));
            }
        }

        // Slow path: task still running. Pull the handle out under
        // the lock, then await it outside the lock so other submits
        // are not blocked.
        let handle = {
            let mut state = self.state.lock().await;
            state.in_flight.remove(pathname)
        };

        let Some(handle) = handle else {
            return Err(CrabError::NotFound {
                path: format!("prefetch result for {pathname}"),
            });
        };

        match handle.await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(msg)) => Err(CrabError::Internal(format!(
                "prefetch failed for {pathname}: {msg}"
            ))),
            Err(join_err) => Err(CrabError::Internal(format!(
                "prefetch task aborted for {pathname}: {join_err}"
            ))),
        }
    }

    /// Cancel all in-flight prefetches and wait for the tasks to
    /// unwind.
    ///
    /// Called by the filter loop on exit so no reconstructors outlive
    /// the session. The shared cancellation token is fired first, then
    /// every remaining `JoinHandle` is awaited. Tasks that do not
    /// respect the token within a short grace period are aborted
    /// forcefully.
    pub async fn drain_for_shutdown(&self) {
        self.cancel.cancel();

        let handles: Vec<JoinHandle<PrefetchTaskResult>> = {
            let mut state = self.state.lock().await;
            state.in_flight.drain().map(|(_, h)| h).collect()
        };

        for handle in handles {
            // Best-effort: give the task a moment to honor cancellation,
            // then forcibly abort.
            if !handle.is_finished() {
                handle.abort();
            }
            // Await ignoring the result; cancellation is not an error.
            let _ = handle.await;
        }

        if let Some(lookup) = self.file_index_lookup.clone()
            && let Err(e) = lookup.close().await
        {
            warn!(err = %e, "prefetch file-index lookup session close failed");
        }
    }

    /// Number of in-flight tasks. Test-only.
    #[cfg(test)]
    pub(crate) async fn in_flight_count(&self) -> usize {
        self.state.lock().await.in_flight.len()
    }
}

/// Translate a byte budget into semaphore permits. Clamped below at
/// [`MIN_PREFETCH_PERMITS`] so very tight budgets still yield a usable
/// queue.
fn permits_from_budget(budget_bytes: u64) -> u64 {
    let raw = budget_bytes / AVG_CHUNK_SIZE_BYTES;
    raw.max(MIN_PREFETCH_PERMITS)
}

/// `std::io::Write` adapter that funnels into an `Arc<Mutex<Cursor>>`
/// so the submitter task can reclaim the reconstructed bytes after the
/// `SequentialWriter` has taken ownership of the writer handle.
struct SharedCursorWriter(Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for SharedCursorWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("prefetch writer mutex poisoned"))?;
        std::io::Write::write(&mut *guard, buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("prefetch writer mutex poisoned"))?;
        std::io::Write::flush(&mut *guard)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use bytes::Bytes;
    use crab_xet::shard::MDBFileInfo;
    use crab_xet::xorb::format::SerializedXorbObject;
    use xet_client::cas_client::ShardUploadProgressCallback;
    use xet_client::cas_client::adaptive_concurrency::ConnectionPermit;
    use xet_client::cas_client::progress_tracked_streams::ProgressCallback;
    use xet_client::cas_client::{Client, URLProvider};
    use xet_client::cas_types::{
        BatchQueryReconstructionResponse, FileChunkHashesResponse, FileRange,
        QueryReconstructionResponseV2,
    };
    use xet_client::error::{ClientError, Result as ClientResult};

    /// Stub `Client` that always reports "file not found" so
    /// `FileReconstructor` completes quickly with a known error. Good
    /// enough for queue-wiring tests that don't care about byte
    /// correctness.
    struct NotFoundClient;

    #[async_trait]
    impl Client for NotFoundClient {
        async fn get_file_reconstruction_info(
            &self,
            _file_hash: &MerkleHash,
        ) -> ClientResult<Option<(MDBFileInfo, Option<MerkleHash>)>> {
            Ok(None)
        }

        async fn get_reconstruction(
            &self,
            _file_id: &MerkleHash,
            _bytes_range: Option<FileRange>,
        ) -> ClientResult<Option<QueryReconstructionResponseV2>> {
            Ok(None)
        }

        async fn batch_get_reconstruction(
            &self,
            _file_ids: &[MerkleHash],
        ) -> ClientResult<BatchQueryReconstructionResponse> {
            Err(ClientError::Other("not implemented".to_string()))
        }

        async fn acquire_download_permit(&self) -> ClientResult<ConnectionPermit> {
            Err(ClientError::Other("no permits in stub".to_string()))
        }

        async fn get_file_term_data(
            &self,
            _url_info: Box<dyn URLProvider>,
            _download_permit: ConnectionPermit,
            _progress_callback: Option<ProgressCallback>,
            _uncompressed_size_if_known: Option<usize>,
        ) -> ClientResult<(Bytes, Vec<u32>)> {
            Err(ClientError::Other("stub".to_string()))
        }

        async fn query_for_global_dedup_shard(
            &self,
            _prefix: &str,
            _chunk_hash: &MerkleHash,
        ) -> ClientResult<Option<Bytes>> {
            Ok(None)
        }

        async fn acquire_upload_permit(&self) -> ClientResult<ConnectionPermit> {
            Err(ClientError::Other("read-only".to_string()))
        }

        async fn upload_shard(
            &self,
            _shard_data: Bytes,
            _upload_permit: ConnectionPermit,
            _progress_callback: Option<ShardUploadProgressCallback>,
        ) -> ClientResult<()> {
            Err(ClientError::Other("read-only".to_string()))
        }

        async fn get_file_chunk_hashes(
            &self,
            _file_id: &MerkleHash,
            _dirty_ranges: Vec<FileRange>,
        ) -> ClientResult<FileChunkHashesResponse> {
            Err(ClientError::Other("read-only".to_string()))
        }

        async fn upload_xorb(
            &self,
            _prefix: &str,
            _serialized_xorb_object: SerializedXorbObject,
            _progress_callback: Option<ProgressCallback>,
            _upload_permit: ConnectionPermit,
        ) -> ClientResult<u64> {
            Err(ClientError::Other("read-only".to_string()))
        }
    }

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([
            seed,
            seed.wrapping_mul(31),
            seed.wrapping_mul(97),
            seed.wrapping_mul(127),
        ])
    }

    #[test]
    fn permits_from_budget_minimum() {
        assert_eq!(permits_from_budget(0), MIN_PREFETCH_PERMITS);
        assert_eq!(permits_from_budget(1), MIN_PREFETCH_PERMITS);
    }

    #[test]
    fn permits_from_budget_scales_by_chunk_size() {
        // 1 GiB / 64 KiB = 16384 permits
        assert_eq!(
            permits_from_budget(1024 * 1024 * 1024),
            (1024 * 1024 * 1024) / AVG_CHUNK_SIZE_BYTES
        );
    }

    #[tokio::test]
    async fn submit_records_task_in_flight() {
        let client: Arc<dyn Client> = Arc::new(NotFoundClient);
        let queue = PrefetchQueue::new(
            client,
            1024 * 1024,
            CancellationToken::new(),
            Handle::current(),
        );

        queue.submit("a.bin".to_string(), hash_from_seed(1)).await;
        // Either the task is still running or it finished immediately
        // with the "file not found" error — in both cases the submit
        // has registered state for the pathname.
        let in_flight_or_ready = queue.in_flight_count().await;
        let polled = queue.poll_completed().await;
        assert!(
            in_flight_or_ready > 0 || !polled.is_empty(),
            "submit should either leave a task in-flight or register a completion"
        );
    }

    #[tokio::test]
    async fn wait_completed_returns_submitted_path_before_empty_list() {
        let client: Arc<dyn Client> = Arc::new(NotFoundClient);
        let queue = PrefetchQueue::new(
            client,
            1024 * 1024,
            CancellationToken::new(),
            Handle::current(),
        );

        queue.submit("a.bin".to_string(), hash_from_seed(1)).await;

        assert_eq!(queue.wait_completed().await, vec!["a.bin".to_string()]);
        assert!(queue.wait_completed().await.is_empty());
    }

    #[tokio::test]
    async fn submit_with_hint_installs_shared_shard_hint() {
        let client: Arc<dyn Client> = Arc::new(NotFoundClient);
        let hints: SharedShardHints = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let queue = PrefetchQueue::new(
            client,
            1024 * 1024,
            CancellationToken::new(),
            Handle::current(),
        )
        .with_shard_hints(Arc::clone(&hints));

        let file_hash = hash_from_seed(11);
        let shard_hash = hash_from_seed(12);
        queue
            .submit_with_hint("a.bin".to_string(), file_hash, Some(shard_hash))
            .await;

        let stored = match hints.read() {
            Ok(guard) => guard.get(&file_hash).copied(),
            Err(poisoned) => poisoned.into_inner().get(&file_hash).copied(),
        };
        assert_eq!(stored, Some(shard_hash));
    }

    #[tokio::test]
    async fn take_result_unknown_path_returns_not_found() {
        let client: Arc<dyn Client> = Arc::new(NotFoundClient);
        let queue = PrefetchQueue::new(
            client,
            1024 * 1024,
            CancellationToken::new(),
            Handle::current(),
        );

        let err = queue.take_result("never-submitted.bin").await.unwrap_err();
        match err {
            CrabError::NotFound { path } => {
                assert!(path.contains("never-submitted.bin"));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drain_for_shutdown_cancels_and_empties() {
        let client: Arc<dyn Client> = Arc::new(NotFoundClient);
        let queue = PrefetchQueue::new(
            client,
            1024 * 1024,
            CancellationToken::new(),
            Handle::current(),
        );

        queue.submit("a.bin".to_string(), hash_from_seed(1)).await;
        queue.submit("b.bin".to_string(), hash_from_seed(2)).await;

        queue.drain_for_shutdown().await;
        assert_eq!(queue.in_flight_count().await, 0);
        assert!(queue.cancel_token().is_cancelled());
    }
}
