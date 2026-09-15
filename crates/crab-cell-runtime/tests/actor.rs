use std::sync::{Arc, mpsc};

use crab_cell_runtime::{
    ApplicationId, CatalogEntry, CatalogRole, CellAuthority, CellRuntime, CellTarget, ControlState,
    Digest, HandlerOutcome, InboxDelivery, IncarnationId, MutationIdentity, NamespaceId, Owner,
    RequestId, Resolution, SessionId, SqlWorkerPool, StoredOutcome, TenantId, Transition,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellObjectKind, CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

struct Fixture {
    _directory: tempfile::TempDir,
    database: std::path::PathBuf,
    target: CellTarget,
    layout: CellStorageLayout,
    replica: CellReplica,
}

fn fixture() -> Fixture {
    fixture_for(b"repository-1")
}

fn fixture_for(partition: &[u8]) -> Fixture {
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        NamespaceId::from_bytes([6; 16]),
        partition,
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([2; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("runtime"), [3; 16]);
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let database = directory.path().join("cell.sqlite");
    Fixture {
        _directory: directory,
        database,
        target,
        layout,
        replica,
    }
}

async fn activate(fixture: &Fixture, node_bytes: usize) -> crab_cell_runtime::CellHandle {
    activate_runtime(fixture, node_bytes).await.1
}

async fn activate_runtime(
    fixture: &Fixture,
    node_bytes: usize,
) -> (CellRuntime, crab_cell_runtime::CellHandle, SqlWorkerPool) {
    let session = SessionId::from_bytes([4; 16]);
    let pool = SqlWorkerPool::new(2, 10).unwrap();
    let runtime = CellRuntime::new(pool.clone(), node_bytes, session).unwrap();
    let handle = bootstrap_on(&runtime, fixture, session).await;
    (runtime, handle, pool)
}

#[tokio::test]
async fn node_byte_reservation_rejects_overcommit_and_releases_capacity() {
    let session = SessionId::from_bytes([40; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 1_024, session).unwrap();
    let held = runtime.try_reserve_node_bytes(1_024).unwrap();

    assert!(matches!(
        runtime.try_reserve_node_bytes(1),
        Err(crab_cell_runtime::Error::Capacity("node retained bytes"))
    ));
    drop(held);
    let released = runtime.try_reserve_node_bytes(1_024).unwrap();
    drop(released);

    runtime.shutdown().await.unwrap();
}

async fn bootstrap_on(
    runtime: &CellRuntime,
    fixture: &Fixture,
    session: SessionId,
) -> crab_cell_runtime::CellHandle {
    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    runtime
        .bootstrap(
            proof,
            fixture.replica.clone(),
            authority,
            observed,
            fixture.database.clone(),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap()
}

fn identity(byte: u8) -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: 10,
        expires_at_ms: 10_000,
    }
}

async fn delete_control_root(fixture: &Fixture) {
    let cell = fixture.target.cell_id();
    let authority = CellAuthority::new(fixture.layout.clone());
    let control = authority.load(cell).await.unwrap().unwrap();
    let root = control.value().root.as_ref().unwrap();
    let root_path = fixture.layout.incarnation_object_path(
        cell.as_bytes(),
        control.value().incarnation.as_bytes(),
        root.digest.as_bytes(),
        CellObjectKind::Root,
    );
    fixture.layout.store().delete(&root_path).await.unwrap();
}

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

    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
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

#[tokio::test]
async fn idle_owner_progress_is_renewed_without_a_per_cell_task() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let initial = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let initial_progress = initial.value().progress;
    let initial_root = initial.value().root.clone();

    let renewed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current = authority
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap();
            if current.value().progress > initial_progress {
                break current;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();

    assert_eq!(renewed.value().root, initial_root);
    assert_eq!(renewed.value().owner, initial.value().owner);
    assert_eq!(renewed.value().revision, initial.value().revision + 1);
    handle.drain().await.unwrap();
}

#[tokio::test]
async fn observed_takeover_fences_the_old_cell_before_more_work() {
    let fixture = fixture();
    let (runtime, handle, _pool) = activate_runtime(&fixture, 16 * 1024 * 1024).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor = observed
        .value()
        .takeover(Owner {
            session: SessionId::from_bytes([99; 16]),
            endpoint: "https://successor.internal:8081".into(),
        })
        .unwrap();
    let successor = authority
        .transition(&observed, successor, Transition::Takeover)
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match handle.query(1, 1, |_| Ok(Vec::new())).await {
                Err(crab_cell_runtime::Error::Fenced) => break,
                Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
                Err(error) => panic!("unexpected query outcome while awaiting fence: {error}"),
            }
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value(), successor.value());
}

#[tokio::test]
async fn idle_control_is_acquired_before_exact_root_restore() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    handle.drain().await.unwrap();

    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let session = SessionId::from_bytes([40; 16]);
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
            authority.clone(),
            idle,
            fixture._directory.path().join("idle-acquire.sqlite"),
            Owner {
                session,
                endpoint: "https://idle-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        0_i64.to_be_bytes()
    );
    let owned = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owned.value().state, ControlState::Recovering);
    assert_eq!(owned.value().epoch, 2);
    assert_eq!(owned.value().owner.as_ref().unwrap().session, session);
    restored.drain().await.unwrap();
}

#[tokio::test]
async fn unchanged_dead_owner_is_taken_over_then_restored() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    drop(handle);

    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let stale = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let session = SessionId::from_bytes([41; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            stale,
            fixture._directory.path().join("takeover.sqlite"),
            Owner {
                session,
                endpoint: "https://takeover-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        0_i64.to_be_bytes()
    );
    let owned = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owned.value().epoch, 2);
    assert_eq!(owned.value().owner.as_ref().unwrap().session, session);
    restored.drain().await.unwrap();
}

#[tokio::test]
async fn unchanged_unpublished_owner_is_taken_over_then_bootstrapped() {
    let fixture = fixture();
    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let stale = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session: SessionId::from_bytes([4; 16]),
                endpoint: "https://stopped-import.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let session = SessionId::from_bytes([42; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let restored = runtime
        .takeover_unpublished(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            stale,
            fixture
                ._directory
                .path()
                .join("takeover-unpublished.sqlite"),
            Owner {
                session,
                endpoint: "https://import-successor.internal:8081".into(),
            },
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (7)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();

    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        7_i64.to_be_bytes()
    );
    let owned = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owned.value().epoch, 2);
    assert_eq!(owned.value().owner.as_ref().unwrap().session, session);
    restored.drain().await.unwrap();
}

#[tokio::test]
async fn slow_bootstrap_renews_unpublished_ownership_before_publication() {
    let fixture = fixture();
    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let session = SessionId::from_bytes([43; 16]);
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://slow-bootstrap.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            observed,
            fixture._directory.path().join("slow-bootstrap.sqlite"),
            |transaction| {
                std::thread::sleep(std::time::Duration::from_secs(4));
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let published = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(published.value().state, ControlState::Serving);
    assert!(published.value().revision >= 3);
    handle.drain().await.unwrap();
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

    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
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

    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
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
async fn node_byte_admission_rejects_before_sql_execution() {
    let fixture = fixture();
    let handle = activate(&fixture, 1024 * 1024).await;
    assert!(matches!(
        handle
            .execute(
                identity(12),
                Digest::from_bytes([13; 32]),
                20,
                1_025,
                1024 * 1024,
                |_| Ok(HandlerOutcome::Success(Vec::new())),
            )
            .await,
        Err(crab_cell_runtime::Error::Capacity(_))
    ));
    handle.drain().await.unwrap();
}

#[tokio::test]
async fn post_commit_publication_failure_returns_resolvable_unknown_outcome() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    delete_control_root(&fixture).await;
    let request = identity(14);
    let digest = Digest::from_bytes([15; 32]);
    assert!(matches!(
        handle
            .execute(request, digest, 20, 1_024, 1_024, |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"not-yet-published".to_vec()))
            })
            .await,
        Err(crab_cell_runtime::Error::OutcomeUnknown {
            request_id,
            operation_digest,
            ..
        }) if request_id == request.request_id && operation_digest == digest
    ));
    assert!(matches!(
        handle
            .execute(request, digest, 21, 1_024, 1_024, |_| {
                Ok(HandlerOutcome::Success(Vec::new()))
            })
            .await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    assert_eq!(
        handle.resolve(request, digest, 21, 1_024).await.unwrap(),
        Resolution::Unknown
    );
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

#[tokio::test]
async fn activation_rejects_control_owned_by_another_node_session() {
    let fixture = fixture();
    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session: SessionId::from_bytes([4; 16]),
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        SessionId::from_bytes([9; 16]),
    )
    .unwrap();
    assert!(matches!(
        runtime
            .bootstrap(
                proof,
                fixture.replica,
                authority,
                observed,
                fixture.database,
                |_| Ok(()),
            )
            .await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
}

#[tokio::test]
async fn failed_bootstrap_keeps_control_unpublished_and_releases_cell_capacity() {
    let fixture = fixture();
    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let session = SessionId::from_bytes([4; 16]);
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 2 * 1024 * 1024, session).unwrap();
    assert!(matches!(
        runtime
            .bootstrap(
                proof.clone(),
                fixture.replica.clone(),
                authority.clone(),
                observed,
                fixture.database.clone(),
                |transaction| {
                    transaction.execute("CREATE TABLE should_rollback(value INTEGER)", [])?;
                    Err(crab_cell_runtime::Error::Command("migration rejected"))
                },
            )
            .await,
        Err(crab_cell_runtime::Error::Command("migration rejected"))
    ));
    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value().state, ControlState::Recovering);
    assert!(current.value().root.is_none());

    let handle = runtime
        .bootstrap(
            proof,
            fixture.replica,
            authority,
            current,
            fixture.database.with_file_name("replacement.sqlite"),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();
}

#[tokio::test]
async fn panicking_bootstrap_keeps_worker_alive_and_releases_cell_capacity() {
    let fixture = fixture_for(b"panicking-bootstrap");
    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let session = SessionId::from_bytes([4; 16]);
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 2 * 1024 * 1024, session).unwrap();

    assert!(matches!(
        runtime
            .bootstrap(
                proof.clone(),
                fixture.replica.clone(),
                authority.clone(),
                observed,
                fixture.database.clone(),
                |_| panic!("bootstrap initializer panic"),
            )
            .await,
        Err(crab_cell_runtime::Error::NativePanic)
    ));
    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value().state, ControlState::Recovering);
    assert!(current.value().root.is_none());

    let handle = runtime
        .bootstrap(
            proof,
            fixture.replica,
            authority,
            current,
            fixture.database.with_file_name("panic-replacement.sqlite"),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn source_loss_takeover_restores_exact_root_and_continues_publication() {
    let target = CellTarget::new(
        TenantId::from_bytes([41; 16]),
        ApplicationId::from_bytes([42; 16]),
        NamespaceId::from_bytes([43; 16]),
        b"repository-cold-start",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([44; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("cold-runtime"), [42; 16]);
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog = crab_cell_runtime::CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Repository,
                Digest::from_bytes([45; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();

    let first_session = SessionId::from_bytes([46; 16]);
    let authority = CellAuthority::new(layout.clone());
    let recovering = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://node-one.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    let first_local = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let first = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            recovering,
            first_local.path().join("cell.sqlite"),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let first_identity = identity(47);
    let first_digest = Digest::from_bytes([48; 32]);
    let first_outcome = first
        .execute(
            first_identity,
            first_digest,
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"first".to_vec()))
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        first_outcome,
        StoredOutcome::Success {
            commit_sequence: 1,
            ..
        }
    ));
    first.drain().await.unwrap();
    drop(runtime);
    first_local.close().unwrap();

    let current = authority.load(cell).await.unwrap().unwrap();
    let second_session = SessionId::from_bytes([49; 16]);
    let mut takeover = current.value().clone();
    takeover.epoch += 1;
    takeover.revision += 1;
    takeover.progress += 1;
    takeover.state = ControlState::Recovering;
    takeover.owner = Some(Owner {
        session: second_session,
        endpoint: "https://node-two.internal:8081".into(),
    });
    let takeover = authority
        .transition(&current, takeover, Transition::Takeover)
        .await
        .unwrap();

    let second_local = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let second = runtime
        .activate_restored(
            proof,
            replica,
            authority.clone(),
            takeover,
            second_local.path().join("cell.sqlite"),
        )
        .await
        .unwrap();
    assert_eq!(
        second
            .resolve(first_identity, first_digest, 21, 1_024)
            .await
            .unwrap(),
        Resolution::Committed(first_outcome)
    );
    assert!(matches!(
        second
            .execute(
                identity(50),
                Digest::from_bytes([51; 32]),
                21,
                1_024,
                1_024,
                |transaction| {
                    let value = transaction
                        .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(value.to_be_bytes().to_vec()))
                },
            )
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 2 }
            if result == &1_i64.to_be_bytes()
    ));
    second.drain().await.unwrap();
    assert_eq!(
        authority
            .load(cell)
            .await
            .unwrap()
            .unwrap()
            .value()
            .root
            .as_ref()
            .unwrap()
            .commit_sequence,
        2
    );
}
