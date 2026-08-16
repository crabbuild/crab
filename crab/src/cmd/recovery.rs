//! Crash recovery — auto-recovery scan and post-ref-cleanup.
//!
//! Auto-recovery scan runs at operation start (push/fetch). It lists
//! `staging/push-*.inflight` markers and:
//! - Stale markers (older than retention) → cleaned up
//! - Live markers → retry pending uploads via HEAD check
//!
//! Post-ref-cleanup moves staging data → cache, installs the shard,
//! and deletes files.db rows for the committed push.

use std::time::{Duration, SystemTime};

use tracing::{debug, info, warn};

use crate::core::error::{Result, check_cancelled};

// ---------------------------------------------------------------------------
// Inflight marker metadata
// ---------------------------------------------------------------------------

/// Metadata for a `push-{uuid}.inflight` marker discovered during recovery.
#[derive(Debug, Clone)]
pub struct InflightMarker {
    /// The push ID extracted from the marker filename.
    pub push_id: String,
    /// When the marker was created (file mtime or embedded timestamp).
    pub created: SystemTime,
}

// ---------------------------------------------------------------------------
// Recovery scan outcome
// ---------------------------------------------------------------------------

/// Structured outcome of a recovery scan.
#[derive(Debug, Clone, Default)]
pub struct RecoveryScanOutcome {
    /// Number of stale markers cleaned up.
    pub stale_cleaned: u64,
    /// Number of live markers found and retried.
    pub live_retried: u64,
    /// Number of retry attempts that failed.
    pub retry_failures: u64,
}

impl RecoveryScanOutcome {
    fn log(&self) {
        if self.stale_cleaned == 0 && self.live_retried == 0 {
            debug!("recovery scan: no inflight markers found");
        } else {
            info!(
                stale_cleaned = self.stale_cleaned,
                live_retried = self.live_retried,
                retry_failures = self.retry_failures,
                "recovery scan complete"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Recovery store trait — abstraction for testability
// ---------------------------------------------------------------------------

/// Trait abstracting the storage operations needed by recovery.
///
/// In production, this wraps the staging area and object store.
/// In tests, a mock returns canned results.
pub trait RecoveryStore: Send + Sync {
    /// List all `push-*.inflight` markers with their creation times.
    fn list_inflight_markers(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<InflightMarker>>> + Send + '_>>;

    /// Remove a stale inflight marker.
    fn clean_marker(
        &self,
        push_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// Check whether a pending upload is still live via HEAD request.
    /// Returns `true` if the upload target exists in storage (already committed).
    fn check_upload_exists(
        &self,
        push_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>;

    /// Retry a pending upload for a live inflight marker.
    fn retry_upload(
        &self,
        push_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;
}

/// Trait abstracting the post-ref-cleanup operations.
pub trait PostRefCleaner: Send + Sync {
    /// Move staging data to cache for the given push.
    fn move_staging_to_cache(
        &self,
        push_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// Install the shard produced by the push.
    fn install_shard(
        &self,
        push_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// Delete files.db rows for the committed push.
    fn delete_files_db_rows(
        &self,
        push_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// Remove the inflight marker after successful cleanup.
    fn clear_marker(
        &self,
        push_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;
}

// ---------------------------------------------------------------------------
// Auto-recovery scan
// ---------------------------------------------------------------------------

/// Run the auto-recovery scan at operation start.
///
/// Lists `staging/push-*.inflight` markers and processes them:
/// - Stale (older than `retention`) → clean up the marker
/// - Live → check if the upload target exists; if not, retry
///
/// # Errors
///
/// Returns [`crate::core::error::CrabError::Cancelled`] on SIGINT,
/// or propagates storage errors.
pub async fn run_recovery_scan(
    store: &dyn RecoveryStore,
    retention: Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<RecoveryScanOutcome> {
    let now = SystemTime::now();
    let mut outcome = RecoveryScanOutcome::default();

    check_cancelled(cancel)?;
    let markers = store.list_inflight_markers().await?;

    if markers.is_empty() {
        outcome.log();
        return Ok(outcome);
    }

    debug!(marker_count = markers.len(), "found inflight markers");

    for marker in &markers {
        check_cancelled(cancel)?;

        let age = now.duration_since(marker.created).unwrap_or(Duration::ZERO);

        if age > retention {
            // Stale marker — clean it up.
            debug!(
                push_id = %marker.push_id,
                age_secs = age.as_secs(),
                "cleaning stale inflight marker"
            );
            match store.clean_marker(&marker.push_id).await {
                Ok(()) => outcome.stale_cleaned += 1,
                Err(e) => {
                    warn!(
                        push_id = %marker.push_id,
                        error = %e,
                        "failed to clean stale marker"
                    );
                }
            }
        } else {
            // Live marker — check if upload already committed, otherwise retry.
            debug!(
                push_id = %marker.push_id,
                age_secs = age.as_secs(),
                "checking live inflight marker"
            );
            match store.check_upload_exists(&marker.push_id).await {
                Ok(true) => {
                    // Upload already committed — just clean the marker.
                    debug!(push_id = %marker.push_id, "upload already committed, cleaning marker");
                    if let Err(e) = store.clean_marker(&marker.push_id).await {
                        warn!(push_id = %marker.push_id, error = %e, "failed to clean committed marker");
                    }
                    outcome.live_retried += 1;
                }
                Ok(false) => {
                    // Upload not committed — retry.
                    info!(push_id = %marker.push_id, "retrying pending upload");
                    match store.retry_upload(&marker.push_id).await {
                        Ok(()) => outcome.live_retried += 1,
                        Err(e) => {
                            warn!(
                                push_id = %marker.push_id,
                                error = %e,
                                "retry failed for pending upload"
                            );
                            outcome.retry_failures += 1;
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        push_id = %marker.push_id,
                        error = %e,
                        "HEAD check failed for pending upload"
                    );
                    outcome.retry_failures += 1;
                }
            }
        }
    }

    outcome.log();
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Post-ref-cleanup
// ---------------------------------------------------------------------------

/// Post-ref-cleanup outcome.
#[derive(Debug, Clone, Default)]
pub struct PostRefCleanupOutcome {
    /// Whether staging → cache move succeeded.
    pub staging_moved: bool,
    /// Whether shard installation succeeded.
    pub shard_installed: bool,
    /// Whether files.db row deletion succeeded.
    pub db_rows_deleted: bool,
    /// Whether the inflight marker was cleared.
    pub marker_cleared: bool,
}

/// Run post-ref-cleanup for a committed push.
///
/// Moves staging data → cache, installs the shard, deletes files.db rows,
/// and clears the inflight marker.
///
/// Each step is independent — failures in one step don't prevent the others
/// from running. The caller can inspect the outcome to see which steps
/// succeeded.
///
/// # Errors
///
/// Returns [`crate::core::error::CrabError::Cancelled`] on SIGINT.
pub async fn post_ref_cleanup(
    push_id: &str,
    cleaner: &dyn PostRefCleaner,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<PostRefCleanupOutcome> {
    let mut outcome = PostRefCleanupOutcome::default();

    // Step 1: Move staging → cache.
    check_cancelled(cancel)?;
    match cleaner.move_staging_to_cache(push_id).await {
        Ok(()) => {
            outcome.staging_moved = true;
            debug!(push_id = %push_id, "staging moved to cache");
        }
        Err(e) => {
            warn!(push_id = %push_id, error = %e, "failed to move staging to cache");
        }
    }

    // Step 2: Install shard.
    check_cancelled(cancel)?;
    match cleaner.install_shard(push_id).await {
        Ok(()) => {
            outcome.shard_installed = true;
            debug!(push_id = %push_id, "shard installed");
        }
        Err(e) => {
            warn!(push_id = %push_id, error = %e, "failed to install shard");
        }
    }

    // Step 3: Delete files.db rows.
    check_cancelled(cancel)?;
    match cleaner.delete_files_db_rows(push_id).await {
        Ok(()) => {
            outcome.db_rows_deleted = true;
            debug!(push_id = %push_id, "files.db rows deleted");
        }
        Err(e) => {
            warn!(push_id = %push_id, error = %e, "failed to delete files.db rows");
        }
    }

    // Step 4: Clear inflight marker.
    check_cancelled(cancel)?;
    match cleaner.clear_marker(push_id).await {
        Ok(()) => {
            outcome.marker_cleared = true;
            debug!(push_id = %push_id, "inflight marker cleared");
        }
        Err(e) => {
            warn!(push_id = %push_id, error = %e, "failed to clear inflight marker");
        }
    }

    info!(
        push_id = %push_id,
        staging_moved = outcome.staging_moved,
        shard_installed = outcome.shard_installed,
        db_rows_deleted = outcome.db_rows_deleted,
        marker_cleared = outcome.marker_cleared,
        "post-ref-cleanup complete"
    );

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::error::CrabError;
    use tokio_util::sync::CancellationToken;

    // --- Mock recovery store ---

    struct MockRecoveryStore {
        markers: Vec<InflightMarker>,
        /// Push IDs whose uploads are already committed (HEAD returns true).
        committed: std::collections::HashSet<String>,
        cleaned: std::sync::Mutex<Vec<String>>,
        retried: std::sync::Mutex<Vec<String>>,
    }

    impl MockRecoveryStore {
        fn new(markers: Vec<InflightMarker>) -> Self {
            Self {
                markers,
                committed: std::collections::HashSet::new(),
                cleaned: std::sync::Mutex::new(Vec::new()),
                retried: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_committed(mut self, ids: &[&str]) -> Self {
            for id in ids {
                self.committed.insert(id.to_string());
            }
            self
        }
    }

    impl RecoveryStore for MockRecoveryStore {
        fn list_inflight_markers(
            &self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<InflightMarker>>> + Send + '_>,
        > {
            let markers = self.markers.clone();
            Box::pin(async move { Ok(markers) })
        }

        fn clean_marker(
            &self,
            push_id: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
            if let Ok(mut v) = self.cleaned.lock() {
                v.push(push_id.to_string());
            }
            Box::pin(async { Ok(()) })
        }

        fn check_upload_exists(
            &self,
            push_id: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>
        {
            let exists = self.committed.contains(push_id);
            Box::pin(async move { Ok(exists) })
        }

        fn retry_upload(
            &self,
            push_id: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
            if let Ok(mut v) = self.retried.lock() {
                v.push(push_id.to_string());
            }
            Box::pin(async { Ok(()) })
        }
    }

    // --- Mock post-ref cleaner ---

    struct MockPostRefCleaner {
        staging_moved: std::sync::Mutex<Vec<String>>,
        shards_installed: std::sync::Mutex<Vec<String>>,
        db_rows_deleted: std::sync::Mutex<Vec<String>>,
        markers_cleared: std::sync::Mutex<Vec<String>>,
    }

    impl MockPostRefCleaner {
        fn new() -> Self {
            Self {
                staging_moved: std::sync::Mutex::new(Vec::new()),
                shards_installed: std::sync::Mutex::new(Vec::new()),
                db_rows_deleted: std::sync::Mutex::new(Vec::new()),
                markers_cleared: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl PostRefCleaner for MockPostRefCleaner {
        fn move_staging_to_cache(
            &self,
            push_id: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
            if let Ok(mut v) = self.staging_moved.lock() {
                v.push(push_id.to_string());
            }
            Box::pin(async { Ok(()) })
        }

        fn install_shard(
            &self,
            push_id: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
            if let Ok(mut v) = self.shards_installed.lock() {
                v.push(push_id.to_string());
            }
            Box::pin(async { Ok(()) })
        }

        fn delete_files_db_rows(
            &self,
            push_id: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
            if let Ok(mut v) = self.db_rows_deleted.lock() {
                v.push(push_id.to_string());
            }
            Box::pin(async { Ok(()) })
        }

        fn clear_marker(
            &self,
            push_id: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
            if let Ok(mut v) = self.markers_cleared.lock() {
                v.push(push_id.to_string());
            }
            Box::pin(async { Ok(()) })
        }
    }

    // --- Recovery scan tests ---

    #[tokio::test]
    async fn recovery_scan_no_markers() {
        let store = MockRecoveryStore::new(vec![]);
        let cancel = CancellationToken::new();
        let retention = Duration::from_secs(3600);

        let outcome = run_recovery_scan(&store, retention, &cancel)
            .await
            .expect("should succeed");

        assert_eq!(outcome.stale_cleaned, 0);
        assert_eq!(outcome.live_retried, 0);
    }

    #[tokio::test]
    async fn recovery_scan_cleans_stale_markers() {
        let now = SystemTime::now();
        let store = MockRecoveryStore::new(vec![
            InflightMarker {
                push_id: "stale-1".to_string(),
                created: now - Duration::from_secs(7200),
            },
            InflightMarker {
                push_id: "stale-2".to_string(),
                created: now - Duration::from_secs(14400),
            },
        ]);
        let cancel = CancellationToken::new();
        let retention = Duration::from_secs(3600);

        let outcome = run_recovery_scan(&store, retention, &cancel)
            .await
            .expect("should succeed");

        assert_eq!(outcome.stale_cleaned, 2);
        assert_eq!(outcome.live_retried, 0);
        let cleaned = store.cleaned.lock().expect("lock");
        assert_eq!(cleaned.len(), 2);
    }

    #[tokio::test]
    async fn recovery_scan_retries_live_markers() {
        let now = SystemTime::now();
        let store = MockRecoveryStore::new(vec![InflightMarker {
            push_id: "live-1".to_string(),
            created: now - Duration::from_secs(600),
        }]);
        let cancel = CancellationToken::new();
        let retention = Duration::from_secs(3600);

        let outcome = run_recovery_scan(&store, retention, &cancel)
            .await
            .expect("should succeed");

        assert_eq!(outcome.stale_cleaned, 0);
        assert_eq!(outcome.live_retried, 1);
        let retried = store.retried.lock().expect("lock");
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0], "live-1");
    }

    #[tokio::test]
    async fn recovery_scan_committed_upload_cleans_marker() {
        let now = SystemTime::now();
        let store = MockRecoveryStore::new(vec![InflightMarker {
            push_id: "committed-1".to_string(),
            created: now - Duration::from_secs(600),
        }])
        .with_committed(&["committed-1"]);
        let cancel = CancellationToken::new();
        let retention = Duration::from_secs(3600);

        let outcome = run_recovery_scan(&store, retention, &cancel)
            .await
            .expect("should succeed");

        assert_eq!(outcome.live_retried, 1);
        let cleaned = store.cleaned.lock().expect("lock");
        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0], "committed-1");
        // Should NOT have retried since upload was already committed.
        let retried = store.retried.lock().expect("lock");
        assert!(retried.is_empty());
    }

    #[tokio::test]
    async fn recovery_scan_mixed_stale_and_live() {
        let now = SystemTime::now();
        let store = MockRecoveryStore::new(vec![
            InflightMarker {
                push_id: "stale-1".to_string(),
                created: now - Duration::from_secs(7200),
            },
            InflightMarker {
                push_id: "live-1".to_string(),
                created: now - Duration::from_secs(300),
            },
        ]);
        let cancel = CancellationToken::new();
        let retention = Duration::from_secs(3600);

        let outcome = run_recovery_scan(&store, retention, &cancel)
            .await
            .expect("should succeed");

        assert_eq!(outcome.stale_cleaned, 1);
        assert_eq!(outcome.live_retried, 1);
    }

    #[tokio::test]
    async fn recovery_scan_respects_cancellation() {
        let store = MockRecoveryStore::new(vec![]);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = run_recovery_scan(&store, Duration::from_secs(3600), &cancel).await;
        assert!(matches!(result, Err(CrabError::Cancelled)));
    }

    // --- Post-ref-cleanup tests ---

    #[tokio::test]
    async fn post_ref_cleanup_runs_all_steps() {
        let cleaner = MockPostRefCleaner::new();
        let cancel = CancellationToken::new();

        let outcome = post_ref_cleanup("push-abc", &cleaner, &cancel)
            .await
            .expect("should succeed");

        assert!(outcome.staging_moved);
        assert!(outcome.shard_installed);
        assert!(outcome.db_rows_deleted);
        assert!(outcome.marker_cleared);

        let moved = cleaner.staging_moved.lock().expect("lock");
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0], "push-abc");

        let installed = cleaner.shards_installed.lock().expect("lock");
        assert_eq!(installed.len(), 1);

        let deleted = cleaner.db_rows_deleted.lock().expect("lock");
        assert_eq!(deleted.len(), 1);

        let cleared = cleaner.markers_cleared.lock().expect("lock");
        assert_eq!(cleared.len(), 1);
    }

    #[tokio::test]
    async fn post_ref_cleanup_respects_cancellation() {
        let cleaner = MockPostRefCleaner::new();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = post_ref_cleanup("push-abc", &cleaner, &cancel).await;
        assert!(matches!(result, Err(CrabError::Cancelled)));
    }
}
