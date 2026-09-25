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
    /// Returns one coherent lifecycle and admission snapshot.
    #[must_use]
    pub fn status(&self) -> NodeStatus {
        NodeStatus {
            state: self.state(),
            shutting_down: self.is_shutting_down(),
            stats: self.stats(),
        }
    }
    /// Returns whether the owned runtime has entered shutdown.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.runtime.is_shutting_down()
    }
    /// Binds a product-created typed client to this application's tenant scope.
    ///
    /// Rejects a client built from a different compiled registry.
    pub fn application_handle<A: CellApplication>(
        &self,
        client: CellClient,
        tenant: TenantId,
        application: ApplicationId,
    ) -> crab_cell_runtime::Result<ApplicationHandle<A>> {
        ApplicationHandle::new(client, Arc::clone(&self.application), tenant, application)
    }
}

mod components;
mod lifecycle;
mod qualification;
mod scale_down;
