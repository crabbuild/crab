//! Cleanup must drain every owner without serializing independent backend I/O.
use super::with_ref_namespaces_wait;
use crate::WriteError;
use crab_storage::{StorageObservation, StorageObserver, StorageOperation, Store, StoreLayout};
use object_store::{
    memory::InMemory,
    throttle::{ThrottleConfig, ThrottledStore},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct CleanupObserver {
    enabled: AtomicBool,
    active: AtomicUsize,
    maximum: AtomicUsize,
}

impl StorageObserver for CleanupObserver {
    fn started(&self, _: StorageOperation) {
        if self.enabled.load(Ordering::Relaxed) {
            let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
            self.maximum.fetch_max(active, Ordering::Relaxed);
        }
    }
    fn finished(&self, _: StorageObservation) {
        if self.enabled.load(Ordering::Relaxed) {
            self.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[tokio::test]
async fn namespace_cleanup_overlaps_io_and_drains_before_success_or_failure_returns() {
    for fail_operation in [false, true] {
        let observer = Arc::new(CleanupObserver::default());
        let backend = ThrottledStore::new(
            InMemory::new(),
            ThrottleConfig {
                wait_put_per_call: Duration::from_millis(5),
                wait_get_per_call: Duration::from_millis(5),
                ..Default::default()
            },
        );
        let store = Store::new(Arc::new(backend)).with_storage_observer(observer.clone());
        let layout = StoreLayout::new(store.clone(), "repositories/namespace-cleanup".to_owned());
        let refs = (0..65)
            .map(|i| format!("refs/heads/owner-{i:04}"))
            .collect::<Vec<_>>();
        let cancel = CancellationToken::new();
        let operation_observer = observer.clone();
        let result: Result<(), WriteError> = with_ref_namespaces_wait(
            &store,
            &layout,
            &refs,
            Duration::from_secs(60),
            Duration::ZERO,
            &cancel,
            move |_| async move {
                operation_observer.enabled.store(true, Ordering::Relaxed);
                if fail_operation {
                    Err(WriteError::Cancelled)
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(result.is_err(), fail_operation);
        assert_eq!(observer.active.load(Ordering::Relaxed), 0);
        let maximum = observer.maximum.load(Ordering::Relaxed);
        assert!(maximum > 1, "independent cleanup remained serial");
        assert!(
            maximum <= 32,
            "cleanup exposed {maximum} simultaneous operations"
        );
        observer.enabled.store(false, Ordering::Relaxed);
        let result: Result<(), WriteError> = with_ref_namespaces_wait(
            &store,
            &layout,
            &refs,
            Duration::from_secs(60),
            Duration::ZERO,
            &cancel,
            |_| async { Ok(()) },
        )
        .await;
        assert!(
            result.is_ok(),
            "all namespace owners must be released before return"
        );
    }
}
