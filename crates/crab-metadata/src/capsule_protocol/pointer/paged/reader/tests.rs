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
    checkpoint.publish_catalog(&layout).await.unwrap();
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
    reads.store(0, Ordering::SeqCst);
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
async fn mixed_dependency_queries_keep_unrelated_history_cold() {
    for count in [100, 20_000] {
        let full = catalog(count);
        let mut expected = PointerCatalog::new();
        let file = format!("{:064x}", 42);
        expected
            .insert_file(file.clone(), full.files[&file].clone())
            .unwrap();
        for index in [7, 42] {
            let hash = format!("{:064x}", 3_000_000 + index);
            expected
                .insert_shard(hash.clone(), full.shards[&hash].clone())
                .unwrap();
        }
        for index in [7, 9, 42] {
            let hash = format!("{:064x}", 1_000_000 + index);
            expected
                .insert_xorb(hash.clone(), full.xorbs[&hash].clone())
                .unwrap();
        }
        let (layout, _, lookup, reads) = fixture(full, 512 * 1024).await;
        let files = [file.clone(), file];
        let shards = [format!("{:064x}", 3_000_007), "f".repeat(64)];
        let xorbs = [format!("{:064x}", 1_000_009)];
        let selected = lookup
            .dependencies(layout.store(), &files, &shards, &xorbs)
            .await
            .unwrap();
        assert_eq!(selected, expected);
        let cold = reads.load(Ordering::SeqCst);
        assert!(cold <= 24, "fixed mixed query exceeded its page bound");
        lookup
            .dependencies(layout.store(), &files, &shards, &xorbs)
            .await
            .unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), cold);
    }
}

#[tokio::test]
async fn explicit_identity_misses_do_not_hide_missing_file_dependencies() {
    let (layout, _, lookup, _) = fixture(catalog(1), 1024 * 1024).await;
    let missing = ["f".repeat(64)];
    assert!(
        lookup
            .dependencies(layout.store(), &missing, &missing, &missing)
            .await
            .unwrap()
            .is_empty()
    );
    let mut delta = PointerCatalog::new();
    delta
        .insert_file(missing[0].clone(), FileCatalogEntry::new(7, "e".repeat(64)))
        .unwrap();
    let lookup = append_run(&layout, lookup, vec![delta]).await;
    assert!(
        lookup
            .dependencies(layout.store(), &missing, &[], &[])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn explicit_shard_selection_rejects_conflicting_visible_descriptors() {
    let (layout, _, lookup, _) = fixture(catalog(1), 1024 * 1024).await;
    let shard = format!("{:064x}", 3_000_000);
    let mut delta = PointerCatalog::new();
    delta
        .insert_shard(
            shard.clone(),
            ShardCatalogEntry::new(81, vec![format!("{:064x}", 1_000_000)]),
        )
        .unwrap();
    let lookup = append_run(&layout, lookup, vec![delta]).await;
    assert!(
        lookup
            .dependencies(layout.store(), &[], &[shard], &[])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn mixed_query_keys_share_admission_before_origin_reads() {
    let (layout, _, lookup, reads) = fixture(catalog(1), 191).await;
    assert!(
        lookup
            .dependencies(
                layout.store(),
                &[format!("{:064x}", 0)],
                &[format!("{:064x}", 3_000_000)],
                &[],
            )
            .await
            .is_err()
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ordered_runs_override_file_bindings_but_preserve_descriptor_conflicts() {
    let (layout, _, lookup, _) = fixture(catalog(2), 1024 * 1024).await;
    let file = format!("{:064x}", 0);
    let mut delta = PointerCatalog::new();
    let replacement = format!("{:064x}", 3_000_001);
    delta
        .insert_file(file.clone(), FileCatalogEntry::new(7, replacement.clone()))
        .unwrap();
    let lookup = append_run(&layout, lookup, vec![delta]).await;
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
    let lookup = append_run(&layout, lookup, vec![conflict]).await;
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
    let lookup = append_run(&layout, lookup, vec![delta]).await;
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
        let source = lookup.checkpoint.as_ref().unwrap();
        let path = if missing {
            layout.capsule_catalog_path(checkpoint.catalog().unwrap().hash())
        } else {
            source
                .pages
                .path(&source.descriptor.files.as_ref().unwrap().hash)
        };
        if missing {
            layout.store().delete(&path).await.unwrap();
        } else {
            let mut bytes = layout
                .store()
                .get_with_etag(&path)
                .await
                .unwrap()
                .0
                .to_vec();
            bytes[0] ^= 1;
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
    let lookup = append_run(&layout, lookup, vec![delta]).await;
    assert!(
        lookup
            .files(layout.store(), &["f".repeat(64)])
            .await
            .is_err()
    );
    // A re-signed reverse index must still agree with its authenticated xorb.
    let source = lookup.checkpoint.as_ref().unwrap();
    let mut descriptor = source.descriptor.clone();
    let mut value = blake3::Hash::from_hex(format!("{:064x}", 1_000_000))
        .unwrap()
        .as_bytes()
        .to_vec();
    value.extend_from_slice(&1u32.to_be_bytes());
    value.extend_from_slice(&7u32.to_be_bytes());
    descriptor.chunks = source
        .pages
        .edit(
            layout.store(),
            descriptor.chunks.as_ref(),
            Kind::CatalogChunks,
            vec![(
                blake3::Hash::from_hex(format!("{:064x}", 2_000_000))
                    .unwrap()
                    .as_bytes()
                    .to_vec(),
                value,
            )],
            &mut PageWork::new(1024 * 1024, 0),
        )
        .await
        .unwrap();
    let body = descriptor.encode().unwrap();
    let identity = *blake3::hash(&body).as_bytes();
    layout
        .store()
        .put_overwrite(&source.pages.path(&identity), body.clone())
        .await
        .unwrap();
    let pages = CatalogPages::new(
        layout.capsule_catalog_prefix(),
        descriptor,
        identity,
        body.len() as u64,
    )
    .unwrap();
    let lookup = PointerCatalogLookup::checkpoint(Arc::new(pages), 1024 * 1024);
    assert!(
        lookup
            .chunks(layout.store(), &[format!("{:064x}", 2_000_000)])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn query_budget_applies_to_warm_pages_and_run_dependencies() {
    let (layout, _, mut lookup, _) = fixture(catalog(2), 1024 * 1024).await;
    let files = [format!("{:064x}", 0)];
    lookup.files(layout.store(), &files).await.unwrap();
    lookup.maximum = 1;
    assert!(lookup.files(layout.store(), &files).await.is_err());
    let lookup = append_run(&layout, PointerCatalogLookup::empty(1), vec![catalog(2)]).await;
    assert!(lookup.files(layout.store(), &files).await.is_err());
}

async fn run_source(
    layout: &StoreLayout<Store>,
    catalogs: Vec<PointerCatalog>,
) -> (RunCatalogSource, Vec<String>) {
    use crate::capsule_protocol::{
        Capsule, CapsuleRefEdit, CapsuleRun, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
    };
    let mut runs: Vec<_> = catalogs
        .into_iter()
        .enumerate()
        .map(|(position, catalog)| {
            let transaction = CapsuleTransaction::new(
                &"e".repeat(64),
                vec![CapsuleRefEdit::new(
                    format!("refs/tags/{position}"),
                    None,
                    Some("f".repeat(40)),
                    None,
                )],
            )
            .unwrap();
            let capsule = Capsule::build(
                &transaction,
                Vec::new(),
                vec![CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    catalog.encode_delta().unwrap(),
                )],
            )
            .unwrap();
            CapsuleRun::leaf(capsule).unwrap()
        })
        .collect();
    let run = if runs.len() == 1 {
        runs.pop().unwrap()
    } else {
        CapsuleRun::compact(runs).unwrap()
    };
    layout
        .store()
        .put_overwrite(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
    let order = run
        .capsules()
        .iter()
        .map(|capsule| capsule.hash().to_owned())
        .collect();
    let source = run.catalog_source(layout).unwrap().unwrap();
    (source, order)
}

#[tokio::test]
async fn selected_run_queries_keep_unselected_conflicts_out_of_the_view() {
    let (layout, _, lookup, _) = fixture(catalog(2), 1024 * 1024).await;
    let mut conflict = catalog(1);
    conflict
        .xorbs
        .get_mut(&format!("{:064x}", 1_000_000))
        .unwrap()
        .encoded_size = 100;
    let (source, order) = run_source(&layout, vec![catalog(1), conflict]).await;
    let file = format!("{:064x}", 0);
    let lookup = lookup
        .with_run_sources(vec![source.clone()], &order[..1])
        .unwrap();
    assert_eq!(
        lookup
            .files(layout.store(), std::slice::from_ref(&file))
            .await
            .unwrap(),
        catalog(1)
    );
    let lookup = PointerCatalogLookup::empty(1024 * 1024)
        .with_run_sources(vec![source], &order)
        .unwrap();
    assert!(lookup.files(layout.store(), &[file]).await.is_err());
}

#[tokio::test]
async fn selected_run_file_precedence_uses_logical_order_instead_of_physical_position() {
    let (layout, _, lookup, _) = fixture(catalog(2), 1024 * 1024).await;
    let file = format!("{:064x}", 0);
    let mut older = PointerCatalog::new();
    older
        .insert_file(
            file.clone(),
            FileCatalogEntry::new(7, format!("{:064x}", 3_000_000)),
        )
        .unwrap();
    let mut newer = PointerCatalog::new();
    let replacement = format!("{:064x}", 3_000_001);
    newer
        .insert_file(file.clone(), FileCatalogEntry::new(7, replacement.clone()))
        .unwrap();
    let (source, order) = run_source(&layout, vec![newer, older]).await;
    let lookup = lookup
        .with_run_sources(vec![source], &[order[1].clone(), order[0].clone()])
        .unwrap();
    assert_eq!(
        lookup
            .files(layout.store(), std::slice::from_ref(&file))
            .await
            .unwrap()
            .files[&file]
            .shard_hash,
        replacement
    );
}

#[tokio::test]
async fn run_lookup_reads_selected_pages_with_one_budget_and_warm_cache() {
    for count in [100, 20_000] {
        let (layout, _, _, reads) = fixture(PointerCatalog::new(), 512 * 1024).await;
        let expected = catalog(count);
        let (source, order) = run_source(&layout, vec![expected.clone()]).await;
        let lookup = PointerCatalogLookup::empty(512 * 1024)
            .with_run_sources(vec![source], &order)
            .unwrap();
        reads.store(0, Ordering::SeqCst);
        let file = format!("{:064x}", 42);
        let selected = lookup
            .files(layout.store(), std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(selected.files.get(&file), expected.files.get(&file));
        let chunks = lookup
            .chunks(layout.store(), &[format!("{:064x}", 2_000_042)])
            .await
            .unwrap();
        assert_eq!(chunks.xorbs, selected.xorbs);
        let cold = reads.load(Ordering::SeqCst);
        assert!(cold <= 16, "selected run query exceeded its page bound");
        lookup.files(layout.store(), &[file]).await.unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), cold);
    }
}

#[tokio::test]
async fn run_chunks_choose_the_first_occurrence_in_the_lowest_selected_xorb() {
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
    let (source, order) = run_source(&layout, vec![delta]).await;
    let lookup = lookup.with_run_sources(vec![source], &order).unwrap();
    let selected = lookup.chunks(layout.store(), &[chunk]).await.unwrap();
    assert_eq!(
        selected.xorbs.keys().collect::<Vec<_>>(),
        vec![&"0".repeat(64)]
    );
}

async fn append_run(
    layout: &StoreLayout<Store>,
    mut lookup: PointerCatalogLookup,
    catalogs: Vec<PointerCatalog>,
) -> PointerCatalogLookup {
    let (source, additional) = run_source(layout, catalogs).await;
    let mut ranked = std::mem::take(&mut lookup.ranks)
        .into_iter()
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(_, rank)| *rank);
    let mut order = ranked.into_iter().map(|(hash, _)| hash).collect::<Vec<_>>();
    order.extend(additional);
    let mut sources = std::mem::take(&mut lookup.runs);
    sources.push(source);
    lookup.with_run_sources(sources, &order).unwrap()
}

#[tokio::test]
async fn equal_run_variants_reuse_unselected_physical_sources_without_losing_authority() {
    let (layout, _, _, _) = fixture(PointerCatalog::new(), 1024 * 1024).await;
    let (source, order) = run_source(&layout, vec![catalog(2), catalog(3)]).await;
    let lookup = PointerCatalogLookup::empty(1024 * 1024)
        .with_run_sources(vec![source], &order[1..])
        .unwrap();
    let selected = lookup
        .files(layout.store(), &[format!("{:064x}", 0)])
        .await
        .unwrap();
    assert_eq!(selected, catalog(1));
}

#[tokio::test]
async fn query_page_budget_is_shared_across_runs_including_warm_sources() {
    let (layout, _, _, _) = fixture(PointerCatalog::new(), 128 * 1024).await;
    let mut lookup = append_run(
        &layout,
        PointerCatalogLookup::empty(128 * 1024),
        vec![catalog(1000)],
    )
    .await;
    let file = [format!("{:064x}", 42)];
    lookup.files(layout.store(), &file).await.unwrap();
    lookup = append_run(&layout, lookup, vec![catalog(1001)]).await;
    let error = lookup.files(layout.store(), &file).await.unwrap_err();
    assert!(
        matches!(error, crate::error::MetadataError::CorruptObject { reason, .. }
        if reason.contains("byte limit"))
    );
}
