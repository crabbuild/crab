use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::codec::{BoundedEncoder, WireValue};
use crate::identity::IncarnationId;
use crate::identity::{CellId, CellTarget, NamespaceId, partition_for_shard, shard_for_scope};
use crate::primitives::effects::EffectBatch;
use crate::primitives::effects::EffectCommandIntent;
use crate::{Error, Result};

mod api;

pub use api::{
    QueueClaimCommand, QueueClaimRequest, QueueControlCommand, QueueDeadLetterTarget,
    QueueInfoQuery, QueueInfoRequest, QueueLeaseCommand, QueueLeaseRequest, QueueModule,
    QueueNamespace, QueueSendCommand, QueueValidateClaimQuery, QueueValidateRequest,
    register_queue,
};

const QUEUE_SCHEMA: &str = include_str!("../migrations/queue.sql");
pub(crate) const QUEUE_TABLE: &str = "queue_messages";
const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
pub(crate) const QUEUE_SEND_MAX_INPUT_BYTES: u32 = MAX_PAYLOAD_BYTES as u32 + 32;
const MAX_CLAIM_BYTES: usize = 512 * 1024;
const MAX_CLAIM_ITEMS: usize = 32;
pub(crate) const MAX_ATTEMPTS: u32 = 20;
const MAX_RECLAIM_ITEMS: usize = 128;
const MIN_LEASE_MS: u32 = 5_000;
const MAX_LEASE_MS: u32 = 300_000;
const MAX_RETRY_DELAY_MS: u32 = 3_600_000;
const MAX_AVAILABLE_DELAY_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const DELIVERY_MARGIN_MS: i64 = 1_000;
const DEAD_LETTER_EFFECT_LIFETIME_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

pub(crate) struct QueueDeadLetterWriter<'a> {
    target: QueueDeadLetterTarget,
    effects: &'a mut EffectBatch,
}

impl<'a> QueueDeadLetterWriter<'a> {
    pub(crate) const fn new(target: QueueDeadLetterTarget, effects: &'a mut EffectBatch) -> Self {
        Self { target, effects }
    }

    fn insert(
        &mut self,
        transaction: &Transaction<'_>,
        message_id: [u8; 16],
        payload: &[u8],
        now_ms: i64,
    ) -> Result<[u8; 32]> {
        let source = self.effects.source_target();
        let producer_id = dead_letter_producer_id(
            self.target.namespace(),
            source.cell_id(),
            self.effects.source_incarnation(),
            message_id,
        );
        let shard = shard_for_scope(self.target.namespace(), &producer_id, self.target.shards())?;
        let target = CellTarget::new(
            source.tenant(),
            source.application(),
            self.target.namespace(),
            &partition_for_shard(shard),
        )?;
        let request = QueueSendRequest {
            producer_id,
            payload: payload.to_vec(),
            available_at_ms: now_ms,
        };
        let mut encoder = BoundedEncoder::new(QUEUE_SEND_MAX_INPUT_BYTES)?;
        request.encode(&mut encoder)?;
        let expires_at_ms = now_ms
            .checked_add(DEAD_LETTER_EFFECT_LIFETIME_MS)
            .ok_or(Error::Command("queue dead-letter effect expiry overflow"))?;
        self.effects.insert_command(
            transaction,
            &EffectCommandIntent {
                target,
                command_id: self.target.send_command_id(),
                codec_version: self.target.codec_version(),
                input: encoder.finish(),
                expires_at_ms,
            },
        )
    }
}

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

/// Administrative mutation applied to one queue shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueControlAction {
    Pause,
    Resume,
    Purge { limit: u32 },
    Redrive { limit: u32 },
}

/// Result of one queue control mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueControlOutcome {
    Paused { generation: u64 },
    Resumed { generation: u64 },
    Purged { messages: u32 },
    Redriven { messages: u32 },
}

/// Bounded queue shard state returned to operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueInfo {
    pub paused: bool,
    pub generation: u64,
    pub ready: u64,
    pub leased: u64,
    pub acked: u64,
    pub dead: u64,
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
    if request.available_at_ms < 0 || request.available_at_ms > latest {
        return Err(Error::Command(
            "queue available time outside seven-day window",
        ));
    }
    let due_at_ms = request.available_at_ms.max(now_ms);
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
            due_at_ms,
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
    queue_claim_with_dead_letter(transaction, now_ms, limit, lease_ms, tokens, None)
}

pub(crate) fn queue_claim_with_dead_letter(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
    lease_ms: u32,
    tokens: &mut impl QueueTokenSource,
    dead_letter: Option<&mut QueueDeadLetterWriter<'_>>,
) -> Result<Vec<QueueMessage>> {
    validate_now(now_ms)?;
    if !(1..=MAX_CLAIM_ITEMS).contains(&limit) {
        return Err(Error::Command("queue claim limit must be in 1..=32"));
    }
    if !(MIN_LEASE_MS..=MAX_LEASE_MS).contains(&lease_ms) {
        return Err(Error::Command("queue lease must be in 5..=300 seconds"));
    }
    if queue_paused(transaction)? {
        return Ok(Vec::new());
    }
    queue_reclaim_expired_bounded_with_dead_letter(
        transaction,
        now_ms,
        MAX_RECLAIM_ITEMS,
        dead_letter,
    )?;

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
    if queue_paused(connection)? {
        return Ok(false);
    }
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

/// Applies a bounded administrative action to one queue shard.
pub fn queue_control(
    transaction: &Transaction<'_>,
    now_ms: i64,
    action: QueueControlAction,
) -> Result<QueueControlOutcome> {
    validate_now(now_ms)?;
    match action {
        QueueControlAction::Pause => {
            let generation = set_queue_paused(transaction, now_ms, true)?;
            Ok(QueueControlOutcome::Paused { generation })
        }
        QueueControlAction::Resume => {
            let generation = set_queue_paused(transaction, now_ms, false)?;
            Ok(QueueControlOutcome::Resumed { generation })
        }
        QueueControlAction::Purge { limit } => {
            let messages = purge_queue(transaction, limit)?;
            Ok(QueueControlOutcome::Purged { messages })
        }
        QueueControlAction::Redrive { limit } => {
            let messages = redrive_queue(transaction, now_ms, limit)?;
            Ok(QueueControlOutcome::Redriven { messages })
        }
    }
}

/// Returns aggregate state for one queue shard.
pub fn queue_info(connection: &Connection) -> Result<QueueInfo> {
    let (paused, generation, ready, leased, acked, dead) = connection.query_row(
        "SELECT paused, generation, ready_count, leased_count, acked_count, dead_count FROM queue_control WHERE singleton = 1",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        },
    )?;
    Ok(QueueInfo {
        paused: paused != 0,
        generation: nonnegative_u64(generation, "invalid queue control generation")?,
        ready: nonnegative_u64(ready, "invalid ready queue count")?,
        leased: nonnegative_u64(leased, "invalid leased queue count")?,
        acked: nonnegative_u64(acked, "invalid acked queue count")?,
        dead: nonnegative_u64(dead, "invalid dead queue count")?,
    })
}

/// Verifies Queue's transactionally maintained state counters without repairing them.
pub fn verify_queue_counts(connection: &Connection) -> Result<()> {
    let stored = connection.query_row(
        "SELECT ready_count, leased_count, acked_count, dead_count FROM queue_control WHERE singleton = 1",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    let computed = connection.query_row(
        "SELECT count(*) FILTER (WHERE state = 0), count(*) FILTER (WHERE state = 1), count(*) FILTER (WHERE state = 2), count(*) FILTER (WHERE state = 3) FROM queue_messages",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    if stored != computed {
        return Err(Error::Command("queue state counters do not match messages"));
    }
    Ok(())
}

/// Applies an ack, retry or extension only to the exact live lease token.
pub fn queue_apply_lease(
    transaction: &Transaction<'_>,
    now_ms: i64,
    message_id: [u8; 16],
    token: [u8; 16],
    action: QueueLeaseAction,
) -> Result<QueueLeaseOutcome> {
    queue_apply_lease_with_dead_letter(transaction, now_ms, message_id, token, action, None)
}

pub(crate) fn queue_apply_lease_with_dead_letter(
    transaction: &Transaction<'_>,
    now_ms: i64,
    message_id: [u8; 16],
    token: [u8; 16],
    action: QueueLeaseAction,
    dead_letter: Option<&mut QueueDeadLetterWriter<'_>>,
) -> Result<QueueLeaseOutcome> {
    validate_now(now_ms)?;
    let current = transaction
        .query_row(
            "SELECT state, attempt, token, lease_until_ms, expires_at_ms, payload FROM queue_messages WHERE message_id = ?1",
            [message_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((state, attempt, stored_token, lease_until_ms, expires_at_ms, payload)) = current
    else {
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
    let dead_letter_effect_id = if next_state == QueueState::Dead {
        dead_letter
            .map(|writer| writer.insert(transaction, message_id, &payload, now_ms))
            .transpose()?
    } else {
        None
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
            "UPDATE queue_messages SET state = ?1, token = NULL, lease_until_ms = NULL, dead_letter_effect_id = ?5 WHERE message_id = ?2 AND state = 1 AND token = ?3 AND lease_until_ms > ?4",
            (
                next_state.encode(),
                message_id.as_slice(),
                token.as_slice(),
                now_ms,
                dead_letter_effect_id.as_ref().map(<[u8; 32]>::as_slice),
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
    let messages = transaction.execute(
        "DELETE FROM queue_messages WHERE message_id IN (SELECT message_id FROM queue_messages INDEXED BY queue_retention WHERE expires_at_ms <= ?1 AND (state = 2 OR (state = 3 AND (dead_letter_effect_id IS NULL OR NOT EXISTS (SELECT 1 FROM sys_effects WHERE effect_id = queue_messages.dead_letter_effect_id AND state IN (0, 1, 3))))) ORDER BY expires_at_ms, message_id LIMIT ?2)",
        (now_ms, limit as i64),
    )?;
    let remaining = limit.saturating_sub(messages);
    if remaining == 0 {
        return Ok(messages);
    }
    let dedup = transaction.execute(
        "DELETE FROM queue_dedup WHERE producer_id IN (SELECT producer_id FROM queue_dedup INDEXED BY queue_dedup_expiry WHERE retain_until_ms <= ?1 AND NOT EXISTS (SELECT 1 FROM queue_messages WHERE message_id = queue_dedup.message_id) ORDER BY retain_until_ms, producer_id LIMIT ?2)",
        (now_ms, remaining as i64),
    )?;
    messages
        .checked_add(dedup)
        .ok_or(Error::Command("queue cleanup count overflow"))
}

pub(crate) fn queue_reclaim_expired_bounded(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
) -> Result<usize> {
    queue_reclaim_expired_bounded_with_dead_letter(transaction, now_ms, limit, None)
}

pub(crate) fn queue_reclaim_expired_bounded_with_dead_letter(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
    mut dead_letter: Option<&mut QueueDeadLetterWriter<'_>>,
) -> Result<usize> {
    validate_now(now_ms)?;
    validate_maintenance_limit(limit)?;
    if limit == 0 {
        return Ok(0);
    }
    let mut statement = transaction.prepare(
        "SELECT message_id, attempt, expires_at_ms, payload FROM queue_messages INDEXED BY queue_leases WHERE state = 1 AND lease_until_ms <= ?1 ORDER BY lease_until_ms, message_id LIMIT ?2",
    )?;
    let rows = statement.query_map((now_ms, limit as i64), |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    let mut expired = Vec::new();
    for row in rows {
        expired.push(row?);
    }
    drop(statement);
    let count = expired.len();
    for (message_id, attempt, expires_at_ms, payload) in expired {
        let message_id: [u8; 16] = message_id
            .try_into()
            .map_err(|_| Error::Command("invalid stored queue message ID"))?;
        let next_state = if attempt >= i64::from(MAX_ATTEMPTS) || expires_at_ms <= now_ms {
            QueueState::Dead
        } else {
            QueueState::Ready
        };
        let dead_letter_effect_id = if next_state == QueueState::Dead {
            dead_letter
                .as_deref_mut()
                .map(|writer| writer.insert(transaction, message_id, &payload, now_ms))
                .transpose()?
        } else {
            None
        };
        let changed = transaction.execute(
            "UPDATE queue_messages SET state = ?1, due_at_ms = CASE WHEN ?1 = 0 THEN ?2 ELSE due_at_ms END, token = NULL, lease_until_ms = NULL, dead_letter_effect_id = ?4 WHERE message_id = ?3 AND state = 1 AND lease_until_ms <= ?2",
            (
                next_state.encode(),
                now_ms,
                message_id.as_slice(),
                dead_letter_effect_id.as_ref().map(<[u8; 32]>::as_slice),
            ),
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
    queue_expire_ready_bounded_with_dead_letter(transaction, now_ms, limit, None)
}

pub(crate) fn queue_expire_ready_bounded_with_dead_letter(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
    mut dead_letter: Option<&mut QueueDeadLetterWriter<'_>>,
) -> Result<usize> {
    validate_now(now_ms)?;
    validate_maintenance_limit(limit)?;
    if limit == 0 {
        return Ok(0);
    }
    let mut statement = transaction.prepare(
        "SELECT message_id, payload FROM queue_messages INDEXED BY queue_ready WHERE state = 0 AND (expires_at_ms <= ?1 OR attempt >= ?2) ORDER BY due_at_ms, message_id LIMIT ?3",
    )?;
    let rows = statement.query_map((now_ms, i64::from(MAX_ATTEMPTS), limit as i64), |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let expired = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    for (message_id, payload) in &expired {
        let message_id: [u8; 16] = message_id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Command("invalid stored queue message ID"))?;
        let dead_letter_effect_id = dead_letter
            .as_deref_mut()
            .map(|writer| writer.insert(transaction, message_id, payload, now_ms))
            .transpose()?;
        let changed = transaction.execute(
            "UPDATE queue_messages SET state = 3, dead_letter_effect_id = ?1 WHERE message_id = ?2 AND state = 0 AND (expires_at_ms <= ?3 OR attempt >= ?4)",
            (
                dead_letter_effect_id.as_ref().map(<[u8; 32]>::as_slice),
                message_id.as_slice(),
                now_ms,
                i64::from(MAX_ATTEMPTS),
            ),
        )?;
        if changed != 1 {
            return Err(Error::Command("queue expiry lost selected ready row"));
        }
    }
    Ok(expired.len())
}

fn queue_paused(connection: &Connection) -> Result<bool> {
    let paused = connection.query_row(
        "SELECT paused FROM queue_control WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(paused != 0)
}

fn set_queue_paused(transaction: &Transaction<'_>, now_ms: i64, paused: bool) -> Result<u64> {
    let paused_value = if paused { 1_i64 } else { 0_i64 };
    transaction.execute(
        "UPDATE queue_control SET paused = ?1, generation = generation + CASE WHEN paused = ?1 THEN 0 ELSE 1 END, updated_at_ms = ?2 WHERE singleton = 1",
        (paused_value, now_ms),
    )?;
    let generation = transaction.query_row(
        "SELECT generation FROM queue_control WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    nonnegative_u64(generation, "invalid queue control generation")
}

fn purge_queue(transaction: &Transaction<'_>, limit: u32) -> Result<u32> {
    let limit = control_limit(limit)?;
    let mut statement = transaction.prepare(
        "SELECT message_id FROM queue_messages WHERE state != 1 ORDER BY message_id LIMIT ?1",
    )?;
    let rows = statement.query_map([i64::from(limit)], |row| row.get::<_, Vec<u8>>(0))?;
    let ids = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    for id in &ids {
        transaction.execute("DELETE FROM queue_dedup WHERE message_id = ?1", [id])?;
        let changed = transaction.execute(
            "DELETE FROM queue_messages WHERE message_id = ?1 AND state != 1",
            [id],
        )?;
        if changed != 1 {
            return Err(Error::Command("queue purge lost selected message"));
        }
    }
    u32::try_from(ids.len()).map_err(|_| Error::Command("queue purge count overflow"))
}

fn redrive_queue(transaction: &Transaction<'_>, now_ms: i64, limit: u32) -> Result<u32> {
    let limit = control_limit(limit)?;
    let changed = transaction.execute(
        "UPDATE queue_messages SET state = 0, attempt = 0, due_at_ms = ?1, expires_at_ms = ?2, token = NULL, lease_until_ms = NULL, result_code = NULL, dead_letter_effect_id = NULL WHERE message_id IN (SELECT message_id FROM queue_messages WHERE state = 3 AND (dead_letter_effect_id IS NULL OR NOT EXISTS (SELECT 1 FROM sys_effects WHERE effect_id = queue_messages.dead_letter_effect_id AND state IN (0, 1))) ORDER BY message_id LIMIT ?3)",
        (
            now_ms,
            now_ms
                .checked_add(RETENTION_MS)
                .ok_or(Error::Command("queue redrive expiry overflow"))?,
            i64::from(limit),
        ),
    )?;
    u32::try_from(changed).map_err(|_| Error::Command("queue redrive count overflow"))
}

fn control_limit(limit: u32) -> Result<u32> {
    if !(1..=MAX_RECLAIM_ITEMS as u32).contains(&limit) {
        return Err(Error::Command("queue control limit must be in 1..=128"));
    }
    Ok(limit)
}

fn nonnegative_u64(value: i64, message: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Command(message))
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

fn dead_letter_producer_id(
    target: NamespaceId,
    source: CellId,
    incarnation: IncarnationId,
    message_id: [u8; 16],
) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.queue-dead-letter.v1\0");
    hasher.update(target.as_bytes());
    hasher.update(source.as_bytes());
    hasher.update(incarnation.as_bytes());
    hasher.update(&message_id);
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
mod tests;
