use std::{sync::Arc, time::UNIX_EPOCH};

use crab_cell_runtime::{
    ApplicationId, BuildDescriptor, CatalogEntry, CatalogRole, CellAuthority, CellCatalog,
    CellClient, CellModule, CellRuntime, CellTarget, Digest, Error, IncarnationId, InvocationError,
    MigrationDescriptor, ModuleDescriptor, MutationIdentity, NamespaceDescriptor, NamespaceId,
    OperationDescriptor, Owner, RegistryBuilder, RequestId, SessionId, SqlWorkerPool, TenantId,
    WorkflowCancelCommand, WorkflowContext, WorkflowDecision, WorkflowDefinition, WorkflowGetQuery,
    WorkflowModule, WorkflowNamespace, WorkflowOutcome, WorkflowSignal, WorkflowSignalCommand,
    WorkflowStartCommand, WorkflowStatus, install_workflow_schema, register_workflow,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

const WORKFLOW_MODULE: &str = "workflow-api-test";
const WORKFLOW_NAMESPACE: NamespaceId = NamespaceId::from_bytes([8; 16]);
const WORKFLOW_MIGRATION: &str = include_str!("../src/migrations/workflow.sql");
const DEFINITION_DIGEST: Digest = Digest::from_bytes([6; 32]);
const COMMANDS: &[OperationDescriptor] = &[
    operation(1, 1024 * 1024, 64),
    operation(2, 1024 * 1024, 64),
    operation(3, 1024 * 1024, 64),
];
const QUERIES: &[OperationDescriptor] = &[operation(1, 2048, 1024 * 1024)];

struct Definition;

static DEFINITION: Definition = Definition;

impl WorkflowDefinition for Definition {
    fn digest(&self) -> Digest {
        DEFINITION_DIGEST
    }

    fn transition(
        &self,
        _state: &[u8],
        event: &[u8],
        _context: WorkflowContext,
    ) -> crab_cell_runtime::Result<WorkflowDecision> {
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

struct TestWorkflow;

impl WorkflowModule for TestWorkflow {
    const MODULE: &'static str = WORKFLOW_MODULE;
    const NAMESPACE: NamespaceId = WORKFLOW_NAMESPACE;
    const DEFINITION: &'static dyn WorkflowDefinition = &DEFINITION;
    const START_COMMAND_ID: u32 = 1;
    const SIGNAL_COMMAND_ID: u32 = 2;
    const CANCEL_COMMAND_ID: u32 = 3;
    const GET_QUERY_ID: u32 = 1;
}

impl CellModule for TestWorkflow {
    const NAME: &'static str = WORKFLOW_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<ModuleDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: WORKFLOW_MODULE,
            source_digest: Digest::from_bytes([4; 32]),
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: WORKFLOW_MIGRATION,
                digest: Digest::from_bytes(*blake3::hash(WORKFLOW_MIGRATION.as_bytes()).as_bytes()),
            }])),
            commands: COMMANDS,
            queries: QUERIES,
            workflow_definitions: &[DEFINITION_DIGEST],
            activity_types: &[],
            namespaces: &[NamespaceDescriptor {
                id: WORKFLOW_NAMESPACE,
                name: WORKFLOW_MODULE,
                role: CatalogRole::Workflow,
                shards: 1,
                effect_targets: &[],
                dead_letter: None,
            }],
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_workflow::<Self>(registry)
    }
}

struct MissingDefinitionBinding;

impl CellModule for MissingDefinitionBinding {
    const NAME: &'static str = WORKFLOW_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        TestWorkflow.descriptor()
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_command::<WorkflowStartCommand<TestWorkflow>>()?;
        registry.bind_command::<WorkflowSignalCommand<TestWorkflow>>()?;
        registry.bind_command::<WorkflowCancelCommand<TestWorkflow>>()?;
        registry.bind_query::<WorkflowGetQuery<TestWorkflow>>()
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
fn registry_rejects_a_declared_workflow_without_its_transition_binding() {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "workflow-api-test".into(),
        cargo_lock_digest: Digest::from_bytes([5; 32]),
    });
    builder.register(MissingDefinitionBinding).unwrap();
    assert!(matches!(
        builder.finish(),
        Err(Error::Registry(
            "descriptor and workflow definition bindings differ"
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
    let workflows = WorkflowNamespace::<TestWorkflow>::new(
        CellClient::local(registry.clone(), handle.clone()),
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
