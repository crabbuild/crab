//! Xorb prefetch queue for the delayed-smudge filter protocol.
//!
//! When git sends a smudge command with `can-delay=1`, the filter may
//! respond "delayed" and kick off background reconstruction so the
//! file is ready by the time git asks for it via
//! `list_available_blobs`. This module owns that background state.
//!
//! # Architecture
//!
//! Each submission uses the canonical [`crab_read::ShardHydrator`] to stream
//! size- and hash-verified bytes into an auto-deleting temporary file. Inline
//! and delayed reads share its cache, restore hook, download controller, and
//! byte-denominated buffer semaphore. This queue owns only protocol lifetime
//! and completed outputs, not a second reconstruction policy.
//!
//! # Cancellation
//!
//! All reconstructors share a single `CancellationToken`. A SIGINT
//! fires the token, the reconstructors abort at their next check
//! point, and `PrefetchQueue::drain_for_shutdown` joins the remaining
//! tasks so the filter process exits without leaking bytes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result};
use crate::core::metrics::Metrics;
use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use crab_types::pointer::Pointer;
const WAIT_COMPLETED_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Shared prefetch queue for the filter-process delay protocol.
///
/// Cheap to clone via `Arc`. One instance per filter-process session.
pub struct PrefetchQueue {
    hydrator: crab_read::ShardHydrator,
    cancel: CancellationToken,
    handle: Handle,
    metrics: Option<Arc<Metrics>>,
    file_index_lookup: Option<SharedFileIndexLookup>,

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
    results: HashMap<String, PrefetchedFile>,
    errors: HashMap<String, CrabError>,
}

type PrefetchTaskResult = Result<PrefetchedFile>;

/// Completed delayed-smudge output, deleted automatically after serving or shutdown.
#[derive(Debug)]
pub struct PrefetchedFile {
    path: tempfile::TempPath,
    size: u64,
}

impl PrefetchedFile {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl PrefetchQueue {
    /// Build a delayed-output queue around the already configured read runtime.
    ///
    /// `handle` is a tokio runtime handle used to spawn prefetch tasks
    /// from the otherwise-blocking filter-protocol loop.
    #[must_use]
    pub fn new(
        hydrator: crab_read::ShardHydrator,
        cancel: CancellationToken,
        handle: Handle,
    ) -> Self {
        Self {
            hydrator,
            cancel,
            handle,
            metrics: None,
            file_index_lookup: None,
            state: tokio::sync::Mutex::new(PrefetchState::default()),
        }
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
    /// Duplicate submissions are ignored until the previous result is taken.
    pub async fn submit(&self, pathname: String, pointer: Pointer) {
        let mut state = self.state.lock().await;
        if state.in_flight.contains_key(&pathname) || state.results.contains_key(&pathname) {
            debug!(path = %pathname, "prefetch submit ignored: already in flight or completed");
            return;
        }

        let hydrator = self.hydrator.clone();
        let file_index_lookup = self.file_index_lookup.clone();
        let cancel = self.cancel.clone();
        let metrics = self.metrics.clone();

        let handle = self.handle.spawn(async move {
            if let Some(m) = metrics.as_ref() {
                m.inc_prefetch_started();
            }

            // The output remains live until Git consumes it. Keep it outside
            // disposable cache storage so cache pruning cannot unlink it.
            let temporary = tempfile::NamedTempFile::new()?;
            let writer = temporary.reopen()?;
            let path = temporary.into_temp_path();
            let bytes_written = hydrator
                .reconstruct_to_writer_with_cancel(
                    &pointer,
                    writer,
                    file_index_lookup.as_ref(),
                    &cancel,
                )
                .await?;
            if let Some(m) = metrics.as_ref() {
                m.inc_prefetch_completed();
                m.add_prefetch_bytes(bytes_written);
            }
            Ok(PrefetchedFile {
                path,
                size: bytes_written,
            })
        });
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
                    Ok(Err(error)) => {
                        warn!(path = %path, %error, "prefetch task failed");
                        state.errors.insert(path.clone(), error);
                    }
                    Err(join_err) => {
                        warn!(path = %path, error = %join_err, "prefetch task join failed");
                        state
                            .errors
                            .insert(path.clone(), CrabError::Io(std::io::Error::other(join_err)));
                    }
                }
                state.completed.push(path.clone());
            }
        }

        // Warn when completed but un-taken results grow large. Content is
        // disk-backed, but callers should still release temporary files.
        const WARN_RESULT_COUNT: usize = 64;
        if state.results.len() >= WARN_RESULT_COUNT {
            let total_bytes: u64 = state.results.values().map(|result| result.size).sum();
            warn!(
                pending_results = state.results.len(),
                bytes_held = total_bytes,
                "prefetch results accumulating on disk; caller must drain via take_result"
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
    pub async fn take_result(&self, pathname: &str) -> Result<PrefetchedFile> {
        // Fast path: result already materialized by `poll_completed`.
        {
            let mut state = self.state.lock().await;
            if let Some(bytes) = state.results.remove(pathname) {
                return Ok(bytes);
            }
            if let Some(error) = state.errors.remove(pathname) {
                return Err(error);
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

        handle
            .await
            .map_err(|error| CrabError::Io(std::io::Error::other(error)))?
    }

    /// Cancel all in-flight prefetches and wait for the tasks to
    /// unwind.
    ///
    /// Called by the filter loop on exit so no reconstructors outlive
    /// the session. Cancel and abort pending tasks, join them before closing
    /// their shared lookup, and remove unconsumed temporary outputs.
    pub async fn drain_for_shutdown(&self) {
        self.cancel.cancel();

        let handles: Vec<JoinHandle<PrefetchTaskResult>> = {
            let mut state = self.state.lock().await;
            state.in_flight.drain().map(|(_, h)| h).collect()
        };

        for handle in handles {
            if !handle.is_finished() {
                handle.abort();
            }
            // Await ignoring the result; cancellation is not an error.
            let _ = handle.await;
        }

        // Completed output is live protocol state, not reusable cache data.
        // Shutdown releases it even if another owner retains this queue.
        {
            let mut state = self.state.lock().await;
            state.results.clear();
            state.errors.clear();
            state.completed.clear();
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn queue() -> (tempfile::TempDir, PrefetchQueue) {
        let directory = tempfile::tempdir().unwrap();
        let origin = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let local = Arc::new(crate::cache::LocalCache::new(
            directory.path().join("cache"),
        ));
        let caching = crab_cache_store::CachingStore::new_with_local_cache(
            origin.clone(),
            crab_cache_store::CacheConfig::default(),
            local,
        )
        .unwrap();
        let layout = crab_read::ReadStoreLayout::new(origin, "prefetch-test".into());
        let hydrator = crab_read::ReadRuntimeBuilder::new(caching, layout, 2)
            .with_buffer_budget(1024 * 1024)
            .build()
            .unwrap();
        let queue = PrefetchQueue::new(hydrator, CancellationToken::new(), Handle::current());
        (directory, queue)
    }

    fn missing_pointer() -> Pointer {
        Pointer {
            file_hash: *blake3::hash(b"missing").as_bytes(),
            size: 7,
            shard_hint: None,
        }
    }

    #[tokio::test]
    async fn submit_records_task_in_flight() {
        let (_directory, queue) = queue();
        queue.submit("a.bin".into(), missing_pointer()).await;
        let in_flight_or_ready = queue.in_flight_count().await;
        let polled = queue.poll_completed().await;
        assert!(
            in_flight_or_ready > 0 || !polled.is_empty(),
            "submit must register the pending or completed task"
        );
        queue.drain_for_shutdown().await;
    }

    #[tokio::test]
    async fn wait_completed_reports_failed_path_before_empty_list() {
        let (_directory, queue) = queue();
        queue.submit("a.bin".into(), missing_pointer()).await;
        assert_eq!(queue.wait_completed().await, vec!["a.bin".to_owned()]);
        assert!(queue.take_result("a.bin").await.is_err());
        assert!(queue.wait_completed().await.is_empty());
        queue.drain_for_shutdown().await;
    }

    #[tokio::test]
    async fn cancellation_remains_typed_after_completion_polling() {
        let (_directory, queue) = queue();
        queue.cancel_token().cancel();
        queue.submit("a.bin".into(), missing_pointer()).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while queue.poll_completed().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled task finishes");
        assert!(matches!(
            queue.take_result("a.bin").await,
            Err(CrabError::Cancelled)
        ));
        queue.drain_for_shutdown().await;
    }

    #[tokio::test]
    async fn take_result_unknown_path_returns_not_found() {
        let (_directory, queue) = queue();
        assert!(matches!(
            queue.take_result("never-submitted.bin").await,
            Err(CrabError::NotFound { .. })
        ));
        queue.drain_for_shutdown().await;
    }

    #[tokio::test]
    async fn drain_for_shutdown_releases_tasks_and_completed_outputs() {
        let (_directory, queue) = queue();
        queue.submit("a.bin".into(), missing_pointer()).await;
        let temporary = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        let output_path = temporary.to_path_buf();
        queue.state.lock().await.results.insert(
            "completed.bin".into(),
            PrefetchedFile {
                path: temporary,
                size: 0,
            },
        );
        queue.drain_for_shutdown().await;
        assert_eq!(queue.in_flight_count().await, 0);
        assert!(queue.cancel_token().is_cancelled());
        assert!(!output_path.exists());
    }
}
