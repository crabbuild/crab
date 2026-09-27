//! Advisory placement of existing, ownerless data and index Cells.

use std::sync::Arc;

use crab_cell_peer_http::PeerHttpRoundTrip;
use crab_cell_runtime::{
    Error, Result,
    fleet::placement::{PlacementObservation, PlacementPlanner},
    identity::{CellTarget, SessionId},
    node::NodeDirectory,
    peer::{PeerOperation, PeerRoundTrip, PeerSigner, decode_peer_reply, wire},
};

use super::{
    node_lease::unix_time_ms,
    peer_receiver::{ACTIVATE_ACTION, peer_principal},
};

pub(super) fn is_data_target(target: &CellTarget) -> bool {
    [crate::DATA_NAMESPACE, crate::global_index::NAMESPACE].contains(&target.namespace())
}

pub(super) struct ColdPlacement {
    pub directory: NodeDirectory,
    pub session: SessionId,
    pub signer: Arc<PeerSigner>,
    pub round_trip: Arc<PeerHttpRoundTrip>,
}

impl ColdPlacement {
    pub(super) async fn select_local(
        &self,
        target: &CellTarget,
        recovering: Option<SessionId>,
    ) -> Result<bool> {
        let observed_at = unix_time_ms()?;
        // Discovery fails on overflow instead of placing from a partial fleet.
        // Admission at the destination and ownership CAS remain authoritative.
        let node = if let Some(session) = recovering {
            if session == self.session {
                return Ok(true);
            }
            // Resume a canceled activation only on its exact claimed owner.
            // Another destination needs the separate expired-lease takeover.
            self.directory
                .load(session, observed_at)
                .await?
                .ok_or(Error::CellNotActive)?
                .advertisement()
                .clone()
        } else {
            let nodes = self.directory.live(observed_at, 1_024).await?;
            let observations = nodes
                .iter()
                .filter_map(|node| {
                    PlacementObservation::from_signed_advertisement(node, observed_at, false).ok()
                })
                .collect::<Vec<_>>();
            let selected = PlacementPlanner::default()
                .choose(target.cell_id(), observed_at, &observations)?
                .ok_or(Error::Capacity(
                    "no eligible BeyondDB placement destination",
                ))?;
            if selected.session == self.session {
                return Ok(true);
            }
            nodes
                .into_iter()
                .find(|node| node.session() == selected.session)
                .ok_or(Error::Node("selected placement session disappeared"))?
        };
        let now_ms = unix_time_ms()?;
        let expires = now_ms
            .checked_add(60_000)
            .ok_or(Error::Peer("activation deadline overflow"))?;
        let mut principal = peer_principal(self.directory.fleet(), self.session);
        principal.actions = vec![ACTIVATE_ACTION.into()];
        let request = self.signer.sign(
            principal,
            now_ms,
            expires,
            30_000,
            PeerOperation::Read(wire::ReadRequest {
                target: Some(wire::Target {
                    tenant_id: target.tenant().as_bytes().to_vec(),
                    application_id: target.application().as_bytes().to_vec(),
                    namespace_id: target.namespace().as_bytes().to_vec(),
                    partition: target.partition().to_vec(),
                }),
                timeout_ms: 30_000,
                minimum: None,
                operation: Some(wire::read_request::Operation::Describe(true)),
            }),
        )?;
        let reply = self
            .round_trip
            .send_to_node(target.clone(), node, request, 30_000)
            .await?;
        match decode_peer_reply(&reply)?.outcome {
            Some(wire::peer_reply::Outcome::Read(read))
                if matches!(&read.result,
                Some(wire::read_reply::Result::Description(description))
                    if description.cell_id.as_slice() == target.cell_id().as_bytes()) =>
            {
                Ok(false)
            }
            Some(wire::peer_reply::Outcome::Error(error)) => {
                tracing::debug!(code = error.code, "remote Cell activation rejected");
                match wire::error::Code::try_from(error.code) {
                    Ok(wire::error::Code::ResourceExhausted) => {
                        Err(Error::Capacity("remote Cell activation"))
                    }
                    Ok(wire::error::Code::PermissionDenied) => {
                        Err(Error::PeerAuthorization("remote activation denied"))
                    }
                    Ok(wire::error::Code::Unavailable) => Err(Error::CellNotActive),
                    _ => Err(Error::Peer("remote Cell activation rejected")),
                }
            }
            _ => Err(Error::Peer(
                "remote activation returned an invalid description",
            )),
        }
    }
}
