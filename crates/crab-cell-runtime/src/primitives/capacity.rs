//! Durable SQLite page reservations shared by application commands and runtime writes.

use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::{Error, Result};

/// Schema installed by Cells that reserve database capacity for deferred work.
pub const SCHEMA: &str = include_str!("../migrations/capacity.sql");

pub(crate) fn reserve(transaction: &Transaction<'_>, key: &[u8], bytes: u64) -> Result<()> {
    if key.is_empty() || key.len() > 128 || bytes == 0 {
        return Err(Error::Command("invalid database capacity reservation"));
    }
    let page_size: u64 = transaction.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let pages = i64::try_from(bytes.div_ceil(page_size))
        .map_err(|_| Error::Capacity("database reservation is too large"))?;
    let maximum: i64 = transaction.query_row("PRAGMA max_page_count", [], |row| row.get(0))?;
    if pages > maximum {
        return Err(Error::Capacity(
            "database reservation exceeds the Cell limit",
        ));
    }
    let existing: Option<i64> = transaction
        .query_row(
            "SELECT pages FROM capacity_reservations WHERE reservation_key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        return if existing == pages {
            Ok(())
        } else {
            Err(Error::Command("database reservation identity changed"))
        };
    }
    transaction.execute(
        "INSERT INTO capacity_reservations(reservation_key, pages) VALUES (?1, ?2)",
        (key, pages),
    )?;
    if transaction.execute(
        "UPDATE capacity_total SET pages = pages + ?1 WHERE singleton = 1",
        [pages],
    )? != 1
    {
        return Err(Error::Command("database reservation total is missing"));
    }
    validate(transaction)
}

pub(crate) fn release(transaction: &Transaction<'_>, key: &[u8]) -> Result<bool> {
    if key.is_empty() || key.len() > 128 {
        return Err(Error::Command("invalid database capacity reservation key"));
    }
    let pages: Option<i64> = transaction
        .query_row(
            "DELETE FROM capacity_reservations WHERE reservation_key = ?1 RETURNING pages",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    let Some(pages) = pages else { return Ok(false) };
    if transaction.execute(
        "UPDATE capacity_total SET pages = pages - ?1 WHERE singleton = 1 AND pages >= ?1",
        [pages],
    )? != 1
    {
        return Err(Error::Command("database reservation total is invalid"));
    }
    Ok(true)
}

// Run after the runtime receipt and metadata writes, not just after the handler.
// Otherwise those writes, effect delivery, or migration could spend capacity
// already promised to another command. Refusal rolls back the whole transaction.
pub(crate) fn validate(connection: &Connection) -> Result<()> {
    let installed: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name IN ('capacity_reservations', 'capacity_total')",
        [], |row| row.get(0),
    )?;
    if installed == 0 {
        return Ok(());
    }
    if installed != 2 {
        return Err(Error::Command("database reservation schema is incomplete"));
    }
    let reserved: i64 = connection.query_row(
        "SELECT pages FROM capacity_total WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if reserved == 0 {
        return Ok(());
    }
    let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let free: i64 = connection.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    let maximum: i64 = connection.query_row("PRAGMA max_page_count", [], |row| row.get(0))?;
    let available = maximum
        .checked_sub(pages)
        .and_then(|value| value.checked_add(free))
        .ok_or(Error::Command("invalid database page accounting"))?;
    if reserved > available {
        return Err(Error::Capacity(
            "database pages are reserved for deferred work",
        ));
    }
    Ok(())
}
