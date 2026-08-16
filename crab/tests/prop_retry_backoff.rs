//! Property: the retry schedule stays inside the policy envelope —
//! at most `max_attempts` total attempts, each inter-attempt wait is
//! bounded by the exponential cap plus `Retry-After`, and a throttled
//! error's `Retry-After` hint acts as a lower bound on the next wait.

#![cfg(feature = "testing")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crab::core::error::{CrabError, Result};
use crab::storage::retry::{RetryPolicy, retry};
use proptest::prelude::*;

fn paused_block_on<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    rt.block_on(fut)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(15))]

    #[test]
    fn retry_schedule_respects_bounds_and_retry_after(
        max_attempts in 2u32..=8,
        base_ms in 1u64..=500,
        cap_ms in 500u64..=10_000,
        retry_after_ms in 0u64..=5_000,
    ) {
        let policy = RetryPolicy {
            max_attempts,
            base: Duration::from_millis(base_ms),
            cap: Duration::from_millis(cap_ms),
        };
        let retry_after = Duration::from_millis(retry_after_ms);

        // Track the virtual timestamp of every attempt so we can inspect
        // the gaps between successive calls after the retry loop exits.
        let timestamps: Arc<Mutex<Vec<tokio::time::Instant>>> = Arc::new(Mutex::new(Vec::new()));

        let timestamps_ref = Arc::clone(&timestamps);
        let result: Result<()> = paused_block_on(async move {
            retry(&policy, || {
                let timestamps_ref = Arc::clone(&timestamps_ref);
                async move {
                    timestamps_ref.lock().unwrap().push(tokio::time::Instant::now());
                    Err(CrabError::Throttled {
                        retry_after: Some(retry_after),
                    })
                }
            })
            .await
        });

        prop_assert!(
            matches!(result, Err(CrabError::Throttled { .. })),
            "retry should surface the last throttled error after exhaustion, got {:?}",
            result,
        );

        let ts = timestamps.lock().unwrap().clone();
        prop_assert_eq!(
            u32::try_from(ts.len()).unwrap_or(u32::MAX),
            max_attempts,
            "attempt count must equal max_attempts on exhaustion",
        );

        // Each gap between successive attempts honors Retry-After as a
        // lower bound and stays inside `retry_after + cap` (the jitter
        // added on top of Retry-After is uniform in `[0, cap]`).
        let jitter_ceiling = retry_after.saturating_add(policy.cap);
        for pair in ts.windows(2) {
            let gap = pair[1].duration_since(pair[0]);
            prop_assert!(
                gap >= retry_after,
                "gap {:?} must be at least Retry-After {:?}",
                gap,
                retry_after,
            );
            prop_assert!(
                gap <= jitter_ceiling,
                "gap {:?} must not exceed Retry-After + cap {:?}",
                gap,
                jitter_ceiling,
            );
        }
    }
}
