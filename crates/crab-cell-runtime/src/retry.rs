//! Shared storage-retry classification and backoff for runtime senders.
//!
//! Publication, release progress, and the Cell catalog retry the same storage
//! classes. Keeping the classifier and the delay schedule in one module stops
//! one sender from retrying a class another sender treats as fatal.

use std::time::Duration;

use crab_storage::{RetryClass, StorageError};

use crate::{Error, Result};

/// Longest delay any sender waits between storage retries.
pub(crate) const MAX_RETRY_DELAY_MS: u64 = 1_000;

/// Whether a sender that holds its own attempt budget may retry this failure.
pub(crate) fn retryable_storage_error(error: &StorageError) -> bool {
    matches!(
        crab_storage::retry_class(error),
        RetryClass::Transient
            | RetryClass::Throttled { .. }
            | RetryClass::StateDependent
            | RetryClass::InspectErrno
    )
}

/// Delay the provider asked for, when the failure carried one.
pub(crate) fn retry_hint(error: &StorageError) -> Option<Duration> {
    match crab_storage::retry_class(error) {
        RetryClass::Throttled { retry_after } => retry_after,
        _ => None,
    }
}

/// Bounded exponential backoff for one sender's retry loop.
pub(crate) struct Backoff {
    delay_ms: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { delay_ms: 100 }
    }
}

impl Backoff {
    /// Sleeps at least the current delay — and at least `minimum` when the
    /// provider asked for longer — then doubles the delay.
    pub(crate) async fn wait(&mut self, minimum: Option<Duration>) {
        let delay = Duration::from_millis(self.delay_ms);
        tokio::time::sleep(minimum.map_or(delay, |minimum| minimum.max(delay))).await;
        self.delay_ms = self.delay_ms.saturating_mul(2).min(MAX_RETRY_DELAY_MS);
    }

    /// [`Backoff::wait`] bounded by `deadline`: a wait that would run past it
    /// reports the fence instead, so a sender cannot outlive its ownership.
    pub(crate) async fn wait_until(
        &mut self,
        minimum: Option<Duration>,
        deadline: std::time::Instant,
    ) -> Result<()> {
        let delay = Duration::from_millis(self.delay_ms);
        let delay = minimum.map_or(delay, |minimum| minimum.max(delay));
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or(Error::Fenced)?;
        if delay >= remaining {
            return Err(Error::Fenced);
        }
        tokio::time::sleep(delay).await;
        self.delay_ms = self.delay_ms.saturating_mul(2).min(MAX_RETRY_DELAY_MS);
        Ok(())
    }
}
