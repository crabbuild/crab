use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crab_cell_runtime::cell::actor::CellRuntime;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::catalog::{CatalogEntry, CellCatalog};
use crab_cell_runtime::cell::executor::HandlerOutcome;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::control::{ControlState, Owner};
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_cell_runtime::identity::{IncarnationId, NodeId};
use crab_cell_runtime::node::durability::{NodeDurability, NodeLogAuthority};
use crab_cell_runtime::node::lease::NodeLeaseGuard;
use crab_cell_runtime::node::log::{DurabilityGate, NodeLogRotationBarrier};
use crab_cell_runtime::node::log_shipper::NodeLogShipper;
use crab_cell_runtime::node::log_transport::{LocalFollowerTransport, NodeLogTransport};
use crab_cell_runtime::peer::{
    MigrationPeerClient, PeerAuthorizer, PeerCellResolver, PeerDispatcher, PeerPrincipal,
    PeerRoundTrip, PeerSigner, PeerVerifier, VerifiedPeerRequest,
};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, ModuleDescriptor, NamespaceDescriptor, Registry, RegistryBuilder,
};
use crab_cell_runtime::registry::{MigrationDescriptor, RetainedCodeDescriptor};
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Limits};
use crab_storage::Store;
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory, path::Path,
};

const MODULE: &str = "migration-test";
const NAMESPACE: NamespaceId = NamespaceId::from_bytes([61; 16]);
const MIGRATION_ONE: &str =
    "CREATE TABLE records(id INTEGER PRIMARY KEY, value BLOB NOT NULL) STRICT";
const MIGRATION_TWO: &str = "ALTER TABLE records ADD COLUMN label TEXT";
const PREDECESSOR_CODE: Digest = Digest::from_bytes([60; 32]);

struct MigrationModule;

struct MigrationNodeAuthority;

impl NodeLogAuthority for MigrationNodeAuthority {
    fn activate<'a>(
        &'a self,
        _log_epoch: u64,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn advance_coverage<'a>(
        &'a self,
        _log_epoch: u64,
        _tiered_through: u64,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn close<'a>(
        &'a self,
        _barrier: &'a NodeLogRotationBarrier,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug)]
struct PausedPutStore {
    inner: Arc<InMemory>,
    pause_next: AtomicBool,
    started: Arc<tokio::sync::Barrier>,
    release: Arc<tokio::sync::Barrier>,
}

impl PausedPutStore {
    fn new() -> Self {
        Self {
            inner: Arc::new(InMemory::new()),
            pause_next: AtomicBool::new(false),
            started: Arc::new(tokio::sync::Barrier::new(2)),
            release: Arc::new(tokio::sync::Barrier::new(2)),
        }
    }

    fn pause_next(&self) {
        self.pause_next.store(true, Ordering::Release);
    }
}

impl fmt::Display for PausedPutStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("paused-put-store")
    }
}

#[async_trait::async_trait]
impl ObjectStore for PausedPutStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if self.pause_next.swap(false, Ordering::AcqRel) {
            self.started.wait().await;
            self.release.wait().await;
        }
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

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
        retained_codes: &[RetainedCodeDescriptor {
            code: PREDECESSOR_CODE,
            schema_min: 1,
            schema_max: 2,
        }],
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

struct RuntimeResolver {
    target: CellTarget,
    proof: crab_cell_runtime::cell::catalog::CatalogProof,
    authority: CellAuthority,
    runtime: CellRuntime,
}

impl PeerCellResolver for RuntimeResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = crab_cell_runtime::Result<crab_cell_runtime::cell::actor::CellHandle>,
                > + Send
                + 'static,
        >,
    > {
        let matches = target == self.target;
        let proof = self.proof.clone();
        let authority = self.authority.clone();
        let runtime = self.runtime.clone();
        Box::pin(async move {
            if !matches {
                return Err(crab_cell_runtime::Error::CellNotActive);
            }
            let control = authority
                .load(proof.entry().cell())
                .await?
                .ok_or(crab_cell_runtime::Error::CellNotActive)?;
            runtime
                .local_handle(proof, &control)
                .await?
                .ok_or(crab_cell_runtime::Error::CellNotActive)
        })
    }
}

struct MigrationAuthorizer;

impl PeerAuthorizer for MigrationAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> crab_cell_runtime::Result<()> {
        if request.permits("cell.release.migrate") {
            Ok(())
        } else {
            Err(crab_cell_runtime::Error::PeerAuthorization(
                "missing release migration action",
            ))
        }
    }
}

struct LoopbackRoundTrip {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
}

impl PeerRoundTrip for LoopbackRoundTrip {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>> {
        let verifier = Arc::clone(&self.verifier);
        let dispatcher = Arc::clone(&self.dispatcher);
        Box::pin(async move {
            let verified = verifier.verify(&request, 10)?;
            if verified.target() != &target {
                return Err(crab_cell_runtime::Error::Peer(
                    "loopback migration target changed",
                ));
            }
            dispatcher.dispatch_bytes(&verified, 10).await
        })
    }
}

mod peer;
mod schema;
