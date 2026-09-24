//! Node log: durability gate, rotation barrier, and recovery overlays.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use tokio::sync::Notify;

use crate::identity::NodeId;
use crate::identity::SessionId;
use crate::node::log_transport::{NodeLogTransport, RetireRequest};
use crate::node::{NodeDirectory, VersionedNodeAdvertisement};
use crate::{Error, Result};

mod recovery;

pub use recovery::*;

pub(crate) const MAX_TICKET_FRAMES: u64 = 1_024;

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

/// Proof that one gate stopped issuance after every old-epoch ticket became object-covered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeLogRotationBarrier {
    leader_session: SessionId,
    log_epoch: u64,
    members: Vec<NodeId>,
    covered_through: u64,
}

/// New authoritative enrollment and inactive durability gate after rotation.
pub struct RotatedNodeLog {
    pub enrollment: VersionedNodeAdvertisement,
    pub gate: DurabilityGate,
}

impl NodeLogRotationBarrier {
    #[must_use]
    pub const fn leader_session(&self) -> SessionId {
        self.leader_session
    }

    #[must_use]
    pub const fn log_epoch(&self) -> u64 {
        self.log_epoch
    }

    #[must_use]
    pub fn members(&self) -> &[NodeId] {
        &self.members
    }

    #[must_use]
    pub const fn covered_through(&self) -> u64 {
        self.covered_through
    }
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
    members: HashSet<NodeId>,
    follower_through: HashMap<NodeId, u64>,
    object_covered: BTreeSet<u64>,
    tiered_through: u64,
    next_sequence: u64,
    fleet_active: bool,
    rotating: bool,
    fenced: bool,
}

impl DurabilityGate {
    /// Creates one inactive gate for the exact recruited follower ensemble.
    pub fn new(
        leader_session: SessionId,
        leader_node: NodeId,
        log_epoch: u64,
        members: impl IntoIterator<Item = NodeId>,
    ) -> Result<Self> {
        let members = members.into_iter().collect::<HashSet<_>>();
        if leader_session.as_bytes().iter().all(|byte| *byte == 0)
            || leader_node.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || members.is_empty()
            || members.contains(&leader_node)
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
                rotating: false,
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
        if state.rotating {
            return Err(Error::Node("node log is rotating"));
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

    pub(crate) fn preview(&self, frame_count: u64) -> Result<CommitTicket> {
        if !(1..=MAX_TICKET_FRAMES).contains(&frame_count) {
            return Err(Error::Node("invalid node-log ticket size"));
        }
        let state = self.lock()?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        if state.rotating {
            return Err(Error::Node("node log is rotating"));
        }
        let first_sequence = state.next_sequence;
        let last_sequence = first_sequence
            .checked_add(frame_count - 1)
            .ok_or(Error::Node("node sequence overflow"))?;
        Ok(CommitTicket {
            leader_session: state.leader_session,
            log_epoch: state.log_epoch,
            first_sequence,
            last_sequence,
        })
    }

    pub(crate) fn commit(&self, ticket: CommitTicket) -> Result<()> {
        let mut state = self.lock()?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        if state.rotating {
            return Err(Error::Node("node log is rotating"));
        }
        if ticket.leader_session != state.leader_session
            || ticket.log_epoch != state.log_epoch
            || ticket.first_sequence != state.next_sequence
            || ticket.first_sequence == 0
            || ticket.first_sequence > ticket.last_sequence
            || ticket.last_sequence.saturating_sub(ticket.first_sequence) >= MAX_TICKET_FRAMES
        {
            return Err(Error::Node("node-log ticket reservation changed"));
        }
        state.next_sequence = ticket
            .last_sequence
            .checked_add(1)
            .ok_or(Error::Node("node sequence overflow"))?;
        Ok(())
    }

    pub(crate) fn shipping_scope(&self) -> Result<(SessionId, u64, Vec<NodeId>)> {
        let state = self.lock()?;
        if state.fenced || state.rotating {
            return Err(Error::Fenced);
        }
        let mut members = state.members.iter().copied().collect::<Vec<_>>();
        members.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        Ok((state.leader_session, state.log_epoch, members))
    }

    pub(crate) fn stop_shipping(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.fleet_active = false;
            state.rotating = true;
        }
        self.changed.notify_waiters();
    }

    /// Enables fleet proofs only after the authoritative active CAS succeeds.
    pub fn activate_fleet(&self) -> Result<()> {
        let mut state = self.lock()?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        if state.rotating {
            return Err(Error::Node("node log is rotating"));
        }
        state.fleet_active = true;
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Records one authenticated follower's fsynced contiguous watermark.
    pub fn acknowledge(&self, member: NodeId, durable_through: u64) -> Result<()> {
        let mut state = self.lock()?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        if state.rotating {
            return Err(Error::Node("node log is rotating"));
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

    /// Waits until every enrolled follower has fsynced the complete ticket.
    ///
    /// This does not enable fleet durability. The caller must first complete
    /// the authoritative inactive-to-active CAS and then call `activate_fleet`.
    pub async fn wait_followers(&self, ticket: CommitTicket) -> Result<()> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.lock()?;
                validate_ticket(&state, ticket)?;
                if state.fenced {
                    return Err(Error::Fenced);
                }
                if state.rotating {
                    return Err(Error::Node("node log is rotating"));
                }
                if state.members.iter().all(|member| {
                    state
                        .follower_through
                        .get(member)
                        .is_some_and(|through| *through >= ticket.last_sequence)
                }) {
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    /// Marks exactly the frame range now reachable through an authoritative root.
    pub fn prove_object(&self, ticket: CommitTicket) -> Result<u64> {
        let mut state = self.lock()?;
        validate_ticket(&state, ticket)?;
        if state.fenced {
            return Err(Error::Fenced);
        }
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

    pub(crate) fn preview_object(&self, ticket: CommitTicket) -> Result<u64> {
        let state = self.lock()?;
        validate_ticket(&state, ticket)?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        let mut tiered_through = state.tiered_through;
        while let Some(next) = tiered_through.checked_add(1) {
            if state.object_covered.contains(&next)
                || (ticket.first_sequence..=ticket.last_sequence).contains(&next)
            {
                tiered_through = next;
            } else {
                break;
            }
        }
        Ok(tiered_through)
    }

    #[must_use]
    pub fn tiered_through(&self) -> u64 {
        self.lock().map_or(0, |state| state.tiered_through)
    }

    #[must_use]
    pub fn issued_through(&self) -> u64 {
        self.lock()
            .map_or(0, |state| state.next_sequence.saturating_sub(1))
    }

    /// Stops ticket issuance after the entire old epoch is object-covered.
    pub fn begin_rotation(&self) -> Result<NodeLogRotationBarrier> {
        let mut state = self.lock()?;
        if state.fenced {
            return Err(Error::Fenced);
        }
        let issued_through = state.next_sequence.saturating_sub(1);
        if state.tiered_through != issued_through {
            return Err(Error::PendingPublication);
        }
        state.rotating = true;
        state.fleet_active = false;
        let mut members = state.members.iter().copied().collect::<Vec<_>>();
        members.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        let barrier = NodeLogRotationBarrier {
            leader_session: state.leader_session,
            log_epoch: state.log_epoch,
            members,
            covered_through: state.tiered_through,
        };
        drop(state);
        self.changed.notify_waiters();
        Ok(barrier)
    }

    /// Waits until either complete durability path covers the whole ticket.
    pub async fn prove(&self, ticket: CommitTicket) -> Result<DurabilityProof> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
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

/// Best-effort retires old follower lanes before CASing a newly selected log epoch.
///
/// Unreachable followers may retain inert data, but cannot block rotation.
/// A CAS failure leaves the old gate closed to new tickets. Retrying is safe:
/// the barrier and follower retire markers are exact and idempotent.
pub async fn rotate_node_log(
    directory: &NodeDirectory,
    transport: Arc<dyn NodeLogTransport>,
    observed: &VersionedNodeAdvertisement,
    gate: &DurabilityGate,
    required_follower_bytes: u64,
    live_node_limit: usize,
    now_ms: i64,
) -> Result<RotatedNodeLog> {
    let barrier = gate.begin_rotation()?;
    retire_node_log(transport, &barrier).await?;
    let enrollment = directory
        .rotate_log(
            observed,
            &barrier,
            required_follower_bytes,
            live_node_limit,
            now_ms,
        )
        .await?;
    let log = enrollment
        .advertisement()
        .log()
        .ok_or(Error::Node("rotated node session lost its log"))?;
    let gate = DurabilityGate::new(
        enrollment.advertisement().session(),
        enrollment.advertisement().node(),
        log.epoch(),
        log.members().iter().copied(),
    )?;
    Ok(RotatedNodeLog { enrollment, gate })
}

/// Best-effort retires a covered epoch and CAS-clears it for clean withdrawal.
///
/// Unreachable followers may retain inert bytes. The authoritative clear
/// rejects every later append before the session can be withdrawn.
pub async fn close_node_log(
    directory: &NodeDirectory,
    transport: Arc<dyn NodeLogTransport>,
    observed: &VersionedNodeAdvertisement,
    gate: &DurabilityGate,
    now_ms: i64,
) -> Result<VersionedNodeAdvertisement> {
    let barrier = gate.begin_rotation()?;
    retire_node_log(transport, &barrier).await?;
    directory.close_log(observed, &barrier, now_ms).await
}

pub(crate) async fn retire_node_log(
    transport: Arc<dyn NodeLogTransport>,
    barrier: &NodeLogRotationBarrier,
) -> Result<()> {
    let retirements = join_all(barrier.members().iter().map(|member| {
        let transport = Arc::clone(&transport);
        let member = *member;
        let request = RetireRequest {
            leader_session: barrier.leader_session(),
            log_epoch: barrier.log_epoch(),
            covered_through: barrier.covered_through(),
        };
        async move { transport.retire(member, request).await }
    }))
    .await;
    let expected_base = barrier.covered_through().saturating_add(1);
    for receipt in retirements.into_iter().flatten() {
        if receipt.base_sequence != expected_base
            || receipt.durable_through != barrier.covered_through()
        {
            return Err(Error::Node("follower retire receipt differs"));
        }
    }
    Ok(())
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
mod tests;
