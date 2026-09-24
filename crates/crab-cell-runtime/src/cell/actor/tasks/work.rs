//! Command, query, and resolve completions for one Cell actor.

use super::*;

/// Applies a command execution result and answers its waiters.
pub(super) fn handle_executed(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    mut command: Box<QueuedCommand>,
    mut result: crate::Result<CommandTaskResult>,
    mut fenced: bool,
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
        // Publication failure may fence and remove the Cell before its proof waiter
        // completes; the accepted command still owns exactly one terminal outcome.
        send_command_task_reply(&mut command, result);
        return;
    };
    if active.generation != generation
        || !active
            .coordination
            .effect_matches(effect_id, CoordinationEffect::Work(AdmissionKind::Command))
    {
        send_command_task_reply(&mut command, result);
        return;
    }
    active.finish_task(effect_id, CoordinationEffect::Work(AdmissionKind::Command));
    if node_lease.check().is_err() {
        result = Err(command.operation.unknown(Error::Fenced));
        fenced = true;
    }
    if fenced {
        finish_work(active, true);
        send_command_task_reply(&mut command, result);
        continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        return;
    }
    match result {
        Ok(CommandTaskResult::Recorded(outcome)) => {
            finish_work(active, false);
            send_command_reply(&mut command, Ok(outcome));
        }
        Ok(CommandTaskResult::Pending {
            pending,
            durability,
            retained_reservation,
        }) => {
            let retained_bytes = pending.retained_bytes();
            let publication = active
                .coordination
                .step(CoordinationInput::BeginPublication);
            let CoordinationDecision::Started = publication else {
                drop(retained_reservation);
                finish_work(active, false);
                let error = command.operation.unknown(match publication {
                    CoordinationDecision::Reject(reason) => rejection_error(reason),
                    _ => Error::Fenced,
                });
                send_command_reply(&mut command, Err(error));
                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                return;
            };
            active.publication_bytes = match active.publication_bytes.checked_add(retained_bytes) {
                Some(bytes) => bytes,
                None => {
                    finish_work(active, true);
                    let error = command
                        .operation
                        .unknown(Error::Capacity("pending publication bytes"));
                    send_command_reply(&mut command, Err(error));
                    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                    return;
                }
            };
            let outcome = pending.outcome().clone();
            let commit_sequence = outcome.commit_sequence();
            let (proof, object) = oneshot::channel();
            active.publications.push_back(QueuedPublication {
                pending: *pending,
                durability: durability.clone(),
                retained_reservation,
                submitted_at: std::time::Instant::now(),
                proof,
            });
            if durability.is_some() {
                active.unpublished_node_logs += 1;
                unpublished_node_log_bytes.fetch_add(retained_bytes, Ordering::AcqRel);
            }
            start_publication(cell, active, pool, tasks);
            let pool = pool.clone();
            let generation = active.generation;
            let effect_id = active.begin_task(CoordinationEffect::Proof);
            tasks.spawn(async move {
                prove_command(
                    pool,
                    command,
                    outcome,
                    commit_sequence,
                    durability,
                    object,
                    generation,
                    effect_id,
                )
                .await
            });
        }
        Err(error) => {
            finish_work(active, false);
            send_command_reply(&mut command, Err(error));
        }
    }
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}

/// Applies a query result and answers its waiter.
pub(super) fn handle_queried(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    mut query: Box<QueuedQuery>,
    mut result: crate::Result<Vec<u8>>,
    mut fenced: bool,
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
        // A query accepted before a fence keeps its result even when deactivation wins
        // the actor turn before this completion is delivered.
        send_query_reply(&mut query, result);
        return;
    };
    if active.generation != generation
        || !active
            .coordination
            .effect_matches(effect_id, CoordinationEffect::Work(AdmissionKind::Query))
    {
        send_query_reply(&mut query, result);
        return;
    }
    active.finish_task(effect_id, CoordinationEffect::Work(AdmissionKind::Query));
    if node_lease.check().is_err() {
        result = Err(Error::Fenced);
        fenced = true;
    }
    finish_work(active, fenced);
    send_query_reply(&mut query, result);
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}

/// Applies a resolve result and answers its waiter.
pub(super) fn handle_resolved(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    mut resolve: Box<QueuedResolve>,
    mut result: crate::Result<Resolution>,
    mut fenced: bool,
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
        // Resolution is an accepted observation, not a new admission; preserve its
        // unknown/committed result across a concurrent fenced deactivation.
        send_resolve_reply(&mut resolve, result);
        return;
    };
    if active.generation != generation
        || !active
            .coordination
            .effect_matches(effect_id, CoordinationEffect::Work(AdmissionKind::Resolve))
    {
        send_resolve_reply(&mut resolve, result);
        return;
    }
    active.finish_task(effect_id, CoordinationEffect::Work(AdmissionKind::Resolve));
    if node_lease.check().is_err() {
        result = Ok(Resolution::Unknown);
        fenced = true;
    }
    finish_work(active, fenced);
    send_resolve_reply(&mut resolve, result);
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}
