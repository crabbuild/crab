//! Cleanup must drain every owner without serializing independent backend I/O.
use super::{namespace_resource, with_ref_namespaces_wait};
use crate::WriteError;
use crab_coordination::{CoordinationError, PushLock};
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
    requests: AtomicUsize,
    cancel_on_request: Option<(usize, CancellationToken)>,
}

impl StorageObserver for CleanupObserver {
    fn started(&self, _: StorageOperation) {
        if self.enabled.load(Ordering::Relaxed) {
            let requests = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some((threshold, cancel)) = &self.cancel_on_request
                && requests == *threshold
            {
                cancel.cancel();
            }
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

#[tokio::test(start_paused = true)]
async fn namespace_admission_finishes_before_claims_need_renewal() {
    let observer = Arc::new(CleanupObserver {
        enabled: AtomicBool::new(true),
        ..Default::default()
    });
    let backend = ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: Duration::from_millis(50),
            ..Default::default()
        },
    );
    let store = Store::new(Arc::new(backend)).with_storage_observer(observer.clone());
    let layout = StoreLayout::new(store.clone(), "repositories/namespace-admission".to_owned());
    let refs = (0..65)
        .map(|i| format!("refs/heads/owner-{i:04}"))
        .collect::<Vec<_>>();
    let result: Result<usize, WriteError> = with_ref_namespaces_wait(
        &store,
        &layout,
        &refs,
        Duration::from_secs(3),
        Duration::ZERO,
        &CancellationToken::new(),
        |_| async { Ok(observer.requests.load(Ordering::Relaxed)) },
    )
    .await;

    assert_eq!(result.unwrap(), refs.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_namespace_admission_drains_claims_before_returning() {
    for cancelled in [false, true] {
        let cancel = CancellationToken::new();
        let observer = Arc::new(CleanupObserver {
            enabled: AtomicBool::new(true),
            cancel_on_request: cancelled.then(|| (8, cancel.clone())),
            ..Default::default()
        });
        let backend = ThrottledStore::new(
            InMemory::new(),
            ThrottleConfig {
                wait_put_per_call: Duration::from_millis(5),
                ..Default::default()
            },
        );
        let store = Store::new(Arc::new(backend)).with_storage_observer(observer.clone());
        let layout = StoreLayout::new(store.clone(), "repositories/namespace-partial".to_owned());
        let refs = (0..65)
            .map(|i| format!("refs/heads/owner-{i:04}"))
            .collect::<Vec<_>>();
        let blocker = if cancelled {
            None
        } else {
            let resource = refs
                .iter()
                .map(|name| namespace_resource(name))
                .min()
                .unwrap();
            Some(
                PushLock::acquire_internal(
                    store.inner(),
                    layout.repo_prefix(),
                    &resource,
                    Duration::from_secs(60),
                )
                .await
                .unwrap(),
            )
        };
        let callback_ran = AtomicBool::new(false);
        let result: Result<(), WriteError> = with_ref_namespaces_wait(
            &store,
            &layout,
            &refs,
            Duration::from_secs(60),
            Duration::ZERO,
            &cancel,
            |_| async {
                callback_ran.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;
        if cancelled {
            assert!(matches!(result, Err(WriteError::Cancelled)));
        } else {
            assert!(matches!(
                result,
                Err(WriteError::Coordination(
                    CoordinationError::PushLockHeld { .. }
                ))
            ));
        }
        assert!(!callback_ran.load(Ordering::Relaxed));
        assert_eq!(observer.active.load(Ordering::Relaxed), 0);
        if let Some(blocker) = blocker {
            blocker.release().await.unwrap();
        }
        let result: Result<(), WriteError> = with_ref_namespaces_wait(
            &store,
            &layout,
            &refs,
            Duration::from_secs(60),
            Duration::ZERO,
            &CancellationToken::new(),
            |_| async { Ok(()) },
        )
        .await;
        assert!(
            result.is_ok(),
            "partial claims survived cleanup: {result:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_namespace_batches_wait_without_cyclic_ownership() {
    let backend = ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: Duration::from_millis(5),
            ..Default::default()
        },
    );
    let store = Store::new(Arc::new(backend));
    let layout = StoreLayout::new(store.clone(), "repositories/namespace-overlap".to_owned());
    let refs = vec!["refs/heads/a".to_owned(), "refs/heads/b".to_owned()];
    let active = AtomicUsize::new(0);
    let publish = || async {
        with_ref_namespaces_wait(
            &store,
            &layout,
            &refs,
            Duration::from_secs(60),
            Duration::from_secs(5),
            &CancellationToken::new(),
            |_| async {
                assert_eq!(active.fetch_add(1, Ordering::Relaxed), 0);
                tokio::task::yield_now().await;
                active.fetch_sub(1, Ordering::Relaxed);
                Ok::<_, WriteError>(())
            },
        )
        .await
    };
    let (first, second) = tokio::join!(publish(), publish());

    assert!(first.is_ok() && second.is_ok(), "{first:?}; {second:?}");
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
