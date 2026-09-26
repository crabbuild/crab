//! Item images transferred through bounded SQL parameters and results.

use crab_cell_runtime::registry::CommandContext;
use extenddb_core::types::Item;

use crate::{Error, Result, SqlBatch, SqlResultSet, SqlValue, table::statement};

const CHUNK_BYTES: usize = 256 * 1024;

pub(crate) enum StoredItem<'a> {
    Account {
        table_id: &'a str,
        key: &'a [u8],
    },
    Partition(&'a [u8]),
    TransactionRead {
        transaction_id: &'a [u8; 16],
        position: i64,
    },
}

impl StoredItem<'_> {
    fn address(&self) -> (&'static str, &'static str, Vec<SqlValue>) {
        match self {
            Self::Account { table_id, key } => (
                "ddb_items",
                "table_id = ?2 AND item_key = ?3",
                vec![
                    SqlValue::Null,
                    SqlValue::Text((*table_id).into()),
                    SqlValue::Blob(key.to_vec()),
                ],
            ),
            Self::Partition(key) => (
                "ddb_partition_items",
                "item_key = ?2",
                vec![SqlValue::Null, SqlValue::Blob(key.to_vec())],
            ),
            Self::TransactionRead {
                transaction_id,
                position,
            } => (
                "ddb_transaction_reads",
                "transaction_id = ?2 AND position = ?3",
                vec![
                    SqlValue::Null,
                    SqlValue::Blob(transaction_id.to_vec()),
                    SqlValue::Integer(*position),
                ],
            ),
        }
    }

    pub(crate) fn read(
        &self,
        mut sql: impl FnMut(&SqlBatch) -> Result<Vec<SqlResultSet>>,
    ) -> Result<Option<Item>> {
        let (table, predicate, mut parameters) = self.address();
        let query =
            format!("SELECT substr(item, ?1, {CHUNK_BYTES}) FROM {table} WHERE {predicate}");
        let mut bytes = Vec::new();
        loop {
            parameters[0] = SqlValue::Integer(
                i64::try_from(bytes.len()).map_err(|_| Error::Command("item offset overflow"))? + 1,
            );
            let rows = sql(&statement(&query, parameters.clone()))?;
            let part = match rows[0].rows.first().map(Vec::as_slice) {
                None | Some([SqlValue::Null]) if bytes.is_empty() => return Ok(None),
                Some([SqlValue::Blob(part)]) => part,
                _ => return Err(Error::Command("invalid stored item chunk")),
            };
            bytes.extend_from_slice(part);
            if part.len() < CHUNK_BYTES {
                return Ok(Some(serde_json::from_slice(&bytes)?));
            }
        }
    }

    // The caller first inserts/resets the item to X'' in this command. Every
    // append and its indexes share the command savepoint; partial JSON is never
    // published. BLOB casts keep substr byte-based across UTF-8 chunk boundaries.
    pub(crate) fn append(&self, context: &CommandContext<'_, '_>, item: &Item) -> Result<()> {
        let (table, predicate, mut parameters) = self.address();
        let query = format!("UPDATE {table} SET item = CAST(item || ?1 AS BLOB) WHERE {predicate}");
        let bytes = serde_json::to_vec(item)?;
        for chunk in bytes.chunks(CHUNK_BYTES) {
            parameters[0] = SqlValue::Blob(chunk.to_vec());
            context.sql(&statement(&query, parameters.clone()))?;
        }
        Ok(())
    }
}
