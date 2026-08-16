//! SQLite backing store for cache service metadata.
//!
//! The cache service stores object eviction metadata and the chunk->xorb
//! dedup index in one WAL-mode database. Separate `Connection`s let cache
//! reads, eviction, and dedup queries share the file without sharing one
//! process-wide lock.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, ErrorCode};
use tracing::warn;

use crate::error::{CacheServiceError, Result};

const SCHEMA_VERSION_KEY: &str = "schema_version";
const SCHEMA_VERSION: &str = "1";

/// Canonical SQLite file name under the cache root.
pub const CACHE_DB_FILE: &str = "cache.sqlite";

/// Cache-service SQLite database file.
pub struct CacheDb {
    path: PathBuf,
}

impl CacheDb {
    /// Open or create the cache-service SQLite database.
    ///
    /// Corrupt local cache databases are removed and recreated because cache
    /// metadata is an acceleration tier; cached objects and remote origin state
    /// remain authoritative.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        let path = normalize_db_path(path)?;
        match open_sqlite_cache(&path) {
            Ok(()) => Ok(Self { path }),
            Err(e) if should_recreate_cache(&e) && path.exists() => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "cache service database unreadable, recreating local cache metadata"
                );
                remove_sqlite_files(&path)?;
                open_sqlite_cache(&path)?;
                Ok(Self { path })
            }
            Err(e) => Err(e),
        }
    }

    /// Open a configured connection to the cache-service database.
    pub fn connect(&self) -> Result<Connection> {
        open_connection(&self.path)
    }
}

fn normalize_db_path(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CacheServiceError::InternalError(
                format!(
                    "failed to create cache database parent {}: {e}",
                    parent.display()
                )
                .into(),
            )
        })?;
    }
    Ok(path.to_path_buf())
}

fn open_sqlite_cache(path: &Path) -> Result<()> {
    let conn = open_connection(path)?;
    initialize_schema(&conn)
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).map_err(map_sqlite_err)?;
    configure_connection(&conn)?;
    Ok(conn)
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(map_sqlite_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(map_sqlite_err)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(map_sqlite_err)?;
    Ok(())
}

fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS object_meta (
            meta_key BLOB PRIMARY KEY NOT NULL CHECK(length(meta_key) = 33),
            meta_value BLOB NOT NULL CHECK(length(meta_value) = 32)
        );

        CREATE TABLE IF NOT EXISTS chunk_index (
            chunk_hash BLOB PRIMARY KEY NOT NULL CHECK(length(chunk_hash) = 32),
            xorb_ref BLOB NOT NULL CHECK(length(xorb_ref) = 40)
        );

        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );
        ",
    )
    .map_err(map_sqlite_err)?;

    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![SCHEMA_VERSION_KEY, SCHEMA_VERSION],
    )
    .map_err(map_sqlite_err)?;
    Ok(())
}

fn remove_sqlite_files(path: &Path) -> Result<()> {
    for candidate in [path.to_path_buf(), wal_path(path), shm_path(path)] {
        if let Err(e) = std::fs::remove_file(&candidate)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(CacheServiceError::InternalError(
                format!(
                    "failed to remove cache database file {}: {e}",
                    candidate.display()
                )
                .into(),
            ));
        }
    }
    Ok(())
}

fn wal_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}

fn shm_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-shm", path.display()))
}

fn should_recreate_cache(e: &CacheServiceError) -> bool {
    let CacheServiceError::InternalError(source) = e else {
        return false;
    };
    let Some(sqlite) = source.downcast_ref::<rusqlite::Error>() else {
        return false;
    };
    matches!(
        sqlite,
        rusqlite::Error::SqliteFailure(err, _)
            if matches!(
                err.code,
                ErrorCode::NotADatabase
                    | ErrorCode::DatabaseCorrupt
                    | ErrorCode::CannotOpen
                    | ErrorCode::SchemaChanged
            )
    )
}

pub(crate) fn map_sqlite_err(e: rusqlite::Error) -> CacheServiceError {
    CacheServiceError::InternalError(Box::new(e))
}
