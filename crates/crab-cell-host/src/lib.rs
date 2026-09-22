//! Provider-neutral lifecycle boundary for a compiled Cell application.
//!
//! `CellNode` owns the embedded runtime and its shared admission ledger. A
//! product server supplies providers, authentication and network transports;
//! it must not construct another runtime alongside this host.

use std::{
    any::Any,
    collections::HashSet,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use crab_cell_app::{ApplicationHandle, CellApplication, CompiledApplication};
use crab_cell_runtime::{
    ApplicationId, CellClient, CellRuntime, CellRuntimeStats, DiskBudget, Error, FollowerStore,
    NodeDurabilityConfig, QualificationOperationExecutor, QualificationRunSummary,
    QualificationWorkload, ReplicaHost, ReplicaLimits, SessionId, SqlWorkerPool, TenantId,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const MAX_NODE_FACILITIES: usize = 64;
const MAX_NODE_TASKS: usize = 256;

/// Stable host-owned component name for the follower store.
pub const FOLLOWER_STORE_COMPONENT: &str = "follower-store";
/// Stable host-owned component name for the node-log enrollment provider.
pub const NODE_DURABILITY_PROVIDER_COMPONENT: &str = "node-durability-provider";

/// Error returned by a provider-owned node facility during drain.
pub type FacilityResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Node-log rotation events emitted by the host supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeDurabilityRotation {
    Started,
    Pending,
    Failed,
    Completed,
}

/// Provider-owned enrollment adapter used by the host durability supervisor.
///
/// The provider is responsible for authority and transport enrollment. The
/// host consumes the resulting provider-neutral configuration and is the only
/// owner that constructs and installs [`crab_cell_runtime::NodeDurability`].
pub trait NodeDurabilityProvider: Send + Sync + 'static {
    fn recruit(
        self: Arc<Self>,
        limits: ReplicaLimits,
        required_follower_bytes: u64,
        live_node_limit: usize,
    ) -> Pin<Box<dyn Future<Output = FacilityResult<Option<NodeDurabilityConfig>>> + Send>>;

    fn rotation_event(&self, _event: NodeDurabilityRotation) {}
}

/// Fixed host-owned bounds and identity for the node-log supervisor.
#[derive(Clone, Copy, Debug)]
pub struct NodeDurabilitySupervisorConfig {
    application: ApplicationId,
    limits: ReplicaLimits,
    required_follower_bytes: u64,
    live_node_limit: usize,
    recruit_interval: std::time::Duration,
    rotation_interval: std::time::Duration,
    max_issued_frames: u64,
}

impl NodeDurabilitySupervisorConfig {
    /// Creates a bounded supervisor configuration.
    pub fn new(
        application: ApplicationId,
        limits: ReplicaLimits,
        required_follower_bytes: u64,
        live_node_limit: usize,
        recruit_interval: std::time::Duration,
        rotation_interval: std::time::Duration,
        max_issued_frames: u64,
    ) -> crab_cell_runtime::Result<Self> {
        if application.as_bytes().iter().all(|byte| *byte == 0)
            || required_follower_bytes == 0
            || live_node_limit == 0
            || recruit_interval.is_zero()
            || rotation_interval.is_zero()
            || max_issued_frames == 0
        {
            return Err(Error::Control(
                "invalid CellNode durability supervisor configuration",
            ));
        }
        Ok(Self {
            application,
            limits,
            required_follower_bytes,
            live_node_limit,
            recruit_interval,
            rotation_interval,
            max_issued_frames,
        })
    }
}

/// One provider-owned lifecycle component attached to a [`CellNode`].
pub struct CellNodeFacility {
    name: &'static str,
    owner: Option<Arc<dyn Any + Send + Sync>>,
    drain: Arc<dyn Fn() -> Pin<Box<dyn Future<Output = FacilityResult> + Send>> + Send + Sync>,
}

impl CellNodeFacility {
    /// Creates one named drain callback. The callback must be idempotent and
    /// must finish promptly when the node's shutdown cancellation is observed.
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
            owner: None,
            drain: Arc::new(move || Box::pin(drain())),
        })
    }

    /// Creates one named node-owned component with an idempotent drain callback.
    pub fn owned<T, F, Fut>(
        name: &'static str,
        owner: Arc<T>,
        drain: F,
    ) -> crab_cell_runtime::Result<Self>
    where
        T: Send + Sync + 'static,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = FacilityResult> + Send + 'static,
    {
        if name.is_empty() {
            return Err(Error::Control("CellNodeFacility name is empty"));
        }
        Ok(Self {
            name,
            owner: Some(owner),
            drain: Arc::new(move || Box::pin(drain())),
        })
    }

    fn owner<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.owner.as_ref()?.clone().downcast::<T>().ok()
    }
}

struct AbortOnDrop<T> {
    handle: JoinHandle<T>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self { handle }
    }

    async fn join(&mut self) -> std::result::Result<T, tokio::task::JoinError> {
        (&mut self.handle).await
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Bounded task supervisor owned by a [`CellNode`] facility.
pub struct CellNodeTaskGroup {
    cancellation: CancellationToken,
    node_shutdown: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<FacilityResult>>>,
    failed: Arc<AtomicBool>,
    draining: AtomicBool,
}

impl Drop for CellNodeTaskGroup {
    fn drop(&mut self) {
        let tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(poisoned) => poisoned.into_inner(),
        };
        for task in tasks.iter() {
            task.abort();
        }
    }
}

impl CellNodeTaskGroup {
    fn cancel(&self) {
        self.draining.store(true, Ordering::Release);
        self.cancellation.cancel();
        self.node_shutdown.cancel();
    }

    /// Creates a task group whose cancellation tokens are controlled by the product host.
    #[must_use]
    pub fn new(cancellation: CancellationToken, node_shutdown: CancellationToken) -> Self {
        Self {
            cancellation,
            node_shutdown,
            tasks: Mutex::new(Vec::new()),
            failed: Arc::new(AtomicBool::new(false)),
            draining: AtomicBool::new(false),
        }
    }

    fn is_healthy(&self) -> bool {
        if self.failed.load(Ordering::Acquire) || self.draining.load(Ordering::Acquire) {
            return false;
        }
        self.tasks
            .lock()
            .map(|tasks| tasks.iter().all(|task| !task.is_finished()))
            .unwrap_or(false)
    }

    fn ensure_accepting_tasks(&self) -> crab_cell_runtime::Result<()> {
        if self.draining.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }
        Ok(())
    }

    /// Spawns one bounded node task and retains its join handle for drain.
    pub fn spawn<F, E>(&self, task: F) -> crab_cell_runtime::Result<()>
    where
        F: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        self.ensure_accepting_tasks()?;
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => return Err(Error::Control("CellNode task group lock poisoned")),
        };
        self.ensure_accepting_tasks()?;
        if tasks.len() >= MAX_NODE_TASKS {
            return Err(Error::Capacity("CellNode task limit reached"));
        }
        let failed = Arc::clone(&self.failed);
        let handle = tokio::spawn(async move {
            let mut task = AbortOnDrop::new(tokio::spawn(task));
            match task.join().await {
                Ok(result) => {
                    let result = result.map_err(|error| {
                        Box::new(error) as Box<dyn std::error::Error + Send + Sync>
                    });
                    if result.is_err() {
                        failed.store(true, Ordering::Release);
                    }
                    result
                }
                Err(error) => {
                    failed.store(true, Ordering::Release);
                    Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
                }
            }
        });
        tasks.push(handle);
        Ok(())
    }

    /// Spawns one task that already uses the node's boxed facility error type.
    pub fn spawn_boxed<F>(&self, task: F) -> crab_cell_runtime::Result<()>
    where
        F: Future<Output = FacilityResult> + Send + 'static,
    {
        self.ensure_accepting_tasks()?;
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => return Err(Error::Control("CellNode task group lock poisoned")),
        };
        self.ensure_accepting_tasks()?;
        if tasks.len() >= MAX_NODE_TASKS {
            return Err(Error::Capacity("CellNode task limit reached"));
        }
        let failed = Arc::clone(&self.failed);
        tasks.push(tokio::spawn(async move {
            let mut task = AbortOnDrop::new(tokio::spawn(task));
            match task.join().await {
                Ok(result) => {
                    if result.is_err() {
                        failed.store(true, Ordering::Release);
                    }
                    result
                }
                Err(error) => {
                    failed.store(true, Ordering::Release);
                    Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
                }
            }
        }));
        Ok(())
    }

    /// Cancels admission and joins tasks in reverse registration order.
    pub async fn drain(&self) -> FacilityResult {
        self.drain_until(None).await
    }

    /// Cancels admission and joins tasks until an optional absolute deadline.
    pub async fn drain_until(&self, deadline: Option<Instant>) -> FacilityResult {
        self.cancel();
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
        let mut timed_out = false;
        while let Some(index) = tasks.tasks.len().checked_sub(1) {
            let result = match deadline {
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline.into(), &mut tasks.tasks[index]).await {
                        Ok(result) => result,
                        Err(_) => {
                            timed_out = true;
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
        if !timed_out {
            tasks.abort_on_drop = false;
        }
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

/// Point-in-time lifecycle and admission status for one [`CellNode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeStatus {
    state: NodeState,
    shutting_down: bool,
    stats: CellRuntimeStats,
}

impl NodeStatus {
    /// Returns the lifecycle state observed for this status sample.
    #[must_use]
    pub const fn state(self) -> NodeState {
        self.state
    }

    /// Returns whether runtime admission has been cancelled.
    #[must_use]
    pub const fn is_shutting_down(self) -> bool {
        self.shutting_down
    }

    /// Returns the node-wide admission metrics observed for this sample.
    #[must_use]
    pub const fn stats(self) -> CellRuntimeStats {
        self.stats
    }
}

/// Required inputs for one provider-neutral node.
pub struct CellNodeBuilder {
    application: Arc<CompiledApplication>,
    pool: Option<SqlWorkerPool>,
    replica_host: Option<ReplicaHost>,
    session: Option<SessionId>,
    node_retained_bytes: Option<usize>,
    required_components: Vec<&'static str>,
    follower_store: Option<(PathBuf, ReplicaLimits, DiskBudget)>,
}

struct CellNodeParts {
    application: Arc<CompiledApplication>,
    pool: SqlWorkerPool,
    replica_host: ReplicaHost,
    session: SessionId,
    node_retained_bytes: usize,
    required_components: Vec<&'static str>,
    follower_store: Option<(PathBuf, ReplicaLimits, DiskBudget)>,
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
            required_components: Vec::new(),
            follower_store: None,
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

    /// Declares the owned production components required before readiness.
    pub fn with_required_owned_components(
        mut self,
        names: impl IntoIterator<Item = &'static str>,
    ) -> crab_cell_runtime::Result<Self> {
        append_required_components(&mut self.required_components, names)?;
        Ok(self)
    }

    /// Supplies the durable follower store that the node must retain.
    #[must_use]
    pub fn with_follower_store(
        mut self,
        root: PathBuf,
        limits: ReplicaLimits,
        disk: DiskBudget,
    ) -> Self {
        self.follower_store = Some((root, limits, disk));
        self
    }

    /// Validates all required inputs before starting any background runtime task.
    pub fn build(self) -> crab_cell_runtime::Result<CellNode> {
        let CellNodeParts {
            application,
            pool,
            replica_host,
            session,
            node_retained_bytes,
            required_components,
            follower_store,
        } = self.required_parts()?;
        let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
            pool,
            node_retained_bytes,
            session,
            replica_host,
        )?;
        let node = CellNode {
            application,
            runtime,
            state: Arc::new(Mutex::new(NodeState::Starting)),
            lease_installed: AtomicBool::new(false),
            shutdown_lock: Arc::new(tokio::sync::Mutex::new(())),
            facilities: Arc::new(Mutex::new(Vec::new())),
            required_components: Arc::new(Mutex::new(required_components)),
            task_group: Arc::new(Mutex::new(None)),
        };
        node.install_follower_store(follower_store)?;
        Ok(node)
    }

    /// Builds an unadvertised host for bounded offline maintenance.
    ///
    /// This path intentionally uses object-only runtime admission: the caller
    /// must keep the host private and may not expose serving readiness.
    pub fn build_unleased_for_maintenance(self) -> crab_cell_runtime::Result<CellNode> {
        let CellNodeParts {
            application,
            pool,
            replica_host,
            session,
            node_retained_bytes,
            required_components,
            follower_store,
        } = self.required_parts()?;
        let runtime =
            CellRuntime::new_with_replica_host(pool, node_retained_bytes, session, replica_host)?;
        let node = CellNode {
            application,
            runtime,
            state: Arc::new(Mutex::new(NodeState::Starting)),
            lease_installed: AtomicBool::new(false),
            shutdown_lock: Arc::new(tokio::sync::Mutex::new(())),
            facilities: Arc::new(Mutex::new(Vec::new())),
            required_components: Arc::new(Mutex::new(required_components)),
            task_group: Arc::new(Mutex::new(None)),
        };
        node.install_follower_store(follower_store)?;
        Ok(node)
    }

    fn required_parts(self) -> crab_cell_runtime::Result<CellNodeParts> {
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
        Ok(CellNodeParts {
            application: self.application,
            pool,
            replica_host: host,
            session,
            node_retained_bytes,
            required_components: self.required_components,
            follower_store: self.follower_store,
        })
    }
}

fn append_required_components(
    required: &mut Vec<&'static str>,
    names: impl IntoIterator<Item = &'static str>,
) -> crab_cell_runtime::Result<()> {
    let names = names.into_iter().collect::<Vec<_>>();
    if names.is_empty() || names.len() > MAX_NODE_FACILITIES {
        return Err(Error::Control(
            "CellNode required component count is out of bounds",
        ));
    }
    let mut seen = HashSet::with_capacity(names.len());
    if names
        .iter()
        .any(|name| name.is_empty() || required.contains(name) || !seen.insert(*name))
    {
        return Err(Error::Control(
            "CellNode required component names must be unique and non-empty",
        ));
    }
    if required.len().saturating_add(names.len()) > MAX_NODE_FACILITIES {
        return Err(Error::Capacity("CellNode required component limit reached"));
    }
    required.extend(names);
    Ok(())
}

/// One started application host with an ordered drain/shutdown boundary.
pub struct CellNode {
    application: Arc<CompiledApplication>,
    runtime: CellRuntime,
    state: Arc<Mutex<NodeState>>,
    lease_installed: AtomicBool,
    shutdown_lock: Arc<tokio::sync::Mutex<()>>,
    facilities: Arc<Mutex<Vec<CellNodeFacility>>>,
    required_components: Arc<Mutex<Vec<&'static str>>>,
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
            && self
                .task_group
                .lock()
                .ok()
                .and_then(|task_group| task_group.as_ref().map(|group| group.is_healthy()))
                .unwrap_or(false)
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
    ///
    /// A coordination task group must already be installed. This convenience
    /// path is intentionally fail-closed so a node can never advertise while
    /// its runtime-wide supervisors are unowned.
    pub fn install_node_lease(
        &self,
        lease: crab_cell_runtime::NodeLeaseGuard,
    ) -> crab_cell_runtime::Result<()> {
        self.require_task_group()?;
        self.install_node_lease_for_startup(lease)?;
        self.start()
    }

    /// Installs the lease without opening readiness to the product boundary.
    ///
    /// Servers use this during startup, then call [`Self::start`] only
    /// after their listeners and owned facilities have been started.
    pub fn install_node_lease_for_startup(
        &self,
        lease: crab_cell_runtime::NodeLeaseGuard,
    ) -> crab_cell_runtime::Result<()> {
        self.runtime.install_node_lease(lease)?;
        self.lease_installed.store(true, Ordering::Release);
        Ok(())
    }

    /// Declares the typed production components that must be retained before
    /// readiness can open. The declaration is immutable after startup begins;
    /// this keeps a product adapter from accidentally starting a node with a
    /// missing owner and discovering the gap only on its first request.
    pub fn require_owned_components(
        &self,
        names: impl IntoIterator<Item = &'static str>,
    ) -> crab_cell_runtime::Result<()> {
        if self.state() != NodeState::Starting {
            return Err(Error::CellDraining);
        }
        let mut required = self
            .required_components
            .lock()
            .map_err(|_| Error::Control("CellNode required-component lock poisoned"))?;
        append_required_components(&mut required, names)
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

    /// Installs the provider enrollment adapter and moves node-log recruitment
    /// and rotation into the host-owned task group.
    pub fn install_node_durability_provider<P>(
        &self,
        provider: Arc<P>,
        configuration: NodeDurabilitySupervisorConfig,
    ) -> crab_cell_runtime::Result<()>
    where
        P: NodeDurabilityProvider,
    {
        let task_group = self
            .task_group
            .lock()
            .map_err(|_| Error::Control("CellNode task group lock poisoned"))?
            .clone()
            .ok_or(Error::Control(
                "CellNode durability provider requires an installed task group",
            ))?;
        self.install_owned_component(NODE_DURABILITY_PROVIDER_COMPONENT, Arc::clone(&provider))?;
        let runtime = self.runtime.clone();
        let cancellation = task_group.cancellation.clone();
        let result = task_group.spawn_boxed(async move {
            run_node_durability_supervisor(provider, runtime, configuration, cancellation).await
        });
        if result.is_err() {
            self.remove_facility(NODE_DURABILITY_PROVIDER_COMPONENT)?;
        }
        result
    }

    /// Opens readiness after all product startup probes have completed.
    pub fn start(&self) -> crab_cell_runtime::Result<()> {
        if !self.lease_installed.load(Ordering::Acquire) {
            return Err(Error::Control(
                "CellNode cannot become ready before its node lease is installed",
            ));
        }
        self.require_task_group()?;
        if !self
            .task_group
            .lock()
            .map_err(|_| Error::Control("CellNode task group lock poisoned"))?
            .as_ref()
            .is_some_and(|task_group| task_group.is_healthy())
        {
            return Err(Error::Control("CellNode task group is unhealthy"));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Control("CellNode lifecycle lock poisoned"))?;
        if *state == NodeState::Starting {
            self.require_components_present()?;
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

    fn require_components_present(&self) -> crab_cell_runtime::Result<()> {
        let required = self
            .required_components
            .lock()
            .map_err(|_| Error::Control("CellNode required-component lock poisoned"))?
            .clone();
        if required.is_empty() {
            return Ok(());
        }
        let facilities = self
            .facilities
            .lock()
            .map_err(|_| Error::Control("CellNode facility lock poisoned"))?;
        for name in required {
            if !facilities
                .iter()
                .any(|facility| facility.name == name && facility.owner.is_some())
            {
                return Err(Error::Control("CellNode required component is missing"));
            }
        }
        Ok(())
    }

    fn remove_facility(&self, name: &'static str) -> crab_cell_runtime::Result<()> {
        let mut facilities = self
            .facilities
            .lock()
            .map_err(|_| Error::Control("CellNode facility lock poisoned"))?;
        let Some(index) = facilities.iter().position(|facility| facility.name == name) else {
            return Err(Error::Control(
                "CellNode facility rollback target is missing",
            ));
        };
        facilities.remove(index);
        Ok(())
    }

    /// Returns one coherent lifecycle and admission snapshot.
    #[must_use]
    pub fn status(&self) -> NodeStatus {
        NodeStatus {
            state: self.state(),
            shutting_down: self.is_shutting_down(),
            stats: self.stats(),
        }
    }

    fn require_task_group(&self) -> crab_cell_runtime::Result<()> {
        let installed = self
            .task_group
            .lock()
            .map_err(|_| Error::Control("CellNode task group lock poisoned"))?;
        if installed.is_none() {
            return Err(Error::Control(
                "CellNode cannot become ready before its task group is installed",
            ));
        }
        Ok(())
    }

    /// Returns whether the owned runtime has entered shutdown.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.runtime.is_shutting_down()
    }

    /// Attaches one provider-owned lifecycle component during node startup.
    pub fn install_facility(&self, facility: CellNodeFacility) -> crab_cell_runtime::Result<()> {
        self.install_facilities(std::iter::once(facility))
    }

    /// Atomically attaches a bounded batch of provider-owned facilities.
    ///
    /// All names and capacity are validated before any facility is retained, so
    /// a failed composition cannot leave the node with a partial owner set.
    /// Registration closes when readiness opens so the owner set cannot change
    /// underneath admitted requests.
    pub fn install_facilities(
        &self,
        facilities: impl IntoIterator<Item = CellNodeFacility>,
    ) -> crab_cell_runtime::Result<()> {
        let mut additions = Vec::new();
        for facility in facilities {
            if additions.len() >= MAX_NODE_FACILITIES {
                return Err(Error::Capacity("CellNode facility limit reached"));
            }
            additions.push(facility);
        }
        let state = self
            .state
            .lock()
            .map_err(|_| Error::Control("CellNode lifecycle lock poisoned"))?;
        if *state != NodeState::Starting {
            return Err(Error::CellDraining);
        }
        let mut facilities = self
            .facilities
            .lock()
            .map_err(|_| Error::Control("CellNode facility lock poisoned"))?;
        if facilities.len().saturating_add(additions.len()) > MAX_NODE_FACILITIES {
            return Err(Error::Capacity("CellNode facility limit reached"));
        }
        let mut names = facilities
            .iter()
            .map(|facility| facility.name)
            .collect::<HashSet<_>>();
        if additions
            .iter()
            .any(|facility| !names.insert(facility.name))
        {
            return Err(Error::Control("CellNode facility name already installed"));
        }
        facilities.extend(additions);
        Ok(())
    }

    /// Retains one shared composition component under the node lifecycle.
    ///
    /// Components are deliberately type-erased only inside the host. Callers
    /// retrieve them by the same stable name and concrete type, while the
    /// node remains the sole owner of the production composition boundary.
    pub fn install_owned_component<T>(
        &self,
        name: &'static str,
        component: Arc<T>,
    ) -> crab_cell_runtime::Result<()>
    where
        T: Send + Sync + 'static,
    {
        self.install_owned_component_with_drain(name, component, || async { Ok(()) })
    }

    /// Retains one component and attaches its idempotent drain callback.
    pub fn install_owned_component_with_drain<T, F, Fut>(
        &self,
        name: &'static str,
        component: Arc<T>,
        drain: F,
    ) -> crab_cell_runtime::Result<()>
    where
        T: Send + Sync + 'static,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = FacilityResult> + Send + 'static,
    {
        self.install_facility(CellNodeFacility::owned(name, component, drain)?)
    }

    fn install_follower_store(
        &self,
        configuration: Option<(PathBuf, ReplicaLimits, DiskBudget)>,
    ) -> crab_cell_runtime::Result<()> {
        let Some((root, limits, disk)) = configuration else {
            return Ok(());
        };
        let store = FollowerStore::open(root, limits, disk)?;
        self.install_owned_component(FOLLOWER_STORE_COMPONENT, Arc::new(store))
    }

    /// Looks up one node-owned component for a product adapter.
    #[must_use]
    pub fn owned_component<T>(&self, name: &str) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.facilities
            .lock()
            .ok()?
            .iter()
            .find(|facility| facility.name == name)
            .and_then(CellNodeFacility::owner)
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

    /// Runs a deterministic qualification workload while this node is ready.
    ///
    /// The typed executor remains responsible for primitive requests and
    /// verification. The host only admits a run while serving and rejects a
    /// result if shutdown or a supervisor failure removed readiness during the
    /// run, so callers cannot retain evidence from a draining node.
    pub async fn run_qualification<E>(
        &self,
        workload: &QualificationWorkload,
        executor: &mut E,
    ) -> crab_cell_runtime::Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor,
    {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        let summary = workload.run_with_case_coverage(executor).await?;
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        Ok(summary)
    }

    /// Runs an observed workload while this node is ready without asserting
    /// that every scheduled lifecycle case was exercised.
    ///
    /// This entry point is for local wiring and smoke evidence. Its result must
    /// not be promoted to a protected profile unless the resulting artifact
    /// proves the profile's required case coverage independently.
    pub async fn run_qualification_observed<E>(
        &self,
        workload: &QualificationWorkload,
        executor: &mut E,
    ) -> crab_cell_runtime::Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor,
    {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        let summary = workload.run(executor).await?;
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        Ok(summary)
    }

    /// Runs independent qualification operations with bounded concurrency.
    ///
    /// The application-specific executor must make each scheduled operation
    /// independent or idempotent. Readiness is checked before admission and
    /// after all in-flight work drains, so a result from a draining or failed
    /// node is never accepted as qualification evidence.
    pub async fn run_qualification_concurrent<E>(
        &self,
        workload: &QualificationWorkload,
        executor: E,
        concurrency: usize,
    ) -> crab_cell_runtime::Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor + Clone + Send + 'static,
    {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        let summary = workload
            .run_concurrent_with_case_coverage(executor, concurrency)
            .await?;
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        Ok(summary)
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
        let task_group = self
            .task_group
            .lock()
            .map(|task_group| task_group.clone())
            .map_err(|_| Error::Control("CellNode task group lock poisoned"));
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
        if let Some(error) = task_group.as_ref().err().map(|error| match error {
            Error::Control(message) => Error::Control(message),
            _ => Error::Control("CellNode task group unavailable during drain"),
        }) {
            first_error = Some(error);
        }
        if let Ok(Some(task_group)) = task_group.as_ref() {
            task_group.cancel();
        }
        match facilities {
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
            Ok(facilities) => {
                for (name, drain) in facilities {
                    let result = if name == "cell-coordination-tasks" {
                        // The task group is a retained owner, so its join must share the
                        // node deadline; an unbounded callback could strand shutdown.
                        match task_group.as_ref() {
                            Ok(Some(task_group)) => task_group.drain_until(deadline).await,
                            _ => drain().await,
                        }
                    } else {
                        match deadline {
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
                        }
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
        let result = if result.is_ok() {
            match self.facilities.lock() {
                Ok(mut facilities) => {
                    facilities.clear();
                    Ok(())
                }
                Err(_) => Err(Error::Control("CellNode facility lock poisoned")),
            }
        } else {
            result
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

async fn run_node_durability_supervisor<P>(
    provider: Arc<P>,
    runtime: CellRuntime,
    configuration: NodeDurabilitySupervisorConfig,
    cancellation: CancellationToken,
) -> FacilityResult
where
    P: NodeDurabilityProvider,
{
    let mut recruit = tokio::time::interval(configuration.recruit_interval);
    let mut rotation = tokio::time::interval(configuration.rotation_interval);
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            _ = recruit.tick(), if runtime.node_durability().is_none() => {
                match provider.clone().recruit(
                    configuration.limits,
                    configuration.required_follower_bytes,
                    configuration.live_node_limit,
                ).await {
                    Ok(Some(config)) => {
                        match config.build() {
                            Ok(durability) => {
                                if let Err(error) = runtime.install_node_durability(
                                    configuration.application,
                                    durability,
                                ) {
                                    provider.rotation_event(NodeDurabilityRotation::Failed);
                                    return Err(Box::new(error));
                                }
                            }
                            Err(_error) => {
                                provider.rotation_event(NodeDurabilityRotation::Failed);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(_) => provider.rotation_event(NodeDurabilityRotation::Failed),
                }
            }
            _ = rotation.tick(), if runtime.node_durability().is_some() => {
                rotate_node_durability(
                    Arc::clone(&provider),
                    runtime.clone(),
                    configuration,
                    cancellation.clone(),
                ).await?;
            }
        }
    }
}

async fn rotate_node_durability<P>(
    provider: Arc<P>,
    runtime: CellRuntime,
    configuration: NodeDurabilitySupervisorConfig,
    cancellation: CancellationToken,
) -> FacilityResult
where
    P: NodeDurabilityProvider,
{
    let Some((application, durability)) = runtime.node_durability() else {
        return Ok(());
    };
    if application != configuration.application {
        return Err(Box::new(Error::Control(
            "CellNode node durability application changed during rotation",
        )));
    }
    if !durability.needs_rotation(configuration.max_issued_frames) {
        return Ok(());
    }
    provider.rotation_event(NodeDurabilityRotation::Started);
    loop {
        match durability.shutdown().await {
            Ok(()) => break,
            Err(Error::PendingPublication) => {
                provider.rotation_event(NodeDurabilityRotation::Pending);
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    () = tokio::time::sleep(configuration.recruit_interval) => {}
                }
            }
            Err(error) => {
                provider.rotation_event(NodeDurabilityRotation::Failed);
                return Err(Box::new(error));
            }
        }
    }
    if cancellation.is_cancelled() {
        return Ok(());
    }
    let replacement = loop {
        match provider
            .clone()
            .recruit(
                configuration.limits,
                configuration.required_follower_bytes,
                configuration.live_node_limit,
            )
            .await
        {
            Ok(Some(config)) => match config.build() {
                Ok(durability) => break durability,
                Err(_) => provider.rotation_event(NodeDurabilityRotation::Failed),
            },
            Ok(None) => {}
            Err(_) => provider.rotation_event(NodeDurabilityRotation::Failed),
        }
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(configuration.recruit_interval) => {}
        }
    };
    if cancellation.is_cancelled() {
        replacement
            .shutdown()
            .await
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }
    match runtime.replace_node_durability(configuration.application, Arc::clone(&replacement)) {
        Ok(_) => {
            provider.rotation_event(NodeDurabilityRotation::Completed);
            Ok(())
        }
        Err(error) => {
            provider.rotation_event(NodeDurabilityRotation::Failed);
            replacement.shutdown().await.map_err(|shutdown_error| {
                Box::new(shutdown_error) as Box<dyn std::error::Error + Send + Sync>
            })?;
            Err(Box::new(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_cell_runtime::{
        BuildDescriptor, CatalogRole, CellModule, Digest, ModuleDescriptor, NamespaceDescriptor,
        NodeLeaseGuard, QualificationExecution, QualificationOperation,
        QualificationOperationExecutor, QualificationProfile, QualificationWorkload,
        RegistryBuilder,
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

    #[derive(Clone)]
    struct QualificationStub;

    impl QualificationOperationExecutor for QualificationStub {
        type Future<'a> = std::future::Ready<crab_cell_runtime::Result<QualificationExecution>>;

        fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
            std::future::ready(Ok(
                QualificationExecution::acknowledged(true).with_case(operation.case())
            ))
        }
    }

    #[tokio::test]
    async fn qualification_rejects_a_node_before_readiness() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([32; 16]))
            .build()
            .unwrap();
        let workload = QualificationWorkload::generate_with_size(
            &QualificationProfile::pr_contract(),
            41,
            1,
            56,
            1,
        )
        .unwrap();
        let error = node
            .run_qualification(&workload, &mut QualificationStub)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::CellDraining));
    }

    #[tokio::test]
    async fn concurrent_qualification_is_readiness_gated_and_bounded() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([35; 16]))
            .build()
            .unwrap();
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        let workload = QualificationWorkload::generate_with_size(
            &QualificationProfile::pr_contract(),
            41,
            1,
            56,
            1,
        )
        .unwrap();
        let summary = node
            .run_qualification_concurrent(&workload, QualificationStub, 2)
            .await
            .unwrap();
        assert_eq!(summary.operations(), 56);
        node.shutdown().await.unwrap();
    }

    struct ObservedQualificationStub;

    impl QualificationOperationExecutor for ObservedQualificationStub {
        type Future<'a> = std::future::Ready<crab_cell_runtime::Result<QualificationExecution>>;

        fn execute<'a>(&'a mut self, _operation: QualificationOperation) -> Self::Future<'a> {
            std::future::ready(Ok(QualificationExecution::acknowledged(true)))
        }
    }

    #[tokio::test]
    async fn observed_qualification_preserves_unclaimed_case_coverage() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([36; 16]))
            .build()
            .unwrap();
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        let workload = QualificationWorkload::generate_with_size(
            &QualificationProfile::pr_contract(),
            41,
            1,
            56,
            1,
        )
        .unwrap();
        let summary = node
            .run_qualification_observed(&workload, &mut ObservedQualificationStub)
            .await
            .unwrap();
        assert!(summary.case_coverage().iter().all(|byte| *byte == 0));
        node.shutdown().await.unwrap();
    }

    struct NoopNodeDurabilityProvider;

    impl NodeDurabilityProvider for NoopNodeDurabilityProvider {
        fn recruit(
            self: Arc<Self>,
            _limits: ReplicaLimits,
            _required_follower_bytes: u64,
            _live_node_limit: usize,
        ) -> Pin<Box<dyn Future<Output = FacilityResult<Option<NodeDurabilityConfig>>> + Send>>
        {
            Box::pin(async { Ok(None) })
        }
    }

    #[tokio::test]
    async fn node_durability_supervisor_is_host_owned_and_joined() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([28; 16]))
            .build()
            .unwrap();
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        let provider = Arc::new(NoopNodeDurabilityProvider);
        node.install_node_durability_provider(
            Arc::clone(&provider),
            NodeDurabilitySupervisorConfig::new(
                ApplicationId::from_bytes([29; 16]),
                ReplicaLimits::default(),
                1,
                1,
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(1),
                1,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            node.owned_component::<NoopNodeDurabilityProvider>(NODE_DURABILITY_PROVIDER_COMPONENT)
                .is_some()
        );
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_node_task_removes_readiness_and_is_reported_during_drain() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([30; 16]))
            .build()
            .unwrap();
        let task_group = node
            .install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        node.start().unwrap();
        assert!(node.is_ready());
        task_group
            .spawn(async { Err::<(), _>(std::io::Error::other("supervisor failed")) })
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert!(!node.is_ready());
        let error = node.drain().await.unwrap_err();
        assert!(matches!(
            error,
            Error::Facility {
                name: "cell-coordination-tasks",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn completed_node_task_removes_readiness() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([33; 16]))
            .build()
            .unwrap();
        let task_group = node
            .install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        node.start().unwrap();
        assert!(node.is_ready());

        task_group.spawn(async { Ok::<(), Error>(()) }).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while node.is_ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!node.is_ready());
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn panicked_node_task_removes_readiness_and_is_reported_during_drain() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([31; 16]))
            .build()
            .unwrap();
        let task_group = node
            .install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        node.start().unwrap();
        assert!(node.is_ready());
        task_group
            .spawn_boxed(async {
                panic!("supervisor panicked");
            })
            .unwrap();
        tokio::task::yield_now().await;
        assert!(!node.is_ready());
        let error = node.drain().await.unwrap_err();
        assert!(matches!(
            error,
            Error::Facility {
                name: "cell-coordination-tasks",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn builder_retains_configured_follower_store_as_an_owned_component() {
        let data_dir = tempfile::tempdir().unwrap();
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([3; 16]))
            .with_follower_store(
                data_dir.path().join("followers"),
                ReplicaLimits::default(),
                DiskBudget::new(1 << 20),
            )
            .build()
            .unwrap();

        assert!(
            node.owned_component::<FollowerStore>(FOLLOWER_STORE_COMPONENT)
                .is_some()
        );
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
        let starting = node.status();
        assert_eq!(starting.state(), NodeState::Starting);
        assert!(!starting.is_shutting_down());
        assert_eq!(starting.stats(), node.stats());
        assert!(node.start().is_err());
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        assert_eq!(node.state(), NodeState::Ready);
        assert!(node.is_ready());
        node.shutdown().await.unwrap();
        assert_eq!(node.state(), NodeState::Stopped);
        let stopped = node.status();
        assert_eq!(stopped.state(), NodeState::Stopped);
        assert!(stopped.is_shutting_down());
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
        assert!(node.start().is_err());
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.start().unwrap();
        assert!(node.is_ready());
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn readiness_requires_the_node_task_group() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([22; 16]))
            .build()
            .unwrap();
        node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();

        let error = node.start().unwrap_err();
        assert!(matches!(error, Error::Control(_)));
        assert_eq!(node.state(), NodeState::Starting);

        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.start().unwrap();
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
    async fn task_group_rejects_new_tasks_after_drain_starts() {
        let cancellation = CancellationToken::new();
        let node_shutdown = CancellationToken::new();
        let task_shutdown = node_shutdown.clone();
        let tasks = CellNodeTaskGroup::new(cancellation, node_shutdown);
        tasks
            .spawn(async move {
                task_shutdown.cancelled().await;
                Ok::<(), Error>(())
            })
            .unwrap();
        tasks.drain().await.unwrap();

        assert!(matches!(
            tasks.spawn(async { Ok::<(), Error>(()) }),
            Err(Error::CellDraining)
        ));
        assert!(matches!(
            tasks.spawn_boxed(async { Ok(()) }),
            Err(Error::CellDraining)
        ));
    }

    #[tokio::test]
    async fn task_group_deadline_aborts_unfinished_tasks() {
        struct DropProbe(Arc<AtomicBool>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let tasks = CellNodeTaskGroup::new(CancellationToken::new(), CancellationToken::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);
        tasks
            .spawn(async move {
                let _probe = DropProbe(task_dropped);
                std::future::pending::<()>().await;
                Ok::<(), Error>(())
            })
            .unwrap();

        let result = tasks
            .drain_until(Some(Instant::now() + std::time::Duration::from_millis(10)))
            .await;

        assert!(result.is_err());
        tokio::task::yield_now().await;
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn dropping_task_group_aborts_unjoined_tasks() {
        struct DropProbe(Arc<AtomicBool>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        {
            let tasks = CellNodeTaskGroup::new(CancellationToken::new(), CancellationToken::new());
            let task_dropped = Arc::clone(&dropped);
            let task_started = Arc::clone(&started);
            tasks
                .spawn(async move {
                    let _probe = DropProbe(task_dropped);
                    task_started.notify_one();
                    std::future::pending::<()>().await;
                    Ok::<(), Error>(())
                })
                .unwrap();
            started.notified().await;
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn node_deadline_returns_after_a_stalled_facility() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([21; 16]))
            .build()
            .unwrap();
        node.install_facility(
            CellNodeFacility::new("stalled", || async {
                std::future::pending::<FacilityResult>().await
            })
            .unwrap(),
        )
        .unwrap();

        let result = node
            .shutdown_until(Instant::now() + std::time::Duration::from_millis(10))
            .await;

        assert!(result.is_err());
        assert_eq!(node.state(), NodeState::Draining);
    }

    #[tokio::test]
    async fn node_deadline_bounds_a_stalled_coordination_task() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([23; 16]))
            .build()
            .unwrap();
        let tasks = node
            .install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        tasks
            .spawn(async {
                std::future::pending::<()>().await;
                Ok::<(), Error>(())
            })
            .unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            node.shutdown_until(Instant::now() + std::time::Duration::from_millis(10)),
        )
        .await
        .expect("deadline-aware shutdown must return");

        assert!(result.is_err());
        assert_eq!(node.state(), NodeState::Draining);
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
    async fn node_cancels_admission_before_draining_provider_facilities() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([27; 16]))
            .build()
            .unwrap();
        let cancellation = CancellationToken::new();
        node.install_task_group(cancellation.clone(), CancellationToken::new())
            .unwrap();
        let observed = Arc::new(AtomicBool::new(false));
        let facility_observed = Arc::clone(&observed);
        node.install_facility(
            CellNodeFacility::new("provider", move || {
                let cancellation = cancellation.clone();
                let facility_observed = facility_observed.clone();
                async move {
                    cancellation.cancelled().await;
                    facility_observed.store(true, Ordering::Release);
                    Ok(())
                }
            })
            .unwrap(),
        )
        .unwrap();

        node.shutdown_until(Instant::now() + std::time::Duration::from_secs(1))
            .await
            .unwrap();

        assert!(observed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn node_retains_typed_components_without_duplicate_names() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([26; 16]))
            .build()
            .unwrap();
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        let component = Arc::new(7_u64);
        let weak = Arc::downgrade(&component);
        node.install_facility(CellNodeFacility::new("unowned", || async { Ok(()) }).unwrap())
            .unwrap();
        assert!(
            node.install_owned_component("unowned", Arc::new(6_u64))
                .is_err()
        );
        node.install_owned_component("fixture-component", Arc::clone(&component))
            .unwrap();
        drop(component);
        assert_eq!(
            node.owned_component::<u64>("fixture-component").as_deref(),
            Some(&7)
        );
        assert!(
            node.install_owned_component("fixture-component", Arc::new(8_u64))
                .is_err()
        );
        node.shutdown().await.unwrap();
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn facility_batch_installation_is_atomic_on_name_conflict() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([30; 16]))
            .build()
            .unwrap();
        node.install_owned_component("existing", Arc::new(1_u64))
            .unwrap();
        let first = CellNodeFacility::owned("first", Arc::new(2_u64), || async { Ok(()) }).unwrap();
        let duplicate =
            CellNodeFacility::owned("existing", Arc::new(3_u64), || async { Ok(()) }).unwrap();

        assert!(node.install_facilities([first, duplicate]).is_err());
        assert!(node.owned_component::<u64>("first").is_none());
        assert_eq!(node.owned_component::<u64>("existing").as_deref(), Some(&1));
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn readiness_requires_declared_owned_components() {
        let node = CellNodeBuilder::new(application())
            .with_required_owned_components(["catalog", "router"])
            .unwrap()
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([28; 16]))
            .build()
            .unwrap();
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        node.install_owned_component("catalog", Arc::new(1_u64))
            .unwrap();

        assert!(matches!(node.start(), Err(Error::Control(_))));
        node.install_owned_component("router", Arc::new(2_u64))
            .unwrap();
        node.start().unwrap();
        assert!(node.is_ready());
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn facility_registration_is_frozen_after_readiness() {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([34; 16]))
            .build()
            .unwrap();
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap();
        assert!(node.is_ready());

        assert!(matches!(
            node.install_facility(CellNodeFacility::new("late", || async { Ok(()) }).unwrap()),
            Err(Error::CellDraining)
        ));
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn required_component_declaration_rejects_duplicates_and_late_changes() {
        assert!(
            CellNodeBuilder::new(application())
                .with_required_owned_components(["catalog", "catalog"])
                .is_err()
        );
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([29; 16]))
            .build()
            .unwrap();
        assert!(
            node.require_owned_components(["catalog", "catalog"])
                .is_err()
        );
        node.require_owned_components(["catalog"]).unwrap();
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
            .unwrap_err();
        assert!(node.require_owned_components(["router"]).is_ok());
        node.shutdown().await.unwrap();
        assert!(node.require_owned_components(["late"]).is_err());
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
        for index in 0..MAX_NODE_FACILITIES {
            let name = Box::leak(format!("facility-{index}").into_boxed_str());
            node.install_facility(CellNodeFacility::new(name, || async { Ok(()) }).unwrap())
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
