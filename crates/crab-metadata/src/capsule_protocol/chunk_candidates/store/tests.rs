use super::*;
use crate::capsule_protocol::{PointerCatalog, ShardCatalogEntry, XorbChunkEntry};
use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Observations(Mutex<Vec<StorageObservation>>);
impl StorageObserver for Observations {
    fn started(&self, _: StorageOperation) {}
    fn finished(&self, value: StorageObservation) {
        self.0.lock().unwrap().push(value);
    }
}

fn fixture(
    count: u32,
    seed: u64,
) -> (
    PointerCatalog,
    HashMap<MerkleHash, OriginReceipt>,
    Vec<MerkleHash>,
) {
    let xorb = MerkleHash::from([seed; 4]);
    let hashes = (0..count)
        .map(|index| MerkleHash::from([u64::from(index), seed, 9, 7]))
        .collect::<Vec<_>>();
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_xorb(
            xorb.hex(),
            XorbCatalogEntry::new(
                80,
                "4".repeat(64),
                hashes
                    .iter()
                    .map(|hash| XorbChunkEntry::new(hash.hex(), 7))
                    .collect(),
            ),
        )
        .unwrap();
    catalog
        .insert_shard("2".repeat(64), ShardCatalogEntry::new(10, vec![xorb.hex()]))
        .unwrap();
    let origin = OriginReceipt::new(
        "canonical-origin".to_owned(),
        format!(".crab/xorbs/{}/{}", &xorb.hex()[..2], xorb.hex()),
        xorb.into(),
        [5; 32],
        80,
        Some("etag".to_owned()),
        None,
    );
    (catalog, HashMap::from([(xorb, origin)]), hashes)
}

fn layout() -> StoreLayout<Store> {
    StoreLayout::new(
        Store::new(Arc::new(object_store::memory::InMemory::new())),
        "source".to_owned(),
    )
}

#[tokio::test]
async fn exact_capsule_hints_survive_head_replacement_without_legacy_metadata() {
    let layout = layout();
    let (catalog, origins, hashes) = fixture(3, 3);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let first = CapsuleChunkIndex::new(&layout)
        .candidates(layout.store(), &hashes, 1024 * 1024)
        .await
        .unwrap();
    let newer = StoreLayout::new(layout.store().clone(), "newer-source".to_owned());
    write_capsule_chunk_candidates(&newer, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let current = CapsuleChunkIndex::new(&layout)
        .candidates(layout.store(), &hashes, 1024 * 1024)
        .await
        .unwrap();
    let exact = first
        .placements
        .values()
        .map(|p| (MerkleHash::from(p.chunk_hash), p.placement_id()))
        .collect::<Vec<_>>();
    let restored = CapsuleChunkIndex::new(&layout)
        .candidates_by_id(layout.store(), &exact, 1024 * 1024)
        .await
        .unwrap();
    assert!(first.placements != current.placements && restored.placements == first.placements);
    assert!(
        layout
            .store()
            .list_prefix(&Path::from(".crab/chunk_index_db"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unchanged_root_missing_or_corrupt_pages_remain_errors() {
    let layout = layout();
    let (catalog, origins, hashes) = fixture(1, 3);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let index = CapsuleChunkIndex::new(&layout);
    let mut work = PageWork::new(1024 * 1024, 0);
    let (root, _) = index
        .load(layout.store(), &mut work)
        .await
        .unwrap()
        .unwrap();
    let path = index.pages.path(&root.records.hash);
    let (body, _) = layout
        .store()
        .get_with_etag_bounded(&path, u64::from(root.records.bytes))
        .await
        .unwrap();
    layout.store().delete(&path).await.unwrap();
    assert!(matches!(
        CapsuleChunkIndex::new(&layout)
            .candidates(layout.store(), &hashes, 1024 * 1024)
            .await,
        Err(MetadataError::Storage {
            source: StorageError::NotFound { .. }
        })
    ));
    let mut damaged = body.to_vec();
    damaged[0] ^= 1;
    layout
        .store()
        .put_overwrite(&path, Bytes::from(damaged))
        .await
        .unwrap();
    assert!(matches!(
        CapsuleChunkIndex::new(&layout)
            .candidates(layout.store(), &hashes, 1024 * 1024)
            .await,
        Err(MetadataError::CorruptObject { .. })
    ));
}

#[tokio::test]
async fn cancellation_after_page_writes_does_not_advertise_partial_hints() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let layout = layout();
    let (catalog, origins, _) = fixture(1, 3);
    let calls = AtomicUsize::new(0);
    let result = write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| {
        calls.fetch_add(1, Ordering::Relaxed) >= 2
    })
    .await;
    assert!(matches!(result, Err(MetadataError::CapsuleHintCancelled)));
    assert!(
        !layout
            .store()
            .list_prefix(&layout.capsule_chunk_pages_prefix())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        layout
            .store()
            .get_with_etag(&layout.capsule_chunk_index_path())
            .await,
        Err(StorageError::NotFound { .. })
    ));
}

#[tokio::test]
async fn hint_reads_and_selected_writes_do_not_follow_unrelated_history() {
    for count in [100, 10_000, 100_000] {
        let observations = Arc::new(Observations::default());
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new()))
                .with_storage_observer(observations.clone()),
            "source".to_owned(),
        );
        let (catalog, origins, _) = fixture(count, 3);
        write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
            .await
            .unwrap();
        let (delta, origins, hashes) = fixture(1, 5);
        observations.0.lock().unwrap().clear();
        write_capsule_chunk_candidates(&layout, &delta, &origins, &|| false)
            .await
            .unwrap();
        let writes = observations.0.lock().unwrap().drain(..).collect::<Vec<_>>();
        let read = writes.iter().map(|o| o.bytes_read).sum::<u64>();
        let written = writes.iter().map(|o| o.bytes_written).sum::<u64>();
        assert!(
            read <= 512 * 1024 && written <= 512 * 1024,
            "{count}: read {read}, written {written}"
        );
        let result = CapsuleChunkIndex::new(&layout)
            .candidates(layout.store(), &hashes, 256 * 1024)
            .await
            .unwrap();
        assert_eq!(result.placements.len(), 1);
        let operations = observations.0.lock().unwrap();
        assert!(!operations.iter().any(|o| matches!(
            o.operation,
            StorageOperation::Put | StorageOperation::Delete | StorageOperation::MultipartStart
        )));
    }
}

mod races;
