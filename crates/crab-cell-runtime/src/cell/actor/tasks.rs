//! Task-completion handling for one Cell actor.
//!
//! Everything here reacts to a finished background task: activation,
//! publication, migration, transfer, hydration, eviction, and drain. The loop
//! itself and the request paths stay in `task.rs`; each completion kind has
//! its own handler in `tasks/`, and this root keeps the dispatch.

use super::admission::{
    fail_shutdown, fence_active, fence_admission, finish_migration, finish_work, rejection_error,
    send_command_reply, send_command_task_reply, send_migration_reply, send_query_reply,
    send_resolve_reply, subtract_unpublished_bytes,
};
use super::*;

mod activation;
mod movement;
mod publication;
mod residency;
mod work;

/// Actor-loop facilities a finished task borrows while it is handled.
struct TaskContext<'a> {
    pool: &'a SqlWorkerPool,
    cells: &'a mut HashMap<CellId, ActiveCell>,
    transitioning: &'a mut HashSet<CellId>,
    tasks: &'a mut JoinSet<TaskResult>,
    shutdown: &'a mut Option<ShutdownState>,
    node_lease: &'a RuntimeNodeLease,
    unpublished_node_log_bytes: &'a AtomicU64,
    movement: &'a mut MovementBudget,
    movement_permits: &'a mut HashMap<CellId, MovementPermit>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "the actor adapter passes each independently owned protocol facility explicitly"
)]
pub(super) fn handle_task(
    result: TaskResult,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown: &mut Option<ShutdownState>,
    node_lease: &RuntimeNodeLease,
    unpublished_node_log_bytes: &AtomicU64,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
) {
    let context = TaskContext {
        pool,
        cells,
        transitioning,
        tasks,
        shutdown,
        node_lease,
        unpublished_node_log_bytes,
        movement,
        movement_permits,
    };
    match result {
        TaskResult::Activated {
            cell,
            generation,
            role,
            publisher,
            admission,
            reply,
            result,
            persisted_work,
        } => activation::handle_activated(
            context,
            cell,
            generation,
            role,
            publisher,
            admission,
            reply,
            result,
            persisted_work,
        ),
        TaskResult::Hydrated {
            cell,
            generation,
            effect_id,
            result,
        } => activation::handle_hydrated(context, cell, generation, effect_id, result),
        TaskResult::Executed {
            cell,
            generation,
            effect_id,
            command,
            result,
            fenced,
        } => work::handle_executed(
            context, cell, generation, effect_id, command, result, fenced,
        ),
        TaskResult::Queried {
            cell,
            generation,
            effect_id,
            query,
            result,
            fenced,
        } => work::handle_queried(context, cell, generation, effect_id, query, result, fenced),
        TaskResult::Resolved {
            cell,
            generation,
            effect_id,
            resolve,
            result,
            fenced,
        } => work::handle_resolved(
            context, cell, generation, effect_id, resolve, result, fenced,
        ),
        TaskResult::Proven {
            cell,
            generation,
            effect_id,
            command,
            result,
            fenced,
        } => publication::handle_proven(
            context, cell, generation, effect_id, command, result, fenced,
        ),
        TaskResult::Published {
            cell,
            generation,
            effect_id,
            publisher,
            retained_bytes,
            node_logged,
            result,
            fenced,
        } => publication::handle_published(
            context,
            cell,
            generation,
            effect_id,
            publisher,
            retained_bytes,
            node_logged,
            result,
            fenced,
        ),
        TaskResult::Compacted {
            cell,
            generation,
            effect_id,
            publisher,
            result,
        } => publication::handle_compacted(context, cell, generation, effect_id, publisher, result),
        TaskResult::TransferPreflight {
            cell,
            generation,
            effect_id,
            result,
        } => movement::handle_transfer_preflight(context, cell, generation, effect_id, result),
        TaskResult::Migrated {
            cell,
            generation,
            effect_id,
            publisher,
            migration,
            result,
            fenced,
            preserve_owner,
            unpublished_bytes,
        } => movement::handle_migrated(
            context,
            cell,
            generation,
            effect_id,
            publisher,
            migration,
            result,
            fenced,
            preserve_owner,
            unpublished_bytes,
        ),
        TaskResult::InventoryRefreshed {
            cell,
            generation,
            effect_id,
            result,
        } => residency::handle_inventory_refreshed(context, cell, generation, effect_id, result),
        TaskResult::Renewed {
            cell,
            generation,
            effect_id,
            publisher,
            result,
        } => residency::handle_renewed(context, cell, generation, effect_id, publisher, result),
        TaskResult::Deactivated {
            cell,
            generation,
            reply,
            shutdown_drain,
            result,
        } => {
            residency::handle_deactivated(context, cell, generation, reply, shutdown_drain, result)
        }
    }
}
