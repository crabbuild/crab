use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::{Error, Result, SessionId};

const MAX_TICKET_FRAMES: u64 = 1_024;

/// Exact published Cell state used to validate a recovered node-log witness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryBase {
    pub application: [u8; 16],
    pub cell_epoch: u64,
    pub root: crab_ltx::RootRef,
}

/// One Cell's verified tail selected from a complete node-log witness.
pub struct RecoveredCellTail {
    pub application: [u8; 16],
    pub cell_epoch: u64,
    pub first_node_sequence: u64,
    pub last_node_sequence: u64,
    pub overlay: crab_ltx::RecoveryOverlay,
}

/// Splits one complete, contiguous witness into exact per-Cell overlays.
pub fn build_recovery_overlays(
    frames: Vec<crab_ltx::VerifiedNodeFrame>,
    bases: &[RecoveryBase],
    limits: crab_ltx::Limits,
) -> Result<Vec<RecoveredCellTail>> {
    let first = frames
        .first()
        .ok_or(Error::Node("recovery witness is empty"))?;
    let leader = first.scope().leader_session;
    let log_epoch = first.scope().log_epoch;
    if frames
        .iter()
        .any(|frame| frame.scope().leader_session != leader || frame.scope().log_epoch != log_epoch)
        || !frames.windows(2).all(|pair| {
            pair[0].scope().node_sequence.checked_add(1) == Some(pair[1].scope().node_sequence)
        })
    {
        return Err(Error::Node("recovery witness is not contiguous"));
    }

    type Key = ([u8; 16], [u8; 32], [u8; 16], u64);
    let base_by_key = bases
        .iter()
        .map(|base| {
            (
                (
                    base.application,
                    base.root.cell,
                    base.root.incarnation,
                    base.cell_epoch,
                ),
                *base,
            )
        })
        .collect::<BTreeMap<Key, RecoveryBase>>();
    if base_by_key.len() != bases.len() {
        return Err(Error::Node("recovery bases contain duplicate Cell scope"));
    }

    let mut grouped = BTreeMap::<Key, Vec<crab_ltx::VerifiedNodeFrame>>::new();
    for frame in frames {
        let scope = frame.scope();
        let key = (
            scope.application,
            scope.cell,
            scope.incarnation,
            scope.cell_epoch,
        );
        if !base_by_key.contains_key(&key) {
            return Err(Error::Node("recovery frame has no exact published base"));
        }
        grouped.entry(key).or_default().push(frame);
    }

    let mut recovered = Vec::with_capacity(grouped.len());
    for (key, cell_frames) in grouped {
        let base = base_by_key
            .get(&key)
            .ok_or(Error::Node("recovery frame base disappeared"))?;
        let first_frame = cell_frames
            .first()
            .ok_or(Error::Node("recovery Cell tail is empty"))?;
        let last = cell_frames
            .last()
            .ok_or(Error::Node("recovery Cell tail is empty"))?;
        let first_commit = base
            .root
            .commit_sequence
            .checked_add(1)
            .ok_or(Error::Node("recovery commit sequence overflow"))?;
        if first_frame.scope().commit_sequence != first_commit
            || !cell_frames.windows(2).all(|pair| {
                let left = pair[0].scope().commit_sequence;
                let right = pair[1].scope().commit_sequence;
                right == left || left.checked_add(1) == Some(right)
            })
        {
            return Err(Error::Node("recovery Cell commit sequence has a gap"));
        }
        let entries = cell_frames
            .iter()
            .map(|frame| {
                crab_ltx::bundle::BundleEntry::for_cell(
                    base.root.cell,
                    base.root.incarnation,
                    frame.segment().clone(),
                    frame.body().to_vec(),
                )
            })
            .collect();
        let bundle = crab_ltx::bundle::Bundle::encode(entries, limits)?;
        recovered.push(RecoveredCellTail {
            application: base.application,
            cell_epoch: base.cell_epoch,
            first_node_sequence: first_frame.scope().node_sequence,
            last_node_sequence: last.scope().node_sequence,
            overlay: crab_ltx::RecoveryOverlay::new(
                base.root,
                bundle,
                last.segment().position(),
                last.scope().commit_sequence,
            ),
        });
    }
    Ok(recovered)
}

/// One actor-issued consecutive frame range awaiting a durability proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitTicket {
    leader_session: SessionId,
    log_epoch: u64,
    first_sequence: u64,
    last_sequence: u64,
}

impl CommitTicket {
    #[must_use]
    pub const fn leader_session(&self) -> SessionId {
        self.leader_session
    }

    #[must_use]
    pub const fn log_epoch(&self) -> u64 {
        self.log_epoch
    }

    #[must_use]
    pub const fn first_sequence(&self) -> u64 {
        self.first_sequence
    }

    #[must_use]
    pub const fn last_sequence(&self) -> u64 {
        self.last_sequence
    }
}

/// Durable path that authorized release of one committed result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurabilitySource {
    Fleet,
    Object,
}

/// Non-forgeable proof issued by the gate after one complete path wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurabilityProof {
    ticket: CommitTicket,
    source: DurabilitySource,
}

impl DurabilityProof {
    #[must_use]
    pub const fn ticket(&self) -> CommitTicket {
        self.ticket
    }

    #[must_use]
    pub const fn source(&self) -> DurabilitySource {
        self.source
    }
}

/// Coordinates object and follower durability without exposing forgeable ACKs.
///
/// The gate owns one leader session and one log epoch. Tickets are issued in
/// strict node-sequence order. Fleet proof is disabled until the session's
/// `active=false -> active=true` CAS has been observed by the caller.
#[derive(Clone)]
pub struct DurabilityGate {
    inner: Arc<Mutex<GateState>>,
    changed: Arc<Notify>,
}

struct GateState {
    leader_session: SessionId,
    log_epoch: u64,
    members: HashSet<SessionId>,
    follower_through: HashMap<SessionId, u64>,
    object_covered: BTreeSet<u64>,
    tiered_through: u64,
    next_sequence: u64,
    fleet_active: bool,
    fenced: bool,
}

impl DurabilityGate {
    /// Creates one inactive gate for the exact recruited follower ensemble.
    pub fn new(
        leader_session: SessionId,
        log_epoch: u64,
        members: impl IntoIterator<Item = SessionId>,
    ) -> Result<Self> {
        let members = members.into_iter().collect::<HashSet<_>>();
        if leader_session.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || members.is_empty()
            || members.contains(&leader_session)
            || members
                .iter()
                .any(|member| member.as_bytes().iter().all(|byte| *byte == 0))
        {
            return Err(Error::Node("invalid node-log ensemble"));
        }
        let follower_through = members.iter().map(|member| (*member, 0)).collect();
        Ok(Self {
            inner: Arc::new(Mutex::new(GateState {
                leader_session,
                log_epoch,
                members,
                follower_through,
                object_covered: BTreeSet::new(),
                tiered_through: 0,
                next_sequence: 1,
                fleet_active: false,
                fenced: false,
            })),
            changed: Arc::new(Notify::new()),
        })
    }

    /// Allocates the next consecutive frame range after SQLite capture.
    pub fn issue(&self, frame_count: u64) -> Result<CommitTicket> {
        if !(1..=MAX_TICKET_FRAMES).contains(&frame_count) {
            return Err(Error::Node("invalid node-log ticket size"));
        }
        let mut state = self.lock()?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        let first_sequence = state.next_sequence;
        let last_sequence = first_sequence
            .checked_add(frame_count - 1)
            .ok_or(Error::Node("node sequence overflow"))?;
        state.next_sequence = last_sequence
            .checked_add(1)
            .ok_or(Error::Node("node sequence overflow"))?;
        Ok(CommitTicket {
            leader_session: state.leader_session,
            log_epoch: state.log_epoch,
            first_sequence,
            last_sequence,
        })
    }

    /// Enables fleet proofs only after the authoritative active CAS succeeds.
    pub fn activate_fleet(&self) -> Result<()> {
        let mut state = self.lock()?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        state.fleet_active = true;
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Records one authenticated follower's fsynced contiguous watermark.
    pub fn acknowledge(&self, member: SessionId, durable_through: u64) -> Result<()> {
        let mut state = self.lock()?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        let next_sequence = state.next_sequence;
        let current = state
            .follower_through
            .get_mut(&member)
            .ok_or(Error::Node("node-log ACK came from a non-member"))?;
        if durable_through < *current || durable_through >= next_sequence {
            return Err(Error::Node("node-log ACK watermark is invalid"));
        }
        *current = durable_through;
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Marks exactly the frame range now reachable through an authoritative root.
    pub fn prove_object(&self, ticket: CommitTicket) -> Result<u64> {
        let mut state = self.lock()?;
        validate_ticket(&state, ticket)?;
        for sequence in ticket.first_sequence..=ticket.last_sequence {
            state.object_covered.insert(sequence);
        }
        while state.object_covered.contains(&(state.tiered_through + 1)) {
            state.tiered_through += 1;
        }
        let tiered_through = state.tiered_through;
        drop(state);
        self.changed.notify_waiters();
        Ok(tiered_through)
    }

    #[must_use]
    pub fn tiered_through(&self) -> u64 {
        self.lock().map_or(0, |state| state.tiered_through)
    }

    /// Waits until either complete durability path covers the whole ticket.
    pub async fn prove(&self, ticket: CommitTicket) -> Result<DurabilityProof> {
        loop {
            let notified = self.changed.notified();
            if let Some(proof) = self.proof(ticket)? {
                return Ok(proof);
            }
            notified.await;
        }
    }

    /// Permanently rejects new tickets and wakes every waiting command.
    pub fn fence(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.fenced = true;
        }
        self.changed.notify_waiters();
    }

    fn proof(&self, ticket: CommitTicket) -> Result<Option<DurabilityProof>> {
        let state = self.lock()?;
        validate_ticket(&state, ticket)?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        if (ticket.first_sequence..=ticket.last_sequence)
            .all(|sequence| state.object_covered.contains(&sequence))
        {
            return Ok(Some(DurabilityProof {
                ticket,
                source: DurabilitySource::Object,
            }));
        }
        if state.fleet_active
            && state.members.iter().all(|member| {
                state
                    .follower_through
                    .get(member)
                    .is_some_and(|through| *through >= ticket.last_sequence)
            })
        {
            return Ok(Some(DurabilityProof {
                ticket,
                source: DurabilitySource::Fleet,
            }));
        }
        Ok(None)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, GateState>> {
        self.inner
            .lock()
            .map_err(|_| Error::Node("node-log durability gate poisoned"))
    }
}

fn validate_ticket(state: &GateState, ticket: CommitTicket) -> Result<()> {
    if ticket.leader_session != state.leader_session
        || ticket.log_epoch != state.log_epoch
        || ticket.first_sequence == 0
        || ticket.first_sequence > ticket.last_sequence
        || ticket.last_sequence >= state.next_sequence
    {
        return Err(Error::Node("node-log ticket is not owned by this gate"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn verified_frame(
        sequence: u64,
        cell: [u8; 32],
        incarnation: [u8; 16],
        segment: &crab_ltx::LocalSegment,
    ) -> crab_ltx::VerifiedNodeFrame {
        crab_ltx::encode_node_frame(
            crab_ltx::NodeFrameScope {
                leader_session: [1; 16],
                log_epoch: 2,
                node_sequence: sequence,
                application: [9; 16],
                cell,
                incarnation,
                cell_epoch: 3,
                commit_sequence: 2,
            },
            segment.info().clone(),
            Bytes::from(std::fs::read(segment.path()).unwrap()),
            crab_ltx::Limits::default(),
        )
        .unwrap()
    }

    fn session(byte: u8) -> SessionId {
        SessionId::from_bytes([byte; 16])
    }

    #[tokio::test]
    async fn fleet_requires_activation_and_every_follower() {
        let gate = DurabilityGate::new(session(1), 2, [session(3), session(4)]).unwrap();
        let ticket = gate.issue(2).unwrap();
        gate.acknowledge(session(3), 2).unwrap();
        gate.acknowledge(session(4), 2).unwrap();
        assert!(gate.proof(ticket).unwrap().is_none());
        gate.activate_fleet().unwrap();
        assert_eq!(
            gate.prove(ticket).await.unwrap().source(),
            DurabilitySource::Fleet
        );
    }

    #[tokio::test]
    async fn object_proof_wins_independently_and_watermark_stays_contiguous() {
        let gate = DurabilityGate::new(session(1), 2, [session(3)]).unwrap();
        let first = gate.issue(1).unwrap();
        let second = gate.issue(1).unwrap();
        assert_eq!(gate.prove_object(second).unwrap(), 0);
        assert_eq!(
            gate.prove(second).await.unwrap().source(),
            DurabilitySource::Object
        );
        assert_eq!(gate.prove_object(first).unwrap(), 2);
        assert_eq!(gate.tiered_through(), 2);
    }

    #[tokio::test]
    async fn fencing_wakes_waiters_and_rejects_late_acks() {
        let gate = DurabilityGate::new(session(1), 2, [session(3)]).unwrap();
        let ticket = gate.issue(1).unwrap();
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.prove(ticket).await })
        };
        gate.fence();
        assert!(matches!(waiter.await.unwrap(), Err(Error::Fenced)));
        assert!(matches!(
            gate.acknowledge(session(3), 1),
            Err(Error::Fenced)
        ));
    }

    #[test]
    fn recovery_witness_splits_interleaved_cells_against_exact_bases() {
        let limits = crab_ltx::Limits::default();
        let directory = tempfile::TempDir::new().unwrap();
        let mut left =
            crab_ltx::ManagedDb::open(&directory.path().join("left.sqlite"), limits).unwrap();
        let mut right =
            crab_ltx::ManagedDb::open(&directory.path().join("right.sqlite"), limits).unwrap();
        for database in [&mut left, &mut right] {
            database
                .transaction(|transaction| {
                    transaction.execute_batch(
                        "CREATE TABLE events(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                         INSERT INTO events(body) VALUES ('base')",
                    )
                })
                .unwrap();
        }
        let left_base = left.capture().unwrap();
        let right_base = right.capture().unwrap();
        for database in [&mut left, &mut right] {
            database
                .transaction(|transaction| {
                    transaction.execute("INSERT INTO events(body) VALUES ('tail')", [])?;
                    Ok(())
                })
                .unwrap();
        }
        let left_tail = left.capture().unwrap();
        let right_tail = right.capture().unwrap();
        let left_cell = [4; 32];
        let left_incarnation = [5; 16];
        let right_cell = [6; 32];
        let right_incarnation = [7; 16];
        let frames = vec![
            verified_frame(
                1,
                left_cell,
                left_incarnation,
                left_tail.segments.first().unwrap(),
            ),
            verified_frame(
                2,
                right_cell,
                right_incarnation,
                right_tail.segments.first().unwrap(),
            ),
        ];
        let bases = [
            RecoveryBase {
                application: [9; 16],
                cell_epoch: 3,
                root: crab_ltx::RootRef {
                    cell: left_cell,
                    incarnation: left_incarnation,
                    digest: [10; 32],
                    position: left_base.position,
                    commit_sequence: 1,
                },
            },
            RecoveryBase {
                application: [9; 16],
                cell_epoch: 3,
                root: crab_ltx::RootRef {
                    cell: right_cell,
                    incarnation: right_incarnation,
                    digest: [11; 32],
                    position: right_base.position,
                    commit_sequence: 1,
                },
            },
        ];

        let recovered = build_recovery_overlays(frames, &bases, limits).unwrap();
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0].first_node_sequence, 1);
        assert_eq!(recovered[0].overlay.final_position(), left_tail.position);
        assert_eq!(recovered[1].last_node_sequence, 2);
        assert_eq!(recovered[1].overlay.final_position(), right_tail.position);
        left.close().unwrap();
        right.close().unwrap();
    }
}
