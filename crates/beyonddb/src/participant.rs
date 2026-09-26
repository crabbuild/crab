//! Durable participant identity and decision handling shared by account and data Cells.

use crate::table::statement;
use crate::{Error, Json, Result, SqlValue, TransactionFailure};
use crab_cell_runtime::registry::{CommandContext, CommandResult, QueryContext};
use extenddb_core::types::Item;
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = concat!(
    include_str!("transaction_payload_schema.sql"),
    "\n",
    include_str!("participant_schema.sql")
);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum StagedEffect {
    Write,
    Check,
    Read,
}

/// Result of preparing one account or data Cell participant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PrepareTransactionOutcome {
    Prepared,
    Replay,
    Committed,
    Aborted,
    Mismatch,
    Rejected {
        index: usize,
        reason: TransactionFailure,
    },
    NotInstalled,
    StaleRoute,
    Sealed,
    NotReady,
    WrongPartition,
}

/// A terminal coordinator decision for one participant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolveTransactionInput {
    pub transaction_id: [u8; 16],
    pub coordinator_cell: [u8; 32],
    pub commit: bool,
}

/// Result of applying or replaying a participant decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ResolveTransactionOutcome {
    Committed,
    Aborted,
    MissingPrepare,
    DecisionConflict,
}

/// Identity of a participant record and its coordinator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadTransactionInput {
    pub transaction_id: [u8; 16],
    pub coordinator_cell: [u8; 32],
}

/// Durable identity of the exclusive intent blocking a read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransactionReadConflict {
    pub transaction: ReadTransactionInput,
    pub coordinator_key: Vec<u8>,
}

pub(crate) fn read_conflict(
    context: &QueryContext<'_>,
    locks: &crate::SqlResultSet,
) -> Result<Option<TransactionReadConflict>> {
    let Some(row) = locks.rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Blob(id)] = row.as_slice() else {
        return Err(Error::Command("invalid transaction lock identity"));
    };
    let rows = context.sql(&statement(
        "SELECT coordinator_cell, coordinator_key FROM ddb_transactions WHERE transaction_id = ?1 AND state = 0",
        vec![SqlValue::Blob(id.clone())],
    ))?;
    let Some([SqlValue::Blob(cell), SqlValue::Blob(key)]) = rows[0].rows.first().map(Vec::as_slice)
    else {
        return Err(Error::Command("transaction lock has no prepared identity"));
    };
    Ok(Some(TransactionReadConflict {
        transaction: ReadTransactionInput {
            transaction_id: id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Command("invalid transaction ID"))?,
            coordinator_cell: cell
                .as_slice()
                .try_into()
                .map_err(|_| Error::Command("invalid coordinator Cell"))?,
        },
        coordinator_key: key.clone(),
    }))
}

/// Published participant state observed after a phase invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ParticipantTransactionState {
    Missing,
    Prepared,
    Committed,
    Aborted,
    CoordinatorMismatch,
}

pub(crate) fn prepared(
    context: &mut CommandContext<'_, '_>,
    transaction_id: [u8; 16],
    coordinator_cell: [u8; 32],
    digest: blake3::Hash,
) -> Result<Option<PrepareTransactionOutcome>> {
    let existing = context.sql(&statement(
        "SELECT coordinator_cell, request_digest, state FROM ddb_transactions \
             WHERE transaction_id = ?1",
        vec![SqlValue::Blob(transaction_id.to_vec())],
    ))?;
    if let Some(row) = existing[0].rows.first() {
        let [
            SqlValue::Blob(coordinator),
            request_digest,
            SqlValue::Integer(state),
        ] = row.as_slice()
        else {
            return Err(Error::Command("invalid participant transaction record"));
        };
        if coordinator.as_slice() != coordinator_cell {
            return Ok(Some(PrepareTransactionOutcome::Mismatch));
        }
        let outcome = match state {
            0 if request_digest == &SqlValue::Blob(digest.as_bytes().to_vec()) => {
                PrepareTransactionOutcome::Replay
            }
            1 if request_digest == &SqlValue::Blob(digest.as_bytes().to_vec()) => {
                PrepareTransactionOutcome::Committed
            }
            2 => PrepareTransactionOutcome::Aborted,
            _ => PrepareTransactionOutcome::Mismatch,
        };
        return Ok(Some(outcome));
    }
    Ok(None)
}

pub(crate) fn record_prepare<'a>(
    context: &mut CommandContext<'_, '_>,
    transaction_id: [u8; 16],
    coordinator_cell: [u8; 32],
    digest: blake3::Hash,
    staged: Vec<u8>,
    coordinator_key: &[u8],
    read_result: impl Iterator<Item = Option<&'a Item>>,
) -> Result<()> {
    if coordinator_key.is_empty() || coordinator_key.len() > 128 {
        return Err(Error::Command("invalid coordinator routing key"));
    }
    let chunks = crate::transaction_payload::write(context, transaction_id, 0, &staged)?;
    context.sql(&statement(
        "INSERT INTO ddb_transactions (transaction_id, coordinator_cell, request_digest, state, staged_chunks, coordinator_key) VALUES (?1, ?2, ?3, 0, ?4, ?5)",
        vec![SqlValue::Blob(transaction_id.to_vec()), SqlValue::Blob(coordinator_cell.to_vec()),
             SqlValue::Blob(digest.as_bytes().to_vec()), SqlValue::Integer(chunks), SqlValue::Blob(coordinator_key.to_vec())],
    ))?;
    // Persist images with prepare so recovery never re-reads live rows. COMMIT
    // releases locks but keeps these images until the response can be fetched.
    for (position, item) in read_result.enumerate() {
        let position =
            i64::try_from(position).map_err(|_| Error::Command("read position overflow"))?;
        context.sql(&statement(
            "INSERT INTO ddb_transaction_reads (transaction_id, position, item) VALUES (?1, ?2, ?3)",
            vec![SqlValue::Blob(transaction_id.to_vec()), SqlValue::Integer(position), if item.is_some() { SqlValue::Blob(Vec::new()) } else { SqlValue::Null }],
        ))?;
        if let Some(item) = item {
            crate::item_storage::StoredItem::TransactionRead {
                transaction_id: &transaction_id,
                position,
            }
            .append(context, item)?;
        }
    }
    Ok(())
}

pub(crate) fn resolve(
    context: &mut CommandContext<'_, '_>,
    input: ResolveTransactionInput,
    finish: impl FnOnce(&mut CommandContext<'_, '_>, Option<&[u8]>) -> Result<()>,
) -> Result<CommandResult<Json<ResolveTransactionOutcome>>> {
    let rows = context.sql(&statement(
        "SELECT coordinator_cell, state, staged_chunks \
             FROM ddb_transactions WHERE transaction_id = ?1",
        vec![SqlValue::Blob(input.transaction_id.to_vec())],
    ))?;
    let Some(row) = rows[0].rows.first() else {
        if input.commit {
            return Ok(CommandResult::Rejected(Json(
                ResolveTransactionOutcome::MissingPrepare,
            )));
        }
        // A delayed prepare may arrive after abort resolution. Persist a
        // tombstone first so that it cannot reacquire item locks.
        context.sql(&statement(
            "INSERT INTO ddb_transactions \
                 (transaction_id, coordinator_cell, state) VALUES (?1, ?2, 2)",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Blob(input.coordinator_cell.to_vec()),
            ],
        ))?;
        return Ok(CommandResult::Success(Json(
            ResolveTransactionOutcome::Aborted,
        )));
    };
    let [
        SqlValue::Blob(coordinator),
        SqlValue::Integer(state),
        staged,
    ] = row.as_slice()
    else {
        return Err(Error::Command("invalid participant transaction record"));
    };
    if coordinator.as_slice() != input.coordinator_cell {
        return Ok(CommandResult::Rejected(Json(
            ResolveTransactionOutcome::DecisionConflict,
        )));
    }
    match (*state, input.commit) {
        (1, true) => {
            return Ok(CommandResult::Success(Json(
                ResolveTransactionOutcome::Committed,
            )));
        }
        (2, false) => {
            return Ok(CommandResult::Success(Json(
                ResolveTransactionOutcome::Aborted,
            )));
        }
        (0, _) => {}
        _ => {
            return Ok(CommandResult::Rejected(Json(
                ResolveTransactionOutcome::DecisionConflict,
            )));
        }
    }
    let staged = if input.commit {
        let SqlValue::Integer(chunks) = staged else {
            return Err(Error::Command("prepared transaction has no staged images"));
        };
        Some(crate::transaction_payload::read(
            |batch| context.sql(batch),
            input.transaction_id,
            0,
            *chunks,
        )?)
    } else {
        None
    };
    // Image application and lock release share the command savepoint with the
    // terminal marker. A failed callback must leave the participant prepared.
    finish(context, staged.as_deref())?;
    context.sql(&statement(
        "DELETE FROM ddb_transaction_payloads WHERE transaction_id = ?1",
        vec![SqlValue::Blob(input.transaction_id.to_vec())],
    ))?;
    context.sql(&statement(
        "UPDATE ddb_transactions SET state = ?1, staged_chunks = NULL \
             WHERE transaction_id = ?2 AND state = 0",
        vec![
            SqlValue::Integer(if input.commit { 1 } else { 2 }),
            SqlValue::Blob(input.transaction_id.to_vec()),
        ],
    ))?;
    if !input.commit {
        context.sql(&statement(
            "DELETE FROM ddb_transaction_reads WHERE transaction_id = ?1",
            vec![SqlValue::Blob(input.transaction_id.to_vec())],
        ))?;
    }
    let outcome = if input.commit {
        ResolveTransactionOutcome::Committed
    } else {
        ResolveTransactionOutcome::Aborted
    };
    Ok(CommandResult::Success(Json(outcome)))
}

/// Identity of one immutable read image within a participant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadTransactionResultInput {
    pub transaction: ReadTransactionInput,
    pub position: u8,
}

/// Saved image from a committed read participant, including an absent item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransactionReadResult {
    Unavailable,
    Item(Option<Item>),
}

pub(crate) fn read_result(
    context: &QueryContext<'_>,
    input: ReadTransactionResultInput,
) -> Result<Json<TransactionReadResult>> {
    // Read each saved image by position: a 100-item request must not force a
    // multi-megabyte participant result through one bounded Cell RPC.
    let rows = context.sql(&statement(
        "SELECT 1 FROM ddb_transaction_reads r JOIN ddb_transactions t \
         ON t.transaction_id = r.transaction_id WHERE r.transaction_id = ?1 \
         AND r.position = ?2 AND t.coordinator_cell = ?3 AND t.state = 1",
        vec![
            SqlValue::Blob(input.transaction.transaction_id.to_vec()),
            SqlValue::Integer(i64::from(input.position)),
            SqlValue::Blob(input.transaction.coordinator_cell.to_vec()),
        ],
    ))?;
    if rows[0].rows.is_empty() {
        return Ok(Json(TransactionReadResult::Unavailable));
    }
    let item = crate::item_storage::StoredItem::TransactionRead {
        transaction_id: &input.transaction.transaction_id,
        position: i64::from(input.position),
    }
    .read(|batch| context.sql(batch))?;
    Ok(Json(TransactionReadResult::Item(item)))
}

pub(crate) fn read(
    context: &QueryContext<'_>,
    input: ReadTransactionInput,
) -> Result<Json<ParticipantTransactionState>> {
    let rows = context.sql(&statement(
        "SELECT coordinator_cell, state FROM ddb_transactions WHERE transaction_id = ?1",
        vec![SqlValue::Blob(input.transaction_id.to_vec())],
    ))?;
    let Some(row) = rows[0].rows.first() else {
        return Ok(Json(ParticipantTransactionState::Missing));
    };
    let [SqlValue::Blob(coordinator), SqlValue::Integer(state)] = row.as_slice() else {
        return Err(Error::Command("invalid participant transaction record"));
    };
    if coordinator.as_slice() != input.coordinator_cell {
        return Ok(Json(ParticipantTransactionState::CoordinatorMismatch));
    }
    let outcome = match state {
        0 => ParticipantTransactionState::Prepared,
        1 => ParticipantTransactionState::Committed,
        2 => ParticipantTransactionState::Aborted,
        _ => return Err(Error::Command("invalid participant transaction state")),
    };
    Ok(Json(outcome))
}
