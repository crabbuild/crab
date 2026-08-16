//! Retry policy for object-store operations.
//!
//! Classifies errors via [`retry_class`] and retries according to a
//! policy. The backoff shape is exponential with **full jitter** — on
//! attempt `n` we sleep a random value in `[0, min(cap, base * 2^n)]`.
//! Exponential avoids hammering a flapping endpoint; full jitter
//! prevents a thundering herd of clients waking up in lockstep.
//!
//! `Retry-After` hints (when present on a [`CrabError::Throttled`])
//! act as a lower bound on the next sleep: we always wait at least as
//! long as the server asks, then add jitter on top.

use std::future::Future;
use std::io::ErrorKind;
use std::time::Duration;

use rand::Rng;
use tokio::time::sleep;

use crate::core::error::CrabError;
use crate::core::error::Result;

pub use crab_storage::RetryPolicy;

/// How the retry loop should handle a given error.
///
/// Produced by [`retry_class`]. The [`retry`] helper turns each class
/// into a concrete waiting and attempt-counting strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// Retry with exponential backoff + full jitter, up to `max_attempts`.
    Transient,
    /// Honor the server's `Retry-After` as a lower bound, then behave
    /// like [`Transient`](Self::Transient).
    Throttled { retry_after: Option<Duration> },
    /// Re-read state, then retry (up to `STATE_DEPENDENT` attempts).
    StateDependent,
    /// One retry to cover a transient bit-flip, then surface the error.
    FatalAfterOneRetry,
    /// Peek at `io::ErrorKind`: retry only on `Interrupted` / `WouldBlock`.
    InspectErrno,
    /// No retry; surface to the caller immediately.
    Fatal,
}

/// Classifies a [`CrabError`] into a [`RetryClass`].
///
/// The mapping matches the Retry Classification table in the design
/// document; adding a new error variant requires extending this
/// function (and a test in this module).
#[must_use]
#[expect(
    clippy::match_same_arms,
    reason = "fatal variants are grouped by domain so each storage-retry decision keeps its rationale"
)]
pub fn retry_class(err: &CrabError) -> RetryClass {
    match err {
        CrabError::NetworkTransient(_) => RetryClass::Transient,
        CrabError::Throttled { retry_after } => RetryClass::Throttled {
            retry_after: *retry_after,
        },
        CrabError::CasConflict { .. } => RetryClass::StateDependent,
        CrabError::CorruptObject { .. } | CrabError::PackIntegrity { .. } => {
            RetryClass::FatalAfterOneRetry
        }
        CrabError::Io(_) => RetryClass::InspectErrno,
        CrabError::Storage(e) => classify_storage(e),
        // Partial push outcomes: the retry decision hinges on why the
        // push partially failed, not on the partial-outcome wrapper
        // itself. Recurse into the inner source so a throttled upload
        // deep inside the push pipeline still gets the backoff it
        // needs, while a hard auth failure surfaces fatally.
        CrabError::PushPartialOutcome { source, .. } => retry_class(source),
        CrabError::ManagedRepository { diagnostic } => {
            if matches!(
                diagnostic,
                crab_auth_store::ManagedRepositoryDiagnostic::ServiceUnavailable { .. }
            ) {
                RetryClass::Transient
            } else {
                RetryClass::Fatal
            }
        }
        // Everything below is fatal: retrying won't change the outcome,
        // and the user wants the error now rather than after a minute
        // of pointless waiting.
        CrabError::PushIntegrationFailed { .. }
        | CrabError::NonFastForward { .. }
        | CrabError::RefAlreadyExists { .. }
        | CrabError::PushLockHeld { .. }
        | CrabError::FileChangedDuringStaging { .. }
        | CrabError::ChunkNotFound { .. }
        | CrabError::NotFound { .. }
        | CrabError::Forbidden { .. }
        | CrabError::NoCredentials
        | CrabError::AuthFailed { .. }
        | CrabError::AuthExpired { .. }
        | CrabError::InsufficientSpace { .. }
        | CrabError::Configuration { .. }
        | CrabError::IncompatibleFormat { .. }
        | CrabError::InvalidPattern(_)
        | CrabError::Protocol(_)
        | CrabError::Internal(_)
        | CrabError::StagingCorrupt(_)
        | CrabError::StagingLocked { .. }
        | CrabError::HashMismatch { .. }
        | CrabError::CrcMismatch { .. }
        | CrabError::Cancelled
        | CrabError::BeyondShallowBoundary { .. }
        | CrabError::PackTooLarge { .. }
        | CrabError::FetchNotAllowed { .. }
        | CrabError::FetchTooLarge { .. }
        | CrabError::InvalidLfsPointer { .. }
        | CrabError::LfsObjectCorrupt { .. }
        | CrabError::LfsObjectMissing { .. }
        | CrabError::LfsLockConflict { .. }
        | CrabError::LfsTransferProtocol(_)
        | CrabError::LfsMigrationFailed { .. }
        | CrabError::LfsUnsupported { .. } => RetryClass::Fatal,
        // Cache service errors are non-retryable at the store level;
        // the CachingStore handles fallback to origin.
        CrabError::CacheService { .. } => RetryClass::Fatal,
        // Incomplete shard reconstruction signals a bug in the push
        // pipeline — the placement map doesn't cover every chunk.
        // Retrying the same push will hit the same gap.
        CrabError::IncompleteShardReconstruction { .. } => RetryClass::Fatal,
        // Missing staging data is deterministic — the fix is `crab add`
        // on the affected paths, not a retry.
        CrabError::PointerMissingStaging { .. } => RetryClass::Fatal,
        // Missing objects are a hard failure — retrying the push
        // against the same local ODB won't materialize the object.
        // The user must re-fetch the referenced SHAs or regenerate
        // the pack. Never retry.
        CrabError::PushConnectivityMissing { .. } => RetryClass::Fatal,

        // Malformed objects surface a canonical-encoding violation
        // that will not spontaneously fix itself on retry — the
        // pusher has to rewrite the offending history locally.
        CrabError::PushMalformedObject { .. } => RetryClass::Fatal,
        // A fetched batch whose advertised tip does not resolve is a
        // remote-state integrity problem. Retrying the same committed
        // manifest would re-fetch the same pack inventory. Never retry.
        CrabError::FetchMalformedObject { .. } => RetryClass::Fatal,
        // Import URL validation errors are pure arg-parsing issues —
        // they surface before any storage call, so the retry layer
        // should never see one. Plan-mismatch is deterministic too:
        // the recorded checksum won't change on retry, the user needs
        // to resolve the drift. Classify all as fatal.
        CrabError::ImportSourceMustBeRaw { .. }
        | CrabError::ImportSchemeMismatch { .. }
        | CrabError::ImportPlanMismatch { .. }
        | CrabError::ImportNoJournal { .. }
        | CrabError::ImportVersioningUnavailable { .. }
        | CrabError::ImportCommitCeilingExceeded { .. }
        | CrabError::ImportInvalidHistoryRange { .. }
        | CrabError::ImportTargetNotEmpty { .. }
        | CrabError::ImportSourceIsCrabRepo { .. }
        | CrabError::ImportLfsSourceUnsupported { .. }
        | CrabError::ImportLfsStoreNotFound { .. }
        | CrabError::ImportPrefixCollision { .. }
        | CrabError::ImportMissingGitIdentity
        | CrabError::ImportRemoteExists { .. } => RetryClass::Fatal,
        // Workflow-layer errors never flow through the storage retry
        // path — the workflow orchestrator owns stage-level retry
        // semantics (per-stage budget, exponential backoff, side-effect
        // caps). If one leaks here, fail fast rather than double-retry.
        CrabError::WorkflowParse { .. }
        | CrabError::WorkflowCycle { .. }
        | CrabError::WorkflowUndefinedOut { .. }
        | CrabError::WorkflowStageNameInvalid { .. }
        | CrabError::WorkflowDiscoveryAmbiguous { .. }
        | CrabError::StageDepMissing { .. }
        | CrabError::StageDepMalformed { .. }
        | CrabError::StageOutMalformed { .. }
        | CrabError::StageOutTooLarge { .. }
        | CrabError::StageOutCountExceeded { .. }
        | CrabError::StageEnvMissing { .. }
        | CrabError::StageExecFailed { .. }
        | CrabError::StageExecSignaled { .. }
        | CrabError::StageExecTimeout { .. }
        | CrabError::StageDiskFull { .. }
        | CrabError::StageCacheMiss { .. }
        | CrabError::StageRetryExhausted { .. }
        | CrabError::StageOverwriteConflict { .. }
        | CrabError::StageSideEffectsRetryLimit { .. }
        | CrabError::StageSideEffectHookFailed { .. }
        | CrabError::LockfileStale { .. }
        | CrabError::LockfileCanonicalizationFailed { .. }
        | CrabError::LockfileMergeConflict { .. }
        | CrabError::ExperimentNotFound { .. }
        | CrabError::ExperimentCollision { .. }
        | CrabError::MetricsSchemaMismatch { .. }
        | CrabError::WorkflowJournalOpen { .. }
        | CrabError::WorkflowJournalCorrupt { .. }
        | CrabError::WorkflowJournalSchemaNewer { .. }
        | CrabError::WorkflowResumeFilesystemDrift { .. }
        | CrabError::WorkflowStateTransitionIllegal { .. }
        | CrabError::WorkflowLockTimeout { .. }
        | CrabError::WorkflowDisabled
        | CrabError::WorkflowHermeticViolation { .. }
        | CrabError::CacheEntrySchemaNewer { .. }
        | CrabError::StageRemoteExecutionUnsupported
        | CrabError::StageHermeticNotImplemented { .. }
        | CrabError::WorkflowDuplicateOutput { .. }
        | CrabError::WorkflowValidationError { .. }
        | CrabError::WorkflowSelfLoop { .. }
        | CrabError::JournalDiskFull { .. }
        | CrabError::WorkflowExperimentIdInvalid { .. }
        | CrabError::WorkflowExperimentMetadataSchemaNewer { .. } => RetryClass::Fatal,
        // Storage economy errors: tier, restripe, cost. These are
        // user-facing operational errors — retrying won't help.
        CrabError::TierLifecycleConflict { .. }
        | CrabError::TierApplyUnauthorized { .. }
        | CrabError::TierProviderUnsupported { .. }
        | CrabError::ArchiveRestoreRequired { .. }
        | CrabError::ArchiveRestoreTimeout { .. }
        | CrabError::RestoreTierUnsupported { .. }
        | CrabError::GcEarlyDeleteBlocked { .. }
        | CrabError::ObjectLockedRetention { .. }
        | CrabError::RestripeProfileOutOfRange { .. }
        | CrabError::RestripeCorruptSource { .. }
        | CrabError::RestripeAlreadyInProgress { .. }
        | CrabError::ConcurrentMaintenance { .. }
        | CrabError::CostPricingMissing { .. }
        | CrabError::CostInventoryReportStale { .. }
        | CrabError::ManifestParse { .. }
        | CrabError::PrefetchParse { .. }
        | CrabError::PrefetchProfileNotFound { .. }
        | CrabError::SpeculationDb { .. } => RetryClass::Fatal,

        // Gitoxide wrappers: the retry layer can't meaningfully
        // branch on them. The specific call-site that invoked the
        // gix-* API owns any retry decision there. From the retry
        // layer's perspective, treat them as fatal so retries don't
        // double-drive on top of whatever the call-site already did.
        CrabError::GixRef(_)
        | CrabError::GixObject(_)
        | CrabError::GixPack(_)
        | CrabError::GixTransport(_)
        | CrabError::GixProtocol(_)
        | CrabError::GixFilterHandshake(_)
        | CrabError::GixFilterRequest(_)
        | CrabError::GixWorktree(_)
        | CrabError::GixConfig(_)
        | CrabError::GixCreds(_)
        | CrabError::GixStatus(_)
        | CrabError::GixRevwalk(_)
        | CrabError::GitTag(_) => RetryClass::Fatal,

        // MetaDB errors: the metadb call sites own their own single-retry
        // policy for transient S3 failures (documented in the design). By
        // the time an error reaches the generic retry layer, retrying is
        // not going to help.
        CrabError::MetaDb(_) => RetryClass::Fatal,

        // Remote cache integrity errors are permanent — retrying won't
        // fix corrupted or mismatched data.
        CrabError::CacheEntryCorrupt { .. }
        | CrabError::CacheEntryHashMismatch { .. }
        | CrabError::RemoteCacheReadonly => RetryClass::Fatal,

        // Template resolution errors are config-shape problems — not
        // retry-worthy.
        CrabError::WorkflowTemplateUndefined { .. } => RetryClass::Fatal,
        CrabError::WorkflowForeachEmpty { .. } => RetryClass::Fatal,
        CrabError::WorkflowMatrixEmpty { .. } => RetryClass::Fatal,

        CrabError::InvalidConfigKey { .. }
        | CrabError::UnsupportedShell { .. }
        | CrabError::PullConflict { .. }
        | CrabError::PullRemoteUnreachable { .. }
        | CrabError::UnadoptChunksMissing { .. }
        | CrabError::NothingToUndo => RetryClass::Fatal,
    }
}

// `Storage` wraps a raw `object_store::Error` that bypassed the richer
// `error_map` helper (e.g., because the call site used the blanket
// `#[from]`). Apply the same transient-vs-permanent split here so those
// paths still retry correctly.
fn classify_storage(err: &object_store::Error) -> RetryClass {
    match err {
        object_store::Error::Generic { .. } => RetryClass::Transient,
        object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. } => {
            RetryClass::StateDependent
        }
        // `NotFound`/`PermissionDenied`/`Unauthenticated` are fatal, as is
        // every future variant behind `#[non_exhaustive]`. Collapsed into
        // the wildcard because retrying won't change any of them.
        _ => RetryClass::Fatal,
    }
}

/// Runs `op` with retries according to `policy`.
///
/// Each call to `op` is a fresh attempt; the closure is `FnMut` so
/// callers can rebuild request state (e.g., re-read a conflicting
/// ref) between retries. On success, returns the value. On error,
/// consults [`retry_class`] to decide whether and how to retry.
///
/// The last observed error is returned when retries are exhausted.
///
/// # Errors
///
/// Returns any [`CrabError`] classified as [`RetryClass::Fatal`]
/// immediately, or the final error after exhausting `policy.max_attempts`
/// for a retryable class.
pub async fn retry<F, Fut, T>(policy: &RetryPolicy, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt: u32 = 0;
    loop {
        let err = match op().await {
            Ok(value) => return Ok(value),
            Err(e) => e,
        };

        match retry_class(&err) {
            RetryClass::Fatal => return Err(err),

            RetryClass::FatalAfterOneRetry => {
                if attempt >= 1 {
                    return Err(err);
                }
                // One immediate retry — a transient bit flip on the
                // network should resolve on the next read.
                attempt += 1;
            }

            RetryClass::InspectErrno => {
                // Only `Interrupted` / `WouldBlock` are worth retrying;
                // everything else is a real I/O failure.
                let retryable = matches!(
                    &err,
                    CrabError::Io(e)
                        if e.kind() == ErrorKind::Interrupted
                            || e.kind() == ErrorKind::WouldBlock
                );
                if !retryable {
                    return Err(err);
                }
                if attempt + 1 >= policy.max_attempts {
                    return Err(err);
                }
                sleep(backoff_delay(policy, attempt)).await;
                attempt += 1;
            }

            RetryClass::Transient => {
                if attempt + 1 >= policy.max_attempts {
                    return Err(err);
                }
                sleep(backoff_delay(policy, attempt)).await;
                attempt += 1;
            }

            RetryClass::Throttled { retry_after } => {
                if attempt + 1 >= policy.max_attempts {
                    return Err(err);
                }
                // Server said "wait at least this long"; honor it as a
                // lower bound and add jitter on top.
                let jitter = backoff_delay(policy, attempt);
                let delay = match retry_after {
                    Some(ra) => ra.saturating_add(jitter),
                    None => jitter,
                };
                sleep(delay).await;
                attempt += 1;
            }

            RetryClass::StateDependent => {
                // CAS conflicts need more attempts (races with concurrent
                // writers), but skip the exponential wait — the useful
                // work is re-reading state, not waiting. A small jittered
                // sleep still breaks thundering-herd patterns.
                let budget = RetryPolicy::STATE_DEPENDENT.max_attempts;
                if attempt + 1 >= budget {
                    return Err(err);
                }
                sleep(small_jitter(policy.base)).await;
                attempt += 1;
            }
        }
    }
}

/// Full-jitter exponential backoff: uniform in `[0, min(cap, base * 2^n)]`.
fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let shift = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let exp = policy.base.saturating_mul(shift);
    let bound = exp.min(policy.cap);
    let bound_nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
    if bound_nanos == 0 {
        return Duration::ZERO;
    }
    let pick = rand::rng().random_range(0..=bound_nanos);
    Duration::from_nanos(pick)
}

/// Small uniform jitter in `[0, base]`; used for `StateDependent` so
/// concurrent retriers don't synchronize on the same re-read tick.
fn small_jitter(base: Duration) -> Duration {
    let base_nanos = u64::try_from(base.as_nanos()).unwrap_or(u64::MAX);
    if base_nanos == 0 {
        return Duration::ZERO;
    }
    let pick = rand::rng().random_range(0..=base_nanos);
    Duration::from_nanos(pick)
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn boxed(msg: &'static str) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        Box::<dyn std::error::Error + Send + Sync>::from(msg)
    }

    fn transient_err() -> CrabError {
        CrabError::NetworkTransient(object_store::Error::Generic {
            store: "S3",
            source: boxed("connection reset"),
        })
    }

    #[test]
    fn classifies_network_transient_as_transient() {
        assert_eq!(retry_class(&transient_err()), RetryClass::Transient);
    }

    #[test]
    fn classifies_non_fast_forward_as_fatal() {
        let err = CrabError::NonFastForward {
            ref_name: "refs/heads/main".into(),
            have: "abc".into(),
            want: "def".into(),
        };
        assert_eq!(retry_class(&err), RetryClass::Fatal);
    }

    #[test]
    fn classifies_file_changed_during_staging_as_fatal() {
        let err = CrabError::FileChangedDuringStaging {
            path: "model.bin".into(),
            first_hash: "aaa".into(),
            second_hash: "bbb".into(),
            first_size: 1,
            second_size: 2,
        };
        assert_eq!(retry_class(&err), RetryClass::Fatal);
    }

    #[test]
    fn classifies_push_integration_failed_as_fatal() {
        let err = CrabError::PushIntegrationFailed {
            command: "git pull --rebase --autostash origin main".into(),
            message: "CONFLICT".into(),
        };
        assert_eq!(retry_class(&err), RetryClass::Fatal);
    }

    #[test]
    fn classifies_cas_conflict_as_state_dependent() {
        let err = CrabError::CasConflict {
            path: "repo/refs/heads/main".into(),
            expected_etag: None,
        };
        assert_eq!(retry_class(&err), RetryClass::StateDependent);
    }

    #[test]
    fn classifies_corrupt_object_as_fatal_after_one_retry() {
        let err = CrabError::CorruptObject {
            path: "repo/objects/x".into(),
            reason: "hash mismatch".into(),
        };
        assert_eq!(retry_class(&err), RetryClass::FatalAfterOneRetry);
    }

    #[test]
    fn classifies_throttled_and_carries_retry_after() {
        let err = CrabError::Throttled {
            retry_after: Some(Duration::from_millis(250)),
        };
        assert_eq!(
            retry_class(&err),
            RetryClass::Throttled {
                retry_after: Some(Duration::from_millis(250))
            }
        );
    }

    #[test]
    fn classifies_io_as_inspect_errno() {
        let err = CrabError::Io(std::io::Error::from(ErrorKind::Interrupted));
        assert_eq!(retry_class(&err), RetryClass::InspectErrno);
    }

    #[test]
    fn classifies_chunk_not_found_as_fatal() {
        let err = CrabError::ChunkNotFound {
            hash: "deadbeef".into(),
        };
        assert_eq!(retry_class(&err), RetryClass::Fatal);
    }

    #[test]
    fn classifies_storage_generic_as_transient() {
        let err = CrabError::Storage(object_store::Error::Generic {
            store: "S3",
            source: boxed("5xx"),
        });
        assert_eq!(retry_class(&err), RetryClass::Transient);
    }

    #[test]
    fn classifies_storage_precondition_as_state_dependent() {
        let err = CrabError::Storage(object_store::Error::Precondition {
            path: "p".into(),
            source: boxed("if-match failed"),
        });
        assert_eq!(retry_class(&err), RetryClass::StateDependent);
    }

    #[test]
    fn classifies_auth_failed_as_fatal() {
        let err = CrabError::AuthFailed {
            path: "repo/packs/abc".into(),
        };
        assert_eq!(retry_class(&err), RetryClass::Fatal);
    }

    #[test]
    fn classifies_auth_expired_as_fatal() {
        let err = CrabError::AuthExpired {
            path: "repo/packs/abc".into(),
        };
        assert_eq!(retry_class(&err), RetryClass::Fatal);
    }

    #[test]
    fn classifies_managed_service_outage_as_transient() {
        let err = CrabError::ManagedRepository {
            diagnostic: crab_auth_store::ManagedRepositoryDiagnostic::ServiceUnavailable {
                authority: "crab.build".to_owned(),
            },
        };

        assert_eq!(retry_class(&err), RetryClass::Transient);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_succeeds_after_transient_errors() {
        let policy = RetryPolicy::DEFAULT;
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::clone(&calls);

        let result: Result<u32> = retry(&policy, move || {
            let calls = Arc::clone(&calls2);
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 { Err(transient_err()) } else { Ok(42) }
            }
        })
        .await;

        assert_eq!(result.ok(), Some(42));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_surfaces_fatal_immediately_without_retries() {
        let policy = RetryPolicy::DEFAULT;
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::clone(&calls);

        let result: Result<()> = retry(&policy, move || {
            let calls = Arc::clone(&calls2);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(CrabError::NonFastForward {
                    ref_name: "refs/heads/main".into(),
                    have: "a".into(),
                    want: "b".into(),
                })
            }
        })
        .await;

        assert!(matches!(result, Err(CrabError::NonFastForward { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_honors_retry_after_as_lower_bound() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base: Duration::from_millis(100),
            cap: Duration::from_secs(10),
        };
        let retry_after = Duration::from_secs(2);
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::clone(&calls);

        let start = tokio::time::Instant::now();
        let result: Result<u32> = retry(&policy, move || {
            let calls = Arc::clone(&calls2);
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(CrabError::Throttled {
                        retry_after: Some(retry_after),
                    })
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        let elapsed = start.elapsed();

        assert_eq!(result.ok(), Some(7));
        assert!(
            elapsed >= retry_after,
            "expected at least {retry_after:?} elapsed, got {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_exhausts_max_attempts_and_returns_last_error() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base: Duration::from_millis(10),
            cap: Duration::from_millis(50),
        };
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::clone(&calls);

        let result: Result<()> = retry(&policy, move || {
            let calls = Arc::clone(&calls2);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(transient_err())
            }
        })
        .await;

        assert!(matches!(result, Err(CrabError::NetworkTransient(_))));
        assert_eq!(calls.load(Ordering::SeqCst), policy.max_attempts);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_fatal_after_one_retry_tries_twice_then_fails() {
        let policy = RetryPolicy::DEFAULT;
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::clone(&calls);

        let result: Result<()> = retry(&policy, move || {
            let calls = Arc::clone(&calls2);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(CrabError::CorruptObject {
                    path: "repo/objects/x".into(),
                    reason: "hash mismatch".into(),
                })
            }
        })
        .await;

        assert!(matches!(result, Err(CrabError::CorruptObject { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_io_non_retryable_errno_surfaces_immediately() {
        let policy = RetryPolicy::DEFAULT;
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::clone(&calls);

        let result: Result<()> = retry(&policy, move || {
            let calls = Arc::clone(&calls2);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(CrabError::Io(std::io::Error::from(
                    ErrorKind::PermissionDenied,
                )))
            }
        })
        .await;

        assert!(matches!(result, Err(CrabError::Io(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backoff_delay_respects_cap() {
        let policy = RetryPolicy {
            max_attempts: 20,
            base: Duration::from_millis(100),
            cap: Duration::from_secs(10),
        };
        for attempt in 0..20 {
            let d = backoff_delay(&policy, attempt);
            assert!(
                d <= policy.cap,
                "attempt {attempt}: {d:?} exceeds cap {:?}",
                policy.cap
            );
        }
    }
}
