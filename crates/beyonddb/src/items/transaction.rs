//! Account participant staging and the same-Cell transaction path.

use super::*;
use crate::participant::StagedEffect;
use crate::participant::{
    self, ParticipantTransactionState, PrepareTransactionOutcome, ReadTransactionInput,
    ResolveTransactionInput, ResolveTransactionOutcome,
};

#[derive(Serialize, Deserialize)]
struct StagedImage {
    table_id: String,
    key: Vec<u8>,
    image: Option<Item>,
    effect: StagedEffect,
}

fn stage(
    context: &mut CommandContext<'_, '_>,
    operations: Vec<TransactionOperation>,
) -> Result<std::result::Result<Vec<StagedImage>, (usize, TransactionFailure)>> {
    if operations.is_empty() || operations.len() > 100 {
        return Ok(Err((
            0,
            TransactionFailure::Validation("transaction operation count is outside 1..=100".into()),
        )));
    }
    let mut touched = HashSet::with_capacity(operations.len());
    let mut staged = Vec::with_capacity(operations.len());
    let mut read_bytes = 0;
    for (index, operation) in operations.into_iter().enumerate() {
        let invalid = |message: &str| (index, TransactionFailure::Validation(message.into()));
        let shared = matches!(operation, TransactionOperation::Read(_));
        let (name, table_id, item, condition) = match &operation {
            TransactionOperation::Read(input) => {
                (&input.table_name, &input.table_id, &input.key, None)
            }
            TransactionOperation::Put(input) => (
                &input.table_name,
                &input.table_id,
                &input.item,
                input.condition.as_ref(),
            ),
            TransactionOperation::Delete(input) => (
                &input.table_name,
                &input.table_id,
                &input.key,
                input.condition.as_ref(),
            ),
            TransactionOperation::Update(input) => (
                &input.table_name,
                &input.table_id,
                &input.key,
                input.condition.as_ref(),
            ),
            TransactionOperation::ConditionCheck(input) => (
                &input.table_name,
                &input.table_id,
                &input.key,
                Some(&input.condition),
            ),
        };
        let Some(table) = command_unrouted_table(context, name)? else {
            return Ok(Err(invalid("table does not exist or has a data route")));
        };
        if table.id != *table_id {
            return Ok(Err(invalid("table identity is stale")));
        }
        let valid = match operation {
            TransactionOperation::Put(_) => valid_item(item, &table),
            _ => valid_key(item, &table),
        };
        if !valid {
            return Ok(Err(invalid("item violates table schema")));
        }
        let key = item_key(item, &table.key_schema)?;
        if !touched.insert((table.id.clone(), key.clone())) && !shared {
            return Ok(Err(invalid(
                "more than one operation addresses the same item",
            )));
        }
        if !context.sql(&lock_query(&table.id, &key, shared))?[0]
            .rows
            .is_empty()
        {
            return Ok(Err((index, TransactionFailure::Conflict)));
        }
        let old = command_item(context, &table.id, &key)?;
        if let Some(condition) = condition {
            let empty = Item::new();
            match condition.evaluate(old.as_ref().unwrap_or(&empty)) {
                Ok(true) => {}
                Ok(false) => return Ok(Err((index, TransactionFailure::ConditionFailed(old)))),
                Err(reason) => return Ok(Err(invalid(&reason))),
            }
        }
        let (image, effect) = match operation {
            TransactionOperation::Put(input) => (Some(input.item), StagedEffect::Write),
            TransactionOperation::Delete(_) => (None, StagedEffect::Write),
            TransactionOperation::Update(input) => {
                let mut new = old.unwrap_or(input.key);
                if let Err(reason) = input.update.apply(&mut new, &table.attribute_definitions) {
                    return Ok(Err(invalid(&reason)));
                }
                if !valid_item(&new, &table) || item_key(&new, &table.key_schema)? != key {
                    return Ok(Err(invalid("item violates table schema")));
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
                return Ok(Err(invalid("transaction read exceeds 4 MiB")));
            }
        }
        staged.push(StagedImage {
            table_id: table.id,
            key,
            image,
            effect,
        });
    }
    Ok(Ok(staged))
}

fn apply(context: &mut CommandContext<'_, '_>, staged: Vec<StagedImage>) -> Result<()> {
    for image in staged {
        if image.effect != StagedEffect::Write {
            continue;
        }
        if let Some(item) = image.image {
            write_item(context, &image.table_id, &image.key, &item)?;
        } else {
            context.sql(&statement(
                "DELETE FROM ddb_items WHERE table_id = ?1 AND item_key = ?2",
                vec![SqlValue::Text(image.table_id), SqlValue::Blob(image.key)],
            ))?;
        }
    }
    Ok(())
}

pub(super) fn write(
    context: &mut CommandContext<'_, '_>,
    operations: Vec<TransactionOperation>,
) -> Result<TransactionOutcome> {
    let staged = match stage(context, operations)? {
        Ok(staged) => staged,
        Err((index, reason)) => return Ok(TransactionOutcome::Rejected { index, reason }),
    };
    apply(context, staged)?;
    Ok(TransactionOutcome::Applied)
}

/// Prepare read or write operations on unrouted tables in one account Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrepareAccountTransactionInput {
    pub transaction_id: [u8; 16],
    pub coordinator_cell: [u8; 32],
    pub coordinator_key: Vec<u8>,
    pub operations: Vec<TransactionOperation>,
}

/// Persist account item images and locks without exposing any writes.
pub struct PrepareAccountTransaction;

impl crate::MultipartTransactionCommand for PrepareAccountTransaction {
    type Payload = PrepareAccountTransactionInput;
    const UPLOAD_COMMAND_ID: u32 = 23;
}

impl Command for PrepareAccountTransaction {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 21;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<crate::TransactionPayloadRef>;
    type Output = Json<PrepareTransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let input = crate::transaction_transport::consume::<Self>(context, input)?;
        let digest = blake3::hash(&serde_json::to_vec(&input)?);
        if let Some(outcome) = participant::prepared(
            context,
            input.transaction_id,
            input.coordinator_cell,
            digest,
        )? {
            return Ok(CommandResult::Rejected(Json(outcome)));
        }
        let staged = match stage(context, input.operations)? {
            Ok(staged) => staged,
            Err((index, reason)) => {
                return Ok(CommandResult::Rejected(Json(
                    PrepareTransactionOutcome::Rejected { index, reason },
                )));
            }
        };
        participant::record_prepare(
            context,
            input.transaction_id,
            input.coordinator_cell,
            digest,
            serde_json::to_vec(&staged)?,
            &input.coordinator_key,
            staged
                .iter()
                .filter(|image| image.effect == StagedEffect::Read)
                .map(|image| image.image.as_ref()),
        )?;
        for image in staged {
            context.sql(&statement("INSERT OR IGNORE INTO ddb_account_transaction_locks (table_id, item_key, transaction_id, write_lock) VALUES (?1, ?2, ?3, ?4)",
                vec![SqlValue::Text(image.table_id), SqlValue::Blob(image.key), SqlValue::Blob(input.transaction_id.to_vec()), SqlValue::Integer(i64::from(image.effect != StagedEffect::Read))]))?;
        }
        Ok(CommandResult::Success(Json(
            PrepareTransactionOutcome::Prepared,
        )))
    }
}

/// Apply a coordinator decision and release the account participant's locks atomically.
pub struct ResolveAccountTransaction;

impl Command for ResolveAccountTransaction {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 22;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ResolveTransactionInput>;
    type Output = Json<ResolveTransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        participant::resolve(context, input.clone(), |context, staged| {
            if let Some(bytes) = staged {
                apply(context, serde_json::from_slice(bytes)?)?;
            }
            context.sql(&statement(
                "DELETE FROM ddb_account_transaction_locks WHERE transaction_id = ?1",
                vec![SqlValue::Blob(input.transaction_id.to_vec())],
            ))?;
            Ok(())
        })
    }
}

/// Read the durable account participant state after an uncertain phase reply.
pub struct ReadAccountTransaction;

impl Query for ReadAccountTransaction {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 25;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ReadTransactionInput>;
    type Output = Json<ParticipantTransactionState>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        participant::read(context, input)
    }
}

pub(super) fn key_locked(
    context: &CommandContext<'_, '_>,
    table_id: &str,
    key: &[u8],
) -> Result<bool> {
    Ok(!context.sql(&lock_query(table_id, key, false))?[0]
        .rows
        .is_empty())
}

pub(super) fn read_key_conflict(
    context: &QueryContext<'_>,
    table_id: &str,
    key: &[u8],
) -> Result<Option<crate::TransactionReadConflict>> {
    crate::participant::read_conflict(context, &context.sql(&lock_query(table_id, key, true))?[0])
}

fn lock_query(table_id: &str, key: &[u8], write_only: bool) -> SqlBatch {
    statement(
        if write_only {
            "SELECT transaction_id FROM ddb_account_transaction_locks WHERE table_id = ?1 AND item_key = ?2 AND write_lock = 1 LIMIT 1"
        } else {
            "SELECT 1 FROM ddb_account_transaction_locks WHERE table_id = ?1 AND item_key = ?2"
        },
        vec![
            SqlValue::Text(table_id.to_owned()),
            SqlValue::Blob(key.to_vec()),
        ],
    )
}

pub(crate) fn table_locked(context: &CommandContext<'_, '_>, table_id: &str) -> Result<bool> {
    Ok(!context.sql(&statement(
        "SELECT 1 FROM ddb_account_transaction_locks WHERE table_id = ?1 LIMIT 1",
        vec![SqlValue::Text(table_id.to_owned())],
    ))?[0]
        .rows
        .is_empty())
}

/// Read one committed immutable image after a transactional read releases locks.
pub struct ReadAccountTransactionResult;
impl Query for ReadAccountTransactionResult {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 27;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<crate::ReadTransactionResultInput>;
    type Output = Json<crate::TransactionReadResult>;
    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        crate::participant::read_result(context, input)
    }
}
