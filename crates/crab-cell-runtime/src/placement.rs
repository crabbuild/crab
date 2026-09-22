use std::collections::HashSet;

use crate::{CellId, Error, NodeAdvertisement, NodeId, NodePlacementCapacity, Result, SessionId};

const MAX_OBSERVATION_AGE_MS: i64 = 30_000;
const SCORE_SCALE: u128 = 1_000;
const MIN_TRANSFER_GAIN: u128 = 50_000;
const MIN_RESIDENCE_MS: i64 = 60_000;
const MAX_TRANSFERS_PER_TICK: usize = 2;
const MAX_TRANSFER_BYTES_PER_TICK: u64 = 8 * 1024 * 1024 * 1024;

/// Pressure class supplied by the signed node observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlacementPressure {
    Normal,
    Constrained,
    Shedding,
    Critical,
}

/// Authenticated, bounded node values consumed by the pure placement planner.
///
/// Authentication and timestamp validation remain the advertisement owner's
/// responsibility. The planner treats a missing/invalid observation as
/// ineligible rather than interpreting it as zero load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacementObservation {
    pub node: NodeId,
    pub session: SessionId,
    pub observed_at_ms: i64,
    pub memory_capacity_bytes: u64,
    pub free_memory_bytes: u64,
    pub disk_capacity_bytes: u64,
    pub free_disk_bytes: u64,
    pub active_cells: u32,
    pub max_active_cells: u32,
    pub running_jobs: u32,
    pub job_capacity: u32,
    pub publication_backlog: u32,
    pub hydration_backlog: u32,
    pub primitive_backlog: u32,
    pub pressure: PlacementPressure,
    pub draining: bool,
    pub authenticated: bool,
    pub locality_bonus: u16,
    pub current_owner: bool,
}

impl PlacementObservation {
    /// Converts the signed runtime block carried by a live advertisement into
    /// planner input without substituting host-wide or zero-valued guesses.
    pub fn from_signed_advertisement(
        advertisement: &NodeAdvertisement,
        now_ms: i64,
        current_owner: bool,
    ) -> Result<Self> {
        let placement = advertisement
            .placement_capacity()
            .ok_or(Error::Node("placement snapshot is missing"))?;
        let capacity = advertisement.capacity();
        if now_ms < 0
            || advertisement.expires_at_ms() <= now_ms
            || !advertisement.has_signed_placement()
        {
            return Err(Error::Node("placement advertisement is stale"));
        }
        Ok(Self::from_signed_capacity(
            advertisement,
            placement,
            capacity,
            now_ms,
            current_owner,
        ))
    }

    fn from_signed_capacity(
        advertisement: &NodeAdvertisement,
        placement: NodePlacementCapacity,
        capacity: crate::NodeCapacity,
        _now_ms: i64,
        current_owner: bool,
    ) -> Self {
        Self {
            node: advertisement.node(),
            session: advertisement.session(),
            observed_at_ms: advertisement.issued_at_ms(),
            memory_capacity_bytes: placement.memory_capacity_bytes,
            free_memory_bytes: capacity.free_memory_bytes,
            disk_capacity_bytes: placement.disk_capacity_bytes,
            free_disk_bytes: capacity.free_disk_bytes,
            active_cells: placement.active_cells,
            max_active_cells: placement.max_active_cells,
            running_jobs: placement.running_jobs,
            job_capacity: placement.job_capacity,
            publication_backlog: placement.publication_backlog,
            hydration_backlog: placement.hydration_backlog,
            primitive_backlog: placement.primitive_backlog,
            pressure: if capacity.free_memory_bytes == 0 || capacity.free_disk_bytes == 0 {
                PlacementPressure::Critical
            } else {
                PlacementPressure::Normal
            },
            draining: capacity.free_memory_bytes == 0 || capacity.free_disk_bytes == 0,
            authenticated: true,
            locality_bonus: 0,
            current_owner,
        }
    }
}

/// Stable reason for accepting or rejecting one placement candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlacementEligibility {
    Eligible,
    Unauthenticated,
    Stale,
    Draining,
    CriticalPressure,
    NoMemoryHeadroom,
    NoDiskHeadroom,
    NoCellCapacity,
    NoJobCapacity,
}

/// Score and eligibility explanation for one candidate node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacementScore {
    pub node: NodeId,
    pub session: SessionId,
    pub score: u128,
    pub eligibility: PlacementEligibility,
}

/// Actor-sampled demand for one locally owned Cell. A missing or unsettled
/// sample cannot be used as a transfer hint; the actor must recheck on release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellTransferDemand {
    pub cell: CellId,
    pub source: SessionId,
    pub generation: u64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub job_credits: u32,
    pub resident_since_ms: i64,
    pub last_moved_at_ms: Option<i64>,
    pub stable_observations: u8,
    pub settled: bool,
}

/// Advisory transfer proposal. It conveys neither release nor receiver admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellTransferIntent {
    pub cell: CellId,
    pub source: SessionId,
    pub generation: u64,
    pub destination: SessionId,
    pub observed_at_ms: i64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub job_credits: u32,
}

/// Deterministic weighted placement policy. It never mutates authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacementPlanner {
    max_observation_age_ms: i64,
}

impl Default for PlacementPlanner {
    fn default() -> Self {
        Self {
            max_observation_age_ms: MAX_OBSERVATION_AGE_MS,
        }
    }
}

impl PlacementPlanner {
    /// Creates a planner with a bounded observation freshness window.
    pub fn new(max_observation_age_ms: i64) -> Result<Self> {
        if max_observation_age_ms <= 0 {
            return Err(Error::Control("placement freshness window is invalid"));
        }
        Ok(Self {
            max_observation_age_ms,
        })
    }

    /// Ranks candidates for one Cell with a total, permutation-invariant order.
    pub fn rank(
        &self,
        cell: CellId,
        now_ms: i64,
        observations: &[PlacementObservation],
    ) -> Result<Vec<PlacementScore>> {
        if now_ms < 0 {
            return Err(Error::Control("placement time is invalid"));
        }
        let mut scores = observations
            .iter()
            .map(|observation| self.score(cell, now_ms, *observation))
            .collect::<Vec<_>>();
        scores.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| right.eligibility.cmp(&left.eligibility))
                .then_with(|| left.node.as_bytes().cmp(right.node.as_bytes()))
                .then_with(|| left.session.as_bytes().cmp(right.session.as_bytes()))
        });
        Ok(scores)
    }

    /// Returns the best eligible node, if any.
    pub fn choose(
        &self,
        cell: CellId,
        now_ms: i64,
        observations: &[PlacementObservation],
    ) -> Result<Option<PlacementScore>> {
        Ok(self
            .rank(cell, now_ms, observations)?
            .into_iter()
            .find(|score| score.eligibility == PlacementEligibility::Eligible))
    }

    /// Plans at most two settled transfers and 8 GiB of projected restore
    /// bytes from one authenticated fleet snapshot. Receiver capacity is
    /// projected across selected intents, then rechecked during activation.
    pub fn plan_transfers(
        &self,
        now_ms: i64,
        observations: &[PlacementObservation],
        demands: &[CellTransferDemand],
    ) -> Result<Vec<CellTransferIntent>> {
        if now_ms < 0 || observations.len() > 10_000 || demands.len() > 10_000 {
            return Err(Error::Control("transfer snapshot is invalid"));
        }
        let mut nodes = HashSet::new();
        let mut sessions = HashSet::new();
        for observation in observations {
            if !nodes.insert(observation.node) || !sessions.insert(observation.session) {
                return Err(Error::Control("transfer snapshot duplicates a node"));
            }
        }
        let mut cells = HashSet::new();
        for demand in demands {
            if !cells.insert(demand.cell) {
                return Err(Error::Control("transfer snapshot duplicates a Cell"));
            }
        }
        let mut projected = observations.to_vec();
        for observation in &mut projected {
            observation.current_owner = false;
        }
        let mut candidates = demands.to_vec();
        candidates.sort_by(|left, right| {
            let priority = |demand: &CellTransferDemand| {
                observations
                    .iter()
                    .find(|node| node.session == demand.source)
                    .map_or(0, |node| {
                        u8::from(node.draining) * 2
                            + u8::from(node.pressure >= PlacementPressure::Shedding)
                    })
            };
            priority(right)
                .cmp(&priority(left))
                .then_with(|| left.cell.as_bytes().cmp(right.cell.as_bytes()))
        });
        let mut intents = Vec::new();
        let mut bytes = 0u64;
        for demand in candidates {
            if intents.len() == MAX_TRANSFERS_PER_TICK {
                break;
            }
            let Some(source) = observations
                .iter()
                .find(|node| node.session == demand.source)
            else {
                continue;
            };
            if !demand.settled
                || demand.generation == 0
                || demand.memory_bytes == 0
                || demand.disk_bytes == 0
                || demand.job_credits == 0
                || demand.resident_since_ms < 0
                || demand.resident_since_ms > now_ms
                || demand
                    .last_moved_at_ms
                    .is_some_and(|at| at < 0 || at > now_ms)
                || !source.authenticated
                || self.eligibility(now_ms, *source) == PlacementEligibility::Stale
            {
                continue;
            }
            let urgent = source.draining || source.pressure >= PlacementPressure::Shedding;
            if !urgent
                && (demand.stable_observations < 2
                    || now_ms - demand.resident_since_ms < MIN_RESIDENCE_MS
                    || demand
                        .last_moved_at_ms
                        .is_some_and(|at| now_ms - at < MIN_RESIDENCE_MS))
            {
                continue;
            }
            let Some(next_bytes) = bytes.checked_add(demand.disk_bytes) else {
                continue;
            };
            if next_bytes > MAX_TRANSFER_BYTES_PER_TICK {
                continue;
            }
            let mut owned_source = *source;
            owned_source.current_owner = true;
            let source_score = self.score(demand.cell, now_ms, owned_source).score;
            let destination = self
                .rank(demand.cell, now_ms, &projected)?
                .into_iter()
                .find(|score| {
                    score.session != demand.source
                        && score.eligibility == PlacementEligibility::Eligible
                        && projected
                            .iter()
                            .find(|node| node.session == score.session)
                            .is_some_and(|node| {
                                node.free_memory_bytes >= demand.memory_bytes
                                    && node.free_disk_bytes >= demand.disk_bytes
                                    && node.max_active_cells.saturating_sub(node.active_cells) >= 1
                                    && node.job_capacity.saturating_sub(node.running_jobs)
                                        >= demand.job_credits
                            })
                        && (urgent || score.score >= source_score.saturating_add(MIN_TRANSFER_GAIN))
                });
            let Some(destination) = destination else {
                continue;
            };
            let Some(receiver) = projected
                .iter_mut()
                .find(|node| node.session == destination.session)
            else {
                continue;
            };
            receiver.free_memory_bytes -= demand.memory_bytes;
            receiver.free_disk_bytes -= demand.disk_bytes;
            receiver.active_cells += 1;
            receiver.running_jobs += demand.job_credits;
            bytes = next_bytes;
            intents.push(CellTransferIntent {
                cell: demand.cell,
                source: demand.source,
                generation: demand.generation,
                destination: destination.session,
                observed_at_ms: source.observed_at_ms,
                memory_bytes: demand.memory_bytes,
                disk_bytes: demand.disk_bytes,
                job_credits: demand.job_credits,
            });
        }
        Ok(intents)
    }

    fn score(
        &self,
        cell: CellId,
        now_ms: i64,
        observation: PlacementObservation,
    ) -> PlacementScore {
        let eligibility = self.eligibility(now_ms, observation);
        if eligibility != PlacementEligibility::Eligible {
            return PlacementScore {
                node: observation.node,
                session: observation.session,
                score: 0,
                eligibility,
            };
        }
        let memory = ratio(
            observation.free_memory_bytes,
            observation.memory_capacity_bytes,
        );
        let disk = ratio(observation.free_disk_bytes, observation.disk_capacity_bytes);
        let cells = ratio(
            u64::from(
                observation
                    .max_active_cells
                    .saturating_sub(observation.active_cells),
            ),
            u64::from(observation.max_active_cells),
        );
        let jobs = ratio(
            u64::from(
                observation
                    .job_capacity
                    .saturating_sub(observation.running_jobs),
            ),
            u64::from(observation.job_capacity),
        );
        let backlog = u128::from(
            observation
                .publication_backlog
                .saturating_add(observation.hydration_backlog)
                .saturating_add(observation.primitive_backlog),
        );
        let locality = u128::from(observation.locality_bonus.min(100));
        let sticky = u128::from(observation.current_owner) * 50;
        let hash_bonus = placement_hash(cell, observation.node, observation.session) % 100;
        let score = (memory * 400
            + disk * 250
            + cells * 200
            + jobs * 100
            + locality * 10
            + sticky * SCORE_SCALE
            + hash_bonus)
            .saturating_sub(backlog.min(100) * SCORE_SCALE);
        PlacementScore {
            node: observation.node,
            session: observation.session,
            score,
            eligibility,
        }
    }

    fn eligibility(&self, now_ms: i64, observation: PlacementObservation) -> PlacementEligibility {
        if !observation.authenticated {
            return PlacementEligibility::Unauthenticated;
        }
        if observation.observed_at_ms < 0
            || now_ms < observation.observed_at_ms
            || now_ms.saturating_sub(observation.observed_at_ms) > self.max_observation_age_ms
        {
            return PlacementEligibility::Stale;
        }
        if observation.draining {
            return PlacementEligibility::Draining;
        }
        if observation.pressure >= PlacementPressure::Critical {
            return PlacementEligibility::CriticalPressure;
        }
        if observation.memory_capacity_bytes == 0 || observation.free_memory_bytes == 0 {
            return PlacementEligibility::NoMemoryHeadroom;
        }
        if observation.disk_capacity_bytes == 0 || observation.free_disk_bytes == 0 {
            return PlacementEligibility::NoDiskHeadroom;
        }
        if observation.max_active_cells == 0
            || observation.active_cells >= observation.max_active_cells
        {
            return PlacementEligibility::NoCellCapacity;
        }
        if observation.job_capacity == 0 || observation.running_jobs >= observation.job_capacity {
            return PlacementEligibility::NoJobCapacity;
        }
        PlacementEligibility::Eligible
    }
}

fn ratio(numerator: u64, denominator: u64) -> u128 {
    if denominator == 0 {
        0
    } else {
        u128::from(numerator.min(denominator)) * SCORE_SCALE / u128::from(denominator)
    }
}

fn placement_hash(cell: CellId, node: NodeId, session: SessionId) -> u128 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.cell.placement.v1\0");
    hasher.update(cell.as_bytes());
    hasher.update(node.as_bytes());
    hasher.update(session.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    u128::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(byte: u8) -> PlacementObservation {
        PlacementObservation {
            node: NodeId::from_bytes([byte; 16]),
            session: SessionId::from_bytes([byte + 1; 16]),
            observed_at_ms: 100,
            memory_capacity_bytes: 1_000,
            free_memory_bytes: 700,
            disk_capacity_bytes: 1_000,
            free_disk_bytes: 700,
            active_cells: 1,
            max_active_cells: 10,
            running_jobs: 1,
            job_capacity: 10,
            publication_backlog: 0,
            hydration_backlog: 0,
            primitive_backlog: 0,
            pressure: PlacementPressure::Normal,
            draining: false,
            authenticated: true,
            locality_bonus: 0,
            current_owner: false,
        }
    }

    #[test]
    fn ranking_is_permutation_invariant_and_total() {
        let planner = PlacementPlanner::default();
        let left = planner
            .rank(
                CellId::from_bytes([9; 32]),
                100,
                &[observation(1), observation(2)],
            )
            .unwrap();
        let right = planner
            .rank(
                CellId::from_bytes([9; 32]),
                100,
                &[observation(2), observation(1)],
            )
            .unwrap();
        assert_eq!(left, right);
        assert!(left[0].score > 0);
    }

    #[test]
    fn stale_and_full_nodes_are_not_candidates() {
        let planner = PlacementPlanner::default();
        let mut stale = observation(1);
        stale.observed_at_ms = -1;
        let mut full = observation(2);
        full.active_cells = full.max_active_cells;
        let scores = planner
            .rank(CellId::from_bytes([9; 32]), 100, &[stale, full])
            .unwrap();
        assert!(
            scores
                .iter()
                .all(|score| score.eligibility != PlacementEligibility::Eligible)
        );
        assert!(
            planner
                .choose(CellId::from_bytes([9; 32]), 100, &[stale, full])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn worsening_headroom_cannot_improve_score() {
        let planner = PlacementPlanner::default();
        let healthy = observation(1);
        let mut worse = healthy;
        worse.free_memory_bytes = 100;
        worse.free_disk_bytes = 100;
        let healthy_score = planner
            .choose(CellId::from_bytes([9; 32]), 100, &[healthy])
            .unwrap()
            .unwrap()
            .score;
        let worse_score = planner
            .choose(CellId::from_bytes([9; 32]), 100, &[worse])
            .unwrap()
            .unwrap()
            .score;
        assert!(worse_score < healthy_score);
    }

    #[test]
    fn measured_backlog_reduces_placement_score() {
        let planner = PlacementPlanner::default();
        let healthy = observation(1);
        let mut busy = healthy;
        busy.publication_backlog = 3;
        busy.hydration_backlog = 2;
        busy.primitive_backlog = 1;
        let healthy_score = planner
            .choose(CellId::from_bytes([9; 32]), 100, &[healthy])
            .unwrap()
            .unwrap()
            .score;
        let busy_score = planner
            .choose(CellId::from_bytes([9; 32]), 100, &[busy])
            .unwrap()
            .unwrap()
            .score;
        assert!(busy_score < healthy_score);
    }

    fn demand(cell: u8, source: SessionId) -> CellTransferDemand {
        CellTransferDemand {
            cell: CellId::from_bytes([cell; 32]),
            source,
            generation: 1,
            memory_bytes: 400,
            disk_bytes: 400,
            job_credits: 1,
            resident_since_ms: 0,
            last_moved_at_ms: None,
            stable_observations: 2,
            settled: true,
        }
    }

    #[test]
    fn transfer_projection_prevents_receiver_overcommit() {
        let planner = PlacementPlanner::default();
        let mut donor = observation(1);
        donor.draining = true;
        let receiver = observation(2);
        let first = demand(1, donor.session);
        let second = demand(2, donor.session);
        let left = planner
            .plan_transfers(100, &[donor, receiver], &[first, second])
            .unwrap();
        let right = planner
            .plan_transfers(100, &[receiver, donor], &[second, first])
            .unwrap();
        assert_eq!(left, right);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].cell, first.cell);
    }

    #[test]
    fn transfer_requires_settlement_residence_and_fresh_destination() {
        let planner = PlacementPlanner::default();
        let donor = observation(1);
        let mut receiver = observation(2);
        receiver.free_memory_bytes = 1_000;
        receiver.free_disk_bytes = 1_000;
        let mut candidate = demand(1, donor.session);
        assert!(
            planner
                .plan_transfers(100, &[donor, receiver], &[candidate])
                .unwrap()
                .is_empty()
        );
        candidate.settled = false;
        assert!(
            planner
                .plan_transfers(100_000, &[donor, receiver], &[candidate])
                .unwrap()
                .is_empty()
        );
        candidate.settled = true;
        receiver.observed_at_ms = 100_000;
        assert_eq!(
            planner
                .plan_transfers(100_000, &[donor, receiver], &[candidate])
                .unwrap()
                .len(),
            0
        );
        let mut donor = donor;
        donor.draining = true;
        receiver.observed_at_ms = 100;
        assert!(
            planner
                .plan_transfers(100_000, &[donor, receiver], &[candidate])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn duplicate_nodes_and_cells_fail_closed() {
        let planner = PlacementPlanner::default();
        let node = observation(1);
        let candidate = demand(1, node.session);
        assert!(
            planner
                .plan_transfers(100, &[node, node], &[candidate])
                .is_err()
        );
        assert!(
            planner
                .plan_transfers(100, &[node], &[candidate, candidate])
                .is_err()
        );
    }

    #[test]
    fn normal_transfer_requires_stable_gain_and_cooldown() {
        let planner = PlacementPlanner::default();
        let mut donor = observation(1);
        donor.observed_at_ms = 100_000;
        donor.free_memory_bytes = 100;
        donor.free_disk_bytes = 100;
        let mut receiver = observation(2);
        receiver.observed_at_ms = 100_000;
        receiver.free_memory_bytes = 1_000;
        receiver.free_disk_bytes = 1_000;
        let mut candidate = demand(1, donor.session);
        candidate.memory_bytes = 100;
        candidate.disk_bytes = 100;
        assert_eq!(
            planner
                .plan_transfers(100_000, &[donor, receiver], &[candidate])
                .unwrap()
                .len(),
            1
        );
        candidate.last_moved_at_ms = Some(99_999);
        assert!(
            planner
                .plan_transfers(100_000, &[donor, receiver], &[candidate])
                .unwrap()
                .is_empty()
        );
        candidate.last_moved_at_ms = None;
        candidate.stable_observations = 1;
        assert!(
            planner
                .plan_transfers(100_000, &[donor, receiver], &[candidate])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn transfer_budget_counts_projected_restore_bytes() {
        let planner = PlacementPlanner::default();
        let mut donor = observation(1);
        donor.draining = true;
        let mut receiver = observation(2);
        receiver.disk_capacity_bytes = 12 * 1024 * 1024 * 1024;
        receiver.free_disk_bytes = receiver.disk_capacity_bytes;
        let mut first = demand(1, donor.session);
        first.disk_bytes = 5 * 1024 * 1024 * 1024;
        let mut second = demand(2, donor.session);
        second.disk_bytes = first.disk_bytes;
        assert_eq!(
            planner
                .plan_transfers(100, &[donor, receiver], &[first, second])
                .unwrap()
                .len(),
            1
        );
    }
}
