use std::sync::Arc;

use bytes::Bytes;
use object_store::memory::InMemory;
use sha2::{Digest, Sha256};

use super::*;
use crab_storage::{RetryPolicy, Store};

fn test_lfs_store() -> LfsObjectStore {
    let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
    let policy = RetryPolicy {
        max_attempts: 2,
        base: std::time::Duration::from_millis(1),
        cap: std::time::Duration::from_millis(5),
    };
    let store = Store::with_retry(inner, policy);
    LfsObjectStore::new(store, "repo")
}

fn sha256_oid(data: &[u8]) -> [u8; 32] {
    let hash = Sha256::digest(data);
    let mut oid = [0u8; 32];
    oid.copy_from_slice(&hash);
    oid
}

fn make_pointer(data: &[u8]) -> LfsPointer {
    LfsPointer {
        oid: sha256_oid(data),
        size: data.len() as u64,
        extensions: Vec::new(),
    }
}

fn test_config() -> LfsConfig {
    LfsConfig {
        concurrent_transfers: 4,
        ..LfsConfig::default()
    }
}

#[tokio::test]
async fn caller_cancellation_rejects_empty_and_nonempty_batch_operations() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &cancel,
    );
    let pointer = make_pointer(b"cancelled batch");
    cancel.cancel();
    for pointers in [&[][..], std::slice::from_ref(&pointer)] {
        assert!(matches!(
            resolver.find_missing_for_push(pointers).await,
            Err(CrabError::Cancelled)
        ));
        assert!(matches!(
            resolver.upload_missing(pointers).await,
            Err(CrabError::Cancelled)
        ));
        assert!(matches!(
            resolver.download_missing(pointers).await,
            Err(CrabError::Cancelled)
        ));
        assert!(matches!(
            resolver.download_objects(pointers, true).await,
            Err(CrabError::Cancelled)
        ));
    }
    assert!(matches!(
        resolver.find_missing_for_fetch(&[("asset.bin".into(), pointer.clone())], None, None),
        Err(CrabError::Cancelled)
    ));
    assert!(!store.exists(&pointer.oid).await.unwrap());
    assert!(!local_object_path_for(dir.path(), &pointer.oid).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_download_drains_without_installing_its_temporary_file() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    use std::future::Future as _;

    let inner = Arc::new(InMemory::new());
    let data = Bytes::from_static(b"cancelled before cache installation");
    let pointer = make_pointer(&data);
    LfsObjectStore::new(Store::new(inner.clone()), "repo")
        .put(&pointer.oid, data)
        .await
        .unwrap();
    let store = Arc::new(LfsObjectStore::new(
        Store::new(Arc::new(ThrottledStore::new(
            inner,
            ThrottleConfig {
                wait_get_per_call: std::time::Duration::from_secs(1),
                ..ThrottleConfig::default()
            },
        ))),
        "repo",
    ));
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let resolver = BatchResolver::new(store, dir.path().to_path_buf(), test_config(), &cancel);
    let download = resolver.download_missing(std::slice::from_ref(&pointer));
    tokio::pin!(download);
    std::future::poll_fn(|cx| {
        assert!(download.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(
        std::fs::read_dir(dir.path().join("tmp")).unwrap().count(),
        1
    );
    cancel.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), download)
        .await
        .unwrap();
    assert!(matches!(result, Err(CrabError::Cancelled)));
    assert!(!local_object_path_for(dir.path(), &pointer.oid).exists());
    assert_eq!(
        std::fs::read_dir(dir.path().join("tmp")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn find_missing_for_push_returns_absent_objects() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    let data_a = b"object-a";
    let data_b = b"object-b";
    let ptr_a = make_pointer(data_a);
    let ptr_b = make_pointer(data_b);

    // Upload object A to remote so it's present.
    store
        .put(&ptr_a.oid, Bytes::from(data_a.to_vec()))
        .await
        .unwrap();

    let missing = resolver
        .find_missing_for_push(&[ptr_a.clone(), ptr_b.clone()])
        .await
        .unwrap();

    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].oid, ptr_b.oid);
}

#[tokio::test]
async fn find_missing_for_fetch_applies_include_filter() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    let ptr = make_pointer(b"some-data");
    let entries = vec![
        ("models/large.bin".to_string(), ptr.clone()),
        ("docs/readme.md".to_string(), ptr.clone()),
    ];

    let include = PatternFilter::new("*.bin").unwrap();
    let missing = resolver
        .find_missing_for_fetch(&entries, Some(&include), None)
        .unwrap();

    assert_eq!(missing.len(), 1);
}

#[tokio::test]
async fn find_missing_for_fetch_applies_exclude_filter() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    let ptr = make_pointer(b"some-data");
    let entries = vec![
        ("models/large.bin".to_string(), ptr.clone()),
        ("docs/readme.md".to_string(), ptr.clone()),
    ];

    let exclude = PatternFilter::new("*.md").unwrap();
    let missing = resolver
        .find_missing_for_fetch(&entries, None, Some(&exclude))
        .unwrap();

    // Both point to the same OID, but readme.md is excluded.
    // Since the OID is the same and not local, only the .bin entry passes.
    assert_eq!(missing.len(), 1);
}

#[tokio::test]
async fn find_missing_for_fetch_skips_locally_present() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    let data = b"local-object";
    let ptr = make_pointer(data);

    // Write the object to local storage.
    let local_path = local_object_path_for(dir.path(), &ptr.oid);
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, data).unwrap();

    let entries = vec![("file.bin".to_string(), ptr)];
    let missing = resolver
        .find_missing_for_fetch(&entries, None, None)
        .unwrap();

    assert!(missing.is_empty());
}

#[tokio::test]
async fn find_missing_for_fetch_refetches_corrupt_local_object() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );
    let ptr = make_pointer(b"valid-content");
    let local_path = local_object_path_for(dir.path(), &ptr.oid);
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(local_path, b"corrupt").unwrap();

    let missing = resolver
        .find_missing_for_fetch(&[("file.bin".to_owned(), ptr.clone())], None, None)
        .unwrap();

    assert_eq!(missing, vec![ptr]);
}

#[tokio::test]
async fn upload_missing_transfers_objects() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    let data = b"upload-me";
    let ptr = make_pointer(data);

    // Write the object to local storage so upload can read it.
    let local_path = local_object_path_for(dir.path(), &ptr.oid);
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, data).unwrap();

    resolver.upload_missing(&[ptr.clone()]).await.unwrap();

    // Verify it's now on the remote.
    assert!(store.exists(&ptr.oid).await.unwrap());
    let downloaded = store.get(&ptr.oid).await.unwrap();
    assert_eq!(&downloaded[..], data);
}

#[tokio::test]
async fn upload_missing_skips_already_present() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    let data = b"already-there";
    let ptr = make_pointer(data);

    // Pre-upload to remote.
    store
        .put(&ptr.oid, Bytes::from(data.to_vec()))
        .await
        .unwrap();

    // No local file needed — the upload should be skipped entirely.
    resolver.upload_missing(&[ptr]).await.unwrap();
}

#[tokio::test]
async fn download_missing_transfers_objects() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    let data = b"download-me";
    let ptr = make_pointer(data);

    // Upload to remote so download can fetch it.
    store
        .put(&ptr.oid, Bytes::from(data.to_vec()))
        .await
        .unwrap();

    resolver.download_missing(&[ptr.clone()]).await.unwrap();

    // Verify it's now in local storage.
    let local_path = local_object_path_for(dir.path(), &ptr.oid);
    assert!(local_path.is_file());
    let content = std::fs::read(&local_path).unwrap();
    assert_eq!(&content[..], data);
}

#[tokio::test]
async fn download_missing_skips_already_present() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    let data = b"already-local";
    let ptr = make_pointer(data);

    // Write to local storage.
    let local_path = local_object_path_for(dir.path(), &ptr.oid);
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, data).unwrap();

    // No remote object needed — the download should be skipped.
    resolver.download_missing(&[ptr]).await.unwrap();
}

#[tokio::test]
async fn download_objects_refetch_overwrites_present_object() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    let data = b"remote-content";
    let ptr = make_pointer(data);
    store
        .put(&ptr.oid, Bytes::from(data.to_vec()))
        .await
        .unwrap();

    let local_path = local_object_path_for(dir.path(), &ptr.oid);
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, b"stale").unwrap();

    resolver
        .download_objects(&[ptr.clone()], true)
        .await
        .unwrap();

    let content = std::fs::read(&local_path).unwrap();
    assert_eq!(&content[..], data);
}

#[tokio::test]
async fn upload_missing_empty_list_is_noop() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    resolver.upload_missing(&[]).await.unwrap();
}

#[tokio::test]
async fn download_missing_empty_list_is_noop() {
    let store = Arc::new(test_lfs_store());
    let dir = tempfile::tempdir().unwrap();
    let resolver = BatchResolver::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );

    resolver.download_missing(&[]).await.unwrap();
}

#[tokio::test]
async fn pattern_filter_comma_separated() {
    let filter = PatternFilter::new("*.bin, *.dat").unwrap();
    assert!(filter.matches("model.bin"));
    assert!(filter.matches("data.dat"));
    assert!(!filter.matches("readme.md"));
}

#[tokio::test]
async fn upload_download_round_trip() {
    let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
    let policy = RetryPolicy {
        max_attempts: 2,
        base: std::time::Duration::from_millis(1),
        cap: std::time::Duration::from_millis(5),
    };

    let store = Arc::new(LfsObjectStore::new(
        Store::with_retry(Arc::clone(&inner), policy.clone()),
        "repo",
    ));

    let upload_dir = tempfile::tempdir().unwrap();
    let download_dir = tempfile::tempdir().unwrap();

    let data = b"round-trip-content";
    let ptr = make_pointer(data);

    // Write to upload-side local storage.
    let upload_path = local_object_path_for(upload_dir.path(), &ptr.oid);
    std::fs::create_dir_all(upload_path.parent().unwrap()).unwrap();
    std::fs::write(&upload_path, data).unwrap();

    // Upload.
    let uploader = BatchResolver::new(
        Arc::clone(&store),
        upload_dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );
    uploader.upload_missing(&[ptr.clone()]).await.unwrap();

    // Download to a different local dir.
    let downloader = BatchResolver::new(
        Arc::clone(&store),
        download_dir.path().to_path_buf(),
        test_config(),
        &CancellationToken::new(),
    );
    downloader.download_missing(&[ptr.clone()]).await.unwrap();

    let downloaded_path = local_object_path_for(download_dir.path(), &ptr.oid);
    let content = std::fs::read(&downloaded_path).unwrap();
    assert_eq!(&content[..], data);
}

#[test]
fn local_object_path_format() {
    let dir = PathBuf::from("/tmp/lfs");
    let mut oid = [0u8; 32];
    oid[0] = 0xab;
    oid[1] = 0xcd;
    let path = local_object_path_for(&dir, &oid);
    let path_str = path.to_string_lossy();
    let hex = hex_encode(&oid);
    assert!(path_str.contains("/objects/ab/cd/"));
    assert!(path_str.ends_with(&hex));
}
