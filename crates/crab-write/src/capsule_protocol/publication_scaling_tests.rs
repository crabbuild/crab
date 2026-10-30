//! Bound initial multi-ref admission without changing ordered ref/head binding.
#![expect(clippy::unwrap_used, reason = "test assertions")]
use super::*;
use crab_metadata::capsule_protocol::CapsuleRefEdit;
use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
use object_store::{
    memory::InMemory,
    throttle::{ThrottleConfig, ThrottledStore},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct ConcurrentReads {
    active: AtomicUsize,
    maximum: AtomicUsize,
}
impl StorageObserver for ConcurrentReads {
    fn started(&self, operation: StorageOperation) {
        if operation == StorageOperation::Get {
            let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
            self.maximum.fetch_max(active, Ordering::Relaxed);
        }
    }
    fn finished(&self, observation: StorageObservation) {
        if observation.operation == StorageOperation::Get {
            self.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[derive(Default)]
struct ConcurrentWrites {
    active: AtomicUsize,
    maximum: AtomicUsize,
    total: AtomicUsize,
}
impl StorageObserver for ConcurrentWrites {
    fn started(&self, operation: StorageOperation) {
        if operation == StorageOperation::Put {
            self.total.fetch_add(1, Ordering::Relaxed);
            let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
            self.maximum.fetch_max(active, Ordering::Relaxed);
        }
    }
    fn finished(&self, observation: StorageObservation) {
        if observation.operation == StorageOperation::Put {
            self.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

async fn prepared_refs(router: &StoreLayout<Store>, count: usize) -> PreparedCapsulePublication {
    initialize(router, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let base = open_root(router).await.unwrap();
    let edits = (0..count)
        .map(|index| {
            CapsuleRefEdit::new(
                format!("refs/heads/agent-{index:04}"),
                None,
                Some(format!("{:040x}", index + 2)),
                None,
            )
        })
        .collect();
    let transaction = CapsuleTransaction::new(base.record().digest(), edits).unwrap();
    let capsule = super::tests::capsule(&transaction);
    prepare_publication(router, base, &transaction, &capsule)
        .await
        .unwrap()
}

#[tokio::test]
async fn multi_ref_commit_bounds_parallel_writes_and_preserves_every_ref() {
    let observer = Arc::new(ConcurrentWrites::default());
    let inner = ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: Duration::from_millis(5),
            ..Default::default()
        },
    );
    let store = Store::new(Arc::new(inner)).with_storage_observer(observer.clone());
    let router = StoreLayout::new(store, "repositories/multi-ref-commit-bound".to_owned());
    let prepared = prepared_refs(&router, 257).await;
    let transaction_id = prepared.transaction.id().unwrap();
    observer.maximum.store(0, Ordering::Relaxed);
    commit_multi_ref(
        &router,
        &transaction_id,
        &activation_id(&transaction_id),
        prepared.refs,
    )
    .await
    .unwrap();
    let maximum = observer.maximum.load(Ordering::Relaxed);
    assert!(
        maximum > 1,
        "multi-ref publication must overlap backend writes"
    );
    assert!(
        maximum <= 32,
        "publication exposed {maximum} simultaneous writes"
    );
    assert_eq!(observer.active.load(Ordering::Relaxed), 0);
    for edit in prepared.transaction.edits() {
        let head = read_ref_head(&router, prepared.base.record().root(), edit.ref_name())
            .await
            .unwrap();
        assert_eq!(head.visible.oid(), edit.new_oid());
    }
}

#[tokio::test]
async fn failed_multi_ref_commit_drains_writes_and_leaves_all_refs_old() {
    let observer = Arc::new(ConcurrentWrites::default());
    let inner = ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: Duration::from_millis(5),
            ..Default::default()
        },
    );
    let store = Store::new(Arc::new(inner)).with_storage_observer(observer.clone());
    let router = StoreLayout::new(store, "repositories/multi-ref-commit-failure".to_owned());
    let mut prepared = prepared_refs(&router, 257).await;
    let transaction_id = prepared.transaction.id().unwrap();
    let activation = activation_id(&transaction_id);
    // This conditional update cannot create the absent first ref. Other initial
    // writes remain in flight and must settle before rollback and return.
    prepared.refs[0].original.etag = Some(ETag {
        e_tag: Some("absent-original".to_owned()),
        version: None,
    });
    observer.maximum.store(0, Ordering::Relaxed);
    observer.total.store(0, Ordering::Relaxed);
    commit_multi_ref(&router, &transaction_id, &activation, prepared.refs)
        .await
        .expect_err("a failed head must prevent atomic activation");
    assert_eq!(observer.active.load(Ordering::Relaxed), 0);
    assert!(observer.maximum.load(Ordering::Relaxed) > 1);
    assert!(observer.maximum.load(Ordering::Relaxed) <= 32);
    assert!(observer.total.load(Ordering::Relaxed) < 257);
    let (body, _) = router
        .store()
        .get_with_etag_bounded(
            &router.capsule_transaction_path(&activation),
            crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
        )
        .await
        .unwrap();
    let record = crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&body).unwrap();
    assert_eq!(
        record.status(),
        crab_metadata::capsule_protocol::CapsuleTransactionStatus::Aborted
    );
    assert!(matches!(
        router
            .store()
            .head(&router.capsule_committed_transaction_path(&activation))
            .await,
        Err(StorageError::NotFound { .. })
    ));
    for edit in prepared.transaction.edits() {
        let head = read_ref_head(&router, prepared.base.record().root(), edit.ref_name())
            .await
            .unwrap();
        assert_eq!(head.visible.oid(), None);
        assert_eq!(head.head.prepared_activation_id(), None);
    }
}

#[tokio::test]
async fn multi_ref_preparation_bounds_reads_and_preserves_exact_ref_order() {
    let observer = Arc::new(ConcurrentReads::default());
    let inner = ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_get_per_call: Duration::from_millis(5),
            ..Default::default()
        },
    );
    let store = Store::new(Arc::new(inner)).with_storage_observer(observer.clone());
    let router = StoreLayout::new(store, "repositories/multi-ref-bound".to_owned());
    initialize(&router, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let base = open_root(&router).await.unwrap();
    observer.maximum.store(0, Ordering::Relaxed);
    let edits = (0..257)
        .map(|index| {
            CapsuleRefEdit::new(
                format!("refs/heads/agent-{index:04}"),
                None,
                Some(format!("{:040x}", index + 2)),
                None,
            )
        })
        .collect::<Vec<_>>();
    let transaction = CapsuleTransaction::new(base.record().digest(), edits).unwrap();
    let capsule = super::tests::capsule(&transaction);
    let prepared = prepare_publication(&router, base, &transaction, &capsule)
        .await
        .unwrap();
    let maximum = observer.maximum.load(Ordering::Relaxed);
    assert!(maximum > 1, "admission must retain parallel I/O");
    assert!(
        maximum <= 32,
        "admission exposed {maximum} simultaneous reads"
    );
    assert_eq!(prepared.refs.len(), 257);
    for (head, edit) in prepared.refs.iter().zip(transaction.edits()) {
        assert_eq!(head.original.head.ref_name(), edit.ref_name());
        assert_eq!(head.original.visible.oid(), None);
        assert_eq!(
            head.candidate.visible(&Default::default()).oid(),
            edit.new_oid()
        );
    }
}
