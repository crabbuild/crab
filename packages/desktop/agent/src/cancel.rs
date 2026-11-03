//! Cancellation registry for long-running RPC handlers.
//!
//! Each cancellable handler registers its `op_id` and receives a
//! `oneshot::Receiver<()>`. The handler runs inside a `tokio::select!`
//! that races the work future against the cancel signal. When the
//! renderer sends `rpc/cancel { op_id }`, the registry fires the
//! corresponding sender, causing the handler to return `OpCancelled`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, oneshot};

/// Shared cancel registry. Cloned into the dispatch state.
#[derive(Clone, Default)]
pub struct CancelRegistry {
    inner: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new cancellable operation. Returns the receiver the
    /// handler should select on.
    pub async fn register(&self, op_id: String) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().await.insert(op_id, tx);
        rx
    }

    /// Fire the cancel signal for `op_id`. Returns `true` if the
    /// operation was found and signalled, `false` if it had already
    /// completed or was never registered.
    pub async fn cancel(&self, op_id: &str) -> bool {
        let tx = self.inner.lock().await.remove(op_id);
        match tx {
            Some(sender) => {
                // The receiver may have been dropped if the handler
                // already finished — that's fine, we just discard.
                let _ = sender.send(());
                true
            }
            None => false,
        }
    }

    /// Remove a completed operation from the registry without firing
    /// the cancel signal. Called by handlers on normal completion.
    pub async fn unregister(&self, op_id: &str) {
        self.inner.lock().await.remove(op_id);
    }
}
