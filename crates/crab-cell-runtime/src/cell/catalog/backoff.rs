//! Catalog retry policy over the shared storage retry classes.

use super::*;

const MAX_RETRY_DELAY_MS: u64 = 1_000;

pub(super) fn retryable_storage_error(error: &StorageError) -> bool {
    matches!(
        crab_storage::retry_class(error),
        crab_storage::RetryClass::Transient
            | crab_storage::RetryClass::Throttled { .. }
            | crab_storage::RetryClass::StateDependent
            | crab_storage::RetryClass::InspectErrno
    )
}

pub(super) fn retry_hint(error: &StorageError) -> Option<std::time::Duration> {
    match crab_storage::retry_class(error) {
        crab_storage::RetryClass::Throttled { retry_after } => retry_after,
        _ => None,
    }
}

pub(super) struct CatalogBackoff {
    delay_ms: u64,
}

impl Default for CatalogBackoff {
    fn default() -> Self {
        Self { delay_ms: 100 }
    }
}

impl CatalogBackoff {
    pub(super) async fn wait(&mut self, minimum: Option<std::time::Duration>) {
        let delay = std::time::Duration::from_millis(self.delay_ms);
        tokio::time::sleep(minimum.map_or(delay, |minimum| minimum.max(delay))).await;
        self.delay_ms = self.delay_ms.saturating_mul(2).min(MAX_RETRY_DELAY_MS);
    }
}
