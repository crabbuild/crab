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

use super::{
    MAX_ATTEMPTS, MAX_CLAIM_ITEMS, MAX_PAYLOAD_BYTES, QueueControlAction, QueueControlOutcome,
    QueueDeadLetterWriter, QueueInfo, QueueLeaseAction, QueueLeaseOutcome, QueueMessage,
    QueueSendOutcome, QueueSendRequest, QueueState, SystemQueueTokens, queue_apply_lease,
    queue_apply_lease_with_dead_letter, queue_claim, queue_claim_with_dead_letter, queue_control,
    queue_info, queue_send, queue_validate_claim,
};

const SENT_TAG: u8 = 0;
const PRODUCER_CONFLICT_TAG: u8 = 1;
const ACK_TAG: u8 = 0;
const RETRY_TAG: u8 = 1;
const EXTEND_TAG: u8 = 2;
const APPLIED_TAG: u8 = 0;
const LEASE_LOST_TAG: u8 = 1;
const PAUSE_TAG: u8 = 0;
const RESUME_TAG: u8 = 1;
const PURGE_TAG: u8 = 2;
const REDRIVE_TAG: u8 = 3;

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

impl WireValue for QueueSendRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.producer_id)?;
        encoder.write_bytes(&self.payload)?;
        encoder.write_i64(self.available_at_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            producer_id: read_fixed(decoder, "queue producer ID length")?,
            payload: decoder.read_bytes()?.to_vec(),
            available_at_ms: decoder.read_i64()?,
        })
    }
}

impl WireValue for QueueSendOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Sent { message_id } => {
                encoder.write_u8(SENT_TAG)?;
                encoder.write_bytes(message_id)
            }
            Self::ProducerConflict => encoder.write_u8(PRODUCER_CONFLICT_TAG),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            SENT_TAG => Ok(Self::Sent {
                message_id: read_fixed(decoder, "queue message ID length")?,
            }),
            PRODUCER_CONFLICT_TAG => Ok(Self::ProducerConflict),
            _ => Err(CodecError::Invalid("invalid queue send outcome tag")),
        }
    }
}

impl WireValue for QueueClaimRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u32(self.limit)?;
        encoder.write_u32(self.lease_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            limit: decoder.read_u32()?,
            lease_ms: decoder.read_u32()?,
        })
    }
}

impl WireValue for QueueMessage {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        validate_message(self)?;
        encoder.write_bytes(&self.message_id)?;
        encoder.write_bytes(&self.payload)?;
        encoder.write_bytes(&self.token)?;
        encoder.write_u32(self.attempt)?;
        encoder.write_i64(self.lease_until_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let message = Self {
            message_id: read_fixed(decoder, "queue message ID length")?,
            payload: decoder.read_bytes()?.to_vec(),
            token: read_fixed(decoder, "queue lease token length")?,
            attempt: decoder.read_u32()?,
            lease_until_ms: decoder.read_i64()?,
        };
        validate_message(&message)?;
        Ok(message)
    }
}

impl WireValue for Vec<QueueMessage> {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self.len() > MAX_CLAIM_ITEMS {
            return Err(CodecError::Invalid("too many queue messages"));
        }
        encoder.write_count(self.len())?;
        for message in self {
            message.encode(encoder)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let count = decoder.read_count()?;
        if count > MAX_CLAIM_ITEMS {
            return Err(CodecError::Invalid("too many queue messages"));
        }
        let mut messages = Vec::with_capacity(count);
        for _ in 0..count {
            messages.push(QueueMessage::decode(decoder)?);
        }
        Ok(messages)
    }
}

impl WireValue for QueueLeaseRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.message_id)?;
        encoder.write_bytes(&self.token)?;
        match self.action {
            QueueLeaseAction::Ack => encoder.write_u8(ACK_TAG),
            QueueLeaseAction::Retry { delay_ms } => {
                encoder.write_u8(RETRY_TAG)?;
                encoder.write_u32(delay_ms)
            }
            QueueLeaseAction::Extend { extension_ms } => {
                encoder.write_u8(EXTEND_TAG)?;
                encoder.write_u32(extension_ms)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let message_id = read_fixed(decoder, "queue message ID length")?;
        let token = read_fixed(decoder, "queue lease token length")?;
        let action = match decoder.read_u8()? {
            ACK_TAG => QueueLeaseAction::Ack,
            RETRY_TAG => QueueLeaseAction::Retry {
                delay_ms: decoder.read_u32()?,
            },
            EXTEND_TAG => QueueLeaseAction::Extend {
                extension_ms: decoder.read_u32()?,
            },
            _ => return Err(CodecError::Invalid("invalid queue lease action tag")),
        };
        Ok(Self {
            message_id,
            token,
            action,
        })
    }
}

impl WireValue for QueueLeaseOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Applied {
                state,
                lease_until_ms,
            } => {
                validate_lease_outcome(*state, *lease_until_ms)?;
                encoder.write_u8(APPLIED_TAG)?;
                encode_state(*state, encoder)?;
                lease_until_ms.encode(encoder)
            }
            Self::LeaseLost => encoder.write_u8(LEASE_LOST_TAG),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            APPLIED_TAG => {
                let state = decode_state(decoder)?;
                let lease_until_ms = Option::<i64>::decode(decoder)?;
                validate_lease_outcome(state, lease_until_ms)?;
                Ok(Self::Applied {
                    state,
                    lease_until_ms,
                })
            }
            LEASE_LOST_TAG => Ok(Self::LeaseLost),
            _ => Err(CodecError::Invalid("invalid queue lease outcome tag")),
        }
    }
}

impl WireValue for QueueValidateRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.claimed.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            claimed: Vec::<QueueMessage>::decode(decoder)?,
        })
    }
}

impl WireValue for QueueControlAction {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Pause => encoder.write_u8(PAUSE_TAG),
            Self::Resume => encoder.write_u8(RESUME_TAG),
            Self::Purge { limit } => {
                encoder.write_u8(PURGE_TAG)?;
                encoder.write_u32(*limit)
            }
            Self::Redrive { limit } => {
                encoder.write_u8(REDRIVE_TAG)?;
                encoder.write_u32(*limit)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            PAUSE_TAG => Ok(Self::Pause),
            RESUME_TAG => Ok(Self::Resume),
            PURGE_TAG => Ok(Self::Purge {
                limit: decoder.read_u32()?,
            }),
            REDRIVE_TAG => Ok(Self::Redrive {
                limit: decoder.read_u32()?,
            }),
            _ => Err(CodecError::Invalid("invalid queue control action tag")),
        }
    }
}

impl WireValue for QueueControlOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Paused { generation } => {
                encoder.write_u8(PAUSE_TAG)?;
                encoder.write_u64(*generation)
            }
            Self::Resumed { generation } => {
                encoder.write_u8(RESUME_TAG)?;
                encoder.write_u64(*generation)
            }
            Self::Purged { messages } => {
                encoder.write_u8(PURGE_TAG)?;
                encoder.write_u32(*messages)
            }
            Self::Redriven { messages } => {
                encoder.write_u8(REDRIVE_TAG)?;
                encoder.write_u32(*messages)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            PAUSE_TAG => Ok(Self::Paused {
                generation: decoder.read_u64()?,
            }),
            RESUME_TAG => Ok(Self::Resumed {
                generation: decoder.read_u64()?,
            }),
            PURGE_TAG => Ok(Self::Purged {
                messages: decoder.read_u32()?,
            }),
            REDRIVE_TAG => Ok(Self::Redriven {
                messages: decoder.read_u32()?,
            }),
            _ => Err(CodecError::Invalid("invalid queue control outcome tag")),
        }
    }
}

impl WireValue for QueueInfoRequest {
    fn encode(&self, _: &mut BoundedEncoder) -> Result<(), CodecError> {
        Ok(())
    }

    fn decode(_: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self)
    }
}

impl WireValue for QueueInfo {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.paused.encode(encoder)?;
        encoder.write_u64(self.generation)?;
        encoder.write_u64(self.ready)?;
        encoder.write_u64(self.leased)?;
        encoder.write_u64(self.acked)?;
        encoder.write_u64(self.dead)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            paused: bool::decode(decoder)?,
            generation: decoder.read_u64()?,
            ready: decoder.read_u64()?,
            leased: decoder.read_u64()?,
            acked: decoder.read_u64()?,
            dead: decoder.read_u64()?,
        })
    }
}

fn encode_state(state: QueueState, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
    encoder.write_u8(match state {
        QueueState::Ready => 0,
        QueueState::Leased => 1,
        QueueState::Acked => 2,
        QueueState::Dead => 3,
    })
}

fn decode_state(decoder: &mut BoundedDecoder<'_>) -> Result<QueueState, CodecError> {
    match decoder.read_u8()? {
        0 => Ok(QueueState::Ready),
        1 => Ok(QueueState::Leased),
        2 => Ok(QueueState::Acked),
        3 => Ok(QueueState::Dead),
        _ => Err(CodecError::Invalid("invalid queue state tag")),
    }
}

fn validate_message(message: &QueueMessage) -> Result<(), CodecError> {
    if message.payload.len() > MAX_PAYLOAD_BYTES
        || !(1..=MAX_ATTEMPTS).contains(&message.attempt)
        || message.lease_until_ms < 0
    {
        return Err(CodecError::Invalid("invalid queue claim"));
    }
    Ok(())
}

fn validate_lease_outcome(
    state: QueueState,
    lease_until_ms: Option<i64>,
) -> Result<(), CodecError> {
    if (state == QueueState::Leased) != lease_until_ms.is_some()
        || lease_until_ms.is_some_and(|deadline| deadline < 0)
    {
        return Err(CodecError::Invalid("inconsistent queue lease outcome"));
    }
    Ok(())
}

fn read_fixed<const N: usize>(
    decoder: &mut BoundedDecoder<'_>,
    message: &'static str,
) -> Result<[u8; N], CodecError> {
    decoder
        .read_bytes()?
        .try_into()
        .map_err(|_| CodecError::Invalid(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: WireValue + PartialEq + std::fmt::Debug>(value: T) {
        let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
        value.encode(&mut encoder).unwrap();
        let bytes = encoder.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 1024 * 1024).unwrap();
        assert_eq!(T::decode(&mut decoder).unwrap(), value);
        decoder.finish().unwrap();
    }

    #[test]
    fn queue_codecs_roundtrip_every_operation_shape() {
        roundtrip(QueueSendRequest {
            producer_id: [1; 16],
            payload: vec![0, 255],
            available_at_ms: i64::MAX,
        });
        roundtrip(QueueSendOutcome::Sent {
            message_id: [2; 16],
        });
        roundtrip(QueueSendOutcome::ProducerConflict);
        roundtrip(QueueClaimRequest {
            limit: 32,
            lease_ms: 300_000,
        });
        let message = QueueMessage {
            message_id: [3; 16],
            payload: b"job".to_vec(),
            token: [4; 16],
            attempt: 20,
            lease_until_ms: i64::MAX,
        };
        roundtrip(vec![message.clone()]);
        for action in [
            QueueLeaseAction::Ack,
            QueueLeaseAction::Retry { delay_ms: 100 },
            QueueLeaseAction::Extend {
                extension_ms: 5_000,
            },
        ] {
            roundtrip(QueueLeaseRequest {
                message_id: message.message_id,
                token: message.token,
                action,
            });
        }
        roundtrip(QueueLeaseOutcome::Applied {
            state: QueueState::Leased,
            lease_until_ms: Some(10),
        });
        roundtrip(QueueLeaseOutcome::LeaseLost);
        roundtrip(QueueValidateRequest {
            claimed: vec![message],
        });
        roundtrip(QueueControlAction::Purge { limit: 128 });
        roundtrip(QueueControlOutcome::Paused { generation: 7 });
        roundtrip(QueueInfo {
            paused: true,
            generation: 7,
            ready: 1,
            leased: 2,
            acked: 3,
            dead: 4,
        });
    }

    #[test]
    fn queue_decoder_rejects_invalid_fixed_ids_and_unbounded_claims() {
        let mut bad_id = BoundedEncoder::new(32).unwrap();
        bad_id.write_bytes(&[1; 15]).unwrap();
        bad_id.write_bytes(&[]).unwrap();
        bad_id.write_i64(0).unwrap();
        let bytes = bad_id.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 32).unwrap();
        assert!(matches!(
            QueueSendRequest::decode(&mut decoder),
            Err(CodecError::Invalid("queue producer ID length"))
        ));

        let mut too_many = BoundedEncoder::new(16).unwrap();
        too_many.write_count(MAX_CLAIM_ITEMS + 1).unwrap();
        let bytes = too_many.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 16).unwrap();
        assert!(matches!(
            Vec::<QueueMessage>::decode(&mut decoder),
            Err(CodecError::Invalid("too many queue messages"))
        ));

        let mut inconsistent = BoundedEncoder::new(16).unwrap();
        inconsistent.write_u8(APPLIED_TAG).unwrap();
        encode_state(QueueState::Acked, &mut inconsistent).unwrap();
        Some(10_i64).encode(&mut inconsistent).unwrap();
        let bytes = inconsistent.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 16).unwrap();
        assert!(matches!(
            QueueLeaseOutcome::decode(&mut decoder),
            Err(CodecError::Invalid("inconsistent queue lease outcome"))
        ));
    }
}
