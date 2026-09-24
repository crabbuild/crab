//! Claiming, validating, acking, retrying, and extending effect leases.
//!
//! Every transition here is a CAS on one attempt: a claim reserves a bounded
//! lease, an ack settles it, and a retry or extension only moves forward, so a
//! duplicate delivery can never double-apply an effect.

use super::*;

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
