use std::{collections::VecDeque, io::Read as _, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::future::join_all;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::{
    AppendRequest, ApplicationId, CellId, CommitTicket, DurabilityGate, Error, IncarnationId,
    NodeId, NodeLogTransport, Result,
};

const MAX_BATCH_FRAMES: usize = 64;
const MAX_QUEUED_SUBMISSIONS: usize = 512;
const NODE_FRAME_HEADER_BYTES: u64 = 240;
const BATCH_INTERVAL: Duration = Duration::from_millis(1);

/// One captured Cell commit awaiting ordered node-log assignment.
pub struct NodeLogSubmission {
    application: ApplicationId,
    cell: CellId,
    incarnation: IncarnationId,
    cell_epoch: u64,
    commit_sequence: u64,
    segments: Vec<crab_ltx::LocalSegment>,
    encoded_bytes: u64,
}

impl NodeLogSubmission {
    /// Binds one nonempty capture batch to its exact Cell authority generation.
    pub fn new(
        application: ApplicationId,
        cell: CellId,
        incarnation: IncarnationId,
        cell_epoch: u64,
        commit_sequence: u64,
        cuts: &crab_ltx::CaptureBatch,
    ) -> Result<Self> {
        let encoded_bytes = cuts.segments.iter().try_fold(0_u64, |total, segment| {
            total
                .checked_add(segment.info().size_bytes)
                .and_then(|bytes| bytes.checked_add(NODE_FRAME_HEADER_BYTES))
        });
        if application.as_bytes().iter().all(|byte| *byte == 0)
            || cell.as_bytes().iter().all(|byte| *byte == 0)
            || incarnation.as_bytes().iter().all(|byte| *byte == 0)
            || cell_epoch == 0
            || commit_sequence == 0
            || cuts.segments.is_empty()
            || encoded_bytes.is_none()
        {
            return Err(Error::Node("invalid node-log submission"));
        }
        Ok(Self {
            application,
            cell,
            incarnation,
            cell_epoch,
            commit_sequence,
            segments: cuts.segments.clone(),
            encoded_bytes: encoded_bytes.ok_or(Error::Node("node-log byte count overflow"))?,
        })
    }

    fn frame_count(&self) -> Result<u64> {
        u64::try_from(self.segments.len()).map_err(|_| Error::Node("node-log frame count overflow"))
    }

    fn load(self, limits: crab_ltx::Limits) -> Result<LoadedNodeLogSubmission> {
        let segments = self
            .segments
            .into_iter()
            .map(|segment| {
                let body = Bytes::from(read_segment(
                    segment.path(),
                    segment.info().size_bytes,
                    limits.max_capture_bytes,
                )?);
                Ok((segment.info().clone(), body))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(LoadedNodeLogSubmission {
            application: self.application,
            cell: self.cell,
            incarnation: self.incarnation,
            cell_epoch: self.cell_epoch,
            commit_sequence: self.commit_sequence,
            segments,
        })
    }
}

struct LoadedNodeLogSubmission {
    application: ApplicationId,
    cell: CellId,
    incarnation: IncarnationId,
    cell_epoch: u64,
    commit_sequence: u64,
    segments: Vec<(crab_ltx::SegmentInfo, Bytes)>,
}

impl LoadedNodeLogSubmission {
    fn encode(self, ticket: CommitTicket, limits: crab_ltx::Limits) -> Result<Vec<Bytes>> {
        self.segments
            .into_iter()
            .enumerate()
            .map(|(offset, (segment, body))| {
                let offset = u64::try_from(offset)
                    .map_err(|_| Error::Node("node-log frame offset overflow"))?;
                let node_sequence = ticket
                    .first_sequence()
                    .checked_add(offset)
                    .ok_or(Error::Node("node-log sequence overflow"))?;
                crab_ltx::encode_node_frame(
                    crab_ltx::NodeFrameScope {
                        leader_session: *ticket.leader_session().as_bytes(),
                        log_epoch: ticket.log_epoch(),
                        node_sequence,
                        application: *self.application.as_bytes(),
                        cell: *self.cell.as_bytes(),
                        incarnation: *self.incarnation.as_bytes(),
                        cell_epoch: self.cell_epoch,
                        commit_sequence: self.commit_sequence,
                    },
                    segment,
                    body,
                    limits,
                )
                .map(|frame| frame.encoded().clone())
                .map_err(Error::from)
            })
            .collect()
    }
}

fn read_segment(path: &std::path::Path, expected_bytes: u64, limit: u64) -> Result<Vec<u8>> {
    let length = usize::try_from(expected_bytes)
        .ok()
        .filter(|_| expected_bytes <= limit)
        .ok_or(Error::Capacity("node-log frame bytes"))?;
    let mut file = std::fs::File::open(path)?;
    if file.metadata()?.len() != expected_bytes {
        return Err(Error::Node("node-log segment size changed"));
    }
    let mut body = vec![0_u8; length];
    file.read_exact(&mut body)?;
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(Error::Node("node-log segment grew while reading"));
    }
    Ok(body)
}

/// Bounded node-wide multiplexer for one leader session and log epoch.
///
/// Submissions receive consecutive tickets before entering the ordered queue.
/// The worker batches frames across Cells and credits fleet durability only
/// after every selected member returns an fsynced contiguous watermark.
pub struct NodeLogShipper {
    sender: std::sync::Mutex<Option<mpsc::Sender<QueuedSubmission>>>,
    worker: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    bytes: Arc<Semaphore>,
    order: tokio::sync::Mutex<()>,
    max_outstanding_bytes: u64,
    gate: DurabilityGate,
    limits: crab_ltx::Limits,
}

impl NodeLogShipper {
    /// Starts one shipper for the exact ensemble owned by `gate`.
    pub fn new(
        gate: DurabilityGate,
        transport: Arc<dyn NodeLogTransport>,
        limits: crab_ltx::Limits,
    ) -> Result<Self> {
        Self::start(
            gate,
            transport,
            limits,
            crate::CellTelemetryHandle::default(),
            BATCH_INTERVAL,
        )
    }

    /// Starts a shipper with a bounded operational telemetry sink.
    pub fn new_with_telemetry(
        gate: DurabilityGate,
        transport: Arc<dyn NodeLogTransport>,
        limits: crab_ltx::Limits,
        telemetry: crate::CellTelemetryHandle,
    ) -> Result<Self> {
        Self::start(gate, transport, limits, telemetry, BATCH_INTERVAL)
    }

    fn start(
        gate: DurabilityGate,
        transport: Arc<dyn NodeLogTransport>,
        limits: crab_ltx::Limits,
        telemetry: crate::CellTelemetryHandle,
        interval: Duration,
    ) -> Result<Self> {
        let (leader, log_epoch, members) = gate.shipping_scope()?;
        let batch_bytes = limits
            .max_capture_bytes
            .checked_add((MAX_BATCH_FRAMES as u64) * NODE_FRAME_HEADER_BYTES)
            .ok_or(Error::Capacity("node-log outstanding bytes"))?;
        let permits = usize::try_from(batch_bytes)
            .ok()
            .filter(|bytes| *bytes <= Semaphore::MAX_PERMITS && *bytes <= u32::MAX as usize)
            .ok_or(Error::Capacity("node-log outstanding bytes"))?;
        let runtime = tokio::runtime::Handle::try_current().map_err(Error::RuntimeStart)?;
        let (sender, receiver) = mpsc::channel(MAX_QUEUED_SUBMISSIONS);
        let bytes = Arc::new(Semaphore::new(permits));
        let worker_gate = gate.clone();
        let worker = runtime.spawn(run_shipper(
            receiver,
            worker_gate,
            Arc::clone(&bytes),
            transport,
            leader,
            log_epoch,
            members,
            batch_bytes,
            telemetry.clone(),
            interval,
        ));
        Ok(Self {
            sender: std::sync::Mutex::new(Some(sender)),
            worker: std::sync::Mutex::new(Some(worker)),
            bytes,
            order: tokio::sync::Mutex::new(()),
            max_outstanding_bytes: batch_bytes,
            gate,
            limits,
        })
    }

    /// Assigns a consecutive ticket and retains the encoded frames for shipping.
    ///
    /// Queue, byte admission, disk reads, and canonical encoding happen before
    /// the ticket reservation commits, so failures cannot create a sequence gap.
    pub async fn submit(&self, submission: NodeLogSubmission) -> Result<CommitTicket> {
        let frame_count = submission.frame_count()?;
        if frame_count > crate::node_log::MAX_TICKET_FRAMES
            || submission.encoded_bytes > self.max_outstanding_bytes
        {
            return Err(Error::Capacity("node-log submission"));
        }
        let permit_count = u32::try_from(submission.encoded_bytes)
            .ok()
            .filter(|bytes| *bytes != 0)
            .ok_or(Error::Capacity("node-log outstanding bytes"))?;
        let reservation = Arc::clone(&self.bytes)
            .acquire_many_owned(permit_count)
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let sender = self
            .sender
            .lock()
            .map_err(|_| Error::Node("node-log shipper lock poisoned"))?
            .clone()
            .ok_or(Error::RuntimeClosed)?;
        let slot = sender
            .reserve_owned()
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let limits = self.limits;
        let loaded = tokio::task::spawn_blocking(move || submission.load(limits))
            .await
            .map_err(Error::FollowerWorkerJoin)??;
        let _ordered = self.order.lock().await;
        let ticket = self.gate.preview(frame_count)?;
        let encoded = match tokio::task::spawn_blocking(move || loaded.encode(ticket, limits)).await
        {
            Ok(Ok(encoded)) => encoded,
            Ok(Err(error)) => return Err(error),
            Err(error) => return Err(Error::FollowerWorkerJoin(error)),
        };
        self.gate.commit(ticket)?;
        let reservation = Arc::new(OutstandingBytes {
            _permit: reservation,
        });
        let frames = encoded
            .into_iter()
            .enumerate()
            .map(|(offset, encoded)| QueuedFrame {
                sequence: ticket.first_sequence().saturating_add(offset as u64),
                encoded,
                _reservation: Arc::clone(&reservation),
            })
            .collect();
        slot.send(QueuedSubmission { frames });
        Ok(ticket)
    }

    /// Closes admission and drains every accepted frame to the current epoch.
    pub async fn shutdown(&self) -> Result<()> {
        self.bytes.close();
        self.sender
            .lock()
            .map_err(|_| Error::Node("node-log shipper lock poisoned"))?
            .take();
        let worker = self
            .worker
            .lock()
            .map_err(|_| Error::Node("node-log shipper lock poisoned"))?
            .take();
        if let Some(worker) = worker {
            worker.await.map_err(Error::FollowerWorkerJoin)?;
        }
        self.gate.stop_shipping();
        Ok(())
    }
}

impl Drop for NodeLogShipper {
    fn drop(&mut self) {
        self.gate.stop_shipping();
    }
}

struct OutstandingBytes {
    _permit: OwnedSemaphorePermit,
}

struct QueuedSubmission {
    frames: Vec<QueuedFrame>,
}

struct QueuedFrame {
    sequence: u64,
    encoded: Bytes,
    _reservation: Arc<OutstandingBytes>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "the worker keeps the exact log epoch, ensemble and bounded admission explicit"
)]
async fn run_shipper(
    mut receiver: mpsc::Receiver<QueuedSubmission>,
    gate: DurabilityGate,
    bytes: Arc<Semaphore>,
    transport: Arc<dyn NodeLogTransport>,
    leader: crate::SessionId,
    log_epoch: u64,
    members: Vec<NodeId>,
    max_batch_bytes: u64,
    telemetry: crate::CellTelemetryHandle,
    interval: Duration,
) {
    let mut pending = VecDeque::<QueuedFrame>::new();
    let mut closed = false;
    loop {
        if pending.is_empty() {
            if closed {
                bytes.close();
                return;
            }
            match receiver.recv().await {
                Some(submission) => pending.extend(submission.frames),
                None => {
                    bytes.close();
                    return;
                }
            }
        }

        let deadline = tokio::time::Instant::now() + interval;
        let mut batch = Vec::<QueuedFrame>::new();
        let mut batch_bytes = 0_u64;
        loop {
            while batch.len() < MAX_BATCH_FRAMES {
                let Some(next) = pending.front() else {
                    break;
                };
                let Some(next_bytes) = batch_bytes.checked_add(next.encoded.len() as u64) else {
                    stop_shipper(&gate, &bytes);
                    return;
                };
                if !batch.is_empty() && next_bytes > max_batch_bytes {
                    break;
                }
                if next_bytes > max_batch_bytes {
                    stop_shipper(&gate, &bytes);
                    return;
                }
                let Some(next) = pending.pop_front() else {
                    stop_shipper(&gate, &bytes);
                    return;
                };
                batch_bytes = next_bytes;
                batch.push(next);
            }
            if batch.len() == MAX_BATCH_FRAMES
                || batch_bytes == max_batch_bytes
                || closed
                || !pending.is_empty()
            {
                break;
            }
            match tokio::time::timeout_at(deadline, receiver.recv()).await {
                Ok(Some(submission)) => pending.extend(submission.frames),
                Ok(None) => {
                    closed = true;
                    break;
                }
                Err(_) => break,
            }
        }

        let append_bytes = batch
            .iter()
            .try_fold(0_u64, |total, frame| {
                total.checked_add(frame.encoded.len() as u64)
            })
            .and_then(|bytes| bytes.checked_mul(members.len() as u64))
            .unwrap_or(u64::MAX);
        let result = append_batch(
            &gate,
            Arc::clone(&transport),
            leader,
            log_epoch,
            &members,
            batch,
        )
        .await;
        telemetry.node_log_append(result.is_ok(), append_bytes);
        if result.is_err() {
            stop_shipper(&gate, &bytes);
            receiver.close();
            return;
        }
    }
}

fn stop_shipper(gate: &DurabilityGate, bytes: &Semaphore) {
    gate.stop_shipping();
    bytes.close();
}

async fn append_batch(
    gate: &DurabilityGate,
    transport: Arc<dyn NodeLogTransport>,
    leader: crate::SessionId,
    log_epoch: u64,
    members: &[NodeId],
    batch: Vec<QueuedFrame>,
) -> Result<()> {
    let first = batch
        .first()
        .ok_or(Error::Node("node-log append batch is empty"))?
        .sequence;
    let last = batch
        .last()
        .ok_or(Error::Node("node-log append batch is empty"))?
        .sequence;
    if !batch
        .windows(2)
        .all(|pair| pair[0].sequence.checked_add(1) == Some(pair[1].sequence))
    {
        return Err(Error::Node("node-log append batch is not contiguous"));
    }
    let frames = batch
        .iter()
        .map(|frame| frame.encoded.clone())
        .collect::<Vec<_>>();
    let covered_through = gate.tiered_through();
    let replies = join_all(members.iter().map(|member| {
        let transport = Arc::clone(&transport);
        let request = AppendRequest {
            leader_session: leader,
            log_epoch,
            frames: frames.clone(),
            covered_through,
        };
        let member = *member;
        async move { (member, transport.append(member, request).await) }
    }))
    .await;
    let mut acknowledgements = Vec::with_capacity(replies.len());
    for (member, reply) in replies {
        let receipt = reply?;
        if receipt.durable_through < last
            || receipt.base_sequence > receipt.durable_through.saturating_add(1)
            || receipt.base_sequence > first && receipt.base_sequence <= last
        {
            return Err(Error::Node("node-log append receipt differs"));
        }
        acknowledgements.push((member, receipt.durable_through));
    }
    for (member, durable_through) in acknowledgements {
        gate.acknowledge(member, durable_through)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::future::BoxFuture;

    use super::*;
    use crate::{
        FollowerReceipt, FollowerStore, LocalFollowerTransport, RetireRequest, SealRequest,
        SessionId, TailRequest,
    };

    #[derive(Default)]
    struct RecordingTransport {
        batches: Mutex<Vec<(NodeId, Vec<u64>)>>,
        fail: Option<NodeId>,
    }

    #[derive(Default)]
    struct RecordingTelemetry {
        appends: Mutex<Vec<(bool, u64)>>,
    }

    impl crate::CellTelemetry for RecordingTelemetry {
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
                Ok(FollowerReceipt {
                    base_sequence: first,
                    durable_through: last,
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
        let mut database = crab_ltx::ManagedDb::open(
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
            crate::CellTelemetryHandle::default(),
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
            crate::DurabilitySource::Fleet
        );
        assert_eq!(
            gate.prove(second).await.unwrap().source(),
            crate::DurabilitySource::Fleet
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
        let handle = crate::CellTelemetryHandle::default();
        handle.install(telemetry.clone()).unwrap();
        let shipper = NodeLogShipper::new_with_telemetry(
            gate,
            transport,
            crab_ltx::Limits::default(),
            handle,
        )
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
            crate::CellTelemetryHandle::default(),
            Duration::from_millis(50),
        )
        .unwrap();

        let ticket = shipper.submit(submission(&cuts)).await.unwrap();
        assert_eq!(
            gate.prove(ticket).await.unwrap().source(),
            crate::DurabilitySource::Fleet
        );
        shipper.shutdown().await.unwrap();

        assert_eq!(ticket.first_sequence(), 1);
        assert_eq!(ticket.last_sequence(), 65);
        assert_eq!(transport.batch_sizes(node(2)), [64, 1]);
        assert!(gate.issue(1).is_err());
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
            crate::CellTelemetryHandle::default(),
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
            crate::DurabilitySource::Object
        );
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
            crate::CellTelemetryHandle::default(),
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
            crate::DurabilitySource::Object
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
        invalid.segments[0] =
            crab_ltx::LocalSegment::new(invalid.segments[0].path().to_owned(), info);
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
            crate::DurabilitySource::Fleet
        );
        shipper.shutdown().await.unwrap();
    }
}
