//! Bounded ordered iteration over one data Cell's items.

use extenddb_core::types::{Item, extract_key, item_size_bytes};
use serde::{Deserialize, Serialize};

use super::{
    AccessState, DATA_MODULE, Json, Query, QueryContext, Result, SqlValue, decode_spec,
    query_access,
};
use crate::Error;
use crate::items::{item_key, valid_key};
use crate::table::statement;

/// One bounded scan of an independently owned data range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionScanInput {
    /// Immutable table identity.
    pub table_id: String,
    /// Source data Cell epoch used for routing.
    pub epoch: u64,
    /// Maximum number of items evaluated.
    pub limit: Option<u32>,
    /// Complete key after which to resume in primary-key order.
    pub exclusive_start_key: Option<Item>,
}

/// Result of scanning one data Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionScanOutcome {
    /// A bounded page from this Cell and a continuation if more items remain.
    Page {
        /// Items in canonical primary-key order.
        items: Vec<Item>,
        /// Last key evaluated when this Cell has more items.
        last_evaluated_key: Option<Item>,
    },
    /// No partition contract is installed.
    NotInstalled,
    /// The table ID or source Cell epoch is stale.
    StaleRoute,
    /// Ordinary reads are fenced after the source is sealed.
    Sealed,
    /// This split child is still importing its source range.
    NotReady,
    /// Export is only available after the source is sealed.
    NotSealed,
    /// The continuation key does not match the table schema.
    InvalidKey,
    /// Limit must be positive.
    InvalidLimit,
    /// An unvisited key has an unresolved transaction intent.
    Conflict(crate::TransactionReadConflict),
}

/// Scan a single range through a bounded Cell query.
pub struct PartitionScan;

impl Query for PartitionScan {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionScanInput>;
    type Output = Json<PartitionScanOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        scan_page(context, input, false)
    }
}

/// Read a stable, bounded page from a sealed source for split recovery.
pub struct PartitionExport;

impl Query for PartitionExport {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionScanInput>;
    type Output = Json<PartitionScanOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        scan_page(context, input, true)
    }
}

fn scan_page(
    context: &mut QueryContext<'_>,
    input: PartitionScanInput,
    export: bool,
) -> Result<Json<PartitionScanOutcome>> {
    let rows = context.sql(&statement(
        "SELECT spec FROM ddb_partition WHERE singleton = 1",
        vec![],
    ))?;
    let Some(spec) = decode_spec(&rows[0])? else {
        return Ok(Json(PartitionScanOutcome::NotInstalled));
    };
    if spec.table.id != input.table_id || spec.epoch != input.epoch {
        return Ok(Json(PartitionScanOutcome::StaleRoute));
    }
    match (query_access(context)?, export) {
        (AccessState::Serving, false) => {}
        (AccessState::Sealed, true) => {}
        (AccessState::Sealed, false) => return Ok(Json(PartitionScanOutcome::Sealed)),
        (AccessState::Importing, false) => return Ok(Json(PartitionScanOutcome::NotReady)),
        (_, true) => return Ok(Json(PartitionScanOutcome::NotSealed)),
    }
    if input.limit == Some(0) {
        return Ok(Json(PartitionScanOutcome::InvalidLimit));
    }
    let mut cursor = match input.exclusive_start_key {
        Some(key) if valid_key(&key, &spec.table) => item_key(&key, &spec.table.key_schema)?,
        Some(_) => return Ok(Json(PartitionScanOutcome::InvalidKey)),
        None => Vec::new(),
    };
    // Check absent keys too; reading only live rows would skip committed but
    // unapplied creates. Sealed export is already fenced by SealPartition.
    if !export {
        let locks = context.sql(&statement(
            "SELECT transaction_id FROM ddb_partition_transaction_locks WHERE item_key > ?1 AND write_lock = 1 LIMIT 1",
            vec![SqlValue::Blob(cursor.clone())],
        ))?;
        if let Some(conflict) = crate::participant::read_conflict(context, &locks[0])? {
            return Ok(Json(PartitionScanOutcome::Conflict(conflict)));
        }
    }
    let max_items = input.limit.unwrap_or(u32::MAX).min(10_000) as usize;
    let mut items = Vec::new();
    let mut bytes = 0_usize;
    let mut stopped = false;
    loop {
        let rows = context.sql(&statement(
            "SELECT item_key FROM ddb_partition_items WHERE item_key > ?1 \
                 ORDER BY item_key LIMIT 64",
            vec![SqlValue::Blob(cursor.clone())],
        ))?;
        let key_rows = &rows[0].rows;
        if key_rows.is_empty() {
            break;
        }
        for row in key_rows {
            let [SqlValue::Blob(key)] = row.as_slice() else {
                return Err(Error::Command("invalid partition scan key row"));
            };
            let Some(item) =
                crate::item_storage::StoredItem::Partition(key).read(|batch| context.sql(batch))?
            else {
                return Err(Error::Command("partition scan key has no item"));
            };
            let encoded_bytes = serde_json::to_vec(&item)?.len();
            let next_bytes = bytes
                .saturating_add(encoded_bytes.max(item_size_bytes(&item)))
                .saturating_add(key.len() + 64);
            // Reserve wire framing space below the 1 MiB DynamoDB page limit.
            if !items.is_empty() && next_bytes > 900_000 {
                stopped = true;
                break;
            }
            bytes = next_bytes;
            cursor = key.clone();
            items.push(item);
            if items.len() >= max_items {
                stopped = true;
                break;
            }
        }
        if stopped || key_rows.len() < 64 {
            break;
        }
    }
    let last_evaluated_key = if stopped {
        let remaining = context.sql(&statement(
            "SELECT 1 FROM ddb_partition_items WHERE item_key > ?1 LIMIT 1",
            vec![SqlValue::Blob(cursor)],
        ))?;
        if remaining[0].rows.is_empty() {
            None
        } else {
            items
                .last()
                .map(|item| extract_key(item, &spec.table.key_schema))
        }
    } else {
        None
    };
    Ok(Json(PartitionScanOutcome::Page {
        items,
        last_evaluated_key,
    }))
}
