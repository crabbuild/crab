use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::{
    CellId, Digest, Error, HandlerOutcome, IncarnationId, Resolution, Result, StoredOutcome,
};

mod api;
mod supervisor;

pub use api::{
    EffectAckRequest, EffectClaimCommand, EffectClaimRequest, EffectLeaseCommand,
    EffectLeaseRequest, EffectModule, EffectSource, EffectValidateClaimQuery,
    EffectValidateRequest, register_effect_delivery,
};
pub use supervisor::{EffectRunOutcome, EffectSupervisor, EffectSupervisorError};

const MAX_EFFECTS_PER_COMMAND: usize = 128;
const MAX_EFFECT_BYTES: usize = 1 << 20;
const EFFECT_CLAIM_FIXED_BYTES: usize = 160;
const EFFECT_CLAIM_LIST_BYTES: usize = 4;
const EFFECT_ACK_FIXED_BYTES: usize = 73;
// Full typed claim outputs and acknowledgement inputs share the registry's 1 MiB ceiling.
pub(crate) const MAX_EFFECT_OPERATION_BYTES: usize =
    MAX_EFFECT_BYTES - EFFECT_CLAIM_LIST_BYTES - EFFECT_CLAIM_FIXED_BYTES;
pub(crate) const MAX_EFFECT_RESULT_BYTES: usize = MAX_EFFECT_BYTES - EFFECT_ACK_FIXED_BYTES;
const MAX_CLAIM_ITEMS: usize = 32;
const MAX_RECLAIM_ITEMS: usize = 128;
const MAX_ATTEMPTS: u32 = 20;
const MIN_LEASE_MS: u32 = 5_000;
const MAX_LEASE_MS: u32 = 300_000;
const DELIVERY_MARGIN_MS: i64 = 1_000;
const EFFECT_LIFETIME_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const INBOX_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

/// Durable source-side effect state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectState {
    Ready,
    Leased,
    Delivered,
    Failed,
}

/// Business outcome of a source-side effect lease mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectLeaseOutcome {
    Delivered,
    Retrying { due_at_ms: i64 },
    Failed,
    Extended { lease_until_ms: i64 },
    LeaseLost,
}

impl EffectState {
    fn encode(self) -> i64 {
        match self {
            Self::Ready => 0,
            Self::Leased => 1,
            Self::Delivered => 2,
            Self::Failed => 3,
        }
    }
}

/// One pre-resolved destination operation emitted by a source command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectIntent {
    pub destination: CellId,
    pub operation: Vec<u8>,
    pub expires_at_ms: i64,
}

/// Source of unpredictable effect lease tokens.
pub trait EffectTokenSource {
    fn next_token(&mut self) -> Result<[u8; 16]>;
}

/// Cryptographically seeded process-local effect token source.
pub struct SystemEffectTokens;

impl EffectTokenSource for SystemEffectTokens {
    fn next_token(&mut self) -> Result<[u8; 16]> {
        loop {
            let mut token = [0; 16];
            rand::rng().fill_bytes(&mut token);
            if token != [0; 16] {
                return Ok(token);
            }
        }
    }
}

/// One published source-side delivery lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectClaim {
    pub effect_id: [u8; 32],
    pub destination: CellId,
    pub operation: Vec<u8>,
    pub operation_digest: Digest,
    pub attempt: u32,
    pub token: [u8; 16],
    pub lease_until_ms: i64,
    pub expires_at_ms: i64,
    pub created_sequence: u64,
}

/// Minimal identity needed to mutate one exact source lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectLease {
    pub effect_id: [u8; 32],
    pub attempt: u32,
    pub token: [u8; 16],
    pub expires_at_ms: i64,
}

impl From<&EffectClaim> for EffectLease {
    fn from(claim: &EffectClaim) -> Self {
        Self {
            effect_id: claim.effect_id,
            attempt: claim.attempt,
            token: claim.token,
            expires_at_ms: claim.expires_at_ms,
        }
    }
}

/// Exact target-side identity and expiry carried by a private delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboxDelivery {
    pub effect_id: [u8; 32],
    pub operation_digest: Digest,
    pub expires_at_ms: i64,
}

/// Durable target-side result of an effect delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxApplyOutcome {
    Success {
        result: Vec<u8>,
        commit_sequence: u64,
        duplicate: bool,
    },
    Rejected {
        result: Vec<u8>,
        commit_sequence: u64,
        duplicate: bool,
    },
    Conflict,
    Expired,
}

/// Inserts one immutable effect intention inside its source command.
pub fn effect_insert(
    transaction: &Transaction<'_>,
    cell: CellId,
    incarnation: IncarnationId,
    sequence: u64,
    ordinal: u32,
    now_ms: i64,
    intent: &EffectIntent,
) -> Result<[u8; 32]> {
    validate_now(now_ms)?;
    if sequence == 0
        || sequence > i64::MAX as u64
        || ordinal as usize >= MAX_EFFECTS_PER_COMMAND
        || intent.operation.is_empty()
        || intent.operation.len() > MAX_EFFECT_OPERATION_BYTES
        || intent.expires_at_ms <= now_ms
        || intent.expires_at_ms > now_ms.saturating_add(EFFECT_LIFETIME_MS)
    {
        return Err(Error::Command("invalid effect intention"));
    }
    let id = effect_id(cell, incarnation, sequence, ordinal);
    let existing = transaction
        .query_row(
            "SELECT destination, operation, expires_at_ms, created_sequence FROM sys_effects WHERE effect_id = ?1",
            [id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((destination, operation, expires_at_ms, created_sequence)) = existing {
        if destination.as_slice() == intent.destination.as_bytes()
            && operation == intent.operation
            && expires_at_ms == intent.expires_at_ms
            && created_sequence == sequence as i64
        {
            return Ok(id);
        }
        return Err(Error::Command(
            "effect ordinal was reused for different bytes",
        ));
    }
    let (count, bytes): (i64, i64) = transaction.query_row(
        "SELECT count(*), COALESCE(sum(length(operation)), 0) FROM sys_effects WHERE created_sequence = ?1",
        [sequence as i64],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let prospective = usize::try_from(bytes)
        .ok()
        .and_then(|bytes| bytes.checked_add(intent.operation.len()))
        .ok_or(Error::Command("effect byte count overflow"))?;
    if count < 0 || count as usize >= MAX_EFFECTS_PER_COMMAND || prospective > MAX_EFFECT_BYTES {
        return Err(Error::Command("command effects exceed limits"));
    }
    transaction.execute(
        "INSERT INTO sys_effects(effect_id, destination, operation, state, attempt, due_at_ms, expires_at_ms, token, lease_until_ms, created_sequence, result) VALUES (?1, ?2, ?3, 0, 0, ?4, ?5, NULL, NULL, ?6, NULL)",
        (
            id.as_slice(),
            intent.destination.as_bytes().as_slice(),
            intent.operation.as_slice(),
            now_ms,
            intent.expires_at_ms,
            sequence as i64,
        ),
    )?;
    Ok(id)
}

/// Reclaims expired leases and claims bounded due effects for private delivery.
pub fn effect_claim(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
    lease_ms: u32,
    tokens: &mut impl EffectTokenSource,
) -> Result<Vec<EffectClaim>> {
    validate_now(now_ms)?;
    if !(1..=MAX_CLAIM_ITEMS).contains(&limit) {
        return Err(Error::Command("effect claim limit must be in 1..=32"));
    }
    if !(MIN_LEASE_MS..=MAX_LEASE_MS).contains(&lease_ms) {
        return Err(Error::Command("effect lease must be in 5..=300 seconds"));
    }
    effect_reclaim_expired_bounded(transaction, now_ms, MAX_RECLAIM_ITEMS)?;
    let candidates = {
        let mut statement = transaction.prepare(
            "SELECT effect_id, destination, operation, attempt, expires_at_ms, created_sequence FROM sys_effects INDEXED BY sys_effects_due WHERE state = 0 AND due_at_ms <= ?1 AND expires_at_ms > ?1 AND attempt < ?2 ORDER BY due_at_ms, effect_id LIMIT ?3",
        )?;
        statement
            .query_map(
                (now_ms, i64::from(MAX_ATTEMPTS), (limit + 1) as i64),
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let requested_deadline = now_ms
        .checked_add(i64::from(lease_ms))
        .ok_or(Error::Command("effect lease deadline overflow"))?;
    let mut bytes = EFFECT_CLAIM_LIST_BYTES;
    let mut claimed = Vec::with_capacity(limit);
    for (effect_id, destination, operation, attempt, expires_at_ms, created_sequence) in candidates
    {
        if claimed.len() == limit {
            break;
        }
        let effect_id = exact::<32>(effect_id, "invalid stored effect ID")?;
        let destination = CellId::from_bytes(exact::<32>(
            destination,
            "invalid stored effect destination",
        )?);
        if operation.is_empty()
            || operation.len() > MAX_EFFECT_OPERATION_BYTES
            || attempt < 0
            || created_sequence <= 0
        {
            return Err(Error::Command("invalid stored effect"));
        }
        let prospective = bytes
            .checked_add(EFFECT_CLAIM_FIXED_BYTES)
            .and_then(|bytes| bytes.checked_add(operation.len()))
            .ok_or(Error::Command("effect claim byte count overflow"))?;
        if prospective > MAX_EFFECT_BYTES {
            break;
        }
        let attempt =
            u32::try_from(attempt).map_err(|_| Error::Command("invalid stored effect attempt"))?;
        let created_sequence = u64::try_from(created_sequence)
            .map_err(|_| Error::Command("invalid stored effect sequence"))?;
        let token = tokens.next_token()?;
        let lease_until_ms = requested_deadline.min(expires_at_ms);
        if transaction.execute(
            "UPDATE sys_effects SET state = 1, attempt = attempt + 1, token = ?1, lease_until_ms = ?2 WHERE effect_id = ?3 AND state = 0 AND due_at_ms <= ?4 AND expires_at_ms > ?4 AND attempt = ?5",
            (
                token.as_slice(),
                lease_until_ms,
                effect_id.as_slice(),
                now_ms,
                i64::from(attempt),
            ),
        )? != 1
        {
            return Err(Error::Command("effect claim lost selected row"));
        }
        bytes = prospective;
        claimed.push(EffectClaim {
            effect_id,
            destination,
            operation_digest: effect_operation_digest(destination, effect_id, &operation),
            operation,
            attempt: attempt + 1,
            token,
            lease_until_ms,
            expires_at_ms,
            created_sequence,
        });
    }
    Ok(claimed)
}

/// Verifies that every effect lease is still published and safe to emit.
pub fn effect_validate_claim(
    connection: &Connection,
    now_ms: i64,
    claimed: &[EffectClaim],
) -> Result<bool> {
    validate_now(now_ms)?;
    if claimed.len() > MAX_CLAIM_ITEMS {
        return Err(Error::Command("effect claim exceeds 32 items"));
    }
    let minimum = now_ms
        .checked_add(DELIVERY_MARGIN_MS)
        .ok_or(Error::Command("effect delivery margin overflow"))?;
    for effect in claimed {
        if effect.operation.is_empty()
            || effect.operation.len() > MAX_EFFECT_OPERATION_BYTES
            || effect.lease_until_ms < minimum
            || effect.operation_digest
                != effect_operation_digest(effect.destination, effect.effect_id, &effect.operation)
        {
            return Ok(false);
        }
        let live = connection
            .query_row(
                "SELECT 1 FROM sys_effects WHERE effect_id = ?1 AND destination = ?2 AND operation = ?3 AND state = 1 AND attempt = ?4 AND token = ?5 AND lease_until_ms = ?6 AND lease_until_ms >= ?7 AND expires_at_ms = ?8 AND created_sequence = ?9",
                (
                    effect.effect_id.as_slice(),
                    effect.destination.as_bytes().as_slice(),
                    effect.operation.as_slice(),
                    i64::from(effect.attempt),
                    effect.token.as_slice(),
                    effect.lease_until_ms,
                    minimum,
                    effect.expires_at_ms,
                    effect.created_sequence as i64,
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

/// Marks one exact published effect lease delivered after target publication.
pub fn effect_ack_delivered(
    transaction: &Transaction<'_>,
    now_ms: i64,
    claim: &EffectClaim,
    result: &[u8],
) -> Result<EffectLeaseOutcome> {
    effect_ack_lease(transaction, now_ms, EffectLease::from(claim), result)
}

pub(crate) fn effect_ack_lease(
    transaction: &Transaction<'_>,
    now_ms: i64,
    lease: EffectLease,
    result: &[u8],
) -> Result<EffectLeaseOutcome> {
    validate_now(now_ms)?;
    if result.len() > MAX_EFFECT_RESULT_BYTES {
        return Err(Error::Command(
            "effect result exceeds acknowledgement wire limit",
        ));
    }
    if apply_lease(
        transaction,
        now_ms,
        &lease,
        EffectState::Delivered,
        None,
        Some(result),
    )? {
        Ok(EffectLeaseOutcome::Delivered)
    } else {
        Ok(EffectLeaseOutcome::LeaseLost)
    }
}

/// Releases one failed delivery for bounded retry or terminal expiry.
pub fn effect_retry(
    transaction: &Transaction<'_>,
    now_ms: i64,
    claim: &EffectClaim,
) -> Result<EffectLeaseOutcome> {
    effect_retry_lease(transaction, now_ms, EffectLease::from(claim))
}

pub(crate) fn effect_retry_lease(
    transaction: &Transaction<'_>,
    now_ms: i64,
    lease: EffectLease,
) -> Result<EffectLeaseOutcome> {
    validate_now(now_ms)?;
    let delay = retry_delay_ms(lease.attempt);
    let due_at_ms = now_ms.saturating_add(delay);
    let state = if lease.attempt >= MAX_ATTEMPTS || due_at_ms >= lease.expires_at_ms {
        EffectState::Failed
    } else {
        EffectState::Ready
    };
    if !apply_lease(
        transaction,
        now_ms,
        &lease,
        state,
        (state == EffectState::Ready).then_some(due_at_ms),
        None,
    )? {
        return Ok(EffectLeaseOutcome::LeaseLost);
    }
    match state {
        EffectState::Ready => Ok(EffectLeaseOutcome::Retrying { due_at_ms }),
        EffectState::Failed => Ok(EffectLeaseOutcome::Failed),
        _ => Err(Error::Command("invalid effect retry state")),
    }
}

/// Extends one current effect lease without shortening it.
pub fn effect_extend(
    transaction: &Transaction<'_>,
    now_ms: i64,
    claim: &EffectClaim,
    extension_ms: u32,
) -> Result<EffectLeaseOutcome> {
    validate_now(now_ms)?;
    if !(MIN_LEASE_MS..=MAX_LEASE_MS).contains(&extension_ms) {
        return Err(Error::Command(
            "effect extension must be in 5..=300 seconds",
        ));
    }
    let current = transaction
        .query_row(
            "SELECT lease_until_ms, expires_at_ms FROM sys_effects WHERE effect_id = ?1 AND state = 1 AND attempt = ?2 AND token = ?3 AND lease_until_ms > ?4",
            (
                claim.effect_id.as_slice(),
                i64::from(claim.attempt),
                claim.token.as_slice(),
                now_ms,
            ),
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((current, expires_at_ms)) = current else {
        return Ok(EffectLeaseOutcome::LeaseLost);
    };
    let requested = now_ms
        .checked_add(i64::from(extension_ms))
        .ok_or(Error::Command("effect extension deadline overflow"))?;
    let lease_until_ms = current.max(requested.min(expires_at_ms));
    if transaction.execute(
        "UPDATE sys_effects SET lease_until_ms = ?1 WHERE effect_id = ?2 AND state = 1 AND attempt = ?3 AND token = ?4 AND lease_until_ms = ?5",
        (
            lease_until_ms,
            claim.effect_id.as_slice(),
            i64::from(claim.attempt),
            claim.token.as_slice(),
            current,
        ),
    )? != 1
    {
        return Err(Error::Command("effect lease changed during extension"));
    }
    Ok(EffectLeaseOutcome::Extended { lease_until_ms })
}

/// Applies or replays one private effect in the destination inbox.
pub fn inbox_apply(
    transaction: &Transaction<'_>,
    now_ms: i64,
    delivery: InboxDelivery,
    max_result_bytes: usize,
    handler: impl FnOnce(&Transaction<'_>) -> Result<HandlerOutcome>,
) -> Result<InboxApplyOutcome> {
    validate_now(now_ms)?;
    if delivery.expires_at_ms <= now_ms {
        return Ok(InboxApplyOutcome::Expired);
    }
    if max_result_bytes > MAX_EFFECT_BYTES {
        return Err(Error::Command("effect result limit exceeds 1 MiB"));
    }
    let existing = transaction
        .query_row(
            "SELECT operation_digest, outcome, result, commit_sequence, expires_at_ms FROM sys_inbox WHERE effect_id = ?1",
            [delivery.effect_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    if let Some((digest, outcome, result, sequence, expires_at_ms)) = existing {
        if digest.as_slice() != delivery.operation_digest.as_bytes()
            || expires_at_ms != delivery.expires_at_ms
        {
            return Ok(InboxApplyOutcome::Conflict);
        }
        return inbox_outcome(outcome, result, sequence, max_result_bytes, true);
    }

    let sequence: i64 = transaction.query_row(
        "SELECT commit_sequence + 1 FROM sys_meta WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if sequence <= 0 {
        return Err(Error::Command("effect destination sequence overflow"));
    }
    transaction.execute_batch("SAVEPOINT effect_application")?;
    let decision = handler(transaction)?;
    let (outcome, result) = match decision {
        HandlerOutcome::Success(result) => {
            transaction.execute_batch("RELEASE effect_application")?;
            (1, result)
        }
        HandlerOutcome::Rejected(result) => {
            transaction
                .execute_batch("ROLLBACK TO effect_application; RELEASE effect_application")?;
            (2, result)
        }
    };
    if result.len() > max_result_bytes {
        return Err(Error::Command("effect handler result exceeds limit"));
    }
    let retain_until_ms = delivery
        .expires_at_ms
        .checked_add(INBOX_RETENTION_MS)
        .ok_or(Error::Command("effect inbox retention overflow"))?;
    transaction.execute(
        "INSERT INTO sys_inbox(effect_id, operation_digest, outcome, result, commit_sequence, expires_at_ms, retain_until_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        (
            delivery.effect_id.as_slice(),
            delivery.operation_digest.as_bytes().as_slice(),
            outcome,
            result.as_slice(),
            sequence,
            delivery.expires_at_ms,
            retain_until_ms,
        ),
    )?;
    inbox_outcome(outcome, result, sequence, max_result_bytes, false)
}

/// Resolves one destination inbox identity without executing its handler.
pub fn inbox_resolve(
    connection: &Connection,
    now_ms: i64,
    delivery: InboxDelivery,
    max_result_bytes: usize,
) -> Result<Resolution> {
    validate_now(now_ms)?;
    if delivery.expires_at_ms <= now_ms {
        return Ok(Resolution::Expired);
    }
    if max_result_bytes > MAX_EFFECT_BYTES {
        return Err(Error::Command("effect result limit exceeds 1 MiB"));
    }
    let existing = connection
        .query_row(
            "SELECT operation_digest, outcome, result, commit_sequence, expires_at_ms FROM sys_inbox WHERE effect_id = ?1",
            [delivery.effect_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((digest, outcome, result, sequence, expires_at_ms)) = existing else {
        return Ok(Resolution::Absent);
    };
    if digest.as_slice() != delivery.operation_digest.as_bytes()
        || expires_at_ms != delivery.expires_at_ms
    {
        return Err(Error::RequestConflict);
    }
    Ok(Resolution::Committed(stored_inbox_outcome(
        outcome,
        result,
        sequence,
        max_result_bytes,
    )?))
}

/// Removes at most 128 terminal source effects after their delivery horizon.
pub fn effect_cleanup_terminal(transaction: &Transaction<'_>, now_ms: i64) -> Result<usize> {
    effect_cleanup_terminal_bounded(transaction, now_ms, MAX_RECLAIM_ITEMS)
}

pub(crate) fn effect_cleanup_terminal_bounded(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
) -> Result<usize> {
    validate_now(now_ms)?;
    validate_maintenance_limit(limit)?;
    if limit == 0 {
        return Ok(0);
    }
    let ids = select_ids(
        transaction,
        "SELECT effect_id FROM sys_effects WHERE state IN (2, 3) AND expires_at_ms <= ?1 ORDER BY expires_at_ms, effect_id LIMIT ?2",
        now_ms,
        limit,
    )?;
    for id in &ids {
        transaction.execute(
            "DELETE FROM sys_effects WHERE effect_id = ?1 AND state IN (2, 3) AND expires_at_ms <= ?2",
            (id.as_slice(), now_ms),
        )?;
    }
    Ok(ids.len())
}

/// Removes at most 128 inbox receipts after sender expiry plus seven days.
pub fn inbox_cleanup_expired(transaction: &Transaction<'_>, now_ms: i64) -> Result<usize> {
    inbox_cleanup_expired_bounded(transaction, now_ms, MAX_RECLAIM_ITEMS)
}

pub(crate) fn inbox_cleanup_expired_bounded(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
) -> Result<usize> {
    validate_now(now_ms)?;
    validate_maintenance_limit(limit)?;
    if limit == 0 {
        return Ok(0);
    }
    let ids = select_ids(
        transaction,
        "SELECT effect_id FROM sys_inbox WHERE retain_until_ms <= ?1 ORDER BY retain_until_ms, effect_id LIMIT ?2",
        now_ms,
        limit,
    )?;
    for id in &ids {
        transaction.execute(
            "DELETE FROM sys_inbox WHERE effect_id = ?1 AND retain_until_ms <= ?2",
            (id.as_slice(), now_ms),
        )?;
    }
    Ok(ids.len())
}

/// Computes the destination operation digest carried unchanged across retries.
#[must_use]
pub fn effect_operation_digest(
    destination: CellId,
    effect_id: [u8; 32],
    operation: &[u8],
) -> Digest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.effect-op.v1\0");
    hasher.update(destination.as_bytes());
    hasher.update(&effect_id);
    hasher.update(&(operation.len() as u32).to_be_bytes());
    hasher.update(operation);
    Digest::from_bytes(*hasher.finalize().as_bytes())
}

/// Derives the immutable identity for one source transaction effect ordinal.
#[must_use]
pub fn effect_id(
    cell: CellId,
    incarnation: IncarnationId,
    sequence: u64,
    ordinal: u32,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.effect.v1\0");
    hasher.update(cell.as_bytes());
    hasher.update(incarnation.as_bytes());
    hasher.update(&sequence.to_be_bytes());
    hasher.update(&ordinal.to_be_bytes());
    *hasher.finalize().as_bytes()
}

fn apply_lease(
    transaction: &Transaction<'_>,
    now_ms: i64,
    lease: &EffectLease,
    state: EffectState,
    due_at_ms: Option<i64>,
    result: Option<&[u8]>,
) -> Result<bool> {
    let due_at_ms = due_at_ms.unwrap_or(0);
    let changed = transaction.execute(
        "UPDATE sys_effects SET state = ?1, due_at_ms = CASE WHEN ?1 = 0 THEN ?2 ELSE due_at_ms END, token = NULL, lease_until_ms = NULL, result = ?3 WHERE effect_id = ?4 AND state = 1 AND attempt = ?5 AND token = ?6 AND lease_until_ms > ?7 AND expires_at_ms = ?8",
        (
            state.encode(),
            due_at_ms,
            result,
            lease.effect_id.as_slice(),
            i64::from(lease.attempt),
            lease.token.as_slice(),
            now_ms,
            lease.expires_at_ms,
        ),
    )?;
    Ok(changed == 1)
}

pub(crate) fn effect_reclaim_expired_bounded(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
) -> Result<usize> {
    validate_now(now_ms)?;
    validate_maintenance_limit(limit)?;
    if limit == 0 {
        return Ok(0);
    }
    let expired = {
        let mut statement = transaction.prepare(
            "SELECT effect_id, attempt, expires_at_ms FROM sys_effects INDEXED BY sys_effects_leases WHERE state = 1 AND lease_until_ms <= ?1 ORDER BY lease_until_ms, effect_id LIMIT ?2",
        )?;
        statement
            .query_map((now_ms, limit as i64), |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let count = expired.len();
    for (id, attempt, expires_at_ms) in expired {
        if attempt < 0 {
            return Err(Error::Command("invalid stored effect attempt"));
        }
        let attempt =
            u32::try_from(attempt).map_err(|_| Error::Command("invalid stored effect attempt"))?;
        let due_at_ms = now_ms.saturating_add(retry_delay_ms(attempt));
        let state = if attempt >= MAX_ATTEMPTS || due_at_ms >= expires_at_ms {
            EffectState::Failed
        } else {
            EffectState::Ready
        };
        transaction.execute(
            "UPDATE sys_effects SET state = ?1, due_at_ms = CASE WHEN ?1 = 0 THEN ?2 ELSE due_at_ms END, token = NULL, lease_until_ms = NULL WHERE effect_id = ?3 AND state = 1 AND lease_until_ms <= ?4",
            (state.encode(), due_at_ms, id, now_ms),
        )?;
    }
    Ok(count)
}

pub(crate) fn effect_expire_ready_bounded(
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
        "UPDATE sys_effects SET state = 3 WHERE effect_id IN (SELECT effect_id FROM sys_effects INDEXED BY sys_effects_due WHERE state = 0 AND (expires_at_ms <= ?1 OR attempt >= ?2) ORDER BY due_at_ms, effect_id LIMIT ?3)",
        (now_ms, i64::from(MAX_ATTEMPTS), limit as i64),
    )?)
}

fn inbox_outcome(
    outcome: i64,
    result: Vec<u8>,
    sequence: i64,
    max_result_bytes: usize,
    duplicate: bool,
) -> Result<InboxApplyOutcome> {
    match stored_inbox_outcome(outcome, result, sequence, max_result_bytes)? {
        StoredOutcome::Success {
            result,
            commit_sequence,
        } => Ok(InboxApplyOutcome::Success {
            result,
            commit_sequence,
            duplicate,
        }),
        StoredOutcome::Rejected {
            result,
            commit_sequence,
        } => Ok(InboxApplyOutcome::Rejected {
            result,
            commit_sequence,
            duplicate,
        }),
    }
}

fn stored_inbox_outcome(
    outcome: i64,
    result: Vec<u8>,
    sequence: i64,
    max_result_bytes: usize,
) -> Result<StoredOutcome> {
    if result.len() > max_result_bytes || sequence <= 0 {
        return Err(Error::Command("invalid stored inbox outcome"));
    }
    let commit_sequence =
        u64::try_from(sequence).map_err(|_| Error::Command("invalid stored inbox sequence"))?;
    match outcome {
        1 => Ok(StoredOutcome::Success {
            result,
            commit_sequence,
        }),
        2 => Ok(StoredOutcome::Rejected {
            result,
            commit_sequence,
        }),
        _ => Err(Error::Command("invalid stored inbox outcome")),
    }
}

fn select_ids(
    transaction: &Transaction<'_>,
    sql: &str,
    now_ms: i64,
    limit: usize,
) -> Result<Vec<[u8; 32]>> {
    let mut statement = transaction.prepare(sql)?;
    statement
        .query_map((now_ms, limit as i64), |row| row.get::<_, Vec<u8>>(0))?
        .map(|row| exact::<32>(row.map_err(Error::from)?, "invalid stored effect ID"))
        .collect()
}

fn validate_maintenance_limit(limit: usize) -> Result<()> {
    if limit > MAX_RECLAIM_ITEMS {
        return Err(Error::Command("effect maintenance limit exceeds 128"));
    }
    Ok(())
}

fn retry_delay_ms(attempt: u32) -> i64 {
    (100_i64 << attempt.min(10)).min(60_000)
}

fn exact<const N: usize>(value: Vec<u8>, message: &'static str) -> Result<[u8; N]> {
    value.try_into().map_err(|_| Error::Command(message))
}

fn validate_now(now_ms: i64) -> Result<()> {
    if now_ms < 0 {
        return Err(Error::Command("effect time must be non-negative"));
    }
    Ok(())
}
