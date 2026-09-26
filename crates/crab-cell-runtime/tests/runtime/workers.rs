use std::sync::{Arc, mpsc};

use crab_cell_runtime::Error;
use crab_cell_runtime::cell::executor::{CellExecutor, HandlerOutcome, StoredOutcome};
use crab_cell_runtime::cell::schema::install_runtime_schema;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::cell::worker::WorkerExecution;
use crab_cell_runtime::identity::IncarnationId;
use crab_cell_runtime::identity::{CellId, Digest};
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Db, Limits};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

use crate::support::fixtures::mutation_identity_window;

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
    let request = mutation_identity_window(4, 10, 10_000);
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
                mutation_identity_window(7, 10, 10_000),
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
    let queued = {
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.execute(
                cell,
                mutation_identity_window(9, 10, 10_000),
                Digest::from_bytes([10; 32]),
                20,
                RESULT_LIMIT,
                |_| Ok(HandlerOutcome::Success(Vec::new())),
            )
            .await
        })
    };
    tokio::task::yield_now().await;
    assert!(
        !queued.is_finished(),
        "a worker-saturated request must wait for admission"
    );
    queued.abort();
    assert!(matches!(queued.await, Err(error) if error.is_cancelled()));
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

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_shard_does_not_reserve_an_idle_workers_capacity() {
    let first = fixture(0);
    let queued = fixture(2);
    let resident = fixture(1);
    let pool = SqlWorkerPool::new(2, 3).unwrap();
    pool.activate(first.cell, first.executor).await.unwrap();
    pool.activate(queued.cell, queued.executor).await.unwrap();
    pool.activate(resident.cell, resident.executor)
        .await
        .unwrap();

    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, blocked) = mpsc::channel();
    let running = {
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.execute(
                first.cell,
                mutation_identity_window(30, 10, 10_000),
                Digest::from_bytes([30; 32]),
                20,
                RESULT_LIMIT,
                move |_| {
                    started.send(()).unwrap();
                    blocked.recv().unwrap();
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            )
            .await
        })
    };
    entered.await.unwrap();

    // Cells 0 and 2 share a worker. Poll until admission or its reply blocks,
    // then keep that waiter alive while Cell 1 uses the other worker.
    let mut waiting = Box::pin(pool.execute(
        queued.cell,
        mutation_identity_window(31, 10, 10_000),
        Digest::from_bytes([31; 32]),
        20,
        RESULT_LIMIT,
        |_| Ok(HandlerOutcome::Success(Vec::new())),
    ));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    let independent = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        pool.execute(
            resident.cell,
            mutation_identity_window(32, 10, 10_000),
            Digest::from_bytes([32; 32]),
            20,
            RESULT_LIMIT,
            |transaction| {
                let value: i64 =
                    transaction.query_row("SELECT value FROM counter", [], |row| row.get(0))?;
                Ok(HandlerOutcome::Success(value.to_le_bytes().to_vec()))
            },
        ),
    )
    .await;
    drop(waiting);
    release.send(()).unwrap();
    running.await.unwrap().unwrap();

    // Release the blocked worker before asserting so a failed regression
    // cannot deadlock the pool's thread join during unwinding.
    assert!(
        independent.is_ok(),
        "a queued job held the idle worker's capacity"
    );
    independent.unwrap().unwrap();
    assert!(pool.pending(queued.cell).await.unwrap().is_none());
    for (cell, replica) in [
        (first.cell, first.replica),
        (queued.cell, queued.replica),
        (resident.cell, resident.replica),
    ] {
        if let Some(pending) = pool.pending(cell).await.unwrap() {
            let prepared = replica
                .prepare(None, pending.cuts(), pending.outcome().commit_sequence(), 1)
                .await
                .unwrap();
            pool.bind_prepared(cell, prepared.clone()).await.unwrap();
            pool.confirm_published(cell, prepared.root()).await.unwrap();
        }
        pool.deactivate(cell).await.unwrap();
    }
    pool.shutdown().await.unwrap();
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
            mutation_identity_window(21, 10, 10_000),
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
            mutation_identity_window(22, 10, 10_000),
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
            mutation_identity_window(23, 10, 10_000),
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
