//! Reply, receipt, and description conversion between runtime and wire types.

use super::*;

pub(super) fn migrated_description(
    observed: CellDescription,
    expected: CellDescription,
    plan: MigrationPlan,
) -> bool {
    observed.cell == expected.cell
        && observed.incarnation == expected.incarnation
        && observed.code == plan.to_code()
        && observed.schema >= plan.to_schema()
}

pub(super) fn checked_effect_request(
    claim: &EffectClaim,
    now_ms: i64,
) -> Result<(wire::EffectRequest, CellTarget)> {
    if claim.attempt == 0
        || claim.token.iter().all(|byte| *byte == 0)
        || claim.operation.is_empty()
        || claim.expires_at_ms <= now_ms
    {
        return Err(Error::Command("invalid effect claim for delivery"));
    }
    let request = wire::EffectRequest::decode(claim.operation.as_slice())?;
    if request.encode_to_vec() != claim.operation {
        return Err(Error::Peer("stored effect request is not canonical"));
    }
    if !request.destination_incarnation.is_empty() {
        return Err(Error::Peer(
            "stored effect request pins a destination incarnation",
        ));
    }
    let mut validated = request.clone();
    validated.destination_incarnation = vec![1; 16];
    PeerOperation::DeliverEffect(validated).validate(now_ms)?;
    let target = runtime_target(
        request
            .target
            .as_ref()
            .ok_or(Error::Peer("effect target is missing"))?,
    )?;
    if target.cell_id() != claim.destination
        || crate::primitives::effects::effect_operation_digest(
            target.cell_id(),
            claim.effect_id,
            &claim.operation,
        ) != claim.operation_digest
    {
        return Err(Error::Command("effect claim target or digest changed"));
    }
    let identity = request
        .identity
        .as_ref()
        .ok_or(Error::Peer("effect identity is missing"))?;
    if identity.effect_id.as_slice() != claim.effect_id
        || identity.source_sequence != claim.created_sequence
        || identity.expires_at_ms != claim.expires_at_ms
    {
        return Err(Error::Command("effect claim source identity changed"));
    }
    Ok((request, target))
}

pub(super) fn runtime_target(value: &wire::Target) -> Result<CellTarget> {
    CellTarget::new(
        crate::TenantId::try_from(value.tenant_id.as_slice())?,
        crate::ApplicationId::try_from(value.application_id.as_slice())?,
        crate::NamespaceId::try_from(value.namespace_id.as_slice())?,
        &value.partition,
    )
}

pub(super) fn effect_mutation_outcome(
    reply: wire::MutationReply,
    expected: CellDescription,
    claim: &EffectClaim,
) -> Result<StoredOutcome> {
    let receipt = checked_receipt(
        reply
            .receipt
            .ok_or(Error::Peer("effect reply receipt is missing"))?,
        expected,
    )?;
    match reply.outcome {
        Some(wire::mutation_reply::Outcome::Result(wire::MutationResult {
            result: Some(wire::mutation_result::Result::CommandOutput(result)),
        })) => Ok(StoredOutcome::Success {
            result,
            commit_sequence: receipt.commit_sequence,
        }),
        Some(wire::mutation_reply::Outcome::Error(error))
            if error.code == wire::error::Code::PreconditionFailed as i32
                && error.outcome == wire::error::Outcome::Rejected as i32 =>
        {
            Ok(StoredOutcome::Rejected {
                result: error.application_details,
                commit_sequence: receipt.commit_sequence,
            })
        }
        Some(wire::mutation_reply::Outcome::Error(error)) => {
            Err(effect_error(error, claim.effect_id, claim.operation_digest))
        }
        _ => Err(Error::Peer("unexpected effect mutation result")),
    }
}

pub(super) fn effect_resolution_outcome(
    reply: wire::ResolveReply,
    expected: CellDescription,
    claim: &EffectClaim,
) -> Result<Resolution> {
    match wire::resolve_reply::State::try_from(reply.state)
        .map_err(|_| Error::Peer("unknown effect Resolve state"))?
    {
        wire::resolve_reply::State::Committed | wire::resolve_reply::State::Rejected => {
            Ok(Resolution::Committed(effect_mutation_outcome(
                reply
                    .reply
                    .ok_or(Error::Peer("resolved effect reply is missing"))?,
                expected,
                claim,
            )?))
        }
        wire::resolve_reply::State::Absent => Ok(Resolution::Absent),
        wire::resolve_reply::State::Unknown => Ok(Resolution::Unknown),
        wire::resolve_reply::State::Expired => Ok(Resolution::Expired),
        wire::resolve_reply::State::Invalid => Err(Error::Peer("invalid effect Resolve state")),
    }
}

pub(super) fn effect_error(error: wire::Error, effect_id: [u8; 32], digest: Digest) -> Error {
    if error.code == wire::error::Code::OutcomeUnknown as i32
        || error.outcome == wire::error::Outcome::Unknown as i32
    {
        return Error::EffectOutcomeUnknown {
            effect_id,
            operation_digest: digest,
            source: Box::new(runtime_error(error)),
        };
    }
    if error.code == wire::error::Code::RequestExpired as i32 {
        return Error::EffectExpired;
    }
    runtime_error(error)
}

pub(super) fn mutation_outcome(
    reply: wire::MutationReply,
    expected: CellDescription,
    identity: MutationIdentity,
    digest: Digest,
) -> Result<StoredOutcome> {
    let receipt = checked_receipt(
        reply
            .receipt
            .ok_or(Error::Peer("mutation reply receipt is missing"))?,
        expected,
    )?;
    match reply.outcome {
        Some(wire::mutation_reply::Outcome::Result(wire::MutationResult {
            result: Some(wire::mutation_result::Result::CommandOutput(result)),
        })) => Ok(StoredOutcome::Success {
            result,
            commit_sequence: receipt.commit_sequence,
        }),
        Some(wire::mutation_reply::Outcome::Error(error))
            if error.code == wire::error::Code::PreconditionFailed as i32
                && error.outcome == wire::error::Outcome::Rejected as i32 =>
        {
            Ok(StoredOutcome::Rejected {
                result: error.application_details,
                commit_sequence: receipt.commit_sequence,
            })
        }
        Some(wire::mutation_reply::Outcome::Error(error)) => {
            Err(command_error(error, identity, digest))
        }
        _ => Err(Error::Peer("unexpected mutation result")),
    }
}

pub(super) fn resolution_outcome(
    reply: wire::ResolveReply,
    expected: CellDescription,
    identity: MutationIdentity,
    digest: Digest,
) -> Result<Resolution> {
    match wire::resolve_reply::State::try_from(reply.state)
        .map_err(|_| Error::Peer("unknown resolve state"))?
    {
        wire::resolve_reply::State::Committed | wire::resolve_reply::State::Rejected => {
            Ok(Resolution::Committed(mutation_outcome(
                reply
                    .reply
                    .ok_or(Error::Peer("resolved mutation reply is missing"))?,
                expected,
                identity,
                digest,
            )?))
        }
        wire::resolve_reply::State::Absent => Ok(Resolution::Absent),
        wire::resolve_reply::State::Unknown => Ok(Resolution::Unknown),
        wire::resolve_reply::State::Expired => Ok(Resolution::Expired),
        wire::resolve_reply::State::Invalid => Err(Error::Peer("invalid resolve state")),
    }
}

pub(super) fn command_error(
    error: wire::Error,
    identity: MutationIdentity,
    digest: Digest,
) -> Error {
    if error.code == wire::error::Code::OutcomeUnknown as i32
        || error.outcome == wire::error::Outcome::Unknown as i32
    {
        return Error::OutcomeUnknown {
            request_id: identity.request_id,
            operation_digest: digest,
            source: Box::new(runtime_error(error)),
        };
    }
    runtime_error(error)
}

pub(crate) fn runtime_error(error: wire::Error) -> Error {
    match wire::error::Code::try_from(error.code) {
        Ok(wire::error::Code::PermissionDenied) => {
            Error::PeerAuthorization("remote peer denied the principal")
        }
        Ok(wire::error::Code::RequestIdConflict) => Error::RequestConflict,
        Ok(wire::error::Code::ResourceExhausted) => Error::Capacity("remote Cell owner"),
        Ok(wire::error::Code::SchemaIncompatible) => {
            Error::Registry("remote owner rejected the compiled operation")
        }
        Ok(wire::error::Code::ReplicaBehind) => match error.application_details.as_slice() {
            [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] => Error::ReplicaBehind {
                observed_sequence: u64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, *h]),
                minimum_sequence: u64::from_be_bytes([*i, *j, *k, *l, *m, *n, *o, *p]),
            },
            _ => Error::Peer("remote read replica returned invalid position details"),
        },
        Ok(wire::error::Code::ReplicaUnavailable) => Error::ReplicaUnavailable,
        Ok(wire::error::Code::Unavailable | wire::error::Code::OutcomeUnknown) => {
            Error::CellNotActive
        }
        Ok(
            wire::error::Code::PreconditionFailed
            | wire::error::Code::LeaseLost
            | wire::error::Code::RequestExpired,
        ) => Error::Fenced,
        Ok(
            wire::error::Code::InvalidArgument
            | wire::error::Code::NotFound
            | wire::error::Code::Internal
            | wire::error::Code::Invalid,
        )
        | Err(_) => Error::Peer("remote peer rejected the request"),
    }
}

pub(super) fn runtime_description(value: wire::CellDescription) -> Result<CellDescription> {
    Ok(CellDescription {
        cell: CellId::try_from(value.cell_id.as_slice())?,
        incarnation: IncarnationId::try_from(value.incarnation.as_slice())?,
        code: Digest::try_from(value.code.as_slice())?,
        schema: value.schema,
    })
}

pub(super) fn checked_receipt(value: wire::Receipt, expected: CellDescription) -> Result<Receipt> {
    let receipt = Receipt {
        cell: CellId::try_from(value.cell_id.as_slice())?,
        incarnation: IncarnationId::try_from(value.incarnation.as_slice())?,
        commit_sequence: value.commit_sequence,
    };
    if receipt.cell != expected.cell || receipt.incarnation != expected.incarnation {
        return Err(Error::Peer("peer receipt does not match described Cell"));
    }
    Ok(receipt)
}

pub(super) fn wire_target(target: &CellTarget) -> wire::Target {
    wire::Target {
        tenant_id: target.tenant().as_bytes().to_vec(),
        application_id: target.application().as_bytes().to_vec(),
        namespace_id: target.namespace().as_bytes().to_vec(),
        partition: target.partition().to_vec(),
    }
}

pub(super) fn wire_identity(
    identity: MutationIdentity,
    incarnation: IncarnationId,
) -> wire::MutationIdentity {
    wire::MutationIdentity {
        request_id: identity.request_id.as_bytes().to_vec(),
        incarnation: incarnation.as_bytes().to_vec(),
        issued_at_ms: identity.issued_at_ms,
        expires_at_ms: identity.expires_at_ms,
    }
}

pub(super) fn wire_receipt(receipt: Receipt) -> wire::Receipt {
    wire::Receipt {
        cell_id: receipt.cell.as_bytes().to_vec(),
        incarnation: receipt.incarnation.as_bytes().to_vec(),
        commit_sequence: receipt.commit_sequence,
    }
}
