use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    time::Duration,
};

use crab_cell_runtime::{
    ActivityRunOutcome, ApplicationIdentity, CellAuthority, CellCatalog, CellId, CellTarget,
    DueCellScan, EffectRunOutcome, InvocationError, MaintenanceTickOutcome, MaintenanceTickRequest,
    MutationIdentity, NodeDirectory, Registry, RequestId, SchedulerFleet, SessionId,
    preferred_scanner,
};
use crab_storage::CellStorageLayout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::RepositoryCellRouter;

const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_LIVE_NODES: usize = 10_000;
const MAX_DUE_PER_CYCLE: usize = 128;
const EFFECT_LEASE_MS: u32 = 30_000;
const ACTIVITY_LEASE_MS: u32 = 30_000;
const MAX_ACTIVITY_JOBS: usize = 16;
const SCHEDULER_STALE_AFTER_MS: i64 = 15_000;
const NODE_COLLECTION_INTERVAL_MS: i64 = 60_000;
const NODE_COLLECTION_LIMIT: usize = 128;

/// Shared scanner progress used by enrollment, readiness and metrics.
#[derive(Clone)]
pub(crate) struct SchedulerStatus {
    progress: Arc<AtomicU64>,
    last_completed_ms: Arc<AtomicI64>,
    completed: Arc<AtomicBool>,
}

impl SchedulerStatus {
    pub(crate) fn new(now_ms: i64) -> crate::Result<Self> {
        if now_ms < 0 {
            return Err(crab_cell_runtime::Error::Control("negative scheduler status time").into());
        }
        Ok(Self {
            progress: Arc::new(AtomicU64::new(1)),
            last_completed_ms: Arc::new(AtomicI64::new(now_ms)),
            completed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn progress(&self) -> u64 {
        self.progress.load(Ordering::Acquire)
    }

    pub(crate) fn lag_ms(&self, now_ms: i64) -> u64 {
        u64::try_from(now_ms.saturating_sub(self.last_completed_ms.load(Ordering::Acquire)))
            .unwrap_or(0)
    }

    pub(crate) fn is_healthy(&self, now_ms: i64) -> bool {
        let last = self.last_completed_ms.load(Ordering::Acquire);
        self.completed.load(Ordering::Acquire)
            && now_ms >= last
            && now_ms - last < SCHEDULER_STALE_AFTER_MS
    }

    pub(crate) fn mark_completed(&self, now_ms: i64) {
        self.last_completed_ms.store(now_ms, Ordering::Release);
        self.completed.store(true, Ordering::Release);
        let _ = self
            .progress
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |progress| {
                progress.checked_add(1)
            });
    }
}

/// Fleet-assigned scanner for compiled Cell maintenance, activities and effects.
pub(crate) struct RepositoryCellScheduler {
    identity: ApplicationIdentity,
    catalog: CellCatalog,
    authority: CellAuthority,
    directory: NodeDirectory,
    router: RepositoryCellRouter,
    session: SessionId,
    status: SchedulerStatus,
    fleet: SchedulerFleet,
    registry: Arc<Registry>,
    activity_admission: Arc<tokio::sync::Semaphore>,
    activity_cells: Arc<Mutex<HashSet<CellId>>>,
    activity_jobs: tokio::task::JoinSet<()>,
    last_node_collection_ms: i64,
}

impl RepositoryCellScheduler {
    pub(crate) fn new(
        identity: ApplicationIdentity,
        layout: CellStorageLayout,
        directory: NodeDirectory,
        router: RepositoryCellRouter,
        session: SessionId,
        status: SchedulerStatus,
    ) -> Self {
        let registry = router.registry();
        Self {
            identity,
            catalog: CellCatalog::new(layout.clone(), identity.tenant()),
            authority: CellAuthority::new(layout),
            directory,
            router,
            session,
            status,
            fleet: SchedulerFleet::default(),
            registry,
            activity_admission: Arc::new(tokio::sync::Semaphore::new(
                std::thread::available_parallelism()
                    .map_or(1, |count| count.get())
                    .min(MAX_ACTIVITY_JOBS),
            )),
            activity_cells: Arc::new(Mutex::new(HashSet::new())),
            activity_jobs: tokio::task::JoinSet::new(),
            last_node_collection_ms: 0,
        }
    }

    pub(crate) async fn run(mut self, cancellation: CancellationToken) -> crate::Result<()> {
        loop {
            if cancellation.is_cancelled() {
                self.activity_jobs.abort_all();
                while self.activity_jobs.join_next().await.is_some() {}
                return Ok(());
            }
            self.reap_activity_jobs();
            match self.scan_once().await {
                Ok(()) => self.status.mark_completed(super::unix_now_ms()?),
                Err(error) => {
                    tracing::warn!(error = %error, "Cell scheduler scan failed");
                }
            }
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = tokio::time::sleep(SCAN_INTERVAL) => {}
            }
        }
    }

    async fn scan_once(&mut self) -> crate::Result<()> {
        let now_ms = super::unix_now_ms()?;
        let advertisements = self.directory.live(now_ms, MAX_LIVE_NODES).await?;
        let nodes =
            self.fleet
                .eligible_sessions(&advertisements, now_ms, SCHEDULER_STALE_AFTER_MS)?;
        if preferred_scanner(0, &nodes)? == Some(self.session)
            && now_ms.saturating_sub(self.last_node_collection_ms) >= NODE_COLLECTION_INTERVAL_MS
        {
            let removed = self
                .directory
                .collect_stale(now_ms, NODE_COLLECTION_LIMIT)
                .await?;
            self.last_node_collection_ms = now_ms;
            tracing::debug!(removed, "collected stale Cell node advertisements");
        }
        let mut remaining = MAX_DUE_PER_CYCLE;
        for shard in 0_u8..=u8::MAX {
            if remaining == 0 || preferred_scanner(shard, &nodes)? != Some(self.session) {
                continue;
            }
            let mut scan = DueCellScan::new(&self.catalog, self.authority.clone(), shard).await?;
            while remaining != 0 {
                let Some(batch) = scan.next_batch(now_ms).await? else {
                    break;
                };
                for due in batch.into_iter().take(remaining) {
                    remaining -= 1;
                    if let Err(error) = self.process(due).await {
                        tracing::warn!(error = %error, "Cell scheduler item failed");
                    }
                }
            }
        }
        Ok(())
    }

    async fn process(&mut self, due: crab_cell_runtime::DueCell) -> crate::Result<()> {
        let entry = due.catalog().entry();
        let control = due.control().value();
        if !self.registry.supports_cell(
            entry.namespace(),
            entry.role(),
            control.code,
            control.schema,
        ) {
            return Err(crab_cell_runtime::Error::Registry(
                "due Cell is unsupported by the running release",
            )
            .into());
        }
        let target = CellTarget::new(
            self.identity.tenant(),
            self.identity.application(),
            entry.namespace(),
            entry.partition(),
        )?;
        let expected_commit_sequence = control
            .root
            .as_ref()
            .ok_or(crab_cell_runtime::Error::CellNotActive)?
            .commit_sequence;
        let scheduled = self.router.route_scheduler_target(target.clone()).await?;
        let release_after = scheduled.should_release();
        self.process_cell(scheduled.cell, expected_commit_sequence, release_after)
            .await
    }

    async fn process_cell(
        &mut self,
        cell: super::RepositoryCell,
        expected_commit_sequence: u64,
        release_after: bool,
    ) -> crate::Result<()> {
        let tick = self
            .registry
            .run_maintenance_once(
                cell.client.clone(),
                cell.target.clone(),
                mutation_identity()?,
                MaintenanceTickRequest {
                    expected_commit_sequence,
                },
            )
            .await;
        let processed = match tick {
            Ok(committed) => match committed.output {
                MaintenanceTickOutcome::Applied { processed } => processed,
                MaintenanceTickOutcome::Stale => {
                    return self.release_after(&cell.target, release_after).await;
                }
            },
            Err(InvocationError::Rejected(_)) => {
                return self.release_after(&cell.target, release_after).await;
            }
            Err(error) => {
                tracing::warn!(error = %error, "Cell Tick was not resolved");
                return self.release_after(&cell.target, release_after).await;
            }
        };
        if processed != 0 {
            return self.release_after(&cell.target, release_after).await;
        }
        if self.registry.has_activity_runner(cell.target.namespace())
            && let Ok(permit) = Arc::clone(&self.activity_admission).try_acquire_owned()
            && let Some(activity_cell) = self.reserve_activity(cell.target.cell_id())
        {
            let registry = Arc::clone(&self.registry);
            let router = self.router.clone();
            self.activity_jobs.spawn(async move {
                let target = cell.target.clone();
                match registry
                    .run_activity_once(cell.client.clone(), &target, ACTIVITY_LEASE_MS)
                    .await
                {
                    Ok(ActivityRunOutcome::Idle { .. })
                    | Ok(ActivityRunOutcome::LeaseLost { .. })
                    | Ok(ActivityRunOutcome::IdentityConflict { .. })
                    | Ok(ActivityRunOutcome::Retrying { .. })
                    | Ok(ActivityRunOutcome::Completed { .. })
                    | Ok(ActivityRunOutcome::Duplicate { .. }) => {}
                    Err(error) => {
                        tracing::warn!(error = %error, "Workflow activity was not resolved");
                    }
                }
                if registry.has_effect_runner(target.namespace()) {
                    run_effect(&registry, &router, cell).await;
                }
                if release_after && let Err(error) = router.drain_local_target(&target).await {
                    tracing::warn!(error = %error, "Workflow scheduler Cell release failed");
                }
                drop(activity_cell);
                drop(permit);
            });
            return Ok(());
        }
        if !self.registry.has_effect_runner(cell.target.namespace()) {
            return self.release_after(&cell.target, release_after).await;
        }
        let target = cell.target.clone();
        match self
            .registry
            .run_effect_once(
                cell.client,
                cell.target,
                self.router.effect_peer_client(),
                EFFECT_LEASE_MS,
            )
            .await
        {
            Ok(EffectRunOutcome::Idle { .. })
            | Ok(EffectRunOutcome::Delivered { .. })
            | Ok(EffectRunOutcome::Retrying { .. })
            | Ok(EffectRunOutcome::Failed { .. })
            | Ok(EffectRunOutcome::LeaseLost { .. }) => {
                self.release_after(&target, release_after).await
            }
            Err(error) => {
                tracing::warn!(error = %error, "Cell effect was not resolved");
                self.release_after(&target, release_after).await
            }
        }
    }

    async fn release_after(&self, target: &CellTarget, release_after: bool) -> crate::Result<()> {
        if release_after {
            self.router.drain_local_target(target).await
        } else {
            Ok(())
        }
    }

    fn reap_activity_jobs(&mut self) {
        while let Some(result) = self.activity_jobs.try_join_next() {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                tracing::warn!(error = %error, "Workflow activity task failed");
            }
        }
    }

    fn reserve_activity(&self, cell: CellId) -> Option<ActivityCellReservation> {
        let mut cells = self
            .activity_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cells.insert(cell).then(|| ActivityCellReservation {
            cell,
            cells: Arc::clone(&self.activity_cells),
        })
    }
}

struct ActivityCellReservation {
    cell: CellId,
    cells: Arc<Mutex<HashSet<CellId>>>,
}

impl Drop for ActivityCellReservation {
    fn drop(&mut self) {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.cell);
    }
}

async fn run_effect(
    registry: &Registry,
    router: &RepositoryCellRouter,
    cell: super::RepositoryCell,
) {
    match registry
        .run_effect_once(
            cell.client,
            cell.target,
            router.effect_peer_client(),
            EFFECT_LEASE_MS,
        )
        .await
    {
        Ok(EffectRunOutcome::Idle { .. })
        | Ok(EffectRunOutcome::Delivered { .. })
        | Ok(EffectRunOutcome::Retrying { .. })
        | Ok(EffectRunOutcome::Failed { .. })
        | Ok(EffectRunOutcome::LeaseLost { .. }) => {}
        Err(error) => tracing::warn!(error = %error, "Cell effect was not resolved"),
    }
}

fn mutation_identity() -> crate::Result<MutationIdentity> {
    let now_ms = super::unix_now_ms()?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms
            .checked_add(60_000)
            .ok_or(crab_cell_runtime::Error::Command(
                "scheduler request expiry overflow",
            ))?,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use crab_cell_runtime::{
        ActivityContext, ActivityExecution, ActivityHandler, ApplicationId, BuildDescriptor,
        CellModule, CellReplica, CellRuntime, CellTarget, Digest, IncarnationId, MaintenanceModule,
        MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor, NamespaceId, NodeAdvertisement,
        NodeCapacity, OperationDescriptor, Owner, PeerRoundTrip, PeerSigner, RegistryBuilder,
        ReplicaLimits, SqlWorkerPool, TenantId, WorkflowAction, WorkflowActivityModule,
        WorkflowContext, WorkflowDecision, WorkflowDefinition, WorkflowModule, WorkflowNamespace,
        WorkflowStatus, install_workflow_schema, register_activity, register_maintenance,
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
        include_str!("../../../crab-cell-runtime/src/migrations/workflow.sql");
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
        const WORKFLOW_DEFINITIONS: &'static [&'static dyn WorkflowDefinition] =
            &WORKFLOW_DEFINITIONS;
    }

    struct SchedulerEcho;

    impl ActivityHandler for SchedulerEcho {
        const TYPE: &'static str = "scheduler-echo";

        fn execute(
            _context: ActivityContext,
            input: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = ActivityExecution> + Send + 'static>> {
            Box::pin(async move {
                WORKFLOW_ACTIVITY_RUNS.fetch_add(1, Ordering::AcqRel);
                while !WORKFLOW_ACTIVITY_RELEASE.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                ActivityExecution::Completed(input)
            })
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
                    digest: Digest::from_bytes(
                        *blake3::hash(WORKFLOW_MIGRATION.as_bytes()).as_bytes(),
                    ),
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
            register_activity::<Self, SchedulerEcho>(registry)?;
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
        ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>>
        {
            Box::pin(async { Err(crab_cell_runtime::Error::CellNotActive) })
        }
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
            RepositoryCellScheduler::new(identity, layout, node_directory, router, session, status);
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
        let target = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            repository.as_bytes(),
        )
        .unwrap();
        let (proof, authority) = provision_repository(&layout, identity, &registry, &target)
            .await
            .unwrap();
        let session = SessionId::from_bytes([4; 16]);
        let endpoint = "https://localhost:8789".to_owned();
        let owner = Owner {
            session,
            endpoint: endpoint.clone(),
        };
        let observed = authority
            .create_initial(&proof, IncarnationId::from_bytes([5; 16]), owner.clone())
            .await
            .unwrap();
        let runtime =
            CellRuntime::new(SqlWorkerPool::new(1, 4).unwrap(), 16 * 1024 * 1024, session).unwrap();
        let replica = CellReplica::new(
            layout.clone(),
            *target.cell_id().as_bytes(),
            *observed.value().incarnation.as_bytes(),
            ReplicaLimits::default(),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let sqlite = directory.path().join("bootstrap.sqlite");
        let repository_bytes = repository.into_bytes();
        let handle = runtime
            .bootstrap(
                proof,
                replica,
                authority.clone(),
                observed,
                sqlite,
                move |transaction| {
                    transaction.execute_batch(REPOSITORY_MIGRATION)?;
                    transaction.execute(
                        "INSERT INTO repository_identity(singleton, repository_uuid) VALUES (1, ?1)",
                        [repository_bytes.as_slice()],
                    )?;
                    transaction.execute(
                        "INSERT INTO sys_inbox VALUES (?1, ?2, 1, X'', 0, 1, 1)",
                        ([9_u8; 32].as_slice(), [8_u8; 32].as_slice()),
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        handle.drain().await.unwrap();
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
        );
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
}
