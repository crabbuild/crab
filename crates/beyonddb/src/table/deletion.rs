//! Generation-fenced deletion and bounded metadata cleanup.

use super::*;

/// A table name paired with the immutable generation selected by its caller.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableGeneration {
    pub table_name: String,
    pub table_id: String,
}

/// Catalog visibility while a generation is live or being deleted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TableLifecycle {
    Live(TableRecord),
    Deleting(TableRecord),
    Missing,
}

fn lifecycle(
    sql: impl FnOnce(&SqlBatch) -> Result<Vec<SqlResultSet>>,
    name: &str,
) -> Result<TableLifecycle> {
    let rows = sql(&statement(
        "SELECT t.record, d.table_id FROM ddb_tables t LEFT JOIN ddb_table_deletions d \
         ON d.table_id = t.table_id WHERE t.table_name = ?1",
        vec![SqlValue::Text(name.into())],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(TableLifecycle::Missing),
        Some([SqlValue::Blob(record), SqlValue::Null]) => {
            Ok(TableLifecycle::Live(serde_json::from_slice(record)?))
        }
        Some([SqlValue::Blob(record), SqlValue::Text(_)]) => {
            Ok(TableLifecycle::Deleting(serde_json::from_slice(record)?))
        }
        _ => Err(Error::Command("invalid table lifecycle row")),
    }
}

pub(super) fn command_lifecycle(
    context: &CommandContext<'_, '_>,
    name: &str,
) -> Result<TableLifecycle> {
    lifecycle(|batch| context.sql(batch), name)
}

/// Read a table generation and its deletion fence in one snapshot.
pub struct ReadTableLifecycle;
impl Query for ReadTableLifecycle {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 38;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<TableLifecycle>;
    fn execute(context: &mut QueryContext<'_>, Json(name): Self::Input) -> Result<Self::Output> {
        Ok(Json(lifecycle(|batch| context.sql(batch), &name)?))
    }
}

/// Result of fencing a table generation for deletion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DeleteTableOutcome {
    TransactionConflict,
    Deleted(TableRecord),
    TableNotFound,
    DeletionProtected,
}

/// Fence a generation and remove one bounded batch of its metadata.
pub struct DeleteTable;
impl Command for DeleteTable {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<TableGeneration>;
    type Output = Json<DeleteTableOutcome>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let (table, deleting) = match command_lifecycle(context, &input.table_name)? {
            TableLifecycle::Live(table) => (table, false),
            TableLifecycle::Deleting(table) => (table, true),
            TableLifecycle::Missing => {
                return Ok(CommandResult::Rejected(Json(
                    DeleteTableOutcome::TableNotFound,
                )));
            }
        };
        if table.id != input.table_id {
            return Ok(CommandResult::Rejected(Json(
                DeleteTableOutcome::TableNotFound,
            )));
        }
        if !deleting {
            if table.deletion_protection_enabled {
                return Ok(CommandResult::Rejected(Json(
                    DeleteTableOutcome::DeletionProtected,
                )));
            }
            // Existing account prepares need their table to apply COMMIT. The
            // live-table view fences every later prepare before cleanup starts.
            if crate::items::transaction::table_locked(context, &table.id)? {
                return Ok(CommandResult::Rejected(Json(
                    DeleteTableOutcome::TransactionConflict,
                )));
            }
            context.sql(&statement(
                "INSERT INTO ddb_table_deletions (table_id) VALUES (?1)",
                vec![SqlValue::Text(table.id.clone())],
            ))?;
        }
        cleanup(context, &table.id)?;
        Ok(CommandResult::Success(Json(DeleteTableOutcome::Deleted(
            table,
        ))))
    }
}

/// Resume bounded deletion of exactly one previously fenced generation.
pub struct ContinueTableDeletion;
impl Command for ContinueTableDeletion {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 32;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(id): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let deleting = context.sql(&statement(
            "SELECT 1 FROM ddb_table_deletions WHERE table_id = ?1",
            vec![SqlValue::Text(id.clone())],
        ))?;
        if deleting[0].rows.is_empty() {
            // A delayed cleanup cannot delete a live or replacement generation.
            return Ok(CommandResult::Success(Json(true)));
        }
        Ok(CommandResult::Success(Json(cleanup(context, &id)?)))
    }
}

fn cleanup(context: &CommandContext<'_, '_>, id: &str) -> Result<bool> {
    let mut remaining = 64_u64;
    // Remove children before parents, so FK cascades cannot hide unbounded work.
    // Statistics follow item deletion because its triggers decrement the totals.
    for (table, key, predicate) in [
        ("ddb_route_partitions", "rowid", "table_id = ?1"),
        ("ddb_split_members", "rowid", "table_id = ?1"),
        ("ddb_split_plans", "rowid", "table_id = ?1"),
        ("ddb_routes", "rowid", "table_id = ?1"),
        (
            "ddb_local_index_items",
            "(table_id, index_name, item_key)",
            "table_id = ?1",
        ),
        ("ddb_items", "rowid", "table_id = ?1"),
        ("ddb_index_changes", "rowid", "table_id = ?1"),
        (
            "ddb_local_index_statistics",
            "(table_id, index_name)",
            "table_id = ?1",
        ),
        ("ddb_table_tags", "rowid", "table_id = ?1"),
        ("ddb_table_ttl", "rowid", "table_id = ?1"),
        ("ddb_table_statistics", "rowid", "table_id = ?1"),
        (
            "ddb_global_index_partitions",
            "rowid",
            "table_id IN (SELECT table_id FROM ddb_global_index_routes WHERE base_table_id = ?1)",
        ),
        (
            "ddb_global_index_split_members",
            "rowid",
            "table_id IN (SELECT table_id FROM ddb_global_index_routes WHERE base_table_id = ?1)",
        ),
        (
            "ddb_global_index_split_plans",
            "rowid",
            "table_id IN (SELECT table_id FROM ddb_global_index_routes WHERE base_table_id = ?1)",
        ),
        ("ddb_global_index_routes", "rowid", "base_table_id = ?1"),
    ] {
        if remaining == 0 {
            return Ok(false);
        }
        let columns = key.trim_matches(['(', ')']);
        let deleted = context.sql(&statement(
            &format!("DELETE FROM {table} WHERE {key} IN (SELECT {columns} FROM {table} WHERE {predicate} LIMIT ?2)"),
            vec![SqlValue::Text(id.into()), SqlValue::Integer(i64::try_from(remaining).map_err(|_| Error::Command("invalid deletion budget"))?)],
        ))?[0].rows_affected;
        remaining = remaining
            .checked_sub(deleted)
            .ok_or(Error::Command("deletion exceeded its row budget"))?;
    }
    if remaining == 0 {
        return Ok(false);
    }
    context.sql(&statement(
        "DELETE FROM ddb_tables WHERE table_id = ?1",
        vec![SqlValue::Text(id.into())],
    ))?;
    Ok(true)
}
