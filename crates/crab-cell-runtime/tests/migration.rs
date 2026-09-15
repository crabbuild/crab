use std::sync::{Arc, OnceLock};

use crab_cell_runtime::{
    ApplicationId, BuildDescriptor, CatalogEntry, CatalogRole, CellAuthority, CellCatalog,
    CellModule, CellRuntime, CellTarget, Digest, HandlerOutcome, IncarnationId,
    MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor, NamespaceId, Owner, Registry,
    RegistryBuilder, SessionId, SqlWorkerPool, TenantId,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

const MODULE: &str = "migration-test";
const NAMESPACE: NamespaceId = NamespaceId::from_bytes([61; 16]);
const MIGRATION_ONE: &str =
    "CREATE TABLE records(id INTEGER PRIMARY KEY, value BLOB NOT NULL) STRICT";
const MIGRATION_TWO: &str = "ALTER TABLE records ADD COLUMN label TEXT";

struct MigrationModule;

impl CellModule for MigrationModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        descriptor()
    }

    fn register(self, _registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        Ok(())
    }
}

fn descriptor() -> &'static ModuleDescriptor {
    static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| ModuleDescriptor {
        name: MODULE,
        source_digest: Digest::from_bytes([62; 32]),
        schema_min: 1,
        schema_max: 2,
        migrations: Box::leak(Box::new([
            MigrationDescriptor {
                version: 1,
                sql: MIGRATION_ONE,
                digest: Digest::from_bytes(*blake3::hash(MIGRATION_ONE.as_bytes()).as_bytes()),
            },
            MigrationDescriptor {
                version: 2,
                sql: MIGRATION_TWO,
                digest: Digest::from_bytes(*blake3::hash(MIGRATION_TWO.as_bytes()).as_bytes()),
            },
        ])),
        commands: &[],
        queries: &[],
        workflow_definitions: &[],
        activity_types: &[],
        namespaces: Box::leak(Box::new([NamespaceDescriptor {
            id: NAMESPACE,
            name: MODULE,
            role: CatalogRole::Sql,
            shards: 1,
            effect_targets: &[],
            dead_letter: None,
        }])),
    })
}

fn compiled_registry() -> Registry {
    let mut registry = RegistryBuilder::new(BuildDescriptor {
        source_revision: "migration-test".into(),
        cargo_lock_digest: Digest::from_bytes([63; 32]),
    });
    registry.register(MigrationModule).unwrap();
    registry.finish().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn migration_replaces_capability_publishes_schema_and_restores_exact_root() {
    let registry = compiled_registry();
    let code = registry.module_code(MODULE).unwrap();
    let target = CellTarget::new(
        TenantId::from_bytes([64; 16]),
        ApplicationId::from_bytes([65; 16]),
        NAMESPACE,
        b"schema-step",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([66; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("migration-runtime"),
        [65; 16],
    );
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(CatalogEntry::new(&target, CatalogRole::Sql, code, 1).unwrap())
        .await
        .unwrap();
    let authority = CellAuthority::new(layout);
    let first_session = SessionId::from_bytes([67; 16]);
    let control = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://migration-first.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let files = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 2).unwrap(),
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
            files.path().join("schema-one.sqlite"),
            |transaction| {
                transaction.execute_batch(MIGRATION_ONE)?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let old_handle = handle.clone();
    let plan = registry
        .next_migration(NAMESPACE, handle.code(), handle.schema())
        .unwrap()
        .unwrap();
    let migration_digest = plan.digest();
    let migrated = handle.migrate(plan, 10).await.unwrap();
    assert_eq!(migrated.outcome.code, code);
    assert_eq!(migrated.outcome.schema, 2);
    assert_eq!(migrated.outcome.commit_sequence, 1);
    assert!(matches!(
        old_handle.query(1, 1, |_| Ok(Vec::new())).await,
        Err(crab_cell_runtime::Error::CellDraining)
    ));
    assert!(
        registry
            .next_migration(NAMESPACE, migrated.handle.code(), migrated.handle.schema())
            .unwrap()
            .is_none()
    );
    let metadata = migrated
        .handle
        .query(64, 128, |connection| {
            let schema = connection.query_row(
                "SELECT schema_version FROM sys_meta WHERE singleton = 1",
                [],
                |row| row.get::<_, u32>(0),
            )?;
            let (digest, sequence) = connection.query_row(
                "SELECT digest, applied_sequence FROM sys_migrations WHERE version = 2",
                [],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u64>(1)?)),
            )?;
            let mut result = schema.to_be_bytes().to_vec();
            result.extend_from_slice(&sequence.to_be_bytes());
            result.extend_from_slice(&digest);
            Ok(result)
        })
        .await
        .unwrap();
    assert_eq!(&metadata[..4], &2_u32.to_be_bytes());
    assert_eq!(&metadata[4..12], &1_u64.to_be_bytes());
    assert_eq!(&metadata[12..], migration_digest.as_bytes());
    let written = migrated
        .handle
        .execute(
            crab_cell_runtime::MutationIdentity {
                request_id: crab_cell_runtime::RequestId::from_bytes([68; 16]),
                issued_at_ms: 10,
                expires_at_ms: 1_000,
            },
            Digest::from_bytes([69; 32]),
            20,
            128,
            64,
            |transaction| {
                transaction.execute(
                    "INSERT INTO records(id, value, label) VALUES (1, x'01', 'migrated')",
                    [],
                )?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    assert_eq!(written.commit_sequence(), 2);
    migrated.handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
    assert_eq!(idle.value().schema, 2);
    assert_eq!(idle.value().code, code);
    assert_eq!(idle.value().root.as_ref().unwrap().commit_sequence, 2);
    let second_session = SessionId::from_bytes([70; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 2).unwrap(),
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
            files.path().join("schema-two.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://migration-second.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(restored.schema(), 2);
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                Ok(
                    connection.query_row("SELECT label FROM records WHERE id = 1", [], |row| {
                        row.get::<_, String>(0).map(String::into_bytes)
                    })?,
                )
            })
            .await
            .unwrap(),
        b"migrated"
    );
    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn migration_digest_conflict_blocks_activation() {
    let registry = compiled_registry();
    let code = registry.module_code(MODULE).unwrap();
    let target = CellTarget::new(
        TenantId::from_bytes([71; 16]),
        ApplicationId::from_bytes([72; 16]),
        NAMESPACE,
        b"migration-conflict",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([73; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("migration-conflict"),
        [72; 16],
    );
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(CatalogEntry::new(&target, CatalogRole::Sql, code, 1).unwrap())
        .await
        .unwrap();
    let authority = CellAuthority::new(layout);
    let session = SessionId::from_bytes([74; 16]);
    let initial = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: "https://migration-conflict.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let files = tempfile::TempDir::new().unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            replica,
            authority.clone(),
            initial,
            files.path().join("conflict.sqlite"),
            |transaction| {
                transaction.execute_batch(MIGRATION_ONE)?;
                transaction.execute(
                    "INSERT INTO sys_migrations(version, digest, applied_sequence) VALUES (2, ?1, 0)",
                    [[99_u8; 32].as_slice()],
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let before = authority.load(cell).await.unwrap().unwrap();
    let plan = registry
        .next_migration(NAMESPACE, handle.code(), handle.schema())
        .unwrap()
        .unwrap();
    assert!(matches!(
        handle.migrate(plan, 10).await,
        Err(crab_cell_runtime::Error::Registry(
            "migration digest conflicts with SQLite history"
        ))
    ));
    let after = authority.load(cell).await.unwrap().unwrap();
    assert_eq!(after.value(), before.value());
    assert_eq!(after.value().schema, 1);
    assert_eq!(after.value().root.as_ref().unwrap().commit_sequence, 0);
    assert!(matches!(
        handle.query(1, 1, |_| Ok(Vec::new())).await,
        Err(crab_cell_runtime::Error::CellDraining) | Err(crab_cell_runtime::Error::Fenced)
    ));
    runtime.shutdown().await.unwrap();
}
