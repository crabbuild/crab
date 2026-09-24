//! Provider-neutral lifecycle boundary for a compiled Cell application.
//!
//! `CellNode` owns the embedded runtime and its shared admission ledger. A
//! product server supplies providers, authentication and network transports;
//! it must not construct another runtime alongside this host.

#![deny(missing_docs)]
// A panic in a filter process or FUSE path corrupts a worktree, so production
// builds deny unwrap, expect, panic, todo, and unimplemented; test builds keep
// them available.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]

pub use builder::CellNodeBuilder;
pub use durability::{
    FacilityResult, NodeDurabilityProvider, NodeDurabilityRotation, NodeDurabilitySupervisorConfig,
};
pub use facility::CellNodeFacility;
pub use node::CellNode;
pub use status::{NodeState, NodeStatus, ScaleDownStatus};
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
    time::{Duration, Instant},
};
pub use tasks::CellNodeTaskGroup;
mod builder;
mod durability;
mod facility;
mod node;
mod status;
mod tasks;

use crab_cell_app::{ApplicationHandle, CellApplication, CompiledApplication};
use crab_cell_runtime::Error;
use crab_cell_runtime::cell::actor::{CellRuntime, CellRuntimeStats};
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::client::CellClient;
use crab_cell_runtime::follower::FollowerStore;
use crab_cell_runtime::identity::{ApplicationId, CellId, SessionId, TenantId};
use crab_cell_runtime::ltx::DiskBudget;
use crab_cell_runtime::ltx::{Host as ReplicaHost, Limits as ReplicaLimits};
use crab_cell_runtime::node::durability::NodeDurabilityConfig;
use crab_cell_runtime::qualification::{
    QualificationOperationExecutor, QualificationRunSummary, QualificationWorkload,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const MAX_NODE_FACILITIES: usize = 64;
const MAX_NODE_TASKS: usize = 256;

/// Stable host-owned component name for the follower store.
pub const FOLLOWER_STORE_COMPONENT: &str = "follower-store";
/// Stable host-owned component name for the node-log enrollment provider.
pub const NODE_DURABILITY_PROVIDER_COMPONENT: &str = "node-durability-provider";
