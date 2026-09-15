use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use crab_cell_runtime::{
    ActivityContext, ActivityExecution, ApplicationId, BlockingActivityHandler, BuildDescriptor,
    CellModule, CellReplica, CellRuntime, CellTarget, Digest, IncarnationId, MaintenanceModule,
    MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor, NamespaceId, NodeAdvertisement,
    NodeCapacity, OperationDescriptor, Owner, PeerRoundTrip, PeerSigner, RegistryBuilder,
    ReplicaLimits, SqlWorkerPool, TenantId, WorkflowAction, WorkflowActivityModule,
    WorkflowContext, WorkflowDecision, WorkflowDefinition, WorkflowModule, WorkflowNamespace,
    WorkflowStatus, install_workflow_schema, register_blocking_activity, register_maintenance,
    register_workflow, register_workflow_activities,
};
use crab_storage::{CellStorageLayout, StorageError, Store};
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
        node_directory,
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
    assert!(matches!(
        layout
            .store()
            .get_with_etag_bounded(&layout.node_path(stale_session.as_bytes()), 1)
            .await,
        Err(StorageError::NotFound { .. })
    ));
    runtime.shutdown().await.unwrap();
}
