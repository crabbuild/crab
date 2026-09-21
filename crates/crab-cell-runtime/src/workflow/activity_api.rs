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

use crate::{
    ApplicationId, BlockingActivityReservation, CatalogRole, CellClient, CellTarget, Command,
    CommandContext, CommandResult, Committed, Error, InvocationError, MutationIdentity, Observed,
    PendingMutation, Query, QueryContext, Receipt, RegistryBuilder, RequestId, Resolution,
    TenantId, partition_for_shard,
};

use super::{
    ActivityClaim, ActivityCompletion, ActivityCompletionOutcome, ActivityLeaseOutcome,
    ActivitySupport, SystemActivityTokens, WorkflowModule, WorkflowOutcome,
    activity::MAX_ACTIVITY_PAYLOAD_BYTES,
    api::{definition, definitions},
    workflow_claim_activities, workflow_complete_activity, workflow_definition_digest_by_run,
    workflow_extend_activity, workflow_validate_activity_claim,
};

const HANDLER_IDENTITY_LIFETIME_MS: i64 = 60_000;

/// Compile-time operation IDs for native activity execution in one Workflow module.
pub trait WorkflowActivityModule: WorkflowModule {
    const ACTIVITY_TYPES: &'static [&'static str];
    const ACTIVITY_CLAIM_COMMAND_ID: u32;
    const ACTIVITY_COMPLETE_COMMAND_ID: u32;
    const ACTIVITY_EXTEND_COMMAND_ID: u32;
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

    #[must_use]
    pub const fn run_id(&self) -> [u8; 16] {
        self.run_id
    }

    #[must_use]
    pub const fn activity_id(&self) -> [u8; 16] {
        self.activity_id
    }

    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

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

    #[must_use]
    pub fn lease_until_ms(&self) -> i64 {
        self.lease_until_ms.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn cancellation(&self) -> ActivityCancellation {
        self.cancellation.clone()
    }
}

/// Cloneable cancellation observation shared with one activity attempt.
#[derive(Clone, Default)]
pub struct ActivityCancellation(Arc<AtomicBool>);

impl ActivityCancellation {
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
    Completed(Vec<u8>),
    Failed { details: Vec<u8>, retryable: bool },
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
    const TYPE: &'static str;

    fn execute(
        context: ActivityContext,
        input: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = ActivityExecution> + Send + 'static>>;
}

/// Statically linked blocking activity implemented by trusted Rust code.
pub trait BlockingActivityHandler: Send + Sync + 'static {
    const TYPE: &'static str;

    fn execute(context: ActivityContext, input: Vec<u8>) -> ActivityExecution;
}

/// Bounded activity claim parameters supplied by the native supervisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowActivityClaimRequest {
    pub limit: u32,
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
    pub claim: ActivityClaim,
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

/// Outcome of one bounded claim, execute and completion supervisor cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityRunOutcome {
    Idle {
        receipt: Receipt,
    },
    LeaseLost {
        receipt: Receipt,
    },
    IdentityConflict {
        receipt: Receipt,
    },
    Retrying {
        due_at_ms: i64,
        receipt: Receipt,
    },
    Completed {
        workflow: WorkflowOutcome,
        receipt: Receipt,
    },
    Duplicate {
        result: Vec<u8>,
        receipt: Receipt,
    },
}

/// Failure that preserves unresolved mutation evidence from supervisor commands.
pub enum ActivitySupervisorError {
    Pending(Box<PendingMutation>),
    InvalidPublishedResult {
        receipt: Receipt,
        source: Box<Error>,
    },
    Runtime(Error),
}

impl fmt::Debug for ActivitySupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending(pending) => formatter.debug_tuple("Pending").field(pending).finish(),
            Self::InvalidPublishedResult { receipt, source } => formatter
                .debug_struct("InvalidPublishedResult")
                .field("receipt", receipt)
                .field("source", source)
                .finish(),
            Self::Runtime(error) => formatter.debug_tuple("Runtime").field(error).finish(),
        }
    }
}

impl fmt::Display for ActivitySupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending(_) => {
                formatter.write_str("activity supervisor mutation needs resolution")
            }
            Self::InvalidPublishedResult { .. } => {
                formatter.write_str("activity supervisor received an invalid published result")
            }
            Self::Runtime(error) => write!(formatter, "activity supervisor failed: {error}"),
        }
    }
}

impl std::error::Error for ActivitySupervisorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPublishedResult { source, .. } => Some(source.as_ref()),
            Self::Runtime(error) => Some(error),
            Self::Pending(_) => None,
        }
    }
}

/// Runs native activity attempts without holding a SQLite transaction across await.
pub struct ActivitySupervisor<M> {
    activities: WorkflowActivities<M>,
    lease_ms: u32,
}

impl<M: WorkflowActivityModule> ActivitySupervisor<M> {
    /// Creates a supervisor using a 5..=300 second heartbeat lease.
    pub fn new(activities: WorkflowActivities<M>, lease_ms: u32) -> crate::Result<Self> {
        if !(5_000..=300_000).contains(&lease_ms) {
            return Err(Error::Command(
                "activity supervisor lease must be in 5..=300 seconds",
            ));
        }
        Ok(Self {
            activities,
            lease_ms,
        })
    }

    /// Claims and executes at most one activity from an explicit Workflow shard.
    pub async fn run_once(
        &self,
        shard: u32,
        blocking: Option<BlockingActivityReservation>,
    ) -> std::result::Result<ActivityRunOutcome, ActivitySupervisorError> {
        let claimed = self
            .activities
            .claim(internal_identity()?, shard, self.lease_ms)
            .await
            .map_err(unexpected_invocation)?;
        let Some(mut claim) = claimed.output.into_iter().next() else {
            return Ok(ActivityRunOutcome::Idle {
                receipt: claimed.receipt,
            });
        };
        let validation = self
            .activities
            .validate(shard, claim.clone(), claimed.receipt)
            .await
            .map_err(unexpected_invocation)?;
        if !validation.output {
            return Ok(ActivityRunOutcome::LeaseLost {
                receipt: validation.receipt,
            });
        }

        let context = ActivityContext::new(&claim);
        let cancellation = context.cancellation();
        let _cancellation_guard = CancellationGuard(cancellation.clone());
        let lease_deadline = context.lease_until_ms.clone();
        let execution = self.activities.execute(
            claim.definition_digest,
            claim.activity_type.clone(),
            claim.input.clone(),
            context,
            blocking,
        );
        tokio::pin!(execution);
        let heartbeat_period = Duration::from_millis(u64::from(self.lease_ms / 3));
        let heartbeat = tokio::time::sleep(heartbeat_period);
        tokio::pin!(heartbeat);
        let execution = loop {
            tokio::select! {
                result = &mut execution => break result.map_err(ActivitySupervisorError::Runtime)?,
                () = &mut heartbeat => {
                    let extension = self.activities
                        .extend(internal_identity()?, shard, claim.clone(), self.lease_ms)
                        .await;
                    match extension {
                        Ok(committed) => match committed.output {
                            ActivityLeaseOutcome::Extended { lease_until_ms } => {
                                claim.lease_until_ms = lease_until_ms;
                                lease_deadline.store(lease_until_ms, Ordering::Release);
                            }
                            ActivityLeaseOutcome::LeaseLost => {
                                cancellation.cancel();
                                return Ok(ActivityRunOutcome::LeaseLost { receipt: committed.receipt });
                            }
                        },
                        Err(InvocationError::Rejected(committed))
                            if committed.output == ActivityLeaseOutcome::LeaseLost =>
                        {
                            cancellation.cancel();
                            return Ok(ActivityRunOutcome::LeaseLost { receipt: committed.receipt });
                        }
                        Err(error) => {
                            cancellation.cancel();
                            return Err(unexpected_invocation(error));
                        }
                    }
                    heartbeat.as_mut().reset(tokio::time::Instant::now() + heartbeat_period);
                }
            }
        };

        let (result, failed, retryable) = execution.payload();
        if result.len() > MAX_ACTIVITY_PAYLOAD_BYTES {
            return Err(ActivitySupervisorError::Runtime(Error::Command(
                "activity handler result exceeds 256 KiB",
            )));
        }
        let completion = ActivityCompletion {
            run_id: claim.run_id,
            activity_id: claim.activity_id,
            attempt: claim.attempt,
            lease_token: claim.token,
            completion_token: completion_token(&claim),
            result: result.to_vec(),
            failed,
            retryable,
        };
        let completion_identity = internal_identity()?;
        let result = self
            .activities
            .complete(completion_identity, shard, completion.clone())
            .await;
        let result = match result {
            Err(InvocationError::Pending(pending)) => {
                // Resolve the exact completion before returning or retrying: rerunning the
                // handler could repeat an external side effect after its result committed.
                let resolution = match self.activities.client.resolve(&pending).await {
                    Ok(resolution) => resolution,
                    Err(_) => return Err(ActivitySupervisorError::Pending(pending)),
                };
                match resolution {
                    Resolution::Committed(outcome) => crate::client::decode_pending::<
                        ActivityCompletionOutcome,
                    >(&pending, outcome),
                    Resolution::Absent => {
                        self.activities
                            .complete(completion_identity, shard, completion)
                            .await
                    }
                    Resolution::Unknown | Resolution::Expired => {
                        return Err(ActivitySupervisorError::Pending(pending));
                    }
                }
            }
            result => result,
        };
        let committed = match result {
            Ok(committed) => committed,
            Err(InvocationError::Rejected(committed)) => {
                return match committed.output {
                    ActivityCompletionOutcome::LeaseLost => Ok(ActivityRunOutcome::LeaseLost {
                        receipt: committed.receipt,
                    }),
                    ActivityCompletionOutcome::IdentityConflict => {
                        Ok(ActivityRunOutcome::IdentityConflict {
                            receipt: committed.receipt,
                        })
                    }
                    _ => Err(ActivitySupervisorError::Runtime(Error::Command(
                        "activity completion returned an invalid rejection",
                    ))),
                };
            }
            Err(error) => return Err(unexpected_invocation(error)),
        };
        Ok(match committed.output {
            ActivityCompletionOutcome::Applied(workflow) => ActivityRunOutcome::Completed {
                workflow,
                receipt: committed.receipt,
            },
            ActivityCompletionOutcome::Retrying { due_at_ms } => ActivityRunOutcome::Retrying {
                due_at_ms,
                receipt: committed.receipt,
            },
            ActivityCompletionOutcome::Duplicate { result } => ActivityRunOutcome::Duplicate {
                result,
                receipt: committed.receipt,
            },
            ActivityCompletionOutcome::IdentityConflict => ActivityRunOutcome::IdentityConflict {
                receipt: committed.receipt,
            },
            ActivityCompletionOutcome::LeaseLost => ActivityRunOutcome::LeaseLost {
                receipt: committed.receipt,
            },
        })
    }
}

struct CancellationGuard(ActivityCancellation);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn completion_token(claim: &ActivityClaim) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.activity-completion.v1\0");
    hasher.update(&claim.run_id);
    hasher.update(&claim.activity_id);
    hasher.update(&claim.attempt.to_be_bytes());
    let mut token = [0; 16];
    token.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    token
}

fn internal_identity() -> std::result::Result<MutationIdentity, ActivitySupervisorError> {
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| {
                ActivitySupervisorError::Runtime(Error::Command(
                    "system clock is before Unix epoch",
                ))
            })?
            .as_millis(),
    )
    .map_err(|_| {
        ActivitySupervisorError::Runtime(Error::Command("system clock exceeds i64 milliseconds"))
    })?;
    let expires_at_ms = now_ms.checked_add(HANDLER_IDENTITY_LIFETIME_MS).ok_or(
        ActivitySupervisorError::Runtime(Error::Command("activity mutation expiry overflow")),
    )?;
    let mut request_id = [0; 16];
    rand::rng().fill_bytes(&mut request_id);
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(request_id),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

fn unexpected_invocation<T>(error: InvocationError<T>) -> ActivitySupervisorError {
    match error {
        InvocationError::Pending(pending) => ActivitySupervisorError::Pending(pending),
        InvocationError::InvalidPublishedResult { receipt, source } => {
            ActivitySupervisorError::InvalidPublishedResult { receipt, source }
        }
        InvocationError::NotStarted(error) => ActivitySupervisorError::Runtime(error),
        InvocationError::Rejected(_) => ActivitySupervisorError::Runtime(Error::Command(
            "activity supervisor command was unexpectedly rejected",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_an_attempt_guard_signals_cooperative_cancellation() {
        let cancellation = ActivityCancellation::default();
        {
            let _guard = CancellationGuard(cancellation.clone());
            assert!(!cancellation.is_cancelled());
        }
        assert!(cancellation.is_cancelled());
    }
}
