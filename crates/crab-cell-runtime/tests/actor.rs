use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use bytes::Bytes;
use crab_cell_runtime::{
    ACTIVE_CELL_FILE_DESCRIPTORS, AppendRequest, ApplicationId, CatalogEntry, CatalogRole,
    CellAuthority, CellRuntime, CellTarget, ControlState, Digest, DiskBudget, DurabilityGate,
    FollowerReceipt, HandlerOutcome, InboxDelivery, IncarnationId, MutationIdentity, NamespaceId,
    NodeDurability, NodeId, NodeLeaseGuard, NodeLogAuthority, NodeLogRotationBarrier,
    NodeLogShipper, NodeLogTransport, Owner, PressureSample, PressureState, ReplicaHost, RequestId,
    Resolution, RetireRequest, SealRequest, SessionId, SqlWorkerPool, StoredOutcome, TailRequest,
    TenantId, Transition, install_queue_schema, install_workflow_schema,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{
    CellObjectKind, CellStorageLayout, ObjectStoreCredentials, Store, build_explicit_store,
};
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory, path::Path,
};
use tokio::sync::Notify;

#[derive(Debug)]
struct PausingStore {
    inner: Arc<InMemory>,
    armed: AtomicBool,
    failing: AtomicBool,
    failed: AtomicBool,
    blocked: AtomicBool,
    released: AtomicBool,
    entered: Notify,
    release: Notify,
    parallel_catalog_heads: AtomicBool,
    catalog_head_barrier: tokio::sync::Barrier,
}

impl PausingStore {
    fn new(inner: Arc<InMemory>) -> Self {
        Self {
            inner,
            armed: AtomicBool::new(false),
            failing: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
            released: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
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

    fn fail_puts(&self) {
        self.failing.store(true, Ordering::Release);
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
        if self.armed.load(Ordering::Acquire) && !self.blocked.swap(true, Ordering::AcqRel) {
            self.entered.notify_waiters();
            while !self.released.load(Ordering::Acquire) {
                self.release.notified().await;
            }
        }
        self.inner.put_opts(location, payload, options).await
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
        if self.parallel_catalog_heads.load(Ordering::Acquire)
            && location.as_ref().ends_with("/head.json")
        {
            self.catalog_head_barrier.wait().await;
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
    inner: crab_cell_runtime::LocalFollowerTransport,
    acknowledged_once: AtomicBool,
}

impl NodeLogTransport for LostAckFollowerTransport {
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        Box::pin(async move {
            let receipt = self.inner.append(member, request).await?;
            if !self.acknowledged_once.swap(true, Ordering::AcqRel) {
                return Ok(receipt);
            }
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

async fn fence_session(
    layout: &CellStorageLayout,
    session: SessionId,
    claimant: SessionId,
) -> crab_cell_runtime::FencedNodeSession {
    let fleet = Digest::from_bytes([90; 32]);
    let image = Digest::from_bytes([91; 32]);
    let release = Digest::from_bytes([92; 32]);
    let directory = crab_cell_runtime::NodeDirectory::new(layout.clone(), fleet, image, release);
    let key = ed25519_dalek::SigningKey::from_bytes(&[93; 32]);
    directory
        .create(
            crab_cell_runtime::NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*session.as_bytes()),
                session,
                "https://expired.internal:8081".into(),
                fleet,
                Digest::from_bytes([94; 32]),
                image,
                release,
                &key,
                1,
                1,
                10_001,
                vec![Digest::from_bytes([95; 32])],
                vec![1],
                crab_cell_runtime::NodeFailureDomain::default(),
                crab_cell_runtime::NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    job_credits: 1,
                    ..crab_cell_runtime::NodeCapacity::default()
                },
            )
            .unwrap(),
            1,
        )
        .await
        .unwrap();
    directory
        .create(
            crab_cell_runtime::NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*claimant.as_bytes()),
                claimant,
                "https://claimant.internal:8081".into(),
                fleet,
                Digest::from_bytes([94; 32]),
                image,
                release,
                &key,
                1,
                10_000,
                20_000,
                vec![Digest::from_bytes([95; 32])],
                vec![1],
                crab_cell_runtime::NodeFailureDomain::default(),
                crab_cell_runtime::NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    job_credits: 1,
                    ..crab_cell_runtime::NodeCapacity::default()
                },
            )
            .unwrap(),
            10_000,
        )
        .await
        .unwrap();
    directory
        .claim_expired(session, claimant, 10_001)
        .await
        .unwrap()
}

async fn fence_log_session(
    layout: &CellStorageLayout,
    session: SessionId,
    claimant: SessionId,
    member: SessionId,
    tiered_through: u64,
) -> crab_cell_runtime::FencedNodeSession {
    let fleet = Digest::from_bytes([90; 32]);
    let image = Digest::from_bytes([91; 32]);
    let release = Digest::from_bytes([92; 32]);
    let directory = crab_cell_runtime::NodeDirectory::new(layout.clone(), fleet, image, release);
    let key = ed25519_dalek::SigningKey::from_bytes(&[93; 32]);
    let signed =
        |session: crab_cell_runtime::SessionId, endpoint: &str, issued_at_ms, expires_at_ms| {
            crab_cell_runtime::NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*session.as_bytes()),
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
                crab_cell_runtime::NodeFailureDomain::default(),
                crab_cell_runtime::NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    follower_free_bytes: 1,
                    follower_retained_bytes: 0,
                    job_credits: 1,
                    log_protocol: crab_cell_runtime::NODE_LOG_PROTOCOL_VERSION,
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

#[tokio::test(flavor = "multi_thread")]
async fn recovery_inventory_reads_catalog_heads_concurrently() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let object_store: Arc<dyn ObjectStore> = pausing.clone();
    let fixture = fixture_with_limits_and_store(
        b"parallel-recovery-inventory",
        Limits::default(),
        Store::new(object_store),
    );
    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let authority = CellAuthority::new(fixture.layout.clone());
    pausing.require_parallel_catalog_heads();

    let inventory = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        crab_cell_runtime::recoverable_cells(
            &catalog,
            &authority,
            SessionId::from_bytes([99; 16]),
            10,
        ),
    )
    .await
    .expect("catalog head reads remained serial")
    .unwrap();

    assert!(inventory.is_empty());
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
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        NamespaceId::from_bytes([6; 16]),
        partition,
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([2; 16]);
    let layout = CellStorageLayout::new(store, Path::from("runtime"), [3; 16]);
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
    let local_disk = DiskBudget::new(4_096);
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(1, 1).unwrap(),
        1_024,
        session,
        ReplicaHost::default().with_local_disk_budget(local_disk.clone()),
    )
    .unwrap();
    let disk = local_disk.try_reserve(512).unwrap();
    let held = runtime.try_reserve_node_bytes(1_024).unwrap();

    let full = runtime.stats();
    assert_eq!(full.active_cells(), 0);
    assert_eq!(full.active_cell_capacity(), 1);
    assert_eq!(full.file_descriptors(), 0);
    assert_eq!(
        full.file_descriptor_capacity(),
        ACTIVE_CELL_FILE_DESCRIPTORS
    );
    assert_eq!(full.retained_bytes(), 1_024);
    assert_eq!(full.retained_capacity_bytes(), 1_024);
    assert_eq!(full.local_disk_reserved_bytes(), 512);
    assert_eq!(full.local_disk_capacity_bytes(), 4_096);

    assert!(matches!(
        runtime.try_reserve_node_bytes(1),
        Err(crab_cell_runtime::Error::Capacity("node retained bytes"))
    ));
    drop(held);
    drop(disk);
    let empty = runtime.stats();
    assert_eq!(empty.file_descriptors(), 0);
    assert_eq!(
        empty.file_descriptor_capacity(),
        ACTIVE_CELL_FILE_DESCRIPTORS
    );
    assert_eq!(empty.retained_bytes(), 0);
    assert_eq!(empty.local_disk_reserved_bytes(), 0);
    let released = runtime.try_reserve_node_bytes(1_024).unwrap();
    drop(released);

    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn actor_pressure_observation_uses_hysteresis_and_shared_eviction_path() {
    let session = SessionId::from_bytes([41; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 1_024, session).unwrap();
    let high = PressureSample {
        at_ms: 0,
        memory_used_permille: 900,
        disk_used_permille: 100,
        jobs_used_permille: 100,
        stale: false,
    };
    assert_eq!(
        runtime.observe_pressure(high).await.unwrap(),
        PressureState::Normal
    );
    assert_eq!(
        runtime
            .observe_pressure(PressureSample {
                at_ms: 1_000,
                ..high
            })
            .await
            .unwrap(),
        PressureState::Shedding
    );
    assert_eq!(runtime.evict_idle(1).await.unwrap(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_replaces_node_durability_binding_for_a_new_epoch() {
    let fixture = fixture_for(b"node-durability-rotation");
    let session = SessionId::from_bytes([60; 16]);
    let leader = NodeId::from_bytes([61; 16]);
    let follower = NodeId::from_bytes([62; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(TestNodeTransport(None));
    let make_durability = |log_epoch| {
        let gate = DurabilityGate::new(session, leader, log_epoch, [follower]).unwrap();
        let shipper =
            NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
        let authority: Arc<dyn NodeLogAuthority> = Arc::new(TestNodeAuthority::default());
        Arc::new(NodeDurability::new(
            gate,
            shipper,
            authority,
            Arc::clone(&transport),
            lease.clone(),
        ))
    };
    let first = make_durability(1);
    runtime
        .install_node_durability(fixture.target.application(), Arc::clone(&first))
        .unwrap();
    let second = make_durability(2);

    let previous = runtime
        .replace_node_durability(fixture.target.application(), Arc::clone(&second))
        .unwrap();

    assert!(Arc::ptr_eq(&previous, &first));
    let (application, current) = runtime.node_durability().unwrap();
    assert_eq!(application, fixture.target.application());
    assert!(Arc::ptr_eq(&current, &second));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn fleet_runtime_stays_fenced_until_one_live_node_lease_is_installed() {
    let session = SessionId::from_bytes([41; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        1_024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();

    assert!(matches!(
        runtime.try_reserve_node_bytes(1),
        Err(crab_cell_runtime::Error::Fenced)
    ));
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    assert!(runtime.install_node_lease(lease.clone()).is_err());
    drop(runtime.try_reserve_node_bytes(1).unwrap());

    lease.fence();
    assert!(matches!(
        runtime.try_reserve_node_bytes(1),
        Err(crab_cell_runtime::Error::Fenced)
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn node_lease_expiry_hides_an_inflight_committed_command() {
    let fixture = fixture_for(b"node-lease-output-gate");
    let session = SessionId::from_bytes([42; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();

    let command = tokio::spawn(async move {
        handle
            .execute(
                identity(43),
                Digest::from_bytes([44; 32]),
                20,
                1,
                16,
                move |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    entered_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    Ok(HandlerOutcome::Success(b"hidden".to_vec()))
                },
            )
            .await
    });
    tokio::task::spawn_blocking(move || {
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
    })
    .await
    .unwrap();
    lease.fence();
    resume_tx.send(()).unwrap();

    match command.await.unwrap() {
        Err(crab_cell_runtime::Error::OutcomeUnknown { source, .. }) => {
            assert!(matches!(*source, crab_cell_runtime::Error::Fenced));
        }
        other => panic!("unexpected command result: {other:?}"),
    }
    let control = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(control.value().root.as_ref().unwrap().commit_sequence, 0);
    assert!(matches!(
        runtime.shutdown().await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn command_flows_through_follower_proof_object_coverage_and_clean_close() {
    let fixture = fixture_for(b"node-durability-command");
    let session = SessionId::from_bytes([46; 16]);
    let leader = NodeId::from_bytes([47; 16]);
    let follower = NodeId::from_bytes([48; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let gate = DurabilityGate::new(session, leader, 1, [follower]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(TestNodeTransport::default());
    let shipper =
        NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
    let authority = Arc::new(TestNodeAuthority::default());
    let node_authority: Arc<dyn NodeLogAuthority> = authority.clone();
    let durability = Arc::new(NodeDurability::new(
        gate,
        shipper,
        node_authority,
        transport,
        lease,
    ));
    runtime
        .install_node_durability(fixture.target.application(), durability)
        .unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;

    let outcome = handle
        .execute(
            identity(49),
            Digest::from_bytes([50; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"durable".to_vec()))
            },
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        StoredOutcome::Success {
            ref result,
            commit_sequence: 1
        } if result == b"durable"
    ));
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(*authority.coverage.lock().unwrap(), vec![(1, 1)]);
    assert_eq!(*authority.closes.lock().unwrap(), vec![1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn follower_proofs_advance_logical_head_and_bound_the_object_backlog() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let object_store: Arc<dyn ObjectStore> = pausing.clone();
    let fixture = fixture_with_limits_and_store(
        b"dual-head-command",
        Limits::default(),
        Store::new(object_store),
    );
    let session = SessionId::from_bytes([51; 16]);
    let leader = NodeId::from_bytes([52; 16]);
    let follower = NodeId::from_bytes([53; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let gate = DurabilityGate::new(session, leader, 1, [follower]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(TestNodeTransport::default());
    let shipper =
        NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
    let authority = Arc::new(TestNodeAuthority::default());
    let node_authority: Arc<dyn NodeLogAuthority> = authority.clone();
    runtime
        .install_node_durability(
            fixture.target.application(),
            Arc::new(NodeDurability::new(
                gate,
                shipper,
                node_authority,
                transport,
                lease,
            )),
        )
        .unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    pausing.arm();

    let first = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle.execute(
            identity(54),
            Digest::from_bytes([55; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"one".to_vec()))
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.commit_sequence(), 1);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        pausing.wait_until_blocked(),
    )
    .await
    .unwrap();

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle.execute(
            identity(56),
            Digest::from_bytes([57; 32]),
            21,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"two".to_vec()))
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.commit_sequence(), 2);
    let value = handle
        .query(64, 64, |connection| {
            let value = connection
                .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
            Ok(value.to_be_bytes().to_vec())
        })
        .await
        .unwrap();
    assert_eq!(i64::from_be_bytes(value.try_into().unwrap()), 2);

    for sequence in 3_u8..=64 {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            handle.execute(
                identity(sequence.saturating_add(54)),
                Digest::from_bytes([sequence.saturating_add(55); 32]),
                20 + i64::from(sequence),
                1_024,
                1_024,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(outcome.commit_sequence(), u64::from(sequence));
    }
    let sixty_fifth = handle.execute(
        identity(119),
        Digest::from_bytes([120; 32]),
        85,
        1_024,
        1_024,
        |transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(HandlerOutcome::Success(Vec::new()))
        },
    );
    tokio::pin!(sixty_fifth);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut sixty_fifth)
            .await
            .is_err()
    );
    let blocked = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blocked.value().root.as_ref().unwrap().commit_sequence, 0);
    assert!(runtime.stats().unpublished_node_log_bytes() > 0);

    pausing.release();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), &mut sixty_fifth)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.commit_sequence(), 65);
    tokio::time::timeout(std::time::Duration::from_secs(5), handle.drain())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runtime.stats().unpublished_node_log_bytes(), 0);
    runtime.shutdown().await.unwrap();
    let released = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(released.value().root.as_ref().unwrap().commit_sequence, 65);
    assert_eq!(*authority.activations.lock().unwrap(), vec![1]);
    assert!(
        authority
            .coverage
            .lock()
            .unwrap()
            .last()
            .is_some_and(|(epoch, covered)| *epoch == 1 && *covered >= 65)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fleet_proof_retains_owner_when_object_publication_fails_first() {
    let store = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let object_store: Arc<dyn ObjectStore> = store.clone();
    let fixture = fixture_with_limits_and_store(
        b"fleet-object-failure",
        Limits::default(),
        Store::new(object_store),
    );
    let session = SessionId::from_bytes([121; 16]);
    let leader = NodeId::from_bytes([122; 16]);
    let follower = NodeId::from_bytes([123; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let gate = DurabilityGate::new(session, leader, 1, [follower]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(TestNodeTransport(Some(
        std::time::Duration::from_millis(50),
    )));
    let shipper =
        NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
    runtime
        .install_node_durability(
            fixture.target.application(),
            Arc::new(NodeDurability::new(
                gate,
                shipper,
                Arc::new(TestNodeAuthority::default()),
                transport,
                lease,
            )),
        )
        .unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    store.fail_puts();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle.execute(
            identity(124),
            Digest::from_bytes([125; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"fleet-proof".to_vec()))
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.commit_sequence(), 1);
    tokio::time::timeout(std::time::Duration::from_secs(2), store.wait_until_failed())
        .await
        .unwrap();

    let control = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(control.value().root.as_ref().unwrap().commit_sequence, 0);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match handle.query(1, 1, |_| Ok(Vec::new())).await {
                Err(crab_cell_runtime::Error::Fenced) => break,
                Ok(_) => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected query result after publication failure: {error}"),
            }
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while runtime.stats().active_cells() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let fenced = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fenced.value().state, ControlState::Serving);
    assert_eq!(fenced.value().owner.as_ref().unwrap().session, session);
    assert_eq!(fenced.value().root.as_ref().unwrap().commit_sequence, 0);
    assert!(runtime.stats().unpublished_node_log_bytes() > 0);
    store.allow_puts();
    assert!(matches!(
        runtime.shutdown().await,
        Err(crab_cell_runtime::Error::PendingPublication)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_ack_suffix_recovers_an_ambiguous_command_without_reexecution() {
    let store = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let object_store: Arc<dyn ObjectStore> = store.clone();
    let fixture = fixture_with_limits_and_store(
        b"lost-ack-recovery",
        Limits::default(),
        Store::new(object_store),
    );
    let leader = SessionId::from_bytes([126; 16]);
    let follower = SessionId::from_bytes([127; 16]);
    let member = NodeId::from_bytes(*follower.as_bytes());
    let successor = SessionId::from_bytes([128; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        leader,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let follower_directory = tempfile::TempDir::new().unwrap();
    let follower_store = crab_cell_runtime::FollowerStore::open(
        follower_directory.path().to_owned(),
        Limits::default(),
        DiskBudget::new(1 << 30),
    )
    .unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(LostAckFollowerTransport {
        inner: crab_cell_runtime::LocalFollowerTransport::new(member, follower_store.clone()),
        acknowledged_once: AtomicBool::new(false),
    });
    let gate =
        DurabilityGate::new(leader, NodeId::from_bytes(*leader.as_bytes()), 1, [member]).unwrap();
    let shipper =
        NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
    runtime
        .install_node_durability(
            fixture.target.application(),
            Arc::new(NodeDurability::new(
                gate,
                shipper,
                Arc::new(TestNodeAuthority::default()),
                Arc::clone(&transport),
                lease,
            )),
        )
        .unwrap();
    let handle = bootstrap_on(&runtime, &fixture, leader).await;
    let first = handle
        .execute(
            identity(126),
            Digest::from_bytes([126; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"published".to_vec()))
            },
        )
        .await
        .unwrap();
    assert_eq!(first.commit_sequence(), 1);
    let authority = CellAuthority::new(fixture.layout.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current = authority
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap();
            if current.value().root.as_ref().unwrap().commit_sequence == 1
                && runtime.stats().unpublished_node_log_bytes() == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let request_identity = identity(127);
    let operation_digest = Digest::from_bytes([127; 32]);
    store.fail_puts();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle.execute(
            request_identity,
            operation_digest,
            21,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"ambiguous".to_vec()))
            },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(crab_cell_runtime::Error::OutcomeUnknown {
            request_id,
            operation_digest: digest,
            ..
        }) if request_id == request_identity.request_id && digest == operation_digest
    ));
    tokio::time::timeout(std::time::Duration::from_secs(2), store.wait_until_failed())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while runtime.stats().active_cells() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(follower_store.retained_bytes() > 0);
    store.allow_puts();
    assert!(runtime.shutdown().await.is_err());

    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let stale = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stale.value().root.as_ref().unwrap().commit_sequence, 1);
    let fenced = fence_log_session(&fixture.layout, leader, successor, follower, 1).await;
    let recovery = crab_cell_runtime::NodeLogRecovery::from_fenced(
        Arc::clone(&transport),
        &fenced,
        Limits::default(),
    )
    .unwrap();
    let manifests =
        crab_cell_runtime::RecoveryManifestStore::new(fixture.layout.clone(), Limits::default());
    let coordinator = crab_cell_runtime::RecoveryCoordinator::new(recovery, manifests.clone());
    let inventory = crab_cell_runtime::recoverable_cells(&catalog, &authority, leader, 10)
        .await
        .unwrap();
    let directory = crab_cell_runtime::NodeDirectory::new(
        fixture.layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        Digest::from_bytes([92; 32]),
    );
    let completed = coordinator
        .recover_and_seal(&directory, fenced, inventory, 10_002)
        .await
        .unwrap();
    let attached = completed.controls.into_iter().next().unwrap();
    let successor_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = successor_runtime
        .takeover_restored(
            proof,
            fixture.replica.clone(),
            authority,
            attached,
            completed.takeover,
            manifests,
            fixture._directory.path().join("lost-ack-successor.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://lost-ack-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    let replayed = restored
        .execute(
            request_identity,
            operation_digest,
            30,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"executed-twice".to_vec()))
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        replayed,
        StoredOutcome::Success {
            ref result,
            commit_sequence: 2
        } if result == b"ambiguous"
    ));
    let value = restored
        .query(64, 64, |connection| {
            let value = connection
                .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
            Ok(value.to_be_bytes().to_vec())
        })
        .await
        .unwrap();
    assert_eq!(i64::from_be_bytes(value.try_into().unwrap()), 2);
    restored.drain().await.unwrap();
    successor_runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn runtime_stats_follow_active_cell_lifecycle() {
    let fixture = fixture_for(b"runtime-stats");
    let (runtime, handle, _pool) = activate_runtime(&fixture, 2 * 1024 * 1024).await;

    assert_eq!(runtime.stats().active_cells(), 1);
    assert_eq!(
        runtime.stats().resident_bytes(),
        crab_cell_runtime::ACTIVE_CELL_NATIVE_BYTES as usize
    );
    assert_eq!(
        runtime.stats().resident_capacity_bytes(),
        10 * crab_cell_runtime::ACTIVE_CELL_NATIVE_BYTES as usize
    );
    assert_eq!(
        runtime.stats().file_descriptors(),
        ACTIVE_CELL_FILE_DESCRIPTORS
    );
    assert_eq!(
        runtime.stats().file_descriptor_capacity(),
        10 * ACTIVE_CELL_FILE_DESCRIPTORS
    );
    handle.drain().await.unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    assert_eq!(runtime.stats().resident_bytes(), 0);
    assert_eq!(runtime.stats().file_descriptors(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn resident_lookup_is_invalidated_before_drain_releases_the_cell() {
    let fixture = fixture_for(b"resident-drain-race");
    let (runtime, handle, _pool) = activate_runtime(&fixture, 2 * 1024 * 1024).await;

    assert!(
        runtime
            .resident_handle(&fixture.target, CatalogRole::Repository)
            .await
            .unwrap()
            .is_some()
    );

    handle.drain().await.unwrap();

    assert!(
        runtime
            .resident_handle(&fixture.target, CatalogRole::Repository)
            .await
            .unwrap()
            .is_none()
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn churn_evicts_idle_cells_and_restores_exact_roots() {
    let first = fixture_for(b"churn-first");
    let second = fixture_for(b"churn-second");
    let third = fixture_for(b"churn-third");
    let session = SessionId::from_bytes([74; 16]);
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(1, 2).unwrap(),
        16 * 1024 * 1024,
        session,
        ReplicaHost::default().with_local_disk_budget(DiskBudget::new(64 * 1024 * 1024)),
    )
    .unwrap();

    let first_handle = bootstrap_on(&runtime, &first, session).await;
    let second_handle = bootstrap_on(&runtime, &second, session).await;

    let authority_first = CellAuthority::new(first.layout.clone());
    let authority_second = CellAuthority::new(second.layout.clone());
    let root_first = authority_first
        .load(first.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .ltx_root()
        .unwrap();
    let root_second = authority_second
        .load(second.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .ltx_root()
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if runtime.evict_idle(1).await.unwrap() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();

    let (evicted_fixture, evicted_authority, evicted_root, evicted_idle) =
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let first_control = authority_first
                    .load(first.target.cell_id())
                    .await
                    .unwrap()
                    .unwrap();
                if first_control.value().state == ControlState::Idle {
                    break (&first, &authority_first, root_first, first_control);
                }
                let second_control = authority_second
                    .load(second.target.cell_id())
                    .await
                    .unwrap()
                    .unwrap();
                if second_control.value().state == ControlState::Idle {
                    break (&second, &authority_second, root_second, second_control);
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
    assert_eq!(evicted_idle.value().ltx_root(), Some(evicted_root));
    assert_eq!(runtime.stats().active_cells(), 1);

    let catalog = crab_cell_runtime::CellCatalog::new(
        evicted_fixture.layout.clone(),
        evicted_fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(evicted_fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            evicted_fixture.replica.clone(),
            evicted_authority.clone(),
            evicted_idle,
            evicted_fixture
                ._directory
                .path()
                .join("churn-restored.sqlite"),
            Owner {
                session,
                endpoint: "https://churn-successor.internal:8081".into(),
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
    restored.drain().await.unwrap();

    let third_handle = bootstrap_on(&runtime, &third, session).await;
    assert_eq!(runtime.stats().active_cells(), 2);
    third_handle.drain().await.unwrap();
    if evicted_fixture.target.cell_id() == first.target.cell_id() {
        second_handle.drain().await.unwrap();
    } else {
        first_handle.drain().await.unwrap();
    }
    assert_eq!(runtime.stats().active_cells(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn persisted_work_blocks_idle_eviction_until_explicit_release() {
    let fixture = fixture_for(b"eviction-persisted-work");
    let session = SessionId::from_bytes([78; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    assert!(matches!(
        handle
            .execute(
                identity(79),
                Digest::from_bytes([79; 32]),
                20,
                64,
                64,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            )
            .await
            .unwrap(),
        StoredOutcome::Success {
            commit_sequence: 1,
            ..
        }
    ));

    assert_eq!(runtime.evict_idle(1).await.unwrap(), 0);
    assert_eq!(runtime.stats().active_cells(), 1);

    handle.drain().await.unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_primitive_inventory_blocks_churn_until_drain_and_restores_root() {
    let queue = fixture_for(b"mixed-queue");
    let workflow = fixture_for(b"mixed-workflow");
    let third = fixture_for(b"mixed-third");
    let session = SessionId::from_bytes([80; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 2).unwrap(), 16 * 1024 * 1024, session).unwrap();

    let queue_handle = bootstrap_role_on(
        &runtime,
        &queue,
        session,
        CatalogRole::Queue,
        |transaction| {
            install_queue_schema(transaction)?;
            transaction.execute(
                "INSERT INTO queue_messages VALUES (zeroblob(16), X'01', 0, 0, 1, 1000, NULL, NULL, NULL, NULL)",
                [],
            )?;
            Ok(())
        },
    )
    .await;
    let workflow_handle = bootstrap_role_on(
        &runtime,
        &workflow,
        session,
        CatalogRole::Workflow,
        |transaction| {
            install_workflow_schema(transaction)?;
            transaction.execute(
                "INSERT INTO workflow_runs VALUES (X'02', zeroblob(16), zeroblob(32), 0, X'', 0, NULL, NULL)",
                [],
            )?;
            Ok(())
        },
    )
    .await;

    wait_for_persisted_work(
        &queue_handle,
        CatalogRole::Queue,
        "maintenance release is blocked by retained Queue messages",
    )
    .await;
    wait_for_persisted_work(
        &workflow_handle,
        CatalogRole::Workflow,
        "maintenance release is blocked by retained Workflow runs",
    )
    .await;

    assert_eq!(runtime.evict_idle(2).await.unwrap(), 0);
    assert_eq!(runtime.stats().active_cells(), 2);

    let queue_authority = CellAuthority::new(queue.layout.clone());
    let queue_root = queue_authority
        .load(queue.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let queue_root = queue_root.value().ltx_root().unwrap();

    queue_handle.drain().await.unwrap();
    workflow_handle.drain().await.unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    let queue_idle = queue_authority
        .load(queue.target.cell_id())
        .await
        .unwrap()
        .unwrap();

    let queue_proof =
        crab_cell_runtime::CellCatalog::new(queue.layout.clone(), queue.target.tenant())
            .lookup(queue.target.cell_id())
            .await
            .unwrap()
            .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            queue_proof,
            queue.replica.clone(),
            queue_authority,
            queue_idle,
            queue._directory.path().join("mixed-queue-restored.sqlite"),
            Owner {
                session,
                endpoint: "https://mixed-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let count =
                    connection.query_row("SELECT count(*) FROM queue_messages", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                Ok(count.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        1_i64.to_be_bytes()
    );
    assert_eq!(
        crab_cell_runtime::CellAuthority::new(queue.layout.clone())
            .load(queue.target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .ltx_root(),
        Some(queue_root)
    );
    restored.drain().await.unwrap();

    let third_handle = bootstrap_on(&runtime, &third, session).await;
    assert_eq!(runtime.stats().active_cells(), 1);
    third_handle.drain().await.unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    runtime.shutdown().await.unwrap();
}

async fn wait_for_persisted_work(
    handle: &crab_cell_runtime::CellHandle,
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
) -> crab_cell_runtime::CellHandle {
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
) -> crab_cell_runtime::CellHandle
where
    F: for<'connection> FnOnce(
            &crab_ltx::rusqlite::Transaction<'connection>,
        ) -> crab_cell_runtime::Result<()>
        + Send
        + 'static,
{
    let catalog =
        crab_cell_runtime::CellCatalog::new(fixture.layout.clone(), fixture.target.tenant());
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
    assert_eq!(owned.value().state, ControlState::Serving);
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
    let fenced = fence_session(
        &fixture.layout,
        stale.value().owner.as_ref().unwrap().session,
        SessionId::from_bytes([41; 16]),
    )
    .await;
    let takeover = fenced.direct_takeover().unwrap();
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
            takeover,
            crab_cell_runtime::RecoveryManifestStore::new(
                fixture.layout.clone(),
                Limits::default(),
            ),
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
    assert_eq!(owned.value().state, ControlState::Serving);
    assert_eq!(owned.value().epoch, 2);
    assert_eq!(owned.value().owner.as_ref().unwrap().session, session);
    restored.drain().await.unwrap();
}

#[tokio::test]
async fn takeover_resumes_pinned_recovery_before_serving() {
    recover_retained_tail(false).await;
}

#[tokio::test]
async fn recovery_seals_already_rooted_tail_without_an_empty_manifest() {
    recover_retained_tail(true).await;
}

async fn recover_retained_tail(rooted: bool) {
    let fixture = fixture_for(b"recovered-takeover");
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
    let predecessor = stale.value().ltx_root().unwrap();
    let leader = stale.value().owner.as_ref().unwrap().session;

    let tail_directory = tempfile::TempDir::new().unwrap();
    let tail_path = tail_directory.path().join("tail.sqlite");
    let writable = fixture
        .replica
        .open_root(&predecessor)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&tail_path)
        .await
        .unwrap();
    let mut writer = writable.open_writable(&tail_path).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            transaction.execute(
                "UPDATE sys_meta SET commit_sequence = commit_sequence + 1, logical_time_ms = logical_time_ms + 1 WHERE singleton = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let capture = writer.capture().unwrap();
    let mut frames = Vec::with_capacity(capture.segments.len());
    for (index, segment) in capture.segments.iter().enumerate() {
        frames.push(
            crab_ltx::encode_node_frame(
                crab_ltx::NodeFrameScope {
                    leader_session: *leader.as_bytes(),
                    log_epoch: 1,
                    node_sequence: u64::try_from(index).unwrap() + 1,
                    application: *fixture.layout.application_id(),
                    cell: *fixture.target.cell_id().as_bytes(),
                    incarnation: *stale.value().incarnation.as_bytes(),
                    cell_epoch: stale.value().epoch,
                    commit_sequence: predecessor.commit_sequence + 1,
                },
                segment.info().clone(),
                Bytes::from(std::fs::read(segment.path()).unwrap()),
                Limits::default(),
            )
            .unwrap()
            .encoded()
            .clone(),
        );
    }
    if rooted {
        // Crash after the exact Cell root CAS but before shared node coverage.
        let prepared = fixture
            .replica
            .prepare(
                Some(&predecessor),
                &capture,
                predecessor.commit_sequence + 1,
                stale.value().schema,
            )
            .await
            .unwrap();
        authority
            .transition(
                &stale,
                stale.value().publish_prepared(&prepared, None).unwrap(),
                Transition::Publish,
            )
            .await
            .unwrap();
    }
    writer.close().unwrap();
    let follower = SessionId::from_bytes([43; 16]);
    let follower_directory = tempfile::TempDir::new().unwrap();
    let follower_store = crab_cell_runtime::FollowerStore::open(
        follower_directory.path().to_owned(),
        Limits::default(),
        crab_cell_runtime::DiskBudget::new(1 << 30),
    )
    .unwrap();
    let transport: Arc<dyn crab_cell_runtime::NodeLogTransport> =
        Arc::new(crab_cell_runtime::LocalFollowerTransport::new(
            crab_cell_runtime::NodeId::from_bytes(*follower.as_bytes()),
            follower_store,
        ));
    transport
        .append(
            crab_cell_runtime::NodeId::from_bytes(*follower.as_bytes()),
            crab_cell_runtime::AppendRequest {
                leader_session: leader,
                log_epoch: 1,
                frames,
                covered_through: 0,
            },
        )
        .await
        .unwrap();
    let manifests =
        crab_cell_runtime::RecoveryManifestStore::new(fixture.layout.clone(), Limits::default());
    let successor = SessionId::from_bytes([42; 16]);
    let fenced = fence_log_session(&fixture.layout, leader, successor, follower, 0).await;
    assert!(matches!(
        fenced.direct_takeover(),
        Err(crab_cell_runtime::Error::PendingPublication)
    ));
    let recovery = crab_cell_runtime::NodeLogRecovery::from_fenced(
        Arc::clone(&transport),
        &fenced,
        Limits::default(),
    )
    .unwrap();
    let coordinator = crab_cell_runtime::RecoveryCoordinator::new(recovery, manifests.clone());
    let inventory = crab_cell_runtime::recoverable_cells(&catalog, &authority, leader, 10)
        .await
        .unwrap();
    assert_eq!(inventory.len(), 1);
    if rooted {
        let directory = crab_cell_runtime::NodeDirectory::new(
            fixture.layout.clone(),
            Digest::from_bytes([90; 32]),
            Digest::from_bytes([91; 32]),
            Digest::from_bytes([92; 32]),
        );
        let completed = coordinator
            .recover_and_seal(&directory, fenced, inventory, 10_002)
            .await
            .unwrap();
        assert!(completed.controls.is_empty());
        assert_eq!(
            completed.sealed.log().phase(),
            crab_cell_runtime::NodeLogPhase::Sealed
        );
        return;
    }
    let attached = coordinator
        .recover(fenced.clone(), inventory)
        .await
        .unwrap();
    assert_eq!(attached.len(), 1);
    drop(coordinator);
    let resumed_recovery =
        crab_cell_runtime::NodeLogRecovery::from_fenced(transport, &fenced, Limits::default())
            .unwrap();
    let resumed = crab_cell_runtime::RecoveryCoordinator::new(resumed_recovery, manifests.clone());
    let directory = crab_cell_runtime::NodeDirectory::new(
        fixture.layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        Digest::from_bytes([92; 32]),
    );
    let completed = resumed
        .recover_and_seal(
            &directory,
            fenced.clone(),
            vec![crab_cell_runtime::RecoveryCell {
                application: fixture.target.application(),
                authority: authority.clone(),
                observed: attached[0].clone(),
            }],
            10_002,
        )
        .await
        .unwrap();
    assert_eq!(completed.controls[0].value(), attached[0].value());
    assert_eq!(
        completed.sealed.log().phase(),
        crab_cell_runtime::NodeLogPhase::Sealed
    );
    let repeated = resumed
        .recover_and_seal(
            &directory,
            fenced.clone(),
            vec![crab_cell_runtime::RecoveryCell {
                application: fixture.target.application(),
                authority: authority.clone(),
                observed: completed.controls[0].clone(),
            }],
            10_003,
        )
        .await
        .unwrap();
    assert_eq!(repeated.sealed, completed.sealed);
    let attached = repeated.controls.into_iter().next().unwrap();
    let takeover = repeated.takeover;
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            attached,
            takeover,
            manifests,
            fixture._directory.path().join("recovered-takeover.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://recovered-successor.internal:8081".into(),
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
        1_i64.to_be_bytes()
    );
    let serving = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serving.value().state, ControlState::Serving);
    assert!(serving.value().recovery.is_none());
    assert_eq!(
        serving.value().root.as_ref().unwrap().commit_sequence,
        predecessor.commit_sequence + 1
    );
    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
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
    let fenced = fence_session(
        &fixture.layout,
        stale.value().owner.as_ref().unwrap().session,
        SessionId::from_bytes([42; 16]),
    )
    .await;
    let takeover = fenced.direct_takeover().unwrap();
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
            takeover,
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
    source_loss_takeover(
        Store::new(Arc::new(InMemory::new())),
        Path::from("cold-runtime"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_source_loss_takeover_restores_exact_root_and_continues_publication() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let store = build_explicit_store(
        &required("CRAB_CELL_TEST_BUCKET"),
        ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_CELL_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    let prefix = Path::from(format!(
        "{}/cold-runtime",
        required("CRAB_CELL_TEST_PREFIX")
    ));
    source_loss_takeover(store, prefix).await;
}

async fn source_loss_takeover(store: Store, prefix: Path) {
    let target = CellTarget::new(
        TenantId::from_bytes([41; 16]),
        ApplicationId::from_bytes([42; 16]),
        NamespaceId::from_bytes([43; 16]),
        b"repository-cold-start",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([44; 16]);
    let layout = CellStorageLayout::new(store, prefix, [42; 16]);
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
            crab_cell_runtime::RecoveryManifestStore::new(layout.clone(), Limits::default()),
            second_local.path().join("cell.sqlite"),
        )
        .await
        .unwrap();
    assert_eq!(
        authority.load(cell).await.unwrap().unwrap().value().state,
        ControlState::Serving
    );
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
