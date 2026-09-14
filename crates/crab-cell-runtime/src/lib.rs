//! Embedded SQLite Cell runtime contracts for Crab services.
//!
//! This crate owns reusable Cell identities, control records, transitions and
//! runtime schema installation. HTTP, authentication and provider construction
//! remain product concerns of `crab-http-server`.

mod authority;
mod control;
mod error;
mod identity;
mod schema;

pub use authority::{CellAuthority, VersionedControl};
pub use control::{Control, ControlState, Owner, RootRef, Transition};
pub use error::{Error, Result};
pub use identity::{
    ApplicationId, CellId, CellTarget, Digest, IncarnationId, NamespaceId, RequestId, SessionId,
    TenantId, partition_for_shard, shard_for_scope,
};
pub use schema::{install_runtime_schema, verify_runtime_schema};
