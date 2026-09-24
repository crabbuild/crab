//! Durability gate: commit tickets and fleet or object proofs.
use std::sync::Arc;

use futures_util::future::BoxFuture;
use tokio::sync::{Mutex, OnceCell};

use crate::identity::NodeId;
use crate::identity::SessionId;
use crate::node::lease::NodeLeaseGuard;
use crate::node::log::{
    CommitTicket, DurabilityGate, DurabilityProof, DurabilitySource, NodeLogRotationBarrier,
};
use crate::node::log_shipper::{NodeLogShipper, NodeLogSubmission};
use crate::node::log_transport::NodeLogTransport;
use crate::{Error, Result};

/// Authoritative node-session mutations required by follower durability.
///
/// Implementations must serialize these mutations with heartbeat refreshes and
/// reconcile an ambiguous CAS only when the exact session and log epoch match.
pub trait NodeLogAuthority: Send + Sync {
    fn activate<'a>(&'a self, log_epoch: u64) -> BoxFuture<'a, Result<()>>;

    fn advance_coverage<'a>(
        &'a self,
        log_epoch: u64,
        tiered_through: u64,
    ) -> BoxFuture<'a, Result<()>>;

    fn close<'a>(&'a self, barrier: &'a NodeLogRotationBarrier) -> BoxFuture<'a, Result<()>>;
}

/// Provider-neutral inputs for constructing one node-log durability epoch.
///
/// Providers own enrollment and the authority/transport implementations. The
/// host owns turning these inputs into the runtime durability object so the
/// product boundary cannot accidentally create a second shipping path.
pub struct NodeDurabilityConfig {
    session: SessionId,
    node: NodeId,
    log_epoch: u64,
    members: Vec<NodeId>,
    transport: Arc<dyn NodeLogTransport>,
    authority: Arc<dyn NodeLogAuthority>,
    node_lease: NodeLeaseGuard,
    limits: crab_ltx::Limits,
    telemetry: crate::fleet::telemetry::CellTelemetryHandle,
}

impl NodeDurabilityConfig {
    /// Binds provider enrollment to one exact node session and log epoch.
    #[expect(
        clippy::too_many_arguments,
        reason = "the provider-neutral boundary keeps every enrollment contract explicit"
    )]
    pub fn new(
        session: SessionId,
        node: NodeId,
        log_epoch: u64,
        members: Vec<NodeId>,
        transport: Arc<dyn NodeLogTransport>,
        authority: Arc<dyn NodeLogAuthority>,
        node_lease: NodeLeaseGuard,
        limits: crab_ltx::Limits,
        telemetry: crate::fleet::telemetry::CellTelemetryHandle,
    ) -> Result<Self> {
        if session.as_bytes().iter().all(|byte| *byte == 0)
            || node.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || members.is_empty()
        {
            return Err(Error::Node("invalid node-log durability configuration"));
        }
        Ok(Self {
            session,
            node,
            log_epoch,
            members,
            transport,
            authority,
            node_lease,
            limits,
            telemetry,
        })
    }

    /// Constructs the runtime-owned durability object for this epoch.
    pub fn build(self) -> Result<Arc<NodeDurability>> {
        let gate = DurabilityGate::new(self.session, self.node, self.log_epoch, self.members)?;
        let shipper = NodeLogShipper::new_with_telemetry(
            gate.clone(),
            Arc::clone(&self.transport),
            self.limits,
            self.telemetry,
        )?;
        Ok(Arc::new(NodeDurability::new(
            gate,
            shipper,
            self.authority,
            self.transport,
            self.node_lease,
        )))
    }
}

/// One enrolled node-log epoch and its non-forgeable durability proof boundary.
///
/// The runtime can submit captured cuts immediately after enrollment, but no
/// fleet proof becomes visible until the first ticket is fsynced by every
/// member and the authoritative inactive-to-active CAS succeeds.
pub struct NodeDurability {
    gate: DurabilityGate,
    shipper: NodeLogShipper,
    authority: Arc<dyn NodeLogAuthority>,
    transport: Arc<dyn NodeLogTransport>,
    node_lease: NodeLeaseGuard,
    activated: OnceCell<()>,
    object_coverage: Mutex<()>,
    shutdown: Mutex<()>,
    closed: std::sync::atomic::AtomicBool,
}

impl NodeDurability {
    #[must_use]
    pub fn new(
        gate: DurabilityGate,
        shipper: NodeLogShipper,
        authority: Arc<dyn NodeLogAuthority>,
        transport: Arc<dyn NodeLogTransport>,
        node_lease: NodeLeaseGuard,
    ) -> Self {
        Self {
            gate,
            shipper,
            authority,
            transport,
            node_lease,
            activated: OnceCell::new(),
            object_coverage: Mutex::new(()),
            shutdown: Mutex::new(()),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Returns whether shipping stopped or the epoch reached its rotation threshold.
    #[must_use]
    pub fn needs_rotation(&self, max_issued_frames: u64) -> bool {
        self.gate.shipping_scope().is_err() || self.gate.issued_through() >= max_issued_frames
    }

    /// Assigns and asynchronously ships one captured commit to every member.
    pub async fn submit(&self, submission: NodeLogSubmission) -> Result<CommitTicket> {
        self.node_lease.check()?;
        let ticket = tokio::select! {
            result = self.shipper.submit(submission) => result?,
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        };
        self.node_lease.check()?;
        Ok(ticket)
    }

    /// Returns fleet proof only after follower fsync and authoritative activation.
    pub async fn prove_fleet(&self, ticket: CommitTicket) -> Result<DurabilityProof> {
        self.node_lease.check()?;
        tokio::select! {
            result = self.gate.wait_followers(ticket) => result?,
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        }
        self.activated
            .get_or_try_init(|| async {
                self.node_lease.check()?;
                tokio::select! {
                    result = self.authority.activate(ticket.log_epoch()) => result?,
                    () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
                }
                self.node_lease.check()?;
                self.gate.activate_fleet()?;
                Ok::<(), Error>(())
            })
            .await?;
        let proof = tokio::select! {
            result = self.gate.prove(ticket) => result?,
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        };
        self.node_lease.check()?;
        if proof.source() != DurabilitySource::Fleet {
            return Err(Error::Node(
                "fleet proof was superseded by object durability",
            ));
        }
        Ok(proof)
    }

    /// Returns the first valid follower or object proof for one ticket.
    pub async fn prove(&self, ticket: CommitTicket) -> Result<DurabilityProof> {
        self.node_lease.check()?;
        let activate = async {
            self.gate.wait_followers(ticket).await?;
            self.activated
                .get_or_try_init(|| async {
                    self.node_lease.check()?;
                    tokio::select! {
                        result = self.authority.activate(ticket.log_epoch()) => result?,
                        () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
                    }
                    self.node_lease.check()?;
                    self.gate.activate_fleet()?;
                    Ok::<(), Error>(())
                })
                .await?;
            Ok::<(), Error>(())
        };
        tokio::pin!(activate);
        let proof = tokio::select! {
            proof = self.gate.prove(ticket) => proof?,
            activated = &mut activate => {
                activated?;
                self.gate.prove(ticket).await?
            }
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        };
        self.node_lease.check()?;
        Ok(proof)
    }

    /// Records an already-published object root and persists its contiguous watermark.
    ///
    /// Callers must complete the exact Cell root CAS before invoking this method.
    pub async fn prove_object(&self, ticket: CommitTicket) -> Result<DurabilityProof> {
        // Publishers from different Cells share this watermark. Serialize the
        // preview, authority CAS and local confirmation so concurrent completions
        // cannot leave the persisted prefix behind the local truncation proof.
        let _coverage = self.object_coverage.lock().await;
        self.node_lease.check()?;
        let tiered_through = self.gate.preview_object(ticket)?;
        tokio::select! {
            result = self.authority.advance_coverage(ticket.log_epoch(), tiered_through) => result?,
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        }
        self.node_lease.check()?;
        self.gate.prove_object(ticket)?;
        let proof = self.gate.prove(ticket).await?;
        if proof.source() != DurabilitySource::Object {
            return Err(Error::Node("object proof lost its durability race"));
        }
        Ok(proof)
    }

    /// Drains accepted frames and permanently closes fleet issuance for this epoch.
    pub async fn shutdown(&self) -> Result<()> {
        let _shutdown = self.shutdown.lock().await;
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        self.shipper.shutdown().await?;
        let barrier = self.gate.begin_rotation()?;
        crate::node::log::retire_node_log(Arc::clone(&self.transport), &barrier).await?;
        self.authority.close(&barrier).await?;
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
                let frame =
                    crab_ltx::inspect_node_frame(last.clone(), crab_ltx::Limits::default())?;
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
}
