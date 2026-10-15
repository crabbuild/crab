use super::*;
use crate::capsule_protocol::{
    CapsuleGitPack, FileCatalogEntry, LayeredCheckpoint, PackLayer, PointerCatalog,
    PointerCatalogLookup, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
};
use bytes::Bytes;
use object_store::memory::InMemory;
use std::sync::atomic::AtomicUsize;

async fn capture(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    catalog: PointerCatalog,
) -> (PointerCatalogLookup, object_store::path::Path) {
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
    let checkpoint =
        LayeredCheckpoint::build(1, &"3".repeat(64), vec![source], catalog, None).unwrap();
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
        control.pointer_catalog_lookup(layout, 512 * 1024).unwrap(),
        path,
    )
}

fn catalog(count: usize) -> PointerCatalog {
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
        let (captured, _) = capture(&layout, catalog(count)).await;
        let lookup = SharedFileIndexLookup::for_pointer_catalog(layout.clone(), captured);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "construction must leave pages cold"
        );
        // A later authenticated catalog must not change an existing capture.
        capture(&layout, catalog(count + 1)).await;
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
    let (captured, path) = capture(&layout, catalog(100)).await;
    capture(&layout, catalog(101)).await;
    layout.store().delete(&path).await.unwrap();
    let lookup = SharedFileIndexLookup::for_pointer_catalog(layout, captured);
    assert!(
        lookup
            .lookup(&super::tests::hash_from_seed(42))
            .await
            .is_err()
    );
    lookup.close().await.unwrap();
}
