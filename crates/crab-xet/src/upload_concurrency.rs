//! Xet-backed upload concurrency limiter.

use std::sync::Arc;

use tokio::sync::Semaphore;
use tracing::warn;

use crate::error::{Result, XetError};

/// Concurrency strategy for xorb uploads.
///
/// Uses a static semaphore. Adaptive upload reporting needs xet-core permit
/// lifecycle APIs that are not public in the pinned dependency.
pub enum UploadConcurrency {
    Fixed {
        semaphore: Arc<Semaphore>,
        count: usize,
    },
}

impl UploadConcurrency {
    /// Builds from push config values.
    ///
    /// `concurrency` is the fixed permit count. When `adaptive` is requested,
    /// `min` and `max` bound the fixed permit count that substitutes for the
    /// unavailable adaptive controller.
    #[must_use]
    pub fn from_config(adaptive: bool, concurrency: usize, min: usize, max: usize) -> Self {
        if adaptive {
            let min = min.max(1);
            let max = max.max(min);
            let initial = concurrency.clamp(min, max);
            warn!(
                concurrency = initial,
                "adaptive upload concurrency unavailable with the pinned xet-core API; using fixed concurrency"
            );
            Self::fixed(initial)
        } else {
            Self::fixed(concurrency)
        }
    }

    /// Creates a fixed-concurrency limiter with `n` permits.
    #[must_use]
    pub fn fixed(n: usize) -> Self {
        let n = n.max(1);
        Self::Fixed {
            semaphore: Arc::new(Semaphore::new(n)),
            count: n,
        }
    }

    /// Creates the fixed-concurrency limiter used while adaptive reporting is unavailable.
    #[must_use]
    pub fn adaptive() -> Self {
        warn!(
            "adaptive upload concurrency unavailable with the pinned xet-core API; using one fixed permit"
        );
        Self::fixed(1)
    }

    /// Returns the current total permit count.
    #[must_use]
    pub fn total_permits(&self) -> usize {
        match self {
            Self::Fixed { count, .. } => *count,
        }
    }

    /// Acquires a permit, waiting until one is available.
    ///
    /// # Errors
    ///
    /// Returns [`XetError::Internal`] if the fixed semaphore is closed or the
    /// adaptive controller rejects permit acquisition.
    pub async fn acquire(&self) -> Result<UploadPermit> {
        match self {
            Self::Fixed { semaphore, .. } => {
                let permit = semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| XetError::Internal(format!("upload semaphore closed: {e}")))?;
                Ok(UploadPermit::Fixed(permit))
            }
        }
    }
}

/// A held upload permit.
///
/// Permits release on drop.
pub enum UploadPermit {
    Fixed(tokio::sync::OwnedSemaphorePermit),
}

impl UploadPermit {
    /// Signals that the transfer is about to start.
    pub async fn transfer_starting(&self) {}

    /// Reports a completed transfer.
    pub async fn report_completion(self, _n_bytes: u64, _success: bool) {}

    /// Reports a transient failure without consuming the permit.
    pub async fn report_retryable_failure(&self) {}
}

#[cfg(test)]
mod tests {
    use super::UploadConcurrency;

    #[test]
    fn fixed_concurrency_zero_clamps_to_one_permit() {
        let limiter = UploadConcurrency::fixed(0);

        assert_eq!(limiter.total_permits(), 1);
    }

    #[test]
    fn fixed_config_zero_clamps_to_one_permit() {
        let limiter = UploadConcurrency::from_config(false, 0, 0, 0);

        assert_eq!(limiter.total_permits(), 1);
    }

    #[test]
    fn adaptive_config_uses_bounded_fixed_permits() {
        let limiter = UploadConcurrency::from_config(true, 4, 2, 8);

        assert_eq!(limiter.total_permits(), 4);
        assert!(matches!(limiter, UploadConcurrency::Fixed { .. }));
    }
}
