//! Publication proof, publish, and compaction completions.

use super::*;

/// Applies a publication proof result and answers its waiter.
pub(super) fn handle_proven(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    mut command: Box<QueuedCommand>,
    mut result: crate::Result<StoredOutcome>,
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
        // The publication task may fence and remove the actor first; proof owns the
        // caller's final result and must not be rewritten as CellNotActive.
        send_command_reply(&mut command, result);
        return;
    };
    if active.generation != generation
        || !active
            .coordination
            .effect_matches(effect_id, CoordinationEffect::Proof)
    {
        send_command_reply(&mut command, result);
        return;
    }
    active.finish_task(effect_id, CoordinationEffect::Proof);
    if node_lease.check().is_err() {
        result = Err(command.operation.unknown(Error::Fenced));
        fenced = true;
    }
    finish_work(active, fenced);
    send_command_reply(&mut command, result);
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}

/// Applies a publish result, its byte accounting, and its failure cleanup.
#[expect(
    clippy::too_many_arguments,
    reason = "the actor loop hands each protocol facility and finished-task field to the handler explicitly"
)]
pub(super) fn handle_published(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    publisher: Box<CellPublisher>,
    retained_bytes: u64,
    node_logged: bool,
    next_due_ms: Option<i64>,
    commit_sequence: u64,
    mut result: crate::Result<()>,
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
        return;
    };
    if active.generation != generation
        || !active
            .coordination
            .effect_matches(effect_id, CoordinationEffect::Publication)
    {
        return;
    }
    active.finish_task(effect_id, CoordinationEffect::Publication);
    active.last_work_at = std::time::Instant::now();
    let object_published = result.is_ok();
    if object_published {
        // Control now names this commit, so the local mirror can answer a due
        // scan without reading the record back.
        active.next_due_ms = next_due_ms;
        active.published_sequence = commit_sequence;
    }
    if node_lease.check().is_err() {
        result = Err(Error::Fenced);
        fenced = true;
    }
    active.publisher = Some(*publisher);
    active.publication_bytes = active.publication_bytes.saturating_sub(retained_bytes);
    if node_logged && object_published {
        active.unpublished_node_logs = active.unpublished_node_logs.saturating_sub(1);
        subtract_unpublished_bytes(unpublished_node_log_bytes, retained_bytes);
    }
    let decision = active
        .coordination
        .step(CoordinationInput::FinishPublication {
            fenced,
            succeeded: result.is_ok(),
        });
    if matches!(decision, CoordinationDecision::Fence) {
        fence_active(active);
    } else {
        start_publication(cell, active, pool, tasks);
    }
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}

/// Applies a compaction result and returns the publisher to the Cell.
pub(super) fn handle_compacted(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    publisher: Box<CellPublisher>,
    result: crate::Result<Option<bool>>,
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
            .effect_matches(effect_id, CoordinationEffect::Compaction)
    {
        return;
    }
    active.finish_task(effect_id, CoordinationEffect::Compaction);
    let fenced = result.is_err() || node_lease.check().is_err();
    active.publisher = Some(*publisher);
    if matches!(result, Ok(None)) {
        active.compaction_retry_at = std::time::Instant::now() + COMPACTION_RETRY;
    }
    let decision = active
        .coordination
        .step(CoordinationInput::FinishCompaction { fenced });
    if matches!(decision, CoordinationDecision::Fence) {
        fence_active(active);
    }
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}
