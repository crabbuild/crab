use std::sync::{Arc, mpsc};

use crab_cell_runtime::{
    CellExecutor, CellId, Digest, Error, HandlerOutcome, IncarnationId, MutationIdentity,
    RequestId, SqlWorkerPool, StoredOutcome, WorkerExecution, install_runtime_schema,
};
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Db, Limits};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

const RESULT_LIMIT: usize = 1 << 20;

struct Fixture {
    _directory: tempfile::TempDir,
    cell: CellId,
    replica: CellReplica,
    executor: CellExecutor,
}

fn fixture(cell_byte: u8) -> Fixture {
    let cell = CellId::from_bytes([cell_byte; 32]);
    let incarnation = IncarnationId::from_bytes([2; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("runtime"), [3; 16]);
    let replica = CellReplica::new(
        layout,
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
    let writer = Db::open(&database, Limits::default()).unwrap();
    Fixture {
        _directory: directory,
        cell,
        replica,
        executor: CellExecutor::new(writer, cell, incarnation, 1),
    }
}

fn identity(byte: u8) -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: 10,
        expires_at_ms: 10_000,
    }
}

#[tokio::test]
async fn fixed_workers_own_execute_prepare_confirm_and_dedup() {
    let Fixture {
        _directory,
        cell,
        replica,
        executor,
    } = fixture(1);
    let pool = SqlWorkerPool::new(2, 10).unwrap();
    pool.activate(cell, executor).await.unwrap();
    let request = identity(4);
    let digest = Digest::from_bytes([5; 32]);
    let pending = match pool
        .execute(cell, request, digest, 20, RESULT_LIMIT, |transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(HandlerOutcome::Success(b"one".to_vec()))
        })
        .await
        .unwrap()
    {
        WorkerExecution::Pending(pending) => pending,
        WorkerExecution::Recorded(_) => panic!("first execution cannot be recorded"),
    };
    let prepared = replica
        .prepare(None, pending.cuts(), pending.outcome().commit_sequence(), 1)
        .await
        .unwrap();
    pool.bind_prepared(cell, prepared.clone()).await.unwrap();
    assert!(matches!(
        pool.confirm_published(cell, prepared.root()).await.unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"one"
    ));
    assert!(matches!(
        pool.execute(cell, request, digest, 21, RESULT_LIMIT, |_| {
            Ok(HandlerOutcome::Success(b"wrong".to_vec()))
        })
        .await
        .unwrap(),
        WorkerExecution::Recorded(StoredOutcome::Success { ref result, commit_sequence: 1 })
            if result == b"one"
    ));
    pool.deactivate(cell).await.unwrap();
}

#[tokio::test]
async fn cancelled_waiter_does_not_cancel_an_accepted_sql_command() {
    let Fixture {
        _directory,
        cell,
        replica,
        executor,
    } = fixture(6);
    let pool = SqlWorkerPool::new(1, 1).unwrap();
    pool.activate(cell, executor).await.unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let waiting = {
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.execute(
                cell,
                identity(7),
                Digest::from_bytes([8; 32]),
                20,
                RESULT_LIMIT,
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
    assert!(matches!(
        pool.execute(
            cell,
            identity(9),
            Digest::from_bytes([10; 32]),
            20,
            RESULT_LIMIT,
            |_| Ok(HandlerOutcome::Success(Vec::new())),
        )
        .await,
        Err(Error::Capacity(_))
    ));
    waiting.abort();
    assert!(matches!(waiting.await, Err(error) if error.is_cancelled()));
    release_tx.send(()).unwrap();

    let pending = pool.pending(cell).await.unwrap().unwrap();
    let prepared = replica
        .prepare(None, pending.cuts(), pending.outcome().commit_sequence(), 1)
        .await
        .unwrap();
    pool.bind_prepared(cell, prepared.clone()).await.unwrap();
    assert!(matches!(
        pool.confirm_published(cell, prepared.root()).await.unwrap(),
        StoredOutcome::Success { ref result, .. } if result == b"survived"
    ));
    pool.deactivate(cell).await.unwrap();
}

#[tokio::test]
async fn panicking_handler_fences_only_its_cell_and_worker_continues() {
    let first = fixture(21);
    let second = fixture(22);
    let pool = SqlWorkerPool::new(1, 2).unwrap();
    pool.activate(first.cell, first.executor).await.unwrap();
    pool.activate(second.cell, second.executor).await.unwrap();

    let panic = pool
        .execute(
            first.cell,
            identity(21),
            Digest::from_bytes([21; 32]),
            20,
            RESULT_LIMIT,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                panic!("native handler panic")
            },
        )
        .await;
    assert!(matches!(panic, Err(Error::NativePanic)));
    assert!(matches!(
        pool.execute(
            first.cell,
            identity(22),
            Digest::from_bytes([22; 32]),
            21,
            RESULT_LIMIT,
            |_| Ok(HandlerOutcome::Success(Vec::new())),
        )
        .await,
        Err(Error::Fenced)
    ));

    assert!(matches!(
        pool.execute(
            second.cell,
            identity(23),
            Digest::from_bytes([23; 32]),
            22,
            RESULT_LIMIT,
            |_| Ok(HandlerOutcome::Success(b"worker survived".to_vec())),
        )
        .await
        .unwrap(),
        WorkerExecution::Pending(_)
    ));
}

#[tokio::test]
async fn active_cell_admission_is_global_and_released_after_drain() {
    let first = fixture(1);
    let second = fixture(2);
    let pool = SqlWorkerPool::new(2, 1).unwrap();
    pool.activate(first.cell, first.executor).await.unwrap();
    assert!(matches!(
        pool.activate(second.cell, second.executor).await,
        Err(Error::Capacity(_))
    ));
    pool.deactivate(first.cell).await.unwrap();

    let replacement = fixture(2);
    pool.activate(replacement.cell, replacement.executor)
        .await
        .unwrap();
    pool.deactivate(replacement.cell).await.unwrap();
}

#[tokio::test]
async fn worker_shutdown_requires_an_empty_pool_and_closes_every_clone() {
    let fixture = fixture(3);
    let cell = fixture.cell;
    let pool = SqlWorkerPool::new(2, 10).unwrap();
    let clone = pool.clone();
    pool.activate(cell, fixture.executor).await.unwrap();
    assert!(matches!(pool.shutdown().await, Err(Error::Control(_))));

    pool.deactivate(cell).await.unwrap();
    pool.shutdown().await.unwrap();
    assert!(matches!(
        clone.pending(cell).await,
        Err(Error::RuntimeClosed)
    ));
    assert!(matches!(clone.shutdown().await, Err(Error::RuntimeClosed)));
}
