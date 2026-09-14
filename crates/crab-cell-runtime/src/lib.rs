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
mod effects;
mod error;
mod executor;
mod identity;
mod kv;
mod publication;
mod queue;
mod registry;
mod scheduler;
mod schema;
mod sql;
mod worker;
mod workflow;

pub use actor::{CellHandle, CellRuntime};
pub use authority::{CellAuthority, VersionedControl};
pub use catalog::{CatalogEntry, CatalogProof, CatalogRole, CellCatalog};
pub use control::{Control, ControlState, Owner, RootRef, Transition};
pub use effects::{
    EffectClaim, EffectIntent, EffectLeaseOutcome, EffectState, EffectTokenSource,
    InboxApplyOutcome, InboxDelivery, SystemEffectTokens, effect_ack_delivered, effect_claim,
    effect_cleanup_terminal, effect_extend, effect_insert, effect_operation_digest, effect_retry,
    effect_validate_claim, inbox_apply, inbox_cleanup_expired,
};
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
pub use queue::{
    QueueLeaseAction, QueueLeaseOutcome, QueueMessage, QueueSendOutcome, QueueSendRequest,
    QueueState, QueueTokenSource, SystemQueueTokens, install_queue_schema, queue_apply_lease,
    queue_claim, queue_cleanup_expired, queue_send, queue_validate_claim,
};
pub use registry::{
    BuildDescriptor, CellModule, CommandContext, CommandHandler, CommandInvocation,
    MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor, OperationDescriptor, QueryContext,
    QueryHandler, QueryInvocation, Registry, RegistryBuilder, RegistryError,
};
pub use scheduler::scheduler_next_due_ms;
pub use schema::{install_runtime_schema, verify_runtime_schema};
pub use sql::{SqlBatch, SqlResultSet, SqlStatement, SqlValue, sql_batch, sql_query_batch};
pub use worker::{SqlWorkerPool, WorkerExecution};
pub use workflow::{
    ActivityClaim, ActivityCompletion, ActivityCompletionOutcome, ActivityLeaseOutcome,
    ActivitySupport, ActivityTokenSource, SystemActivityTokens, WorkflowAction, WorkflowContext,
    WorkflowDecision, WorkflowDefinition, WorkflowOutcome, WorkflowSignal, WorkflowStart,
    WorkflowStatus, install_workflow_schema, workflow_cancel, workflow_claim_activities,
    workflow_cleanup_terminal, workflow_complete_activity, workflow_extend_activity,
    workflow_fire_timer, workflow_signal, workflow_start, workflow_validate_activity_claim,
};
