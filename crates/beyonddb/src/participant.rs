//! Durable participant identity and decision handling shared by account and data Cells.

use crate::table::statement;
use crate::{Error, Json, Result, SqlValue, TransactionFailure};
use crab_cell_runtime::registry::{CommandContext, CommandResult, QueryContext};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = include_str!("participant_schema.sql");

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

pub(crate) fn record_prepare(
    context: &mut CommandContext<'_, '_>,
    transaction_id: [u8; 16],
    coordinator_cell: [u8; 32],
    digest: blake3::Hash,
    staged: Vec<u8>,
) -> Result<()> {
    context.sql(&statement(
        "INSERT INTO ddb_transactions (transaction_id, coordinator_cell, request_digest, state, staged) VALUES (?1, ?2, ?3, 0, ?4)",
        vec![SqlValue::Blob(transaction_id.to_vec()), SqlValue::Blob(coordinator_cell.to_vec()),
             SqlValue::Blob(digest.as_bytes().to_vec()), SqlValue::Blob(staged)],
    ))?;
    Ok(())
}

pub(crate) fn resolve(
    context: &mut CommandContext<'_, '_>,
    input: ResolveTransactionInput,
    finish: impl FnOnce(&mut CommandContext<'_, '_>, Option<&[u8]>) -> Result<()>,
) -> Result<CommandResult<Json<ResolveTransactionOutcome>>> {
    let rows = context.sql(&statement(
        "SELECT coordinator_cell, state, staged \
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
        let SqlValue::Blob(bytes) = staged else {
            return Err(Error::Command("prepared transaction has no staged images"));
        };
        Some(bytes.as_slice())
    } else {
        None
    };
    // Image application and lock release share the command savepoint with the
    // terminal marker. A failed callback must leave the participant prepared.
    finish(context, staged)?;
    context.sql(&statement(
        "UPDATE ddb_transactions SET state = ?1, staged = NULL \
             WHERE transaction_id = ?2 AND state = 0",
        vec![
            SqlValue::Integer(if input.commit { 1 } else { 2 }),
            SqlValue::Blob(input.transaction_id.to_vec()),
        ],
    ))?;
    let outcome = if input.commit {
        ResolveTransactionOutcome::Committed
    } else {
        ResolveTransactionOutcome::Aborted
    };
    Ok(CommandResult::Success(Json(outcome)))
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
