use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::{Error, NamespaceId, Result};

mod api;

pub use api::{
    QueueClaimCommand, QueueClaimRequest, QueueLeaseCommand, QueueLeaseRequest, QueueModule,
    QueueNamespace, QueueSendCommand, QueueValidateClaimQuery, QueueValidateRequest,
    register_queue,
};

const QUEUE_SCHEMA: &str = include_str!("migrations/queue.sql");
const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_CLAIM_BYTES: usize = 512 * 1024;
const MAX_CLAIM_ITEMS: usize = 32;
const MAX_ATTEMPTS: u32 = 20;
const MAX_RECLAIM_ITEMS: usize = 128;
const MIN_LEASE_MS: u32 = 5_000;
const MAX_LEASE_MS: u32 = 300_000;
const MAX_RETRY_DELAY_MS: u32 = 3_600_000;
const MAX_AVAILABLE_DELAY_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const DELIVERY_MARGIN_MS: i64 = 1_000;

/// Source of unpredictable queue lease tokens.
pub trait QueueTokenSource {
    fn next_token(&mut self) -> Result<[u8; 16]>;
}

/// Cryptographically seeded process-local queue token source.
pub struct SystemQueueTokens;

impl QueueTokenSource for SystemQueueTokens {
    fn next_token(&mut self) -> Result<[u8; 16]> {
        let mut token = [0; 16];
        rand::rng().fill_bytes(&mut token);
        Ok(token)
    }
}

/// Durable queue lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueState {
    Ready,
    Leased,
    Acked,
    Dead,
}

impl QueueState {
    fn encode(self) -> i64 {
        match self {
            Self::Ready => 0,
            Self::Leased => 1,
            Self::Acked => 2,
            Self::Dead => 3,
        }
    }

    fn decode(value: i64) -> Result<Self> {
        match value {
            0 => Ok(Self::Ready),
            1 => Ok(Self::Leased),
            2 => Ok(Self::Acked),
            3 => Ok(Self::Dead),
            _ => Err(Error::Command("invalid stored queue state")),
        }
    }
}

/// Identity and scheduling input for an idempotent queue send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSendRequest {
    pub producer_id: [u8; 16],
    pub payload: Vec<u8>,
    pub available_at_ms: i64,
}

/// Result of producer-level queue deduplication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueSendOutcome {
    Sent { message_id: [u8; 16] },
    ProducerConflict,
}

/// A published queue delivery lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueMessage {
    pub message_id: [u8; 16],
    pub payload: Vec<u8>,
    pub token: [u8; 16],
    pub attempt: u32,
    pub lease_until_ms: i64,
}

/// Conditional mutation of one current queue lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueLeaseAction {
    Ack,
    Retry { delay_ms: u32 },
    Extend { extension_ms: u32 },
}

/// Business outcome of a queue lease mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueLeaseOutcome {
    Applied {
        state: QueueState,
        lease_until_ms: Option<i64>,
    },
    LeaseLost,
}

/// Installs the exact version-one Queue schema inside bootstrap or migration SQL.
pub fn install_queue_schema(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(QUEUE_SCHEMA)?;
    Ok(())
}

/// Inserts or deduplicates one producer send inside a runtime command.
pub fn queue_send(
    transaction: &Transaction<'_>,
    namespace: NamespaceId,
    now_ms: i64,
    request: &QueueSendRequest,
) -> Result<QueueSendOutcome> {
    validate_now(now_ms)?;
    if request.payload.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::Command("queue payload exceeds 256 KiB"));
    }
    let latest = now_ms
        .checked_add(MAX_AVAILABLE_DELAY_MS)
        .ok_or(Error::Command("queue available time overflow"))?;
    if request.available_at_ms < now_ms || request.available_at_ms > latest {
        return Err(Error::Command(
            "queue available time outside seven-day window",
        ));
    }
    let digest = send_digest(&request.payload, request.available_at_ms);
    let existing = transaction
        .query_row(
            "SELECT payload_digest, message_id FROM queue_dedup WHERE producer_id = ?1",
            [request.producer_id.as_slice()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    if let Some((stored_digest, stored_id)) = existing {
        if stored_digest.as_slice() != digest {
            return Ok(QueueSendOutcome::ProducerConflict);
        }
        let message_id = stored_id
            .try_into()
            .map_err(|_| Error::Command("invalid stored queue message ID"))?;
        return Ok(QueueSendOutcome::Sent { message_id });
    }

    let message_id = message_id(namespace, request.producer_id);
    let expires_at_ms = now_ms
        .checked_add(RETENTION_MS)
        .ok_or(Error::Command("queue expiry overflow"))?;
    transaction.execute(
        "INSERT INTO queue_messages(message_id, payload, state, attempt, due_at_ms, expires_at_ms, token, lease_until_ms, result_code) VALUES (?1, ?2, 0, 0, ?3, ?4, NULL, NULL, NULL)",
        (
            message_id.as_slice(),
            request.payload.as_slice(),
            request.available_at_ms,
            expires_at_ms,
        ),
    )?;
    transaction.execute(
        "INSERT INTO queue_dedup(producer_id, payload_digest, message_id, retain_until_ms) VALUES (?1, ?2, ?3, ?4)",
        (
            request.producer_id.as_slice(),
            digest.as_slice(),
            message_id.as_slice(),
            expires_at_ms,
        ),
    )?;
    Ok(QueueSendOutcome::Sent { message_id })
}

/// Reclaims expired leases and claims a bounded ready batch.
pub fn queue_claim(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
    lease_ms: u32,
    tokens: &mut impl QueueTokenSource,
) -> Result<Vec<QueueMessage>> {
    validate_now(now_ms)?;
    if !(1..=MAX_CLAIM_ITEMS).contains(&limit) {
        return Err(Error::Command("queue claim limit must be in 1..=32"));
    }
    if !(MIN_LEASE_MS..=MAX_LEASE_MS).contains(&lease_ms) {
        return Err(Error::Command("queue lease must be in 5..=300 seconds"));
    }
    queue_reclaim_expired_bounded(transaction, now_ms, MAX_RECLAIM_ITEMS)?;

    let mut statement = transaction.prepare(
        "SELECT message_id, payload, attempt, expires_at_ms FROM queue_messages INDEXED BY queue_ready WHERE state = 0 AND due_at_ms <= ?1 AND expires_at_ms > ?1 AND attempt < ?2 ORDER BY due_at_ms, message_id LIMIT ?3",
    )?;
    let rows = statement.query_map(
        (now_ms, i64::from(MAX_ATTEMPTS), (limit + 1) as i64),
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    let mut candidates = Vec::with_capacity(limit);
    let mut payload_bytes = 0_usize;
    for row in rows {
        if candidates.len() == limit {
            break;
        }
        let (message_id, payload, attempt, expires_at_ms) = row?;
        let next_bytes = payload_bytes
            .checked_add(payload.len())
            .ok_or(Error::Command("queue claim byte count overflow"))?;
        if next_bytes > MAX_CLAIM_BYTES {
            break;
        }
        let message_id: [u8; 16] = message_id
            .try_into()
            .map_err(|_| Error::Command("invalid stored queue message ID"))?;
        if payload.len() > MAX_PAYLOAD_BYTES || attempt < 0 {
            return Err(Error::Command("invalid stored queue message"));
        }
        let attempt =
            u32::try_from(attempt).map_err(|_| Error::Command("invalid stored queue attempt"))?;
        payload_bytes = next_bytes;
        candidates.push((message_id, payload, attempt, expires_at_ms));
    }
    drop(statement);

    let requested_deadline = now_ms
        .checked_add(i64::from(lease_ms))
        .ok_or(Error::Command("queue lease deadline overflow"))?;
    let mut claimed = Vec::with_capacity(candidates.len());
    for (message_id, payload, attempt, expires_at_ms) in candidates {
        let token = tokens.next_token()?;
        let lease_until_ms = requested_deadline.min(expires_at_ms);
        let changed = transaction.execute(
            "UPDATE queue_messages SET state = 1, attempt = attempt + 1, token = ?1, lease_until_ms = ?2 WHERE message_id = ?3 AND state = 0 AND due_at_ms <= ?4 AND expires_at_ms > ?4 AND attempt < ?5",
            (
                token.as_slice(),
                lease_until_ms,
                message_id.as_slice(),
                now_ms,
                i64::from(MAX_ATTEMPTS),
            ),
        )?;
        if changed != 1 {
            return Err(Error::Command("queue claim lost selected ready row"));
        }
        claimed.push(QueueMessage {
            message_id,
            payload,
            token,
            attempt: attempt + 1,
            lease_until_ms,
        });
    }
    Ok(claimed)
}

/// Rechecks published claim tokens immediately before task emission.
pub fn queue_validate_claim(
    connection: &Connection,
    now_ms: i64,
    claimed: &[QueueMessage],
) -> Result<bool> {
    validate_now(now_ms)?;
    for message in claimed {
        let state = connection
            .query_row(
                "SELECT state, attempt, token, lease_until_ms FROM queue_messages WHERE message_id = ?1",
                [message.message_id.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((state, attempt, token, lease_until_ms)) = state else {
            return Ok(false);
        };
        let margin = now_ms
            .checked_add(DELIVERY_MARGIN_MS)
            .ok_or(Error::Command("queue delivery margin overflow"))?;
        if QueueState::decode(state)? != QueueState::Leased
            || attempt != i64::from(message.attempt)
            || token.as_deref() != Some(message.token.as_slice())
            || lease_until_ms != Some(message.lease_until_ms)
            || message.lease_until_ms < margin
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Applies an ack, retry or extension only to the exact live lease token.
pub fn queue_apply_lease(
    transaction: &Transaction<'_>,
    now_ms: i64,
    message_id: [u8; 16],
    token: [u8; 16],
    action: QueueLeaseAction,
) -> Result<QueueLeaseOutcome> {
    validate_now(now_ms)?;
    let current = transaction
        .query_row(
            "SELECT state, attempt, token, lease_until_ms, expires_at_ms FROM queue_messages WHERE message_id = ?1",
            [message_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((state, attempt, stored_token, lease_until_ms, expires_at_ms)) = current else {
        return Ok(QueueLeaseOutcome::LeaseLost);
    };
    let live = QueueState::decode(state)? == QueueState::Leased
        && stored_token.as_deref() == Some(token.as_slice())
        && lease_until_ms.is_some_and(|deadline| deadline > now_ms);
    if !live {
        return Ok(QueueLeaseOutcome::LeaseLost);
    }
    let prior_deadline = lease_until_ms.ok_or(Error::Command("leased queue row lacks deadline"))?;
    let (next_state, next_deadline, due_at_ms) = match action {
        QueueLeaseAction::Ack => (QueueState::Acked, None, None),
        QueueLeaseAction::Retry { delay_ms } => {
            if delay_ms > MAX_RETRY_DELAY_MS {
                return Err(Error::Command("queue retry delay exceeds one hour"));
            }
            let due_at_ms = now_ms
                .checked_add(i64::from(delay_ms))
                .ok_or(Error::Command("queue retry time overflow"))?;
            if attempt >= i64::from(MAX_ATTEMPTS) || due_at_ms >= expires_at_ms {
                (QueueState::Dead, None, None)
            } else {
                (QueueState::Ready, None, Some(due_at_ms))
            }
        }
        QueueLeaseAction::Extend { extension_ms } => {
            if !(MIN_LEASE_MS..=MAX_LEASE_MS).contains(&extension_ms) {
                return Err(Error::Command("queue extension must be in 5..=300 seconds"));
            }
            let requested = now_ms
                .checked_add(i64::from(extension_ms))
                .ok_or(Error::Command("queue extension overflow"))?;
            let deadline = prior_deadline.max(requested.min(expires_at_ms));
            (QueueState::Leased, Some(deadline), None)
        }
    };
    let changed = match next_state {
        QueueState::Leased => transaction.execute(
            "UPDATE queue_messages SET lease_until_ms = ?1 WHERE message_id = ?2 AND state = 1 AND token = ?3 AND lease_until_ms > ?4",
            (next_deadline, message_id.as_slice(), token.as_slice(), now_ms),
        )?,
        QueueState::Ready => transaction.execute(
            "UPDATE queue_messages SET state = 0, due_at_ms = ?1, token = NULL, lease_until_ms = NULL WHERE message_id = ?2 AND state = 1 AND token = ?3 AND lease_until_ms > ?4",
            (due_at_ms, message_id.as_slice(), token.as_slice(), now_ms),
        )?,
        QueueState::Acked | QueueState::Dead => transaction.execute(
            "UPDATE queue_messages SET state = ?1, token = NULL, lease_until_ms = NULL WHERE message_id = ?2 AND state = 1 AND token = ?3 AND lease_until_ms > ?4",
            (
                next_state.encode(),
                message_id.as_slice(),
                token.as_slice(),
                now_ms,
            ),
        )?,
    };
    if changed != 1 {
        return Err(Error::Command("queue lease changed after validation"));
    }
    Ok(QueueLeaseOutcome::Applied {
        state: next_state,
        lease_until_ms: next_deadline,
    })
}

/// Deletes bounded expired dedup and terminal message rows.
pub fn queue_cleanup_expired(transaction: &Transaction<'_>, now_ms: i64) -> Result<usize> {
    queue_cleanup_expired_bounded(transaction, now_ms, MAX_RECLAIM_ITEMS)
}

pub(crate) fn queue_cleanup_expired_bounded(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
) -> Result<usize> {
    validate_now(now_ms)?;
    validate_maintenance_limit(limit)?;
    if limit == 0 {
        return Ok(0);
    }
    let dedup = transaction.execute(
        "DELETE FROM queue_dedup WHERE producer_id IN (SELECT producer_id FROM queue_dedup INDEXED BY queue_dedup_expiry WHERE retain_until_ms <= ?1 ORDER BY retain_until_ms, producer_id LIMIT ?2)",
        (now_ms, limit as i64),
    )?;
    let remaining = limit.saturating_sub(dedup);
    if remaining == 0 {
        return Ok(dedup);
    }
    let messages = transaction.execute(
        "DELETE FROM queue_messages WHERE message_id IN (SELECT message_id FROM queue_messages INDEXED BY queue_retention WHERE state IN (2, 3) AND expires_at_ms <= ?1 ORDER BY expires_at_ms, message_id LIMIT ?2)",
        (now_ms, remaining as i64),
    )?;
    dedup
        .checked_add(messages)
        .ok_or(Error::Command("queue cleanup count overflow"))
}

pub(crate) fn queue_reclaim_expired_bounded(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
) -> Result<usize> {
    validate_now(now_ms)?;
    validate_maintenance_limit(limit)?;
    if limit == 0 {
        return Ok(0);
    }
    let mut statement = transaction.prepare(
        "SELECT message_id, attempt, expires_at_ms FROM queue_messages INDEXED BY queue_leases WHERE state = 1 AND lease_until_ms <= ?1 ORDER BY lease_until_ms, message_id LIMIT ?2",
    )?;
    let rows = statement.query_map((now_ms, limit as i64), |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut expired = Vec::new();
    for row in rows {
        expired.push(row?);
    }
    drop(statement);
    let count = expired.len();
    for (message_id, attempt, expires_at_ms) in expired {
        let next_state = if attempt >= i64::from(MAX_ATTEMPTS) || expires_at_ms <= now_ms {
            QueueState::Dead
        } else {
            QueueState::Ready
        };
        let changed = transaction.execute(
            "UPDATE queue_messages SET state = ?1, due_at_ms = CASE WHEN ?1 = 0 THEN ?2 ELSE due_at_ms END, token = NULL, lease_until_ms = NULL WHERE message_id = ?3 AND state = 1 AND lease_until_ms <= ?2",
            (next_state.encode(), now_ms, message_id.as_slice()),
        )?;
        if changed != 1 {
            return Err(Error::Command("queue reclaim lost selected lease"));
        }
    }
    Ok(count)
}

pub(crate) fn queue_expire_ready_bounded(
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
        "UPDATE queue_messages SET state = 3 WHERE message_id IN (SELECT message_id FROM queue_messages INDEXED BY queue_ready WHERE state = 0 AND (expires_at_ms <= ?1 OR attempt >= ?2) ORDER BY due_at_ms, message_id LIMIT ?3)",
        (now_ms, i64::from(MAX_ATTEMPTS), limit as i64),
    )?)
}

fn validate_maintenance_limit(limit: usize) -> Result<()> {
    if limit > MAX_RECLAIM_ITEMS {
        return Err(Error::Command("queue maintenance limit exceeds 128"));
    }
    Ok(())
}

fn message_id(namespace: NamespaceId, producer_id: [u8; 16]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.queue-message.v1\0");
    hasher.update(namespace.as_bytes());
    hasher.update(&producer_id);
    let mut id = [0; 16];
    id.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    id
}

fn send_digest(payload: &[u8], available_at_ms: i64) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.queue-send.v1\0");
    hasher.update(&(payload.len() as u32).to_be_bytes());
    hasher.update(payload);
    hasher.update(&available_at_ms.to_be_bytes());
    *hasher.finalize().as_bytes()
}

fn validate_now(now_ms: i64) -> Result<()> {
    if now_ms < 0 {
        return Err(Error::Command("negative queue logical time"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_queue_migration_matches_normative_contract() {
        assert_eq!(
            QUEUE_SCHEMA,
            include_str!("../../../crab/docs/architecture/platform/contracts/queue.sql")
        );
    }

    #[test]
    fn message_identity_binds_namespace_and_producer() {
        assert_ne!(
            message_id(NamespaceId::from_bytes([1; 16]), [2; 16]),
            message_id(NamespaceId::from_bytes([3; 16]), [2; 16])
        );
    }
}
