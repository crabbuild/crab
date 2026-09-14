//! Embedded SQLite Cell runtime contracts for Crab services.
//!
//! This crate owns reusable Cell identities, control records, transitions,
//! runtime schema installation and the single-Cell command/pending-publication
//! executor. HTTP, authentication and provider construction remain product
//! concerns of `crab-http-server`.

mod authority;
mod control;
mod error;
mod executor;
mod identity;
mod schema;

pub use authority::{CellAuthority, VersionedControl};
pub use control::{Control, ControlState, Owner, RootRef, Transition};
pub use error::{Error, Result};
pub use executor::{
    CellExecutor, CommandExecution, HandlerOutcome, MutationIdentity, PendingCommit, StoredOutcome,
};
pub use identity::{
    ApplicationId, CellId, CellTarget, Digest, IncarnationId, NamespaceId, RequestId, SessionId,
    TenantId, partition_for_shard, shard_for_scope,
};
pub use schema::{install_runtime_schema, verify_runtime_schema};
