use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::*;

#[tokio::test]
async fn case_distinct_keys_keep_separate_accounting_and_maintenance_owners() {
    use crate::catalog::CacheCatalog;

    for clean in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let cache = CrabRangeCache {
            root: root.join("chunks"),
            capacity: Some(1024 * 1024),
            catalog: CacheCatalog::new(root.clone(), 1024 * 1024),
        };
        let range = ChunkRange::new(0, 1);
        let mut keys = Vec::new();
        // These hashes formerly selected Base64 buckets AB and Ab.
        for (first, second) in [(0, 0x10), (1, 0xb0)] {
            let mut hash = [0u8; 32];
            hash[..2].copy_from_slice(&[first, second]);
            let key = Key {
                prefix: String::new(),
                hash: hash.into(),
            };
            cache.put(&key, &range, &[0, 1], &[second]).await.unwrap();
            keys.push(key);
        }
        assert_ne!(cache.key_directory(&keys[0]), cache.key_directory(&keys[1]));
        assert_eq!(xet_chunk_cache_stats(&cache.root).await.unwrap().entries, 2);
        assert_eq!(verify_xet_chunk_cache(&cache.root).await.unwrap().valid, 2);
        assert_eq!(
            CacheCatalog::read_only_stats(&root).unwrap().total_bytes,
            26
        );
        for (key, expected) in keys.iter().zip([0x10, 0xb0]) {
            assert_eq!(
                cache.get(key, &range).await.unwrap().unwrap().data,
                [expected]
            );
        }

        let path = fs::read_dir(cache.key_directory(&keys[0]))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let lease = cache.catalog.lease(&path).await.unwrap();
        if clean {
            assert_eq!(
                crate::clean_cache(&root, false, &CancellationToken::new())
                    .await
                    .unwrap()
                    .files_removed,
                1
            );
        } else {
            assert_eq!(
                prune_xet_chunk_cache(&cache.root, 0, false, false)
                    .await
                    .unwrap()
                    .entries_evicted,
                1
            );
        }
        assert_eq!(
            CacheCatalog::read_only_stats(&root).unwrap().total_bytes,
            13
        );
        assert_eq!(
            cache.get(&keys[0], &range).await.unwrap().unwrap().data,
            [0x10]
        );
        drop(lease);
        assert_eq!(
            crate::clean_cache(&root, false, &CancellationToken::new())
                .await
                .unwrap()
                .files_removed,
            1
        );
        assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().total_bytes, 0);
    }
}

#[test]
fn range_name_encoding_is_injective_after_case_folding() {
    let mut names = std::collections::HashSet::new();
    for byte in 0..=u8::MAX {
        let name = encode_range_name(&[byte]);
        assert!(names.insert(name.to_lowercase()));
        assert_eq!(decode_range_name(&name), Some(vec![byte]));
    }
    for malformed in ["AB", "aB", "a", "..", "00/11"] {
        assert_eq!(decode_range_name(malformed), None);
    }
}

#[tokio::test]
async fn maintenance_preserves_unknown_live_and_unpublished_entries() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("chunks");
    let payload = write_xet_range(&root, b"payload").await;
    let parent = payload.parent().unwrap();
    let retained = [
        parent.join(".tmp-123-1"),
        parent.join("notes.txt"),
        parent.join(".catalog.sqlite"),
        parent.join("live/workspace"),
        root.join("AA/notes.txt"),
        root.join("unknown/workspace"),
    ];
    for path in &retained {
        crate::private_fs::atomic_write(&root, path, b"sentinel")
            .await
            .unwrap();
    }
    // Unknown trees are not inventory authority, even if they contain links.
    symlink(temp.path(), parent.join("live/link")).unwrap();
    assert_eq!(xet_chunk_cache_stats(&root).await.unwrap().entries, 1);
    assert_eq!(verify_xet_chunk_cache(&root).await.unwrap().valid, 1);
    assert_eq!(
        prune_xet_chunk_cache(&root, 0, false, false)
            .await
            .unwrap()
            .entries_evicted,
        1
    );
    for path in retained {
        assert_eq!(fs::read(path).unwrap(), b"sentinel");
    }
    assert!(!payload.exists());
}

#[tokio::test]
async fn maintenance_skips_active_readers_in_preview_and_apply() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("chunks");
    let payload = write_xet_range(&root, b"payload").await;
    fs::write(&payload, b"bad").unwrap();
    let reader = crate::private_fs::open_read(&root, &payload).await.unwrap();
    for dry_run in [true, false] {
        let report = prune_xet_chunk_cache(&root, 0, dry_run, true)
            .await
            .unwrap();
        assert_eq!((report.entries_evicted, report.bytes_freed), (0, 0));
    }
    assert_eq!(verify_xet_chunk_cache(&root).await.unwrap().total, 0);
    assert_eq!(fs::read(&payload).unwrap(), b"bad");
    drop(reader);
    let report = verify_xet_chunk_cache(&root).await.unwrap();
    assert_eq!((report.total, report.corrupt), (1, 1));
    assert!(!payload.exists());
}

#[tokio::test]
async fn maintenance_rejects_unsafe_roots_and_entries_without_touching_targets() {
    for case in [
        "root-link",
        "parent-link",
        "leaf-link",
        "hardlink",
        "root-mode",
        "leaf-mode",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("chunks");
        let payload = write_xet_range(&root, b"payload").await;
        let sentinel = temp.path().join("sentinel");
        fs::write(&sentinel, b"outside").unwrap();
        match case {
            "root-link" => {
                let moved = temp.path().join("moved");
                fs::rename(&root, &moved).unwrap();
                symlink(moved, &root).unwrap();
            }
            "parent-link" => {
                let parent = payload.parent().unwrap();
                let moved = temp.path().join("moved");
                fs::rename(parent, &moved).unwrap();
                symlink(moved, parent).unwrap();
            }
            "leaf-link" | "hardlink" => {
                fs::remove_file(&payload).unwrap();
                if case == "leaf-link" {
                    symlink(&sentinel, &payload).unwrap();
                } else {
                    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
                    fs::hard_link(&sentinel, &payload).unwrap();
                }
            }
            "root-mode" => fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap(),
            _ => fs::set_permissions(&payload, fs::Permissions::from_mode(0o644)).unwrap(),
        }
        assert!(
            matches!(
                xet_chunk_cache_stats(&root).await,
                Err(CacheError::UnsafeRoot { .. })
            ),
            "{case}"
        );
        assert!(
            matches!(
                verify_xet_chunk_cache(&root).await,
                Err(CacheError::UnsafeRoot { .. })
            ),
            "{case}"
        );
        for dry_run in [true, false] {
            assert!(
                matches!(
                    prune_xet_chunk_cache(&root, 0, dry_run, true).await,
                    Err(CacheError::UnsafeRoot { .. })
                ),
                "{case}"
            );
        }
        assert_eq!(fs::read(&sentinel).unwrap(), b"outside", "{case}");
        assert!(payload.exists(), "{case}");
    }
}

#[tokio::test]
async fn maintenance_missing_roots_remain_missing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("absent");
    for dry_run in [true, false] {
        assert_eq!(
            prune_xet_chunk_cache(&root, 0, dry_run, true)
                .await
                .unwrap()
                .entries_evicted,
            0
        );
    }
    assert_eq!(verify_xet_chunk_cache(&root).await.unwrap().total, 0);
    assert!(!root.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_scan_cancels_its_worker_without_cancelling_the_parent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("chunks");
    crate::private_fs::ensure_directory(&root).unwrap();
    let parent = CancellationToken::new();
    let token = parent.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (stopped, done) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        with_pinned_root(&root, &token, move |_, cancel| {
            started.send(()).unwrap();
            tokio::runtime::Handle::current().block_on(cancel.cancelled());
            stopped.send(()).unwrap();
            Ok(())
        })
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(5), done)
        .await
        .unwrap()
        .unwrap();
    assert!(!parent.is_cancelled());
}

#[tokio::test]
async fn verify_checks_chunk_identity_without_treating_xorb_keys_as_chunk_hashes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache/chunks");
    let cache = XetChunkCacheHandle::open(&root, 1024 * 1024).unwrap();
    let range = ChunkRange::new(0, 1);
    for prefix in [CHUNK_HASH_PREFIX, "xorb"] {
        let key = Key {
            prefix: prefix.into(),
            hash: (*blake3::hash(b"good").as_bytes()).into(),
        };
        cache
            .cache
            .put(&key, &range, &[0, 4], b"bad!")
            .await
            .unwrap();
    }
    let report = verify_xet_chunk_cache(&root).await.unwrap();
    assert_eq!((report.total, report.valid, report.corrupt), (2, 1, 1));
    assert_eq!(xet_chunk_cache_stats(&root).await.unwrap().entries, 1);
}
