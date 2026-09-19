use std::{sync::Arc, time::UNIX_EPOCH};

mod support;

use crab_cell_runtime::{
    ApplicationId, BlobCondition, BlobModule, BlobMutation, BlobMutationOutcome, BlobNamespace,
    BlobQuery, BlobQueryResult, BuildDescriptor, CatalogEntry, CatalogRole, CellAuthority,
    CellCatalog, CellClient, CellModule, CellRuntime, CellTarget, Command, CommandContext,
    CommandResult, CronInvocation, CronModule, CronMutation, CronNamespace, CronQueryResult,
    CronTarget, Digest, IncarnationId, MaintenanceModule, MaintenanceTickOutcome,
    MaintenanceTickRequest, MigrationDescriptor, ModuleDescriptor, MutationIdentity,
    NamespaceDescriptor, NamespaceId, OperationDescriptor, Owner, RegistryBuilder, RequestId,
    SessionId, SqlWorkerPool, TenantId, register_blob, register_cron,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

const BLOB_MODULE: &str = "blob-test";
const BLOB_NAMESPACE: NamespaceId = NamespaceId::from_bytes([1; 16]);
const BLOB_MIGRATION: &str = include_str!("../src/migrations/blob.sql");
const CRON_MODULE: &str = "cron-test";
const CRON_NAMESPACE: NamespaceId = NamespaceId::from_bytes([2; 16]);
const CRON_MIGRATION: &str = include_str!("../src/migrations/cron.sql");
const TARGET_MODULE: &str = "cron-target-test";
const TARGET_NAMESPACE: NamespaceId = NamespaceId::from_bytes([3; 16]);
const TARGET_MIGRATION: &str = "CREATE TABLE cron_target(value BLOB) STRICT;";
const TARGET_INPUT_LIMIT: u32 = 300 * 1024;
const CRON_TARGETS: &[CronTarget] = &[CronTarget::new(
    TARGET_MODULE,
    TARGET_NAMESPACE,
    9,
    1,
    TARGET_INPUT_LIMIT,
)];

const BLOB_COMMANDS: &[OperationDescriptor] = &[operation(1, 300 * 1024, 64), operation(2, 8, 5)];
const BLOB_QUERIES: &[OperationDescriptor] = &[operation(1, 4 * 1024, 600 * 1024)];
const CRON_COMMANDS: &[OperationDescriptor] = &[operation(1, 300 * 1024, 16), operation(2, 8, 5)];
const CRON_QUERIES: &[OperationDescriptor] = &[operation(1, 64, 600 * 1024)];
const TARGET_COMMANDS: &[OperationDescriptor] = &[operation(9, TARGET_INPUT_LIMIT, 1)];

struct TestBlob;

impl MaintenanceModule for TestBlob {
    const MODULE: &'static str = BLOB_MODULE;
    const TICK_COMMAND_ID: u32 = 2;
}

impl BlobModule for TestBlob {
    const NAMESPACE: NamespaceId = BLOB_NAMESPACE;
    const MUTATE_COMMAND_ID: u32 = 1;
    const QUERY_ID: u32 = 1;
}

impl CellModule for TestBlob {
    const NAME: &'static str = BLOB_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        descriptor(
            BLOB_MODULE,
            BLOB_NAMESPACE,
            CatalogRole::Blob,
            BLOB_MIGRATION,
            BLOB_COMMANDS,
            BLOB_QUERIES,
            &[],
            10,
        )
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_blob::<Self>(registry)
    }
}

struct TestCron;

impl MaintenanceModule for TestCron {
    const MODULE: &'static str = CRON_MODULE;
    const TICK_COMMAND_ID: u32 = 2;
    const CRON_TARGETS: &'static [CronTarget] = CRON_TARGETS;
}

impl CronModule for TestCron {
    const NAMESPACE: NamespaceId = CRON_NAMESPACE;
    const MUTATE_COMMAND_ID: u32 = 1;
    const QUERY_ID: u32 = 1;
}

impl CellModule for TestCron {
    const NAME: &'static str = CRON_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        descriptor(
            CRON_MODULE,
            CRON_NAMESPACE,
            CatalogRole::Cron,
            CRON_MIGRATION,
            CRON_COMMANDS,
            CRON_QUERIES,
            &[TARGET_NAMESPACE],
            11,
        )
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_cron::<Self>(registry)
    }
}

struct CronTargetModule;

struct ReceiveCron;

impl Command for ReceiveCron {
    const MODULE: &'static str = TARGET_MODULE;
    const ID: u32 = 9;
    const CODEC_VERSION: u32 = 1;
    type Input = CronInvocation;
    type Output = ();

    fn execute(
        _: &mut CommandContext<'_, '_>,
        _: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        Ok(CommandResult::Success(()))
    }
}

impl CellModule for CronTargetModule {
    const NAME: &'static str = TARGET_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        descriptor(
            TARGET_MODULE,
            TARGET_NAMESPACE,
            CatalogRole::Repository,
            TARGET_MIGRATION,
            TARGET_COMMANDS,
            &[],
            &[],
            12,
        )
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_command::<ReceiveCron>()
    }
}

#[test]
fn blob_and_cron_bindings_match_release_descriptors() {
    let mut registry = RegistryBuilder::new(BuildDescriptor {
        source_revision: "blob-cron-test".into(),
        cargo_lock_digest: Digest::from_bytes([9; 32]),
    });
    registry.register(CronTargetModule).unwrap();
    registry.register(TestBlob).unwrap();
    registry.register(TestCron).unwrap();
    registry.finish().unwrap();
}

#[tokio::test]
async fn typed_blob_and_cron_recover_after_owner_loss() {
    let registry = registry();
    let tenant = TenantId::from_bytes([20; 16]);
    let application = ApplicationId::from_bytes([21; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(
        store,
        Path::from("blob-cron-runtime"),
        *application.as_bytes(),
    );

    let blob_target =
        CellTarget::new(tenant, application, BLOB_NAMESPACE, &0_u32.to_be_bytes()).unwrap();
    let blob_incarnation = IncarnationId::from_bytes([23; 16]);
    let blob_replica = CellReplica::new(
        layout.clone(),
        *blob_target.cell_id().as_bytes(),
        *blob_incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog = CellCatalog::new(layout.clone(), tenant);
    let blob_proof = catalog
        .provision(
            CatalogEntry::new(
                &blob_target,
                CatalogRole::Blob,
                registry.module_code(BLOB_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let blob_session = SessionId::from_bytes([24; 16]);
    let blob_control = authority
        .create_initial(
            &blob_proof,
            blob_incarnation,
            Owner {
                session: blob_session,
                endpoint: "https://blob.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let blob_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        blob_session,
    )
    .unwrap();
    let blob_handle = blob_runtime
        .bootstrap(
            blob_proof,
            blob_replica,
            authority.clone(),
            blob_control,
            directory.path().join("blob.sqlite"),
            crab_cell_runtime::install_blob_schema,
        )
        .await
        .unwrap();
    let blobs = BlobNamespace::<TestBlob>::new(
        CellClient::local(registry.clone(), blob_handle.clone()),
        tenant,
        application,
    )
    .unwrap();
    let key = b"artifacts/result".to_vec();
    let upload_id = [25; 16];
    let start_now_ms = now_ms();
    blobs
        .mutate(
            identity(26, start_now_ms),
            BlobMutation::Begin {
                key: key.clone(),
                upload_id,
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms: start_now_ms + 60_000,
            },
        )
        .await
        .unwrap();
    blobs
        .mutate(
            identity(27, start_now_ms),
            BlobMutation::PutPart {
                key: key.clone(),
                upload_id,
                part_number: 1,
                payload: b"published".to_vec(),
            },
        )
        .await
        .unwrap();
    let committed = blobs
        .mutate(
            identity(28, start_now_ms),
            BlobMutation::Complete {
                key: key.clone(),
                upload_id,
                part_count: 1,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        committed.output,
        BlobMutationOutcome::Committed { size: 9, .. }
    ));
    let read = blobs
        .query(
            BlobQuery::Read {
                key,
                offset: 0,
                limit: 32,
            },
            Some(committed.receipt),
        )
        .await
        .unwrap();
    assert!(matches!(
        read.output,
        BlobQueryResult::Read(Some(ref value)) if value.bytes == b"published"
    ));
    drop(blobs);
    drop(blob_handle);
    drop(blob_runtime);
    let stale_blob = authority
        .load(blob_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let blob_successor = SessionId::from_bytes([34; 16]);
    let blob_takeover = support::fence_session(&layout, blob_session, blob_successor).await;
    let blob_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        blob_successor,
    )
    .unwrap();
    let blob_restored = blob_runtime
        .takeover_restored(
            catalog
                .lookup(blob_target.cell_id())
                .await
                .unwrap()
                .unwrap(),
            CellReplica::new(
                layout.clone(),
                *blob_target.cell_id().as_bytes(),
                *blob_incarnation.as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            authority.clone(),
            stale_blob,
            blob_takeover.direct_takeover().unwrap(),
            crab_cell_runtime::RecoveryManifestStore::new(layout.clone(), Limits::default()),
            directory.path().join("blob-takeover.sqlite"),
            Owner {
                session: blob_successor,
                endpoint: "https://blob-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_blobs = BlobNamespace::<TestBlob>::new(
        CellClient::local(registry.clone(), blob_restored.clone()),
        tenant,
        application,
    )
    .unwrap();
    assert!(matches!(
        restored_blobs
            .query(
                BlobQuery::Read {
                    key: b"artifacts/result".to_vec(),
                    offset: 0,
                    limit: 32,
                },
                Some(committed.receipt),
            )
            .await
            .unwrap()
            .output,
        BlobQueryResult::Read(Some(ref value)) if value.bytes == b"published"
    ));
    drop(restored_blobs);
    blob_restored.drain().await.unwrap();
    blob_runtime.shutdown().await.unwrap();

    let cron_target =
        CellTarget::new(tenant, application, CRON_NAMESPACE, &0_u32.to_be_bytes()).unwrap();
    let cron_incarnation = IncarnationId::from_bytes([29; 16]);
    let cron_replica = CellReplica::new(
        layout.clone(),
        *cron_target.cell_id().as_bytes(),
        *cron_incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let cron_proof = catalog
        .provision(
            CatalogEntry::new(
                &cron_target,
                CatalogRole::Cron,
                registry.module_code(CRON_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let cron_session = SessionId::from_bytes([30; 16]);
    let cron_control = authority
        .create_initial(
            &cron_proof,
            cron_incarnation,
            Owner {
                session: cron_session,
                endpoint: "https://cron.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let cron_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        cron_session,
    )
    .unwrap();
    let cron_handle = cron_runtime
        .bootstrap(
            cron_proof,
            cron_replica,
            authority.clone(),
            cron_control,
            directory.path().join("cron.sqlite"),
            crab_cell_runtime::install_cron_schema,
        )
        .await
        .unwrap();
    let cron_client = CellClient::local(registry.clone(), cron_handle.clone());
    let cron = CronNamespace::<TestCron>::new(cron_client.clone(), tenant, application).unwrap();
    let schedule_id = [31; 16];
    let cron_start_now_ms = now_ms();
    let scheduled = cron
        .mutate(
            identity(32, cron_start_now_ms),
            CronMutation::Upsert {
                schedule_id,
                target_index: 0,
                target_partition: b"destination".to_vec(),
                payload: b"run".to_vec(),
                interval_ms: 1_000,
                next_due_ms: cron_start_now_ms + 100,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let tick_now_ms = now_ms();
    let tick = registry
        .run_maintenance_once(
            cron_client,
            cron_target.clone(),
            identity(33, tick_now_ms),
            MaintenanceTickRequest {
                expected_commit_sequence: scheduled.receipt.commit_sequence,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        tick.output,
        MaintenanceTickOutcome::Applied { processed: 1 }
    );
    let state = cron.get(schedule_id, Some(tick.receipt)).await.unwrap();
    assert!(matches!(
        state.output,
        CronQueryResult::Get(Some(ref schedule)) if schedule.occurrence == 1
    ));
    drop(cron);
    drop(cron_handle);
    drop(cron_runtime);
    let stale_cron = authority
        .load(cron_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let cron_successor = SessionId::from_bytes([35; 16]);
    let cron_takeover = support::fence_session(&layout, cron_session, cron_successor).await;
    let cron_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        cron_successor,
    )
    .unwrap();
    let cron_restored = cron_runtime
        .takeover_restored(
            catalog
                .lookup(cron_target.cell_id())
                .await
                .unwrap()
                .unwrap(),
            CellReplica::new(
                layout.clone(),
                *cron_target.cell_id().as_bytes(),
                *cron_incarnation.as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            authority.clone(),
            stale_cron,
            cron_takeover.direct_takeover().unwrap(),
            crab_cell_runtime::RecoveryManifestStore::new(layout.clone(), Limits::default()),
            directory.path().join("cron-takeover.sqlite"),
            Owner {
                session: cron_successor,
                endpoint: "https://cron-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_cron = CronNamespace::<TestCron>::new(
        CellClient::local(registry, cron_restored.clone()),
        tenant,
        application,
    )
    .unwrap();
    assert!(matches!(
        restored_cron.get(schedule_id, Some(tick.receipt)).await.unwrap().output,
        CronQueryResult::Get(Some(ref schedule)) if schedule.occurrence == 1
    ));
    drop(restored_cron);
    cron_restored.drain().await.unwrap();
    cron_runtime.shutdown().await.unwrap();
}

fn registry() -> Arc<crab_cell_runtime::Registry> {
    let mut registry = RegistryBuilder::new(BuildDescriptor {
        source_revision: "blob-cron-test".into(),
        cargo_lock_digest: Digest::from_bytes([9; 32]),
    });
    registry.register(CronTargetModule).unwrap();
    registry.register(TestBlob).unwrap();
    registry.register(TestCron).unwrap();
    Arc::new(registry.finish().unwrap())
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn identity(byte: u8, now_ms: i64) -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

fn descriptor(
    name: &'static str,
    namespace: NamespaceId,
    role: CatalogRole,
    migration: &'static str,
    commands: &'static [OperationDescriptor],
    queries: &'static [OperationDescriptor],
    effect_targets: &'static [NamespaceId],
    digest: u8,
) -> &'static ModuleDescriptor {
    Box::leak(Box::new(ModuleDescriptor {
        name,
        source_digest: Digest::from_bytes([digest; 32]),
        retained_codes: &[],
        schema_min: 1,
        schema_max: 1,
        migrations: Box::leak(Box::new([MigrationDescriptor {
            version: 1,
            sql: migration,
            digest: Digest::from_bytes(*blake3::hash(migration.as_bytes()).as_bytes()),
        }])),
        commands,
        queries,
        workflow_definitions: &[],
        activity_types: &[],
        namespaces: Box::leak(Box::new([NamespaceDescriptor {
            id: namespace,
            name,
            role,
            shards: 1,
            effect_targets,
            dead_letter: None,
        }])),
    }))
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
