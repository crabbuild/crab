use std::{collections::BTreeMap, sync::Arc, time::Duration};

use bytes::Bytes;
use crab_coordination::{CoordinationError, GIT_MANIFEST_RESOURCE, PushLock};
use crab_metadata::{manifest_store, manifests::Manifest, ref_journal};
use crab_storage::{Store, StoreLayout};
use crab_write::{
    WriteError,
    journal::{commit_edits, compact_for_owner, compact_for_reader},
};
use futures_util::TryStreamExt;
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
    let snapshot = manifest_store::read_repository_snapshot(store, layout)
        .await
        .unwrap();
    commit_edits(
        store,
        layout,
        &snapshot,
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
    .await
    .unwrap();
    lease
}

fn storage(prefix: &str) -> (Store, StoreLayout<Store>) {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), prefix.to_owned());
    (store, layout)
}

fn edit(name: &str, old: Option<char>, new: Option<char>) -> ref_journal::RefJournalEdit {
    ref_journal::RefJournalEdit {
        ref_name: name.to_owned(),
        old_oid: old.map(|value| value.to_string().repeat(40)),
        new_oid: new.map(|value| value.to_string().repeat(40)),
        peeled_oid: None,
        lock_holder: None,
        visibility_evidence_hash: None,
    }
}

#[tokio::test]
async fn batches_preserve_causal_parents_and_exact_ref_changes_before_compaction() {
    let (store, layout) = storage("commit-edits");
    let tag = "refs/tags/release";
    let dev = "refs/heads/dev";
    manifest_store::create_manifest(&store, &layout, &Manifest::default_for_repo(REF))
        .await
        .unwrap();
    let mut leases = Vec::new();
    for name in [REF, tag, dev] {
        leases.push(
            PushLock::acquire_ref(store.inner(), layout.repo_prefix(), name, TTL)
                .await
                .unwrap(),
        );
    }
    let snapshot = manifest_store::read_repository_snapshot(&store, &layout)
        .await
        .unwrap();
    let mut annotated = edit(tag, None, Some('b'));
    annotated.peeled_oid = Some("a".repeat(40));
    let first = commit_edits(
        &store,
        &layout,
        &snapshot,
        vec![annotated, edit(REF, None, Some('a'))],
        None,
        vec![],
        vec![],
    )
    .await
    .unwrap();
    let snapshot = manifest_store::read_repository_snapshot(&store, &layout)
        .await
        .unwrap();
    assert_eq!(snapshot.journal.peeled_refs.get(tag), Some(&"a".repeat(40)));
    let second = commit_edits(
        &store,
        &layout,
        &snapshot,
        vec![
            edit(tag, Some('b'), None),
            edit(REF, Some('a'), Some('c')),
            edit(dev, None, Some('d')),
        ],
        Some(dev.to_owned()),
        vec![],
        vec![],
    )
    .await
    .unwrap();
    for lease in leases {
        lease.release().await.unwrap();
    }
    let body = ref_journal::read_transaction(&store, &layout, &second.transaction_id)
        .await
        .unwrap();
    assert_eq!(
        body.parents,
        BTreeMap::from([
            (REF.to_owned(), Some(first.transaction_id.clone())),
            (tag.to_owned(), Some(first.transaction_id)),
            (dev.to_owned(), None),
        ])
    );
    let final_state = manifest_store::read_repository_snapshot(&store, &layout)
        .await
        .unwrap();
    assert_eq!(
        (
            final_state.journal.refs,
            final_state.journal.peeled_refs,
            final_state.journal.head
        ),
        (
            BTreeMap::from([
                (REF.to_owned(), "c".repeat(40)),
                (dev.to_owned(), "d".repeat(40))
            ]),
            BTreeMap::new(),
            dev.to_owned()
        )
    );
    assert!(final_state.manifest.refs.is_empty());
}

#[tokio::test]
async fn invalid_or_stale_batch_leaves_no_journal_artifacts() {
    let (store, layout) = storage("rejected-edits");
    let lease = pending(&store, &layout).await;
    let sibling = "refs/heads/dev";
    let sibling_lease = PushLock::acquire_ref(store.inner(), layout.repo_prefix(), sibling, TTL)
        .await
        .unwrap();
    let snapshot = manifest_store::read_repository_snapshot(&store, &layout)
        .await
        .unwrap();
    let before: Vec<_> = store.inner().list(None).try_collect().await.unwrap();
    for (edits, stale) in [
        (
            vec![edit(sibling, None, Some('c')), edit(REF, None, Some('b'))],
            true,
        ),
        (
            vec![
                edit(sibling, None, Some('c')),
                edit(sibling, None, Some('d')),
            ],
            false,
        ),
    ] {
        let result = commit_edits(&store, &layout, &snapshot, edits, None, vec![], vec![]).await;
        if stale {
            assert!(
                matches!(result, Err(WriteError::RefChanged { ref_name, .. }) if ref_name == REF)
            );
        } else {
            assert!(matches!(result, Err(WriteError::Metadata(_))));
        }
        let after: Vec<_> = store.inner().list(None).try_collect().await.unwrap();
        assert_eq!(after, before);
    }
    sibling_lease.release().await.unwrap();
    lease.release().await.unwrap();
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
