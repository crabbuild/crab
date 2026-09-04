use super::*;
use tokio_util::sync::CancellationToken;

async fn seeded(root: &Path) -> PathBuf {
    let relative = format!("chunks/ab/{}", "ab".repeat(32));
    let path = root.join(relative);
    let catalog = CacheCatalog::new(root.to_owned(), 1024 * 1024);
    let reservation = catalog.reserve(&path, 4).await.unwrap().unwrap();
    let reservation = reservation.write(b"data").await.unwrap();
    catalog
        .record_and_maintain("chunk", "fixture".into(), 4, reservation)
        .await
        .unwrap();
    path
}

#[cfg(feature = "local-cache")]
#[tokio::test]
async fn object_deletion_surfaces_retire_only_removed_accounting() {
    use crate::{CacheKey, LocalCache, PruneOptions};

    for action in ["prune", "verify", "clean", "evict", "read"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let cache = LocalCache::new(root.clone());
        let hash = crab_xet::hash::compute_data_hash(b"data");
        let key = CacheKey::Chunk(hash);
        cache.put(&key, b"data").await.unwrap();
        let hex = hash.hex();
        let path = root.join("chunks").join(&hex[..2]).join(hex);
        crate::private_fs::atomic_write(&root, &root.join("retained/file"), b"retain")
            .await
            .unwrap();
        let before = CacheCatalog::read_only_stats(&root).unwrap();
        let pruner = LocalCache::with_limits(root.clone(), 0, Some(0));
        match action {
            "prune" => {
                pruner
                    .prune_with_options(PruneOptions {
                        dry_run: true,
                        record_entries: false,
                    })
                    .await
                    .unwrap();
                assert_eq!(CacheCatalog::read_only_stats(&root).unwrap(), before);
                pruner.prune().await.unwrap();
            }
            "clean" => {
                crate::clean_cache(&root, false, &CancellationToken::new())
                    .await
                    .unwrap();
            }
            "evict" => {
                cache.evict(&key).await.unwrap();
            }
            _ => {
                std::fs::write(&path, b"bad!").unwrap();
                if action == "verify" {
                    cache.verify().await.unwrap();
                } else {
                    assert!(!cache.contains_verified(&key).await);
                }
            }
        }
        assert!(!path.exists(), "{action}");
        let after = CacheCatalog::read_only_stats(&root).unwrap();
        assert_eq!((after.entries, after.total_bytes), (0, 0), "{action}");
        assert_eq!(
            std::fs::read(root.join("retained/file")).unwrap(),
            b"retain"
        );
    }
}

#[cfg(feature = "xet-chunk-cache")]
#[tokio::test]
async fn range_deletion_surfaces_retire_only_removed_accounting() {
    use crate::xet_chunk_cache::{
        XetChunkCacheHandle, prune_xet_chunk_cache, verify_xet_chunk_cache,
    };
    use xet_client::cas_types::{ChunkRange, Key};

    for action in ["prune", "verify", "clean", "read"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let ranges = root.join("chunks");
        let handle = XetChunkCacheHandle::open(ranges.clone(), 1024 * 1024).unwrap();
        let key = Key {
            prefix: "repo".into(),
            hash: Default::default(),
        };
        let range = ChunkRange::new(0, 1);
        handle
            .cache
            .put(&key, &range, &[0, 4], b"data")
            .await
            .unwrap();
        let pinned = PinnedRoot::open(&root).unwrap();
        let mut paths = Vec::new();
        pinned
            .visit_files(&mut |path, _| {
                if classify_family(path.to_str().unwrap()) == "decoded-range" {
                    paths.push(root.join(path));
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(paths.len(), 1);
        let before = CacheCatalog::read_only_stats(&root).unwrap();
        match action {
            "prune" => {
                prune_xet_chunk_cache(&ranges, 0, true, false)
                    .await
                    .unwrap();
                assert_eq!(CacheCatalog::read_only_stats(&root).unwrap(), before);
                prune_xet_chunk_cache(&ranges, 0, false, false)
                    .await
                    .unwrap();
            }
            "clean" => {
                crate::clean_cache(&root, false, &CancellationToken::new())
                    .await
                    .unwrap();
            }
            _ => {
                std::fs::write(&paths[0], b"bad").unwrap();
                if action == "verify" {
                    verify_xet_chunk_cache(&ranges).await.unwrap();
                } else {
                    assert!(handle.cache.get(&key, &range).await.unwrap().is_none());
                }
            }
        }
        assert!(!paths[0].exists(), "{action}");
        let after = CacheCatalog::read_only_stats(&root).unwrap();
        assert_eq!((after.entries, after.total_bytes), (0, 0), "{action}");
    }
}

#[tokio::test]
async fn declined_or_failed_removal_preserves_rows() {
    for failure in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let path = seeded(&root).await;
        let pinned = PinnedRoot::open(&root).unwrap();
        let mut removal = PayloadRemoval::open(Some(&pinned), &root, false).unwrap();
        let result = removal.remove(path.strip_prefix(&root).unwrap(), || {
            if failure {
                Err(CacheError::Cancelled)
            } else {
                Ok(None)
            }
        });
        assert_eq!(result.is_err(), failure);
        drop(removal);
        assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().total_bytes, 4);
        assert_eq!(std::fs::read(path).unwrap(), b"data");
    }
}

#[tokio::test]
async fn removal_excludes_replacement_registration_until_commit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let path = seeded(&root).await;
    let relative = path.strip_prefix(&root).unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    let contender = pinned
        .open_database(
            Path::new(CATALOG_FILE),
            DatabaseMode::ReadWrite,
            std::time::Duration::ZERO,
        )
        .unwrap();
    let mut removal = PayloadRemoval::open(Some(&pinned), &root, false).unwrap();
    removal
        .remove(relative, || {
            let error = contender
                .execute(
                    "INSERT INTO reservations VALUES ('next', ?1, 4, 1, 0)",
                    [relative.to_str().unwrap()],
                )
                .unwrap_err();
            assert_eq!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy)
            );
            pinned.remove_file(relative).map(Some)
        })
        .unwrap();
    drop(removal);
    drop(contender);
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 0);
    seeded(&root).await;
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().total_bytes, 4);
}

#[cfg(unix)]
#[tokio::test]
async fn database_replacement_stops_removal_before_filesystem_action() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let path = seeded(&root).await;
    let pinned = PinnedRoot::open(&root).unwrap();
    let mut removal = PayloadRemoval::open(Some(&pinned), &root, false).unwrap();
    std::fs::rename(root.join(CATALOG_FILE), root.join("saved-catalog")).unwrap();
    std::fs::write(root.join(CATALOG_FILE), b"replacement database").unwrap();
    let mut called = false;
    let result = removal.remove(path.strip_prefix(&root).unwrap(), || {
        called = true;
        Ok(Some(4))
    });
    assert!(result.is_err());
    assert!(!called);
    assert_eq!(std::fs::read(path).unwrap(), b"data");
}

#[cfg(unix)]
#[tokio::test]
async fn root_replacement_keeps_deletion_and_accounting_in_original_tree() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let path = seeded(&root).await;
    let relative = path.strip_prefix(&root).unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    let mut removal = PayloadRemoval::open(Some(&pinned), &root, false).unwrap();
    let moved = temp.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    seeded(&root).await;
    removal
        .remove(relative, || pinned.remove_file(relative).map(Some))
        .unwrap();
    drop(removal);
    assert_eq!(CacheCatalog::read_only_stats(&moved).unwrap().entries, 0);
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().total_bytes, 4);
    assert_eq!(std::fs::read(path).unwrap(), b"data");
}

#[tokio::test]
async fn missing_or_corrupt_catalog_does_not_require_repair_for_payload_cleanup() {
    for state in ["missing", "corrupt", "schema"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let path = seeded(&root).await;
        let database = root.join(CATALOG_FILE);
        match state {
            "missing" => std::fs::remove_file(&database).unwrap(),
            "corrupt" => std::fs::write(&database, b"invalid SQLite").unwrap(),
            _ => {
                let pinned = PinnedRoot::open(&root).unwrap();
                let connection = pinned
                    .open_database(
                        Path::new(CATALOG_FILE),
                        DatabaseMode::ReadWrite,
                        std::time::Duration::ZERO,
                    )
                    .unwrap();
                connection
                    .execute_batch("DROP TABLE cache_entries")
                    .unwrap();
            }
        }
        let before = std::fs::read(&database).ok();
        let report = crate::clean_cache(&root, false, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(report.files_removed, 1);
        assert!(!path.exists());
        assert_eq!(std::fs::read(database).ok(), before);
    }
}

#[tokio::test]
async fn busy_catalog_stops_before_payload_removal() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let path = seeded(&root).await;
    let pinned = PinnedRoot::open(&root).unwrap();
    let writer = pinned
        .open_database(
            Path::new(CATALOG_FILE),
            DatabaseMode::ReadWrite,
            std::time::Duration::ZERO,
        )
        .unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut called = false;
    let result = PayloadRemoval::open(Some(&pinned), &root, false).and_then(|mut removal| {
        removal.remove(path.strip_prefix(&root).unwrap(), || {
            called = true;
            Ok(Some(4))
        })
    });
    assert!(busy(&result.unwrap_err()));
    assert!(!called);
    drop(writer);
    assert_eq!(std::fs::read(path).unwrap(), b"data");
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().total_bytes, 4);
}

#[cfg(feature = "xet-chunk-cache")]
#[cfg(unix)]
#[tokio::test]
async fn range_parent_and_leaf_remain_paired_after_parent_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let path = seeded(&root).await;
    let (ranges, parent) = PinnedRoot::open_with_private_parent(&root.join("chunks")).unwrap();
    let mut removal = PayloadRemoval::open(Some(parent.as_ref().unwrap()), &root, false).unwrap();
    let moved = temp.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    seeded(&root).await;
    removal
        .remove(path.strip_prefix(&root).unwrap(), || {
            ranges
                .remove_file(path.strip_prefix(root.join("chunks")).unwrap())
                .map(Some)
        })
        .unwrap();
    drop(removal);
    assert_eq!(CacheCatalog::read_only_stats(&moved).unwrap().entries, 0);
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().total_bytes, 4);
    assert_eq!(std::fs::read(path).unwrap(), b"data");
}
