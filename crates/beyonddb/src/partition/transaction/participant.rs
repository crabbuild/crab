//! Durable data Cell participant intents for cross-Cell transactions.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::super::{
    AccessState, DATA_MODULE, Error, Json, Result, SqlValue, command_access, decode_spec, statement,
};
use super::{StagedImage, apply_staged, stage_operations};
use crate::items::{TransactionFailure, TransactionWrite};

/// Prepare a local subset of a cross-Cell transaction without exposing writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreparePartitionTransactionInput {
    pub table_id: String,
    pub epoch: u64,
    pub transaction_id: [u8; 16],
    pub coordinator_cell: [u8; 32],
    pub operations: Vec<TransactionWrite>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PreparePartitionTransactionOutcome {
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

/// Persist staged images and exclusive item locks in one published command.
pub struct PreparePartitionTransaction;

impl Command for PreparePartitionTransaction {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PreparePartitionTransactionInput>;
    type Output = Json<PreparePartitionTransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let digest = blake3::hash(&serde_json::to_vec(&input)?);
        let existing = context.sql(&statement(
            "SELECT coordinator_cell, request_digest, state FROM ddb_partition_transactions \
             WHERE transaction_id = ?1",
            vec![SqlValue::Blob(input.transaction_id.to_vec())],
        ))?;
        if let Some(row) = existing[0].rows.first() {
            let [
                SqlValue::Blob(coordinator),
                request_digest,
                SqlValue::Integer(state),
            ] = row.as_slice()
            else {
                return Err(Error::Command("invalid partition transaction record"));
            };
            if coordinator.as_slice() != input.coordinator_cell {
                return Ok(prepare_rejected(
                    PreparePartitionTransactionOutcome::Mismatch,
                ));
            }
            let outcome = match state {
                0 if request_digest == &SqlValue::Blob(digest.as_bytes().to_vec()) => {
                    PreparePartitionTransactionOutcome::Replay
                }
                1 if request_digest == &SqlValue::Blob(digest.as_bytes().to_vec()) => {
                    PreparePartitionTransactionOutcome::Committed
                }
                2 => PreparePartitionTransactionOutcome::Aborted,
                _ => PreparePartitionTransactionOutcome::Mismatch,
            };
            return Ok(prepare_rejected(outcome));
        }
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(prepare_rejected(
                PreparePartitionTransactionOutcome::NotInstalled,
            ));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(prepare_rejected(
                PreparePartitionTransactionOutcome::StaleRoute,
            ));
        }
        match command_access(context)? {
            AccessState::Serving => {}
            AccessState::Sealed => {
                return Ok(prepare_rejected(PreparePartitionTransactionOutcome::Sealed));
            }
            AccessState::Importing => {
                return Ok(prepare_rejected(
                    PreparePartitionTransactionOutcome::NotReady,
                ));
            }
        }
        if input.operations.is_empty() || input.operations.len() > 100 {
            return Ok(prepare_validation(
                0,
                "transaction operation count is outside 1..=100",
            ));
        }
        let staged = match stage_operations(context, &spec, input.operations)? {
            Ok(staged) => staged,
            Err(reason) => return Ok(prepare_rejected(reason.prepare_outcome())),
        };
        context.sql(&statement(
            "INSERT INTO ddb_partition_transactions \
             (transaction_id, coordinator_cell, request_digest, state, table_id, epoch, staged) \
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Blob(input.coordinator_cell.to_vec()),
                SqlValue::Blob(digest.as_bytes().to_vec()),
                SqlValue::Text(input.table_id),
                SqlValue::Integer(
                    i64::try_from(input.epoch)
                        .map_err(|_| Error::Command("partition epoch overflow"))?,
                ),
                SqlValue::Blob(serde_json::to_vec(&staged)?),
            ],
        ))?;
        for image in staged {
            context.sql(&statement(
                "INSERT INTO ddb_partition_transaction_locks \
                 (item_key, partition_key, sort_key, transaction_id) VALUES (?1, ?2, ?3, ?4)",
                vec![
                    SqlValue::Blob(image.key),
                    SqlValue::Blob(image.partition_key),
                    SqlValue::Blob(image.sort_key),
                    SqlValue::Blob(input.transaction_id.to_vec()),
                ],
            ))?;
        }
        Ok(CommandResult::Success(Json(
            PreparePartitionTransactionOutcome::Prepared,
        )))
    }
}

fn prepare_rejected(
    outcome: PreparePartitionTransactionOutcome,
) -> CommandResult<Json<PreparePartitionTransactionOutcome>> {
    CommandResult::Rejected(Json(outcome))
}

fn prepare_validation(
    index: usize,
    message: &str,
) -> CommandResult<Json<PreparePartitionTransactionOutcome>> {
    prepare_rejected(PreparePartitionTransactionOutcome::Rejected {
        index,
        reason: TransactionFailure::Validation(message.into()),
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvePartitionTransactionInput {
    pub transaction_id: [u8; 16],
    pub coordinator_cell: [u8; 32],
    pub commit: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ResolvePartitionTransactionOutcome {
    Committed,
    Aborted,
    MissingPrepare,
    DecisionConflict,
}

/// Apply one durable decision and release every local key lock atomically.
pub struct ResolvePartitionTransaction;

impl Command for ResolvePartitionTransaction {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 13;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ResolvePartitionTransactionInput>;
    type Output = Json<ResolvePartitionTransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT coordinator_cell, state, table_id, epoch, staged \
             FROM ddb_partition_transactions WHERE transaction_id = ?1",
            vec![SqlValue::Blob(input.transaction_id.to_vec())],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            if input.commit {
                return Ok(CommandResult::Rejected(Json(
                    ResolvePartitionTransactionOutcome::MissingPrepare,
                )));
            }
            // A delayed prepare may arrive after abort resolution. Persist a
            // tombstone first so that it cannot reacquire item locks.
            context.sql(&statement(
                "INSERT INTO ddb_partition_transactions \
                 (transaction_id, coordinator_cell, state) VALUES (?1, ?2, 2)",
                vec![
                    SqlValue::Blob(input.transaction_id.to_vec()),
                    SqlValue::Blob(input.coordinator_cell.to_vec()),
                ],
            ))?;
            return Ok(CommandResult::Success(Json(
                ResolvePartitionTransactionOutcome::Aborted,
            )));
        };
        let [
            SqlValue::Blob(coordinator),
            SqlValue::Integer(state),
            table_id,
            epoch,
            staged,
        ] = row.as_slice()
        else {
            return Err(Error::Command("invalid partition transaction record"));
        };
        if coordinator.as_slice() != input.coordinator_cell {
            return Ok(CommandResult::Rejected(Json(
                ResolvePartitionTransactionOutcome::DecisionConflict,
            )));
        }
        match (*state, input.commit) {
            (1, true) => {
                return Ok(CommandResult::Success(Json(
                    ResolvePartitionTransactionOutcome::Committed,
                )));
            }
            (2, false) => {
                return Ok(CommandResult::Success(Json(
                    ResolvePartitionTransactionOutcome::Aborted,
                )));
            }
            (0, _) => {}
            _ => {
                return Ok(CommandResult::Rejected(Json(
                    ResolvePartitionTransactionOutcome::DecisionConflict,
                )));
            }
        }
        if input.commit {
            let SqlValue::Blob(bytes) = staged else {
                return Err(Error::Command("prepared transaction has no staged images"));
            };
            let images: Vec<StagedImage> = serde_json::from_slice(bytes)?;
            let spec_rows = context.sql(&statement(
                "SELECT spec FROM ddb_partition WHERE singleton = 1",
                vec![],
            ))?;
            let Some(spec) = decode_spec(&spec_rows[0])? else {
                return Err(Error::Command("prepared partition is missing"));
            };
            if table_id != &SqlValue::Text(spec.table.id.clone())
                || epoch
                    != &SqlValue::Integer(
                        i64::try_from(spec.epoch)
                            .map_err(|_| Error::Command("partition epoch overflow"))?,
                    )
            {
                return Err(Error::Command("prepared partition identity changed"));
            }
            apply_staged(context, &spec.table.key_schema, images)?;
        }
        context.sql(&statement(
            "DELETE FROM ddb_partition_transaction_locks WHERE transaction_id = ?1",
            vec![SqlValue::Blob(input.transaction_id.to_vec())],
        ))?;
        context.sql(&statement(
            "UPDATE ddb_partition_transactions SET state = ?1, staged = NULL \
             WHERE transaction_id = ?2 AND state = 0",
            vec![
                SqlValue::Integer(if input.commit { 1 } else { 2 }),
                SqlValue::Blob(input.transaction_id.to_vec()),
            ],
        ))?;
        let outcome = if input.commit {
            ResolvePartitionTransactionOutcome::Committed
        } else {
            ResolvePartitionTransactionOutcome::Aborted
        };
        Ok(CommandResult::Success(Json(outcome)))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadPartitionTransactionInput {
    pub transaction_id: [u8; 16],
    pub coordinator_cell: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReadPartitionTransactionOutcome {
    Missing,
    Prepared,
    Committed,
    Aborted,
    CoordinatorMismatch,
}

/// Read a participant's durable state after an ambiguous phase reply.
pub struct ReadPartitionTransaction;

impl Query for ReadPartitionTransaction {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ReadPartitionTransactionInput>;
    type Output = Json<ReadPartitionTransactionOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT coordinator_cell, state FROM ddb_partition_transactions WHERE transaction_id = ?1",
            vec![SqlValue::Blob(input.transaction_id.to_vec())],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(Json(ReadPartitionTransactionOutcome::Missing));
        };
        let [SqlValue::Blob(coordinator), SqlValue::Integer(state)] = row.as_slice() else {
            return Err(Error::Command("invalid partition transaction record"));
        };
        if coordinator.as_slice() != input.coordinator_cell {
            return Ok(Json(ReadPartitionTransactionOutcome::CoordinatorMismatch));
        }
        let outcome = match state {
            0 => ReadPartitionTransactionOutcome::Prepared,
            1 => ReadPartitionTransactionOutcome::Committed,
            2 => ReadPartitionTransactionOutcome::Aborted,
            _ => return Err(Error::Command("invalid partition transaction state")),
        };
        Ok(Json(outcome))
    }
}
