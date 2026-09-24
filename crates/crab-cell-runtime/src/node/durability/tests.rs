use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::Notify;

use super::*;
use crate::follower::FollowerReceipt;
use crate::identity::{ApplicationId, CellId, SessionId};
use crate::identity::{IncarnationId, NodeId};
use crate::node::log_transport::{
    AppendRequest, NodeLogTransport, RetireRequest, SealRequest, TailRequest,
};

#[derive(Default)]
struct AuthorityState {
    activations: Vec<u64>,
    coverage: Vec<(u64, u64)>,
    reject_coverage: bool,
    yield_coverage: bool,
}

#[derive(Default)]
struct RecordingAuthority(Mutex<AuthorityState>);

impl NodeLogAuthority for RecordingAuthority {
    fn activate<'a>(&'a self, log_epoch: u64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.0.lock().unwrap().activations.push(log_epoch);
            Ok(())
        })
    }

    fn advance_coverage<'a>(
        &'a self,
        log_epoch: u64,
        tiered_through: u64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let yield_coverage = self.0.lock().unwrap().yield_coverage;
            if yield_coverage {
                tokio::task::yield_now().await;
            }
            let mut state = self.0.lock().unwrap();
            if state.reject_coverage {
                return Err(Error::Node("coverage rejected"));
            }
            state.coverage.push((log_epoch, tiered_through));
            Ok(())
        })
    }

    fn close<'a>(&'a self, _barrier: &'a NodeLogRotationBarrier) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

struct BlockingAuthority {
    activation_started: Option<Arc<Notify>>,
    coverage_started: Option<Arc<Notify>>,
}

impl NodeLogAuthority for BlockingAuthority {
    fn activate<'a>(&'a self, _log_epoch: u64) -> BoxFuture<'a, Result<()>> {
        let Some(started) = &self.activation_started else {
            return Box::pin(async { Ok(()) });
        };
        let started = Arc::clone(started);
        Box::pin(async move {
            started.notify_one();
            std::future::pending().await
        })
    }

    fn advance_coverage<'a>(
        &'a self,
        _log_epoch: u64,
        _tiered_through: u64,
    ) -> BoxFuture<'a, Result<()>> {
        let Some(started) = &self.coverage_started else {
            return Box::pin(async { Ok(()) });
        };
        let started = Arc::clone(started);
        Box::pin(async move {
            started.notify_one();
            std::future::pending().await
        })
    }

    fn close<'a>(&'a self, _barrier: &'a NodeLogRotationBarrier) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

struct ImmediateTransport;

impl NodeLogTransport for ImmediateTransport {
    fn append<'a>(
        &'a self,
        _member: NodeId,
        request: AppendRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async move {
            let last = request
                .frames
                .last()
                .ok_or(Error::Node("empty test append"))?;
            let frame = crab_ltx::inspect_node_frame(last.clone(), crab_ltx::Limits::default())?;
            Ok(FollowerReceipt {
                base_sequence: 1,
                durable_through: frame.scope().node_sequence,
            })
        })
    }

    fn seal<'a>(
        &'a self,
        _member: NodeId,
        _request: SealRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("unused test seal")) })
    }

    fn tail<'a>(
        &'a self,
        _member: NodeId,
        _request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        Box::pin(async { Err(Error::Node("unused test tail")) })
    }

    fn retire<'a>(
        &'a self,
        _member: NodeId,
        _request: RetireRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("unused test retire")) })
    }
}

fn session(byte: u8) -> SessionId {
    SessionId::from_bytes([byte; 16])
}

fn node(byte: u8) -> NodeId {
    NodeId::from_bytes([byte; 16])
}

fn lease() -> NodeLeaseGuard {
    NodeLeaseGuard::new(1_000, 61_000).unwrap()
}

fn capture() -> (tempfile::TempDir, crab_ltx::CaptureBatch) {
    let directory = tempfile::TempDir::new().unwrap();
    let mut database = crab_ltx::Db::open(
        &directory.path().join("durability.sqlite"),
        crab_ltx::Limits::default(),
    )
    .unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE items(value)"))
        .unwrap();
    let cuts = database.capture().unwrap();
    database.close().unwrap();
    (directory, cuts)
}

fn submission(cuts: &crab_ltx::CaptureBatch) -> NodeLogSubmission {
    NodeLogSubmission::new(
        ApplicationId::from_bytes([1; 16]),
        CellId::from_bytes([2; 32]),
        IncarnationId::from_bytes([3; 16]),
        4,
        5,
        cuts,
    )
    .unwrap()
}

#[tokio::test]
async fn first_fsynced_ticket_activates_authority_before_fleet_proof() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(ImmediateTransport);
    let shipper = NodeLogShipper::new(
        gate.clone(),
        Arc::clone(&transport),
        crab_ltx::Limits::default(),
    )
    .unwrap();
    let authority = Arc::new(RecordingAuthority::default());
    let durability = NodeDurability::new(gate, shipper, authority.clone(), transport, lease());
    let (_directory, cuts) = capture();
    let ticket = durability.submit(submission(&cuts)).await.unwrap();

    let proof = durability.prove_fleet(ticket).await.unwrap();

    assert_eq!(proof.source(), DurabilitySource::Fleet);
    assert_eq!(authority.0.lock().unwrap().activations, vec![2]);
}

#[tokio::test]
async fn fencing_cancels_a_blocked_fleet_activation() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(ImmediateTransport);
    let shipper = NodeLogShipper::new(
        gate.clone(),
        Arc::clone(&transport),
        crab_ltx::Limits::default(),
    )
    .unwrap();
    let activation_started = Arc::new(Notify::new());
    let authority = Arc::new(BlockingAuthority {
        activation_started: Some(Arc::clone(&activation_started)),
        coverage_started: None,
    });
    let lease = lease();
    let durability = Arc::new(NodeDurability::new(
        gate,
        shipper,
        authority,
        transport,
        lease.clone(),
    ));
    let (_directory, cuts) = capture();
    let ticket = durability.submit(submission(&cuts)).await.unwrap();
    let task = tokio::spawn({
        let durability = Arc::clone(&durability);
        async move { durability.prove_fleet(ticket).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        activation_started.notified(),
    )
    .await
    .unwrap();
    lease.fence();
    assert!(matches!(task.await.unwrap(), Err(Error::Fenced)));
    durability.shutdown().await.unwrap_err();
}

#[tokio::test]
async fn fencing_cancels_a_blocked_object_coverage_cas() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(ImmediateTransport);
    let shipper = NodeLogShipper::new(
        gate.clone(),
        Arc::clone(&transport),
        crab_ltx::Limits::default(),
    )
    .unwrap();
    let coverage_started = Arc::new(Notify::new());
    let authority = Arc::new(BlockingAuthority {
        activation_started: None,
        coverage_started: Some(Arc::clone(&coverage_started)),
    });
    let lease = lease();
    let durability = Arc::new(NodeDurability::new(
        gate,
        shipper,
        authority,
        transport,
        lease.clone(),
    ));
    let (_directory, cuts) = capture();
    let ticket = durability.submit(submission(&cuts)).await.unwrap();
    let task = tokio::spawn({
        let durability = Arc::clone(&durability);
        async move { durability.prove_object(ticket).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        coverage_started.notified(),
    )
    .await
    .unwrap();
    lease.fence();
    assert!(matches!(task.await.unwrap(), Err(Error::Fenced)));
    durability.shutdown().await.unwrap_err();
}

#[tokio::test]
async fn object_proof_advances_authoritative_contiguous_coverage() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(ImmediateTransport);
    let shipper = NodeLogShipper::new(
        gate.clone(),
        Arc::clone(&transport),
        crab_ltx::Limits::default(),
    )
    .unwrap();
    let authority = Arc::new(RecordingAuthority::default());
    let durability = NodeDurability::new(gate, shipper, authority.clone(), transport, lease());
    let (_directory, cuts) = capture();
    let ticket = durability.submit(submission(&cuts)).await.unwrap();

    let proof = durability.prove_object(ticket).await.unwrap();

    assert_eq!(proof.source(), DurabilitySource::Object);
    assert_eq!(authority.0.lock().unwrap().coverage, vec![(2, 1)]);
}

#[tokio::test]
async fn concurrent_object_proofs_persist_the_complete_contiguous_prefix() {
    for reverse in [false, true] {
        let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
        let transport: Arc<dyn NodeLogTransport> = Arc::new(ImmediateTransport);
        let shipper = NodeLogShipper::new(
            gate.clone(),
            Arc::clone(&transport),
            crab_ltx::Limits::default(),
        )
        .unwrap();
        let authority = Arc::new(RecordingAuthority(Mutex::new(AuthorityState {
            yield_coverage: true,
            ..AuthorityState::default()
        })));
        let durability =
            NodeDurability::new(gate.clone(), shipper, authority.clone(), transport, lease());
        let first = gate.issue(1).unwrap();
        let second = gate.issue(1).unwrap();
        let (left, right) = if reverse {
            (second, first)
        } else {
            (first, second)
        };

        let (left, right) = tokio::join!(
            durability.prove_object(left),
            durability.prove_object(right),
        );
        left.unwrap();
        right.unwrap();

        assert_eq!(gate.tiered_through(), 2);
        assert_eq!(authority.0.lock().unwrap().coverage.last(), Some(&(2, 2)));
    }
}

#[tokio::test]
async fn rejected_object_coverage_does_not_release_a_local_proof() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(ImmediateTransport);
    let shipper = NodeLogShipper::new(
        gate.clone(),
        Arc::clone(&transport),
        crab_ltx::Limits::default(),
    )
    .unwrap();
    let authority = Arc::new(RecordingAuthority(Mutex::new(AuthorityState {
        reject_coverage: true,
        ..AuthorityState::default()
    })));
    let durability = NodeDurability::new(gate.clone(), shipper, authority, transport, lease());
    let (_directory, cuts) = capture();
    let ticket = durability.submit(submission(&cuts)).await.unwrap();

    assert!(durability.prove_object(ticket).await.is_err());
    assert_eq!(gate.tiered_through(), 0);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), gate.prove(ticket))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rotation_threshold_tracks_issued_frames() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(ImmediateTransport);
    let shipper = NodeLogShipper::new(
        gate.clone(),
        Arc::clone(&transport),
        crab_ltx::Limits::default(),
    )
    .unwrap();
    let authority = Arc::new(RecordingAuthority::default());
    let durability = NodeDurability::new(gate.clone(), shipper, authority, transport, lease());

    assert!(!durability.needs_rotation(1));
    gate.issue(1).unwrap();
    assert!(durability.needs_rotation(1));
    assert!(!durability.needs_rotation(2));
    gate.stop_shipping();
    assert!(durability.needs_rotation(1_000_000));
}

#[tokio::test]
async fn shutdown_retries_after_object_coverage_and_is_idempotent() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(ImmediateTransport);
    let shipper = NodeLogShipper::new(
        gate.clone(),
        Arc::clone(&transport),
        crab_ltx::Limits::default(),
    )
    .unwrap();
    let authority = Arc::new(RecordingAuthority::default());
    let durability = NodeDurability::new(gate, shipper, authority, transport, lease());
    let (_directory, cuts) = capture();
    let ticket = durability.submit(submission(&cuts)).await.unwrap();

    assert!(matches!(
        durability.shutdown().await,
        Err(Error::PendingPublication)
    ));
    durability.prove_object(ticket).await.unwrap();
    durability.shutdown().await.unwrap();
    durability.shutdown().await.unwrap();
}
