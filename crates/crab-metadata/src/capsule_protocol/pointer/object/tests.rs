use super::*;
use crate::pages::PageWork;
use crab_storage::{StorageObservation, StorageObserver, Store};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Observations(Mutex<Vec<StorageObservation>>);
impl StorageObserver for Observations {
    fn started(&self, _: crab_storage::StorageOperation) {}
    fn finished(&self, observation: StorageObservation) {
        self.0.lock().unwrap().push(observation);
    }
}

async fn fixture(catalog: &PointerCatalog) -> (CatalogPages, Store, Arc<Observations>) {
    let observations = Arc::new(Observations::default());
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(observations.clone());
    let prefix = object_store::path::Path::from("catalog");
    let (descriptor, objects) = Descriptor::build(catalog).unwrap();
    let root = descriptor.encode().unwrap();
    let identity = *blake3::hash(&root).as_bytes();
    let pages = CatalogPages::new(prefix, descriptor, identity, root.len() as u64).unwrap();
    for (hash, body) in objects {
        store
            .put_if_absent_verified(&pages.pages.path(&hash), body)
            .await
            .unwrap();
    }
    observations.0.lock().unwrap().clear();
    (pages, store, observations)
}

fn catalog(files: u32) -> PointerCatalog {
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_shard("a".repeat(64), ShardCatalogEntry::new(1, Vec::new()))
        .unwrap();
    for index in 0..files {
        catalog
            .insert_file(
                format!("{index:064x}"),
                FileCatalogEntry::new(1, "a".repeat(64)),
            )
            .unwrap();
    }
    catalog
}

#[tokio::test]
async fn fixed_binding_update_reuses_unrelated_catalog_pages() {
    for count in [100, 10_000, 100_000] {
        let expected = catalog(count);
        let (pages, store, observations) = fixture(&expected).await;
        let mut delta = PointerCatalog::new();
        delta
            .insert_file(
                format!("{count:064x}"),
                FileCatalogEntry::new(1, "a".repeat(64)),
            )
            .unwrap();
        let descriptor = pages
            .apply(&store, &[delta.clone()], &mut PageWork::new(256 * 1024, 0))
            .await
            .unwrap();
        let io = observations.0.lock().unwrap().clone();
        assert!(io.iter().map(|op| op.bytes_read).sum::<u64>() <= 256 * 1024);
        assert!(io.iter().map(|op| op.bytes_written).sum::<u64>() <= 256 * 1024);
        assert_eq!(
            pages
                .full(&store, &mut PageWork::new(64 * 1024 * 1024, 0))
                .await
                .unwrap(),
            expected
        );
        let root = descriptor.encode().unwrap();
        let identity = *blake3::hash(&root).as_bytes();
        store
            .put_if_absent_verified(&pages.pages.path(&identity), root.clone())
            .await
            .unwrap();
        let next = CatalogPages::new(
            object_store::path::Path::from("catalog"),
            descriptor,
            identity,
            root.len() as u64,
        )
        .unwrap();
        let mut expected = expected;
        expected.apply(&delta).unwrap();
        assert_eq!(
            next.full(&store, &mut PageWork::new(64 * 1024 * 1024, 0))
                .await
                .unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn ordered_deltas_resolve_later_dependencies_and_preserve_immutable_contracts() {
    let (pages, store, _) = fixture(&catalog(1)).await;
    let mut file = PointerCatalog::new();
    file.insert_file("1".repeat(64), FileCatalogEntry::new(7, "b".repeat(64)))
        .unwrap();
    let mut shard = PointerCatalog::new();
    shard
        .insert_shard("b".repeat(64), ShardCatalogEntry::new(2, Vec::new()))
        .unwrap();
    assert!(
        pages
            .apply(&store, &[file.clone()], &mut PageWork::new(1024 * 1024, 0))
            .await
            .is_err()
    );
    let next = pages
        .apply(&store, &[file, shard], &mut PageWork::new(1024 * 1024, 0))
        .await
        .unwrap();
    assert!(next.files.is_some() && next.shards.is_some());
    let mut conflict = PointerCatalog::new();
    conflict
        .insert_shard("a".repeat(64), ShardCatalogEntry::new(99, Vec::new()))
        .unwrap();
    assert!(
        pages
            .apply(&store, &[conflict], &mut PageWork::new(1024 * 1024, 0))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn new_lower_xorb_updates_shared_chunk_precedence() {
    let chunk = "c".repeat(64);
    let mut base = PointerCatalog::new();
    base.insert_xorb(
        "9".repeat(64),
        XorbCatalogEntry::new(
            80,
            "d".repeat(64),
            vec![
                XorbChunkEntry::new(chunk.clone(), 7),
                XorbChunkEntry::new(chunk.clone(), 7),
            ],
        ),
    )
    .unwrap();
    let (pages, store, _) = fixture(&base).await;
    let mut delta = PointerCatalog::new();
    delta
        .insert_xorb(
            "1".repeat(64),
            XorbCatalogEntry::new(
                90,
                "e".repeat(64),
                vec![XorbChunkEntry::new(chunk.clone(), 7)],
            ),
        )
        .unwrap();
    let descriptor = pages
        .apply(&store, &[delta.clone()], &mut PageWork::new(1024 * 1024, 0))
        .await
        .unwrap();
    let root = descriptor.encode().unwrap();
    let identity = *blake3::hash(&root).as_bytes();
    store
        .put_if_absent_verified(&pages.pages.path(&identity), root.clone())
        .await
        .unwrap();
    let next = CatalogPages::new(
        object_store::path::Path::from("catalog"),
        descriptor,
        identity,
        root.len() as u64,
    )
    .unwrap();
    base.apply(&delta).unwrap();
    assert_eq!(
        next.full(&store, &mut PageWork::new(1024 * 1024, 0))
            .await
            .unwrap(),
        base
    );
}

#[tokio::test]
async fn reused_nested_pages_cannot_amplify_materialization_past_budget() {
    let mut catalog = PointerCatalog::new();
    for identity in 1..=16 {
        catalog
            .insert_xorb(
                format!("{identity:064x}"),
                XorbCatalogEntry::new(
                    80,
                    "d".repeat(64),
                    (0..1000)
                        .map(|_| XorbChunkEntry::new("c".repeat(64), 7))
                        .collect(),
                ),
            )
            .unwrap();
    }
    let (pages, store, _) = fixture(&catalog).await;
    let mut work = PageWork::new(256 * 1024, 0);
    let result = pages.full(&store, &mut work).await;
    assert!(
        result.is_err() && work.bytes <= 256 * 1024,
        "shared physical pages must not grant unbounded materialization"
    );
}

#[tokio::test]
async fn delta_row_admission_precedes_origin_reads_and_recipe_clones() {
    let (pages, store, observations) = fixture(&catalog(1)).await;
    let mut delta = PointerCatalog::new();
    for index in 0..1000 {
        delta
            .insert_shard(format!("{index:064x}"), ShardCatalogEntry::new(1, vec![]))
            .unwrap();
    }
    let result = pages
        .apply(&store, &[delta], &mut PageWork::new(4096, 0))
        .await;
    assert!(result.is_err() && observations.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn superseded_reverse_placement_still_validates_prior_dependency() {
    let chunk = "c".repeat(64);
    let mut base = PointerCatalog::new();
    base.insert_xorb(
        "9".repeat(64),
        XorbCatalogEntry::new(
            80,
            "d".repeat(64),
            vec![XorbChunkEntry::new(chunk.clone(), 7)],
        ),
    )
    .unwrap();
    let (pages, store, _) = fixture(&base).await;
    let mut descriptor = pages.descriptor.clone();
    let mut placement = hash(&"f".repeat(64)).unwrap().to_vec();
    placement.extend_from_slice(&0u32.to_be_bytes());
    placement.extend_from_slice(&7u32.to_be_bytes());
    descriptor.chunks = pages
        .pages
        .edit(
            &store,
            descriptor.chunks.as_ref(),
            Kind::CatalogChunks,
            vec![(hash(&chunk).unwrap().to_vec(), placement)],
            &mut PageWork::new(1024 * 1024, 0),
        )
        .await
        .unwrap();
    let root = descriptor.encode().unwrap();
    let identity = *blake3::hash(&root).as_bytes();
    store
        .put_if_absent_verified(&pages.pages.path(&identity), root.clone())
        .await
        .unwrap();
    let forged = CatalogPages::new(
        object_store::path::Path::from("catalog"),
        descriptor,
        identity,
        root.len() as u64,
    )
    .unwrap();
    let mut delta = PointerCatalog::new();
    delta
        .insert_xorb(
            "1".repeat(64),
            XorbCatalogEntry::new(90, "e".repeat(64), vec![XorbChunkEntry::new(chunk, 7)]),
        )
        .unwrap();
    assert!(
        forged
            .apply(&store, &[delta], &mut PageWork::new(1024 * 1024, 0))
            .await
            .is_err()
    );
}
