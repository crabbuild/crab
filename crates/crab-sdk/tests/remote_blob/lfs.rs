use crab_sdk::{ErrorKind, GitPath, ReadOptions, Snapshot};
use futures_util::TryStreamExt;

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
    for limits in [
        crab_sdk::ReadLimits {
            max_fetched_bytes: 1,
            ..Default::default()
        },
        crab_sdk::ReadLimits {
            max_storage_requests: 1,
            ..Default::default()
        },
    ] {
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
    }
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
