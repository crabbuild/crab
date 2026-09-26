//! Reply and authorization validation for peer requests.

use super::*;

pub(super) fn validate_reply(reply: &wire::PeerReply) -> Result<()> {
    match reply.outcome.as_ref() {
        Some(wire::peer_reply::Outcome::Mutation(reply)) => validate_mutation_reply(reply),
        Some(wire::peer_reply::Outcome::Read(reply)) => validate_read_reply(reply),
        Some(wire::peer_reply::Outcome::Resolve(reply)) => validate_resolve_reply(reply),
        Some(wire::peer_reply::Outcome::Error(error)) => validate_error(error),
        Some(wire::peer_reply::Outcome::Migration(reply)) => validate_description_wire(
            reply
                .description
                .as_ref()
                .ok_or(Error::Peer("migration reply description is missing"))?,
        ),
        None => Err(Error::Peer("peer reply outcome is missing")),
    }
}

pub(super) fn validate_mutation_reply(reply: &wire::MutationReply) -> Result<()> {
    validate_receipt(
        reply
            .receipt
            .as_ref()
            .ok_or(Error::Peer("mutation reply receipt is missing"))?,
    )?;
    match reply.outcome.as_ref() {
        Some(wire::mutation_reply::Outcome::Result(result)) => match result.result.as_ref() {
            Some(wire::mutation_result::Result::CommandOutput(output))
                if output.len() <= MAX_OPERATION_BYTES =>
            {
                Ok(())
            }
            Some(wire::mutation_result::Result::CommandOutput(_)) => {
                Err(Error::Peer("mutation result exceeds wire limit"))
            }
            None => Err(Error::Peer("mutation result is missing")),
        },
        Some(wire::mutation_reply::Outcome::Error(error)) => validate_error(error),
        None => Err(Error::Peer("mutation reply outcome is missing")),
    }
}

pub(super) fn validate_read_reply(reply: &wire::ReadReply) -> Result<()> {
    if let Some(receipt) = &reply.receipt {
        validate_receipt(receipt)?;
    }
    match reply.result.as_ref() {
        Some(wire::read_reply::Result::Description(description)) => {
            if reply.receipt.is_some()
                || description.cell_id.len() != 32
                || description.incarnation.len() != 16
                || description.code.len() != 32
                || description.schema == 0
            {
                return Err(Error::Peer("invalid peer Cell description"));
            }
            Ok(())
        }
        Some(wire::read_reply::Result::CommandOutput(output)) => {
            validate_receipt(
                reply
                    .receipt
                    .as_ref()
                    .ok_or(Error::Peer("read reply receipt is missing"))?,
            )?;
            if output.len() > MAX_OPERATION_BYTES {
                return Err(Error::Peer("read result exceeds wire limit"));
            }
            Ok(())
        }
        Some(wire::read_reply::Result::ReplicaReady(true)) => validate_receipt(
            reply
                .receipt
                .as_ref()
                .ok_or(Error::Peer("replica-ready receipt is missing"))?,
        ),
        Some(wire::read_reply::Result::ReplicaReady(false)) => {
            Err(Error::Peer("replica-ready selector must be true"))
        }
        Some(wire::read_reply::Result::Error(error)) => validate_error(error),
        None => Err(Error::Peer("read reply result is missing")),
    }
}

pub(super) fn validate_resolve_reply(reply: &wire::ResolveReply) -> Result<()> {
    let state = wire::resolve_reply::State::try_from(reply.state)
        .map_err(|_| Error::Peer("unknown resolve state"))?;
    match state {
        wire::resolve_reply::State::Committed | wire::resolve_reply::State::Rejected => {
            validate_mutation_reply(
                reply
                    .reply
                    .as_ref()
                    .ok_or(Error::Peer("resolved mutation reply is missing"))?,
            )
        }
        wire::resolve_reply::State::Absent
        | wire::resolve_reply::State::Unknown
        | wire::resolve_reply::State::Expired
            if reply.reply.is_none() =>
        {
            Ok(())
        }
        wire::resolve_reply::State::Invalid => Err(Error::Peer("invalid resolve state")),
        _ => Err(Error::Peer("resolve state and reply disagree")),
    }
}

pub(super) fn validate_receipt(receipt: &wire::Receipt) -> Result<()> {
    if receipt.cell_id.len() != 32 || receipt.incarnation.len() != 16 {
        return Err(Error::Peer("invalid peer receipt identity"));
    }
    Ok(())
}

pub(super) fn validate_error(error: &wire::Error) -> Result<()> {
    let code = wire::error::Code::try_from(error.code)
        .map_err(|_| Error::Peer("unknown peer error code"))?;
    let outcome = wire::error::Outcome::try_from(error.outcome)
        .map_err(|_| Error::Peer("unknown peer error outcome"))?;
    if code == wire::error::Code::Invalid
        || outcome == wire::error::Outcome::Unspecified
        || error.message.is_empty()
        || error.message.len() > 2_048
        || error.retry_after_ms > 60_000
        || error.application_details.len() > MAX_OPERATION_BYTES
    {
        return Err(Error::Peer("invalid peer error bounds"));
    }
    if code == wire::error::Code::ReplicaBehind && error.application_details.len() != 16 {
        return Err(Error::Peer("invalid read replica position details"));
    }
    Ok(())
}

pub(super) fn validate_principal(principal: &PeerPrincipal) -> Result<()> {
    if principal.issuer.is_empty()
        || principal.subject.is_empty()
        || principal.issuer.len() > MAX_PRINCIPAL_BYTES
        || principal.subject.len() > MAX_PRINCIPAL_BYTES
        || principal.actions.is_empty()
        || principal.actions.len() > MAX_ACTIONS
    {
        return Err(Error::Peer("invalid peer principal bounds"));
    }
    let mut previous: Option<&str> = None;
    for action in &principal.actions {
        if action.is_empty()
            || action.len() > 128
            || !action.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'_' | b'-')
            })
            || previous.is_some_and(|previous| previous >= action.as_str())
        {
            return Err(Error::Peer(
                "peer actions must be sorted unique identifiers",
            ));
        }
        previous = Some(action);
    }
    Ok(())
}

pub(super) fn validate_authorization(
    authorization: &wire::PeerAuthorization,
    now_ms: i64,
    remaining_ms: u32,
) -> Result<()> {
    let principal = PeerPrincipal {
        issuer: authorization.principal_issuer.clone(),
        subject: authorization.principal_subject.clone(),
        actions: authorization.actions.clone(),
    };
    validate_principal(&principal)?;
    if authorization.origin_session.len() != 16
        || authorization.release_digest.len() != 32
        || authorization.payload_digest.len() != 32
        || authorization.signature.len() != 64
    {
        return Err(Error::Peer("invalid peer authorization identity length"));
    }
    validate_time_bounds(
        authorization.issued_at_ms,
        authorization.expires_at_ms,
        now_ms,
        remaining_ms,
    )
}

pub(super) fn validate_time_bounds(
    issued_at_ms: i64,
    expires_at_ms: i64,
    now_ms: i64,
    remaining_ms: u32,
) -> Result<()> {
    if issued_at_ms < 0
        || expires_at_ms <= issued_at_ms
        || expires_at_ms - issued_at_ms > MAX_AUTH_LIFETIME_MS
        || issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
        || expires_at_ms <= now_ms
        || !(1..=60_000).contains(&remaining_ms)
        || i64::from(remaining_ms) > expires_at_ms - now_ms
    {
        return Err(Error::Peer("invalid or expired peer authorization time"));
    }
    Ok(())
}

pub(super) fn operation_target(
    operation: Option<&wire::peer_request::Operation>,
) -> Result<CellTarget> {
    let target = match operation {
        Some(wire::peer_request::Operation::Mutate(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::Read(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::Resolve(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::DeliverEffect(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::ResolveEffect(value)) => value.target.as_ref(),
        Some(wire::peer_request::Operation::Migrate(value)) => value.target.as_ref(),
        None => return Err(Error::Peer("peer operation is missing")),
    }
    .ok_or(Error::Peer("peer target is missing"))?;
    CellTarget::new(
        TenantId::try_from(target.tenant_id.as_slice())?,
        ApplicationId::try_from(target.application_id.as_slice())?,
        NamespaceId::try_from(target.namespace_id.as_slice())?,
        &target.partition,
    )
}

pub(super) fn validate_decoded_operation(
    operation: Option<&wire::peer_request::Operation>,
    now_ms: i64,
) -> Result<()> {
    match operation {
        Some(wire::peer_request::Operation::Mutate(value)) => validate_mutation(value, now_ms),
        Some(wire::peer_request::Operation::Read(value)) => validate_read(value),
        Some(wire::peer_request::Operation::Resolve(value)) => validate_resolve(value, now_ms),
        Some(wire::peer_request::Operation::DeliverEffect(value)) => validate_effect(value, now_ms),
        Some(wire::peer_request::Operation::ResolveEffect(value)) => {
            validate_effect_resolve(value, now_ms)
        }
        Some(wire::peer_request::Operation::Migrate(value)) => validate_migration(value),
        None => Err(Error::Peer("peer operation is missing")),
    }
}

pub(super) fn validate_mutation(request: &wire::MutationRequest, now_ms: i64) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    validate_timeout(request.timeout_ms)?;
    let identity = request
        .identity
        .as_ref()
        .ok_or(Error::Peer("mutation identity is missing"))?;
    validate_mutation_identity(identity, now_ms)?;
    match request.operation.as_ref() {
        Some(wire::mutation_request::Operation::CellCommand(command))
            if command.command_id != 0 && command.codec_version != 0 =>
        {
            Ok(())
        }
        Some(wire::mutation_request::Operation::CellCommand(_)) => {
            Err(Error::Peer("invalid Cell command identifier"))
        }
        None => Err(Error::Peer("mutation operation is missing")),
    }
}

pub(super) fn validate_read(request: &wire::ReadRequest) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    validate_timeout(request.timeout_ms)?;
    if let Some(receipt) = &request.minimum
        && (receipt.cell_id.len() != 32 || receipt.incarnation.len() != 16)
    {
        return Err(Error::Peer("invalid minimum receipt identity length"));
    }
    match request.operation.as_ref() {
        Some(wire::read_request::Operation::Describe(true)) => Ok(()),
        Some(wire::read_request::Operation::Describe(false)) => {
            Err(Error::Peer("describe selector must be true"))
        }
        Some(wire::read_request::Operation::CellQuery(query))
            if query.query_id != 0 && query.codec_version != 0 =>
        {
            Ok(())
        }
        Some(wire::read_request::Operation::ReplicaActivate(true)) if request.minimum.is_none() => {
            Ok(())
        }
        Some(wire::read_request::Operation::ReplicaQuery(query))
            if query.query_id != 0 && query.codec_version != 0 =>
        {
            Ok(())
        }
        Some(wire::read_request::Operation::CellQuery(_)) => {
            Err(Error::Peer("invalid Cell query identifier"))
        }
        Some(wire::read_request::Operation::ReplicaQuery(_)) => {
            Err(Error::Peer("invalid replica query identifier"))
        }
        Some(wire::read_request::Operation::ReplicaActivate(_)) => {
            Err(Error::Peer("invalid replica activation request"))
        }
        None => Err(Error::Peer("read operation is missing")),
    }
}

pub(super) fn validate_resolve(request: &wire::ResolveRequest, now_ms: i64) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    let identity = request
        .identity
        .as_ref()
        .ok_or(Error::Peer("resolve identity is missing"))?;
    validate_mutation_identity(identity, now_ms)?;
    if request.operation_digest.len() != 32 {
        return Err(Error::Peer("invalid operation digest length"));
    }
    Ok(())
}

pub(super) fn validate_effect(request: &wire::EffectRequest, now_ms: i64) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    if request.destination_incarnation.len() != 16 {
        return Err(Error::Peer("invalid effect destination incarnation"));
    }
    validate_effect_identity(
        request
            .identity
            .as_ref()
            .ok_or(Error::Peer("effect identity is missing"))?,
        now_ms,
    )?;
    match request.operation.as_ref() {
        Some(wire::effect_request::Operation::CellCommand(command))
            if command.command_id != 0 && command.codec_version != 0 =>
        {
            Ok(())
        }
        Some(wire::effect_request::Operation::CellCommand(_)) => {
            Err(Error::Peer("invalid effect Cell command identifier"))
        }
        None => Err(Error::Peer("effect operation is missing")),
    }
}

pub(super) fn validate_effect_resolve(
    request: &wire::EffectResolveRequest,
    now_ms: i64,
) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    if request.destination_incarnation.len() != 16 || request.operation_digest.len() != 32 {
        return Err(Error::Peer("invalid effect Resolve identity"));
    }
    validate_effect_identity(
        request
            .identity
            .as_ref()
            .ok_or(Error::Peer("effect Resolve identity is missing"))?,
        now_ms,
    )
}

pub(super) fn validate_migration(request: &wire::MigrationRequest) -> Result<()> {
    validate_target_wire(request.target.as_ref())?;
    if request.incarnation.len() != 16
        || request.from_code.len() != 32
        || request.to_code.len() != 32
        || request.from_schema == 0
        || request.to_schema == 0
        || request.from_code == request.to_code && request.from_schema == request.to_schema
    {
        return Err(Error::Peer("invalid peer migration versions"));
    }
    Ok(())
}

pub(super) fn validate_description_wire(description: &wire::CellDescription) -> Result<()> {
    if description.cell_id.len() != 32
        || description.incarnation.len() != 16
        || description.code.len() != 32
        || description.schema == 0
    {
        return Err(Error::Peer("invalid peer Cell description"));
    }
    Ok(())
}

pub(super) fn validate_effect_identity(identity: &wire::EffectIdentity, now_ms: i64) -> Result<()> {
    let remaining_ms = identity.expires_at_ms.checked_sub(now_ms);
    if identity.effect_id.len() != 32
        || identity.source_cell.len() != 32
        || identity.source_incarnation.len() != 16
        || identity.source_sequence == 0
        || now_ms < 0
        || remaining_ms.is_none_or(|remaining| remaining <= 0 || remaining > MAX_EFFECT_LIFETIME_MS)
    {
        return Err(Error::Peer("invalid or expired effect identity"));
    }
    let source_cell = crate::CellId::try_from(identity.source_cell.as_slice())?;
    let source_incarnation = IncarnationId::try_from(identity.source_incarnation.as_slice())?;
    if identity.effect_id.as_slice()
        != crate::primitives::effects::effect_id(
            source_cell,
            source_incarnation,
            identity.source_sequence,
            identity.ordinal,
        )
    {
        return Err(Error::Peer("effect identity derivation does not match"));
    }
    Ok(())
}

pub(super) fn validate_target_wire(target: Option<&wire::Target>) -> Result<()> {
    let target = target.ok_or(Error::Peer("peer target is missing"))?;
    if target.tenant_id.len() != 16
        || target.application_id.len() != 16
        || target.namespace_id.len() != 16
        || target.partition.len() > 1_024
    {
        return Err(Error::Peer("invalid peer target bounds"));
    }
    Ok(())
}

pub(super) fn validate_mutation_identity(
    identity: &wire::MutationIdentity,
    now_ms: i64,
) -> Result<()> {
    if identity.request_id.len() != 16
        || identity.incarnation.len() != 16
        || identity.issued_at_ms < 0
        || identity.expires_at_ms <= identity.issued_at_ms
        || identity.expires_at_ms - identity.issued_at_ms > MAX_MUTATION_LIFETIME_MS
        || identity.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
        || identity.expires_at_ms <= now_ms
    {
        return Err(Error::Peer("invalid or expired mutation identity"));
    }
    Ok(())
}

pub(super) fn validate_timeout(timeout_ms: u32) -> Result<()> {
    if timeout_ms > 60_000 {
        return Err(Error::Peer("peer operation timeout exceeds 60 seconds"));
    }
    Ok(())
}
