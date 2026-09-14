use std::sync::{Arc, mpsc};

use crab_cell_runtime::{
    ApplicationId, CatalogEntry, CatalogRole, CellAuthority, CellExecutor, CellRuntime, CellTarget,
    ControlState, Digest, HandlerOutcome, IncarnationId, MutationIdentity, NamespaceId, Owner,
    RequestId, SessionId, SqlWorkerPool, StoredOutcome, TenantId, Transition,
    install_runtime_schema,
};
use crab_ltx::{CellReplica, Limits, ManagedDb};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

struct Fixture {
    _directory: tempfile::TempDir,
    database: std::path::PathBuf,
    target: CellTarget,
    layout: CellStorageLayout,
    replica: CellReplica,
    executor: Option<CellExecutor>,
}

fn fixture() -> Fixture {
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        NamespaceId::from_bytes([6; 16]),
        b"repository-1",
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
    let mut connection = crab_ltx::rusqlite::Connection::open(&database).unwrap();
    install_runtime_schema(&mut connection, cell, incarnation, 1).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
        )
        .unwrap();
    drop(connection);
    let writer = ManagedDb::open(&database, Limits::default()).unwrap();
    Fixture {
        _directory: directory,
        database,
        target,
        layout,
        replica,
        executor: Some(CellExecutor::new(writer, cell, incarnation, 1)),
    }
}

async fn activate(fixture: &mut Fixture, node_bytes: usize) -> crab_cell_runtime::CellHandle {
    let replica = fixture.replica.clone();
    activate_with_replica(fixture, node_bytes, replica).await
}

async fn activate_with_replica(
    fixture: &mut Fixture,
    node_bytes: usize,
    replica: CellReplica,
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
        CellRuntime::new(SqlWorkerPool::new(2, 10).unwrap(), node_bytes, session).unwrap();
    runtime
        .activate(
            proof,
            fixture.executor.take().unwrap(),
            replica,
            authority,
            observed,
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

#[tokio::test]
async fn dispatcher_serializes_and_publishes_commands_before_drain() {
    let mut fixture = fixture();
    let handle = activate(&mut fixture, 16 * 1024 * 1024).await;
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
                    None,
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
                    Some(500),
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

    let connection = crab_ltx::rusqlite::Connection::open(&fixture.database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn cancelled_command_waiter_is_resolved_by_original_identity() {
    let mut fixture = fixture();
    let handle = activate(&mut fixture, 16 * 1024 * 1024).await;
    let request = identity(10);
    let digest = Digest::from_bytes([11; 32]);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let waiting = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    request,
                    digest,
                    20,
                    1_024,
                    1_024,
                    None,
                    move |transaction| {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        transaction.execute("UPDATE counter SET value = value + 1", [])?;
                        Ok(HandlerOutcome::Success(b"survived".to_vec()))
                    },
                )
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
            .execute(request, digest, 21, 1_024, 1_024, None, |_| {
                Ok(HandlerOutcome::Success(b"wrong".to_vec()))
            })
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"survived"
    ));
    handle.drain().await.unwrap();
}

#[tokio::test]
async fn node_byte_admission_rejects_before_sql_execution() {
    let mut fixture = fixture();
    let handle = activate(&mut fixture, 1024 * 1024).await;
    assert!(matches!(
        handle
            .execute(
                identity(12),
                Digest::from_bytes([13; 32]),
                20,
                1_025,
                1024 * 1024,
                None,
                |_| Ok(HandlerOutcome::Success(Vec::new())),
            )
            .await,
        Err(crab_cell_runtime::Error::Capacity(_))
    ));
    handle.drain().await.unwrap();
}

#[tokio::test]
async fn post_commit_publication_failure_returns_resolvable_unknown_outcome() {
    let mut fixture = fixture();
    let wrong_replica =
        CellReplica::new(fixture.layout.clone(), [99; 32], [2; 16], Limits::default()).unwrap();
    let handle = activate_with_replica(&mut fixture, 16 * 1024 * 1024, wrong_replica).await;
    let request = identity(14);
    let digest = Digest::from_bytes([15; 32]);
    assert!(matches!(
        handle
            .execute(request, digest, 20, 1_024, 1_024, None, |transaction| {
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
            .execute(request, digest, 21, 1_024, 1_024, None, |_| {
                Ok(HandlerOutcome::Success(Vec::new()))
            })
            .await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
}

#[tokio::test]
async fn proven_handler_rollback_keeps_the_cell_servable() {
    let mut fixture = fixture();
    let handle = activate(&mut fixture, 16 * 1024 * 1024).await;
    assert!(matches!(
        handle
            .execute(
                identity(16),
                Digest::from_bytes([17; 32]),
                20,
                1_024,
                1_024,
                None,
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
                None,
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
                None,
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
    let mut fixture = fixture();
    let handle = activate(&mut fixture, 16 * 1024 * 1024).await;
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
                    None,
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
                    None,
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
    let mut fixture = fixture();
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
            .activate(
                proof,
                fixture.executor.take().unwrap(),
                fixture.replica,
                authority,
                observed,
            )
            .await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
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

    let bootstrap = tempfile::TempDir::new().unwrap();
    let bootstrap_path = bootstrap.path().join("bootstrap.sqlite");
    let mut connection = crab_ltx::rusqlite::Connection::open(&bootstrap_path).unwrap();
    install_runtime_schema(&mut connection, cell, incarnation, 1).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
        )
        .unwrap();
    drop(connection);
    let mut writer = ManagedDb::open(&bootstrap_path, Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute("UPDATE sys_meta SET logical_time_ms = 1", [])?;
            Ok(())
        })
        .unwrap();
    let prepared = replica
        .prepare(None, &writer.capture().unwrap(), 0, 1)
        .await
        .unwrap();
    writer.close().unwrap();
    bootstrap.close().unwrap();

    let first_session = SessionId::from_bytes([46; 16]);
    let authority = CellAuthority::new(layout);
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
    let serving = recovering
        .value()
        .publish_prepared(&prepared, None)
        .unwrap();
    let serving = authority
        .transition(&recovering, serving, Transition::Publish)
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
        .activate_restored(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            serving,
            first_local.path().join("cell.sqlite"),
        )
        .await
        .unwrap();
    assert!(matches!(
        first
            .execute(
                identity(47),
                Digest::from_bytes([48; 32]),
                20,
                1_024,
                1_024,
                None,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(b"first".to_vec()))
                },
            )
            .await
            .unwrap(),
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
    assert!(matches!(
        second
            .execute(
                identity(50),
                Digest::from_bytes([51; 32]),
                21,
                1_024,
                1_024,
                None,
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
