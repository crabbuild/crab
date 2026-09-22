use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
};

use crab_cell_app::{ApplicationBuilder, CellApplication, CellType};
use crab_cell_runtime::{
    ActivityContext, ActivityExecution, ActivityHandler, ApplicationId, BlobModule, BoundedEncoder,
    BuildDescriptor, CatalogEntry, CatalogRole, CellAuthority, CellHandle, CellModule, CellRuntime,
    CellStorageLayout, CellTarget, Command, CommandContext, CommandResult, CronInvocation,
    CronModule, CronTarget, Digest, EffectModule, IncarnationId, KvModule, MaintenanceModule,
    ModuleDescriptor, NamespaceDescriptor, NamespaceId, OperationDescriptor, Owner,
    QueueDeadLetterTarget, QueueModule, Registry, RegistryBuilder, Result, SqlBatch, SqlModule,
    SqlResultSet, SqlStatement, SqlValue, TenantId, WireValue, WorkflowAction,
    WorkflowActivityModule, WorkflowContext, WorkflowDecision, WorkflowDefinition, WorkflowModule,
    WorkflowStatus, partition_for_shard, register_activity, register_blob, register_cron,
    register_effect_delivery, register_kv, register_maintenance, register_queue, register_sql,
    register_workflow, register_workflow_activities,
};
use crab_ltx::{CellReplica, DiskBudget, Host, Limits};

pub const SQL_NAMESPACE: NamespaceId = NamespaceId::from_bytes([1; 16]);
pub const KV_NAMESPACE: NamespaceId = NamespaceId::from_bytes([2; 16]);
pub const BLOB_NAMESPACE: NamespaceId = NamespaceId::from_bytes([3; 16]);
pub const QUEUE_NAMESPACE: NamespaceId = NamespaceId::from_bytes([4; 16]);
pub const DEAD_LETTER_NAMESPACE: NamespaceId = NamespaceId::from_bytes([5; 16]);
pub const CRON_NAMESPACE: NamespaceId = NamespaceId::from_bytes([6; 16]);
pub const WORKFLOW_NAMESPACE: NamespaceId = NamespaceId::from_bytes([7; 16]);
pub const WORKFLOW_DIGEST: Digest = Digest::from_bytes([8; 32]);

pub const SQL_MODULE: &str = "reference-sql";
pub const KV_MODULE: &str = "reference-kv";
pub const BLOB_MODULE: &str = "reference-blob";
pub const QUEUE_MODULE: &str = "reference-queue";
pub const DEAD_LETTER_MODULE: &str = "reference-dead-letter";
pub const CRON_MODULE: &str = "reference-cron";
pub const WORKFLOW_MODULE: &str = "reference-workflow";

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
            .map(|id| {
                let mut descriptor = operation(id);
                if module == KV_MODULE {
                    descriptor.input_limit = 4 * 1024 * 1024 + 64 * 1024;
                }
                descriptor
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let query_descriptors = Box::leak(
        queries
            .iter()
            .copied()
            .map(|id| {
                let mut descriptor = operation(id);
                if module == KV_MODULE {
                    descriptor.output_limit = 4 * 1024 * 1024 + 64 * 1024;
                }
                descriptor
            })
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

pub struct ReferenceSql;
pub struct ReferenceCronDestination;

pub fn install_reference_sql_schema(
    transaction: &crab_ltx::rusqlite::Transaction<'_>,
) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE qualification_rows (id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
         CREATE TABLE qualification_cron_invocations (schedule_id BLOB NOT NULL, generation INTEGER NOT NULL, occurrence INTEGER NOT NULL, scheduled_at_ms INTEGER NOT NULL, payload BLOB NOT NULL, PRIMARY KEY (schedule_id, generation, occurrence))",
    )?;
    Ok(())
}

impl Command for ReferenceCronDestination {
    const MODULE: &'static str = SQL_MODULE;
    const ID: u32 = 6;
    const CODEC_VERSION: u32 = 1;
    type Input = CronInvocation;
    type Output = Vec<SqlResultSet>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        invocation: Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let generation = i64::try_from(invocation.generation)
            .map_err(|_| crab_cell_runtime::Error::Command("cron generation overflow"))?;
        let occurrence = i64::try_from(invocation.occurrence)
            .map_err(|_| crab_cell_runtime::Error::Command("cron occurrence overflow"))?;
        let rows = context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "INSERT INTO qualification_cron_invocations (schedule_id, generation, occurrence, scheduled_at_ms, payload) VALUES (?1, ?2, ?3, ?4, ?5)".into(),
                parameters: vec![
                    SqlValue::Blob(invocation.schedule_id.to_vec()),
                    SqlValue::Integer(generation),
                    SqlValue::Integer(occurrence),
                    SqlValue::Integer(invocation.scheduled_at_ms),
                    SqlValue::Blob(invocation.payload),
                ],
            }],
        })?;
        Ok(CommandResult::Success(rows))
    }
}

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
        descriptor(SQL_MODULE, &[1, 3, 4, 6], &[2, 5], NAMESPACES, &[], &[])
    }
    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_sql::<Self>(registry)?;
        register_effect_delivery::<Self>(registry)?;
        registry.bind_command::<ReferenceCronDestination>()
    }
}

pub struct ReferenceKv;
impl KvModule for ReferenceKv {
    const MODULE: &'static str = KV_MODULE;
    const ATOMIC_COMMAND_ID: u32 = 1;
    const GET_QUERY_ID: u32 = 2;
    const LIST_QUERY_ID: u32 = 3;
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

pub struct ReferenceBlob;
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

pub struct ReferenceQueue;
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

pub struct ReferenceCron;
impl MaintenanceModule for ReferenceCron {
    const MODULE: &'static str = CRON_MODULE;
    const TICK_COMMAND_ID: u32 = 3;
    const CRON_TARGETS: &'static [CronTarget] =
        &[CronTarget::new(SQL_MODULE, SQL_NAMESPACE, 6, 1, 1 << 20)];
}
impl EffectModule for ReferenceCron {
    const MODULE: &'static str = CRON_MODULE;
    const CLAIM_COMMAND_ID: u32 = 4;
    const LEASE_COMMAND_ID: u32 = 5;
    const VALIDATE_QUERY_ID: u32 = 6;
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
        descriptor(CRON_MODULE, &[1, 3, 4, 5], &[2, 6], NAMESPACES, &[], &[])
    }
    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_cron::<Self>(registry)?;
        register_effect_delivery::<Self>(registry)
    }
}

pub struct ReferenceWorkflow;
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
        if event == b"effect-valid" {
            let mut encoded = BoundedEncoder::new(1 << 20)?;
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "INSERT INTO qualification_rows (id, payload) VALUES (90, ?1)".into(),
                    parameters: vec![SqlValue::Blob(b"delivered-effect".to_vec())],
                }],
            }
            .encode(&mut encoded)?;
            return Ok(WorkflowDecision {
                status: WorkflowStatus::Completed,
                state: b"effect-valid-published".to_vec(),
                result: Some(b"effect-valid-scheduled".to_vec()),
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
                        input: encoded.finish(),
                        expires_at_ms: context.now_ms() + 60_000,
                    },
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

pub struct ReferenceApplication;
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

pub fn compiled() -> crab_cell_app::CompiledApplication {
    ReferenceApplication::compile(BuildDescriptor {
        source_revision: "reference-source".into(),
        cargo_lock_digest: Digest::from_bytes([42; 32]),
    })
    .expect("reference application compiles")
}
#[expect(
    clippy::too_many_arguments,
    reason = "the qualification fixture keeps each Cell contract explicit"
)]
pub async fn bootstrap_reference_cell<F>(
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

pub fn reference_host() -> Host {
    Host::default().with_local_disk_budget(DiskBudget::new(1 << 30))
}
