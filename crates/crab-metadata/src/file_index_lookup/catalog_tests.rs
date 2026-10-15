use super::*;
use crate::capsule_protocol::{
    CapsuleGitPack, CheckpointPointer, FileCatalogEntry, LayeredCheckpoint, PackLayer,
    PointerCatalog, PointerCatalogLookup, RepositoryRoot, RootRecord, ShardCatalogEntry,
    XorbCatalogEntry, XorbChunkEntry,
};
use bytes::Bytes;
use object_store::memory::InMemory;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;

pub(super) async fn capture(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    catalog: PointerCatalog,
    maximum: u64,
) -> (PointerCatalogLookup, object_store::path::Path, RootRecord) {
    let pack = CapsuleGitPack::new(
        Bytes::from_static(b"pack"),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "1".repeat(40),
        1,
    )
    .unwrap();
    let source = PackLayer::build(&pack)
        .unwrap()
        .source_descriptor()
        .unwrap();
    let initial =
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap();
    let transaction = crate::capsule_protocol::CapsuleTransaction::new(
        initial.digest(),
        vec![crate::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("1".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let run = crate::capsule_protocol::CapsuleRun::leaf(
        crate::capsule_protocol::Capsule::build(&transaction, vec![], vec![]).unwrap(),
    )
    .unwrap();
    let run_pointer = crate::capsule_protocol::CapsulePointer::new(
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
    layout
        .store()
        .put_overwrite(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
    let base = RootRecord::encode(
        initial
            .root()
            .advance(
                initial.digest(),
                BTreeMap::from([("refs/heads/main".to_owned(), "1".repeat(40))]),
                BTreeMap::new(),
                vec![run_pointer],
                transaction.id().unwrap().as_str(),
            )
            .unwrap(),
    )
    .unwrap();
    let checkpoint = LayeredCheckpoint::build(
        base.root().generation(),
        base.digest(),
        vec![source],
        catalog,
        None,
    )
    .unwrap();
    let pointer = CheckpointPointer::new_layered(
        checkpoint.hash(),
        checkpoint.bytes().len() as u64,
        checkpoint.control_offset(),
        checkpoint.control_size(),
        checkpoint.footer_hash(),
        checkpoint.covered_generation(),
        checkpoint.covered_root_digest(),
        checkpoint.pack_count().unwrap(),
        checkpoint.object_count().unwrap(),
    )
    .unwrap();
    let history = crate::capsule_protocol::HistorySegment::build(
        pointer.clone(),
        None,
        crate::capsule_protocol::HistorySegmentState::new(
            base.root().refs().clone(),
            base.root().peeled_refs().clone(),
            base.root().head().to_owned(),
            base.root().compacted_ref_transactions().clone(),
            base.root().capsule_frontier().to_vec(),
        ),
    )
    .unwrap();
    layout
        .store()
        .put_overwrite(
            &layout.capsule_history_segment_path(history.hash()),
            history.bytes().clone(),
        )
        .await
        .unwrap();
    let root = RootRecord::encode(
        base.root()
            .install_checkpoint(base.digest(), pointer, Some(history.pointer().unwrap()))
            .unwrap(),
    )
    .unwrap();
    layout
        .store()
        .put_overwrite(&layout.capsule_root_path(), root.bytes().clone())
        .await
        .unwrap();
    let path = layout.capsule_checkpoint_path(checkpoint.hash());
    layout
        .store()
        .put_overwrite(&path, checkpoint.bytes().clone())
        .await
        .unwrap();
    let control = LayeredCheckpoint::decode_control(
        checkpoint
            .bytes()
            .slice(checkpoint.control_offset() as usize..),
        checkpoint.bytes().len() as u64,
        checkpoint.control_offset(),
        checkpoint.hash(),
        checkpoint.footer_hash(),
    )
    .unwrap();
    (
        control.pointer_catalog_lookup(layout, maximum).unwrap(),
        path,
        root,
    )
}

pub(super) fn catalog(count: usize) -> PointerCatalog {
    let mut result = PointerCatalog::new();
    for index in 0..count {
        let file = super::tests::hash_from_seed(index as u64);
        let shard = super::tests::hash_from_seed(1_000_000 + index as u64);
        let xorb = super::tests::hash_from_seed(2_000_000 + index as u64);
        result
            .insert_file(file.hex(), FileCatalogEntry::new(16, shard.hex()))
            .unwrap();
        result
            .insert_shard(shard.hex(), ShardCatalogEntry::new(80, vec![xorb.hex()]))
            .unwrap();
        result
            .insert_xorb(
                xorb.hex(),
                XorbCatalogEntry::new(
                    99,
                    "e".repeat(64),
                    vec![XorbChunkEntry::new(
                        super::tests::hash_from_seed(3_000_000 + index as u64).hex(),
                        16,
                    )],
                ),
            )
            .unwrap();
    }
    result
}

#[tokio::test]
async fn pinned_batches_preserve_order_misses_and_page_bounds_as_history_grows() {
    for count in [100, 20_000] {
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let storage = crab_storage::Store::new(Arc::new(InMemory::new()))
            .with_read_request_observer(Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
            }));
        let layout = crab_storage::StoreLayout::new(storage, "pinned-lookup".to_owned());
        let (captured, _, _) = capture(&layout, catalog(count), 512 * 1024).await;
        let lookup = SharedFileIndexLookup::for_pointer_catalog(layout.clone(), captured);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "construction must leave pages cold"
        );
        // A later authenticated catalog must not change an existing capture.
        capture(&layout, catalog(count + 1), 512 * 1024).await;
        let file = super::tests::hash_from_seed(42);
        let later = super::tests::hash_from_seed(count as u64);
        let shard = super::tests::hash_from_seed(1_000_042);
        assert_eq!(
            lookup.lookup_batch(&[file, later, file]).await.unwrap(),
            vec![Some(shard), None, Some(shard)]
        );
        let cold = reads.load(Ordering::SeqCst);
        assert!(
            cold <= 24,
            "fixed file lookup exceeded its page bound: {cold}"
        );
        assert_eq!(lookup.lookup(&file).await.unwrap(), Some(shard));
        assert_eq!(
            reads.load(Ordering::SeqCst),
            cold,
            "warm lookup reread pages"
        );
        let clone = lookup.clone();
        lookup.close().await.unwrap();
        assert!(clone.lookup(&file).await.is_err());
    }
}

#[tokio::test]
async fn missing_captured_pages_are_errors_even_when_a_new_catalog_exists() {
    let layout = crab_storage::StoreLayout::new(
        crab_storage::Store::new(Arc::new(InMemory::new())),
        "missing-page".to_owned(),
    );
    let (captured, path, _) = capture(&layout, catalog(100), 512 * 1024).await;
    let (original, _) = layout
        .store()
        .get_with_etag_bounded(&path, 512 * 1024)
        .await
        .unwrap();
    capture(&layout, catalog(101), 512 * 1024).await;
    layout.store().delete(&path).await.unwrap();
    let lookup = SharedFileIndexLookup::for_pointer_catalog(layout.clone(), captured);
    assert!(
        lookup
            .lookup(&super::tests::hash_from_seed(42))
            .await
            .is_err()
    );
    layout.store().put_overwrite(&path, original).await.unwrap();
    assert_eq!(
        lookup
            .lookup(&super::tests::hash_from_seed(42))
            .await
            .unwrap(),
        Some(super::tests::hash_from_seed(1_000_042)),
        "failed closure reads must not cache absence"
    );
    lookup.close().await.unwrap();
}

#[tokio::test]
async fn current_state_opener_keeps_catalog_pages_selected_and_pins_its_first_root() {
    use std::sync::atomic::AtomicU64;
    for count in [100, 20_000] {
        let reads = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let observed_reads = reads.clone();
        let observed_bytes = bytes.clone();
        let storage = crab_storage::Store::new(Arc::new(InMemory::new()))
            .with_read_request_observer(Arc::new(move |_| {
                observed_reads.fetch_add(1, Ordering::SeqCst);
            }))
            .with_read_byte_observer(Arc::new(move |size| {
                observed_bytes.fetch_add(size, Ordering::SeqCst);
            }));
        let layout = crab_storage::StoreLayout::new(storage.clone(), "current-catalog".to_owned());
        let (_, _, root) = capture(&layout, catalog(count), 512 * 1024).await;
        assert_eq!(root.root().generation(), 1);
        let lookup = SharedFileIndexLookup::new_for_storage(&storage, layout.repo_prefix());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        let file = super::tests::hash_from_seed(42);
        let shard = super::tests::hash_from_seed(1_000_042);
        assert_eq!(
            lookup.lookup_batch(&[file, file]).await.unwrap(),
            vec![Some(shard), Some(shard)]
        );
        assert!(
            reads.load(Ordering::SeqCst) <= 32 && bytes.load(Ordering::SeqCst) <= 512 * 1024,
            "current-state fixed query materialized unrelated catalog history: count={count}, reads={}, bytes={}",
            reads.load(Ordering::SeqCst),
            bytes.load(Ordering::SeqCst)
        );
        let (_, _, later_root) = capture(&layout, catalog(count + 1), 512 * 1024).await;
        layout
            .store()
            .put_overwrite(&layout.capsule_root_path(), later_root.bytes().clone())
            .await
            .unwrap();
        let later_file = super::tests::hash_from_seed(count as u64);
        assert_eq!(lookup.lookup(&later_file).await.unwrap(), None);
        let latest = SharedFileIndexLookup::new_for_storage(&storage, layout.repo_prefix());
        assert_eq!(
            latest.lookup(&later_file).await.unwrap(),
            Some(super::tests::hash_from_seed(1_000_000 + count as u64))
        );
        lookup.close().await.unwrap();
        latest.close().await.unwrap();
    }
}

#[tokio::test]
async fn repeated_large_recipe_binding_queries_reuse_verified_results() {
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let storage = crab_storage::Store::new(Arc::new(InMemory::new())).with_read_request_observer(
        Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let layout = crab_storage::StoreLayout::new(storage, "large-recipe-binding".to_owned());
    let file = super::tests::hash_from_seed(42);
    let shard = super::tests::hash_from_seed(1_000_042);
    let mut catalog = PointerCatalog::new();
    let mut xorbs = Vec::new();
    for index in 0..160_u64 {
        let xorb = super::tests::hash_from_seed(2_000_000 + index).hex();
        let chunks = (0..1024)
            .map(|offset| {
                XorbChunkEntry::new(
                    super::tests::hash_from_seed(3_000_000 + index * 1024 + offset).hex(),
                    64 * 1024,
                )
            })
            .collect();
        catalog
            .insert_xorb(
                xorb.clone(),
                XorbCatalogEntry::new(64 * 1024 * 1024, "e".repeat(64), chunks),
            )
            .unwrap();
        xorbs.push(xorb);
    }
    catalog
        .insert_shard(shard.hex(), ShardCatalogEntry::new(80, xorbs))
        .unwrap();
    // This is metadata for a 10 GiB recipe, not a payload reconstruction fixture.
    // Its verified closure outgrows the ordinary decoded-page cache.
    catalog
        .insert_file(
            file.hex(),
            FileCatalogEntry::new(10 * 1024 * 1024 * 1024, shard.hex()),
        )
        .unwrap();
    let (captured, _, _) = capture(&layout, catalog, 16 * 1024 * 1024).await;
    let lookup = SharedFileIndexLookup::for_pointer_catalog(layout, captured);
    let cold = lookup.lookup(&file).await;
    let cold_reads = reads.load(Ordering::SeqCst);
    let warm = lookup.lookup_batch(&[file, file]).await;
    let warm_reads = reads.load(Ordering::SeqCst);
    lookup.close().await.unwrap();
    assert_eq!(
        (cold.unwrap(), warm.unwrap()),
        (Some(shard), vec![Some(shard), Some(shard)])
    );
    assert!(cold_reads > 0);
    assert_eq!(
        warm_reads, cold_reads,
        "verified immutable bindings must not re-read the large recipe"
    );
}

#[tokio::test]
async fn cached_binding_batches_keep_the_original_input_admission() {
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let storage = crab_storage::Store::new(Arc::new(InMemory::new())).with_read_request_observer(
        Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let layout = crab_storage::StoreLayout::new(storage, "cached-admission".to_owned());
    let file = super::tests::hash_from_seed(42);
    let (captured, _, _) = capture(&layout, catalog(100), 512 * 1024).await;
    let queries = vec![file; 10_000];
    let keys = queries.iter().map(MerkleHash::hex).collect::<Vec<_>>();
    assert!(captured.files(layout.store(), &keys).await.is_err());
    let lookup = SharedFileIndexLookup::for_pointer_catalog(layout, captured);
    lookup.lookup(&file).await.unwrap().unwrap();
    let cold_reads = reads.load(Ordering::SeqCst);
    let result = lookup.lookup_batch(&queries).await;
    lookup.close().await.unwrap();
    assert!(matches!(result, Err(MetadataError::CorruptObject { .. })));
    assert_eq!(
        reads.load(Ordering::SeqCst),
        cold_reads,
        "cached input rejection must precede I/O"
    );
}
