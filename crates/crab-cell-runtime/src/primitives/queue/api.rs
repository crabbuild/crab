use std::marker::PhantomData;

use crate::cell::catalog::CatalogRole;
use crate::client::{CellClient, Committed, InvocationError, Observed, Receipt};
use crate::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};
use crate::identity::{
    ApplicationId, CellTarget, NamespaceId, TenantId, partition_for_shard, shard_for_scope,
};
use crate::primitives::maintenance::{MaintenanceModule, register_maintenance};
use crate::registry::{Command, Query, RegistryBuilder};
use crate::registry::{CommandContext, CommandResult, QueryContext};

mod codec;

use super::{
    MAX_ATTEMPTS, MAX_CLAIM_ITEMS, MAX_PAYLOAD_BYTES, QueueControlAction, QueueControlOutcome,
    QueueDeadLetterWriter, QueueInfo, QueueLeaseAction, QueueLeaseOutcome, QueueMessage,
    QueueSendOutcome, QueueSendRequest, QueueState, SystemQueueTokens, queue_apply_lease,
    queue_apply_lease_with_dead_letter, queue_claim, queue_claim_with_dead_letter, queue_control,
    queue_info, queue_send, queue_validate_claim,
};

/// Compile-time routing contract for one Queue namespace's dead-letter target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueDeadLetterTarget {
    module: &'static str,
    namespace: NamespaceId,
    shards: u32,
    send_command_id: u32,
    codec_version: u32,
}

impl QueueDeadLetterTarget {
    #[must_use]
    pub const fn new(
        module: &'static str,
        namespace: NamespaceId,
        shards: u32,
        send_command_id: u32,
        codec_version: u32,
    ) -> Self {
        Self {
            module,
            namespace,
            shards,
            send_command_id,
            codec_version,
        }
    }

    pub(crate) const fn module(self) -> &'static str {
        self.module
    }

    pub(crate) const fn namespace(self) -> NamespaceId {
        self.namespace
    }

    pub(crate) const fn shards(self) -> u32 {
        self.shards
    }

    pub(crate) const fn send_command_id(self) -> u32 {
        self.send_command_id
    }

    pub(crate) const fn codec_version(self) -> u32 {
        self.codec_version
    }
}

/// Compile-time namespace and operation identifiers for one native Queue module.
pub trait QueueModule: MaintenanceModule {
    const NAMESPACE: NamespaceId;
    const SEND_COMMAND_ID: u32;
    const CLAIM_COMMAND_ID: u32;
    const LEASE_COMMAND_ID: u32;
    const VALIDATE_QUERY_ID: u32;
    const CONTROL_COMMAND_ID: u32;
    const INFO_QUERY_ID: u32;
}

/// Registers all typed Queue bindings contributed by one compiled module.
pub fn register_queue<M: QueueModule>(registry: &mut RegistryBuilder) -> crate::Result<()> {
    registry.bind_queue_module(
        M::MODULE,
        M::NAMESPACE,
        M::SEND_COMMAND_ID,
        M::CODEC_VERSION,
        M::QUEUE_DEAD_LETTER,
    )?;
    registry.bind_command::<QueueSendCommand<M>>()?;
    registry.bind_command::<QueueClaimCommand<M>>()?;
    registry.bind_command::<QueueLeaseCommand<M>>()?;
    registry.bind_command::<QueueControlCommand<M>>()?;
    registry.bind_query::<QueueValidateClaimQuery<M>>()?;
    registry.bind_query::<QueueInfoQuery<M>>()?;
    register_maintenance::<M>(registry)
}

/// Typed queue control command bound to immutable module operation IDs.
pub struct QueueControlCommand<M>(PhantomData<fn() -> M>);

impl<M: QueueModule> Command for QueueControlCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::CONTROL_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = QueueControlAction;
    type Output = QueueControlOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        Ok(CommandResult::Success(queue_control(
            context.primitive_transaction(),
            context.now_ms(),
            input,
        )?))
    }
}

/// Empty request for queue shard information.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueInfoRequest;

/// Typed queue shard information query.
pub struct QueueInfoQuery<M>(PhantomData<fn() -> M>);

impl<M: QueueModule> Query for QueueInfoQuery<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::INFO_QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = QueueInfoRequest;
    type Output = QueueInfo;

    fn execute(context: &mut QueryContext<'_>, _: Self::Input) -> crate::Result<Self::Output> {
        queue_info(context.primitive_connection())
    }
}

/// Typed Queue send bound to immutable module operation IDs.
pub struct QueueSendCommand<M>(PhantomData<fn() -> M>);

impl<M: QueueModule> Command for QueueSendCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::SEND_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = QueueSendRequest;
    type Output = QueueSendOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let outcome = queue_send(
            context.primitive_transaction(),
            M::NAMESPACE,
            context.now_ms(),
            &input,
        )?;
        Ok(match outcome {
            QueueSendOutcome::Sent { .. } => CommandResult::Success(outcome),
            QueueSendOutcome::ProducerConflict => CommandResult::Rejected(outcome),
        })
    }
}

/// Bounded claim parameters for one explicitly selected Queue shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueClaimRequest {
    pub limit: u32,
    pub lease_ms: u32,
}

/// Typed Queue claim bound to immutable module operation IDs.
pub struct QueueClaimCommand<M>(PhantomData<fn() -> M>);

impl<M: QueueModule> Command for QueueClaimCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::CLAIM_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = QueueClaimRequest;
    type Output = Vec<QueueMessage>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let limit = usize::try_from(input.limit)
            .map_err(|_| crate::Error::Command("queue claim limit overflow"))?;
        let mut tokens = SystemQueueTokens;
        let claimed = if let Some(target) = M::QUEUE_DEAD_LETTER {
            let now_ms = context.now_ms();
            let (transaction, effects) = context.primitive_effects()?;
            let mut writer = QueueDeadLetterWriter::new(target, effects);
            queue_claim_with_dead_letter(
                transaction,
                now_ms,
                limit,
                input.lease_ms,
                &mut tokens,
                Some(&mut writer),
            )?
        } else {
            queue_claim(
                context.primitive_transaction(),
                context.now_ms(),
                limit,
                input.lease_ms,
                &mut tokens,
            )?
        };
        Ok(CommandResult::Success(claimed))
    }
}

/// Exact lease identity and transition for one Queue message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLeaseRequest {
    pub message_id: [u8; 16],
    pub token: [u8; 16],
    pub action: QueueLeaseAction,
}

/// Typed Queue lease transition bound to immutable module operation IDs.
pub struct QueueLeaseCommand<M>(PhantomData<fn() -> M>);

impl<M: QueueModule> Command for QueueLeaseCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::LEASE_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = QueueLeaseRequest;
    type Output = QueueLeaseOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let outcome = if let Some(target) = M::QUEUE_DEAD_LETTER {
            let now_ms = context.now_ms();
            let (transaction, effects) = context.primitive_effects()?;
            let mut writer = QueueDeadLetterWriter::new(target, effects);
            queue_apply_lease_with_dead_letter(
                transaction,
                now_ms,
                input.message_id,
                input.token,
                input.action,
                Some(&mut writer),
            )?
        } else {
            queue_apply_lease(
                context.primitive_transaction(),
                context.now_ms(),
                input.message_id,
                input.token,
                input.action,
            )?
        };
        Ok(match outcome {
            QueueLeaseOutcome::Applied { .. } => CommandResult::Success(outcome),
            QueueLeaseOutcome::LeaseLost => CommandResult::Rejected(outcome),
        })
    }
}

/// Exact published claims to revalidate before native task emission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueValidateRequest {
    pub claimed: Vec<QueueMessage>,
}

/// Typed Queue claim validation bound to immutable module operation IDs.
pub struct QueueValidateClaimQuery<M>(PhantomData<fn() -> M>);

impl<M: QueueModule> Query for QueueValidateClaimQuery<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::VALIDATE_QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = QueueValidateRequest;
    type Output = bool;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        queue_validate_claim(
            context.primitive_connection(),
            context.now_ms(),
            &input.claimed,
        )
    }
}

/// Authorized native Queue capability with deterministic producer sharding.
pub struct QueueNamespace<M> {
    client: CellClient,
    tenant: TenantId,
    application: ApplicationId,
    shards: u32,
    module: PhantomData<fn() -> M>,
}

impl<M> Clone for QueueNamespace<M> {
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

impl<M: QueueModule> QueueNamespace<M> {
    /// Creates a Queue capability after validating the compiled namespace role.
    pub fn new(
        client: CellClient,
        tenant: TenantId,
        application: ApplicationId,
    ) -> crate::Result<Self> {
        let shards = client.require_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Queue)?;
        Ok(Self {
            client,
            tenant,
            application,
            shards,
            module: PhantomData,
        })
    }

    /// Sends one producer-deduplicated message to its deterministic shard.
    pub async fn send(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        request: QueueSendRequest,
    ) -> std::result::Result<Committed<QueueSendOutcome>, InvocationError<QueueSendOutcome>> {
        let target = self
            .producer_target(&request.producer_id)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<QueueSendCommand<M>>(&target, identity, request)
            .await
    }

    /// Claims a bounded message batch from one explicitly selected shard.
    pub async fn claim(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
        request: QueueClaimRequest,
    ) -> std::result::Result<Committed<Vec<QueueMessage>>, InvocationError<Vec<QueueMessage>>> {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<QueueClaimCommand<M>>(&target, identity, request)
            .await
    }

    /// Revalidates exact published leases before their payloads are emitted.
    pub async fn validate_claim(
        &self,
        shard: u32,
        claimed: Vec<QueueMessage>,
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<bool>, InvocationError<bool>> {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .query::<QueueValidateClaimQuery<M>>(&target, minimum, QueueValidateRequest { claimed })
            .await
    }

    /// Acknowledges one exact live message lease on its claimed shard.
    pub async fn ack(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
        message_id: [u8; 16],
        token: [u8; 16],
    ) -> std::result::Result<Committed<QueueLeaseOutcome>, InvocationError<QueueLeaseOutcome>> {
        self.apply_lease(identity, shard, message_id, token, QueueLeaseAction::Ack)
            .await
    }

    /// Returns one exact live lease to its shard after a bounded delay.
    pub async fn retry(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
        message_id: [u8; 16],
        token: [u8; 16],
        delay_ms: u32,
    ) -> std::result::Result<Committed<QueueLeaseOutcome>, InvocationError<QueueLeaseOutcome>> {
        self.apply_lease(
            identity,
            shard,
            message_id,
            token,
            QueueLeaseAction::Retry { delay_ms },
        )
        .await
    }

    /// Extends one exact live lease without shortening its current deadline.
    pub async fn extend(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
        message_id: [u8; 16],
        token: [u8; 16],
        extension_ms: u32,
    ) -> std::result::Result<Committed<QueueLeaseOutcome>, InvocationError<QueueLeaseOutcome>> {
        self.apply_lease(
            identity,
            shard,
            message_id,
            token,
            QueueLeaseAction::Extend { extension_ms },
        )
        .await
    }

    /// Pauses new claims while allowing live lease settlement.
    pub async fn pause(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
    ) -> std::result::Result<Committed<QueueControlOutcome>, InvocationError<QueueControlOutcome>>
    {
        self.control(identity, shard, QueueControlAction::Pause)
            .await
    }

    /// Resumes claims on one queue shard.
    pub async fn resume(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
    ) -> std::result::Result<Committed<QueueControlOutcome>, InvocationError<QueueControlOutcome>>
    {
        self.control(identity, shard, QueueControlAction::Resume)
            .await
    }

    /// Deletes a bounded batch of non-leased messages.
    pub async fn purge(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
        limit: u32,
    ) -> std::result::Result<Committed<QueueControlOutcome>, InvocationError<QueueControlOutcome>>
    {
        self.control(identity, shard, QueueControlAction::Purge { limit })
            .await
    }

    /// Returns a bounded batch of dead messages to ready state.
    pub async fn redrive(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
        limit: u32,
    ) -> std::result::Result<Committed<QueueControlOutcome>, InvocationError<QueueControlOutcome>>
    {
        self.control(identity, shard, QueueControlAction::Redrive { limit })
            .await
    }

    /// Reads aggregate control and lifecycle state for one queue shard.
    pub async fn info(
        &self,
        shard: u32,
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<QueueInfo>, InvocationError<QueueInfo>> {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .query::<QueueInfoQuery<M>>(&target, minimum, QueueInfoRequest)
            .await
    }

    async fn control(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
        action: QueueControlAction,
    ) -> std::result::Result<Committed<QueueControlOutcome>, InvocationError<QueueControlOutcome>>
    {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<QueueControlCommand<M>>(&target, identity, action)
            .await
    }

    async fn apply_lease(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        shard: u32,
        message_id: [u8; 16],
        token: [u8; 16],
        action: QueueLeaseAction,
    ) -> std::result::Result<Committed<QueueLeaseOutcome>, InvocationError<QueueLeaseOutcome>> {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<QueueLeaseCommand<M>>(
                &target,
                identity,
                QueueLeaseRequest {
                    message_id,
                    token,
                    action,
                },
            )
            .await
    }

    fn producer_target(&self, producer_id: &[u8; 16]) -> crate::Result<CellTarget> {
        let shard = shard_for_scope(M::NAMESPACE, producer_id, self.shards)?;
        self.shard_target(shard)
    }

    fn shard_target(&self, shard: u32) -> crate::Result<CellTarget> {
        if shard >= self.shards {
            return Err(crate::Error::Identity("queue shard outside namespace"));
        }
        CellTarget::new(
            self.tenant,
            self.application,
            M::NAMESPACE,
            &partition_for_shard(shard),
        )
    }
}
