//! Inventory refresh, lease renewal, and deactivation completions.

use super::*;

/// Applies an inventory refresh result to the Cell's persisted work.
pub(super) fn handle_inventory_refreshed(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    result: crate::Result<crate::primitives::maintenance::PersistedWorkInventory>,
) {
    let TaskContext {
        pool,
        cells,
        transitioning,
        tasks,
        node_lease,
        ..
    } = context;
    let Some(active) = cells.get_mut(&cell) else {
        return;
    };
    if active.generation != generation
        || !active
            .coordination
            .effect_matches(effect_id, CoordinationEffect::Inventory)
    {
        return;
    }
    active.finish_task(effect_id, CoordinationEffect::Inventory);
    active.inventory_refreshing = false;
    if let Ok(inventory) = result {
        active.persisted_work = inventory;
    }
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}

/// Applies a lease renewal result and continues the Cell's schedule.
pub(super) fn handle_renewed(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    publisher: Box<CellPublisher>,
    mut result: crate::Result<()>,
) {
    let TaskContext {
        pool,
        cells,
        transitioning,
        tasks,
        node_lease,
        ..
    } = context;
    let Some(active) = cells.get_mut(&cell) else {
        return;
    };
    if active.generation != generation
        || !active
            .coordination
            .effect_matches(effect_id, CoordinationEffect::Renewal)
    {
        return;
    }
    active.finish_task(effect_id, CoordinationEffect::Renewal);
    if node_lease.check().is_err() {
        result = Err(Error::Fenced);
    }
    active.publisher = Some(*publisher);
    let decision = active.coordination.step(CoordinationInput::FinishRenewal {
        fenced: result.is_err(),
    });
    if matches!(decision, CoordinationDecision::Fence) {
        fence_active(active);
    }
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}

/// Applies a deactivation result, releasing movement permits and shutdown waiters.
pub(super) fn handle_deactivated(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    reply: Option<oneshot::Sender<crate::Result<()>>>,
    shutdown_drain: bool,
    result: crate::Result<()>,
) {
    let TaskContext {
        cells,
        transitioning,
        shutdown,
        movement,
        movement_permits,
        ..
    } = context;
    if cells
        .get(&cell)
        .is_some_and(|active| active.generation != generation)
    {
        return;
    }
    if let Some(mut permit) = movement_permits.remove(&cell) {
        movement.complete(&mut permit);
    }
    transitioning.remove(&cell);
    let runtime_waiting = shutdown_drain || shutdown.as_ref().is_some_and(|state| state.draining);
    match (reply, runtime_waiting) {
        (Some(reply), true) => {
            if result.is_err() {
                fail_shutdown(
                    shutdown,
                    Error::Control("one or more Cells failed to drain"),
                );
            }
            let _ = reply.send(result);
        }
        (Some(reply), false) => {
            let _ = reply.send(result);
        }
        (None, true) => {
            if let Err(error) = result {
                fail_shutdown(shutdown, error);
            }
        }
        (None, false) => {}
    }
}
