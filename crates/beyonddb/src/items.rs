use super::expression_wire::{WireCondition, WireUpdate};
use super::table::{command_unrouted_table, query_unrouted_table, statement};
use super::*;
use extenddb_core::types::AttributeValue;

/// One item replacement in a named table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PutItemInput {
    /// Table to mutate.
    pub table_name: String,
    /// Immutable table identity from ExtendDB's catalog lookup.
    pub table_id: String,
    /// Full DynamoDB item.
    pub item: Item,
    /// Condition evaluated against the previous item in the same transaction.
    pub condition: Option<WireCondition>,
}

/// Outcome of a keyed item mutation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ItemMutationOutcome {
    /// An unresolved transaction holds this item's lock.
    Conflict,
    /// The mutation committed; this is the previous item if present.
    Applied(Option<Item>),
    /// The table does not exist.
    TableNotFound,
    /// The key or item violates the table contract.
    InvalidItem,
    /// The condition was false; the prior item may be returned by the caller.
    ConditionFailed(Option<Item>),
    /// The parsed expression could not be evaluated.
    InvalidExpression(String),
}

/// Replace an item through one durable Cell command.
pub struct PutItem;

impl Command for PutItem {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PutItemInput>;
    type Output = Json<ItemMutationOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(table) = command_unrouted_table(context, &input.table_name)? else {
            return Ok(CommandResult::Rejected(Json(
                ItemMutationOutcome::TableNotFound,
            )));
        };
        if table.id != input.table_id {
            return Ok(CommandResult::Rejected(Json(
                ItemMutationOutcome::TableNotFound,
            )));
        }
        if !valid_item(&input.item, &table) {
            return Ok(CommandResult::Rejected(Json(
                ItemMutationOutcome::InvalidItem,
            )));
        }
        let key = item_key(&input.item, &table.key_schema)?;
        if transaction::key_locked(context, &table.id, &key)? {
            return Ok(CommandResult::Rejected(Json(ItemMutationOutcome::Conflict)));
        }
        let old = command_item(context, &table.id, &key)?;
        if let Some(condition) = input.condition {
            let empty = Item::new();
            match condition.evaluate(old.as_ref().unwrap_or(&empty)) {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(CommandResult::Rejected(Json(
                        ItemMutationOutcome::ConditionFailed(old),
                    )));
                }
                Err(message) => {
                    return Ok(CommandResult::Rejected(Json(
                        ItemMutationOutcome::InvalidExpression(message),
                    )));
                }
            }
        }
        context.sql(&statement(
            "INSERT INTO ddb_items (table_id, item_key, item) VALUES (?1, ?2, ?3) \
             ON CONFLICT(table_id, item_key) DO UPDATE SET item = excluded.item",
            vec![
                SqlValue::Text(table.id),
                SqlValue::Blob(key),
                SqlValue::Blob(serde_json::to_vec(&input.item)?),
            ],
        ))?;
        Ok(CommandResult::Success(Json(ItemMutationOutcome::Applied(
            old,
        ))))
    }
}

/// One item deletion in a named table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeleteItemInput {
    /// Table to mutate.
    pub table_name: String,
    /// Immutable table identity from ExtendDB's catalog lookup.
    pub table_id: String,
    /// Complete primary key.
    pub key: Item,
    /// Condition evaluated against the previous item in the same transaction.
    pub condition: Option<WireCondition>,
}

/// Delete an item through one durable Cell command.
pub struct DeleteItem;

impl Command for DeleteItem {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DeleteItemInput>;
    type Output = Json<ItemMutationOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(table) = command_unrouted_table(context, &input.table_name)? else {
            return Ok(CommandResult::Rejected(Json(
                ItemMutationOutcome::TableNotFound,
            )));
        };
        if table.id != input.table_id {
            return Ok(CommandResult::Rejected(Json(
                ItemMutationOutcome::TableNotFound,
            )));
        }
        if !valid_key(&input.key, &table) {
            return Ok(CommandResult::Rejected(Json(
                ItemMutationOutcome::InvalidItem,
            )));
        }
        let key = item_key(&input.key, &table.key_schema)?;
        if transaction::key_locked(context, &table.id, &key)? {
            return Ok(CommandResult::Rejected(Json(ItemMutationOutcome::Conflict)));
        }
        let old = command_item(context, &table.id, &key)?;
        if let Some(condition) = input.condition {
            let empty = Item::new();
            match condition.evaluate(old.as_ref().unwrap_or(&empty)) {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(CommandResult::Rejected(Json(
                        ItemMutationOutcome::ConditionFailed(old),
                    )));
                }
                Err(message) => {
                    return Ok(CommandResult::Rejected(Json(
                        ItemMutationOutcome::InvalidExpression(message),
                    )));
                }
            }
        }
        context.sql(&statement(
            "DELETE FROM ddb_items WHERE table_id = ?1 AND item_key = ?2",
            vec![SqlValue::Text(table.id), SqlValue::Blob(key)],
        ))?;
        Ok(CommandResult::Success(Json(ItemMutationOutcome::Applied(
            old,
        ))))
    }
}

/// One atomic item update in a named table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpdateItemInput {
    /// Table to mutate.
    pub table_name: String,
    /// Immutable table identity from ExtendDB's catalog lookup.
    pub table_id: String,
    /// Complete primary key.
    pub key: Item,
    pub(crate) update: WireUpdate,
    pub(crate) condition: Option<WireCondition>,
}

/// Result of applying an item update.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UpdateItemOutcome {
    /// An unresolved transaction holds this item's lock.
    Conflict,
    /// The update committed with both item images.
    Applied { old: Option<Item>, new: Item },
    /// The table does not exist.
    TableNotFound,
    /// The key or resulting item violates the table contract.
    InvalidItem,
    /// The condition was false against the previous item.
    ConditionFailed(Option<Item>),
    /// The parsed expression could not be evaluated.
    InvalidExpression(String),
}

/// Apply an ExtendDB update expression in one durable Cell command.
pub struct UpdateItem;

impl Command for UpdateItem {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 9;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<UpdateItemInput>;
    type Output = Json<UpdateItemOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(table) = command_unrouted_table(context, &input.table_name)? else {
            return Ok(CommandResult::Rejected(Json(
                UpdateItemOutcome::TableNotFound,
            )));
        };
        if table.id != input.table_id {
            return Ok(CommandResult::Rejected(Json(
                UpdateItemOutcome::TableNotFound,
            )));
        }
        if !valid_key(&input.key, &table) {
            return Ok(CommandResult::Rejected(Json(
                UpdateItemOutcome::InvalidItem,
            )));
        }
        let key = item_key(&input.key, &table.key_schema)?;
        if transaction::key_locked(context, &table.id, &key)? {
            return Ok(CommandResult::Rejected(Json(UpdateItemOutcome::Conflict)));
        }
        let old = command_item(context, &table.id, &key)?;
        if let Some(condition) = input.condition {
            let empty = Item::new();
            match condition.evaluate(old.as_ref().unwrap_or(&empty)) {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(CommandResult::Rejected(Json(
                        UpdateItemOutcome::ConditionFailed(old),
                    )));
                }
                Err(message) => {
                    return Ok(CommandResult::Rejected(Json(
                        UpdateItemOutcome::InvalidExpression(message),
                    )));
                }
            }
        }
        let mut new = old.clone().unwrap_or_else(|| input.key.clone());
        if let Err(message) = input.update.apply(&mut new, &table.attribute_definitions) {
            return Ok(CommandResult::Rejected(Json(
                UpdateItemOutcome::InvalidExpression(message),
            )));
        }
        if !valid_item(&new, &table) || item_key(&new, &table.key_schema)? != key {
            return Ok(CommandResult::Rejected(Json(
                UpdateItemOutcome::InvalidItem,
            )));
        }
        context.sql(&statement(
            "INSERT INTO ddb_items (table_id, item_key, item) VALUES (?1, ?2, ?3) \
             ON CONFLICT(table_id, item_key) DO UPDATE SET item = excluded.item",
            vec![
                SqlValue::Text(table.id),
                SqlValue::Blob(key),
                SqlValue::Blob(serde_json::to_vec(&new)?),
            ],
        ))?;
        Ok(CommandResult::Success(Json(UpdateItemOutcome::Applied {
            old,
            new,
        })))
    }
}

/// One keyed item read in a named table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GetItemInput {
    /// Table to read.
    pub table_name: String,
    /// Immutable table identity from ExtendDB's catalog lookup.
    pub table_id: String,
    /// Complete primary key.
    pub key: Item,
}

/// Result of reading an item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GetItemOutcome {
    /// An unresolved transaction holds this item's lock.
    Conflict,
    /// The table exists; the item may be absent.
    Found(Option<Item>),
    /// The table does not exist.
    TableNotFound,
    /// The key violates the table contract.
    InvalidKey,
}

/// Read an item from the current Cell owner.
pub struct GetItem;

impl Query for GetItem {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GetItemInput>;
    type Output = Json<GetItemOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let Some(table) = query_unrouted_table(context, &input.table_name)? else {
            return Ok(Json(GetItemOutcome::TableNotFound));
        };
        if table.id != input.table_id {
            return Ok(Json(GetItemOutcome::TableNotFound));
        }
        if !valid_key(&input.key, &table) {
            return Ok(Json(GetItemOutcome::InvalidKey));
        }
        let key = item_key(&input.key, &table.key_schema)?;
        if transaction::read_key_locked(context, &table.id, &key)? {
            return Ok(Json(GetItemOutcome::Conflict));
        }
        let rows = context.sql(&statement(
            "SELECT item FROM ddb_items WHERE table_id = ?1 AND item_key = ?2",
            vec![SqlValue::Text(table.id), SqlValue::Blob(key)],
        ))?;
        Ok(Json(GetItemOutcome::Found(decode_item(&rows[0])?)))
    }
}

/// A write staged with other account-local operations in one Cell transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransactionWrite {
    /// Replace the item at its primary key.
    Put(PutItemInput),
    /// Delete the item at its primary key.
    Delete(DeleteItemInput),
    /// Apply an update expression to one item.
    Update(UpdateItemInput),
    /// Check one item without mutating it.
    ConditionCheck(ConditionCheckInput),
}

/// A condition evaluated with other transaction operations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConditionCheckInput {
    /// Table to read.
    pub table_name: String,
    /// Immutable table identity from ExtendDB's catalog lookup.
    pub table_id: String,
    /// Complete primary key.
    pub key: Item,
    pub(crate) condition: WireCondition,
}

/// Account-local transactional writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransactWriteInput {
    /// Ordered operations, which may address distinct tables in this account.
    pub operations: Vec<TransactionWrite>,
}

/// Result of an account-local transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransactionOutcome {
    /// Every operation committed.
    Applied,
    /// Nothing committed; the operation at this position failed validation.
    Rejected {
        /// Position of the failing operation.
        index: usize,
        /// Failure and optional old item.
        reason: TransactionFailure,
    },
}

/// Why a transaction operation was rejected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransactionFailure {
    /// Another transaction or ownership transition conflicts with this operation.
    Conflict,
    /// Invalid input or stale table metadata.
    Validation(String),
    /// Condition false against the old item in the same transaction.
    ConditionFailed(Option<Item>),
}

/// Atomically commits writes to multiple tables in one account Cell.
pub struct TransactWrite;

impl Command for TransactWrite {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 5;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<TransactWriteInput>;
    type Output = Json<TransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let outcome = transaction::write(context, input.operations)?;
        if outcome != TransactionOutcome::Applied {
            return Ok(CommandResult::Rejected(Json(outcome)));
        }
        Ok(CommandResult::Success(Json(TransactionOutcome::Applied)))
    }
}

pub(crate) mod transaction;
pub use transaction::{
    PrepareAccountTransaction, PrepareAccountTransactionInput, ReadAccountTransaction,
    ResolveAccountTransaction,
};

mod scan;
pub use scan::*;

/// A consistent, account-local read of several keys across tables.
pub struct TransactGet;

/// Result of one all-or-error transactional read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransactionGetOutcome {
    /// An unresolved transaction holds one requested key.
    Conflict { index: usize },
    /// All requested keys were valid and read from one Cell snapshot.
    Found(Vec<Option<Item>>),
    /// The request must contain between one and one hundred keys.
    InvalidCount,
    /// A requested table does not exist.
    TableNotFound { index: usize },
    /// A requested key violates its table contract.
    InvalidKey { index: usize },
}

impl Query for TransactGet {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 6;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<Vec<GetItemInput>>;
    type Output = Json<TransactionGetOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if input.is_empty() || input.len() > 100 {
            return Ok(Json(TransactionGetOutcome::InvalidCount));
        }
        let mut output = Vec::with_capacity(input.len());
        for (index, request) in input.into_iter().enumerate() {
            let Some(table) = query_unrouted_table(context, &request.table_name)? else {
                return Ok(Json(TransactionGetOutcome::TableNotFound { index }));
            };
            if table.id != request.table_id {
                return Ok(Json(TransactionGetOutcome::TableNotFound { index }));
            }
            if !valid_key(&request.key, &table) {
                return Ok(Json(TransactionGetOutcome::InvalidKey { index }));
            }
            let key = item_key(&request.key, &table.key_schema)?;
            if transaction::read_key_locked(context, &table.id, &key)? {
                return Ok(Json(TransactionGetOutcome::Conflict { index }));
            }
            let rows = context.sql(&statement(
                "SELECT item FROM ddb_items WHERE table_id = ?1 AND item_key = ?2",
                vec![SqlValue::Text(table.id), SqlValue::Blob(key)],
            ))?;
            output.push(decode_item(&rows[0])?);
        }
        Ok(Json(TransactionGetOutcome::Found(output)))
    }
}

pub(crate) fn valid_item(item: &Item, table: &TableRecord) -> bool {
    let limits = LimitsConfig::default();
    validation::validate_item_keys(item, &table.key_schema, &table.attribute_definitions).is_ok()
        && validation::validate_attribute_name_sizes(item, &limits).is_ok()
        && validation::validate_item_numbers(item).is_ok()
        && validation::validate_item_nesting_depth(item).is_ok()
        && validation::validate_item_size(item, limits.max_item_size_bytes).is_ok()
        && validation::validate_key_sizes(item, &table.key_schema, &limits).is_ok()
}

pub(crate) fn valid_key(key: &Item, table: &TableRecord) -> bool {
    validation::validate_key_only(key, &table.key_schema, &table.attribute_definitions).is_ok()
        && validation::validate_key_sizes(key, &table.key_schema, &LimitsConfig::default()).is_ok()
}

fn command_item(
    context: &CommandContext<'_, '_>,
    table_id: &str,
    key: &[u8],
) -> Result<Option<Item>> {
    let rows = context.sql(&statement(
        "SELECT item FROM ddb_items WHERE table_id = ?1 AND item_key = ?2",
        vec![
            SqlValue::Text(table_id.to_owned()),
            SqlValue::Blob(key.to_vec()),
        ],
    ))?;
    decode_item(&rows[0])
}

pub(crate) fn decode_item(result: &SqlResultSet) -> Result<Option<Item>> {
    let Some(row) = result.rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Blob(item)] = row.as_slice() else {
        return Err(Error::Command("invalid account item row"));
    };
    Ok(Some(serde_json::from_slice(item)?))
}

pub(crate) fn item_key(item: &Item, key_schema: &[KeySchemaElement]) -> Result<Vec<u8>> {
    let mut key = extract_key(item, key_schema);
    for value in key.values_mut() {
        if let AttributeValue::N(number) = value {
            *number = number
                .parse::<bigdecimal::BigDecimal>()
                .map_err(|_| Error::Command("invalid numeric key"))?
                .normalized()
                .to_string();
        }
    }
    Ok(serde_json::to_vec(&key)?)
}
