use std::{
    future::Future,
    io::Write,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use crab_cell_runtime::Error;
use crab_cell_runtime::cell::actor::CellRuntime;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::catalog::{CatalogEntry, CellCatalog};
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::control::Owner;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::fleet::scheduler::DueCellScan;
use crab_cell_runtime::identity::IncarnationId;
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_cell_runtime::primitives::activity_pool::BlockingActivityPool;
use crab_cell_runtime::primitives::maintenance::{
    MaintenanceModule, MaintenanceTickCommand, MaintenanceTickOutcome, MaintenanceTickRequest,
    register_maintenance,
};
use crab_cell_runtime::primitives::workflow::{
    ActivityContext, ActivityExecution, ActivityHandler, ActivityRunOutcome,
    BlockingActivityHandler, WorkflowAction, WorkflowActivityClaimCommand,
    WorkflowActivityCompleteCommand, WorkflowActivityExtendCommand, WorkflowActivityValidateQuery,
    WorkflowCancelCommand, WorkflowContext, WorkflowControlCommand, WorkflowDecision,
    WorkflowDefinition, WorkflowGetQuery, WorkflowOutcome, WorkflowSignal, WorkflowSignalCommand,
    WorkflowStartCommand, WorkflowStatus, install_workflow_schema, register_activity,
    register_blocking_activity, register_workflow, register_workflow_activities,
};
use crab_cell_runtime::primitives::workflow::{
    WorkflowActivityModule, WorkflowModule, WorkflowNamespace,
};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, ModuleDescriptor, NamespaceDescriptor, RegistryBuilder,
};
use crab_cell_runtime::registry::{MigrationDescriptor, OperationDescriptor};
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Limits};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

use crate::support::fencing::fence_session;
use crate::support::fixtures::mutation_identity;

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
const WORKFLOW_MIGRATION: &str = include_str!("../../src/migrations/workflow.sql");
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
    operation(8, 1024 * 1024, 64),
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
        if let Some(path) = event.strip_prefix(b"publish-report\0") {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Running,
                state: b"report-pending".to_vec(),
                result: None,
                actions: vec![WorkflowAction::Activity {
                    activity_type: "publish-report".into(),
                    input: path.to_vec(),
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
    const CONTROL_COMMAND_ID: u32 = 8;
    const GET_QUERY_ID: u32 = 1;
}

impl WorkflowActivityModule for TestWorkflow {
    const ACTIVITY_TYPES: &'static [&'static str] = &["blocking-echo", "echo", "publish-report"];
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

struct PublishReportActivity;

impl ActivityHandler for PublishReportActivity {
    const TYPE: &'static str = "publish-report";

    fn execute(
        context: ActivityContext,
        input: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = ActivityExecution> + Send + 'static>> {
        Box::pin(async move {
            let key = context.idempotency_key();
            let report = format!("build report\nrequest={key:02x?}\n").into_bytes();
            let written = tokio::task::spawn_blocking(move || {
                let path = std::path::PathBuf::from(
                    String::from_utf8(input).map_err(|error| error.to_string())?,
                );
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(mut file) => file.write_all(&report).map_err(|error| error.to_string()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let existing = std::fs::read(path).map_err(|error| error.to_string())?;
                        if existing == report {
                            Ok(())
                        } else {
                            Err("report identity conflict".into())
                        }
                    }
                    Err(error) => Err(error.to_string()),
                }
            })
            .await;
            match written {
                Ok(Ok(())) if context.attempt() == 1 => ActivityExecution::Failed {
                    details: b"response lost after report publication".to_vec(),
                    retryable: true,
                },
                Ok(Ok(())) => ActivityExecution::Completed(key.to_vec()),
                Ok(Err(error)) => ActivityExecution::Failed {
                    details: error.into_bytes(),
                    retryable: false,
                },
                Err(error) => ActivityExecution::Failed {
                    details: error.to_string().into_bytes(),
                    retryable: true,
                },
            }
        })
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
            activity_types: &["blocking-echo", "echo", "publish-report"],
            namespaces: &WORKFLOW_NAMESPACES,
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_workflow::<Self>(registry)?;
        register_workflow_activities::<Self>(registry)?;
        register_activity::<Self, EchoActivity>(registry)?;
        register_activity::<Self, PublishReportActivity>(registry)?;
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
            activity_types: &["blocking-echo", "echo", "publish-report"],
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
        registry.bind_command::<WorkflowControlCommand<TestWorkflow>>()?;
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

mod activity;
mod namespace;
mod retry;
