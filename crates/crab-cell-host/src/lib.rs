//! Provider-neutral lifecycle boundary for a compiled Cell application.
//!
//! `CellNode` owns the embedded runtime and its shared admission ledger. A
//! product server supplies providers, authentication and network transports;
//! it must not construct another runtime alongside this host.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crab_cell_app::{ApplicationHandle, CellApplication, CompiledApplication};
use crab_cell_runtime::{
    ApplicationId, CellClient, CellRuntime, CellRuntimeStats, Error, ReplicaHost, SessionId,
    SqlWorkerPool, TenantId,
};

/// Node lifecycle state visible to readiness and shutdown adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    Starting,
    Ready,
    Draining,
    Stopped,
}

/// Required inputs for one provider-neutral node.
pub struct CellNodeBuilder {
    application: Arc<CompiledApplication>,
    pool: Option<SqlWorkerPool>,
    replica_host: Option<ReplicaHost>,
    session: Option<SessionId>,
    node_retained_bytes: Option<usize>,
}

impl CellNodeBuilder {
    /// Starts a builder for one immutable compiled application.
    #[must_use]
    pub fn new(application: Arc<CompiledApplication>) -> Self {
        Self {
            application,
            pool: None,
            replica_host: None,
            session: None,
            node_retained_bytes: None,
        }
    }

    /// Supplies the shared SQL worker pool and its runtime byte ceiling.
    #[must_use]
    pub fn with_runtime(mut self, pool: SqlWorkerPool, node_retained_bytes: usize) -> Self {
        self.pool = Some(pool);
        self.node_retained_bytes = Some(node_retained_bytes);
        self
    }

    /// Supplies the LTX host admission and local capacity policy.
    #[must_use]
    pub fn with_replica_host(mut self, host: ReplicaHost) -> Self {
        self.replica_host = Some(host);
        self
    }

    /// Supplies the unique node session used by runtime ownership records.
    #[must_use]
    pub fn with_session(mut self, session: SessionId) -> Self {
        self.session = Some(session);
        self
    }

    /// Validates all required inputs before starting any background runtime task.
    pub fn build(self) -> crab_cell_runtime::Result<CellNode> {
        let (application, pool, host, session, node_retained_bytes) = self.required_parts()?;
        let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
            pool,
            node_retained_bytes,
            session,
            host,
        )?;
        Ok(CellNode {
            application,
            runtime,
            state: Arc::new(Mutex::new(NodeState::Starting)),
            lease_installed: AtomicBool::new(false),
            shutdown_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Builds an unadvertised host for bounded offline maintenance.
    ///
    /// This path intentionally uses object-only runtime admission: the caller
    /// must keep the host private and may not expose serving readiness.
    pub fn build_unleased_for_maintenance(self) -> crab_cell_runtime::Result<CellNode> {
        let (application, pool, host, session, node_retained_bytes) = self.required_parts()?;
        let runtime = CellRuntime::new_with_replica_host(pool, node_retained_bytes, session, host)?;
        Ok(CellNode {
            application,
            runtime,
            state: Arc::new(Mutex::new(NodeState::Starting)),
            lease_installed: AtomicBool::new(false),
            shutdown_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    fn required_parts(
        self,
    ) -> crab_cell_runtime::Result<(
        Arc<CompiledApplication>,
        SqlWorkerPool,
        ReplicaHost,
        SessionId,
        usize,
    )> {
        let pool = self
            .pool
            .ok_or(Error::Control("CellNode requires a SQL worker pool"))?;
        let host = self
            .replica_host
            .ok_or(Error::Control("CellNode requires an LTX replica host"))?;
        let session = self
            .session
            .ok_or(Error::Control("CellNode requires a node session"))?;
        let node_retained_bytes = self
            .node_retained_bytes
            .filter(|bytes| *bytes != 0)
            .ok_or(Error::Control("CellNode retained-byte ceiling is missing"))?;
        Ok((self.application, pool, host, session, node_retained_bytes))
    }
}

/// One started application host with an ordered drain/shutdown boundary.
pub struct CellNode {
    application: Arc<CompiledApplication>,
    runtime: CellRuntime,
    state: Arc<Mutex<NodeState>>,
    lease_installed: AtomicBool,
    shutdown_lock: Arc<tokio::sync::Mutex<()>>,
}

impl CellNode {
    /// Returns the compiled application artifact owned by this node.
    #[must_use]
    pub fn application(&self) -> &CompiledApplication {
        &self.application
    }

    /// Returns the node-owned runtime for operator telemetry and admission.
    #[must_use]
    pub fn runtime(&self) -> CellRuntime {
        self.runtime.clone()
    }

    /// Returns the current lifecycle state.
    #[must_use]
    pub fn state(&self) -> NodeState {
        self.state
            .lock()
            .map(|state| *state)
            .unwrap_or(NodeState::Stopped)
    }

    /// Returns whether the node has installed its lease and accepts work.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state() == NodeState::Ready
    }

    /// Returns current shared runtime admission metrics.
    #[must_use]
    pub fn stats(&self) -> CellRuntimeStats {
        self.runtime.stats()
    }

    /// Installs the product's metrics adapter before the node is advertised.
    pub fn install_telemetry(
        &self,
        telemetry: Arc<dyn crab_cell_runtime::CellTelemetry>,
    ) -> crab_cell_runtime::Result<()> {
        self.runtime.install_telemetry(telemetry)
    }

    /// Installs the authoritative node lease before readiness is exposed.
    pub fn install_node_lease(
        &self,
        lease: crab_cell_runtime::NodeLeaseGuard,
    ) -> crab_cell_runtime::Result<()> {
        self.runtime.install_node_lease(lease)?;
        self.lease_installed.store(true, Ordering::Release);
        self.mark_ready()
    }

    /// Marks the node ready after all product startup probes have completed.
    pub fn mark_ready(&self) -> crab_cell_runtime::Result<()> {
        if !self.lease_installed.load(Ordering::Acquire) {
            return Err(Error::Control(
                "CellNode cannot become ready before its node lease is installed",
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Control("CellNode lifecycle lock poisoned"))?;
        if *state == NodeState::Starting {
            *state = NodeState::Ready;
            return Ok(());
        }
        if *state == NodeState::Ready {
            return Ok(());
        }
        Err(Error::Control(
            "CellNode cannot become ready after shutdown",
        ))
    }

    /// Returns whether the owned runtime has entered shutdown.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.runtime.is_shutting_down()
    }

    /// Binds a product-created typed client to this application's tenant scope.
    pub fn application_handle<A: CellApplication>(
        &self,
        client: CellClient,
        tenant: TenantId,
        application: ApplicationId,
    ) -> ApplicationHandle<A> {
        ApplicationHandle::new(client, Arc::clone(&self.application), tenant, application)
    }

    /// Stops admission, drains the runtime, and waits for its dispatcher.
    pub async fn drain(&self) -> crab_cell_runtime::Result<()> {
        let _shutdown = self.shutdown_lock.lock().await;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::Control("CellNode lifecycle lock poisoned"))?;
            if *state == NodeState::Stopped {
                return Ok(());
            }
            *state = NodeState::Draining;
        }
        let result = self.runtime.shutdown().await;
        if result.is_ok()
            && let Ok(mut state) = self.state.lock()
        {
            *state = NodeState::Stopped;
        }
        result
    }

    /// Idempotent alias for graceful drain used by process shutdown hooks.
    pub async fn shutdown(&self) -> crab_cell_runtime::Result<()> {
        self.drain().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_cell_runtime::{
        BuildDescriptor, CatalogRole, CellModule, Digest, ModuleDescriptor, NamespaceDescriptor,
        NodeLeaseGuard, RegistryBuilder,
    };

    struct Module;

    impl CellModule for Module {
        const NAME: &'static str = "host-test";

        fn descriptor(&self) -> &'static ModuleDescriptor {
            static DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
                name: "host-test",
                source_digest: Digest::from_bytes([1; 32]),
                retained_codes: &[],
                schema_min: 1,
                schema_max: 1,
                migrations: &[crab_cell_runtime::MigrationDescriptor {
                    version: 1,
                    sql: "-- host migration v1",
                    digest: Digest::from_bytes([
                        0xd7, 0x41, 0xcb, 0x18, 0xae, 0xd4, 0x80, 0xb0, 0xe1, 0x55, 0x8e, 0x34,
                        0x5a, 0x6b, 0xef, 0xf5, 0xe1, 0x60, 0x80, 0x59, 0x06, 0xba, 0xfe, 0x75,
                        0xff, 0x9f, 0xa0, 0x7d, 0x10, 0xe7, 0x77, 0xbf,
                    ]),
                }],
                commands: &[],
                queries: &[],
                workflow_definitions: &[],
                activity_types: &[],
                namespaces: &[NamespaceDescriptor {
                    id: crab_cell_runtime::NamespaceId::from_bytes([2; 16]),
                    name: "host-test",
                    role: CatalogRole::Sql,
                    shards: 1,
                    effect_targets: &[],
                    dead_letter: None,
                }],
            };
            &DESCRIPTOR
        }

        fn register(self, _registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
            Ok(())
        }
    }

    fn application() -> Arc<CompiledApplication> {
        let mut builder = crab_cell_app::ApplicationBuilder::new(
            "host-test",
            BuildDescriptor {
                source_revision: "host-test".into(),
                cargo_lock_digest: Digest::from_bytes([7; 32]),
            },
        )
        .unwrap();
        builder.register(Module).unwrap();
        builder
            .cell_type(
                crab_cell_app::CellType::new(
                    "host-test",
                    "host-test",
                    crab_cell_runtime::NamespaceId::from_bytes([2; 16]),
                    CatalogRole::Sql,
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        Arc::new(builder.finish().unwrap())
    }

    #[test]
    fn builder_rejects_missing_owners_before_starting() {
        let error = match CellNodeBuilder::new(application()).build() {
            Ok(_) => panic!("missing node owners must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::Control(_)));
    }

    #[tokio::test]
    async fn node_shutdown_is_idempotent_and_returns_stopped_state() {
        let pool = SqlWorkerPool::new(1, 1).unwrap();
        let host = ReplicaHost::default();
        let node = CellNodeBuilder::new(application())
            .with_runtime(pool, 16 * 1024 * 1024)
            .with_replica_host(host)
            .with_session(SessionId::from_bytes([4; 16]))
            .build()
            .unwrap();
        assert_eq!(node.state(), NodeState::Starting);
        assert!(!node.is_ready());
        assert!(node.mark_ready().is_err());
        node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        assert_eq!(node.state(), NodeState::Ready);
        assert!(node.is_ready());
        node.shutdown().await.unwrap();
        assert_eq!(node.state(), NodeState::Stopped);
        node.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_shutdown_waits_for_the_single_runtime_drain() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([5; 16]))
            .build()
            .unwrap();
        let first = node.shutdown();
        let second = node.shutdown();
        let (first, second) = tokio::join!(first, second);
        first.unwrap();
        second.unwrap();
        assert_eq!(node.state(), NodeState::Stopped);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_nodes_have_independent_lifecycle_and_resource_ledgers() {
        let mut nodes = Vec::new();
        for index in 0..3_u8 {
            let node = CellNodeBuilder::new(application())
                .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
                .with_replica_host(ReplicaHost::default())
                .with_session(SessionId::from_bytes([index + 10; 16]))
                .build()
                .unwrap();
            assert_eq!(node.state(), NodeState::Starting);
            assert!(!node.is_ready());
            nodes.push(node);
        }

        for node in &nodes {
            node.shutdown().await.unwrap();
            assert_eq!(node.state(), NodeState::Stopped);
            assert!(node.is_shutting_down());
        }
    }
}
