//! Cancellation for admitted RPCs and their owned handlers.
//!
//! Admission and handler subscription share a token, so an early cancel is
//! latched until the handler can drain. Only the handler's terminal response
//! establishes completion; acknowledgement of a signal does not.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

#[derive(Default)]
struct Registry {
    operations: HashMap<String, CancellationToken>,
    closed: bool,
}

#[derive(Clone, Default)]
pub struct CancelRegistry {
    inner: Arc<Mutex<Registry>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to one admitted operation; IDs are unique until terminal cleanup.
    pub async fn register(&self, op_id: String) -> WaitForCancellationFutureOwned {
        let mut inner = self.inner.lock().await;
        let token = if inner.closed {
            let token = CancellationToken::new();
            token.cancel();
            token
        } else {
            inner.operations.entry(op_id).or_default().clone()
        };
        token.cancelled_owned()
    }

    /// Signal an admitted operation without discarding its cancellation state.
    pub async fn cancel(&self, op_id: &str) -> bool {
        let token = self.inner.lock().await.operations.get(op_id).cloned();
        if let Some(token) = token {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// Release an operation after its work and cleanup finish.
    pub async fn unregister(&self, op_id: &str) {
        self.inner.lock().await.operations.remove(op_id);
    }

    /// Cancel admitted work and reject future subscriptions during shutdown.
    pub async fn close(&self) {
        let operations = {
            let mut inner = self.inner.lock().await;
            inner.closed = true;
            std::mem::take(&mut inner.operations)
        };
        for token in operations.into_values() {
            token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Waker};
    use std::time::Duration;

    #[tokio::test]
    async fn signal_wakes_an_existing_subscription() {
        let registry = CancelRegistry::new();
        let signal = registry.register("owned".into()).await;
        tokio::pin!(signal);
        let mut context = Context::from_waker(Waker::noop());
        assert!(signal.as_mut().poll(&mut context).is_pending());
        assert!(registry.cancel("owned").await);
        assert!(signal.as_mut().poll(&mut context).is_ready());
    }

    #[tokio::test]
    async fn admitted_cancellation_is_latched_until_the_handler_subscribes() {
        let registry = CancelRegistry::new();
        drop(registry.register("owned".into()).await);
        assert!(registry.cancel("owned").await);
        let signal = registry.register("owned".into()).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), signal)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_admitted_and_late_subscriptions() {
        let registry = CancelRegistry::new();
        let admitted = registry.register("admitted".into()).await;
        registry.close().await;
        let late = registry.register("late".into()).await;
        for signal in [admitted, late] {
            assert!(
                tokio::time::timeout(Duration::from_millis(50), signal)
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn unknown_and_completed_ids_do_not_poison_new_operations() {
        let registry = CancelRegistry::new();
        assert!(!registry.cancel("unknown").await);
        drop(registry.register("unknown".into()).await);
        registry.cancel("unknown").await;
        registry.unregister("unknown").await;
        assert!(!registry.cancel("unknown").await);
        let next = registry.register("unknown".into()).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(5), next)
                .await
                .is_err()
        );
    }
}
