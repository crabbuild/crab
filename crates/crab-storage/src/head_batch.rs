//! Batched resume check: determines which xorbs already exist on the remote.
//!
//! For small batches (below `HeadBatchConfig::threshold`), individual HEAD
//! requests are issued per xorb. For larger batches, xorbs are partitioned
//! by their 2-hex prefix and parallel LIST requests are used instead,
//! intersecting the results with the planned set.
//!
//! If the backend does not support LIST (returns `NotSupported`), the
//! implementation falls back to per-xorb HEADs and logs a one-time warning.
//!
//! Transient errors (5xx, timeouts) are retried according to
//! [`HeadBatchConfig::max_retries`]. After retries are exhausted, the
//! affected xorbs are conservatively treated as not-existing, which
//! causes them to be uploaded. Uploading a xorb that already exists is
//! safe (CAS keyed on content hash); failing to upload a genuinely
//! missing xorb is not, so we always err on the side of re-upload.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::error::{Result, StorageError};
use crate::retry::{RetryClass, RetryPolicy, retry, retry_class};

/// Abstraction over the object store for head-batch operations.
///
/// Allows the head-batch logic to be tested independently of the full
/// `Store` implementation.
pub trait HeadBatchStore: Send + Sync {
    /// Returns `true` if the xorb identified by `hash` exists on the remote.
    fn head_exists(&self, hash: &str) -> impl Future<Output = Result<bool>> + Send;

    /// Lists all xorb hashes under the given 2-hex prefix.
    ///
    /// Returns `Err(StorageError::NotSupported)` if the backend
    /// does not support listing.
    fn list_prefix(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> + Send;
}

/// Configuration for the batched HEAD resume check.
#[derive(Debug, Clone)]
pub struct HeadBatchConfig {
    /// Below this count, use per-xorb HEADs; at or above, use prefix LIST.
    pub threshold: usize,
    /// Maximum number of concurrent HEAD or LIST requests in flight.
    pub concurrency: usize,
    /// Maximum attempts (including the first) for a single HEAD or LIST
    /// request before the xorb is conservatively treated as needing upload.
    ///
    /// Mirrors `Config::max_retries`; defaults to 5.
    pub max_retries: u32,
}

impl Default for HeadBatchConfig {
    fn default() -> Self {
        Self {
            threshold: 32,
            concurrency: 64,
            max_retries: 5,
        }
    }
}

impl HeadBatchConfig {
    /// Build the [`RetryPolicy`] used when retrying individual HEAD or
    /// LIST calls. Uses `max_retries` for the attempt budget and the
    /// `DEFAULT` backoff shape (100 ms base, 10 s cap).
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_attempts: self.max_retries.max(1),
            base: Duration::from_millis(100),
            cap: Duration::from_secs(10),
        }
    }
}

/// Result of a batched HEAD resume check.
///
/// `existing` is the set of planned xorbs confirmed present on the remote.
/// `head_check_errors` counts HEAD/LIST requests that exhausted retries
/// on a transient error — those xorbs are *not* in `existing` and will be
/// re-uploaded conservatively. For the LIST path, a single exhausted
/// prefix counts as one error regardless of how many xorbs it covered.
#[derive(Debug, Clone, Default)]
pub struct HeadBatchOutcome {
    /// Planned xorbs confirmed to exist on the remote.
    pub existing: HashSet<String>,
    /// Number of HEAD or LIST requests that exhausted retries on transient
    /// errors and were treated as "unknown → upload".
    pub head_check_errors: u64,
}

/// Returns the set of `xorb_hashes` that already exist on the remote,
/// along with a count of transient failures encountered.
///
/// Dispatches to per-xorb HEADs or prefix-partitioned LISTs based on
/// `config.threshold`. In both paths, in-flight requests are bounded by
/// `config.concurrency`, and each individual HEAD/LIST is retried up
/// to `config.max_retries` on transient failures.
///
/// Xorbs whose HEAD/LIST exhausts retries with a transient error are
/// conservatively excluded from the returned set, causing them to be
/// uploaded; each such request bumps `HeadBatchOutcome::head_check_errors`.
/// Only genuinely fatal errors (auth, config) propagate.
pub async fn head_batch(
    xorb_hashes: &[String],
    store: &(impl HeadBatchStore + ?Sized),
    config: &HeadBatchConfig,
) -> Result<HeadBatchOutcome> {
    if xorb_hashes.is_empty() {
        return Ok(HeadBatchOutcome::default());
    }
    validate_hashes(xorb_hashes)?;

    // Semaphore::new panics on zero permits; clamp to at least 1.
    let permits = config.concurrency.max(1);
    let sem = Arc::new(Semaphore::new(permits));
    let policy = config.retry_policy();

    if xorb_hashes.len() < config.threshold {
        head_batch_point(xorb_hashes, store, &sem, &policy).await
    } else {
        let fell_back = AtomicBool::new(false);
        match head_batch_list(xorb_hashes, store, &sem, &policy, &fell_back).await {
            Ok(outcome) => Ok(outcome),
            Err(_) if fell_back.load(Ordering::Relaxed) => {
                // LIST was not supported; fall back to per-xorb HEADs.
                head_batch_point(xorb_hashes, store, &sem, &policy).await
            }
            Err(e) => Err(e),
        }
    }
}

/// Per-xorb HEAD path: issue one HEAD per hash, collect those that exist.
///
/// Each HEAD is retried up to `policy.max_attempts` on transient errors.
/// If retries are exhausted with a transient error, the xorb is omitted
/// from the result (treated as not-existing → will be uploaded). Only
/// fatal errors propagate.
async fn head_batch_point(
    xorb_hashes: &[String],
    store: &(impl HeadBatchStore + ?Sized),
    sem: &Arc<Semaphore>,
    policy: &RetryPolicy,
) -> Result<HeadBatchOutcome> {
    let mut outcome = HeadBatchOutcome::default();
    // Issue HEADs concurrently, bounded by the shared semaphore.
    let futures: Vec<_> = xorb_hashes
        .iter()
        .map(|hash| {
            let sem = sem.clone();
            async move {
                // acquire_owned cannot fail because we never close the semaphore.
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        return (
                            hash.clone(),
                            Err(StorageError::Internal(
                                "head_batch semaphore closed".to_owned(),
                            )),
                        );
                    }
                };
                let result = retry(policy, || store.head_exists(hash)).await;
                (hash.clone(), result)
            }
        })
        .collect();

    let results = futures_util::future::join_all(futures).await;
    for (hash, result) in results {
        match result {
            Ok(true) => {
                outcome.existing.insert(hash);
            }
            // NotFound is expected for missing xorbs.
            Ok(false) | Err(StorageError::NotFound { .. }) => {}
            Err(e) if is_retryable(&e) => {
                // Transient failure after retries exhausted: conservatively
                // treat the xorb as not-existing so it gets uploaded.
                outcome.head_check_errors += 1;
                tracing::warn!(
                    xorb_hash = %hash,
                    error = %e,
                    "HEAD exhausted retries on transient error, will upload conservatively"
                );
            }
            Err(e) => return Err(e),
        }
    }
    Ok(outcome)
}

/// Prefix-partitioned LIST path: group by 2-hex prefix, issue parallel LISTs,
/// intersect with the planned set.
///
/// Each LIST is retried up to `policy.max_attempts` on transient errors.
/// If retries are exhausted for a prefix with a transient error, that
/// prefix contributes nothing to the result set — its xorbs will be
/// uploaded conservatively rather than skipped.
async fn head_batch_list(
    xorb_hashes: &[String],
    store: &(impl HeadBatchStore + ?Sized),
    sem: &Arc<Semaphore>,
    policy: &RetryPolicy,
    fell_back: &AtomicBool,
) -> Result<HeadBatchOutcome> {
    // Build a lookup set for fast intersection.
    let planned: HashSet<String> = xorb_hashes.iter().cloned().collect();

    // Partition by 2-hex prefix (first byte of the hash).
    let mut prefix_groups: HashMap<String, Vec<String>> = HashMap::new();
    for hash in xorb_hashes {
        let prefix = hex_prefix(hash);
        prefix_groups.entry(prefix).or_default().push(hash.clone());
    }

    // Issue one LIST per unique prefix, concurrently, bounded by the shared
    // semaphore.
    let futures: Vec<_> = prefix_groups
        .keys()
        .map(|prefix| {
            let prefix = prefix.clone();
            let sem = sem.clone();
            async move {
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        return (
                            prefix.clone(),
                            Err(StorageError::Internal(
                                "head_batch semaphore closed".to_owned(),
                            )),
                        );
                    }
                };
                let result = retry(policy, || store.list_prefix(&prefix)).await;
                (prefix.clone(), result)
            }
        })
        .collect();

    let results = futures_util::future::join_all(futures).await;

    let mut outcome = HeadBatchOutcome::default();
    for (prefix, result) in results {
        match result {
            Ok(remote_hashes) => {
                for rh in remote_hashes {
                    if planned.contains(&rh) {
                        outcome.existing.insert(rh);
                    }
                }
            }
            Err(StorageError::NotSupported { .. }) => {
                tracing::warn!(
                    prefix = %prefix,
                    "LIST not supported by backend, falling back to per-xorb HEADs"
                );
                fell_back.store(true, Ordering::Relaxed);
                return Err(StorageError::Internal("list not supported".to_owned()));
            }
            Err(e) if is_retryable(&e) => {
                // Transient failure after retries exhausted: contribute
                // nothing for this prefix → its xorbs upload conservatively.
                // A single exhausted prefix counts as one error regardless
                // of how many planned xorbs it covered; the caller uses
                // this as a coarse "something went wrong" signal.
                outcome.head_check_errors += 1;
                tracing::warn!(
                    prefix = %prefix,
                    error = %e,
                    "LIST exhausted retries on transient error, will upload prefix conservatively"
                );
            }
            Err(e) => return Err(e),
        }
    }

    Ok(outcome)
}

/// Extracts the 2-hex-character prefix from a hash string.
fn hex_prefix(hash: &str) -> String {
    hash.chars().take(2).collect()
}

fn validate_hashes(xorb_hashes: &[String]) -> Result<()> {
    for hash in xorb_hashes {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StorageError::InvalidHash { hash: hash.clone() });
        }
    }
    Ok(())
}

/// Whether an error is the kind we conservatively treat as
/// "xorb unknown → upload" after retries are exhausted.
///
/// Retryable classes (`Transient`, `Throttled`, `StateDependent`,
/// `InspectErrno`) mean the HEAD/LIST couldn't reach a definitive answer.
/// Genuine fatals (auth, config, cancellation) are surfaced instead.
fn is_retryable(err: &StorageError) -> bool {
    !matches!(retry_class(err), RetryClass::Fatal)
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
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// Test store that tracks which methods were called and returns
    /// configurable results.
    struct TestStore {
        /// Hashes that "exist" on the remote.
        existing: HashSet<String>,
        /// If true, `list_prefix` returns `NotSupported`.
        list_not_supported: bool,
        /// Track HEAD calls for assertion.
        head_calls: Mutex<Vec<String>>,
        /// Track LIST calls for assertion.
        list_calls: Mutex<Vec<String>>,
    }

    impl TestStore {
        fn new(existing: HashSet<String>) -> Self {
            Self {
                existing,
                list_not_supported: false,
                head_calls: Mutex::new(Vec::new()),
                list_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_list_not_supported(mut self) -> Self {
            self.list_not_supported = true;
            self
        }

        fn head_call_count(&self) -> usize {
            self.head_calls.lock().unwrap().len()
        }

        fn list_call_count(&self) -> usize {
            self.list_calls.lock().unwrap().len()
        }

        fn list_prefixes_called(&self) -> Vec<String> {
            self.list_calls.lock().unwrap().clone()
        }
    }

    impl HeadBatchStore for TestStore {
        async fn head_exists(&self, hash: &str) -> Result<bool> {
            self.head_calls.lock().unwrap().push(hash.to_owned());
            Ok(self.existing.contains(hash))
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
            self.list_calls.lock().unwrap().push(prefix.to_owned());
            if self.list_not_supported {
                return Err(StorageError::NotSupported {
                    source: object_store::Error::NotSupported {
                        source: Box::<dyn std::error::Error + Send + Sync>::from(
                            "LIST not supported",
                        ),
                    },
                });
            }
            // Return all existing hashes that match this prefix.
            let matching: Vec<String> = self
                .existing
                .iter()
                .filter(|h| hex_prefix(h) == prefix)
                .cloned()
                .collect();
            Ok(matching)
        }
    }

    fn make_hash(seed: u64) -> String {
        format!("{seed:064x}")
    }

    #[tokio::test]
    async fn below_threshold_uses_head_requests() {
        let h1 = make_hash(1);
        let h2 = make_hash(2);
        let h3 = make_hash(3);

        let existing: HashSet<String> = [h1.clone(), h3.clone()].into_iter().collect();
        let store = TestStore::new(existing);
        let config = HeadBatchConfig {
            threshold: 32,
            ..HeadBatchConfig::default()
        };

        let result = head_batch(&[h1.clone(), h2.clone(), h3.clone()], &store, &config)
            .await
            .unwrap();

        assert!(result.existing.contains(&h1));
        assert!(!result.existing.contains(&h2));
        assert!(result.existing.contains(&h3));
        assert_eq!(result.head_check_errors, 0);
        assert_eq!(store.head_call_count(), 3);
        assert_eq!(store.list_call_count(), 0);
    }

    #[tokio::test]
    async fn above_threshold_uses_list_requests() {
        let hashes: Vec<String> = (0..40).map(make_hash).collect();
        let existing: HashSet<String> = hashes[..10].iter().cloned().collect();
        let store = TestStore::new(existing.clone());
        // Set threshold low so we trigger LIST path.
        let config = HeadBatchConfig {
            threshold: 5,
            ..HeadBatchConfig::default()
        };

        let result = head_batch(&hashes, &store, &config).await.unwrap();

        assert_eq!(result.existing, existing);
        assert_eq!(result.head_check_errors, 0);
        // Should have used LIST, not HEAD.
        assert_eq!(store.head_call_count(), 0);
        assert!(store.list_call_count() > 0);
    }

    #[tokio::test]
    async fn fallback_to_head_on_not_supported() {
        let h1 = make_hash(1);
        let h2 = make_hash(2);
        let existing: HashSet<String> = [h1.clone()].into_iter().collect();
        let store = TestStore::new(existing).with_list_not_supported();
        // Threshold of 1 forces LIST path, which will fail and fall back.
        let config = HeadBatchConfig {
            threshold: 1,
            ..HeadBatchConfig::default()
        };

        let result = head_batch(&[h1.clone(), h2.clone()], &store, &config)
            .await
            .unwrap();

        assert!(result.existing.contains(&h1));
        assert!(!result.existing.contains(&h2));
        // Should have attempted LIST, then fallen back to HEAD.
        assert!(store.list_call_count() > 0);
        assert_eq!(store.head_call_count(), 2);
    }

    #[tokio::test]
    async fn correct_partitioning_by_prefix() {
        // Create hashes that we know have different prefixes.
        let h1 = make_hash(1);
        let h2 = make_hash(2);
        let h3 = make_hash(3);

        let prefix1 = hex_prefix(&h1);
        let prefix2 = hex_prefix(&h2);
        let prefix3 = hex_prefix(&h3);

        let existing: HashSet<String> = [h1.clone(), h2.clone(), h3.clone()].into_iter().collect();
        let store = TestStore::new(existing);
        let config = HeadBatchConfig {
            threshold: 1,
            ..HeadBatchConfig::default()
        };

        let result = head_batch(&[h1.clone(), h2.clone(), h3.clone()], &store, &config)
            .await
            .unwrap();

        assert_eq!(result.existing.len(), 3);

        // Verify that LIST was called for each unique prefix.
        let prefixes_called = store.list_prefixes_called();
        let unique_expected: HashSet<String> = [prefix1, prefix2, prefix3].into_iter().collect();
        let unique_called: HashSet<String> = prefixes_called.into_iter().collect();
        assert_eq!(unique_called, unique_expected);
    }

    #[tokio::test]
    async fn empty_input_returns_empty_set() {
        let store = TestStore::new(HashSet::new());
        let config = HeadBatchConfig::default();

        let result = head_batch(&[], &store, &config).await.unwrap();
        assert!(result.existing.is_empty());
        assert_eq!(result.head_check_errors, 0);
        assert_eq!(store.head_call_count(), 0);
        assert_eq!(store.list_call_count(), 0);
    }

    #[tokio::test]
    async fn list_returns_superset_but_only_planned_hashes_returned() {
        // The remote has extra hashes under the same prefix that aren't planned.
        let planned = make_hash(1);
        let extra = make_hash(100);

        // Both share the same prefix in this test store.
        let mut existing = HashSet::new();
        existing.insert(planned.clone());
        existing.insert(extra.clone());

        let store = TestStore::new(existing);
        let config = HeadBatchConfig {
            threshold: 1,
            ..HeadBatchConfig::default()
        };

        let result = head_batch(std::slice::from_ref(&planned), &store, &config)
            .await
            .unwrap();

        assert!(result.existing.contains(&planned));
        // Extra hash should NOT be in the result since it wasn't planned.
        assert!(!result.existing.contains(&extra));
    }

    /// Test store that counts concurrent HEAD calls and records the peak,
    /// so we can verify `HeadBatchConfig::concurrency` actually bounds
    /// in-flight requests.
    struct ConcurrencyTrackingStore {
        in_flight: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    impl ConcurrencyTrackingStore {
        fn new() -> Self {
            Self {
                in_flight: std::sync::atomic::AtomicUsize::new(0),
                peak: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn peak(&self) -> usize {
            self.peak.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl HeadBatchStore for ConcurrencyTrackingStore {
        async fn head_exists(&self, _hash: &str) -> Result<bool> {
            let cur = self
                .in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            // Monotonically bump the observed peak.
            self.peak
                .fetch_max(cur, std::sync::atomic::Ordering::SeqCst);
            // Yield so other pending HEADs get a chance to race in.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            Ok(false)
        }

        async fn list_prefix(&self, _prefix: &str) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn head_batch_bounds_in_flight_requests_by_concurrency() {
        // 32 HEADs with concurrency=4 should never exceed 4 in flight.
        let hashes: Vec<String> = (0..32).map(make_hash).collect();
        let store = ConcurrencyTrackingStore::new();
        let config = HeadBatchConfig {
            threshold: usize::MAX, // force point path
            concurrency: 4,
            ..HeadBatchConfig::default()
        };

        let _ = head_batch(&hashes, &store, &config).await.unwrap();

        let peak = store.peak();
        assert!(
            peak <= 4,
            "observed peak of {peak} in-flight HEADs, expected <= 4"
        );
        // Sanity: at least one request observed.
        assert!(peak >= 1, "expected at least one HEAD to be observed");
    }

    // --- Transient HEAD/LIST failure handling ---

    /// Store whose HEAD fails transiently for a target hash N-1 times,
    /// then either succeeds (if `succeed_after`) or keeps failing.
    struct FlakyStore {
        target: String,
        fail_count: std::sync::atomic::AtomicU32,
        max_failures: u32,
        succeed_after: bool,
        /// Non-target hashes are reported as existing if here.
        existing: HashSet<String>,
    }

    impl FlakyStore {
        fn new(target: String, max_failures: u32, succeed_after: bool) -> Self {
            Self {
                target,
                fail_count: std::sync::atomic::AtomicU32::new(0),
                max_failures,
                succeed_after,
                existing: HashSet::new(),
            }
        }

        fn attempts(&self) -> u32 {
            self.fail_count.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn transient_storage_err() -> StorageError {
        StorageError::NetworkTransient {
            source: object_store::Error::Generic {
                store: "S3",
                source: Box::<dyn std::error::Error + Send + Sync>::from("simulated 5xx"),
            },
        }
    }

    impl HeadBatchStore for FlakyStore {
        async fn head_exists(&self, hash: &str) -> Result<bool> {
            if hash == self.target {
                let n = self
                    .fail_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < self.max_failures {
                    return Err(transient_storage_err());
                }
                if self.succeed_after {
                    return Ok(true);
                }
                return Err(transient_storage_err());
            }
            Ok(self.existing.contains(hash))
        }

        async fn list_prefix(&self, _prefix: &str) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn head_retries_transient_errors_and_succeeds() {
        // Fails twice, succeeds on 3rd attempt → recorded as existing.
        let target = make_hash(42);
        let store = FlakyStore::new(target.clone(), 2, true);
        let config = HeadBatchConfig {
            threshold: usize::MAX, // point path
            max_retries: 5,
            ..HeadBatchConfig::default()
        };

        let result = head_batch(std::slice::from_ref(&target), &store, &config)
            .await
            .unwrap();
        assert!(result.existing.contains(&target));
        assert_eq!(result.head_check_errors, 0);
        assert_eq!(store.attempts(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn head_exhausted_retries_treated_as_not_existing() {
        // Always fails → after retries are exhausted, treat as not existing
        // so the xorb gets uploaded conservatively.
        let target = make_hash(7);
        let store = FlakyStore::new(target.clone(), u32::MAX, false);
        let config = HeadBatchConfig {
            threshold: usize::MAX,
            max_retries: 3,
            ..HeadBatchConfig::default()
        };

        let result = head_batch(std::slice::from_ref(&target), &store, &config)
            .await
            .unwrap();
        assert!(
            !result.existing.contains(&target),
            "transient-exhausted HEAD must not be marked as existing"
        );
        assert_eq!(
            result.head_check_errors, 1,
            "transient-exhausted HEAD must bump head_check_errors"
        );
        assert_eq!(store.attempts(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn head_fatal_error_propagates_immediately() {
        // Auth errors must never be swallowed — they need user attention.
        struct AuthStore;
        impl HeadBatchStore for AuthStore {
            async fn head_exists(&self, _hash: &str) -> Result<bool> {
                Err(StorageError::AuthFailed {
                    path: "test".into(),
                })
            }
            async fn list_prefix(&self, _prefix: &str) -> Result<Vec<String>> {
                Ok(Vec::new())
            }
        }
        let config = HeadBatchConfig {
            threshold: usize::MAX,
            max_retries: 3,
            ..HeadBatchConfig::default()
        };

        let err = head_batch(&[make_hash(1)], &AuthStore, &config)
            .await
            .expect_err("auth failure must surface");
        assert!(matches!(err, StorageError::AuthFailed { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn list_exhausted_retries_skips_prefix_conservatively() {
        // LIST always fails transiently → that prefix's xorbs should not
        // be marked as existing, so they'll be uploaded.
        struct FlakyListStore {
            attempts: std::sync::atomic::AtomicU32,
        }
        impl HeadBatchStore for FlakyListStore {
            async fn head_exists(&self, _hash: &str) -> Result<bool> {
                Ok(false)
            }
            async fn list_prefix(&self, _prefix: &str) -> Result<Vec<String>> {
                self.attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(transient_storage_err())
            }
        }
        let store = FlakyListStore {
            attempts: std::sync::atomic::AtomicU32::new(0),
        };
        // Force LIST path with threshold=1.
        let config = HeadBatchConfig {
            threshold: 1,
            max_retries: 3,
            ..HeadBatchConfig::default()
        };

        let h = make_hash(99);
        let result = head_batch(&[h], &store, &config).await.unwrap();
        assert!(
            result.existing.is_empty(),
            "LIST-exhausted prefix must contribute nothing to existing set"
        );
        assert_eq!(
            result.head_check_errors, 1,
            "LIST-exhausted prefix must bump head_check_errors"
        );
        assert!(
            store.attempts.load(std::sync::atomic::Ordering::SeqCst) >= 3,
            "LIST should have been retried up to max_retries"
        );
    }
}
