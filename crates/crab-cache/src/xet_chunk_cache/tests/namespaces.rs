use super::*;
use crate::{CacheCatalog, LocalCache};

#[tokio::test]
async fn live_range_handles_observe_root_policy_without_changing_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), None);
    let local = LocalCache::namespaced(catalog.clone(), &"a".repeat(64)).unwrap();
    let directory = local.root().join("chunks");
    let handle = XetChunkCacheHandle::open(&directory, catalog.clone()).unwrap();
    let first = test_key();
    let range = ChunkRange::new(0, 1);
    handle
        .cache
        .put(&first, &range, &[0, 4], b"data")
        .await
        .unwrap();
    catalog.set_retention_budget(Some(0)).await.unwrap();
    let reopened = XetChunkCacheHandle::open(&directory, catalog.clone()).unwrap();
    assert!(Arc::ptr_eq(&handle.cache, &reopened.cache));
    let second = Key {
        prefix: "another".into(),
        hash: Default::default(),
    };
    handle
        .cache
        .put(&second, &range, &[0, 4], b"more")
        .await
        .unwrap();
    assert!(handle.cache.get(&second, &range).await.unwrap().is_none());
    assert!(handle.cache.get(&first, &range).await.unwrap().is_some());
    catalog
        .set_retention_budget(Some(1024 * 1024))
        .await
        .unwrap();
    handle
        .cache
        .put(&second, &range, &[0, 4], b"more")
        .await
        .unwrap();
    assert!(handle.cache.get(&second, &range).await.unwrap().is_some());
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 2);
    assert!(!local.root().join(".catalog.sqlite").exists());
    let wrong_owner = CacheCatalog::new(local.root().to_owned(), None);
    assert!(matches!(
        XetChunkCacheHandle::open(&directory, wrong_owner),
        Err(CacheError::UnsafeRoot { .. })
    ));
}

#[tokio::test]
async fn range_namespace_maintenance_preserves_global_logical_leases() {
    for action in ["prune", "verify", "clean", "read"] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let catalog = CacheCatalog::new(root.clone(), None);
        let local = LocalCache::namespaced(catalog.clone(), &"a".repeat(64)).unwrap();
        let directory = local.root().join("chunks");
        let handle = XetChunkCacheHandle::open(&directory, catalog.clone()).unwrap();
        let key = test_key();
        let range = ChunkRange::new(0, 1);
        handle
            .cache
            .put(&key, &range, &[0, 4], b"data")
            .await
            .unwrap();
        let pinned = PinnedRoot::open(&directory).unwrap();
        let entries = collect_xet_chunk_cache_entries(&pinned, &CancellationToken::new()).unwrap();
        let path = directory.join(&entries[0].path);
        if matches!(action, "verify" | "read") {
            fs::write(&path, b"corrupt").unwrap();
        }
        let lease = catalog.lease(&path).await.unwrap();
        let act = || async {
            match action {
                "prune" => {
                    prune_xet_chunk_cache(&directory, &catalog, 0, false, false)
                        .await
                        .unwrap();
                }
                "verify" => {
                    verify_xet_chunk_cache(&directory, &catalog).await.unwrap();
                }
                "clean" => {
                    crate::clean_cache(&root, false, &CancellationToken::new())
                        .await
                        .unwrap();
                }
                "read" => {
                    assert!(handle.cache.get(&key, &range).await.unwrap().is_none());
                }
                _ => unreachable!(),
            }
        };
        act().await;
        assert!(path.exists(), "{action} ignored the global lease");
        assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 1);
        drop(lease);
        act().await;
        assert!(!path.exists(), "{action} retained its eligible range");
        assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 0);
    }
}

#[cfg(unix)]
#[test]
fn range_initialization_cannot_create_through_an_owner_descendant_symlink() {
    use std::os::unix::fs::symlink;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let outside = temporary.path().join("outside");
    crate::ensure_private_cache_directory(&root).unwrap();
    crate::ensure_private_cache_directory(&outside).unwrap();
    symlink(&outside, root.join("redirect")).unwrap();
    let catalog = CacheCatalog::new(root.clone(), None);
    let result = XetChunkCacheHandle::open(root.join("redirect/chunks"), catalog);
    assert_eq!(
        (result.is_err(), outside.join("chunks").exists()),
        (true, false)
    );
}
