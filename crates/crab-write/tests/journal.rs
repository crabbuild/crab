use std::{collections::BTreeMap, sync::Arc, time::Duration};

use bytes::Bytes;
use crab_coordination::{CoordinationError, GIT_MANIFEST_RESOURCE, PushLock};
use crab_metadata::{manifest_store, manifests::Manifest, ref_journal};
use crab_storage::{Store, StoreLayout};
use crab_write::{
    WriteError,
    journal::{compact_for_owner, compact_for_reader},
};
use object_store::ObjectStoreExt;
use tokio_util::sync::CancellationToken;

const TTL: Duration = Duration::from_secs(60);
const REF: &str = "refs/heads/main";

async fn pending(store: &Store, layout: &StoreLayout<Store>) -> PushLock {
    manifest_store::create_manifest(store, layout, &Manifest::default_for_repo(REF))
        .await
        .unwrap();
    let lease = PushLock::acquire_ref(store.inner(), layout.repo_prefix(), REF, TTL)
        .await
        .unwrap();
    let head = ref_journal::read_ref_head(store, layout, REF)
        .await
        .unwrap();
    let transaction = ref_journal::RefJournalTransaction::new(
        BTreeMap::from([(REF.to_owned(), None)]),
        vec![ref_journal::RefJournalEdit {
            ref_name: REF.to_owned(),
            old_oid: None,
            new_oid: Some("a".repeat(40)),
            peeled_oid: None,
            lock_holder: Some(lease.holder().to_owned()),
            visibility_evidence_hash: None,
        }],
        None,
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    ref_journal::commit_ref_transaction(store, layout, &transaction, &[head])
        .await
        .unwrap();
    lease
}

fn storage(prefix: &str) -> (Store, StoreLayout<Store>) {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), prefix.to_owned());
    (store, layout)
}

#[tokio::test]
async fn compaction_releases_only_the_committed_holder() {
    for replaced in [false, true] {
        let (store, layout) = storage("holder-handoff");
        let original = pending(&store, &layout).await;
        let lease = if replaced {
            original.release().await.unwrap();
            PushLock::acquire_ref(store.inner(), layout.repo_prefix(), REF, TTL)
                .await
                .unwrap()
        } else {
            original
        };
        let compacted = if replaced {
            compact_for_reader(&store, &layout, TTL, None, &CancellationToken::new()).await
        } else {
            compact_for_owner(&store, &layout, TTL, None, &CancellationToken::new()).await
        }
        .unwrap();
        assert!(compacted);
        let snapshot = manifest_store::read_repository_snapshot(&store, &layout)
            .await
            .unwrap();
        assert_eq!(snapshot.manifest.refs.get(REF), Some(&"a".repeat(40)));
        assert!(snapshot.journal.transactions.is_empty());
        let acquisition =
            PushLock::acquire_ref(store.inner(), layout.repo_prefix(), REF, TTL).await;
        if replaced {
            assert!(matches!(
                acquisition,
                Err(CoordinationError::PushLockHeld { .. })
            ));
        } else {
            acquisition
                .expect("committed holder was released")
                .release()
                .await
                .unwrap();
        }
        lease.release().await.unwrap();
        assert!(
            !compact_for_owner(&store, &layout, TTL, None, &CancellationToken::new())
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn contended_reader_skips_and_cancelled_owner_preserves_pending_transaction() {
    let (store, layout) = storage("contention");
    let lease = pending(&store, &layout).await;
    let blocker = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        GIT_MANIFEST_RESOURCE,
        TTL,
    )
    .await
    .unwrap();
    assert!(
        !tokio::time::timeout(
            Duration::from_secs(1),
            compact_for_reader(&store, &layout, TTL, None, &CancellationToken::new(),)
        )
        .await
        .unwrap()
        .unwrap()
    );
    let cancel = CancellationToken::new();
    let operation = compact_for_owner(&store, &layout, TTL, None, &cancel);
    let signal = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(operation, signal)
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(WriteError::Cancelled)));
    let (manifest, _) = manifest_store::read_manifest(&store, &layout)
        .await
        .unwrap();
    assert!(manifest.refs.is_empty());
    assert_eq!(
        ref_journal::list_active_transactions(&store, &layout)
            .await
            .unwrap()
            .len(),
        1
    );
    blocker.release().await.unwrap();
    lease.release().await.unwrap();
    assert!(
        compact_for_owner(&store, &layout, TTL, None, &CancellationToken::new())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn failed_compaction_releases_manifest_lease_and_can_retry() {
    let (store, layout) = storage("repair");
    let lease = pending(&store, &layout).await;
    let (original, _) = store.get_with_etag(&layout.manifest_path()).await.unwrap();
    // Deliberately bypass the state-write guard to model damaged origin bytes.
    store
        .inner()
        .put(
            &layout.manifest_path(),
            Bytes::from_static(b"invalid manifest").into(),
        )
        .await
        .unwrap();
    let result = compact_for_owner(&store, &layout, TTL, None, &CancellationToken::new()).await;
    assert!(matches!(result, Err(WriteError::Metadata(_))));
    let probe = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        GIT_MANIFEST_RESOURCE,
        TTL,
    )
    .await
    .expect("failed operation released its lease");
    probe.release().await.unwrap();
    store
        .inner()
        .put(&layout.manifest_path(), original.into())
        .await
        .unwrap();
    assert!(
        compact_for_owner(&store, &layout, TTL, None, &CancellationToken::new())
            .await
            .unwrap()
    );
    lease.release().await.unwrap();
}
