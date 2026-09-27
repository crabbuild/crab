//! Typed queries and lifecycle hints for selected read-only replicas.

use super::*;
use crate::client::Observed;
use crate::codec::{decode_wire, encode_wire};
use crate::node::NodeDirectory;
use crate::registry::{Query, Registry};

const REPLICA_QUERY_TIMEOUT_MS: u32 = 5_000;

/// Explicit, typed read client for one selected read-only Cell snapshot.
#[derive(Clone)]
pub struct ReplicaPeerClient {
    registry: Arc<Registry>,
    transport: PeerClientTransport,
}

impl ReplicaPeerClient {
    /// Binds typed replica reads to the compiled registry and authenticated peer transport.
    #[must_use]
    pub fn new(
        registry: Arc<Registry>,
        signer: Arc<PeerSigner>,
        principal: PeerPrincipal,
        round_trip: Arc<dyn PeerRoundTrip>,
    ) -> Self {
        Self {
            registry,
            transport: PeerClientTransport::new(signer, principal, round_trip),
        }
    }

    /// Queries one selected node without falling back to the current owner.
    pub async fn query<Q: Query>(
        &self,
        target: &CellTarget,
        node: NodeAdvertisement,
        expected: CellDescription,
        minimum: Option<Receipt>,
        input: Q::Input,
    ) -> Result<Observed<Q::Output>> {
        if expected.cell != target.cell_id()
            || minimum.is_some_and(|receipt| {
                receipt.cell != expected.cell || receipt.incarnation != expected.incarnation
            })
        {
            return Err(Error::Fenced);
        }
        let operation = self.registry.query_contract::<Q>(target.namespace())?;
        crate::client::validate_description(&self.registry, Q::MODULE, expected, operation)?;
        let input = encode_wire(&input, operation.input_limit)?;
        let observation = self
            .query_encoded(
                node,
                EncodedQuery {
                    target: target.clone(),
                    expected,
                    minimum,
                    now_ms: unix_time_ms()?,
                    module: Q::MODULE,
                    operation_id: Q::ID,
                    codec_version: Q::CODEC_VERSION,
                    input,
                    input_limit: operation.input_limit,
                    output_limit: operation.output_limit,
                },
                tokio::time::Instant::now()
                    + std::time::Duration::from_millis(u64::from(REPLICA_QUERY_TIMEOUT_MS)),
            )
            .await?;
        Ok(Observed {
            output: decode_wire(&observation.output, operation.output_limit)?,
            receipt: observation.receipt,
        })
    }

    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Requests admission on a selected boot session and checks its exact Cell lifetime.
    ///
    /// The caller supplies an activation-authorized principal. Discovery is
    /// revalidated just before dispatch so queued hints cannot target a later boot.
    pub async fn activate(
        &self,
        target: &CellTarget,
        directory: &NodeDirectory,
        node: NodeAdvertisement,
        expected: CellDescription,
    ) -> Result<Receipt> {
        if expected.cell != target.cell_id() {
            return Err(Error::Fenced);
        }
        let observed = directory
            .load(node.session(), unix_time_ms()?)
            .await?
            .ok_or(Error::CellNotActive)?;
        let node = observed.advertisement().clone();
        let (receipt, ready) = self
            .readiness(
                target,
                node,
                expected,
                wire::read_request::Operation::ReplicaActivate(true),
                DEFAULT_TIMEOUT_MS,
            )
            .await?;
        if !ready {
            return Err(Error::ReplicaUnavailable);
        }
        Ok(receipt)
    }

    /// Probes a selected snapshot without activating it or granting ownership.
    pub async fn status(
        &self,
        target: &CellTarget,
        node: NodeAdvertisement,
        expected: CellDescription,
    ) -> Result<(Receipt, bool)> {
        self.readiness(
            target,
            node,
            expected,
            wire::read_request::Operation::ReplicaStatus(true),
            REPLICA_QUERY_TIMEOUT_MS,
        )
        .await
    }

    async fn readiness(
        &self,
        target: &CellTarget,
        node: NodeAdvertisement,
        expected: CellDescription,
        operation: wire::read_request::Operation,
        timeout_ms: u32,
    ) -> Result<(Receipt, bool)> {
        if expected.cell != target.cell_id() {
            return Err(Error::Fenced);
        }
        let now_ms = unix_time_ms()?;
        let (expires_at_ms, remaining_ms) =
            peer_time_budget(now_ms, now_ms.saturating_add(i64::from(timeout_ms)))?;
        let request = self.transport.signer.sign(
            self.transport.principal.clone(),
            now_ms,
            expires_at_ms,
            remaining_ms,
            PeerOperation::Read(wire::ReadRequest {
                target: Some(wire_target(target)),
                timeout_ms: remaining_ms,
                minimum: None,
                expected: None,
                operation: Some(operation),
            }),
        )?;
        let reply = self
            .transport
            .round_trip
            .send_to_node(target.clone(), node, request, remaining_ms)
            .await?;
        match decode_peer_reply(&reply)?.outcome {
            Some(wire::peer_reply::Outcome::Read(wire::ReadReply {
                receipt: Some(receipt),
                result: Some(wire::read_reply::Result::ReplicaReady(ready)),
            })) => Ok((checked_receipt(receipt, expected)?, ready)),
            Some(wire::peer_reply::Outcome::Error(error)) => Err(runtime_error(error)),
            _ => Err(Error::Peer("unexpected read replica readiness reply")),
        }
    }

    pub(crate) async fn query_encoded(
        &self,
        node: NodeAdvertisement,
        query: EncodedQuery,
        deadline: tokio::time::Instant,
    ) -> Result<EncodedObservation> {
        let now_ms = unix_time_ms()?;
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or(Error::Deadline)?;
        let budget_ms = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
        if budget_ms == 0 {
            return Err(Error::Deadline);
        }
        let expires_at_ms = now_ms.saturating_add(i64::from(budget_ms));
        let (authorization_expires_at_ms, remaining_ms) = peer_time_budget(now_ms, expires_at_ms)?;
        let request = self.transport.signer.sign(
            self.transport.principal.clone(),
            now_ms,
            authorization_expires_at_ms,
            remaining_ms,
            PeerOperation::Read(wire::ReadRequest {
                target: Some(wire_target(&query.target)),
                timeout_ms: remaining_ms,
                minimum: query.minimum.map(wire_receipt),
                expected: Some(wire_description(query.expected)),
                operation: Some(wire::read_request::Operation::ReplicaQuery(
                    wire::CellQuery {
                        query_id: query.operation_id,
                        codec_version: query.codec_version,
                        input: query.input,
                    },
                )),
            }),
        )?;
        let bytes = self
            .transport
            .round_trip
            .send_to_node(query.target, node, request, remaining_ms)
            .await?;
        let reply = decode_peer_reply(&bytes)?;
        match reply.outcome {
            Some(wire::peer_reply::Outcome::Read(wire::ReadReply {
                receipt: Some(receipt),
                result: Some(wire::read_reply::Result::CommandOutput(output)),
            })) => {
                let receipt = checked_receipt(receipt, query.expected)?;
                if query
                    .minimum
                    .is_some_and(|minimum| receipt.commit_sequence < minimum.commit_sequence)
                {
                    return Err(Error::Peer("read replica returned an older receipt"));
                }
                Ok(EncodedObservation { output, receipt })
            }
            Some(wire::peer_reply::Outcome::Error(error)) => Err(runtime_error(error)),
            _ => Err(Error::Peer("unexpected read replica reply")),
        }
    }
}
