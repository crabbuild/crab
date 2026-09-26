//! Admission, fencing, completion, and reply helpers.

use super::*;

pub(super) fn fail_shutdown(shutdown: &mut Option<ShutdownState>, error: Error) {
    if let Some(state) = shutdown.as_mut()
        && state.error.is_none()
    {
        state.error = Some(error);
    }
}

pub(super) fn finish_shutdown(shutdown: &mut Option<ShutdownState>) {
    let Some(mut state) = shutdown.take() else {
        return;
    };
    let result = state.error.take().map_or(Ok(()), Err);
    let _ = state.reply.send(result);
}

pub(super) fn subtract_unpublished_bytes(total: &AtomicU64, bytes: u64) {
    let _ = total.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_sub(bytes))
    });
}

pub(super) fn rejection_error(reason: RejectReason) -> Error {
    match reason {
        RejectReason::NotActive => Error::CellNotActive,
        RejectReason::Fenced => Error::Fenced,
        RejectReason::Draining => Error::CellDraining,
        RejectReason::Busy => Error::CellDraining,
        RejectReason::PublicationPending => Error::PendingPublication,
    }
}

pub(super) fn finish_work(active: &mut ActiveCell, fenced: bool) -> CoordinationDecision {
    active.last_work_at = std::time::Instant::now();
    let decision = active
        .coordination
        .step(CoordinationInput::FinishWork { fenced });
    if matches!(decision, CoordinationDecision::Fence) {
        fence_active(active);
    }
    decision
}

pub(super) fn finish_migration(active: &mut ActiveCell, fenced: bool) -> CoordinationDecision {
    active.last_work_at = std::time::Instant::now();
    let decision = active
        .coordination
        .step(CoordinationInput::FinishMigration { fenced });
    if matches!(decision, CoordinationDecision::Fence) {
        fence_active(active);
    }
    decision
}

pub(super) fn fence_active(active: &mut ActiveCell) {
    active.coordination.step(CoordinationInput::Fence);
    fence_admission(&active.admission);
    if let Some(transfer) = active.transfer.take() {
        let _ = transfer.reply.send(Err(Error::Fenced));
    }
    active.inventory_refreshing = false;
    while let Some(publication) = active.publications.pop_front() {
        active
            .coordination
            .step(CoordinationInput::FinishPublication {
                fenced: true,
                succeeded: false,
            });
        active.publication_bytes = active
            .publication_bytes
            .saturating_sub(publication.pending.retained_bytes());
        let _ = publication.proof.send(Err(Error::Fenced));
    }
    while let Some(queued) = active.queue.pop_front() {
        match queued {
            QueuedWork::Command(mut command) => {
                send_command_reply(&mut command, Err(Error::Fenced));
            }
            QueuedWork::Query(mut query) => {
                send_query_reply(&mut query, Err(Error::Fenced));
            }
            QueuedWork::Resolve(mut resolve) => {
                send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
            }
            QueuedWork::Migration(mut migration) => {
                send_migration_reply(&mut migration, Err(Error::Fenced));
            }
        }
    }
}

pub(super) fn fence_admission(admission: &CellAdmission) {
    admission.fenced.store(true, Ordering::Release);
    admission.draining.store(true, Ordering::Release);
    admission.requests.close();
    admission.bytes.close();
}

pub(super) fn new_cell_admission() -> Arc<CellAdmission> {
    Arc::new(CellAdmission {
        requests: Arc::new(Semaphore::new(CELL_REQUESTS)),
        bytes: Arc::new(Semaphore::new(CELL_BYTES)),
        draining: AtomicBool::new(false),
        fenced: AtomicBool::new(false),
    })
}

pub(super) fn send_command_reply(
    command: &mut QueuedCommand,
    result: crate::Result<StoredOutcome>,
) {
    if let Some(reply) = command.reply.take() {
        let sequence = result.as_ref().ok().map(StoredOutcome::commit_sequence);
        if reply.send(result).is_ok()
            && let Some(commit_sequence) = sequence
        {
            use crate::fleet::telemetry::CommandResponseSource;
            use crate::node::log::DurabilitySource;

            let (source, confirmation) = match command.response_proof {
                Some((DurabilitySource::Fleet, elapsed)) => (CommandResponseSource::Fleet, elapsed),
                Some((DurabilitySource::Object, elapsed)) => {
                    (CommandResponseSource::Object, elapsed)
                }
                None => (CommandResponseSource::Recorded, std::time::Duration::ZERO),
            };
            // Final admission may refuse a proven command. Observe at the one
            // reply boundary so failed or later object proofs cannot add winners.
            let elapsed = command.queued_at.elapsed();
            tracing::debug!(
                target: "crab_cell_runtime::action",
                parent: &command.trace,
                event = "cell_command_response",
                commit_sequence,
                source = ?source,
                response_us = elapsed.as_micros(),
                confirmation_us = confirmation.as_micros(),
                "Cell command response released"
            );
            command
                .telemetry
                .command_response(source, elapsed, confirmation);
        }
    }
}

pub(super) fn send_command_task_reply(
    command: &mut QueuedCommand,
    result: crate::Result<CommandTaskResult>,
) {
    let result = match result {
        Ok(CommandTaskResult::Recorded(outcome)) => Ok(outcome),
        Ok(CommandTaskResult::Pending { .. }) => Err(command.operation.unknown(Error::Fenced)),
        Err(error) => Err(error),
    };
    send_command_reply(command, result);
}

pub(super) fn send_query_reply(query: &mut QueuedQuery, result: crate::Result<Vec<u8>>) {
    if let Some(reply) = query.reply.take() {
        let _ = reply.send(result);
    }
}

pub(super) fn send_resolve_reply(resolve: &mut QueuedResolve, result: crate::Result<Resolution>) {
    if let Some(reply) = resolve.reply.take() {
        let _ = reply.send(result);
    }
}

pub(super) fn send_migration_reply(
    migration: &mut QueuedMigration,
    result: crate::Result<MigratedAdmission>,
) {
    if let Some(reply) = migration.reply.take() {
        let _ = reply.send(result);
    }
}
