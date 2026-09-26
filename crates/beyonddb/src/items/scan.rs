//! Bounded base-table Scan over account Cell items.

use super::*;
use extenddb_core::types::item_size_bytes;

/// One bounded base-table scan request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScanItemsInput {
    /// Table to scan.
    pub table_name: String,
    /// Immutable table identity from ExtendDB's catalog lookup.
    pub table_id: String,
    /// Local secondary index to scan, or the base table.
    pub index_name: Option<String>,
    /// Maximum items evaluated before returning a continuation.
    pub limit: Option<u32>,
    /// Complete key after which to resume.
    pub exclusive_start_key: Option<Item>,
}

/// Result of a bounded base-table scan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScanItemsOutcome {
    /// An unresolved intent intersects the unvisited scan range.
    Conflict(crate::TransactionReadConflict),
    /// Items in stable storage order and a key to resume if more remain.
    Page {
        /// Returned items.
        items: Vec<Item>,
        /// Last evaluated key when another page exists.
        last_evaluated_key: Option<Item>,
    },
    /// The table no longer exists.
    TableNotFound,
    /// The continuation key is invalid for this table.
    InvalidKey,
    /// Limit must be positive.
    InvalidLimit,
}

/// Scan one account table through a bounded Cell query.
pub struct ScanItems;

impl Query for ScanItems {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ScanItemsInput>;
    type Output = Json<ScanItemsOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let Some(table) = query_unrouted_table(context, &input.table_name)? else {
            return Ok(Json(ScanItemsOutcome::TableNotFound));
        };
        if table.id != input.table_id {
            return Ok(Json(ScanItemsOutcome::TableNotFound));
        }
        let index = match input.index_name.as_deref() {
            Some(name) => match table
                .local_secondary_indexes
                .iter()
                .find(|index| index.index_name == name)
            {
                Some(index) => Some(index),
                None => return Ok(Json(ScanItemsOutcome::InvalidKey)),
            },
            None => None,
        };
        let key_schema = index.map_or_else(
            || table.key_schema.clone(),
            |index| crate::secondary_index::key_schema(&table, index),
        );
        if input.limit == Some(0) {
            return Ok(Json(ScanItemsOutcome::InvalidLimit));
        }
        let mut cursor = match input.exclusive_start_key {
            Some(key)
                if index.map_or_else(
                    || valid_key(&key, &table),
                    |index| crate::secondary_index::valid_cursor(&key, &table, index),
                ) =>
            {
                item_key(&key, &table.key_schema)?
            }
            Some(_) => return Ok(Json(ScanItemsOutcome::InvalidKey)),
            None => Vec::new(),
        };
        // Missing live rows can still have prepared creates. Probe locks first
        // so Scan cannot skip those intents just because their live rows are absent.
        let locked = context.sql(&statement(
            "SELECT transaction_id FROM ddb_account_transaction_locks WHERE table_id = ?1 AND item_key > ?2 AND write_lock = 1 LIMIT 1",
            vec![SqlValue::Text(table.id.clone()), SqlValue::Blob(cursor.clone())],
        ))?;
        if let Some(conflict) = crate::participant::read_conflict(context, &locked[0])? {
            return Ok(Json(ScanItemsOutcome::Conflict(conflict)));
        }
        let max_items = input.limit.unwrap_or(u32::MAX).min(10_000) as usize;
        let mut items = Vec::new();
        let mut bytes = 0_usize;
        let mut stopped = false;
        loop {
            let key_rows = crate::secondary_index::scan_keys(
                context,
                &table.id,
                input.index_name.as_deref(),
                &cursor,
                false,
                64,
            )?;
            if key_rows.is_empty() {
                break;
            }
            for row in &key_rows {
                let [SqlValue::Blob(key)] = row.as_slice() else {
                    return Err(Error::Command("invalid scan key row"));
                };
                let Some(item) = crate::item_storage::StoredItem::Account {
                    table_id: &table.id,
                    key,
                }
                .read(|batch| context.sql(batch))?
                else {
                    return Err(Error::Command("scan key has no item"));
                };
                let encoded_bytes = serde_json::to_vec(&item)?.len();
                let next_bytes = bytes
                    .saturating_add(encoded_bytes.max(item_size_bytes(&item)))
                    .saturating_add(key.len() + 64);
                // Reserve space for JSON framing while staying below DynamoDB's 1 MiB page.
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
            let remaining = crate::secondary_index::scan_keys(
                context,
                &table.id,
                input.index_name.as_deref(),
                &cursor,
                false,
                1,
            )?;
            if remaining.is_empty() {
                None
            } else {
                items.last().map(|item| extract_key(item, &key_schema))
            }
        } else {
            None
        };
        Ok(Json(ScanItemsOutcome::Page {
            items,
            last_evaluated_key,
        }))
    }
}
