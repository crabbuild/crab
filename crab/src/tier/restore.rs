//! Restore orchestrator for archive-class objects.
//!
//! [`RestoreOrchestrator`] manages the lifecycle of archive restore
//! requests across providers. It implements a per-object state machine:
//!
//! ```text
//! initial → query state()
//!   Ready           → return Ready
//!   InProgress      → poll loop (exponential backoff with full jitter)
//!   NotRequested    → issue restore(); transition to InProgress → poll
//!   Failed          → if retryable && budget left, retry; else propagate
//! ```
//!
//! Concurrency is bounded by a [`tokio::sync::Semaphore`]. Active
//! restores are tracked in a [`DashMap`] so that dropping a future
//! mid-poll does NOT cancel the provider-side restore — the next
//! `ensure_warm` call observes `InProgress` and resumes polling.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use rand::Rng;
use tokio::sync::Semaphore;
use tokio::time::sleep;

use crate::core::error::{CrabError, Result};
use crate::tier::provider::{
    ObjectPath, RestoreBackend, RestoreHandle, RestoreState, RestoreTier, StorageClass,
};

// ── Backoff constants ───────────────────────────────────────────────

/// Initial poll interval for the exponential backoff.
const BACKOFF_INITIAL: Duration = Duration::from_secs(30);

/// Multiplier applied to the backoff interval on each poll iteration.
/// Stored as a rational (3/2) to avoid floating-point in duration math.
const BACKOFF_MULTIPLIER_NUM: u32 = 3;
const BACKOFF_MULTIPLIER_DEN: u32 = 2;

/// Maximum poll interval (cap).
const BACKOFF_CAP: Duration = Duration::from_secs(600);

/// Maximum number of retry attempts for retryable `Failed` states.
const MAX_RETRIES: u32 = 3;

// ── RestoreOrchestrator ─────────────────────────────────────────────

/// Orchestrates archive-class restore requests with concurrency
/// limiting, exponential backoff polling, and drop-safe tracking.
pub struct RestoreOrchestrator {
    backend: Arc<dyn RestoreBackend>,
    semaphore: Arc<Semaphore>,
    active: DashMap<String, RestoreHandle>,
    timeout: Duration,
    options: RestoreOptions,
}

/// Per-request archive restore options.
#[derive(Debug, Clone, Copy)]
pub struct RestoreOptions {
    /// Provider restore speed tier.
    pub tier: RestoreTier,
    /// How long the temporary restored copy should remain readable.
    pub duration: Duration,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            tier: RestoreTier::Standard,
            duration: Duration::from_secs(7 * 86_400),
        }
    }
}

impl RestoreOrchestrator {
    /// Create a new orchestrator.
    ///
    /// `max_concurrency` bounds the number of concurrent restore
    /// submissions via a semaphore. `timeout` is the maximum wall-clock
    /// time to wait for a single object's restore to complete.
    pub fn new(backend: Arc<dyn RestoreBackend>, max_concurrency: u32, timeout: Duration) -> Self {
        Self::with_options(backend, max_concurrency, timeout, RestoreOptions::default())
    }

    /// Create a new orchestrator with explicit restore options.
    pub fn with_options(
        backend: Arc<dyn RestoreBackend>,
        max_concurrency: u32,
        timeout: Duration,
        options: RestoreOptions,
    ) -> Self {
        Self {
            backend,
            semaphore: Arc::new(Semaphore::new(max_concurrency as usize)),
            active: DashMap::new(),
            timeout,
            options,
        }
    }

    /// Ensure the object at `path` is warm (readable).
    ///
    /// Implements the per-object state machine described in the module
    /// docs. Returns `RestoreState::Ready` on success, or propagates
    /// a non-retryable error.
    pub async fn ensure_warm(&self, path: &ObjectPath) -> Result<RestoreState> {
        #[cfg(feature = "otlp")]
        let _span = tracing::info_span!(
            "tier.restore",
            object_path = %path,
        )
        .entered();

        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut retries: u32 = 0;

        loop {
            let state = self.backend.state(path).await?;

            match state {
                RestoreState::Ready => {
                    self.active.remove(path);
                    return Ok(RestoreState::Ready);
                }

                RestoreState::InProgress { .. } => {
                    return self.poll_until_ready(path, deadline).await;
                }

                RestoreState::NotRequested => {
                    self.issue_restore(path).await?;
                    return self.poll_until_ready(path, deadline).await;
                }

                RestoreState::Failed { retryable, reason } => {
                    if retryable && retries < MAX_RETRIES {
                        retries += 1;
                        self.issue_restore(path).await?;
                        // Loop back to poll — poll_until_ready may
                        // return a retryable Failed, which re-enters
                        // this loop.
                        match self.poll_until_ready(path, deadline).await? {
                            RestoreState::Ready => return Ok(RestoreState::Ready),
                            // poll_until_ready returned a retryable
                            // failure; continue the outer retry loop.
                            other => {
                                if let RestoreState::Failed {
                                    retryable: false, ..
                                } = &other
                                {
                                    return Ok(other);
                                }
                                // Retryable failure — loop again.
                                continue;
                            }
                        }
                    }

                    return Err(CrabError::ArchiveRestoreRequired {
                        xorb: path.clone(),
                        class: String::new(),
                        estimated_eta: Some(reason),
                    });
                }
            }
        }
    }

    /// Submit restores for multiple paths under the concurrency cap.
    ///
    /// Each path is processed concurrently; the semaphore inside
    /// `issue_restore` bounds the number of concurrent provider-side
    /// restore submissions.
    pub async fn ensure_warm_batch(&self, paths: &[ObjectPath]) -> Result<Vec<RestoreState>> {
        futures_util::future::try_join_all(paths.iter().map(|path| self.ensure_warm(path))).await
    }

    /// Acquire a semaphore permit, issue the restore, and track it in
    /// the active map for drop-safety.
    async fn issue_restore(&self, path: &ObjectPath) -> Result<()> {
        match self.active.entry(path.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => return Ok(()),
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(RestoreHandle {
                    id: "pending".into(),
                });
            }
        }

        let _permit = self.semaphore.acquire().await.map_err(|_| {
            self.active.remove(path);
            CrabError::Internal("restore semaphore closed".into())
        })?;

        let restore_result = self
            .backend
            .restore(path, self.options.tier, self.options.duration)
            .await;

        let handle = match restore_result {
            Ok(handle) => handle,
            Err(err) => {
                self.active.remove(path);
                return Err(err);
            }
        };

        // Track the active restore so drop-safety is maintained:
        // even if the caller drops this future, the DashMap entry
        // persists and the next ensure_warm call sees InProgress.
        self.active.insert(path.clone(), handle);
        Ok(())
    }

    /// Poll the backend until the object is `Ready` or the deadline
    /// expires.
    ///
    /// Uses exponential backoff: 30s initial, 1.5× multiplier, 10min
    /// cap, full jitter.
    async fn poll_until_ready(
        &self,
        path: &ObjectPath,
        deadline: tokio::time::Instant,
    ) -> Result<RestoreState> {
        let mut interval = BACKOFF_INITIAL;

        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(CrabError::ArchiveRestoreTimeout {
                    xorb: path.clone(),
                    class: String::new(),
                    elapsed_secs: self.timeout.as_secs(),
                });
            }

            // Full jitter: sleep a random duration in [0, interval].
            let jittered = full_jitter(interval);
            sleep(jittered).await;

            let state = self.backend.state(path).await?;

            match state {
                RestoreState::Ready => {
                    self.active.remove(path);
                    return Ok(RestoreState::Ready);
                }
                RestoreState::InProgress { .. } => {
                    interval = advance_backoff(interval);
                }
                RestoreState::NotRequested => {
                    // Unexpected: the restore may have expired or been
                    // cancelled provider-side. Return so the caller's
                    // state machine can re-issue.
                    return Ok(RestoreState::NotRequested);
                }
                RestoreState::Failed { retryable, reason } => {
                    // Surface to the caller's retry loop.
                    return Ok(RestoreState::Failed { retryable, reason });
                }
            }
        }
    }
}

/// Validate that a restore tier is supported for the given storage class.
///
/// Returns `Ok(())` if the tier is in the backend's supported list for
/// the class, or `RestoreTierUnsupported` otherwise.
pub fn validate_restore_tier(
    backend: &dyn RestoreBackend,
    class: &StorageClass,
    tier: RestoreTier,
) -> Result<()> {
    let supported = backend.supported_tiers(class);
    if supported.contains(&tier) {
        Ok(())
    } else {
        Err(CrabError::RestoreTierUnsupported {
            tier: format!("{tier:?}"),
            class: format!("{class:?}"),
            supported: supported.iter().map(|t| format!("{t:?}")).collect(),
        })
    }
}

/// Full jitter: uniform random in `[0, interval]`.
fn full_jitter(interval: Duration) -> Duration {
    let nanos = u64::try_from(interval.as_nanos()).unwrap_or(u64::MAX);
    if nanos == 0 {
        return Duration::ZERO;
    }
    let pick = rand::rng().random_range(0..=nanos);
    Duration::from_nanos(pick)
}

/// Advance the backoff interval: `min(cap, interval * 3/2)`.
fn advance_backoff(current: Duration) -> Duration {
    let next = current
        .saturating_mul(BACKOFF_MULTIPLIER_NUM)
        .checked_div(BACKOFF_MULTIPLIER_DEN)
        .unwrap_or(current);
    next.min(BACKOFF_CAP)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;

    // ── Mock RestoreBackend ─────────────────────────────────────────

    /// A configurable mock backend that returns a sequence of states
    /// for each `state()` call and tracks `restore()` invocations.
    struct MockBackend {
        states: Mutex<Vec<RestoreState>>,
        restore_count: AtomicU32,
        supported_tiers_fn: Box<dyn Fn(&StorageClass) -> &'static [RestoreTier] + Send + Sync>,
    }

    struct BlockingAfterRestoreBackend {
        state_calls: AtomicU32,
        restore_count: AtomicU32,
        blocked_state_entered: tokio::sync::Notify,
    }

    impl MockBackend {
        fn new(states: Vec<RestoreState>) -> Self {
            Self {
                states: Mutex::new(states),
                restore_count: AtomicU32::new(0),
                supported_tiers_fn: Box::new(|_| &[RestoreTier::Standard, RestoreTier::Bulk]),
            }
        }

        fn with_supported_tiers(
            mut self,
            f: impl Fn(&StorageClass) -> &'static [RestoreTier] + Send + Sync + 'static,
        ) -> Self {
            self.supported_tiers_fn = Box::new(f);
            self
        }

        fn restore_count(&self) -> u32 {
            self.restore_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl RestoreBackend for MockBackend {
        async fn restore(
            &self,
            _path: &ObjectPath,
            _tier: RestoreTier,
            _duration: Duration,
        ) -> Result<RestoreHandle> {
            self.restore_count.fetch_add(1, Ordering::SeqCst);
            Ok(RestoreHandle {
                id: "mock-restore-id".into(),
            })
        }

        async fn state(&self, _path: &ObjectPath) -> Result<RestoreState> {
            let mut states = self.states.lock().unwrap();
            if states.is_empty() {
                Ok(RestoreState::Ready)
            } else {
                Ok(states.remove(0))
            }
        }

        fn supported_tiers(&self, class: &StorageClass) -> &'static [RestoreTier] {
            (self.supported_tiers_fn)(class)
        }
    }

    impl BlockingAfterRestoreBackend {
        fn new() -> Self {
            Self {
                state_calls: AtomicU32::new(0),
                restore_count: AtomicU32::new(0),
                blocked_state_entered: tokio::sync::Notify::new(),
            }
        }

        fn restore_count(&self) -> u32 {
            self.restore_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl RestoreBackend for BlockingAfterRestoreBackend {
        async fn restore(
            &self,
            _path: &ObjectPath,
            _tier: RestoreTier,
            _duration: Duration,
        ) -> Result<RestoreHandle> {
            self.restore_count.fetch_add(1, Ordering::SeqCst);
            Ok(RestoreHandle {
                id: "mock-restore-id".into(),
            })
        }

        async fn state(&self, _path: &ObjectPath) -> Result<RestoreState> {
            match self.state_calls.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(RestoreState::NotRequested),
                1 => {
                    self.blocked_state_entered.notify_one();
                    std::future::pending::<()>().await;
                    unreachable!("blocked state call should be aborted")
                }
                2 => Ok(RestoreState::InProgress {
                    started_at: "2026-01-01T00:00:00Z".into(),
                    expected_ready_at: "2026-01-01T06:00:00Z".into(),
                }),
                _ => Ok(RestoreState::Ready),
            }
        }

        fn supported_tiers(&self, _class: &StorageClass) -> &'static [RestoreTier] {
            &[RestoreTier::Standard, RestoreTier::Bulk]
        }
    }

    fn make_orchestrator(
        backend: Arc<MockBackend>,
        max_concurrency: u32,
        timeout: Duration,
    ) -> RestoreOrchestrator {
        RestoreOrchestrator::new(backend as Arc<dyn RestoreBackend>, max_concurrency, timeout)
    }

    // ── State machine: Ready → returns immediately ──────────────────

    #[tokio::test(start_paused = true)]
    async fn ready_returns_immediately() {
        let backend = Arc::new(MockBackend::new(vec![RestoreState::Ready]));
        let orch = make_orchestrator(backend, 16, Duration::from_secs(3600));

        let result = orch.ensure_warm(&"obj/ready".into()).await.unwrap();
        assert_eq!(result, RestoreState::Ready);
    }

    // ── State machine: NotRequested → restore → poll → Ready ────────

    #[tokio::test(start_paused = true)]
    async fn not_requested_issues_restore_then_polls_to_ready() {
        let backend = Arc::new(MockBackend::new(vec![
            RestoreState::NotRequested,
            // After restore is issued, poll sees InProgress then Ready.
            RestoreState::InProgress {
                started_at: "2026-01-01T00:00:00Z".into(),
                expected_ready_at: "2026-01-01T06:00:00Z".into(),
            },
            RestoreState::Ready,
        ]));
        let backend_ref = Arc::clone(&backend);
        let orch = make_orchestrator(backend, 16, Duration::from_secs(3600));

        let result = orch.ensure_warm(&"obj/cold".into()).await.unwrap();
        assert_eq!(result, RestoreState::Ready);
        assert_eq!(backend_ref.restore_count(), 1);
        // After Ready, the active map entry is cleaned up.
        assert!(orch.active.is_empty());
    }

    // ── State machine: InProgress → poll → Ready ────────────────────

    #[tokio::test(start_paused = true)]
    async fn in_progress_polls_until_ready() {
        let backend = Arc::new(MockBackend::new(vec![
            RestoreState::InProgress {
                started_at: "2026-01-01T00:00:00Z".into(),
                expected_ready_at: "2026-01-01T06:00:00Z".into(),
            },
            // Two polls of InProgress, then Ready.
            RestoreState::InProgress {
                started_at: "2026-01-01T00:00:00Z".into(),
                expected_ready_at: "2026-01-01T06:00:00Z".into(),
            },
            RestoreState::Ready,
        ]));
        let backend_ref = Arc::clone(&backend);
        let orch = make_orchestrator(backend, 16, Duration::from_secs(3600));

        let result = orch.ensure_warm(&"obj/restoring".into()).await.unwrap();
        assert_eq!(result, RestoreState::Ready);
        // No restore calls — object was already in progress.
        assert_eq!(backend_ref.restore_count(), 0);
    }

    // ── State machine: Failed retryable → retry → Ready ─────────────

    #[tokio::test(start_paused = true)]
    async fn failed_retryable_retries_then_succeeds() {
        let backend = Arc::new(MockBackend::new(vec![
            RestoreState::Failed {
                retryable: true,
                reason: "transient provider error".into(),
            },
            // After retry restore is issued, poll returns Ready.
            RestoreState::Ready,
        ]));
        let backend_ref = Arc::clone(&backend);
        let orch = make_orchestrator(backend, 16, Duration::from_secs(3600));

        let result = orch.ensure_warm(&"obj/failed".into()).await.unwrap();
        assert_eq!(result, RestoreState::Ready);
        assert_eq!(backend_ref.restore_count(), 1);
    }

    // ── State machine: Failed non-retryable → propagates error ──────

    #[tokio::test(start_paused = true)]
    async fn failed_non_retryable_propagates_error() {
        let backend = Arc::new(MockBackend::new(vec![RestoreState::Failed {
            retryable: false,
            reason: "permanent failure".into(),
        }]));
        let orch = make_orchestrator(backend, 16, Duration::from_secs(3600));

        let result = orch.ensure_warm(&"obj/perm-fail".into()).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CrabError::ArchiveRestoreRequired { .. }
        ));
    }

    // ── Drop-safety: dropping future mid-poll doesn't cancel restore ─

    #[tokio::test(start_paused = true)]
    async fn drop_safety_restore_persists_after_future_drop() {
        let backend = Arc::new(BlockingAfterRestoreBackend::new());
        let backend_ref = Arc::clone(&backend);

        let orch = Arc::new(RestoreOrchestrator::new(
            backend as Arc<dyn RestoreBackend>,
            16,
            Duration::from_secs(3600),
        ));

        let path: ObjectPath = "obj/drop-test".into();

        let orch_clone = Arc::clone(&orch);
        let path_clone = path.clone();
        let blocked_state = backend_ref.blocked_state_entered.notified();
        tokio::pin!(blocked_state);
        let task = tokio::spawn(async move { orch_clone.ensure_warm(&path_clone).await });

        while !orch.active.contains_key(&path) {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(BACKOFF_INITIAL).await;
        blocked_state.await;

        // The restore was issued.
        assert_eq!(backend_ref.restore_count(), 1);

        // The active map still has the entry — drop-safety.
        assert!(orch.active.contains_key(&path));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(orch.active.contains_key(&path));

        // A second call observes InProgress and polls to Ready.
        let result = orch.ensure_warm(&path).await.unwrap();
        assert_eq!(result, RestoreState::Ready);

        // No additional restore calls — the existing one was reused.
        assert_eq!(backend_ref.restore_count(), 1);
    }

    // ── Supported-tier matrix ───────────────────────────────────────

    #[test]
    fn expedited_on_deep_archive_returns_unsupported() {
        let backend = MockBackend::new(vec![]).with_supported_tiers(|class| match class {
            StorageClass::S3GlacierDeepArchive => &[RestoreTier::Standard, RestoreTier::Bulk],
            StorageClass::S3GlacierFlexibleRetrieval => &[
                RestoreTier::Expedited,
                RestoreTier::Standard,
                RestoreTier::Bulk,
            ],
            _ => &[],
        });

        let result = validate_restore_tier(
            &backend,
            &StorageClass::S3GlacierDeepArchive,
            RestoreTier::Expedited,
        );
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CrabError::RestoreTierUnsupported { .. }
        ));
    }

    #[test]
    fn bulk_on_azure_archive_returns_unsupported() {
        let backend = MockBackend::new(vec![]).with_supported_tiers(|class| match class {
            StorageClass::AzureArchive => &[RestoreTier::High, RestoreTier::Standard],
            _ => &[],
        });

        let result =
            validate_restore_tier(&backend, &StorageClass::AzureArchive, RestoreTier::Bulk);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CrabError::RestoreTierUnsupported { .. }
        ));
    }

    #[test]
    fn standard_on_deep_archive_is_valid() {
        let backend = MockBackend::new(vec![]).with_supported_tiers(|class| match class {
            StorageClass::S3GlacierDeepArchive => &[RestoreTier::Standard, RestoreTier::Bulk],
            _ => &[],
        });

        assert!(
            validate_restore_tier(
                &backend,
                &StorageClass::S3GlacierDeepArchive,
                RestoreTier::Standard,
            )
            .is_ok()
        );
    }

    #[test]
    fn standard_on_azure_archive_is_valid() {
        let backend = MockBackend::new(vec![]).with_supported_tiers(|class| match class {
            StorageClass::AzureArchive => &[RestoreTier::High, RestoreTier::Standard],
            _ => &[],
        });

        assert!(
            validate_restore_tier(&backend, &StorageClass::AzureArchive, RestoreTier::Standard,)
                .is_ok()
        );
    }

    #[test]
    fn expedited_on_glacier_flexible_is_valid() {
        let backend = MockBackend::new(vec![]).with_supported_tiers(|class| match class {
            StorageClass::S3GlacierFlexibleRetrieval => &[
                RestoreTier::Expedited,
                RestoreTier::Standard,
                RestoreTier::Bulk,
            ],
            _ => &[],
        });

        assert!(
            validate_restore_tier(
                &backend,
                &StorageClass::S3GlacierFlexibleRetrieval,
                RestoreTier::Expedited,
            )
            .is_ok()
        );
    }

    // ── Batch: multiple paths submitted under concurrency cap ────────

    #[tokio::test(start_paused = true)]
    async fn batch_processes_multiple_paths() {
        let backend = Arc::new(MockBackend::new(vec![
            RestoreState::Ready,
            RestoreState::Ready,
            RestoreState::Ready,
        ]));
        let orch = make_orchestrator(backend, 2, Duration::from_secs(3600));

        let paths: Vec<ObjectPath> = vec!["obj/a".into(), "obj/b".into(), "obj/c".into()];
        let results = orch.ensure_warm_batch(&paths).await.unwrap();

        assert_eq!(results.len(), 3);
        for r in &results {
            assert_eq!(*r, RestoreState::Ready);
        }
    }

    // ── Backoff helper tests ────────────────────────────────────────

    #[test]
    fn advance_backoff_applies_multiplier() {
        let next = advance_backoff(Duration::from_secs(30));
        assert_eq!(next, Duration::from_secs(45)); // 30 * 3/2 = 45
    }

    #[test]
    fn advance_backoff_respects_cap() {
        let next = advance_backoff(Duration::from_secs(500));
        // 500 * 3/2 = 750, but cap is 600.
        assert_eq!(next, BACKOFF_CAP);
    }

    #[test]
    fn full_jitter_within_bounds() {
        for _ in 0..100 {
            let d = full_jitter(Duration::from_secs(10));
            assert!(d <= Duration::from_secs(10));
        }
    }

    #[test]
    fn full_jitter_zero_returns_zero() {
        assert_eq!(full_jitter(Duration::ZERO), Duration::ZERO);
    }

    // ── Timeout test ────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn timeout_returns_archive_restore_timeout() {
        let mut states = Vec::new();
        for _ in 0..1000 {
            states.push(RestoreState::InProgress {
                started_at: "2026-01-01T00:00:00Z".into(),
                expected_ready_at: "2026-01-01T06:00:00Z".into(),
            });
        }
        let backend = Arc::new(MockBackend::new(states));
        let orch = make_orchestrator(backend, 16, Duration::from_secs(60));

        let result = orch.ensure_warm(&"obj/stuck".into()).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CrabError::ArchiveRestoreTimeout { .. }
        ));
    }
}
