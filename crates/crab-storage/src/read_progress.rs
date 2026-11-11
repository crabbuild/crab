use std::sync::Arc;

use tokio::sync::watch;

use crate::ReadAdmission;

/// Cumulative origin response-body bytes shared by explicitly related reads.
///
/// Counts consumed HTTP body chunks, including failed attempts and repeated
/// ranges. Excludes headers, transport overhead, local reads and cache copies.
#[derive(Clone, Debug)]
pub struct ReadProgress(watch::Sender<u64>);

/// Coalescing observations of one origin-byte counter.
#[derive(Debug)]
pub struct ReadProgressReceiver(watch::Receiver<u64>);

impl ReadProgress {
    /// Create a zero-byte counter and its bounded observation channel.
    #[must_use]
    pub fn channel() -> (Self, ReadProgressReceiver) {
        let (send, receive) = watch::channel(0);
        (Self(send), ReadProgressReceiver(receive))
    }

    /// Return bytes observed so far, independently of progress consumption.
    #[must_use]
    pub fn received_bytes(&self) -> u64 {
        *self.0.borrow()
    }

    /// Observe subsequent changes without consuming another receiver's position.
    #[must_use]
    pub fn subscribe(&self) -> ReadProgressReceiver {
        ReadProgressReceiver(self.0.subscribe())
    }

    /// Record one physical body chunk at its read-admission owner.
    pub fn record_received(&self, bytes: u64) {
        if bytes != 0 {
            self.0
                .send_modify(|total| *total = total.saturating_add(bytes));
        }
    }

    /// Observe origin receipts without replacing the caller's admission policy.
    #[must_use]
    pub fn admission(&self, inner: Arc<dyn ReadAdmission>) -> Arc<dyn ReadAdmission> {
        Arc::new(ObservedAdmission {
            inner,
            progress: self.clone(),
        })
    }

    /// Observe reads with caller cancellation and no additional resource budget.
    #[must_use]
    pub fn read_admission(
        &self,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Arc<dyn ReadAdmission> {
        Arc::new(ProgressAdmission {
            cancellation,
            progress: self.clone(),
        })
    }
}

impl ReadProgressReceiver {
    /// Return the latest cumulative count, or None after all producers close.
    pub async fn next(&mut self) -> Option<u64> {
        self.0.changed().await.ok()?;
        Some(*self.0.borrow_and_update())
    }
}

struct ObservedAdmission {
    inner: Arc<dyn ReadAdmission>,
    progress: ReadProgress,
}

struct ProgressAdmission {
    cancellation: tokio_util::sync::CancellationToken,
    progress: ReadProgress,
}

#[async_trait::async_trait]
impl ReadAdmission for ProgressAdmission {
    fn cancellation(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancellation
    }
    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn bytes(&self, _bytes: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn received(&self, bytes: u64) {
        self.progress.record_received(bytes);
    }
}

#[async_trait::async_trait]
impl ReadAdmission for ObservedAdmission {
    fn cancellation(&self) -> &tokio_util::sync::CancellationToken {
        self.inner.cancellation()
    }

    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.inner.request().await
    }

    async fn bytes(&self, bytes: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.inner.bytes(bytes).await
    }

    async fn received(&self, bytes: u64) {
        self.progress.record_received(bytes);
        self.inner.received(bytes).await;
    }
}
