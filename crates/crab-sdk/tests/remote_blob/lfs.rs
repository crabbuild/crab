use crab_sdk::operation::ReadOptions;
use crab_sdk::remote::Snapshot;
use crab_sdk::{ErrorKind, GitPath};
use futures_util::TryStreamExt;

pub(super) async fn verify_origin(
    options: crab_sdk::storage::DirectStoreOptions,
    storage: &crab_storage::Store,
    oid: [u8; 32],
    original: &[u8],
) {
    let directory = tempfile::tempdir().unwrap();
    let client = crab_sdk::Client::builder()
        .direct_store(options)
        .content_cache(
            crab_sdk::storage::ContentCache::new(&directory.path().join("cache"), 0).unwrap(),
        )
        .build()
        .unwrap();
    let snapshot = open_snapshot(&client).await;
    verify(&snapshot, storage, oid, original).await;
    client.close().await.unwrap();
}

async fn open_snapshot(client: &crab_sdk::Client) -> Snapshot {
    client
        .open(crab_sdk::OpenOptions::remote(
            crab_sdk::RepositoryLocator::new("repository").unwrap(),
        ))
        .await
        .unwrap()
        .remote()
        .unwrap()
        .snapshot(crab_sdk::Revision::branch("main").unwrap())
        .await
        .unwrap()
}

async fn read(snapshot: &Snapshot, range: std::ops::Range<u64>) -> crab_sdk::Result<Vec<u8>> {
    let mut stream = snapshot
        .open_file(GitPath::new("z-lfs").unwrap())
        .with_options(ReadOptions::default().with_range(range).unwrap())
        .await?;
    let mut bytes = Vec::new();
    let result = async {
        while let Some(chunk) = stream.next().await? {
            assert!(chunk.len() <= 64 * 1024);
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    .await;
    stream.close().await?;
    result
}

pub(super) async fn verify_retention(
    options: crab_sdk::storage::DirectStoreOptions,
    storage: &crab_storage::Store,
    oid: [u8; 32],
    original: &[u8],
    cache: &std::path::Path,
) {
    let new_client = || {
        crab_sdk::Client::builder()
            .direct_store(options.clone())
            .content_cache(crab_sdk::storage::ContentCache::new(cache, None).unwrap())
            .build()
            .unwrap()
    };
    let object = crab_lfs::LfsObjectStore::object_path_for_prefix("repository", &oid);
    let size = original.len() as u64;
    let first = new_client();
    assert_eq!(
        read(&open_snapshot(&first).await, 0..size).await.unwrap(),
        original
    );
    first.close().await.unwrap();
    storage.delete(&object).await.unwrap();
    let restarted = new_client();
    let snapshot = open_snapshot(&restarted).await;
    for range in [0..size, 65530..65547, size..size] {
        assert_eq!(
            read(&snapshot, range.clone()).await.unwrap(),
            original[range.start as usize..range.end as usize]
        );
    }
    restarted.close().await.unwrap();

    let oid_hex = crab_git::lfs_pointer::hex_encode(&oid);
    let namespace = std::fs::read_dir(cache.join("namespaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let retained = namespace.join("lfs").join(&oid_hex[..2]).join(&oid_hex);
    std::fs::write(&retained, vec![0; original.len()]).unwrap();
    storage
        .put_overwrite(&object, bytes::Bytes::copy_from_slice(original))
        .await
        .unwrap();
    let repaired = new_client();
    assert_eq!(
        read(&open_snapshot(&repaired).await, 0..size)
            .await
            .unwrap(),
        original
    );
    repaired.close().await.unwrap();
    assert_eq!(std::fs::read(&retained).unwrap(), original);

    let catalog = crab_cache::CacheCatalog::new(cache.to_owned(), None);
    catalog.set_retention_budget(Some(0)).await.unwrap();
    catalog.maintain().await.unwrap();
    assert!(!retained.exists());
    let disabled = new_client();
    assert_eq!(
        read(&open_snapshot(&disabled).await, 0..size)
            .await
            .unwrap(),
        original
    );
    disabled.close().await.unwrap();
    assert!(
        !retained.exists(),
        "zero root budget must not retain LFS reads"
    );
}

pub(super) async fn verify(
    snapshot: &Snapshot,
    storage: &crab_storage::Store,
    oid: [u8; 32],
    original: &[u8],
) {
    let path = GitPath::new(b"z-lfs".to_vec()).unwrap();
    let size = original.len() as u64;
    for range in [0..size, 0..0, size..size, 1..size - 1, 65530..65547] {
        let mut stream = snapshot
            .open_file(path.clone())
            .with_options(ReadOptions::default().with_range(range.clone()).unwrap())
            .await
            .unwrap();
        let mut actual = Vec::new();
        while let Some(bytes) = stream.next().await.unwrap() {
            assert!(bytes.len() <= 64 * 1024);
            actual.extend_from_slice(&bytes);
        }
        assert_eq!(actual, original[range.start as usize..range.end as usize]);
        stream.close().await.unwrap();
    }
    // Native captures may resolve Git pointers without origin I/O. A single
    // LFS GET then fits a one-request budget; byte exhaustion still proves the
    // additional content read shares its Git operation's admission.
    let limits = crab_sdk::operation::ReadLimits {
        max_fetched_bytes: 1,
        ..Default::default()
    };
    super::ranges::verify_hydration_limit(
        snapshot,
        path.clone(),
        ReadOptions::default()
            .with_limits(limits)
            .unwrap()
            .with_range(1..size - 1)
            .unwrap(),
    )
    .await;
    let invalid = snapshot
        .open_file(path.clone())
        .with_options(ReadOptions::default().with_range(size..size + 1).unwrap())
        .await
        .err()
        .unwrap();
    assert_eq!(invalid.kind(), ErrorKind::InvalidInput);
    for advance in [false, true] {
        let mut stream = snapshot.open_file(path.clone()).await.unwrap();
        if advance {
            assert!(stream.next().await.unwrap().is_some());
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.close())
            .await
            .unwrap()
            .unwrap();
    }
    let mut stream = snapshot
        .open_file(path.clone())
        .with_options(
            ReadOptions::default()
                .with_timeout(std::time::Duration::from_millis(200))
                .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let error = loop {
        match stream.next().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("idle LFS stream escaped its deadline"),
            Err(error) => break error,
        }
    };
    assert_eq!(error.kind(), ErrorKind::Timeout);
    stream.close().await.unwrap();
    let receipts = object_store::path::Path::from("repository/lfs/receipts");
    assert!(
        storage
            .inner()
            .list(Some(&receipts))
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .is_empty()
    );
    let object = crab_lfs::LfsObjectStore::object_path_for_prefix("repository", &oid);
    let mut corrupt = original.to_vec();
    corrupt[0] ^= 1;
    storage
        .put_overwrite(&object, corrupt.into())
        .await
        .unwrap();
    let mut stream = snapshot.open_file(path.clone()).await.unwrap();
    let error = loop {
        match stream.next().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("corrupt LFS content reached successful EOF"),
            Err(error) => break error,
        }
    };
    assert_eq!(error.kind(), ErrorKind::Corruption);
    super::archive::fails_without_completing_entry(snapshot, b"z-lfs", ErrorKind::Corruption).await;
    stream.close().await.unwrap();
    assert_eq!(
        snapshot
            .open_file(path.clone())
            .with_options(ReadOptions::default().with_range(1..size - 1).unwrap())
            .await
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Corruption
    );
    storage
        .put_overwrite(
            &object,
            bytes::Bytes::copy_from_slice(&original[..original.len() - 1]),
        )
        .await
        .unwrap();
    assert_eq!(
        snapshot.open_file(path.clone()).await.err().unwrap().kind(),
        ErrorKind::Corruption
    );
    storage.delete(&object).await.unwrap();
    super::archive::fails_without_completing_entry(snapshot, b"z-lfs", ErrorKind::NotFound).await;
    assert_eq!(
        snapshot.open_file(path).await.err().unwrap().kind(),
        ErrorKind::NotFound
    );
    storage
        .put_overwrite(&object, bytes::Bytes::copy_from_slice(original))
        .await
        .unwrap();
}
