//! Properties the weighted placement planner promises for one fleet snapshot.
//!
//! The planner is pure and deterministic, so its contracts are properties of a
//! generated snapshot: the same view must elect the same donor whatever order
//! the observations arrive in, an ineligible member never receives, a
//! pre-batch view moves nothing, and one snapshot never plans more than the
//! tick cap.

use crab_cell_runtime::fleet::placement::{
    CellTransferDemand, FleetBalance, PlacementEligibility, PlacementObservation, PlacementPlanner,
    PlacementPressure,
};
use crab_cell_runtime::identity::{CellId, NodeId, SessionId};
use proptest::prelude::*;

const NOW_MS: i64 = 1_000_000;

fn node(byte: u8) -> NodeId {
    NodeId::from_bytes([byte; 16])
}

fn session(byte: u8) -> SessionId {
    SessionId::from_bytes([byte; 16])
}

fn observation(
    index: u8,
    active_cells: u32,
    max_active_cells: u32,
    pressure: PlacementPressure,
    draining: bool,
) -> PlacementObservation {
    PlacementObservation {
        node: node(index),
        session: session(index),
        observed_at_ms: NOW_MS,
        memory_capacity_bytes: 1 << 30,
        free_memory_bytes: 1 << 29,
        disk_capacity_bytes: 1 << 30,
        free_disk_bytes: 1 << 29,
        active_cells,
        max_active_cells,
        running_jobs: 0,
        job_capacity: 8,
        publication_backlog: 0,
        hydration_backlog: 0,
        primitive_backlog: 0,
        pressure,
        draining,
        authenticated: true,
        current_owner: false,
    }
}

fn pressure() -> impl Strategy<Value = PlacementPressure> {
    prop_oneof![
        Just(PlacementPressure::Normal),
        Just(PlacementPressure::Constrained),
        Just(PlacementPressure::Shedding),
        Just(PlacementPressure::Critical),
    ]
}

/// Generates a small fleet whose capacities and counts collide often enough for
/// ties to appear.
fn fleet() -> impl Strategy<Value = Vec<PlacementObservation>> {
    prop::collection::vec((0u32..4, 1u32..4, pressure(), any::<bool>()), 1..5).prop_map(|entries| {
        entries
            .into_iter()
            .enumerate()
            .map(|(index, (active, max_active, pressure, draining))| {
                let active = active.min(max_active);
                observation(index as u8, active, max_active, pressure, draining)
            })
            .collect()
    })
}

fn balance(observations: &[PlacementObservation], since_ms: i64) -> Option<FleetBalance> {
    PlacementPlanner::default()
        .fleet_balance(NOW_MS, observations, since_ms)
        .expect("a generated snapshot is a valid view")
}

/// A dense member hands over to an empty one, which keeps the generated
/// properties above from passing vacuously: the planner does return a plan for
/// a skewed snapshot, and the plan is bounded by the tick cap.
#[test]
fn a_dense_member_hands_over_to_an_empty_one() {
    let planner = PlacementPlanner::default();
    let dense = observation(1, 3, 3, PlacementPressure::Normal, false);
    let empty = observation(2, 0, 3, PlacementPressure::Normal, false);
    let observations = [dense, empty];
    let balance = planner
        .fleet_balance(NOW_MS, &observations, NOW_MS - 1)
        .expect("a compact snapshot is a valid view")
        .expect("a skewed snapshot elects one donor");
    assert_eq!(balance.donor, dense.session);
    assert_eq!(balance.receivers, vec![empty.session]);
    assert!(balance.surplus > 0 && balance.surplus <= 2);

    let demand = CellTransferDemand {
        cell: CellId::from_bytes([9; 32]),
        source: dense.session,
        generation: 1,
        memory_bytes: 1 << 20,
        disk_bytes: 1 << 20,
        job_credits: 1,
        resident_since_ms: NOW_MS - 120_000,
        last_moved_at_ms: None,
        stable_observations: 3,
        settled: true,
    };
    let intents = planner
        .plan_transfers(NOW_MS, &observations, &[demand], Some(&balance))
        .expect("a compact snapshot is a valid view");
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].source, dense.session);
    assert_eq!(intents[0].destination, empty.session);
    assert_eq!(intents[0].cell, demand.cell);
}

#[test]
fn small_fleets_converge_after_owner_loss_with_fresh_settled_views() {
    let planner = PlacementPlanner::default();
    let mut unconverged = Vec::new();
    for nodes in [3u8, 5, 10, 20] {
        let mut observations = (0..nodes)
            .map(|index| {
                observation(
                    index,
                    if index == 0 { 20 } else { 0 },
                    64,
                    PlacementPressure::Normal,
                    false,
                )
            })
            .collect::<Vec<_>>();
        let mut owners = vec![session(0); 20];
        for tick in 0..40 {
            let now = NOW_MS + tick * 120_000;
            for value in &mut observations {
                value.observed_at_ms = now;
            }
            let Some(balance) = planner.fleet_balance(now, &observations, now - 1).unwrap() else {
                break;
            };
            let demands = owners
                .iter()
                .enumerate()
                .filter(|(_, owner)| **owner == balance.donor)
                .map(|(cell, owner)| CellTransferDemand {
                    cell: CellId::from_bytes([cell as u8; 32]),
                    source: *owner,
                    generation: 1,
                    memory_bytes: 1 << 20,
                    disk_bytes: 1 << 20,
                    job_credits: 1,
                    resident_since_ms: now - 120_000,
                    last_moved_at_ms: None,
                    stable_observations: 3,
                    settled: true,
                })
                .collect::<Vec<_>>();
            let intents = planner
                .plan_transfers(now, &observations, &demands, Some(&balance))
                .unwrap();
            for intent in intents {
                owners[intent.cell.as_bytes()[0] as usize] = intent.destination;
                observations
                    .iter_mut()
                    .find(|node| node.session == intent.source)
                    .unwrap()
                    .active_cells -= 1;
                observations
                    .iter_mut()
                    .find(|node| node.session == intent.destination)
                    .unwrap()
                    .active_cells += 1;
            }
        }
        let counts = observations
            .iter()
            .map(|node| node.active_cells)
            .collect::<Vec<_>>();
        if counts.iter().any(|count| {
            *count < 20 / u32::from(nodes) || *count > 20u32.div_ceil(u32::from(nodes))
        }) {
            unconverged.push((nodes, counts));
        }
    }
    assert!(
        unconverged.is_empty(),
        "ownership did not converge: {unconverged:?}"
    );
}

#[test]
fn donations_do_not_overfill_a_preferred_receivers_weighted_share() {
    let planner = PlacementPlanner::default();
    let mut observations = [5, 1, 0]
        .into_iter()
        .enumerate()
        .map(|(index, count)| observation(index as u8, count, 64, PlacementPressure::Normal, false))
        .collect::<Vec<_>>();
    // The first receiver has enough headroom advantage to rank first for both
    // Cells, but neither receiver exceeds the source's sticky score.
    observations[0].free_memory_bytes = 1 << 30;
    observations[1].free_memory_bytes = 3 << 28;
    let demands = (0..4)
        .map(|index| CellTransferDemand {
            cell: CellId::from_bytes([index; 32]),
            source: session(0),
            generation: 1,
            memory_bytes: 1,
            disk_bytes: 1,
            job_credits: 1,
            resident_since_ms: NOW_MS - 120_000,
            last_moved_at_ms: None,
            stable_observations: 3,
            settled: true,
        })
        .collect::<Vec<_>>();
    // Its whole-Cell margin is zero at target two; the existing Cell leaves
    // room for only one more donation in this snapshot.
    let balance = planner
        .fleet_balance(NOW_MS, &observations, NOW_MS - 1)
        .unwrap()
        .unwrap();
    let intents = planner
        .plan_transfers(NOW_MS, &observations, &demands, Some(&balance))
        .unwrap();
    let destinations = intents
        .iter()
        .map(|intent| intent.destination)
        .collect::<Vec<_>>();
    assert_eq!(destinations, vec![session(1), session(2)]);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// One snapshot elects one donor and a bounded batch, whatever order the
    /// observations arrive in: two hosts scanning the same directory must not
    /// pick different donors for the same generation.
    #[test]
    fn balance_is_permutation_invariant(observations in fleet()) {
        let mut reversed = observations.clone();
        reversed.reverse();
        let mut rotated = observations.clone();
        rotated.rotate_left(1);

        let expected = balance(&observations, NOW_MS - 1);
        prop_assert_eq!(balance(&reversed, NOW_MS - 1), expected.clone());
        prop_assert_eq!(balance(&rotated, NOW_MS - 1), expected);
    }

    /// A receiver is eligible, below the shedding tier, and never the donor.
    #[test]
    fn balance_never_names_an_ineligible_receiver(observations in fleet()) {
        let planner = PlacementPlanner::default();
        let Some(balance) = balance(&observations, NOW_MS - 1) else {
            return Ok(());
        };
        let scores = planner
            .rank(CellId::from_bytes([7; 32]), NOW_MS, &observations)
            .expect("a generated snapshot is a valid view");
        prop_assert!(balance.surplus > 0);
        for receiver in &balance.receivers {
            let observation = observations
                .iter()
                .find(|observation| observation.session == *receiver)
                .expect("a receiver came from the snapshot");
            prop_assert_ne!(*receiver, balance.donor);
            prop_assert!(observation.pressure < PlacementPressure::Shedding);
            prop_assert_eq!(
                scores
                    .iter()
                    .find(|score| score.session == *receiver)
                    .expect("every observation is ranked")
                    .eligibility,
                PlacementEligibility::Eligible
            );
        }
    }

    /// A sample from the previous movement batch, or one outside the freshness
    /// window, moves nothing on the count rule.
    #[test]
    fn balance_fails_closed_on_a_pre_batch_or_stale_view(
        observations in fleet(),
        age_ms in 30_001i64..600_000,
    ) {
        let mut stale = observations.clone();
        for observation in &mut stale {
            observation.observed_at_ms = NOW_MS - age_ms;
        }
        prop_assert_eq!(balance(&stale, NOW_MS - age_ms - 1), None);
        // The same view at the sample instant, before any batch: nothing moves.
        prop_assert_eq!(balance(&observations, NOW_MS), None);
    }

    /// One snapshot never plans more transfers than the tick cap, and no Cell is
    /// planned twice or moved onto its own node.
    #[test]
    fn transfer_plan_stays_within_the_tick_cap(
        observations in fleet(),
        demand_bytes in prop::collection::vec(1u64 << 20..6u64 << 30, 1..5),
    ) {
        let planner = PlacementPlanner::default();
        let balance = balance(&observations, NOW_MS - 1);
        let demands = observations
            .iter()
            .enumerate()
            .map(|(index, observation)| {
                let bytes = demand_bytes[index % demand_bytes.len()];
                CellTransferDemand {
                    cell: CellId::from_bytes([index as u8 + 1; 32]),
                    source: observation.session,
                    generation: 1,
                    memory_bytes: bytes,
                    disk_bytes: bytes,
                    job_credits: 1,
                    resident_since_ms: NOW_MS - 1_000,
                    last_moved_at_ms: None,
                    stable_observations: 2,
                    settled: true,
                }
            })
            .collect::<Vec<_>>();
        let intents = planner
            .plan_transfers(NOW_MS, &observations, &demands, balance.as_ref())
            .expect("a generated snapshot is a valid view");

        prop_assert!(intents.len() <= 2);
        let projected = intents
            .iter()
            .map(|intent| intent.disk_bytes)
            .sum::<u64>();
        prop_assert!(projected <= 8 * 1024 * 1024 * 1024);
        let mut cells = intents
            .iter()
            .map(|intent| *intent.cell.as_bytes())
            .collect::<Vec<_>>();
        cells.sort();
        let before = cells.len();
        cells.dedup();
        prop_assert_eq!(cells.len(), before);
        for intent in &intents {
            prop_assert_ne!(intent.destination, intent.source);
        }
    }
}
