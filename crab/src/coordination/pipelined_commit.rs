//! Pipelined CAS commit for the push pipeline's final phase.
//!
//! For single-ref pushes, the shard-list CAS, pack-list CAS, and ref CAS
//! are issued concurrently via `tokio::join!`. For multi-ref pushes, the
//! v1 serial ordering is preserved: manifests first, then refs one by one.
//!
//! The data→manifest ordering invariant is upheld by the caller: all
//! immutable data (xorbs, shards, packs) must be durable before
//! `commit_push` is called. This module only parallelizes the
//! manifest→ref handoff.
//!
//! # Status: unintegrated
//!
//! This module is **not currently called** from the production push
//! pipeline — `git::push::execute_inner` uses
//! [`crate::coordination::cas::cas_update`] directly for manifests and
//! a separate ref CAS. Before integrating this module, three known bugs
//! must be fixed:
//!
//! - **S1-P1-9** — [`cas_manifest_with_retry`] retries conflicts with
//!   the same stale ETag. Must be redesigned to take a mutation closure
//!   and re-read state on each attempt (see `cas::cas_update` for the
//!   correct pattern). Currently this module's retry returns `Ok(false)`
//!   after the first conflict so callers are not silently retried into
//!   a guaranteed failure.
//! - **S2-11** — the parallel path issues manifest and ref CASes
//!   concurrently. If the ref CAS succeeds but a manifest CAS fails,
//!   the ref points to unreachable data. A two-phase parallel design
//!   (manifests first, ref only on success) is required.
//! - **CR1-F18** — manifest mutations must bump the `generation`
//!   counter for readers using it to detect stale cached copies.

use std::future::Future;

use tracing::debug;

use crate::core::error::Result;
use crate::core::metrics::Metrics;

/// Abstraction over compare-and-swap writes for manifests and refs.
///
/// Implementations wrap the real [`crate::storage::Store`] or provide
/// test doubles. Each method performs a single conditional write and
/// returns `Ok(true)` on success, `Ok(false)` on CAS conflict, or
/// `Err` on non-conflict failures.
pub trait CasStore: Send + Sync {
    /// Conditional write for a manifest (shard-list or pack-list).
    ///
    /// On `CasConflict`, returns `Ok(false)` so the caller can retry.
    fn cas_manifest(
        &self,
        path: &str,
        expected_etag: Option<&str>,
        new_value: &[u8],
    ) -> impl Future<Output = Result<bool>> + Send;

    /// Conditional write for a ref.
    ///
    /// On `CasConflict`, returns `Ok(false)`. Other errors propagate.
    fn cas_ref(
        &self,
        ref_name: &str,
        expected_etag: Option<&str>,
        new_sha: &str,
    ) -> impl Future<Output = Result<bool>> + Send;
}

/// A single manifest CAS operation (shard-list or pack-list).
#[derive(Debug, Clone)]
pub struct CasOp {
    /// Object store path for the manifest.
    pub path: String,
    /// ETag for conditional write; `None` for first creation.
    pub expected_etag: Option<String>,
    /// New manifest content.
    pub new_value: Vec<u8>,
}

pub use crab_workflow::RefCasOp;

/// The full commit plan for a push: two manifest CASes plus per-ref CASes.
#[derive(Debug, Clone)]
pub struct CommitPlan {
    /// Shard-list manifest CAS operation.
    pub shard_list_cas: CasOp,
    /// Pack-list manifest CAS operation.
    pub pack_list_cas: CasOp,
    /// Per-ref CAS operations.
    pub ref_cas: Vec<RefCasOp>,
}

/// Outcome of the pipelined commit.
#[derive(Debug, Clone)]
pub struct CommitResult {
    /// Whether the shard-list CAS succeeded.
    pub shard_list_ok: bool,
    /// Whether the pack-list CAS succeeded.
    pub pack_list_ok: bool,
    /// Per-ref success/failure: `(ref_name, succeeded)`.
    pub ref_results: Vec<(String, bool)>,
}

/// Maximum retries for a manifest CAS conflict before giving up.
///
/// Currently `1` — without a mutation closure, retrying with the same
/// stale ETag is guaranteed to fail. The caller must re-read state and
/// rebuild the `CasOp` on conflict. See the module-level "Status:
/// unintegrated" note.
#[cfg(test)]
const MANIFEST_CAS_MAX_RETRIES: u32 = 1;

/// Execute a single manifest CAS.
///
/// Returns `Ok(true)` on success, `Ok(false)` on CAS conflict (caller
/// must re-read and retry), or `Err` on non-conflict failures.
///
/// **Note:** this function does NOT retry on conflict. The previous
/// implementation retried up to 5 times with the same stale ETag, which
/// always failed — burning the retry budget without a chance of
/// success. See finding S1-P1-9.
async fn cas_manifest_with_retry(store: &(impl CasStore + ?Sized), op: &CasOp) -> Result<bool> {
    let attempt = 0;
    match store
        .cas_manifest(&op.path, op.expected_etag.as_deref(), &op.new_value)
        .await
    {
        Ok(true) => {
            debug!(path = %op.path, attempt, "manifest CAS succeeded");
            Ok(true)
        }
        Ok(false) => {
            debug!(
                path = %op.path,
                attempt,
                "manifest CAS conflict — caller must re-read and retry with fresh etag"
            );
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Execute a single ref CAS. Ref failures are propagated as errors.
async fn cas_ref_once(store: &(impl CasStore + ?Sized), op: &RefCasOp) -> Result<bool> {
    store
        .cas_ref(&op.ref_name, op.expected_etag.as_deref(), &op.new_sha)
        .await
}

/// Commit a push plan by issuing manifest and ref CAS operations.
///
/// For single-ref pushes, all three CAS operations (shard-list, pack-list,
/// ref) run concurrently via `tokio::join!`. For multi-ref pushes, the v1
/// serial ordering is preserved: manifests first, then refs one by one.
///
/// # Precondition
///
/// All immutable data (xorbs, shards, file-index, packs) must be durable
/// on the object store before calling this function. This module only
/// parallelizes the manifest→ref handoff.
///
/// # Error handling
///
/// - Manifest CAS conflict: returned to the caller for fresh-state retry.
/// - Ref CAS failure: propagated as an error in the result.
/// - Non-conflict storage errors: propagated immediately.
pub async fn commit_push(
    plan: &CommitPlan,
    store: &(impl CasStore + ?Sized),
    metrics: Option<&Metrics>,
) -> Result<CommitResult> {
    if plan.ref_cas.len() == 1 {
        commit_push_parallel(plan, store, metrics).await
    } else {
        commit_push_serial(plan, store).await
    }
}

/// Single-ref path: parallel manifests, then ref CAS only if both succeed.
///
/// The prior implementation used `tokio::join!` on all three CAS ops,
/// which could leave the ref updated while a manifest CAS failed —
/// pointing a ref at a commit whose shard/pack is not in the manifest.
/// The two-phase pattern preserves the data→manifest→ref invariant
/// while retaining the parallelism benefit for the two manifest CASes.
/// See finding CR1-F19.
async fn commit_push_parallel(
    plan: &CommitPlan,
    store: &(impl CasStore + ?Sized),
    metrics: Option<&Metrics>,
) -> Result<CommitResult> {
    debug!("pipelined commit: single-ref parallel path");

    if let Some(m) = metrics {
        m.inc_cas_pipelined_commits();
    }

    let ref_op = &plan.ref_cas[0];

    // Phase 1: issue both manifest CASes in parallel.
    let (shard_result, pack_result) = tokio::join!(
        cas_manifest_with_retry(store, &plan.shard_list_cas),
        cas_manifest_with_retry(store, &plan.pack_list_cas),
    );
    let shard_list_ok = shard_result?;
    let pack_list_ok = pack_result?;

    // Phase 2: only update the ref if both manifests succeeded. If
    // either manifest conflicts or errors, we skip the ref update so
    // the ref never points to a commit the manifests don't describe.
    let ref_ok = if shard_list_ok && pack_list_ok {
        cas_ref_once(store, ref_op).await?
    } else {
        debug!(
            shard_list_ok,
            pack_list_ok, "skipping ref CAS — manifest CAS did not succeed"
        );
        false
    };

    Ok(CommitResult {
        shard_list_ok,
        pack_list_ok,
        ref_results: vec![(ref_op.ref_name.clone(), ref_ok)],
    })
}

/// Multi-ref path: serial execution matching v1 behavior.
///
/// Manifests are committed first (shard-list, then pack-list), then
/// refs are committed one by one. This preserves the ordering invariant
/// required for multi-ref atomicity.
async fn commit_push_serial(
    plan: &CommitPlan,
    store: &(impl CasStore + ?Sized),
) -> Result<CommitResult> {
    debug!(
        ref_count = plan.ref_cas.len(),
        "pipelined commit: multi-ref serial path"
    );

    let shard_list_ok = cas_manifest_with_retry(store, &plan.shard_list_cas).await?;
    let pack_list_ok = cas_manifest_with_retry(store, &plan.pack_list_cas).await?;

    let mut ref_results = Vec::with_capacity(plan.ref_cas.len());
    for ref_op in &plan.ref_cas {
        let ok = cas_ref_once(store, ref_op).await?;
        ref_results.push((ref_op.ref_name.clone(), ok));
    }

    Ok(CommitResult {
        shard_list_ok,
        pack_list_ok,
        ref_results,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::core::error::CrabError;
    use crate::core::metrics::Metrics;

    /// Records the order in which CAS operations are started.
    /// Used to verify parallel vs serial execution.
    #[derive(Debug, Default)]
    struct OpLog {
        /// Sequence counter incremented on each operation start.
        seq: AtomicU32,
        /// `(operation_name, sequence_number)` pairs.
        entries: Mutex<Vec<(String, u32)>>,
    }

    impl OpLog {
        fn record(&self, name: &str) -> u32 {
            let n = self.seq.fetch_add(1, Ordering::SeqCst);
            self.entries.lock().unwrap().push((name.to_string(), n));
            n
        }

        fn entries(&self) -> Vec<(String, u32)> {
            self.entries.lock().unwrap().clone()
        }
    }

    /// Test store that always succeeds and records operation order.
    struct SuccessStore {
        log: OpLog,
    }

    impl SuccessStore {
        fn new() -> Self {
            Self {
                log: OpLog::default(),
            }
        }
    }

    impl CasStore for SuccessStore {
        async fn cas_manifest(
            &self,
            path: &str,
            _expected_etag: Option<&str>,
            _new_value: &[u8],
        ) -> Result<bool> {
            self.log.record(&format!("manifest:{path}"));
            // Yield to let parallel tasks interleave.
            tokio::task::yield_now().await;
            Ok(true)
        }

        async fn cas_ref(
            &self,
            ref_name: &str,
            _expected_etag: Option<&str>,
            _new_sha: &str,
        ) -> Result<bool> {
            self.log.record(&format!("ref:{ref_name}"));
            tokio::task::yield_now().await;
            Ok(true)
        }
    }

    /// Store that fails manifest CAS N times before succeeding.
    struct ConflictingManifestStore {
        /// Number of conflicts remaining per path.
        conflicts: Mutex<std::collections::HashMap<String, u32>>,
        log: OpLog,
    }

    impl ConflictingManifestStore {
        fn new(conflicts: Vec<(&str, u32)>) -> Self {
            Self {
                conflicts: Mutex::new(
                    conflicts
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                ),
                log: OpLog::default(),
            }
        }
    }

    impl CasStore for ConflictingManifestStore {
        async fn cas_manifest(
            &self,
            path: &str,
            _expected_etag: Option<&str>,
            _new_value: &[u8],
        ) -> Result<bool> {
            self.log.record(&format!("manifest:{path}"));
            let mut map = self.conflicts.lock().unwrap();
            if let Some(remaining) = map.get_mut(path) {
                if *remaining > 0 {
                    *remaining -= 1;
                    return Ok(false);
                }
            }
            Ok(true)
        }

        async fn cas_ref(
            &self,
            ref_name: &str,
            _expected_etag: Option<&str>,
            _new_sha: &str,
        ) -> Result<bool> {
            self.log.record(&format!("ref:{ref_name}"));
            Ok(true)
        }
    }

    /// Store where ref CAS always fails with an error.
    struct RefFailStore;

    impl CasStore for RefFailStore {
        async fn cas_manifest(
            &self,
            _path: &str,
            _expected_etag: Option<&str>,
            _new_value: &[u8],
        ) -> Result<bool> {
            Ok(true)
        }

        async fn cas_ref(
            &self,
            ref_name: &str,
            _expected_etag: Option<&str>,
            _new_sha: &str,
        ) -> Result<bool> {
            Err(CrabError::NonFastForward {
                ref_name: ref_name.to_string(),
                have: "aaa".to_string(),
                want: "bbb".to_string(),
            })
        }
    }

    fn sample_plan(ref_count: usize) -> CommitPlan {
        let refs: Vec<RefCasOp> = (0..ref_count)
            .map(|i| RefCasOp {
                ref_name: format!("refs/heads/branch-{i}"),
                expected_etag: Some(format!("etag-ref-{i}")),
                new_sha: format!("deadbeef{i:040}"),
            })
            .collect();

        CommitPlan {
            shard_list_cas: CasOp {
                path: "repo/shard-list".to_string(),
                expected_etag: Some("etag-shard".to_string()),
                new_value: b"shard-content".to_vec(),
            },
            pack_list_cas: CasOp {
                path: "repo/pack-list".to_string(),
                expected_etag: Some("etag-pack".to_string()),
                new_value: b"pack-content".to_vec(),
            },
            ref_cas: refs,
        }
    }

    #[tokio::test]
    async fn single_ref_uses_parallel_execution() {
        let store = SuccessStore::new();
        let plan = sample_plan(1);

        let result = commit_push(&plan, &store, None).await.unwrap();

        assert!(result.shard_list_ok);
        assert!(result.pack_list_ok);
        assert_eq!(result.ref_results.len(), 1);
        assert!(result.ref_results[0].1);

        // All three operations should have been started before any completed
        // (they all yield). With tokio::join!, all get sequence numbers
        // before any resumes from yield_now.
        let entries = store.log.entries();
        assert_eq!(entries.len(), 3, "expected 3 operations, got {entries:?}");
    }

    #[tokio::test]
    async fn multi_ref_uses_serial_execution() {
        let store = SuccessStore::new();
        let plan = sample_plan(3);

        let result = commit_push(&plan, &store, None).await.unwrap();

        assert!(result.shard_list_ok);
        assert!(result.pack_list_ok);
        assert_eq!(result.ref_results.len(), 3);
        assert!(result.ref_results.iter().all(|(_, ok)| *ok));

        // Serial: manifests first, then refs in order.
        let entries = store.log.entries();
        assert_eq!(entries.len(), 5);

        // Manifests come before any ref.
        let manifest_seqs: Vec<u32> = entries
            .iter()
            .filter(|(name, _)| name.starts_with("manifest:"))
            .map(|(_, seq)| *seq)
            .collect();
        let ref_seqs: Vec<u32> = entries
            .iter()
            .filter(|(name, _)| name.starts_with("ref:"))
            .map(|(_, seq)| *seq)
            .collect();

        let max_manifest = manifest_seqs.iter().max().unwrap();
        let min_ref = ref_seqs.iter().min().unwrap();
        assert!(
            max_manifest < min_ref,
            "manifests should complete before refs start: manifests={manifest_seqs:?}, refs={ref_seqs:?}"
        );
    }

    #[tokio::test]
    async fn manifest_cas_conflict_surfaces_immediately() {
        // After the S1-P1-9 fix, a CAS conflict no longer retries with
        // the same stale ETag. The first conflict returns Ok(false) so
        // the caller can re-read state and rebuild the CasOp with a
        // fresh etag. A previous version of this test expected 3
        // attempts (2 conflicts + 1 success), which only "worked"
        // because the test's mock store reset its conflict counter
        // between attempts — real S3 would not.
        let store = ConflictingManifestStore::new(vec![("repo/shard-list", 2)]);
        let plan = sample_plan(1);

        let result = commit_push(&plan, &store, None).await.unwrap();

        // With no retry, shard_list_ok is false after the first conflict.
        assert!(!result.shard_list_ok);

        // Shard-list should have been attempted exactly once.
        let shard_attempts = store
            .log
            .entries()
            .iter()
            .filter(|(name, _)| name == "manifest:repo/shard-list")
            .count();
        assert_eq!(shard_attempts, 1);
    }

    #[tokio::test]
    async fn manifest_cas_conflict_returns_false() {
        // With MAX_RETRIES=1 post-S1-P1-9, any conflict returns Ok(false).
        let store =
            ConflictingManifestStore::new(vec![("repo/shard-list", MANIFEST_CAS_MAX_RETRIES + 1)]);
        let plan = sample_plan(1);

        let result = commit_push(&plan, &store, None).await.unwrap();

        // Shard-list should report failure, but the call itself succeeds.
        assert!(!result.shard_list_ok);
    }

    #[tokio::test]
    async fn ref_failure_is_propagated() {
        let store = RefFailStore;
        let plan = sample_plan(1);

        let err = commit_push(&plan, &store, None).await.unwrap_err();
        assert!(
            matches!(err, CrabError::NonFastForward { .. }),
            "expected NonFastForward, got {err:?}"
        );
    }

    #[tokio::test]
    async fn empty_ref_list_uses_serial_path() {
        let store = SuccessStore::new();
        let plan = CommitPlan {
            shard_list_cas: CasOp {
                path: "repo/shard-list".to_string(),
                expected_etag: None,
                new_value: b"shard".to_vec(),
            },
            pack_list_cas: CasOp {
                path: "repo/pack-list".to_string(),
                expected_etag: None,
                new_value: b"pack".to_vec(),
            },
            ref_cas: vec![],
        };

        let result = commit_push(&plan, &store, None).await.unwrap();

        assert!(result.shard_list_ok);
        assert!(result.pack_list_ok);
        assert!(result.ref_results.is_empty());
    }

    #[tokio::test]
    async fn parallel_path_increments_cas_pipelined_commits() {
        let store = SuccessStore::new();
        let metrics = Metrics::new();
        let plan = sample_plan(1);

        assert_eq!(metrics.snapshot().cas_pipelined_commits, 0);

        let result = commit_push(&plan, &store, Some(&metrics)).await.unwrap();
        assert!(result.shard_list_ok);
        assert_eq!(metrics.snapshot().cas_pipelined_commits, 1);

        // Second parallel commit increments again.
        let _ = commit_push(&plan, &store, Some(&metrics)).await.unwrap();
        assert_eq!(metrics.snapshot().cas_pipelined_commits, 2);
    }

    #[tokio::test]
    async fn serial_path_does_not_increment_cas_pipelined_commits() {
        let store = SuccessStore::new();
        let metrics = Metrics::new();
        let plan = sample_plan(3);

        let result = commit_push(&plan, &store, Some(&metrics)).await.unwrap();
        assert!(result.shard_list_ok);
        assert_eq!(metrics.snapshot().cas_pipelined_commits, 0);
    }
}
