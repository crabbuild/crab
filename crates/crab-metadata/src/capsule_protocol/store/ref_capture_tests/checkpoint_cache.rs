use super::*;

#[tokio::test]
async fn a_single_ref_cache_must_not_add_reads_after_retokening() {
    let (layout, root, store) = fixture(Identity::ETag, false, false).await;
    store.replacements.lock().unwrap().clear();
    store
        .inner
        .delete(&layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/unchanged")))
        .await
        .unwrap();
    let layout = StoreLayout::new(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
    );
    let (root, _) = checkpoint_heads(&layout, &root).await;
    let path = layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/changed"));
    let body = store.inner.get(&path).await.unwrap().bytes().await.unwrap();
    layout.store().put_overwrite(&path, body).await.unwrap();
    store.reads.lock().unwrap().clear();
    capture_ref_heads(&layout, &root).await.unwrap();
    assert_eq!(store.reads.lock().unwrap().values().sum::<usize>(), 1);
}

async fn checkpoint_heads(
    layout: &StoreLayout<Store>,
    root: &crate::capsule_protocol::RepositoryRoot,
) -> (crate::capsule_protocol::RepositoryRoot, LayeredCheckpoint) {
    use crate::capsule_protocol::{
        Capsule, CapsuleGitPack, CapsulePointer, CapsuleRefEdit, CapsuleTransaction, CatalogInput,
        HistorySegment, HistorySegmentState, PackLayer,
    };
    let initial = RootRecord::encode(root.clone()).unwrap();
    let transaction = CapsuleTransaction::new(
        initial.digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/seed",
            None,
            Some("4".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let run = CapsuleRun::leaf(Capsule::build(&transaction, vec![], vec![]).unwrap()).unwrap();
    let pointer = CapsulePointer::new(
        run.hash(),
        run.bytes().len() as u64,
        run.control_offset(),
        run.control_size(),
        run.footer_hash(),
        run.level(),
        run.transaction_ids(),
        run.newest_base_root_digest(),
    )
    .unwrap();
    let mut refs = root.refs().clone();
    refs.insert("refs/heads/seed".to_owned(), "4".repeat(40));
    let root = root
        .advance(
            initial.digest(),
            refs,
            root.peeled_refs().clone(),
            vec![pointer],
            &transaction.id().unwrap(),
        )
        .unwrap();
    let snapshot = capture_ref_head_snapshot(layout, &root).await.unwrap();
    let cache = snapshot.checkpoint_cache().unwrap();
    let base = RootRecord::encode(root.clone()).unwrap();
    let pack = CapsuleGitPack::new(
        Bytes::from_static(b"PACK\0\0\0\0\0\0\0\0"),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "1".repeat(40),
        1,
    )
    .unwrap();
    let checkpoint = LayeredCheckpoint::build_with_catalog(
        root.generation(),
        base.digest(),
        vec![
            PackLayer::build(&pack)
                .unwrap()
                .source_descriptor()
                .unwrap(),
        ],
        CatalogInput::New(PointerCatalog::new()),
        None,
        cache.as_ref(),
        u64::MAX,
    )
    .unwrap();
    layout
        .store()
        .put_overwrite(
            &layout.capsule_checkpoint_path(checkpoint.hash()),
            checkpoint.bytes().clone(),
        )
        .await
        .unwrap();
    let history = HistorySegment::build(
        checkpoint.pointer().unwrap(),
        None,
        HistorySegmentState::new(
            root.refs().clone(),
            root.peeled_refs().clone(),
            root.head().to_owned(),
            root.compacted_ref_transactions().clone(),
            root.capsule_frontier().to_vec(),
        ),
    )
    .unwrap();
    let root = root
        .install_checkpoint(
            base.digest(),
            checkpoint.pointer().unwrap(),
            Some(history.pointer().unwrap()),
        )
        .unwrap();
    (root, checkpoint)
}

#[tokio::test]
async fn checkpoint_cache_reuses_only_identified_heads_and_still_lists_twice() {
    for identity in [
        Identity::ETag,
        Identity::WeakETag,
        Identity::Version,
        Identity::NullVersion,
        Identity::EmptyVersion,
        Identity::Absent,
    ] {
        let (layout, root, store) = fixture(identity, false, false).await;
        store.replacements.lock().unwrap().clear();
        let layout = StoreLayout::new(
            layout.store().clone().with_target_identity([1; 32]),
            layout.repo_prefix().to_owned(),
        );
        let (root, _) = checkpoint_heads(&layout, &root).await;
        store.reads.lock().unwrap().clear();
        store.lists.store(0, Ordering::SeqCst);
        let (heads, active) = capture_ref_heads(&layout, &root).await.unwrap();
        let count: usize = store
            .reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.starts_with(layout.capsule_ref_heads_prefix().as_ref()))
            .map(|(_, count)| count)
            .sum();
        assert_eq!(
            (
                heads.len(),
                active.len(),
                store.lists.load(Ordering::SeqCst),
                count
            ),
            (
                2,
                0,
                2,
                if matches!(identity, Identity::ETag | Identity::Version) {
                    0
                } else {
                    2
                }
            ),
            "{identity:?}",
        );
    }
}

#[tokio::test]
async fn push_inventory_reuses_only_version_bound_checkpoint_heads() {
    for identity in [
        Identity::ETag,
        Identity::Version,
        Identity::WeakETag,
        Identity::Absent,
    ] {
        let (layout, root, store) = fixture(identity, false, false).await;
        store.replacements.lock().unwrap().clear();
        let layout = StoreLayout::new(
            layout.store().clone().with_target_identity([1; 32]),
            layout.repo_prefix().to_owned(),
        );
        let (root, _) = checkpoint_heads(&layout, &root).await;
        store.reads.lock().unwrap().clear();
        store.lists.store(0, Ordering::SeqCst);
        let heads = load_ref_heads_for_push(&layout, &root).await.unwrap();
        let reads: usize = store
            .reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.starts_with(layout.capsule_ref_heads_prefix().as_ref()))
            .map(|(_, count)| count)
            .sum();
        let expected_reads = if matches!(identity, Identity::ETag | Identity::Version) {
            0
        } else {
            2
        };
        assert_eq!(
            (heads.len(), store.lists.load(Ordering::SeqCst), reads),
            (2, 1, expected_reads),
            "{identity:?}"
        );
    }
}

#[tokio::test]
async fn checkpoint_cache_does_not_cross_physical_targets() {
    let (layout, root, store) = fixture(Identity::ETag, false, false).await;
    store.replacements.lock().unwrap().clear();
    let layout = StoreLayout::new(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
    );
    let (root, _) = checkpoint_heads(&layout, &root).await;
    for identity in [[2; 32], [1; 32]] {
        let layout = StoreLayout::new(
            layout.store().clone().with_target_identity(identity),
            layout.repo_prefix().to_owned(),
        );
        store.reads.lock().unwrap().clear();
        capture_ref_heads(&layout, &root).await.unwrap();
        let count: usize = store
            .reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.starts_with(layout.capsule_ref_heads_prefix().as_ref()))
            .map(|(_, count)| count)
            .sum();
        assert_eq!(count, if identity == [1; 32] { 0 } else { 2 });
    }
}

#[tokio::test]
async fn checkpoint_cache_does_not_cross_ref_prefixes_with_coincident_etags() {
    let (layout, root, store) = fixture(Identity::FixedETag, false, false).await;
    store.replacements.lock().unwrap().clear();
    let layout = StoreLayout::new(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
    );
    let (root, checkpoint) = checkpoint_heads(&layout, &root).await;
    let other = StoreLayout::new(
        layout.store().clone(),
        "repositories/other-prefix".to_owned(),
    );
    for name in ["refs/heads/changed", "refs/heads/unchanged"] {
        let key = capsule_ref_name_key(name);
        store
            .inner
            .copy(
                &layout.capsule_ref_head_path(&key),
                &other.capsule_ref_head_path(&key),
            )
            .await
            .unwrap();
    }
    store
        .inner
        .copy(
            &layout.capsule_checkpoint_path(checkpoint.hash()),
            &other.capsule_checkpoint_path(checkpoint.hash()),
        )
        .await
        .unwrap();
    store.reads.lock().unwrap().clear();
    capture_ref_heads(&other, &root).await.unwrap();
    let count: usize = store
        .reads
        .lock()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.starts_with(other.capsule_ref_heads_prefix().as_ref()))
        .map(|(_, count)| count)
        .sum();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn routed_ref_heads_do_not_inherit_the_base_transport_cache() {
    let (layout, root, store) = fixture(Identity::FixedETag, false, false).await;
    store.replacements.lock().unwrap().clear();
    let layout = StoreLayout::new(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
    );
    let (root, _) = checkpoint_heads(&layout, &root).await;
    let (other_layout, _, other) = fixture(Identity::FixedETag, false, false).await;
    other.replacements.lock().unwrap().clear();
    let path = other_layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/changed"));
    let retired =
        CapsuleRefHead::from_root("refs/heads/changed", "2".repeat(64), None, None).unwrap();
    other_layout
        .store()
        .put_overwrite(&path, retired.encode().unwrap())
        .await
        .unwrap();
    other.reads.lock().unwrap().clear();
    let routed = layout.store().clone().with_read_routes(vec![(
        layout.capsule_ref_heads_prefix().to_string(),
        other.clone(),
    )]);
    let layout = StoreLayout::new(routed, layout.repo_prefix().to_owned());
    let (heads, _) = capture_ref_heads(&layout, &root).await.unwrap();
    let reads: usize = other.reads.lock().unwrap().values().sum();
    assert_eq!(
        (
            heads
                .iter()
                .map(CapsuleRefHead::ref_name)
                .collect::<Vec<_>>(),
            reads
        ),
        (vec!["refs/heads/unchanged"], 2)
    );
}

#[tokio::test]
async fn checkpoint_cache_does_not_cross_repository_identity() {
    let (layout, root, store) = fixture(Identity::ETag, false, false).await;
    store.replacements.lock().unwrap().clear();
    let layout = StoreLayout::new(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
    );
    let (root, _) = checkpoint_heads(&layout, &root).await;
    let mut value = serde_json::to_value(root).unwrap();
    value["repository_id"] = "2".repeat(64).into();
    let root = RootRecord::encode(serde_json::from_value(value).unwrap()).unwrap();
    store.reads.lock().unwrap().clear();
    capture_ref_heads(&layout, root.root()).await.unwrap();
    let count: usize = store
        .reads
        .lock()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.starts_with(layout.capsule_ref_heads_prefix().as_ref()))
        .map(|(_, count)| count)
        .sum();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn checkpoint_cache_drops_deleted_heads_and_reloads_changed_heads() {
    for deleted in [false, true] {
        let (layout, root, store) = fixture(Identity::ETag, false, false).await;
        store.replacements.lock().unwrap().clear();
        let layout = StoreLayout::new(
            layout.store().clone().with_target_identity([1; 32]),
            layout.repo_prefix().to_owned(),
        );
        let (root, _) = checkpoint_heads(&layout, &root).await;
        let path = layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/changed"));
        if deleted {
            store.inner.delete(&path).await.unwrap();
        } else {
            let retired =
                CapsuleRefHead::from_root("refs/heads/changed", "2".repeat(64), None, None)
                    .unwrap();
            layout
                .store()
                .put_overwrite(&path, retired.encode().unwrap())
                .await
                .unwrap();
        }
        store.reads.lock().unwrap().clear();
        let (heads, _) = capture_ref_heads(&layout, &root).await.unwrap();
        let reads = store.reads.lock().unwrap();
        let unchanged = layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/unchanged"));
        assert_eq!(
            (
                heads
                    .iter()
                    .map(CapsuleRefHead::ref_name)
                    .collect::<Vec<_>>(),
                reads.get(&path.to_string()).copied().unwrap_or(0),
                reads.get(&unchanged.to_string()).copied().unwrap_or(0)
            ),
            (vec!["refs/heads/unchanged"], usize::from(!deleted), 0),
        );
    }
}

#[tokio::test]
async fn checkpoint_cache_missing_or_corrupt_selected_bytes_are_errors() {
    let (layout, root, store) = fixture(Identity::ETag, false, false).await;
    store.replacements.lock().unwrap().clear();
    let layout = StoreLayout::new(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
    );
    let (root, checkpoint) = checkpoint_heads(&layout, &root).await;
    let path = layout.capsule_checkpoint_path(checkpoint.hash());
    let cache = root.checkpoint().unwrap().ref_heads.as_ref().unwrap();
    let mut damaged = checkpoint.bytes().to_vec();
    damaged[cache.range.offset() as usize] ^= 1;
    layout
        .store()
        .put_overwrite(&path, Bytes::from(damaged))
        .await
        .unwrap();
    assert!(capture_ref_heads(&layout, &root).await.is_err());
    store.inner.delete(&path).await.unwrap();
    assert!(capture_ref_heads(&layout, &root).await.is_err());
}

#[tokio::test]
async fn checkpoint_cache_resolves_activation_changes_without_ref_body_reads() {
    let (layout, root, store, preparing, oid) = prepared_head_fixture().await;
    store.replacements.lock().unwrap().clear();
    let layout = StoreLayout::new(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
    );
    let (root, _) = checkpoint_heads(&layout, &root).await;
    let record_path = layout.capsule_transaction_path(preparing.activation_id());
    layout
        .store()
        .put_overwrite(&record_path, preparing.commit().unwrap().encode().unwrap())
        .await
        .unwrap();
    store.reads.lock().unwrap().clear();
    let (heads, active) = capture_ref_heads(&layout, &root).await.unwrap();
    let visible = heads
        .iter()
        .find(|head| head.ref_name() == "refs/heads/unchanged")
        .unwrap()
        .visible(&active);
    let head_path = layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/unchanged"));
    let reads = store.reads.lock().unwrap();
    assert_eq!(
        (
            visible.oid(),
            reads.get(&head_path.to_string()).copied().unwrap_or(0),
            reads[&record_path.to_string()]
        ),
        (Some(oid.as_str()), 0, 1),
    );
}

#[tokio::test]
async fn checkpoint_cache_still_rejects_missing_activation_records() {
    let (layout, root, store, preparing, _) = prepared_head_fixture().await;
    store.replacements.lock().unwrap().clear();
    let layout = StoreLayout::new(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
    );
    let (root, _) = checkpoint_heads(&layout, &root).await;
    store
        .inner
        .delete(&layout.capsule_transaction_path(preparing.activation_id()))
        .await
        .unwrap();
    assert!(capture_ref_heads(&layout, &root).await.is_err());
}

#[tokio::test]
async fn restored_epoch_does_not_use_cached_heads_or_require_retired_activations() {
    let (layout, root, store, preparing, _) = prepared_head_fixture().await;
    store.replacements.lock().unwrap().clear();
    let layout = StoreLayout::new(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
    );
    let (root, _) = checkpoint_heads(&layout, &root).await;
    let record = RootRecord::encode(root).unwrap();
    let retired = record
        .root()
        .begin_restore(
            record.digest(),
            crate::capsule_protocol::GcFence::new("6".repeat(64), 1).unwrap(),
            "7".repeat(64),
        )
        .unwrap();
    store
        .inner
        .delete(&layout.capsule_transaction_path(preparing.activation_id()))
        .await
        .unwrap();
    assert_eq!(
        capture_ref_heads(&layout, &retired).await.unwrap(),
        (Vec::new(), BTreeSet::new())
    );
}
