//! Request validation and reply conversion for dispatched peer operations.

use super::*;

pub(super) fn validate_effect_incarnation(value: &[u8], expected: CellDescription) -> Result<()> {
    if IncarnationId::try_from(value)? != expected.incarnation {
        return Err(Error::Fenced);
    }
    Ok(())
}

pub(super) fn exact_effect_id(identity: &wire::EffectIdentity) -> Result<[u8; 32]> {
    let effect_id = <[u8; 32]>::try_from(identity.effect_id.as_slice())
        .map_err(|_| Error::Peer("invalid effect ID length"))?;
    let expected = crate::primitives::effects::effect_id(
        crate::CellId::try_from(identity.source_cell.as_slice())?,
        IncarnationId::try_from(identity.source_incarnation.as_slice())?,
        identity.source_sequence,
        identity.ordinal,
    );
    if effect_id != expected {
        return Err(Error::Peer("effect identity derivation does not match"));
    }
    Ok(effect_id)
}

pub(super) fn next_sequence(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> Result<u64> {
    let sequence = transaction.query_row(
        "SELECT commit_sequence + 1 FROM sys_meta WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    u64::try_from(sequence)
        .ok()
        .filter(|sequence| *sequence != 0)
        .ok_or(Error::Command("invalid next Cell sequence"))
}

pub(super) fn mutation_identity(
    identity: &wire::MutationIdentity,
    expected_incarnation: IncarnationId,
) -> Result<MutationIdentity> {
    if IncarnationId::try_from(identity.incarnation.as_slice())? != expected_incarnation {
        return Err(Error::Fenced);
    }
    Ok(MutationIdentity {
        request_id: RequestId::try_from(identity.request_id.as_slice())?,
        issued_at_ms: identity.issued_at_ms,
        expires_at_ms: identity.expires_at_ms,
    })
}

pub(super) fn request_target(target: Option<&wire::Target>) -> Result<CellTarget> {
    let target = target.ok_or(Error::Peer("peer target is missing"))?;
    CellTarget::new(
        crate::TenantId::try_from(target.tenant_id.as_slice())?,
        crate::ApplicationId::try_from(target.application_id.as_slice())?,
        crate::NamespaceId::try_from(target.namespace_id.as_slice())?,
        &target.partition,
    )
}

pub(super) fn validate_description(
    registry: &Registry,
    module: &str,
    description: CellDescription,
    schema_min: u32,
    schema_max: u32,
) -> Result<()> {
    if !registry.supports_module_code(module, description.code, description.schema)
        || !(schema_min..=schema_max).contains(&description.schema)
    {
        return Err(Error::Fenced);
    }
    Ok(())
}

pub(super) fn mutation_reply(
    transport: &LocalCellTransport,
    outcome: StoredOutcome,
) -> wire::MutationReply {
    let description = local_description(&transport.handle);
    match outcome {
        StoredOutcome::Success {
            result,
            commit_sequence,
        } => wire::MutationReply {
            receipt: Some(wire_receipt(receipt(description, commit_sequence))),
            outcome: Some(wire::mutation_reply::Outcome::Result(
                wire::MutationResult {
                    result: Some(wire::mutation_result::Result::CommandOutput(result)),
                },
            )),
        },
        StoredOutcome::Rejected {
            result,
            commit_sequence,
        } => wire::MutationReply {
            receipt: Some(wire_receipt(receipt(description, commit_sequence))),
            outcome: Some(wire::mutation_reply::Outcome::Error(wire::Error {
                code: wire::error::Code::PreconditionFailed as i32,
                outcome: wire::error::Outcome::Rejected as i32,
                message: "Cell command was durably rejected".into(),
                retry_after_ms: 0,
                application_details: result,
            })),
        },
    }
}

pub(super) fn resolve_reply(
    transport: &LocalCellTransport,
    resolution: Resolution,
) -> wire::ResolveReply {
    match resolution {
        Resolution::Committed(outcome) => wire::ResolveReply {
            state: match &outcome {
                StoredOutcome::Success { .. } => wire::resolve_reply::State::Committed as i32,
                StoredOutcome::Rejected { .. } => wire::resolve_reply::State::Rejected as i32,
            },
            reply: Some(mutation_reply(transport, outcome)),
        },
        Resolution::Absent => wire::ResolveReply {
            state: wire::resolve_reply::State::Absent as i32,
            reply: None,
        },
        Resolution::Unknown => wire::ResolveReply {
            state: wire::resolve_reply::State::Unknown as i32,
            reply: None,
        },
        Resolution::Expired => wire::ResolveReply {
            state: wire::resolve_reply::State::Expired as i32,
            reply: None,
        },
    }
}

pub(super) fn description(value: CellDescription) -> wire::CellDescription {
    wire::CellDescription {
        cell_id: value.cell.as_bytes().to_vec(),
        incarnation: value.incarnation.as_bytes().to_vec(),
        code: value.code.as_bytes().to_vec(),
        schema: value.schema,
    }
}

pub(super) fn runtime_receipt(value: &wire::Receipt) -> Result<Receipt> {
    Ok(Receipt {
        cell: crate::CellId::try_from(value.cell_id.as_slice())?,
        incarnation: IncarnationId::try_from(value.incarnation.as_slice())?,
        commit_sequence: value.commit_sequence,
    })
}

pub(super) fn wire_receipt(value: Receipt) -> wire::Receipt {
    wire::Receipt {
        cell_id: value.cell.as_bytes().to_vec(),
        incarnation: value.incarnation.as_bytes().to_vec(),
        commit_sequence: value.commit_sequence,
    }
}

pub(crate) fn error_reply(error: Error) -> wire::PeerReply {
    let application_details = match &error {
        Error::ReplicaBehind {
            observed_sequence,
            minimum_sequence,
        } => [
            observed_sequence.to_be_bytes(),
            minimum_sequence.to_be_bytes(),
        ]
        .concat(),
        _ => Vec::new(),
    };
    let (code, outcome, message, retry_after_ms) = match error {
        Error::RequestConflict => (
            wire::error::Code::RequestIdConflict,
            wire::error::Outcome::Rejected,
            "request identity conflicts with a stored operation",
            0,
        ),
        Error::OutcomeUnknown { .. } | Error::EffectOutcomeUnknown { .. } => (
            wire::error::Code::OutcomeUnknown,
            wire::error::Outcome::Unknown,
            "accepted command outcome requires resolution",
            100,
        ),
        Error::EffectExpired => (
            wire::error::Code::RequestExpired,
            wire::error::Outcome::Rejected,
            "Cell effect expired before delivery",
            0,
        ),
        Error::Capacity(_) => (
            wire::error::Code::ResourceExhausted,
            wire::error::Outcome::NotStarted,
            "Cell runtime capacity is exhausted",
            100,
        ),
        Error::ReplicaBehind { .. } => (
            wire::error::Code::ReplicaBehind,
            wire::error::Outcome::NotStarted,
            "Cell read replica is behind the requested receipt",
            100,
        ),
        Error::ReplicaUnavailable => (
            wire::error::Code::ReplicaUnavailable,
            wire::error::Outcome::NotStarted,
            "Cell read replica is unavailable",
            100,
        ),
        Error::Fenced
        | Error::CellNotActive
        | Error::CellDraining
        | Error::RuntimeClosed
        | Error::StreamCancelled
        | Error::PendingPublication => (
            wire::error::Code::Unavailable,
            wire::error::Outcome::NotStarted,
            "Cell owner is unavailable",
            100,
        ),
        Error::Registry(_) => (
            wire::error::Code::SchemaIncompatible,
            wire::error::Outcome::Rejected,
            "compiled Cell operation is unavailable",
            0,
        ),
        Error::Identity(_) | Error::Command(_) | Error::Peer(_) | Error::PeerDecode(_) => (
            wire::error::Code::InvalidArgument,
            wire::error::Outcome::Rejected,
            "invalid Cell peer request",
            0,
        ),
        Error::Deadline => (
            wire::error::Code::Unavailable,
            wire::error::Outcome::Unknown,
            "Cell operation deadline expired",
            100,
        ),
        Error::PeerSignature(_) => (
            wire::error::Code::PermissionDenied,
            wire::error::Outcome::Rejected,
            "Cell peer authorization failed",
            0,
        ),
        Error::PeerAuthorization(_) => (
            wire::error::Code::PermissionDenied,
            wire::error::Outcome::Rejected,
            "Cell peer principal is not authorized",
            0,
        ),
        Error::Control(_)
        | Error::Catalog(_)
        | Error::Node(_)
        | Error::Release(_)
        | Error::Backup(_)
        | Error::Retention(_)
        | Error::RetentionIo(_)
        | Error::RetentionWorkerJoin(_)
        | Error::FollowerIo(_)
        | Error::FollowerWorkerJoin(_)
        | Error::PeerTransport { .. }
        | Error::PeerTransportUnknown { .. }
        | Error::CatalogCollision
        | Error::CatalogFull
        | Error::Json(_)
        | Error::Codec(_)
        | Error::Sqlite(_)
        | Error::Utf8(_)
        | Error::Storage(_)
        | Error::Ltx(_)
        | Error::WorkerStart(_)
        | Error::WorkerJoin(_)
        | Error::WorkerPanic
        | Error::ActivityWorkerStart(_)
        | Error::ActivityWorkerJoin(_)
        | Error::ActivityWorkerPanic
        | Error::ActivityPanic
        | Error::NativePanic
        | Error::RuntimeStart(_)
        | Error::Facility { .. }
        | Error::CellAlreadyActive => (
            wire::error::Code::Internal,
            wire::error::Outcome::Unknown,
            "Cell runtime failed",
            100,
        ),
    };
    wire::PeerReply {
        outcome: Some(wire::peer_reply::Outcome::Error(wire::Error {
            code: code as i32,
            outcome: outcome as i32,
            message: message.into(),
            retry_after_ms,
            application_details,
        })),
    }
}
