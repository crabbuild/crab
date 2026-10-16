#![expect(clippy::unwrap_used, reason = "test assertions")]
use super::*;
use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
    FileCatalogEntry, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
};
use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
use crab_xet::{
    shard::{ShardWriter, file_info_from_placements, xorb_info_from_placements},
    xorb::{
        builder::{RunId, XorbBuilder},
        format::Chunk,
    },
};
use std::sync::Mutex;

#[derive(Default)]
struct Observations(Mutex<Vec<StorageObservation>>);
impl StorageObserver for Observations {
    fn started(&self, _: StorageOperation) {}
    fn finished(&self, observation: StorageObservation) {
        self.0.lock().unwrap().push(observation);
    }
}

async fn fixture(
    wrong_size: bool,
) -> (
    super::super::StoreChecker,
    StoreLayout<Store>,
    Arc<Observations>,
    Option<String>,
) {
    let observations = Arc::new(Observations::default());
    let storage = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(observations.clone());
    let layout = StoreLayout::new(storage.clone(), "grouped-fsck".to_owned());
    let base =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let mut builder = XorbBuilder::new();
    let mut chunks = Vec::new();
    for index in 0..129u64 {
        let bytes = index.to_le_bytes();
        let chunk = Chunk::new(Bytes::copy_from_slice(&bytes));
        builder.push(&chunk, RunId(0)).unwrap();
        chunks.push((MerkleHash::from(*blake3::hash(&bytes).as_bytes()), chunk));
    }
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let placements = xorb
        .placements
        .iter()
        .cloned()
        .map(|placement| (placement.chunk_hash, placement))
        .collect();
    let mut shard = ShardWriter::new();
    shard
        .add_xorb(Arc::new(
            xorb_info_from_placements(xorb.hash, &xorb.placements).unwrap(),
        ))
        .unwrap();
    for (file_hash, chunk) in &chunks {
        shard
            .add_file(file_info_from_placements(*file_hash, &[chunk.hash], &placements).unwrap())
            .unwrap();
    }
    let (shard_body, shard_hash) = shard.finalize().unwrap();
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_xorb(
            xorb.hash.hex(),
            XorbCatalogEntry::new(
                xorb.bytes.len() as u64,
                blake3::hash(&xorb.bytes).to_hex().to_string(),
                chunks
                    .iter()
                    .map(|(_, chunk)| XorbChunkEntry::new(chunk.hash.hex(), 8))
                    .collect(),
            ),
        )
        .unwrap();
    catalog
        .insert_shard(
            shard_hash.hex(),
            ShardCatalogEntry::new(shard_body.len() as u64, vec![xorb.hash.hex()]),
        )
        .unwrap();
    let mut hashes = chunks
        .iter()
        .map(|(hash, _)| hash.hex())
        .collect::<Vec<_>>();
    hashes.sort();
    let wrong = wrong_size.then(|| hashes[100].clone());
    for hash in hashes {
        let size = if wrong.as_ref() == Some(&hash) { 9 } else { 8 };
        catalog
            .insert_file(hash, FileCatalogEntry::new(size, shard_hash.hex()))
            .unwrap();
    }
    storage
        .put(&layout.xorb_path(&xorb.hash), xorb.bytes)
        .await
        .unwrap();
    storage
        .put(&layout.shard_path(&shard_hash), shard_body.into())
        .await
        .unwrap();
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
    let root = crab_write::capsule_protocol::publish(&layout, base, &transaction, &capsule)
        .await
        .unwrap();
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            Some("2".repeat(40)),
            None,
            None,
        )],
    )
    .unwrap();
    let capsule = Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap();
    let root = crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
        .await
        .unwrap();
    let checker = super::super::StoreChecker::for_capsule_repository(
        crate::storage::store::Store::from_storage(storage),
        layout.repo_prefix().to_owned(),
        root,
    )
    .await
    .unwrap();
    observations.0.lock().unwrap().clear();
    (checker, layout, observations, wrong)
}

#[tokio::test]
async fn shard_group_reads_each_body_once_across_multiple_file_batches() {
    let (checker, layout, observations, _) = fixture(false).await;
    let state = checker.capsule.as_ref().unwrap();
    let issues = check_objects(&layout, &state.view, &state.lookup)
        .await
        .unwrap();
    let operations = observations.0.lock().unwrap();
    assert_eq!(
        (
            issues.len(),
            operations
                .iter()
                .filter(|op| op.operation == StorageOperation::Get)
                .count(),
            operations.iter().any(|op| op.bytes_written > 0)
        ),
        (0, 2, false)
    );
}

#[tokio::test]
async fn shard_group_rejects_a_bad_recipe_after_the_first_file_batch() {
    let (checker, layout, observations, wrong) = fixture(true).await;
    let state = checker.capsule.as_ref().unwrap();
    let issues = check_objects(&layout, &state.view, &state.lookup)
        .await
        .unwrap();
    assert_eq!(issues.len(), 1);
    assert!(
        matches!(&issues[0].kind, crate::cmd::fsck::IssueKind::CorruptShard { detail, .. } if detail.contains(wrong.as_ref().unwrap()) && detail.contains("covers 8 bytes"))
    );
    let operations = observations.0.lock().unwrap();
    assert_eq!(
        operations
            .iter()
            .filter(|op| op.operation == StorageOperation::Get)
            .count(),
        2
    );
}

#[tokio::test]
async fn shard_header_verification_retains_chunk_metadata_comparison() {
    let (checker, layout, _, _) = fixture(false).await;
    let state = checker.capsule.as_ref().unwrap();
    let catalog = state.view.pointer_catalog().await.unwrap();
    let (hash, entry) = catalog.shards().iter().next().unwrap();
    let body = layout
        .store()
        .get_with_etag_bounded(
            &layout.shard_path(&MerkleHash::from_hex(hash).unwrap()),
            entry.encoded_size(),
        )
        .await
        .unwrap()
        .0;
    let (xorb_hash, xorb) = catalog.xorbs().iter().next().unwrap();
    let mut chunks = xorb.chunks().to_vec();
    chunks[0] = XorbChunkEntry::new(chunks[0].hash(), chunks[0].uncompressed_size() + 1);
    let mut inconsistent = PointerCatalog::new();
    inconsistent
        .insert_xorb(
            xorb_hash.clone(),
            XorbCatalogEntry::new(xorb.encoded_size(), xorb.body_digest(), chunks),
        )
        .unwrap();
    assert!(
        matches!(prepare_shard(hash, entry, &inconsistent, body), Err(reason) if reason.contains("differs from the catalog"))
    );
}
