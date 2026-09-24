//! Query and resolve ordering, identity, and fencing.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn query_waits_for_preceding_publication_and_cannot_write() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mutation = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    identity(52),
                    Digest::from_bytes([53; 32]),
                    20,
                    1_024,
                    1_024,
                    move |transaction| {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        transaction.execute("UPDATE counter SET value = value + 1", [])?;
                        Ok(HandlerOutcome::Success(Vec::new()))
                    },
                )
                .await
        })
    };
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();
    let query = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .query(64, 64, |connection| {
                    let value = connection
                        .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                    Ok(value.to_be_bytes().to_vec())
                })
                .await
        })
    };
    release_tx.send(()).unwrap();
    mutation.await.unwrap().unwrap();
    assert_eq!(query.await.unwrap().unwrap(), 1_i64.to_be_bytes());

    assert!(matches!(
        handle.query(1, 1, |_| Ok(vec![0; 2])).await,
        Err(crab_cell_runtime::Error::Command(
            "query result exceeds command limit"
        ))
    ));
    assert!(matches!(
        handle
            .query(64, 64, |connection| {
                connection.execute("UPDATE counter SET value = 99", [])?;
                Ok(Vec::new())
            })
            .await,
        Err(crab_cell_runtime::Error::Sqlite(_))
    ));
    assert_eq!(
        handle
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        1_i64.to_be_bytes()
    );
    handle.drain().await.unwrap();
}
#[tokio::test]
async fn cancelled_command_waiter_is_resolved_by_original_identity() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let request = identity(10);
    let digest = Digest::from_bytes([11; 32]);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let waiting = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(request, digest, 20, 1_024, 1_024, move |transaction| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(b"survived".to_vec()))
                })
                .await
        })
    };
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();
    waiting.abort();
    assert!(matches!(waiting.await, Err(error) if error.is_cancelled()));
    release_tx.send(()).unwrap();

    assert!(matches!(
        handle
            .execute(request, digest, 21, 1_024, 1_024, |_| {
                Ok(HandlerOutcome::Success(b"wrong".to_vec()))
            })
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"survived"
    ));
    handle.drain().await.unwrap();
}
#[tokio::test]
async fn resolve_distinguishes_committed_absent_conflict_and_expired() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let request = identity(54);
    let digest = Digest::from_bytes([55; 32]);
    let outcome = handle
        .execute(request, digest, 20, 1_024, 1_024, |transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(HandlerOutcome::Rejected(b"recorded".to_vec()))
        })
        .await
        .unwrap();
    assert_eq!(
        handle.resolve(request, digest, 21, 1_024).await.unwrap(),
        Resolution::Committed(outcome)
    );
    assert!(matches!(
        handle
            .resolve(request, Digest::from_bytes([56; 32]), 21, 1_024)
            .await,
        Err(crab_cell_runtime::Error::RequestConflict)
    ));
    assert_eq!(
        handle
            .resolve(identity(57), Digest::from_bytes([58; 32]), 21, 1_024)
            .await
            .unwrap(),
        Resolution::Absent
    );
    assert_eq!(
        handle
            .resolve(identity(59), Digest::from_bytes([60; 32]), 10_000, 1_024)
            .await
            .unwrap(),
        Resolution::Expired
    );
    handle.drain().await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn resolve_waits_for_inflight_publication_and_returns_unknown_after_fence() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    delete_control_root(&fixture).await;
    let request = identity(61);
    let digest = Digest::from_bytes([62; 32]);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mutation = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(request, digest, 20, 1_024, 1_024, move |transaction| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                })
                .await
        })
    };
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();
    let resolution = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.resolve(request, digest, 21, 1_024).await })
    };
    release_tx.send(()).unwrap();
    assert!(matches!(
        mutation.await.unwrap(),
        Err(crab_cell_runtime::Error::OutcomeUnknown { .. })
    ));
    assert_eq!(resolution.await.unwrap().unwrap(), Resolution::Unknown);
}
