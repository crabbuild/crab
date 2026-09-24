//! Dispatch, compaction, and command/query/resolve semantics.

use super::*;

#[tokio::test]
async fn dispatcher_serializes_and_publishes_commands_before_drain() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let first = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    identity(6),
                    Digest::from_bytes([7; 32]),
                    20,
                    1_024,
                    1_024,
                    |transaction| {
                        transaction.execute("UPDATE counter SET value = value + 1", [])?;
                        Ok(HandlerOutcome::Success(b"one".to_vec()))
                    },
                )
                .await
        })
    };
    let second = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    identity(8),
                    Digest::from_bytes([9; 32]),
                    21,
                    1_024,
                    1_024,
                    |transaction| {
                        transaction.execute("UPDATE counter SET value = value + 1", [])?;
                        Ok(HandlerOutcome::Success(b"two".to_vec()))
                    },
                )
                .await
        })
    };
    assert!(matches!(
        first.await.unwrap().unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"one"
    ));
    assert!(matches!(
        second.await.unwrap().unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 2 } if result == b"two"
    ));
    handle.drain().await.unwrap();

    let released = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(released.value().state, ControlState::Idle);
    assert!(released.value().owner.is_none());
    assert_eq!(
        released.value().next_due_ms,
        Some(10_000 + 24 * 60 * 60 * 1000)
    );

    let connection = crab_ltx::rusqlite::Connection::open(&fixture.database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn dispatcher_compacts_before_segment_admission_is_exhausted() {
    let fixture = fixture_with_limits(
        b"compacting-repository",
        Limits {
            max_segments: 4,
            ..Limits::default()
        },
    );
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    for sequence in 1_u8..=10 {
        assert!(matches!(
            handle
                .execute(
                    identity(sequence),
                    Digest::from_bytes([sequence.saturating_add(20); 32]),
                    20,
                    1_024,
                    1_024,
                    |transaction| {
                        transaction.execute("UPDATE counter SET value = value + 1", [])?;
                        Ok(HandlerOutcome::Success(Vec::new()))
                    },
                )
                .await
                .unwrap(),
            StoredOutcome::Success {
                commit_sequence,
                ..
            } if commit_sequence == u64::from(sequence)
        ));
    }

    let authority = CellAuthority::new(fixture.layout.clone());
    let control = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let root = control.value().ltx_root().unwrap();
    assert!(
        fixture
            .replica
            .open_root(&root)
            .await
            .unwrap()
            .segment_count()
            < 4
    );
    handle.drain().await.unwrap();
    let restored_directory = tempfile::TempDir::new().unwrap();
    let restored = restored_directory.path().join("restored.sqlite");
    fixture
        .replica
        .open_root(&root)
        .await
        .unwrap()
        .restore(&restored)
        .await
        .unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        10
    );
}

#[tokio::test]
async fn dispatcher_promotes_after_burst_becomes_quiet() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let store: Arc<dyn ObjectStore> = pausing.clone();
    let fixture = fixture_with_limits_and_store(
        b"quiet-compaction-runtime",
        Limits::default(),
        Store::with_retry(
            store,
            RetryPolicy {
                max_attempts: 1,
                base: std::time::Duration::from_millis(1),
                cap: std::time::Duration::from_millis(1),
            },
        ),
    );
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    for sequence in 1_u8..=8 {
        handle
            .execute(
                identity(sequence),
                Digest::from_bytes([sequence.saturating_add(30); 32]),
                20,
                1_024,
                1_024,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            )
            .await
            .unwrap();
    }

    let authority = CellAuthority::new(fixture.layout.clone());
    let before = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .ltx_root()
        .unwrap();
    let initial_segments = fixture
        .replica
        .open_root(&before)
        .await
        .unwrap()
        .segment_count();
    assert!(initial_segments >= 8);
    pausing.fail_next_put_transiently();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        pausing.wait_until_failed(),
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        authority
            .load(fixture.target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .ltx_root(),
        Some(before)
    );

    let promoted = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let root = authority
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap()
                .value()
                .ltx_root()
                .unwrap();
            if fixture
                .replica
                .open_root(&root)
                .await
                .unwrap()
                .segment_count()
                < initial_segments
            {
                break root;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(promoted.position, before.position);
    assert_eq!(promoted.commit_sequence, before.commit_sequence);
    handle.drain().await.unwrap();
    let restored_directory = tempfile::TempDir::new().unwrap();
    let restored = restored_directory.path().join("restored.sqlite");
    fixture
        .replica
        .open_root(&promoted)
        .await
        .unwrap()
        .restore(&restored)
        .await
        .unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        8
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn command_arriving_during_quiet_compaction_waits_for_publisher() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let store: Arc<dyn ObjectStore> = pausing.clone();
    let fixture = fixture_with_limits_and_store(
        b"quiet-compaction-queued-command",
        Limits::default(),
        Store::new(store),
    );
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    for sequence in 1_u8..=8 {
        handle
            .execute(
                identity(sequence),
                Digest::from_bytes([sequence.saturating_add(70); 32]),
                20,
                1_024,
                1_024,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            )
            .await
            .unwrap();
    }
    pausing.arm();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        pausing.wait_until_blocked(),
    )
    .await
    .unwrap();
    let ninth = handle.execute(
        identity(9),
        Digest::from_bytes([79; 32]),
        20,
        1_024,
        1_024,
        |transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(HandlerOutcome::Success(Vec::new()))
        },
    );
    tokio::pin!(ninth);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut ninth)
            .await
            .is_err()
    );
    pausing.release();
    assert_eq!(ninth.await.unwrap().commit_sequence(), 9);
    handle.drain().await.unwrap();
    let root = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .ltx_root()
        .unwrap();
    let restored_directory = tempfile::TempDir::new().unwrap();
    let restored = restored_directory.path().join("restored.sqlite");
    fixture
        .replica
        .open_root(&root)
        .await
        .unwrap()
        .restore(&restored)
        .await
        .unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        9
    );
}

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
                    identity(42),
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
            identity(44),
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

#[tokio::test]
async fn proven_handler_rollback_keeps_the_cell_servable() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    assert!(matches!(
        handle
            .execute(
                identity(16),
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
                identity(24),
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
                identity(18),
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

#[tokio::test]
async fn per_cell_request_admission_caps_inflight_and_queued_commands() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let first = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    identity(29),
                    Digest::from_bytes([29; 32]),
                    20,
                    0,
                    1,
                    move |_| {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(HandlerOutcome::Success(Vec::new()))
                    },
                )
                .await
        })
    };
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();

    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
    for byte in 30..94 {
        let handle = handle.clone();
        let result_tx = result_tx.clone();
        tokio::spawn(async move {
            let result = handle
                .execute(
                    identity(byte),
                    Digest::from_bytes([byte; 32]),
                    21,
                    0,
                    1,
                    |_| Ok(HandlerOutcome::Success(Vec::new())),
                )
                .await;
            let _ = result_tx.send(result);
        });
    }
    drop(result_tx);
    assert!(matches!(
        result_rx.recv().await.unwrap(),
        Err(crab_cell_runtime::Error::Capacity(_))
    ));
    release_tx.send(()).unwrap();
    first.await.unwrap().unwrap();
    for _ in 0..63 {
        assert!(result_rx.recv().await.unwrap().is_ok());
    }
    handle.drain().await.unwrap();
}
