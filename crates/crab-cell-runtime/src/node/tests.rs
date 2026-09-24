use crate::node::directory::RecoveryCandidateRecord;
use crate::node::directory::RecoveryCandidateWindow;
use std::sync::Arc;

use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use futures_util::future::BoxFuture;
use object_store::{memory::InMemory, path::Path};

use super::*;
use crate::fleet::placement::PlacementPlanner;
use crate::identity::{ApplicationId, CellTarget, NamespaceId, TenantId};
use crate::node::log_transport::{
    AppendRequest, NodeLogTransport, RetireRequest, SealRequest, TailRequest,
};
use crate::peer::{PeerOperation, PeerPrincipal, PeerSigner, wire as peer_wire};

// Capability modules keep the fixture-heavy suite navigable; the shared
// fixtures stay here.
mod candidates;
mod log;
mod placement;
mod records;
mod sessions;

const NOW_MS: i64 = 1_000_000;

fn node(session: SessionId) -> NodeId {
    NodeId::from_bytes(*session.as_bytes())
}

struct UnavailableFollowerTransport;

impl NodeLogTransport for UnavailableFollowerTransport {
    fn append<'a>(
        &'a self,
        _member: NodeId,
        _request: AppendRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("injected unavailable follower")) })
    }

    fn seal<'a>(
        &'a self,
        _member: NodeId,
        _request: SealRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("injected unavailable follower")) })
    }

    fn retire<'a>(
        &'a self,
        _member: NodeId,
        _request: RetireRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("injected unavailable follower")) })
    }

    fn tail<'a>(
        &'a self,
        _member: NodeId,
        _request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        Box::pin(async { Err(Error::Node("injected unavailable follower")) })
    }
}

fn advertisement(key: &SigningKey, progress: u64, issued_at_ms: i64) -> NodeAdvertisement {
    advertisement_for(SessionId::from_bytes([1; 16]), key, progress, issued_at_ms)
}

fn advertisement_for(
    session: SessionId,
    key: &SigningKey,
    progress: u64,
    issued_at_ms: i64,
) -> NodeAdvertisement {
    advertisement_for_node_capacity(
        node(session),
        session,
        key,
        progress,
        issued_at_ms,
        NodeCapacity {
            free_memory_bytes: 1_000,
            free_disk_bytes: 2_000,
            follower_free_bytes: 2_000,
            follower_retained_bytes: 800,
            job_credits: 3,
            log_protocol: NODE_LOG_PROTOCOL_VERSION,
        },
    )
}

fn advertisement_for_capacity(
    session: SessionId,
    key: &SigningKey,
    progress: u64,
    issued_at_ms: i64,
    capacity: NodeCapacity,
) -> NodeAdvertisement {
    advertisement_for_node_capacity(
        node(session),
        session,
        key,
        progress,
        issued_at_ms,
        capacity,
    )
}

fn advertisement_for_node_capacity(
    node: NodeId,
    session: SessionId,
    key: &SigningKey,
    progress: u64,
    issued_at_ms: i64,
    capacity: NodeCapacity,
) -> NodeAdvertisement {
    advertisement_for_node_capacity_in_domain(
        node,
        session,
        key,
        progress,
        issued_at_ms,
        NodeFailureDomain::default(),
        capacity,
    )
}

fn advertisement_for_node_capacity_in_domain(
    node: NodeId,
    session: SessionId,
    key: &SigningKey,
    progress: u64,
    issued_at_ms: i64,
    failure_domain: NodeFailureDomain,
    capacity: NodeCapacity,
) -> NodeAdvertisement {
    NodeAdvertisement::sign(
        node,
        session,
        "https://node-1.internal:8789".into(),
        Digest::from_bytes([2; 32]),
        Digest::from_bytes([3; 32]),
        Digest::from_bytes([4; 32]),
        Digest::from_bytes([5; 32]),
        key,
        progress,
        issued_at_ms,
        issued_at_ms + 10_000,
        vec![Digest::from_bytes([6; 32]), Digest::from_bytes([7; 32])],
        vec![1],
        failure_domain,
        capacity,
    )
    .unwrap()
}

fn directory() -> NodeDirectory {
    NodeDirectory::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("root"),
            [9; 16],
        ),
        Digest::from_bytes([2; 32]),
        Digest::from_bytes([4; 32]),
        Digest::from_bytes([5; 32]),
    )
}
