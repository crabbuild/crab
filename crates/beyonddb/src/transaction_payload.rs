//! Transaction payloads stored within the runtime's bounded SQL interface.

use crab_cell_runtime::registry::CommandContext;

use crate::table::statement;
use crate::{Error, Result, SqlBatch, SqlResultSet, SqlValue};

const CHUNK_BYTES: usize = 256 * 1024;

pub(crate) fn write(
    context: &CommandContext<'_, '_>,
    transaction_id: [u8; 16],
    position: i64,
    bytes: &[u8],
) -> Result<i64> {
    if bytes.is_empty() {
        return Err(Error::Command("empty transaction payload"));
    }
    // Every SQL call remains below 1 MiB. All chunks and their owning phase
    // record still share one command savepoint and one publication boundary.
    let mut count = 0;
    for chunk in bytes.chunks(CHUNK_BYTES) {
        context.sql(&statement(
            "INSERT INTO ddb_transaction_payloads (transaction_id, position, chunk, payload) \
             VALUES (?1, ?2, ?3, ?4)",
            vec![
                SqlValue::Blob(transaction_id.to_vec()),
                SqlValue::Integer(position),
                SqlValue::Integer(count),
                SqlValue::Blob(chunk.to_vec()),
            ],
        ))?;
        count += 1;
    }
    Ok(count)
}

pub(crate) fn read(
    mut sql: impl FnMut(&SqlBatch) -> Result<Vec<SqlResultSet>>,
    transaction_id: [u8; 16],
    position: i64,
    chunks: i64,
) -> Result<Vec<u8>> {
    if chunks <= 0 {
        return Err(Error::Command("invalid transaction payload length"));
    }
    let mut bytes = Vec::new();
    for chunk in 0..chunks {
        let rows = sql(&statement(
            "SELECT payload FROM ddb_transaction_payloads \
             WHERE transaction_id = ?1 AND position = ?2 AND chunk = ?3",
            vec![
                SqlValue::Blob(transaction_id.to_vec()),
                SqlValue::Integer(position),
                SqlValue::Integer(chunk),
            ],
        ))?;
        let Some([SqlValue::Blob(payload)]) = rows[0].rows.first().map(Vec::as_slice) else {
            return Err(Error::Command("transaction payload chunk is missing"));
        };
        bytes.extend_from_slice(payload);
    }
    Ok(bytes)
}
