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
        .plan_transfers(100, &[donor, receiver], &[first, second], None)
        .unwrap();
    let right = planner
        .plan_transfers(100, &[receiver, donor], &[second, first], None)
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
            .plan_transfers(100, &[donor, receiver], &[candidate], None)
            .unwrap()
            .is_empty()
    );
    candidate.settled = false;
    assert!(
        planner
            .plan_transfers(100_000, &[donor, receiver], &[candidate], None)
            .unwrap()
            .is_empty()
    );
    candidate.settled = true;
    receiver.observed_at_ms = 100_000;
    assert_eq!(
        planner
            .plan_transfers(100_000, &[donor, receiver], &[candidate], None)
            .unwrap()
            .len(),
        0
    );
    let mut donor = donor;
    donor.draining = true;
    receiver.observed_at_ms = 100;
    assert!(
        planner
            .plan_transfers(100_000, &[donor, receiver], &[candidate], None)
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
            .plan_transfers(100, &[node, node], &[candidate], None)
            .is_err()
    );
    assert!(
        planner
            .plan_transfers(100, &[node], &[candidate, candidate], None)
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
            .plan_transfers(100_000, &[donor, receiver], &[candidate], None)
            .unwrap()
            .len(),
        1
    );
    candidate.last_moved_at_ms = Some(99_999);
    assert!(
        planner
            .plan_transfers(100_000, &[donor, receiver], &[candidate], None)
            .unwrap()
            .is_empty()
    );
    candidate.last_moved_at_ms = None;
    candidate.stable_observations = 1;
    assert!(
        planner
            .plan_transfers(100_000, &[donor, receiver], &[candidate], None)
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
            .plan_transfers(100, &[donor, receiver], &[first, second], None)
            .unwrap()
            .len(),
        1
    );
}

fn owned(byte: u8, cells: u32, slots: u32) -> PlacementObservation {
    let mut observation = observation(byte);
    observation.observed_at_ms = 100_000;
    observation.active_cells = cells;
    observation.max_active_cells = slots;
    observation
}

#[test]
fn balance_elects_one_weighted_donor_and_orders_receivers() {
    let planner = PlacementPlanner::default();
    let dense = owned(1, 20, 10);
    let wide = owned(2, 5, 20);
    let small = owned(3, 5, 10);
    let left = planner
        .fleet_balance(100_000, &[dense, wide, small], 0)
        .unwrap()
        .unwrap();
    let right = planner
        .fleet_balance(100_000, &[small, dense, wide], 0)
        .unwrap()
        .unwrap();
    assert_eq!(left, right);
    assert_eq!(left.donor, dense.session);
    // Twelve Cells over target, thirteen Cells of receiver room, and a
    // two-Cell batch: the batch is the binding limit.
    assert_eq!(left.surplus, 2);
    assert_eq!(left.receivers, vec![wide.session, small.session]);
    assert!(left.receivers.contains(&wide.session));
    assert!(!left.receivers.contains(&dense.session));
}

#[test]
fn balance_breaks_a_density_tie_by_node_identity() {
    let planner = PlacementPlanner::default();
    let left = owned(1, 10, 10);
    let right = owned(2, 10, 10);
    let idle = owned(3, 0, 10);
    let balance = planner
        .fleet_balance(100_000, &[left, right, idle], 0)
        .unwrap()
        .unwrap();
    assert_eq!(balance.donor, left.session);
    assert_eq!(balance.receivers, vec![idle.session]);
    let balance = planner
        .fleet_balance(100_000, &[right, left, idle], 0)
        .unwrap()
        .unwrap();
    assert_eq!(balance.donor, left.session);
}

#[test]
fn balance_deadband_bounds_the_donation() {
    let planner = PlacementPlanner::default();
    let donor = owned(1, 103, 200);
    let roomy = owned(2, 97, 200);
    // Each member targets 100 Cells. The receiver's two-Cell margin leaves
    // one donation slot, even though the donor could fill the two-Cell batch.
    let balance = planner
        .fleet_balance(100_000, &[donor, roomy], 0)
        .unwrap()
        .unwrap();
    assert_eq!(balance.surplus, 1);
    assert_eq!(balance.receivers, vec![roomy.session]);
}

#[test]
fn balance_view_fails_closed_on_mixed_or_unusable_samples() {
    let planner = PlacementPlanner::default();
    let donor = owned(1, 4, 10);
    let receiver = owned(2, 0, 10);
    assert!(planner.fleet_balance(100_000, &[], 0).unwrap().is_none());
    assert!(
        planner
            .fleet_balance(100_000, &[donor, receiver], 100_000)
            .unwrap()
            .is_none()
    );
    let mut stale = receiver;
    stale.observed_at_ms = 100_000 - MAX_OBSERVATION_AGE_MS - 1;
    assert!(
        planner
            .fleet_balance(100_000, &[donor, stale], 0)
            .unwrap()
            .is_none()
    );
    let mut forged = receiver;
    forged.authenticated = false;
    assert!(
        planner
            .fleet_balance(100_000, &[donor, forged], 0)
            .unwrap()
            .is_none()
    );
    assert!(planner.fleet_balance(100_000, &[donor, donor], 0).is_err());
    let mut draining = receiver;
    draining.draining = true;
    assert!(
        planner
            .fleet_balance(100_000, &[donor, draining], 0)
            .unwrap()
            .is_none()
    );
    let mut shedding = receiver;
    shedding.pressure = PlacementPressure::Shedding;
    assert!(
        planner
            .fleet_balance(100_000, &[donor, shedding], 0)
            .unwrap()
            .is_none()
    );
    let balanced = owned(3, 2, 10);
    let equal = owned(4, 2, 10);
    assert!(
        planner
            .fleet_balance(100_000, &[balanced, equal], 0)
            .unwrap()
            .is_none()
    );
}

#[test]
fn balance_moves_an_idle_cell_without_headroom_gain() {
    let planner = PlacementPlanner::default();
    let donor = owned(1, 3, 10);
    let receiver = owned(2, 0, 10);
    let candidate = demand(1, donor.session);
    // Equal headroom ratios leave no material gain, so the transfer score
    // gate refuses the move on its own.
    assert!(
        planner
            .plan_transfers(100_000, &[donor, receiver], &[candidate], None)
            .unwrap()
            .is_empty()
    );
    let balance = planner
        .fleet_balance(100_000, &[donor, receiver], 0)
        .unwrap()
        .unwrap();
    let intents = planner
        .plan_transfers(100_000, &[donor, receiver], &[candidate], Some(&balance))
        .unwrap();
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].destination, receiver.session);
    assert_eq!(intents[0].cell, candidate.cell);
}

#[test]
fn balance_donation_requires_the_elected_donor() {
    let planner = PlacementPlanner::default();
    let dense = owned(1, 4, 10);
    let first = owned(2, 0, 10);
    let second = owned(3, 0, 10);
    let balance = planner
        .fleet_balance(100_000, &[dense, first, second], 0)
        .unwrap()
        .unwrap();
    assert_eq!(balance.donor, dense.session);
    let candidate = demand(1, first.session);
    assert!(
        planner
            .plan_transfers(
                100_000,
                &[dense, first, second],
                &[candidate],
                Some(&balance),
            )
            .unwrap()
            .is_empty()
    );
}
