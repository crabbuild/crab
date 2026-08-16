//! Pure workflow stage contract types.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Stable output kind for stage outs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutKind {
    File,
    Directory,
    /// The command's stdout is captured and written to the declared output.
    Stdout,
}

impl OutKind {
    /// Stable string tag used in canonical serialization.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OutKind::File => "file",
            OutKind::Directory => "directory",
            OutKind::Stdout => "stdout",
        }
    }
}

/// Environment policy for a workflow stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvSpec {
    Inherit,
    Allowlist(Vec<String>),
    Empty,
}

/// Resource requirements for a workflow stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    /// Number of CPU slots required.
    pub cpu: u32,
    /// Number of GPU slots required.
    pub gpu: u32,
    /// Memory requirement in bytes.
    pub memory_bytes: u64,
}

impl Resources {
    /// Default resource requirement: 1 CPU, no GPU, no memory reservation.
    #[must_use]
    pub fn default_resources() -> Self {
        Self {
            cpu: 1,
            gpu: 0,
            memory_bytes: 0,
        }
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self::default_resources()
    }
}

/// Retry policy attached to a workflow stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub backoff_multiplier: f64,
    pub on_exit_codes: Vec<i32>,
    pub on_signals: Vec<i32>,
    pub on_timeout: bool,
}

impl RetryPolicy {
    /// Default exponential-backoff multiplier.
    pub const DEFAULT_BACKOFF_MULTIPLIER: f64 = 2.0;

    /// Policy that does not retry: one attempt, zero backoffs.
    #[must_use]
    pub fn no_retry() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            backoff_multiplier: Self::DEFAULT_BACKOFF_MULTIPLIER,
            on_exit_codes: Vec::new(),
            on_signals: Vec::new(),
            on_timeout: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_kind_tags_are_stable() {
        assert_eq!(OutKind::File.as_str(), "file");
        assert_eq!(OutKind::Directory.as_str(), "directory");
        assert_eq!(OutKind::Stdout.as_str(), "stdout");
    }

    #[test]
    fn resources_default_is_one_cpu_only() {
        assert_eq!(
            Resources::default(),
            Resources {
                cpu: 1,
                gpu: 0,
                memory_bytes: 0
            }
        );
    }

    #[test]
    fn no_retry_policy_has_single_attempt() {
        let policy = RetryPolicy::no_retry();
        assert_eq!(policy.max_attempts, 1);
        assert_eq!(policy.initial_backoff, Duration::ZERO);
        assert_eq!(policy.max_backoff, Duration::ZERO);
        assert!(policy.on_exit_codes.is_empty());
        assert!(policy.on_signals.is_empty());
        assert!(!policy.on_timeout);
    }
}
