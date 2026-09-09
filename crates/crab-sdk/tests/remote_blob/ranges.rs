use crab_sdk::{
    ContentMode, ErrorKind, GitPath, HistoryTraversal, PageRequest, ReadLimits, ReadOptions,
    Revision, Snapshot,
};

fn invalid<T>(result: crab_sdk::Result<T>) {
    assert_eq!(result.err().unwrap().kind(), ErrorKind::InvalidInput);
}

pub(super) async fn verify(snapshot: &Snapshot, name: &[u8], original: &[u8]) {
    let path = GitPath::new(name.to_vec()).unwrap();
    let size = original.len() as u64;
    for range in [
        0..0,
        0..1,
        1..size - 1,
        65530..65547,
        size..size,
        0..size,
        size - 1..size,
    ] {
        let options = ReadOptions::default().with_range(range.clone()).unwrap();
        let expected = &original[range.start as usize..range.end as usize];
        assert_eq!(
            snapshot
                .read_blob(path.clone())
                .with_options(options.clone())
                .await
                .unwrap()
                .as_ref(),
            expected
        );
        let mut stream = snapshot
            .open_file(path.clone())
            .with_options(options.clone())
            .await
            .unwrap();
        let mut actual = Vec::new();
        while let Some(chunk) = stream.next().await.unwrap() {
            actual.extend_from_slice(&chunk);
        }
        assert_eq!(actual, expected);
        stream.close().await.unwrap();
    }
    for range in [size..size + 1, size + 1..size + 1, 0..u64::MAX] {
        let options = ReadOptions::default().with_range(range).unwrap();
        invalid(
            snapshot
                .read_blob(path.clone())
                .with_options(options.clone())
                .await,
        );
        invalid(
            snapshot
                .open_file(path.clone())
                .with_options(options.clone())
                .await,
        );
    }
    invalid(ReadOptions::default().with_range(std::ops::Range { start: 2, end: 1 }));
    let options = ReadOptions::default().with_range(0..0).unwrap();
    invalid(snapshot.commit().with_options(options.clone()).await);
    invalid(
        snapshot
            .tree(GitPath::root(), PageRequest::new(1, None).unwrap())
            .with_options(options.clone())
            .await,
    );
    invalid(
        snapshot
            .history(
                HistoryTraversal::AllParents,
                PageRequest::new(1, None).unwrap(),
            )
            .with_options(options.clone())
            .await,
    );
    invalid(
        snapshot
            .diff(
                Revision::commit(snapshot.commit_id().unwrap()),
                path.clone(),
            )
            .with_options(options.clone())
            .await,
    );
    invalid(snapshot.blame(path).with_options(options.clone()).await);
    invalid(
        snapshot
            .archive(ContentMode::Git)
            .with_options(options.clone())
            .await,
    );
}

// Run after successful reads above so semantic/object caches are already warm.
pub(super) async fn verify_limits(snapshot: &Snapshot, name: &[u8]) {
    let path = GitPath::new(name.to_vec()).unwrap();
    let options = ReadOptions::default()
        .with_limits(ReadLimits {
            max_response_bytes: 1,
            ..ReadLimits::default()
        })
        .unwrap();
    assert_eq!(
        snapshot
            .read_blob(path.clone())
            .with_options(options.clone())
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::LimitExceeded,
    );
    let error = snapshot
        .open_file(path.clone())
        .with_options(options.clone())
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::LimitExceeded);
    // A failed operation must not tighten the next operation's budget.
    assert!(snapshot.read_blob(path).await.is_ok());
}

#[cfg(feature = "content")]
pub(super) async fn verify_hydration_limit(
    snapshot: &Snapshot,
    path: GitPath,
    options: ReadOptions,
) {
    let expected = snapshot.read_blob(path.clone()).await.unwrap();
    let requested = options.read_limits();
    let bytes = requested.max_fetched_bytes == 1;
    if !bytes {
        assert_eq!(requested.max_storage_requests, 1);
    }
    let limits_for = |value| {
        let mut limits = requested;
        if bytes {
            limits.max_fetched_bytes = value;
        } else {
            limits.max_storage_requests = value;
        }
        limits
    };
    // Account for real locator reads even when the Git pointer is cached.
    // Find its exact admission floor, leaving no arbitrary hydration headroom.
    let mut lower = 1;
    let mut upper = if bytes {
        ReadLimits::default().max_fetched_bytes
    } else {
        ReadLimits::default().max_storage_requests
    };
    while lower < upper {
        let middle = lower + (upper - lower) / 2;
        let options = ReadOptions::default()
            .with_limits(limits_for(middle))
            .unwrap();
        match snapshot.read_blob(path.clone()).with_options(options).await {
            Ok(_) => upper = middle,
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::LimitExceeded);
                lower = middle + 1;
            }
        }
    }
    let limits = limits_for(lower);
    let raw_options = ReadOptions::default().with_limits(limits).unwrap();
    let options = options.with_limits(limits).unwrap();
    // Rejection must arise in the additional content read, not pointer resolution.
    assert_eq!(
        snapshot
            .read_blob(path.clone())
            .with_options(raw_options.clone())
            .await
            .unwrap(),
        expected
    );
    let error = match snapshot.open_file(path).with_options(options.clone()).await {
        Err(error) => error,
        Ok(mut stream) => {
            let error = loop {
                match stream.next().await {
                    Err(error) => break error,
                    Ok(Some(_)) => continue,
                    Ok(None) => panic!("hydration escaped its origin budget"),
                }
            };
            stream.close().await.unwrap();
            error
        }
    };
    assert_eq!(error.kind(), ErrorKind::LimitExceeded);
}

pub(super) async fn verify_controls(snapshot: &Snapshot, name: &[u8]) {
    use crab_sdk::{Cancellation, OperationOptions};
    let path = GitPath::new(name.to_vec()).unwrap();
    let cancellation = Cancellation::default();
    cancellation.cancel();
    for (operation, expected) in [
        (
            OperationOptions::default().with_cancellation(cancellation),
            ErrorKind::Cancelled,
        ),
        (
            OperationOptions::default().with_deadline(std::time::Instant::now()),
            ErrorKind::Timeout,
        ),
    ] {
        let options = ReadOptions::default().with_operation(operation);
        assert_eq!(
            snapshot
                .commit()
                .with_options(options.clone())
                .await
                .err()
                .unwrap()
                .kind(),
            expected
        );
        assert_eq!(
            snapshot
                .read_blob(path.clone())
                .with_options(options.clone())
                .await
                .err()
                .unwrap()
                .kind(),
            expected
        );
        assert_eq!(
            snapshot
                .open_file(path.clone())
                .with_options(options.clone())
                .await
                .err()
                .unwrap()
                .kind(),
            expected
        );
        assert_eq!(
            snapshot
                .archive(ContentMode::Git)
                .with_options(options)
                .await
                .err()
                .unwrap()
                .kind(),
            expected
        );
    }
    let cancellation = Cancellation::default();
    let options = ReadOptions::default()
        .with_operation(OperationOptions::default().with_cancellation(cancellation.clone()));
    let mut stream = snapshot
        .open_file(path.clone())
        .with_options(options)
        .await
        .unwrap();
    let id = stream.operation_id();
    assert!(stream.next().await.unwrap().is_some());
    cancellation.cancel();
    let error = loop {
        match stream.next().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("cancelled stream reported complete integrity"),
            Err(error) => break error,
        }
    };
    assert_eq!(
        (error.kind(), error.operation_id(), stream.operation_id()),
        (ErrorKind::Cancelled, Some(id), id)
    );
    stream.close().await.unwrap();
    assert!(snapshot.read_blob(path).await.is_ok());
}

pub(super) async fn verify_progress(snapshot: &Snapshot, name: &[u8]) {
    use crab_sdk::{OperationOptions, Progress, ProgressEvent, ProgressUpdate};
    let (progress, mut receive) = Progress::channel();
    let options =
        ReadOptions::default().with_operation(OperationOptions::default().with_progress(progress));
    let mut stream = snapshot
        .open_file(GitPath::new(name.to_vec()).unwrap())
        .with_options(options)
        .await
        .unwrap();
    let id = stream.operation_id();
    let (mut bytes, mut items) = (0, 0);
    while let Some(chunk) = stream.next().await.unwrap() {
        bytes += chunk.len() as u64;
        items += 1;
    }
    stream.close().await.unwrap();
    assert_eq!(
        receive.next().await,
        Some(ProgressEvent {
            operation_id: id,
            update: ProgressUpdate::Delivered { bytes, items },
        })
    );
    assert_eq!(receive.next().await, None);

    let (progress, mut receive) = Progress::channel();
    let options =
        ReadOptions::default().with_operation(OperationOptions::default().with_progress(progress));
    let mut stream = snapshot
        .archive(ContentMode::Git)
        .with_options(options)
        .await
        .unwrap();
    let id = stream.operation_id();
    let (mut bytes, mut items) = (0, 0);
    while let Some(entry) = stream.next().await.unwrap() {
        if let crab_sdk::ArchiveEvent::Data(value) = entry {
            bytes += value.len() as u64;
        }
        items += 1;
    }
    stream.close().await.unwrap();
    assert_eq!(
        receive.next().await,
        Some(ProgressEvent {
            operation_id: id,
            update: ProgressUpdate::Delivered { bytes, items },
        })
    );
    assert_eq!(receive.next().await, None);
}
