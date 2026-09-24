//! Timer firing and activity-expiry maintenance.
//!
//! Both entry points apply their event, decision effects, and terminal
//! activity bookkeeping inside the caller's transaction, so a Sweeper or
//! Cron Tick can never publish a half-applied timer.

use super::*;

/// Fires one due timer and applies its event in the same transaction.
pub fn workflow_fire_timer(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    now_ms: i64,
    run_id: [u8; 16],
    timer_id: [u8; 16],
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    let mut effects = next_effect_batch(transaction, source, now_ms)?;
    workflow_fire_timer_with_effects(
        transaction,
        &mut effects,
        source,
        now_ms,
        run_id,
        timer_id,
        definition,
    )
}

fn workflow_fire_timer_with_effects(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    source: &CellTarget,
    now_ms: i64,
    run_id: [u8; 16],
    timer_id: [u8; 16],
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    validate_now(now_ms)?;
    let Some(run) = load_run_by_id(transaction, run_id)? else {
        return Ok(WorkflowOutcome::RunMismatch);
    };
    verify_definition(&run, definition)?;
    let timer = transaction
        .query_row(
            "SELECT due_at_ms, state FROM workflow_timers WHERE run_id = ?1 AND timer_id = ?2",
            (run_id.as_slice(), timer_id.as_slice()),
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((due_at_ms, state)) = timer else {
        return Ok(WorkflowOutcome::RunMismatch);
    };
    if state == 1 {
        return Ok(WorkflowOutcome::Duplicate {
            run_id,
            status: run.status,
            event_sequence: run.event_sequence,
        });
    }
    if state != 0 || run.status != WorkflowStatus::Running {
        return Ok(WorkflowOutcome::NotRunning);
    }
    if due_at_ms > now_ms {
        return Ok(WorkflowOutcome::NotDue);
    }
    let mut event = b"timer\0".to_vec();
    event.extend_from_slice(&timer_id);
    let (sequence, decision) =
        prepare_transition(transaction, source, &run, definition, &event, now_ms)?;
    if transaction.execute(
        "UPDATE workflow_timers SET state = 1 WHERE run_id = ?1 AND timer_id = ?2 AND state = 0 AND due_at_ms <= ?3",
        (run_id.as_slice(), timer_id.as_slice(), now_ms),
    )? != 1
    {
        return Err(Error::Command("workflow timer changed during serialized fire"));
    }
    commit_transition(
        transaction,
        effects,
        run_id,
        sequence,
        timer_event_id(run_id, timer_id),
        &event,
        now_ms,
        decision,
    )
}

pub(crate) fn workflow_fail_one_expired_activity(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    source: &CellTarget,
    now_ms: i64,
    definitions: &[&'static dyn WorkflowDefinition],
) -> Result<bool> {
    validate_now(now_ms)?;
    let candidate = transaction
        .query_row(
            "SELECT a.run_id, a.activity_id, a.attempt, a.expires_at_ms, r.definition_digest FROM workflow_activities a INDEXED BY activities_due JOIN workflow_runs r ON r.run_id = a.run_id WHERE ((a.state = 0 AND (a.expires_at_ms <= ?1 OR a.attempt >= ?2)) OR (a.state = 1 AND a.lease_until_ms <= ?1 AND (a.expires_at_ms <= ?1 OR a.attempt >= ?2))) AND r.status = 0 ORDER BY a.due_at_ms, a.run_id, a.activity_id LIMIT 1",
            (now_ms, i64::from(activity::MAX_ATTEMPTS)),
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((run_id, activity_id, attempt, expires_at_ms, digest)) = candidate else {
        return Ok(false);
    };
    let run_id = exact_array(run_id, "invalid stored workflow run ID")?;
    let activity_id = exact_array(activity_id, "invalid stored workflow activity ID")?;
    let digest = Digest::from_bytes(exact_array(
        digest,
        "invalid stored workflow definition digest",
    )?);
    let definition = retained_definition(definitions, digest)?;
    let run = load_run_by_id(transaction, run_id)?
        .ok_or(Error::Command("workflow activity references a missing run"))?;
    let details = if expires_at_ms <= now_ms {
        b"expired".as_slice()
    } else if attempt >= i64::from(activity::MAX_ATTEMPTS) {
        b"attempts-exhausted".as_slice()
    } else {
        return Err(Error::Command("invalid due workflow activity"));
    };
    let event = activity_failure_event(activity_id, details);
    let (sequence, decision) =
        prepare_transition(transaction, source, &run, definition, &event, now_ms)?;
    if transaction.execute(
        "UPDATE workflow_activities SET state = 3, token = NULL, lease_until_ms = NULL WHERE run_id = ?1 AND activity_id = ?2 AND ((state = 0 AND (expires_at_ms <= ?3 OR attempt >= ?4)) OR (state = 1 AND lease_until_ms <= ?3 AND (expires_at_ms <= ?3 OR attempt >= ?4)))",
        (
            run_id.as_slice(),
            activity_id.as_slice(),
            now_ms,
            i64::from(activity::MAX_ATTEMPTS),
        ),
    )? != 1
    {
        return Err(Error::Command(
            "workflow activity changed during terminal failure",
        ));
    }
    commit_transition(
        transaction,
        effects,
        run_id,
        sequence,
        activity_failure_event_id(run_id, activity_id),
        &event,
        now_ms,
        decision,
    )?;
    Ok(true)
}

pub(crate) fn workflow_fire_one_due_timer(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    source: &CellTarget,
    now_ms: i64,
    definitions: &[&'static dyn WorkflowDefinition],
) -> Result<bool> {
    validate_now(now_ms)?;
    let candidate = transaction
        .query_row(
            "SELECT t.run_id, t.timer_id, r.definition_digest FROM workflow_timers t INDEXED BY timers_due JOIN workflow_runs r ON r.run_id = t.run_id WHERE t.state = 0 AND t.due_at_ms <= ?1 AND r.status = 0 ORDER BY t.due_at_ms, t.run_id, t.timer_id LIMIT 1",
            [now_ms],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((run_id, timer_id, digest)) = candidate else {
        return Ok(false);
    };
    let run_id = exact_array(run_id, "invalid stored workflow run ID")?;
    let timer_id = exact_array(timer_id, "invalid stored workflow timer ID")?;
    let digest = Digest::from_bytes(exact_array(
        digest,
        "invalid stored workflow definition digest",
    )?);
    workflow_fire_timer_with_effects(
        transaction,
        effects,
        source,
        now_ms,
        run_id,
        timer_id,
        retained_definition(definitions, digest)?,
    )?;
    Ok(true)
}

pub(super) fn next_effect_batch(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    now_ms: i64,
) -> Result<EffectBatch> {
    let sequence = transaction.query_row(
        "SELECT commit_sequence + 1 FROM sys_meta WHERE singleton = 1 AND commit_sequence < 9223372036854775807",
        [],
        |row| row.get::<_, u64>(0),
    )?;
    EffectBatch::new(transaction, source, sequence, now_ms)
}

fn retained_definition(
    definitions: &[&'static dyn WorkflowDefinition],
    digest: Digest,
) -> Result<&'static dyn WorkflowDefinition> {
    definitions
        .iter()
        .copied()
        .find(|definition| definition.digest() == digest)
        .ok_or(Error::Command("workflow definition digest is unavailable"))
}

fn activity_failure_event(activity_id: [u8; 16], details: &[u8]) -> Vec<u8> {
    let mut event = Vec::with_capacity(34 + details.len());
    event.extend_from_slice(b"activity\0");
    event.push(1);
    event.extend_from_slice(&activity_id);
    event.extend_from_slice(&(details.len() as u32).to_be_bytes());
    event.extend_from_slice(details);
    event
}

fn activity_failure_event_id(run_id: [u8; 16], activity_id: [u8; 16]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.activity-terminal.v1\0");
    hasher.update(&run_id);
    hasher.update(&activity_id);
    *hasher.finalize().as_bytes()
}
