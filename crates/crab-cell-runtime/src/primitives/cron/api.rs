use std::marker::PhantomData;

use crate::cell::catalog::CatalogRole;
use crate::client::{CellClient, Committed, InvocationError, Observed, Receipt};
use crate::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue, read_fixed};
use crate::identity::{
    ApplicationId, CellTarget, NamespaceId, TenantId, partition_for_shard, shard_for_scope,
};
use crate::primitives::maintenance::{MaintenanceModule, register_maintenance};
use crate::registry::{Command, Query, RegistryBuilder};
use crate::registry::{CommandContext, CommandResult, QueryContext};

use super::{
    CronInvocation, CronMutation, CronMutationOutcome, CronQuery, CronQueryResult, CronSchedule,
    MAX_PAYLOAD_BYTES, cron_mutate, cron_query,
};

/// Compile-time namespace, targets, and operation IDs for one Cron module.
pub trait CronModule: MaintenanceModule {
    /// Namespace that owns this Cron module.
    const NAMESPACE: NamespaceId;
    /// Command id that mutates cron schedules and occurrence state.
    const MUTATE_COMMAND_ID: u32;
    /// Query id that reads schedules and occurrences.
    const QUERY_ID: u32;
}

/// Registers Cron bindings and its scheduler Tick command.
pub fn register_cron<M: CronModule>(registry: &mut RegistryBuilder) -> crate::Result<()> {
    registry.bind_cron_module(M::MODULE, M::NAMESPACE, M::CRON_TARGETS)?;
    registry.bind_command::<CronCommand<M>>()?;
    registry.bind_query::<CronQueryCommand<M>>()?;
    register_maintenance::<M>(registry)
}

/// Typed Cron mutation command.
pub struct CronCommand<M>(PhantomData<fn() -> M>);

impl<M: CronModule> Command for CronCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::MUTATE_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = CronMutation;
    type Output = CronMutationOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let outcome = cron_mutate(
            context.primitive_transaction(),
            context.now_ms(),
            context.issued_at_ms(),
            M::CRON_TARGETS,
            &input,
        )?;
        Ok(match outcome {
            CronMutationOutcome::NotFound => CommandResult::Rejected(outcome),
            _ => CommandResult::Success(outcome),
        })
    }
}

/// Typed Cron query.
pub struct CronQueryCommand<M>(PhantomData<fn() -> M>);

impl<M: CronModule> Query for CronQueryCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = CronQuery;
    type Output = CronQueryResult;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        cron_query(context.primitive_connection(), &input)
    }
}

/// Authorized Cron capability with deterministic schedule sharding.
pub struct CronNamespace<M> {
    client: CellClient,
    tenant: TenantId,
    application: ApplicationId,
    shards: u32,
    module: PhantomData<fn() -> M>,
}

impl<M> Clone for CronNamespace<M> {
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

impl<M: CronModule> CronNamespace<M> {
    /// Creates a Cron capability after validating its compiled namespace role.
    pub fn new(
        client: CellClient,
        tenant: TenantId,
        application: ApplicationId,
    ) -> crate::Result<Self> {
        let shards = client.require_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Cron)?;
        Ok(Self {
            client,
            tenant,
            application,
            shards,
            module: PhantomData,
        })
    }

    /// Applies one durable schedule mutation on its deterministic shard.
    pub async fn mutate(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        mutation: CronMutation,
    ) -> std::result::Result<Committed<CronMutationOutcome>, InvocationError<CronMutationOutcome>>
    {
        let target = self
            .target(mutation_id(&mutation))
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<CronCommand<M>>(&target, identity, mutation)
            .await
    }

    /// Reads one schedule at an optional minimum publication receipt.
    pub async fn get(
        &self,
        schedule_id: [u8; 16],
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<CronQueryResult>, InvocationError<CronQueryResult>> {
        let target = self
            .target(schedule_id)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .query::<CronQueryCommand<M>>(&target, minimum, CronQuery::Get { schedule_id })
            .await
    }

    /// Lists one explicit Cron shard without unbounded fleet fan-out.
    pub async fn list_shard(
        &self,
        shard: u32,
        after: Option<[u8; 16]>,
        limit: u32,
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<CronQueryResult>, InvocationError<CronQueryResult>> {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .query::<CronQueryCommand<M>>(&target, minimum, CronQuery::List { after, limit })
            .await
    }

    fn target(&self, schedule_id: [u8; 16]) -> crate::Result<CellTarget> {
        let shard = shard_for_scope(M::NAMESPACE, &schedule_id, self.shards)?;
        self.shard_target(shard)
    }

    fn shard_target(&self, shard: u32) -> crate::Result<CellTarget> {
        if shard >= self.shards {
            return Err(crate::Error::Identity("cron shard outside namespace"));
        }
        CellTarget::new(
            self.tenant,
            self.application,
            M::NAMESPACE,
            &partition_for_shard(shard),
        )
    }
}

fn mutation_id(mutation: &CronMutation) -> [u8; 16] {
    match mutation {
        CronMutation::Upsert { schedule_id, .. }
        | CronMutation::Pause { schedule_id }
        | CronMutation::Resume { schedule_id, .. }
        | CronMutation::Delete { schedule_id } => *schedule_id,
    }
}

impl WireValue for CronInvocation {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self.payload.len() > MAX_PAYLOAD_BYTES {
            return Err(CodecError::Invalid("cron payload exceeds 256 KiB"));
        }
        encoder.write_bytes(&self.schedule_id)?;
        encoder.write_u64(self.generation)?;
        encoder.write_u64(self.occurrence)?;
        encoder.write_i64(self.scheduled_at_ms)?;
        encoder.write_bytes(&self.payload)
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let value = Self {
            schedule_id: read_fixed(decoder, "cron schedule ID length")?,
            generation: decoder.read_u64()?,
            occurrence: decoder.read_u64()?,
            scheduled_at_ms: decoder.read_i64()?,
            payload: decoder.read_bytes()?.to_vec(),
        };
        if value.payload.len() > MAX_PAYLOAD_BYTES {
            return Err(CodecError::Invalid("cron payload exceeds 256 KiB"));
        }
        Ok(value)
    }
}

impl WireValue for CronMutation {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Upsert {
                schedule_id,
                target_index,
                target_partition,
                payload,
                interval_ms,
                next_due_ms,
            } => {
                if payload.len() > MAX_PAYLOAD_BYTES {
                    return Err(CodecError::Invalid("cron payload exceeds 256 KiB"));
                }
                encoder.write_u8(0)?;
                encoder.write_bytes(schedule_id)?;
                encoder.write_u32(*target_index)?;
                encoder.write_bytes(target_partition)?;
                encoder.write_bytes(payload)?;
                encoder.write_u64(*interval_ms)?;
                encoder.write_i64(*next_due_ms)
            }
            Self::Pause { schedule_id } => {
                encoder.write_u8(1)?;
                encoder.write_bytes(schedule_id)
            }
            Self::Resume {
                schedule_id,
                next_due_ms,
            } => {
                encoder.write_u8(2)?;
                encoder.write_bytes(schedule_id)?;
                encoder.write_i64(*next_due_ms)
            }
            Self::Delete { schedule_id } => {
                encoder.write_u8(3)?;
                encoder.write_bytes(schedule_id)
            }
        }
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => {
                let schedule_id = read_fixed(decoder, "cron schedule ID length")?;
                let target_index = decoder.read_u32()?;
                let target_partition = decoder.read_bytes()?.to_vec();
                let payload = decoder.read_bytes()?.to_vec();
                if payload.len() > MAX_PAYLOAD_BYTES {
                    return Err(CodecError::Invalid("cron payload exceeds 256 KiB"));
                }
                Ok(Self::Upsert {
                    schedule_id,
                    target_index,
                    target_partition,
                    payload,
                    interval_ms: decoder.read_u64()?,
                    next_due_ms: decoder.read_i64()?,
                })
            }
            1 => Ok(Self::Pause {
                schedule_id: read_fixed(decoder, "cron schedule ID length")?,
            }),
            2 => Ok(Self::Resume {
                schedule_id: read_fixed(decoder, "cron schedule ID length")?,
                next_due_ms: decoder.read_i64()?,
            }),
            3 => Ok(Self::Delete {
                schedule_id: read_fixed(decoder, "cron schedule ID length")?,
            }),
            _ => Err(CodecError::Invalid("invalid cron mutation tag")),
        }
    }
}

impl WireValue for CronMutationOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Applied { generation } => {
                encoder.write_u8(0)?;
                encoder.write_u64(*generation)
            }
            Self::Deleted => encoder.write_u8(1),
            Self::NotFound => encoder.write_u8(2),
        }
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => Ok(Self::Applied {
                generation: decoder.read_u64()?,
            }),
            1 => Ok(Self::Deleted),
            2 => Ok(Self::NotFound),
            _ => Err(CodecError::Invalid("invalid cron outcome tag")),
        }
    }
}

impl WireValue for CronQuery {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Get { schedule_id } => {
                encoder.write_u8(0)?;
                encoder.write_bytes(schedule_id)
            }
            Self::List { after, limit } => {
                encoder.write_u8(1)?;
                encode_optional_id(*after, encoder)?;
                encoder.write_u32(*limit)
            }
        }
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => Ok(Self::Get {
                schedule_id: read_fixed(decoder, "cron schedule ID length")?,
            }),
            1 => Ok(Self::List {
                after: decode_optional_id(decoder)?,
                limit: decoder.read_u32()?,
            }),
            _ => Err(CodecError::Invalid("invalid cron query tag")),
        }
    }
}

impl WireValue for CronSchedule {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.schedule_id)?;
        encoder.write_u32(self.target_index)?;
        encoder.write_bytes(&self.target_partition)?;
        encoder.write_bytes(&self.payload)?;
        encoder.write_u64(self.interval_ms)?;
        encoder.write_i64(self.next_due_ms)?;
        encoder.write_u64(self.occurrence)?;
        self.enabled.encode(encoder)?;
        encoder.write_u64(self.generation)
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            schedule_id: read_fixed(decoder, "cron schedule ID length")?,
            target_index: decoder.read_u32()?,
            target_partition: decoder.read_bytes()?.to_vec(),
            payload: decoder.read_bytes()?.to_vec(),
            interval_ms: decoder.read_u64()?,
            next_due_ms: decoder.read_i64()?,
            occurrence: decoder.read_u64()?,
            enabled: bool::decode(decoder)?,
            generation: decoder.read_u64()?,
        })
    }
}

impl WireValue for CronQueryResult {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Get(value) => {
                encoder.write_u8(0)?;
                value.encode(encoder)
            }
            Self::List { schedules, next } => {
                if schedules.len() > 128 {
                    return Err(CodecError::Invalid("cron page exceeds 128 schedules"));
                }
                encoder.write_u8(1)?;
                encoder.write_count(schedules.len())?;
                for schedule in schedules {
                    schedule.encode(encoder)?;
                }
                encode_optional_id(*next, encoder)
            }
        }
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => Ok(Self::Get(Option::<CronSchedule>::decode(decoder)?)),
            1 => {
                let count = decoder.read_count()?;
                if count > 128 {
                    return Err(CodecError::Invalid("cron page exceeds 128 schedules"));
                }
                let mut schedules = Vec::with_capacity(count);
                for _ in 0..count {
                    schedules.push(CronSchedule::decode(decoder)?);
                }
                Ok(Self::List {
                    schedules,
                    next: decode_optional_id(decoder)?,
                })
            }
            _ => Err(CodecError::Invalid("invalid cron query result tag")),
        }
    }
}

fn encode_optional_id(
    value: Option<[u8; 16]>,
    encoder: &mut BoundedEncoder,
) -> Result<(), CodecError> {
    match value {
        None => encoder.write_u8(0),
        Some(value) => {
            encoder.write_u8(1)?;
            encoder.write_bytes(&value)
        }
    }
}

fn decode_optional_id(decoder: &mut BoundedDecoder<'_>) -> Result<Option<[u8; 16]>, CodecError> {
    match decoder.read_u8()? {
        0 => Ok(None),
        1 => Ok(Some(read_fixed(decoder, "cron schedule ID length")?)),
        _ => Err(CodecError::Invalid("invalid optional cron schedule ID")),
    }
}
