//! Durable data Cell participant intents for cross-Cell transactions.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::super::{
    AccessState, DATA_MODULE, Error, Json, Result, SqlValue, command_access, decode_spec, statement,
};
use super::{StagedImage, apply_staged, stage_operations};
use crate::participant::StagedEffect;
use crate::participant::{
    ParticipantTransactionState, PrepareTransactionOutcome, ReadTransactionInput,
    ResolveTransactionInput, ResolveTransactionOutcome,
};

#[derive(Serialize, Deserialize)]
struct PreparedPartition {
    table_id: String,
    epoch: u64,
    images: Vec<StagedImage>,
}
use crate::items::{TransactionFailure, TransactionOperation};

/// Prepare a local subset of a cross-Cell transaction without exposing writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreparePartitionTransactionInput {
    pub table_id: String,
    pub epoch: u64,
    pub transaction_id: [u8; 16],
    pub coordinator_cell: [u8; 32],
    pub coordinator_key: Vec<u8>,
    pub operations: Vec<TransactionOperation>,
}

/// Persist staged images and shared or exclusive locks in one published command.
pub struct PreparePartitionTransaction;

impl crate::MultipartTransactionCommand for PreparePartitionTransaction {
    type Payload = PreparePartitionTransactionInput;
    const UPLOAD_COMMAND_ID: u32 = 14;
}

impl Command for PreparePartitionTransaction {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<crate::TransactionPayloadRef>;
    type Output = Json<PrepareTransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let input = crate::transaction_transport::consume::<Self>(context, input)?;
        let digest = blake3::hash(&serde_json::to_vec(&input)?);
        if let Some(outcome) = crate::participant::prepared(
            context,
            input.transaction_id,
            input.coordinator_cell,
            digest,
        )? {
            return Ok(prepare_rejected(outcome));
        }
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(prepare_rejected(PrepareTransactionOutcome::NotInstalled));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(prepare_rejected(PrepareTransactionOutcome::StaleRoute));
        }
        match command_access(context)? {
            AccessState::Serving => {}
            AccessState::Sealed => {
                return Ok(prepare_rejected(PrepareTransactionOutcome::Sealed));
            }
            AccessState::Importing => {
                return Ok(prepare_rejected(PrepareTransactionOutcome::NotReady));
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
        let prepared = PreparedPartition {
            table_id: input.table_id,
            epoch: input.epoch,
            images: staged,
        };
        crate::participant::record_prepare(
            context,
            input.transaction_id,
            input.coordinator_cell,
            digest,
            crate::participant::PreparedPayload {
                bytes: serde_json::to_vec(&prepared)?,
                operations: prepared.images.len(),
                index_edits: prepared
                    .images
                    .iter()
                    .map(|image| image.index_capacity.edits)
                    .sum(),
                index_overflow_bytes: prepared
                    .images
                    .iter()
                    .map(|image| image.index_capacity.overflow_bytes)
                    .sum(),
            },
            &input.coordinator_key,
            prepared
                .images
                .iter()
                .filter(|image| image.effect == StagedEffect::Read)
                .map(|image| image.image.as_ref()),
        )?;
        for image in prepared.images {
            context.sql(&statement(
                "INSERT OR IGNORE INTO ddb_partition_transaction_locks \
                 (item_key, partition_key, sort_key, transaction_id, write_lock) VALUES (?1, ?2, ?3, ?4, ?5)",
                vec![
                    SqlValue::Blob(image.key),
                    SqlValue::Blob(image.partition_key),
                    SqlValue::Blob(image.sort_key),
                    SqlValue::Blob(input.transaction_id.to_vec()),
                    SqlValue::Integer(i64::from(image.effect != StagedEffect::Read)),
                ],
            ))?;
        }
        Ok(CommandResult::Success(Json(
            PrepareTransactionOutcome::Prepared,
        )))
    }
}

fn prepare_rejected(
    outcome: PrepareTransactionOutcome,
) -> CommandResult<Json<PrepareTransactionOutcome>> {
    CommandResult::Rejected(Json(outcome))
}

fn prepare_validation(
    index: usize,
    message: &str,
) -> CommandResult<Json<PrepareTransactionOutcome>> {
    prepare_rejected(PrepareTransactionOutcome::Rejected {
        index,
        reason: TransactionFailure::Validation(message.into()),
    })
}

/// Apply one durable decision and release every local key lock atomically.
pub struct ResolvePartitionTransaction;

impl Command for ResolvePartitionTransaction {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 13;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ResolveTransactionInput>;
    type Output = Json<ResolveTransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        crate::participant::resolve(context, input.clone(), |context, staged| {
            if let Some(bytes) = staged {
                let prepared: PreparedPartition = serde_json::from_slice(bytes)?;
                let rows = context.sql(&statement(
                    "SELECT spec FROM ddb_partition WHERE singleton = 1",
                    vec![],
                ))?;
                let Some(spec) = decode_spec(&rows[0])? else {
                    return Err(Error::Command("prepared partition is missing"));
                };
                if prepared.table_id != spec.table.id || prepared.epoch != spec.epoch {
                    return Err(Error::Command("prepared partition identity changed"));
                }
                apply_staged(context, &spec.table, prepared.images)?;
            }
            context.sql(&statement(
                "DELETE FROM ddb_partition_transaction_locks WHERE transaction_id = ?1",
                vec![SqlValue::Blob(input.transaction_id.to_vec())],
            ))?;
            Ok(())
        })
    }
}

/// Read a participant's durable state after an ambiguous phase reply.
pub struct ReadPartitionTransaction;

impl Query for ReadPartitionTransaction {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ReadTransactionInput>;
    type Output = Json<ParticipantTransactionState>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        crate::participant::read(context, input)
    }
}

/// Read one committed immutable image after a transactional read releases locks.
pub struct ReadPartitionTransactionResult;
impl Query for ReadPartitionTransactionResult {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 11;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<crate::ReadTransactionResultInput>;
    type Output = Json<crate::TransactionReadResult>;
    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        crate::participant::read_result(context, input)
    }
}
