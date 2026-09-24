//! Embedded SQLite Cell runtime contracts for Crab services.
//!
//! This crate owns reusable Cell identities, control records, transitions,
//! runtime schema installation and the single-Cell command/pending-publication
//! executor. HTTP, authentication and provider construction remain product
//! concerns of `crab-http-server`.

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

mod coordination;
mod error;
mod retry;

// Test-only object-store instrumentation. Integration targets and the
// `cell_movement_probe` binary enable the feature; in-crate unit tests do not
// depend on it, so the module never enters a default build.
#[cfg(feature = "test-support")]
pub mod test_support;

pub mod cell;
pub mod client;
pub mod codec;
pub mod control;
pub mod fleet;
pub mod follower;
pub mod identity;
pub mod ltx;
pub mod node;
pub mod peer;
pub mod primitives;
pub mod publication;
pub mod qualification;
pub mod recovery;
pub mod registry;

pub use cell::actor::{CellRuntime, CellRuntimeStats};

pub use cell::catalog::CatalogRole;
pub use cell::executor::{MutationIdentity, Resolution};

pub use cell::worker::SqlWorkerPool;
pub use client::{
    CellClient, Committed, InvocationError, Observed, PendingMutation, PreparedCommand, Receipt,
};

pub use error::{Error, Result};

pub use follower::FollowerStore;
pub use identity::{
    ApplicationId, CellId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
    partition_for_shard, shard_for_scope,
};
pub use node::durability::NodeDurabilityConfig;
pub use node::lease::NodeLeaseGuard;

pub use primitives::blob::{BlobArtifactStore, BlobModule, BlobNamespace};
pub use primitives::cron::{CronModule, CronNamespace};
pub use primitives::effects::{EffectModule, EffectSource};
pub use primitives::kv::{KvModule, KvNamespace};

pub use primitives::queue::{QueueModule, QueueNamespace};
pub use primitives::sql::{SqlCell, SqlModule};
pub use primitives::workflow::{
    WorkflowActivities, WorkflowActivityModule, WorkflowModule, WorkflowNamespace,
};

pub use qualification::{
    QualificationExecution, QualificationOperation, QualificationOperationExecutor,
    QualificationProfile, QualificationRunSummary, QualificationWorkload,
};

pub use registry::{
    BuildDescriptor, CellModule, Command, MigrationDescriptor, ModuleDescriptor,
    NamespaceDescriptor, Query, Registry, RegistryBuilder,
};
