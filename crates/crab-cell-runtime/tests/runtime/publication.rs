use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use bytes::Bytes;
use crab_cell_runtime::cell::executor::{
    CellExecutor, CommandExecution, HandlerOutcome, MutationIdentity, StoredOutcome,
};
use crab_cell_runtime::cell::schema::install_runtime_schema;
use crab_cell_runtime::control::authority::{CellAuthority, VersionedControl};
use crab_cell_runtime::control::{Control, ControlState, Owner, Transition};
use crab_cell_runtime::identity::{CellId, Digest, SessionId};
use crab_cell_runtime::identity::{IncarnationId, RequestId};
use crab_cell_runtime::publication::CellPublisher;
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Db, Host, Limits};
use crab_storage::Store;
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory, path::Path,
};

use crate::runtime::fault_fs::FaultFileSystem;

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
    fixture_with_store(Store::new(Arc::new(InMemory::new())))
}

fn fixture_with_store(store: Store) -> Fixture {
    fixture_with_store_and_host(store, Host::default())
}

fn fixture_with_store_and_host(store: Store, host: Host) -> Fixture {
    let cell = CellId::from_bytes([1; 32]);
    let incarnation = IncarnationId::from_bytes([2; 16]);
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
    let writer = Db::open_with_host(&database, Limits::default(), host).unwrap();
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

#[derive(Debug)]
struct LostUpdateResponseStore {
    inner: Arc<InMemory>,
    remaining_failures: AtomicUsize,
    updates: AtomicUsize,
}

impl LostUpdateResponseStore {
    fn new(inner: Arc<InMemory>) -> Self {
        Self {
            inner,
            remaining_failures: AtomicUsize::new(1),
            updates: AtomicUsize::new(0),
        }
    }
}

impl fmt::Display for LostUpdateResponseStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("lost-update-response-store")
    }
}

#[async_trait::async_trait]
impl ObjectStore for LostUpdateResponseStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        let update = matches!(&options.mode, PutMode::Update(_));
        let result = self.inner.put_opts(location, payload, options).await?;
        if !update {
            return Ok(result);
        }
        self.updates.fetch_add(1, Ordering::SeqCst);
        if self
            .remaining_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(object_store::Error::Generic {
                store: "lost-update-response-store",
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "control CAS response lost after commit",
                )),
            });
        }
        Ok(result)
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
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
    assert_eq!(pending.cuts().timing.fsync_nanos, 0);
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

    let compacted = replica
        .prepare_compaction(&prepared.root(), 0..1, 9, _directory.path())
        .await
        .unwrap();
    let compacted_control = published.publish_prepared(&compacted, Some(42)).unwrap();
    assert_eq!(compacted.root().position, prepared.root().position);
    assert_eq!(
        compacted.root().commit_sequence,
        prepared.root().commit_sequence
    );
    assert_ne!(compacted.root().digest, prepared.root().digest);
    assert_eq!(compacted_control.ltx_root(), Some(compacted.root()));
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
    let mut publisher =
        CellPublisher::new(replica, authority, observed, _directory.path().to_owned());
    assert!(matches!(
        publisher
            .publish_pending(&mut executor)
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"committed"
    ));
    assert_eq!(
        publisher.control().value().next_due_ms,
        Some(20_000 + 24 * 60 * 60 * 1000)
    );
    assert!(executor.pending().is_none());
    executor.close().unwrap();
}

#[tokio::test]
async fn lost_publication_response_reconciles_without_replaying_sql() {
    let fault_store = Arc::new(LostUpdateResponseStore::new(Arc::new(InMemory::new())));
    let Fixture {
        _directory,
        database: _,
        cell,
        incarnation,
        layout,
        replica,
        mut executor,
    } = fixture_with_store(Store::new(fault_store.clone()));
    let (_, authority, observed) = initialized_authority(&layout, cell, incarnation).await;
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([12; 16]),
        issued_at_ms: 100,
        expires_at_ms: 20_000,
    };
    let digest = Digest::from_bytes([13; 32]);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = calls.clone();
    executor
        .execute(identity, digest, 110, RESULT_LIMIT, move |transaction| {
            observed_calls.fetch_add(1, Ordering::SeqCst);
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(HandlerOutcome::Success(b"published".to_vec()))
        })
        .unwrap();

    let mut publisher =
        CellPublisher::new(replica, authority, observed, _directory.path().to_owned());
    assert!(matches!(
        publisher
            .publish_pending(&mut executor)
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"published"
    ));
    assert_eq!(fault_store.updates.load(Ordering::SeqCst), 1);
    assert_eq!(fault_store.remaining_failures.load(Ordering::SeqCst), 0);
    assert!(executor.pending().is_none());
    assert!(matches!(
        executor
            .execute(identity, digest, 111, RESULT_LIMIT, |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(HandlerOutcome::Success(b"replayed".to_vec()))
            })
            .unwrap(),
        CommandExecution::Recorded(StoredOutcome::Success { ref result, commit_sequence: 1 })
            if result == b"published"
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    executor.close().unwrap();
}

#[tokio::test]
async fn published_root_survives_local_prune_failure_without_replaying_sql() {
    let filesystem = Arc::new(FaultFileSystem::new());
    let host = Host::default().with_filesystem(filesystem.clone());
    let Fixture {
        _directory,
        database: _,
        cell,
        incarnation,
        layout,
        replica,
        mut executor,
    } = fixture_with_store_and_host(Store::new(Arc::new(InMemory::new())), host);
    let (_, authority, observed) = initialized_authority(&layout, cell, incarnation).await;
    let recovery_replica = replica.clone();
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([21; 16]),
        issued_at_ms: 100,
        expires_at_ms: 20_000,
    };
    let digest = Digest::from_bytes([22; 32]);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = calls.clone();
    assert_eq!(
        executor
            .execute(identity, digest, 110, RESULT_LIMIT, move |transaction| {
                observed_calls.fetch_add(1, Ordering::SeqCst);
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"committed".to_vec()))
            })
            .unwrap(),
        CommandExecution::Pending
    );

    filesystem.fail_next_prune();
    let mut publisher =
        CellPublisher::new(replica, authority, observed, _directory.path().to_owned());
    assert!(matches!(
        publisher.publish_pending(&mut executor).await,
        Err(crab_cell_runtime::Error::Ltx(_))
    ));
    assert!(filesystem.prune_failure_consumed());
    assert!(executor.pending().is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let root = publisher.control().value().ltx_root().unwrap();
    let observed_control = CellAuthority::new(layout)
        .load(cell)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed_control.value().ltx_root(), Some(root));
    assert_eq!(root.commit_sequence, 1);
    assert_eq!(root.position.txid, 1);
    assert!(matches!(
        executor.execute(identity, digest, 111, RESULT_LIMIT, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(HandlerOutcome::Success(b"duplicate".to_vec()))
        }),
        Err(crab_cell_runtime::Error::PendingPublication)
    ));
    drop(executor);

    let restored = _directory.path().join("restored.sqlite");
    let verified = recovery_replica.open_root(&root).await.unwrap();
    assert_eq!(verified.root(), root);
    assert_eq!(verified.restore(&restored).await.unwrap(), root.position);
    let mut recovered = CellExecutor::new(
        Db::open(&restored, Limits::default()).unwrap(),
        cell,
        incarnation,
        1,
    );
    assert!(matches!(
        recovered
            .execute(identity, digest, 112, RESULT_LIMIT, |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(HandlerOutcome::Success(b"duplicate".to_vec()))
            })
            .unwrap(),
        CommandExecution::Recorded(StoredOutcome::Success { ref result, commit_sequence: 1 })
            if result == b"committed"
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    recovered.close().unwrap();
}

#[tokio::test]
async fn published_root_observed_after_takeover_fences_the_old_executor() {
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
        request_id: RequestId::from_bytes([18; 16]),
        issued_at_ms: 100,
        expires_at_ms: 20_000,
    };
    executor
        .execute(
            identity,
            Digest::from_bytes([19; 32]),
            110,
            RESULT_LIMIT,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(
                    b"published-before-takeover".to_vec(),
                ))
            },
        )
        .unwrap();
    let pending = executor.pending().unwrap();
    let prepared = replica
        .prepare(None, pending.cuts(), pending.outcome().commit_sequence(), 1)
        .await
        .unwrap();
    let published = authority
        .transition(
            &stale,
            initial.publish_prepared(&prepared, None).unwrap(),
            Transition::Publish,
        )
        .await
        .unwrap();
    let mut takeover = published.value().clone();
    takeover.epoch += 1;
    takeover.revision += 1;
    takeover.progress += 1;
    takeover.state = ControlState::Recovering;
    takeover.owner = Some(Owner {
        session: SessionId::from_bytes([20; 16]),
        endpoint: "https://replacement.internal:8081".into(),
    });
    authority
        .transition(&published, takeover, Transition::Takeover)
        .await
        .unwrap();

    let mut publisher = CellPublisher::new(replica, authority, stale, _directory.path().to_owned());
    assert!(matches!(
        publisher.publish_pending(&mut executor).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    assert!(matches!(
        executor.execute(
            identity,
            Digest::from_bytes([19; 32]),
            111,
            RESULT_LIMIT,
            |_| Ok(HandlerOutcome::Success(Vec::new())),
        ),
        Err(crab_cell_runtime::Error::Fenced)
    ));
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

    let mut publisher = CellPublisher::new(replica, authority, stale, _directory.path().to_owned());
    assert!(matches!(
        publisher
            .publish_pending(&mut executor)
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"renewed"
    ));
    assert_eq!(publisher.control().value().revision, 3);
    executor.close().unwrap();
}
