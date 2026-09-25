//! A small, independently owned Cell service for the Compose scale example.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use bytes::Bytes;
use crab_cell_app::{ApplicationBuilder, CellApplication, CellType};
use crab_cell_host::{CellNode, CellNodeBuilder};
use crab_cell_runtime::NodeLeaseGuard;
use crab_cell_runtime::cell::{
    catalog::{CatalogEntry, CatalogRole, CellCatalog},
    executor::MutationIdentity,
    worker::SqlWorkerPool,
};
use crab_cell_runtime::client::CellClient;
use crab_cell_runtime::control::{Owner, authority::CellAuthority};
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, IncarnationId, NamespaceId, RequestId, SessionId, TenantId,
    partition_for_shard,
};
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::primitives::kv::{
    KvAtomicOutcome, KvAtomicRequest, KvModule, KvMutation, KvNamespace, install_kv_schema,
    register_kv,
};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor,
    OperationDescriptor, RegistryBuilder,
};
use crab_cell_runtime::{Error, Result};
use crab_ltx::{CellReplica, DiskBudget, Host, Limits};
use crab_storage::{ObjectStoreCredentials, Store, build_explicit_store};
use object_store::path::Path as ObjectPath;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const NAMESPACE: NamespaceId = NamespaceId::from_bytes([17; 16]);
const APPLICATION: ApplicationId = ApplicationId::from_bytes([18; 16]);
const SCOPE: &[u8] = b"reference";
const MIGRATION_SQL: &str = "-- KV tables are installed by the registered primitive";
static COMMANDS: [OperationDescriptor; 1] = [operation(1)];
static QUERIES: [OperationDescriptor; 2] = [operation(2), operation(3)];

struct ReferenceKv;

impl KvModule for ReferenceKv {
    const MODULE: &'static str = "compose-reference-kv";
    const ATOMIC_COMMAND_ID: u32 = 1;
    const GET_QUERY_ID: u32 = 2;
    const LIST_QUERY_ID: u32 = 3;
}

impl CellModule for ReferenceKv {
    const NAME: &'static str = Self::MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: LazyLock<ModuleDescriptor> = LazyLock::new(|| {
            let migrations = Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: MIGRATION_SQL,
                digest: Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
            }]));
            ModuleDescriptor {
                name: ReferenceKv::MODULE,
                source_digest: Digest::from_bytes(
                    *blake3::hash(include_bytes!("compose_kv_service.rs")).as_bytes(),
                ),
                retained_codes: &[],
                schema_min: 1,
                schema_max: 1,
                migrations,
                commands: &COMMANDS,
                queries: &QUERIES,
                workflow_definitions: &[],
                activity_types: &[],
                namespaces: &[NamespaceDescriptor {
                    id: NAMESPACE,
                    name: "compose-reference-kv",
                    role: CatalogRole::Kv,
                    shards: 1,
                    effect_targets: &[],
                    dead_letter: None,
                }],
            }
        });
        &DESCRIPTOR
    }

    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_kv::<Self>(registry)
    }
}

const fn operation(id: u32) -> OperationDescriptor {
    OperationDescriptor {
        id,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 64 * 1024,
        output_limit: 64 * 1024,
    }
}

struct ReferenceApplication;

impl CellApplication for ReferenceApplication {
    const NAME: &'static str = "compose-reference-application";

    fn register(builder: &mut ApplicationBuilder) -> Result<()> {
        builder.register(ReferenceKv)?;
        builder.cell_type(CellType::new(
            ReferenceKv::MODULE,
            "kv",
            NAMESPACE,
            CatalogRole::Kv,
            1,
        )?)
    }
}

#[derive(Clone)]
struct Service {
    node_name: String,
    node: Arc<CellNode>,
    lease: NodeLeaseGuard,
    kv: KvNamespace<ReferenceKv>,
}

#[derive(Deserialize)]
struct Write {
    request_id: Uuid,
    value: String,
}

type HttpResult = std::result::Result<Json<Value>, (StatusCode, String)>;

#[tokio::main]
async fn main() -> Result<()> {
    let node_name = required("CRAB_CELL_NODE_NAME")?;
    let prefix = required("CRAB_CELL_PREFIX")?;
    let data_dir = PathBuf::from(required("CRAB_CELL_DATA_DIR")?);
    let store = build_explicit_store(
        &required("CRAB_CELL_BUCKET")?,
        ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID")?,
            secret_access_key: required("AWS_SECRET_ACCESS_KEY")?,
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_CELL_ENDPOINT")?),
        true,
    )?;
    let layout = CellStorageLayout::new(
        store.clone(),
        ObjectPath::from(prefix.clone()),
        *APPLICATION.as_bytes(),
    );
    let tenant = TenantId::from_bytes(
        *<&[u8; 16]>::try_from(&blake3::hash(node_name.as_bytes()).as_bytes()[..16])
            .map_err(|_| Error::Identity("node digest"))?,
    );
    let session = SessionId::from_bytes(*Uuid::now_v7().as_bytes());
    let incarnation = IncarnationId::from_bytes(*Uuid::now_v7().as_bytes());
    let application = Arc::new(ReferenceApplication::compile(BuildDescriptor {
        source_revision: option_env!("CRAB_CELL_SCALE_SOURCE_REVISION")
            .unwrap_or("compose-reference")
            .into(),
        cargo_lock_digest: Digest::from_bytes(
            *blake3::hash(include_bytes!("../../../Cargo.lock")).as_bytes(),
        ),
    })?);
    let node = Arc::new(
        CellNodeBuilder::new(Arc::clone(&application))
            .with_runtime(SqlWorkerPool::new(1, 4)?, 16 * 1024 * 1024)
            .with_replica_host(
                Host::default().with_local_disk_budget(DiskBudget::new(128 * 1024 * 1024)),
            )
            .with_session(session)
            .build()?,
    );
    node.install_task_group(CancellationToken::new(), CancellationToken::new())?;
    let lease = create_lease(&store, &prefix, session).await?;
    node.install_node_lease(lease.clone())?;

    let target = CellTarget::new(tenant, APPLICATION, NAMESPACE, &partition_for_shard(0))?;
    let proof = CellCatalog::new(layout.clone(), tenant)
        .provision(CatalogEntry::new(
            &target,
            CatalogRole::Kv,
            application
                .registry()
                .module_code(ReferenceKv::MODULE)
                .ok_or(Error::Registry("KV module code"))?,
            1,
        )?)
        .await?;
    let authority = CellAuthority::new(layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: format!("https://{node_name}:8443"),
            },
        )
        .await?;
    let handle = node
        .runtime()
        .bootstrap(
            proof,
            CellReplica::new(
                layout,
                *target.cell_id().as_bytes(),
                *incarnation.as_bytes(),
                Limits {
                    max_database_bytes: 64 * 1024 * 1024,
                    max_capture_bytes: 16 * 1024 * 1024,
                    ..Limits::default()
                },
            )?,
            authority,
            observed,
            data_dir.join("reference.sqlite"),
            install_kv_schema,
        )
        .await?;
    let typed = node.application_handle::<ReferenceApplication>(
        CellClient::local(application.registry(), handle),
        tenant,
        APPLICATION,
    )?;
    let state = Service {
        node_name,
        node: Arc::clone(&node),
        lease: lease.clone(),
        kv: typed.kv::<ReferenceKv>(NAMESPACE)?,
    };
    let router = Router::new()
        .route("/health", get(health))
        .route("/kv/{key}", get(read).put(write))
        .with_state(state);
    let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], 8080))).await?;
    let result = tokio::select! {
        result = axum::serve(listener, router) => result.map_err(Error::FollowerIo),
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = lease.wait_fenced() => Err(Error::Fenced),
    };
    node.shutdown().await?;
    result
}

async fn create_lease(store: &Store, prefix: &str, session: SessionId) -> Result<NodeLeaseGuard> {
    let path = ObjectPath::from(format!(
        "{prefix}/sessions/{}",
        Uuid::from_bytes(*session.as_bytes())
    ));
    let now = now_ms()?;
    let mut token = store
        .create_strict_with_etag(&path, Bytes::copy_from_slice(&now.to_be_bytes()))
        .await?;
    let lease = NodeLeaseGuard::new(now, now + 60_000)?;
    let heartbeat = lease.clone();
    let store = store.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            let Ok(now) = now_ms() else { break };
            match store
                .update(&path, Bytes::copy_from_slice(&now.to_be_bytes()), token)
                .await
            {
                Ok(next) if heartbeat.renew(now, now + 60_000).is_ok() => token = next,
                _ => break,
            }
        }
        heartbeat.fence();
    });
    Ok(lease)
}

async fn health(State(state): State<Service>) -> HttpResult {
    if !state.node.is_ready() || state.lease.check().is_err() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Cell node is not ready".into(),
        ));
    }
    let stats = state.node.stats();
    Ok(Json(json!({
        "node": state.node_name,
        "ready": true,
        "active_cells": stats.active_cells(),
        "resident_bytes": stats.resident_bytes(),
        "local_disk_reserved_bytes": stats.local_disk_reserved_bytes(),
    })))
}

async fn write(
    State(state): State<Service>,
    Path(key): Path<String>,
    Json(body): Json<Write>,
) -> HttpResult {
    if key.len() > 1024 || body.value.len() > 4096 {
        return Err((StatusCode::BAD_REQUEST, "key or value too large".into()));
    }
    let now = now_ms().map_err(internal)?;
    let committed = state
        .kv
        .atomic(
            MutationIdentity {
                request_id: RequestId::from_bytes(*body.request_id.as_bytes()),
                issued_at_ms: now,
                expires_at_ms: now + 60_000,
            },
            KvAtomicRequest {
                scope: SCOPE.to_vec(),
                checks: Vec::new(),
                mutations: vec![KvMutation::Put {
                    key: key.into_bytes(),
                    value: body.value.into_bytes(),
                    expires_at_ms: None,
                }],
            },
        )
        .await
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
    if !matches!(committed.output, KvAtomicOutcome::Applied(_)) {
        return Err((StatusCode::CONFLICT, "KV precondition failed".into()));
    }
    Ok(Json(json!({"committed": true})))
}

async fn read(State(state): State<Service>, Path(key): Path<String>) -> HttpResult {
    let observed = state
        .kv
        .get(SCOPE.to_vec(), key.into_bytes(), None)
        .await
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
    let Some(entry) = observed.output else {
        return Err((StatusCode::NOT_FOUND, "key not found".into()));
    };
    let value = String::from_utf8(entry.value).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid stored UTF-8".into(),
        )
    })?;
    Ok(Json(json!({"value": value})))
}

fn required(name: &'static str) -> Result<String> {
    std::env::var(name).map_err(|_| Error::Node(name))
}

fn now_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Node("system clock precedes Unix epoch"))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| Error::Node("system clock exceeds Cell timestamp"))
}

fn internal(error: Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
