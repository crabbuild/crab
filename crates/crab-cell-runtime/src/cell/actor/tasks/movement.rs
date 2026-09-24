//! Transfer preflight and migration completions.

use super::*;

/// Applies a transfer preflight result: deactivate, continue, or abort.
pub(super) fn handle_transfer_preflight(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    result: crate::Result<crate::primitives::maintenance::TransferWorkInventory>,
) {
    let TaskContext {
        pool,
        cells,
        transitioning,
        tasks,
        node_lease,
        movement,
        movement_permits,
        ..
    } = context;
    let preflight = {
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
        match active.transfer.take() {
            Some(transfer) => {
                let mut ready_to_deactivate = false;
                let mut fenced = false;
                let transfer_result = if node_lease.check().is_err() {
                    fenced = true;
                    active.coordination.step(CoordinationInput::Fence);
                    fence_active(active);
                    Err(Error::Fenced)
                } else {
                    match result {
                        Ok(inventory) => {
                            if inventory.is_settled()
                                && active.queue.is_empty()
                                && active.coordination.can_deactivate()
                                && transfer_observation(cell, active, inventory).eligible()
                            {
                                match active.coordination.step(CoordinationInput::ConfirmTransfer) {
                                    CoordinationDecision::ReadyToDeactivate => {
                                        ready_to_deactivate = true;
                                        Ok(())
                                    }
                                    CoordinationDecision::Started => Ok(()),
                                    CoordinationDecision::Reject(RejectReason::Fenced) => {
                                        fenced = true;
                                        fence_active(active);
                                        Err(Error::Fenced)
                                    }
                                    CoordinationDecision::Reject(reason) => {
                                        Err(rejection_error(reason))
                                    }
                                    _ => Err(Error::CellDraining),
                                }
                            } else {
                                Err(Error::CellDraining)
                            }
                        }
                        Err(error) => Err(error),
                    }
                };
                if transfer_result.is_err() && !fenced {
                    active.coordination.step(CoordinationInput::AbortTransfer);
                }
                Some((transfer, ready_to_deactivate, fenced, transfer_result))
            }
            None => None,
        }
    };
    let Some((transfer, ready_to_deactivate, _fenced, transfer_result)) = preflight else {
        continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        return;
    };
    if transfer_result.is_ok() {
        if ready_to_deactivate {
            if let Some(active) = cells.get_mut(&cell) {
                active.drain = Some(transfer.reply);
            }
            start_deactivate(cell, pool, cells, transitioning, tasks);
        } else if let Some(active) = cells.get_mut(&cell) {
            active.drain = Some(transfer.reply);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        } else {
            if let Some(mut permit) = movement_permits.remove(&cell) {
                movement.complete(&mut permit);
            }
        }
    } else {
        let _ = transfer.reply.send(transfer_result);
        if let Some(mut permit) = movement_permits.remove(&cell) {
            movement.complete(&mut permit);
        }
        continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
    }
}

/// Applies a migration result and reconciles the source Cell.
#[expect(
    clippy::too_many_arguments,
    reason = "the actor loop hands each protocol facility and finished-task field to the handler explicitly"
)]
pub(super) fn handle_migrated(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    publisher: Box<CellPublisher>,
    mut migration: Box<QueuedMigration>,
    mut result: crate::Result<MigrationOutcome>,
    mut fenced: bool,
    preserve_owner: bool,
    unpublished_bytes: u64,
) {
    let TaskContext {
        pool,
        cells,
        transitioning,
        tasks,
        node_lease,
        unpublished_node_log_bytes,
        ..
    } = context;
    let Some(active) = cells.get_mut(&cell) else {
        let result = match result {
            Ok(_) => Err(Error::Fenced),
            Err(error) => Err(error),
        };
        send_migration_reply(&mut migration, result);
        return;
    };
    if active.generation != generation
        || !active.coordination.effect_matches(
            effect_id,
            CoordinationEffect::Work(AdmissionKind::Migration),
        )
    {
        let result = match result {
            Ok(_) => Err(Error::Fenced),
            Err(error) => Err(error),
        };
        send_migration_reply(&mut migration, result);
        return;
    }
    active.finish_task(
        effect_id,
        CoordinationEffect::Work(AdmissionKind::Migration),
    );
    if node_lease.check().is_err() {
        result = Err(Error::Fenced);
        fenced = true;
    }
    active.publisher = Some(*publisher);
    if preserve_owner {
        active.unpublished_node_logs = active.unpublished_node_logs.saturating_add(1);
        unpublished_node_log_bytes.fetch_add(unpublished_bytes, Ordering::AcqRel);
    }
    let completion = finish_migration(active, fenced);
    match result {
        Ok(outcome) if !matches!(completion, CoordinationDecision::Fence) => {
            active.code = outcome.code;
            active.schema = outcome.schema;
            let admission = Arc::clone(&migration.successor_admission);
            send_migration_reply(&mut migration, Ok(MigratedAdmission { admission, outcome }));
        }
        Ok(_) => send_migration_reply(&mut migration, Err(Error::Fenced)),
        Err(error) => send_migration_reply(&mut migration, Err(error)),
    }
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}
