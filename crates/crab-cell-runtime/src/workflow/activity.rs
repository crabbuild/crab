use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction, params_from_iter, types::Value};

use super::{
    WorkflowDefinition, WorkflowOutcome, WorkflowStatus, commit_transition, load_run_by_id,
    prepare_transition, verify_definition,
};
use crate::{Digest, EffectBatch, Error, Result};

pub(super) const MAX_CLAIM_ITEMS: usize = 32;
const MAX_CLAIM_BYTES: usize = 512 << 10;
const MAX_SCAN_ITEMS: usize = 128;
pub(super) const MAX_ACTIVITY_BYTES: usize = 256 << 10;
const MAX_ACTIVITY_TYPE_BYTES: usize = 256;
pub(super) const MAX_ATTEMPTS: u32 = 20;
const MIN_LEASE_MS: u32 = 5_000;
const MAX_LEASE_MS: u32 = 300_000;
const DELIVERY_MARGIN_MS: i64 = 1_000;
const TERMINAL_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;

/// Source of unpredictable activity lease tokens.
pub trait ActivityTokenSource {
    fn next_token(&mut self) -> Result<[u8; 16]>;
}

/// Cryptographically seeded process-local activity token source.
pub struct SystemActivityTokens;

impl ActivityTokenSource for SystemActivityTokens {
    fn next_token(&mut self) -> Result<[u8; 16]> {
        let mut token = [0; 16];
        rand::rng().fill_bytes(&mut token);
        Ok(token)
    }
}

/// One activity type and pinned workflow definition available on a worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivitySupport {
    pub activity_type: String,
    pub definition_digest: Digest,
}

/// A published activity lease that may be emitted to native Rust code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityClaim {
    pub run_id: [u8; 16],
    pub activity_id: [u8; 16],
    pub activity_type: String,
    pub input: Vec<u8>,
    pub definition_digest: Digest,
    pub attempt: u32,
    pub token: [u8; 16],
    pub lease_until_ms: i64,
}

/// Idempotent completion or failure from one exact activity attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityCompletion {
    pub run_id: [u8; 16],
    pub activity_id: [u8; 16],
    pub attempt: u32,
    pub lease_token: [u8; 16],
    pub completion_token: [u8; 16],
    pub result: Vec<u8>,
    pub failed: bool,
    pub retryable: bool,
}

/// Business outcome of an activity lease extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityLeaseOutcome {
    Extended { lease_until_ms: i64 },
    LeaseLost,
}

/// Business outcome of applying an activity completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityCompletionOutcome {
    Applied(WorkflowOutcome),
    Retrying { due_at_ms: i64 },
    Duplicate { result: Vec<u8> },
    IdentityConflict,
    LeaseLost,
}

/// Reclaims expired leases and claims a bounded supported activity batch.
pub fn workflow_claim_activities(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
    lease_ms: u32,
    supported: &[ActivitySupport],
    tokens: &mut impl ActivityTokenSource,
) -> Result<Vec<ActivityClaim>> {
    validate_now(now_ms)?;
    if !(1..=MAX_CLAIM_ITEMS).contains(&limit) {
        return Err(Error::Command("activity claim limit must be in 1..=32"));
    }
    if !(MIN_LEASE_MS..=MAX_LEASE_MS).contains(&lease_ms) {
        return Err(Error::Command("activity lease must be in 5..=300 seconds"));
    }
    validate_support(supported)?;
    workflow_reclaim_expired_bounded(transaction, now_ms, MAX_SCAN_ITEMS)?;

    let mut sql = "SELECT a.run_id, a.activity_id, a.activity_type, a.input, r.definition_digest, a.attempt, a.expires_at_ms FROM workflow_activities a INDEXED BY activities_ready JOIN workflow_runs r ON r.run_id = a.run_id WHERE a.state = 0 AND a.due_at_ms <= ? AND a.expires_at_ms > ? AND a.attempt < ? AND r.status = 0 AND (a.activity_type, r.definition_digest) IN (".to_owned();
    for index in 0..supported.len() {
        if index != 0 {
            sql.push(',');
        }
        sql.push_str("(?, ?)");
    }
    sql.push_str(") ORDER BY a.due_at_ms, a.run_id, a.activity_id LIMIT ?");
    let mut parameters = Vec::with_capacity(4 + supported.len() * 2);
    parameters.extend([
        Value::Integer(now_ms),
        Value::Integer(now_ms),
        Value::Integer(i64::from(MAX_ATTEMPTS)),
    ]);
    for support in supported {
        parameters.push(Value::Text(support.activity_type.clone()));
        parameters.push(Value::Blob(support.definition_digest.as_bytes().to_vec()));
    }
    parameters.push(Value::Integer(MAX_SCAN_ITEMS as i64));
    let mut statement = transaction.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(parameters.iter()), |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, Vec<u8>>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut candidates = Vec::with_capacity(limit);
    let mut payload_bytes = 0_usize;
    for row in rows {
        if candidates.len() == limit {
            break;
        }
        let (run_id, activity_id, activity_type, input, definition, attempt, expires_at_ms) = row?;
        let run_id = exact::<16>(run_id, "workflow activity run ID")?;
        let activity_id = exact::<16>(activity_id, "workflow activity ID")?;
        let definition = Digest::from_bytes(exact::<32>(
            definition,
            "workflow activity definition digest",
        )?);
        if activity_type.is_empty()
            || activity_type.len() > MAX_ACTIVITY_TYPE_BYTES
            || input.len() > MAX_ACTIVITY_BYTES
            || attempt < 0
        {
            return Err(Error::Command("invalid stored workflow activity"));
        }
        let next_bytes = payload_bytes
            .checked_add(activity_type.len())
            .and_then(|value| value.checked_add(input.len()))
            .ok_or(Error::Command("activity claim byte count overflow"))?;
        if next_bytes > MAX_CLAIM_BYTES {
            break;
        }
        let attempt = u32::try_from(attempt)
            .map_err(|_| Error::Command("invalid stored workflow activity attempt"))?;
        payload_bytes = next_bytes;
        candidates.push((
            run_id,
            activity_id,
            activity_type,
            input,
            definition,
            attempt,
            expires_at_ms,
        ));
    }
    drop(statement);

    let requested_deadline = now_ms
        .checked_add(i64::from(lease_ms))
        .ok_or(Error::Command("activity lease deadline overflow"))?;
    let mut claimed = Vec::with_capacity(candidates.len());
    for (run_id, activity_id, activity_type, input, definition, attempt, expires_at_ms) in
        candidates
    {
        let token = tokens.next_token()?;
        let lease_until_ms = requested_deadline.min(expires_at_ms);
        if transaction.execute(
            "UPDATE workflow_activities SET state = 1, attempt = attempt + 1, token = ?1, lease_until_ms = ?2, completion_token = NULL, completion_digest = NULL, result = NULL WHERE run_id = ?3 AND activity_id = ?4 AND state = 0 AND due_at_ms <= ?5 AND expires_at_ms > ?5 AND attempt = ?6 AND EXISTS (SELECT 1 FROM workflow_runs r WHERE r.run_id = workflow_activities.run_id AND r.status = 0)",
            (
                token.as_slice(),
                lease_until_ms,
                run_id.as_slice(),
                activity_id.as_slice(),
                now_ms,
                i64::from(attempt),
            ),
        )? != 1
        {
            return Err(Error::Command("activity claim lost selected ready row"));
        }
        claimed.push(ActivityClaim {
            run_id,
            activity_id,
            activity_type,
            input,
            definition_digest: definition,
            attempt: attempt + 1,
            token,
            lease_until_ms,
        });
    }
    Ok(claimed)
}

/// Verifies that every claimed activity is still live after its root publishes.
pub fn workflow_validate_activity_claim(
    connection: &Connection,
    now_ms: i64,
    claimed: &[ActivityClaim],
) -> Result<bool> {
    validate_now(now_ms)?;
    if claimed.len() > MAX_CLAIM_ITEMS {
        return Err(Error::Command("activity claim exceeds 32 items"));
    }
    let minimum = now_ms
        .checked_add(DELIVERY_MARGIN_MS)
        .ok_or(Error::Command("activity delivery margin overflow"))?;
    for activity in claimed {
        if activity.input.len() > MAX_ACTIVITY_BYTES || activity.lease_until_ms < minimum {
            return Ok(false);
        }
        let live = connection
            .query_row(
                "SELECT 1 FROM workflow_activities a JOIN workflow_runs r ON r.run_id = a.run_id WHERE a.run_id = ?1 AND a.activity_id = ?2 AND a.state = 1 AND a.attempt = ?3 AND a.token = ?4 AND a.lease_until_ms = ?5 AND a.lease_until_ms >= ?6 AND r.status = 0 AND r.definition_digest = ?7",
                (
                    activity.run_id.as_slice(),
                    activity.activity_id.as_slice(),
                    i64::from(activity.attempt),
                    activity.token.as_slice(),
                    activity.lease_until_ms,
                    minimum,
                    activity.definition_digest.as_bytes().as_slice(),
                ),
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !live {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Extends one exact live activity lease without shortening it.
pub fn workflow_extend_activity(
    transaction: &Transaction<'_>,
    now_ms: i64,
    claim: &ActivityClaim,
    extension_ms: u32,
) -> Result<ActivityLeaseOutcome> {
    validate_now(now_ms)?;
    if !(MIN_LEASE_MS..=MAX_LEASE_MS).contains(&extension_ms) {
        return Err(Error::Command(
            "activity extension must be in 5..=300 seconds",
        ));
    }
    let current = transaction
        .query_row(
            "SELECT a.lease_until_ms, a.expires_at_ms FROM workflow_activities a JOIN workflow_runs r ON r.run_id = a.run_id WHERE a.run_id = ?1 AND a.activity_id = ?2 AND a.state = 1 AND a.attempt = ?3 AND a.token = ?4 AND a.lease_until_ms > ?5 AND r.status = 0 AND r.definition_digest = ?6",
            (
                claim.run_id.as_slice(),
                claim.activity_id.as_slice(),
                i64::from(claim.attempt),
                claim.token.as_slice(),
                now_ms,
                claim.definition_digest.as_bytes().as_slice(),
            ),
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((current, expires_at_ms)) = current else {
        return Ok(ActivityLeaseOutcome::LeaseLost);
    };
    let requested = now_ms
        .checked_add(i64::from(extension_ms))
        .ok_or(Error::Command("activity extension deadline overflow"))?;
    let lease_until_ms = current.max(requested.min(expires_at_ms));
    if transaction.execute(
        "UPDATE workflow_activities SET lease_until_ms = ?1 WHERE run_id = ?2 AND activity_id = ?3 AND state = 1 AND attempt = ?4 AND token = ?5 AND lease_until_ms = ?6",
        (
            lease_until_ms,
            claim.run_id.as_slice(),
            claim.activity_id.as_slice(),
            i64::from(claim.attempt),
            claim.token.as_slice(),
            current,
        ),
    )? != 1
    {
        return Err(Error::Command("activity lease changed during serialized extension"));
    }
    Ok(ActivityLeaseOutcome::Extended { lease_until_ms })
}

/// Completes, fails or reschedules one exact activity attempt atomically.
pub fn workflow_complete_activity(
    transaction: &Transaction<'_>,
    source: &crate::CellTarget,
    now_ms: i64,
    completion: &ActivityCompletion,
    definition: &dyn WorkflowDefinition,
) -> Result<ActivityCompletionOutcome> {
    let mut effects = super::next_effect_batch(transaction, source, now_ms)?;
    workflow_complete_activity_with_effects(
        transaction,
        &mut effects,
        source,
        now_ms,
        completion,
        definition,
    )
}

pub(super) fn workflow_complete_activity_with_effects(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    source: &crate::CellTarget,
    now_ms: i64,
    completion: &ActivityCompletion,
    definition: &dyn WorkflowDefinition,
) -> Result<ActivityCompletionOutcome> {
    validate_now(now_ms)?;
    if completion.result.len() > MAX_ACTIVITY_BYTES || (!completion.failed && completion.retryable)
    {
        return Err(Error::Command("invalid activity completion"));
    }
    let Some(run) = load_run_by_id(transaction, completion.run_id)? else {
        return Ok(ActivityCompletionOutcome::LeaseLost);
    };
    verify_definition(&run, definition)?;
    let stored = load_activity(transaction, completion.run_id, completion.activity_id)?;
    let Some(stored) = stored else {
        return Ok(ActivityCompletionOutcome::LeaseLost);
    };
    if stored.attempt != completion.attempt {
        return Ok(ActivityCompletionOutcome::LeaseLost);
    }
    let digest = completion_digest(completion);
    if let Some(token) = stored.completion_token {
        return if token == completion.completion_token && stored.completion_digest == Some(digest) {
            Ok(ActivityCompletionOutcome::Duplicate {
                result: stored.result,
            })
        } else {
            Ok(ActivityCompletionOutcome::IdentityConflict)
        };
    }
    if run.status != WorkflowStatus::Running
        || stored.state != 1
        || stored.token != Some(completion.lease_token)
        || stored
            .lease_until_ms
            .is_none_or(|deadline| deadline <= now_ms)
    {
        return Ok(ActivityCompletionOutcome::LeaseLost);
    }

    if completion.failed && completion.retryable && completion.attempt < MAX_ATTEMPTS {
        let due_at_ms = now_ms.saturating_add(retry_delay_ms(completion.attempt));
        if due_at_ms < stored.expires_at_ms {
            update_completion(transaction, completion, &stored, 0, Some(due_at_ms), digest)?;
            return Ok(ActivityCompletionOutcome::Retrying { due_at_ms });
        }
    }

    let event = completion_event(completion);
    let (sequence, decision) =
        prepare_transition(transaction, source, &run, definition, &event, now_ms)?;
    update_completion(
        transaction,
        completion,
        &stored,
        if completion.failed { 3 } else { 2 },
        None,
        digest,
    )?;
    let outcome = commit_transition(
        transaction,
        effects,
        completion.run_id,
        sequence,
        completion_event_id(completion),
        &event,
        now_ms,
        decision,
    )?;
    Ok(ActivityCompletionOutcome::Applied(outcome))
}

/// Deletes at most 128 terminal workflow runs whose retention has elapsed.
pub fn workflow_cleanup_terminal(transaction: &Transaction<'_>, now_ms: i64) -> Result<usize> {
    workflow_cleanup_terminal_bounded(transaction, now_ms, MAX_SCAN_ITEMS)
}

pub(crate) fn workflow_cleanup_terminal_bounded(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
) -> Result<usize> {
    validate_now(now_ms)?;
    validate_maintenance_limit(limit)?;
    if limit == 0 {
        return Ok(0);
    }
    let cutoff = now_ms.saturating_sub(TERMINAL_RETENTION_MS);
    let run_ids = {
        let mut statement = transaction.prepare(
            "SELECT run_id FROM workflow_runs WHERE status != 0 AND completed_at_ms <= ?1 ORDER BY completed_at_ms, run_id LIMIT ?2",
        )?;
        statement
            .query_map((cutoff, limit as i64), |row| row.get::<_, Vec<u8>>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for value in &run_ids {
        let run_id = exact::<16>(value.clone(), "terminal workflow run ID")?;
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
            "DELETE FROM workflow_runs WHERE run_id = ?1 AND status != 0 AND completed_at_ms <= ?2",
            (run_id.as_slice(), cutoff),
        )? != 1
        {
            return Err(Error::Command("terminal workflow changed during cleanup"));
        }
    }
    Ok(run_ids.len())
}

struct StoredActivity {
    state: i64,
    attempt: u32,
    token: Option<[u8; 16]>,
    lease_until_ms: Option<i64>,
    expires_at_ms: i64,
    completion_token: Option<[u8; 16]>,
    completion_digest: Option<[u8; 32]>,
    result: Vec<u8>,
}

fn load_activity(
    transaction: &Transaction<'_>,
    run_id: [u8; 16],
    activity_id: [u8; 16],
) -> Result<Option<StoredActivity>> {
    let stored = transaction
        .query_row(
            "SELECT state, attempt, token, lease_until_ms, expires_at_ms, completion_token, completion_digest, result FROM workflow_activities WHERE run_id = ?1 AND activity_id = ?2",
            (run_id.as_slice(), activity_id.as_slice()),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, Option<Vec<u8>>>(6)?,
                    row.get::<_, Option<Vec<u8>>>(7)?,
                ))
            },
        )
        .optional()?;
    stored
        .map(
            |(state, attempt, token, lease_until_ms, expires_at_ms, completion, digest, result)| {
                let result = result.unwrap_or_default();
                if !(0..=4).contains(&state)
                    || attempt < 0
                    || expires_at_ms < 0
                    || result.len() > MAX_ACTIVITY_BYTES
                {
                    return Err(Error::Command("invalid stored workflow activity"));
                }
                Ok(StoredActivity {
                    state,
                    attempt: u32::try_from(attempt)
                        .map_err(|_| Error::Command("invalid stored workflow activity attempt"))?,
                    token: optional_exact(token, "workflow activity token")?,
                    lease_until_ms,
                    expires_at_ms,
                    completion_token: optional_exact(completion, "activity completion token")?,
                    completion_digest: optional_exact(digest, "activity completion digest")?,
                    result,
                })
            },
        )
        .transpose()
}

fn update_completion(
    transaction: &Transaction<'_>,
    completion: &ActivityCompletion,
    stored: &StoredActivity,
    state: i64,
    due_at_ms: Option<i64>,
    digest: [u8; 32],
) -> Result<()> {
    let due_at_ms = due_at_ms.unwrap_or(0);
    if transaction.execute(
        "UPDATE workflow_activities SET state = ?1, due_at_ms = CASE WHEN ?1 = 0 THEN ?2 ELSE due_at_ms END, token = NULL, lease_until_ms = NULL, completion_token = ?3, completion_digest = ?4, result = ?5 WHERE run_id = ?6 AND activity_id = ?7 AND state = 1 AND attempt = ?8 AND token = ?9 AND lease_until_ms = ?10",
        (
            state,
            due_at_ms,
            completion.completion_token.as_slice(),
            digest.as_slice(),
            completion.result.as_slice(),
            completion.run_id.as_slice(),
            completion.activity_id.as_slice(),
            i64::from(completion.attempt),
            completion.lease_token.as_slice(),
            stored.lease_until_ms,
        ),
    )? != 1
    {
        return Err(Error::Command("activity changed during serialized completion"));
    }
    Ok(())
}

pub(crate) fn workflow_reclaim_expired_bounded(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
) -> Result<usize> {
    validate_now(now_ms)?;
    validate_maintenance_limit(limit)?;
    if limit == 0 {
        return Ok(0);
    }
    Ok(transaction.execute(
        "WITH expired(run_id, activity_id) AS (SELECT run_id, activity_id FROM workflow_activities INDEXED BY activities_leases WHERE state = 1 AND lease_until_ms <= ?1 ORDER BY lease_until_ms, run_id, activity_id LIMIT ?2) UPDATE workflow_activities SET state = 0, due_at_ms = CASE WHEN attempt >= ?3 OR expires_at_ms <= ?1 THEN due_at_ms ELSE ?1 END, token = NULL, lease_until_ms = NULL WHERE (run_id, activity_id) IN (SELECT run_id, activity_id FROM expired)",
        (now_ms, limit as i64, i64::from(MAX_ATTEMPTS)),
    )?)
}

fn validate_maintenance_limit(limit: usize) -> Result<()> {
    if limit > MAX_SCAN_ITEMS {
        return Err(Error::Command("workflow maintenance limit exceeds 128"));
    }
    Ok(())
}

fn validate_support(supported: &[ActivitySupport]) -> Result<()> {
    if supported.is_empty() || supported.len() > MAX_SCAN_ITEMS {
        return Err(Error::Command("activity support count must be in 1..=128"));
    }
    for (index, support) in supported.iter().enumerate() {
        if support.activity_type.is_empty()
            || support.activity_type.len() > MAX_ACTIVITY_TYPE_BYTES
            || supported[..index].contains(support)
        {
            return Err(Error::Command("invalid or duplicate activity support"));
        }
    }
    Ok(())
}

fn completion_digest(completion: &ActivityCompletion) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.activity-completion.v1\0");
    hasher.update(&[u8::from(completion.failed), u8::from(completion.retryable)]);
    hasher.update(&(completion.result.len() as u32).to_be_bytes());
    hasher.update(&completion.result);
    *hasher.finalize().as_bytes()
}

fn completion_event(completion: &ActivityCompletion) -> Vec<u8> {
    let mut event = Vec::with_capacity(34 + completion.result.len());
    event.extend_from_slice(b"activity\0");
    event.push(u8::from(completion.failed));
    event.extend_from_slice(&completion.activity_id);
    event.extend_from_slice(&(completion.result.len() as u32).to_be_bytes());
    event.extend_from_slice(&completion.result);
    event
}

fn completion_event_id(completion: &ActivityCompletion) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&completion.run_id);
    hasher.update(&completion.activity_id);
    hasher.update(&completion.completion_token);
    hasher.update(if completion.failed {
        b"failed"
    } else {
        b"completed"
    });
    *hasher.finalize().as_bytes()
}

fn retry_delay_ms(attempt: u32) -> i64 {
    (100_i64 << attempt.min(10)).min(60_000)
}

fn exact<const N: usize>(value: Vec<u8>, field: &'static str) -> Result<[u8; N]> {
    value.try_into().map_err(|_| Error::Command(field))
}

fn optional_exact<const N: usize>(
    value: Option<Vec<u8>>,
    field: &'static str,
) -> Result<Option<[u8; N]>> {
    value.map(|value| exact(value, field)).transpose()
}

fn validate_now(now_ms: i64) -> Result<()> {
    if now_ms < 0 {
        return Err(Error::Command("activity time must be non-negative"));
    }
    Ok(())
}
