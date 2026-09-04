use super::*;
use crate::CacheCatalog;

#[tokio::test]
async fn successful_object_read_surfaces_update_recency() {
    for mode in [
        "chunk", "shard", "stage", "xorb", "body", "metadata", "range", "payload",
    ] {
        let (_temp, cache) = temp_cache();
        let (hash, xorb) = test_xorb(b"data");
        let (key, data) = match mode {
            "chunk" => (
                CacheKey::Chunk(compute_data_hash(b"data")),
                Bytes::from_static(b"data"),
            ),
            "shard" => (
                CacheKey::Shard(compute_data_hash(b"data")),
                Bytes::from_static(b"data"),
            ),
            "stage" => (
                CacheKey::Stage(crab_types::workflow::StageHash([7; 32])),
                Bytes::from_static(b"data"),
            ),
            _ => (CacheKey::Xorb(hash), xorb),
        };
        cache.put(&key, &data).await.unwrap();
        let file = std::fs::File::open(cache.hash_path(&key)).unwrap();
        let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(60);
        file.set_modified(old_time).unwrap();
        let hit = match mode {
            "body" => cache
                .get_read_xorb_if_present(&hash)
                .await
                .unwrap()
                .is_some(),
            "metadata" => cache
                .get_xorb_metadata_if_present(&hash)
                .await
                .unwrap()
                .is_some(),
            "range" => cache.get_xorb_range_if_present(&hash, 0..1).await.is_some(),
            "payload" => cache.contains_verified(&key).await,
            _ => cache
                .try_read_key_limited(&key, Some(MAX_XORB_SIZE as u64))
                .await
                .is_some(),
        };
        assert!(hit, "{mode}");
        assert!(
            file.metadata().unwrap().modified().unwrap() > old_time,
            "{mode}"
        );
    }
}

#[tokio::test]
async fn probes_manifest_reads_and_invalid_ranges_do_not_update_recency() {
    let (_temp, cache) = temp_cache();
    let (hash, data) = test_xorb(b"data");
    let xorb = CacheKey::Xorb(hash);
    let manifest = CacheKey::Manifest {
        name: "manifest".into(),
        etag: Some("version".into()),
    };
    cache.put(&xorb, &data).await.unwrap();
    cache.put(&manifest, b"manifest").await.unwrap();
    let files = [
        cache.hash_path(&xorb),
        cache.hash_path(&manifest),
        cache.manifest_etag_path("manifest"),
    ]
    .map(|path| std::fs::File::open(path).unwrap());
    let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(60);
    for file in &files {
        file.set_modified(old_time).unwrap();
    }
    assert!(cache.contains(&xorb).await);
    assert_eq!(
        cache.cached_size(&xorb).await.unwrap(),
        Some(data.len() as u64)
    );
    assert!(
        cache
            .get_xorb_range_if_present(&hash, 0..data.len() as u64 + 1)
            .await
            .is_none()
    );
    assert_eq!(
        cache.cached_manifest_etag("manifest").await.as_deref(),
        Some("version")
    );
    assert!(cache.try_read_key_limited(&manifest, None).await.is_some());
    let stale = CacheKey::Manifest {
        name: "manifest".into(),
        etag: Some("other".into()),
    };
    assert!(cache.try_read_key_limited(&stale, None).await.is_none());
    for file in files {
        assert_eq!(file.metadata().unwrap().modified().unwrap(), old_time);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn validated_read_updates_only_original_file_recency() {
    for replace_root in std::iter::once(false).chain(cfg!(unix).then_some(true)) {
        let (temp, cache) = temp_cache();
        let data = Bytes::from_static(b"data");
        let key = CacheKey::Chunk(compute_data_hash(&data));
        cache.put(&key, &data).await.unwrap();
        let path = cache.hash_path(&key);
        let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(60);
        let original = std::fs::File::open(&path).unwrap();
        original.set_modified(old_time).unwrap();
        let read_root = cache.root.clone();
        let read_path = path.clone();
        let (validated, ready) = tokio::sync::oneshot::channel();
        let (resume, wait) = std::sync::mpsc::channel();
        let reader = tokio::spawn(async move {
            let (result, entry) =
                read_file_bounded_result(&read_root, &read_path, 4, move |bytes| {
                    verify_data_hash(bytes, &compute_data_hash(b"data"))?;
                    validated.send(()).unwrap();
                    wait.recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                    Ok(())
                })
                .await
                .unwrap()
                .unwrap();
            entry.touch().await;
            result
        });
        ready.await.unwrap();
        if replace_root {
            std::fs::rename(&cache.root, temp.path().join("moved")).unwrap();
        }
        cache.put(&key, &data).await.unwrap();
        let replacement = std::fs::File::open(&path).unwrap();
        replacement.set_modified(old_time).unwrap();
        resume.send(()).unwrap();
        assert_eq!(reader.await.unwrap(), data);
        assert_eq!(
            replacement.metadata().unwrap().modified().unwrap(),
            old_time
        );
        assert!(original.metadata().unwrap().modified().unwrap() > old_time);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bounded_validation_cannot_remove_a_later_publication() {
    let (hash, xorb) = test_xorb(b"data");
    for (key, data) in [
        (
            CacheKey::Chunk(compute_data_hash(b"data")),
            Bytes::from_static(b"data"),
        ),
        (
            CacheKey::Shard(compute_data_hash(b"data")),
            Bytes::from_static(b"data"),
        ),
        (CacheKey::Xorb(hash), xorb),
    ] {
        for replace_root in std::iter::once(false).chain(cfg!(unix).then_some(true)) {
            let (temp, cache) = temp_cache();
            cache.put(&key, &data).await.unwrap();
            let path = cache.hash_path(&key);
            std::fs::write(&path, b"bad").unwrap();
            let read_root = cache.root.clone();
            let read_path = path.clone();
            let read_key = key.clone();
            let (failed, ready) = tokio::sync::oneshot::channel();
            let (resume, wait) = std::sync::mpsc::channel();
            let reader = tokio::spawn(async move {
                read_file_bounded_result(
                    &read_root,
                    &read_path,
                    MAX_XORB_SIZE as u64,
                    move |bytes| {
                        let error = match read_key {
                            CacheKey::Chunk(hash) | CacheKey::Shard(hash) => {
                                verify_data_hash(bytes, &hash)
                            }
                            CacheKey::Xorb(hash) => verify_xorb_identity(bytes, &hash),
                            _ => unreachable!(),
                        }
                        .unwrap_err();
                        failed.send(()).unwrap();
                        wait.recv_timeout(std::time::Duration::from_secs(10))
                            .unwrap();
                        Err(error)
                    },
                )
                .await
            });
            ready.await.unwrap();
            let moved = temp.path().join("moved");
            if replace_root {
                std::fs::rename(&cache.root, &moved).unwrap();
            }
            cache.put(&key, &data).await.unwrap();
            let before = CacheCatalog::read_only_stats(&cache.root).unwrap();
            resume.send(()).unwrap();
            assert!(reader.await.unwrap().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), data);
            assert_eq!(CacheCatalog::read_only_stats(&cache.root).unwrap(), before);
            if replace_root {
                assert!(!moved.join(path.strip_prefix(&cache.root).unwrap()).exists());
                assert_eq!(CacheCatalog::read_only_stats(&moved).unwrap().entries, 0);
            }
        }
    }
}

#[tokio::test]
async fn xorb_read_variants_retire_the_failed_file_and_accounting() {
    let (hash, data) = test_xorb(b"data");
    for mode in ["body", "metadata", "range", "payload"] {
        let (_temp, cache) = temp_cache();
        cache.put_read_xorb(&hash, data.clone()).await.unwrap();
        let path = cache.hash_path(&CacheKey::Xorb(hash));
        std::fs::write(&path, b"bad").unwrap();
        match mode {
            "body" => assert!(cache.get_read_xorb_if_present(&hash).await.is_err()),
            "metadata" => assert!(cache.get_xorb_metadata_if_present(&hash).await.is_err()),
            "range" => assert!(cache.get_xorb_range_if_present(&hash, 0..1).await.is_none()),
            "payload" => assert!(!cache.contains_verified(&CacheKey::Xorb(hash)).await),
            _ => unreachable!(),
        }
        assert!(!path.exists(), "{mode}");
        assert_eq!(
            CacheCatalog::read_only_stats(&cache.root).unwrap().entries,
            0,
            "{mode}"
        );
    }
}

#[tokio::test]
async fn oversized_logical_body_repair_does_not_remove_its_etag() {
    for key in [
        CacheKey::Stage(crab_types::workflow::StageHash([7; 32])),
        CacheKey::Manifest {
            name: "manifest".into(),
            etag: Some("version".into()),
        },
    ] {
        let (_temp, cache) = temp_cache();
        cache.put(&key, b"data").await.unwrap();
        assert!(cache.try_read_key_limited(&key, Some(1)).await.is_none());
        assert!(!cache.hash_path(&key).exists());
        if let CacheKey::Manifest { name, .. } = key {
            assert_eq!(
                cache.cached_manifest_etag(&name).await.as_deref(),
                Some("version")
            );
        }
    }
}

#[tokio::test]
async fn fresh_xorb_repair_preserves_live_readers_then_retires_corruption() {
    let (_temp, cache) = temp_cache();
    let (hash, data) = test_xorb(b"data");
    cache.put_read_xorb(&hash, data).await.unwrap();
    let path = cache.hash_path(&CacheKey::Xorb(hash));
    std::fs::write(&path, b"bad").unwrap();
    let reader = crate::private_fs::open_read(&cache.root, &path)
        .await
        .unwrap();
    let before = CacheCatalog::read_only_stats(&cache.root).unwrap();
    assert!(matches!(cache.evict_corrupt_xorb(&hash).await,
        Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock));
    assert_eq!(CacheCatalog::read_only_stats(&cache.root).unwrap(), before);
    drop(reader);
    cache.evict_corrupt_xorb(&hash).await.unwrap();
    assert!(!path.exists());
    assert_eq!(
        CacheCatalog::read_only_stats(&cache.root).unwrap().entries,
        0
    );
}
