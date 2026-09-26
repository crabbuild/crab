use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
};

use bytes::Bytes;
use crab_cell_runtime::cell::actor::CellRuntime;
use crab_cell_runtime::cell::catalog::CatalogEntry;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::executor::Resolution;
use crab_cell_runtime::cell::executor::{HandlerOutcome, StoredOutcome};
use crab_cell_runtime::cell::worker::ACTIVE_CELL_FILE_DESCRIPTORS;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::control::{ControlState, Owner, Transition};
use crab_cell_runtime::fleet::pressure::{PressureSample, PressureState};
use crab_cell_runtime::follower::FollowerReceipt;
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_cell_runtime::identity::{IncarnationId, NodeId};
use crab_cell_runtime::ltx::{DiskBudget, Host as ReplicaHost};
use crab_cell_runtime::node::durability::{NodeDurability, NodeLogAuthority};
use crab_cell_runtime::node::lease::NodeLeaseGuard;
use crab_cell_runtime::node::log::{DurabilityGate, NodeLogRotationBarrier};
use crab_cell_runtime::node::log_shipper::NodeLogShipper;
use crab_cell_runtime::node::log_transport::{
    AppendRequest, NodeLogTransport, RetireRequest, SealRequest, TailRequest,
};
use crab_cell_runtime::primitives::effects::InboxDelivery;
use crab_cell_runtime::primitives::queue::install_queue_schema;
use crab_cell_runtime::primitives::workflow::install_workflow_schema;
use crab_ltx::{CellObjectKind, CellStorageLayout};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{ObjectStoreCredentials, RetryPolicy, Store, build_explicit_store};
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory, path::Path,
};
use tokio::sync::Notify;

use crate::support::fencing::fence_session;
use crate::support::fixtures::{mutation_identity_window, now_ms};

// Capability modules keep the suite navigable; shared fixtures and
// helpers used by more than one capability stay here.
pub mod durability;
pub mod execution;
pub mod idle;
pub mod ownership;
pub mod residency;

#[derive(Debug)]
struct PausingStore {
    inner: Arc<InMemory>,
    armed: AtomicBool,
    update_armed: AtomicBool,
    failing: AtomicBool,
    transient_put_failures: AtomicUsize,
    lost_update_response: AtomicBool,
    failed: AtomicBool,
    blocked: AtomicBool,
    released: AtomicBool,
    get_armed: AtomicBool,
    fail_next_get: AtomicBool,
    transient_get_failures: AtomicUsize,
    get_blocked: AtomicBool,
    get_released: AtomicBool,
    entered: Notify,
    release: Notify,
    get_entered: Notify,
    get_release: Notify,
    parallel_catalog_heads: AtomicBool,
    catalog_head_barrier: tokio::sync::Barrier,
}

impl PausingStore {
    fn new(inner: Arc<InMemory>) -> Self {
        Self {
            inner,
            armed: AtomicBool::new(false),
            update_armed: AtomicBool::new(false),
            failing: AtomicBool::new(false),
            transient_put_failures: AtomicUsize::new(0),
            lost_update_response: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
            released: AtomicBool::new(false),
            get_armed: AtomicBool::new(false),
            fail_next_get: AtomicBool::new(false),
            transient_get_failures: AtomicUsize::new(0),
            get_blocked: AtomicBool::new(false),
            get_released: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
            get_entered: Notify::new(),
            get_release: Notify::new(),
            parallel_catalog_heads: AtomicBool::new(false),
            catalog_head_barrier: tokio::sync::Barrier::new(2),
        }
    }

    fn require_parallel_catalog_heads(&self) {
        self.parallel_catalog_heads.store(true, Ordering::Release);
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    fn arm_next_update(&self) {
        self.update_armed.store(true, Ordering::Release);
    }

    fn fail_puts(&self) {
        self.failing.store(true, Ordering::Release);
    }

    fn fail_next_put_transiently(&self) {
        self.transient_put_failures.store(1, Ordering::Release);
    }

    fn lose_next_update_response(&self) {
        self.lost_update_response.store(true, Ordering::Release);
    }

    fn lost_update_response_consumed(&self) -> bool {
        !self.lost_update_response.load(Ordering::Acquire)
    }

    fn allow_puts(&self) {
        self.failing.store(false, Ordering::Release);
    }

    async fn wait_until_failed(&self) {
        while !self.failed.load(Ordering::Acquire) {
            self.entered.notified().await;
        }
    }

    async fn wait_until_blocked(&self) {
        while !self.blocked.load(Ordering::Acquire) {
            self.entered.notified().await;
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.release.notify_waiters();
    }

    fn arm_gets(&self) {
        self.get_armed.store(true, Ordering::Release);
    }

    fn fail_next_get(&self) {
        self.fail_next_get.store(true, Ordering::Release);
    }

    async fn wait_until_get_blocked(&self) {
        while !self.get_blocked.load(Ordering::Acquire) {
            self.get_entered.notified().await;
        }
    }

    fn release_gets(&self) {
        self.get_released.store(true, Ordering::Release);
        self.get_release.notify_waiters();
    }
}

impl fmt::Display for PausingStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("pausing-store")
    }
}

#[async_trait::async_trait]
impl ObjectStore for PausingStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if self
            .transient_put_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            self.failed.store(true, Ordering::Release);
            self.entered.notify_waiters();
            return Err(object_store::Error::Generic {
                store: "pausing-store",
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected transient put failure",
                )),
            });
        }
        if self.failing.load(Ordering::Acquire) {
            self.failed.store(true, Ordering::Release);
            self.entered.notify_waiters();
            return Err(object_store::Error::PermissionDenied {
                path: location.to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected put failure",
                )),
            });
        }
        let update = matches!(&options.mode, PutMode::Update(_));
        if (self.armed.load(Ordering::Acquire)
            || (update && self.update_armed.load(Ordering::Acquire)))
            && !self.blocked.swap(true, Ordering::AcqRel)
        {
            self.entered.notify_waiters();
            while !self.released.load(Ordering::Acquire) {
                self.release.notified().await;
            }
        }
        let result = self.inner.put_opts(location, payload, options).await?;
        if update && self.lost_update_response.swap(false, Ordering::AcqRel) {
            return Err(object_store::Error::Generic {
                store: "pausing-store",
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
        if self.get_armed.load(Ordering::Acquire) && !self.get_blocked.swap(true, Ordering::AcqRel)
        {
            self.get_entered.notify_waiters();
            while !self.get_released.load(Ordering::Acquire) {
                self.get_release.notified().await;
            }
        }
        if self.parallel_catalog_heads.load(Ordering::Acquire)
            && location.as_ref().ends_with("/head.json")
        {
            self.catalog_head_barrier.wait().await;
        }
        if options.range.is_some()
            && self
                .transient_get_failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
        {
            return Err(object_store::Error::Generic {
                store: "pausing-store",
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected transient get failure",
                )),
            });
        }
        if self.fail_next_get.swap(false, Ordering::AcqRel) {
            return Err(object_store::Error::PermissionDenied {
                path: location.to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected origin read failure during hydration",
                )),
            });
        }
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

#[derive(Default)]
struct TestNodeAuthority {
    activations: Mutex<Vec<u64>>,
    coverage: Mutex<Vec<(u64, u64)>>,
    closes: Mutex<Vec<u64>>,
}

impl NodeLogAuthority for TestNodeAuthority {
    fn activate<'a>(
        &'a self,
        log_epoch: u64,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<()>> {
        Box::pin(async move {
            self.activations.lock().unwrap().push(log_epoch);
            Ok(())
        })
    }

    fn advance_coverage<'a>(
        &'a self,
        log_epoch: u64,
        tiered_through: u64,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<()>> {
        Box::pin(async move {
            self.coverage
                .lock()
                .unwrap()
                .push((log_epoch, tiered_through));
            Ok(())
        })
    }

    fn close<'a>(
        &'a self,
        barrier: &'a NodeLogRotationBarrier,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<()>> {
        Box::pin(async move {
            self.closes.lock().unwrap().push(barrier.log_epoch());
            Ok(())
        })
    }
}

#[derive(Default)]
struct TestNodeTransport(Option<std::time::Duration>);

impl NodeLogTransport for TestNodeTransport {
    fn append<'a>(
        &'a self,
        _member: NodeId,
        request: AppendRequest,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        let delay = self.0;
        Box::pin(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let last = request.frames.last().ok_or(crab_cell_runtime::Error::Node(
                "test node-log append is empty",
            ))?;
            let frame = crab_ltx::inspect_node_frame(last.clone(), Limits::default())?;
            Ok(FollowerReceipt {
                base_sequence: 1,
                durable_through: frame.scope().node_sequence,
            })
        })
    }

    fn seal<'a>(
        &'a self,
        _member: NodeId,
        _request: SealRequest,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        Box::pin(async { Err(crab_cell_runtime::Error::Node("unused test seal")) })
    }

    fn tail<'a>(
        &'a self,
        _member: NodeId,
        _request: TailRequest,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<Vec<Bytes>>> {
        Box::pin(async { Err(crab_cell_runtime::Error::Node("unused test tail")) })
    }

    fn retire<'a>(
        &'a self,
        _member: NodeId,
        request: RetireRequest,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        Box::pin(async move {
            Ok(FollowerReceipt {
                base_sequence: request.covered_through.saturating_add(1),
                durable_through: request.covered_through,
            })
        })
    }
}

struct LostAckFollowerTransport {
    inner: crab_cell_runtime::node::log_transport::LocalFollowerTransport,
    acknowledged_once: AtomicBool,
    lost_ticket: Mutex<
        Option<(
            NodeId,
            crab_ltx::NodeFrameScope,
            crab_ltx::NodeFrameScope,
            FollowerReceipt,
        )>,
    >,
}

impl NodeLogTransport for LostAckFollowerTransport {
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        Box::pin(async move {
            let first = crab_ltx::inspect_node_frame(
                request.frames.first().unwrap().clone(),
                Limits::default(),
            )?;
            let last = crab_ltx::inspect_node_frame(
                request.frames.last().unwrap().clone(),
                Limits::default(),
            )?;
            let receipt = self.inner.append(member, request).await?;
            if !self.acknowledged_once.swap(true, Ordering::AcqRel) {
                return Ok(receipt);
            }
            *self.lost_ticket.lock().unwrap() =
                Some((member, first.scope(), last.scope(), receipt));
            Err(crab_cell_runtime::Error::Node(
                "injected follower acknowledgement loss",
            ))
        })
    }

    fn seal<'a>(
        &'a self,
        member: NodeId,
        request: SealRequest,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        self.inner.seal(member, request)
    }

    fn tail<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<Vec<Bytes>>> {
        self.inner.tail(member, request)
    }

    fn retire<'a>(
        &'a self,
        member: NodeId,
        request: RetireRequest,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        self.inner.retire(member, request)
    }
}

async fn fence_log_session(
    layout: &CellStorageLayout,
    session: SessionId,
    claimant: SessionId,
    member: SessionId,
    tiered_through: u64,
) -> crab_cell_runtime::node::FencedNodeSession {
    let fleet = Digest::from_bytes([90; 32]);
    let image = Digest::from_bytes([91; 32]);
    let release = Digest::from_bytes([92; 32]);
    let directory =
        crab_cell_runtime::node::NodeDirectory::new(layout.clone(), fleet, image, release);
    let key = ed25519_dalek::SigningKey::from_bytes(&[93; 32]);
    let signed =
        |session: crab_cell_runtime::SessionId, endpoint: &str, issued_at_ms, expires_at_ms| {
            crab_cell_runtime::node::NodeAdvertisement::sign(
                crab_cell_runtime::identity::NodeId::from_bytes(*session.as_bytes()),
                session,
                endpoint.into(),
                fleet,
                Digest::from_bytes([94; 32]),
                image,
                release,
                &key,
                1,
                issued_at_ms,
                expires_at_ms,
                vec![Digest::from_bytes([95; 32])],
                vec![1],
                crab_cell_runtime::node::NodeFailureDomain::default(),
                crab_cell_runtime::node::NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    follower_free_bytes: 1,
                    follower_retained_bytes: 0,
                    job_credits: 1,
                    log_protocol: crab_cell_runtime::node::NODE_LOG_PROTOCOL_VERSION,
                },
            )
            .unwrap()
        };
    let leader = directory
        .create(
            signed(session, "https://expired.internal:8081", 1, 10_001),
            1,
        )
        .await
        .unwrap();
    directory
        .create(
            signed(member, "https://follower.internal:8081", 10_000, 20_000),
            2,
        )
        .await
        .unwrap();
    let enrolled = directory.recruit_log(&leader, 1, 1, 2, 2).await.unwrap();
    let enrolled = directory.activate_log(&enrolled, 3).await.unwrap();
    if tiered_through != 0 {
        directory
            .advance_log_coverage(&enrolled, tiered_through, 4)
            .await
            .unwrap();
    }
    if claimant != member {
        directory
            .create(
                signed(claimant, "https://claimant.internal:8081", 10_000, 20_000),
                3,
            )
            .await
            .unwrap();
    }
    directory
        .claim_expired(session, claimant, 10_001)
        .await
        .unwrap()
}

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
    fixture_with_limits(partition, Limits::default())
}

fn fixture_with_limits(partition: &[u8], limits: Limits) -> Fixture {
    fixture_with_limits_and_store(partition, limits, Store::new(Arc::new(InMemory::new())))
}

fn fixture_with_limits_and_store(partition: &[u8], limits: Limits, store: Store) -> Fixture {
    fixture_with_limits_and_store_at_prefix(partition, limits, store, Path::from("runtime"))
}

fn fixture_with_limits_and_store_at_prefix(
    partition: &[u8],
    limits: Limits,
    store: Store,
    prefix: Path,
) -> Fixture {
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        NamespaceId::from_bytes([6; 16]),
        partition,
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([2; 16]);
    let layout = CellStorageLayout::new(store, prefix, [3; 16]);
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        limits,
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

#[cfg(feature = "test-support")]
fn filesystem_fixture(partition: &[u8], root: &std::path::Path) -> Fixture {
    let store = crab_cell_runtime::test_support::FilesystemCasStore::new(root).unwrap();
    fixture_with_limits_and_store(partition, Limits::default(), Store::new(Arc::new(store)))
}

async fn activate(
    fixture: &Fixture,
    node_bytes: usize,
) -> crab_cell_runtime::cell::actor::CellHandle {
    activate_runtime(fixture, node_bytes).await.1
}

async fn activate_runtime(
    fixture: &Fixture,
    node_bytes: usize,
) -> (
    CellRuntime,
    crab_cell_runtime::cell::actor::CellHandle,
    SqlWorkerPool,
) {
    let session = SessionId::from_bytes([4; 16]);
    let pool = SqlWorkerPool::new(2, 10).unwrap();
    let runtime = CellRuntime::new(pool.clone(), node_bytes, session).unwrap();
    let handle = bootstrap_on(&runtime, fixture, session).await;
    (runtime, handle, pool)
}

async fn wait_for_persisted_work(
    handle: &crab_cell_runtime::cell::actor::CellHandle,
    role: CatalogRole,
    blocker: &'static str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if handle
                .persisted_work_inventory(role)
                .await
                .unwrap()
                .first_blocker()
                == Some(blocker)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}

async fn bootstrap_on(
    runtime: &CellRuntime,
    fixture: &Fixture,
    session: SessionId,
) -> crab_cell_runtime::cell::actor::CellHandle {
    bootstrap_role_on(
        runtime,
        fixture,
        session,
        CatalogRole::Repository,
        |transaction| {
            transaction.execute_batch(
                "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
            )?;
            Ok(())
        },
    )
    .await
}

async fn bootstrap_role_on<F>(
    runtime: &CellRuntime,
    fixture: &Fixture,
    session: SessionId,
    role: CatalogRole,
    initialize: F,
) -> crab_cell_runtime::cell::actor::CellHandle
where
    F: for<'connection> FnOnce(
            &crab_ltx::rusqlite::Transaction<'connection>,
        ) -> crab_cell_runtime::Result<()>
        + Send
        + 'static,
{
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .provision(
            CatalogEntry::new(&fixture.target, role, Digest::from_bytes([5; 32]), 1).unwrap(),
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
            initialize,
        )
        .await
        .unwrap()
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
