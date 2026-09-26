//! Account participant staging and the same-Cell transaction path.

use super::*;
use crate::participant::{
    self, ParticipantTransactionState, PrepareTransactionOutcome, ReadTransactionInput,
    ResolveTransactionInput, ResolveTransactionOutcome,
};

#[derive(Serialize, Deserialize)]
struct StagedImage {
    table_id: String,
    key: Vec<u8>,
    image: Option<Item>,
    write: bool,
}

fn stage(
    context: &mut CommandContext<'_, '_>,
    operations: Vec<TransactionWrite>,
) -> Result<std::result::Result<Vec<StagedImage>, (usize, TransactionFailure)>> {
    if operations.is_empty() || operations.len() > 100 {
        return Ok(Err((
            0,
            TransactionFailure::Validation("transaction operation count is outside 1..=100".into()),
        )));
    }
    let mut touched = HashSet::with_capacity(operations.len());
    let mut staged = Vec::with_capacity(operations.len());
    for (index, operation) in operations.into_iter().enumerate() {
        let invalid = |message: &str| (index, TransactionFailure::Validation(message.into()));
        let (name, table_id, item, condition) = match &operation {
            TransactionWrite::Put(input) => (
                &input.table_name,
                &input.table_id,
                &input.item,
                input.condition.as_ref(),
            ),
            TransactionWrite::Delete(input) => (
                &input.table_name,
                &input.table_id,
                &input.key,
                input.condition.as_ref(),
            ),
            TransactionWrite::Update(input) => (
                &input.table_name,
                &input.table_id,
                &input.key,
                input.condition.as_ref(),
            ),
            TransactionWrite::ConditionCheck(input) => (
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
            TransactionWrite::Put(_) => valid_item(item, &table),
            _ => valid_key(item, &table),
        };
        if !valid {
            return Ok(Err(invalid("item violates table schema")));
        }
        let key = item_key(item, &table.key_schema)?;
        if !touched.insert((table.id.clone(), key.clone())) {
            return Ok(Err(invalid(
                "more than one operation addresses the same item",
            )));
        }
        if key_locked(context, &table.id, &key)? {
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
        let (image, write) = match operation {
            TransactionWrite::Put(input) => (Some(input.item), true),
            TransactionWrite::Delete(_) => (None, true),
            TransactionWrite::Update(input) => {
                let mut new = old.unwrap_or(input.key);
                if let Err(reason) = input.update.apply(&mut new, &table.attribute_definitions) {
                    return Ok(Err(invalid(&reason)));
                }
                if !valid_item(&new, &table) || item_key(&new, &table.key_schema)? != key {
                    return Ok(Err(invalid("item violates table schema")));
                }
                (Some(new), true)
            }
            TransactionWrite::ConditionCheck(_) => (None, false),
        };
        staged.push(StagedImage {
            table_id: table.id,
            key,
            image,
            write,
        });
    }
    Ok(Ok(staged))
}

fn apply(context: &mut CommandContext<'_, '_>, staged: Vec<StagedImage>) -> Result<()> {
    for image in staged {
        if !image.write {
            continue;
        }
        if let Some(item) = image.image {
            context.sql(&statement(
                "INSERT INTO ddb_items (table_id, item_key, item) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(table_id, item_key) DO UPDATE SET item = excluded.item",
                vec![
                    SqlValue::Text(image.table_id),
                    SqlValue::Blob(image.key),
                    SqlValue::Blob(serde_json::to_vec(&item)?),
                ],
            ))?;
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
    operations: Vec<TransactionWrite>,
) -> Result<TransactionOutcome> {
    let staged = match stage(context, operations)? {
        Ok(staged) => staged,
        Err((index, reason)) => return Ok(TransactionOutcome::Rejected { index, reason }),
    };
    apply(context, staged)?;
    Ok(TransactionOutcome::Applied)
}

/// Prepare writes to unrouted tables in one account Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrepareAccountTransactionInput {
    pub transaction_id: [u8; 16],
    pub coordinator_cell: [u8; 32],
    pub operations: Vec<TransactionWrite>,
}

/// Persist account item images and locks without exposing any writes.
pub struct PrepareAccountTransaction;

impl Command for PrepareAccountTransaction {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 21;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PrepareAccountTransactionInput>;
    type Output = Json<PrepareTransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
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
        )?;
        for image in staged {
            context.sql(&statement("INSERT INTO ddb_account_transaction_locks (table_id, item_key, transaction_id) VALUES (?1, ?2, ?3)",
                vec![SqlValue::Text(image.table_id), SqlValue::Blob(image.key), SqlValue::Blob(input.transaction_id.to_vec())]))?;
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
    Ok(!context.sql(&lock_query(table_id, key))?[0].rows.is_empty())
}

pub(super) fn read_key_locked(
    context: &QueryContext<'_>,
    table_id: &str,
    key: &[u8],
) -> Result<bool> {
    Ok(!context.sql(&lock_query(table_id, key))?[0].rows.is_empty())
}

fn lock_query(table_id: &str, key: &[u8]) -> SqlBatch {
    statement(
        "SELECT 1 FROM ddb_account_transaction_locks WHERE table_id = ?1 AND item_key = ?2",
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
