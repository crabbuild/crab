//! The Workflow command surface: install, start, signal, cancel, control.
//!
//! Each entry point prepares one transition, applies its decision effects, and
//! commits the event and run row in the caller's transaction, so a caller
//! never observes a half-applied decision.

use super::*;

/// Installs the exact version-one Workflow schema during bootstrap or migration.
pub fn install_workflow_schema(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(WORKFLOW_SCHEMA)?;
    Ok(())
}

/// Starts one workflow and applies its first deterministic transition atomically.
pub fn workflow_start(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    now_ms: i64,
    request: &WorkflowStart,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    let mut effects = next_effect_batch(transaction, source, now_ms)?;
    workflow_start_with_effects(
        transaction,
        &mut effects,
        source,
        now_ms,
        request,
        definition,
    )
}

fn workflow_start_with_effects(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    source: &CellTarget,
    now_ms: i64,
    request: &WorkflowStart,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    validate_now(now_ms)?;
    validate_identifier(&request.workflow_id)?;
    validate_event(&request.event)?;
    if transaction
        .query_row(
            "SELECT 1 FROM workflow_runs WHERE workflow_id = ?1",
            [request.workflow_id.as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        return Ok(WorkflowOutcome::AlreadyExists);
    }

    let run_id = run_id(source.namespace(), request.request_id);
    let sequence = next_sequence(transaction, 0)?;
    let decision = definition.transition(
        &[],
        &request.event,
        WorkflowContext {
            source: source.clone(),
            run_id,
            event_sequence: sequence,
            now_ms,
        },
    )?;
    validate_decision(&decision, source, definition.effect_targets(), now_ms)?;
    transaction.execute(
        "INSERT INTO workflow_runs(workflow_id, run_id, definition_digest, status, state, event_sequence, result, completed_at_ms) VALUES (?1, ?2, ?3, 0, X'', 0, NULL, NULL)",
        (
            request.workflow_id.as_slice(),
            run_id.as_slice(),
            definition.digest().as_bytes().as_slice(),
        ),
    )?;
    insert_event(
        transaction,
        run_id,
        sequence,
        event_id(run_id, request.request_id.as_bytes()),
        &request.event,
    )?;
    apply_decision(transaction, effects, run_id, sequence, now_ms, decision)
}

/// Applies one idempotent signal to a running workflow.
pub fn workflow_signal(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    now_ms: i64,
    signal: &WorkflowSignal,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    let mut effects = next_effect_batch(transaction, source, now_ms)?;
    workflow_signal_with_effects(
        transaction,
        &mut effects,
        source,
        now_ms,
        signal,
        definition,
    )
}

fn workflow_signal_with_effects(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    source: &CellTarget,
    now_ms: i64,
    signal: &WorkflowSignal,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    validate_now(now_ms)?;
    validate_identifier(&signal.workflow_id)?;
    validate_event(&signal.event)?;
    let run = load_run(transaction, &signal.workflow_id)?;
    let Some(run) = run else {
        return Ok(WorkflowOutcome::RunMismatch);
    };
    if run.run_id != signal.run_id {
        return Ok(WorkflowOutcome::RunMismatch);
    }
    verify_definition(&run, definition)?;
    let id = event_id(run.run_id, &signal.signal_id);
    if let Some(digest) = stored_event_digest(transaction, run.run_id, id)? {
        return if digest == event_digest(&signal.event) {
            Ok(WorkflowOutcome::Duplicate {
                run_id: run.run_id,
                status: run.status,
                event_sequence: run.event_sequence,
            })
        } else {
            Ok(WorkflowOutcome::IdentityConflict)
        };
    }
    if run.status != WorkflowStatus::Running {
        return Ok(WorkflowOutcome::NotRunning);
    }
    let (sequence, decision) =
        prepare_transition(transaction, source, &run, definition, &signal.event, now_ms)?;
    commit_transition(
        transaction,
        effects,
        run.run_id,
        sequence,
        id,
        &signal.event,
        now_ms,
        decision,
    )
}

/// Cancels a running or paused workflow and its outstanding local work.
pub fn workflow_cancel(
    transaction: &Transaction<'_>,
    now_ms: i64,
    signal: &WorkflowSignal,
) -> Result<WorkflowOutcome> {
    validate_now(now_ms)?;
    validate_identifier(&signal.workflow_id)?;
    validate_event(&signal.event)?;
    let Some(run) = load_run(transaction, &signal.workflow_id)? else {
        return Ok(WorkflowOutcome::RunMismatch);
    };
    if run.run_id != signal.run_id {
        return Ok(WorkflowOutcome::RunMismatch);
    }
    let id = event_id(run.run_id, &signal.signal_id);
    if let Some(digest) = stored_event_digest(transaction, run.run_id, id)? {
        return if digest == event_digest(&signal.event) {
            Ok(WorkflowOutcome::Duplicate {
                run_id: run.run_id,
                status: run.status,
                event_sequence: run.event_sequence,
            })
        } else {
            Ok(WorkflowOutcome::IdentityConflict)
        };
    }
    if !matches!(run.status, WorkflowStatus::Running | WorkflowStatus::Paused) {
        return Ok(WorkflowOutcome::NotRunning);
    }
    let sequence = next_sequence(transaction, run.event_sequence)?;
    insert_event(transaction, run.run_id, sequence, id, &signal.event)?;
    if transaction.execute(
        "UPDATE workflow_runs SET status = 3, event_sequence = ?1, completed_at_ms = ?2 WHERE run_id = ?3 AND status IN (0, 4)",
        (sequence as i64, now_ms, run.run_id.as_slice()),
    )? != 1
    {
        return Err(Error::Command("workflow run changed during cancellation"));
    }
    cancel_outstanding(transaction, run.run_id)?;
    Ok(WorkflowOutcome::Applied {
        run_id: run.run_id,
        status: WorkflowStatus::Cancelled,
        event_sequence: sequence,
    })
}

/// Pauses, resumes, or restarts one exact workflow run.
pub fn workflow_control(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    now_ms: i64,
    request: &WorkflowControl,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    validate_now(now_ms)?;
    validate_identifier(&request.workflow_id)?;
    let Some(run) = load_run(transaction, &request.workflow_id)? else {
        return Ok(WorkflowOutcome::RunMismatch);
    };
    if run.run_id != request.run_id {
        return Ok(WorkflowOutcome::RunMismatch);
    }
    match &request.action {
        WorkflowControlAction::Pause => {
            if run.status != WorkflowStatus::Running {
                return Ok(WorkflowOutcome::NotRunning);
            }
            let leased = transaction.query_row(
                "SELECT count(*) FROM workflow_activities WHERE run_id = ?1 AND state = 1",
                [run.run_id.as_slice()],
                |row| row.get::<_, i64>(0),
            )?;
            if leased != 0 {
                return Ok(WorkflowOutcome::Busy);
            }
            if transaction.execute(
                "UPDATE workflow_runs SET status = 4 WHERE run_id = ?1 AND status = 0",
                [run.run_id.as_slice()],
            )? != 1
            {
                return Err(Error::Command("workflow changed during pause"));
            }
            Ok(WorkflowOutcome::Applied {
                run_id: run.run_id,
                status: WorkflowStatus::Paused,
                event_sequence: run.event_sequence,
            })
        }
        WorkflowControlAction::Resume => {
            if run.status != WorkflowStatus::Paused {
                return Ok(WorkflowOutcome::NotRunning);
            }
            if transaction.execute(
                "UPDATE workflow_runs SET status = 0 WHERE run_id = ?1 AND status = 4",
                [run.run_id.as_slice()],
            )? != 1
            {
                return Err(Error::Command("workflow changed during resume"));
            }
            Ok(WorkflowOutcome::Applied {
                run_id: run.run_id,
                status: WorkflowStatus::Running,
                event_sequence: run.event_sequence,
            })
        }
        WorkflowControlAction::Restart { request_id, event } => {
            if matches!(run.status, WorkflowStatus::Running | WorkflowStatus::Paused) {
                return Ok(WorkflowOutcome::Busy);
            }
            if run_id(source.namespace(), *request_id) == run.run_id {
                return Ok(WorkflowOutcome::IdentityConflict);
            }
            validate_event(event)?;
            delete_workflow_run(transaction, run.run_id)?;
            workflow_start(
                transaction,
                source,
                now_ms,
                &WorkflowStart {
                    workflow_id: request.workflow_id.clone(),
                    request_id: *request_id,
                    event: event.clone(),
                },
                definition,
            )
        }
    }
}

fn delete_workflow_run(transaction: &Transaction<'_>, run_id: [u8; 16]) -> Result<()> {
    transaction.execute(
        "DELETE FROM workflow_activities WHERE run_id = ?1",
        [run_id.as_slice()],
    )?;
    transaction.execute(
        "DELETE FROM workflow_timers WHERE run_id = ?1",
        [run_id.as_slice()],
    )?;
    transaction.execute(
        "DELETE FROM workflow_events WHERE run_id = ?1",
        [run_id.as_slice()],
    )?;
    if transaction.execute(
        "DELETE FROM workflow_runs WHERE run_id = ?1 AND status BETWEEN 1 AND 3",
        [run_id.as_slice()],
    )? != 1
    {
        return Err(Error::Command("workflow changed during restart"));
    }
    Ok(())
}
