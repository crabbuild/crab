//! Status internals for the Cell node host.

use super::*;

/// Node lifecycle state visible to readiness and shutdown adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// The node is installing its runtime and does not serve yet.
    Starting,
    /// The node serves Cells and may take new ownership.
    Ready,
    /// Scale-down started: the node still serves but takes no new ownership.
    ScalingDown,
    /// The node is releasing its Cells and refuses new work.
    Draining,
    /// Every Cell is released and the node's facilities are stopped.
    Stopped,
}

/// Progress while a node serves the Cells that cannot yet move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScaleDownStatus {
    /// Cells the node still owns, whether or not they can move yet.
    pub remaining_cells: usize,
    /// Cells the fleet still lists as transfer candidates for this node.
    pub settled_candidates: usize,
    /// Cells confirmed released during this planning window.
    pub released_cells: usize,
    /// Cells that cannot move yet: failed release attempts plus remaining
    /// Cells with no settled candidate.
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
