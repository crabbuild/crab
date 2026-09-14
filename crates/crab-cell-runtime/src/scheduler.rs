use std::collections::HashSet;

use crab_ltx::rusqlite::Transaction;

use crate::{Error, Result};

const WORKFLOW_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Computes the earliest durable work or retention deadline after a command.
///
/// Optional primitive schemas may be absent. Every returned deadline is clamped
/// to the command's logical time so stale work remains immediately discoverable.
pub fn scheduler_next_due_ms(
    transaction: &Transaction<'_>,
    logical_time_ms: i64,
) -> Result<Option<i64>> {
    if logical_time_ms < 0 {
        return Err(Error::Command("negative scheduler logical time"));
    }
    let tables = installed_tables(transaction)?;
    let mut next = None;

    include_minimum(
        transaction,
        "SELECT min(retain_until_ms) FROM sys_requests INDEXED BY sys_requests_expiry",
        logical_time_ms,
        &mut next,
    )?;
    include_minimum(
        transaction,
        "SELECT min(retain_until_ms) FROM sys_inbox INDEXED BY sys_inbox_expiry",
        logical_time_ms,
        &mut next,
    )?;
    include_minimum(
        transaction,
        "SELECT min(due_at_ms) FROM sys_effects INDEXED BY sys_effects_due WHERE state = 0",
        logical_time_ms,
        &mut next,
    )?;
    include_minimum(
        transaction,
        "SELECT min(lease_until_ms) FROM sys_effects INDEXED BY sys_effects_leases WHERE state = 1",
        logical_time_ms,
        &mut next,
    )?;
    include_minimum(
        transaction,
        "SELECT min(expires_at_ms) FROM sys_effects INDEXED BY sys_effects_expiry",
        logical_time_ms,
        &mut next,
    )?;

    if tables.contains("kv_entries") {
        include_minimum(
            transaction,
            "SELECT min(expires_at_ms) FROM kv_entries INDEXED BY kv_expiry WHERE expires_at_ms IS NOT NULL",
            logical_time_ms,
            &mut next,
        )?;
    }
    if tables.contains("queue_messages") {
        include_minimum(
            transaction,
            "SELECT min(due_at_ms) FROM queue_messages INDEXED BY queue_ready WHERE state = 0",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(lease_until_ms) FROM queue_messages INDEXED BY queue_leases WHERE state = 1",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(expires_at_ms) FROM queue_messages INDEXED BY queue_retention",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(retain_until_ms) FROM queue_dedup INDEXED BY queue_dedup_expiry",
            logical_time_ms,
            &mut next,
        )?;
    }
    if tables.contains("workflow_activities") {
        include_minimum(
            transaction,
            "SELECT min(due_at_ms) FROM workflow_activities INDEXED BY activities_due WHERE state = 0",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(lease_until_ms) FROM workflow_activities INDEXED BY activities_leases WHERE state = 1",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(expires_at_ms) FROM workflow_activities INDEXED BY activities_expiry WHERE state IN (0, 1)",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(due_at_ms) FROM workflow_timers INDEXED BY timers_due WHERE state = 0",
            logical_time_ms,
            &mut next,
        )?;
        let completed = minimum(
            transaction,
            "SELECT min(completed_at_ms) FROM workflow_runs INDEXED BY workflow_retention WHERE status != 0 AND completed_at_ms IS NOT NULL",
        )?;
        if let Some(completed_at_ms) = completed {
            let retention = completed_at_ms
                .checked_add(WORKFLOW_RETENTION_MS)
                .ok_or(Error::Command("workflow retention deadline overflow"))?;
            merge_due(retention, logical_time_ms, &mut next)?;
        }
    }
    Ok(next)
}

fn installed_tables(transaction: &Transaction<'_>) -> Result<HashSet<String>> {
    let mut statement = transaction.prepare(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name IN ('kv_entries', 'queue_messages', 'workflow_activities')",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<HashSet<_>, _>>()?;
    Ok(tables)
}

fn include_minimum(
    transaction: &Transaction<'_>,
    query: &'static str,
    logical_time_ms: i64,
    next: &mut Option<i64>,
) -> Result<()> {
    if let Some(value) = minimum(transaction, query)? {
        merge_due(value, logical_time_ms, next)?;
    }
    Ok(())
}

fn minimum(transaction: &Transaction<'_>, query: &'static str) -> Result<Option<i64>> {
    Ok(transaction.query_row(query, [], |row| row.get::<_, Option<i64>>(0))?)
}

fn merge_due(value: i64, logical_time_ms: i64, next: &mut Option<i64>) -> Result<()> {
    if value < 0 {
        return Err(Error::Command("negative stored scheduler deadline"));
    }
    let value = value.max(logical_time_ms);
    *next = Some(next.map_or(value, |current| current.min(value)));
    Ok(())
}
