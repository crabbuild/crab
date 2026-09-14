//! Embedded SQLite Cell runtime contracts for Crab services.
//!
//! This crate owns reusable Cell identities, control records, transitions,
//! runtime schema installation and the single-Cell command/pending-publication
//! executor. HTTP, authentication and provider construction remain product
//! concerns of `crab-http-server`.

mod actor;
mod authority;
mod catalog;
mod control;
mod error;
mod executor;
mod identity;
mod kv;
mod publication;
mod schema;
mod worker;

pub use actor::{CellHandle, CellRuntime};
pub use authority::{CellAuthority, VersionedControl};
pub use catalog::{CatalogEntry, CatalogProof, CatalogRole, CellCatalog};
pub use control::{Control, ControlState, Owner, RootRef, Transition};
pub use error::{Error, Result};
pub use executor::{
    CellExecutor, CommandExecution, HandlerOutcome, MutationIdentity, PendingCommit, Resolution,
    StoredOutcome,
};
pub use identity::{
    ApplicationId, CellId, CellTarget, Digest, IncarnationId, NamespaceId, RequestId, SessionId,
    TenantId, partition_for_shard, shard_for_scope,
};
pub use kv::{
    KvAtomicOutcome, KvAtomicRequest, KvCheck, KvCondition, KvEntry, KvMutation, KvMutationResult,
    KvPage, install_kv_schema, kv_atomic, kv_cleanup_expired, kv_get, kv_list,
};
pub use publication::CellPublisher;
pub use schema::{install_runtime_schema, verify_runtime_schema};
pub use worker::{SqlWorkerPool, WorkerExecution};
