use super::*;
use crate::CacheCatalog;

#[tokio::test]
async fn namespace_payloads_share_one_catalog_without_sharing_identity_hits() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), None);
    let first = LocalCache::namespaced(catalog.clone(), &"a".repeat(64)).unwrap();
    let second = LocalCache::namespaced(catalog.clone(), &"b".repeat(64)).unwrap();
    first.prepare().unwrap();
    second.prepare().unwrap();
    let data = b"content";
    let key = CacheKey::Chunk(compute_data_hash(data));
    first.put(&key, data).await.unwrap();
    assert!(first.contains_verified(&key).await);
    assert!(!second.contains(&key).await);
    second.put(&key, data).await.unwrap();
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 2);
    assert!(root.join(".catalog.sqlite").exists());
    for cache in [&first, &second] {
        assert!(!cache.root().join(".catalog.sqlite").exists());
        assert_eq!(cache.stats().await.unwrap().chunk_count, 1);
    }
    catalog.set_retention_budget(Some(0)).await.unwrap();
    let fresh = b"fresh";
    let fresh_key = CacheKey::Chunk(compute_data_hash(fresh));
    assert_eq!(
        first
            .get_or_fetch(&fresh_key, || async { Ok(Bytes::from_static(fresh)) })
            .await
            .unwrap(),
        fresh.as_slice()
    );
    assert!(!first.hash_path(&fresh_key).exists());
}

#[tokio::test]
async fn namespace_removal_surfaces_preserve_global_logical_leases() {
    for operation in ["evict", "evict-bytes", "prune", "verify", "read-repair"] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let catalog = CacheCatalog::new(root.clone(), None);
        let namespace = "a".repeat(64);
        let cache = LocalCache::namespaced(catalog.clone(), &namespace).unwrap();
        let pruner =
            LocalCache::namespaced(CacheCatalog::new(root.clone(), 0), &namespace).unwrap();
        let data = b"content";
        let key = CacheKey::Chunk(compute_data_hash(data));
        cache.put(&key, data).await.unwrap();
        let path = cache.hash_path(&key);
        if matches!(operation, "verify" | "read-repair") {
            crate::private_fs::atomic_write(&root, &path, b"corrupt")
                .await
                .unwrap();
        }
        let lease = catalog.lease(&path).await.unwrap();
        let act = || async {
            match operation {
                "evict" => {
                    let _ = cache.evict(&key).await;
                }
                "evict-bytes" => {
                    cache.evict_bytes(u64::MAX).await.unwrap();
                }
                "prune" => {
                    pruner.prune().await.unwrap();
                }
                "verify" => {
                    cache.verify().await.unwrap();
                }
                "read-repair" => {
                    assert!(!cache.contains_verified(&key).await);
                }
                _ => unreachable!(),
            }
        };
        act().await;
        assert!(path.exists(), "{operation} ignored the global lease");
        assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 1);
        drop(lease);
        act().await;
        assert!(
            !path.exists(),
            "{operation} did not remove its namespace payload"
        );
        assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 0);
        assert!(!cache.root().join(".catalog.sqlite").exists());
    }
}

#[cfg(feature = "xet-chunk-cache")]
#[tokio::test]
async fn root_verification_reaches_namespace_objects_and_ranges_without_bypassing_leases() {
    use xet_client::cas_types::{ChunkRange, Key};
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), None);
    let first = LocalCache::namespaced(catalog.clone(), &"a".repeat(64)).unwrap();
    let second = LocalCache::namespaced(catalog.clone(), &"b".repeat(64)).unwrap();
    let key = CacheKey::Chunk(compute_data_hash(b"data"));
    first.put(&key, b"data").await.unwrap();
    let range =
        crate::XetChunkCacheHandle::open(second.root().join("chunks"), catalog.clone()).unwrap();
    range
        .cache
        .put(
            &Key {
                prefix: "repo".into(),
                hash: Default::default(),
            },
            &ChunkRange::new(0, 1),
            &[0, 4],
            b"data",
        )
        .await
        .unwrap();
    let global = LocalCache::new(root.clone());
    assert_eq!(global.stats().await.unwrap().chunk_count, 1);
    assert_eq!(global.verify().await.unwrap().valid, 2);
    let mut files = Vec::new();
    crate::private_fs::PinnedRoot::open(&root)
        .unwrap()
        .visit_files(&mut |path, _| {
            if crate::catalog::classify_family(path.to_str().unwrap()) == "decoded-range" {
                files.push(root.join(path));
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(files.len(), 1);
    std::fs::write(&files[0], b"corrupt").unwrap();
    let lease = catalog.lease(&files[0]).await.unwrap();
    assert_eq!(global.verify().await.unwrap().total, 1);
    assert!(files[0].exists());
    drop(lease);
    let report = global.verify().await.unwrap();
    assert_eq!((report.valid, report.corrupt), (1, 1));
    assert!(!files[0].exists());
    assert!(first.contains_verified(&key).await);
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 1);
}
