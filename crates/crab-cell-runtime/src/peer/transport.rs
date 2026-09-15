use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    CellDescription, CellId, CellTarget, Digest, Error, IncarnationId, MutationIdentity, Receipt,
    Resolution, Result, StoredOutcome,
    client::{CellTransport, EncodedCommand, EncodedObservation, EncodedQuery, EncodedResolve},
};

use super::{PeerOperation, PeerPrincipal, PeerSigner, decode_peer_reply, wire};

const DEFAULT_TIMEOUT_MS: u32 = 30_000;

/// Sends one authenticated request to the current owner and returns exact reply bytes.
pub trait PeerRoundTrip: Send + Sync + 'static {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>>;
}

pub(crate) struct PeerClientTransport {
    signer: Arc<PeerSigner>,
    principal: PeerPrincipal,
    round_trip: Arc<dyn PeerRoundTrip>,
}

impl PeerClientTransport {
    pub(crate) fn new(
        signer: Arc<PeerSigner>,
        principal: PeerPrincipal,
        round_trip: Arc<dyn PeerRoundTrip>,
    ) -> Self {
        Self {
            signer,
            principal,
            round_trip,
        }
    }

    async fn exchange(
        &self,
        target: CellTarget,
        now_ms: i64,
        expires_at_ms: i64,
        operation: PeerOperation,
    ) -> Result<wire::PeerReply> {
        let remaining_ms = remaining_ms(now_ms, expires_at_ms)?;
        let request = self.signer.sign(
            self.principal.clone(),
            now_ms,
            expires_at_ms,
            remaining_ms,
            operation,
        )?;
        let reply = self.round_trip.send(target, request, remaining_ms).await?;
        decode_peer_reply(&reply)
    }
}

impl CellTransport for PeerClientTransport {
    fn describe(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellDescription>> + Send + 'static>> {
        let transport = self.clone();
        Box::pin(async move {
            let now_ms = unix_time_ms()?;
            let reply = transport
                .exchange(
                    target.clone(),
                    now_ms,
                    now_ms.saturating_add(60_000),
                    PeerOperation::Read(wire::ReadRequest {
                        target: Some(wire_target(&target)),
                        timeout_ms: DEFAULT_TIMEOUT_MS,
                        minimum: None,
                        operation: Some(wire::read_request::Operation::Describe(true)),
                    }),
                )
                .await?;
            match reply.outcome {
                Some(wire::peer_reply::Outcome::Read(wire::ReadReply {
                    result: Some(wire::read_reply::Result::Description(description)),
                    ..
                })) => runtime_description(description),
                Some(wire::peer_reply::Outcome::Error(error)) => Err(runtime_error(error)),
                _ => Err(Error::Peer("unexpected describe reply")),
            }
        })
    }

    fn command(
        &self,
        command: EncodedCommand,
    ) -> Pin<Box<dyn Future<Output = Result<StoredOutcome>> + Send + 'static>> {
        let transport = self.clone();
        Box::pin(async move {
            let expires_at_ms = command
                .now_ms
                .saturating_add(60_000)
                .min(command.identity.expires_at_ms);
            let reply = match transport
                .exchange(
                    command.target.clone(),
                    command.now_ms,
                    expires_at_ms,
                    PeerOperation::Mutate(wire::MutationRequest {
                        target: Some(wire_target(&command.target)),
                        identity: Some(wire_identity(
                            command.identity,
                            command.expected.incarnation,
                        )),
                        timeout_ms: remaining_ms(command.now_ms, expires_at_ms)?,
                        operation: Some(wire::mutation_request::Operation::CellCommand(
                            wire::CellCommand {
                                command_id: command.operation_id,
                                codec_version: command.codec_version,
                                input: command.input.clone(),
                            },
                        )),
                    }),
                )
                .await
            {
                Err(source @ Error::PeerTransportUnknown { .. }) => {
                    return Err(Error::OutcomeUnknown {
                        request_id: command.identity.request_id,
                        operation_digest: command.operation_digest,
                        source: Box::new(source),
                    });
                }
                result => result?,
            };
            match reply.outcome {
                Some(wire::peer_reply::Outcome::Mutation(reply)) => mutation_outcome(
                    reply,
                    command.expected,
                    command.identity,
                    command.operation_digest,
                ),
                Some(wire::peer_reply::Outcome::Error(error)) => Err(command_error(
                    error,
                    command.identity,
                    command.operation_digest,
                )),
                _ => Err(Error::Peer("unexpected mutation reply")),
            }
        })
    }

    fn query(
        &self,
        query: EncodedQuery,
    ) -> Pin<Box<dyn Future<Output = Result<EncodedObservation>> + Send + 'static>> {
        let transport = self.clone();
        Box::pin(async move {
            let expires_at_ms = query.now_ms.saturating_add(60_000);
            let reply = transport
                .exchange(
                    query.target.clone(),
                    query.now_ms,
                    expires_at_ms,
                    PeerOperation::Read(wire::ReadRequest {
                        target: Some(wire_target(&query.target)),
                        timeout_ms: DEFAULT_TIMEOUT_MS,
                        minimum: query.minimum.map(wire_receipt),
                        operation: Some(wire::read_request::Operation::CellQuery(
                            wire::CellQuery {
                                query_id: query.operation_id,
                                codec_version: query.codec_version,
                                input: query.input,
                            },
                        )),
                    }),
                )
                .await?;
            match reply.outcome {
                Some(wire::peer_reply::Outcome::Read(wire::ReadReply {
                    receipt: Some(receipt),
                    result: Some(wire::read_reply::Result::CommandOutput(output)),
                })) => Ok(EncodedObservation {
                    output,
                    receipt: checked_receipt(receipt, query.expected)?,
                }),
                Some(wire::peer_reply::Outcome::Error(error)) => Err(runtime_error(error)),
                _ => Err(Error::Peer("unexpected query reply")),
            }
        })
    }

    fn resolve(
        &self,
        resolve: EncodedResolve,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution>> + Send + 'static>> {
        let transport = self.clone();
        Box::pin(async move {
            let expires_at_ms = resolve
                .now_ms
                .saturating_add(60_000)
                .min(resolve.identity.expires_at_ms);
            let reply = transport
                .exchange(
                    resolve.target.clone(),
                    resolve.now_ms,
                    expires_at_ms,
                    PeerOperation::Resolve(wire::ResolveRequest {
                        target: Some(wire_target(&resolve.target)),
                        identity: Some(wire_identity(
                            resolve.identity,
                            resolve.expected.incarnation,
                        )),
                        operation_digest: resolve.operation_digest.as_bytes().to_vec(),
                    }),
                )
                .await?;
            match reply.outcome {
                Some(wire::peer_reply::Outcome::Resolve(reply)) => resolution_outcome(
                    reply,
                    resolve.expected,
                    resolve.identity,
                    resolve.operation_digest,
                ),
                Some(wire::peer_reply::Outcome::Error(error)) => Err(command_error(
                    error,
                    resolve.identity,
                    resolve.operation_digest,
                )),
                _ => Err(Error::Peer("unexpected resolve reply")),
            }
        })
    }
}

impl Clone for PeerClientTransport {
    fn clone(&self) -> Self {
        Self {
            signer: Arc::clone(&self.signer),
            principal: self.principal.clone(),
            round_trip: Arc::clone(&self.round_trip),
        }
    }
}

fn mutation_outcome(
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

fn resolution_outcome(
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

fn command_error(error: wire::Error, identity: MutationIdentity, digest: Digest) -> Error {
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

fn runtime_error(error: wire::Error) -> Error {
    match wire::error::Code::try_from(error.code) {
        Ok(wire::error::Code::PermissionDenied) => {
            Error::PeerAuthorization("remote peer denied the principal")
        }
        Ok(wire::error::Code::RequestIdConflict) => Error::RequestConflict,
        Ok(wire::error::Code::ResourceExhausted) => Error::Capacity("remote Cell owner"),
        Ok(wire::error::Code::SchemaIncompatible) => {
            Error::Registry("remote owner rejected the compiled operation")
        }
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

fn runtime_description(value: wire::CellDescription) -> Result<CellDescription> {
    Ok(CellDescription {
        cell: CellId::try_from(value.cell_id.as_slice())?,
        incarnation: IncarnationId::try_from(value.incarnation.as_slice())?,
        code: Digest::try_from(value.code.as_slice())?,
        schema: value.schema,
    })
}

fn checked_receipt(value: wire::Receipt, expected: CellDescription) -> Result<Receipt> {
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

fn wire_target(target: &CellTarget) -> wire::Target {
    wire::Target {
        tenant_id: target.tenant().as_bytes().to_vec(),
        application_id: target.application().as_bytes().to_vec(),
        namespace_id: target.namespace().as_bytes().to_vec(),
        partition: target.partition().to_vec(),
    }
}

fn wire_identity(identity: MutationIdentity, incarnation: IncarnationId) -> wire::MutationIdentity {
    wire::MutationIdentity {
        request_id: identity.request_id.as_bytes().to_vec(),
        incarnation: incarnation.as_bytes().to_vec(),
        issued_at_ms: identity.issued_at_ms,
        expires_at_ms: identity.expires_at_ms,
    }
}

fn wire_receipt(receipt: Receipt) -> wire::Receipt {
    wire::Receipt {
        cell_id: receipt.cell.as_bytes().to_vec(),
        incarnation: receipt.incarnation.as_bytes().to_vec(),
        commit_sequence: receipt.commit_sequence,
    }
}

fn remaining_ms(now_ms: i64, expires_at_ms: i64) -> Result<u32> {
    let remaining = expires_at_ms
        .checked_sub(now_ms)
        .filter(|remaining| *remaining > 0)
        .ok_or(Error::Peer("peer request already expired"))?;
    u32::try_from(remaining.min(i64::from(DEFAULT_TIMEOUT_MS)))
        .map_err(|_| Error::Peer("peer deadline overflow"))
}

fn unix_time_ms() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Peer("system clock precedes Unix epoch"))?;
    i64::try_from(duration.as_millis()).map_err(|_| Error::Peer("system clock overflow"))
}
