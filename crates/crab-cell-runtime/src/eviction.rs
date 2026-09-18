use crate::{CellId, ResourceCost};

/// Lifecycle class used when choosing a local Cell to evict.
///
/// The selector only accepts cells that are already idle or quiescing. The
/// actor remains responsible for driving the selected cell through the
/// durability and close protocol before releasing its reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EvictionState {
    Idle,
    Quiescing,
}

/// Immutable actor observation consumed by the deterministic victim selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EvictionObservation {
    pub(crate) cell: CellId,
    pub(crate) state: EvictionState,
    pub(crate) last_used_ms: i64,
    pub(crate) cost: ResourceCost,
    pub(crate) busy: bool,
    pub(crate) retained_obligation: bool,
    pub(crate) migrating: bool,
    pub(crate) backup_pinned: bool,
    pub(crate) leased_work: bool,
    pub(crate) primitive_obligation: bool,
    pub(crate) accounting_known: bool,
}

impl EvictionObservation {
    fn eligible(self) -> bool {
        self.accounting_known
            && !self.busy
            && !self.retained_obligation
            && !self.migrating
            && !self.backup_pinned
            && !self.leased_work
            && !self.primitive_obligation
            && self.last_used_ms >= 0
    }
}

/// Selects at most `limit` safe victims with a total, permutation-invariant order.
pub(crate) fn select_victims(observations: &[EvictionObservation], limit: usize) -> Vec<CellId> {
    let mut candidates = observations
        .iter()
        .copied()
        .filter(|observation| observation.eligible())
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.last_used_ms
            .cmp(&right.last_used_ms)
            .then_with(|| right.state.cmp(&left.state))
            .then_with(|| right.cost.active_cells().cmp(&left.cost.active_cells()))
            .then_with(|| right.cost.retained_bytes().cmp(&left.cost.retained_bytes()))
            .then_with(|| right.cost.disk_bytes().cmp(&left.cost.disk_bytes()))
            .then_with(|| left.cell.as_bytes().cmp(right.cell.as_bytes()))
    });
    candidates
        .into_iter()
        .take(limit)
        .map(|candidate| candidate.cell)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(cell: u8, last_used_ms: i64) -> EvictionObservation {
        EvictionObservation {
            cell: CellId::from_bytes([cell; 32]),
            state: EvictionState::Idle,
            last_used_ms,
            cost: ResourceCost::active_cell()
                .with_retained_bytes(64)
                .with_disk_bytes(128),
            busy: false,
            retained_obligation: false,
            migrating: false,
            backup_pinned: false,
            leased_work: false,
            primitive_obligation: false,
            accounting_known: true,
        }
    }

    #[test]
    fn unsafe_cells_are_never_selected() {
        let mut busy = observation(1, 1);
        busy.busy = true;
        let mut retained = observation(2, 2);
        retained.retained_obligation = true;
        let mut migrating = observation(3, 3);
        migrating.migrating = true;
        let mut pinned = observation(4, 4);
        pinned.backup_pinned = true;
        let mut leased = observation(5, 5);
        leased.leased_work = true;
        let mut unknown = observation(6, 6);
        unknown.accounting_known = false;
        let mut primitive = observation(7, 7);
        primitive.primitive_obligation = true;
        assert!(
            select_victims(
                &[
                    busy, retained, migrating, pinned, leased, unknown, primitive
                ],
                10
            )
            .is_empty()
        );
    }

    #[test]
    fn selection_is_oldest_first_and_permutation_invariant() {
        let first = observation(1, 20);
        let second = observation(2, 10);
        let third = observation(3, 30);
        let left = select_victims(&[first, second, third], 2);
        let right = select_victims(&[third, first, second], 2);
        assert_eq!(left, right);
        assert_eq!(left, vec![second.cell, first.cell]);
    }

    #[test]
    fn tie_break_prefers_more_reclaimable_cost_then_cell_id() {
        let mut smaller = observation(1, 10);
        let mut larger = observation(2, 10);
        larger.cost = larger.cost.with_disk_bytes(512);
        assert_eq!(select_victims(&[smaller, larger], 1), vec![larger.cell]);
        smaller.cost = larger.cost;
        assert_eq!(select_victims(&[smaller, larger], 1), vec![smaller.cell]);
    }
}
