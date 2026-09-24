use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;

use super::*;
use crate::follower::FollowerReceipt;
use crate::follower::FollowerStore;
use crate::identity::SessionId;
use crate::node::log_transport::{LocalFollowerTransport, RetireRequest, SealRequest, TailRequest};

#[derive(Default)]
struct RecordingTransport {
    batches: Mutex<Vec<(NodeId, Vec<u64>)>>,
    fail: Option<NodeId>,
    delay: Option<Duration>,
    receipt: Option<FollowerReceipt>,
}

#[derive(Default)]
struct RecordingTelemetry {
    appends: Mutex<Vec<(bool, u64)>>,
}

impl crate::fleet::telemetry::CellTelemetry for RecordingTelemetry {
    fn node_log_append(&self, acknowledged: bool, bytes: u64) {
        self.appends.lock().unwrap().push((acknowledged, bytes));
    }
}

struct LostAckTransport {
    inner: LocalFollowerTransport,
}

impl NodeLogTransport for LostAckTransport {
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async move {
            self.inner.append(member, request).await?;
            Err(Error::Node("injected lost follower acknowledgement"))
        })
    }

    fn seal<'a>(
        &'a self,
        member: NodeId,
        request: SealRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        self.inner.seal(member, request)
    }

    fn retire<'a>(
        &'a self,
        member: NodeId,
        request: RetireRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        self.inner.retire(member, request)
    }

    fn tail<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        self.inner.tail(member, request)
    }
}

impl RecordingTransport {
    fn failing(member: NodeId) -> Self {
        Self {
            batches: Mutex::new(Vec::new()),
            fail: Some(member),
            delay: None,
            receipt: None,
        }
    }

    fn slow(delay: Duration) -> Self {
        Self {
            batches: Mutex::new(Vec::new()),
            fail: None,
            delay: Some(delay),
            receipt: None,
        }
    }

    fn batch_sizes(&self, member: NodeId) -> Vec<usize> {
        self.batches
            .lock()
            .unwrap()
            .iter()
            .filter(|(observed, _)| *observed == member)
            .map(|(_, sequences)| sequences.len())
            .collect()
    }
}

impl NodeLogTransport for RecordingTransport {
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async move {
            if self.fail == Some(member) {
                return Err(Error::Node("injected follower failure"));
            }
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            let sequences = request
                .frames
                .iter()
                .map(|frame| {
                    crab_ltx::inspect_node_frame(frame.clone(), crab_ltx::Limits::default())
                        .map(|frame| frame.scope().node_sequence)
                        .map_err(Error::from)
                })
                .collect::<Result<Vec<_>>>()?;
            let first = *sequences.first().ok_or(Error::Node("empty test append"))?;
            let last = *sequences.last().ok_or(Error::Node("empty test append"))?;
            self.batches.lock().unwrap().push((member, sequences));
            Ok(self.receipt.unwrap_or(FollowerReceipt {
                base_sequence: first,
                durable_through: last,
            }))
        })
    }

    fn seal<'a>(
        &'a self,
        _member: NodeId,
        _request: SealRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("unused test seal")) })
    }

    fn retire<'a>(
        &'a self,
        _member: NodeId,
        _request: RetireRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("unused test retire")) })
    }

    fn tail<'a>(
        &'a self,
        _member: NodeId,
        _request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        Box::pin(async { Err(Error::Node("unused test tail")) })
    }
}

fn session(byte: u8) -> SessionId {
    SessionId::from_bytes([byte; 16])
}

fn node(byte: u8) -> NodeId {
    NodeId::from_bytes([byte; 16])
}

fn capture() -> (tempfile::TempDir, crab_ltx::CaptureBatch) {
    let directory = tempfile::TempDir::new().unwrap();
    let mut database = crab_ltx::Db::open(
        &directory.path().join("shipper.sqlite"),
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
        ApplicationId::from_bytes([9; 16]),
        CellId::from_bytes([8; 32]),
        IncarnationId::from_bytes([7; 16]),
        3,
        4,
        cuts,
    )
    .unwrap()
}

#[tokio::test]
async fn concurrent_submissions_stay_ordered_and_require_every_member_ack() {
    let (_directory, cuts) = capture();
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2), node(3)]).unwrap();
    gate.activate_fleet().unwrap();
    let transport = Arc::new(RecordingTransport::default());
    let shipper = NodeLogShipper::start(
        gate.clone(),
        transport.clone(),
        crab_ltx::Limits::default(),
        crate::fleet::telemetry::CellTelemetryHandle::default(),
        Duration::from_millis(50),
    )
    .unwrap();

    let (first, second) = tokio::join!(
        shipper.submit(submission(&cuts)),
        shipper.submit(submission(&cuts))
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(
        gate.prove(first).await.unwrap().source(),
        crate::node::log::DurabilitySource::Fleet
    );
    assert_eq!(
        gate.prove(second).await.unwrap().source(),
        crate::node::log::DurabilitySource::Fleet
    );
    shipper.shutdown().await.unwrap();

    assert_eq!(transport.batch_sizes(node(2)), [2]);
    assert_eq!(transport.batch_sizes(node(3)), [2]);
    assert!(gate.issue(1).is_err());
}

#[tokio::test]
async fn append_telemetry_records_one_result_for_each_batch() {
    let (_directory, cuts) = capture();
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    gate.activate_fleet().unwrap();
    let transport = Arc::new(RecordingTransport::default());
    let telemetry = Arc::new(RecordingTelemetry::default());
    let handle = crate::fleet::telemetry::CellTelemetryHandle::default();
    handle.install(telemetry.clone()).unwrap();
    let shipper =
        NodeLogShipper::new_with_telemetry(gate, transport, crab_ltx::Limits::default(), handle)
            .unwrap();

    shipper.submit(submission(&cuts)).await.unwrap();
    shipper.shutdown().await.unwrap();

    assert_eq!(telemetry.appends.lock().unwrap().len(), 1);
    assert!(telemetry.appends.lock().unwrap()[0].0);
    assert!(telemetry.appends.lock().unwrap()[0].1 > 0);
}

#[tokio::test]
async fn splits_large_submission_at_sixty_four_frames() {
    let (_directory, mut cuts) = capture();
    cuts.segments = std::iter::repeat_n(cuts.segments[0].clone(), 65).collect();
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    gate.activate_fleet().unwrap();
    let transport = Arc::new(RecordingTransport::default());
    let shipper = NodeLogShipper::start(
        gate.clone(),
        transport.clone(),
        crab_ltx::Limits::default(),
        crate::fleet::telemetry::CellTelemetryHandle::default(),
        Duration::from_millis(50),
    )
    .unwrap();

    let ticket = shipper.submit(submission(&cuts)).await.unwrap();
    assert_eq!(
        gate.prove(ticket).await.unwrap().source(),
        crate::node::log::DurabilitySource::Fleet
    );
    shipper.shutdown().await.unwrap();

    assert_eq!(ticket.first_sequence(), 1);
    assert_eq!(ticket.last_sequence(), 65);
    assert_eq!(transport.batch_sizes(node(2)), [64, 1]);
    assert!(gate.issue(1).is_err());
}

#[tokio::test]
async fn covered_queued_prefix_keeps_the_uncovered_suffix_fleet_durable() {
    let (_directory, cuts) = capture();
    let limits = crab_ltx::Limits::default();
    let leader = session(1);
    let member = node(2);
    let gate = DurabilityGate::new(leader, node(1), 2, [member]).unwrap();
    gate.activate_fleet().unwrap();
    let first = gate.issue(1).unwrap();
    let second = gate.issue(1).unwrap();
    gate.prove_object(first).unwrap();
    let follower_directory = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(
        follower_directory.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    let transport = Arc::new(LocalFollowerTransport::new(member, store.clone()));
    let reservation = Arc::new(OutstandingBytes {
        _permit: Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
    });
    let batch = [first, second]
        .into_iter()
        .flat_map(|ticket| {
            submission(&cuts)
                .load(limits)
                .unwrap()
                .encode(ticket, limits)
                .unwrap()
        })
        .enumerate()
        .map(|(offset, encoded)| QueuedFrame {
            sequence: offset as u64 + 1,
            encoded,
            _reservation: Arc::clone(&reservation),
        })
        .collect();

    append_batch(&gate, transport, leader, 2, &[member], batch)
        .await
        .unwrap();

    assert_eq!(
        gate.prove(second).await.unwrap().source(),
        crate::node::log::DurabilitySource::Fleet
    );
    assert_eq!(store.seal(leader, 2).await.unwrap().base_sequence, 2);
    assert_eq!(store.read_tail(leader, 2, 2).await.unwrap().len(), 1);
}

#[tokio::test]
async fn receipts_without_the_uncovered_frame_never_authorize_fleet_proof() {
    let (_directory, cuts) = capture();
    for (base_sequence, durable_through) in [(2, 1), (0, 1), (3, 1), (1, 0)] {
        let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
        gate.activate_fleet().unwrap();
        let transport = Arc::new(RecordingTransport {
            receipt: Some(FollowerReceipt {
                base_sequence,
                durable_through,
            }),
            ..RecordingTransport::default()
        });
        let shipper =
            NodeLogShipper::new(gate.clone(), transport, crab_ltx::Limits::default()).unwrap();
        let ticket = shipper.submit(submission(&cuts)).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), gate.wait_followers(ticket))
                .await
                .unwrap()
                .is_err()
        );
        shipper.shutdown().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), gate.prove(ticket))
                .await
                .is_err()
        );
        gate.prove_object(ticket).unwrap();
        assert_eq!(
            gate.prove(ticket).await.unwrap().source(),
            crate::node::log::DurabilitySource::Object
        );
    }
}

#[tokio::test]
async fn follower_failure_stops_fleet_issuance_but_preserves_object_proof() {
    let (_directory, cuts) = capture();
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2), node(3)]).unwrap();
    gate.activate_fleet().unwrap();
    let transport = Arc::new(RecordingTransport::failing(node(3)));
    let shipper = NodeLogShipper::start(
        gate.clone(),
        transport,
        crab_ltx::Limits::default(),
        crate::fleet::telemetry::CellTelemetryHandle::default(),
        Duration::from_millis(1),
    )
    .unwrap();

    let ticket = shipper.submit(submission(&cuts)).await.unwrap();
    for _ in 0..100 {
        if gate.shipping_scope().is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(gate.shipping_scope().is_err());
    let retry = tokio::time::timeout(Duration::from_secs(1), shipper.submit(submission(&cuts)))
        .await
        .expect("failed shipper must release blocked byte admission");
    assert!(retry.is_err());
    shipper.shutdown().await.unwrap();

    assert!(gate.issue(1).is_err());
    gate.prove_object(ticket).unwrap();
    assert_eq!(
        gate.prove(ticket).await.unwrap().source(),
        crate::node::log::DurabilitySource::Object
    );
}

#[tokio::test]
async fn slow_follower_delays_fleet_proof_until_every_member_acknowledges() {
    let (_directory, cuts) = capture();
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2), node(3)]).unwrap();
    gate.activate_fleet().unwrap();
    let delay = Duration::from_millis(40);
    let transport = Arc::new(RecordingTransport::slow(delay));
    let shipper = NodeLogShipper::start(
        gate.clone(),
        transport,
        crab_ltx::Limits::default(),
        crate::fleet::telemetry::CellTelemetryHandle::default(),
        Duration::from_millis(1),
    )
    .unwrap();

    let started = tokio::time::Instant::now();
    let ticket = shipper.submit(submission(&cuts)).await.unwrap();
    let proof = gate.prove(ticket).await.unwrap();
    assert_eq!(proof.source(), crate::node::log::DurabilitySource::Fleet);
    assert!(started.elapsed() >= delay);
    shipper.shutdown().await.unwrap();
}

#[tokio::test]
async fn lost_ack_keeps_the_durable_follower_tail_without_issuing_fleet_proof() {
    let (_directory, cuts) = capture();
    let leader = session(1);
    let member = node(2);
    let gate = DurabilityGate::new(leader, node(1), 2, [member]).unwrap();
    gate.activate_fleet().unwrap();
    let follower_directory = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(
        follower_directory.path().to_owned(),
        crab_ltx::Limits::default(),
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(LostAckTransport {
        inner: LocalFollowerTransport::new(member, store.clone()),
    });
    let shipper = NodeLogShipper::start(
        gate.clone(),
        transport,
        crab_ltx::Limits::default(),
        crate::fleet::telemetry::CellTelemetryHandle::default(),
        Duration::from_millis(1),
    )
    .unwrap();

    let ticket = shipper.submit(submission(&cuts)).await.unwrap();
    shipper.shutdown().await.unwrap();

    assert!(gate.issue(1).is_err());
    assert_eq!(store.seal(leader, 2).await.unwrap().durable_through, 1);
    assert_eq!(store.read_tail(leader, 2, 1).await.unwrap().len(), 1);
    gate.prove_object(ticket).unwrap();
    assert_eq!(
        gate.prove(ticket).await.unwrap().source(),
        crate::node::log::DurabilitySource::Object
    );
}

#[tokio::test]
async fn oversized_submission_is_rejected_before_waiting_for_capacity() {
    let (_directory, mut cuts) = capture();
    let mut info = cuts.segments[0].info().clone();
    info.size_bytes = crab_ltx::Limits::default()
        .max_capture_bytes
        .saturating_add((MAX_BATCH_FRAMES as u64) * NODE_FRAME_HEADER_BYTES);
    cuts.segments[0] = crab_ltx::LocalSegment::new(cuts.segments[0].path().to_owned(), info);
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    let shipper = NodeLogShipper::new(
        gate,
        Arc::new(RecordingTransport::default()),
        crab_ltx::Limits::default(),
    )
    .unwrap();

    assert!(matches!(
        shipper.submit(submission(&cuts)).await,
        Err(Error::Capacity("node-log submission"))
    ));
    shipper.shutdown().await.unwrap();
}

#[tokio::test]
async fn encoding_failure_does_not_consume_a_node_sequence() {
    let (_directory, cuts) = capture();
    let mut invalid = cuts.clone();
    let mut info = invalid.segments[0].info().clone();
    info.blake3 = [0; 32];
    invalid.segments[0] = crab_ltx::LocalSegment::new(invalid.segments[0].path().to_owned(), info);
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(2)]).unwrap();
    gate.activate_fleet().unwrap();
    let shipper = NodeLogShipper::new(
        gate.clone(),
        Arc::new(RecordingTransport::default()),
        crab_ltx::Limits::default(),
    )
    .unwrap();

    assert!(shipper.submit(submission(&invalid)).await.is_err());
    let ticket = shipper.submit(submission(&cuts)).await.unwrap();

    assert_eq!(ticket.first_sequence(), 1);
    assert_eq!(
        gate.prove(ticket).await.unwrap().source(),
        crate::node::log::DurabilitySource::Fleet
    );
    shipper.shutdown().await.unwrap();
}
