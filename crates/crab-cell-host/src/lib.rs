//! Provider-neutral lifecycle boundary for a compiled Cell application.
//!
//! `CellNode` owns the embedded runtime and its shared admission ledger. A
//! product server supplies providers, authentication and network transports;
//! it must not construct another runtime alongside this host.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use crab_cell_app::{ApplicationHandle, CellApplication, CompiledApplication};
use crab_cell_runtime::{
    ApplicationId, CellClient, CellRuntime, CellRuntimeStats, Error, ReplicaHost, SessionId,
    SqlWorkerPool, TenantId,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const MAX_NODE_FACILITIES: usize = 64;
const MAX_NODE_TASKS: usize = 256;

/// Error returned by a provider-owned node facility during drain.
pub type FacilityResult = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// One provider-owned lifecycle component attached to a [`CellNode`].
pub struct CellNodeFacility {
    name: &'static str,
    drain: Arc<dyn Fn() -> Pin<Box<dyn Future<Output = FacilityResult> + Send>> + Send + Sync>,
}

impl CellNodeFacility {
    /// Creates one named drain callback. The callback must be idempotent.
    pub fn new<F, Fut>(name: &'static str, drain: F) -> crab_cell_runtime::Result<Self>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = FacilityResult> + Send + 'static,
    {
        if name.is_empty() {
            return Err(Error::Control("CellNodeFacility name is empty"));
        }
        Ok(Self {
            name,
            drain: Arc::new(move || Box::pin(drain())),
        })
    }
}

/// Bounded task supervisor owned by a [`CellNode`] facility.
pub struct CellNodeTaskGroup {
    cancellation: CancellationToken,
    node_shutdown: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<FacilityResult>>>,
}

impl CellNodeTaskGroup {
    /// Creates a task group whose cancellation tokens are controlled by the product host.
    #[must_use]
    pub fn new(cancellation: CancellationToken, node_shutdown: CancellationToken) -> Self {
        Self {
            cancellation,
            node_shutdown,
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Spawns one bounded node task and retains its join handle for drain.
    pub fn spawn<F, E>(&self, task: F) -> crab_cell_runtime::Result<()>
    where
        F: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => return Err(Error::Control("CellNode task group lock poisoned")),
        };
        if tasks.len() >= MAX_NODE_TASKS {
            return Err(Error::Capacity("CellNode task limit reached"));
        }
        let handle = tokio::spawn(async move {
            task.await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
        });
        tasks.push(handle);
        Ok(())
    }

    /// Cancels admission and joins tasks in reverse registration order.
    pub async fn drain(&self) -> FacilityResult {
        self.drain_until(None).await
    }

    /// Cancels admission and joins tasks until an optional absolute deadline.
    pub async fn drain_until(&self, deadline: Option<Instant>) -> FacilityResult {
        self.cancellation.cancel();
        self.node_shutdown.cancel();
        let tasks = match self.tasks.lock() {
            Ok(mut tasks) => std::mem::take(&mut *tasks),
            Err(poisoned) => {
                for task in poisoned.into_inner().drain(..) {
                    task.abort();
                }
                return Err(Box::new(std::io::Error::other(
                    "CellNode task group lock poisoned",
                )));
            }
        };
        let mut tasks = TaskBatch {
            tasks,
            abort_on_drop: true,
        };
        let mut first_error = None;
        while let Some(index) = tasks.tasks.len().checked_sub(1) {
            let result = match deadline {
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline.into(), &mut tasks.tasks[index]).await {
                        Ok(result) => result,
                        Err(_) => {
                            first_error.get_or_insert_with(|| {
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "CellNode task group drain deadline exceeded",
                                ))
                                    as Box<dyn std::error::Error + Send + Sync>
                            });
                            break;
                        }
                    }
                }
                None => (&mut tasks.tasks[index]).await,
            };
            tasks.tasks.pop();
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Ok(Err(_)) => {}
                Err(error) if first_error.is_none() => {
                    first_error = Some(Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
                }
                Err(_) => {}
            }
        }
        tasks.abort_on_drop = false;
        first_error.map_or(Ok(()), Err)
    }
}

struct TaskBatch {
    tasks: Vec<JoinHandle<FacilityResult>>,
    abort_on_drop: bool,
}

impl Drop for TaskBatch {
    fn drop(&mut self) {
        if self.abort_on_drop {
            for task in &self.tasks {
                task.abort();
            }
        }
    }
}

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
            facilities: Arc::new(Mutex::new(Vec::new())),
            task_group: Arc::new(Mutex::new(None)),
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
            facilities: Arc::new(Mutex::new(Vec::new())),
            task_group: Arc::new(Mutex::new(None)),
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
        if session.as_bytes().iter().all(|byte| *byte == 0) {
            return Err(Error::Control("CellNode node session is zero"));
        }
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
    facilities: Arc<Mutex<Vec<CellNodeFacility>>>,
    task_group: Arc<Mutex<Option<Arc<CellNodeTaskGroup>>>>,
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
        self.install_node_lease_for_startup(lease)?;
        self.mark_ready()
    }

    /// Installs the lease without opening readiness to the product boundary.
    ///
    /// Servers use this during startup, then call [`Self::mark_ready`] only
    /// after their listeners and owned facilities have been started.
    pub fn install_node_lease_for_startup(
        &self,
        lease: crab_cell_runtime::NodeLeaseGuard,
    ) -> crab_cell_runtime::Result<()> {
        self.runtime.install_node_lease(lease)?;
        self.lease_installed.store(true, Ordering::Release);
        Ok(())
    }

    /// Creates and retains the bounded coordination task group for this node.
    pub fn install_task_group(
        &self,
        cancellation: CancellationToken,
        node_shutdown: CancellationToken,
    ) -> crab_cell_runtime::Result<Arc<CellNodeTaskGroup>> {
        let task_group = Arc::new(CellNodeTaskGroup::new(cancellation, node_shutdown));
        let mut installed = self
            .task_group
            .lock()
            .map_err(|_| Error::Control("CellNode task group lock poisoned"))?;
        if installed.is_some() {
            return Err(Error::Control("CellNode task group already installed"));
        }
        let drain_group = Arc::clone(&task_group);
        let facility = CellNodeFacility::new("cell-coordination-tasks", move || {
            let drain_group = Arc::clone(&drain_group);
            async move { drain_group.drain().await }
        })?;
        self.install_facility(facility)?;
        *installed = Some(Arc::clone(&task_group));
        Ok(task_group)
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

    /// Attaches one provider-owned lifecycle component before node shutdown.
    pub fn install_facility(&self, facility: CellNodeFacility) -> crab_cell_runtime::Result<()> {
        let mut facilities = self
            .facilities
            .lock()
            .map_err(|_| Error::Control("CellNode facility lock poisoned"))?;
        if !matches!(self.state(), NodeState::Starting | NodeState::Ready) {
            return Err(Error::CellDraining);
        }
        if facilities.len() >= MAX_NODE_FACILITIES {
            return Err(Error::Capacity("CellNode facility limit reached"));
        }
        facilities.push(facility);
        Ok(())
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
        self.drain_until(None).await
    }

    /// Stops admission and completes every owned drain phase by `deadline`.
    pub async fn drain_until(&self, deadline: Option<Instant>) -> crab_cell_runtime::Result<()> {
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
        let facilities = self
            .facilities
            .lock()
            .map(|facilities| {
                facilities
                    .iter()
                    .rev()
                    .map(|facility| (facility.name, Arc::clone(&facility.drain)))
                    .collect::<Vec<_>>()
            })
            .map_err(|_| Error::Control("CellNode facility lock poisoned"));
        let mut first_error = None;
        match facilities {
            Err(error) => first_error = Some(error),
            Ok(facilities) => {
                for (name, drain) in facilities {
                    let result = match deadline {
                        Some(deadline) => {
                            match tokio::time::timeout_at(deadline.into(), drain()).await {
                                Ok(result) => result,
                                Err(_) => Err(Box::new(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "CellNode facility drain deadline exceeded",
                                ))
                                    as Box<dyn std::error::Error + Send + Sync>),
                            }
                        }
                        None => drain().await,
                    };
                    if let Err(source) = result
                        && first_error.is_none()
                    {
                        first_error = Some(Error::Facility { name, source });
                    }
                }
            }
        }
        let runtime_result = match deadline {
            Some(deadline) => {
                match tokio::time::timeout_at(deadline.into(), self.runtime.shutdown()).await {
                    Ok(result) => result,
                    Err(_) => Err(Error::Control("CellNode runtime drain deadline exceeded")),
                }
            }
            None => self.runtime.shutdown().await,
        };
        if first_error.is_none() {
            first_error = runtime_result.err();
        }
        let result = match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        };
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

    /// Deadline-aware alias for graceful shutdown hooks.
    pub async fn shutdown_until(&self, deadline: Instant) -> crab_cell_runtime::Result<()> {
        self.drain_until(Some(deadline)).await
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

    #[test]
    fn builder_rejects_zero_node_session_before_starting() {
        let result = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([0; 16]))
            .build();
        let error = match result {
            Ok(_) => panic!("zero node sessions must fail closed"),
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

    #[tokio::test]
    async fn startup_lease_does_not_open_readiness_before_host_startup_finishes() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([19; 16]))
            .build()
            .unwrap();
        node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        assert_eq!(node.state(), NodeState::Starting);
        assert!(!node.is_ready());
        node.mark_ready().unwrap();
        assert!(node.is_ready());
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_group_cancels_both_tokens_and_joins_tasks() {
        let cancellation = CancellationToken::new();
        let node_shutdown = CancellationToken::new();
        let finished = Arc::new(AtomicBool::new(false));
        let task_cancellation = cancellation.clone();
        let task_shutdown = node_shutdown.clone();
        let task_finished = Arc::clone(&finished);
        let tasks = CellNodeTaskGroup::new(cancellation.clone(), node_shutdown.clone());
        tasks
            .spawn(async move {
                task_cancellation.cancelled().await;
                task_shutdown.cancelled().await;
                task_finished.store(true, Ordering::Release);
                Ok::<(), Error>(())
            })
            .unwrap();

        tasks.drain().await.unwrap();

        assert!(cancellation.is_cancelled());
        assert!(node_shutdown.is_cancelled());
        assert!(finished.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn task_group_deadline_aborts_unfinished_tasks() {
        let tasks = CellNodeTaskGroup::new(CancellationToken::new(), CancellationToken::new());
        tasks
            .spawn(async {
                std::future::pending::<()>().await;
                Ok::<(), Error>(())
            })
            .unwrap();

        let result = tasks
            .drain_until(Some(Instant::now() + std::time::Duration::from_millis(10)))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn node_owns_one_task_group_and_drains_it() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([20; 16]))
            .build()
            .unwrap();
        let cancellation = CancellationToken::new();
        let node_shutdown = CancellationToken::new();
        let tasks = node
            .install_task_group(cancellation.clone(), node_shutdown.clone())
            .unwrap();
        assert!(
            node.install_task_group(CancellationToken::new(), CancellationToken::new())
                .is_err()
        );
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = Arc::clone(&finished);
        let task_shutdown = node_shutdown.clone();
        tasks
            .spawn(async move {
                task_shutdown.cancelled().await;
                task_finished.store(true, Ordering::Release);
                Ok::<(), Error>(())
            })
            .unwrap();

        node.shutdown().await.unwrap();

        assert!(cancellation.is_cancelled());
        assert!(node_shutdown.is_cancelled());
        assert!(finished.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn task_group_rejects_tasks_above_bound() {
        let cancellation = CancellationToken::new();
        let node_shutdown = CancellationToken::new();
        let tasks = CellNodeTaskGroup::new(cancellation, node_shutdown.clone());
        for _ in 0..MAX_NODE_TASKS {
            let node_shutdown = node_shutdown.clone();
            tasks
                .spawn(async move {
                    node_shutdown.cancelled().await;
                    Ok::<(), Error>(())
                })
                .unwrap();
        }
        let node_shutdown = node_shutdown.clone();
        let error = tasks
            .spawn(async move {
                node_shutdown.cancelled().await;
                Ok::<(), Error>(())
            })
            .unwrap_err();
        assert!(matches!(error, Error::Capacity(_)));

        tasks.drain().await.unwrap();
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

    #[tokio::test]
    async fn facilities_drain_in_reverse_registration_order() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([16; 16]))
            .build()
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        for name in ["storage", "transport", "scheduler"] {
            let events = Arc::clone(&events);
            node.install_facility(
                CellNodeFacility::new(name, move || {
                    let events = Arc::clone(&events);
                    async move {
                        events
                            .lock()
                            .map_err(|_| {
                                Box::new(std::io::Error::other("event lock poisoned"))
                                    as Box<dyn std::error::Error + Send + Sync>
                            })?
                            .push(name);
                        Ok(())
                    }
                })
                .unwrap(),
            )
            .unwrap();
        }

        node.shutdown().await.unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec!["scheduler", "transport", "storage"]
        );
    }

    #[tokio::test]
    async fn facility_failure_is_reported_after_all_facilities_attempt_and_runtime_drains() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([17; 16]))
            .build()
            .unwrap();
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_clone = Arc::clone(&completed);
        node.install_facility(
            CellNodeFacility::new("healthy", move || {
                let completed = Arc::clone(&completed_clone);
                async move {
                    completed.store(true, Ordering::Release);
                    Ok(())
                }
            })
            .unwrap(),
        )
        .unwrap();
        node.install_facility(
            CellNodeFacility::new("broken", || async {
                Err(Box::new(std::io::Error::other("drain failed"))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
            .unwrap(),
        )
        .unwrap();

        let error = node.shutdown().await.unwrap_err();
        assert!(matches!(error, Error::Facility { name: "broken", .. }));
        assert!(completed.load(Ordering::Acquire));
        assert!(node.is_shutting_down());
        assert_eq!(node.state(), NodeState::Draining);
    }

    #[tokio::test]
    async fn facility_registration_is_bounded_and_rejected_after_drain() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([18; 16]))
            .build()
            .unwrap();
        assert!(CellNodeFacility::new("", || async { Ok(()) }).is_err());
        for _ in 0..MAX_NODE_FACILITIES {
            node.install_facility(CellNodeFacility::new("facility", || async { Ok(()) }).unwrap())
                .unwrap();
        }
        assert!(matches!(
            node.install_facility(CellNodeFacility::new("overflow", || async { Ok(()) }).unwrap()),
            Err(Error::Capacity(_))
        ));
        node.shutdown().await.unwrap();
        assert!(matches!(
            node.install_facility(CellNodeFacility::new("late", || async { Ok(()) }).unwrap()),
            Err(Error::CellDraining)
        ));
    }
}
