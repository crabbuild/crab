use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

use crab_cell_runtime::{
    ActivityContext, ActivityExecution, ActivityHandler, ActivityRunOutcome, ApplicationId,
    BlockingActivityHandler, BlockingActivityPool, BuildDescriptor, CatalogEntry, CatalogRole,
    CellAuthority, CellCatalog, CellClient, CellModule, CellRuntime, CellTarget, Digest,
    DueCellScan, Error, IncarnationId, InvocationError, MaintenanceModule, MaintenanceTickCommand,
    MaintenanceTickOutcome, MaintenanceTickRequest, MigrationDescriptor, ModuleDescriptor,
    MutationIdentity, NamespaceDescriptor, NamespaceId, OperationDescriptor, Owner,
    RegistryBuilder, RequestId, SessionId, SqlWorkerPool, TenantId, WorkflowAction,
    WorkflowActivityClaimCommand, WorkflowActivityCompleteCommand, WorkflowActivityExtendCommand,
    WorkflowActivityModule, WorkflowActivityValidateQuery, WorkflowCancelCommand, WorkflowContext,
    WorkflowDecision, WorkflowDefinition, WorkflowGetQuery, WorkflowModule, WorkflowNamespace,
    WorkflowOutcome, WorkflowSignal, WorkflowSignalCommand, WorkflowStartCommand, WorkflowStatus,
    install_workflow_schema, register_activity, register_blocking_activity, register_maintenance,
    register_workflow, register_workflow_activities,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

const WORKFLOW_MODULE: &str = "workflow-api-test";
const WORKFLOW_NAMESPACE: NamespaceId = NamespaceId::from_bytes([8; 16]);
const EFFECT_NAMESPACE: NamespaceId = NamespaceId::from_bytes([9; 16]);
static EFFECT_TARGETS: [NamespaceId; 1] = [EFFECT_NAMESPACE];
static WORKFLOW_NAMESPACES: [NamespaceDescriptor; 2] = [
    NamespaceDescriptor {
        id: WORKFLOW_NAMESPACE,
        name: WORKFLOW_MODULE,
        role: CatalogRole::Workflow,
        shards: 1,
        effect_targets: &EFFECT_TARGETS,
        dead_letter: None,
    },
    NamespaceDescriptor {
        id: EFFECT_NAMESPACE,
        name: "workflow-effect-target",
        role: CatalogRole::Repository,
        shards: 1,
        effect_targets: &[],
        dead_letter: None,
    },
];
static DRIFT_NAMESPACES: [NamespaceDescriptor; 2] = [
    NamespaceDescriptor {
        id: WORKFLOW_NAMESPACE,
        name: WORKFLOW_MODULE,
        role: CatalogRole::Workflow,
        shards: 1,
        effect_targets: &[],
        dead_letter: None,
    },
    NamespaceDescriptor {
        id: EFFECT_NAMESPACE,
        name: "workflow-effect-target",
        role: CatalogRole::Repository,
        shards: 1,
        effect_targets: &[],
        dead_letter: None,
    },
];
const WORKFLOW_MIGRATION: &str = include_str!("../src/migrations/workflow.sql");
const DEFINITION_DIGEST: Digest = Digest::from_bytes([6; 32]);
const LEGACY_DEFINITION_DIGEST: Digest = Digest::from_bytes([7; 32]);
const COMMANDS: &[OperationDescriptor] = &[
    operation(1, 1024 * 1024, 64),
    operation(2, 1024 * 1024, 64),
    operation(3, 1024 * 1024, 64),
    operation(4, 1024 * 1024, 1024 * 1024),
    operation(5, 1024 * 1024, 1024 * 1024),
    operation(6, 1024 * 1024, 64),
    operation(7, 8, 5),
];
const QUERIES: &[OperationDescriptor] = &[
    operation(1, 2048, 1024 * 1024),
    operation(2, 1024 * 1024, 1),
];
static HEARTBEAT_OBSERVED: AtomicBool = AtomicBool::new(false);
static FAILOVER_ACTIVITY_ENTERED: AtomicBool = AtomicBool::new(false);
static FAILOVER_ACTIVITY_BLOCKED: AtomicBool = AtomicBool::new(false);
static FAILOVER_ACTIVITY_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

struct Definition;

static DEFINITION: Definition = Definition;
static LEGACY_DEFINITION: LegacyDefinition = LegacyDefinition;
static DEFINITIONS: [&dyn WorkflowDefinition; 2] = [&LEGACY_DEFINITION, &DEFINITION];

impl WorkflowDefinition for Definition {
    fn digest(&self) -> Digest {
        DEFINITION_DIGEST
    }

    fn effect_targets(&self) -> &'static [NamespaceId] {
        &EFFECT_TARGETS
    }

    fn transition(
        &self,
        _state: &[u8],
        event: &[u8],
        context: WorkflowContext,
    ) -> crab_cell_runtime::Result<WorkflowDecision> {
        if matches!(
            event,
            b"activity" | b"activity-retry" | b"activity-blocking" | b"activity-failover"
        ) {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Running,
                state: b"waiting".to_vec(),
                result: None,
                actions: vec![WorkflowAction::Activity {
                    activity_type: if event == b"activity-blocking" {
                        "blocking-echo".into()
                    } else {
                        "echo".into()
                    },
                    input: match event {
                        b"activity-retry" => b"retry".to_vec(),
                        b"activity-failover" => b"failover".to_vec(),
                        _ => b"payload".to_vec(),
                    },
                    due_at_ms: context.now_ms(),
                    expires_at_ms: context.now_ms() + 60_000,
                }],
            });
        }
        if event.starts_with(b"activity\0") {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Completed,
                state: b"activity-complete".to_vec(),
                result: Some(event.to_vec()),
                actions: Vec::new(),
            });
        }
        if event.starts_with(b"timer\0") {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Completed,
                state: b"timer-complete".to_vec(),
                result: Some(event.to_vec()),
                actions: Vec::new(),
            });
        }
        if event == b"timer" {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Running,
                state: b"timer-waiting".to_vec(),
                result: None,
                actions: vec![WorkflowAction::Timer {
                    due_at_ms: context.now_ms(),
                }],
            });
        }
        if event == b"finish" {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Completed,
                state: b"done".to_vec(),
                result: Some(b"finished".to_vec()),
                actions: Vec::new(),
            });
        }
        Ok(WorkflowDecision {
            status: WorkflowStatus::Running,
            state: event.to_vec(),
            result: None,
            actions: Vec::new(),
        })
    }
}

struct LegacyDefinition;

impl WorkflowDefinition for LegacyDefinition {
    fn digest(&self) -> Digest {
        LEGACY_DEFINITION_DIGEST
    }

    fn transition(
        &self,
        _state: &[u8],
        event: &[u8],
        _context: WorkflowContext,
    ) -> crab_cell_runtime::Result<WorkflowDecision> {
        let mut state = b"legacy:".to_vec();
        state.extend_from_slice(event);
        Ok(WorkflowDecision {
            status: WorkflowStatus::Running,
            state,
            result: None,
            actions: Vec::new(),
        })
    }
}

struct TestWorkflow;

impl WorkflowModule for TestWorkflow {
    const MODULE: &'static str = WORKFLOW_MODULE;
    const NAMESPACE: NamespaceId = WORKFLOW_NAMESPACE;
    const CURRENT_DEFINITION: &'static dyn WorkflowDefinition = &DEFINITION;
    const DEFINITIONS: &'static [&'static dyn WorkflowDefinition] = &DEFINITIONS;
    const START_COMMAND_ID: u32 = 1;
    const SIGNAL_COMMAND_ID: u32 = 2;
    const CANCEL_COMMAND_ID: u32 = 3;
    const GET_QUERY_ID: u32 = 1;
}

impl WorkflowActivityModule for TestWorkflow {
    const ACTIVITY_TYPES: &'static [&'static str] = &["blocking-echo", "echo"];
    const ACTIVITY_CLAIM_COMMAND_ID: u32 = 4;
    const ACTIVITY_COMPLETE_COMMAND_ID: u32 = 5;
    const ACTIVITY_EXTEND_COMMAND_ID: u32 = 6;
    const ACTIVITY_VALIDATE_QUERY_ID: u32 = 2;
}

impl MaintenanceModule for TestWorkflow {
    const MODULE: &'static str = WORKFLOW_MODULE;
    const TICK_COMMAND_ID: u32 = 7;
    const WORKFLOW_DEFINITIONS: &'static [&'static dyn WorkflowDefinition] = &DEFINITIONS;
}

struct EchoActivity;

impl ActivityHandler for EchoActivity {
    const TYPE: &'static str = "echo";

    fn execute(
        context: ActivityContext,
        input: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = ActivityExecution> + Send + 'static>> {
        Box::pin(async move {
            if input == b"retry" {
                return ActivityExecution::Failed {
                    details: b"temporary".to_vec(),
                    retryable: true,
                };
            }
            if input == b"failover" {
                FAILOVER_ACTIVITY_ATTEMPTS.fetch_add(1, Ordering::AcqRel);
                FAILOVER_ACTIVITY_ENTERED.store(true, Ordering::Release);
                while FAILOVER_ACTIVITY_BLOCKED.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            let initial_deadline = context.lease_until_ms();
            assert_ne!(context.idempotency_key(), [0; 32]);
            assert_ne!(context.lease_token(), [0; 16]);
            tokio::time::sleep(Duration::from_millis(1_800)).await;
            HEARTBEAT_OBSERVED.store(
                context.lease_until_ms() > initial_deadline
                    && !context.cancellation().is_cancelled(),
                Ordering::Release,
            );
            let mut result = input;
            result.extend_from_slice(b"-complete");
            ActivityExecution::Completed(result)
        })
    }
}

struct BlockingEchoActivity;

impl BlockingActivityHandler for BlockingEchoActivity {
    const TYPE: &'static str = "blocking-echo";

    fn execute(_context: ActivityContext, mut input: Vec<u8>) -> ActivityExecution {
        input.extend_from_slice(b"-blocking");
        ActivityExecution::Completed(input)
    }
}

impl CellModule for TestWorkflow {
    const NAME: &'static str = WORKFLOW_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<ModuleDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: WORKFLOW_MODULE,
            source_digest: Digest::from_bytes([4; 32]),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: WORKFLOW_MIGRATION,
                digest: Digest::from_bytes(*blake3::hash(WORKFLOW_MIGRATION.as_bytes()).as_bytes()),
            }])),
            commands: COMMANDS,
            queries: QUERIES,
            workflow_definitions: &[LEGACY_DEFINITION_DIGEST, DEFINITION_DIGEST],
            activity_types: &["blocking-echo", "echo"],
            namespaces: &WORKFLOW_NAMESPACES,
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_workflow::<Self>(registry)?;
        register_workflow_activities::<Self>(registry)?;
        register_activity::<Self, EchoActivity>(registry)?;
        register_blocking_activity::<Self, BlockingEchoActivity>(registry)?;
        register_maintenance::<Self>(registry)
    }
}

struct EffectTargetDrift;

impl CellModule for EffectTargetDrift {
    const NAME: &'static str = WORKFLOW_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        Box::leak(Box::new(ModuleDescriptor {
            name: WORKFLOW_MODULE,
            source_digest: Digest::from_bytes([4; 32]),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: TestWorkflow.descriptor().migrations,
            commands: COMMANDS,
            queries: QUERIES,
            workflow_definitions: &[LEGACY_DEFINITION_DIGEST, DEFINITION_DIGEST],
            activity_types: &["blocking-echo", "echo"],
            namespaces: &DRIFT_NAMESPACES,
        }))
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        TestWorkflow.register(registry)
    }
}

struct MissingDefinitionBinding;

impl CellModule for MissingDefinitionBinding {
    const NAME: &'static str = WORKFLOW_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        TestWorkflow.descriptor()
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_workflow_definition(WORKFLOW_MODULE, &DEFINITION)?;
        registry.bind_workflow_definition(WORKFLOW_MODULE, &LEGACY_DEFINITION)?;
        registry.bind_activity_inventory(WORKFLOW_MODULE, DEFINITION_DIGEST, &["echo"])?;
        registry.bind_activity_inventory(WORKFLOW_MODULE, LEGACY_DEFINITION_DIGEST, &["echo"])?;
        registry.bind_command::<WorkflowStartCommand<TestWorkflow>>()?;
        registry.bind_command::<WorkflowSignalCommand<TestWorkflow>>()?;
        registry.bind_command::<WorkflowCancelCommand<TestWorkflow>>()?;
        registry.bind_command::<WorkflowActivityClaimCommand<TestWorkflow>>()?;
        registry.bind_command::<WorkflowActivityCompleteCommand<TestWorkflow>>()?;
        registry.bind_command::<WorkflowActivityExtendCommand<TestWorkflow>>()?;
        registry.bind_query::<WorkflowGetQuery<TestWorkflow>>()?;
        registry.bind_query::<WorkflowActivityValidateQuery<TestWorkflow>>()?;
        registry.bind_command::<MaintenanceTickCommand<TestWorkflow>>()?;
        registry.bind_activity::<EchoActivity>(WORKFLOW_MODULE, DEFINITION_DIGEST)
    }
}

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

fn registry() -> Arc<crab_cell_runtime::Registry> {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "workflow-api-test".into(),
        cargo_lock_digest: Digest::from_bytes([5; 32]),
    });
    builder.register(TestWorkflow).unwrap();
    Arc::new(builder.finish().unwrap())
}

fn identity(byte: u8) -> MutationIdentity {
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

#[test]
fn registry_rejects_a_declared_activity_without_its_native_binding() {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "workflow-api-test".into(),
        cargo_lock_digest: Digest::from_bytes([5; 32]),
    });
    builder.register(MissingDefinitionBinding).unwrap();
    assert!(matches!(
        builder.finish(),
        Err(Error::Registry("descriptor and activity bindings differ"))
    ));
}

#[test]
fn registry_rejects_workflow_effect_target_drift() {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "workflow-effect-target-test".into(),
        cargo_lock_digest: Digest::from_bytes([5; 32]),
    });
    builder.register(EffectTargetDrift).unwrap();
    assert!(matches!(
        builder.finish(),
        Err(Error::Registry(
            "Workflow effect targets and compiled definitions differ"
        ))
    ));
}

#[tokio::test]
async fn typed_workflow_namespace_publishes_rejects_reads_and_survives_restore() {
    let registry = registry();
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        WORKFLOW_NAMESPACE,
        &0_u32.to_be_bytes(),
    )
    .unwrap();
    let cell = target.cell_id();
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
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Workflow,
                registry.module_code(WORKFLOW_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout);
    let first_session = SessionId::from_bytes([4; 16]);
    let control = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://first.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            control,
            directory.path().join("first.sqlite"),
            install_workflow_schema,
        )
        .await
        .unwrap();
    let client = CellClient::local(registry.clone(), handle.clone());
    let workflows = WorkflowNamespace::<TestWorkflow>::new(
        client.clone(),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    let started = workflows
        .start(identity(7), b"build-42".to_vec(), b"start".to_vec())
        .await
        .unwrap();
    let WorkflowOutcome::Applied { run_id, .. } = started.output else {
        panic!("workflow start was not applied");
    };
    let signal = WorkflowSignal {
        workflow_id: b"build-42".to_vec(),
        run_id,
        signal_id: [8; 16],
        event: b"continue".to_vec(),
    };
    let signalled = workflows.signal(identity(9), signal.clone()).await.unwrap();
    assert!(matches!(
        signalled.output,
        WorkflowOutcome::Applied {
            event_sequence: 2,
            ..
        }
    ));
    assert!(matches!(
        workflows
            .signal(identity(10), signal.clone())
            .await
            .unwrap()
            .output,
        WorkflowOutcome::Duplicate {
            event_sequence: 2,
            ..
        }
    ));
    let mut conflicting = signal;
    conflicting.event = b"different".to_vec();
    let conflict = workflows.signal(identity(11), conflicting).await;
    assert!(matches!(
        conflict,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == WorkflowOutcome::IdentityConflict
    ));
    let state = workflows
        .state(b"build-42".to_vec(), Some(signalled.receipt))
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(state.state, b"continue");
    assert_eq!(state.event_sequence, 2);
    let timer = workflows
        .start(identity(16), b"timer-build".to_vec(), b"timer".to_vec())
        .await
        .unwrap();
    let mut due = DueCellScan::new(&catalog, authority.clone(), target.cell_id().as_bytes()[0])
        .await
        .unwrap();
    let due = due.next_batch(i64::MAX).await.unwrap().unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(
        due[0]
            .control()
            .value()
            .root
            .as_ref()
            .unwrap()
            .commit_sequence,
        timer.receipt.commit_sequence
    );
    let routed = runtime
        .local_handle(due[0].catalog().clone(), due[0].control())
        .await
        .unwrap()
        .unwrap();
    let scheduler_client = CellClient::local(registry.clone(), routed);
    let tick = registry
        .run_maintenance_once(
            scheduler_client.clone(),
            target.clone(),
            identity(17),
            MaintenanceTickRequest {
                expected_commit_sequence: timer.receipt.commit_sequence,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        tick.output,
        MaintenanceTickOutcome::Applied { processed: 1 }
    );
    assert_eq!(
        workflows
            .state(b"timer-build".to_vec(), Some(tick.receipt))
            .await
            .unwrap()
            .output
            .unwrap()
            .state,
        b"timer-complete"
    );
    let stale = registry
        .run_maintenance_once(
            scheduler_client,
            target.clone(),
            identity(18),
            MaintenanceTickRequest {
                expected_commit_sequence: timer.receipt.commit_sequence,
            },
        )
        .await
        .unwrap();
    assert_eq!(stale.output, MaintenanceTickOutcome::Stale);
    handle.drain().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
    let second_session = SessionId::from_bytes([12; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            replica,
            authority,
            idle,
            directory.path().join("second.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://second.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_workflows = WorkflowNamespace::<TestWorkflow>::new(
        CellClient::local(registry, restored.clone()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    assert_eq!(
        restored_workflows
            .state(b"build-42".to_vec(), Some(signalled.receipt))
            .await
            .unwrap()
            .output
            .unwrap()
            .state,
        b"continue"
    );
    let cancelled = restored_workflows
        .cancel(
            identity(13),
            WorkflowSignal {
                workflow_id: b"build-42".to_vec(),
                run_id,
                signal_id: [14; 16],
                event: b"cancel".to_vec(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        cancelled.output,
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Cancelled,
            event_sequence: 3,
            ..
        }
    ));
    restored.drain().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn native_activity_heartbeats_and_recovers_after_node_loss() {
    HEARTBEAT_OBSERVED.store(false, Ordering::Release);
    FAILOVER_ACTIVITY_ENTERED.store(false, Ordering::Release);
    FAILOVER_ACTIVITY_BLOCKED.store(true, Ordering::Release);
    FAILOVER_ACTIVITY_ATTEMPTS.store(0, Ordering::Release);
    let registry = registry();
    assert!(registry.has_blocking_activities());
    assert!(registry.requires_blocking_activity(WORKFLOW_NAMESPACE));
    assert_eq!(
        registry.internal_command_action(WORKFLOW_NAMESPACE, 7, 1),
        Some("cell.scheduler.tick")
    );
    assert_eq!(
        registry.internal_command_action(WORKFLOW_NAMESPACE, 4, 1),
        Some("cell.activity.source")
    );
    assert_eq!(
        registry.internal_query_action(WORKFLOW_NAMESPACE, 2, 1),
        Some("cell.activity.source")
    );
    assert_eq!(
        registry.internal_command_action(WORKFLOW_NAMESPACE, 1, 1),
        None
    );
    let target = CellTarget::new(
        TenantId::from_bytes([21; 16]),
        ApplicationId::from_bytes([22; 16]),
        WORKFLOW_NAMESPACE,
        &0_u32.to_be_bytes(),
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([23; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("activity-runtime"), [22; 16]);
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Workflow,
                registry.module_code(WORKFLOW_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout);
    let first_session = SessionId::from_bytes([25; 16]);
    let control = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://activity-first.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let first_database = directory.path().join("activity-first.sqlite");
    let handle = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            control,
            first_database.clone(),
            install_workflow_schema,
        )
        .await
        .unwrap();
    let client = CellClient::local(registry.clone(), handle.clone());
    let workflows = WorkflowNamespace::<TestWorkflow>::new(
        client.clone(),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    workflows
        .start(
            identity(26),
            b"activity-build".to_vec(),
            b"activity".to_vec(),
        )
        .await
        .unwrap();
    assert!(registry.has_activity_runner(target.namespace()));
    let blocking_pool = BlockingActivityPool::new(1).unwrap();
    assert!(matches!(
        registry
            .run_activity_once(client.clone(), &target, 5_000, None)
            .await,
        Err(crab_cell_runtime::ActivitySupervisorError::Runtime(
            Error::Capacity("blocking activity slot was not reserved")
        ))
    ));
    let completed = registry
        .run_activity_once(client, &target, 5_000, blocking_pool.try_reserve().unwrap())
        .await
        .unwrap();
    let ActivityRunOutcome::Completed { workflow, receipt } = completed else {
        panic!("native activity was not completed");
    };
    assert!(matches!(
        workflow,
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Completed,
            event_sequence: 2,
            ..
        }
    ));
    assert!(HEARTBEAT_OBSERVED.load(Ordering::Acquire));
    let state = workflows
        .state(b"activity-build".to_vec(), Some(receipt))
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(state.state, b"activity-complete");
    assert!(state.result.unwrap().ends_with(b"payload-complete"));
    workflows
        .start(
            identity(28),
            b"retry-build".to_vec(),
            b"activity-retry".to_vec(),
        )
        .await
        .unwrap();
    assert!(matches!(
        registry
            .run_activity_once(
                CellClient::local(registry.clone(), handle.clone()),
                &target,
                5_000,
                blocking_pool.try_reserve().unwrap(),
            )
            .await
            .unwrap(),
        ActivityRunOutcome::Retrying { .. }
    ));
    workflows
        .start(
            identity(29),
            b"blocking-build".to_vec(),
            b"activity-blocking".to_vec(),
        )
        .await
        .unwrap();
    let blocking = blocking_pool.try_reserve().unwrap().unwrap();
    assert!(matches!(
        registry
            .run_activity_once(
                CellClient::local(registry.clone(), handle.clone()),
                &target,
                5_000,
                Some(blocking),
            )
            .await
            .unwrap(),
        ActivityRunOutcome::Completed { .. }
    ));
    assert!(
        workflows
            .state(b"blocking-build".to_vec(), None)
            .await
            .unwrap()
            .output
            .unwrap()
            .result
            .unwrap()
            .ends_with(b"payload-blocking")
    );
    workflows
        .start(
            identity(30),
            b"failover-build".to_vec(),
            b"activity-failover".to_vec(),
        )
        .await
        .unwrap();
    let failover_registry = registry.clone();
    let failover_client = CellClient::local(registry.clone(), handle.clone());
    let failover_target = target.clone();
    let failover_reservation = blocking_pool.try_reserve().unwrap();
    let first_attempt = tokio::spawn(async move {
        failover_registry
            .run_activity_once(
                failover_client,
                &failover_target,
                5_000,
                failover_reservation,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !FAILOVER_ACTIVITY_ENTERED.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    first_attempt.abort();
    assert!(first_attempt.await.unwrap_err().is_cancelled());
    assert_eq!(FAILOVER_ACTIVITY_ATTEMPTS.load(Ordering::Acquire), 1);
    blocking_pool.shutdown().await.unwrap();
    drop(workflows);
    drop(handle);
    drop(runtime);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match std::fs::remove_file(&first_database) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("dropped node did not release its local SQLite file");

    let stale_owner = authority.load(cell).await.unwrap().unwrap();
    assert_eq!(
        stale_owner.value().owner.as_ref().unwrap().session,
        first_session
    );
    assert!(stale_owner.value().root.is_some());
    let second_session = SessionId::from_bytes([27; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            proof,
            replica,
            authority.clone(),
            stale_owner,
            directory.path().join("activity-second.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://activity-second.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_workflows = WorkflowNamespace::<TestWorkflow>::new(
        CellClient::local(registry.clone(), restored.clone()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    assert_eq!(
        restored_workflows
            .state(b"activity-build".to_vec(), Some(receipt))
            .await
            .unwrap()
            .output
            .unwrap()
            .state,
        b"activity-complete"
    );
    let current = authority.load(cell).await.unwrap().unwrap();
    let current_sequence = current.value().root.as_ref().unwrap().commit_sequence;
    let restored_client = CellClient::local(registry.clone(), restored.clone());
    let reclaimed = registry
        .run_maintenance_once(
            restored_client.clone(),
            target.clone(),
            identity(31),
            MaintenanceTickRequest {
                expected_commit_sequence: current_sequence,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        reclaimed.output,
        MaintenanceTickOutcome::Applied { processed: 1 }
    );
    FAILOVER_ACTIVITY_BLOCKED.store(false, Ordering::Release);
    let restored_blocking_pool = BlockingActivityPool::new(1).unwrap();
    let older_retry = registry
        .run_activity_once(
            restored_client.clone(),
            &target,
            5_000,
            restored_blocking_pool.try_reserve().unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(older_retry, ActivityRunOutcome::Retrying { .. }));
    let failover = registry
        .run_activity_once(
            restored_client,
            &target,
            5_000,
            restored_blocking_pool.try_reserve().unwrap(),
        )
        .await
        .unwrap();
    assert!(
        matches!(failover, ActivityRunOutcome::Completed { .. }),
        "unexpected failover outcome: {failover:?}"
    );
    assert_eq!(FAILOVER_ACTIVITY_ATTEMPTS.load(Ordering::Acquire), 2);
    assert!(
        restored_workflows
            .state(b"failover-build".to_vec(), None)
            .await
            .unwrap()
            .output
            .unwrap()
            .result
            .unwrap()
            .ends_with(b"failover-complete")
    );
    restored_blocking_pool.shutdown().await.unwrap();
    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}
