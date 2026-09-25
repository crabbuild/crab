//! Atomic reads and writes within one routed data Cell.

use std::collections::HashSet;

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use extenddb_core::types::Item;
use serde::{Deserialize, Serialize};

use super::{
    AccessState, DATA_MODULE, Json, PartitionGetInput, Result, SqlValue, command_access,
    command_item, data_key_hash, decode_spec, item_key, query_access, statement, valid_item,
    valid_key, write_item,
};
use crate::items::{TransactionFailure, TransactionWrite, decode_item};

/// Ordered writes that must all address the same installed data Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionTransactWriteInput {
    /// Routed table identity.
    pub table_id: String,
    /// Data Cell epoch observed at routing time.
    pub epoch: u64,
    /// Put and Delete operations in request order.
    pub operations: Vec<TransactionWrite>,
}

/// Result of one partition-local transactional write.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionTransactWriteOutcome {
    /// Every write and its query index entry committed.
    Applied,
    /// No operation committed.
    Rejected {
        /// Position of the failing operation.
        index: usize,
        /// Validation or condition failure.
        reason: TransactionFailure,
    },
    /// The data Cell has no installed partition.
    NotInstalled,
    /// Table identity or routing epoch changed.
    StaleRoute,
    /// The source has been sealed for a split.
    Sealed,
    /// An import-only child is not yet serving.
    NotReady,
    /// A key is outside this partition's range.
    WrongPartition,
}

/// Commit a batch to one data Cell or roll back every staged write.
pub struct PartitionTransactWrite;

impl Command for PartitionTransactWrite {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 9;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionTransactWriteInput>;
    type Output = Json<PartitionTransactWriteOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(rejected(PartitionTransactWriteOutcome::NotInstalled));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(rejected(PartitionTransactWriteOutcome::StaleRoute));
        }
        match command_access(context)? {
            AccessState::Serving => {}
            AccessState::Sealed => return Ok(rejected(PartitionTransactWriteOutcome::Sealed)),
            AccessState::Importing => return Ok(rejected(PartitionTransactWriteOutcome::NotReady)),
        }
        if input.operations.is_empty() || input.operations.len() > 100 {
            return Ok(validation(
                0,
                "transaction operation count is outside 1..=100",
            ));
        }

        let mut touched = HashSet::with_capacity(input.operations.len());
        for (index, operation) in input.operations.into_iter().enumerate() {
            let (table_id, item, condition, delete) = match operation {
                TransactionWrite::Put(input) => {
                    (input.table_id, input.item, input.condition, false)
                }
                TransactionWrite::Delete(input) => {
                    (input.table_id, input.key, input.condition, true)
                }
            };
            if table_id != spec.table.id {
                return Ok(rejected(PartitionTransactWriteOutcome::StaleRoute));
            }
            if !(if delete {
                valid_key(&item, &spec.table)
            } else {
                valid_item(&item, &spec.table)
            }) {
                return Ok(validation(index, "item violates table schema"));
            }
            let key = item_key(&item, &spec.table.key_schema)?;
            if !spec.contains(data_key_hash(
                &spec.table.id,
                &item,
                &spec.table.key_schema,
            )?) {
                return Ok(rejected(PartitionTransactWriteOutcome::WrongPartition));
            }
            if !touched.insert(key.clone()) {
                return Ok(validation(
                    index,
                    "more than one operation addresses the same item",
                ));
            }
            if let Some(condition) = condition {
                let old = command_item(context, &key)?;
                let empty = Item::new();
                match condition.evaluate(old.as_ref().unwrap_or(&empty)) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Ok(rejected(PartitionTransactWriteOutcome::Rejected {
                            index,
                            reason: TransactionFailure::ConditionFailed(old),
                        }));
                    }
                    Err(reason) => return Ok(validation(index, &reason)),
                }
            }
            if delete {
                context.sql(&statement(
                    "DELETE FROM ddb_partition_items WHERE item_key = ?1",
                    vec![SqlValue::Blob(key)],
                ))?;
            } else {
                write_item(context, key, &item, &spec.table.key_schema)?;
            }
        }
        Ok(CommandResult::Success(Json(
            PartitionTransactWriteOutcome::Applied,
        )))
    }
}

fn rejected(
    outcome: PartitionTransactWriteOutcome,
) -> CommandResult<Json<PartitionTransactWriteOutcome>> {
    CommandResult::Rejected(Json(outcome))
}

fn validation(index: usize, message: &str) -> CommandResult<Json<PartitionTransactWriteOutcome>> {
    rejected(PartitionTransactWriteOutcome::Rejected {
        index,
        reason: TransactionFailure::Validation(message.into()),
    })
}

/// Result of one consistent read from a routed data Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionTransactGetOutcome {
    /// All keys were read from one Cell snapshot.
    Found(Vec<Option<Item>>),
    /// The request must contain between one and one hundred keys.
    InvalidCount,
    /// The Cell has no installed partition.
    NotInstalled,
    /// Table identity or routing epoch changed.
    StaleRoute,
    /// The source has been sealed for a split.
    Sealed,
    /// An import-only child is not yet serving.
    NotReady,
    /// A key is outside this partition's range.
    WrongPartition,
    /// The key violates the table schema.
    InvalidKey { index: usize },
}

/// Read several keys in one data Cell snapshot.
pub struct PartitionTransactGet;

impl Query for PartitionTransactGet {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 6;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<Vec<PartitionGetInput>>;
    type Output = Json<PartitionTransactGetOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if input.is_empty() || input.len() > 100 {
            return Ok(Json(PartitionTransactGetOutcome::InvalidCount));
        }
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(Json(PartitionTransactGetOutcome::NotInstalled));
        };
        match query_access(context)? {
            AccessState::Serving => {}
            AccessState::Sealed => return Ok(Json(PartitionTransactGetOutcome::Sealed)),
            AccessState::Importing => return Ok(Json(PartitionTransactGetOutcome::NotReady)),
        }
        let mut output = Vec::with_capacity(input.len());
        for (index, request) in input.into_iter().enumerate() {
            if spec.table.id != request.table_id || spec.epoch != request.epoch {
                return Ok(Json(PartitionTransactGetOutcome::StaleRoute));
            }
            if !valid_key(&request.key, &spec.table) {
                return Ok(Json(PartitionTransactGetOutcome::InvalidKey { index }));
            }
            let key = item_key(&request.key, &spec.table.key_schema)?;
            if !spec.contains(data_key_hash(
                &spec.table.id,
                &request.key,
                &spec.table.key_schema,
            )?) {
                return Ok(Json(PartitionTransactGetOutcome::WrongPartition));
            }
            let rows = context.sql(&statement(
                "SELECT item FROM ddb_partition_items WHERE item_key = ?1",
                vec![SqlValue::Blob(key)],
            ))?;
            output.push(decode_item(&rows[0])?);
        }
        Ok(Json(PartitionTransactGetOutcome::Found(output)))
    }
}
