//! Atomic reads and writes within one routed data Cell.

use crate::participant::StagedEffect;
use std::collections::HashSet;

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use extenddb_core::types::{Item, KeySchemaElement};
use serde::{Deserialize, Serialize};

use super::{
    AccessState, DATA_MODULE, Json, PartitionGetInput, PartitionSpec, Result, SqlValue,
    command_access, command_item, data_key_hash, decode_spec, item_key, query_access, statement,
    valid_item, valid_key, write_item,
};
use crate::PrepareTransactionOutcome;
use crate::items::{TransactionFailure, TransactionOperation};

/// Ordered writes that must all address the same installed data Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionTransactWriteInput {
    /// Routed table identity.
    pub table_id: String,
    /// Data Cell epoch observed at routing time.
    pub epoch: u64,
    /// Item writes and condition checks in request order.
    pub operations: Vec<TransactionOperation>,
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
        if spec.table.id != input.table_id {
            return Ok(rejected(PartitionTransactWriteOutcome::StaleRoute));
        }
        if spec.epoch != input.epoch {
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

        let staged = match stage_operations(context, &spec, input.operations)? {
            Ok(staged) => staged,
            Err(reason) => return Ok(rejected(reason.single_outcome())),
        };
        apply_staged(context, &spec.table.key_schema, staged)?;
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StagedImage {
    key: Vec<u8>,
    partition_key: Vec<u8>,
    sort_key: Vec<u8>,
    image: Option<Item>,
    effect: StagedEffect,
}

enum StageError {
    StaleRoute,
    WrongPartition,
    Rejected {
        index: usize,
        reason: TransactionFailure,
    },
}

impl StageError {
    fn single_outcome(self) -> PartitionTransactWriteOutcome {
        match self {
            Self::StaleRoute => PartitionTransactWriteOutcome::StaleRoute,
            Self::WrongPartition => PartitionTransactWriteOutcome::WrongPartition,
            Self::Rejected { index, reason } => {
                PartitionTransactWriteOutcome::Rejected { index, reason }
            }
        }
    }

    fn prepare_outcome(self) -> PrepareTransactionOutcome {
        match self {
            Self::StaleRoute => PrepareTransactionOutcome::StaleRoute,
            Self::WrongPartition => PrepareTransactionOutcome::WrongPartition,
            Self::Rejected { index, reason } => {
                PrepareTransactionOutcome::Rejected { index, reason }
            }
        }
    }
}

fn stage_operations(
    context: &mut CommandContext<'_, '_>,
    spec: &PartitionSpec,
    operations: Vec<TransactionOperation>,
) -> Result<std::result::Result<Vec<StagedImage>, StageError>> {
    let mut touched = HashSet::with_capacity(operations.len());
    let mut staged = Vec::with_capacity(operations.len());
    let mut read_bytes = 0;
    for (index, operation) in operations.into_iter().enumerate() {
        let shared = matches!(operation, TransactionOperation::Read(_));
        let (table_id, item, condition) = match &operation {
            TransactionOperation::Read(input) => (&input.table_id, &input.key, None),
            TransactionOperation::Put(input) => {
                (&input.table_id, &input.item, input.condition.as_ref())
            }
            TransactionOperation::Delete(input) => {
                (&input.table_id, &input.key, input.condition.as_ref())
            }
            TransactionOperation::Update(input) => {
                (&input.table_id, &input.key, input.condition.as_ref())
            }
            TransactionOperation::ConditionCheck(input) => {
                (&input.table_id, &input.key, Some(&input.condition))
            }
        };
        if *table_id != spec.table.id {
            return Ok(Err(StageError::StaleRoute));
        }
        let valid = match operation {
            TransactionOperation::Put(_) => valid_item(item, &spec.table),
            _ => valid_key(item, &spec.table),
        };
        if !valid {
            return Ok(Err(stage_validation(index, "item violates table schema")));
        }
        let key = item_key(item, &spec.table.key_schema)?;
        if !spec.contains(data_key_hash(&spec.table.id, item, &spec.table.key_schema)?) {
            return Ok(Err(StageError::WrongPartition));
        }
        if !touched.insert(key.clone()) && !shared {
            return Ok(Err(stage_validation(
                index,
                "more than one operation addresses the same item",
            )));
        }
        if !context.sql(&lock_query(&key, shared))?[0].rows.is_empty() {
            return Ok(Err(StageError::Rejected {
                index,
                reason: TransactionFailure::Conflict,
            }));
        }
        let old = command_item(context, &key)?;
        if let Some(condition) = condition {
            let empty = Item::new();
            match condition.evaluate(old.as_ref().unwrap_or(&empty)) {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(Err(StageError::Rejected {
                        index,
                        reason: TransactionFailure::ConditionFailed(old),
                    }));
                }
                Err(reason) => return Ok(Err(stage_validation(index, &reason))),
            }
        }
        let (partition_key, sort_key) = super::key::index_key(item, &spec.table.key_schema)?;
        let (image, effect) = match operation {
            TransactionOperation::Put(input) => (Some(input.item), StagedEffect::Write),
            TransactionOperation::Delete(_) => (None, StagedEffect::Write),
            TransactionOperation::Update(input) => {
                let mut new = old.unwrap_or(input.key);
                if let Err(reason) = input
                    .update
                    .apply(&mut new, &spec.table.attribute_definitions)
                {
                    return Ok(Err(stage_validation(index, &reason)));
                }
                if !valid_item(&new, &spec.table) || item_key(&new, &spec.table.key_schema)? != key
                {
                    return Ok(Err(stage_validation(index, "item violates table schema")));
                }
                (Some(new), StagedEffect::Write)
            }
            TransactionOperation::ConditionCheck(_) => (None, StagedEffect::Check),
            TransactionOperation::Read(_) => (old, StagedEffect::Read),
        };
        if effect == StagedEffect::Read {
            read_bytes += image
                .as_ref()
                .map_or(0, extenddb_core::types::item_size_bytes);
            if read_bytes > 4 * 1024 * 1024 {
                return Ok(Err(stage_validation(
                    index,
                    "transaction read exceeds 4 MiB",
                )));
            }
        }
        staged.push(StagedImage {
            key,
            partition_key,
            sort_key,
            image,
            effect,
        });
    }
    Ok(Ok(staged))
}

fn stage_validation(index: usize, message: &str) -> StageError {
    StageError::Rejected {
        index,
        reason: TransactionFailure::Validation(message.into()),
    }
}

fn apply_staged(
    context: &mut CommandContext<'_, '_>,
    schema: &[KeySchemaElement],
    staged: Vec<StagedImage>,
) -> Result<()> {
    for image in staged {
        if image.effect != StagedEffect::Write {
            continue;
        }
        if let Some(item) = image.image {
            write_item(context, image.key, &item, schema)?;
        } else {
            context.sql(&statement(
                "DELETE FROM ddb_partition_items WHERE item_key = ?1",
                vec![SqlValue::Blob(image.key)],
            ))?;
        }
    }
    Ok(())
}

mod participant;
pub use participant::*;

pub(super) fn key_locked(context: &mut CommandContext<'_, '_>, key: &[u8]) -> Result<bool> {
    Ok(!context.sql(&lock_query(key, false))?[0].rows.is_empty())
}

fn lock_query(key: &[u8], write_only: bool) -> crate::SqlBatch {
    statement(
        if write_only {
            "SELECT transaction_id FROM ddb_partition_transaction_locks WHERE item_key = ?1 AND write_lock = 1 LIMIT 1"
        } else {
            "SELECT 1 FROM ddb_partition_transaction_locks WHERE item_key = ?1"
        },
        vec![SqlValue::Blob(key.to_vec())],
    )
}

pub(super) fn has_transaction_locks(context: &mut CommandContext<'_, '_>) -> Result<bool> {
    let rows = context.sql(&statement(
        "SELECT 1 FROM ddb_partition_transaction_locks LIMIT 1",
        vec![],
    ))?;
    Ok(!rows[0].rows.is_empty())
}

pub(super) fn read_key_conflict(
    context: &QueryContext<'_>,
    key: &[u8],
) -> Result<Option<crate::TransactionReadConflict>> {
    crate::participant::read_conflict(context, &context.sql(&lock_query(key, true))?[0])
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
    /// A requested key has an unresolved transaction intent.
    Conflict { index: usize },
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
            if read_key_conflict(context, &key)?.is_some() {
                return Ok(Json(PartitionTransactGetOutcome::Conflict { index }));
            }
            output.push(
                crate::item_storage::StoredItem::Partition(&key)
                    .read(|batch| context.sql(batch))?,
            );
        }
        Ok(Json(PartitionTransactGetOutcome::Found(output)))
    }
}
