use std::sync::Arc;

use crate::{CacheCatalog, XetChunkCacheHandle};
use tokio::sync::Barrier;
use tokio::task::JoinSet;
use xet_client::cas_types::{ChunkRange, Key};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_disjoint_ranges_survive_reopening() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let directory = root.join("chunks");
    let catalog = CacheCatalog::new(root, Some(16 * 1024 * 1024));
    let cache = XetChunkCacheHandle::open(&directory, catalog.clone()).unwrap();
    let barrier = Arc::new(Barrier::new(32));
    let mut workers = JoinSet::new();
    for index in 0..32u32 {
        let cache = cache.cache.clone();
        let barrier = barrier.clone();
        workers.spawn(async move {
            let key = Key {
                prefix: "repository".to_owned(),
                hash: Default::default(),
            };
            let data = vec![index as u8; 8192];
            barrier.wait().await;
            (
                index,
                cache
                    .put(&key, &ChunkRange::new(index, index + 1), &[0, 8192], &data)
                    .await,
            )
        });
    }
    let mut failures = Vec::new();
    while let Some(result) = workers.join_next().await {
        let (index, result) = result.unwrap();
        if let Err(error) = result {
            failures.push(format!("range {index}: {error}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    drop(cache);
    let reopened = XetChunkCacheHandle::open(directory, catalog).unwrap();
    let key = Key {
        prefix: "repository".to_owned(),
        hash: Default::default(),
    };
    for index in 0..32u32 {
        let range = reopened
            .cache
            .get(&key, &ChunkRange::new(index, index + 1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(range.data, vec![index as u8; 8192]);
    }
}
