use super::*;
use bytes::Bytes;
use crab_lfs::LfsObjectStore;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_stops_lfs_materialization_before_headers_and_during_body() {
    for to_file in [true, false] {
        for before_headers in [true, false] {
            let (reader, snapshot, counted, headers, slow, directory) =
                super::test_support::lfs_snapshot(512 * 1024).await;
            slow.config_mut(|config| {
                if before_headers {
                    config.wait_get_per_call = Duration::from_secs(30);
                } else {
                    config.wait_get_per_byte = Duration::from_micros(100);
                }
            });
            let destination = directory.path().join("result");
            std::fs::write(&destination, b"original").unwrap();
            let read = async {
                if to_file {
                    snapshot.download_to_path("file.lfs", &destination).await
                } else {
                    snapshot.write_to_writer("file.lfs", std::io::sink()).await
                }
            };
            let cancel = async {
                tokio::time::timeout(Duration::from_secs(2), async {
                    // The inner counter runs after the header throttle; InMemory
                    // metadata returns immediately before the delayed body is polled.
                    let entered = if before_headers { &counted } else { &headers };
                    while entered.counts().full == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                snapshot.repo.inner.cancel.cancel();
            };
            let (result, ()) =
                tokio::join!(tokio::time::timeout(Duration::from_secs(3), read), cancel);
            reader.shutdown().await;
            assert!(
                matches!(result, Ok(Err(CrabError::Cancelled))),
                "to_file={to_file}, before_headers={before_headers}: {result:?}"
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lfs_path_streams_privately_before_verified_publication() {
    let (reader, snapshot, counted, _, slow, directory) =
        super::test_support::lfs_snapshot(512 * 1024).await;
    slow.config_mut(|config| config.wait_get_per_byte = Duration::from_micros(1));
    let destination = directory.path().join("result");
    std::fs::write(&destination, b"original").unwrap();
    let progress = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let streaming = std::fs::read_dir(directory.path())
                    .unwrap()
                    .filter_map(|entry| entry.ok())
                    .any(|entry| {
                        let path = entry.path();
                        path != destination
                            && path.is_file()
                            && entry
                                .metadata()
                                .is_ok_and(|meta| meta.len() > 0 && meta.len() < 512 * 1024)
                    });
                if streaming {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
    };
    let (result, ()) = tokio::join!(
        snapshot.download_to_path("file.lfs", &destination),
        progress
    );
    reader.shutdown().await;
    assert_eq!(result.unwrap(), 512 * 1024);
    assert_eq!(std::fs::read(&destination).unwrap(), vec![0x5a; 512 * 1024]);
    assert_eq!(counted.counts().full, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_lfs_preserves_existing_destination_and_integrity_diagnostic() {
    for to_file in [true, false] {
        let (reader, snapshot, _, _, _, directory) =
            super::test_support::lfs_snapshot(512 * 1024).await;
        let remote = snapshot.repo.inner.remote.get().unwrap();
        let mut content = vec![0x5a; 512 * 1024];
        use sha2::Digest as _;
        let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
        *content.last_mut().unwrap() ^= 1;
        let lfs = LfsObjectStore::new(
            remote.caching_store.origin().clone(),
            remote.router.repo_prefix(),
        );
        remote
            .caching_store
            .origin()
            .put_overwrite(&lfs.object_path_for(&oid), Bytes::from(content))
            .await
            .unwrap();
        let destination = directory.path().join("result");
        std::fs::write(&destination, b"original").unwrap();
        let result = if to_file {
            snapshot.download_to_path("file.lfs", &destination).await
        } else {
            snapshot.write_to_writer("file.lfs", std::io::sink()).await
        };
        reader.shutdown().await;
        assert!(matches!(result, Err(CrabError::CorruptObject { .. })));
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_file())
                .count(),
            1
        );
    }
}
