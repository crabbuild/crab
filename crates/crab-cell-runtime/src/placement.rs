use crate::{CellId, Error, NodeAdvertisement, NodeId, NodePlacementCapacity, Result, SessionId};

const MAX_OBSERVATION_AGE_MS: i64 = 30_000;
const SCORE_SCALE: u128 = 1_000;

/// Pressure class supplied by the signed node observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlacementPressure {
    Normal,
    Constrained,
    Shedding,
    Critical,
}

/// Runtime-measured values joined to one signed node advertisement before
/// planning. Missing snapshots are not converted to optimistic zero load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacementRuntimeSnapshot {
    pub node: NodeId,
    pub memory_capacity_bytes: u64,
    pub disk_capacity_bytes: u64,
    pub active_cells: u32,
    pub max_active_cells: u32,
    pub running_jobs: u32,
    pub pressure: PlacementPressure,
    pub draining: bool,
    pub locality_bonus: u16,
    pub current_owner: bool,
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
    /// Converts an already signature-verified live advertisement into planner
    /// input. Runtime ledger values remain explicit so stale startup hints are
    /// never guessed from the wire capacity alone.
    #[expect(
        clippy::too_many_arguments,
        reason = "the conversion binds signed identity to measured runtime values"
    )]
    pub fn from_advertisement(
        advertisement: &NodeAdvertisement,
        now_ms: i64,
        memory_capacity_bytes: u64,
        disk_capacity_bytes: u64,
        active_cells: u32,
        max_active_cells: u32,
        running_jobs: u32,
        pressure: PlacementPressure,
        draining: bool,
        locality_bonus: u16,
        current_owner: bool,
    ) -> Result<Self> {
        if now_ms < 0
            || advertisement.expires_at_ms() <= now_ms
            || !advertisement.has_signed_placement()
        {
            return Err(Error::Node("placement advertisement is stale"));
        }
        let capacity = advertisement.capacity();
        Ok(Self {
            node: advertisement.node(),
            session: advertisement.session(),
            observed_at_ms: advertisement.issued_at_ms(),
            memory_capacity_bytes,
            free_memory_bytes: capacity.free_memory_bytes,
            disk_capacity_bytes,
            free_disk_bytes: capacity.free_disk_bytes,
            active_cells,
            max_active_cells,
            running_jobs,
            job_capacity: capacity.job_credits,
            publication_backlog: 0,
            hydration_backlog: 0,
            primitive_backlog: 0,
            pressure,
            draining,
            authenticated: true,
            locality_bonus,
            current_owner,
        })
    }

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
            publication_backlog: 0,
            hydration_backlog: 0,
            primitive_backlog: 0,
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
            .saturating_sub(backlog.min(100) * 5);
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
}
