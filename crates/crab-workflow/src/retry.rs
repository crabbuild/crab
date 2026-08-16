//! Retry decisioning for failed stage attempts.
//!
//! Classifies each failure against a stage retry policy and decides whether
//! the executor should retry or surface the original error.

use std::time::Duration;

use crate::RetryPolicy;

/// How a stage attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The user command exited non-zero.
    ExitCode(i32),
    /// The child was killed by a signal.
    Signal(i32),
    /// The stage exceeded its declared timeout.
    Timeout,
}

/// Decision returned by the retry planner for a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Retry after the computed backoff.
    Retry {
        /// Delay before the next attempt.
        backoff: Duration,
    },
    /// No more attempts.
    Exhausted,
}

/// Decides whether to retry a failed attempt.
///
/// `attempt` is one-indexed: the first attempt is `attempt == 1`.
pub fn should_retry(policy: &RetryPolicy, kind: &FailureKind, attempt: u32) -> RetryDecision {
    if attempt >= policy.max_attempts {
        return RetryDecision::Exhausted;
    }

    let qualifies = match kind {
        FailureKind::ExitCode(code) => policy.on_exit_codes.contains(code),
        FailureKind::Signal(signal) => policy.on_signals.contains(signal),
        FailureKind::Timeout => policy.on_timeout,
    };
    if !qualifies {
        return RetryDecision::Exhausted;
    }

    RetryDecision::Retry {
        backoff: compute_backoff(policy, attempt),
    }
}

fn compute_backoff(policy: &RetryPolicy, attempt: u32) -> Duration {
    if policy.initial_backoff.is_zero() {
        return Duration::ZERO;
    }

    let exponent = attempt.saturating_sub(1);
    let base_ms = policy.initial_backoff.as_millis() as f64;
    let factor = policy.backoff_multiplier.max(1.0).powi(exponent as i32);
    let computed_ms = base_ms * factor;
    let computed = if computed_ms.is_finite() && computed_ms >= 0.0 {
        Duration::from_millis(computed_ms.min(u64::MAX as f64) as u64)
    } else {
        policy.max_backoff
    };
    computed.min(policy.max_backoff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(
        max_attempts: u32,
        on_exit_codes: Vec<i32>,
        on_signals: Vec<i32>,
        on_timeout: bool,
    ) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(60),
            backoff_multiplier: 2.0,
            on_exit_codes,
            on_signals,
            on_timeout,
        }
    }

    #[test]
    fn retries_matching_exit_code_until_budget_exhausted() {
        let p = policy(3, vec![1], vec![], false);

        assert!(matches!(
            should_retry(&p, &FailureKind::ExitCode(1), 1),
            RetryDecision::Retry { .. }
        ));
        assert!(matches!(
            should_retry(&p, &FailureKind::ExitCode(1), 2),
            RetryDecision::Retry { .. }
        ));
        assert_eq!(
            should_retry(&p, &FailureKind::ExitCode(1), 3),
            RetryDecision::Exhausted
        );
    }

    #[test]
    fn skips_retry_for_unlisted_exit_codes() {
        let p = policy(5, vec![1], vec![], false);
        assert_eq!(
            should_retry(&p, &FailureKind::ExitCode(137), 1),
            RetryDecision::Exhausted
        );
    }

    #[test]
    fn retries_matching_signal() {
        let p = policy(3, vec![], vec![9], false);
        assert!(matches!(
            should_retry(&p, &FailureKind::Signal(9), 1),
            RetryDecision::Retry { .. }
        ));
    }

    #[test]
    fn signals_not_listed_do_not_retry() {
        let p = policy(3, vec![], vec![9], false);
        assert_eq!(
            should_retry(&p, &FailureKind::Signal(15), 1),
            RetryDecision::Exhausted
        );
    }

    #[test]
    fn timeout_retries_only_when_flag_set() {
        let off = policy(3, vec![], vec![], false);
        let on = policy(3, vec![], vec![], true);
        assert_eq!(
            should_retry(&off, &FailureKind::Timeout, 1),
            RetryDecision::Exhausted
        );
        assert!(matches!(
            should_retry(&on, &FailureKind::Timeout, 1),
            RetryDecision::Retry { .. }
        ));
    }

    #[test]
    fn no_retry_policy_exhausts_on_first_failure() {
        let p = RetryPolicy::no_retry();
        assert_eq!(
            should_retry(&p, &FailureKind::ExitCode(1), 1),
            RetryDecision::Exhausted
        );
    }

    #[test]
    fn backoff_grows_exponentially_until_cap() {
        let p = policy(20, vec![1], vec![], false);
        let d1 = match should_retry(&p, &FailureKind::ExitCode(1), 1) {
            RetryDecision::Retry { backoff } => backoff,
            RetryDecision::Exhausted => panic!(),
        };
        let d2 = match should_retry(&p, &FailureKind::ExitCode(1), 2) {
            RetryDecision::Retry { backoff } => backoff,
            RetryDecision::Exhausted => panic!(),
        };
        assert_eq!(d1, Duration::from_millis(100));
        assert_eq!(d2, Duration::from_millis(200));

        let d_many = match should_retry(&p, &FailureKind::ExitCode(1), 15) {
            RetryDecision::Retry { backoff } => backoff,
            RetryDecision::Exhausted => panic!(),
        };
        assert_eq!(d_many, p.max_backoff);
    }

    #[test]
    fn zero_initial_backoff_yields_immediate_retry() {
        let p = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::from_secs(10),
            backoff_multiplier: 2.0,
            on_exit_codes: vec![1],
            on_signals: vec![],
            on_timeout: false,
        };
        assert_eq!(
            should_retry(&p, &FailureKind::ExitCode(1), 1),
            RetryDecision::Retry {
                backoff: Duration::ZERO
            }
        );
    }
}
