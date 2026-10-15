use super::*;
use crate::capsule_protocol::{CapsuleGitPack, LayeredCheckpoint, PackLayer};
use crab_storage::StoreLayout;
use object_store::memory::InMemory;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn catalog(count: usize) -> PointerCatalog {
    let mut catalog = PointerCatalog::new();
    for index in 0..count {
        let xorb = format!("{:064x}", index + 1_000_000);
        let shard = format!("{:064x}", index + 3_000_000);
        catalog
            .insert_xorb(
                xorb.clone(),
                XorbCatalogEntry::new(
                    99,
                    "e".repeat(64),
                    vec![XorbChunkEntry::new(
                        format!("{:064x}", index + 2_000_000),
                        7,
                    )],
                ),
            )
            .unwrap();
        catalog
            .insert_shard(shard.clone(), ShardCatalogEntry::new(80, vec![xorb]))
            .unwrap();
        catalog
            .insert_file(format!("{index:064x}"), FileCatalogEntry::new(7, shard))
            .unwrap();
    }
    catalog
}

async fn fixture(
    catalog: PointerCatalog,
    maximum: u64,
) -> (
    StoreLayout<Store>,
    LayeredCheckpoint,
    PointerCatalogLookup,
    Arc<AtomicUsize>,
) {
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
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
    let layout = StoreLayout::new(store, "catalog-lookup".to_owned());
    layout
        .store()
        .put_overwrite(
            &layout.capsule_checkpoint_path(checkpoint.hash()),
            checkpoint.bytes().clone(),
        )
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
    let lookup = control.pointer_catalog_lookup(&layout, maximum).unwrap();
    (layout, checkpoint, lookup, reads)
}

#[tokio::test]
async fn fixed_file_and_chunk_queries_read_selected_closure_as_history_grows() {
    for count in [100, 20_000] {
        let (layout, _, lookup, reads) = fixture(catalog(count), 512 * 1024).await;
        let file = format!("{:064x}", 42);
        let selected = lookup
            .files(
                layout.store(),
                &[file.clone(), file.clone(), "f".repeat(64)],
            )
            .await
            .unwrap();
        assert_eq!(
            (
                selected.files.len(),
                selected.shards.len(),
                selected.xorbs.len()
            ),
            (1, 1, 1)
        );
        let chunks = lookup
            .chunks(layout.store(), &[format!("{:064x}", 2_000_042)])
            .await
            .unwrap();
        assert_eq!(chunks.xorbs, selected.xorbs);
        assert!(
            reads.load(Ordering::SeqCst) <= 16,
            "fixed query exceeded page request bound"
        );
        let cold = reads.load(Ordering::SeqCst);
        lookup.files(layout.store(), &[file]).await.unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), cold);
    }
}

#[tokio::test]
async fn ordered_deltas_override_file_bindings_but_preserve_descriptor_conflicts() {
    let (layout, _, lookup, _) = fixture(catalog(2), 1024 * 1024).await;
    let file = format!("{:064x}", 0);
    let mut delta = PointerCatalog::new();
    let replacement = format!("{:064x}", 3_000_001);
    delta
        .insert_file(file.clone(), FileCatalogEntry::new(7, replacement.clone()))
        .unwrap();
    let lookup = lookup.with_deltas(vec![delta]).unwrap();
    assert_eq!(
        lookup
            .files(layout.store(), std::slice::from_ref(&file))
            .await
            .unwrap()
            .files[&file]
            .shard_hash,
        replacement
    );
    let mut conflict = PointerCatalog::new();
    conflict
        .insert_xorb(
            format!("{:064x}", 1_000_001),
            XorbCatalogEntry::new(
                100,
                "e".repeat(64),
                vec![XorbChunkEntry::new(format!("{:064x}", 2_000_001), 7)],
            ),
        )
        .unwrap();
    let lookup = lookup.with_deltas(vec![conflict]).unwrap();
    assert!(
        lookup
            .files(layout.store(), &[format!("{:064x}", 1)])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn chunk_selection_uses_lowest_xorb_then_first_occurrence_across_sources() {
    let (layout, _, lookup, _) = fixture(catalog(1), 1024 * 1024).await;
    let chunk = format!("{:064x}", 2_000_000);
    let mut delta = PointerCatalog::new();
    delta
        .insert_xorb(
            "0".repeat(64),
            XorbCatalogEntry::new(
                88,
                "e".repeat(64),
                vec![
                    XorbChunkEntry::new(chunk.clone(), 7),
                    XorbChunkEntry::new(chunk.clone(), 7),
                ],
            ),
        )
        .unwrap();
    let lookup = lookup.with_deltas(vec![delta]).unwrap();
    let selected = lookup.chunks(layout.store(), &[chunk]).await.unwrap();
    assert_eq!(
        selected.xorbs.keys().collect::<Vec<_>>(),
        vec![&"0".repeat(64)]
    );
}

#[tokio::test]
async fn missing_and_corrupted_pages_are_errors_instead_of_catalog_misses() {
    for missing in [false, true] {
        let (layout, checkpoint, lookup, _) = fixture(catalog(2), 1024 * 1024).await;
        let path = layout.capsule_checkpoint_path(checkpoint.hash());
        if missing {
            layout.store().delete(&path).await.unwrap();
        } else {
            let mut bytes = checkpoint.bytes().to_vec();
            let source = lookup.checkpoint.as_ref().unwrap();
            let offset = source.range.offset() + source.descriptor.files.as_ref().unwrap().offset;
            bytes[offset as usize] ^= 1;
            layout
                .store()
                .put_overwrite(&path, bytes.into())
                .await
                .unwrap();
        }
        assert!(
            lookup
                .files(layout.store(), &[format!("{:064x}", 0)])
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn loaded_dependencies_and_chunk_placements_fail_closed() {
    let (layout, _, lookup, _) = fixture(catalog(1), 1024 * 1024).await;
    let mut delta = PointerCatalog::new();
    delta
        .insert_file("f".repeat(64), FileCatalogEntry::new(7, "d".repeat(64)))
        .unwrap();
    let lookup = lookup.with_deltas(vec![delta]).unwrap();
    assert!(
        lookup
            .files(layout.store(), &["f".repeat(64)])
            .await
            .is_err()
    );
    // A re-signed reverse index must still agree with its authenticated xorb.
    let bytes = catalog(1).encode().unwrap();
    let mut descriptor = Descriptor::decode(&bytes).unwrap();
    let root = descriptor.chunks.as_mut().unwrap();
    let mut body = bytes[..descriptor.body_bytes as usize].to_vec();
    let size = root.offset as usize + 14 + 32 + 4 + 36;
    body[size] ^= 1;
    root.hash =
        *blake3::hash(&body[root.offset as usize..root.offset as usize + root.bytes as usize])
            .as_bytes();
    descriptor.body_hash = *blake3::hash(&body).as_bytes();
    let header = json(&descriptor).unwrap();
    body.extend_from_slice(&header);
    crate::pages::put_u32(&mut body, header.len()).unwrap();
    body.extend_from_slice(MAGIC);
    let descriptor = Descriptor::decode(&body).unwrap();
    let range = PackRange::new(0, &body).unwrap();
    let path = object_store::path::Path::from("resigned-catalog");
    layout
        .store()
        .put_overwrite(&path, body.into())
        .await
        .unwrap();
    let lookup = PointerCatalogLookup::checkpoint(path, range, descriptor, 1024 * 1024).unwrap();
    assert!(
        lookup
            .chunks(layout.store(), &[format!("{:064x}", 2_000_000)])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn query_budget_applies_to_warm_pages_and_materialized_delta_dependencies() {
    let (layout, _, mut lookup, _) = fixture(catalog(2), 1024 * 1024).await;
    let files = [format!("{:064x}", 0)];
    lookup.files(layout.store(), &files).await.unwrap();
    lookup.maximum = 1;
    assert!(lookup.files(layout.store(), &files).await.is_err());
    let lookup = PointerCatalogLookup::empty(1)
        .with_deltas(vec![catalog(2)])
        .unwrap();
    assert!(lookup.files(layout.store(), &files).await.is_err());
}
