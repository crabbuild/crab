use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::*;

#[tokio::test]
async fn maintenance_retains_unknown_entries_and_stats_count_only_owned_payloads() {
    let (_temp, cache) = temp_cache();
    let data = b"data";
    let (xorb_hash, xorb_data) = test_xorb(data);
    let chunk = CacheKey::Chunk(compute_data_hash(data));
    for key in [&chunk, &CacheKey::Shard(compute_data_hash(data))] {
        cache.put(key, data).await.unwrap();
    }
    cache
        .put(&CacheKey::Xorb(xorb_hash), &xorb_data)
        .await
        .unwrap();
    cache
        .put(
            &CacheKey::Stage(crab_types::workflow::StageHash([3; 32])),
            data,
        )
        .await
        .unwrap();
    cache
        .put(
            &CacheKey::Manifest {
                name: "manifest".into(),
                etag: Some("etag".into()),
            },
            data,
        )
        .await
        .unwrap();
    let prefix = cache.hash_path(&chunk).parent().unwrap().to_owned();
    let retained = [
        prefix.join(".tmp-inflight"),
        prefix.join("notes.txt"),
        prefix.join("live/workspace"),
        cache.root.join("chunks/xx/not-a-hash"),
        cache.root.join("xorb-index/retained.db"),
        cache.root.join("manifests/.tmp-inflight"),
        cache.root.join("unknown/retained.db"),
    ];
    for path in &retained {
        crate::private_fs::atomic_write(&cache.root, path, b"sentinel")
            .await
            .unwrap();
    }
    symlink(cache.root(), prefix.join("live/link")).unwrap();
    let stats = cache.stats().await.unwrap();
    assert_eq!(
        (
            stats.chunk_count,
            stats.shard_count,
            stats.xorb_count,
            stats.stage_count,
            stats.manifest_count
        ),
        (1, 1, 1, 1, 1)
    );
    assert_eq!(
        (
            stats.chunk_bytes,
            stats.shard_bytes,
            stats.xorb_bytes,
            stats.stage_bytes
        ),
        (4, 4, xorb_data.len() as u64, 4)
    );
    assert_eq!(cache.verify().await.unwrap().valid, 3);
    let pruner = LocalCache::with_limits(cache.root.clone(), 0, Some(0));
    let preview = pruner
        .prune_with_options(PruneOptions {
            dry_run: true,
            record_entries: true,
        })
        .await
        .unwrap();
    let applied = pruner.prune().await.unwrap();
    assert_eq!(
        (preview.objects_evicted(), applied.objects_evicted()),
        (3, 3)
    );
    assert_eq!(applied.bytes_freed, 8 + xorb_data.len() as u64);
    let stats = cache.stats().await.unwrap();
    assert_eq!((stats.stage_count, stats.manifest_count), (1, 1));
    for path in retained {
        assert_eq!(fs::read(path).unwrap(), b"sentinel");
    }
}

#[tokio::test]
async fn every_object_family_retains_active_readers_during_maintenance() {
    let (xorb_hash, xorb) = test_xorb(b"data");
    for (key, data) in [
        (
            CacheKey::Chunk(compute_data_hash(b"data")),
            b"data".as_slice(),
        ),
        (
            CacheKey::Shard(compute_data_hash(b"data")),
            b"data".as_slice(),
        ),
        (CacheKey::Xorb(xorb_hash), xorb.as_ref()),
    ] {
        let (_temp, cache) = temp_cache();
        cache.put(&key, data).await.unwrap();
        let path = cache.hash_path(&key);
        fs::write(&path, b"corrupt").unwrap();
        let reader = crate::private_fs::open_read(&cache.root, &path)
            .await
            .unwrap();
        let pruner = LocalCache::with_limits(cache.root.clone(), 0, Some(0));
        for dry_run in [true, false] {
            let report = pruner
                .prune_with_options(PruneOptions {
                    dry_run,
                    record_entries: true,
                })
                .await
                .unwrap();
            assert_eq!((report.objects_evicted(), report.bytes_freed), (0, 0));
        }
        assert_eq!(pruner.evict_bytes(1).await.unwrap().objects_evicted(), 0);
        assert_eq!(pruner.verify().await.unwrap().total, 0);
        assert_eq!(fs::read(&path).unwrap(), b"corrupt");
        drop(reader);
        let report = pruner.verify().await.unwrap();
        assert_eq!((report.total, report.corrupt), (1, 1));
        assert!(!path.exists());
    }
}

#[tokio::test]
async fn maintenance_rejects_unsafe_roots_parents_and_payloads() {
    for case in [
        "root-link",
        "parent-link",
        "leaf-link",
        "hardlink",
        "root-mode",
        "leaf-mode",
    ] {
        let (temp, cache) = temp_cache();
        let key = CacheKey::Chunk(compute_data_hash(b"data"));
        cache.put(&key, b"data").await.unwrap();
        let path = cache.hash_path(&key);
        let sentinel = temp.path().join("sentinel");
        fs::write(&sentinel, b"outside").unwrap();
        match case {
            "root-link" => {
                let moved = temp.path().join("moved");
                fs::rename(&cache.root, &moved).unwrap();
                symlink(moved, &cache.root).unwrap();
            }
            "parent-link" => {
                let moved = temp.path().join("moved");
                fs::rename(path.parent().unwrap(), &moved).unwrap();
                symlink(moved, path.parent().unwrap()).unwrap();
            }
            "leaf-link" | "hardlink" => {
                fs::remove_file(&path).unwrap();
                if case == "leaf-link" {
                    symlink(&sentinel, &path).unwrap();
                } else {
                    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
                    fs::hard_link(&sentinel, &path).unwrap();
                }
            }
            "root-mode" => {
                fs::set_permissions(&cache.root, fs::Permissions::from_mode(0o755)).unwrap()
            }
            _ => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
        }
        assert!(
            matches!(cache.stats().await, Err(CacheError::UnsafeRoot { .. })),
            "{case}"
        );
        assert!(
            matches!(cache.verify().await, Err(CacheError::UnsafeRoot { .. })),
            "{case}"
        );
        assert!(
            matches!(
                cache.evict_bytes(1).await,
                Err(CacheError::UnsafeRoot { .. })
            ),
            "{case}"
        );
        let pruner = LocalCache::with_limits(cache.root.clone(), 0, Some(0));
        for dry_run in [true, false] {
            assert!(
                matches!(
                    pruner
                        .prune_with_options(PruneOptions {
                            dry_run,
                            record_entries: true
                        })
                        .await,
                    Err(CacheError::UnsafeRoot { .. })
                ),
                "{case}"
            );
        }
        assert_eq!(fs::read(&sentinel).unwrap(), b"outside");
        assert!(path.exists());
    }
}

#[tokio::test]
async fn maintenance_does_not_create_missing_roots_or_repair_an_index_during_stats() {
    let (_temp, cache) = temp_cache();
    assert_eq!(cache.stats().await.unwrap().chunk_count, 0);
    assert_eq!(cache.verify().await.unwrap().total, 0);
    assert_eq!(cache.prune().await.unwrap().objects_evicted(), 0);
    assert_eq!(cache.evict_bytes(1).await.unwrap().objects_evicted(), 0);
    assert!(!cache.root.exists());
    let index = cache.xorb_index_path();
    crate::private_fs::atomic_write(&cache.root, &index, b"not a database")
        .await
        .unwrap();
    assert_eq!(cache.stats().await.unwrap().xorb_count, 0);
    assert_eq!(fs::read(index).unwrap(), b"not a database");
}

#[tokio::test]
async fn all_full_xorb_verifiers_reject_a_wrong_footer_payload_digest() {
    let (temp, cache) = temp_cache();
    let (hash, data) = test_xorb(b"payload identity");
    let mut corrupt = data.to_vec();
    let digest_start = corrupt.len() - FOOTER_SIZE + 12;
    corrupt[digest_start] ^= 1;
    let parser = XorbParser::parse(Bytes::copy_from_slice(&corrupt)).unwrap();
    assert_eq!(parser.hash(), hash);
    parser.verify_all_chunks().unwrap();
    let key = CacheKey::Xorb(hash);
    assert!(LocalCache::validate(&key, &corrupt).is_err());
    let source = temp.path().join("source.xorb");
    fs::write(&source, &corrupt).unwrap();
    assert!(matches!(
        cache
            .put_xorb_file(&hash, &source, corrupt.len() as u64)
            .await,
        Err(CacheError::CorruptObject { .. })
    ));
    assert!(!cache.hash_path(&key).exists());
    for verified_read in [false, true] {
        cache.put_unchecked_for_test(&key, &corrupt).await.unwrap();
        if verified_read {
            assert!(!cache.contains_verified(&key).await);
        } else {
            assert_eq!(cache.verify().await.unwrap().corrupt, 1);
        }
        assert!(!cache.hash_path(&key).exists());
    }
}
