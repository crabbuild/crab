use super::*;

#[tokio::test]
async fn verified_fetch_survives_cache_write_failure_for_every_family() {
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
        (
            CacheKey::Stage(crab_types::workflow::StageHash([5; 32])),
            Bytes::from_static(b"stage"),
        ),
        (
            CacheKey::Manifest {
                name: "manifest".into(),
                etag: Some("version".into()),
            },
            Bytes::from_static(b"manifest"),
        ),
    ] {
        let (_temp, cache) = temp_cache();
        std::fs::write(cache.root(), b"retain this file").unwrap();
        let calls = AtomicUsize::new(0);
        let value = cache
            .get_or_fetch_bounded_with(&key, data.len() as u64, || async {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok::<_, CacheError>(data.clone())
            })
            .await
            .unwrap();
        assert_eq!(value, data, "{key:?}");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "{key:?}");
        assert_eq!(std::fs::read(cache.root()).unwrap(), b"retain this file");
        assert!(
            cache.put_bytes(&key, data).await.is_err(),
            "explicit write remains fallible"
        );
    }
}

#[tokio::test]
async fn unavailable_cache_does_not_hide_fetch_errors_or_invalid_origin_bytes() {
    let (_temp, cache) = temp_cache();
    std::fs::write(cache.root(), b"retained").unwrap();
    let key = CacheKey::Shard(compute_data_hash(b"valid"));
    let result = cache
        .get_or_fetch(&key, || async { Err(CacheError::Cancelled) })
        .await;
    assert!(matches!(result, Err(CacheError::Cancelled)));
    let result = cache
        .get_or_fetch(&key, || async { Ok(Bytes::from_static(b"wrong")) })
        .await;
    assert!(matches!(result, Err(CacheError::HashMismatch { .. })));
    let result = cache
        .get_or_fetch_bounded_with(&key, 1, || async {
            Ok::<_, CacheError>(Bytes::from_static(b"valid"))
        })
        .await;
    assert!(matches!(result, Err(CacheError::CorruptObject { .. })));
    assert_eq!(std::fs::read(cache.root()).unwrap(), b"retained");
}
