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
