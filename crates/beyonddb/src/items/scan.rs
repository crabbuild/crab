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
    /// Maximum items evaluated before returning a continuation.
    pub limit: Option<u32>,
    /// Complete key after which to resume.
    pub exclusive_start_key: Option<Item>,
}

/// Result of a bounded base-table scan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScanItemsOutcome {
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
        if input.limit == Some(0) {
            return Ok(Json(ScanItemsOutcome::InvalidLimit));
        }
        let mut cursor = match input.exclusive_start_key {
            Some(key) if valid_key(&key, &table) => item_key(&key, &table.key_schema)?,
            Some(_) => return Ok(Json(ScanItemsOutcome::InvalidKey)),
            None => Vec::new(),
        };
        let max_items = input.limit.unwrap_or(u32::MAX).min(10_000) as usize;
        let mut items = Vec::new();
        let mut bytes = 0_usize;
        let mut stopped = false;
        loop {
            let rows = context.sql(&statement(
                "SELECT item_key FROM ddb_items WHERE table_id = ?1 AND item_key > ?2 \
                 ORDER BY item_key LIMIT 64",
                vec![
                    SqlValue::Text(table.id.clone()),
                    SqlValue::Blob(cursor.clone()),
                ],
            ))?;
            let key_rows = &rows[0].rows;
            if key_rows.is_empty() {
                break;
            }
            for row in key_rows {
                let [SqlValue::Blob(key)] = row.as_slice() else {
                    return Err(Error::Command("invalid scan key row"));
                };
                let item_rows = context.sql(&statement(
                    "SELECT item FROM ddb_items WHERE table_id = ?1 AND item_key = ?2",
                    vec![
                        SqlValue::Text(table.id.clone()),
                        SqlValue::Blob(key.clone()),
                    ],
                ))?;
                let Some(item) = decode_item(&item_rows[0])? else {
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
            let remaining = context.sql(&statement(
                "SELECT 1 FROM ddb_items WHERE table_id = ?1 AND item_key > ?2 LIMIT 1",
                vec![SqlValue::Text(table.id), SqlValue::Blob(cursor)],
            ))?;
            if remaining[0].rows.is_empty() {
                None
            } else {
                items
                    .last()
                    .map(|item| extract_key(item, &table.key_schema))
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
