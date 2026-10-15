use std::collections::HashMap;
use std::sync::Arc;

use crate::capsule_protocol::{PointerCatalog, XorbCatalogEntry, XorbChunkEntry};
use crate::receipts::OriginReceipt;
use crab_xet::hash::MerkleHash;
use object_store::memory::InMemory;

use super::{RemoteIndexConfig, write_capsule_chunk_candidates};

#[tokio::test]
async fn capsule_candidates_reject_xorbs_without_a_source_shard_root() {
    let hash = MerkleHash::from([3; 32]);
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_xorb(
            hash.hex(),
            XorbCatalogEntry::new(
                100,
                "4".repeat(64),
                vec![XorbChunkEntry::new(MerkleHash::from([1; 32]).hex(), 9)],
            ),
        )
        .unwrap();
    let origin = OriginReceipt::new(
        "canonical-origin".to_owned(),
        format!(".crab/xorbs/03/{}", hash.hex()),
        hash.into(),
        [5; 32],
        100,
        Some("etag".to_owned()),
        None,
    );
    let result = write_capsule_chunk_candidates(
        Arc::new(InMemory::new()),
        &RemoteIndexConfig::for_repo("repo"),
        "repo",
        &catalog,
        &HashMap::from([(hash, origin)]),
    )
    .await;
    assert!(result.is_err(), "an xorb descriptor alone is not a GC root");
}

#[tokio::test]
async fn capsule_candidates_writer_initializes_canonical_format() {
    let store: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
    let config = RemoteIndexConfig::for_repo("repo");
    let writer = super::RemoteIndexWriter::open(Arc::clone(&store), &config, false, true)
        .await
        .unwrap();
    writer.close().await.unwrap();
    let reader = super::open_reader(store, &config.chunk_index_path, "chunk_index_db")
        .await
        .unwrap()
        .unwrap();
    let key = crate::key_codec::encode_system_key(crate::key_codec::SYS_FORMAT_VERSION);
    let value = reader.get(&key).await.unwrap();
    reader.close().await.unwrap();
    assert_eq!(value.as_deref(), Some(1u32.to_le_bytes().as_slice()));
}

#[tokio::test]
async fn capsule_candidates_writer_rejects_noncanonical_existing_databases() {
    for marker in [None, Some(2u32.to_le_bytes().to_vec()), Some(vec![1])] {
        let store: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let config = RemoteIndexConfig::for_repo("repo");
        let seed = slatedb::Db::open(config.chunk_index_path.clone(), Arc::clone(&store))
            .await
            .unwrap();
        if let Some(marker) = marker {
            seed.put(
                crate::key_codec::encode_system_key(crate::key_codec::SYS_FORMAT_VERSION),
                marker,
            )
            .await
            .unwrap();
        }
        seed.close().await.unwrap();
        let result = super::RemoteIndexWriter::open(store, &config, false, true).await;
        if let Ok(writer) = result {
            writer.close().await.unwrap();
            panic!("invalid existing format was accepted");
        }
    }
}
