//! Cancellation and shutdown drain behaviour.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn effect_delivery_survives_cancellation_and_exact_root_restore() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let delivery = InboxDelivery {
        effect_id: [70; 32],
        operation_digest: Digest::from_bytes([71; 32]),
        expires_at_ms: 10_000,
    };
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let first = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .deliver_effect(delivery, 20, 64, 1_024, move |transaction| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(b"delivered".to_vec()))
                })
                .await
        }
    });
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    release_tx.send(()).unwrap();

    let replayed = handle
        .deliver_effect(delivery, 21, 64, 1_024, |_| {
            panic!("published inbox delivery must not execute twice")
        })
        .await
        .unwrap();
    assert_eq!(
        replayed,
        StoredOutcome::Success {
            result: b"delivered".to_vec(),
            commit_sequence: 1,
        }
    );
    assert_eq!(
        handle.resolve_effect(delivery, 22, 1_024).await.unwrap(),
        Resolution::Committed(replayed.clone())
    );
    assert!(matches!(
        handle
            .resolve_effect(
                InboxDelivery {
                    operation_digest: Digest::from_bytes([72; 32]),
                    ..delivery
                },
                22,
                1_024,
            )
            .await,
        Err(crab_cell_runtime::Error::RequestConflict)
    ));
    assert!(matches!(
        handle
            .resolve_effect(
                InboxDelivery {
                    expires_at_ms: delivery.expires_at_ms + 1,
                    ..delivery
                },
                22,
                1_024,
            )
            .await,
        Err(crab_cell_runtime::Error::RequestConflict)
    ));
    assert_eq!(
        handle
            .resolve_effect(
                InboxDelivery {
                    effect_id: [73; 32],
                    ..delivery
                },
                22,
                1_024,
            )
            .await
            .unwrap(),
        Resolution::Absent
    );
    let authority = CellAuthority::new(fixture.layout.clone());
    let published = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(published.value().root.as_ref().unwrap().commit_sequence, 1);
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

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let session = SessionId::from_bytes([45; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            idle,
            fixture._directory.path().join("effect-restored.sqlite"),
            Owner {
                session,
                endpoint: "https://effect-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        restored.resolve_effect(delivery, 30, 1_024).await.unwrap(),
        Resolution::Committed(replayed)
    );
    assert_eq!(
        restored
            .resolve_effect(delivery, delivery.expires_at_ms, 1_024)
            .await
            .unwrap(),
        Resolution::Expired
    );
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        1_i64.to_be_bytes()
    );
    restored.drain().await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn runtime_shutdown_drains_accepted_work_and_releases_all_owners() {
    let fixture = fixture();
    let (runtime, handle, pool) = activate_runtime(&fixture, 16 * 1024 * 1024).await;
    let second_fixture = fixture_for(b"repository-2");
    let second = bootstrap_on(&runtime, &second_fixture, SessionId::from_bytes([4; 16])).await;
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mutation = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    identity(111),
                    Digest::from_bytes([112; 32]),
                    20,
                    1_024,
                    1_024,
                    move |transaction| {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        transaction.execute("UPDATE counter SET value = value + 1", [])?;
                        Ok(HandlerOutcome::Success(b"published".to_vec()))
                    },
                )
                .await
        })
    };
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();

    let shutdown = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.shutdown().await }
    });
    while !runtime.is_shutting_down() {
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        handle.query(1, 1, |_| Ok(Vec::new())).await,
        Err(crab_cell_runtime::Error::RuntimeClosed)
    ));
    assert!(!shutdown.is_finished());

    release_tx.send(()).unwrap();
    assert!(matches!(
        mutation.await.unwrap().unwrap(),
        StoredOutcome::Success {
            ref result,
            commit_sequence: 1
        } if result == b"published"
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        runtime.shutdown().await,
        Err(crab_cell_runtime::Error::RuntimeClosed)
    ));
    assert!(matches!(
        pool.shutdown().await,
        Err(crab_cell_runtime::Error::RuntimeClosed)
    ));

    let released = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(released.value().state, ControlState::Idle);
    assert!(released.value().owner.is_none());
    assert_eq!(released.value().root.as_ref().unwrap().commit_sequence, 1);
    let second_released = CellAuthority::new(second_fixture.layout.clone())
        .load(second.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second_released.value().state, ControlState::Idle);
    assert!(second_released.value().owner.is_none());
}
