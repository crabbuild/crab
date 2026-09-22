use std::marker::PhantomData;

use crate::{
    BoundedDecoder, BoundedEncoder, CellClient, CellTarget, CodecError, Command, CommandContext,
    CommandResult, Committed, InvocationError, Observed, Query, QueryContext, Receipt,
    RegistryBuilder, WireValue,
};

use super::{
    EffectClaim, EffectLease, EffectLeaseOutcome, EffectState, EffectStatus,
    MAX_EFFECT_OPERATION_BYTES, MAX_EFFECT_RESULT_BYTES, SystemEffectTokens, effect_ack_lease,
    effect_claim, effect_retry_lease, effect_status, effect_validate_claim,
};

const ACK_TAG: u8 = 0;
const RETRY_TAG: u8 = 1;
const DELIVERED_TAG: u8 = 0;
const RETRYING_TAG: u8 = 1;
const FAILED_TAG: u8 = 2;
const EXTENDED_TAG: u8 = 3;
const LEASE_LOST_TAG: u8 = 4;

/// Compile-time operation identifiers for source effect supervision.
pub trait EffectModule: Send + Sync + 'static {
    const MODULE: &'static str;
    const CODEC_VERSION: u32 = 1;
    const CLAIM_COMMAND_ID: u32;
    const LEASE_COMMAND_ID: u32;
    const VALIDATE_QUERY_ID: u32;
    const STATUS_QUERY_ID: u32;
}

/// Registers source effect claim, lease transition and validation bindings.
pub fn register_effect_delivery<M: EffectModule>(
    registry: &mut RegistryBuilder,
) -> crate::Result<()> {
    registry.bind_effect_runner::<M>()?;
    registry.bind_command::<EffectClaimCommand<M>>()?;
    registry.bind_command::<EffectLeaseCommand<M>>()?;
    registry.bind_query::<EffectValidateClaimQuery<M>>()?;
    registry.bind_query::<EffectStatusQuery<M>>()
}

/// Bounded claim parameters for one source Cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectClaimRequest {
    pub limit: u32,
    pub lease_ms: u32,
}

/// Claims a bounded source effect batch through ordinary publication.
pub struct EffectClaimCommand<M>(PhantomData<fn() -> M>);

impl<M: EffectModule> Command for EffectClaimCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::CLAIM_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = EffectClaimRequest;
    type Output = Vec<EffectClaim>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let limit = usize::try_from(input.limit)
            .map_err(|_| crate::Error::Command("effect claim limit overflow"))?;
        let mut tokens = SystemEffectTokens;
        Ok(CommandResult::Success(effect_claim(
            context.primitive_transaction(),
            context.now_ms(),
            limit,
            input.lease_ms,
            &mut tokens,
        )?))
    }
}

/// Published claims to revalidate before network emission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectValidateRequest {
    pub claimed: Vec<EffectClaim>,
}

/// Revalidates exact source leases at or after the claim receipt.
pub struct EffectValidateClaimQuery<M>(PhantomData<fn() -> M>);

impl<M: EffectModule> Query for EffectValidateClaimQuery<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::VALIDATE_QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = EffectValidateRequest;
    type Output = bool;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        effect_validate_claim(
            context.primitive_connection(),
            context.now_ms(),
            &input.claimed,
        )
    }
}

/// Selects one exact source effect by its stable ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectStatusRequest {
    pub effect_id: [u8; 32],
}

/// Reads the durable source outcome without exposing the lease token.
pub struct EffectStatusQuery<M>(PhantomData<fn() -> M>);

impl<M: EffectModule> Query for EffectStatusQuery<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::STATUS_QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = EffectStatusRequest;
    type Output = Option<EffectStatus>;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        effect_status(context.primitive_connection(), input.effect_id)
    }
}

/// Acknowledges one target result for an exact source lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectAckRequest {
    pub lease: EffectLease,
    pub result: Vec<u8>,
}

/// Selects one idempotent source lease transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectLeaseRequest {
    Ack(EffectAckRequest),
    Retry(EffectLease),
}

/// Applies target acknowledgement or retry to an exact source lease.
pub struct EffectLeaseCommand<M>(PhantomData<fn() -> M>);

impl<M: EffectModule> Command for EffectLeaseCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::LEASE_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = EffectLeaseRequest;
    type Output = EffectLeaseOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let outcome = match input {
            EffectLeaseRequest::Ack(request) => effect_ack_lease(
                context.primitive_transaction(),
                context.now_ms(),
                request.lease,
                &request.result,
            )?,
            EffectLeaseRequest::Retry(lease) => {
                effect_retry_lease(context.primitive_transaction(), context.now_ms(), lease)?
            }
        };
        Ok(match outcome {
            EffectLeaseOutcome::LeaseLost => CommandResult::Rejected(outcome),
            _ => CommandResult::Success(outcome),
        })
    }
}

/// Capability for one explicit source Cell's durable effect ledger.
#[derive(Clone)]
pub struct EffectSource<M> {
    client: CellClient,
    target: CellTarget,
    module: PhantomData<fn() -> M>,
}

impl<M: EffectModule> EffectSource<M> {
    #[must_use]
    pub fn new(client: CellClient, target: CellTarget) -> Self {
        Self {
            client,
            target,
            module: PhantomData,
        }
    }

    #[must_use]
    pub const fn target(&self) -> &CellTarget {
        &self.target
    }

    pub async fn claim(
        &self,
        identity: crate::MutationIdentity,
        request: EffectClaimRequest,
    ) -> std::result::Result<Committed<Vec<EffectClaim>>, InvocationError<Vec<EffectClaim>>> {
        self.client
            .command::<EffectClaimCommand<M>>(&self.target, identity, request)
            .await
    }

    pub async fn validate(
        &self,
        claimed: Vec<EffectClaim>,
        minimum: Receipt,
    ) -> std::result::Result<Observed<bool>, InvocationError<bool>> {
        self.client
            .query::<EffectValidateClaimQuery<M>>(
                &self.target,
                Some(minimum),
                EffectValidateRequest { claimed },
            )
            .await
    }

    /// Reads one source effect at or after the requested receipt.
    pub async fn status(
        &self,
        effect_id: [u8; 32],
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<Option<EffectStatus>>, InvocationError<Option<EffectStatus>>>
    {
        self.client
            .query::<EffectStatusQuery<M>>(&self.target, minimum, EffectStatusRequest { effect_id })
            .await
    }

    pub async fn ack(
        &self,
        identity: crate::MutationIdentity,
        claim: EffectClaim,
        result: Vec<u8>,
    ) -> std::result::Result<Committed<EffectLeaseOutcome>, InvocationError<EffectLeaseOutcome>>
    {
        self.client
            .command::<EffectLeaseCommand<M>>(
                &self.target,
                identity,
                EffectLeaseRequest::Ack(EffectAckRequest {
                    lease: EffectLease::from(&claim),
                    result,
                }),
            )
            .await
    }

    pub async fn retry(
        &self,
        identity: crate::MutationIdentity,
        claim: EffectClaim,
    ) -> std::result::Result<Committed<EffectLeaseOutcome>, InvocationError<EffectLeaseOutcome>>
    {
        self.client
            .command::<EffectLeaseCommand<M>>(
                &self.target,
                identity,
                EffectLeaseRequest::Retry(EffectLease::from(&claim)),
            )
            .await
    }
}

impl WireValue for EffectClaimRequest {
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

impl WireValue for EffectClaim {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        validate_claim(self)?;
        encoder.write_bytes(&self.effect_id)?;
        encoder.write_bytes(self.destination.as_bytes())?;
        encoder.write_bytes(&self.operation)?;
        encoder.write_bytes(self.operation_digest.as_bytes())?;
        encoder.write_u32(self.attempt)?;
        encoder.write_bytes(&self.token)?;
        encoder.write_i64(self.lease_until_ms)?;
        encoder.write_i64(self.expires_at_ms)?;
        encoder.write_u64(self.created_sequence)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let claim = Self {
            effect_id: read_fixed(decoder, "effect ID length")?,
            destination: crate::CellId::try_from(decoder.read_bytes()?)
                .map_err(|_| CodecError::Invalid("effect destination length"))?,
            operation: decoder.read_bytes()?.to_vec(),
            operation_digest: crate::Digest::try_from(decoder.read_bytes()?)
                .map_err(|_| CodecError::Invalid("effect digest length"))?,
            attempt: decoder.read_u32()?,
            token: read_fixed(decoder, "effect token length")?,
            lease_until_ms: decoder.read_i64()?,
            expires_at_ms: decoder.read_i64()?,
            created_sequence: decoder.read_u64()?,
        };
        validate_claim(&claim)?;
        Ok(claim)
    }
}

impl WireValue for EffectLease {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        validate_lease(self)?;
        encoder.write_bytes(&self.effect_id)?;
        encoder.write_u32(self.attempt)?;
        encoder.write_bytes(&self.token)?;
        encoder.write_i64(self.expires_at_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let lease = Self {
            effect_id: read_fixed(decoder, "effect ID length")?,
            attempt: decoder.read_u32()?,
            token: read_fixed(decoder, "effect token length")?,
            expires_at_ms: decoder.read_i64()?,
        };
        validate_lease(&lease)?;
        Ok(lease)
    }
}

impl WireValue for Vec<EffectClaim> {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self.len() > 32 {
            return Err(CodecError::Invalid("too many effect claims"));
        }
        encoder.write_count(self.len())?;
        for claim in self {
            claim.encode(encoder)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let count = decoder.read_count()?;
        if count > 32 {
            return Err(CodecError::Invalid("too many effect claims"));
        }
        let mut claims = Vec::with_capacity(count);
        for _ in 0..count {
            claims.push(EffectClaim::decode(decoder)?);
        }
        Ok(claims)
    }
}

impl WireValue for EffectValidateRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.claimed.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            claimed: Vec::<EffectClaim>::decode(decoder)?,
        })
    }
}

impl WireValue for EffectStatusRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.effect_id)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            effect_id: read_fixed(decoder, "effect ID length")?,
        })
    }
}

impl WireValue for EffectStatus {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self
            .result
            .as_ref()
            .is_some_and(|result| result.len() > MAX_EFFECT_RESULT_BYTES)
        {
            return Err(CodecError::Invalid(
                "effect status result exceeds wire limit",
            ));
        }
        let state = match self.state {
            EffectState::Ready => 0,
            EffectState::Leased => 1,
            EffectState::Delivered => 2,
            EffectState::Failed => 3,
        };
        encoder.write_u8(state)?;
        encoder.write_u32(self.attempt)?;
        encoder.write_bool(self.token_present)?;
        self.lease_until_ms.encode(encoder)?;
        encoder.write_i64(self.expires_at_ms)?;
        self.result.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let state = match decoder.read_u8()? {
            0 => EffectState::Ready,
            1 => EffectState::Leased,
            2 => EffectState::Delivered,
            3 => EffectState::Failed,
            _ => return Err(CodecError::Invalid("invalid effect status state")),
        };
        let status = Self {
            state,
            attempt: decoder.read_u32()?,
            token_present: decoder.read_bool()?,
            lease_until_ms: Option::<i64>::decode(decoder)?,
            expires_at_ms: decoder.read_i64()?,
            result: Option::<Vec<u8>>::decode(decoder)?,
        };
        if status
            .result
            .as_ref()
            .is_some_and(|result| result.len() > MAX_EFFECT_RESULT_BYTES)
        {
            return Err(CodecError::Invalid(
                "effect status result exceeds wire limit",
            ));
        }
        Ok(status)
    }
}

impl WireValue for EffectAckRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self.result.len() > MAX_EFFECT_RESULT_BYTES {
            return Err(CodecError::Invalid("effect result exceeds wire limit"));
        }
        self.lease.encode(encoder)?;
        encoder.write_bytes(&self.result)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let lease = EffectLease::decode(decoder)?;
        let result = decoder.read_bytes()?.to_vec();
        if result.len() > MAX_EFFECT_RESULT_BYTES {
            return Err(CodecError::Invalid("effect result exceeds wire limit"));
        }
        Ok(Self { lease, result })
    }
}

impl WireValue for EffectLeaseRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Ack(request) => {
                encoder.write_u8(ACK_TAG)?;
                request.encode(encoder)
            }
            Self::Retry(lease) => {
                encoder.write_u8(RETRY_TAG)?;
                lease.encode(encoder)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            ACK_TAG => Ok(Self::Ack(EffectAckRequest::decode(decoder)?)),
            RETRY_TAG => Ok(Self::Retry(EffectLease::decode(decoder)?)),
            _ => Err(CodecError::Invalid("invalid effect lease request tag")),
        }
    }
}

impl WireValue for EffectLeaseOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Delivered => encoder.write_u8(DELIVERED_TAG),
            Self::Retrying { due_at_ms } => {
                encoder.write_u8(RETRYING_TAG)?;
                encoder.write_i64(*due_at_ms)
            }
            Self::Failed => encoder.write_u8(FAILED_TAG),
            Self::Extended { lease_until_ms } => {
                encoder.write_u8(EXTENDED_TAG)?;
                encoder.write_i64(*lease_until_ms)
            }
            Self::LeaseLost => encoder.write_u8(LEASE_LOST_TAG),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            DELIVERED_TAG => Ok(Self::Delivered),
            RETRYING_TAG => Ok(Self::Retrying {
                due_at_ms: decoder.read_i64()?,
            }),
            FAILED_TAG => Ok(Self::Failed),
            EXTENDED_TAG => Ok(Self::Extended {
                lease_until_ms: decoder.read_i64()?,
            }),
            LEASE_LOST_TAG => Ok(Self::LeaseLost),
            _ => Err(CodecError::Invalid("invalid effect lease outcome tag")),
        }
    }
}

fn validate_claim(claim: &EffectClaim) -> Result<(), CodecError> {
    if claim.operation.is_empty()
        || claim.operation.len() > MAX_EFFECT_OPERATION_BYTES
        || !(1..=20).contains(&claim.attempt)
        || claim.token.iter().all(|byte| *byte == 0)
        || claim.lease_until_ms <= 0
        || claim.expires_at_ms < claim.lease_until_ms
        || claim.created_sequence == 0
    {
        return Err(CodecError::Invalid("invalid effect claim"));
    }
    Ok(())
}

fn validate_lease(lease: &EffectLease) -> Result<(), CodecError> {
    if !(1..=20).contains(&lease.attempt)
        || lease.token.iter().all(|byte| *byte == 0)
        || lease.expires_at_ms <= 0
    {
        return Err(CodecError::Invalid("invalid effect lease"));
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

    fn claim(operation_bytes: usize) -> EffectClaim {
        EffectClaim {
            effect_id: [1; 32],
            destination: crate::CellId::from_bytes([2; 32]),
            operation: vec![3; operation_bytes],
            operation_digest: crate::Digest::from_bytes([4; 32]),
            attempt: 1,
            token: [5; 16],
            lease_until_ms: 10,
            expires_at_ms: 11,
            created_sequence: 1,
        }
    }

    #[test]
    fn maximum_claim_and_ack_fit_the_registry_wire_limit() {
        let claim = claim(MAX_EFFECT_OPERATION_BYTES);
        let mut claims = BoundedEncoder::new(1 << 20).unwrap();
        vec![claim.clone()].encode(&mut claims).unwrap();
        assert_eq!(claims.finish().len(), 1 << 20);

        let mut acknowledgement = BoundedEncoder::new(1 << 20).unwrap();
        EffectLeaseRequest::Ack(EffectAckRequest {
            lease: EffectLease::from(&claim),
            result: vec![6; MAX_EFFECT_RESULT_BYTES],
        })
        .encode(&mut acknowledgement)
        .unwrap();
        assert_eq!(acknowledgement.finish().len(), 1 << 20);
    }

    #[test]
    fn claim_and_ack_reject_values_one_byte_past_the_wire_budget() {
        let mut oversized_claim = BoundedEncoder::new(1 << 20).unwrap();
        assert!(
            vec![claim(MAX_EFFECT_OPERATION_BYTES + 1)]
                .encode(&mut oversized_claim)
                .is_err()
        );

        let mut oversized_ack = BoundedEncoder::new(1 << 20).unwrap();
        assert!(
            EffectLeaseRequest::Ack(EffectAckRequest {
                lease: EffectLease::from(&claim(1)),
                result: vec![7; MAX_EFFECT_RESULT_BYTES + 1],
            })
            .encode(&mut oversized_ack)
            .is_err()
        );
    }
}
