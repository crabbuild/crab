//! Status internals for the Cell node host.

use super::*;

/// Node lifecycle state visible to readiness and shutdown adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    Starting,
    Ready,
    ScalingDown,
    Draining,
    Stopped,
}

/// Progress while a node serves the Cells that cannot yet move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScaleDownStatus {
    pub remaining_cells: usize,
    pub settled_candidates: usize,
    pub released_cells: usize,
    pub blocked_cells: usize,
}

impl ScaleDownStatus {
    /// Reports confirmed local release; receiver service is a separate proof.
    #[must_use]
    pub const fn ready_to_stop(self) -> bool {
        self.remaining_cells == 0
    }
}

/// Point-in-time lifecycle and admission status for one [`CellNode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeStatus {
    pub(super) state: NodeState,
    pub(super) shutting_down: bool,
    pub(super) stats: CellRuntimeStats,
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
