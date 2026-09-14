use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crab_cell_runtime::{
    CellExecutor, CellId, CommandExecution, Control, Digest, HandlerOutcome, IncarnationId,
    MutationIdentity, Owner, RequestId, SessionId, StoredOutcome, install_runtime_schema,
};
use crab_ltx::{CellReplica, Limits, ManagedDb};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

struct Fixture {
    _directory: tempfile::TempDir,
    database: std::path::PathBuf,
    cell: CellId,
    incarnation: IncarnationId,
    replica: CellReplica,
    executor: CellExecutor,
}

fn fixture() -> Fixture {
    let cell = CellId::from_bytes([1; 32]);
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
    let writer = ManagedDb::open(&database, Limits::default()).unwrap();
    Fixture {
        _directory: directory,
        database,
        cell,
        incarnation,
        replica,
        executor: CellExecutor::new(writer, cell, incarnation, 1),
    }
}

#[tokio::test]
async fn prepared_root_becomes_one_valid_control_successor() {
    let Fixture {
        _directory,
        database: _,
        cell,
        incarnation,
        replica,
        mut executor,
    } = fixture();
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([6; 16]),
        issued_at_ms: 10,
        expires_at_ms: 10_000,
    };
    let operation_digest = Digest::from_bytes([7; 32]);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    assert_eq!(
        executor
            .execute(identity, operation_digest, 20, move |transaction| {
                observed.fetch_add(1, Ordering::SeqCst);
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"one".to_vec()))
            })
            .unwrap(),
        CommandExecution::Pending
    );
    assert!(matches!(
        executor.execute(identity, operation_digest, 20, |_| {
            Ok(HandlerOutcome::Success(Vec::new()))
        }),
        Err(crab_cell_runtime::Error::PendingPublication)
    ));
    let pending = executor.pending().unwrap();
    let prepared = replica
        .prepare(None, pending.cuts(), pending.outcome().commit_sequence(), 1)
        .await
        .unwrap();
    let control = Control::initial(
        cell,
        incarnation,
        Owner {
            session: SessionId::from_bytes([4; 16]),
            endpoint: "https://node.internal:8081".into(),
        },
        Digest::from_bytes([5; 32]),
        1,
    )
    .unwrap();
    let published = control.publish_prepared(&prepared, Some(42)).unwrap();
    assert_eq!(published.ltx_root(), Some(prepared.root()));
    assert_eq!(published.next_due_ms, Some(42));
    assert_eq!(published.revision, 2);
    executor.bind_prepared(&prepared).unwrap();
    let wrong_root = crab_ltx::RootRef {
        digest: [11; 32],
        ..prepared.root()
    };
    assert!(executor.confirm_published(&wrong_root).is_err());
    assert_eq!(
        executor.pending().unwrap().prepared(),
        Some(prepared.root())
    );
    assert_eq!(
        executor.confirm_published(&prepared.root()).unwrap(),
        StoredOutcome::Success {
            result: b"one".to_vec(),
            commit_sequence: 1,
        }
    );
    assert!(matches!(
        executor
            .execute(identity, operation_digest, 21, |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(HandlerOutcome::Success(b"two".to_vec()))
            })
            .unwrap(),
        CommandExecution::Recorded(StoredOutcome::Success { ref result, commit_sequence: 1 })
            if result == b"one"
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    executor.close().unwrap();
}

#[tokio::test]
async fn business_rejection_rolls_back_domain_writes_and_publishes_the_outcome() {
    let Fixture {
        _directory,
        database,
        cell: _,
        incarnation: _,
        replica,
        mut executor,
    } = fixture();
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([8; 16]),
        issued_at_ms: 100,
        expires_at_ms: 20_000,
    };
    let digest = Digest::from_bytes([9; 32]);
    assert_eq!(
        executor
            .execute(identity, digest, 110, |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Rejected(b"insufficient quota".to_vec()))
            })
            .unwrap(),
        CommandExecution::Pending
    );
    let pending = executor.pending().unwrap();
    assert!(matches!(pending.outcome(), StoredOutcome::Rejected { .. }));
    let prepared = replica
        .prepare(None, pending.cuts(), pending.outcome().commit_sequence(), 1)
        .await
        .unwrap();
    executor.bind_prepared(&prepared).unwrap();
    assert!(matches!(
        executor.confirm_published(&prepared.root()).unwrap(),
        StoredOutcome::Rejected { ref result, commit_sequence: 1 }
            if result == b"insufficient quota"
    ));
    assert!(matches!(
        executor.execute(identity, Digest::from_bytes([10; 32]), 111, |_| {
            Ok(HandlerOutcome::Success(Vec::new()))
        }),
        Err(crab_cell_runtime::Error::RequestConflict)
    ));
    executor.close().unwrap();

    let connection = crab_ltx::rusqlite::Connection::open(database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT outcome FROM sys_requests WHERE request_id = ?1",
                [identity.request_id.as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );
}
