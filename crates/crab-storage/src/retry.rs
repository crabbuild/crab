//! Retry policy for storage-domain operations.

use std::future::Future;
use std::io::ErrorKind;
use std::time::Duration;

use rand::Rng;
use tokio::time::sleep;

use crate::error::{Result, StorageError};

/// How aggressively to retry a fallible storage operation.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Maximum number of attempts including the first try.
    pub max_attempts: u32,
    /// Base delay used to compute exponential backoff.
    pub base: Duration,
    /// Upper bound on any single backoff delay.
    pub cap: Duration,
}

impl RetryPolicy {
    /// Default policy for transient storage errors.
    pub const DEFAULT: Self = Self {
        max_attempts: 5,
        base: Duration::from_millis(100),
        cap: Duration::from_secs(10),
    };

    /// Policy for state-dependent storage conflicts.
    pub const STATE_DEPENDENT: Self = Self {
        max_attempts: 10,
        base: Duration::from_millis(100),
        cap: Duration::from_secs(10),
    };
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How the retry loop should handle a storage error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// Retry with exponential backoff and jitter.
    Transient,
    /// Honor a retry hint as a lower bound before jitter.
    Throttled { retry_after: Option<Duration> },
    /// Re-read state and retry with the state-dependent attempt budget.
    StateDependent,
    /// Inspect the local errno and retry only interrupt-style failures.
    InspectErrno,
    /// Retry once to cover a transient corrupted read, then surface.
    FatalAfterOneRetry,
    /// No retry.
    Fatal,
}

/// Classifies a storage-domain error for retry.
#[must_use]
pub fn retry_class(err: &StorageError) -> RetryClass {
    match err {
        StorageError::NetworkTransient { .. } => RetryClass::Transient,
        StorageError::Throttled { retry_after } => RetryClass::Throttled {
            retry_after: *retry_after,
        },
        StorageError::StateConflict { .. } => RetryClass::StateDependent,
        StorageError::Io { source } => {
            if matches!(
                source.kind(),
                ErrorKind::Interrupted | ErrorKind::WouldBlock
            ) {
                RetryClass::InspectErrno
            } else {
                RetryClass::Fatal
            }
        }
        StorageError::CorruptObject { .. } => RetryClass::FatalAfterOneRetry,
        StorageError::ObjectStore { source } => classify_object_store(source),
        StorageError::NotFound { .. }
        | StorageError::InvalidHash { .. }
        | StorageError::NotSupported { .. }
        | StorageError::UnsupportedProvider { .. }
        | StorageError::InvalidStaticEnvTarget { .. }
        | StorageError::StaticEnvProviderMismatch { .. }
        | StorageError::ProviderConfig { .. }
        | StorageError::InvalidObjectStoreUrl { .. }
        | StorageError::UrlStoreConfig { .. }
        | StorageError::AuthFailed { .. }
        | StorageError::AuthExpired { .. }
        | StorageError::NoCredentials
        | StorageError::Forbidden { .. }
        | StorageError::Cancelled
        | StorageError::Internal(_) => RetryClass::Fatal,
    }
}

fn classify_object_store(err: &object_store::Error) -> RetryClass {
    match err {
        object_store::Error::Generic { .. } => RetryClass::Transient,
        object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. } => {
            RetryClass::StateDependent
        }
        _ => RetryClass::Fatal,
    }
}

/// Runs `op` with retries according to `policy`.
pub async fn retry<F, Fut, T>(policy: &RetryPolicy, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt: u32 = 0;
    loop {
        let err = match op().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };

        match retry_class(&err) {
            RetryClass::Fatal => return Err(err),
            RetryClass::InspectErrno | RetryClass::Transient => {
                if attempt + 1 >= policy.max_attempts {
                    return Err(err);
                }
                sleep(backoff_delay(policy, attempt)).await;
                attempt += 1;
            }
            RetryClass::FatalAfterOneRetry => {
                if attempt >= 1 {
                    return Err(err);
                }
                sleep(small_jitter(policy.base)).await;
                attempt += 1;
            }
            RetryClass::Throttled { retry_after } => {
                if attempt + 1 >= policy.max_attempts {
                    return Err(err);
                }
                let jitter = backoff_delay(policy, attempt);
                let delay = match retry_after {
                    Some(retry_after) => retry_after.saturating_add(jitter),
                    None => jitter,
                };
                sleep(delay).await;
                attempt += 1;
            }
            RetryClass::StateDependent => {
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

fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let shift = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let exp = policy.base.saturating_mul(shift);
    let bound = exp.min(policy.cap);
    let bound_nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
    if bound_nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(rand::rng().random_range(0..=bound_nanos))
}

fn small_jitter(base: Duration) -> Duration {
    let base_nanos = u64::try_from(base.as_nanos()).unwrap_or(u64::MAX);
    if base_nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(rand::rng().random_range(0..=base_nanos))
}

#[cfg(test)]
#[expect(clippy::panic, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn boxed(msg: &'static str) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        Box::<dyn std::error::Error + Send + Sync>::from(msg)
    }

    fn transient_err() -> StorageError {
        StorageError::NetworkTransient {
            source: object_store::Error::Generic {
                store: "S3",
                source: boxed("connection reset"),
            },
        }
    }

    #[test]
    fn classifies_network_transient_as_transient() {
        assert_eq!(retry_class(&transient_err()), RetryClass::Transient);
    }

    #[test]
    fn classifies_throttled_and_carries_retry_after() {
        let retry_after = Some(Duration::from_millis(250));
        let err = StorageError::Throttled { retry_after };

        assert_eq!(retry_class(&err), RetryClass::Throttled { retry_after });
    }

    #[test]
    fn classifies_state_conflict_as_state_dependent() {
        let err = StorageError::StateConflict {
            path: "repo/refs/heads/main".to_owned(),
        };

        assert_eq!(retry_class(&err), RetryClass::StateDependent);
    }

    #[test]
    fn classifies_retryable_io_as_inspect_errno() {
        let err = StorageError::Io {
            source: std::io::Error::from(ErrorKind::Interrupted),
        };

        assert_eq!(retry_class(&err), RetryClass::InspectErrno);
    }

    #[test]
    fn classifies_non_retryable_io_as_fatal() {
        let err = StorageError::Io {
            source: std::io::Error::from(ErrorKind::PermissionDenied),
        };

        assert_eq!(retry_class(&err), RetryClass::Fatal);
    }

    #[test]
    fn classifies_raw_object_store_generic_as_transient() {
        let err = StorageError::ObjectStore {
            source: object_store::Error::Generic {
                store: "S3",
                source: boxed("5xx"),
            },
        };

        assert_eq!(retry_class(&err), RetryClass::Transient);
    }

    #[test]
    fn classifies_raw_object_store_precondition_as_state_dependent() {
        let err = StorageError::ObjectStore {
            source: object_store::Error::Precondition {
                path: "repo/refs/heads/main".into(),
                source: boxed("if-match failed"),
            },
        };

        assert_eq!(retry_class(&err), RetryClass::StateDependent);
    }

    #[test]
    fn classifies_auth_errors_as_fatal() {
        let err = StorageError::AuthFailed {
            path: "repo/packs/abc".to_owned(),
        };

        assert_eq!(retry_class(&err), RetryClass::Fatal);
    }

    #[test]
    fn classifies_corrupt_object_as_fatal_after_one_retry() {
        let err = StorageError::CorruptObject {
            path: ".crab/xorbs/abc".to_owned(),
            reason: "hash mismatch".to_owned(),
        };

        assert_eq!(retry_class(&err), RetryClass::FatalAfterOneRetry);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_succeeds_after_transient_errors() {
        let policy = RetryPolicy::DEFAULT;
        let calls = Arc::new(AtomicU32::new(0));
        let calls_ref = Arc::clone(&calls);

        let result: Result<u32> = retry(&policy, move || {
            let calls = Arc::clone(&calls_ref);
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
        let calls_ref = Arc::clone(&calls);

        let result: Result<()> = retry(&policy, move || {
            let calls = Arc::clone(&calls_ref);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(StorageError::Forbidden {
                    path: "repo/private".to_owned(),
                })
            }
        })
        .await;

        assert!(matches!(result, Err(StorageError::Forbidden { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_gives_corruption_one_retry() {
        let policy = RetryPolicy::DEFAULT;
        let calls = Arc::new(AtomicU32::new(0));
        let calls_ref = Arc::clone(&calls);

        let result: Result<()> = retry(&policy, move || {
            let calls = Arc::clone(&calls_ref);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(StorageError::CorruptObject {
                    path: ".crab/xorbs/abc".to_owned(),
                    reason: "hash mismatch".to_owned(),
                })
            }
        })
        .await;

        assert!(matches!(result, Err(StorageError::CorruptObject { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
        let calls_ref = Arc::clone(&calls);

        let start = tokio::time::Instant::now();
        let result: Result<u32> = retry(&policy, move || {
            let calls = Arc::clone(&calls_ref);
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(StorageError::Throttled {
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
        let calls_ref = Arc::clone(&calls);

        let result: Result<()> = retry(&policy, move || {
            let calls = Arc::clone(&calls_ref);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(transient_err())
            }
        })
        .await;

        assert!(matches!(result, Err(StorageError::NetworkTransient { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), policy.max_attempts);
    }

    #[test]
    fn backoff_delay_respects_cap() {
        let policy = RetryPolicy {
            max_attempts: 20,
            base: Duration::from_millis(100),
            cap: Duration::from_secs(10),
        };

        for attempt in 0..20 {
            let delay = backoff_delay(&policy, attempt);
            assert!(
                delay <= policy.cap,
                "attempt {attempt}: {delay:?} exceeds cap {:?}",
                policy.cap
            );
        }
    }
}
