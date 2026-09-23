//! Node internals for the Cell node host.

use super::*;
use crate::builder::append_required_components;
use crate::durability::run_node_durability_supervisor;

/// One started application host with an ordered drain/shutdown boundary.
pub struct CellNode {
    pub(super) application: Arc<CompiledApplication>,
    pub(super) runtime: CellRuntime,
    pub(super) session: SessionId,
    pub(super) state: Arc<Mutex<NodeState>>,
    pub(super) lease_installed: AtomicBool,
    pub(super) shutdown_lock: Arc<tokio::sync::Mutex<()>>,
    pub(super) facilities: Arc<Mutex<Vec<CellNodeFacility>>>,
    pub(super) required_components: Arc<Mutex<Vec<&'static str>>>,
    pub(super) task_group: Arc<Mutex<Option<Arc<CellNodeTaskGroup>>>>,
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
        matches!(self.state(), NodeState::Ready | NodeState::ScalingDown)
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

    /// Lists settled local Cells as advisory candidates for the fleet planner.
    pub async fn idle_transfer_candidates(
        &self,
    ) -> crab_cell_runtime::Result<Vec<(CellId, u64, i64, crab_cell_runtime::CatalogRole)>> {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        self.runtime.idle_transfer_candidates().await
    }

    /// Releases one exact settled Cell generation and waits for owner release.
    pub async fn release_idle_cell(
        &self,
        cell: CellId,
        source: SessionId,
        generation: u64,
    ) -> crab_cell_runtime::Result<()> {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        self.runtime
            .release_idle_cell(cell, source, generation)
            .await
    }

    /// Stops new Cell acquisition while retaining the lease and current owners.
    pub fn begin_scale_down(&self) -> crab_cell_runtime::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Control("CellNode lifecycle lock poisoned"))?;
        match *state {
            NodeState::Ready => {
                self.runtime.stop_acquiring()?;
                *state = NodeState::ScalingDown;
                Ok(())
            }
            NodeState::ScalingDown => Ok(()),
            _ => Err(Error::CellDraining),
        }
    }

    /// Waits for confirmed releases. An incomplete result leaves the node
    /// serving and its facilities alive for the next fleet planning window.
    pub async fn drain_for_scale_down(
        &self,
        deadline: Instant,
    ) -> crab_cell_runtime::Result<ScaleDownStatus> {
        let _shutdown = self.shutdown_lock.lock().await;
        if self.state() == NodeState::Stopped {
            return Ok(ScaleDownStatus {
                remaining_cells: 0,
                settled_candidates: 0,
                released_cells: 0,
                blocked_cells: 0,
            });
        }
        self.begin_scale_down()?;
        let mut released_cells = 0_usize;
        let mut blocked = HashSet::new();
        loop {
            let candidates = self.runtime.idle_transfer_candidates().await?;
            for (cell, generation, _, _) in candidates.iter().copied() {
                if Instant::now() >= deadline {
                    break;
                }
                let result = tokio::time::timeout_at(
                    deadline.into(),
                    self.runtime
                        .release_idle_cell(cell, self.session, generation),
                )
                .await;
                match result {
                    Ok(Ok(())) => {
                        released_cells = released_cells.saturating_add(1);
                        blocked.remove(&cell);
                    }
                    Ok(Err(_)) => {
                        blocked.insert(cell);
                    }
                    Err(_) => break,
                }
            }
            let remaining_cells = self.runtime.unreleased_cell_count().await?;
            let current_candidates = self.runtime.idle_transfer_candidates().await?;
            let settled_candidates = current_candidates.len();
            let candidate_ids = current_candidates
                .iter()
                .map(|(cell, _, _, _)| *cell)
                .collect::<HashSet<_>>();
            blocked.retain(|cell| candidate_ids.contains(cell));
            let status = ScaleDownStatus {
                remaining_cells,
                settled_candidates,
                released_cells,
                blocked_cells: blocked
                    .len()
                    .saturating_add(remaining_cells.saturating_sub(settled_candidates)),
            };
            if status.ready_to_stop() {
                if Instant::now() >= deadline {
                    return Ok(status);
                }
                self.drain_until_locked(Some(deadline)).await?;
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Ok(status);
            }
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(1));
            tokio::time::sleep(wait).await;
        }
    }

    /// Installs the product's metrics adapter before the node is advertised.
    pub fn install_telemetry(
        &self,
        telemetry: Arc<dyn crab_cell_runtime::fleet::telemetry::CellTelemetry>,
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

    pub(super) fn require_components_present(&self) -> crab_cell_runtime::Result<()> {
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

    pub(super) fn remove_facility(&self, name: &'static str) -> crab_cell_runtime::Result<()> {
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

    pub(super) fn require_task_group(&self) -> crab_cell_runtime::Result<()> {
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

    pub(super) fn install_follower_store(
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
        self.drain_until_locked(deadline).await
    }

    pub(super) async fn drain_until_locked(
        &self,
        deadline: Option<Instant>,
    ) -> crab_cell_runtime::Result<()> {
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
