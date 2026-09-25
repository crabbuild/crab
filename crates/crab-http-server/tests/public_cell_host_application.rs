use std::{sync::Arc, time::UNIX_EPOCH};

use crab_cell_app::{ApplicationBuilder, CellApplication, CellType, CompiledApplication};
use crab_cell_host::CellNodeBuilder;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::catalog::{CatalogEntry, CellCatalog};
use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::client::CellClient;
use crab_cell_runtime::control::Owner;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::{
    CellTarget, Digest, NamespaceId, SessionId, TenantId, partition_for_shard,
};
use crab_cell_runtime::identity::{IncarnationId, RequestId};
use crab_cell_runtime::ltx::{CellReplica, CellStorageLayout};
use crab_cell_runtime::ltx::{Host as ReplicaHost, Limits as ReplicaLimits};
use crab_cell_runtime::node::lease::NodeLeaseGuard;
use crab_cell_runtime::primitives::sql::SqlModule;
use crab_cell_runtime::primitives::sql::{SqlBatch, SqlStatement, SqlValue, register_sql};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, ModuleDescriptor, NamespaceDescriptor, RegistryBuilder,
};
use crab_cell_runtime::registry::{MigrationDescriptor, OperationDescriptor};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};
use tokio_util::sync::CancellationToken;

const NAMESPACE: NamespaceId = NamespaceId::from_bytes([61; 16]);
const MODULE: &str = "public-host-sql";

struct PublicSql;

impl SqlModule for PublicSql {
    const MODULE: &'static str = MODULE;
    const BATCH_COMMAND_ID: u32 = 1;
    const BATCH_QUERY_ID: u32 = 2;
}

impl CellModule for PublicSql {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static COMMANDS: &[OperationDescriptor] = &[OperationDescriptor {
            id: 1,
            codec_version: 1,
            schema_min: 1,
            schema_max: 1,
            input_limit: 1 << 20,
            output_limit: 1 << 20,
        }];
        static QUERIES: &[OperationDescriptor] = &[OperationDescriptor {
            id: 2,
            codec_version: 1,
            schema_min: 1,
            schema_max: 1,
            input_limit: 1 << 20,
            output_limit: 1 << 20,
        }];
        static NAMESPACES: &[NamespaceDescriptor] = &[NamespaceDescriptor {
            id: NAMESPACE,
            name: MODULE,
            role: CatalogRole::Sql,
            shards: 1,
            effect_targets: &[],
            dead_letter: None,
        }];
        static MIGRATIONS: &[MigrationDescriptor] = &[MigrationDescriptor {
            version: 1,
            sql: "-- public host migration v1",
            digest: Digest::from_bytes([
                0xd9, 0x3b, 0xf8, 0xf3, 0x1c, 0x7a, 0x42, 0x69, 0xd6, 0x9b, 0x8b, 0x0e, 0x10, 0x30,
                0xcf, 0x85, 0xcd, 0xb0, 0xb8, 0x49, 0x16, 0xd4, 0xa0, 0xb0, 0x54, 0xe2, 0xef, 0x25,
                0xda, 0x3c, 0x1b, 0x12,
            ]),
        }];
        static DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
            name: MODULE,
            source_digest: Digest::from_bytes([62; 32]),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: MIGRATIONS,
            commands: COMMANDS,
            queries: QUERIES,
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: NAMESPACES,
        };
        &DESCRIPTOR
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_sql::<Self>(registry)
    }
}

struct PublicApplication;

impl CellApplication for PublicApplication {
    const NAME: &'static str = "public-host-application";

    fn register(builder: &mut ApplicationBuilder) -> crab_cell_runtime::Result<()> {
        builder.register(PublicSql)?;
        builder.cell_type(CellType::new(
            MODULE,
            "sql",
            NAMESPACE,
            CatalogRole::Sql,
            1,
        )?)?;
        Ok(())
    }
}

fn compiled_application() -> Arc<CompiledApplication> {
    Arc::new(
        PublicApplication::compile(BuildDescriptor {
            source_revision: "public-host-source".into(),
            cargo_lock_digest: Digest::from_bytes([63; 32]),
        })
        .expect("public host application compiles"),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_public_application_handle_executes_typed_sql() {
    let application = compiled_application();
    let tenant = TenantId::from_bytes([64; 16]);
    let application_id = crab_cell_runtime::ApplicationId::from_bytes([65; 16]);
    let target =
        CellTarget::new(tenant, application_id, NAMESPACE, &partition_for_shard(0)).unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("public-host-application"),
        *application_id.as_bytes(),
    );
    let catalog = CellCatalog::new(layout.clone(), tenant);
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Sql,
                application.registry().module_code(MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let session = SessionId::from_bytes([66; 16]);
    let incarnation = IncarnationId::from_bytes([67; 16]);
    let observed = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: "https://public-host.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let node = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 2).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(session)
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    assert!(node.is_ready());

    let directory = tempfile::tempdir().unwrap();
    let rejected = node
        .runtime()
        .bootstrap(
            proof.clone(),
            CellReplica::new(
                layout.clone(),
                *target.cell_id().as_bytes(),
                *incarnation.as_bytes(),
                ReplicaLimits::default(),
            )
            .unwrap(),
            authority.clone(),
            observed.clone(),
            directory.path().join("rejected.sqlite"),
            |_| Ok(()),
        )
        .await
        .err()
        .expect("descriptor limits must reject a larger replica ceiling");
    assert!(matches!(
        rejected,
        crab_cell_runtime::Error::Control("Cell storage limits differ from application")
    ));
    assert!(
        authority
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .root
            .is_none()
    );
    let handle = node
        .runtime()
        .bootstrap(
            proof,
            CellReplica::new(
                layout,
                *target.cell_id().as_bytes(),
                *incarnation.as_bytes(),
                ReplicaLimits {
                    max_database_bytes: 64 * 1024 * 1024,
                    max_capture_bytes: 16 * 1024 * 1024,
                    ..ReplicaLimits::default()
                },
            )
            .unwrap(),
            authority,
            observed,
            directory.path().join("public-host.sqlite"),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let client = CellClient::local(application.registry(), handle.clone());
    let typed = node.application_handle::<PublicApplication>(client, tenant, application_id);
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let result = typed
        .sql::<PublicSql>(target)
        .unwrap()
        .batch(
            MutationIdentity {
                request_id: RequestId::from_bytes([68; 16]),
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
    assert_eq!(result.output[0].rows.len(), 1);

    handle.drain().await.unwrap();
    node.shutdown().await.unwrap();
}
