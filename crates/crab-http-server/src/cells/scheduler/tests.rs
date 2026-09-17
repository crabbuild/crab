use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use bytes::Bytes;
use crab_cell_runtime::{
    ActivityContext, ActivityExecution, ApplicationId, BlockingActivityHandler, BuildDescriptor,
    CellAuthority, CellCatalog, CellModule, CellReplica, CellRuntime, CellTarget, Digest,
    IncarnationId, MaintenanceModule, MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor,
    NamespaceId, NodeAdvertisement, NodeCapacity, OperationDescriptor, Owner, PeerRoundTrip,
    PeerSigner, RecoveryManifestStore, RegistryBuilder, ReplicaLimits, RetainedCodeDescriptor,
    SqlWorkerPool, TenantId, WorkflowAction, WorkflowActivityModule, WorkflowContext,
    WorkflowDecision, WorkflowDefinition, WorkflowModule, WorkflowNamespace, WorkflowStatus,
    install_workflow_schema, register_blocking_activity, register_maintenance, register_workflow,
    register_workflow_activities,
};
use crab_storage::{CellStorageLayout, Store};
use ed25519_dalek::SigningKey;
use object_store::{memory::InMemory, path::Path};

use super::*;
use crate::cells::{
    REPOSITORY_MIGRATION, REPOSITORY_NAMESPACE, bootstrap_release_at, provision_repository,
};

const WORKFLOW_MODULE: &str = "scheduler-workflow-test";
const WORKFLOW_NAMESPACE: NamespaceId = NamespaceId::from_bytes([31; 16]);
const WORKFLOW_MIGRATION: &str =
    include_str!("../../../../crab-cell-runtime/src/migrations/workflow.sql");
const WORKFLOW_COMMANDS: &[OperationDescriptor] = &[
    operation(1, 1 << 20, 64),
    operation(2, 1 << 20, 64),
    operation(3, 1 << 20, 64),
    operation(4, 1 << 20, 1 << 20),
    operation(5, 1 << 20, 1 << 20),
    operation(6, 1 << 20, 64),
    operation(7, 8, 5),
];
const WORKFLOW_QUERIES: &[OperationDescriptor] =
    &[operation(1, 2048, 1 << 20), operation(2, 1 << 20, 1)];
const WORKFLOW_DEFINITION_DIGEST: Digest = Digest::from_bytes([32; 32]);
const WORKFLOW_PREDECESSOR: Digest = Digest::from_bytes([34; 32]);
static WORKFLOW_ACTIVITY_RUNS: AtomicUsize = AtomicUsize::new(0);
static WORKFLOW_ACTIVITY_RELEASE: AtomicBool = AtomicBool::new(false);
static WORKFLOW_DEFINITION: SchedulerWorkflowDefinition = SchedulerWorkflowDefinition;
static WORKFLOW_DEFINITIONS: [&dyn WorkflowDefinition; 1] = [&WORKFLOW_DEFINITION];
static WORKFLOW_NAMESPACES: [NamespaceDescriptor; 1] = [NamespaceDescriptor {
    id: WORKFLOW_NAMESPACE,
    name: WORKFLOW_MODULE,
    role: crab_cell_runtime::CatalogRole::Workflow,
    shards: 1,
    effect_targets: &[],
    dead_letter: None,
}];

const fn operation(id: u32, input_limit: u32, output_limit: u32) -> OperationDescriptor {
    OperationDescriptor {
        id,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit,
        output_limit,
    }
}

#[tokio::test]
async fn expired_active_node_log_is_recovered_and_sealed_automatically() {
    let application = ApplicationId::from_bytes([61; 16]);
    let tenant = TenantId::from_bytes([62; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("scheduler-recovery"),
        *application.as_bytes(),
    );
    let fleet = Digest::from_bytes([63; 32]);
    let image = Digest::from_bytes([64; 32]);
    let release = Digest::from_bytes([65; 32]);
    let certificate = Digest::from_bytes([66; 32]);
    let key = SigningKey::from_bytes(&[67; 32]);
    let leader = crab_cell_runtime::SessionId::from_bytes([68; 16]);
    let member = crab_cell_runtime::SessionId::from_bytes([69; 16]);
    let claimant = crab_cell_runtime::SessionId::from_bytes([70; 16]);
    let now_ms = super::super::unix_now_ms().unwrap();
    let initial_ms = now_ms - 20_000;
    let capacity = NodeCapacity {
        free_memory_bytes: 1 << 30,
        free_disk_bytes: 1 << 30,
        follower_free_bytes: 1 << 30,
        job_credits: 1,
        log_protocol: crab_cell_runtime::NODE_LOG_PROTOCOL_VERSION,
    };
    let advertisement = |session: crab_cell_runtime::SessionId,
                         endpoint: &str,
                         progress,
                         issued_at_ms,
                         expires_at_ms| {
        NodeAdvertisement::sign(
            crab_cell_runtime::NodeId::from_bytes(*session.as_bytes()),
            session,
            endpoint.into(),
            fleet,
            certificate,
            image,
            release,
            &key,
            progress,
            issued_at_ms,
            expires_at_ms,
            vec![Digest::from_bytes([71; 32])],
            vec![1],
            capacity,
        )
        .unwrap()
    };
    let directory = NodeDirectory::new(layout.clone(), fleet, image, release);
    let leader_record = directory
        .create(
            advertisement(
                leader,
                "https://expired.internal:8081",
                1,
                initial_ms,
                initial_ms + 15_000,
            ),
            initial_ms,
        )
        .await
        .unwrap();
    let member_record = directory
        .create(
            advertisement(
                member,
                "https://follower.internal:8081",
                1,
                initial_ms,
                initial_ms + 15_000,
            ),
            initial_ms,
        )
        .await
        .unwrap();
    let enrolled = directory
        .recruit_log(&leader_record, 1, 1, 2, initial_ms + 1)
        .await
        .unwrap();
    let active = directory
        .activate_log(&enrolled, initial_ms + 2)
        .await
        .unwrap();
    directory
        .advance_log_coverage(&active, 1, initial_ms + 3)
        .await
        .unwrap();
    directory
        .refresh(
            &member_record,
            advertisement(
                member,
                "https://follower.internal:8081",
                2,
                now_ms,
                now_ms + 15_000,
            ),
            now_ms,
        )
        .await
        .unwrap();
    directory
        .create(
            advertisement(
                claimant,
                "https://claimant.internal:8081",
                1,
                now_ms,
                now_ms + 15_000,
            ),
            now_ms,
        )
        .await
        .unwrap();

    let follower_directory = tempfile::TempDir::new().unwrap();
    let follower = crab_cell_runtime::FollowerStore::open(
        follower_directory.path().to_owned(),
        super::super::repository_replica_limits(),
        crab_cell_runtime::DiskBudget::new(1 << 30),
    )
    .unwrap();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = crab_ltx::ManagedDb::open(
        &source.path().join("follower.sqlite"),
        super::super::repository_replica_limits(),
    )
    .unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let capture = database.capture().unwrap();
    let segment = capture.segments.first().unwrap();
    let frame = crab_ltx::encode_node_frame(
        crab_ltx::NodeFrameScope {
            leader_session: *leader.as_bytes(),
            log_epoch: 1,
            node_sequence: 1,
            application: *application.as_bytes(),
            cell: [72; 32],
            incarnation: [73; 16],
            cell_epoch: 1,
            commit_sequence: 1,
        },
        segment.info().clone(),
        Bytes::from(std::fs::read(segment.path()).unwrap()),
        super::super::repository_replica_limits(),
    )
    .unwrap()
    .encoded()
    .clone();
    follower.append(leader, 1, vec![frame], 0).await.unwrap();
    database.close().unwrap();
    let transport: Arc<dyn crab_cell_runtime::NodeLogTransport> =
        Arc::new(crab_cell_runtime::LocalFollowerTransport::new(
            crab_cell_runtime::NodeId::from_bytes(*member.as_bytes()),
            follower,
        ));
    recover_node_session(
        directory.clone(),
        CellCatalog::new(layout.clone(), tenant),
        CellAuthority::new(layout.clone()),
        RecoveryManifestStore::new(layout, super::super::repository_replica_limits()),
        transport,
        leader,
        claimant,
    )
    .await
    .unwrap();

    assert!(
        directory
            .recovery_candidates(claimant, super::super::unix_now_ms().unwrap(), 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        directory
            .takeover_proof(leader, claimant, super::super::unix_now_ms().unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

struct SchedulerWorkflowDefinition;

impl WorkflowDefinition for SchedulerWorkflowDefinition {
    fn digest(&self) -> Digest {
        WORKFLOW_DEFINITION_DIGEST
    }

    fn transition(
        &self,
        _state: &[u8],
        event: &[u8],
        context: WorkflowContext,
    ) -> crab_cell_runtime::Result<WorkflowDecision> {
        if event.starts_with(b"activity\0") {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Completed,
                state: b"completed-by-scheduler".to_vec(),
                result: Some(event.to_vec()),
                actions: Vec::new(),
            });
        }
        Ok(WorkflowDecision {
            status: WorkflowStatus::Running,
            state: b"waiting".to_vec(),
            result: None,
            actions: vec![WorkflowAction::Activity {
                activity_type: "scheduler-echo".into(),
                input: event.to_vec(),
                due_at_ms: context.now_ms(),
                expires_at_ms: context.now_ms() + 60_000,
            }],
        })
    }
}

struct SchedulerWorkflow;

impl WorkflowModule for SchedulerWorkflow {
    const MODULE: &'static str = WORKFLOW_MODULE;
    const NAMESPACE: NamespaceId = WORKFLOW_NAMESPACE;
    const CURRENT_DEFINITION: &'static dyn WorkflowDefinition = &WORKFLOW_DEFINITION;
    const DEFINITIONS: &'static [&'static dyn WorkflowDefinition] = &WORKFLOW_DEFINITIONS;
    const START_COMMAND_ID: u32 = 1;
    const SIGNAL_COMMAND_ID: u32 = 2;
    const CANCEL_COMMAND_ID: u32 = 3;
    const GET_QUERY_ID: u32 = 1;
}

impl WorkflowActivityModule for SchedulerWorkflow {
    const ACTIVITY_TYPES: &'static [&'static str] = &["scheduler-echo"];
    const ACTIVITY_CLAIM_COMMAND_ID: u32 = 4;
    const ACTIVITY_COMPLETE_COMMAND_ID: u32 = 5;
    const ACTIVITY_EXTEND_COMMAND_ID: u32 = 6;
    const ACTIVITY_VALIDATE_QUERY_ID: u32 = 2;
}

impl MaintenanceModule for SchedulerWorkflow {
    const MODULE: &'static str = WORKFLOW_MODULE;
    const TICK_COMMAND_ID: u32 = 7;
    const WORKFLOW_DEFINITIONS: &'static [&'static dyn WorkflowDefinition] = &WORKFLOW_DEFINITIONS;
}

struct SchedulerEcho;

impl BlockingActivityHandler for SchedulerEcho {
    const TYPE: &'static str = "scheduler-echo";

    fn execute(_context: ActivityContext, input: Vec<u8>) -> ActivityExecution {
        WORKFLOW_ACTIVITY_RUNS.fetch_add(1, Ordering::AcqRel);
        while !WORKFLOW_ACTIVITY_RELEASE.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        ActivityExecution::Completed(input)
    }
}

impl CellModule for SchedulerWorkflow {
    const NAME: &'static str = WORKFLOW_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<ModuleDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: WORKFLOW_MODULE,
            source_digest: Digest::from_bytes([33; 32]),
            retained_codes: &[RetainedCodeDescriptor {
                code: WORKFLOW_PREDECESSOR,
                schema_min: 1,
                schema_max: 1,
            }],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: WORKFLOW_MIGRATION,
                digest: Digest::from_bytes(*blake3::hash(WORKFLOW_MIGRATION.as_bytes()).as_bytes()),
            }])),
            commands: WORKFLOW_COMMANDS,
            queries: WORKFLOW_QUERIES,
            workflow_definitions: &[WORKFLOW_DEFINITION_DIGEST],
            activity_types: &[SchedulerEcho::TYPE],
            namespaces: &WORKFLOW_NAMESPACES,
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_workflow::<Self>(registry)?;
        register_workflow_activities::<Self>(registry)?;
        register_blocking_activity::<Self, SchedulerEcho>(registry)?;
        register_maintenance::<Self>(registry)
    }
}

struct UnavailablePeer;

impl PeerRoundTrip for UnavailablePeer {
    fn send(
        &self,
        _target: CellTarget,
        _request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>> {
        Box::pin(async { Err(crab_cell_runtime::Error::CellNotActive) })
    }
}

struct CountingUnavailablePeer(Arc<AtomicUsize>);

impl PeerRoundTrip for CountingUnavailablePeer {
    fn send(
        &self,
        _target: CellTarget,
        _request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Err(crab_cell_runtime::Error::CellNotActive) })
    }
}

async fn bootstrap_due_repository(
    identity: ApplicationIdentity,
    layout: &CellStorageLayout,
    registry: &Arc<Registry>,
    runtime: &CellRuntime,
    owner: Owner,
    directory: &std::path::Path,
    repository: Uuid,
    inbox: u8,
) -> (CellTarget, CellAuthority) {
    let target = CellTarget::new(
        identity.tenant(),
        identity.application(),
        REPOSITORY_NAMESPACE,
        repository.as_bytes(),
    )
    .unwrap();
    let (proof, authority) = provision_repository(layout, identity, registry, &target)
        .await
        .unwrap();
    let observed = authority
        .create_initial(&proof, IncarnationId::from_bytes([inbox; 16]), owner)
        .await
        .unwrap();
    let replica = CellReplica::new(
        layout.clone(),
        *target.cell_id().as_bytes(),
        *observed.value().incarnation.as_bytes(),
        ReplicaLimits::default(),
    )
    .unwrap();
    let repository_bytes = repository.into_bytes();
    let handle = runtime
        .bootstrap(
            proof,
            replica,
            authority.clone(),
            observed,
            directory.join(format!("{repository}.sqlite")),
            move |transaction| {
                transaction.execute_batch(REPOSITORY_MIGRATION)?;
                transaction.execute(
                    "INSERT INTO repository_identity(singleton, repository_uuid) VALUES (1, ?1)",
                    [repository_bytes.as_slice()],
                )?;
                transaction.execute(
                    "INSERT INTO sys_inbox VALUES (?1, ?2, 1, X'', 0, 1, 1)",
                    (
                        [inbox; 32].as_slice(),
                        [inbox.wrapping_add(1); 32].as_slice(),
                    ),
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();
    (target, authority)
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_executes_registered_workflow_activity_without_blocking_the_scanner() {
    WORKFLOW_ACTIVITY_RUNS.store(0, Ordering::Release);
    WORKFLOW_ACTIVITY_RELEASE.store(false, Ordering::Release);
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([41; 16]),
        ApplicationId::from_bytes([42; 16]),
    );
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "scheduler-workflow-test".into(),
        cargo_lock_digest: Digest::from_bytes([43; 32]),
    });
    builder.register(SchedulerWorkflow).unwrap();
    let registry = Arc::new(builder.finish().unwrap());
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("workflow-scheduler"),
        *identity.application().as_bytes(),
    );
    bootstrap_release_at(
        &layout,
        identity,
        &registry,
        &format!("sha256:{}", "2b".repeat(32)),
    )
    .await
    .unwrap();
    let target = CellTarget::new(
        identity.tenant(),
        identity.application(),
        WORKFLOW_NAMESPACE,
        &0_u32.to_be_bytes(),
    )
    .unwrap();
    let catalog = crab_cell_runtime::CellCatalog::new(layout.clone(), identity.tenant());
    let proof = catalog
        .provision(
            crab_cell_runtime::CatalogEntry::new(
                &target,
                crab_cell_runtime::CatalogRole::Workflow,
                registry.module_code(WORKFLOW_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let session = SessionId::from_bytes([44; 16]);
    let endpoint = "https://workflow-scheduler.internal:8789".to_owned();
    let owner = Owner {
        session,
        endpoint: endpoint.clone(),
    };
    let control = authority
        .create_initial(&proof, IncarnationId::from_bytes([45; 16]), owner.clone())
        .await
        .unwrap();
    let local = tempfile::tempdir().unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 4).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            CellReplica::new(
                layout.clone(),
                *target.cell_id().as_bytes(),
                *control.value().incarnation.as_bytes(),
                ReplicaLimits::default(),
            )
            .unwrap(),
            authority,
            control,
            local.path().join("workflow.sqlite"),
            install_workflow_schema,
        )
        .await
        .unwrap();
    let client = crab_cell_runtime::CellClient::local(registry.clone(), handle.clone());
    let workflows = WorkflowNamespace::<SchedulerWorkflow>::new(
        client,
        identity.tenant(),
        identity.application(),
    )
    .unwrap();
    workflows
        .start(
            mutation_identity().unwrap(),
            b"scheduled-workflow".to_vec(),
            b"payload".to_vec(),
        )
        .await
        .unwrap();
    workflows
        .start(
            mutation_identity().unwrap(),
            b"second-workflow".to_vec(),
            b"payload".to_vec(),
        )
        .await
        .unwrap();

    let fleet = Digest::from_bytes([46; 32]);
    let image = Digest::from_bytes([47; 32]);
    let certificate = Digest::from_bytes([48; 32]);
    let node_directory =
        NodeDirectory::new(layout.clone(), fleet, image, registry.release_digest());
    let key = SigningKey::from_bytes(&[49; 32]);
    let now_ms = super::super::unix_now_ms().unwrap();
    node_directory
        .create(
            NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*session.as_bytes()),
                session,
                endpoint,
                fleet,
                certificate,
                image,
                registry.release_digest(),
                &key,
                1,
                now_ms,
                now_ms + 15_000,
                registry.module_digests(),
                vec![1],
                NodeCapacity {
                    free_memory_bytes: 1024 * 1024 * 1024,
                    free_disk_bytes: 1024 * 1024 * 1024,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            now_ms,
        )
        .await
        .unwrap();
    let router = RepositoryCellRouter::new(
        identity,
        layout.clone(),
        registry.clone(),
        runtime.clone(),
        super::super::RepositoryCellPeer::new(
            node_directory.clone(),
            Arc::new(PeerSigner::new(session, registry.release_digest(), key)),
            Arc::new(UnavailablePeer),
            owner,
        ),
        local.path().join("session"),
    )
    .unwrap();
    let status = SchedulerStatus::new(now_ms).unwrap();
    let mut scheduler =
        RepositoryCellScheduler::new(identity, layout, node_directory, router, session, status)
            .unwrap();
    let blocking_pool = scheduler.blocking_activities.as_ref().unwrap().clone();
    let mut blocking_reservations = Vec::new();
    while let Some(reservation) = blocking_pool.try_reserve().unwrap() {
        blocking_reservations.push(reservation);
    }
    scheduler.scan_once().await.unwrap();
    assert_eq!(WORKFLOW_ACTIVITY_RUNS.load(Ordering::Acquire), 0);
    assert!(scheduler.activity_jobs.is_empty());
    drop(blocking_reservations);
    scheduler.scan_once().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while WORKFLOW_ACTIVITY_RUNS.load(Ordering::Acquire) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    scheduler.scan_once().await.unwrap();
    assert_eq!(WORKFLOW_ACTIVITY_RUNS.load(Ordering::Acquire), 1);
    assert_eq!(scheduler.activity_jobs.len(), 1);
    WORKFLOW_ACTIVITY_RELEASE.store(true, Ordering::Release);
    scheduler.activity_jobs.join_next().await.unwrap().unwrap();
    scheduler.scan_once().await.unwrap();
    scheduler.activity_jobs.join_next().await.unwrap().unwrap();

    assert_eq!(WORKFLOW_ACTIVITY_RUNS.load(Ordering::Acquire), 2);
    let state = workflows
        .state(b"scheduled-workflow".to_vec(), None)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(state.status, WorkflowStatus::Completed);
    assert_eq!(state.state, b"completed-by-scheduler");
    let second = workflows
        .state(b"second-workflow".to_vec(), None)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(second.status, WorkflowStatus::Completed);
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn scan_cursor_advances_when_the_cycle_budget_is_exhausted() {
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([51; 16]),
        ApplicationId::from_bytes([52; 16]),
    );
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("repository-scheduler-fairness"),
        *identity.application().as_bytes(),
    );
    let registry = Arc::new(crate::cells::compiled_registry().unwrap());
    bootstrap_release_at(
        &layout,
        identity,
        &registry,
        &format!("sha256:{}", "35".repeat(32)),
    )
    .await
    .unwrap();
    let first_repository = Uuid::from_bytes([53; 16]);
    let first_target = CellTarget::new(
        identity.tenant(),
        identity.application(),
        REPOSITORY_NAMESPACE,
        first_repository.as_bytes(),
    )
    .unwrap();
    let shard = first_target.cell_id().as_bytes()[0];
    let second_repository = (1_u128..)
        .map(Uuid::from_u128)
        .find(|repository| {
            *repository != first_repository
                && CellTarget::new(
                    identity.tenant(),
                    identity.application(),
                    REPOSITORY_NAMESPACE,
                    repository.as_bytes(),
                )
                .is_ok_and(|target| target.cell_id().as_bytes()[0] == shard)
        })
        .unwrap();
    let session = SessionId::from_bytes([54; 16]);
    let endpoint = "https://scheduler-fairness.internal:8789".to_owned();
    let owner = Owner {
        session,
        endpoint: endpoint.clone(),
    };
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 4).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (first, authority) = bootstrap_due_repository(
        identity,
        &layout,
        &registry,
        &runtime,
        owner.clone(),
        directory.path(),
        first_repository,
        55,
    )
    .await;
    let (second, _) = bootstrap_due_repository(
        identity,
        &layout,
        &registry,
        &runtime,
        owner.clone(),
        directory.path(),
        second_repository,
        57,
    )
    .await;

    let fleet = Digest::from_bytes([58; 32]);
    let image = Digest::from_bytes([59; 32]);
    let certificate = Digest::from_bytes([60; 32]);
    let node_directory =
        NodeDirectory::new(layout.clone(), fleet, image, registry.release_digest());
    let key = SigningKey::from_bytes(&[61; 32]);
    let now_ms = super::super::unix_now_ms().unwrap();
    node_directory
        .create(
            NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*session.as_bytes()),
                session,
                endpoint,
                fleet,
                certificate,
                image,
                registry.release_digest(),
                &key,
                1,
                now_ms,
                now_ms + 15_000,
                registry.module_digests(),
                vec![1],
                NodeCapacity {
                    free_memory_bytes: 1024 * 1024 * 1024,
                    free_disk_bytes: 1024 * 1024 * 1024,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            now_ms,
        )
        .await
        .unwrap();
    let router = RepositoryCellRouter::new(
        identity,
        layout.clone(),
        Arc::clone(&registry),
        runtime.clone(),
        super::super::RepositoryCellPeer::new(
            node_directory.clone(),
            Arc::new(PeerSigner::new(session, registry.release_digest(), key)),
            Arc::new(UnavailablePeer),
            owner,
        ),
        directory.path().join("session"),
    )
    .unwrap();
    let status = SchedulerStatus::new(now_ms).unwrap();
    let mut scheduler =
        RepositoryCellScheduler::new(identity, layout, node_directory, router, session, status)
            .unwrap();

    scheduler.scan_once_bounded(1).await.unwrap();
    let mut first_cycle = vec![
        authority
            .load(first.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .root
            .as_ref()
            .unwrap()
            .commit_sequence,
        authority
            .load(second.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .root
            .as_ref()
            .unwrap()
            .commit_sequence,
    ];
    first_cycle.sort_unstable();
    assert_eq!(first_cycle, vec![0, 1]);

    scheduler.scan_once_bounded(1).await.unwrap();
    let mut second_cycle = Vec::new();
    for target in [first, second] {
        second_cycle.push(
            authority
                .load(target.cell_id())
                .await
                .unwrap()
                .unwrap()
                .value()
                .root
                .as_ref()
                .unwrap()
                .commit_sequence,
        );
    }
    assert_eq!(second_cycle, vec![1, 1]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_remote_schedule_keeps_durable_due_state_for_the_next_cycle() {
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([91; 16]),
        ApplicationId::from_bytes([92; 16]),
    );
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("repository-scheduler-retry"),
        *identity.application().as_bytes(),
    );
    let registry = Arc::new(crate::cells::compiled_registry().unwrap());
    bootstrap_release_at(
        &layout,
        identity,
        &registry,
        &format!("sha256:{}", "5d".repeat(32)),
    )
    .await
    .unwrap();

    let remote_session = SessionId::from_bytes([93; 16]);
    let remote_endpoint = "https://scheduler-remote.internal:8789".to_owned();
    let remote_owner = Owner {
        session: remote_session,
        endpoint: remote_endpoint.clone(),
    };
    let remote_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 4).unwrap(),
        16 * 1024 * 1024,
        remote_session,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let repository = Uuid::from_bytes([94; 16]);
    let (target, authority) = bootstrap_due_repository(
        identity,
        &layout,
        &registry,
        &remote_runtime,
        remote_owner.clone(),
        directory.path(),
        repository,
        95,
    )
    .await;
    let catalog = crab_cell_runtime::CellCatalog::new(layout.clone(), identity.tenant());
    let proof = catalog.lookup(target.cell_id()).await.unwrap().unwrap();
    let idle = authority.load(target.cell_id()).await.unwrap().unwrap();
    let replica = CellReplica::new(
        layout.clone(),
        *target.cell_id().as_bytes(),
        *idle.value().incarnation.as_bytes(),
        ReplicaLimits::default(),
    )
    .unwrap();
    let remote = remote_runtime
        .acquire_idle_restored(
            proof,
            replica,
            authority.clone(),
            idle,
            directory.path().join("remote-active.sqlite"),
            remote_owner,
        )
        .await
        .unwrap();
    let before = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(before.value().next_due_ms, Some(1));
    assert_eq!(before.value().root.as_ref().unwrap().commit_sequence, 0);

    let local_session = SessionId::from_bytes([96; 16]);
    let local_endpoint = "https://scheduler-local.internal:8789".to_owned();
    let fleet = Digest::from_bytes([97; 32]);
    let image = Digest::from_bytes([98; 32]);
    let certificate = Digest::from_bytes([99; 32]);
    let node_directory =
        NodeDirectory::new(layout.clone(), fleet, image, registry.release_digest());
    let now_ms = super::super::unix_now_ms().unwrap();
    let remote_key = SigningKey::from_bytes(&[100; 32]);
    node_directory
        .create(
            NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*remote_session.as_bytes()),
                remote_session,
                remote_endpoint,
                fleet,
                certificate,
                image,
                registry.release_digest(),
                &remote_key,
                1,
                now_ms,
                now_ms + 15_000,
                registry.module_digests(),
                vec![1],
                NodeCapacity {
                    free_memory_bytes: 0,
                    free_disk_bytes: 0,
                    job_credits: 0,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            now_ms,
        )
        .await
        .unwrap();
    let local_key = SigningKey::from_bytes(&[101; 32]);
    node_directory
        .create(
            NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*local_session.as_bytes()),
                local_session,
                local_endpoint.clone(),
                fleet,
                certificate,
                image,
                registry.release_digest(),
                &local_key,
                1,
                now_ms,
                now_ms + 15_000,
                registry.module_digests(),
                vec![1],
                NodeCapacity {
                    free_memory_bytes: 1024 * 1024 * 1024,
                    free_disk_bytes: 1024 * 1024 * 1024,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            now_ms,
        )
        .await
        .unwrap();

    let attempts = Arc::new(AtomicUsize::new(0));
    let local_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 4).unwrap(),
        16 * 1024 * 1024,
        local_session,
    )
    .unwrap();
    let router = RepositoryCellRouter::new(
        identity,
        layout.clone(),
        Arc::clone(&registry),
        local_runtime.clone(),
        super::super::RepositoryCellPeer::new(
            node_directory.clone(),
            Arc::new(PeerSigner::new(
                local_session,
                registry.release_digest(),
                local_key,
            )),
            Arc::new(CountingUnavailablePeer(Arc::clone(&attempts))),
            Owner {
                session: local_session,
                endpoint: local_endpoint,
            },
        ),
        directory.path().join("local-session"),
    )
    .unwrap();
    let status = SchedulerStatus::new(now_ms).unwrap();
    let mut scheduler = RepositoryCellScheduler::new(
        identity,
        layout,
        node_directory,
        router,
        local_session,
        status,
    )
    .unwrap();

    scheduler.scan_once().await.unwrap();
    scheduler.scan_once().await.unwrap();
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    let after = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(after.value().root, before.value().root);
    assert_eq!(after.value().next_due_ms, Some(1));
    assert_eq!(after.value().owner, before.value().owner);

    remote.drain().await.unwrap();
    remote_runtime.shutdown().await.unwrap();
    local_runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn scan_routes_due_cell_publishes_progress_and_collects_stale_node() {
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
    );
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("repository-scheduler"),
        *identity.application().as_bytes(),
    );
    let registry = Arc::new(crate::cells::compiled_registry().unwrap());
    let image = Digest::from_bytes([12; 32]);
    bootstrap_release_at(
        &layout,
        identity,
        &registry,
        &format!("sha256:{}", "0c".repeat(32)),
    )
    .await
    .unwrap();
    let repository = Uuid::from_bytes([3; 16]);
    let session = SessionId::from_bytes([4; 16]);
    let endpoint = "https://localhost:8789".to_owned();
    let owner = Owner {
        session,
        endpoint: endpoint.clone(),
    };
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 4).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (target, authority) = bootstrap_due_repository(
        identity,
        &layout,
        &registry,
        &runtime,
        owner.clone(),
        directory.path(),
        repository,
        5,
    )
    .await;
    let before = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(before.value().root.as_ref().unwrap().commit_sequence, 0);
    assert_eq!(before.value().next_due_ms, Some(1));

    let fleet = Digest::from_bytes([10; 32]);
    let certificate = Digest::from_bytes([11; 32]);
    let node_directory =
        NodeDirectory::new(layout.clone(), fleet, image, registry.release_digest());
    let key = SigningKey::from_bytes(&[6; 32]);
    let now_ms = super::super::unix_now_ms().unwrap();
    let stale_session = SessionId::from_bytes([7; 16]);
    let stale_issued_at_ms = now_ms - 400_000;
    node_directory
        .create(
            NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*stale_session.as_bytes()),
                stale_session,
                "https://stale.internal:8789".into(),
                fleet,
                certificate,
                image,
                registry.release_digest(),
                &SigningKey::from_bytes(&[8; 32]),
                1,
                stale_issued_at_ms,
                stale_issued_at_ms + 15_000,
                registry.module_digests(),
                vec![1],
                NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            stale_issued_at_ms,
        )
        .await
        .unwrap();
    node_directory
        .create(
            NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*session.as_bytes()),
                session,
                endpoint,
                fleet,
                certificate,
                image,
                registry.release_digest(),
                &key,
                1,
                now_ms,
                now_ms + 15_000,
                registry.module_digests(),
                vec![1],
                NodeCapacity {
                    free_memory_bytes: 1024 * 1024 * 1024,
                    free_disk_bytes: 1024 * 1024 * 1024,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            now_ms,
        )
        .await
        .unwrap();
    let router = RepositoryCellRouter::new(
        identity,
        layout.clone(),
        Arc::clone(&registry),
        runtime.clone(),
        super::super::RepositoryCellPeer::new(
            node_directory.clone(),
            Arc::new(PeerSigner::new(session, registry.release_digest(), key)),
            Arc::new(UnavailablePeer),
            owner,
        ),
        directory.path().join("session"),
    )
    .unwrap();
    let status = SchedulerStatus::new(now_ms).unwrap();
    let mut scheduler = RepositoryCellScheduler::new(
        identity,
        layout.clone(),
        node_directory.clone(),
        router,
        session,
        status.clone(),
    )
    .unwrap();
    scheduler.scan_once().await.unwrap();
    status.mark_completed(super::super::unix_now_ms().unwrap());

    let after = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(after.value().root.as_ref().unwrap().commit_sequence, 1);
    assert!(after.value().next_due_ms.is_some_and(|due| due > now_ms));
    assert_eq!(after.value().state, crab_cell_runtime::ControlState::Idle);
    assert_eq!(status.progress(), 2);
    assert!(status.is_healthy(super::super::unix_now_ms().unwrap()));
    assert!(
        !node_directory
            .is_live(stale_session, super::super::unix_now_ms().unwrap())
            .await
            .unwrap()
    );
    assert!(
        !node_directory
            .advertised_sessions(super::super::unix_now_ms().unwrap(), 8)
            .await
            .unwrap()
            .contains(&stale_session)
    );
    assert!(
        layout
            .store()
            .get_with_etag_bounded(&layout.node_path(stale_session.as_bytes()), 1_024)
            .await
            .is_ok()
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn activating_release_migrates_idle_cell_before_ready_gate() {
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([71; 16]),
        ApplicationId::from_bytes([72; 16]),
    );
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "scheduler-migration-test".into(),
        cargo_lock_digest: Digest::from_bytes([73; 32]),
    });
    builder.register(SchedulerWorkflow).unwrap();
    let registry = Arc::new(builder.finish().unwrap());
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("scheduler-release-migration"),
        *identity.application().as_bytes(),
    );
    let first_image = format!("sha256:{}", "4a".repeat(32));
    bootstrap_release_at(&layout, identity, &registry, &first_image)
        .await
        .unwrap();

    let target = CellTarget::new(
        identity.tenant(),
        identity.application(),
        WORKFLOW_NAMESPACE,
        b"retained-workflow",
    )
    .unwrap();
    let catalog = crab_cell_runtime::CellCatalog::new(layout.clone(), identity.tenant());
    let proof = catalog
        .provision(
            crab_cell_runtime::CatalogEntry::new(
                &target,
                crab_cell_runtime::CatalogRole::Workflow,
                WORKFLOW_PREDECESSOR,
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let session = SessionId::from_bytes([74; 16]);
    let endpoint = "https://scheduler-migration.internal:8789".to_owned();
    let owner = Owner {
        session,
        endpoint: endpoint.clone(),
    };
    let initial = authority
        .create_initial(&proof, IncarnationId::from_bytes([75; 16]), owner.clone())
        .await
        .unwrap();
    let local = tempfile::tempdir().unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 4).unwrap(), 16 * 1024 * 1024, session).unwrap();
    runtime
        .bootstrap(
            proof,
            CellReplica::new(
                layout.clone(),
                *target.cell_id().as_bytes(),
                *initial.value().incarnation.as_bytes(),
                ReplicaLimits::default(),
            )
            .unwrap(),
            authority.clone(),
            initial,
            local.path().join("retained.sqlite"),
            install_workflow_schema,
        )
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();

    let releases = crab_cell_runtime::ReleaseStore::new(layout.clone(), identity).unwrap();
    let ready = releases.load().await.unwrap().unwrap();
    let operation = RequestId::from_bytes([76; 16]);
    let second_image = format!("sha256:{}", "4b".repeat(32));
    let prepared = releases
        .prepare(
            registry.release_bytes(),
            registry.release_digest(),
            ready.record().revision(),
            &second_image,
            operation,
        )
        .await
        .unwrap();
    let activating = releases
        .start_activation(prepared.revision(), operation)
        .await
        .unwrap();

    let fleet = Digest::from_bytes([77; 32]);
    let image = Digest::from_bytes([78; 32]);
    let certificate = Digest::from_bytes([79; 32]);
    let node_directory =
        NodeDirectory::new(layout.clone(), fleet, image, registry.release_digest());
    let key = SigningKey::from_bytes(&[80; 32]);
    let now_ms = super::super::unix_now_ms().unwrap();
    node_directory
        .create(
            NodeAdvertisement::sign(
                crab_cell_runtime::NodeId::from_bytes(*session.as_bytes()),
                session,
                endpoint,
                fleet,
                certificate,
                image,
                registry.release_digest(),
                &key,
                1,
                now_ms,
                now_ms + 15_000,
                registry.module_digests(),
                vec![1],
                NodeCapacity {
                    free_memory_bytes: 1024 * 1024 * 1024,
                    free_disk_bytes: 1024 * 1024 * 1024,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            now_ms,
        )
        .await
        .unwrap();
    let router = RepositoryCellRouter::new(
        identity,
        layout.clone(),
        Arc::clone(&registry),
        runtime.clone(),
        super::super::RepositoryCellPeer::new(
            node_directory.clone(),
            Arc::new(PeerSigner::new(session, registry.release_digest(), key)),
            Arc::new(UnavailablePeer),
            owner,
        ),
        local.path().join("session"),
    )
    .unwrap();
    let status = SchedulerStatus::new(now_ms).unwrap();
    let mut scheduler = RepositoryCellScheduler::new(
        identity,
        layout.clone(),
        node_directory,
        router,
        session,
        status,
    )
    .unwrap();

    for _ in 0..2 {
        scheduler.scan_once().await.unwrap();
        if !scheduler.migration_jobs.is_empty() {
            break;
        }
    }
    scheduler
        .migration_jobs
        .join_next()
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let migrated = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(
        migrated.value().code,
        registry.module_code(WORKFLOW_MODULE).unwrap()
    );
    assert_eq!(migrated.value().schema, 1);
    assert_eq!(migrated.value().root.as_ref().unwrap().commit_sequence, 1);
    assert_eq!(migrated.value().state, ControlState::Idle);
    let progress = crab_cell_runtime::MigrationProgressStore::new(layout.clone(), identity)
        .unwrap()
        .load(target.cell_id(), operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        progress.state(),
        crab_cell_runtime::MigrationProgressState::Completed
    );
    assert_eq!(progress.attempts(), 1);
    super::super::verify_current_cells(&layout, identity, &registry)
        .await
        .unwrap();
    releases
        .complete_activation(activating.revision(), operation)
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
}
