//! Idle eviction, drain, and transfer observations for one Cell actor.

use super::*;

pub(in crate::cell::actor) fn start_bounded_evictions(
    limit: usize,
    now_ms: i64,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
) -> usize {
    let mut started = 0;
    for _ in 0..limit {
        let Ok(permit) = movement.try_start(now_ms) else {
            break;
        };
        let mut selected = begin_idle_evictions(1, pool, cells, transitioning, tasks);
        let Some(cell) = selected.pop() else {
            let mut permit = permit;
            movement.complete(&mut permit);
            break;
        };
        movement_permits.insert(cell, permit);
        started += 1;
    }
    started
}

pub(in crate::cell::actor) fn begin_idle_evictions(
    limit: usize,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
) -> Vec<CellId> {
    let observations = cells
        .iter()
        .map(|(cell, active)| eviction_observation(*cell, active))
        .collect::<Vec<_>>();
    let victims = select_victims(&observations, limit);
    let mut started = Vec::new();
    for cell in victims {
        if begin_idle_cell_eviction(cell, pool, cells, transitioning, tasks, None) {
            started.push(cell);
        }
    }
    started
}

pub(in crate::cell::actor) fn eviction_observation(
    cell: CellId,
    active: &ActiveCell,
) -> EvictionObservation {
    EvictionObservation {
        cell,
        state: if active.draining() {
            EvictionState::Quiescing
        } else {
            EvictionState::Idle
        },
        last_used_ms: active.last_used_ms,
        cost: ResourceCost::active_cell()
            .with_retained_bytes(usize::try_from(active.publication_bytes).unwrap_or(usize::MAX)),
        busy: active.busy() || active.renewing() || active.transfer.is_some(),
        retained_obligation: active.coordination.publication_count() != 0,
        migrating: active.publisher.is_none(),
        backup_pinned: active.unpublished_node_logs != 0,
        leased_work: !active.queue.is_empty(),
        primitive_obligation: !active.persisted_work.is_transfer_settled(),
        accounting_known: !active.persisted_work.is_unknown(),
    }
}

pub(in crate::cell::actor) fn transfer_candidate_observation(
    cell: CellId,
    active: &ActiveCell,
) -> EvictionObservation {
    let mut observation = eviction_observation(cell, active);
    if !active.persisted_work.is_unknown() {
        observation.primitive_obligation = false;
    }
    observation
}

pub(in crate::cell::actor) fn transfer_observation(
    cell: CellId,
    active: &ActiveCell,
    inventory: crate::primitives::maintenance::TransferWorkInventory,
) -> EvictionObservation {
    let mut observation = eviction_observation(cell, active);
    observation.primitive_obligation = !inventory.is_settled();
    observation.accounting_known = true;
    observation
}

pub(in crate::cell::actor) fn begin_idle_cell_eviction(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    reply: Option<oneshot::Sender<crate::Result<()>>>,
) -> bool {
    let Some(active) = cells.get_mut(&cell) else {
        if let Some(reply) = reply {
            let _ = reply.send(Err(Error::CellNotActive));
        }
        return false;
    };
    let decision = active.coordination.step(CoordinationInput::BeginDrain);
    if matches!(decision, CoordinationDecision::Reject(_)) {
        if let Some(reply) = reply {
            let _ = reply.send(Err(Error::CellDraining));
        }
        return false;
    }
    active.admission.draining.store(true, Ordering::Release);
    active.admission.requests.close();
    active.admission.bytes.close();
    active.drain = reply;
    if matches!(
        schedule(active, true),
        CoordinationDecision::ReadyToDeactivate
    ) {
        start_deactivate(cell, pool, cells, transitioning, tasks);
    }
    true
}
