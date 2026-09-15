//! Embedded SQLite Cell runtime contracts for Crab services.
//!
//! This crate owns reusable Cell identities, control records, transitions,
//! runtime schema installation and the single-Cell command/pending-publication
//! executor. HTTP, authentication and provider construction remain product
//! concerns of `crab-http-server`.

mod activity_pool;
mod actor;
mod application;
mod authority;
mod catalog;
mod client;
mod codec;
mod control;
mod effects;
mod error;
mod executor;
mod identity;
mod kv;
mod maintenance;
mod node;
mod peer;
mod publication;
mod queue;
mod registry;
mod release;
mod release_progress;
mod scheduler;
mod schema;
mod sql;
mod worker;
mod workflow;

pub use activity_pool::{BlockingActivityPool, BlockingActivityReservation};
pub use actor::{CellHandle, CellRuntime, MigratedCell, NodeByteReservation};
pub use application::{ApplicationIdentity, ApplicationIdentityStore};
pub use authority::{CellAuthority, VersionedControl};
pub use catalog::{
    CatalogEntry, CatalogProof, CatalogRole, CatalogScanPage, CatalogShardScan, CellCatalog,
};
pub use client::{
    CellClient, CellDescription, Committed, InvocationError, Observed, PendingMutation, Receipt,
    command_operation_digest,
};
pub use codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};
pub use control::{Control, ControlState, Owner, RootRef, Transition};
pub use crab_ltx::{CellReplica, Limits as ReplicaLimits};
pub use effects::{
    EffectAckRequest, EffectBatch, EffectClaim, EffectClaimCommand, EffectClaimRequest,
    EffectCommandIntent, EffectLease, EffectLeaseCommand, EffectLeaseOutcome, EffectLeaseRequest,
    EffectModule, EffectRunOutcome, EffectSource, EffectState, EffectSupervisor,
    EffectSupervisorError, EffectTokenSource, EffectValidateClaimQuery, EffectValidateRequest,
    InboxApplyOutcome, InboxDelivery, SystemEffectTokens, effect_ack_delivered, effect_claim,
    effect_cleanup_terminal, effect_extend, effect_id, effect_operation_digest, effect_retry,
    effect_validate_claim, inbox_apply, inbox_cleanup_expired, inbox_resolve,
    register_effect_delivery,
};
pub use error::{Error, Result};
pub use executor::{
    CellExecutor, CommandExecution, HandlerOutcome, MigrationOutcome, MutationIdentity,
    PendingCommit, PendingMigration, Resolution, StoredOutcome,
};
pub use identity::{
    ApplicationId, CellId, CellTarget, Digest, IncarnationId, NamespaceId, RequestId, SessionId,
    TenantId, partition_for_shard, shard_for_scope,
};
pub use kv::{
    KvAtomicCommand, KvAtomicOutcome, KvAtomicRequest, KvCheck, KvCondition, KvEntry, KvGetQuery,
    KvGetRequest, KvListQuery, KvListRequest, KvModule, KvMutation, KvMutationResult, KvNamespace,
    KvPage, install_kv_schema, kv_atomic, kv_cleanup_expired, kv_get, kv_list, register_kv,
};
pub use maintenance::{
    MaintenanceModule, MaintenanceTickCommand, MaintenanceTickOutcome, MaintenanceTickRequest,
    register_maintenance,
};
pub use node::{NodeAdvertisement, NodeCapacity, NodeDirectory, VersionedNodeAdvertisement};
pub use peer::{
    EffectPeerClient, MAX_PEER_REQUEST_BYTES, MigrationPeerClient, PeerAuthorizer,
    PeerCellResolver, PeerDispatcher, PeerOperation, PeerPrincipal, PeerRoundTrip, PeerSigner,
    PeerVerifier, VerifiedPeerRequest, claimed_peer_session, decode_peer_reply, encode_peer_reply,
    wire as peer_wire,
};
pub use publication::CellPublisher;
pub use queue::{
    QueueClaimCommand, QueueClaimRequest, QueueDeadLetterTarget, QueueLeaseAction,
    QueueLeaseCommand, QueueLeaseOutcome, QueueLeaseRequest, QueueMessage, QueueModule,
    QueueNamespace, QueueSendCommand, QueueSendOutcome, QueueSendRequest, QueueState,
    QueueTokenSource, QueueValidateClaimQuery, QueueValidateRequest, SystemQueueTokens,
    install_queue_schema, queue_apply_lease, queue_claim, queue_cleanup_expired, queue_send,
    queue_validate_claim, register_queue,
};
pub use registry::{
    BuildDescriptor, CellModule, Command, CommandContext, CommandInvocation, CommandResult,
    MigrationDescriptor, MigrationPlan, ModuleDescriptor, NamespaceDescriptor, OperationDescriptor,
    Query, QueryContext, QueryInvocation, Registry, RegistryBuilder, RegistryError,
    RetainedCodeDescriptor,
};
pub use release::{ReleaseRecord, ReleaseState, ReleaseStore, VersionedRelease};
pub use release_progress::{
    MigrationFailure, MigrationProgress, MigrationProgressAttempt, MigrationProgressState,
    MigrationProgressStore,
};
pub use scheduler::{
    DueCell, DueCellScan, SchedulerFleet, SchedulerTickOutcome, preferred_scanner,
    scheduler_next_due_ms, scheduler_tick,
};
pub use schema::{install_runtime_schema, verify_runtime_schema};
pub use sql::{
    SqlBatch, SqlBatchCommand, SqlBatchQuery, SqlCell, SqlModule, SqlResultSet, SqlStatement,
    SqlValue, register_sql, sql_batch, sql_query_batch,
};
pub use worker::{
    ACTIVE_CELL_FILE_DESCRIPTORS, ACTIVE_CELL_PAGE_CACHE_BYTES, SqlWorkerPool, WorkerExecution,
};
pub use workflow::{
    ActivityCancellation, ActivityClaim, ActivityCompletion, ActivityCompletionOutcome,
    ActivityContext, ActivityExecution, ActivityHandler, ActivityLeaseOutcome, ActivityRunOutcome,
    ActivitySupervisor, ActivitySupervisorError, ActivitySupport, ActivityTokenSource,
    BlockingActivityHandler, MAX_ACTIVITY_PAYLOAD_BYTES, SystemActivityTokens, WorkflowAction,
    WorkflowActivities, WorkflowActivityClaimCommand, WorkflowActivityClaimRequest,
    WorkflowActivityCompleteCommand, WorkflowActivityExtendCommand, WorkflowActivityExtendRequest,
    WorkflowActivityModule, WorkflowActivityValidateQuery, WorkflowActivityValidateRequest,
    WorkflowCancelCommand, WorkflowContext, WorkflowDecision, WorkflowDefinition, WorkflowGetQuery,
    WorkflowGetRequest, WorkflowModule, WorkflowNamespace, WorkflowOutcome, WorkflowRun,
    WorkflowSignal, WorkflowSignalCommand, WorkflowStart, WorkflowStartCommand, WorkflowStatus,
    install_workflow_schema, register_activity, register_blocking_activity, register_workflow,
    register_workflow_activities, workflow_cancel, workflow_claim_activities,
    workflow_cleanup_terminal, workflow_complete_activity, workflow_extend_activity,
    workflow_fire_timer, workflow_signal, workflow_start, workflow_state,
    workflow_validate_activity_claim,
};
