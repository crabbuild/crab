use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use bytes::Bytes;
use crab_cell_runtime::{
    CellAuthority, CellExecutor, CellId, CellPublisher, CommandExecution, Control, Digest,
    HandlerOutcome, IncarnationId, MutationIdentity, Owner, RequestId, SessionId, StoredOutcome,
    Transition, VersionedControl, install_runtime_schema,
};
use crab_ltx::{CellReplica, Limits, ManagedDb};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

const RESULT_LIMIT: usize = 1 << 20;

struct Fixture {
    _directory: tempfile::TempDir,
    database: std::path::PathBuf,
    cell: CellId,
    incarnation: IncarnationId,
    layout: CellStorageLayout,
    replica: CellReplica,
    executor: CellExecutor,
}

fn fixture() -> Fixture {
    let cell = CellId::from_bytes([1; 32]);
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
        cell,
        incarnation,
        layout,
        replica,
        executor: CellExecutor::new(writer, cell, incarnation, 1),
    }
}

async fn initialized_authority(
    layout: &CellStorageLayout,
    cell: CellId,
    incarnation: IncarnationId,
) -> (Control, CellAuthority, VersionedControl) {
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
    layout
        .store()
        .create_strict(
            &layout.control_path(cell.as_bytes()),
            Bytes::from(control.encode().unwrap()),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let observed = authority.load(cell).await.unwrap().unwrap();
    (control, authority, observed)
}

#[tokio::test]
async fn prepared_root_becomes_one_valid_control_successor() {
    let Fixture {
        _directory,
        database: _,
        cell,
        incarnation,
        layout: _,
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
            .execute(
                identity,
                operation_digest,
                20,
                RESULT_LIMIT,
                move |transaction| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(b"one".to_vec()))
                }
            )
            .unwrap(),
        CommandExecution::Pending
    );
    assert!(matches!(
        executor.execute(identity, operation_digest, 20, RESULT_LIMIT, |_| {
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
            .execute(identity, operation_digest, 21, RESULT_LIMIT, |_| {
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
        layout: _,
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
            .execute(identity, digest, 110, RESULT_LIMIT, |transaction| {
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
        executor.execute(
            identity,
            Digest::from_bytes([10; 32]),
            111,
            RESULT_LIMIT,
            |_| { Ok(HandlerOutcome::Success(Vec::new())) },
        ),
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

#[tokio::test]
async fn publisher_uploads_cas_and_releases_one_result() {
    let Fixture {
        _directory,
        database: _,
        cell,
        incarnation,
        layout,
        replica,
        mut executor,
    } = fixture();
    let (_, authority, observed) = initialized_authority(&layout, cell, incarnation).await;
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([14; 16]),
        issued_at_ms: 100,
        expires_at_ms: 20_000,
    };
    executor
        .execute(
            identity,
            Digest::from_bytes([15; 32]),
            110,
            RESULT_LIMIT,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"committed".to_vec()))
            },
        )
        .unwrap();
    let mut publisher = CellPublisher::new(replica, authority, observed);
    assert!(matches!(
        publisher
            .publish_pending(&mut executor, Some(500))
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"committed"
    ));
    assert_eq!(publisher.control().value().next_due_ms, Some(500));
    assert!(executor.pending().is_none());
    executor.close().unwrap();
}

#[tokio::test]
async fn lost_publication_response_reconciles_without_replaying_sql() {
    let Fixture {
        _directory,
        database: _,
        cell,
        incarnation,
        layout,
        replica,
        mut executor,
    } = fixture();
    let (initial, authority, stale) = initialized_authority(&layout, cell, incarnation).await;
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([12; 16]),
        issued_at_ms: 100,
        expires_at_ms: 20_000,
    };
    let digest = Digest::from_bytes([13; 32]);
    executor
        .execute(identity, digest, 110, RESULT_LIMIT, |transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(HandlerOutcome::Success(b"published".to_vec()))
        })
        .unwrap();

    let pending = executor.pending().unwrap();
    let prepared = replica
        .prepare(None, pending.cuts(), pending.outcome().commit_sequence(), 1)
        .await
        .unwrap();
    let winner = initial.publish_prepared(&prepared, None).unwrap();
    authority
        .transition(&stale, winner.clone(), Transition::Publish)
        .await
        .unwrap();

    let mut publisher = CellPublisher::new(replica, authority, stale);
    assert!(matches!(
        publisher
            .publish_pending(&mut executor, None)
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"published"
    ));
    assert_eq!(publisher.control().value(), &winner);
    assert!(executor.pending().is_none());
    executor.close().unwrap();
}

#[tokio::test]
async fn publication_rebases_over_a_pure_lease_renewal_without_sql_replay() {
    let Fixture {
        _directory,
        database: _,
        cell,
        incarnation,
        layout,
        replica,
        mut executor,
    } = fixture();
    let (initial, authority, stale) = initialized_authority(&layout, cell, incarnation).await;
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([16; 16]),
        issued_at_ms: 100,
        expires_at_ms: 20_000,
    };
    executor
        .execute(
            identity,
            Digest::from_bytes([17; 32]),
            110,
            RESULT_LIMIT,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"renewed".to_vec()))
            },
        )
        .unwrap();

    let mut renewed = initial;
    renewed.revision += 1;
    renewed.progress += 1;
    authority
        .transition(&stale, renewed, Transition::Renew)
        .await
        .unwrap();

    let mut publisher = CellPublisher::new(replica, authority, stale);
    assert!(matches!(
        publisher
            .publish_pending(&mut executor, None)
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"renewed"
    ));
    assert_eq!(publisher.control().value().revision, 3);
    executor.close().unwrap();
}
