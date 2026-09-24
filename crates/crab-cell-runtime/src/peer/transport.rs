use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use prost::Message;

use crate::cell::executor::{MutationIdentity, Resolution, StoredOutcome};
use crate::client::{CellDescription, Receipt};
use crate::client::{
    CellTransport, EncodedCommand, EncodedObservation, EncodedQuery, EncodedResolve,
};
use crate::identity::{CellId, CellTarget, Digest, IncarnationId};
use crate::node::NodeAdvertisement;
use crate::primitives::effects::EffectClaim;
use crate::registry::MigrationPlan;
use crate::{Error, Result};

use super::{PeerOperation, PeerPrincipal, PeerSigner, decode_peer_reply, wire};

mod convert;

use convert::*;

const DEFAULT_TIMEOUT_MS: u32 = 30_000;

/// Sends one authenticated request to the current owner and returns exact reply bytes.
pub trait PeerRoundTrip: Send + Sync + 'static {
    /// Sends one authenticated request and returns the exact reply bytes.
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>>;

    /// Sends one already-authenticated request to a live enrolled node.
    ///
    /// This is an activation hint, not an ownership operation. The receiving
    /// node must still acquire the Cell through the normal authority CAS before
    /// it can serve the request. Implementations that only route through the
    /// current owner may keep the default fail-closed behavior.
    fn send_to_node(
        &self,
        _target: CellTarget,
        _node: NodeAdvertisement,
        _request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        Box::pin(async { Err(Error::Peer("direct node activation is unavailable")) })
    }
}

pub(crate) struct PeerClientTransport {
    signer: Arc<PeerSigner>,
    principal: PeerPrincipal,
    round_trip: Arc<dyn PeerRoundTrip>,
}

/// Authenticated private transport for one already-published source effect lease.
#[derive(Clone)]
pub struct EffectPeerClient {
    transport: PeerClientTransport,
}

/// Authenticated private control client for one registry-selected Cell migration.
#[derive(Clone)]
pub struct MigrationPeerClient {
    transport: PeerClientTransport,
}

impl MigrationPeerClient {
    /// Creates a migration peer client over a signer, principal, and round trip.
    #[must_use]
    pub fn new(
        signer: Arc<PeerSigner>,
        principal: PeerPrincipal,
        round_trip: Arc<dyn PeerRoundTrip>,
    ) -> Self {
        Self {
            transport: PeerClientTransport::new(signer, principal, round_trip),
        }
    }

    /// Migrates one exact remote capability without accepting migration SQL on the wire.
    pub async fn migrate(
        &self,
        target: CellTarget,
        expected: CellDescription,
        plan: MigrationPlan,
        now_ms: i64,
    ) -> Result<CellDescription> {
        if expected.cell != target.cell_id()
            || expected.code != plan.from_code()
            || expected.schema != plan.from_schema()
        {
            return Err(Error::Registry(
                "peer migration plan does not match described Cell",
            ));
        }
        let expires_at_ms = now_ms.saturating_add(60_000);
        let operation = PeerOperation::Migrate(wire::MigrationRequest {
            target: Some(wire_target(&target)),
            incarnation: expected.incarnation.as_bytes().to_vec(),
            from_code: plan.from_code().as_bytes().to_vec(),
            from_schema: plan.from_schema(),
            to_code: plan.to_code().as_bytes().to_vec(),
            to_schema: plan.to_schema(),
        });
        let reply = match self
            .transport
            .exchange(target.clone(), now_ms, expires_at_ms, operation)
            .await
        {
            Ok(reply) => reply,
            Err(source @ Error::PeerTransportUnknown { .. }) => {
                let observed = self.transport.describe(target).await?;
                if migrated_description(observed, expected, plan) {
                    return Ok(observed);
                }
                return Err(source);
            }
            Err(error) => return Err(error),
        };
        match reply.outcome {
            Some(wire::peer_reply::Outcome::Migration(reply)) => {
                let observed = runtime_description(
                    reply
                        .description
                        .ok_or(Error::Peer("migration reply description is missing"))?,
                )?;
                if migrated_description(observed, expected, plan) {
                    Ok(observed)
                } else {
                    Err(Error::Peer(
                        "migration reply does not match the requested successor",
                    ))
                }
            }
            Some(wire::peer_reply::Outcome::Error(error)) => Err(runtime_error(error)),
            _ => Err(Error::Peer("unexpected migration reply")),
        }
    }
}

impl EffectPeerClient {
    /// Creates an effect peer client over a signer, principal, and round trip.
    #[must_use]
    pub fn new(
        signer: Arc<PeerSigner>,
        principal: PeerPrincipal,
        round_trip: Arc<dyn PeerRoundTrip>,
    ) -> Self {
        Self {
            transport: PeerClientTransport::new(signer, principal, round_trip),
        }
    }

    /// Delivers one exact lease after the caller has validated its published token.
    pub async fn deliver(&self, claim: &EffectClaim, now_ms: i64) -> Result<StoredOutcome> {
        if claim.lease_until_ms < now_ms.saturating_add(1_000) {
            return Err(Error::Command(
                "effect lease has insufficient delivery margin",
            ));
        }
        let (mut request, target) = checked_effect_request(claim, now_ms)?;
        let expected = self.transport.describe(target.clone()).await?;
        if expected.cell != target.cell_id() {
            return Err(Error::Fenced);
        }
        request.destination_incarnation = expected.incarnation.as_bytes().to_vec();
        let expires_at_ms = now_ms.saturating_add(60_000).min(claim.expires_at_ms);
        let reply = match self
            .transport
            .exchange(
                target,
                now_ms,
                expires_at_ms,
                PeerOperation::DeliverEffect(request),
            )
            .await
        {
            Err(source @ Error::PeerTransportUnknown { .. }) => {
                return Err(Error::EffectOutcomeUnknown {
                    effect_id: claim.effect_id,
                    operation_digest: claim.operation_digest,
                    source: Box::new(source),
                });
            }
            result => result?,
        };
        match reply.outcome {
            Some(wire::peer_reply::Outcome::Mutation(reply)) => {
                effect_mutation_outcome(reply, expected, claim)
            }
            Some(wire::peer_reply::Outcome::Error(error)) => {
                Err(effect_error(error, claim.effect_id, claim.operation_digest))
            }
            _ => Err(Error::Peer("unexpected effect delivery reply")),
        }
    }

    /// Resolves one ambiguous delivery against the destination inbox.
    pub async fn resolve(&self, claim: &EffectClaim, now_ms: i64) -> Result<Resolution> {
        let (mut request, target) = checked_effect_request(claim, now_ms)?;
        let expected = self.transport.describe(target.clone()).await?;
        if expected.cell != target.cell_id() {
            return Err(Error::Fenced);
        }
        request.destination_incarnation = expected.incarnation.as_bytes().to_vec();
        let expires_at_ms = now_ms.saturating_add(60_000).min(claim.expires_at_ms);
        let reply = self
            .transport
            .exchange(
                target,
                now_ms,
                expires_at_ms,
                PeerOperation::ResolveEffect(wire::EffectResolveRequest {
                    target: request.target,
                    destination_incarnation: request.destination_incarnation,
                    identity: request.identity,
                    operation_digest: claim.operation_digest.as_bytes().to_vec(),
                }),
            )
            .await?;
        match reply.outcome {
            Some(wire::peer_reply::Outcome::Resolve(reply)) => {
                effect_resolution_outcome(reply, expected, claim)
            }
            Some(wire::peer_reply::Outcome::Error(error)) => {
                Err(effect_error(error, claim.effect_id, claim.operation_digest))
            }
            _ => Err(Error::Peer("unexpected effect Resolve reply")),
        }
    }
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
        let (authorization_expires_at_ms, remaining_ms) = peer_time_budget(now_ms, expires_at_ms)?;
        let request = self.signer.sign(
            self.principal.clone(),
            now_ms,
            authorization_expires_at_ms,
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

fn remaining_ms(now_ms: i64, expires_at_ms: i64) -> Result<u32> {
    let remaining = expires_at_ms
        .checked_sub(now_ms)
        .filter(|remaining| *remaining > 0)
        .ok_or(Error::Peer("peer request already expired"))?;
    u32::try_from(remaining.min(i64::from(DEFAULT_TIMEOUT_MS)))
        .map_err(|_| Error::Peer("peer deadline overflow"))
}

pub(super) fn peer_time_budget(now_ms: i64, operation_expires_at_ms: i64) -> Result<(i64, u32)> {
    let remaining_ms = remaining_ms(now_ms, operation_expires_at_ms)?;
    // Authorization must outlive the operation budget so transit does not
    // invalidate a signed request before the peer can verify it.
    let authorization_expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(Error::Peer("peer authorization deadline overflow"))?;
    Ok((authorization_expires_at_ms, remaining_ms))
}

fn unix_time_ms() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Peer("system clock precedes Unix epoch"))?;
    i64::try_from(duration.as_millis()).map_err(|_| Error::Peer("system clock overflow"))
}
