//! Live ordered-range transfer proof against an existing immutable S3 object.
use crab_storage::{StorageProviderKind, StorageReadKind, build_static_env_store};
use futures_util::StreamExt as _;
use object_store::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a live S3 bucket and a known immutable object larger than 512 MiB"]
async fn parallel_download_preserves_the_complete_immutable_body() {
    let bucket = std::env::var("CRAB_TEST_BUCKET").unwrap();
    let path = Path::from(std::env::var("CRAB_TEST_DOWNLOAD_KEY").unwrap());
    let expected =
        blake3::Hash::from_hex(std::env::var("CRAB_TEST_DOWNLOAD_BLAKE3").unwrap()).unwrap();
    let store = build_static_env_store(&bucket, StorageProviderKind::S3).unwrap();
    let size = store.head(&path).await.unwrap().size;
    assert!(size > 512 * 1024 * 1024);
    let reads = Arc::new(AtomicUsize::new(0));
    let observer = reads.clone();
    let store = store.with_read_request_observer(Arc::new(move |kind| {
        if kind == StorageReadKind::Stream {
            observer.fetch_add(1, Ordering::SeqCst);
        }
    }));
    for parallel in [false, true] {
        reads.store(0, Ordering::SeqCst);
        let start = std::time::Instant::now();
        let cancel = CancellationToken::new();
        let (meta, range, mut body) = if parallel {
            store
                .get_parallel_stream(&path, size, None, &cancel)
                .await
                .unwrap()
        } else {
            store.get_stream(&path, None).await.unwrap()
        };
        assert_eq!((meta.size, range), (size, 0..size));
        let mut hash = blake3::Hasher::new();
        let mut bytes = 0u64;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.unwrap();
            bytes += chunk.len() as u64;
            hash.update(&chunk);
        }
        assert_eq!((bytes, hash.finalize()), (size, expected));
        let requests = reads.load(Ordering::SeqCst);
        assert_eq!(
            requests,
            if parallel {
                size.div_ceil(32 * 1024 * 1024) as usize
            } else {
                1
            }
        );
        println!(
            "{}",
            serde_json::json!({"parallel":parallel,"bytes":bytes,"stream_requests":requests,"elapsed_s":start.elapsed().as_secs_f64(),"complete_hash_verified":true})
        );
    }
    reads.store(0, Ordering::SeqCst);
    let start = std::time::Instant::now();
    let body = store.get_immutable_bounded(&path, size).await.unwrap();
    assert_eq!((body.len() as u64, blake3::hash(&body)), (size, expected));
    assert_eq!(
        reads.load(Ordering::SeqCst),
        size.div_ceil(32 * 1024 * 1024) as usize
    );
    println!(
        "{}",
        serde_json::json!({"materialized":true, "bytes":size, "elapsed_s":start.elapsed().as_secs_f64(), "stream_requests":reads.load(Ordering::SeqCst), "complete_hash_verified":true})
    );
}
