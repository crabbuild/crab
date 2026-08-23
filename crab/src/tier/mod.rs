//! Tiering subsystem — lifecycle rule generation, restore orchestration,
//! and storage-class metadata.
//!
//! Generates provider-specific lifecycle rules (`crab tier plan`),
//! applies them via CAS (`--apply`), rolls back from backups, and
//! orchestrates archive-class restores so `crab hydrate` works
//! transparently against cold xorbs.
//!
//! # Submodules
//!
//! - `provider/` — `LifecycleProvider` + `RestoreBackend` traits and
//!   per-provider impls (S3, GCS, Azure).
//! - `classes` — `StorageClass` enum, min-retention, min-object-size.
//!
//! # Additional submodules
//!
//! - `apply.rs` — CAS apply, backup writer, rollback.
//! - `restore.rs` — `RestoreOrchestrator` state machine.
//! - `audit_shim.rs` — audit event bridge for tier, xorb optimization, and GC.

pub mod apply;
pub mod audit_shim;
pub mod classes;
pub mod conflict;
pub mod plan;
pub mod provider;
pub mod restore;
pub mod runtime;

pub use classes::StorageClass;
pub use plan::BucketProbe;
pub use provider::{
    Format, Guard, HeadMeta, LifecycleProvider, ObjectPath, Provider, PutOutcome,
    RenderedLifecycle, RestoreBackend, RestoreHandle, RestoreState, RestoreTier, TierPlan,
    TierRule, Transition,
};
