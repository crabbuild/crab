#![expect(clippy::unwrap_used, reason = "test assertions")]
use super::*;
use crate::capsule_protocol::{
    FileCatalogEntry, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
};
use crab_storage::{StorageObservation, StorageObserver, StorageOperation, Store, StoreLayout};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
struct Writes(AtomicU64);
impl StorageObserver for Writes {
    fn started(&self, _: StorageOperation) {}
    fn finished(&self, observation: StorageObservation) {
        self.0
            .fetch_add(observation.bytes_written, Ordering::Relaxed);
    }
}
fn layout() -> (StoreLayout<Store>, Arc<Writes>) {
    let writes = Arc::new(Writes::default());
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(writes.clone());
    (StoreLayout::new(store, "catalog-merge".to_owned()), writes)
}
async fn durable(layout: &StoreLayout<Store>, catalog: &PointerCatalog) -> CheckpointCatalog {
    let (descriptor, objects) = Descriptor::build(catalog).unwrap();
    publish_objects(layout, &objects).await.unwrap();
    CheckpointCatalog::new(descriptor).unwrap()
}
fn catalog(files: std::ops::Range<u32>, shard: u32, xorb: u32) -> PointerCatalog {
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_xorb(
            format!("{xorb:064x}"),
            XorbCatalogEntry::new(
                70,
                "d".repeat(64),
                vec![
                    XorbChunkEntry::new("c".repeat(64), 7),
                    XorbChunkEntry::new("c".repeat(64), 7),
                ],
            ),
        )
        .unwrap();
    catalog
        .insert_shard(
            format!("{shard:064x}"),
            ShardCatalogEntry::new(40, vec![format!("{xorb:064x}")]),
        )
        .unwrap();
    for file in files {
        catalog
            .insert_file(
                format!("{file:064x}"),
                FileCatalogEntry::new(14, format!("{shard:064x}")),
            )
            .unwrap();
    }
    catalog
}
async fn materialize(layout: &StoreLayout<Store>, source: &CheckpointCatalog) -> PointerCatalog {
    source
        .reader(layout)
        .unwrap()
        .full(
            layout.store(),
            &mut crate::pages::PageWork::new(32 * 1024 * 1024, 0),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn union_matches_ordered_catalog_apply_including_reverse_placements() {
    for (old_xorb, new_xorb) in [(1, 9), (9, 1)] {
        let (layout, _) = layout();
        let old = catalog(0..10, 21, old_xorb);
        let new = catalog(5..15, 22, new_xorb);
        let older = durable(&layout, &old).await;
        let newer = durable(&layout, &new).await;
        let result = merge_checkpoint_catalogs(&layout, Some(&older), Some(&newer), 1024 * 1024)
            .await
            .unwrap()
            .unwrap();
        let mut expected = old;
        expected.apply(&new).unwrap();
        assert_eq!(materialize(&layout, &result).await, expected);
    }
}

#[tokio::test]
async fn union_rejects_conflicts_even_when_newer_bindings_win() {
    for conflict in ["file-size", "shard", "shard-xorbs", "xorb", "xorb-chunks"] {
        let (layout, writes) = layout();
        let old = catalog(0..2, 21, 1);
        let mut new = PointerCatalog::new();
        for (identity, entry) in old.files() {
            let size = entry.size() + u64::from(conflict == "file-size");
            new.insert_file(
                identity.clone(),
                FileCatalogEntry::new(size, entry.shard_hash()),
            )
            .unwrap();
        }
        for (identity, entry) in old.shards() {
            new.insert_shard(
                identity.clone(),
                ShardCatalogEntry::new(
                    entry.encoded_size() + u64::from(conflict == "shard"),
                    if conflict == "shard-xorbs" {
                        vec![format!("{:064x}", 2)]
                    } else {
                        entry.xorb_hashes().to_vec()
                    },
                ),
            )
            .unwrap();
        }
        for (identity, entry) in old.xorbs() {
            new.insert_xorb(
                identity.clone(),
                XorbCatalogEntry::new(
                    entry.encoded_size() + u64::from(conflict == "xorb"),
                    entry.body_digest(),
                    if conflict == "xorb-chunks" {
                        vec![
                            XorbChunkEntry::new("e".repeat(64), 7),
                            XorbChunkEntry::new("e".repeat(64), 7),
                        ]
                    } else {
                        entry.chunks().to_vec()
                    },
                ),
            )
            .unwrap();
        }
        if conflict == "shard-xorbs" {
            new.insert_xorb(
                format!("{:064x}", 2),
                old.xorbs().values().next().unwrap().clone(),
            )
            .unwrap();
        }
        let older = durable(&layout, &old).await;
        let newer = durable(&layout, &new).await;
        writes.0.store(0, Ordering::Relaxed);
        assert!(
            merge_checkpoint_catalogs(&layout, Some(&older), Some(&newer), 1024 * 1024)
                .await
                .is_err(),
            "{conflict}"
        );
        assert_eq!(writes.0.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn shared_catalog_reuses_newer_root_without_writes() {
    let (layout, writes) = layout();
    let old = catalog(0..100_000, 21, 1);
    let older = durable(&layout, &old).await;
    let delta = catalog(99_999..100_001, 22, 2);
    // The extra descriptor is immutable, while an existing file changes shard.
    let newer = fold_checkpoint_catalog(
        &layout,
        Some(&older),
        std::slice::from_ref(&delta),
        32 * 1024 * 1024,
    )
    .await
    .unwrap()
    .unwrap();
    drop(old);
    let mut objects = BTreeMap::new();
    for source in [&older, &newer] {
        objects.extend(
            source
                .reader(&layout)
                .unwrap()
                .inspected_objects(layout.store(), 32 * 1024 * 1024)
                .await
                .unwrap(),
        );
    }
    let maximum = objects.values().sum();
    writes.0.store(0, Ordering::Relaxed);
    let merged = merge_checkpoint_catalogs(&layout, Some(&older), Some(&newer), maximum)
        .await
        .unwrap();
    assert_eq!(merged, Some(newer));
    assert_eq!(writes.0.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn union_admission_covers_both_inputs_before_any_writes() {
    let (layout, writes) = layout();
    let older = durable(&layout, &catalog(0..3000, 21, 1)).await;
    let newer = durable(&layout, &catalog(5000..8000, 22, 2)).await;
    let mut sizes = Vec::new();
    for source in [&older, &newer] {
        sizes.push(
            source
                .reader(&layout)
                .unwrap()
                .inspected_objects(layout.store(), 1024 * 1024)
                .await
                .unwrap()
                .iter()
                .map(|(_, size)| size)
                .sum::<u64>(),
        );
    }
    writes.0.store(0, Ordering::Relaxed);
    assert!(
        merge_checkpoint_catalogs(
            &layout,
            Some(&older),
            Some(&newer),
            *sizes.iter().max().unwrap()
        )
        .await
        .is_err()
    );
    assert_eq!(writes.0.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn newer_bindings_cannot_hide_a_missing_older_descriptor() {
    let (layout, writes) = layout();
    let older = durable(&layout, &catalog(0..2, 21, 1)).await;
    let newer = durable(&layout, &catalog(0..3, 22, 2)).await;
    layout
        .store()
        .delete(&layout.capsule_catalog_path(older.hash()))
        .await
        .unwrap();
    writes.0.store(0, Ordering::Relaxed);
    assert!(
        merge_checkpoint_catalogs(&layout, Some(&older), Some(&newer), 1024 * 1024)
            .await
            .is_err()
    );
    assert_eq!(writes.0.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn union_retains_present_roots_and_empty_descriptors() {
    let (layout, _) = layout();
    let empty = durable(&layout, &PointerCatalog::new()).await;
    for (older, newer) in [
        (None, None),
        (Some(&empty), None),
        (None, Some(&empty)),
        (Some(&empty), Some(&empty)),
    ] {
        assert_eq!(
            merge_checkpoint_catalogs(&layout, older, newer, 4096)
                .await
                .unwrap(),
            newer.or(older).cloned()
        );
    }
}

#[tokio::test]
async fn empty_newer_catalog_reuses_older_columns_without_writes() {
    let (layout, writes) = layout();
    let older = durable(&layout, &catalog(0..3000, 21, 1)).await;
    let newer = durable(&layout, &PointerCatalog::new()).await;
    writes.0.store(0, Ordering::Relaxed);
    assert_eq!(
        merge_checkpoint_catalogs(&layout, Some(&older), Some(&newer), 1024 * 1024)
            .await
            .unwrap(),
        Some(older)
    );
    assert_eq!(writes.0.load(Ordering::Relaxed), 0);
}
