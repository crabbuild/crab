use super::RemoteIndexConfig;
use object_store::memory::InMemory;
use std::sync::Arc;

#[tokio::test]
async fn writer_initializes_canonical_format() {
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
async fn writer_rejects_noncanonical_existing_databases() {
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
