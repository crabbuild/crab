//! Handler deadlines, panics, rollbacks, and query interruption.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn queued_query_deadline_does_not_fence_untouched_cell() {
    queued_deadline(1, "query").await;
    queued_deadline(2, "query").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_mutation_deadline_prevents_late_writes_without_fencing() {
    for operation in ["command", "effect"] {
        queued_deadline(2, operation).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_resolution_deadline_preserves_owner_for_retry() {
    for operation in ["resolve", "resolve_effect"] {
        queued_deadline(2, operation).await;
    }
}

async fn queued_deadline(workers: usize, operation: &str) {
    let first = fixture_for(b"deadline-blocker");
    let lane = |cell: crab_cell_runtime::identity::CellId| {
        let prefix: [u8; 8] = cell.as_bytes()[..8].try_into().unwrap();
        u64::from_be_bytes(prefix) as usize % workers
    };
    // With two permits the second operation reaches the worker's actual
    // queue; one permit exercises expiry while waiting for job admission.
    let second = (0..100)
        .map(|index| fixture_for(format!("deadline-queued-{index}").as_bytes()))
        .find(|fixture| lane(fixture.target.cell_id()) == lane(first.target.cell_id()))
        .unwrap();
    let session = SessionId::from_bytes([4; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(workers, 2).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let blocker = bootstrap_on(&runtime, &first, session).await;
    let queued = bootstrap_on(&runtime, &second, session).await;
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let blocking = tokio::spawn(async move {
        blocker
            .query(64, 64, move |_| {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                Ok(Vec::new())
            })
            .await
    });
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let executed = entered.clone();
    let identity = mutation_identity_window(42, 10, 10_000);
    let digest = Digest::from_bytes([43; 32]);
    let delivery = InboxDelivery {
        effect_id: [42; 32],
        operation_digest: digest,
        expires_at_ms: 10_000,
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(7), async {
        let write = move |transaction: &crab_ltx::rusqlite::Transaction<'_>| {
            executed.store(true, Ordering::SeqCst);
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(HandlerOutcome::Success(Vec::new()))
        };
        match operation {
            "query" => {
                let entered = entered.clone();
                queued
                    .query(64, 64, move |_| {
                        entered.store(true, Ordering::SeqCst);
                        Ok(Vec::new())
                    })
                    .await
                    .map(|_| ())
            }
            "command" => queued
                .execute(identity, digest, 20, 64, 64, write)
                .await
                .map(|_| ()),
            "effect" => queued
                .deliver_effect(delivery, 20, 64, 64, write)
                .await
                .map(|_| ()),
            "resolve" => queued
                .resolve(identity, digest, 20, 64)
                .await
                .map(|result| {
                    assert_eq!(result, Resolution::Unknown);
                }),
            "resolve_effect" => queued.resolve_effect(delivery, 20, 64).await.map(|result| {
                assert_eq!(result, Resolution::Unknown);
            }),
            _ => unreachable!(),
        }
    })
    .await;
    // Release before assertions so failures cannot leave teardown blocked.
    release_tx.send(()).unwrap();
    let _ = blocking.await.unwrap();
    let result = result.unwrap();
    if operation.starts_with("resolve") {
        result.unwrap();
    } else {
        assert!(
            matches!(result, Err(crab_cell_runtime::Error::Deadline)),
            "{operation}: {result:?}"
        );
    }
    let readable = queued
        .query(64, 64, |connection| {
            let value: i64 =
                connection.query_row("SELECT value FROM counter", [], |row| row.get(0))?;
            Ok(value.to_le_bytes().to_vec())
        })
        .await;
    runtime.shutdown().await.unwrap();
    assert_eq!(readable.unwrap(), 0_i64.to_le_bytes(), "{operation}");
    assert!(
        !entered.load(Ordering::SeqCst),
        "{operation} executed after queue expiry"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn native_handler_deadline_discards_late_commit_and_reopens_authoritative_root() {
    let fixture = fixture();
    let (runtime, handle, _pool) = activate_runtime(&fixture, 16 * 1024 * 1024).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let before = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .root
        .clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mutation = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    mutation_identity_window(42, 10, 10_000),
                    Digest::from_bytes([43; 32]),
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
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(7), mutation)
        .await
        .unwrap()
        .unwrap();
    match outcome {
        Err(crab_cell_runtime::Error::OutcomeUnknown { source, .. }) => {
            assert!(matches!(*source, crab_cell_runtime::Error::Deadline));
        }
        other => panic!("expected deadline outcome, got {other:?}"),
    }
    assert!(matches!(
        handle.query(1, 1, |_| Ok(Vec::new())).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));

    release_tx.send(()).unwrap();
    let after = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let current = authority
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap();
            if current.value().state == ControlState::Idle {
                break current;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(after.value().root, before);
    assert!(after.value().owner.is_none());

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let session = SessionId::from_bytes([4; 16]);
    let recovered = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            after,
            fixture._directory.path().join("deadline-recovered.sqlite"),
            Owner {
                session,
                endpoint: "https://deadline-recovered.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        recovered
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        0_i64.to_be_bytes()
    );
    recovered.drain().await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn native_handler_panic_discards_transaction_and_reopens_authoritative_root() {
    let fixture = fixture_for(b"panicking-command");
    let (runtime, handle, _pool) = activate_runtime(&fixture, 16 * 1024 * 1024).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let before = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .root
        .clone();

    let outcome = handle
        .execute(
            mutation_identity_window(44, 10, 10_000),
            Digest::from_bytes([45; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                panic!("command handler panic")
            },
        )
        .await;
    match outcome {
        Err(crab_cell_runtime::Error::OutcomeUnknown { source, .. }) => {
            assert!(matches!(*source, crab_cell_runtime::Error::NativePanic));
        }
        other => panic!("expected native panic outcome, got {other:?}"),
    }
    assert!(matches!(
        handle.query(1, 1, |_| Ok(Vec::new())).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));

    let idle = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let current = authority
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap();
            if current.value().state == ControlState::Idle {
                break current;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(idle.value().root, before);
    assert!(idle.value().owner.is_none());

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let recovered = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            idle,
            fixture._directory.path().join("panic-recovered.sqlite"),
            Owner {
                session: SessionId::from_bytes([4; 16]),
                endpoint: "https://panic-recovered.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        recovered
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        0_i64.to_be_bytes()
    );
    recovered.drain().await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn sqlite_query_is_interrupted_at_wall_deadline() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(7),
        handle.query(64, 64, |connection| {
            let value = connection.query_row(
                "WITH RECURSIVE counter(value) AS (VALUES(0) UNION ALL SELECT value + 1 FROM counter WHERE value < 1000000000) SELECT sum(value) FROM counter",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            Ok(value.to_be_bytes().to_vec())
        }),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(crab_cell_runtime::Error::Deadline)));
    assert!(matches!(
        handle.query(1, 1, |_| Ok(Vec::new())).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
}
#[tokio::test]
async fn proven_handler_rollback_keeps_the_cell_servable() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    assert!(matches!(
        handle
            .execute(
                mutation_identity_window(16, 10, 10_000),
                Digest::from_bytes([17; 32]),
                20,
                1_024,
                1_024,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 10", [])?;
                    Err(crab_cell_runtime::Error::Command("application failure"))
                },
            )
            .await,
        Err(crab_cell_runtime::Error::Command("application failure"))
    ));
    assert!(matches!(
        handle
            .execute(
                mutation_identity_window(24, 10, 10_000),
                Digest::from_bytes([25; 32]),
                20,
                1_024,
                1,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 10", [])?;
                    Ok(HandlerOutcome::Success(b"too large".to_vec()))
                },
            )
            .await,
        Err(crab_cell_runtime::Error::Command(
            "handler result exceeds command limit"
        ))
    ));
    assert!(matches!(
        handle
            .execute(
                mutation_identity_window(18, 10, 10_000),
                Digest::from_bytes([19; 32]),
                21,
                1_024,
                1_024,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(b"recovered".to_vec()))
                },
            )
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"recovered"
    ));
    handle.drain().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_sqlite_rollback_keeps_the_cell_servable() {
    let fixture = fixture_with_limits(
        b"automatic-rollback",
        Limits {
            max_database_bytes: 512 * 1024,
            ..Limits::default()
        },
    );
    let (runtime, handle, _pool) = activate_runtime(&fixture, 16 * 1024 * 1024).await;
    let failed = handle
        .execute(
            mutation_identity_window(71, 10, 10_000),
            Digest::from_bytes([72; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = 99", [])?;
                transaction.execute("CREATE TABLE oversized(value BLOB)", [])?;
                transaction.execute("INSERT INTO oversized VALUES(zeroblob(1048576))", [])?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await;
    assert!(
        matches!(failed, Err(crab_cell_runtime::Error::Sqlite(ref error))
        if error.sqlite_error_code() == Some(crab_ltx::rusqlite::ErrorCode::DiskFull)),
        "expected a rolled-back capacity refusal, got {failed:?}"
    );
    let outcome = handle
        .execute(
            mutation_identity_window(73, 10, 10_000),
            Digest::from_bytes([74; 32]),
            21,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                let value: i64 =
                    transaction.query_row("SELECT value FROM counter", [], |row| row.get(0))?;
                Ok(HandlerOutcome::Success(value.to_be_bytes().to_vec()))
            },
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        StoredOutcome::Success {
            result: 1_i64.to_be_bytes().to_vec(),
            commit_sequence: 1,
        }
    );
    runtime.shutdown().await.unwrap();
}
