#![expect(clippy::expect_used, clippy::unwrap_used, reason = "test assertions")]
use super::*;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
    FileCatalogEntry, PointerCatalog, ShardCatalogEntry,
};
use object_store::memory::InMemory;

#[tokio::test]
async fn capsule_file_index_verification_does_not_create_legacy_metadata() {
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let storage = crab_storage::Store::new(Arc::clone(&inner));
    let layout = crab_storage::StoreLayout::new(storage, "org/v2-recover".to_owned());
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .expect("initialize capsule root");

    let verified = verify_capsule_file_index(
        &layout,
        root,
        &[(MerkleHash::from([1; 32]), MerkleHash::from([2; 32]))],
    )
    .await
    .expect("verify capsule catalog");

    assert_eq!(verified, vec![false]);
    let legacy_objects = inner
        .list(Some(&ObjectPath::from("org/v2-recover/file_index_db")))
        .try_collect::<Vec<_>>()
        .await
        .expect("list legacy metadata prefix");
    assert!(legacy_objects.is_empty());
}

async fn fixture(
    missing_shard: bool,
    unrelated: u32,
    broken_unrelated: bool,
) -> (
    crab_storage::StoreLayout<crab_storage::Store>,
    crab_metadata::capsule_protocol::RootSnapshot,
) {
    let layout = crab_storage::StoreLayout::new(
        crab_storage::Store::new(Arc::new(InMemory::new())),
        "selected-recovery".to_owned(),
    );
    let base =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_file(
            MerkleHash::from([1; 32]).hex(),
            FileCatalogEntry::new(42, MerkleHash::from([2; 32]).hex()),
        )
        .unwrap();
    if !missing_shard {
        catalog
            .insert_shard(
                MerkleHash::from([2; 32]).hex(),
                ShardCatalogEntry::new(7, Vec::new()),
            )
            .unwrap();
    }
    for index in 0..unrelated {
        catalog
            .insert_file(
                format!("{index:064x}"),
                FileCatalogEntry::new(42, MerkleHash::from([2; 32]).hex()),
            )
            .unwrap();
    }
    if broken_unrelated {
        catalog
            .insert_file(
                MerkleHash::from([5; 32]).hex(),
                FileCatalogEntry::new(42, MerkleHash::from([6; 32]).hex()),
            )
            .unwrap();
    }
    let transaction = CapsuleTransaction::new(
        base.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("2".repeat(40)),
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
    crab_write::capsule_protocol::publish(&layout, base, &transaction, &capsule)
        .await
        .unwrap();
    let root = crab_metadata::capsule_protocol::load_root(&layout)
        .await
        .unwrap();
    (layout, root)
}

#[tokio::test]
async fn selected_mapping_verification_preserves_order_and_duplicates_across_batches() {
    let (layout, root) = fixture(false, 0, false).await;
    let cases = [([1; 32], [2; 32]), ([1; 32], [3; 32]), ([4; 32], [2; 32])];
    let entries = (0..129)
        .map(|index| {
            let (file, shard) = cases[index % cases.len()];
            (MerkleHash::from(file), MerkleHash::from(shard))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        verify_capsule_file_index(&layout, root, &entries)
            .await
            .unwrap(),
        (0..129).map(|index| index % 3 == 0).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn selected_mapping_missing_dependency_is_an_error() {
    let (layout, root) = fixture(true, 0, false).await;
    assert!(
        verify_capsule_file_index(
            &layout,
            root,
            &[(MerkleHash::from([1; 32]), MerkleHash::from([2; 32]))]
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn selected_mapping_does_not_certify_unrelated_catalog_dependencies() {
    let (layout, root) = fixture(false, 0, true).await;
    assert_eq!(
        verify_capsule_file_index(
            &layout,
            root.clone(),
            &[(MerkleHash::from([1; 32]), MerkleHash::from([2; 32]))]
        )
        .await
        .unwrap(),
        vec![true]
    );
    assert!(
        crab_metadata::capsule_protocol::inspect_pointer_catalog_from_root(&layout, &root)
            .await
            .is_err()
    );
}

#[derive(Default)]
struct ReadBytes(std::sync::atomic::AtomicU64);
impl crab_storage::StorageObserver for ReadBytes {
    fn started(&self, _: crab_storage::StorageOperation) {}
    fn finished(&self, observation: crab_storage::StorageObservation) {
        self.0
            .fetch_add(observation.bytes_read, std::sync::atomic::Ordering::Relaxed);
    }
}

#[tokio::test]
async fn fixed_recovery_query_reads_pages_as_unrelated_catalog_grows() {
    for unrelated in [1, 10_000, 100_000] {
        let (layout, root) = fixture(false, unrelated, false).await;
        let reads = Arc::new(ReadBytes::default());
        let observed = crab_storage::StoreLayout::new(
            layout.store().clone().with_storage_observer(reads.clone()),
            layout.repo_prefix().to_owned(),
        );
        assert_eq!(
            verify_capsule_file_index(
                &observed,
                root,
                &[(MerkleHash::from([1; 32]), MerkleHash::from([2; 32]))]
            )
            .await
            .unwrap(),
            vec![true]
        );
        let bytes = reads.0.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            bytes < 512 * 1024,
            "{unrelated} unrelated mappings read {bytes} bytes"
        );
    }
}
