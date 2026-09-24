use std::{
    fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rand::RngCore;

use crate::Error;
use crate::cell::catalog::CatalogRole;
use crate::cell::executor::{MutationIdentity, Resolution};
use crate::client::{CellClient, Committed, InvocationError, Observed, PendingMutation, Receipt};
use crate::identity::RequestId;
use crate::identity::{ApplicationId, CellTarget, TenantId, partition_for_shard};
use crate::primitives::activity_pool::BlockingActivityReservation;
use crate::registry::{Command, Query, RegistryBuilder};
use crate::registry::{CommandContext, CommandResult, QueryContext};

use super::{
    ActivityClaim, ActivityCompletion, ActivityCompletionOutcome, ActivityLeaseOutcome,
    ActivitySupport, SystemActivityTokens, WorkflowModule, WorkflowOutcome,
    activity::{MAX_ACTIVITY_PAYLOAD_BYTES, MAX_LEASE_MS},
    api::{definition, definitions},
    workflow_claim_activities, workflow_complete_activity, workflow_definition_digest_by_run,
    workflow_extend_activity, workflow_validate_activity_claim,
};

const HANDLER_IDENTITY_LIFETIME_MS: i64 = 60_000;

/// Compile-time operation IDs for native activity execution in one Workflow module.
pub trait WorkflowActivityModule: WorkflowModule {
    /// Registered activity types the module serves.
    const ACTIVITY_TYPES: &'static [&'static str];
    /// Command id that claims activity leases.
    const ACTIVITY_CLAIM_COMMAND_ID: u32;
    /// Command id that applies activity completions.
    const ACTIVITY_COMPLETE_COMMAND_ID: u32;
    /// Command id that extends activity leases.
    const ACTIVITY_EXTEND_COMMAND_ID: u32;
    /// Query id that revalidates claimed activity leases.
    const ACTIVITY_VALIDATE_QUERY_ID: u32;
}

/// Registers the typed claim, completion, extension and validation bindings.
pub fn register_workflow_activities<M: WorkflowActivityModule>(
    registry: &mut RegistryBuilder,
) -> crate::Result<()> {
    for definition in definitions::<M>()? {
        registry.bind_activity_inventory(M::MODULE, definition.digest(), M::ACTIVITY_TYPES)?;
    }
    registry.bind_activity_runner::<M>()?;
    registry.bind_command::<WorkflowActivityClaimCommand<M>>()?;
    registry.bind_command::<WorkflowActivityCompleteCommand<M>>()?;
    registry.bind_command::<WorkflowActivityExtendCommand<M>>()?;
    registry.bind_query::<WorkflowActivityValidateQuery<M>>()
}

/// Registers one native handler for a module definition and activity type.
pub fn register_activity<M: WorkflowModule, A: ActivityHandler>(
    registry: &mut RegistryBuilder,
) -> crate::Result<()> {
    for definition in definitions::<M>()? {
        registry.bind_activity::<A>(M::MODULE, definition.digest())?;
    }
    Ok(())
}

/// Registers one trusted blocking handler on the node-owned activity pool.
pub fn register_blocking_activity<M: WorkflowModule, A: BlockingActivityHandler>(
    registry: &mut RegistryBuilder,
) -> crate::Result<()> {
    for definition in definitions::<M>()? {
        registry.bind_blocking_activity::<A>(M::MODULE, definition.digest())?;
    }
    Ok(())
}

/// Cooperative cancellation and lease state supplied to native activity code.
#[derive(Clone)]
pub struct ActivityContext {
    run_id: [u8; 16],
    activity_id: [u8; 16],
    attempt: u32,
    lease_token: [u8; 16],
    cancellation: ActivityCancellation,
    lease_until_ms: Arc<AtomicI64>,
}

impl ActivityContext {
    fn new(claim: &ActivityClaim) -> Self {
        Self {
            run_id: claim.run_id,
            activity_id: claim.activity_id,
            attempt: claim.attempt,
            lease_token: claim.token,
            cancellation: ActivityCancellation::default(),
            lease_until_ms: Arc::new(AtomicI64::new(claim.lease_until_ms)),
        }
    }

    /// Returns the workflow run the claim belongs to.
    #[must_use]
    pub const fn run_id(&self) -> [u8; 16] {
        self.run_id
    }

    /// Returns the activity identity within the run.
    #[must_use]
    pub const fn activity_id(&self) -> [u8; 16] {
        self.activity_id
    }

    /// Returns the delivery attempt this claim represents.
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the lease token required to extend or complete.
    #[must_use]
    pub const fn lease_token(&self) -> [u8; 16] {
        self.lease_token
    }

    /// Returns a stable external idempotency key shared by every retry attempt.
    #[must_use]
    pub fn idempotency_key(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab.activity-idempotency.v1\0");
        hasher.update(&self.run_id);
        hasher.update(&self.activity_id);
        *hasher.finalize().as_bytes()
    }

    /// Returns the logical time the lease expires.
    #[must_use]
    pub fn lease_until_ms(&self) -> i64 {
        self.lease_until_ms.load(Ordering::Acquire)
    }

    /// Returns a handle the handler can poll for cancellation.
    #[must_use]
    pub fn cancellation(&self) -> ActivityCancellation {
        self.cancellation.clone()
    }
}

/// Cloneable cancellation observation shared with one activity attempt.
#[derive(Clone, Default)]
pub struct ActivityCancellation(Arc<AtomicBool>);

impl ActivityCancellation {
    /// Reports whether the activity was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Native handler result converted to one durable completion transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityExecution {
    /// The handler completed with this result.
    Completed(Vec<u8>),
    /// The handler failed.
    Failed {
        /// Failure details recorded for the run.
        details: Vec<u8>,
        /// Whether the workflow may retry the activity.
        retryable: bool,
    },
}

impl ActivityExecution {
    fn payload(&self) -> (&[u8], bool, bool) {
        match self {
            Self::Completed(result) => (result, false, false),
            Self::Failed { details, retryable } => (details, true, *retryable),
        }
    }
}

/// Statically linked asynchronous activity implemented by trusted Rust code.
pub trait ActivityHandler: Send + Sync + 'static {
    /// Activity type this handler serves.
    const TYPE: &'static str;

    /// Runs the handler for one claim.
    fn execute(
        context: ActivityContext,
        input: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = ActivityExecution> + Send + 'static>>;
}

/// Statically linked blocking activity implemented by trusted Rust code.
pub trait BlockingActivityHandler: Send + Sync + 'static {
    /// Activity type this handler serves.
    const TYPE: &'static str;

    /// Runs the blocking handler on the dedicated worker.
    fn execute(context: ActivityContext, input: Vec<u8>) -> ActivityExecution;
}

/// Bounded activity claim parameters supplied by the native supervisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowActivityClaimRequest {
    /// Maximum activities to claim.
    pub limit: u32,
    /// Lease duration granted to each claim.
    pub lease_ms: u32,
}

/// Typed activity claim bound to one Workflow module.
pub struct WorkflowActivityClaimCommand<M>(PhantomData<fn() -> M>);

impl<M: WorkflowActivityModule> Command for WorkflowActivityClaimCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::ACTIVITY_CLAIM_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = WorkflowActivityClaimRequest;
    type Output = Vec<ActivityClaim>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let limit = usize::try_from(input.limit)
            .map_err(|_| Error::Command("activity claim limit overflow"))?;
        let supported = definitions::<M>()?
            .iter()
            .flat_map(|definition| {
                M::ACTIVITY_TYPES
                    .iter()
                    .map(|activity_type| ActivitySupport {
                        activity_type: (*activity_type).to_owned(),
                        definition_digest: definition.digest(),
                    })
            })
            .collect::<Vec<_>>();
        let mut tokens = SystemActivityTokens;
        Ok(CommandResult::Success(workflow_claim_activities(
            context.primitive_transaction(),
            context.now_ms(),
            limit,
            input.lease_ms,
            &supported,
            &mut tokens,
        )?))
    }
}

/// Typed activity completion bound to its pinned Workflow definition.
pub struct WorkflowActivityCompleteCommand<M>(PhantomData<fn() -> M>);

impl<M: WorkflowActivityModule> Command for WorkflowActivityCompleteCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::ACTIVITY_COMPLETE_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = ActivityCompletion;
    type Output = ActivityCompletionOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let source = context.target().clone();
        let Some(digest) =
            workflow_definition_digest_by_run(context.primitive_transaction(), input.run_id)?
        else {
            return Ok(CommandResult::Rejected(
                ActivityCompletionOutcome::LeaseLost,
            ));
        };
        let outcome = workflow_complete_activity(
            context.primitive_transaction(),
            &source,
            context.now_ms(),
            &input,
            definition::<M>(digest)?,
        )?;
        Ok(match outcome {
            ActivityCompletionOutcome::Applied(_)
            | ActivityCompletionOutcome::Retrying { .. }
            | ActivityCompletionOutcome::Duplicate { .. } => CommandResult::Success(outcome),
            ActivityCompletionOutcome::IdentityConflict | ActivityCompletionOutcome::LeaseLost => {
                CommandResult::Rejected(outcome)
            }
        })
    }
}

/// Exact lease and requested extension for one activity attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowActivityExtendRequest {
    /// Claim whose lease is extended.
    pub claim: ActivityClaim,
    /// Additional lease time to grant.
    pub extension_ms: u32,
}

/// Typed heartbeat extension bound to one Workflow module.
pub struct WorkflowActivityExtendCommand<M>(PhantomData<fn() -> M>);

impl<M: WorkflowActivityModule> Command for WorkflowActivityExtendCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::ACTIVITY_EXTEND_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = WorkflowActivityExtendRequest;
    type Output = ActivityLeaseOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let outcome = workflow_extend_activity(
            context.primitive_transaction(),
            context.now_ms(),
            &input.claim,
            input.extension_ms,
        )?;
        Ok(match outcome {
            ActivityLeaseOutcome::Extended { .. } => CommandResult::Success(outcome),
            ActivityLeaseOutcome::LeaseLost => CommandResult::Rejected(outcome),
        })
    }
}

/// Exact published claim set revalidated before native execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowActivityValidateRequest {
    /// Claims to revalidate before native emission.
    pub claimed: Vec<ActivityClaim>,
}

/// Typed post-publication activity validation query.
pub struct WorkflowActivityValidateQuery<M>(PhantomData<fn() -> M>);

impl<M: WorkflowActivityModule> Query for WorkflowActivityValidateQuery<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::ACTIVITY_VALIDATE_QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = WorkflowActivityValidateRequest;
    type Output = bool;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        workflow_validate_activity_claim(
            context.primitive_connection(),
            context.now_ms(),
            &input.claimed,
        )
    }
}

/// Per-namespace capability used only by the native activity supervisor.
pub struct WorkflowActivities<M> {
    client: CellClient,
    tenant: TenantId,
    application: ApplicationId,
    shards: u32,
    module: PhantomData<fn() -> M>,
}

impl<M> Clone for WorkflowActivities<M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            tenant: self.tenant,
            application: self.application,
            shards: self.shards,
            module: PhantomData,
        }
    }
}

impl<M: WorkflowActivityModule> WorkflowActivities<M> {
    /// Creates a native activity capability from the immutable compiled registry.
    pub fn new(
        client: CellClient,
        tenant: TenantId,
        application: ApplicationId,
    ) -> crate::Result<Self> {
        let shards = client.require_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Workflow)?;
        for definition in definitions::<M>()? {
            client.activity_support(M::MODULE, definition.digest())?;
        }
        Ok(Self {
            client,
            tenant,
            application,
            shards,
            module: PhantomData,
        })
    }

    async fn claim(
        &self,
        identity: MutationIdentity,
        shard: u32,
        lease_ms: u32,
    ) -> std::result::Result<Committed<Vec<ActivityClaim>>, InvocationError<Vec<ActivityClaim>>>
    {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<WorkflowActivityClaimCommand<M>>(
                &target,
                identity,
                WorkflowActivityClaimRequest { limit: 1, lease_ms },
            )
            .await
    }

    async fn validate(
        &self,
        shard: u32,
        claim: ActivityClaim,
        minimum: Receipt,
    ) -> std::result::Result<Observed<bool>, InvocationError<bool>> {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .query::<WorkflowActivityValidateQuery<M>>(
                &target,
                Some(minimum),
                WorkflowActivityValidateRequest {
                    claimed: vec![claim],
                },
            )
            .await
    }

    async fn extend(
        &self,
        identity: MutationIdentity,
        shard: u32,
        claim: ActivityClaim,
        extension_ms: u32,
    ) -> std::result::Result<Committed<ActivityLeaseOutcome>, InvocationError<ActivityLeaseOutcome>>
    {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<WorkflowActivityExtendCommand<M>>(
                &target,
                identity,
                WorkflowActivityExtendRequest {
                    claim,
                    extension_ms,
                },
            )
            .await
    }

    async fn complete(
        &self,
        identity: MutationIdentity,
        shard: u32,
        completion: ActivityCompletion,
    ) -> std::result::Result<
        Committed<ActivityCompletionOutcome>,
        InvocationError<ActivityCompletionOutcome>,
    > {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<WorkflowActivityCompleteCommand<M>>(&target, identity, completion)
            .await
    }

    async fn execute(
        &self,
        definition: crate::Digest,
        activity_type: String,
        input: Vec<u8>,
        context: ActivityContext,
        blocking: Option<BlockingActivityReservation>,
    ) -> crate::Result<ActivityExecution> {
        self.client
            .execute_activity(
                M::MODULE,
                definition,
                &activity_type,
                context,
                input,
                blocking,
            )
            .await
    }

    fn shard_target(&self, shard: u32) -> crate::Result<CellTarget> {
        if shard >= self.shards {
            return Err(Error::Identity("Workflow shard outside namespace"));
        }
        CellTarget::new(
            self.tenant,
            self.application,
            M::NAMESPACE,
            &partition_for_shard(shard),
        )
    }
}

mod supervisor;

pub use supervisor::{ActivityRunOutcome, ActivitySupervisor, ActivitySupervisorError};
