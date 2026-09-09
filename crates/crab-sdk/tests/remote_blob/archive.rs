use bytes::Bytes;
use crab_sdk::{ArchiveEvent, ContentMode, Snapshot};

pub(super) async fn collect(snapshot: &Snapshot, mode: ContentMode) -> Vec<(Vec<u8>, Bytes)> {
    let mut stream = snapshot.archive(mode).await.unwrap();
    let mut entries = Vec::new();
    let mut pending = None;
    while let Some(event) = stream.next().await.unwrap() {
        match event {
            ArchiveEvent::Entry { entry, size } => {
                assert!(pending.is_none());
                pending = Some((entry, Vec::new(), size));
            }
            ArchiveEvent::Data(bytes) => {
                assert!(bytes.len() <= 64 * 1024);
                pending.as_mut().unwrap().1.extend_from_slice(&bytes);
            }
            ArchiveEvent::EndEntry => {
                let (entry, bytes, size) = pending.take().unwrap();
                if let Some(size) = size {
                    assert_eq!(bytes.len() as u64, size);
                }
                entries.push((entry.path.as_bytes().to_vec(), bytes.into()));
            }
            _ => panic!("unexpected archive event"),
        }
    }
    assert!(pending.is_none());
    assert!(stream.next().await.unwrap().is_none());
    stream.close().await.unwrap();
    entries
}

#[cfg(feature = "content")]
pub(super) async fn logical_limit(snapshot: &Snapshot, first_size: usize) {
    let options = crab_sdk::ReadOptions::default()
        .with_limits(crab_sdk::ReadLimits {
            max_archive_bytes: first_size as u64 + 1024,
            ..Default::default()
        })
        .unwrap();
    let mut stream = snapshot
        .archive(ContentMode::Hydrated)
        .with_options(options)
        .await
        .unwrap();
    let mut completed = 0;
    let error = loop {
        match stream.next().await {
            Ok(Some(ArchiveEvent::EndEntry)) => completed += 1,
            Ok(Some(_)) => {}
            Ok(None) => panic!("aggregate archive limit was not enforced"),
            Err(error) => break error,
        }
    };
    assert_eq!(
        (completed, error.kind()),
        (2, crab_sdk::ErrorKind::LimitExceeded)
    );
    stream.close().await.unwrap();
}

#[cfg(feature = "content")]
pub(super) async fn fails_without_completing_entry(
    snapshot: &Snapshot,
    path: &[u8],
    kind: crab_sdk::ErrorKind,
) {
    let mut stream = snapshot.archive(ContentMode::Hydrated).await.unwrap();
    let mut pending = None;
    let error = loop {
        match stream.next().await {
            Ok(Some(ArchiveEvent::Entry { entry, .. })) => {
                assert!(pending.is_none());
                pending = Some(entry.path.as_bytes().to_vec());
            }
            Ok(Some(ArchiveEvent::EndEntry)) => {
                assert_ne!(pending.as_deref(), Some(path));
                pending = None;
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("failed archive reached successful EOF"),
            Err(error) => break error,
        }
    };
    assert_eq!((pending.as_deref(), error.kind()), (Some(path), kind));
    stream.close().await.unwrap();
}

#[cfg(feature = "content")]
pub(super) async fn close_during_hydration(snapshot: &Snapshot) {
    for (path, cancel) in [
        (b"z-crab".as_slice(), false),
        (b"z-lfs".as_slice(), false),
        (b"z-crab".as_slice(), true),
        (b"z-lfs".as_slice(), true),
    ] {
        let cancellation = crab_sdk::Cancellation::default();
        let options = crab_sdk::ReadOptions::default().with_operation(
            crab_sdk::OperationOptions::default().with_cancellation(cancellation.clone()),
        );
        let mut stream = snapshot
            .archive(ContentMode::Hydrated)
            .with_options(options)
            .await
            .unwrap();
        let mut pending = Vec::new();
        loop {
            match stream.next().await.unwrap().unwrap() {
                ArchiveEvent::Entry { entry, .. } => pending = entry.path.as_bytes().to_vec(),
                ArchiveEvent::Data(_) if pending == path => break,
                _ => {}
            }
        }
        if cancel {
            cancellation.cancel();
            let error = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    match stream.next().await {
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("cancelled archive reached successful EOF"),
                        Err(error) => break error,
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(error.kind(), crab_sdk::ErrorKind::Cancelled);
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.close())
            .await
            .unwrap()
            .unwrap();
    }
}
