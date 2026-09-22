use std::{future::Future, pin::Pin, sync::Arc, sync::OnceLock, time::UNIX_EPOCH};

use crab_cell_app::{ApplicationBuilder, ApplicationHandle, CellApplication, CellType};
use crab_cell_runtime::{
    ActivityContext, ActivityExecution, ActivityHandler, ActivityRunOutcome, ApplicationId,
    BlobArtifactStore, BlobCondition, BlobModule, BlobMutation, BlobMutationOutcome, BlobQuery,
    BlobQueryResult, BuildDescriptor, CatalogEntry, CatalogRole, CellAuthority, CellClient,
    CellHandle, CellModule, CellRuntime, CellStorageLayout, CellTarget, CronModule, CronMutation,
    CronQueryResult, CronTarget, Digest, EffectClaimRequest, EffectLeaseOutcome, EffectModule,
    Error, FencedNodeSession, IncarnationId, InvocationError, KvAtomicCommand, KvAtomicRequest,
    KvGetQuery, KvGetRequest, KvModule, KvMutation, MaintenanceModule, ModuleDescriptor,
    MutationIdentity, NamespaceDescriptor, NamespaceId, NodeAdvertisement, NodeCapacity,
    NodeDirectory, NodeFailureDomain, NodeId, OperationDescriptor, Owner, QualificationExecution,
    QualificationOperation, QualificationOperationExecutor, QualificationProfile,
    QualificationWorkload, QueueClaimRequest, QueueDeadLetterTarget, QueueLeaseOutcome,
    QueueModule, QueueSendRequest, Registry, RegistryBuilder, RequestId, Result, SqlBatch,
    SqlModule, SqlStatement, SqlValue, SqlWorkerPool, TenantId, WorkflowAction,
    WorkflowActivityModule, WorkflowContext, WorkflowDecision, WorkflowDefinition, WorkflowModule,
    WorkflowStatus, install_blob_schema, install_cron_schema, install_kv_schema,
    install_queue_schema, install_workflow_schema, partition_for_shard, register_activity,
    register_blob, register_cron, register_effect_delivery, register_kv, register_maintenance,
    register_queue, register_sql, register_workflow, register_workflow_activities,
};
use crab_ltx::{CellReplica, DiskBudget, Host, Limits};
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use object_store::memory::InMemory;

const SQL_NAMESPACE: NamespaceId = NamespaceId::from_bytes([1; 16]);
const KV_NAMESPACE: NamespaceId = NamespaceId::from_bytes([2; 16]);
const BLOB_NAMESPACE: NamespaceId = NamespaceId::from_bytes([3; 16]);
const QUEUE_NAMESPACE: NamespaceId = NamespaceId::from_bytes([4; 16]);
const DEAD_LETTER_NAMESPACE: NamespaceId = NamespaceId::from_bytes([5; 16]);
const CRON_NAMESPACE: NamespaceId = NamespaceId::from_bytes([6; 16]);
const WORKFLOW_NAMESPACE: NamespaceId = NamespaceId::from_bytes([7; 16]);
const WORKFLOW_DIGEST: Digest = Digest::from_bytes([8; 32]);

const SQL_MODULE: &str = "reference-sql";
const KV_MODULE: &str = "reference-kv";
const BLOB_MODULE: &str = "reference-blob";
const QUEUE_MODULE: &str = "reference-queue";
const DEAD_LETTER_MODULE: &str = "reference-dead-letter";
const CRON_MODULE: &str = "reference-cron";
const WORKFLOW_MODULE: &str = "reference-workflow";

fn operation(id: u32) -> OperationDescriptor {
    OperationDescriptor {
        id,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 1 << 20,
        output_limit: 1 << 20,
    }
}

fn migration() -> &'static [crab_cell_runtime::MigrationDescriptor] {
    static MIGRATION: OnceLock<&'static [crab_cell_runtime::MigrationDescriptor]> = OnceLock::new();
    MIGRATION.get_or_init(|| {
        Box::leak(Box::new([crab_cell_runtime::MigrationDescriptor {
            version: 1,
            sql: "-- reference application migration v1",
            digest: Digest::from_bytes(
                *blake3::hash(b"-- reference application migration v1").as_bytes(),
            ),
        }]))
    })
}

fn descriptor(
    module: &'static str,
    commands: &'static [u32],
    queries: &'static [u32],
    namespaces: &'static [NamespaceDescriptor],
    workflows: &'static [Digest],
    activities: &'static [&'static str],
) -> &'static ModuleDescriptor {
    static DESCRIPTORS: OnceLock<std::sync::Mutex<Vec<&'static ModuleDescriptor>>> =
        OnceLock::new();
    let descriptors = DESCRIPTORS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut descriptors = descriptors.lock().expect("reference descriptor lock");
    if let Some(descriptor) = descriptors
        .iter()
        .copied()
        .find(|descriptor| descriptor.name == module)
    {
        return descriptor;
    }
    let command_descriptors = Box::leak(
        commands
            .iter()
            .copied()
            .map(operation)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let query_descriptors = Box::leak(
        queries
            .iter()
            .copied()
            .map(operation)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let descriptor = Box::leak(Box::new(ModuleDescriptor {
        name: module,
        source_digest: Digest::from_bytes(*blake3::hash(module.as_bytes()).as_bytes()),
        retained_codes: &[],
        schema_min: 1,
        schema_max: 1,
        migrations: migration(),
        commands: command_descriptors,
        queries: query_descriptors,
        workflow_definitions: workflows,
        activity_types: activities,
        namespaces,
    }));
    descriptors.push(descriptor);
    descriptor
}

struct ReferenceSql;
impl SqlModule for ReferenceSql {
    const MODULE: &'static str = SQL_MODULE;
    const BATCH_COMMAND_ID: u32 = 1;
    const BATCH_QUERY_ID: u32 = 2;
}
impl EffectModule for ReferenceSql {
    const MODULE: &'static str = SQL_MODULE;
    const CLAIM_COMMAND_ID: u32 = 3;
    const LEASE_COMMAND_ID: u32 = 4;
    const VALIDATE_QUERY_ID: u32 = 5;
}
impl CellModule for ReferenceSql {
    const NAME: &'static str = SQL_MODULE;
    fn descriptor(&self) -> &'static ModuleDescriptor {
        static NAMESPACES: &[NamespaceDescriptor] = &[NamespaceDescriptor {
            id: SQL_NAMESPACE,
            name: "reference-sql",
            role: CatalogRole::Sql,
            shards: 1,
            effect_targets: &[],
            dead_letter: None,
        }];
        descriptor(SQL_MODULE, &[1, 3, 4], &[2, 5], NAMESPACES, &[], &[])
    }
    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_sql::<Self>(registry)?;
        register_effect_delivery::<Self>(registry)
    }
}

struct ReferenceKv;
impl KvModule for ReferenceKv {
    const MODULE: &'static str = KV_MODULE;
    const ATOMIC_COMMAND_ID: u32 = 1;
    const GET_QUERY_ID: u32 = 2;
    const LIST_QUERY_ID: u32 = 3;
}

struct UnregisteredEffects;
impl EffectModule for UnregisteredEffects {
    const MODULE: &'static str = KV_MODULE;
    const CLAIM_COMMAND_ID: u32 = 20;
    const LEASE_COMMAND_ID: u32 = 21;
    const VALIDATE_QUERY_ID: u32 = 22;
}
impl CellModule for ReferenceKv {
    const NAME: &'static str = KV_MODULE;
    fn descriptor(&self) -> &'static ModuleDescriptor {
        static NAMESPACES: &[NamespaceDescriptor] = &[NamespaceDescriptor {
            id: KV_NAMESPACE,
            name: "reference-kv",
            role: CatalogRole::Kv,
            shards: 1,
            effect_targets: &[],
            dead_letter: None,
        }];
        descriptor(KV_MODULE, &[1], &[2, 3], NAMESPACES, &[], &[])
    }
    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_kv::<Self>(registry)
    }
}

struct ReferenceBlob;
impl MaintenanceModule for ReferenceBlob {
    const MODULE: &'static str = BLOB_MODULE;
    const TICK_COMMAND_ID: u32 = 3;
}
impl BlobModule for ReferenceBlob {
    const NAMESPACE: NamespaceId = BLOB_NAMESPACE;
    const MUTATE_COMMAND_ID: u32 = 1;
    const QUERY_ID: u32 = 2;
}
impl CellModule for ReferenceBlob {
    const NAME: &'static str = BLOB_MODULE;
    fn descriptor(&self) -> &'static ModuleDescriptor {
        static NAMESPACES: &[NamespaceDescriptor] = &[NamespaceDescriptor {
            id: BLOB_NAMESPACE,
            name: "reference-blob",
            role: CatalogRole::Blob,
            shards: 1,
            effect_targets: &[],
            dead_letter: None,
        }];
        descriptor(BLOB_MODULE, &[1, 3], &[2], NAMESPACES, &[], &[])
    }
    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_blob::<Self>(registry)
    }
}

struct ReferenceQueue;
impl MaintenanceModule for ReferenceQueue {
    const MODULE: &'static str = QUEUE_MODULE;
    const TICK_COMMAND_ID: u32 = 7;
    const QUEUE_DEAD_LETTER: Option<QueueDeadLetterTarget> = Some(QueueDeadLetterTarget::new(
        DEAD_LETTER_MODULE,
        DEAD_LETTER_NAMESPACE,
        1,
        1,
        1,
    ));
}
impl QueueModule for ReferenceQueue {
    const NAMESPACE: NamespaceId = QUEUE_NAMESPACE;
    const SEND_COMMAND_ID: u32 = 1;
    const CLAIM_COMMAND_ID: u32 = 2;
    const LEASE_COMMAND_ID: u32 = 3;
    const VALIDATE_QUERY_ID: u32 = 4;
    const CONTROL_COMMAND_ID: u32 = 5;
    const INFO_QUERY_ID: u32 = 6;
}
impl CellModule for ReferenceQueue {
    const NAME: &'static str = QUEUE_MODULE;
    fn descriptor(&self) -> &'static ModuleDescriptor {
        static NAMESPACES: &[NamespaceDescriptor] = &[NamespaceDescriptor {
            id: QUEUE_NAMESPACE,
            name: "reference-queue",
            role: CatalogRole::Queue,
            shards: 1,
            effect_targets: &[DEAD_LETTER_NAMESPACE],
            dead_letter: Some(DEAD_LETTER_NAMESPACE),
        }];
        descriptor(
            QUEUE_MODULE,
            &[1, 2, 3, 5, 7],
            &[4, 6],
            NAMESPACES,
            &[],
            &[],
        )
    }
    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_queue::<Self>(registry)
    }
}

struct ReferenceDeadLetter;
impl MaintenanceModule for ReferenceDeadLetter {
    const MODULE: &'static str = DEAD_LETTER_MODULE;
    const TICK_COMMAND_ID: u32 = 2;
}
impl QueueModule for ReferenceDeadLetter {
    const NAMESPACE: NamespaceId = DEAD_LETTER_NAMESPACE;
    const SEND_COMMAND_ID: u32 = 1;
    const CLAIM_COMMAND_ID: u32 = 3;
    const LEASE_COMMAND_ID: u32 = 4;
    const VALIDATE_QUERY_ID: u32 = 5;
    const CONTROL_COMMAND_ID: u32 = 6;
    const INFO_QUERY_ID: u32 = 7;
}
impl CellModule for ReferenceDeadLetter {
    const NAME: &'static str = DEAD_LETTER_MODULE;
    fn descriptor(&self) -> &'static ModuleDescriptor {
        static NAMESPACES: &[NamespaceDescriptor] = &[NamespaceDescriptor {
            id: DEAD_LETTER_NAMESPACE,
            name: "reference-dead-letter",
            role: CatalogRole::Queue,
            shards: 1,
            effect_targets: &[],
            dead_letter: None,
        }];
        descriptor(
            DEAD_LETTER_MODULE,
            &[1, 2, 3, 4, 6],
            &[5, 7],
            NAMESPACES,
            &[],
            &[],
        )
    }
    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_queue::<Self>(registry)
    }
}

struct ReferenceCron;
impl MaintenanceModule for ReferenceCron {
    const MODULE: &'static str = CRON_MODULE;
    const TICK_COMMAND_ID: u32 = 3;
    const CRON_TARGETS: &'static [CronTarget] =
        &[CronTarget::new(SQL_MODULE, SQL_NAMESPACE, 1, 1, 1 << 20)];
}
impl CronModule for ReferenceCron {
    const NAMESPACE: NamespaceId = CRON_NAMESPACE;
    const MUTATE_COMMAND_ID: u32 = 1;
    const QUERY_ID: u32 = 2;
}
impl CellModule for ReferenceCron {
    const NAME: &'static str = CRON_MODULE;
    fn descriptor(&self) -> &'static ModuleDescriptor {
        static NAMESPACES: &[NamespaceDescriptor] = &[NamespaceDescriptor {
            id: CRON_NAMESPACE,
            name: "reference-cron",
            role: CatalogRole::Cron,
            shards: 1,
            effect_targets: &[SQL_NAMESPACE],
            dead_letter: None,
        }];
        descriptor(CRON_MODULE, &[1, 3], &[2], NAMESPACES, &[], &[])
    }
    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_cron::<Self>(registry)
    }
}

struct ReferenceWorkflow;
struct ReferenceDefinition;
impl WorkflowDefinition for ReferenceDefinition {
    fn digest(&self) -> Digest {
        WORKFLOW_DIGEST
    }
    fn effect_targets(&self) -> &'static [NamespaceId] {
        &[SQL_NAMESPACE]
    }
    fn transition(
        &self,
        state: &[u8],
        event: &[u8],
        context: WorkflowContext,
    ) -> Result<WorkflowDecision> {
        if event == b"activity" {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Running,
                state: b"activity-pending".to_vec(),
                result: None,
                actions: vec![WorkflowAction::Activity {
                    activity_type: "reference-activity".into(),
                    input: b"activity-result".to_vec(),
                    due_at_ms: context.now_ms(),
                    expires_at_ms: context.now_ms() + 60_000,
                }],
            });
        }
        if event == b"effect" {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Completed,
                state: b"effect-published".to_vec(),
                result: Some(b"effect-scheduled".to_vec()),
                actions: vec![WorkflowAction::Effect {
                    intent: crab_cell_runtime::EffectCommandIntent {
                        target: CellTarget::new(
                            context.source().tenant(),
                            context.source().application(),
                            SQL_NAMESPACE,
                            &partition_for_shard(0),
                        )?,
                        command_id: ReferenceSql::BATCH_COMMAND_ID,
                        codec_version: <ReferenceSql as SqlModule>::CODEC_VERSION,
                        input: b"qualification-effect".to_vec(),
                        expires_at_ms: context.now_ms() + 60_000,
                    },
                }],
            });
        }
        if event.starts_with(b"activity\0") {
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Completed,
                state: event.to_vec(),
                result: Some(event.to_vec()),
                actions: Vec::new(),
            });
        }
        Ok(WorkflowDecision {
            status: WorkflowStatus::Completed,
            state: state.to_vec(),
            result: Some(event.to_vec()),
            actions: Vec::new(),
        })
    }
}
static REFERENCE_DEFINITION: ReferenceDefinition = ReferenceDefinition;
static REFERENCE_DEFINITIONS: &[&'static dyn WorkflowDefinition] = &[&REFERENCE_DEFINITION];
static REFERENCE_ACTIVITY_TYPES: &[&str] = &["reference-activity"];

struct ReferenceActivity;
impl ActivityHandler for ReferenceActivity {
    const TYPE: &'static str = "reference-activity";
    fn execute(
        _context: ActivityContext,
        input: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = ActivityExecution> + Send + 'static>> {
        Box::pin(async move { ActivityExecution::Completed(input) })
    }
}
impl MaintenanceModule for ReferenceWorkflow {
    const MODULE: &'static str = WORKFLOW_MODULE;
    const TICK_COMMAND_ID: u32 = 9;
    const WORKFLOW_DEFINITIONS: &'static [&'static dyn WorkflowDefinition] = REFERENCE_DEFINITIONS;
}
impl WorkflowModule for ReferenceWorkflow {
    const MODULE: &'static str = WORKFLOW_MODULE;
    const NAMESPACE: NamespaceId = WORKFLOW_NAMESPACE;
    const CURRENT_DEFINITION: &'static dyn WorkflowDefinition = &REFERENCE_DEFINITION;
    const DEFINITIONS: &'static [&'static dyn WorkflowDefinition] = REFERENCE_DEFINITIONS;
    const START_COMMAND_ID: u32 = 1;
    const SIGNAL_COMMAND_ID: u32 = 2;
    const CANCEL_COMMAND_ID: u32 = 3;
    const CONTROL_COMMAND_ID: u32 = 4;
    const GET_QUERY_ID: u32 = 5;
}
impl WorkflowActivityModule for ReferenceWorkflow {
    const ACTIVITY_TYPES: &'static [&'static str] = REFERENCE_ACTIVITY_TYPES;
    const ACTIVITY_CLAIM_COMMAND_ID: u32 = 6;
    const ACTIVITY_COMPLETE_COMMAND_ID: u32 = 7;
    const ACTIVITY_EXTEND_COMMAND_ID: u32 = 8;
    const ACTIVITY_VALIDATE_QUERY_ID: u32 = 10;
}
impl EffectModule for ReferenceWorkflow {
    const MODULE: &'static str = WORKFLOW_MODULE;
    const CLAIM_COMMAND_ID: u32 = 11;
    const LEASE_COMMAND_ID: u32 = 12;
    const VALIDATE_QUERY_ID: u32 = 13;
}
impl CellModule for ReferenceWorkflow {
    const NAME: &'static str = WORKFLOW_MODULE;
    fn descriptor(&self) -> &'static ModuleDescriptor {
        static NAMESPACES: &[NamespaceDescriptor] = &[NamespaceDescriptor {
            id: WORKFLOW_NAMESPACE,
            name: "reference-workflow",
            role: CatalogRole::Workflow,
            shards: 1,
            effect_targets: &[SQL_NAMESPACE],
            dead_letter: None,
        }];
        descriptor(
            WORKFLOW_MODULE,
            &[1, 2, 3, 4, 6, 7, 8, 9, 11, 12],
            &[5, 10, 13],
            NAMESPACES,
            &[WORKFLOW_DIGEST],
            REFERENCE_ACTIVITY_TYPES,
        )
    }
    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_workflow::<Self>(registry)?;
        register_workflow_activities::<Self>(registry)?;
        register_activity::<Self, ReferenceActivity>(registry)?;
        register_effect_delivery::<Self>(registry)?;
        register_maintenance::<Self>(registry)
    }
}

struct ReferenceApplication;
impl CellApplication for ReferenceApplication {
    const NAME: &'static str = "reference-application";

    fn register(builder: &mut ApplicationBuilder) -> Result<()> {
        builder.register(ReferenceSql)?;
        builder.register(ReferenceKv)?;
        builder.register(ReferenceBlob)?;
        builder.register(ReferenceQueue)?;
        builder.register(ReferenceDeadLetter)?;
        builder.register(ReferenceCron)?;
        builder.register(ReferenceWorkflow)?;
        for (module, name, namespace, role) in [
            (SQL_MODULE, "sql", SQL_NAMESPACE, CatalogRole::Sql),
            (KV_MODULE, "kv", KV_NAMESPACE, CatalogRole::Kv),
            (BLOB_MODULE, "blob", BLOB_NAMESPACE, CatalogRole::Blob),
            (QUEUE_MODULE, "queue", QUEUE_NAMESPACE, CatalogRole::Queue),
            (
                DEAD_LETTER_MODULE,
                "dead-letter",
                DEAD_LETTER_NAMESPACE,
                CatalogRole::Queue,
            ),
            (CRON_MODULE, "cron", CRON_NAMESPACE, CatalogRole::Cron),
            (
                WORKFLOW_MODULE,
                "workflow",
                WORKFLOW_NAMESPACE,
                CatalogRole::Workflow,
            ),
        ] {
            builder.cell_type(CellType::new(module, name, namespace, role, 1)?)?;
        }
        Ok(())
    }
}

fn compiled() -> crab_cell_app::CompiledApplication {
    ReferenceApplication::compile(BuildDescriptor {
        source_revision: "reference-source".into(),
        cargo_lock_digest: Digest::from_bytes([42; 32]),
    })
    .expect("reference application compiles")
}

fn compile_reference_in_order(reverse: bool) -> crab_cell_app::CompiledApplication {
    let mut builder = ApplicationBuilder::new(
        ReferenceApplication::NAME,
        BuildDescriptor {
            source_revision: "reference-source".into(),
            cargo_lock_digest: Digest::from_bytes([42; 32]),
        },
    )
    .unwrap();
    if reverse {
        builder.register(ReferenceWorkflow).unwrap();
        builder.register(ReferenceCron).unwrap();
        builder.register(ReferenceDeadLetter).unwrap();
        builder.register(ReferenceQueue).unwrap();
        builder.register(ReferenceBlob).unwrap();
        builder.register(ReferenceKv).unwrap();
        builder.register(ReferenceSql).unwrap();
    } else {
        builder.register(ReferenceSql).unwrap();
        builder.register(ReferenceKv).unwrap();
        builder.register(ReferenceBlob).unwrap();
        builder.register(ReferenceQueue).unwrap();
        builder.register(ReferenceDeadLetter).unwrap();
        builder.register(ReferenceCron).unwrap();
        builder.register(ReferenceWorkflow).unwrap();
    }
    for (module, name, namespace, role) in [
        (SQL_MODULE, "sql", SQL_NAMESPACE, CatalogRole::Sql),
        (KV_MODULE, "kv", KV_NAMESPACE, CatalogRole::Kv),
        (BLOB_MODULE, "blob", BLOB_NAMESPACE, CatalogRole::Blob),
        (QUEUE_MODULE, "queue", QUEUE_NAMESPACE, CatalogRole::Queue),
        (
            DEAD_LETTER_MODULE,
            "dead-letter",
            DEAD_LETTER_NAMESPACE,
            CatalogRole::Queue,
        ),
        (CRON_MODULE, "cron", CRON_NAMESPACE, CatalogRole::Cron),
        (
            WORKFLOW_MODULE,
            "workflow",
            WORKFLOW_NAMESPACE,
            CatalogRole::Workflow,
        ),
    ] {
        builder
            .cell_type(CellType::new(module, name, namespace, role, 1).unwrap())
            .unwrap();
    }
    builder.finish().unwrap()
}

#[test]
fn application_descriptor_is_stable_when_modules_register_in_reverse_order() {
    let forward = compile_reference_in_order(false);
    let reverse = compile_reference_in_order(true);
    assert_eq!(forward.descriptor_bytes(), reverse.descriptor_bytes());
    assert_eq!(forward.descriptor_digest(), reverse.descriptor_digest());
}

#[test]
fn reference_application_registers_every_primitive_and_relationship() {
    let application = compiled();
    assert_eq!(application.cell_types().len(), 7);
    assert!(application.registry().has_effect_runner(SQL_NAMESPACE));
    assert_eq!(
        application
            .registry()
            .namespace_contract(QUEUE_NAMESPACE)
            .unwrap()
            .1
            .dead_letter,
        Some(DEAD_LETTER_NAMESPACE)
    );
    assert_eq!(
        application
            .registry()
            .namespace_contract(WORKFLOW_NAMESPACE)
            .unwrap()
            .1
            .effect_targets,
        &[SQL_NAMESPACE]
    );
    assert!(!application.descriptor_bytes().is_empty());
}

#[allow(dead_code)]
fn typed_capability_surface<A: CellApplication>(
    handle: &ApplicationHandle<A>,
    target: CellTarget,
) -> Result<()> {
    let _sql = handle.sql::<ReferenceSql>(target.clone())?;
    let _kv = handle.kv::<ReferenceKv>(KV_NAMESPACE)?;
    let _blob = handle.blob::<ReferenceBlob>()?;
    let _queue = handle.queue::<ReferenceQueue>()?;
    let _cron = handle.cron::<ReferenceCron>()?;
    let _workflow = handle.workflow::<ReferenceWorkflow>()?;
    let _activities = handle.activities::<ReferenceWorkflow>()?;
    let _effects = handle.effects::<ReferenceSql>(target)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reference_application_uses_typed_handle_for_a_real_commit() {
    let application = Arc::new(compiled());
    let tenant = TenantId::from_bytes([21; 16]);
    let application_id = crab_cell_runtime::ApplicationId::from_bytes([22; 16]);
    let target = CellTarget::new(
        tenant,
        application_id,
        SQL_NAMESPACE,
        &crab_cell_runtime::partition_for_shard(0),
    )
    .unwrap();
    let incarnation = IncarnationId::from_bytes([23; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(
        store.clone(),
        object_store::path::Path::from("reference-application"),
        *application_id.as_bytes(),
    );
    let catalog = crab_cell_runtime::CellCatalog::new(layout.clone(), tenant);
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Sql,
                application.registry().module_code(SQL_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let session = crab_cell_runtime::SessionId::from_bytes([24; 16]);
    let observed = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: "https://reference.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(1, 4).unwrap(),
        16 * 1024 * 1024,
        session,
        reference_host(),
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            CellReplica::new(
                layout,
                *target.cell_id().as_bytes(),
                *incarnation.as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            authority,
            observed,
            directory.path().join("reference.sqlite"),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let client = CellClient::local(application.registry(), handle);
    let typed =
        ApplicationHandle::<ReferenceApplication>::new(client, application, tenant, application_id)
            .with_blob_artifact_store(BlobArtifactStore::new(store));
    typed_capability_surface(&typed, target.clone()).unwrap();
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let sql = typed.sql::<ReferenceSql>(target).unwrap();
    let committed = sql
        .batch(
            MutationIdentity {
                request_id: RequestId::from_bytes([25; 16]),
                issued_at_ms: now_ms,
                expires_at_ms: now_ms + 60_000,
            },
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT ?1".into(),
                    parameters: vec![SqlValue::Integer(7)],
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(committed.output[0].rows.len(), 1);
    runtime.shutdown().await.unwrap();
}

#[expect(
    clippy::too_many_arguments,
    reason = "the qualification fixture keeps each Cell contract explicit"
)]
async fn bootstrap_reference_cell<F>(
    runtime: &CellRuntime,
    registry: &Arc<Registry>,
    layout: &CellStorageLayout,
    directory: &tempfile::TempDir,
    tenant: TenantId,
    application: ApplicationId,
    namespace: NamespaceId,
    role: CatalogRole,
    module: &'static str,
    incarnation_byte: u8,
    initialize: F,
) -> crab_cell_runtime::Result<CellHandle>
where
    F: for<'connection> FnOnce(
            &crab_ltx::rusqlite::Transaction<'connection>,
        ) -> crab_cell_runtime::Result<()>
        + Send
        + 'static,
{
    let target = CellTarget::new(tenant, application, namespace, &partition_for_shard(0))?;
    let catalog = crab_cell_runtime::CellCatalog::new(layout.clone(), tenant);
    let proof = catalog
        .provision(CatalogEntry::new(
            &target,
            role,
            registry
                .module_code(module)
                .ok_or(crab_cell_runtime::Error::Registry("module code is missing"))?,
            1,
        )?)
        .await?;
    let authority = CellAuthority::new(layout.clone());
    let incarnation = IncarnationId::from_bytes([incarnation_byte; 16]);
    let session = crab_cell_runtime::SessionId::from_bytes([24; 16]);
    let observed = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: format!("https://{module}.internal:8081"),
            },
        )
        .await?;
    runtime
        .bootstrap(
            proof,
            CellReplica::new(
                layout.clone(),
                *target.cell_id().as_bytes(),
                *incarnation.as_bytes(),
                Limits::default(),
            )?,
            authority,
            observed,
            directory.path().join(format!("{module}.sqlite")),
            initialize,
        )
        .await
}

fn reference_host() -> Host {
    Host::default().with_local_disk_budget(DiskBudget::new(1 << 30))
}

async fn seed_reference_session(layout: &CellStorageLayout, session: crab_cell_runtime::SessionId) {
    let directory = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        Digest::from_bytes([92; 32]),
    );
    let key = SigningKey::from_bytes(&[93; 32]);
    directory
        .create(
            NodeAdvertisement::sign(
                NodeId::from_bytes(*session.as_bytes()),
                session,
                "https://reference-expired.internal:8081".into(),
                directory.fleet(),
                Digest::from_bytes([94; 32]),
                Digest::from_bytes([91; 32]),
                Digest::from_bytes([92; 32]),
                &key,
                1,
                1,
                10_001,
                vec![Digest::from_bytes([95; 32])],
                vec![1],
                NodeFailureDomain::default(),
                NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            1,
        )
        .await
        .unwrap();
}

async fn fence_reference_session(
    layout: &CellStorageLayout,
    session: crab_cell_runtime::SessionId,
    claimant: crab_cell_runtime::SessionId,
) -> FencedNodeSession {
    let directory = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        Digest::from_bytes([92; 32]),
    );
    let key = SigningKey::from_bytes(&[93; 32]);
    directory
        .create(
            NodeAdvertisement::sign(
                NodeId::from_bytes(*claimant.as_bytes()),
                claimant,
                "https://reference-claimant.internal:8081".into(),
                directory.fleet(),
                Digest::from_bytes([94; 32]),
                Digest::from_bytes([91; 32]),
                Digest::from_bytes([92; 32]),
                &key,
                10_000,
                10_000,
                20_000,
                vec![Digest::from_bytes([95; 32])],
                vec![1],
                NodeFailureDomain::default(),
                NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    job_credits: 1,
                    ..NodeCapacity::default()
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

fn reference_identity(byte: u8, now_ms: i64) -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

fn qualification_identity(index: u64, now_ms: i64) -> MutationIdentity {
    let mut request_id = [0; 16];
    request_id[..8].copy_from_slice(&index.to_be_bytes());
    MutationIdentity {
        request_id: RequestId::from_bytes(request_id),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

struct TypedQualificationExecutor {
    handle: ApplicationHandle<ReferenceApplication>,
    tenant: TenantId,
    application: ApplicationId,
    now_ms: i64,
}

impl QualificationOperationExecutor for TypedQualificationExecutor {
    type Future<'a> = Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        let handle = self.handle.clone();
        let tenant = self.tenant;
        let application = self.application;
        let now_ms = self.now_ms;
        Box::pin(async move {
            let identity = qualification_identity(operation.index(), now_ms);
            match operation.primitive() {
                "sql" => {
                    let target = CellTarget::new(
                        tenant,
                        application,
                        SQL_NAMESPACE,
                        &partition_for_shard(0),
                    )?;
                    let sql = handle.sql::<ReferenceSql>(target)?;
                    sql.query(
                        None,
                        SqlBatch {
                            statements: vec![SqlStatement {
                                sql: "SELECT ?1".into(),
                                parameters: vec![SqlValue::Integer(operation.nonce() as i64)],
                            }],
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("qualification SQL invocation failed"))?;
                }
                "kv" => {
                    let kv = handle.kv::<ReferenceKv>(KV_NAMESPACE)?;
                    kv.atomic(
                        identity,
                        KvAtomicRequest {
                            scope: b"qualification-driver".to_vec(),
                            checks: Vec::new(),
                            mutations: vec![KvMutation::Put {
                                key: operation.index().to_be_bytes().to_vec(),
                                value: operation.nonce().to_be_bytes().to_vec(),
                                expires_at_ms: None,
                            }],
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("qualification KV invocation failed"))?;
                }
                "blob" => {
                    let blob = handle.blob::<ReferenceBlob>()?;
                    blob.query(
                        BlobQuery::Read {
                            key: b"qualification/blob".to_vec(),
                            offset: 0,
                            limit: 128,
                        },
                        None,
                    )
                    .await
                    .map_err(|_| Error::Control("qualification Blob invocation failed"))?;
                }
                "queue" => {
                    let queue = handle.queue::<ReferenceQueue>()?;
                    queue
                        .info(0, None)
                        .await
                        .map_err(|_| Error::Control("qualification Queue invocation failed"))?;
                }
                "cron" => {
                    let cron = handle.cron::<ReferenceCron>()?;
                    cron.get([58; 16], None)
                        .await
                        .map_err(|_| Error::Control("qualification Cron invocation failed"))?;
                }
                "workflow" => {
                    let workflow = handle.workflow::<ReferenceWorkflow>()?;
                    workflow
                        .state(b"effect-run".to_vec(), None)
                        .await
                        .map_err(|_| Error::Control("qualification Workflow invocation failed"))?;
                }
                "activity" => {
                    let activities = handle.activities::<ReferenceWorkflow>()?;
                    let supervisor = crab_cell_runtime::ActivitySupervisor::new(activities, 5_000)?;
                    supervisor
                        .run_once(0, None)
                        .await
                        .map_err(|_| Error::Control("qualification Activity invocation failed"))?;
                }
                "effects" => {
                    let target = CellTarget::new(
                        tenant,
                        application,
                        WORKFLOW_NAMESPACE,
                        &partition_for_shard(0),
                    )?;
                    let effects = handle.effects::<ReferenceWorkflow>(target)?;
                    effects
                        .claim(
                            identity,
                            EffectClaimRequest {
                                limit: 1,
                                lease_ms: 5_000,
                            },
                        )
                        .await
                        .map_err(|_| Error::Control("qualification Effects invocation failed"))?;
                }
                _ => return Err(Error::Control("qualification primitive is not registered")),
            }
            Ok(QualificationExecution::acknowledged(true))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_application_executes_every_primitive_through_a_local_router() {
    let application = Arc::new(compiled());
    let tenant = TenantId::from_bytes([31; 16]);
    let application_id = ApplicationId::from_bytes([32; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(
        store.clone(),
        object_store::path::Path::from("reference-primitive-qualification"),
        *application_id.as_bytes(),
    );
    let directory = tempfile::TempDir::new().unwrap();
    let session = crab_cell_runtime::SessionId::from_bytes([24; 16]);
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(4, 32).unwrap(),
        64 * 1024 * 1024,
        session,
        reference_host(),
    )
    .unwrap();
    let registry = application.registry();
    let handles = vec![
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            SQL_NAMESPACE,
            CatalogRole::Sql,
            SQL_MODULE,
            40,
            |_| Ok(()),
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            KV_NAMESPACE,
            CatalogRole::Kv,
            KV_MODULE,
            41,
            install_kv_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            BLOB_NAMESPACE,
            CatalogRole::Blob,
            BLOB_MODULE,
            42,
            install_blob_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            QUEUE_NAMESPACE,
            CatalogRole::Queue,
            QUEUE_MODULE,
            43,
            install_queue_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            DEAD_LETTER_NAMESPACE,
            CatalogRole::Queue,
            DEAD_LETTER_MODULE,
            44,
            install_queue_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            CRON_NAMESPACE,
            CatalogRole::Cron,
            CRON_MODULE,
            45,
            install_cron_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            WORKFLOW_NAMESPACE,
            CatalogRole::Workflow,
            WORKFLOW_MODULE,
            46,
            install_workflow_schema,
        )
        .await
        .unwrap(),
    ];
    let client = CellClient::local_many(registry.clone(), handles).unwrap();
    let typed = ApplicationHandle::<ReferenceApplication>::new(
        client,
        Arc::clone(&application),
        tenant,
        application_id,
    )
    .with_blob_artifact_store(BlobArtifactStore::new(store.clone()));
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();

    let sql_target = CellTarget::new(
        tenant,
        application_id,
        SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .unwrap();
    let queue_target = CellTarget::new(
        tenant,
        application_id,
        QUEUE_NAMESPACE,
        &partition_for_shard(0),
    )
    .unwrap();
    let wrong_command = typed
        .command::<KvAtomicCommand<ReferenceKv>>(
            &sql_target,
            reference_identity(49, now_ms),
            KvAtomicRequest {
                scope: b"wrong-module".to_vec(),
                checks: Vec::new(),
                mutations: Vec::new(),
            },
        )
        .await;
    assert!(matches!(
        wrong_command,
        Err(InvocationError::NotStarted(Error::Registry(
            "namespace module differs from capability"
        )))
    ));
    let wrong_prepared = typed
        .prepare_command::<KvAtomicCommand<ReferenceKv>>(
            &sql_target,
            reference_identity(50, now_ms),
            KvAtomicRequest {
                scope: b"wrong-module".to_vec(),
                checks: Vec::new(),
                mutations: Vec::new(),
            },
        )
        .await;
    assert!(matches!(
        wrong_prepared,
        Err(InvocationError::NotStarted(Error::Registry(
            "namespace module differs from capability"
        )))
    ));
    let wrong_query = typed
        .query::<KvGetQuery<ReferenceKv>>(
            &sql_target,
            None,
            KvGetRequest {
                scope: b"wrong-module".to_vec(),
                key: b"key".to_vec(),
            },
        )
        .await;
    assert!(matches!(
        wrong_query,
        Err(InvocationError::NotStarted(Error::Registry(
            "namespace module differs from capability"
        )))
    ));
    assert!(typed.sql::<ReferenceSql>(queue_target).is_err());
    assert!(typed.kv::<ReferenceKv>(SQL_NAMESPACE).is_err());
    assert!(
        typed
            .effects::<ReferenceWorkflow>(sql_target.clone())
            .is_err()
    );
    let foreign_target = CellTarget::new(
        TenantId::from_bytes([99; 16]),
        application_id,
        SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .unwrap();
    assert!(typed.sql::<ReferenceSql>(foreign_target).is_err());
    let invalid_partition = CellTarget::new(
        tenant,
        application_id,
        SQL_NAMESPACE,
        &partition_for_shard(1),
    )
    .unwrap();
    assert!(typed.sql::<ReferenceSql>(invalid_partition).is_err());
    let kv_target = CellTarget::new(
        tenant,
        application_id,
        KV_NAMESPACE,
        &partition_for_shard(0),
    )
    .unwrap();
    assert!(typed.effects::<UnregisteredEffects>(kv_target).is_err());
    let sql = typed.sql::<ReferenceSql>(sql_target.clone()).unwrap();
    let sql_result = sql
        .batch(
            reference_identity(50, now_ms),
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT ?1".into(),
                    parameters: vec![SqlValue::Integer(11)],
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(sql_result.output[0].rows.len(), 1);

    let kv = typed.kv::<ReferenceKv>(KV_NAMESPACE).unwrap();
    kv.atomic(
        reference_identity(51, now_ms),
        KvAtomicRequest {
            scope: b"qualification".to_vec(),
            checks: Vec::new(),
            mutations: vec![KvMutation::Put {
                key: b"key".to_vec(),
                value: b"value".to_vec(),
                expires_at_ms: None,
            }],
        },
    )
    .await
    .unwrap();
    let kv_value = kv
        .get(b"qualification".to_vec(), b"key".to_vec(), None)
        .await
        .unwrap();
    assert_eq!(kv_value.output.unwrap().value, b"value");

    let blob = typed.blob::<ReferenceBlob>().unwrap();
    let blob_key = b"qualification/blob".to_vec();
    let upload_id = [52; 16];
    blob.mutate(
        reference_identity(52, now_ms),
        BlobMutation::Begin {
            key: blob_key.clone(),
            upload_id,
            condition: BlobCondition::Missing,
            content_type: None,
            metadata: Vec::new(),
            expires_at_ms: now_ms + 60_000,
        },
    )
    .await
    .unwrap();
    blob.mutate(
        reference_identity(53, now_ms),
        BlobMutation::PutPart {
            key: blob_key.clone(),
            upload_id,
            part_number: 1,
            payload: b"blob-value".to_vec(),
        },
    )
    .await
    .unwrap();
    let completed = blob
        .mutate(
            reference_identity(54, now_ms),
            BlobMutation::Complete {
                key: blob_key.clone(),
                upload_id,
                part_count: 1,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        completed.output,
        BlobMutationOutcome::Committed { .. }
    ));
    let blob_read = blob
        .query(
            BlobQuery::Read {
                key: blob_key,
                offset: 0,
                limit: 128,
            },
            None,
        )
        .await
        .unwrap();
    assert!(matches!(blob_read.output, BlobQueryResult::Read(Some(_))));

    let queue = typed.queue::<ReferenceQueue>().unwrap();
    let sent = queue
        .send(
            reference_identity(55, now_ms),
            QueueSendRequest {
                producer_id: [55; 16],
                payload: b"queue-value".to_vec(),
                available_at_ms: now_ms,
            },
        )
        .await
        .unwrap();
    let claimed = queue
        .claim(
            reference_identity(56, now_ms),
            0,
            QueueClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed.output.len(), 1);
    assert!(
        queue
            .validate_claim(0, claimed.output.clone(), Some(claimed.receipt))
            .await
            .unwrap()
            .output
    );
    let message = &claimed.output[0];
    let acked = queue
        .ack(
            reference_identity(57, now_ms),
            0,
            message.message_id,
            message.token,
        )
        .await
        .unwrap();
    assert!(matches!(acked.output, QueueLeaseOutcome::Applied { .. }));
    assert!(matches!(
        sent.output,
        crab_cell_runtime::QueueSendOutcome::Sent { .. }
    ));

    let cron = typed.cron::<ReferenceCron>().unwrap();
    let schedule_id = [58; 16];
    cron.mutate(
        reference_identity(58, now_ms),
        CronMutation::Upsert {
            schedule_id,
            target_index: 0,
            target_partition: partition_for_shard(0).to_vec(),
            payload: b"cron-value".to_vec(),
            interval_ms: 1_000,
            next_due_ms: now_ms + 1_000,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        cron.get(schedule_id, None).await.unwrap().output,
        CronQueryResult::Get(Some(_))
    ));

    let workflow = typed.workflow::<ReferenceWorkflow>().unwrap();
    let activity_workflow_id = b"activity-run".to_vec();
    let activity_started = workflow
        .start(
            reference_identity(59, now_ms),
            activity_workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .unwrap();
    let activity_run_id = match activity_started.output {
        crab_cell_runtime::WorkflowOutcome::Applied { run_id, .. } => run_id,
        outcome => panic!("unexpected activity start outcome: {outcome:?}"),
    };
    let activity = crab_cell_runtime::ActivitySupervisor::new(
        typed.activities::<ReferenceWorkflow>().unwrap(),
        5_000,
    )
    .unwrap();
    assert!(matches!(
        activity.run_once(0, None).await.unwrap(),
        ActivityRunOutcome::Completed { .. }
    ));
    let activity_state = workflow
        .state(activity_workflow_id, None)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(activity_state.run_id, activity_run_id);
    assert_eq!(activity_state.status, WorkflowStatus::Completed);

    let effect_workflow_id = b"effect-run".to_vec();
    workflow
        .start(
            reference_identity(60, now_ms),
            effect_workflow_id,
            b"effect".to_vec(),
        )
        .await
        .unwrap();
    let effects = typed
        .effects::<ReferenceWorkflow>(
            CellTarget::new(
                tenant,
                application_id,
                WORKFLOW_NAMESPACE,
                &partition_for_shard(0),
            )
            .unwrap(),
        )
        .unwrap();
    let claims = effects
        .claim(
            reference_identity(61, now_ms),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(claims.output.len(), 1);
    assert!(
        effects
            .validate(claims.output.clone(), claims.receipt)
            .await
            .unwrap()
            .output
    );
    let effect = &claims.output[0];
    let acknowledged = effects
        .ack(
            reference_identity(62, now_ms),
            effect.clone(),
            b"effect-result".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(acknowledged.output, EffectLeaseOutcome::Delivered);

    let profile = QualificationProfile::new("typed-smoke".into(), 1, 1, 1, 5_000).unwrap();
    let workload = QualificationWorkload::generate_with_size(&profile, 91, 1, 16, 1).unwrap();
    let mut executor = TypedQualificationExecutor {
        handle: typed.clone(),
        tenant,
        application: application_id,
        now_ms,
    };
    let summary = workload.run(&mut executor).await.unwrap();
    assert_eq!(summary.operations(), 16);
    assert!(
        summary
            .primitive_counts()
            .iter()
            .all(|counts| counts.attempted() > 0 && counts.verified() > 0)
    );
    assert!(
        summary
            .metrics()
            .unwrap()
            .iter()
            .any(|metric| { metric.name() == "p99_latency_ms" && metric.unit() == "ms" })
    );

    // Drop the first owner and its local SQLite sources. The next owner must
    // recover every declared namespace from the published roots before the
    // same typed application handle is allowed to continue.
    drop(activity);
    drop(effects);
    drop(workflow);
    drop(cron);
    drop(queue);
    drop(blob);
    drop(kv);
    drop(sql);
    drop(typed);
    drop(runtime);
    drop(directory);

    let restored_directory = tempfile::TempDir::new().unwrap();
    let takeover_session = crab_cell_runtime::SessionId::from_bytes([70; 16]);
    let takeover_runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(4, 32).unwrap(),
        64 * 1024 * 1024,
        takeover_session,
        reference_host(),
    )
    .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let catalog = crab_cell_runtime::CellCatalog::new(layout.clone(), tenant);
    seed_reference_session(&layout, session).await;
    let mut restored_handles = Vec::new();
    for (namespace, module, incarnation_byte) in [
        (SQL_NAMESPACE, SQL_MODULE, 40_u8),
        (KV_NAMESPACE, KV_MODULE, 41_u8),
        (BLOB_NAMESPACE, BLOB_MODULE, 42_u8),
        (QUEUE_NAMESPACE, QUEUE_MODULE, 43_u8),
        (DEAD_LETTER_NAMESPACE, DEAD_LETTER_MODULE, 44_u8),
        (CRON_NAMESPACE, CRON_MODULE, 45_u8),
        (WORKFLOW_NAMESPACE, WORKFLOW_MODULE, 46_u8),
    ] {
        let target =
            CellTarget::new(tenant, application_id, namespace, &partition_for_shard(0)).unwrap();
        let observed = authority.load(target.cell_id()).await.unwrap().unwrap();
        let restored = takeover_runtime
            .takeover_restored(
                catalog.lookup(target.cell_id()).await.unwrap().unwrap(),
                CellReplica::new(
                    layout.clone(),
                    *target.cell_id().as_bytes(),
                    *IncarnationId::from_bytes([incarnation_byte; 16]).as_bytes(),
                    Limits::default(),
                )
                .unwrap(),
                authority.clone(),
                observed,
                fence_reference_session(&layout, session, takeover_session)
                    .await
                    .direct_takeover()
                    .unwrap(),
                crab_cell_runtime::RecoveryManifestStore::new(layout.clone(), Limits::default()),
                restored_directory
                    .path()
                    .join(format!("{module}-takeover.sqlite")),
                Owner {
                    session: takeover_session,
                    endpoint: "https://reference-takeover.internal:8081".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(restored.cell_id(), target.cell_id());
        restored_handles.push(restored);
    }
    let restored_client = CellClient::local_many(registry.clone(), restored_handles).unwrap();
    let restored = ApplicationHandle::<ReferenceApplication>::new(
        restored_client,
        application,
        tenant,
        application_id,
    )
    .with_blob_artifact_store(BlobArtifactStore::new(store));
    restored
        .sql::<ReferenceSql>(sql_target.clone())
        .unwrap()
        .query(
            None,
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT 11".into(),
                    parameters: Vec::new(),
                }],
            },
        )
        .await
        .unwrap();
    restored
        .kv::<ReferenceKv>(KV_NAMESPACE)
        .unwrap()
        .atomic(
            reference_identity(100, now_ms),
            KvAtomicRequest {
                scope: b"qualification".to_vec(),
                checks: Vec::new(),
                mutations: vec![KvMutation::Put {
                    key: b"after-recovery".to_vec(),
                    value: b"ok".to_vec(),
                    expires_at_ms: None,
                }],
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        restored
            .blob::<ReferenceBlob>()
            .unwrap()
            .query(
                BlobQuery::Read {
                    key: b"qualification/blob".to_vec(),
                    offset: 0,
                    limit: 128,
                },
                None,
            )
            .await
            .unwrap()
            .output,
        BlobQueryResult::Read(Some(_))
    ));
    let restored_queue = restored.queue::<ReferenceQueue>().unwrap();
    restored_queue
        .send(
            reference_identity(101, now_ms),
            QueueSendRequest {
                producer_id: [101; 16],
                payload: b"after-recovery".to_vec(),
                available_at_ms: now_ms,
            },
        )
        .await
        .unwrap();
    assert!(restored_queue.info(0, None).await.unwrap().output.ready >= 1);
    restored
        .cron::<ReferenceCron>()
        .unwrap()
        .get([58; 16], None)
        .await
        .unwrap();
    let restored_workflow = restored.workflow::<ReferenceWorkflow>().unwrap();
    let activity_workflow_id = b"activity-after-recovery".to_vec();
    restored_workflow
        .start(
            reference_identity(102, now_ms),
            activity_workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .unwrap();
    let restored_activity = crab_cell_runtime::ActivitySupervisor::new(
        restored.activities::<ReferenceWorkflow>().unwrap(),
        5_000,
    )
    .unwrap();
    assert!(matches!(
        restored_activity.run_once(0, None).await.unwrap(),
        ActivityRunOutcome::Completed { .. }
    ));
    assert_eq!(
        restored_workflow
            .state(activity_workflow_id, None)
            .await
            .unwrap()
            .output
            .unwrap()
            .status,
        WorkflowStatus::Completed
    );
    restored_workflow
        .start(
            reference_identity(103, now_ms),
            b"effect-after-recovery".to_vec(),
            b"effect".to_vec(),
        )
        .await
        .unwrap();
    let restored_effects = restored
        .effects::<ReferenceWorkflow>(
            CellTarget::new(
                tenant,
                application_id,
                WORKFLOW_NAMESPACE,
                &partition_for_shard(0),
            )
            .unwrap(),
        )
        .unwrap();
    let restored_claims = restored_effects
        .claim(
            reference_identity(104, now_ms),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(restored_claims.output.len(), 1);
    restored_effects
        .ack(
            reference_identity(105, now_ms),
            restored_claims.output[0].clone(),
            b"recovered-effect".to_vec(),
        )
        .await
        .unwrap();
    takeover_runtime.shutdown().await.unwrap();
}

#[test]
fn descriptor_digest_changes_when_build_identity_changes() {
    let first = compiled();
    let second = ReferenceApplication::compile(BuildDescriptor {
        source_revision: "different-source".into(),
        cargo_lock_digest: Digest::from_bytes([42; 32]),
    })
    .unwrap();
    assert_ne!(first.descriptor_digest(), second.descriptor_digest());
}
