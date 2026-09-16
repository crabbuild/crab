use rusqlite::{Connection, OptionalExtension};

use crate::{CellId, Error, IncarnationId, Result};

const RUNTIME_SCHEMA: &str = include_str!("migrations/runtime.sql");

/// Installs runtime schema version one in a new SQLite Cell.
///
/// The connection must not contain an existing runtime schema. Installation and
/// initial metadata insertion commit atomically; callers capture and publish the
/// resulting SQLite transaction through `crab-ltx` before exposing the Cell.
pub fn install_runtime_schema(
    connection: &mut Connection,
    cell: CellId,
    incarnation: IncarnationId,
    schema_version: u32,
) -> Result<()> {
    if schema_version == 0 {
        return Err(Error::Control("invalid initial schema version"));
    }
    connection.execute_batch("PRAGMA foreign_keys = ON")?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    install_runtime_schema_in(&transaction, cell, incarnation, schema_version)?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn install_runtime_schema_in(
    transaction: &rusqlite::Transaction<'_>,
    cell: CellId,
    incarnation: IncarnationId,
    schema_version: u32,
) -> Result<()> {
    if schema_version == 0 {
        return Err(Error::Control("invalid initial schema version"));
    }
    transaction.execute_batch(RUNTIME_SCHEMA)?;
    transaction.execute(
        "INSERT INTO sys_meta(singleton, cell_id, incarnation, commit_sequence, logical_time_ms, schema_version) VALUES (1, ?1, ?2, 0, 0, ?3)",
        (cell.as_bytes().as_slice(), incarnation.as_bytes().as_slice(), schema_version),
    )?;
    Ok(())
}

/// Verifies the persisted identity/schema before an existing Cell is admitted.
pub fn verify_runtime_schema(
    connection: &Connection,
    cell: CellId,
    incarnation: IncarnationId,
    schema_version: u32,
) -> Result<()> {
    let metadata = connection
        .query_row(
            "SELECT cell_id, incarnation, schema_version FROM sys_meta WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, u32>(2)?,
                ))
            },
        )
        .optional()?;
    match metadata {
        Some((stored_cell, stored_incarnation, stored_schema))
            if stored_cell == cell.as_bytes()
                && stored_incarnation == incarnation.as_bytes()
                && stored_schema == schema_version =>
        {
            Ok(())
        }
        Some(_) => Err(Error::Control("SQLite metadata does not match control")),
        None => Err(Error::Control("SQLite runtime metadata is missing")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_installation_is_atomic_and_binds_cell_identity() {
        let mut connection = Connection::open_in_memory().unwrap();
        let cell = CellId::from_bytes([1; 32]);
        let incarnation = IncarnationId::from_bytes([2; 16]);
        install_runtime_schema(&mut connection, cell, incarnation, 3).unwrap();
        verify_runtime_schema(&connection, cell, incarnation, 3).unwrap();
        assert!(
            verify_runtime_schema(&connection, CellId::from_bytes([9; 32]), incarnation, 3)
                .is_err()
        );
        assert!(install_runtime_schema(&mut connection, cell, incarnation, 3).is_err());
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }

    #[test]
    fn embedded_runtime_migration_matches_normative_contract() {
        assert_eq!(
            RUNTIME_SCHEMA,
            include_str!("../../../crab/docs/architecture/platform/contracts/runtime.sql")
        );
    }
}
