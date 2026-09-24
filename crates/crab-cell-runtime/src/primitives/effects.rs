use prost::Message;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::cell::executor::Resolution;
use crate::cell::executor::{HandlerOutcome, StoredOutcome};
use crate::identity::IncarnationId;
use crate::identity::{CellId, CellTarget, Digest};
use crate::peer::wire;
use crate::{Error, Result};

mod api;
mod lease;
mod supervisor;

pub use lease::*;

#[cfg(test)]
mod tests;

pub use api::{
    EffectAckRequest, EffectClaimCommand, EffectClaimRequest, EffectLeaseCommand,
    EffectLeaseRequest, EffectModule, EffectSource, EffectStatusQuery, EffectStatusRequest,
    EffectValidateClaimQuery, EffectValidateRequest, register_effect_delivery,
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

/// Durable source-side effect outcome and lease state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectStatus {
    pub state: EffectState,
    pub attempt: u32,
    pub token_present: bool,
    pub lease_until_ms: Option<i64>,
    pub expires_at_ms: i64,
    pub result: Option<Vec<u8>>,
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

    fn decode(value: i64) -> Result<Self> {
        match value {
            0 => Ok(Self::Ready),
            1 => Ok(Self::Leased),
            2 => Ok(Self::Delivered),
            3 => Ok(Self::Failed),
            _ => Err(Error::Command("invalid stored effect state")),
        }
    }
}

/// Reads one exact durable source effect without exposing its lease token.
pub fn effect_status(connection: &Connection, effect_id: [u8; 32]) -> Result<Option<EffectStatus>> {
    let row = connection
        .query_row(
            "SELECT state, attempt, token IS NOT NULL, lease_until_ms, expires_at_ms, result FROM sys_effects WHERE effect_id = ?1",
            [effect_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((state, attempt, token_present, lease_until_ms, expires_at_ms, result)) = row else {
        return Ok(None);
    };
    if result
        .as_ref()
        .is_some_and(|result| result.len() > MAX_EFFECT_RESULT_BYTES)
    {
        return Err(Error::Command("stored effect result exceeds wire limit"));
    }
    Ok(Some(EffectStatus {
        state: EffectState::decode(state)?,
        attempt: u32::try_from(attempt)
            .map_err(|_| Error::Command("invalid stored effect attempt"))?,
        token_present,
        lease_until_ms,
        expires_at_ms,
        result,
    }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EffectIntent {
    pub destination: CellId,
    pub operation: Vec<u8>,
    pub expires_at_ms: i64,
}

/// One stable typed command whose destination incarnation is resolved at delivery time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectCommandIntent {
    pub target: CellTarget,
    pub command_id: u32,
    pub codec_version: u32,
    pub input: Vec<u8>,
    pub expires_at_ms: i64,
}

/// Command-scoped allocator for durable effect identities.
///
/// One batch must be shared by every primitive transition performed by the
/// same Cell command so each emitted effect receives a unique ordinal.
pub(crate) struct EffectBatch {
    source: CellTarget,
    incarnation: IncarnationId,
    sequence: u64,
    now_ms: i64,
    next_ordinal: u32,
    operation_bytes: usize,
}

impl EffectBatch {
    /// Verifies the supplied source against the authoritative command transaction.
    pub(crate) fn new(
        transaction: &Transaction<'_>,
        source: &CellTarget,
        sequence: u64,
        now_ms: i64,
    ) -> Result<Self> {
        if sequence == 0 {
            return Err(Error::Command("effect batch sequence is zero"));
        }
        validate_now(now_ms)?;
        let (cell, incarnation, commit_sequence) = transaction.query_row(
            "SELECT cell_id, incarnation, commit_sequence FROM sys_meta WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            },
        )?;
        if commit_sequence.checked_add(1) != Some(sequence) {
            return Err(Error::Command(
                "effect batch sequence is not the next Cell commit",
            ));
        }
        let cell = CellId::from_bytes(
            cell.try_into()
                .map_err(|_| Error::Command("invalid stored effect source Cell"))?,
        );
        if source.cell_id() != cell {
            return Err(Error::Command(
                "effect source target does not match its Cell",
            ));
        }
        Ok(Self {
            source: source.clone(),
            incarnation: IncarnationId::from_bytes(
                incarnation
                    .try_into()
                    .map_err(|_| Error::Command("invalid stored effect source incarnation"))?,
            ),
            sequence,
            now_ms,
            next_ordinal: 0,
            operation_bytes: 0,
        })
    }

    /// Inserts one canonical Cell command without pinning a destination owner incarnation.
    pub(crate) fn insert_command(
        &mut self,
        transaction: &Transaction<'_>,
        intent: &EffectCommandIntent,
    ) -> Result<[u8; 32]> {
        if intent.target.tenant() != self.source.tenant()
            || intent.target.application() != self.source.application()
        {
            return Err(Error::Identity(
                "effect target is outside the source application scope",
            ));
        }
        validate_effect_command_intent(self.now_ms, intent)?;
        if self.next_ordinal as usize >= MAX_EFFECTS_PER_COMMAND {
            return Err(Error::Command("command effects exceed limits"));
        }
        let ordinal = self.next_ordinal;
        let id = effect_id(
            self.source.cell_id(),
            self.incarnation,
            self.sequence,
            ordinal,
        );
        let operation = wire::EffectRequest {
            target: Some(wire::Target {
                tenant_id: intent.target.tenant().as_bytes().to_vec(),
                application_id: intent.target.application().as_bytes().to_vec(),
                namespace_id: intent.target.namespace().as_bytes().to_vec(),
                partition: intent.target.partition().to_vec(),
            }),
            // The durable operation is owner-independent. The delivery client
            // fills this field from a fresh Describe immediately before send.
            destination_incarnation: Vec::new(),
            identity: Some(wire::EffectIdentity {
                effect_id: id.to_vec(),
                source_cell: self.source.cell_id().as_bytes().to_vec(),
                source_incarnation: self.incarnation.as_bytes().to_vec(),
                source_sequence: self.sequence,
                ordinal,
                expires_at_ms: intent.expires_at_ms,
            }),
            operation: Some(wire::effect_request::Operation::CellCommand(
                wire::CellCommand {
                    command_id: intent.command_id,
                    codec_version: intent.codec_version,
                    input: intent.input.clone(),
                },
            )),
        }
        .encode_to_vec();
        let operation_len = operation.len();
        let prospective = self
            .operation_bytes
            .checked_add(operation_len)
            .ok_or(Error::Command("effect byte count overflow"))?;
        if prospective > MAX_EFFECT_BYTES {
            return Err(Error::Command("command effects exceed limits"));
        }
        let next_ordinal = ordinal
            .checked_add(1)
            .ok_or(Error::Command("effect ordinal overflow"))?;
        let id = effect_insert(
            transaction,
            self.source.cell_id(),
            self.incarnation,
            self.sequence,
            ordinal,
            self.now_ms,
            &EffectIntent {
                destination: intent.target.cell_id(),
                operation,
                expires_at_ms: intent.expires_at_ms,
            },
        )?;
        self.next_ordinal = next_ordinal;
        self.operation_bytes = prospective;
        Ok(id)
    }

    pub(crate) const fn source_incarnation(&self) -> IncarnationId {
        self.incarnation
    }

    pub(crate) const fn source_target(&self) -> &CellTarget {
        &self.source
    }
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
fn effect_insert(
    transaction: &Transaction<'_>,
    cell: CellId,
    incarnation: IncarnationId,
    sequence: u64,
    ordinal: u32,
    now_ms: i64,
    intent: &EffectIntent,
) -> Result<[u8; 32]> {
    validate_now(now_ms)?;
    validate_effect_intent(now_ms, intent)?;
    if sequence == 0 || sequence > i64::MAX as u64 || ordinal as usize >= MAX_EFFECTS_PER_COMMAND {
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

fn validate_effect_intent(now_ms: i64, intent: &EffectIntent) -> Result<()> {
    validate_now(now_ms)?;
    if intent.operation.is_empty()
        || intent.operation.len() > MAX_EFFECT_OPERATION_BYTES
        || intent.expires_at_ms <= now_ms
        || intent.expires_at_ms > now_ms.saturating_add(EFFECT_LIFETIME_MS)
    {
        return Err(Error::Command("invalid effect intention"));
    }
    Ok(())
}

pub(crate) fn validate_effect_command_intent(
    now_ms: i64,
    intent: &EffectCommandIntent,
) -> Result<()> {
    if intent.command_id == 0 || intent.codec_version == 0 {
        return Err(Error::Command("invalid effect Cell command identifier"));
    }
    let operation = wire::EffectRequest {
        target: Some(wire::Target {
            tenant_id: intent.target.tenant().as_bytes().to_vec(),
            application_id: intent.target.application().as_bytes().to_vec(),
            namespace_id: intent.target.namespace().as_bytes().to_vec(),
            partition: intent.target.partition().to_vec(),
        }),
        destination_incarnation: Vec::new(),
        identity: Some(wire::EffectIdentity {
            effect_id: vec![u8::MAX; 32],
            source_cell: vec![u8::MAX; 32],
            source_incarnation: vec![u8::MAX; 16],
            source_sequence: u64::MAX,
            ordinal: u32::MAX,
            expires_at_ms: intent.expires_at_ms,
        }),
        operation: Some(wire::effect_request::Operation::CellCommand(
            wire::CellCommand {
                command_id: intent.command_id,
                codec_version: intent.codec_version,
                input: intent.input.clone(),
            },
        )),
    }
    .encode_to_vec();
    validate_effect_intent(
        now_ms,
        &EffectIntent {
            destination: intent.target.cell_id(),
            operation,
            expires_at_ms: intent.expires_at_ms,
        },
    )
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
