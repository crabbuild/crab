//! SQLite index layer for the segment-based staging area.
//!
//! Manages schema migrations, chunk locator lookups, segment registry,
//! and the pending-chunks flush protocol. The `Index` struct owns a
//! `rusqlite::Connection` in WAL mode with foreign key enforcement.

use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter, types::Value};
use std::collections::HashMap;
use std::path::Path;
use tracing::debug;

use crate::error::{Result, StagingError};

use super::segment::ChunkLocator;

/// Current on-disk layout version. Uses BLOB columns for hash storage
/// to avoid hex encoding overhead and reduce index size.
const LAYOUT_VERSION: &str = "2";
const LEGACY_LAYOUT_VERSION: &str = "1";

/// A row staged in `pending_chunks` before the segment is fsynced.
///
/// Hashes are stored as raw 32-byte arrays to avoid heap-allocated hex
/// strings on the hot path. SQLite stores them as BLOB(32).
#[derive(Debug, Clone)]
pub struct PendingRow {
    pub chunk_hash: [u8; 32],
    pub file_hash: [u8; 32],
    pub chunk_index: i64,
    pub size: i64,
    pub segment_id: u64,
    pub segment_offset: u64,
}

/// A file chunk row with the exact segment locator selected for that
/// `(file_hash, chunk_index)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileChunkLocator {
    pub chunk_hash: [u8; 32],
    pub size: u64,
    pub locator: ChunkLocator,
}

/// A chunk stored only inside a finalized prepared xorb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreparedChunkLocator {
    pub file_hash: [u8; 32],
    pub xorb_hash: [u8; 32],
    pub payload_hash: [u8; 32],
    pub xorb_bytes: u64,
    pub chunk_index: u32,
    pub size: u32,
}

/// Authoritative add-time push plan stored in the staging index.
#[derive(Debug, Clone)]
pub(crate) struct StoredFilePushPlan {
    pub version: u32,
    pub file_size: u64,
    pub chunk_count: u64,
    pub chunk_sequence_hash: [u8; 32],
    pub plan_json: Vec<u8>,
}

/// Prepared xorb candidate stored in the staging index.
#[derive(Debug, Clone)]
pub(crate) struct StoredPreparedXorb {
    pub file_hash: [u8; 32],
    pub xorb_hash: [u8; 32],
    pub payload_hash: [u8; 32],
    pub bytes: u64,
    pub planned_json: Vec<u8>,
}

/// Raw prepared xorb row used by diagnostics that must report corruption.
pub(crate) struct RawPreparedXorbRow {
    pub file_hash: Vec<u8>,
    pub xorb_hash: Vec<u8>,
    pub payload_hash: Vec<u8>,
    pub bytes: i64,
    pub planned_json: Vec<u8>,
}

pub(crate) struct PreparedXorbWrite {
    pub xorb_hash: [u8; 32],
    pub payload_hash: [u8; 32],
    pub bytes: u64,
    pub planned_json: Vec<u8>,
    pub placements: Vec<PreparedXorbPlacementWrite>,
}

pub(crate) struct PreparedXorbPlacementWrite {
    pub chunk_hash: [u8; 32],
    pub chunk_index: u32,
    pub uncompressed_size: u32,
}

type StoredFilePushPlanRow = (i64, i64, i64, Vec<u8>, Vec<u8>);
type StoredPreparedXorbRow = (Vec<u8>, Vec<u8>, Vec<u8>, i64, Vec<u8>);
pub(crate) type BatchDedupExisting = (usize, [u8; 32], ChunkLocator, bool);
pub(crate) type BatchDedupResult = (Vec<BatchDedupExisting>, Vec<usize>);

const PREPARED_XORB_QUERY_CHUNK_BATCH: usize = 500;
const RECIPE_OCCURRENCE_INSERT_BATCH: usize = 4096;

/// Per-file staging information returned by [`Index::list_files_with_chunks`].
#[derive(Debug, Clone)]
pub struct StagedFileInfo {
    /// Blake3 content hash of the file.
    pub file_hash: [u8; 32],
    /// Original file size in bytes.
    pub total_bytes: u64,
    /// Number of chunks in the committed `chunks` table.
    pub committed_chunks: u64,
    /// Number of chunk rows currently stored in `pending_chunks`.
    pub pending_chunks: u64,
    /// Number of distinct segments holding this file's chunks.
    pub segments: u64,
    /// Original file path (relative to repo root), if recorded during add.
    pub file_path: Option<String>,
}

/// SQLite wrapper for the staging index (`index.db`).
///
/// Owns the `Connection` and exposes transactional helpers for the
/// segment lifecycle (allocate, register, seal, sweep).
pub struct Index {
    conn: Connection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecipeVerification {
    Pending,
    CallerVerified,
}

fn decode_hash_blob(kind: &str, blob: Vec<u8>) -> Result<[u8; 32]> {
    blob.try_into().map_err(|blob: Vec<u8>| {
        StagingError::StagingCorrupt(format!("{kind} has {} bytes, expected 32", blob.len()))
    })
}

fn decode_chunk_hash_blob(blob: Vec<u8>) -> Result<[u8; 32]> {
    decode_hash_blob("chunk hash", blob)
}

fn nonnegative_count(field: &str, value: i64) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| StagingError::StagingCorrupt(format!("{field} is negative: {value}")))
}

fn sqlite_i64(field: &str, value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StagingError::StagingCorrupt(format!("{field} is too large: {value}")))
}

fn validate_chunk_index(expected: usize, actual: i64) -> Result<()> {
    let expected = i64::try_from(expected).map_err(|_| {
        StagingError::StagingCorrupt("file has too many staged chunks to index".to_owned())
    })?;
    if actual != expected {
        return Err(StagingError::StagingCorrupt(format!(
            "chunk indexes are not contiguous: expected {expected}, found {actual}"
        )));
    }
    Ok(())
}

fn has_unique_index(conn: &Connection, table: &str, columns: &[&str]) -> Result<bool> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA index_list({})", quote_identifier(table)))
        .map_err(|e| StagingError::Internal(format!("prepare index_list for {table}: {e}")))?;
    let indexes = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2).map(|unique| unique != 0)?,
            ))
        })
        .map_err(|e| StagingError::Internal(format!("query index_list for {table}: {e}")))?;

    for index in indexes {
        let (name, unique) = index
            .map_err(|e| StagingError::Internal(format!("collect index_list for {table}: {e}")))?;
        if !unique {
            continue;
        }

        let mut info_stmt = conn
            .prepare(&format!("PRAGMA index_info({})", quote_identifier(&name)))
            .map_err(|e| StagingError::Internal(format!("prepare index_info for {name}: {e}")))?;
        let info_rows = info_stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(2)?))
            })
            .map_err(|e| StagingError::Internal(format!("query index_info for {name}: {e}")))?;

        let mut indexed_columns = Vec::new();
        for info in info_rows {
            let (seqno, Some(column)) = info.map_err(|e| {
                StagingError::Internal(format!("collect index_info for {name}: {e}"))
            })?
            else {
                indexed_columns.clear();
                break;
            };
            indexed_columns.push((seqno, column));
        }
        indexed_columns.sort_by_key(|(seqno, _)| *seqno);
        let indexed_columns: Vec<&str> = indexed_columns
            .iter()
            .map(|(_, column)| column.as_str())
            .collect();
        if indexed_columns == columns {
            return Ok(true);
        }
    }

    Ok(false)
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM sqlite_master
            WHERE type = 'table' AND name = ?1
        )",
        params![table],
        |row| row.get(0),
    )
    .map_err(|e| StagingError::Internal(format!("failed to inspect table {table}: {e}")))
}

fn ensure_pending_collision_is_idempotent(
    tx: &rusqlite::Transaction<'_>,
    row: &PendingRow,
) -> Result<()> {
    let fh: &[u8] = &row.file_hash;
    let existing: Option<(Vec<u8>, i64, u64, u64)> = tx
        .query_row(
            "SELECT chunk_hash, size, segment_id, segment_offset
             FROM pending_chunks
             WHERE file_hash = ?1 AND chunk_index = ?2",
            params![fh, row.chunk_index],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(|e| StagingError::Internal(format!("failed to inspect pending collision: {e}")))?;

    let Some((existing_hash, existing_size, existing_segment_id, existing_offset)) = existing
    else {
        return Err(StagingError::Internal(
            "pending insert reported a conflict but the existing row was not found".to_owned(),
        ));
    };

    let existing_hash = decode_hash_blob("pending chunk hash", existing_hash)?;
    if existing_hash == row.chunk_hash
        && existing_size == row.size
        && existing_segment_id == row.segment_id
        && existing_offset == row.segment_offset
    {
        return Ok(());
    }

    Err(StagingError::StagingCorrupt(format!(
        "pending chunk collision at chunk_index {}: existing row differs from new staging row",
        row.chunk_index
    )))
}

impl Index {
    /// Open (or create) the staging index at `path`.
    ///
    /// Enables WAL mode, `PRAGMA synchronous = NORMAL`, and foreign key
    /// enforcement. Runs schema migrations and seeds `layout_version = 2`
    /// on first open.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failures.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| {
            StagingError::Internal(format!(
                "failed to open index db at {}: {e}",
                path.display()
            ))
        })?;

        // WAL mode for concurrent readers + single writer.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| StagingError::Internal(format!("failed to set WAL mode: {e}")))?;

        // NORMAL synchronous is safe with WAL — commits are durable after
        // the WAL write; only a full OS crash can lose the last transaction.
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| {
                StagingError::Internal(format!("failed to set synchronous = NORMAL: {e}"))
            })?;

        // Busy timeout: when another connection holds a write lock (e.g.
        // during WAL checkpoint), retry for up to 5 seconds instead of
        // failing immediately with SQLITE_BUSY. Critical for the
        // shared-lock model where readers and writers coexist.
        conn.pragma_update(None, "busy_timeout", "5000")
            .map_err(|e| StagingError::Internal(format!("failed to set busy_timeout: {e}")))?;

        // Enforce foreign key constraints.
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| StagingError::Internal(format!("failed to enable foreign keys: {e}")))?;

        let mut idx = Self { conn };
        idx.run_migrations()?;
        Ok(idx)
    }

    /// Open the staging index for a shared push handle.
    ///
    /// Runs idempotent migrations before admitting the push, then enables WAL,
    /// foreign keys, and a busy timeout. The SQLite connection is read-write
    /// because callers record push snapshot and retirement lifecycle rows;
    /// they never append or relocate segment payloads through this handle.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failures.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| {
            StagingError::Internal(format!(
                "failed to open index db for shared push at {}: {e}",
                path.display()
            ))
        })?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| StagingError::Internal(format!("failed to set WAL mode: {e}")))?;

        conn.pragma_update(None, "busy_timeout", "5000")
            .map_err(|e| StagingError::Internal(format!("failed to set busy_timeout: {e}")))?;

        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| StagingError::Internal(format!("failed to enable foreign keys: {e}")))?;
        let mut index = Self { conn };
        index.run_migrations()?;
        Ok(index)
    }

    /// Run schema migrations (idempotent — uses `IF NOT EXISTS`).
    fn run_migrations(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS files (
                    file_hash    BLOB PRIMARY KEY,
                    shard_hash   BLOB,
                    total_bytes  INTEGER NOT NULL,
                    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS segments (
                    segment_id       INTEGER PRIMARY KEY,
                    sealed_at        TEXT,
                    size_bytes       INTEGER NOT NULL,
                    chunk_count      INTEGER NOT NULL DEFAULT 0,
                    live_chunk_count INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS chunks (
                    chunk_hash       BLOB NOT NULL,
                    file_hash        BLOB NOT NULL,
                    chunk_index      INTEGER NOT NULL,
                    size             INTEGER NOT NULL,
                    segment_id       INTEGER NOT NULL,
                    segment_offset   INTEGER NOT NULL,
                    PRIMARY KEY (file_hash, chunk_index),
                    FOREIGN KEY (file_hash)  REFERENCES files(file_hash),
                    FOREIGN KEY (segment_id) REFERENCES segments(segment_id)
                );

                CREATE INDEX IF NOT EXISTS chunks_by_hash
                    ON chunks(chunk_hash);

                CREATE INDEX IF NOT EXISTS chunks_by_segment
                    ON chunks(segment_id);

                CREATE TABLE IF NOT EXISTS pending_chunks (
                    chunk_hash       BLOB NOT NULL,
                    file_hash        BLOB NOT NULL,
                    chunk_index      INTEGER NOT NULL,
                    size             INTEGER NOT NULL,
                    segment_id       INTEGER NOT NULL,
                    segment_offset   INTEGER NOT NULL,
                    UNIQUE (file_hash, chunk_index)
                );

                CREATE INDEX IF NOT EXISTS pending_by_hash
                    ON pending_chunks(chunk_hash);

                CREATE INDEX IF NOT EXISTS pending_by_file
                    ON pending_chunks(file_hash);

                CREATE TABLE IF NOT EXISTS staging_meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS file_push_plans (
                    file_hash            BLOB PRIMARY KEY,
                    version              INTEGER NOT NULL,
                    file_size            INTEGER NOT NULL,
                    chunk_count          INTEGER NOT NULL,
                    chunk_sequence_hash  BLOB NOT NULL,
                    plan_json            BLOB NOT NULL,
                    updated_at           TEXT NOT NULL DEFAULT (datetime('now')),
                    FOREIGN KEY (file_hash) REFERENCES files(file_hash)
                );

                CREATE TABLE IF NOT EXISTS prepared_xorbs (
                    file_hash     BLOB NOT NULL,
                    xorb_hash     BLOB NOT NULL,
                    payload_hash  BLOB NOT NULL,
                    bytes         INTEGER NOT NULL,
                    planned_json  BLOB NOT NULL,
                    updated_at    TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (file_hash, xorb_hash),
                    FOREIGN KEY (file_hash) REFERENCES files(file_hash)
                );

                CREATE TABLE IF NOT EXISTS prepared_xorb_chunks (
                    file_hash          BLOB NOT NULL,
                    xorb_hash          BLOB NOT NULL,
                    chunk_hash         BLOB NOT NULL,
                    chunk_index        INTEGER NOT NULL,
                    uncompressed_size  INTEGER NOT NULL,
                    PRIMARY KEY (file_hash, xorb_hash, chunk_index),
                    FOREIGN KEY (file_hash, xorb_hash)
                        REFERENCES prepared_xorbs(file_hash, xorb_hash)
                        ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS prepared_xorb_chunks_by_hash
                    ON prepared_xorb_chunks(chunk_hash);

                CREATE INDEX IF NOT EXISTS prepared_xorb_chunks_by_xorb
                    ON prepared_xorb_chunks(file_hash, xorb_hash);

                CREATE TABLE IF NOT EXISTS staging_batches (
                    batch_id    TEXT PRIMARY KEY,
                    state       TEXT NOT NULL CHECK(state IN ('open', 'published')),
                    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS file_recipes (
                    recipe_hash  BLOB PRIMARY KEY,
                    file_hash    BLOB NOT NULL,
                    file_size    INTEGER NOT NULL,
                    policy_id    TEXT NOT NULL,
                    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS recipe_occurrences (
                    recipe_hash   BLOB NOT NULL,
                    occurrence    INTEGER NOT NULL,
                    chunk_hash    BLOB NOT NULL,
                    chunk_offset  INTEGER NOT NULL,
                    chunk_size    INTEGER NOT NULL,
                    PRIMARY KEY (recipe_hash, occurrence),
                    FOREIGN KEY (recipe_hash) REFERENCES file_recipes(recipe_hash)
                        ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS recipes_by_file
                    ON file_recipes(file_hash);

                CREATE TABLE IF NOT EXISTS verified_recipes (
                    recipe_hash BLOB PRIMARY KEY,
                    verified_at TEXT NOT NULL DEFAULT (datetime('now')),
                    FOREIGN KEY (recipe_hash) REFERENCES file_recipes(recipe_hash)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS path_leases (
                    batch_id    TEXT NOT NULL,
                    path_bytes  BLOB NOT NULL,
                    file_hash   BLOB NOT NULL,
                    recipe_hash BLOB NOT NULL,
                    PRIMARY KEY (batch_id, path_bytes),
                    FOREIGN KEY (batch_id) REFERENCES staging_batches(batch_id)
                        ON DELETE CASCADE,
                    FOREIGN KEY (recipe_hash) REFERENCES file_recipes(recipe_hash)
                );

                CREATE INDEX IF NOT EXISTS leases_by_file
                    ON path_leases(file_hash);

                CREATE TABLE IF NOT EXISTS chunk_payloads (
                    chunk_hash     BLOB PRIMARY KEY,
                    size           INTEGER NOT NULL,
                    segment_id     INTEGER NOT NULL,
                    segment_offset INTEGER NOT NULL,
                    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS recipe_payload_leases (
                    recipe_hash BLOB NOT NULL,
                    chunk_hash  BLOB NOT NULL,
                    PRIMARY KEY (recipe_hash, chunk_hash),
                    FOREIGN KEY (recipe_hash) REFERENCES file_recipes(recipe_hash)
                        ON DELETE CASCADE,
                    FOREIGN KEY (chunk_hash) REFERENCES chunk_payloads(chunk_hash)
                );

                CREATE INDEX IF NOT EXISTS recipe_payload_leases_by_chunk
                    ON recipe_payload_leases(chunk_hash);

                CREATE TABLE IF NOT EXISTS prepared_payloads (
                    xorb_hash    BLOB PRIMARY KEY,
                    payload_hash BLOB NOT NULL,
                    bytes        INTEGER NOT NULL,
                    path_bytes   BLOB,
                    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS prepared_leases (
                    recipe_hash BLOB NOT NULL,
                    xorb_hash   BLOB NOT NULL,
                    PRIMARY KEY (recipe_hash, xorb_hash),
                    FOREIGN KEY (recipe_hash) REFERENCES file_recipes(recipe_hash)
                        ON DELETE CASCADE,
                    FOREIGN KEY (xorb_hash) REFERENCES prepared_payloads(xorb_hash)
                );

                CREATE TABLE IF NOT EXISTS push_snapshots (
                    snapshot_id TEXT PRIMARY KEY,
                    state       TEXT NOT NULL CHECK(state IN ('open', 'committed', 'retired')),
                    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS push_snapshot_recipes (
                    snapshot_id TEXT NOT NULL,
                    recipe_hash BLOB NOT NULL,
                    PRIMARY KEY (snapshot_id, recipe_hash),
                    FOREIGN KEY (snapshot_id) REFERENCES push_snapshots(snapshot_id)
                        ON DELETE CASCADE,
                    FOREIGN KEY (recipe_hash) REFERENCES file_recipes(recipe_hash)
                );

                CREATE TABLE IF NOT EXISTS push_snapshot_leases (
                    snapshot_id TEXT NOT NULL,
                    batch_id    TEXT NOT NULL,
                    path_bytes  BLOB NOT NULL,
                    file_hash   BLOB NOT NULL,
                    recipe_hash BLOB NOT NULL,
                    PRIMARY KEY (snapshot_id, batch_id, path_bytes),
                    FOREIGN KEY (snapshot_id) REFERENCES push_snapshots(snapshot_id)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS staging_quarantine (
                    kind        TEXT NOT NULL,
                    identity    BLOB NOT NULL,
                    reason      TEXT NOT NULL,
                    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (kind, identity)
                );

                CREATE TRIGGER IF NOT EXISTS chunks_register_payload
                AFTER INSERT ON chunks BEGIN
                    INSERT OR IGNORE INTO chunk_payloads
                        (chunk_hash, size, segment_id, segment_offset)
                    VALUES (
                        NEW.chunk_hash, NEW.size, NEW.segment_id, NEW.segment_offset
                    );
                END;

                CREATE TRIGGER IF NOT EXISTS pending_chunks_register_payload
                AFTER INSERT ON pending_chunks BEGIN
                    INSERT OR IGNORE INTO chunk_payloads
                        (chunk_hash, size, segment_id, segment_offset)
                    VALUES (
                        NEW.chunk_hash, NEW.size, NEW.segment_id, NEW.segment_offset
                    );
                END;",
            )
            .map_err(|e| StagingError::Internal(format!("schema migration failed: {e}")))?;

        // Capture every distinct legacy payload locator before normalizing
        // duplicate file positions. A conflicting duplicate is not safe to
        // publish as an ordered recipe, but its bytes may be the only local
        // recovery copy and must remain inventoried for quarantine/repair.
        self.conn
            .execute_batch(
                "INSERT OR IGNORE INTO chunk_payloads
                     (chunk_hash, size, segment_id, segment_offset)
                     SELECT chunk_hash, size, segment_id, segment_offset FROM chunks;
                 INSERT OR IGNORE INTO chunk_payloads
                     (chunk_hash, size, segment_id, segment_offset)
                     SELECT chunk_hash, size, segment_id, segment_offset FROM pending_chunks;",
            )
            .map_err(|e| {
                StagingError::Internal(format!("legacy payload inventory migration failed: {e}"))
            })?;

        // Legacy staging databases may have been created before the
        // per-file chunk-position constraints existed. Normalize those
        // duplicates before adding unique indexes; push treats this table as
        // the source of truth for file layout, so constraint failures must
        // stop startup instead of letting a malformed sequence reach shards.
        self.conn
            .execute_batch(
                "DELETE FROM chunks WHERE rowid NOT IN (
                     SELECT MIN(rowid) FROM chunks
                     GROUP BY file_hash, chunk_index
                 );
                 DELETE FROM pending_chunks WHERE rowid NOT IN (
                     SELECT MIN(rowid) FROM pending_chunks
                     GROUP BY file_hash, chunk_index
                 );",
            )
            .map_err(|e| {
                StagingError::Internal(format!("staging chunk uniqueness migration failed: {e}"))
            })?;
        if !has_unique_index(&self.conn, "chunks", &["file_hash", "chunk_index"])? {
            self.conn
                .execute(
                    "CREATE UNIQUE INDEX chunks_file_chunk_idx ON chunks(file_hash, chunk_index)",
                    [],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to create chunks uniqueness index: {e}"))
                })?;
        }
        if !has_unique_index(&self.conn, "pending_chunks", &["file_hash", "chunk_index"])? {
            self.conn
                .execute(
                    "CREATE UNIQUE INDEX pending_file_chunk_idx
                     ON pending_chunks(file_hash, chunk_index)",
                    [],
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to create pending_chunks uniqueness index: {e}"
                    ))
                })?;
        }

        // Additive migration: file_paths side table for UX (maps hash → path).
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS file_paths (
                    file_hash  BLOB PRIMARY KEY,
                    file_path  TEXT NOT NULL
                );",
            )
            .map_err(|e| {
                tracing::debug!(error = %e, "file_paths migration (non-fatal)");
            })
            .ok();

        self.conn
            .execute_batch(
                "INSERT OR IGNORE INTO chunk_payloads
                     (chunk_hash, size, segment_id, segment_offset)
                     SELECT chunk_hash, size, segment_id, segment_offset FROM chunks;
                 INSERT OR IGNORE INTO chunk_payloads
                     (chunk_hash, size, segment_id, segment_offset)
                     SELECT chunk_hash, size, segment_id, segment_offset FROM pending_chunks;",
            )
            .map_err(|e| {
                StagingError::Internal(format!("payload inventory migration failed: {e}"))
            })?;

        self.conn
            .execute(
                "INSERT INTO staging_meta (key, value)
                 SELECT 'migration_validation_pending', '1'
                 WHERE EXISTS (
                     SELECT 1 FROM file_recipes AS recipe
                     JOIN path_leases AS lease USING (recipe_hash)
                     LEFT JOIN verified_recipes AS verified USING (recipe_hash)
                     WHERE verified.recipe_hash IS NULL
                 )
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to schedule recipe validation: {e}"))
            })?;

        // Seed layout_version on first open; verify on subsequent opens.
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM staging_meta WHERE key = 'layout_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| StagingError::Internal(format!("failed to read layout_version: {e}")))?;

        match existing {
            None => {
                let files: u64 = self
                    .conn
                    .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
                    .map_err(|e| {
                        StagingError::Internal(format!("failed to count legacy files: {e}"))
                    })?;
                if files == 0 {
                    self.conn
                        .execute(
                            "INSERT INTO staging_meta (key, value) VALUES ('layout_version', ?1)",
                            params![LAYOUT_VERSION],
                        )
                        .map_err(|e| {
                            StagingError::Internal(format!("failed to seed layout_version: {e}"))
                        })?;
                    debug!(version = LAYOUT_VERSION, "seeded staging layout_version");
                } else {
                    self.migrate_legacy_layout()?;
                }
            }
            Some(ref v) if v == LAYOUT_VERSION => {}
            Some(ref v) if v == LEGACY_LAYOUT_VERSION => self.migrate_legacy_layout()?,
            Some(v) => {
                return Err(StagingError::StagingCorrupt(format!(
                    "unsupported layout_version: expected {LAYOUT_VERSION}, found {v}"
                )));
            }
        }

        Ok(())
    }

    fn migrate_legacy_layout(&mut self) -> Result<()> {
        struct LegacyRecipe {
            file_hash: [u8; 32],
            path_bytes: Vec<u8>,
            recipe: crate::recipe::FileRecipe,
        }

        let file_rows = {
            let mut statement = self
                .conn
                .prepare(
                    "SELECT files.file_hash, files.total_bytes, file_paths.file_path
                     FROM files LEFT JOIN file_paths USING(file_hash)",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to prepare legacy migration: {e}"))
                })?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })
                .map_err(|e| StagingError::Internal(format!("failed to query legacy files: {e}")))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| {
                    StagingError::Internal(format!("failed to collect legacy files: {e}"))
                })?
        };
        let mut valid = Vec::new();
        let mut quarantined = Vec::new();
        for (raw_hash, raw_size, path) in file_rows {
            let file_hash = match decode_hash_blob("legacy file hash", raw_hash.clone()) {
                Ok(hash) => hash,
                Err(error) => {
                    quarantined.push((raw_hash, error.to_string()));
                    continue;
                }
            };
            let file_size = match u64::try_from(raw_size) {
                Ok(size) => size,
                Err(_) => {
                    quarantined.push((file_hash.to_vec(), "negative legacy file size".to_owned()));
                    continue;
                }
            };
            let chunks = match self.chunks_for_file_with_sizes(&file_hash) {
                Ok(chunks) => chunks,
                Err(error) => {
                    quarantined.push((file_hash.to_vec(), error.to_string()));
                    continue;
                }
            };
            let chunks = chunks
                .into_iter()
                .map(|(hash, size)| (crab_xet::hash::MerkleHash::from(hash), size))
                .collect::<Vec<_>>();
            let recipe = match crate::recipe::FileRecipe::from_staged_chunks(
                crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
                crab_xet::hash::MerkleHash::from(file_hash),
                file_size,
                &chunks,
            ) {
                Ok(recipe) => recipe,
                Err(error) => {
                    quarantined.push((file_hash.to_vec(), error.to_string()));
                    continue;
                }
            };
            let path_bytes = path.map_or_else(
                || {
                    let mut diagnostic = b"legacy-file:".to_vec();
                    diagnostic.extend_from_slice(&file_hash);
                    diagnostic
                },
                String::into_bytes,
            );
            valid.push(LegacyRecipe {
                file_hash,
                path_bytes,
                recipe,
            });
        }

        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin layout-v2 migration: {e}"))
        })?;
        tx.execute("DELETE FROM path_leases WHERE batch_id = 'legacy-v1'", [])
            .map_err(|e| StagingError::Internal(format!("failed to remove legacy leases: {e}")))?;
        tx.execute(
            "DELETE FROM staging_batches WHERE batch_id = 'legacy-v1'",
            [],
        )
        .map_err(|e| StagingError::Internal(format!("failed to remove legacy batch: {e}")))?;
        tx.execute(
            "DELETE FROM file_recipes WHERE policy_id = 'legacy-unknown'",
            [],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to remove legacy recipe hints: {e}"))
        })?;
        tx.execute(
            "INSERT OR REPLACE INTO staging_batches (batch_id, state)
             VALUES ('migration-v2', 'open')",
            [],
        )
        .map_err(|e| StagingError::Internal(format!("failed to create migration batch: {e}")))?;
        for legacy in valid {
            let recipe_hash = legacy.recipe.hash();
            let file_hash: [u8; 32] = legacy.recipe.sequence().file_hash.into();
            tx.execute(
                "INSERT OR IGNORE INTO file_recipes
                 (recipe_hash, file_hash, file_size, policy_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    recipe_hash.as_slice(),
                    file_hash.as_slice(),
                    sqlite_i64("legacy recipe size", legacy.recipe.sequence().file_size)?,
                    legacy.recipe.policy().as_str(),
                ],
            )
            .map_err(|e| StagingError::Internal(format!("failed to migrate recipe: {e}")))?;
            for (occurrence, chunk) in legacy.recipe.sequence().spans.iter().enumerate() {
                let chunk_hash: [u8; 32] = chunk.chunk_hash.into();
                tx.execute(
                    "INSERT OR IGNORE INTO recipe_occurrences
                     (recipe_hash, occurrence, chunk_hash, chunk_offset, chunk_size)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        recipe_hash.as_slice(),
                        i64::try_from(occurrence).map_err(|_| StagingError::StagingCorrupt(
                            "too many legacy recipe occurrences".to_owned()
                        ))?,
                        chunk_hash.as_slice(),
                        sqlite_i64("legacy chunk offset", chunk.offset)?,
                        sqlite_i64("legacy chunk size", chunk.len)?,
                    ],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to migrate occurrence: {e}"))
                })?;
                tx.execute(
                    "INSERT OR IGNORE INTO recipe_payload_leases (recipe_hash, chunk_hash)
                     VALUES (?1, ?2)",
                    params![recipe_hash.as_slice(), chunk_hash.as_slice()],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to migrate payload lease: {e}"))
                })?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO path_leases
                 (batch_id, path_bytes, file_hash, recipe_hash)
                 VALUES ('migration-v2', ?1, ?2, ?3)",
                params![
                    legacy.path_bytes,
                    legacy.file_hash.as_slice(),
                    recipe_hash.as_slice(),
                ],
            )
            .map_err(|e| StagingError::Internal(format!("failed to migrate path lease: {e}")))?;
        }
        for (identity, reason) in quarantined {
            tx.execute(
                "INSERT OR REPLACE INTO staging_quarantine (kind, identity, reason)
                 VALUES ('legacy-file', ?1, ?2)",
                params![identity, reason],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to quarantine legacy file: {e}"))
            })?;
        }
        tx.execute(
            "UPDATE staging_batches SET state = 'published' WHERE batch_id = 'migration-v2'",
            [],
        )
        .map_err(|e| StagingError::Internal(format!("failed to publish migration batch: {e}")))?;
        tx.execute(
            "INSERT INTO staging_meta (key, value) VALUES ('layout_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![LAYOUT_VERSION],
        )
        .map_err(|e| StagingError::Internal(format!("failed to record layout migration: {e}")))?;
        tx.execute(
            "INSERT INTO staging_meta (key, value)
             VALUES ('migration_validation_pending', '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to schedule migration validation: {e}"))
        })?;
        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit layout-v2 migration: {e}"))
        })?;
        debug!(version = LAYOUT_VERSION, "migrated staging layout");
        Ok(())
    }

    pub fn migration_validation_pending(&self) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM staging_meta
                     WHERE key = 'migration_validation_pending' AND value = '1'
                 )",
                [],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to inspect migration validation: {e}"))
            })
    }

    pub fn unverified_recipe_hashes(&self) -> Result<Vec<[u8; 32]>> {
        let recipe_hashes = {
            let mut statement = self
                .conn
                .prepare_cached(
                    "SELECT DISTINCT recipe.recipe_hash
                     FROM file_recipes AS recipe
                     JOIN path_leases AS lease USING (recipe_hash)
                     LEFT JOIN verified_recipes AS verified USING (recipe_hash)
                     WHERE verified.recipe_hash IS NULL
                     ORDER BY recipe.recipe_hash",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to prepare unverified recipes: {e}"))
                })?;
            statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| {
                    StagingError::Internal(format!("failed to query unverified recipes: {e}"))
                })?
                .map(|row| {
                    row.map_err(|e| {
                        StagingError::Internal(format!("failed to collect unverified recipe: {e}"))
                    })
                    .and_then(|hash| decode_hash_blob("unverified recipe hash", hash))
                })
                .collect::<Result<Vec<_>>>()?
        };
        Ok(recipe_hashes)
    }

    pub fn unverified_recipe(&self, recipe_hash: &[u8; 32]) -> Result<crate::recipe::FileRecipe> {
        let (raw_file_hash, raw_file_size, policy_id): (Vec<u8>, i64, String) = self
            .conn
            .query_row(
                "SELECT file_hash, file_size, policy_id
                 FROM file_recipes
                 WHERE recipe_hash = ?1",
                params![recipe_hash.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| StagingError::Internal(format!("failed to load unverified recipe: {e}")))?
            .ok_or_else(|| {
                StagingError::StagingCorrupt("unverified recipe disappeared".to_owned())
            })?;
        let file_hash = decode_hash_blob("unverified recipe file hash", raw_file_hash)?;
        self.load_stored_recipe(recipe_hash, &file_hash, raw_file_size, &policy_id)
    }

    pub fn mark_recipe_verified(&self, recipe_hash: &[u8; 32]) -> Result<()> {
        let changed = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO verified_recipes (recipe_hash) VALUES (?1)",
                params![recipe_hash.as_slice()],
            )
            .map_err(|e| StagingError::Internal(format!("failed to mark recipe verified: {e}")))?;
        if changed == 0 {
            let exists: bool = self
                .conn
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM verified_recipes WHERE recipe_hash = ?1
                     )",
                    params![recipe_hash.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to confirm verified recipe: {e}"))
                })?;
            if !exists {
                return Err(StagingError::StagingCorrupt(
                    "verified recipe disappeared".to_owned(),
                ));
            }
        }
        Ok(())
    }

    pub fn quarantine_unverified_recipe(&self, recipe_hash: &[u8; 32], reason: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin recipe quarantine: {e}"))
        })?;
        tx.execute(
            "DELETE FROM path_leases
             WHERE recipe_hash = ?1",
            params![recipe_hash.as_slice()],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to hide corrupt unverified recipe: {e}"))
        })?;
        tx.execute(
            "INSERT OR REPLACE INTO staging_quarantine (kind, identity, reason)
             VALUES ('unverified-recipe', ?1, ?2)",
            params![recipe_hash.as_slice(), reason],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to quarantine unverified recipe: {e}"))
        })?;
        tx.commit()
            .map_err(|e| StagingError::Internal(format!("failed to commit recipe quarantine: {e}")))
    }

    pub fn finish_migration_validation(&self) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM staging_meta WHERE key = 'migration_validation_pending'",
                [],
            )
            .map(|_| ())
            .map_err(|e| {
                StagingError::Internal(format!("failed to finish migration validation: {e}"))
            })
    }

    /// Allocate a new segment id by inserting a row into `segments`.
    ///
    /// Returns the auto-assigned primary key.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn allocate_segment_id(&self) -> Result<u64> {
        self.conn
            .execute("INSERT INTO segments (size_bytes) VALUES (0)", [])
            .map_err(|e| StagingError::Internal(format!("failed to allocate segment id: {e}")))?;

        #[expect(
            clippy::cast_sign_loss,
            reason = "SQLite rowid is always non-negative for auto-increment PKs"
        )]
        let id = self.conn.last_insert_rowid() as u64;
        debug!(segment_id = id, "allocated segment id");
        Ok(id)
    }

    /// Mark a segment as the current (unsealed) segment.
    ///
    /// Sets `sealed_at = NULL` to indicate the segment is still accepting
    /// writes. This is a no-op if the row already has `sealed_at IS NULL`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn register_current_segment(&self, id: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE segments SET sealed_at = NULL WHERE segment_id = ?1",
                params![id],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to register current segment {id}: {e}"))
            })?;
        Ok(())
    }

    /// Insert pending chunk locators for the current segment.
    ///
    /// These rows live in `pending_chunks`; [`flush_pending`](Self::flush_pending)
    /// records the durable segment boundary that makes them recoverable.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn insert_pending(&self, rows: &[PendingRow]) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin pending insert tx: {e}"))
        })?;

        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO pending_chunks
                     (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(file_hash, chunk_index) DO NOTHING",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to prepare pending insert: {e}"))
                })?;

            for row in rows {
                let ch: &[u8] = &row.chunk_hash;
                let fh: &[u8] = &row.file_hash;
                let inserted = stmt
                    .execute(params![
                        ch,
                        fh,
                        row.chunk_index,
                        row.size,
                        row.segment_id,
                        row.segment_offset,
                    ])
                    .map_err(|e| {
                        StagingError::Internal(format!("failed to insert pending chunk: {e}",))
                    })?;
                if inserted == 0 {
                    ensure_pending_collision_is_idempotent(&tx, row)?;
                }
            }
        }

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit pending insert tx: {e}"))
        })?;
        Ok(())
    }

    /// Flush pending chunks for `segment_id`.
    ///
    /// Despite the name "flush", this function does **not** move rows from
    /// `pending_chunks` to `chunks`. Query paths read both tables; this
    /// function records `segments.size_bytes`, the durable byte boundary
    /// recovery uses to decide which pending locators survive a crash.
    /// The returned count is for metrics only.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn flush_pending(&self, segment_id: u64, new_size_bytes: u64) -> Result<u64> {
        // Count pending rows for metrics, then update the segment size.
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_chunks WHERE segment_id = ?1",
                params![segment_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to count pending for segment {segment_id}: {e}"
                ))
            })?;

        // Update the segment's committed size so recovery knows how
        // far the fsync reached.
        self.conn
            .execute(
                "UPDATE segments SET size_bytes = ?1 WHERE segment_id = ?2",
                params![new_size_bytes, segment_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to update segment {segment_id} size: {e}"))
            })?;

        debug!(segment_id, pending = count, "flushed segment");
        Ok(count as u64)
    }

    /// Look up a chunk by hash, returning its locator if staged.
    ///
    /// Single index lookup on `chunks_by_hash`. Returns `None` if the
    /// chunk is not in the committed `chunks` table.
    ///
    /// # Errors
    ///
    /// Look up a chunk by hash, returning its locator if staged.
    ///
    /// Checks the committed `chunks` table first, falling back to
    /// `pending_chunks`. Post-`flush_pending` rows still live in
    /// `pending_chunks`, so `locate` must check both tables to stay
    /// consistent with `locate_batch` and `get_chunk`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn locate(&self, chunk_hash: &[u8; 32]) -> Result<Option<ChunkLocator>> {
        let hash_slice: &[u8] = chunk_hash;
        let committed = self
            .conn
            .query_row(
                "SELECT segment_id, segment_offset, size
                 FROM chunks
                 WHERE chunk_hash = ?1
                 LIMIT 1",
                params![hash_slice],
                |row| {
                    Ok(ChunkLocator {
                        segment_id: row.get(0)?,
                        offset: row.get(1)?,
                        length: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|e| StagingError::Internal(format!("failed to locate chunk: {e}")))?;

        if committed.is_some() {
            return Ok(committed);
        }

        self.locate_pending(chunk_hash)
    }

    /// Return the byte offset one past the last committed record for a
    /// segment, used for torn-tail recovery.
    ///
    /// Computes `MAX(segment_offset + size + 8)` from `chunks` for the
    /// given segment (the `+8` accounts for the per-record framing:
    /// 4-byte length prefix + 4-byte CRC32C trailer).
    ///
    /// Returns `0` if no committed chunks exist for the segment.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn max_committed_offset(&self, segment_id: u64) -> Result<u64> {
        // Committed bytes are bounded by `segments.size_bytes` (updated
        // by flush_pending after every fsync) and by any rows registered
        // in `chunks`. `pending_chunks` MUST NOT extend the committed
        // boundary — those rows may reference post-fsync writes that
        // were lost in a crash. Recovery truncates anything past the
        // committed offset, so including pending offsets here would
        // make the truncation a no-op and leave torn-tail bytes in
        // place.
        let offset: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(off) FROM (
                    SELECT size_bytes AS off FROM segments WHERE segment_id = ?1
                    UNION ALL
                    SELECT MAX(segment_offset + size + 8) AS off FROM chunks WHERE segment_id = ?1
                )",
                params![segment_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to compute max committed offset for segment {segment_id}: {e}"
                ))
            })?;

        #[expect(
            clippy::cast_sign_loss,
            reason = "offset + size + 8 is always non-negative for valid data"
        )]
        Ok(offset.unwrap_or(0) as u64)
    }

    /// Return the byte offset one past the last promoted `chunks` record.
    ///
    /// Recovery may discard pending rows, but promoted chunk rows are already
    /// canonical staging metadata. If the segment file is shorter than this
    /// boundary, staging is corrupt and must fail closed instead of silently
    /// dropping committed rows.
    pub fn max_promoted_chunk_offset(&self, segment_id: u64) -> Result<u64> {
        let offset: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(segment_offset + size + 8)
                 FROM chunks
                 WHERE segment_id = ?1",
                params![segment_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to compute max promoted chunk offset for segment {segment_id}: {e}"
                ))
            })?;

        #[expect(
            clippy::cast_sign_loss,
            reason = "offset + size + 8 is always non-negative for valid data"
        )]
        Ok(offset.unwrap_or(0) as u64)
    }

    /// Return the byte offset one past the last pending record fully inside a boundary.
    ///
    /// Recovery uses this to trim unreferenced torn-tail bytes while preserving
    /// every pending locator whose full framed record survived on disk.
    pub fn max_recoverable_pending_offset(&self, segment_id: u64, max_offset: u64) -> Result<u64> {
        #[expect(clippy::cast_possible_wrap, reason = "offset fits in i64")]
        let max_offset_i64 = max_offset as i64;
        let offset: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(segment_offset + size + 8)
                 FROM pending_chunks
                 WHERE segment_id = ?1
                   AND segment_offset >= 0
                   AND size >= 0
                   AND segment_offset + size + 8 <= ?2",
                params![segment_id, max_offset_i64],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to compute max recoverable pending offset for segment {segment_id}: {e}"
                ))
            })?;

        #[expect(
            clippy::cast_sign_loss,
            reason = "offset + size + 8 is always non-negative for valid data"
        )]
        Ok(offset.unwrap_or(0) as u64)
    }

    /// Return segment ids eligible for sweep (zero live chunks, sealed,
    /// and no pending rows).
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn sweep_candidates(&self) -> Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.segment_id
                 FROM segments s
                 LEFT JOIN pending_chunks p ON p.segment_id = s.segment_id
                 WHERE s.live_chunk_count = 0
                   AND s.sealed_at IS NOT NULL
                 GROUP BY s.segment_id
                 HAVING COUNT(p.rowid) = 0",
            )
            .map_err(|e| StagingError::Internal(format!("failed to prepare sweep query: {e}")))?;

        let ids = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| StagingError::Internal(format!("failed to query sweep candidates: {e}")))?
            .collect::<std::result::Result<Vec<u64>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!("failed to collect sweep candidates: {e}"))
            })?;

        Ok(ids)
    }

    /// Return the lone unsealed segment when it has bytes but no
    /// locator rows in either chunks table.
    ///
    /// Healthy staging has at most one unsealed segment and stores it
    /// as `segments/current.seg`. A read-only sweep can reclaim that
    /// file after `retire_file` removes the last locator rows. If the
    /// index has zero or multiple unsealed rows, the caller cannot
    /// prove which row owns `current.seg`, so no candidate is returned.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn abandoned_current_segment(&self) -> Result<Option<(u64, u64, u64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.segment_id,
                        s.size_bytes,
                        s.chunk_count,
                        COALESCE(c.n, 0) AS committed_chunks,
                        COALESCE(p.n, 0) AS pending_chunks
                 FROM segments s
                 LEFT JOIN (
                     SELECT segment_id, COUNT(*) AS n FROM chunks GROUP BY segment_id
                 ) c ON c.segment_id = s.segment_id
                 LEFT JOIN (
                     SELECT segment_id, COUNT(*) AS n FROM pending_chunks GROUP BY segment_id
                 ) p ON p.segment_id = s.segment_id
                 WHERE s.sealed_at IS NULL",
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to prepare abandoned current segment query: {e}"
                ))
            })?;

        let rows = stmt
            .query_map([], |row| {
                let segment_id: u64 = row.get(0)?;
                let size_bytes: i64 = row.get(1)?;
                let chunk_count: i64 = row.get(2)?;
                let committed_chunks: i64 = row.get(3)?;
                let pending_chunks: i64 = row.get(4)?;
                #[expect(
                    clippy::cast_sign_loss,
                    reason = "segment counters are always non-negative"
                )]
                Ok((
                    segment_id,
                    size_bytes as u64,
                    chunk_count as u64,
                    committed_chunks as u64,
                    pending_chunks as u64,
                ))
            })
            .map_err(|e| {
                StagingError::Internal(format!("failed to query abandoned current segment: {e}"))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!("failed to collect abandoned current segment: {e}"))
            })?;

        let [(segment_id, size_bytes, chunk_count, committed_chunks, pending_chunks)] =
            rows.as_slice()
        else {
            return Ok(None);
        };

        if *size_bytes == 0 || *committed_chunks > 0 || *pending_chunks > 0 {
            return Ok(None);
        }

        Ok(Some((*segment_id, *size_bytes, *chunk_count)))
    }

    /// Return `(segment_id, size_bytes)` for every segment except
    /// `current_id` that has zero rows in `chunks` and `pending_chunks`.
    ///
    /// Used by the abandoned-segment cleanup path to reclaim disk from
    /// rolled-over-but-never-sealed segments left by a crashed `add`
    /// or an older binary that forgot to set `sealed_at`. These
    /// Segments that still have `pending_chunks` rows may be valid
    /// staged-but-unpushed content; they are kept until a retire path
    /// deletes those rows explicitly.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn abandoned_segments(&self, current_id: u64) -> Result<Vec<(u64, u64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.segment_id, s.size_bytes
                 FROM segments s
                 LEFT JOIN (
                     SELECT segment_id, COUNT(*) AS n FROM chunks GROUP BY segment_id
                 ) c ON c.segment_id = s.segment_id
                 LEFT JOIN (
                     SELECT segment_id, COUNT(*) AS n FROM pending_chunks GROUP BY segment_id
                 ) p ON p.segment_id = s.segment_id
                 WHERE s.segment_id != ?1
                   AND COALESCE(c.n, 0) = 0
                   AND COALESCE(p.n, 0) = 0",
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to prepare abandoned query: {e}"))
            })?;

        #[expect(
            clippy::cast_possible_wrap,
            reason = "segment id fits in i64 for SQLite parameter"
        )]
        let rows = stmt
            .query_map(params![current_id as i64], |row| {
                let seg_id: u64 = row.get(0)?;
                let size: i64 = row.get(1)?;
                #[expect(clippy::cast_sign_loss, reason = "size_bytes is always non-negative")]
                Ok((seg_id, size as u64))
            })
            .map_err(|e| StagingError::Internal(format!("failed to query abandoned: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!("failed to collect abandoned segments: {e}"))
            })?;

        Ok(rows)
    }

    /// Return the number of committed `chunks` rows for a segment.
    ///
    /// Used by `clean_abandoned` to decide whether the writer's active
    /// current segment is orphaned (zero committed chunks) and can be
    /// reset.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn segment_committed_chunk_count(&self, segment_id: u64) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE segment_id = ?1",
                params![segment_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to count committed chunks for segment {segment_id}: {e}"
                ))
            })?;
        #[expect(clippy::cast_sign_loss, reason = "COUNT(*) is always non-negative")]
        Ok(count as u64)
    }

    /// Return the number of `pending_chunks` rows for a segment.
    ///
    /// Used by `clean_abandoned` to preserve current segments that
    /// still hold staged pending rows.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn segment_pending_chunk_count(&self, segment_id: u64) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_chunks WHERE segment_id = ?1",
                params![segment_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to count pending chunks for segment {segment_id}: {e}"
                ))
            })?;
        #[expect(clippy::cast_sign_loss, reason = "COUNT(*) is always non-negative")]
        Ok(count as u64)
    }

    /// Delete every `pending_chunks` row whose `segment_id` no longer
    /// exists in the `segments` table.
    ///
    /// Used by `clean_abandoned` as a safety net to cover historical
    /// orphans from earlier drops that forgot to cascade the pending
    /// rows. Safe because without a corresponding segment row there
    /// is no `.seg` file to read from — the pending rows are
    /// unreadable.
    ///
    /// Returns the number of rows removed.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn delete_orphan_pending_rows(&self) -> Result<u64> {
        let n = self
            .conn
            .execute(
                "DELETE FROM pending_chunks
                 WHERE segment_id NOT IN (SELECT segment_id FROM segments)",
                [],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to delete orphan pending rows: {e}"))
            })?;
        Ok(n as u64)
    }

    /// Mark a segment as sealed with the current timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn seal_segment(&self, segment_id: u64, size_bytes: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE segments SET sealed_at = datetime('now'), size_bytes = ?1 WHERE segment_id = ?2",
                params![size_bytes, segment_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to seal segment {segment_id}: {e}"
                ))
            })?;
        debug!(segment_id, size_bytes, "sealed segment in index");
        Ok(())
    }

    /// Return `(size_bytes, chunk_count)` for a segment.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure or if the
    /// segment does not exist.
    pub fn segment_info(&self, segment_id: u64) -> Result<(u64, u64)> {
        self.conn
            .query_row(
                "SELECT size_bytes, chunk_count FROM segments WHERE segment_id = ?1",
                params![segment_id],
                |row| {
                    let size: i64 = row.get(0)?;
                    let count: i64 = row.get(1)?;
                    #[expect(
                        clippy::cast_sign_loss,
                        reason = "size_bytes and chunk_count are always non-negative"
                    )]
                    Ok((size as u64, count as u64))
                },
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to read segment info for {segment_id}: {e}"))
            })
    }

    /// Delete a segment and all its chunk rows.
    ///
    /// Removes from both `chunks` (first, to satisfy FK) and `segments`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn drop_segment(&self, id: u64) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| StagingError::Internal(format!("failed to begin drop segment tx: {e}")))?;

        tx.execute("DELETE FROM chunks WHERE segment_id = ?1", params![id])
            .map_err(|e| {
                StagingError::Internal(format!("failed to delete chunks for segment {id}: {e}"))
            })?;

        // Also drop pending rows pointing at this segment. Without this,
        // a later allocate_segment_id reusing this id would see leftover
        // pending rows from a different write — and a `staging stats`
        // or sweep would report phantom chunks.
        tx.execute(
            "DELETE FROM pending_chunks WHERE segment_id = ?1",
            params![id],
        )
        .map_err(|e| {
            StagingError::Internal(format!(
                "failed to delete pending chunks for segment {id}: {e}"
            ))
        })?;

        tx.execute("DELETE FROM segments WHERE segment_id = ?1", params![id])
            .map_err(|e| StagingError::Internal(format!("failed to delete segment {id}: {e}")))?;

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit drop segment tx: {e}"))
        })?;

        debug!(segment_id = id, "dropped segment");
        Ok(())
    }

    /// Check whether a chunk hash exists in either `chunks` or `pending_chunks`.
    ///
    /// Used by `register_file` to verify that every chunk in the file's
    /// manifest was previously staged via `stage_chunk`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn chunk_exists_anywhere(&self, chunk_hash: &[u8; 32]) -> Result<bool> {
        let hash_slice: &[u8] = chunk_hash;
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM chunks WHERE chunk_hash = ?1
                    UNION ALL
                    SELECT 1 FROM pending_chunks WHERE chunk_hash = ?1
                    LIMIT 1
                )",
                params![hash_slice],
                |row| row.get(0),
            )
            .map_err(|e| StagingError::Internal(format!("failed to check chunk existence: {e}")))?;
        Ok(exists)
    }

    /// Check whether a chunk exists in `chunks` for a specific file_hash.
    #[cfg(test)]
    #[expect(
        dead_code,
        reason = "property tests probe file-scoped index invariants directly"
    )]
    pub fn chunk_exists_for_file(
        &self,
        chunk_hash: &[u8; 32],
        file_hash: &[u8; 32],
    ) -> Result<bool> {
        let ch: &[u8] = chunk_hash;
        let fh: &[u8] = file_hash;
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM chunks WHERE chunk_hash = ?1 AND file_hash = ?2 LIMIT 1)",
                params![ch, fh],
                |row| row.get(0),
            )
            .map_err(|e| StagingError::Internal(format!("chunk_exists_for_file: {e}")))?;
        Ok(exists)
    }

    /// Check whether a chunk exists in `pending_chunks` for a specific file_hash.
    #[cfg(test)]
    #[expect(
        dead_code,
        reason = "property tests probe pending-row index invariants directly"
    )]
    pub fn pending_chunk_exists_for_file(
        &self,
        chunk_hash: &[u8; 32],
        file_hash: &[u8; 32],
    ) -> Result<bool> {
        let ch: &[u8] = chunk_hash;
        let fh: &[u8] = file_hash;
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pending_chunks WHERE chunk_hash = ?1 AND file_hash = ?2 LIMIT 1)",
                params![ch, fh],
                |row| row.get(0),
            )
            .map_err(|e| StagingError::Internal(format!("pending_chunk_exists_for_file: {e}")))?;
        Ok(exists)
    }

    /// Return the first requested chunk position already present in `pending_chunks`.
    pub fn first_pending_position_for_file(
        &self,
        file_hash: &[u8; 32],
        chunk_indices: &[i64],
    ) -> Result<Option<i64>> {
        if chunk_indices.is_empty() {
            return Ok(None);
        }

        let placeholders = vec!["?"; chunk_indices.len()].join(",");
        let sql = format!(
            "SELECT chunk_index
             FROM pending_chunks
             WHERE file_hash = ? AND chunk_index IN ({placeholders})
             ORDER BY chunk_index
             LIMIT 1"
        );
        let fh: &[u8] = file_hash;
        let mut query_params: Vec<&dyn ToSql> = Vec::with_capacity(chunk_indices.len() + 1);
        query_params.push(&fh);
        query_params.extend(chunk_indices.iter().map(|idx| idx as &dyn ToSql));

        self.conn
            .query_row(&sql, query_params.as_slice(), |row| row.get(0))
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to query pending chunk positions: {e}"))
            })
    }

    /// Return the ordered list of chunk hashes for a given file.
    ///
    /// Queries both `chunks` and `pending_chunks` tables, deduplicated
    /// by `chunk_index` (preferring `chunks` over `pending_chunks`),
    /// ordered by `chunk_index`. Returns an empty vec if the file is
    /// not found.
    pub fn chunks_for_file(&self, file_hash: &[u8; 32]) -> Result<Vec<[u8; 32]>> {
        let fh: &[u8] = file_hash;
        let mut stmt = self
            .conn
            .prepare_cached(
                "WITH combined AS (
                     SELECT chunk_hash, chunk_index, 0 AS priority, rowid
                     FROM chunks
                     WHERE file_hash = ?1
                     UNION ALL
                     SELECT chunk_hash, chunk_index, 1 AS priority, rowid
                     FROM pending_chunks
                     WHERE file_hash = ?1
                 )
                 SELECT chunk_hash, chunk_index
                 FROM combined c
                 WHERE NOT EXISTS (
                     SELECT 1
                     FROM combined p
                     WHERE p.chunk_index = c.chunk_index
                       AND (
                           p.priority < c.priority
                           OR (p.priority = c.priority AND p.rowid < c.rowid)
                       )
                 )
                 ORDER BY chunk_index",
            )
            .map_err(|e| StagingError::Internal(format!("prepare chunks_for_file: {e}")))?;

        let rows: Vec<(Vec<u8>, i64)> = stmt
            .query_map(params![fh], |row| {
                let blob: Vec<u8> = row.get(0)?;
                let chunk_index: i64 = row.get(1)?;
                Ok((blob, chunk_index))
            })
            .map_err(|e| StagingError::Internal(format!("query chunks_for_file: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| StagingError::Internal(format!("collect chunks_for_file: {e}")))?;

        let mut hashes = Vec::with_capacity(rows.len());
        for (expected_index, (hash_blob, chunk_index)) in rows.into_iter().enumerate() {
            validate_chunk_index(expected_index, chunk_index)?;
            hashes.push(decode_chunk_hash_blob(hash_blob)?);
        }

        Ok(hashes)
    }

    /// Return the ordered chunk hashes and sizes for a given file.
    ///
    /// Queries both durable and pending chunk tables, deduplicated by
    /// `chunk_index` with durable rows preferred over pending rows.
    pub fn chunks_for_file_with_sizes(&self, file_hash: &[u8; 32]) -> Result<Vec<([u8; 32], u64)>> {
        self.chunks_for_file_with_locators(file_hash).map(|chunks| {
            chunks
                .into_iter()
                .map(|chunk| (chunk.chunk_hash, chunk.size))
                .collect()
        })
    }

    /// Return the ordered chunk hashes, sizes, and segment locators for a given file.
    ///
    /// Queries both durable and pending chunk tables, deduplicated by
    /// `chunk_index` with durable rows preferred over pending rows.
    pub(crate) fn chunks_for_file_with_locators(
        &self,
        file_hash: &[u8; 32],
    ) -> Result<Vec<FileChunkLocator>> {
        let fh: &[u8] = file_hash;
        let mut stmt = self
            .conn
            .prepare_cached(
                "WITH combined AS (
                     SELECT chunk_hash, size, segment_id, segment_offset, chunk_index, 0 AS priority, rowid
                     FROM chunks
                     WHERE file_hash = ?1
                     UNION ALL
                     SELECT chunk_hash, size, segment_id, segment_offset, chunk_index, 1 AS priority, rowid
                     FROM pending_chunks
                     WHERE file_hash = ?1
                 )
                 SELECT chunk_hash, size, segment_id, segment_offset, chunk_index
                 FROM combined c
                 WHERE NOT EXISTS (
                     SELECT 1
                     FROM combined p
                     WHERE p.chunk_index = c.chunk_index
                       AND (
                           p.priority < c.priority
                           OR (p.priority = c.priority AND p.rowid < c.rowid)
                       )
                 )
                 ORDER BY chunk_index",
            )
            .map_err(|e| {
                StagingError::Internal(format!("prepare chunks_for_file_with_locators: {e}"))
            })?;

        let rows: Vec<(Vec<u8>, i64, u64, u64, i64)> = stmt
            .query_map(params![fh], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(|e| {
                StagingError::Internal(format!("query chunks_for_file_with_locators: {e}"))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!("collect chunks_for_file_with_locators: {e}"))
            })?;

        let mut chunks = Vec::with_capacity(rows.len());
        for (expected_index, (hash_blob, size, segment_id, segment_offset, chunk_index)) in
            rows.into_iter().enumerate()
        {
            validate_chunk_index(expected_index, chunk_index)?;
            let hash = decode_chunk_hash_blob(hash_blob)?;
            let size = u64::try_from(size)
                .map_err(|_| StagingError::StagingCorrupt("chunk size is negative".to_owned()))?;
            let length = u32::try_from(size).map_err(|_| {
                StagingError::StagingCorrupt(format!("chunk size {size} exceeds u32::MAX"))
            })?;
            chunks.push(FileChunkLocator {
                chunk_hash: hash,
                size,
                locator: ChunkLocator {
                    segment_id,
                    offset: segment_offset,
                    length,
                },
            });
        }

        Ok(chunks)
    }

    /// Insert a file row into the `files` table.
    ///
    /// Re-registering a file updates metadata in place so existing
    /// child rows in `chunks` keep their parent row.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "file sizes fit in i64 for SQLite storage"
    )]
    pub fn insert_file(&self, file_hash: &[u8; 32], total_bytes: u64) -> Result<()> {
        let fh: &[u8] = file_hash;
        self.conn
            .execute(
                "INSERT INTO files (file_hash, total_bytes)
                 VALUES (?1, ?2)
                 ON CONFLICT(file_hash) DO UPDATE
                 SET total_bytes = excluded.total_bytes",
                params![fh, total_bytes as i64],
            )
            .map_err(|e| StagingError::Internal(format!("failed to insert file: {e}")))?;
        Ok(())
    }

    /// Record the original file path for a staged file hash.
    ///
    /// Uses `INSERT OR REPLACE` so re-adding a file updates the path
    /// (e.g. if the file was moved between adds).
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn insert_file_path(&self, file_hash: &[u8; 32], file_path: &str) -> Result<()> {
        let fh: &[u8] = file_hash;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO file_paths (file_hash, file_path) VALUES (?1, ?2)",
                params![fh, file_path],
            )
            .map_err(|e| StagingError::Internal(format!("failed to insert file_path: {e}")))?;
        Ok(())
    }

    pub fn insert_batch(&self, batch_id: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO staging_batches (batch_id, state) VALUES (?1, 'open')",
                params![batch_id],
            )
            .map_err(|e| StagingError::Internal(format!("failed to create staging batch: {e}")))?;
        Ok(())
    }

    pub fn mark_batch_published(&self, batch_id: &str) -> Result<()> {
        let changed = self
            .conn
            .execute(
                "UPDATE staging_batches SET state = 'published' WHERE batch_id = ?1",
                params![batch_id],
            )
            .map_err(|e| StagingError::Internal(format!("failed to publish staging batch: {e}")))?;
        if changed != 1 {
            return Err(StagingError::NotFound {
                path: format!("staging batch {batch_id}"),
            });
        }
        Ok(())
    }

    /// Load the exact immutable recipe owned by published path leases.
    ///
    /// This is the push/read boundary for layout v2. Legacy `files/chunks`
    /// rows remain the physical segment-write journal, but they cannot define
    /// a publishable file once migration has completed.
    pub fn published_recipe_for_file(
        &self,
        file_hash: &[u8; 32],
    ) -> Result<Option<crate::recipe::FileRecipe>> {
        let rows = {
            let mut statement = self
                .conn
                .prepare_cached(
                    "SELECT DISTINCT recipe.recipe_hash, recipe.file_size, recipe.policy_id
                     FROM file_recipes AS recipe
                     JOIN verified_recipes AS verified USING (recipe_hash)
                     JOIN path_leases AS lease USING (recipe_hash)
                     JOIN staging_batches AS batch USING (batch_id)
                     WHERE recipe.file_hash = ?1 AND batch.state = 'published'
                     ORDER BY recipe.recipe_hash",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to prepare published recipe query: {e}"))
                })?;
            statement
                .query_map(params![file_hash.as_slice()], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| {
                    StagingError::Internal(format!("failed to query published recipes: {e}"))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| {
                    StagingError::Internal(format!("failed to collect published recipes: {e}"))
                })?
        };

        let mut selected = None;
        for (raw_recipe_hash, raw_file_size, policy_id) in rows {
            let stored_recipe_hash = decode_hash_blob("published recipe hash", raw_recipe_hash)?;
            let recipe =
                self.load_stored_recipe(&stored_recipe_hash, file_hash, raw_file_size, &policy_id)?;
            if let Some(prior) = &selected
                && prior != &recipe
            {
                return Err(StagingError::StagingCorrupt(
                    "one file hash has conflicting published recipes".to_owned(),
                ));
            }
            selected = Some(recipe);
        }
        Ok(selected)
    }

    fn load_stored_recipe(
        &self,
        stored_recipe_hash: &[u8; 32],
        file_hash: &[u8; 32],
        raw_file_size: i64,
        policy_id: &str,
    ) -> Result<crate::recipe::FileRecipe> {
        let file_size = u64::try_from(raw_file_size)
            .map_err(|_| StagingError::StagingCorrupt("negative stored recipe size".to_owned()))?;
        let policy = crate::recipe::ChunkingPolicyId::parse(policy_id)?;
        let occurrences = {
            let mut statement = self
                .conn
                .prepare_cached(
                    "SELECT occurrence.occurrence, occurrence.chunk_hash,
                            occurrence.chunk_offset, occurrence.chunk_size,
                            (
                                SELECT payload.size
                                FROM chunk_payloads AS payload
                                WHERE payload.chunk_hash = occurrence.chunk_hash
                                LIMIT 1
                            ),
                            (
                                SELECT prepared.uncompressed_size
                                FROM prepared_xorb_chunks AS prepared
                                JOIN prepared_xorbs AS xorb
                                  ON xorb.file_hash = prepared.file_hash
                                 AND xorb.xorb_hash = prepared.xorb_hash
                                WHERE prepared.chunk_hash = occurrence.chunk_hash
                                ORDER BY prepared.file_hash, prepared.xorb_hash
                                LIMIT 1
                            )
                     FROM recipe_occurrences AS occurrence
                     WHERE occurrence.recipe_hash = ?1
                     ORDER BY occurrence.occurrence",
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to prepare stored recipe occurrence query: {e}"
                    ))
                })?;
            statement
                .query_map(params![stored_recipe_hash.as_slice()], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                })
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to query stored recipe occurrences: {e}"
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to collect stored recipe occurrences: {e}"
                    ))
                })?
        };
        let mut chunks = Vec::with_capacity(occurrences.len());
        let mut expected_offset = 0u64;
        for (expected_occurrence, row) in occurrences.into_iter().enumerate() {
            let (
                occurrence,
                raw_chunk_hash,
                raw_offset,
                raw_size,
                raw_segment_size,
                raw_prepared_size,
            ) = row;
            validate_chunk_index(expected_occurrence, occurrence)?;
            let chunk_hash = decode_hash_blob("stored recipe chunk", raw_chunk_hash)?;
            let offset = u64::try_from(raw_offset).map_err(|_| {
                StagingError::StagingCorrupt("negative stored recipe chunk offset".to_owned())
            })?;
            let size = u64::try_from(raw_size).map_err(|_| {
                StagingError::StagingCorrupt("negative stored recipe chunk size".to_owned())
            })?;
            let payload_matches = [raw_segment_size, raw_prepared_size]
                .into_iter()
                .flatten()
                .any(|payload_size| u64::try_from(payload_size) == Ok(size));
            if offset != expected_offset || !payload_matches {
                return Err(StagingError::StagingCorrupt(
                    "stored recipe occurrence does not match its payload inventory".to_owned(),
                ));
            }
            expected_offset = expected_offset.checked_add(size).ok_or_else(|| {
                StagingError::StagingCorrupt("stored recipe byte length overflow".to_owned())
            })?;
            chunks.push((crab_xet::hash::MerkleHash::from(chunk_hash), size));
        }
        let recipe = crate::recipe::FileRecipe::from_staged_chunks(
            policy,
            crab_xet::hash::MerkleHash::from(*file_hash),
            file_size,
            &chunks,
        )?;
        if recipe.hash() != *stored_recipe_hash {
            return Err(StagingError::StagingCorrupt(
                "stored recipe hash does not match its occurrences".to_owned(),
            ));
        }
        Ok(recipe)
    }

    /// Atomically pin the exact immutable recipes consumed by one push.
    ///
    /// Every recipe must already be owned by a published path lease. The
    /// snapshot is therefore a reference to committed local staging state,
    /// never a way to manufacture ownership from a pointer alone.
    pub fn create_push_snapshot(
        &self,
        snapshot_id: &str,
        recipes: &[crate::recipe::FileRecipe],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin push snapshot transaction: {e}"))
        })?;
        tx.execute(
            "INSERT INTO push_snapshots (snapshot_id, state) VALUES (?1, 'open')",
            params![snapshot_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to create push snapshot {snapshot_id}: {e}"))
        })?;

        for recipe in recipes {
            let recipe_hash = recipe.hash();
            let file_hash: [u8; 32] = recipe.sequence().file_hash.into();
            let stored: Option<(Vec<u8>, i64, String)> = tx
                .query_row(
                    "SELECT file_hash, file_size, policy_id
                     FROM file_recipes WHERE recipe_hash = ?1",
                    params![recipe_hash.as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to read recipe for push snapshot {snapshot_id}: {e}"
                    ))
                })?;
            let Some((stored_file_hash, stored_size, stored_policy)) = stored else {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} references missing recipe {}",
                    blake3::Hash::from(recipe_hash).to_hex()
                )));
            };
            if stored_file_hash.as_slice() != file_hash
                || stored_size != sqlite_i64("recipe file size", recipe.sequence().file_size)?
                || stored_policy != recipe.policy().as_str()
            {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} recipe {} identity does not match its stored row",
                    blake3::Hash::from(recipe_hash).to_hex()
                )));
            }

            let stored_occurrences = {
                let mut statement = tx
                    .prepare_cached(
                        "SELECT chunk_hash, chunk_offset, chunk_size
                         FROM recipe_occurrences
                         WHERE recipe_hash = ?1
                         ORDER BY occurrence",
                    )
                    .map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to prepare snapshot recipe occurrence query: {e}"
                        ))
                    })?;
                statement
                    .query_map(params![recipe_hash.as_slice()], |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })
                    .map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to query snapshot recipe occurrences: {e}"
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to collect snapshot recipe occurrences: {e}"
                        ))
                    })?
            };
            if stored_occurrences.len() != recipe.sequence().spans.len() {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} recipe {} has {} stored occurrences, expected {}",
                    blake3::Hash::from(recipe_hash).to_hex(),
                    stored_occurrences.len(),
                    recipe.sequence().spans.len()
                )));
            }
            for (stored, expected) in stored_occurrences.iter().zip(&recipe.sequence().spans) {
                let expected_hash: [u8; 32] = expected.chunk_hash.into();
                if stored.0.as_slice() != expected_hash
                    || stored.1 != sqlite_i64("recipe chunk offset", expected.offset)?
                    || stored.2 != sqlite_i64("recipe chunk size", expected.len)?
                {
                    return Err(StagingError::StagingCorrupt(format!(
                        "push snapshot {snapshot_id} recipe {} occurrence data changed",
                        blake3::Hash::from(recipe_hash).to_hex()
                    )));
                }
            }

            let published_lease: bool = tx
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1
                         FROM path_leases AS lease
                         JOIN staging_batches AS batch USING (batch_id)
                         WHERE lease.recipe_hash = ?1 AND batch.state = 'published'
                     )",
                    params![recipe_hash.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to verify published recipe lease for snapshot {snapshot_id}: {e}"
                    ))
                })?;
            if !published_lease {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} recipe {} has no published path lease",
                    blake3::Hash::from(recipe_hash).to_hex()
                )));
            }
            tx.execute(
                "INSERT INTO push_snapshot_recipes (snapshot_id, recipe_hash)
                 VALUES (?1, ?2)",
                params![snapshot_id, recipe_hash.as_slice()],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to pin recipe in push snapshot {snapshot_id}: {e}"
                ))
            })?;
            let captured = tx
                .execute(
                    "INSERT INTO push_snapshot_leases
                     (snapshot_id, batch_id, path_bytes, file_hash, recipe_hash)
                     SELECT ?1, lease.batch_id, lease.path_bytes,
                            lease.file_hash, lease.recipe_hash
                     FROM path_leases AS lease
                     JOIN staging_batches AS batch USING (batch_id)
                     WHERE lease.recipe_hash = ?2 AND batch.state = 'published'",
                    params![snapshot_id, recipe_hash.as_slice()],
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to capture exact leases for push snapshot {snapshot_id}: {e}"
                    ))
                })?;
            if captured == 0 {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} recipe {} lost its published lease",
                    blake3::Hash::from(recipe_hash).to_hex()
                )));
            }
        }

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit push snapshot {snapshot_id}: {e}"))
        })
    }

    pub fn commit_push_snapshot(&self, snapshot_id: &str) -> Result<()> {
        let changed = self
            .conn
            .execute(
                "UPDATE push_snapshots SET state = 'committed'
                 WHERE snapshot_id = ?1 AND state = 'open'",
                params![snapshot_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to commit push snapshot {snapshot_id}: {e}"))
            })?;
        if changed == 1 {
            return Ok(());
        }
        let state: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM push_snapshots WHERE snapshot_id = ?1",
                params![snapshot_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to confirm push snapshot {snapshot_id} commit: {e}"
                ))
            })?;
        match state.as_deref() {
            Some("committed" | "retired") => Ok(()),
            Some(state) => Err(StagingError::StagingCorrupt(format!(
                "push snapshot {snapshot_id} remained {state}"
            ))),
            None => Err(StagingError::NotFound {
                path: format!("push snapshot {snapshot_id}"),
            }),
        }
    }

    /// Retire only the exact published path leases captured by one committed
    /// push snapshot. Returns file hashes whose final lease disappeared and
    /// whose physical chunk rows can therefore be reclaimed.
    pub fn retire_push_snapshot(&self, snapshot_id: &str) -> Result<Vec<[u8; 32]>> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!(
                "failed to begin push snapshot retirement {snapshot_id}: {e}"
            ))
        })?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM push_snapshots WHERE snapshot_id = ?1",
                params![snapshot_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to read push snapshot {snapshot_id} for retirement: {e}"
                ))
            })?;
        match state.as_deref() {
            Some("committed" | "retired") => {}
            Some(state) => {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} is {state}, not committed"
                )));
            }
            None => {
                return Err(StagingError::NotFound {
                    path: format!("push snapshot {snapshot_id}"),
                });
            }
        }

        let file_hashes = {
            let mut statement = tx
                .prepare_cached(
                    "SELECT DISTINCT file_hash FROM push_snapshot_leases
                     WHERE snapshot_id = ?1",
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to prepare snapshot lease files {snapshot_id}: {e}"
                    ))
                })?;
            statement
                .query_map(params![snapshot_id], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to query snapshot lease files {snapshot_id}: {e}"
                    ))
                })?
                .map(|row| {
                    row.map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to collect snapshot lease file {snapshot_id}: {e}"
                        ))
                    })
                    .and_then(|hash| decode_hash_blob("snapshot lease file hash", hash))
                })
                .collect::<Result<Vec<_>>>()?
        };

        if state.as_deref() == Some("committed") {
            tx.execute(
                "DELETE FROM path_leases
                 WHERE EXISTS (
                     SELECT 1 FROM push_snapshot_leases AS captured
                     WHERE captured.snapshot_id = ?1
                       AND captured.batch_id = path_leases.batch_id
                       AND captured.path_bytes = path_leases.path_bytes
                       AND captured.recipe_hash = path_leases.recipe_hash
                 )
                   AND NOT EXISTS (
                     SELECT 1
                     FROM push_snapshot_leases AS other
                     JOIN push_snapshots AS snapshot USING (snapshot_id)
                     WHERE other.snapshot_id != ?1
                       AND snapshot.state = 'open'
                       AND other.batch_id = path_leases.batch_id
                       AND other.path_bytes = path_leases.path_bytes
                       AND other.recipe_hash = path_leases.recipe_hash
                 )",
                params![snapshot_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to retire exact leases for push snapshot {snapshot_id}: {e}"
                ))
            })?;
            tx.execute(
                "UPDATE push_snapshots SET state = 'retired' WHERE snapshot_id = ?1",
                params![snapshot_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to mark push snapshot {snapshot_id} retired: {e}"
                ))
            })?;
            tx.execute(
                "DELETE FROM push_snapshot_recipes
                 WHERE snapshot_id IN (
                     SELECT snapshot_id FROM push_snapshots WHERE state != 'open'
                 )",
                [],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to release committed snapshot recipe pins: {e}"
                ))
            })?;
            tx.execute(
                "DELETE FROM staging_batches
                 WHERE state = 'published'
                   AND NOT EXISTS (
                       SELECT 1 FROM path_leases
                       WHERE path_leases.batch_id = staging_batches.batch_id
                   )",
                [],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to remove empty batches after snapshot retirement: {e}"
                ))
            })?;
        }

        let mut unleased = Vec::new();
        for file_hash in file_hashes {
            let remaining: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM path_leases WHERE file_hash = ?1)",
                    params![file_hash.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to check leases after snapshot retirement: {e}"
                    ))
                })?;
            if !remaining {
                unleased.push(file_hash);
            }
        }
        tx.commit().map_err(|e| {
            StagingError::Internal(format!(
                "failed to commit push snapshot retirement {snapshot_id}: {e}"
            ))
        })?;
        Ok(unleased)
    }

    pub fn remove_push_snapshot(&self, snapshot_id: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM push_snapshots WHERE snapshot_id = ?1",
                params![snapshot_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to remove push snapshot {snapshot_id}: {e}"))
            })?;
        Ok(())
    }

    pub fn remove_open_push_snapshot(&self, snapshot_id: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM push_snapshots
                 WHERE snapshot_id = ?1 AND state = 'open'",
                params![snapshot_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to discard open push snapshot {snapshot_id}: {e}"
                ))
            })?;
        Ok(())
    }

    pub fn push_snapshot_states(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT snapshot_id, state FROM push_snapshots ORDER BY snapshot_id")
            .map_err(|e| {
                StagingError::Internal(format!("failed to prepare push snapshot listing: {e}"))
            })?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| StagingError::Internal(format!("failed to query push snapshots: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| StagingError::Internal(format!("failed to collect push snapshots: {e}")))
    }

    /// Atomically persist an immutable recipe and attach one batch/path lease.
    pub fn insert_recipe_lease(
        &self,
        batch_id: &str,
        path_bytes: &[u8],
        recipe: &crate::recipe::FileRecipe,
        verification: RecipeVerification,
    ) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS incoming_recipe_occurrences (
                    occurrence   INTEGER PRIMARY KEY,
                    chunk_hash   BLOB NOT NULL,
                    chunk_offset INTEGER NOT NULL,
                    chunk_size   INTEGER NOT NULL
                ) WITHOUT ROWID;",
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to create incoming recipe occurrence table: {e}"
                ))
            })?;
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin recipe lease transaction: {e}"))
        })?;
        tx.execute("DELETE FROM temp.incoming_recipe_occurrences", [])
            .map_err(|e| {
                StagingError::Internal(format!("failed to clear incoming recipe occurrences: {e}"))
            })?;
        let recipe_hash_array = recipe.hash();
        let recipe_hash: &[u8] = &recipe_hash_array;
        let file_hash_array: [u8; 32] = recipe.sequence().file_hash.into();
        let file_hash: &[u8] = &file_hash_array;
        tx.execute(
            "INSERT OR IGNORE INTO file_recipes
             (recipe_hash, file_hash, file_size, policy_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                recipe_hash,
                file_hash,
                sqlite_i64("recipe file size", recipe.sequence().file_size)?,
                recipe.policy().as_str()
            ],
        )
        .map_err(|e| StagingError::Internal(format!("failed to insert file recipe: {e}")))?;
        let stored_recipe: (Vec<u8>, i64, String) = tx
            .query_row(
                "SELECT file_hash, file_size, policy_id
                 FROM file_recipes WHERE recipe_hash = ?1",
                params![recipe_hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to validate stored file recipe: {e}"))
            })?;
        if stored_recipe.0.as_slice() != file_hash
            || stored_recipe.1 != sqlite_i64("recipe file size", recipe.sequence().file_size)?
            || stored_recipe.2 != recipe.policy().as_str()
        {
            return Err(StagingError::StagingCorrupt(
                "stored recipe identity collides with different file metadata".to_owned(),
            ));
        }

        for (batch_index, chunks) in recipe
            .sequence()
            .spans
            .chunks(RECIPE_OCCURRENCE_INSERT_BATCH)
            .enumerate()
        {
            let batch_offset = batch_index
                .checked_mul(RECIPE_OCCURRENCE_INSERT_BATCH)
                .ok_or_else(|| {
                    StagingError::StagingCorrupt("too many recipe occurrences".to_owned())
                })?;
            let placeholders = vec!["(?, ?, ?, ?)"; chunks.len()].join(",");
            let sql = format!(
                "INSERT INTO temp.incoming_recipe_occurrences
                 (occurrence, chunk_hash, chunk_offset, chunk_size)
                 VALUES {placeholders}"
            );
            let mut values = Vec::with_capacity(chunks.len() * 4);
            for (relative_index, chunk) in chunks.iter().enumerate() {
                let occurrence = batch_offset.checked_add(relative_index).ok_or_else(|| {
                    StagingError::StagingCorrupt("too many recipe occurrences".to_owned())
                })?;
                let occurrence = i64::try_from(occurrence).map_err(|_| {
                    StagingError::StagingCorrupt("too many recipe occurrences".to_owned())
                })?;
                let chunk_hash: [u8; 32] = chunk.chunk_hash.into();
                values.push(Value::Integer(occurrence));
                values.push(Value::Blob(chunk_hash.to_vec()));
                values.push(Value::Integer(sqlite_i64(
                    "recipe chunk offset",
                    chunk.offset,
                )?));
                values.push(Value::Integer(sqlite_i64("recipe chunk size", chunk.len)?));
            }
            tx.execute(&sql, params_from_iter(values)).map_err(|e| {
                StagingError::Internal(format!(
                    "failed to load incoming recipe occurrence batch: {e}"
                ))
            })?;
        }

        let collision: Option<i64> = tx
            .query_row(
                "SELECT incoming.occurrence
                 FROM temp.incoming_recipe_occurrences AS incoming
                 JOIN recipe_occurrences AS stored
                   ON stored.recipe_hash = ?1
                  AND stored.occurrence = incoming.occurrence
                 WHERE stored.chunk_hash != incoming.chunk_hash
                    OR stored.chunk_offset != incoming.chunk_offset
                    OR stored.chunk_size != incoming.chunk_size
                 LIMIT 1",
                params![recipe_hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to validate stored recipe occurrences: {e}"))
            })?;
        if let Some(occurrence) = collision {
            return Err(StagingError::StagingCorrupt(format!(
                "stored recipe occurrence {occurrence} collides with different chunk metadata"
            )));
        }

        let missing_payload: Option<i64> = tx
            .query_row(
                "SELECT incoming.occurrence
                 FROM temp.incoming_recipe_occurrences AS incoming
                 LEFT JOIN chunk_payloads AS payload
                   ON payload.chunk_hash = incoming.chunk_hash
                  AND payload.size = incoming.chunk_size
                 WHERE payload.chunk_hash IS NULL
                   AND NOT EXISTS (
                       SELECT 1
                       FROM prepared_xorb_chunks AS prepared
                       JOIN prepared_xorbs AS xorb
                         ON xorb.file_hash = prepared.file_hash
                        AND xorb.xorb_hash = prepared.xorb_hash
                       WHERE prepared.chunk_hash = incoming.chunk_hash
                         AND prepared.uncompressed_size = incoming.chunk_size
                   )
                 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to validate recipe payload inventory: {e}"))
            })?;
        if let Some(occurrence) = missing_payload {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe occurrence {occurrence} has no exact staged payload"
            )));
        }

        tx.execute(
            "INSERT OR IGNORE INTO recipe_occurrences
             (recipe_hash, occurrence, chunk_hash, chunk_offset, chunk_size)
             SELECT ?1, occurrence, chunk_hash, chunk_offset, chunk_size
             FROM temp.incoming_recipe_occurrences
             ORDER BY occurrence",
            params![recipe_hash],
        )
        .map_err(|e| StagingError::Internal(format!("failed to insert recipe occurrences: {e}")))?;
        tx.execute(
            "INSERT OR IGNORE INTO recipe_payload_leases (recipe_hash, chunk_hash)
             SELECT DISTINCT ?1, incoming.chunk_hash
             FROM temp.incoming_recipe_occurrences AS incoming
             JOIN chunk_payloads AS payload
               ON payload.chunk_hash = incoming.chunk_hash
              AND payload.size = incoming.chunk_size",
            params![recipe_hash],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to insert recipe payload leases: {e}"))
        })?;

        let occurrence_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM recipe_occurrences WHERE recipe_hash = ?1",
                params![recipe_hash],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to count stored recipe occurrences: {e}"))
            })?;
        let expected_occurrence_count = i64::try_from(recipe.sequence().spans.len())
            .map_err(|_| StagingError::StagingCorrupt("too many recipe occurrences".to_owned()))?;
        if occurrence_count != expected_occurrence_count {
            return Err(StagingError::StagingCorrupt(format!(
                "stored recipe has {occurrence_count} occurrences, expected {expected_occurrence_count}"
            )));
        }

        tx.execute(
            "INSERT OR IGNORE INTO prepared_leases (recipe_hash, xorb_hash)
             SELECT DISTINCT ?1, prepared.xorb_hash
             FROM temp.incoming_recipe_occurrences AS incoming
             JOIN prepared_xorb_chunks AS prepared
               ON prepared.chunk_hash = incoming.chunk_hash
              AND prepared.uncompressed_size = incoming.chunk_size
             JOIN prepared_xorbs AS xorb
               ON xorb.file_hash = prepared.file_hash
              AND xorb.xorb_hash = prepared.xorb_hash",
            params![recipe_hash],
        )
        .map_err(|e| StagingError::Internal(format!("failed to insert prepared leases: {e}")))?;

        let covered_payload_count: i64 = tx
            .query_row(
                "SELECT COUNT(DISTINCT incoming.chunk_hash)
                 FROM temp.incoming_recipe_occurrences AS incoming
                 WHERE EXISTS (
                     SELECT 1 FROM recipe_payload_leases AS lease
                     WHERE lease.recipe_hash = ?1
                       AND lease.chunk_hash = incoming.chunk_hash
                 ) OR EXISTS (
                     SELECT 1
                     FROM prepared_xorb_chunks AS prepared
                     JOIN prepared_leases AS lease
                       ON lease.xorb_hash = prepared.xorb_hash
                      AND lease.recipe_hash = ?1
                     WHERE prepared.chunk_hash = incoming.chunk_hash
                       AND prepared.uncompressed_size = incoming.chunk_size
                 )",
                params![recipe_hash],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to count covered recipe payloads: {e}"))
            })?;
        let expected_lease_count: i64 = tx
            .query_row(
                "SELECT COUNT(DISTINCT chunk_hash)
                 FROM temp.incoming_recipe_occurrences",
                [],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to count incoming recipe payloads: {e}"))
            })?;
        if covered_payload_count != expected_lease_count {
            return Err(StagingError::StagingCorrupt(format!(
                "stored recipe has {covered_payload_count} covered payloads, expected {expected_lease_count}"
            )));
        }

        let batch_state: Option<String> = tx
            .query_row(
                "SELECT state FROM staging_batches WHERE batch_id = ?1",
                params![batch_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to validate staging batch: {e}"))
            })?;
        if batch_state.as_deref() != Some("open") {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe lease requires open staging batch {batch_id}"
            )));
        }

        match verification {
            RecipeVerification::CallerVerified => {
                tx.execute(
                    "INSERT OR IGNORE INTO verified_recipes (recipe_hash) VALUES (?1)",
                    params![recipe_hash],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to verify file recipe: {e}"))
                })?;
            }
            RecipeVerification::Pending => {
                tx.execute(
                    "INSERT INTO staging_meta (key, value)
                     VALUES ('migration_validation_pending', '1')
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to schedule recipe validation: {e}"))
                })?;
            }
        }

        tx.execute(
            "INSERT INTO path_leases
             (batch_id, path_bytes, file_hash, recipe_hash)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(batch_id, path_bytes) DO UPDATE SET
                file_hash = excluded.file_hash,
                recipe_hash = excluded.recipe_hash",
            params![batch_id, path_bytes, file_hash, recipe_hash],
        )
        .map_err(|e| StagingError::Internal(format!("failed to insert path lease: {e}")))?;

        tx.execute("DELETE FROM temp.incoming_recipe_occurrences", [])
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to release incoming recipe occurrences: {e}"
                ))
            })?;

        tx.commit()
            .map_err(|e| StagingError::Internal(format!("failed to commit recipe lease: {e}")))?;
        Ok(())
    }

    pub fn has_file_lease(&self, file_hash: &[u8; 32]) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM path_leases WHERE file_hash = ?1)",
                params![file_hash.as_slice()],
                |row| row.get(0),
            )
            .map_err(|e| StagingError::Internal(format!("failed to check file leases: {e}")))
    }

    pub fn rollback_batch(&self, batch_id: &str) -> Result<Vec<[u8; 32]>> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| StagingError::Internal(format!("failed to begin batch rollback: {e}")))?;
        let file_hashes = {
            let mut statement = tx
                .prepare_cached("SELECT DISTINCT file_hash FROM path_leases WHERE batch_id = ?1")
                .map_err(|e| {
                    StagingError::Internal(format!("failed to prepare batch leases: {e}"))
                })?;
            statement
                .query_map(params![batch_id], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| StagingError::Internal(format!("failed to query batch leases: {e}")))?
                .map(|row| {
                    row.map_err(|e| {
                        StagingError::Internal(format!("failed to collect batch lease: {e}"))
                    })
                    .and_then(|hash| decode_hash_blob("batch lease file hash", hash))
                })
                .collect::<Result<Vec<_>>>()?
        };
        tx.execute(
            "DELETE FROM path_leases WHERE batch_id = ?1",
            params![batch_id],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete batch leases: {e}")))?;
        tx.execute(
            "DELETE FROM staging_batches WHERE batch_id = ?1",
            params![batch_id],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete staging batch: {e}")))?;

        let mut unleased = Vec::new();
        for file_hash in file_hashes {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM path_leases WHERE file_hash = ?1)",
                    params![file_hash.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to check remaining leases: {e}"))
                })?;
            if !exists {
                unleased.push(file_hash);
            }
        }
        tx.commit()
            .map_err(|e| StagingError::Internal(format!("failed to commit batch rollback: {e}")))?;
        Ok(unleased)
    }

    /// Move staged rows from a provisional file hash to the final file hash.
    ///
    /// An existing target is reused only when its exact ordered hash/size
    /// sequence matches the source. Divergent rows for the same whole-file
    /// hash are corruption; adoption never destroys another batch's data.
    ///
    /// Returns the number of rows adopted from the source hash.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "file sizes and row counts fit in i64 for SQLite storage"
    )]
    pub fn adopt_file_hash(
        &self,
        source_file_hash: &[u8; 32],
        target_file_hash: &[u8; 32],
        total_bytes: u64,
    ) -> Result<u64> {
        let source: &[u8] = source_file_hash;
        let target: &[u8] = target_file_hash;
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin adopt_file_hash tx: {e}"))
        })?;

        if source_file_hash == target_file_hash {
            tx.commit().map_err(|e| {
                StagingError::Internal(format!("failed to commit adopt_file_hash tx: {e}"))
            })?;
            return Ok(0);
        }

        let target_file_size: Option<i64> = tx
            .query_row(
                "SELECT total_bytes FROM files WHERE file_hash = ?1",
                params![target],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to inspect adopted target file: {e}"))
            })?;

        let read_sequence = |file_hash: &[u8]| -> Result<Vec<([u8; 32], u64)>> {
            let mut stmt = tx
                .prepare_cached(
                    "SELECT chunk_hash, size FROM (
                         SELECT chunk_hash, size, chunk_index
                         FROM chunks WHERE file_hash = ?1
                         UNION ALL
                         SELECT chunk_hash, size, chunk_index
                         FROM pending_chunks WHERE file_hash = ?1
                     ) ORDER BY chunk_index",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to prepare adopted sequence: {e}"))
                })?;
            stmt.query_map(params![file_hash], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| StagingError::Internal(format!("failed to query adopted sequence: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!("failed to collect adopted sequence: {e}"))
            })?
            .into_iter()
            .map(|(hash, size)| {
                Ok((
                    decode_hash_blob("adopted chunk hash", hash)?,
                    nonnegative_count("adopted chunk size", size)?,
                ))
            })
            .collect()
        };

        let source_sequence = read_sequence(source)?;
        if let Some(existing_size) = target_file_size {
            let existing_size = nonnegative_count("adopted target file size", existing_size)?;
            let target_sequence = read_sequence(target)?;
            if existing_size != total_bytes || target_sequence != source_sequence {
                return Err(StagingError::StagingCorrupt(
                    "one whole-file hash has divergent staged recipes".to_owned(),
                ));
            }

            let source_segment_counts: Vec<(u64, u64)> = {
                let mut stmt = tx
                    .prepare_cached(
                        "SELECT segment_id, COUNT(*) FROM chunks
                         WHERE file_hash = ?1 GROUP BY segment_id",
                    )
                    .map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to prepare reused source segment counts: {e}"
                        ))
                    })?;
                stmt.query_map(params![source], |row| {
                    let segment_id: u64 = row.get(0)?;
                    let count: i64 = row.get(1)?;
                    #[expect(clippy::cast_sign_loss, reason = "COUNT(*) is non-negative")]
                    Ok((segment_id, count as u64))
                })
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to query reused source segment counts: {e}"
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to collect reused source segment counts: {e}"
                    ))
                })?
            };

            let removed_pending = tx
                .execute(
                    "DELETE FROM pending_chunks WHERE file_hash = ?1",
                    params![source],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to remove reused pending rows: {e}"))
                })?;
            let removed_chunks = tx
                .execute("DELETE FROM chunks WHERE file_hash = ?1", params![source])
                .map_err(|e| {
                    StagingError::Internal(format!("failed to remove reused chunk rows: {e}"))
                })?;
            for (segment_id, count) in source_segment_counts {
                tx.execute(
                    "UPDATE segments
                     SET live_chunk_count = MAX(0, live_chunk_count - ?1)
                     WHERE segment_id = ?2",
                    params![count as i64, segment_id],
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to decrement reused segment {segment_id}: {e}"
                    ))
                })?;
            }

            tx.execute(
                "DELETE FROM file_push_plans WHERE file_hash = ?1",
                params![source],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to remove reused source plan: {e}"))
            })?;
            tx.execute(
                "DELETE FROM prepared_xorbs WHERE file_hash = ?1",
                params![source],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to remove reused prepared xorb: {e}"))
            })?;
            tx.execute("DELETE FROM files WHERE file_hash = ?1", params![source])
                .map_err(|e| {
                    StagingError::Internal(format!("failed to remove reused source file: {e}"))
                })?;
            tx.execute(
                "DELETE FROM file_paths WHERE file_hash = ?1",
                params![source],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to remove reused source path: {e}"))
            })?;
            tx.commit().map_err(|e| {
                StagingError::Internal(format!("failed to commit reused adoption: {e}"))
            })?;
            return u64::try_from(removed_pending.saturating_add(removed_chunks)).map_err(|_| {
                StagingError::StagingCorrupt("reused row count cannot be represented".to_owned())
            });
        }

        tx.execute(
            "INSERT INTO files (file_hash, total_bytes) VALUES (?1, ?2)",
            params![target, total_bytes as i64],
        )
        .map_err(|e| StagingError::Internal(format!("failed to insert adopted file: {e}")))?;

        let adopted_pending = tx
            .execute(
                "UPDATE pending_chunks SET file_hash = ?1 WHERE file_hash = ?2",
                params![target, source],
            )
            .map_err(|e| StagingError::Internal(format!("failed to adopt pending rows: {e}")))?;

        let adopted_chunks = tx
            .execute(
                "UPDATE chunks SET file_hash = ?1 WHERE file_hash = ?2",
                params![target, source],
            )
            .map_err(|e| StagingError::Internal(format!("failed to adopt chunk rows: {e}")))?;

        tx.execute(
            "DELETE FROM file_push_plans WHERE file_hash IN (?1, ?2)",
            params![source, target],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to delete adopted file push plan: {e}"))
        })?;

        tx.execute(
            "DELETE FROM prepared_xorbs WHERE file_hash IN (?1, ?2)",
            params![source, target],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to delete adopted prepared xorbs: {e}"))
        })?;

        tx.execute("DELETE FROM files WHERE file_hash = ?1", params![source])
            .map_err(|e| {
                StagingError::Internal(format!("failed to delete source file row: {e}"))
            })?;
        tx.execute(
            "DELETE FROM file_paths WHERE file_hash = ?1",
            params![source],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete source file_path: {e}")))?;

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit adopt_file_hash tx: {e}"))
        })?;

        let adopted_rows = adopted_pending
            .checked_add(adopted_chunks)
            .ok_or_else(|| StagingError::StagingCorrupt("adopted row count overflow".to_owned()))?;
        u64::try_from(adopted_rows).map_err(|_| {
            StagingError::StagingCorrupt("adopted row count cannot be represented".to_owned())
        })
    }

    /// Remove a file and all its chunk/pending rows from the index.
    ///
    /// Returns the segment IDs that had chunks removed (caller should
    /// decrement `live_chunk_count` on those segments). Returns an empty
    /// vec if the file was not found.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn remove_file(&self, file_hash: &[u8; 32]) -> Result<Vec<u64>> {
        let fh: &[u8] = file_hash;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| StagingError::Internal(format!("failed to begin remove_file tx: {e}")))?;

        // Collect affected segment IDs from committed chunks.
        let affected: Vec<u64> = {
            let mut stmt = tx
                .prepare_cached("SELECT DISTINCT segment_id FROM chunks WHERE file_hash = ?1")
                .map_err(|e| {
                    StagingError::Internal(format!("prepare affected segments query: {e}"))
                })?;
            stmt.query_map(params![fh], |row| row.get(0))
                .map_err(|e| StagingError::Internal(format!("query affected segments: {e}")))?
                .collect::<std::result::Result<Vec<u64>, _>>()
                .map_err(|e| StagingError::Internal(format!("collect affected segments: {e}")))?
        };

        // Count committed chunks per segment for live_chunk_count adjustment.
        let segment_counts: Vec<(u64, u64)>;
        {
            let mut stmt = tx
                .prepare_cached(
                    "SELECT segment_id, COUNT(*) FROM chunks WHERE file_hash = ?1 GROUP BY segment_id",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("prepare chunk count query: {e}"))
                })?;
            segment_counts = stmt
                .query_map(params![fh], |row| {
                    Ok((row.get(0)?, row.get::<_, i64>(1)? as u64))
                })
                .map_err(|e| StagingError::Internal(format!("query chunk counts: {e}")))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| StagingError::Internal(format!("collect chunk counts: {e}")))?;
        }

        // Delete pending chunks for this file.
        tx.execute(
            "DELETE FROM pending_chunks WHERE file_hash = ?1",
            params![fh],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to delete pending chunks for file: {e}"))
        })?;

        // Delete committed chunks for this file.
        tx.execute("DELETE FROM chunks WHERE file_hash = ?1", params![fh])
            .map_err(|e| {
                StagingError::Internal(format!("failed to delete chunks for file: {e}"))
            })?;

        // Decrement live_chunk_count on affected segments.
        {
            let mut stmt = tx
                .prepare_cached(
                    "UPDATE segments SET live_chunk_count = MAX(0, live_chunk_count - ?1) WHERE segment_id = ?2",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("prepare live_chunk_count update: {e}"))
                })?;
            #[expect(clippy::cast_possible_wrap, reason = "count fits in i64")]
            for (seg_id, count) in &segment_counts {
                stmt.execute(params![*count as i64, seg_id]).map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to decrement live_chunk_count for segment {seg_id}: {e}"
                    ))
                })?;
            }
        }

        tx.execute(
            "DELETE FROM file_push_plans WHERE file_hash = ?1",
            params![fh],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete file push plan: {e}")))?;

        tx.execute(
            "DELETE FROM prepared_xorbs WHERE file_hash = ?1",
            params![fh],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete prepared xorbs: {e}")))?;

        tx.execute(
            "DELETE FROM file_recipes
             WHERE file_hash = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM path_leases WHERE path_leases.file_hash = ?1
               )
               AND NOT EXISTS (
                   SELECT 1
                   FROM push_snapshot_recipes
                   WHERE push_snapshot_recipes.recipe_hash = file_recipes.recipe_hash
               )",
            params![fh],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete unleased recipes: {e}")))?;

        tx.execute(
            "DELETE FROM chunk_payloads
             WHERE NOT EXISTS (
                       SELECT 1 FROM recipe_payload_leases
                       WHERE recipe_payload_leases.chunk_hash = chunk_payloads.chunk_hash
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM chunks
                       WHERE chunks.chunk_hash = chunk_payloads.chunk_hash
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM pending_chunks
                       WHERE pending_chunks.chunk_hash = chunk_payloads.chunk_hash
                   )",
            [],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to delete unleased chunk payloads: {e}"))
        })?;

        tx.execute(
            "DELETE FROM prepared_payloads
             WHERE NOT EXISTS (
                       SELECT 1 FROM prepared_leases
                       WHERE prepared_leases.xorb_hash = prepared_payloads.xorb_hash
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM prepared_xorbs
                       WHERE prepared_xorbs.xorb_hash = prepared_payloads.xorb_hash
                   )",
            [],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to delete unleased prepared payloads: {e}"))
        })?;

        // Delete the file row.
        tx.execute("DELETE FROM files WHERE file_hash = ?1", params![fh])
            .map_err(|e| StagingError::Internal(format!("failed to delete file: {e}")))?;

        // Clean up the file_paths side table.
        tx.execute("DELETE FROM file_paths WHERE file_hash = ?1", params![fh])
            .map_err(|e| StagingError::Internal(format!("failed to delete file_path: {e}")))?;

        tx.commit()
            .map_err(|e| StagingError::Internal(format!("failed to commit remove_file tx: {e}")))?;

        Ok(affected)
    }

    /// Delete every `chunks` and `pending_chunks` row for `file_hash`.
    ///
    /// Executes as a single SQLite transaction so a partial failure
    /// cannot leave `live_chunk_count` out of sync with the surviving
    /// `chunks` rows. Unlike [`Self::remove_file`], this helper leaves
    /// the `files` row untouched. Pending rows are removed so a failed
    /// or retried add cannot collide on `(file_hash, chunk_index)`, but
    /// only committed `chunks` rows decrement `live_chunk_count`.
    ///
    /// Returns `(rows_deleted, segments_touched)`. A segment whose
    /// `live_chunk_count` hits zero is eligible for sweep via the
    /// existing `sealed_at IS NOT NULL AND live_chunk_count = 0`
    /// predicate — the segment-file reclamation stays in
    /// [`Index::sweep_candidates`] / `StagingArea::sweep_orphans`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn delete_chunks_for_file(&self, file_hash: &[u8; 32]) -> Result<(u64, Vec<u64>)> {
        let fh: &[u8] = file_hash;
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin delete_chunks_for_file tx: {e}"))
        })?;

        // Capture touched segments from both tables for tracing and
        // cleanup decisions, but decrement live counts only for rows
        // already promoted into `chunks`. Pending rows never increment
        // `live_chunk_count`; subtracting them can mark a segment empty
        // while committed rows for another file still reference it.
        let touched_segments: Vec<u64> = {
            let mut stmt = tx
                .prepare_cached(
                    "SELECT DISTINCT segment_id FROM (
                         SELECT segment_id FROM chunks WHERE file_hash = ?1
                         UNION
                         SELECT segment_id FROM pending_chunks WHERE file_hash = ?1
                     )",
                )
                .map_err(|e| StagingError::Internal(format!("prepare segment count query: {e}")))?;
            stmt.query_map(params![fh], |row| row.get(0))
                .map_err(|e| StagingError::Internal(format!("query segment counts: {e}")))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| StagingError::Internal(format!("collect segment counts: {e}")))?
        };

        let committed_segment_counts: Vec<(u64, u64)> = {
            let mut stmt = tx
                .prepare_cached(
                    "SELECT segment_id, COUNT(*)
                     FROM chunks
                     WHERE file_hash = ?1
                     GROUP BY segment_id",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("prepare committed segment count query: {e}"))
                })?;
            stmt.query_map(params![fh], |row| {
                let seg_id: u64 = row.get(0)?;
                let count: i64 = row.get(1)?;
                #[expect(clippy::cast_sign_loss, reason = "COUNT(*) is always non-negative")]
                Ok((seg_id, count as u64))
            })
            .map_err(|e| StagingError::Internal(format!("query committed segment counts: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| StagingError::Internal(format!("collect committed segment counts: {e}")))?
        };

        let chunks_deleted = tx
            .execute("DELETE FROM chunks WHERE file_hash = ?1", params![fh])
            .map_err(|e| StagingError::Internal(format!("failed to delete chunks for file: {e}")))?
            as u64;

        // Also delete from `pending_chunks`. Without this, re-add of a
        // file whose chunks never got promoted to `chunks` leaves the
        // pending rows behind, and subsequent staging collides on the
        // same `(file_hash, chunk_index)` pair after appending bytes.
        let pending_deleted = tx
            .execute(
                "DELETE FROM pending_chunks WHERE file_hash = ?1",
                params![fh],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to delete pending chunks for file: {e}"))
            })? as u64;

        let rows_deleted = chunks_deleted + pending_deleted;

        tx.execute(
            "DELETE FROM file_push_plans WHERE file_hash = ?1",
            params![fh],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete file push plan: {e}")))?;

        tx.execute(
            "DELETE FROM prepared_xorbs WHERE file_hash = ?1",
            params![fh],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete prepared xorbs: {e}")))?;

        // Decrement live_chunk_count for committed rows only. `MAX(0, ...)`
        // guards against pre-existing drift; the count we subtract is the
        // exact number of promoted rows we just removed for this segment.
        if !committed_segment_counts.is_empty() {
            let mut stmt = tx
                .prepare_cached(
                    "UPDATE segments
                     SET live_chunk_count = MAX(0, live_chunk_count - ?1)
                     WHERE segment_id = ?2",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("prepare live_chunk_count decrement: {e}"))
                })?;
            #[expect(clippy::cast_possible_wrap, reason = "count fits in i64")]
            for (seg_id, count) in &committed_segment_counts {
                stmt.execute(params![*count as i64, seg_id]).map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to decrement live_chunk_count for segment {seg_id}: {e}"
                    ))
                })?;
            }
        }

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit delete_chunks_for_file tx: {e}"))
        })?;

        Ok((rows_deleted, touched_segments))
    }

    /// List all files in the staging index with their chunk counts and sizes.
    ///
    /// Returns `(file_hash, total_bytes, committed_chunks, pending_chunks)`
    /// for every row in the `files` table. The chunk counts come from
    /// joining against `chunks` and `pending_chunks` respectively.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn list_files_with_chunks(&self) -> Result<Vec<StagedFileInfo>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT
                     f.file_hash,
                     f.total_bytes,
                     COALESCE(c.cnt, 0),
                     COALESCE(p.cnt, 0),
                     COALESCE(c.seg_count, 0) + COALESCE(p.seg_count, 0),
                     fp.file_path
                 FROM files f
                 LEFT JOIN (
                     SELECT file_hash, COUNT(*) AS cnt, COUNT(DISTINCT segment_id) AS seg_count
                     FROM chunks GROUP BY file_hash
                 ) c ON f.file_hash = c.file_hash
                 LEFT JOIN (
                     SELECT file_hash, COUNT(*) AS cnt, COUNT(DISTINCT segment_id) AS seg_count
                     FROM pending_chunks GROUP BY file_hash
                 ) p ON f.file_hash = p.file_hash
                 LEFT JOIN file_paths fp ON f.file_hash = fp.file_hash
                 ORDER BY f.total_bytes DESC",
            )
            .map_err(|e| StagingError::Internal(format!("prepare list_files query: {e}")))?;

        let rows = stmt
            .query_map([], |row| {
                let hash_blob: Vec<u8> = row.get(0)?;
                let total_bytes: i64 = row.get(1)?;
                let committed: i64 = row.get(2)?;
                let pending: i64 = row.get(3)?;
                let segments: i64 = row.get(4)?;
                let file_path: Option<String> = row.get(5)?;
                Ok((
                    hash_blob,
                    total_bytes,
                    committed,
                    pending,
                    segments,
                    file_path,
                ))
            })
            .map_err(|e| StagingError::Internal(format!("query list_files: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| StagingError::Internal(format!("collect list_files: {e}")))?;

        rows.into_iter()
            .map(
                |(hash_blob, total_bytes, committed, pending, segments, file_path)| {
                    Ok(StagedFileInfo {
                        file_hash: decode_hash_blob("file hash", hash_blob)?,
                        total_bytes: nonnegative_count("file total_bytes", total_bytes)?,
                        committed_chunks: nonnegative_count("committed chunk count", committed)?,
                        pending_chunks: nonnegative_count("pending chunk count", pending)?,
                        segments: nonnegative_count("segment count", segments)?,
                        file_path,
                    })
                },
            )
            .collect()
    }

    /// Check whether a file hash exists in the `files` table.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn file_exists(&self, file_hash: &[u8; 32]) -> Result<bool> {
        let fh: &[u8] = file_hash;
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE file_hash = ?1)",
                params![fh],
                |row| row.get(0),
            )
            .map_err(|e| StagingError::Internal(format!("failed to check file existence: {e}")))?;
        Ok(exists)
    }

    /// Store a verified add-time push plan for one staged file.
    ///
    /// The JSON body preserves the existing plan contract while the indexed
    /// metadata lets push reject stale rows before trusting the plan body.
    pub fn insert_file_push_plan(
        &self,
        file_hash: &[u8; 32],
        version: u32,
        file_size: u64,
        chunk_count: u64,
        chunk_sequence_hash: &[u8; 32],
        plan_json: &[u8],
        prepared_xorbs: &[PreparedXorbWrite],
    ) -> Result<()> {
        let fh: &[u8] = file_hash;
        let sequence_hash: &[u8] = chunk_sequence_hash;
        let file_size = sqlite_i64("file push plan file_size", file_size)?;
        let chunk_count = sqlite_i64("file push plan chunk_count", chunk_count)?;
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin file push plan tx: {e}"))
        })?;
        let recipe_hash: Option<Vec<u8>> = tx
            .query_row(
                "SELECT recipe_hash FROM file_recipes
                 WHERE file_hash = ?1 AND policy_id = 'xet-gear-v1-64k'
                 ORDER BY created_at DESC LIMIT 1",
                params![fh],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to resolve prepared recipe: {e}"))
            })?;

        tx.execute(
            "INSERT INTO file_push_plans
             (file_hash, version, file_size, chunk_count, chunk_sequence_hash, plan_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
             ON CONFLICT(file_hash) DO UPDATE SET
                version = excluded.version,
                file_size = excluded.file_size,
                chunk_count = excluded.chunk_count,
                chunk_sequence_hash = excluded.chunk_sequence_hash,
                plan_json = excluded.plan_json,
                updated_at = excluded.updated_at",
            params![
                fh,
                i64::from(version),
                file_size,
                chunk_count,
                sequence_hash,
                plan_json,
            ],
        )
        .map_err(|e| StagingError::Internal(format!("failed to store file push plan: {e}")))?;

        tx.execute(
            "DELETE FROM prepared_xorbs WHERE file_hash = ?1",
            params![fh],
        )
        .map_err(|e| StagingError::Internal(format!("failed to replace prepared xorbs: {e}")))?;
        if let Some(recipe_hash) = recipe_hash.as_deref() {
            tx.execute(
                "DELETE FROM prepared_leases WHERE recipe_hash = ?1",
                params![recipe_hash],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to replace prepared leases: {e}"))
            })?;
        }

        for prepared in prepared_xorbs {
            let xorb_hash: &[u8] = &prepared.xorb_hash;
            let payload_hash: &[u8] = &prepared.payload_hash;
            let bytes = sqlite_i64("prepared xorb bytes", prepared.bytes)?;
            tx.execute(
                "INSERT INTO prepared_xorbs
                 (file_hash, xorb_hash, payload_hash, bytes, planned_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
                params![
                    fh,
                    xorb_hash,
                    payload_hash,
                    bytes,
                    prepared.planned_json.as_slice(),
                ],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to store prepared xorb candidate: {e}"))
            })?;
            tx.execute(
                "INSERT OR IGNORE INTO prepared_payloads
                 (xorb_hash, payload_hash, bytes) VALUES (?1, ?2, ?3)",
                params![xorb_hash, payload_hash, bytes],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to store prepared payload: {e}"))
            })?;
            let payload_matches: bool = tx
                .query_row(
                    "SELECT payload_hash = ?2 AND bytes = ?3
                     FROM prepared_payloads WHERE xorb_hash = ?1",
                    params![xorb_hash, payload_hash, bytes],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to verify prepared payload: {e}"))
                })?;
            if !payload_matches {
                return Err(StagingError::StagingCorrupt(format!(
                    "prepared xorb payload identity collision for {}",
                    crab_xet::hash::MerkleHash::from(prepared.xorb_hash).hex()
                )));
            }
            if let Some(recipe_hash) = recipe_hash.as_deref() {
                tx.execute(
                    "INSERT OR IGNORE INTO prepared_leases (recipe_hash, xorb_hash)
                     VALUES (?1, ?2)",
                    params![recipe_hash, xorb_hash],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to store prepared lease: {e}"))
                })?;
            }

            for placement in &prepared.placements {
                let chunk_hash: &[u8] = &placement.chunk_hash;
                tx.execute(
                    "INSERT INTO prepared_xorb_chunks
                     (file_hash, xorb_hash, chunk_hash, chunk_index, uncompressed_size)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        fh,
                        xorb_hash,
                        chunk_hash,
                        i64::from(placement.chunk_index),
                        i64::from(placement.uncompressed_size),
                    ],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to store prepared xorb chunk: {e}"))
                })?;
            }
        }

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit file push plan tx: {e}"))
        })?;
        Ok(())
    }

    /// Load the authoritative add-time push plan record for a staged file.
    pub fn file_push_plan(&self, file_hash: &[u8; 32]) -> Result<Option<StoredFilePushPlan>> {
        if !table_exists(&self.conn, "file_push_plans")? {
            return Ok(None);
        }

        let fh: &[u8] = file_hash;
        let row: Option<StoredFilePushPlanRow> = self
            .conn
            .query_row(
                "SELECT version, file_size, chunk_count, chunk_sequence_hash, plan_json
                 FROM file_push_plans
                 WHERE file_hash = ?1",
                params![fh],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| StagingError::Internal(format!("failed to load file push plan: {e}")))?;

        let Some((version, file_size, chunk_count, chunk_sequence_hash, plan_json)) = row else {
            return Ok(None);
        };
        let version = u32::try_from(version).map_err(|_| {
            StagingError::StagingCorrupt(format!("file push plan version is invalid: {version}"))
        })?;
        let file_size = nonnegative_count("file push plan file_size", file_size)?;
        let chunk_count = nonnegative_count("file push plan chunk_count", chunk_count)?;
        let chunk_sequence_hash =
            decode_hash_blob("file push plan chunk sequence hash", chunk_sequence_hash)?;

        Ok(Some(StoredFilePushPlan {
            version,
            file_size,
            chunk_count,
            chunk_sequence_hash,
            plan_json,
        }))
    }

    /// Load prepared xorb candidates that cover any of the requested chunks.
    pub fn prepared_xorbs_for_chunks(
        &self,
        chunk_hashes: &[[u8; 32]],
    ) -> Result<Vec<StoredPreparedXorb>> {
        if chunk_hashes.is_empty() || !table_exists(&self.conn, "prepared_xorbs")? {
            return Ok(Vec::new());
        }

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for batch in chunk_hashes.chunks(PREPARED_XORB_QUERY_CHUNK_BATCH) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let sql = format!(
                "SELECT DISTINCT px.file_hash, px.xorb_hash, px.payload_hash, px.bytes, px.planned_json
                 FROM prepared_xorb_chunks pc
                 INNER JOIN prepared_xorbs px
                   ON px.file_hash = pc.file_hash AND px.xorb_hash = pc.xorb_hash
                 WHERE pc.chunk_hash IN ({placeholders})
                 ORDER BY px.file_hash, px.xorb_hash"
            );
            let mut stmt = self.conn.prepare(&sql).map_err(|e| {
                StagingError::Internal(format!("prepare prepared xorb lookup: {e}"))
            })?;
            let rows = stmt
                .query_map(
                    params_from_iter(batch.iter().map(|hash| hash.as_slice())),
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Vec<u8>>(4)?,
                        ))
                    },
                )
                .map_err(|e| StagingError::Internal(format!("query prepared xorb lookup: {e}")))?;

            for row in rows {
                let (file_hash, xorb_hash, payload_hash, bytes, planned_json): StoredPreparedXorbRow =
                    row.map_err(|e| {
                        StagingError::Internal(format!("read prepared xorb row: {e}"))
                    })?;
                let file_hash = decode_hash_blob("prepared xorb file hash", file_hash)?;
                let xorb_hash = decode_hash_blob("prepared xorb hash", xorb_hash)?;
                if !seen.insert((file_hash, xorb_hash)) {
                    continue;
                }
                out.push(StoredPreparedXorb {
                    file_hash,
                    xorb_hash,
                    payload_hash: decode_hash_blob("prepared xorb payload hash", payload_hash)?,
                    bytes: nonnegative_count("prepared xorb bytes", bytes)?,
                    planned_json,
                });
            }
        }

        Ok(out)
    }

    /// List raw prepared xorb rows for staging diagnostics.
    pub fn raw_prepared_xorb_rows(&self) -> Result<Vec<RawPreparedXorbRow>> {
        if !table_exists(&self.conn, "prepared_xorbs")? {
            return Ok(Vec::new());
        }

        let mut stmt = self
            .conn
            .prepare(
                "SELECT file_hash, xorb_hash, payload_hash, bytes, planned_json
                 FROM prepared_xorbs
                 ORDER BY file_hash, xorb_hash",
            )
            .map_err(|e| StagingError::Internal(format!("prepare prepared xorb rows: {e}")))?;
        stmt.query_map([], |row| {
            Ok(RawPreparedXorbRow {
                file_hash: row.get(0)?,
                xorb_hash: row.get(1)?,
                payload_hash: row.get(2)?,
                bytes: row.get(3)?,
                planned_json: row.get(4)?,
            })
        })
        .map_err(|e| StagingError::Internal(format!("query prepared xorb rows: {e}")))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| StagingError::Internal(format!("collect prepared xorb rows: {e}")))
    }

    /// Insert chunk rows for a file, linking them to their segment locators.
    ///
    /// For each `(chunk_hash, size)` pair, looks up the locator from
    /// `chunks` (preferred) or `pending_chunks` and inserts into `chunks`.
    /// Removes old chunk rows for this file first and keeps
    /// `live_chunk_count` in sync with the replacement.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure or if a chunk
    /// locator cannot be found.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "sizes and indices fit in i64 for SQLite"
    )]
    pub fn insert_chunks_for_file(
        &self,
        file_hash: &[u8; 32],
        chunks: &[([u8; 32], u64)],
    ) -> Result<Vec<u64>> {
        let fh: &[u8] = file_hash;
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin insert_chunks_for_file tx: {e}"))
        })?;

        let old_segment_counts = {
            let mut stmt = tx
                .prepare_cached(
                    "SELECT segment_id, COUNT(*)
                     FROM chunks
                     WHERE file_hash = ?1
                     GROUP BY segment_id",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("prepare old segment count query: {e}"))
                })?;
            let rows = stmt
                .query_map(params![fh], |row| {
                    let seg_id: u64 = row.get(0)?;
                    let count: i64 = row.get(1)?;
                    #[expect(clippy::cast_sign_loss, reason = "COUNT(*) is always non-negative")]
                    Ok((seg_id, count as u64))
                })
                .map_err(|e| StagingError::Internal(format!("query old segment counts: {e}")))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| StagingError::Internal(format!("collect old segment counts: {e}")))?
        };

        // Remove old chunk rows for this file before re-inserting. Their
        // live counts are decremented in the same transaction so a failed
        // replacement cannot leave sealed segments permanently unsweepable.
        tx.execute("DELETE FROM chunks WHERE file_hash = ?1", params![fh])
            .map_err(|e| {
                StagingError::Internal(format!("failed to delete old chunks for file: {e}"))
            })?;

        if !old_segment_counts.is_empty() {
            let mut decrement_stmt = tx
                .prepare_cached(
                    "UPDATE segments
                     SET live_chunk_count = MAX(0, live_chunk_count - ?1)
                     WHERE segment_id = ?2",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("prepare live_chunk_count decrement: {e}"))
                })?;
            for (seg_id, count) in &old_segment_counts {
                decrement_stmt
                    .execute(params![*count as i64, seg_id])
                    .map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to decrement live_chunk_count for segment {seg_id}: {e}"
                        ))
                    })?;
            }
        }

        let mut affected_segments = Vec::new();
        let mut new_segment_counts: HashMap<u64, u64> = HashMap::new();
        {
            let mut locate_stmt = tx
                .prepare_cached(
                    "SELECT segment_id, segment_offset, size FROM chunks WHERE chunk_hash = ?1 LIMIT 1",
                )
                .map_err(|e| StagingError::Internal(format!("prepare locate: {e}")))?;

            let mut locate_pending_stmt = tx
                .prepare_cached(
                    "SELECT segment_id, segment_offset, size FROM pending_chunks WHERE chunk_hash = ?1 LIMIT 1",
                )
                .map_err(|e| StagingError::Internal(format!("prepare locate pending: {e}")))?;

            let mut insert_stmt = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO chunks (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(|e| StagingError::Internal(format!("prepare chunk insert: {e}")))?;

            for (idx, (chunk_hash, size)) in chunks.iter().enumerate() {
                let ch: &[u8] = chunk_hash;
                // Try committed chunks first, then pending.
                let locator: Option<(u64, u64, i64)> = locate_stmt
                    .query_row(params![ch], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                    .optional()
                    .map_err(|e| StagingError::Internal(format!("locate chunk: {e}")))?;

                let (seg_id, seg_offset, _stored_size) = if let Some(loc) = locator {
                    loc
                } else {
                    locate_pending_stmt
                        .query_row(params![ch], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                        })
                        .optional()
                        .map_err(|e| StagingError::Internal(format!("locate pending chunk: {e}")))?
                        .ok_or_else(|| {
                            StagingError::Internal(
                                "chunk not found in chunks or pending_chunks".into(),
                            )
                        })?
                };

                insert_stmt
                    .execute(params![
                        ch,
                        fh,
                        idx as i64,
                        *size as i64,
                        seg_id,
                        seg_offset,
                    ])
                    .map_err(|e| StagingError::Internal(format!("insert chunk: {e}")))?;

                affected_segments.push(seg_id);
                *new_segment_counts.entry(seg_id).or_insert(0) += 1;
            }
        }

        // Now that this file's rows live in `chunks`, drop the
        // corresponding `pending_chunks` rows. Promotion is idempotent
        // — once we've insert-or-replaced into `chunks`, the pending
        // row for the same `(file_hash, chunk_index)` pair is strictly
        // redundant. Keeping both violated the
        // "exactly one of chunks / pending_chunks" invariant that
        // `delete_chunks_for_file`'s per-segment live count relies on.
        tx.execute(
            "DELETE FROM pending_chunks WHERE file_hash = ?1",
            params![fh],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to clear pending rows after promote: {e}"))
        })?;

        if !new_segment_counts.is_empty() {
            let mut increment_stmt = tx
                .prepare_cached(
                    "UPDATE segments
                     SET live_chunk_count = live_chunk_count + ?1
                     WHERE segment_id = ?2",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("prepare live_chunk_count increment: {e}"))
                })?;
            for (seg_id, count) in &new_segment_counts {
                increment_stmt
                    .execute(params![*count as i64, seg_id])
                    .map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to increment live_chunk_count for segment {seg_id}: {e}"
                        ))
                    })?;
            }
        }

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit insert_chunks_for_file tx: {e}"))
        })?;

        Ok(affected_segments)
    }

    /// Increment `live_chunk_count` on a segment by `delta`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    #[expect(clippy::cast_possible_wrap, reason = "delta fits in i64")]
    #[cfg(test)]
    pub fn increment_live_chunk_count(&self, segment_id: u64, delta: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE segments SET live_chunk_count = live_chunk_count + ?1 WHERE segment_id = ?2",
                params![delta as i64, segment_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to increment live_chunk_count for segment {segment_id}: {e}"
                ))
            })?;
        Ok(())
    }

    /// Return all segments with their sealed state and recorded size.
    ///
    /// Used by crash recovery to verify sealed segments and identify the
    /// current (unsealed) segment for torn-tail repair.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn all_segments(&self) -> Result<Vec<(u64, Option<String>, u64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT segment_id, sealed_at, size_bytes FROM segments")
            .map_err(|e| {
                StagingError::Internal(format!("failed to prepare all_segments query: {e}"))
            })?;

        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|e| StagingError::Internal(format!("failed to query all_segments: {e}")))?
            .collect::<std::result::Result<Vec<(u64, Option<String>, u64)>, _>>()
            .map_err(|e| StagingError::Internal(format!("failed to collect all_segments: {e}")))?;

        Ok(rows)
    }

    /// Delete all pending chunk rows for a segment (used in recovery
    /// and abandoned-segment cleanup).
    ///
    /// Returns the number of rows removed.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn delete_pending_for_segment(&self, segment_id: u64) -> Result<u64> {
        let n = self
            .conn
            .execute(
                "DELETE FROM pending_chunks WHERE segment_id = ?1",
                params![segment_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to delete pending chunks for segment {segment_id}: {e}"
                ))
            })?;
        Ok(n as u64)
    }

    /// Delete pending rows whose framed record crosses a recovered boundary.
    ///
    /// Used by recovery to discard rows whose payload or framing was truncated,
    /// while preserving rows fully contained within the valid range.
    pub fn delete_pending_beyond_offset(&self, segment_id: u64, max_offset: u64) -> Result<()> {
        #[expect(clippy::cast_possible_wrap, reason = "offset fits in i64")]
        let max_offset_i64 = max_offset as i64;
        self.conn
            .execute(
                "DELETE FROM pending_chunks
                 WHERE segment_id = ?1
                   AND (segment_offset < 0
                        OR size < 0
                        OR segment_offset + size + 8 > ?2)",
                params![segment_id, max_offset_i64],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to delete pending chunks crossing offset {max_offset} for segment {segment_id}: {e}"
                ))
            })?;
        Ok(())
    }

    /// Look up a chunk in `pending_chunks`, returning its locator if present.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn locate_pending(&self, chunk_hash: &[u8; 32]) -> Result<Option<ChunkLocator>> {
        let hash_slice: &[u8] = chunk_hash;
        self.conn
            .query_row(
                "SELECT segment_id, segment_offset, size
                 FROM pending_chunks
                 WHERE chunk_hash = ?1
                 LIMIT 1",
                params![hash_slice],
                |row| {
                    Ok(ChunkLocator {
                        segment_id: row.get(0)?,
                        offset: row.get(1)?,
                        length: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|e| StagingError::Internal(format!("failed to locate pending chunk: {e}")))
    }

    /// Batch locator lookup across `chunks` and `pending_chunks`.
    ///
    /// Issues a single `WHERE chunk_hash IN (?, ?, ...)` query per table
    /// instead of one round-trip per hash. Returns locators in the same
    /// order as `hashes`, with `None` for chunks absent from both tables.
    ///
    /// Committed rows in `chunks` take precedence over pending rows for
    /// the same hash — matching the fallback order in `get_chunk`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn locate_batch(&self, hashes: &[[u8; 32]]) -> Result<Vec<Option<ChunkLocator>>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        // Bucket input indices by chunk_hash. Duplicate hashes in the
        // input all get the same locator.
        let mut index_by_hash: std::collections::HashMap<[u8; 32], Vec<usize>> =
            std::collections::HashMap::with_capacity(hashes.len());
        for (i, h) in hashes.iter().enumerate() {
            index_by_hash.entry(*h).or_default().push(i);
        }

        let unique_hashes: Vec<[u8; 32]> = index_by_hash.keys().copied().collect();
        let placeholders = vec!["?"; unique_hashes.len()].join(",");

        let mut out: Vec<Option<ChunkLocator>> = vec![None; hashes.len()];

        // Query committed chunks first.
        {
            let sql = format!(
                "SELECT chunk_hash, segment_id, segment_offset, size
                 FROM chunks
                 WHERE chunk_hash IN ({placeholders})"
            );
            let mut stmt = self
                .conn
                .prepare(&sql)
                .map_err(|e| StagingError::Internal(format!("prepare locate_batch: {e}")))?;
            let rows = stmt
                .query_map(
                    params_from_iter(unique_hashes.iter().map(|h| h.as_slice())),
                    |row| {
                        let blob: Vec<u8> = row.get(0)?;
                        let seg_id: u64 = row.get(1)?;
                        let offset: u64 = row.get(2)?;
                        let length: u32 = row.get(3)?;
                        Ok((blob, seg_id, offset, length))
                    },
                )
                .map_err(|e| StagingError::Internal(format!("query locate_batch: {e}")))?;

            for row in rows {
                let (blob, seg_id, offset, length) =
                    row.map_err(|e| StagingError::Internal(format!("read locate_batch row: {e}")))?;
                if blob.len() != 32 {
                    continue;
                }
                let mut h = [0u8; 32];
                h.copy_from_slice(&blob);
                if let Some(indices) = index_by_hash.get(&h) {
                    let loc = ChunkLocator {
                        segment_id: seg_id,
                        offset,
                        length,
                    };
                    for &i in indices {
                        out[i] = Some(loc);
                    }
                }
            }
        }

        // Query pending chunks for hashes still missing.
        let missing_hashes: Vec<[u8; 32]> = unique_hashes
            .iter()
            .copied()
            .filter(|h| {
                index_by_hash
                    .get(h)
                    .is_some_and(|indices| indices.iter().any(|&i| out[i].is_none()))
            })
            .collect();

        if !missing_hashes.is_empty() {
            let pending_placeholders = vec!["?"; missing_hashes.len()].join(",");
            let sql = format!(
                "SELECT chunk_hash, segment_id, segment_offset, size
                 FROM pending_chunks
                 WHERE chunk_hash IN ({pending_placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql).map_err(|e| {
                StagingError::Internal(format!("prepare locate_batch pending: {e}"))
            })?;
            let rows = stmt
                .query_map(
                    params_from_iter(missing_hashes.iter().map(|h| h.as_slice())),
                    |row| {
                        let blob: Vec<u8> = row.get(0)?;
                        let seg_id: u64 = row.get(1)?;
                        let offset: u64 = row.get(2)?;
                        let length: u32 = row.get(3)?;
                        Ok((blob, seg_id, offset, length))
                    },
                )
                .map_err(|e| StagingError::Internal(format!("query locate_batch pending: {e}")))?;

            for row in rows {
                let (blob, seg_id, offset, length) = row.map_err(|e| {
                    StagingError::Internal(format!("read locate_batch pending row: {e}"))
                })?;
                if blob.len() != 32 {
                    continue;
                }
                let mut h = [0u8; 32];
                h.copy_from_slice(&blob);
                if let Some(indices) = index_by_hash.get(&h) {
                    let loc = ChunkLocator {
                        segment_id: seg_id,
                        offset,
                        length,
                    };
                    for &i in indices {
                        if out[i].is_none() {
                            out[i] = Some(loc);
                        }
                    }
                }
            }
        }

        Ok(out)
    }

    /// Batch lookup for chunks whose durable payload is a prepared xorb.
    pub fn locate_prepared_batch(
        &self,
        hashes: &[[u8; 32]],
    ) -> Result<Vec<Option<PreparedChunkLocator>>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        let mut indices_by_hash = HashMap::<[u8; 32], Vec<usize>>::with_capacity(hashes.len());
        for (index, hash) in hashes.iter().enumerate() {
            indices_by_hash.entry(*hash).or_default().push(index);
        }
        let unique_hashes = indices_by_hash.keys().copied().collect::<Vec<_>>();
        let placeholders = vec!["?"; unique_hashes.len()].join(",");
        let sql = format!(
            "SELECT chunk_hash, file_hash, xorb_hash, payload_hash, bytes,
                    chunk_index, uncompressed_size
             FROM (
                 SELECT chunk.chunk_hash, chunk.file_hash, chunk.xorb_hash,
                        xorb.payload_hash, xorb.bytes, chunk.chunk_index,
                        chunk.uncompressed_size,
                        ROW_NUMBER() OVER (
                            PARTITION BY chunk.chunk_hash
                            ORDER BY chunk.file_hash, chunk.xorb_hash
                        ) AS candidate_rank
                 FROM prepared_xorb_chunks AS chunk
                 JOIN prepared_xorbs AS xorb
                   ON xorb.file_hash = chunk.file_hash
                  AND xorb.xorb_hash = chunk.xorb_hash
                 WHERE chunk.chunk_hash IN ({placeholders})
             )
             WHERE candidate_rank = 1"
        );
        let mut statement = self.conn.prepare(&sql).map_err(|error| {
            StagingError::Internal(format!("prepare prepared chunk batch lookup: {error}"))
        })?;
        let rows = statement
            .query_map(
                params_from_iter(unique_hashes.iter().map(|hash| hash.as_slice())),
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .map_err(|error| {
                StagingError::Internal(format!("query prepared chunk batch lookup: {error}"))
            })?;

        let mut out = vec![None; hashes.len()];
        for row in rows {
            let (chunk_hash, file_hash, xorb_hash, payload_hash, bytes, chunk_index, size) = row
                .map_err(|error| {
                    StagingError::Internal(format!("read prepared chunk batch lookup row: {error}"))
                })?;
            let chunk_hash = decode_hash_blob("prepared chunk hash", chunk_hash)?;
            let locator = PreparedChunkLocator {
                file_hash: decode_hash_blob("prepared chunk file hash", file_hash)?,
                xorb_hash: decode_hash_blob("prepared chunk xorb hash", xorb_hash)?,
                payload_hash: decode_hash_blob("prepared chunk payload hash", payload_hash)?,
                xorb_bytes: u64::try_from(bytes).map_err(|_| {
                    StagingError::StagingCorrupt("negative prepared xorb size".to_owned())
                })?,
                chunk_index: u32::try_from(chunk_index).map_err(|_| {
                    StagingError::StagingCorrupt("invalid prepared chunk index".to_owned())
                })?,
                size: u32::try_from(size).map_err(|_| {
                    StagingError::StagingCorrupt("invalid prepared chunk size".to_owned())
                })?,
            };
            if let Some(indices) = indices_by_hash.get(&chunk_hash) {
                for index in indices {
                    out[*index] = Some(locator);
                }
            }
        }
        Ok(out)
    }

    /// Batch dedup check: classify chunks as existing (with locator) or new.
    ///
    /// For each `(index, chunk_hash)` pair, checks both `chunks` and
    /// `pending_chunks` tables. Returns two vecs:
    /// - `existing`: `(original_index, chunk_hash, locator, is_mapped)` for
    ///   chunks already stored. `is_mapped` is true if the chunk is already
    ///   associated with `file_hash`.
    /// - `new_indices`: indices of chunks not found in either table.
    ///
    /// Uses a temp table + batch INSERT to avoid per-chunk round-trips.
    pub fn batch_dedup_check(
        &self,
        hashes: &[(usize, [u8; 32])],
        file_hash: &[u8; 32],
    ) -> Result<BatchDedupResult> {
        if hashes.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let fh: &[u8] = file_hash;
        let mut indices_by_hash: std::collections::HashMap<[u8; 32], Vec<usize>> =
            std::collections::HashMap::with_capacity(hashes.len());
        for (i, hash) in hashes {
            indices_by_hash.entry(*hash).or_default().push(*i);
        }

        let unique_hashes: Vec<[u8; 32]> = indices_by_hash.keys().copied().collect();
        let mut existing_by_hash: std::collections::HashMap<[u8; 32], (ChunkLocator, bool)> =
            std::collections::HashMap::new();

        // Look up one locator per unique hash. Repeated chunk hashes
        // are common for sparse or zero-filled large files; joining
        // every batch row against every existing row for the same hash
        // turns into an O(batch * staged_rows) explosion.
        let placeholders = vec!["?"; unique_hashes.len()].join(",");

        let mut found_set = std::collections::HashSet::new();
        {
            let sql = format!(
                "SELECT picked.chunk_hash, c.segment_id, c.segment_offset, c.size,
                        EXISTS(SELECT 1 FROM chunks c2
                               WHERE c2.chunk_hash = picked.chunk_hash
                                 AND c2.file_hash = ? LIMIT 1)
                 FROM (
                     SELECT chunk_hash, MIN(rowid) AS picked_rowid
                     FROM chunks
                     WHERE chunk_hash IN ({placeholders})
                     GROUP BY chunk_hash
                 ) picked
                 INNER JOIN chunks c ON c.rowid = picked.picked_rowid"
            );
            let mut stmt = self
                .conn
                .prepare(&sql)
                .map_err(|e| StagingError::Internal(format!("prepare committed join: {e}")))?;

            let rows = stmt
                .query_map(
                    params_from_iter(
                        std::iter::once(fh).chain(unique_hashes.iter().map(|h| h.as_slice())),
                    ),
                    |row| {
                        let blob: Vec<u8> = row.get(0)?;
                        let seg_id: u64 = row.get(1)?;
                        let offset: u64 = row.get(2)?;
                        let length: u32 = row.get(3)?;
                        let mapped: bool = row.get(4)?;
                        Ok((blob, seg_id, offset, length, mapped))
                    },
                )
                .map_err(|e| StagingError::Internal(format!("query committed join: {e}")))?;

            for row in rows {
                let (blob, seg_id, offset, length, mapped) =
                    row.map_err(|e| StagingError::Internal(format!("read committed row: {e}")))?;
                let mut hash = [0u8; 32];
                if blob.len() == 32 {
                    hash.copy_from_slice(&blob);
                }
                existing_by_hash.insert(
                    hash,
                    (
                        ChunkLocator {
                            segment_id: seg_id,
                            offset,
                            length,
                        },
                        mapped,
                    ),
                );
            }
        }

        let missing_hashes: Vec<[u8; 32]> = unique_hashes
            .iter()
            .copied()
            .filter(|hash| !existing_by_hash.contains_key(hash))
            .collect();

        if !missing_hashes.is_empty() {
            let pending_placeholders = vec!["?"; missing_hashes.len()].join(",");
            let sql = format!(
                "SELECT picked.chunk_hash, p.segment_id, p.segment_offset, p.size,
                        EXISTS(SELECT 1 FROM pending_chunks p2
                               WHERE p2.chunk_hash = picked.chunk_hash
                                 AND p2.file_hash = ? LIMIT 1)
                 FROM (
                     SELECT chunk_hash, MIN(rowid) AS picked_rowid
                     FROM pending_chunks
                     WHERE chunk_hash IN ({pending_placeholders})
                     GROUP BY chunk_hash
                 ) picked
                 INNER JOIN pending_chunks p ON p.rowid = picked.picked_rowid"
            );
            let mut stmt = self
                .conn
                .prepare(&sql)
                .map_err(|e| StagingError::Internal(format!("prepare pending join: {e}")))?;

            let rows = stmt
                .query_map(
                    params_from_iter(
                        std::iter::once(fh).chain(missing_hashes.iter().map(|h| h.as_slice())),
                    ),
                    |row| {
                        let blob: Vec<u8> = row.get(0)?;
                        let seg_id: u64 = row.get(1)?;
                        let offset: u64 = row.get(2)?;
                        let length: u32 = row.get(3)?;
                        let mapped: bool = row.get(4)?;
                        Ok((blob, seg_id, offset, length, mapped))
                    },
                )
                .map_err(|e| StagingError::Internal(format!("query pending join: {e}")))?;

            for row in rows {
                let (blob, seg_id, offset, length, mapped) =
                    row.map_err(|e| StagingError::Internal(format!("read pending row: {e}")))?;
                let mut hash = [0u8; 32];
                if blob.len() == 32 {
                    hash.copy_from_slice(&blob);
                }
                existing_by_hash.insert(
                    hash,
                    (
                        ChunkLocator {
                            segment_id: seg_id,
                            offset,
                            length,
                        },
                        mapped,
                    ),
                );
            }
        }

        let mut existing = Vec::new();
        for (hash, (loc, mapped)) in existing_by_hash {
            if let Some(indices) = indices_by_hash.get(&hash) {
                for &idx in indices {
                    found_set.insert(idx);
                    existing.push((idx, hash, loc, mapped));
                }
            }
        }

        // Everything not in found_set is new.
        let new_indices: Vec<usize> = hashes
            .iter()
            .filter_map(|(i, _)| {
                if found_set.contains(i) {
                    None
                } else {
                    Some(*i)
                }
            })
            .collect();

        Ok((existing, new_indices))
    }

    /// Return sealed segment ids whose dead-byte ratio exceeds `dead_ratio`.
    ///
    /// Dead ratio is `(size_bytes - live_bytes) / size_bytes` where
    /// `live_bytes = SUM(size + 8)` for all live chunks in the segment
    /// (the `+8` accounts for per-record framing).
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn compaction_candidates(&self, dead_ratio: f64) -> Result<Vec<u64>> {
        // Compute live_bytes per segment from the chunks table, then
        // compare against segments.size_bytes.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.segment_id
                 FROM segments s
                 LEFT JOIN (
                     SELECT segment_id, SUM(size + 8) AS live_bytes
                     FROM chunks
                     GROUP BY segment_id
                 ) c ON s.segment_id = c.segment_id
                 WHERE s.sealed_at IS NOT NULL
                   AND s.size_bytes > 0
                   AND (CAST(s.size_bytes - COALESCE(c.live_bytes, 0) AS REAL)
                        / CAST(s.size_bytes AS REAL)) >= ?1",
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to prepare compaction query: {e}"))
            })?;

        let ids = stmt
            .query_map(params![dead_ratio], |row| row.get(0))
            .map_err(|e| {
                StagingError::Internal(format!("failed to query compaction candidates: {e}"))
            })?
            .collect::<std::result::Result<Vec<u64>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!("failed to collect compaction candidates: {e}"))
            })?;

        Ok(ids)
    }

    /// Return live chunks for a segment, ordered by segment offset.
    ///
    /// Each tuple is `(chunk_hash, file_hash, chunk_index, size, segment_offset)`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    #[expect(
        clippy::type_complexity,
        reason = "tuple matches the design's specified return shape"
    )]
    pub fn live_chunks_for_segment(
        &self,
        segment_id: u64,
    ) -> Result<Vec<([u8; 32], [u8; 32], i64, i64, u64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT chunk_hash, file_hash, chunk_index, size, segment_offset
                 FROM chunks
                 WHERE segment_id = ?1
                 ORDER BY segment_offset ASC",
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to prepare live_chunks query for segment {segment_id}: {e}"
                ))
            })?;

        let rows = stmt
            .query_map(params![segment_id], |row| {
                let ch_blob: Vec<u8> = row.get(0)?;
                let fh_blob: Vec<u8> = row.get(1)?;
                let mut ch = [0u8; 32];
                let mut fh = [0u8; 32];
                if ch_blob.len() == 32 {
                    ch.copy_from_slice(&ch_blob);
                }
                if fh_blob.len() == 32 {
                    fh.copy_from_slice(&fh_blob);
                }
                Ok((
                    ch,
                    fh,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            })
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to query live chunks for segment {segment_id}: {e}"
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to collect live chunks for segment {segment_id}: {e}"
                ))
            })?;

        Ok(rows)
    }

    /// Atomically swap chunk locators from an old segment to a new one.
    ///
    /// In a single transaction: updates each chunk's `segment_id` and
    /// `segment_offset`, inserts the new segment row, and zeroes the old
    /// segment's `live_chunk_count` (marking it for sweep).
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    #[expect(clippy::cast_possible_wrap, reason = "sizes fit in i64 for SQLite")]
    pub fn swap_locators(
        &self,
        old_segment_id: u64,
        new_segment_id: u64,
        new_size_bytes: u64,
        updates: &[([u8; 32], [u8; 32], i64, u64)], // (chunk_hash, file_hash, chunk_index, new_offset)
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin swap_locators tx: {e}"))
        })?;

        // Update each chunk's locator to point at the new segment.
        {
            let mut stmt = tx
                .prepare_cached(
                    "UPDATE chunks
                     SET segment_id = ?1, segment_offset = ?2
                     WHERE chunk_hash = ?3 AND file_hash = ?4 AND chunk_index = ?5",
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to prepare swap update: {e}"))
                })?;

            for (chunk_hash, file_hash, chunk_index, new_offset) in updates {
                let ch: &[u8] = chunk_hash;
                let fh: &[u8] = file_hash;
                stmt.execute(params![
                    new_segment_id,
                    *new_offset as i64,
                    ch,
                    fh,
                    chunk_index,
                ])
                .map_err(|e| {
                    StagingError::Internal(format!("failed to swap locator for chunk: {e}"))
                })?;
            }
        }

        // Insert the new segment as sealed.
        let chunk_count = updates.len() as i64;
        tx.execute(
            "UPDATE segments
             SET sealed_at = datetime('now'),
                 size_bytes = ?1,
                 chunk_count = ?2,
                 live_chunk_count = ?3
             WHERE segment_id = ?4",
            params![
                new_size_bytes as i64,
                chunk_count,
                chunk_count,
                new_segment_id
            ],
        )
        .map_err(|e| {
            StagingError::Internal(format!(
                "failed to update new segment {new_segment_id}: {e}"
            ))
        })?;

        // Zero out the old segment's live_chunk_count so it becomes a sweep candidate.
        tx.execute(
            "UPDATE segments SET live_chunk_count = 0 WHERE segment_id = ?1",
            params![old_segment_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!(
                "failed to zero live_chunk_count for old segment {old_segment_id}: {e}"
            ))
        })?;

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit swap_locators tx: {e}"))
        })?;

        debug!(
            old_segment_id,
            new_segment_id,
            chunks = updates.len(),
            "swapped locators"
        );
        Ok(())
    }

    /// Compute staging statistics via SQL aggregation.
    ///
    /// Queries the `segments`, `chunks`, and `files` tables to produce a
    /// point-in-time snapshot. The current (unsealed) segment's bytes come
    /// from the caller-provided `current_segment_bytes` since the writer
    /// tracks the authoritative offset.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on SQLite failure.
    pub fn staging_stats(&self, current_segment_bytes: u64) -> Result<super::stats::StagingStats> {
        // Sealed segment count and total sealed bytes.
        let (segments_sealed, sealed_bytes): (u64, u64) = self
            .conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0)
                 FROM segments
                 WHERE sealed_at IS NOT NULL",
                [],
                |row| {
                    let count: i64 = row.get(0)?;
                    let bytes: i64 = row.get(1)?;
                    #[expect(clippy::cast_sign_loss, reason = "count and sum are non-negative")]
                    Ok((count as u64, bytes as u64))
                },
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to query sealed segment stats: {e}"))
            })?;

        let total_staged_bytes = sealed_bytes.saturating_add(current_segment_bytes);

        // Count physical records that are still referenced by either a
        // committed or pending occurrence. A locator reused by duplicate
        // files counts once because it occupies bytes only once.
        let (chunk_count, live_bytes): (u64, u64) = self
            .conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(size + 8), 0)
                 FROM (
                     SELECT DISTINCT segment_id, segment_offset, size FROM chunks
                     UNION
                     SELECT DISTINCT segment_id, segment_offset, size FROM pending_chunks
                 )",
                [],
                |row| {
                    let count: i64 = row.get(0)?;
                    let bytes: i64 = row.get(1)?;
                    #[expect(
                        clippy::cast_sign_loss,
                        reason = "count and byte sum are non-negative"
                    )]
                    Ok((count as u64, bytes as u64))
                },
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to query live staging locators: {e}"))
            })?;

        let dead_bytes = total_staged_bytes.saturating_sub(live_bytes);
        #[expect(
            clippy::cast_precision_loss,
            reason = "ratio is for diagnostics; sub-ULP precision loss on multi-PB staging is acceptable"
        )]
        let dead_ratio = if total_staged_bytes > 0 {
            dead_bytes as f64 / total_staged_bytes as f64
        } else {
            0.0
        };

        // File count.
        let file_count: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| {
                let c: i64 = row.get(0)?;
                #[expect(clippy::cast_sign_loss, reason = "count is non-negative")]
                Ok(c as u64)
            })
            .map_err(|e| StagingError::Internal(format!("failed to query file count: {e}")))?;

        Ok(super::stats::StagingStats {
            segments_sealed,
            current_segment_bytes,
            total_staged_bytes,
            live_bytes,
            dead_bytes,
            dead_ratio,
            chunk_count,
            file_count,
        })
    }

    pub fn lifecycle_health(&self) -> Result<super::stats::StagingLifecycleHealth> {
        let layout_version = self
            .conn
            .query_row(
                "SELECT value FROM staging_meta WHERE key = 'layout_version'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to read staging layout version: {e}"))
            })?;
        let count = |query: &str| -> Result<u64> {
            let value: i64 = self
                .conn
                .query_row(query, [], |row| row.get(0))
                .map_err(|e| {
                    StagingError::Internal(format!("failed staging lifecycle query {query:?}: {e}"))
                })?;
            nonnegative_count("staging lifecycle count", value)
        };
        Ok(super::stats::StagingLifecycleHealth {
            layout_version,
            quarantined_entries: count("SELECT COUNT(*) FROM staging_quarantine")?,
            open_push_snapshots: count("SELECT COUNT(*) FROM push_snapshots WHERE state = 'open'")?,
            committed_push_snapshots: count(
                "SELECT COUNT(*) FROM push_snapshots WHERE state = 'committed'",
            )?,
            recipes: count("SELECT COUNT(*) FROM file_recipes")?,
            path_leases: count("SELECT COUNT(*) FROM path_leases")?,
            payloads: count("SELECT COUNT(*) FROM chunk_payloads")?,
        })
    }

    /// Borrow the underlying connection (for recovery and other internal use).
    #[allow(dead_code)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_in_memory() -> Index {
        // Use a temp file so WAL mode works (in-memory doesn't support WAL).
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        Index::open(tmp.path()).expect("open index")
    }

    /// Create a deterministic 32-byte hash from a seed byte for tests.
    fn test_hash(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    /// Insert a file row directly via SQL for test setup.
    fn insert_test_file(idx: &Index, file_hash: &[u8; 32], total_bytes: i64) {
        let fh: &[u8] = file_hash;
        idx.conn
            .execute(
                "INSERT INTO files (file_hash, total_bytes) VALUES (?1, ?2)",
                params![fh, total_bytes],
            )
            .expect("insert test file");
    }

    fn assert_staging_corrupt_contains<T: std::fmt::Debug>(result: Result<T>, expected: &str) {
        match result {
            Err(StagingError::StagingCorrupt(reason)) => {
                assert!(
                    reason.contains(expected),
                    "expected staging corruption containing {expected:?}, got {reason:?}"
                );
            }
            other => panic!("expected staging corruption, got {other:?}"),
        }
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path();

        let idx1 = Index::open(path).expect("first open");
        drop(idx1);
        let idx2 = Index::open(path).expect("second open");

        let version: String = idx2
            .conn
            .query_row(
                "SELECT value FROM staging_meta WHERE key = 'layout_version'",
                [],
                |row| row.get(0),
            )
            .expect("read version");
        assert_eq!(version, LAYOUT_VERSION);
    }

    #[test]
    fn migration_indexes_recipe_payload_leases_by_chunk() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path();

        let idx = Index::open(path).expect("first open");
        idx.conn
            .execute("DROP INDEX IF EXISTS recipe_payload_leases_by_chunk", [])
            .expect("drop chunk lease index");
        drop(idx);

        let idx = Index::open(path).expect("migration reopen");
        let columns: Vec<String> = idx
            .conn
            .prepare("PRAGMA index_info(recipe_payload_leases_by_chunk)")
            .expect("prepare index info")
            .query_map([], |row| row.get(2))
            .expect("query index info")
            .collect::<std::result::Result<_, _>>()
            .expect("collect index columns");

        assert_eq!(columns, vec!["chunk_hash"]);
    }

    #[test]
    fn open_push_snapshot_pins_only_its_exact_recipe() {
        let idx = open_in_memory();
        let segment_id = idx.allocate_segment_id().expect("allocate segment");
        let file_hash = test_hash(0x71);
        let chunk_hash = test_hash(0x72);
        insert_test_file(&idx, &file_hash, 8);
        idx.conn
            .execute(
                "INSERT INTO chunks
                 (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 8, ?3, 0)",
                params![chunk_hash.as_slice(), file_hash.as_slice(), segment_id],
            )
            .expect("insert chunk");
        let recipe = crate::recipe::FileRecipe::from_staged_chunks(
            crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            crab_xet::hash::MerkleHash::from(file_hash),
            8,
            &[(crab_xet::hash::MerkleHash::from(chunk_hash), 8)],
        )
        .expect("recipe");
        idx.insert_batch("batch-a").expect("batch");
        idx.insert_recipe_lease(
            "batch-a",
            b"large.bin",
            &recipe,
            RecipeVerification::CallerVerified,
        )
        .expect("recipe lease");
        idx.mark_batch_published("batch-a").expect("publish");

        idx.create_push_snapshot("push-a", std::slice::from_ref(&recipe))
            .expect("snapshot");
        assert!(
            idx.retire_push_snapshot("push-a").is_err(),
            "an open snapshot cannot be retired before manifest commit"
        );

        idx.insert_batch("batch-b").expect("concurrent batch");
        idx.insert_recipe_lease(
            "batch-b",
            b"copy.bin",
            &recipe,
            RecipeVerification::CallerVerified,
        )
        .expect("concurrent recipe lease");
        idx.mark_batch_published("batch-b")
            .expect("publish concurrent batch");

        idx.commit_push_snapshot("push-a").expect("commit snapshot");
        assert!(
            idx.retire_push_snapshot("push-a")
                .expect("committed retirement")
                .is_empty(),
            "a lease published after the snapshot must retain the shared recipe"
        );
        let remaining: Vec<String> = idx
            .conn
            .prepare("SELECT batch_id FROM path_leases ORDER BY batch_id")
            .expect("prepare remaining leases")
            .query_map([], |row| row.get(0))
            .expect("query remaining leases")
            .collect::<std::result::Result<_, _>>()
            .expect("collect remaining leases");
        assert_eq!(remaining, vec!["batch-b".to_owned()]);
        idx.remove_push_snapshot("push-a").expect("remove snapshot");
    }

    #[test]
    fn only_published_recipe_leases_are_visible_to_push() {
        let idx = open_in_memory();
        let segment_id = idx.allocate_segment_id().expect("allocate segment");
        let file_hash = test_hash(0x81);
        let chunk_hash = test_hash(0x82);
        insert_test_file(&idx, &file_hash, 8);
        idx.conn
            .execute(
                "INSERT INTO chunks
                 (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 8, ?3, 0)",
                params![chunk_hash.as_slice(), file_hash.as_slice(), segment_id],
            )
            .expect("insert chunk");
        let recipe = crate::recipe::FileRecipe::from_staged_chunks(
            crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            crab_xet::hash::MerkleHash::from(file_hash),
            8,
            &[(crab_xet::hash::MerkleHash::from(chunk_hash), 8)],
        )
        .expect("recipe");

        assert!(
            idx.published_recipe_for_file(&file_hash)
                .expect("legacy-only lookup")
                .is_none(),
            "legacy file/chunk rows cannot publish a recipe"
        );

        idx.insert_batch("batch-open").expect("batch");
        idx.insert_recipe_lease(
            "batch-open",
            b"large.bin",
            &recipe,
            RecipeVerification::CallerVerified,
        )
        .expect("recipe lease");
        assert!(
            idx.published_recipe_for_file(&file_hash)
                .expect("open lookup")
                .is_none(),
            "an open batch is not visible before its Git index commit"
        );

        idx.mark_batch_published("batch-open").expect("publish");
        assert_eq!(
            idx.published_recipe_for_file(&file_hash)
                .expect("published lookup"),
            Some(recipe)
        );
    }

    #[test]
    fn recipe_lease_publishes_large_repeated_recipe_exactly_once_per_occurrence() {
        const OCCURRENCES: usize = 10_000;

        let idx = open_in_memory();
        let segment_id = idx.allocate_segment_id().expect("allocate segment");
        let file_hash = test_hash(0x83);
        let chunk_hash = test_hash(0x84);
        let file_size = i64::try_from(OCCURRENCES * 8).expect("file size");
        insert_test_file(&idx, &file_hash, file_size);
        idx.conn
            .execute(
                "INSERT INTO chunks
                 (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 8, ?3, 0)",
                params![chunk_hash.as_slice(), file_hash.as_slice(), segment_id],
            )
            .expect("insert chunk payload");
        let chunks = vec![(crab_xet::hash::MerkleHash::from(chunk_hash), 8); OCCURRENCES];
        let recipe = crate::recipe::FileRecipe::from_staged_chunks(
            crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            crab_xet::hash::MerkleHash::from(file_hash),
            u64::try_from(file_size).expect("nonnegative file size"),
            &chunks,
        )
        .expect("recipe");
        idx.insert_batch("batch-large").expect("batch");

        idx.insert_recipe_lease(
            "batch-large",
            b"large.bin",
            &recipe,
            RecipeVerification::CallerVerified,
        )
        .expect("recipe lease");

        let counts: (i64, i64) = idx
            .conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM recipe_occurrences),
                    (SELECT COUNT(*) FROM recipe_payload_leases)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("recipe row counts");
        assert_eq!(
            counts,
            (i64::try_from(OCCURRENCES).expect("occurrence count"), 1)
        );
    }

    #[test]
    fn recipe_lease_rolls_back_when_payload_is_missing() {
        let idx = open_in_memory();
        let file_hash = test_hash(0x85);
        let missing_chunk = test_hash(0x86);
        let recipe = crate::recipe::FileRecipe::from_staged_chunks(
            crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            crab_xet::hash::MerkleHash::from(file_hash),
            8,
            &[(crab_xet::hash::MerkleHash::from(missing_chunk), 8)],
        )
        .expect("recipe");
        idx.insert_batch("batch-missing").expect("batch");

        assert_staging_corrupt_contains(
            idx.insert_recipe_lease(
                "batch-missing",
                b"missing.bin",
                &recipe,
                RecipeVerification::CallerVerified,
            ),
            "no exact staged payload",
        );

        let counts: (i64, i64, i64) = idx
            .conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM file_recipes),
                    (SELECT COUNT(*) FROM recipe_occurrences),
                    (SELECT COUNT(*) FROM path_leases)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("rollback counts");
        assert_eq!(counts, (0, 0, 0));
    }

    #[test]
    fn retiring_last_recipe_reclaims_payload_inventory() {
        let idx = open_in_memory();
        let segment_id = idx.allocate_segment_id().expect("allocate segment");
        let file_hash = test_hash(0x91);
        let chunk_hash = test_hash(0x92);
        let xorb_hash = test_hash(0x93);
        insert_test_file(&idx, &file_hash, 8);
        idx.conn
            .execute(
                "INSERT INTO chunks
                 (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 8, ?3, 0)",
                params![chunk_hash.as_slice(), file_hash.as_slice(), segment_id],
            )
            .expect("insert chunk");
        let recipe = crate::recipe::FileRecipe::from_staged_chunks(
            crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            crab_xet::hash::MerkleHash::from(file_hash),
            8,
            &[(crab_xet::hash::MerkleHash::from(chunk_hash), 8)],
        )
        .expect("recipe");
        idx.insert_batch("batch-retire").expect("batch");
        idx.insert_recipe_lease(
            "batch-retire",
            b"large.bin",
            &recipe,
            RecipeVerification::CallerVerified,
        )
        .expect("recipe lease");
        idx.mark_batch_published("batch-retire").expect("publish");
        idx.conn
            .execute(
                "INSERT INTO prepared_payloads (xorb_hash, payload_hash, bytes)
                 VALUES (?1, ?2, 16)",
                params![xorb_hash.as_slice(), xorb_hash.as_slice()],
            )
            .expect("prepared payload");
        idx.conn
            .execute(
                "INSERT INTO prepared_leases (recipe_hash, xorb_hash) VALUES (?1, ?2)",
                params![recipe.hash().as_slice(), xorb_hash.as_slice()],
            )
            .expect("prepared lease");

        idx.create_push_snapshot("push-retire", std::slice::from_ref(&recipe))
            .expect("snapshot");
        idx.commit_push_snapshot("push-retire")
            .expect("commit snapshot");
        assert_eq!(
            idx.retire_push_snapshot("push-retire")
                .expect("retire exact snapshot"),
            vec![file_hash]
        );
        idx.remove_file(&file_hash).expect("remove file");
        idx.remove_push_snapshot("push-retire")
            .expect("remove snapshot");

        let remaining: (i64, i64, i64, i64) = idx
            .conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM file_recipes),
                    (SELECT COUNT(*) FROM recipe_payload_leases),
                    (SELECT COUNT(*) FROM chunk_payloads),
                    (SELECT COUNT(*) FROM prepared_payloads)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("remaining ownership rows");
        assert_eq!(remaining, (0, 0, 0, 0));
    }

    #[test]
    fn migration_dedups_legacy_chunk_position_rows() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let file_hash = test_hash(0xF1);
        let chunk_a = test_hash(0xA1);
        let stale_chunk_a = test_hash(0xA2);
        let chunk_b = test_hash(0xB1);
        let stale_chunk_b = test_hash(0xB2);

        {
            let conn = Connection::open(&path).expect("open legacy db");
            conn.execute_batch(
                "CREATE TABLE files (
                    file_hash    BLOB PRIMARY KEY,
                    shard_hash   BLOB,
                    total_bytes  INTEGER NOT NULL,
                    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE segments (
                    segment_id       INTEGER PRIMARY KEY,
                    sealed_at        TEXT,
                    size_bytes       INTEGER NOT NULL,
                    chunk_count      INTEGER NOT NULL DEFAULT 0,
                    live_chunk_count INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE chunks (
                    chunk_hash       BLOB NOT NULL,
                    file_hash        BLOB NOT NULL,
                    chunk_index      INTEGER NOT NULL,
                    size             INTEGER NOT NULL,
                    segment_id       INTEGER NOT NULL,
                    segment_offset   INTEGER NOT NULL
                );
                CREATE TABLE pending_chunks (
                    chunk_hash       BLOB NOT NULL,
                    file_hash        BLOB NOT NULL,
                    chunk_index      INTEGER NOT NULL,
                    size             INTEGER NOT NULL,
                    segment_id       INTEGER NOT NULL,
                    segment_offset   INTEGER NOT NULL
                );",
            )
            .expect("create legacy schema");
            conn.execute(
                "INSERT INTO files (file_hash, total_bytes) VALUES (?1, ?2)",
                params![file_hash.as_slice(), 8_i64],
            )
            .expect("insert file");
            conn.execute(
                "INSERT INTO segments (segment_id, sealed_at, size_bytes, chunk_count, live_chunk_count)
                 VALUES (1, datetime('now'), 32, 4, 4)",
                [],
            )
            .expect("insert segment");
            for (chunk_hash, chunk_index, offset) in
                [(chunk_a, 0_i64, 0_i64), (stale_chunk_a, 0_i64, 8_i64)]
            {
                conn.execute(
                    "INSERT INTO chunks
                     (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                     VALUES (?1, ?2, ?3, 4, 1, ?4)",
                    params![
                        chunk_hash.as_slice(),
                        file_hash.as_slice(),
                        chunk_index,
                        offset
                    ],
                )
                .expect("insert legacy chunk");
            }
            for (chunk_hash, chunk_index, offset) in
                [(chunk_b, 1_i64, 16_i64), (stale_chunk_b, 1_i64, 24_i64)]
            {
                conn.execute(
                    "INSERT INTO pending_chunks
                     (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                     VALUES (?1, ?2, ?3, 4, 1, ?4)",
                    params![
                        chunk_hash.as_slice(),
                        file_hash.as_slice(),
                        chunk_index,
                        offset
                    ],
                )
                .expect("insert legacy pending chunk");
            }
        }

        let idx = Index::open(&path).expect("migrate legacy db");
        assert_eq!(
            idx.chunks_for_file(&file_hash).expect("chunks"),
            vec![chunk_a, chunk_b]
        );
        assert_eq!(
            idx.chunks_for_file_with_sizes(&file_hash)
                .expect("chunks with sizes"),
            vec![(chunk_a, 4), (chunk_b, 4)]
        );
        let migrated: (i64, i64, i64, i64) = idx
            .conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM file_recipes WHERE policy_id = 'xet-gear-v1-64k'),
                    (SELECT COUNT(*) FROM recipe_occurrences),
                    (SELECT COUNT(*) FROM chunk_payloads),
                    (SELECT COUNT(*) FROM recipe_payload_leases)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read migrated ownership rows");
        assert_eq!(
            migrated,
            (1, 2, 4, 2),
            "all distinct legacy payloads survive even when duplicate positions are normalized"
        );
        let lease_count: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM path_leases WHERE batch_id = 'migration-v2'",
                [],
                |row| row.get(0),
            )
            .expect("read migration lease");
        assert_eq!(lease_count, 1);

        let duplicate_chunk = test_hash(0xEE);
        let duplicate_committed = idx.conn.execute(
            "INSERT INTO chunks
             (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
             VALUES (?1, ?2, 0, 4, 1, 36)",
            params![duplicate_chunk.as_slice(), file_hash.as_slice()],
        );
        assert!(
            duplicate_committed.is_err(),
            "migration must enforce unique committed file positions"
        );

        let duplicate_insert = idx.conn.execute(
            "INSERT INTO pending_chunks
             (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
             VALUES (?1, ?2, 1, 4, 1, 40)",
            params![duplicate_chunk.as_slice(), file_hash.as_slice()],
        );
        assert!(
            duplicate_insert.is_err(),
            "migration must enforce unique pending file positions"
        );
    }

    #[test]
    fn wal_mode_is_active() {
        let idx = open_in_memory();
        let mode: String = idx
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("query journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn foreign_keys_enforced() {
        let idx = open_in_memory();

        let seg_id = idx.allocate_segment_id().expect("alloc");
        let ch: &[u8] = &[0xAA; 32];
        let fh: &[u8] = &[0xBB; 32]; // non-existent file
        let result = idx.conn.execute(
            "INSERT INTO chunks (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
             VALUES (?1, ?2, 0, 10, ?3, 0)",
            params![ch, fh, seg_id],
        );
        assert!(
            result.is_err(),
            "FK violation on file_hash should be rejected"
        );
    }

    #[test]
    fn allocate_segment_id_returns_increasing_ids() {
        let idx = open_in_memory();
        let id1 = idx.allocate_segment_id().expect("alloc 1");
        let id2 = idx.allocate_segment_id().expect("alloc 2");
        assert!(id2 > id1, "segment ids should be monotonically increasing");
    }

    #[test]
    fn insert_pending_and_flush_updates_segment_size() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");

        let fh = test_hash(0xF1);
        insert_test_file(&idx, &fh, 100);

        let rows = vec![
            PendingRow {
                chunk_hash: test_hash(0xC1),
                file_hash: fh,
                chunk_index: 0,
                size: 50,
                segment_id: seg_id,
                segment_offset: 0,
            },
            PendingRow {
                chunk_hash: test_hash(0xC2),
                file_hash: fh,
                chunk_index: 1,
                size: 50,
                segment_id: seg_id,
                segment_offset: 58,
            },
        ];

        idx.insert_pending(&rows).expect("insert pending");

        let pending_count: i64 = idx
            .conn
            .query_row("SELECT COUNT(*) FROM pending_chunks", [], |r| r.get(0))
            .expect("count pending");
        assert_eq!(pending_count, 2);

        let flushed = idx.flush_pending(seg_id, 116).expect("flush");
        assert_eq!(flushed, 2);

        let pending_after: i64 = idx
            .conn
            .query_row("SELECT COUNT(*) FROM pending_chunks", [], |r| r.get(0))
            .expect("count pending after");
        assert_eq!(pending_after, 2);

        let chunk_count: i64 = idx
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .expect("count chunks");
        assert_eq!(chunk_count, 0);

        let size: i64 = idx
            .conn
            .query_row(
                "SELECT size_bytes FROM segments WHERE segment_id = ?1",
                params![seg_id],
                |r| r.get(0),
            )
            .expect("read segment");
        assert_eq!(size, 116);
    }

    #[test]
    fn insert_pending_accepts_identical_retry_without_duplicate_row() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xF3);
        insert_test_file(&idx, &fh, 50);

        let row = PendingRow {
            chunk_hash: test_hash(0xC3),
            file_hash: fh,
            chunk_index: 0,
            size: 50,
            segment_id: seg_id,
            segment_offset: 0,
        };
        idx.insert_pending(std::slice::from_ref(&row))
            .expect("first insert");
        idx.insert_pending(std::slice::from_ref(&row))
            .expect("identical retry");

        let pending_count: i64 = idx
            .conn
            .query_row("SELECT COUNT(*) FROM pending_chunks", [], |r| r.get(0))
            .expect("count pending");
        assert_eq!(pending_count, 1);
    }

    #[test]
    fn insert_pending_rejects_conflicting_file_position() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xF4);
        insert_test_file(&idx, &fh, 100);

        idx.insert_pending(&[PendingRow {
            chunk_hash: test_hash(0xC4),
            file_hash: fh,
            chunk_index: 0,
            size: 50,
            segment_id: seg_id,
            segment_offset: 0,
        }])
        .expect("seed pending");

        let err = idx
            .insert_pending(&[PendingRow {
                chunk_hash: test_hash(0xC5),
                file_hash: fh,
                chunk_index: 0,
                size: 50,
                segment_id: seg_id,
                segment_offset: 58,
            }])
            .expect_err("conflicting pending row must fail");

        assert!(
            matches!(err, StagingError::StagingCorrupt(ref msg) if msg.contains("pending chunk collision")),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            idx.chunks_for_file(&fh).expect("chunks"),
            vec![test_hash(0xC4)]
        );
    }

    #[test]
    fn adopt_file_hash_reuses_identical_target_without_retiring_it() {
        let idx = open_in_memory();
        let source_seg = idx.allocate_segment_id().expect("alloc source");
        let target_seg = idx.allocate_segment_id().expect("alloc target");
        let source = test_hash(0xA1);
        let target = test_hash(0xA2);
        let source_chunks = [test_hash(0xC1), test_hash(0xC2)];

        insert_test_file(&idx, &source, 0);
        insert_test_file(&idx, &target, 23);
        idx.insert_pending(&[
            PendingRow {
                chunk_hash: source_chunks[0],
                file_hash: source,
                chunk_index: 0,
                size: 11,
                segment_id: source_seg,
                segment_offset: 0,
            },
            PendingRow {
                chunk_hash: source_chunks[1],
                file_hash: source,
                chunk_index: 1,
                size: 12,
                segment_id: source_seg,
                segment_offset: 19,
            },
            PendingRow {
                chunk_hash: source_chunks[0],
                file_hash: target,
                chunk_index: 0,
                size: 11,
                segment_id: target_seg,
                segment_offset: 0,
            },
            PendingRow {
                chunk_hash: source_chunks[1],
                file_hash: target,
                chunk_index: 1,
                size: 12,
                segment_id: target_seg,
                segment_offset: 19,
            },
        ])
        .expect("seed pending rows");
        idx.insert_chunks_for_file(&target, &[(source_chunks[0], 11), (source_chunks[1], 12)])
            .expect("promote target chunk");

        let adopted = idx
            .adopt_file_hash(&source, &target, 23)
            .expect("adopt source into target");

        assert_eq!(adopted, 2);
        assert!(!idx.file_exists(&source).expect("source existence"));
        assert!(idx.file_exists(&target).expect("target existence"));
        assert_eq!(
            idx.chunks_for_file_with_sizes(&target)
                .expect("target chunks"),
            vec![(source_chunks[0], 11), (source_chunks[1], 12)]
        );

        let source_rows: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_chunks WHERE file_hash = ?1",
                params![&source[..]],
                |r| r.get(0),
            )
            .expect("count source rows");
        assert_eq!(source_rows, 0);

        let target_live: i64 = idx
            .conn
            .query_row(
                "SELECT live_chunk_count FROM segments WHERE segment_id = ?1",
                params![target_seg],
                |r| r.get(0),
            )
            .expect("target live count");
        assert_eq!(target_live, 0);

        let total_bytes: i64 = idx
            .conn
            .query_row(
                "SELECT total_bytes FROM files WHERE file_hash = ?1",
                params![&target[..]],
                |r| r.get(0),
            )
            .expect("target total bytes");
        assert_eq!(total_bytes, 23);
    }

    #[test]
    fn locate_returns_correct_locator() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");

        let fh = test_hash(0xF1);
        insert_test_file(&idx, &fh, 100);

        let ch = test_hash(0xAB);
        let rows = vec![PendingRow {
            chunk_hash: ch,
            file_hash: fh,
            chunk_index: 0,
            size: 64,
            segment_id: seg_id,
            segment_offset: 42,
        }];
        idx.insert_pending(&rows).expect("insert pending");
        idx.flush_pending(seg_id, 72).expect("flush");

        let loc = idx
            .locate_pending(&ch)
            .expect("locate")
            .expect("should find");
        assert_eq!(loc.segment_id, seg_id);
        assert_eq!(loc.offset, 42);
        assert_eq!(loc.length, 64);

        let missing = test_hash(0xFF);
        assert!(idx.locate_pending(&missing).expect("locate none").is_none());
    }

    #[test]
    fn max_committed_offset_returns_correct_value() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");

        assert_eq!(idx.max_committed_offset(seg_id).expect("empty"), 0);

        let fh = test_hash(0xF1);
        insert_test_file(&idx, &fh, 200);

        let rows = vec![
            PendingRow {
                chunk_hash: test_hash(0xC1),
                file_hash: fh,
                chunk_index: 0,
                size: 50,
                segment_id: seg_id,
                segment_offset: 0,
            },
            PendingRow {
                chunk_hash: test_hash(0xC2),
                file_hash: fh,
                chunk_index: 1,
                size: 50,
                segment_id: seg_id,
                segment_offset: 58,
            },
        ];
        idx.insert_pending(&rows).expect("insert");
        idx.flush_pending(seg_id, 116).expect("flush");

        assert_eq!(idx.max_committed_offset(seg_id).expect("offset"), 116);
    }

    #[test]
    fn sweep_candidates_returns_sealed_zero_live() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");

        assert!(idx.sweep_candidates().expect("sweep").is_empty());

        idx.conn
            .execute(
                "UPDATE segments SET sealed_at = datetime('now') WHERE segment_id = ?1",
                params![seg_id],
            )
            .expect("seal");

        let candidates = idx.sweep_candidates().expect("sweep after seal");
        assert_eq!(candidates, vec![seg_id]);
    }

    #[test]
    fn sweep_candidates_keep_sealed_segments_with_pending_rows() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xF1);
        insert_test_file(&idx, &fh, 100);

        idx.insert_pending(&[PendingRow {
            chunk_hash: test_hash(0xC1),
            file_hash: fh,
            chunk_index: 0,
            size: 50,
            segment_id: seg_id,
            segment_offset: 0,
        }])
        .expect("insert pending");
        idx.seal_segment(seg_id, 58).expect("seal");

        assert!(
            idx.sweep_candidates().expect("sweep").is_empty(),
            "pending-only staged rows must keep their sealed segment"
        );
    }

    #[test]
    fn abandoned_segments_keep_pending_only_segments() {
        let idx = open_in_memory();
        let current = idx.allocate_segment_id().expect("alloc current");
        let pending_seg = idx.allocate_segment_id().expect("alloc pending");
        let empty_seg = idx.allocate_segment_id().expect("alloc empty");
        let fh = test_hash(0xF2);
        insert_test_file(&idx, &fh, 100);

        idx.insert_pending(&[PendingRow {
            chunk_hash: test_hash(0xC2),
            file_hash: fh,
            chunk_index: 0,
            size: 50,
            segment_id: pending_seg,
            segment_offset: 0,
        }])
        .expect("insert pending");
        idx.flush_pending(pending_seg, 58).expect("flush pending");
        idx.seal_segment(empty_seg, 4096).expect("seal empty");

        let abandoned = idx.abandoned_segments(current).expect("abandoned");
        assert!(
            abandoned.iter().all(|(seg_id, _)| *seg_id != pending_seg),
            "pending-only staged segment must not be abandoned"
        );
        assert!(
            abandoned.iter().any(|(seg_id, _)| *seg_id == empty_seg),
            "empty segment remains reclaimable"
        );
    }

    #[test]
    fn drop_segment_removes_chunks_and_segment() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");

        let fh = test_hash(0xF1);
        insert_test_file(&idx, &fh, 100);

        let rows = vec![PendingRow {
            chunk_hash: test_hash(0xC1),
            file_hash: fh,
            chunk_index: 0,
            size: 50,
            segment_id: seg_id,
            segment_offset: 0,
        }];
        idx.insert_pending(&rows).expect("insert");
        idx.flush_pending(seg_id, 58).expect("flush");

        idx.drop_segment(seg_id).expect("drop");

        let seg_count: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE segment_id = ?1",
                params![seg_id],
                |r| r.get(0),
            )
            .expect("count segments");
        assert_eq!(seg_count, 0);

        let chunk_count: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE segment_id = ?1",
                params![seg_id],
                |r| r.get(0),
            )
            .expect("count chunks");
        assert_eq!(chunk_count, 0);
    }

    #[test]
    fn list_files_with_chunks_rejects_malformed_file_hash() {
        let idx = open_in_memory();
        idx.conn
            .execute(
                "INSERT INTO files (file_hash, total_bytes) VALUES (?1, ?2)",
                params![vec![0xF1_u8], 100_i64],
            )
            .expect("insert malformed file hash");

        assert_staging_corrupt_contains(
            idx.list_files_with_chunks(),
            "file hash has 1 bytes, expected 32",
        );
    }

    #[test]
    fn list_files_with_chunks_rejects_negative_total_bytes() {
        let idx = open_in_memory();
        let fh = test_hash(0xF1);
        insert_test_file(&idx, &fh, -1);

        assert_staging_corrupt_contains(
            idx.list_files_with_chunks(),
            "file total_bytes is negative: -1",
        );
    }

    #[test]
    fn delete_chunks_for_file_removes_rows_and_updates_segment_counts() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");

        // Two distinct files sharing a single segment; non-overlapping
        // chunks so the accounting is easy to reason about.
        let fh_a = test_hash(0xA1);
        let fh_b = test_hash(0xB2);
        insert_test_file(&idx, &fh_a, 100);
        insert_test_file(&idx, &fh_b, 50);

        // Establish locators via `pending_chunks` so
        // `insert_chunks_for_file` can promote them to `chunks` (and
        // transactionally drop the now-redundant pending rows).
        let pending = vec![
            PendingRow {
                chunk_hash: test_hash(0xC1),
                file_hash: fh_a,
                chunk_index: 0,
                size: 50,
                segment_id: seg_id,
                segment_offset: 0,
            },
            PendingRow {
                chunk_hash: test_hash(0xC2),
                file_hash: fh_a,
                chunk_index: 1,
                size: 50,
                segment_id: seg_id,
                segment_offset: 58,
            },
            PendingRow {
                chunk_hash: test_hash(0xC3),
                file_hash: fh_b,
                chunk_index: 0,
                size: 50,
                segment_id: seg_id,
                segment_offset: 116,
            },
        ];
        idx.insert_pending(&pending).expect("insert pending");

        idx.insert_chunks_for_file(&fh_a, &[(test_hash(0xC1), 50), (test_hash(0xC2), 50)])
            .expect("insert chunks for file A");
        idx.insert_chunks_for_file(&fh_b, &[(test_hash(0xC3), 50)])
            .expect("insert chunks for file B");

        // Seal the segment so it can become a sweep candidate.
        idx.seal_segment(seg_id, 174).expect("seal");

        let (rows_deleted, touched) = idx.delete_chunks_for_file(&fh_a).expect("delete");

        assert_eq!(rows_deleted, 2, "should delete both file-A chunk rows");
        assert_eq!(touched, vec![seg_id]);

        // File-A rows gone; file-B row preserved.
        let a_rows: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE file_hash = ?1",
                params![fh_a.as_slice()],
                |r| r.get(0),
            )
            .expect("count A");
        assert_eq!(a_rows, 0);

        let b_rows: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE file_hash = ?1",
                params![fh_b.as_slice()],
                |r| r.get(0),
            )
            .expect("count B");
        assert_eq!(b_rows, 1);

        // live_chunk_count dropped by exactly 2.
        let live: i64 = idx
            .conn
            .query_row(
                "SELECT live_chunk_count FROM segments WHERE segment_id = ?1",
                params![seg_id],
                |r| r.get(0),
            )
            .expect("read live_chunk_count");
        assert_eq!(live, 1, "one chunk still lives in the segment");

        // Segment is not yet a sweep candidate (file B still references it).
        assert!(idx.sweep_candidates().expect("sweep").is_empty());

        // Retiring file B drops the last chunk and makes the segment empty.
        let (b_deleted, _) = idx.delete_chunks_for_file(&fh_b).expect("delete B");
        assert_eq!(b_deleted, 1);

        let live_after: i64 = idx
            .conn
            .query_row(
                "SELECT live_chunk_count FROM segments WHERE segment_id = ?1",
                params![seg_id],
                |r| r.get(0),
            )
            .expect("read live_chunk_count");
        assert_eq!(live_after, 0);
        assert_eq!(
            idx.sweep_candidates().expect("sweep"),
            vec![seg_id],
            "empty sealed segment should be a sweep candidate"
        );
    }

    #[test]
    fn insert_chunks_for_file_replaces_live_counts_for_existing_file() {
        let idx = open_in_memory();
        let old_seg = idx.allocate_segment_id().expect("alloc old");
        let new_seg = idx.allocate_segment_id().expect("alloc new");

        let fh = test_hash(0xA1);
        insert_test_file(&idx, &fh, 100);

        let old_pending = vec![
            PendingRow {
                chunk_hash: test_hash(0xC1),
                file_hash: fh,
                chunk_index: 0,
                size: 50,
                segment_id: old_seg,
                segment_offset: 0,
            },
            PendingRow {
                chunk_hash: test_hash(0xC2),
                file_hash: fh,
                chunk_index: 1,
                size: 50,
                segment_id: old_seg,
                segment_offset: 58,
            },
        ];
        idx.insert_pending(&old_pending)
            .expect("insert old pending");
        idx.insert_chunks_for_file(&fh, &[(test_hash(0xC1), 50), (test_hash(0xC2), 50)])
            .expect("insert old chunks");
        idx.seal_segment(old_seg, 116).expect("seal old");

        let old_live_before: i64 = idx
            .conn
            .query_row(
                "SELECT live_chunk_count FROM segments WHERE segment_id = ?1",
                params![old_seg],
                |r| r.get(0),
            )
            .expect("read old live before");
        assert_eq!(old_live_before, 2);

        let new_pending = vec![PendingRow {
            chunk_hash: test_hash(0xC3),
            file_hash: fh,
            chunk_index: 0,
            size: 100,
            segment_id: new_seg,
            segment_offset: 0,
        }];
        idx.insert_pending(&new_pending)
            .expect("insert new pending");
        idx.insert_chunks_for_file(&fh, &[(test_hash(0xC3), 100)])
            .expect("replace chunks");

        let old_live_after: i64 = idx
            .conn
            .query_row(
                "SELECT live_chunk_count FROM segments WHERE segment_id = ?1",
                params![old_seg],
                |r| r.get(0),
            )
            .expect("read old live after");
        assert_eq!(
            old_live_after, 0,
            "replacement must release the old segment's live count"
        );

        let new_live_after: i64 = idx
            .conn
            .query_row(
                "SELECT live_chunk_count FROM segments WHERE segment_id = ?1",
                params![new_seg],
                |r| r.get(0),
            )
            .expect("read new live after");
        assert_eq!(new_live_after, 1);

        let old_rows: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE file_hash = ?1 AND segment_id = ?2",
                params![fh.as_slice(), old_seg],
                |r| r.get(0),
            )
            .expect("count old rows");
        assert_eq!(old_rows, 0);

        assert_eq!(
            idx.sweep_candidates().expect("sweep"),
            vec![old_seg],
            "the sealed replaced segment should no longer stay pinned"
        );
    }

    #[test]
    fn insert_file_updates_metadata_without_replacing_chunk_parent() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xA4);
        let chunk = test_hash(0xC4);

        idx.insert_file(&fh, 10).expect("insert file");
        idx.insert_pending(&[PendingRow {
            chunk_hash: chunk,
            file_hash: fh,
            chunk_index: 0,
            size: 10,
            segment_id: seg_id,
            segment_offset: 0,
        }])
        .expect("insert pending");
        idx.insert_chunks_for_file(&fh, &[(chunk, 10)])
            .expect("promote chunk");

        idx.insert_file(&fh, 12)
            .expect("metadata update should not disturb child chunks");

        let total_bytes: i64 = idx
            .conn
            .query_row(
                "SELECT total_bytes FROM files WHERE file_hash = ?1",
                params![&fh[..]],
                |r| r.get(0),
            )
            .expect("read total bytes");
        assert_eq!(total_bytes, 12);
        assert_eq!(
            idx.chunks_for_file_with_sizes(&fh).expect("chunks"),
            vec![(chunk, 10)]
        );
    }

    #[test]
    fn delete_chunks_for_file_missing_file_is_noop() {
        let idx = open_in_memory();
        let (rows, segs) = idx
            .delete_chunks_for_file(&test_hash(0xDE))
            .expect("delete missing");
        assert_eq!(rows, 0);
        assert!(segs.is_empty());
    }

    /// Regression: before this test, `delete_chunks_for_file` targeted
    /// only the `chunks` table, so a file whose rows still lived in
    /// `pending_chunks` (the common case — `flush_pending` does not
    /// promote rows across) left phantom chunks behind after
    /// `retire_file`. That made re-add collide with stale rows and
    /// either fail staging or produce truncated staging data.
    #[test]
    fn delete_chunks_for_file_also_clears_pending_rows() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");

        let fh = test_hash(0xA1);
        insert_test_file(&idx, &fh, 100);

        // File lives only in `pending_chunks` — the post-`flush_pending`
        // steady state for any file the clean filter or `crab add`
        // staged without ever being retired.
        let rows = vec![
            PendingRow {
                chunk_hash: test_hash(0xC1),
                file_hash: fh,
                chunk_index: 0,
                size: 50,
                segment_id: seg_id,
                segment_offset: 0,
            },
            PendingRow {
                chunk_hash: test_hash(0xC2),
                file_hash: fh,
                chunk_index: 1,
                size: 50,
                segment_id: seg_id,
                segment_offset: 58,
            },
        ];
        idx.insert_pending(&rows).expect("insert pending");

        let (rows_deleted, touched) = idx.delete_chunks_for_file(&fh).expect("delete");

        assert_eq!(rows_deleted, 2, "both pending rows must be deleted");
        assert_eq!(touched, vec![seg_id]);

        let pending_rows: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_chunks WHERE file_hash = ?1",
                params![fh.as_slice()],
                |r| r.get(0),
            )
            .expect("count pending");
        assert_eq!(
            pending_rows, 0,
            "retire must purge pending rows so re-add sees an empty slate"
        );

        let live: i64 = idx
            .conn
            .query_row(
                "SELECT live_chunk_count FROM segments WHERE segment_id = ?1",
                params![seg_id],
                |r| r.get(0),
            )
            .expect("read live_chunk_count");
        assert_eq!(
            live, 0,
            "pending-only rows must not affect live_chunk_count"
        );
    }

    #[test]
    fn delete_pending_rows_does_not_make_live_segment_sweepable() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");

        let file_a = test_hash(0xA1);
        let file_b = test_hash(0xB1);
        let chunk_a = test_hash(0xC1);
        let chunk_b = test_hash(0xC2);
        insert_test_file(&idx, &file_a, 50);
        insert_test_file(&idx, &file_b, 50);

        idx.insert_pending(&[PendingRow {
            chunk_hash: chunk_a,
            file_hash: file_a,
            chunk_index: 0,
            size: 50,
            segment_id: seg_id,
            segment_offset: 0,
        }])
        .expect("insert pending A");
        idx.flush_pending(seg_id, 58).expect("flush A");
        idx.insert_chunks_for_file(&file_a, &[(chunk_a, 50)])
            .expect("promote A");

        idx.insert_pending(&[PendingRow {
            chunk_hash: chunk_b,
            file_hash: file_b,
            chunk_index: 0,
            size: 50,
            segment_id: seg_id,
            segment_offset: 58,
        }])
        .expect("insert pending B");
        idx.flush_pending(seg_id, 116).expect("flush B");
        idx.seal_segment(seg_id, 116).expect("seal");

        let before_live: i64 = idx
            .conn
            .query_row(
                "SELECT live_chunk_count FROM segments WHERE segment_id = ?1",
                params![seg_id],
                |r| r.get(0),
            )
            .expect("read live before");
        assert_eq!(before_live, 1, "file A should pin the segment");

        let (rows_deleted, touched) = idx.delete_chunks_for_file(&file_b).expect("delete B");
        assert_eq!(rows_deleted, 1, "pending B row should be deleted");
        assert_eq!(touched, vec![seg_id]);

        let after_live: i64 = idx
            .conn
            .query_row(
                "SELECT live_chunk_count FROM segments WHERE segment_id = ?1",
                params![seg_id],
                |r| r.get(0),
            )
            .expect("read live after");
        assert_eq!(
            after_live, 1,
            "deleting pending B must not unpin committed file A"
        );
        assert!(
            idx.sweep_candidates().expect("sweep").is_empty(),
            "segment with committed file A must not become sweepable"
        );
    }

    #[test]
    fn chunks_for_file_with_sizes_reads_committed_chunks_in_order() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xA1);
        insert_test_file(&idx, &fh, 100);

        idx.conn
            .execute(
                "INSERT INTO chunks (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 1, 60, ?3, 0), (?4, ?2, 0, 40, ?3, 68)",
                params![
                    test_hash(0xC2).as_slice(),
                    fh.as_slice(),
                    seg_id,
                    test_hash(0xC1).as_slice()
                ],
            )
            .expect("insert committed chunks");

        let chunks = idx.chunks_for_file_with_sizes(&fh).expect("chunks");

        assert_eq!(chunks, vec![(test_hash(0xC1), 40), (test_hash(0xC2), 60)]);
    }

    #[test]
    fn chunks_for_file_with_sizes_reads_pending_chunks_in_order() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xA2);
        insert_test_file(&idx, &fh, 100);

        let rows = vec![
            PendingRow {
                chunk_hash: test_hash(0xD2),
                file_hash: fh,
                chunk_index: 1,
                size: 60,
                segment_id: seg_id,
                segment_offset: 0,
            },
            PendingRow {
                chunk_hash: test_hash(0xD1),
                file_hash: fh,
                chunk_index: 0,
                size: 40,
                segment_id: seg_id,
                segment_offset: 68,
            },
        ];
        idx.insert_pending(&rows).expect("insert pending");

        let chunks = idx.chunks_for_file_with_sizes(&fh).expect("chunks");

        assert_eq!(chunks, vec![(test_hash(0xD1), 40), (test_hash(0xD2), 60)]);
    }

    #[test]
    fn chunks_for_file_with_sizes_prefers_committed_row_for_duplicate_index() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xA3);
        insert_test_file(&idx, &fh, 100);

        idx.conn
            .execute(
                "INSERT INTO chunks (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 40, ?3, 0)",
                params![test_hash(0xE1).as_slice(), fh.as_slice(), seg_id],
            )
            .expect("insert committed chunk");
        idx.conn
            .execute(
                "INSERT INTO pending_chunks
                 (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 99, ?3, 48)",
                params![test_hash(0xEF).as_slice(), fh.as_slice(), seg_id],
            )
            .expect("insert duplicate pending chunk");

        let chunks = idx.chunks_for_file_with_sizes(&fh).expect("chunks");

        assert_eq!(chunks, vec![(test_hash(0xE1), 40)]);
    }

    #[test]
    fn chunks_for_file_with_locators_returns_selected_file_row_locator() {
        let idx = open_in_memory();
        let other_seg = idx.allocate_segment_id().expect("alloc other");
        let target_seg = idx.allocate_segment_id().expect("alloc target");
        let other_file = test_hash(0xA4);
        let target_file = test_hash(0xA5);
        let shared_chunk = test_hash(0xE2);
        insert_test_file(&idx, &other_file, 40);
        insert_test_file(&idx, &target_file, 40);

        idx.conn
            .execute(
                "INSERT INTO chunks (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 40, ?3, 11), (?1, ?4, 0, 40, ?5, 99)",
                params![
                    shared_chunk.as_slice(),
                    other_file.as_slice(),
                    other_seg,
                    target_file.as_slice(),
                    target_seg
                ],
            )
            .expect("insert duplicate hash rows");

        let chunks = idx
            .chunks_for_file_with_locators(&target_file)
            .expect("chunks");

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_hash, shared_chunk);
        assert_eq!(chunks[0].size, 40);
        assert_eq!(chunks[0].locator.segment_id, target_seg);
        assert_eq!(chunks[0].locator.offset, 99);
    }

    #[test]
    fn chunks_for_file_prefers_committed_row_for_duplicate_index() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xA4);
        insert_test_file(&idx, &fh, 100);

        idx.conn
            .execute(
                "INSERT INTO chunks (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 40, ?3, 0)",
                params![test_hash(0xF1).as_slice(), fh.as_slice(), seg_id],
            )
            .expect("insert committed chunk");
        idx.conn
            .execute(
                "INSERT INTO pending_chunks
                 (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 99, ?3, 48)",
                params![test_hash(0xFF).as_slice(), fh.as_slice(), seg_id],
            )
            .expect("insert duplicate pending chunk");

        let chunks = idx.chunks_for_file(&fh).expect("chunks");

        assert_eq!(chunks, vec![test_hash(0xF1)]);
    }

    #[test]
    fn chunks_for_file_rejects_malformed_chunk_hash_blob() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xA5);
        insert_test_file(&idx, &fh, 100);

        idx.conn
            .execute(
                "INSERT INTO chunks (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 40, ?3, 0)",
                params![&[0xC1_u8][..], fh.as_slice(), seg_id],
            )
            .expect("insert malformed chunk hash");

        let err = idx.chunks_for_file(&fh).expect_err("malformed hash");

        assert!(matches!(err, StagingError::StagingCorrupt(_)));
        assert!(err.to_string().contains("chunk hash has 1 bytes"));
    }

    #[test]
    fn chunks_for_file_rejects_chunk_index_gap() {
        let idx = open_in_memory();
        let seg_id = idx.allocate_segment_id().expect("alloc");
        let fh = test_hash(0xA6);
        insert_test_file(&idx, &fh, 100);

        idx.conn
            .execute(
                "INSERT INTO chunks (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 40, ?3, 0), (?4, ?2, 2, 60, ?3, 48)",
                params![
                    test_hash(0xB1).as_slice(),
                    fh.as_slice(),
                    seg_id,
                    test_hash(0xB2).as_slice()
                ],
            )
            .expect("insert chunks with index gap");

        let err = idx.chunks_for_file(&fh).expect_err("chunk index gap");
        assert!(matches!(err, StagingError::StagingCorrupt(_)));
        assert!(err.to_string().contains("expected 1, found 2"));

        let err = idx
            .chunks_for_file_with_sizes(&fh)
            .expect_err("chunk index gap");
        assert!(matches!(err, StagingError::StagingCorrupt(_)));
        assert!(err.to_string().contains("expected 1, found 2"));
    }

    #[test]
    fn chunks_for_file_with_sizes_missing_file_hash_is_empty() {
        let idx = open_in_memory();

        let chunks = idx
            .chunks_for_file_with_sizes(&test_hash(0xDD))
            .expect("chunks");

        assert!(chunks.is_empty());
    }
}
