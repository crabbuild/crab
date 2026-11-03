//! Unit tests for the cancellation registry and propagation flow.

#[cfg(test)]
mod tests {
    use crate::cancel::CancelRegistry;

    #[tokio::test]
    async fn cancel_fires_receiver() {
        let registry = CancelRegistry::new();
        let mut rx = registry.register("op-1".to_owned()).await;

        // The receiver should not be ready yet.
        assert!(rx.try_recv().is_err());

        // Fire the cancel signal.
        let found = registry.cancel("op-1").await;
        assert!(found);

        // Now the receiver should resolve.
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn cancel_unknown_op_returns_false() {
        let registry = CancelRegistry::new();
        let found = registry.cancel("nonexistent").await;
        assert!(!found);
    }

    #[tokio::test]
    async fn unregister_prevents_cancel() {
        let registry = CancelRegistry::new();
        let _rx = registry.register("op-2".to_owned()).await;

        // Unregister before cancel.
        registry.unregister("op-2").await;

        // Cancel should now return false (op already completed).
        let found = registry.cancel("op-2").await;
        assert!(!found);
    }

    #[tokio::test]
    async fn select_cancel_returns_op_cancelled() {
        use crate::{AgentError, Result};
        use serde_json::{Value, json};

        let registry = CancelRegistry::new();
        let mut cancel_rx = registry.register("op-3".to_owned()).await;

        // Simulate a long-running operation that gets cancelled.
        // We fire the cancel immediately, then select.
        registry.cancel("op-3").await;

        let work = async {
            // Simulate work that would take a long time.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok::<Value, AgentError>(json!({ "done": true }))
        };

        tokio::pin!(work);
        let result: Result<Value> = tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                Err(AgentError::OpCancelled { op_id: "op-3".to_owned() })
            }
            res = &mut work => res,
        };

        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            AgentError::OpCancelled { op_id } => assert_eq!(op_id, "op-3"),
            other => panic!("expected OpCancelled, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn work_completes_before_cancel() {
        use crate::{AgentError, Result};
        use serde_json::{Value, json};

        let registry = CancelRegistry::new();
        let mut cancel_rx = registry.register("op-4".to_owned()).await;

        // Work completes immediately — cancel never fires.
        let work = async { Ok::<Value, AgentError>(json!({ "result": 42 })) };

        tokio::pin!(work);
        let result: Result<Value> = tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                Err(AgentError::OpCancelled { op_id: "op-4".to_owned() })
            }
            res = &mut work => res,
        };

        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val["result"], 42);

        // Clean up.
        registry.unregister("op-4").await;
    }
}
