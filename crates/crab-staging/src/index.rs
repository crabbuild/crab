//! SQLite index layer for the segment-based staging area.
//!
//! Manages the canonical schema, chunk locator lookups, segment registry,
//! and the pending-chunks flush protocol. The `Index` struct owns a
//! `rusqlite::Connection` in WAL mode with foreign key enforcement.

use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};
use std::collections::HashMap;
use std::path::Path;
use tracing::debug;

use crate::error::{Result, StagingError};

use super::segment::ChunkLocator;

type StoredRecipeRow = (Vec<u8>, i64, i64, Vec<u8>, i64, Vec<u8>, String);
type StoredRecipeMetadata = (i64, i64, Vec<u8>, i64, Vec<u8>, String);
type StoredRecipeWithHashRow = (Vec<u8>, i64, i64, Vec<u8>, i64, Vec<u8>, String, Vec<u8>);
type ResidualAuthorityRow = (i64, Option<Vec<u8>>, Option<Vec<u8>>, Option<i64>);

/// Canonical pre-release on-disk layout contract.
const LAYOUT_VERSION: &str = "1";

const CANONICAL_TABLES: &[&str] = &[
    "add_preparation_batches",
    "add_preparations",
    "chunk_payloads",
    "chunks",
    "file_paths",
    "file_recipes",
    "files",
    "path_heads",
    "path_leases",
    "pending_chunks",
    "preparation_payloads",
    "prepared_chunk_claims",
    "prepared_leases",
    "prepared_payload_chunks",
    "prepared_payloads",
    "publication_intent_entries",
    "publication_intents",
    "push_snapshot_leases",
    "push_snapshot_recipes",
    "push_snapshots",
    "recipe_occurrences",
    "recipe_pages",
    "recipe_payload_leases",
    "recipe_recording_terms",
    "recipe_remote_chunks",
    "recording_remote_chunks",
    "segments",
    "staging_batches",
    "staging_meta",
    "staging_quarantine",
    "verified_recipes",
];

const CANONICAL_INDEXES: &[&str] = &[
    "add_preparation_batches_by_batch",
    "chunks_by_hash",
    "chunks_by_segment",
    "leases_by_file",
    "path_heads_by_file",
    "pending_by_file",
    "pending_by_hash",
    "preparation_payloads_by_xorb",
    "prepared_claims_by_preparation",
    "publication_entries_by_batch",
    "recipe_occurrences_by_chunk",
    "recipe_payload_leases_by_chunk",
    "recipe_remote_chunks_by_hash",
    "recipes_by_file",
];

const CANONICAL_TRIGGERS: &[&str] = &["chunks_register_payload", "pending_chunks_register_payload"];

const PUBLICATION_INTENT_ENTRY_COLUMNS: &[&str] = &[
    "intent_id",
    "batch_id",
    "path_bytes",
    "recipe_hash",
    "expected_pointer_oid",
    "previous_index_state",
];

const REMOTE_CHUNK_COLUMNS: &[&str] = &[
    "batch_id",
    "chunk_hash",
    "xorb_hash",
    "chunk_index",
    "uncompressed_size",
    "placement_id",
    "origin_proof_id",
];

const RECIPE_REMOTE_CHUNK_COLUMNS: &[&str] = &[
    "recipe_hash",
    "chunk_hash",
    "xorb_hash",
    "chunk_index",
    "uncompressed_size",
    "placement_id",
    "origin_proof_id",
];

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
    pub xorb_hash: [u8; 32],
    pub payload_hash: [u8; 32],
    pub xorb_bytes: u64,
    pub chunk_index: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreparedChunkClaim {
    Prepared(PreparedChunkLocator),
    Segment,
    Claimed,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordingAuthorityState {
    Complete,
    Pending,
    Missing,
}

/// One disk-planned residual read group with caller-owned opaque context.
pub(crate) enum IndexedCoalescedReadGroup {
    Segments(Vec<(u64, FileChunkLocator)>),
    Prepared(Vec<(u64, [u8; 32], PreparedChunkLocator)>),
}

/// Prepared xorb candidate stored in the staging index.
#[derive(Debug, Clone)]
pub(crate) struct StoredPreparedXorb {
    pub xorb_hash: [u8; 32],
    pub payload_hash: [u8; 32],
    pub bytes: u64,
    pub placements: Vec<PreparedXorbPlacementWrite>,
}

/// Raw prepared xorb row used by diagnostics that must report corruption.
pub(crate) struct RawPreparedXorbRow {
    pub xorb_hash: Vec<u8>,
    pub payload_hash: Vec<u8>,
    pub bytes: i64,
}

pub(crate) struct PreparedXorbWrite {
    pub xorb_hash: [u8; 32],
    pub payload_hash: [u8; 32],
    pub bytes: u64,
    pub placements: Vec<PreparedXorbPlacementWrite>,
}

pub(crate) struct FilePushPlanWrite<'a> {
    pub file_hash: &'a [u8; 32],
    pub recipe_hash: &'a [u8; 32],
    pub recording_batch_id: Option<&'a str>,
    pub existing_chunks: &'a [ExistingChunkWrite],
    pub prepared_xorbs: &'a [PreparedXorbWrite],
}

pub(crate) struct ExistingChunkWrite {
    pub chunk_hash: [u8; 32],
    pub xorb_hash: [u8; 32],
    pub chunk_index: u32,
    pub uncompressed_size: u32,
    pub placement_id: [u8; 32],
    pub origin_proof_id: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreparedXorbPlacementWrite {
    pub chunk_hash: [u8; 32],
    pub chunk_index: u32,
    pub uncompressed_size: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct StoredPublicationIntentEntry {
    pub batch_id: String,
    pub path_bytes: Vec<u8>,
    pub recipe_hash: [u8; 32],
    pub expected_pointer_oid: String,
    pub previous_index_state: String,
}

#[derive(Debug, Clone)]
pub(crate) struct StoredPublicationIntent {
    pub intent_id: String,
    pub entries: Vec<StoredPublicationIntentEntry>,
}

pub(crate) type BatchDedupExisting = (usize, [u8; 32], ChunkLocator, bool);
pub(crate) type BatchDedupResult = (Vec<BatchDedupExisting>, Vec<usize>);

const PREPARED_XORB_QUERY_CHUNK_BATCH: usize = 500;

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

fn retired_staging_schema() -> StagingError {
    StagingError::StagingCorrupt(
        "staging schema is not canonical v1; remove .crab/staging and restage".to_owned(),
    )
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

fn publication_intent_batch_ids(
    tx: &rusqlite::Transaction<'_>,
    intent_id: &str,
) -> Result<Vec<String>> {
    let mut statement = tx
        .prepare_cached(
            "SELECT DISTINCT batch_id
             FROM publication_intent_entries
             WHERE intent_id = ?1
             ORDER BY batch_id",
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to prepare publication batches: {e}"))
        })?;
    statement
        .query_map(params![intent_id], |row| row.get(0))
        .map_err(|e| StagingError::Internal(format!("failed to query publication batches: {e}")))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| StagingError::Internal(format!("failed to collect publication batches: {e}")))
}

fn unowned_file_hashes(
    tx: &rusqlite::Transaction<'_>,
    mut candidates: Vec<[u8; 32]>,
) -> Result<Vec<[u8; 32]>> {
    candidates.sort_unstable();
    candidates.dedup();
    let mut unowned = Vec::new();
    for file_hash in candidates {
        let owned: bool = tx
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM path_leases WHERE file_hash = ?1
                 ) OR EXISTS(
                     SELECT 1
                     FROM push_snapshot_recipes AS pin
                     JOIN file_recipes AS recipe USING (recipe_hash)
                     WHERE recipe.file_hash = ?1
                 )",
                params![file_hash.as_slice()],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to inspect staged file ownership: {e}"))
            })?;
        if !owned {
            unowned.push(file_hash);
        }
    }
    Ok(unowned)
}

fn remove_empty_published_batches(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM staging_batches
         WHERE state = 'published'
           AND NOT EXISTS (
               SELECT 1 FROM path_leases
               WHERE path_leases.batch_id = staging_batches.batch_id
           )
           AND NOT EXISTS (
               SELECT 1 FROM publication_intent_entries
               WHERE publication_intent_entries.batch_id = staging_batches.batch_id
           )",
        [],
    )
    .map(|_| ())
    .map_err(|e| StagingError::Internal(format!("failed to remove superseded empty batches: {e}")))
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
    /// enforcement. Initializes a fresh canonical v1 schema or rejects any
    /// other shape with an explicit restage instruction.
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

        // Residual push schedules may contain millions of unique chunks.
        // Force connection-local TEMP tables onto disk so recipe scale cannot
        // turn SQLite's compile-time temp-store default into unbounded RAM.
        conn.pragma_update(None, "temp_store", "FILE")
            .map_err(|e| StagingError::Internal(format!("failed to set temp_store = FILE: {e}")))?;

        let mut idx = Self { conn };
        idx.open_canonical_schema()?;
        Ok(idx)
    }

    /// Open the staging index for a shared push handle.
    ///
    /// Validates the canonical schema before admitting the push, then enables
    /// WAL, foreign keys, and a busy timeout. The SQLite connection is read-write
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
        conn.pragma_update(None, "temp_store", "FILE")
            .map_err(|e| StagingError::Internal(format!("failed to set temp_store = FILE: {e}")))?;
        let mut index = Self { conn };
        index.open_canonical_schema()?;
        Ok(index)
    }

    fn open_canonical_schema(&mut self) -> Result<()> {
        let existing_tables = self.application_objects("table")?;
        let fresh = existing_tables.is_empty();
        if !fresh {
            if existing_tables
                .iter()
                .map(String::as_str)
                .ne(CANONICAL_TABLES.iter().copied())
            {
                return Err(retired_staging_schema());
            }
            let existing_indexes = self.application_objects("index")?;
            if existing_indexes
                .iter()
                .map(String::as_str)
                .ne(CANONICAL_INDEXES.iter().copied())
            {
                return Err(retired_staging_schema());
            }
            let existing_triggers = self.application_objects("trigger")?;
            if existing_triggers
                .iter()
                .map(String::as_str)
                .ne(CANONICAL_TRIGGERS.iter().copied())
            {
                return Err(retired_staging_schema());
            }
            if self
                .table_columns("publication_intent_entries")?
                .iter()
                .map(String::as_str)
                .ne(PUBLICATION_INTENT_ENTRY_COLUMNS.iter().copied())
            {
                return Err(retired_staging_schema());
            }
            if self
                .table_columns("recording_remote_chunks")?
                .iter()
                .map(String::as_str)
                .ne(REMOTE_CHUNK_COLUMNS.iter().copied())
                || self
                    .table_columns("recipe_remote_chunks")?
                    .iter()
                    .map(String::as_str)
                    .ne(RECIPE_REMOTE_CHUNK_COLUMNS.iter().copied())
            {
                return Err(retired_staging_schema());
            }
            let version: Option<String> = self
                .conn
                .query_row(
                    "SELECT value FROM staging_meta WHERE key = 'layout_version'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| {
                    StagingError::Internal(format!("failed to read staging layout version: {e}"))
                })?;
            if version.as_deref() != Some(LAYOUT_VERSION) {
                return Err(retired_staging_schema());
            }
        }

        self.initialize_schema()?;
        if fresh {
            self.conn
                .execute(
                    "INSERT INTO staging_meta (key, value) VALUES ('layout_version', ?1)",
                    params![LAYOUT_VERSION],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to seed staging layout version: {e}"))
                })?;
            debug!(
                version = LAYOUT_VERSION,
                "initialized canonical staging schema"
            );
        }
        Ok(())
    }

    fn application_objects(&self, kind: &str) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = ?1 AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to inspect staging schema: {e}"))
            })?;
        statement
            .query_map(params![kind], |row| row.get(0))
            .map_err(|e| StagingError::Internal(format!("failed to query staging schema: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| StagingError::Internal(format!("failed to collect staging schema: {e}")))
    }

    fn table_columns(&self, table: &str) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .map_err(|e| {
                StagingError::Internal(format!("failed to inspect staging table {table}: {e}"))
            })?;
        statement
            .query_map(params![table], |row| row.get(0))
            .map_err(|e| {
                StagingError::Internal(format!("failed to query staging table {table}: {e}"))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!("failed to collect staging table {table}: {e}"))
            })
    }

    fn initialize_schema(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS files (
                    file_hash    BLOB PRIMARY KEY,
                    shard_hash   BLOB,
                    total_bytes  INTEGER NOT NULL,
                    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS file_paths (
                    file_hash  BLOB PRIMARY KEY,
                    file_path  TEXT NOT NULL
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

                CREATE TABLE IF NOT EXISTS recording_remote_chunks (
                    batch_id           TEXT NOT NULL,
                    chunk_hash         BLOB NOT NULL,
                    xorb_hash          BLOB NOT NULL,
                    chunk_index        INTEGER NOT NULL,
                    uncompressed_size  INTEGER NOT NULL,
                    placement_id      BLOB NOT NULL,
                    origin_proof_id    BLOB NOT NULL,
                    PRIMARY KEY (batch_id, chunk_hash),
                    FOREIGN KEY (batch_id) REFERENCES staging_batches(batch_id)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS recipe_remote_chunks (
                    recipe_hash        BLOB NOT NULL,
                    chunk_hash         BLOB NOT NULL,
                    xorb_hash          BLOB NOT NULL,
                    chunk_index        INTEGER NOT NULL,
                    uncompressed_size  INTEGER NOT NULL,
                    placement_id      BLOB NOT NULL,
                    origin_proof_id    BLOB NOT NULL,
                    PRIMARY KEY (recipe_hash, chunk_hash),
                    FOREIGN KEY (recipe_hash) REFERENCES file_recipes(recipe_hash)
                        ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS recipe_remote_chunks_by_hash
                    ON recipe_remote_chunks(chunk_hash);

                CREATE TABLE IF NOT EXISTS staging_batches (
                    batch_id    TEXT PRIMARY KEY,
                    state       TEXT NOT NULL CHECK(state IN ('open', 'published')),
                    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS recipe_recording_terms (
                    batch_id      TEXT NOT NULL,
                    occurrence    INTEGER NOT NULL,
                    chunk_hash    BLOB NOT NULL,
                    chunk_offset  INTEGER NOT NULL,
                    chunk_size    INTEGER NOT NULL,
                    PRIMARY KEY (batch_id, occurrence),
                    FOREIGN KEY (batch_id) REFERENCES staging_batches(batch_id)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS file_recipes (
                    recipe_hash    BLOB PRIMARY KEY,
                    file_hash      BLOB NOT NULL,
                    file_size      INTEGER NOT NULL,
                    chunk_count    INTEGER NOT NULL,
                    sequence_hash  BLOB NOT NULL,
                    page_count     INTEGER NOT NULL,
                    page_root_hash BLOB NOT NULL,
                    policy_id      TEXT NOT NULL,
                    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
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

                CREATE INDEX IF NOT EXISTS recipe_occurrences_by_chunk
                    ON recipe_occurrences(recipe_hash, chunk_hash, chunk_size);

                CREATE TABLE IF NOT EXISTS recipe_pages (
                    recipe_hash      BLOB NOT NULL,
                    page_index       INTEGER NOT NULL,
                    start_occurrence INTEGER NOT NULL,
                    start_offset     INTEGER NOT NULL,
                    occurrence_count INTEGER NOT NULL,
                    page_bytes       INTEGER NOT NULL,
                    page_hash        BLOB NOT NULL,
                    PRIMARY KEY (recipe_hash, page_index),
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

                CREATE TABLE IF NOT EXISTS path_heads (
                    path_bytes   BLOB PRIMARY KEY,
                    batch_id     TEXT NOT NULL,
                    file_hash    BLOB NOT NULL,
                    recipe_hash  BLOB NOT NULL,
                    updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
                    FOREIGN KEY (batch_id, path_bytes)
                        REFERENCES path_leases(batch_id, path_bytes),
                    FOREIGN KEY (recipe_hash) REFERENCES file_recipes(recipe_hash)
                );

                CREATE INDEX IF NOT EXISTS path_heads_by_file
                    ON path_heads(file_hash);

                CREATE TABLE IF NOT EXISTS publication_intents (
                    intent_id   TEXT PRIMARY KEY,
                    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS publication_intent_entries (
                    intent_id             TEXT NOT NULL,
                    batch_id              TEXT NOT NULL,
                    path_bytes            BLOB NOT NULL,
                    recipe_hash           BLOB NOT NULL,
                    expected_pointer_oid  TEXT NOT NULL,
                    previous_index_state  TEXT NOT NULL,
                    PRIMARY KEY (intent_id, path_bytes),
                    FOREIGN KEY (intent_id) REFERENCES publication_intents(intent_id)
                        ON DELETE CASCADE,
                    FOREIGN KEY (batch_id, path_bytes)
                        REFERENCES path_leases(batch_id, path_bytes)
                        ON DELETE CASCADE,
                    FOREIGN KEY (recipe_hash) REFERENCES file_recipes(recipe_hash)
                );

                CREATE INDEX IF NOT EXISTS publication_entries_by_batch
                    ON publication_intent_entries(batch_id, path_bytes);

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

                CREATE TABLE IF NOT EXISTS add_preparations (
                    preparation_id TEXT PRIMARY KEY,
                    state          TEXT NOT NULL CHECK(state IN ('recording', 'sealing')),
                    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS add_preparation_batches (
                    preparation_id TEXT NOT NULL,
                    batch_id       TEXT NOT NULL UNIQUE,
                    PRIMARY KEY (preparation_id, batch_id),
                    FOREIGN KEY (preparation_id) REFERENCES add_preparations(preparation_id)
                        ON DELETE CASCADE,
                    FOREIGN KEY (batch_id) REFERENCES staging_batches(batch_id)
                        ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS add_preparation_batches_by_batch
                    ON add_preparation_batches(batch_id);

                CREATE TABLE IF NOT EXISTS prepared_chunk_claims (
                    chunk_hash        BLOB PRIMARY KEY,
                    preparation_id    TEXT NOT NULL,
                    owner_batch_id    TEXT NOT NULL,
                    uncompressed_size INTEGER NOT NULL,
                    FOREIGN KEY (preparation_id) REFERENCES add_preparations(preparation_id)
                        ON DELETE CASCADE,
                    FOREIGN KEY (owner_batch_id) REFERENCES staging_batches(batch_id)
                        ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS prepared_claims_by_preparation
                    ON prepared_chunk_claims(preparation_id, owner_batch_id);

                CREATE TABLE IF NOT EXISTS prepared_payloads (
                    xorb_hash    BLOB PRIMARY KEY,
                    payload_hash BLOB NOT NULL,
                    bytes        INTEGER NOT NULL,
                    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS prepared_payload_chunks (
                    xorb_hash          BLOB NOT NULL,
                    chunk_index        INTEGER NOT NULL,
                    chunk_hash         BLOB NOT NULL UNIQUE,
                    uncompressed_size  INTEGER NOT NULL,
                    PRIMARY KEY (xorb_hash, chunk_index),
                    FOREIGN KEY (xorb_hash) REFERENCES prepared_payloads(xorb_hash)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS preparation_payloads (
                    preparation_id TEXT NOT NULL,
                    xorb_hash      BLOB NOT NULL,
                    PRIMARY KEY (preparation_id, xorb_hash),
                    FOREIGN KEY (preparation_id) REFERENCES add_preparations(preparation_id)
                        ON DELETE CASCADE,
                    FOREIGN KEY (xorb_hash) REFERENCES prepared_payloads(xorb_hash)
                        ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS preparation_payloads_by_xorb
                    ON preparation_payloads(xorb_hash);

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
            .map_err(|e| StagingError::Internal(format!("schema initialization failed: {e}")))?;

        Ok(())
    }

    pub fn recipe_payload_validation_pending(&self) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM staging_meta
                     WHERE key = 'recipe_payload_validation_pending' AND value = '1'
                 )",
                [],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to inspect recipe payload validation: {e}"))
            })
    }

    pub fn pending_recipe_hashes(&self) -> Result<Vec<[u8; 32]>> {
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

    pub fn pending_recipe(&self, recipe_hash: &[u8; 32]) -> Result<crate::recipe::FileRecipe> {
        let (
            raw_file_hash,
            raw_file_size,
            raw_chunk_count,
            raw_sequence_hash,
            raw_page_count,
            raw_page_root_hash,
            policy_id,
        ): StoredRecipeRow = self
            .conn
            .query_row(
                "SELECT file_hash, file_size, chunk_count, sequence_hash,
                        page_count, page_root_hash, policy_id
                 FROM file_recipes
                 WHERE recipe_hash = ?1",
                params![recipe_hash.as_slice()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| StagingError::Internal(format!("failed to load unverified recipe: {e}")))?
            .ok_or_else(|| {
                StagingError::StagingCorrupt("unverified recipe disappeared".to_owned())
            })?;
        let file_hash = decode_hash_blob("unverified recipe file hash", raw_file_hash)?;
        self.load_stored_recipe(
            recipe_hash,
            &file_hash,
            (
                raw_file_size,
                raw_chunk_count,
                raw_sequence_hash,
                raw_page_count,
                raw_page_root_hash,
                policy_id,
            ),
        )
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

    pub fn quarantine_pending_recipe(&self, recipe_hash: &[u8; 32], reason: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin recipe quarantine: {e}"))
        })?;
        tx.execute(
            "DELETE FROM path_heads
             WHERE recipe_hash = ?1",
            params![recipe_hash.as_slice()],
        )
        .map_err(|e| StagingError::Internal(format!("failed to hide corrupt recipe head: {e}")))?;
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

    pub fn finish_recipe_payload_validation(&self) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM staging_meta WHERE key = 'recipe_payload_validation_pending'",
                [],
            )
            .map(|_| ())
            .map_err(|e| {
                StagingError::Internal(format!("failed to finish recipe payload validation: {e}"))
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

    pub fn insert_add_preparation(&self, preparation_id: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO add_preparations (preparation_id, state)
                 VALUES (?1, 'recording')",
                params![preparation_id],
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to create add preparation: {error}"))
            })?;
        Ok(())
    }

    pub fn attach_add_preparation_batch(&self, preparation_id: &str, batch_id: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO add_preparation_batches (preparation_id, batch_id)
                 SELECT ?1, ?2
                 WHERE EXISTS (
                           SELECT 1 FROM add_preparations
                           WHERE preparation_id = ?1 AND state = 'recording'
                       )
                   AND EXISTS (
                           SELECT 1 FROM staging_batches
                           WHERE batch_id = ?2 AND state = 'open'
                       )",
                params![preparation_id, batch_id],
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to attach batch to add preparation: {error}"
                ))
            })
            .and_then(|inserted| {
                if inserted == 1 {
                    Ok(())
                } else {
                    Err(StagingError::NotFound {
                        path: format!("recording add preparation {preparation_id}/{batch_id}"),
                    })
                }
            })
    }

    pub fn claim_prepared_chunks(
        &self,
        preparation_id: &str,
        batch_id: &str,
        chunks: &[([u8; 32], u64)],
    ) -> Result<Vec<PreparedChunkClaim>> {
        if chunks.len() > super::stream::STAGE_BATCH_CHUNKS {
            return Err(StagingError::StagingCorrupt(format!(
                "prepared claim batch has {} terms, limit is {}",
                chunks.len(),
                super::stream::STAGE_BATCH_CHUNKS
            )));
        }
        let tx = self.conn.unchecked_transaction().map_err(|error| {
            StagingError::Internal(format!("failed to begin prepared claim batch: {error}"))
        })?;
        let valid_owner: bool = tx
            .query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM add_preparations AS preparation
                     JOIN add_preparation_batches AS member USING (preparation_id)
                     JOIN staging_batches AS batch USING (batch_id)
                     WHERE preparation.preparation_id = ?1
                       AND preparation.state = 'recording'
                       AND member.batch_id = ?2
                       AND batch.state = 'open'
                 )",
                params![preparation_id, batch_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to validate prepared claim owner: {error}"))
            })?;
        if !valid_owner {
            return Err(StagingError::NotFound {
                path: format!("recording add preparation {preparation_id}/{batch_id}"),
            });
        }
        if chunks.is_empty() {
            tx.commit().map_err(|error| {
                StagingError::Internal(format!("failed to commit empty claim batch: {error}"))
            })?;
            return Ok(Vec::new());
        }

        let mut sizes = HashMap::<[u8; 32], u64>::with_capacity(chunks.len());
        for (chunk_hash, size) in chunks {
            if let Some(existing) = sizes.insert(*chunk_hash, *size)
                && existing != *size
            {
                return Err(StagingError::StagingCorrupt(format!(
                    "chunk {} has conflicting claim sizes {existing} and {size}",
                    crab_xet::hash::MerkleHash::from(*chunk_hash).hex()
                )));
            }
        }
        let unique_hashes = sizes.keys().copied().collect::<Vec<_>>();
        let placeholders = vec!["?"; unique_hashes.len()].join(",");

        let mut prepared = HashMap::<[u8; 32], PreparedChunkLocator>::new();
        {
            let sql = format!(
                "SELECT chunk.chunk_hash, chunk.xorb_hash, payload.payload_hash,
                        payload.bytes, chunk.chunk_index, chunk.uncompressed_size
                 FROM prepared_payload_chunks AS chunk
                 JOIN prepared_payloads AS payload USING (xorb_hash)
                 WHERE chunk.chunk_hash IN ({placeholders})"
            );
            let mut statement = tx.prepare(&sql).map_err(|error| {
                StagingError::Internal(format!(
                    "failed to prepare canonical authority batch: {error}"
                ))
            })?;
            let rows = statement
                .query_map(
                    params_from_iter(unique_hashes.iter().map(|hash| hash.as_slice())),
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    },
                )
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to query canonical authority batch: {error}"
                    ))
                })?;
            for row in rows {
                let (chunk_hash, xorb_hash, payload_hash, bytes, chunk_index, size) =
                    row.map_err(|error| {
                        StagingError::Internal(format!(
                            "failed to read canonical authority batch: {error}"
                        ))
                    })?;
                let chunk_hash = decode_hash_blob("prepared claim chunk hash", chunk_hash)?;
                prepared.insert(
                    chunk_hash,
                    PreparedChunkLocator {
                        xorb_hash: decode_hash_blob("prepared claim xorb hash", xorb_hash)?,
                        payload_hash: decode_hash_blob(
                            "prepared claim payload hash",
                            payload_hash,
                        )?,
                        xorb_bytes: nonnegative_count("prepared claim payload bytes", bytes)?,
                        chunk_index: u32::try_from(chunk_index).map_err(|_| {
                            StagingError::StagingCorrupt(
                                "prepared claim chunk index is invalid".to_owned(),
                            )
                        })?,
                        size: u32::try_from(size).map_err(|_| {
                            StagingError::StagingCorrupt(
                                "prepared claim chunk size is invalid".to_owned(),
                            )
                        })?,
                    },
                );
            }
        }

        let mut segments = HashMap::<[u8; 32], u64>::new();
        {
            let sql = format!(
                "SELECT chunk_hash, size FROM chunk_payloads
                 WHERE chunk_hash IN ({placeholders})"
            );
            let mut statement = tx.prepare(&sql).map_err(|error| {
                StagingError::Internal(format!(
                    "failed to prepare segment authority batch: {error}"
                ))
            })?;
            let rows = statement
                .query_map(
                    params_from_iter(unique_hashes.iter().map(|hash| hash.as_slice())),
                    |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
                )
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to query segment authority batch: {error}"
                    ))
                })?;
            for row in rows {
                let (chunk_hash, size) = row.map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to read segment authority batch: {error}"
                    ))
                })?;
                segments.insert(
                    decode_hash_blob("segment authority chunk hash", chunk_hash)?,
                    nonnegative_count("segment authority chunk size", size)?,
                );
            }
        }

        let mut claims = HashMap::<[u8; 32], (String, u64)>::new();
        {
            let sql = format!(
                "SELECT chunk_hash, preparation_id, uncompressed_size
                 FROM prepared_chunk_claims WHERE chunk_hash IN ({placeholders})"
            );
            let mut statement = tx.prepare(&sql).map_err(|error| {
                StagingError::Internal(format!("failed to prepare existing claim batch: {error}"))
            })?;
            let rows = statement
                .query_map(
                    params_from_iter(unique_hashes.iter().map(|hash| hash.as_slice())),
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .map_err(|error| {
                    StagingError::Internal(format!("failed to query existing claim batch: {error}"))
                })?;
            for row in rows {
                let (chunk_hash, stored_preparation, size) = row.map_err(|error| {
                    StagingError::Internal(format!("failed to read existing claim batch: {error}"))
                })?;
                claims.insert(
                    decode_hash_blob("existing claim chunk hash", chunk_hash)?,
                    (
                        stored_preparation,
                        nonnegative_count("existing claim chunk size", size)?,
                    ),
                );
            }
        }

        let mut out = Vec::with_capacity(chunks.len());
        for (chunk_hash, size) in chunks {
            if let Some(locator) = prepared.get(chunk_hash) {
                if u64::from(locator.size) != *size {
                    return Err(StagingError::StagingCorrupt(format!(
                        "prepared chunk {} has conflicting sizes {} and {size}",
                        crab_xet::hash::MerkleHash::from(*chunk_hash).hex(),
                        locator.size
                    )));
                }
                out.push(PreparedChunkClaim::Prepared(*locator));
                continue;
            }
            if let Some(stored_size) = segments.get(chunk_hash) {
                if *stored_size != *size {
                    return Err(StagingError::StagingCorrupt(format!(
                        "segment chunk {} has conflicting sizes {stored_size} and {size}",
                        crab_xet::hash::MerkleHash::from(*chunk_hash).hex()
                    )));
                }
                out.push(PreparedChunkClaim::Segment);
                continue;
            }
            if let Some((stored_preparation, stored_size)) = claims.get(chunk_hash) {
                if stored_preparation != preparation_id || *stored_size != *size {
                    return Err(StagingError::StagingCorrupt(format!(
                        "prepared chunk {} has a conflicting ownership claim",
                        crab_xet::hash::MerkleHash::from(*chunk_hash).hex()
                    )));
                }
                out.push(PreparedChunkClaim::Pending);
                continue;
            }

            tx.execute(
                "INSERT INTO prepared_chunk_claims
                 (chunk_hash, preparation_id, owner_batch_id, uncompressed_size)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    chunk_hash.as_slice(),
                    preparation_id,
                    batch_id,
                    sqlite_i64("prepared claim size", *size)?,
                ],
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to claim prepared chunk: {error}"))
            })?;
            claims.insert(*chunk_hash, (preparation_id.to_owned(), *size));
            out.push(PreparedChunkClaim::Claimed);
        }
        tx.commit().map_err(|error| {
            StagingError::Internal(format!("failed to commit prepared claim batch: {error}"))
        })?;
        Ok(out)
    }

    pub fn register_preparation_payloads(
        &self,
        preparation_id: &str,
        owner_batch_id: &str,
        payloads: &[PreparedXorbWrite],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|error| {
            StagingError::Internal(format!("failed to begin prepared payload seal: {error}"))
        })?;
        let recording: bool = tx
            .query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM add_preparations AS preparation
                     JOIN add_preparation_batches AS member USING (preparation_id)
                     JOIN staging_batches AS batch USING (batch_id)
                     WHERE preparation.preparation_id = ?1
                       AND preparation.state = 'recording'
                       AND member.batch_id = ?2
                       AND batch.state = 'open'
                 )",
                params![preparation_id, owner_batch_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to inspect add preparation: {error}"))
            })?;
        if !recording {
            return Err(StagingError::NotFound {
                path: format!("recording add preparation {preparation_id}/{owner_batch_id}"),
            });
        }

        for payload in payloads {
            let xorb_hash = payload.xorb_hash.as_slice();
            let payload_hash = payload.payload_hash.as_slice();
            let bytes = sqlite_i64("prepared payload bytes", payload.bytes)?;
            tx.execute(
                "INSERT OR IGNORE INTO prepared_payloads
                 (xorb_hash, payload_hash, bytes) VALUES (?1, ?2, ?3)",
                params![xorb_hash, payload_hash, bytes],
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to register prepared payload: {error}"))
            })?;
            let matches: bool = tx
                .query_row(
                    "SELECT payload_hash = ?2 AND bytes = ?3
                     FROM prepared_payloads WHERE xorb_hash = ?1",
                    params![xorb_hash, payload_hash, bytes],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    StagingError::Internal(format!("failed to verify prepared payload: {error}"))
                })?;
            if !matches {
                return Err(StagingError::StagingCorrupt(format!(
                    "prepared payload identity collision for {}",
                    crab_xet::hash::MerkleHash::from(payload.xorb_hash).hex()
                )));
            }
            tx.execute(
                "INSERT OR IGNORE INTO preparation_payloads (preparation_id, xorb_hash)
                 VALUES (?1, ?2)",
                params![preparation_id, xorb_hash],
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to retain preparation payload: {error}"))
            })?;

            for placement in &payload.placements {
                let claim: Option<(String, String, i64)> = tx
                    .query_row(
                        "SELECT preparation_id, owner_batch_id, uncompressed_size
                         FROM prepared_chunk_claims WHERE chunk_hash = ?1",
                        params![placement.chunk_hash.as_slice()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(|error| {
                        StagingError::Internal(format!("failed to inspect payload claim: {error}"))
                    })?;
                let Some((claim_preparation, claim_owner, claim_size)) = claim else {
                    return Err(StagingError::StagingCorrupt(format!(
                        "prepared payload chunk {} has no ownership claim",
                        crab_xet::hash::MerkleHash::from(placement.chunk_hash).hex()
                    )));
                };
                if claim_preparation != preparation_id
                    || claim_owner != owner_batch_id
                    || claim_size != i64::from(placement.uncompressed_size)
                {
                    return Err(StagingError::StagingCorrupt(format!(
                        "prepared payload chunk {} escaped its ownership claim",
                        crab_xet::hash::MerkleHash::from(placement.chunk_hash).hex()
                    )));
                }
                tx.execute(
                    "INSERT INTO prepared_payload_chunks
                     (xorb_hash, chunk_index, chunk_hash, uncompressed_size)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        xorb_hash,
                        i64::from(placement.chunk_index),
                        placement.chunk_hash.as_slice(),
                        i64::from(placement.uncompressed_size),
                    ],
                )
                .map_err(|error| {
                    StagingError::StagingCorrupt(format!(
                        "failed to install canonical prepared placement for {}: {error}",
                        crab_xet::hash::MerkleHash::from(placement.chunk_hash).hex()
                    ))
                })?;
                tx.execute(
                    "DELETE FROM prepared_chunk_claims WHERE chunk_hash = ?1",
                    params![placement.chunk_hash.as_slice()],
                )
                .map_err(|error| {
                    StagingError::Internal(format!("failed to resolve prepared claim: {error}"))
                })?;
            }
        }
        tx.commit().map_err(|error| {
            StagingError::Internal(format!("failed to commit prepared payload seal: {error}"))
        })?;
        Ok(())
    }

    pub fn recording_authority_state(&self, batch_id: &str) -> Result<RecordingAuthorityState> {
        let missing: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*)
                 FROM recipe_recording_terms AS term
                 WHERE term.batch_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM recording_remote_chunks AS remote
                       WHERE remote.batch_id = term.batch_id
                         AND remote.chunk_hash = term.chunk_hash
                         AND remote.uncompressed_size = term.chunk_size
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM chunk_payloads AS payload
                       WHERE payload.chunk_hash = term.chunk_hash
                         AND payload.size = term.chunk_size
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM prepared_payload_chunks AS prepared
                       WHERE prepared.chunk_hash = term.chunk_hash
                         AND prepared.uncompressed_size = term.chunk_size
                   )",
                params![batch_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to inspect recording authority: {error}"))
            })?;
        if missing == 0 {
            return Ok(RecordingAuthorityState::Complete);
        }
        let pending: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM recipe_recording_terms AS term
                     JOIN prepared_chunk_claims AS claim USING (chunk_hash)
                     WHERE term.batch_id = ?1
                       AND claim.uncompressed_size = term.chunk_size
                 )",
                params![batch_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to inspect pending authority: {error}"))
            })?;
        Ok(if pending {
            RecordingAuthorityState::Pending
        } else {
            RecordingAuthorityState::Missing
        })
    }

    pub fn finalize_add_preparation(&self, preparation_id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|error| {
            StagingError::Internal(format!("failed to begin preparation finalization: {error}"))
        })?;
        let changed = tx
            .execute(
                "UPDATE add_preparations SET state = 'sealing'
                 WHERE preparation_id = ?1 AND state = 'recording'",
                params![preparation_id],
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to seal add preparation: {error}"))
            })?;
        if changed != 1 {
            return Err(StagingError::NotFound {
                path: format!("recording add preparation {preparation_id}"),
            });
        }
        let unresolved: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM prepared_chunk_claims WHERE preparation_id = ?1",
                params![preparation_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to count unresolved claims: {error}"))
            })?;
        if unresolved != 0 {
            return Err(StagingError::StagingCorrupt(format!(
                "add preparation {preparation_id} has {unresolved} unresolved chunk claims"
            )));
        }
        let member_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM add_preparation_batches WHERE preparation_id = ?1",
                params![preparation_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to count preparation member batches: {error}"
                ))
            })?;
        if member_count == 0 {
            return Err(StagingError::StagingCorrupt(format!(
                "add preparation {preparation_id} has no member batches"
            )));
        }
        let incomplete_batch: Option<String> = tx
            .query_row(
                "SELECT member.batch_id
                 FROM add_preparation_batches AS member
                 LEFT JOIN staging_batches AS batch USING (batch_id)
                 WHERE member.preparation_id = ?1
                   AND (batch.state IS NULL
                        OR batch.state != 'open'
                        OR (SELECT COUNT(*) FROM path_leases AS lease
                            WHERE lease.batch_id = member.batch_id) != 1)
                 LIMIT 1",
                params![preparation_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to validate preparation member recipes: {error}"
                ))
            })?;
        if let Some(batch_id) = incomplete_batch {
            return Err(StagingError::StagingCorrupt(format!(
                "add preparation {preparation_id} member batch {batch_id} has no sealed recipe lease"
            )));
        }
        let unleased: Option<Vec<u8>> = tx
            .query_row(
                "SELECT payload.xorb_hash
                 FROM preparation_payloads AS payload
                 WHERE payload.preparation_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM prepared_leases AS lease
                       WHERE lease.xorb_hash = payload.xorb_hash
                   )
                 LIMIT 1",
                params![preparation_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                StagingError::Internal(format!("failed to validate preparation leases: {error}"))
            })?;
        if let Some(xorb_hash) = unleased {
            return Err(StagingError::StagingCorrupt(format!(
                "prepared payload {} has no sealed recipe lease",
                crab_xet::hash::MerkleHash::from(decode_hash_blob(
                    "unleased prepared payload",
                    xorb_hash,
                )?)
                .hex()
            )));
        }
        tx.execute(
            "DELETE FROM add_preparations WHERE preparation_id = ?1",
            params![preparation_id],
        )
        .map_err(|error| {
            StagingError::Internal(format!("failed to retire add preparation: {error}"))
        })?;
        tx.commit().map_err(|error| {
            StagingError::Internal(format!(
                "failed to commit preparation finalization: {error}"
            ))
        })?;
        Ok(())
    }

    pub fn abort_add_preparation(&self, preparation_id: &str) -> Result<Vec<[u8; 32]>> {
        let tx = self.conn.unchecked_transaction().map_err(|error| {
            StagingError::Internal(format!("failed to begin preparation abort: {error}"))
        })?;
        let payloads = {
            let mut statement = tx
                .prepare_cached(
                    "SELECT xorb_hash FROM preparation_payloads
                     WHERE preparation_id = ?1 ORDER BY xorb_hash",
                )
                .map_err(|error| {
                    StagingError::Internal(format!("failed to prepare abort payloads: {error}"))
                })?;
            statement
                .query_map(params![preparation_id], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|error| {
                    StagingError::Internal(format!("failed to query abort payloads: {error}"))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    StagingError::Internal(format!("failed to collect abort payloads: {error}"))
                })?
        };
        tx.execute(
            "DELETE FROM add_preparations WHERE preparation_id = ?1",
            params![preparation_id],
        )
        .map_err(|error| {
            StagingError::Internal(format!("failed to delete add preparation: {error}"))
        })?;
        let mut removed = Vec::new();
        for raw_hash in payloads {
            let hash = decode_hash_blob("aborted preparation payload", raw_hash)?;
            let deleted = tx
                .execute(
                    "DELETE FROM prepared_payloads
                     WHERE xorb_hash = ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM prepared_leases
                           WHERE prepared_leases.xorb_hash = prepared_payloads.xorb_hash
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM preparation_payloads
                           WHERE preparation_payloads.xorb_hash = prepared_payloads.xorb_hash
                       )",
                    params![hash.as_slice()],
                )
                .map_err(|error| {
                    StagingError::Internal(format!("failed to delete aborted payload: {error}"))
                })?;
            if deleted == 1 {
                removed.push(hash);
            }
        }
        tx.commit().map_err(|error| {
            StagingError::Internal(format!("failed to commit preparation abort: {error}"))
        })?;
        Ok(removed)
    }

    pub fn abort_all_add_preparations(&self) -> Result<Vec<[u8; 32]>> {
        let (ids, batches) = {
            let mut statement = self
                .conn
                .prepare(
                    "SELECT preparation.preparation_id, member.batch_id
                     FROM add_preparations AS preparation
                     LEFT JOIN add_preparation_batches AS member
                       USING (preparation_id)
                     ORDER BY preparation.preparation_id, member.batch_id",
                )
                .map_err(|error| {
                    StagingError::Internal(format!("failed to prepare stale preparations: {error}"))
                })?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(|error| {
                    StagingError::Internal(format!("failed to query stale preparations: {error}"))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    StagingError::Internal(format!("failed to collect stale preparations: {error}"))
                })?;
            let ids = rows
                .iter()
                .map(|(preparation_id, _)| preparation_id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let batches = rows
                .into_iter()
                .filter_map(|(_, batch_id)| batch_id)
                .collect::<std::collections::BTreeSet<_>>();
            (ids, batches)
        };
        let mut removed = Vec::new();
        for id in ids {
            removed.extend(self.abort_add_preparation(&id)?);
        }
        let mut unleased_files = std::collections::BTreeSet::new();
        for batch_id in batches {
            unleased_files.extend(self.rollback_batch(&batch_id)?);
        }
        for file_hash in unleased_files {
            let (_, payloads) = self.remove_file(&file_hash)?;
            removed.extend(payloads);
        }
        removed.sort_unstable();
        removed.dedup();
        Ok(removed)
    }

    /// Append one bounded contiguous term batch to an open recipe recording.
    pub fn append_recipe_recording_terms(
        &self,
        batch_id: &str,
        start_occurrence: u64,
        start_offset: u64,
        chunks: &[(crab_xet::hash::MerkleHash, u64)],
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        if chunks.len() > super::stream::STAGE_BATCH_CHUNKS {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe recording append has {} terms, limit is {}",
                chunks.len(),
                super::stream::STAGE_BATCH_CHUNKS
            )));
        }
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin recipe recording append: {e}"))
        })?;
        let batch_is_open: bool = tx
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM staging_batches
                    WHERE batch_id = ?1 AND state = 'open'
                 )",
                params![batch_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to inspect recipe recording batch: {e}"))
            })?;
        if !batch_is_open {
            return Err(StagingError::NotFound {
                path: format!("open staging batch {batch_id}"),
            });
        }
        let (stored_count, stored_end): (i64, Option<i64>) = tx
            .query_row(
                "SELECT COUNT(*), MAX(chunk_offset + chunk_size)
                 FROM recipe_recording_terms WHERE batch_id = ?1",
                params![batch_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to inspect recipe recording tail: {e}"))
            })?;
        let expected_occurrence = nonnegative_count("recipe recording count", stored_count)?;
        let expected_offset = stored_end
            .map(|value| nonnegative_count("recipe recording offset", value))
            .transpose()?
            .unwrap_or(0);
        if start_occurrence != expected_occurrence || start_offset != expected_offset {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe recording append is not contiguous: expected occurrence {expected_occurrence} offset {expected_offset}, found occurrence {start_occurrence} offset {start_offset}"
            )));
        }

        let mut statement = tx
            .prepare_cached(
                "INSERT INTO recipe_recording_terms
                 (batch_id, occurrence, chunk_hash, chunk_offset, chunk_size)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to prepare recipe recording insert: {e}"))
            })?;
        let mut occurrence = start_occurrence;
        let mut offset = start_offset;
        for (chunk_hash, size) in chunks {
            let raw_hash: [u8; 32] = (*chunk_hash).into();
            statement
                .execute(params![
                    batch_id,
                    sqlite_i64("recipe recording occurrence", occurrence)?,
                    raw_hash.as_slice(),
                    sqlite_i64("recipe recording offset", offset)?,
                    sqlite_i64("recipe recording size", *size)?,
                ])
                .map_err(|e| {
                    StagingError::Internal(format!("failed to append recipe recording term: {e}"))
                })?;
            occurrence = occurrence.checked_add(1).ok_or_else(|| {
                StagingError::StagingCorrupt("recipe recording occurrence overflow".to_owned())
            })?;
            offset = offset.checked_add(*size).ok_or_else(|| {
                StagingError::StagingCorrupt("recipe recording byte offset overflow".to_owned())
            })?;
        }
        drop(statement);
        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit recipe recording append: {e}"))
        })?;
        Ok(())
    }

    /// Append a bounded set of generation-pinned remote payload authorities.
    pub fn append_recording_remote_chunks(
        &self,
        batch_id: &str,
        chunks: &[ExistingChunkWrite],
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        if chunks.len() > super::stream::STAGE_BATCH_CHUNKS {
            return Err(StagingError::StagingCorrupt(format!(
                "remote authority append has {} terms, limit is {}",
                chunks.len(),
                super::stream::STAGE_BATCH_CHUNKS
            )));
        }
        let tx = self.conn.unchecked_transaction().map_err(|error| {
            StagingError::Internal(format!("failed to begin remote authority append: {error}"))
        })?;
        let batch_is_open: bool = tx
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM staging_batches
                    WHERE batch_id = ?1 AND state = 'open'
                 )",
                params![batch_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to inspect remote authority batch: {error}"))
            })?;
        if !batch_is_open {
            return Err(StagingError::NotFound {
                path: format!("open staging batch {batch_id}"),
            });
        }
        for chunk in chunks {
            if chunk.placement_id == [0; 32] || chunk.origin_proof_id == [0; 32] {
                return Err(StagingError::StagingCorrupt(
                    "remote authority has an empty placement or origin proof id".to_owned(),
                ));
            }
            tx.execute(
                "INSERT OR IGNORE INTO recording_remote_chunks
                 (batch_id, chunk_hash, xorb_hash, chunk_index,
                  uncompressed_size, placement_id, origin_proof_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    batch_id,
                    chunk.chunk_hash.as_slice(),
                    chunk.xorb_hash.as_slice(),
                    i64::from(chunk.chunk_index),
                    i64::from(chunk.uncompressed_size),
                    chunk.placement_id.as_slice(),
                    chunk.origin_proof_id.as_slice(),
                ],
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to append recording remote authority: {error}"
                ))
            })?;
            let matches: bool = tx
                .query_row(
                    "SELECT xorb_hash = ?3
                            AND chunk_index = ?4
                            AND uncompressed_size = ?5
                            AND placement_id = ?6
                            AND origin_proof_id = ?7
                     FROM recording_remote_chunks
                     WHERE batch_id = ?1 AND chunk_hash = ?2",
                    params![
                        batch_id,
                        chunk.chunk_hash.as_slice(),
                        chunk.xorb_hash.as_slice(),
                        i64::from(chunk.chunk_index),
                        i64::from(chunk.uncompressed_size),
                        chunk.placement_id.as_slice(),
                        chunk.origin_proof_id.as_slice(),
                    ],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to verify recording remote authority: {error}"
                    ))
                })?;
            if !matches {
                return Err(StagingError::StagingCorrupt(format!(
                    "remote authority for chunk {} changed within one add",
                    crab_xet::hash::MerkleHash::from(chunk.chunk_hash).hex()
                )));
            }
        }
        tx.commit().map_err(|error| {
            StagingError::Internal(format!("failed to commit remote authority append: {error}"))
        })
    }

    pub fn mark_batch_published(&self, batch_id: &str) -> Result<Vec<[u8; 32]>> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin staging publication: {e}"))
        })?;
        let superseded_batches = {
            let mut statement = tx
                .prepare_cached(
                    "SELECT DISTINCT lease.batch_id
                     FROM path_leases AS lease
                     WHERE lease.batch_id != ?1
                       AND EXISTS (
                           SELECT 1 FROM path_leases AS incoming
                           WHERE incoming.batch_id = ?1
                             AND incoming.path_bytes = lease.path_bytes
                       )",
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to prepare superseded staging batches: {e}"
                    ))
                })?;
            statement
                .query_map(params![batch_id], |row| row.get::<_, String>(0))
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to query superseded staging batches: {e}"
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to collect superseded staging batch: {e}"
                    ))
                })?
        };
        let candidates = {
            let mut statement = tx
                .prepare_cached(
                    "SELECT DISTINCT lease.file_hash
                     FROM path_leases AS lease
                     WHERE EXISTS (
                         SELECT 1 FROM path_leases AS incoming
                         WHERE incoming.batch_id = ?1
                           AND incoming.path_bytes = lease.path_bytes
                     )",
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to prepare superseded publication owners: {e}"
                    ))
                })?;
            statement
                .query_map(params![batch_id], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to query superseded publication owners: {e}"
                    ))
                })?
                .map(|row| {
                    row.map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to collect superseded publication owner: {e}"
                        ))
                    })
                    .and_then(|hash| decode_hash_blob("published file hash", hash))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let changed = tx
            .execute(
                "UPDATE staging_batches SET state = 'published'
                 WHERE batch_id = ?1 AND state = 'open'",
                params![batch_id],
            )
            .map_err(|e| StagingError::Internal(format!("failed to publish staging batch: {e}")))?;
        if changed != 1 {
            return Err(StagingError::NotFound {
                path: format!("staging batch {batch_id}"),
            });
        }
        tx.execute(
            "INSERT INTO path_heads (path_bytes, batch_id, file_hash, recipe_hash)
             SELECT path_bytes, batch_id, file_hash, recipe_hash
             FROM path_leases WHERE batch_id = ?1
             ON CONFLICT(path_bytes) DO UPDATE SET
                batch_id = excluded.batch_id,
                file_hash = excluded.file_hash,
                recipe_hash = excluded.recipe_hash,
                updated_at = datetime('now')",
            params![batch_id],
        )
        .map_err(|e| StagingError::Internal(format!("failed to replace path heads: {e}")))?;
        tx.execute(
            "DELETE FROM path_leases AS lease
             WHERE EXISTS (
                 SELECT 1 FROM path_heads AS incoming
                 WHERE incoming.batch_id = ?1
                   AND incoming.path_bytes = lease.path_bytes
             )
               AND NOT EXISTS (
                   SELECT 1 FROM path_heads AS head
                   WHERE head.batch_id = lease.batch_id
                     AND head.path_bytes = lease.path_bytes
                     AND head.recipe_hash = lease.recipe_hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM publication_intent_entries AS pending
                   WHERE pending.batch_id = lease.batch_id
                     AND pending.path_bytes = lease.path_bytes
                     AND pending.recipe_hash = lease.recipe_hash
               )",
            params![batch_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to release superseded path leases: {e}"))
        })?;
        for superseded_batch in superseded_batches {
            tx.execute(
                "DELETE FROM staging_batches
                 WHERE batch_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM path_leases
                       WHERE path_leases.batch_id = staging_batches.batch_id
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM publication_intent_entries
                       WHERE publication_intent_entries.batch_id = staging_batches.batch_id
                   )",
                params![superseded_batch],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to remove superseded empty staging batch: {e}"
                ))
            })?;
        }
        remove_empty_published_batches(&tx)?;
        let unowned = unowned_file_hashes(&tx, candidates)?;
        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit staging publication: {e}"))
        })?;
        Ok(unowned)
    }

    pub fn release_path_head(
        &self,
        path_bytes: &[u8],
        expected_file_hash: &[u8; 32],
    ) -> Result<Option<Vec<[u8; 32]>>> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin path-head release: {e}"))
        })?;
        let head: Option<(String, Vec<u8>)> = tx
            .query_row(
                "SELECT batch_id, file_hash
                 FROM path_heads
                 WHERE path_bytes = ?1 AND file_hash = ?2",
                params![path_bytes, expected_file_hash.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to resolve exact path head: {e}"))
            })?;
        let Some((batch_id, raw_file_hash)) = head else {
            tx.commit().map_err(|e| {
                StagingError::Internal(format!("failed to close empty path-head release: {e}"))
            })?;
            return Ok(None);
        };
        let file_hash = decode_hash_blob("released path-head file hash", raw_file_hash)?;

        tx.execute(
            "DELETE FROM path_heads
             WHERE path_bytes = ?1 AND batch_id = ?2 AND file_hash = ?3",
            params![path_bytes, batch_id, file_hash.as_slice()],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete exact path head: {e}")))?;
        tx.execute(
            "DELETE FROM path_leases
             WHERE batch_id = ?1 AND path_bytes = ?2 AND file_hash = ?3",
            params![batch_id, path_bytes, file_hash.as_slice()],
        )
        .map_err(|e| StagingError::Internal(format!("failed to release exact path lease: {e}")))?;
        tx.execute(
            "DELETE FROM staging_batches
             WHERE batch_id = ?1
               AND state = 'published'
               AND NOT EXISTS (
                   SELECT 1 FROM path_leases
                   WHERE path_leases.batch_id = staging_batches.batch_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM publication_intent_entries
                   WHERE publication_intent_entries.batch_id = staging_batches.batch_id
               )",
            params![batch_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to remove released empty batch: {e}"))
        })?;

        let unowned = unowned_file_hashes(&tx, vec![file_hash])?;
        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit path-head release: {e}"))
        })?;
        Ok(Some(unowned))
    }

    pub fn create_publication_intent(
        &self,
        intent_id: &str,
        entries: &[(String, Vec<u8>, String, String)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Err(StagingError::Internal(
                "publication intent must contain at least one path".to_owned(),
            ));
        }
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin publication intent: {e}"))
        })?;
        tx.execute(
            "INSERT INTO publication_intents (intent_id) VALUES (?1)",
            params![intent_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!(
                "failed to create publication intent {intent_id}: {e}"
            ))
        })?;

        for (batch_id, path_bytes, expected_pointer_oid, previous_index_state) in entries {
            let recipe_hash: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT lease.recipe_hash
                     FROM path_leases AS lease
                     JOIN staging_batches AS batch USING (batch_id)
                     JOIN verified_recipes AS verified USING (recipe_hash)
                     WHERE lease.batch_id = ?1 AND lease.path_bytes = ?2
                       AND batch.state = 'open'",
                    params![batch_id, path_bytes],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to resolve publication lease {batch_id}: {e}"
                    ))
                })?;
            let recipe_hash = recipe_hash.ok_or_else(|| {
                StagingError::StagingCorrupt(format!(
                    "publication intent {intent_id} path has no exact verified open lease in batch {batch_id}"
                ))
            })?;
            tx.execute(
                "INSERT INTO publication_intent_entries
                 (intent_id, batch_id, path_bytes, recipe_hash, expected_pointer_oid,
                  previous_index_state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    intent_id,
                    batch_id,
                    path_bytes,
                    recipe_hash,
                    expected_pointer_oid,
                    previous_index_state
                ],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to record publication intent {intent_id}: {e}"
                ))
            })?;
        }
        tx.commit().map_err(|e| {
            StagingError::Internal(format!(
                "failed to commit publication intent {intent_id}: {e}"
            ))
        })
    }

    pub fn unresolved_publication_intents(&self) -> Result<Vec<StoredPublicationIntent>> {
        let rows = {
            let mut statement = self
                .conn
                .prepare_cached(
                    "SELECT intent_id, batch_id, path_bytes, recipe_hash, expected_pointer_oid,
                            previous_index_state
                     FROM publication_intent_entries
                     ORDER BY intent_id, path_bytes",
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to prepare unresolved publication intents: {e}"
                    ))
                })?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to query unresolved publication intents: {e}"
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to collect unresolved publication intents: {e}"
                    ))
                })?
        };

        let mut intents = Vec::<StoredPublicationIntent>::new();
        for (
            intent_id,
            batch_id,
            path_bytes,
            recipe_hash,
            expected_pointer_oid,
            previous_index_state,
        ) in rows
        {
            if intents
                .last()
                .is_none_or(|intent| intent.intent_id != intent_id)
            {
                intents.push(StoredPublicationIntent {
                    intent_id: intent_id.clone(),
                    entries: Vec::new(),
                });
            }
            let intent = intents.last_mut().ok_or_else(|| {
                StagingError::Internal("publication intent grouping failed".to_owned())
            })?;
            intent.entries.push(StoredPublicationIntentEntry {
                batch_id,
                path_bytes,
                recipe_hash: decode_hash_blob("publication recipe hash", recipe_hash)?,
                expected_pointer_oid,
                previous_index_state,
            });
        }
        Ok(intents)
    }

    pub fn publish_publication_intent(&self, intent_id: &str) -> Result<Vec<[u8; 32]>> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin publication commit: {e}"))
        })?;
        let batch_ids = publication_intent_batch_ids(&tx, intent_id)?;
        if batch_ids.is_empty() {
            return Err(StagingError::NotFound {
                path: format!("publication intent {intent_id}"),
            });
        }
        let candidates = {
            let mut statement = tx
                .prepare_cached(
                    "SELECT DISTINCT lease.file_hash
                     FROM path_leases AS lease
                     WHERE EXISTS (
                         SELECT 1 FROM publication_intent_entries AS incoming
                         WHERE incoming.intent_id = ?1
                           AND incoming.path_bytes = lease.path_bytes
                     )",
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to prepare superseded intent owners: {e}"
                    ))
                })?;
            statement
                .query_map(params![intent_id], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| {
                    StagingError::Internal(format!("failed to query superseded intent owners: {e}"))
                })?
                .map(|row| {
                    row.map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to collect superseded intent owner: {e}"
                        ))
                    })
                    .and_then(|hash| decode_hash_blob("publication intent file hash", hash))
                })
                .collect::<Result<Vec<_>>>()?
        };
        for batch_id in &batch_ids {
            let changed = tx
                .execute(
                    "UPDATE staging_batches SET state = 'published'
                     WHERE batch_id = ?1 AND state = 'open'",
                    params![batch_id],
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to publish batch {batch_id} for intent {intent_id}: {e}"
                    ))
                })?;
            if changed != 1 {
                return Err(StagingError::StagingCorrupt(format!(
                    "publication intent {intent_id} batch {batch_id} is not open"
                )));
            }
        }
        tx.execute(
            "INSERT INTO path_heads (path_bytes, batch_id, file_hash, recipe_hash)
             SELECT entry.path_bytes, entry.batch_id, lease.file_hash, entry.recipe_hash
             FROM publication_intent_entries AS entry
             JOIN path_leases AS lease
               ON lease.batch_id = entry.batch_id
              AND lease.path_bytes = entry.path_bytes
             WHERE entry.intent_id = ?1
             ON CONFLICT(path_bytes) DO UPDATE SET
                batch_id = excluded.batch_id,
                file_hash = excluded.file_hash,
                recipe_hash = excluded.recipe_hash,
                updated_at = datetime('now')",
            params![intent_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!(
                "failed to replace publication path heads {intent_id}: {e}"
            ))
        })?;
        tx.execute(
            "DELETE FROM path_leases AS lease
             WHERE EXISTS (
                 SELECT 1 FROM publication_intent_entries AS incoming
                 WHERE incoming.intent_id = ?1
                   AND incoming.path_bytes = lease.path_bytes
             )
               AND NOT EXISTS (
                   SELECT 1 FROM path_heads AS head
                   WHERE head.batch_id = lease.batch_id
                     AND head.path_bytes = lease.path_bytes
                     AND head.recipe_hash = lease.recipe_hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM publication_intent_entries AS pending
                   WHERE pending.intent_id != ?1
                     AND pending.batch_id = lease.batch_id
                     AND pending.path_bytes = lease.path_bytes
                     AND pending.recipe_hash = lease.recipe_hash
               )",
            params![intent_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!(
                "failed to release superseded intent path leases {intent_id}: {e}"
            ))
        })?;
        tx.execute(
            "DELETE FROM publication_intents WHERE intent_id = ?1",
            params![intent_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!(
                "failed to clear publication intent {intent_id}: {e}"
            ))
        })?;
        remove_empty_published_batches(&tx)?;
        let unowned = unowned_file_hashes(&tx, candidates)?;
        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit publication {intent_id}: {e}"))
        })?;
        Ok(unowned)
    }

    pub fn rollback_publication_intent(&self, intent_id: &str) -> Result<Vec<[u8; 32]>> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin publication rollback: {e}"))
        })?;
        let batch_ids = publication_intent_batch_ids(&tx, intent_id)?;
        if batch_ids.is_empty() {
            return Err(StagingError::NotFound {
                path: format!("publication intent {intent_id}"),
            });
        }
        let mut candidates = Vec::new();
        for batch_id in &batch_ids {
            let mut statement = tx
                .prepare_cached("SELECT DISTINCT file_hash FROM path_leases WHERE batch_id = ?1")
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to prepare publication rollback leases: {e}"
                    ))
                })?;
            let hashes = statement
                .query_map(params![batch_id], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to query publication rollback leases: {e}"
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to collect publication rollback leases: {e}"
                    ))
                })?;
            for hash in hashes {
                candidates.push(decode_hash_blob("publication rollback file hash", hash)?);
            }
        }
        tx.execute(
            "DELETE FROM publication_intents WHERE intent_id = ?1",
            params![intent_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!(
                "failed to clear publication intent {intent_id}: {e}"
            ))
        })?;
        for batch_id in &batch_ids {
            tx.execute(
                "DELETE FROM staging_batches WHERE batch_id = ?1",
                params![batch_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to remove rolled-back batch {batch_id}: {e}"
                ))
            })?;
        }
        let unleased = unowned_file_hashes(&tx, candidates)?;
        tx.commit().map_err(|e| {
            StagingError::Internal(format!(
                "failed to commit publication rollback {intent_id}: {e}"
            ))
        })?;
        Ok(unleased)
    }

    /// Load the exact immutable recipe owned by a canonical path head.
    ///
    /// Physical file/chunk rows cannot define a publishable file without a
    /// verified recipe retained for an unpushed Git history entry.
    pub fn published_recipe_for_file(
        &self,
        file_hash: &[u8; 32],
    ) -> Result<Option<crate::recipe::FileRecipe>> {
        let rows = {
            let mut statement = self
                .conn
                .prepare_cached(
                    "SELECT DISTINCT recipe.recipe_hash, recipe.file_size,
                            recipe.chunk_count, recipe.sequence_hash,
                            recipe.page_count, recipe.page_root_hash, recipe.policy_id
                     FROM file_recipes AS recipe
                     JOIN verified_recipes AS verified USING (recipe_hash)
                     JOIN path_heads AS head USING (recipe_hash)
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
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                        row.get::<_, String>(6)?,
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
        for (
            raw_recipe_hash,
            raw_file_size,
            raw_chunk_count,
            raw_sequence_hash,
            raw_page_count,
            raw_page_root_hash,
            policy_id,
        ) in rows
        {
            let stored_recipe_hash = decode_hash_blob("published recipe hash", raw_recipe_hash)?;
            let recipe = self.load_stored_recipe(
                &stored_recipe_hash,
                file_hash,
                (
                    raw_file_size,
                    raw_chunk_count,
                    raw_sequence_hash,
                    raw_page_count,
                    raw_page_root_hash,
                    policy_id,
                ),
            )?;
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

    /// Load the newest verified open recipe for a path when every chunk has
    /// reusable local segment or prepared-xorb authority.
    pub fn unpublished_local_recipe_for_path(
        &self,
        path_bytes: &[u8],
    ) -> Result<Option<crate::recipe::FileRecipe>> {
        let stored: Option<StoredRecipeWithHashRow> = self
            .conn
            .query_row(
                "SELECT recipe.file_hash, recipe.file_size, recipe.chunk_count,
                        recipe.sequence_hash, recipe.page_count,
                        recipe.page_root_hash, recipe.policy_id,
                        recipe.recipe_hash
                 FROM path_leases AS lease
                 JOIN staging_batches AS batch USING (batch_id)
                 JOIN file_recipes AS recipe USING (recipe_hash)
                 JOIN verified_recipes AS verified USING (recipe_hash)
                 WHERE lease.path_bytes = ?1
                   AND batch.state = 'open'
                   AND NOT EXISTS (
                       SELECT 1
                       FROM recipe_occurrences AS occurrence
                       WHERE occurrence.recipe_hash = recipe.recipe_hash
                         AND NOT EXISTS (
                             SELECT 1
                             FROM chunk_payloads AS payload
                             WHERE payload.chunk_hash = occurrence.chunk_hash
                               AND payload.size = occurrence.chunk_size
                         )
                         AND NOT EXISTS (
                             SELECT 1
                             FROM prepared_payload_chunks AS prepared
                             JOIN prepared_leases AS prepared_lease
                               ON prepared_lease.xorb_hash = prepared.xorb_hash
                              AND prepared_lease.recipe_hash = recipe.recipe_hash
                             WHERE prepared.chunk_hash = occurrence.chunk_hash
                               AND prepared.uncompressed_size = occurrence.chunk_size
                         )
                   )
                 ORDER BY batch.rowid DESC
                 LIMIT 1",
                params![path_bytes],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to query unpublished local recipe for path: {e}"
                ))
            })?;
        let Some((
            file_hash,
            file_size,
            chunk_count,
            sequence_hash,
            page_count,
            page_root_hash,
            policy_id,
            recipe_hash,
        )) = stored
        else {
            return Ok(None);
        };
        let file_hash = decode_hash_blob("unpublished local recipe file hash", file_hash)?;
        let recipe_hash = decode_hash_blob("unpublished local recipe hash", recipe_hash)?;
        self.load_stored_recipe(
            &recipe_hash,
            &file_hash,
            (
                file_size,
                chunk_count,
                sequence_hash,
                page_count,
                page_root_hash,
                policy_id,
            ),
        )
        .map(Some)
    }

    pub fn verified_local_recipe(
        &self,
        recipe_hash: &[u8; 32],
    ) -> Result<Option<crate::recipe::FileRecipe>> {
        let stored: Option<StoredRecipeRow> = self
            .conn
            .query_row(
                "SELECT recipe.file_hash, recipe.file_size, recipe.chunk_count,
                        recipe.sequence_hash, recipe.page_count,
                        recipe.page_root_hash, recipe.policy_id
                 FROM file_recipes AS recipe
                 JOIN verified_recipes AS verified USING (recipe_hash)
                 WHERE recipe.recipe_hash = ?1",
                params![recipe_hash.as_slice()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to query verified local recipe: {e}"))
            })?;
        let Some((
            file_hash,
            file_size,
            chunk_count,
            sequence_hash,
            page_count,
            page_root_hash,
            policy_id,
        )) = stored
        else {
            return Ok(None);
        };
        let file_hash = decode_hash_blob("verified local recipe file hash", file_hash)?;
        self.load_stored_recipe(
            recipe_hash,
            &file_hash,
            (
                file_size,
                chunk_count,
                sequence_hash,
                page_count,
                page_root_hash,
                policy_id,
            ),
        )
        .map(Some)
    }

    fn load_stored_recipe(
        &self,
        stored_recipe_hash: &[u8; 32],
        file_hash: &[u8; 32],
        metadata: StoredRecipeMetadata,
    ) -> Result<crate::recipe::FileRecipe> {
        let (
            raw_file_size,
            raw_chunk_count,
            raw_sequence_hash,
            raw_page_count,
            raw_page_root_hash,
            policy_id,
        ) = metadata;
        let file_size = u64::try_from(raw_file_size)
            .map_err(|_| StagingError::StagingCorrupt("negative stored recipe size".to_owned()))?;
        let chunk_count = nonnegative_count("stored recipe chunk count", raw_chunk_count)?;
        let sequence_hash = decode_hash_blob("stored recipe sequence hash", raw_sequence_hash)?;
        let page_count = nonnegative_count("stored recipe page count", raw_page_count)?;
        let page_root_hash = decode_hash_blob("stored recipe page-root hash", raw_page_root_hash)?;
        let policy = crate::recipe::ChunkingPolicyId::parse(&policy_id)?;
        let recipe = crate::recipe::FileRecipe::from_stored_root(
            policy,
            crab_xet::hash::MerkleHash::from(*file_hash),
            file_size,
            chunk_count,
            sequence_hash,
            page_count,
            page_root_hash,
            *stored_recipe_hash,
        )?;
        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT page_index, page_hash
                 FROM recipe_pages
                 WHERE recipe_hash = ?1
                 ORDER BY page_index",
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to prepare recipe page-root query: {e}"))
            })?;
        let mut rows = statement
            .query(params![stored_recipe_hash.as_slice()])
            .map_err(|e| {
                StagingError::Internal(format!("failed to query recipe page root: {e}"))
            })?;
        let mut page_root_hasher = crate::recipe::new_page_root_hasher();
        let mut found_pages = 0u64;
        while let Some(row) = rows
            .next()
            .map_err(|e| StagingError::Internal(format!("failed to read recipe page root: {e}")))?
        {
            let page_index: i64 = row.get(0).map_err(|e| {
                StagingError::Internal(format!("failed to read recipe page index: {e}"))
            })?;
            if nonnegative_count("recipe page index", page_index)? != found_pages {
                return Err(StagingError::StagingCorrupt(
                    "stored recipe pages are not contiguous".to_owned(),
                ));
            }
            let page_hash = decode_hash_blob(
                "stored recipe page hash",
                row.get(1).map_err(|e| {
                    StagingError::Internal(format!("failed to read recipe page hash: {e}"))
                })?,
            )?;
            page_root_hasher.update(&page_hash);
            found_pages = found_pages.checked_add(1).ok_or_else(|| {
                StagingError::StagingCorrupt("recipe page count overflow".to_owned())
            })?;
        }
        if found_pages != recipe.page_count()
            || page_root_hasher.finalize().as_bytes() != &recipe.page_root_hash()
        {
            return Err(StagingError::StagingCorrupt(
                "stored recipe page root does not match its indexed pages".to_owned(),
            ));
        }
        Ok(recipe)
    }

    /// Read and verify one bounded canonical recipe page.
    pub fn recipe_page(
        &self,
        recipe: &crate::recipe::FileRecipe,
        start_occurrence: u64,
    ) -> Result<crate::recipe::RecipePage> {
        if start_occurrence > recipe.chunk_count()
            || !start_occurrence.is_multiple_of(crate::recipe::RECIPE_PAGE_ENTRIES as u64)
        {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe page start {start_occurrence} is invalid for {} terms",
                recipe.chunk_count()
            )));
        }
        if start_occurrence == recipe.chunk_count() {
            return Ok(crate::recipe::RecipePage {
                start_occurrence,
                start_offset: recipe.file_size(),
                chunks: Vec::new(),
            });
        }
        let page_index = start_occurrence / crate::recipe::RECIPE_PAGE_ENTRIES as u64;
        let metadata: (i64, i64, i64, i64, Vec<u8>) = self
            .conn
            .query_row(
                "SELECT start_occurrence, start_offset, occurrence_count,
                        page_bytes, page_hash
                 FROM recipe_pages
                 WHERE recipe_hash = ?1 AND page_index = ?2",
                params![
                    recipe.hash().as_slice(),
                    sqlite_i64("recipe page index", page_index)?
                ],
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
            .map_err(|e| StagingError::Internal(format!("failed to query recipe page: {e}")))?
            .ok_or_else(|| {
                StagingError::StagingCorrupt(format!(
                    "recipe {} is missing page {page_index}",
                    crab_xet::hash::MerkleHash::from(recipe.hash()).hex()
                ))
            })?;
        let stored_start = nonnegative_count("recipe page start", metadata.0)?;
        let start_offset = nonnegative_count("recipe page offset", metadata.1)?;
        let occurrence_count = nonnegative_count("recipe page term count", metadata.2)?;
        let page_bytes = nonnegative_count("recipe page bytes", metadata.3)?;
        let stored_page_hash = decode_hash_blob("recipe page hash", metadata.4)?;
        if stored_start != start_occurrence
            || occurrence_count == 0
            || occurrence_count > crate::recipe::RECIPE_PAGE_ENTRIES as u64
        {
            return Err(StagingError::StagingCorrupt(
                "stored recipe page metadata is invalid".to_owned(),
            ));
        }

        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT occurrence, chunk_hash, chunk_offset, chunk_size
                 FROM recipe_occurrences AS occurrence
                 WHERE recipe_hash = ?1
                   AND occurrence >= ?2
                   AND occurrence < ?3
                 ORDER BY occurrence",
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to prepare recipe page terms: {e}"))
            })?;
        let end_occurrence = start_occurrence
            .checked_add(occurrence_count)
            .ok_or_else(|| StagingError::StagingCorrupt("recipe page end overflow".to_owned()))?;
        let rows = statement
            .query_map(
                params![
                    recipe.hash().as_slice(),
                    sqlite_i64("recipe page start", start_occurrence)?,
                    sqlite_i64("recipe page end", end_occurrence)?
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to query recipe page terms: {e}"))
            })?;
        let mut chunks = Vec::with_capacity(occurrence_count as usize);
        let mut expected_occurrence = start_occurrence;
        let mut expected_offset = start_offset;
        for row in rows {
            let (occurrence, raw_hash, raw_offset, raw_size) = row.map_err(|e| {
                StagingError::Internal(format!("failed to read recipe page term: {e}"))
            })?;
            if nonnegative_count("recipe occurrence", occurrence)? != expected_occurrence
                || nonnegative_count("recipe occurrence offset", raw_offset)? != expected_offset
            {
                return Err(StagingError::StagingCorrupt(
                    "recipe page is non-contiguous".to_owned(),
                ));
            }
            let size = nonnegative_count("recipe occurrence size", raw_size)?;
            let chunk_hash = crab_xet::hash::MerkleHash::from(decode_hash_blob(
                "recipe occurrence hash",
                raw_hash,
            )?);
            chunks.push(crab_diff::chunk_sequence::ChunkSpan {
                chunk_hash,
                offset: expected_offset,
                len: size,
                origin: crab_diff::chunk_sequence::ChunkOrigin {
                    xorb_hash: None,
                    xorb_chunk_index: None,
                },
            });
            expected_occurrence = expected_occurrence.checked_add(1).ok_or_else(|| {
                StagingError::StagingCorrupt("recipe occurrence overflow".to_owned())
            })?;
            expected_offset = expected_offset.checked_add(size).ok_or_else(|| {
                StagingError::StagingCorrupt("recipe occurrence byte overflow".to_owned())
            })?;
        }
        if expected_occurrence != end_occurrence
            || expected_offset.checked_sub(start_offset) != Some(page_bytes)
        {
            return Err(StagingError::StagingCorrupt(
                "recipe page coverage does not match its metadata".to_owned(),
            ));
        }
        let page_terms = chunks
            .iter()
            .map(|chunk| (chunk.chunk_hash, chunk.len))
            .collect::<Vec<_>>();
        if crate::recipe::page_hash(start_occurrence, start_offset, &page_terms)?
            != stored_page_hash
        {
            return Err(StagingError::StagingCorrupt(
                "recipe page digest does not match its terms".to_owned(),
            ));
        }
        Ok(crate::recipe::RecipePage {
            start_occurrence,
            start_offset,
            chunks,
        })
    }

    /// Verify that one canonical recipe page is backed by this file's segments.
    pub fn recipe_page_has_segment_authority(
        &self,
        recipe: &crate::recipe::FileRecipe,
        start_occurrence: u64,
    ) -> Result<bool> {
        let page = self.recipe_page(recipe, start_occurrence)?;
        if page.chunks.is_empty() {
            return Ok(true);
        }
        let end_occurrence = page.next_occurrence();
        let file_hash: [u8; 32] = recipe.file_hash().into();
        let mut statement = self
            .conn
            .prepare_cached(
                "WITH combined AS (
                     SELECT chunk_hash, size, chunk_index, 0 AS priority, rowid
                     FROM chunks
                     WHERE file_hash = ?1 AND chunk_index >= ?2 AND chunk_index < ?3
                     UNION ALL
                     SELECT chunk_hash, size, chunk_index, 1 AS priority, rowid
                     FROM pending_chunks
                     WHERE file_hash = ?1 AND chunk_index >= ?2 AND chunk_index < ?3
                 )
                 SELECT chunk_index, chunk_hash, size
                 FROM combined AS candidate
                 WHERE NOT EXISTS (
                     SELECT 1
                     FROM combined AS preferred
                     WHERE preferred.chunk_index = candidate.chunk_index
                       AND (
                           preferred.priority < candidate.priority
                           OR (
                               preferred.priority = candidate.priority
                               AND preferred.rowid < candidate.rowid
                           )
                       )
                 )
                 ORDER BY chunk_index",
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to prepare recipe segment-authority page: {e}"
                ))
            })?;
        let rows = statement
            .query_map(
                params![
                    file_hash.as_slice(),
                    sqlite_i64("recipe segment page start", start_occurrence)?,
                    sqlite_i64("recipe segment page end", end_occurrence)?
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to query recipe segment-authority page: {e}"
                ))
            })?;
        let rows = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to collect recipe segment-authority page: {e}"
                ))
            })?;
        if rows.len() != page.chunks.len() {
            return Ok(false);
        }
        for (index, ((raw_occurrence, raw_hash, raw_size), expected)) in
            rows.into_iter().zip(&page.chunks).enumerate()
        {
            let occurrence = nonnegative_count("segment recipe occurrence", raw_occurrence)?;
            let expected_occurrence =
                start_occurrence.checked_add(index as u64).ok_or_else(|| {
                    StagingError::StagingCorrupt("segment recipe occurrence overflow".to_owned())
                })?;
            if occurrence != expected_occurrence
                || decode_hash_blob("segment recipe chunk hash", raw_hash)?
                    != <[u8; 32]>::from(expected.chunk_hash)
                || nonnegative_count("segment recipe chunk size", raw_size)? != expected.len
            {
                return Ok(false);
            }
        }
        Ok(true)
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
            let file_hash: [u8; 32] = recipe.file_hash().into();
            let stored: Option<StoredRecipeRow> = tx
                .query_row(
                    "SELECT file_hash, file_size, chunk_count, sequence_hash,
                            page_count, page_root_hash, policy_id
                     FROM file_recipes WHERE recipe_hash = ?1",
                    params![recipe_hash.as_slice()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to read recipe for push snapshot {snapshot_id}: {e}"
                    ))
                })?;
            let Some((
                stored_file_hash,
                stored_size,
                stored_chunks,
                stored_sequence_hash,
                stored_pages,
                stored_page_root_hash,
                stored_policy,
            )) = stored
            else {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} references missing recipe {}",
                    blake3::Hash::from(recipe_hash).to_hex()
                )));
            };
            if stored_file_hash.as_slice() != file_hash
                || stored_size != sqlite_i64("recipe file size", recipe.file_size())?
                || stored_chunks != sqlite_i64("recipe chunk count", recipe.chunk_count())?
                || stored_sequence_hash.as_slice() != recipe.sequence_hash()
                || stored_pages != sqlite_i64("recipe page count", recipe.page_count())?
                || stored_page_root_hash.as_slice() != recipe.page_root_hash()
                || stored_policy != recipe.policy().as_str()
            {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} recipe {} identity does not match its stored row",
                    blake3::Hash::from(recipe_hash).to_hex()
                )));
            }

            let published_head: bool = tx
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1
                         FROM path_heads AS head
                         JOIN staging_batches AS batch USING (batch_id)
                         WHERE head.recipe_hash = ?1 AND batch.state = 'published'
                     )",
                    params![recipe_hash.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to verify published recipe head for snapshot {snapshot_id}: {e}"
                    ))
                })?;
            if !published_head {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} recipe {} has no published path head",
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
                     SELECT ?1, head.batch_id, head.path_bytes,
                            head.file_hash, head.recipe_hash
                     FROM path_heads AS head
                     JOIN staging_batches AS batch ON batch.batch_id = head.batch_id
                     WHERE head.recipe_hash = ?2 AND batch.state = 'published'",
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
                "DELETE FROM path_heads
                 WHERE EXISTS (
                     SELECT 1 FROM push_snapshot_leases AS captured
                     WHERE captured.snapshot_id = ?1
                       AND captured.batch_id = path_heads.batch_id
                       AND captured.path_bytes = path_heads.path_bytes
                       AND captured.recipe_hash = path_heads.recipe_hash
                 )",
                params![snapshot_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!(
                    "failed to retire exact path heads for push snapshot {snapshot_id}: {e}"
                ))
            })?;
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

        let unleased = unowned_file_hashes(&tx, file_hashes)?;
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

    /// Discard an uncommitted snapshot and reclaim leases that it alone pinned.
    pub fn discard_open_push_snapshot(&self, snapshot_id: &str) -> Result<Vec<[u8; 32]>> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!(
                "failed to begin open push snapshot discard {snapshot_id}: {e}"
            ))
        })?;
        let file_hashes = {
            let mut statement = tx
                .prepare_cached(
                    "SELECT DISTINCT recipe.file_hash
                     FROM push_snapshot_recipes AS pin
                     JOIN file_recipes AS recipe USING (recipe_hash)
                     WHERE pin.snapshot_id = ?1",
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to prepare discarded snapshot owners {snapshot_id}: {e}"
                    ))
                })?;
            statement
                .query_map(params![snapshot_id], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to query discarded snapshot owners {snapshot_id}: {e}"
                    ))
                })?
                .map(|row| {
                    row.map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to collect discarded snapshot owner {snapshot_id}: {e}"
                        ))
                    })
                    .and_then(|hash| decode_hash_blob("discarded snapshot file hash", hash))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let removed = tx
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
        if removed == 0 {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM push_snapshots WHERE snapshot_id = ?1)",
                    params![snapshot_id],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to inspect push snapshot {snapshot_id}: {e}"
                    ))
                })?;
            if exists {
                return Err(StagingError::StagingCorrupt(format!(
                    "push snapshot {snapshot_id} is committed and cannot be discarded"
                )));
            }
            return Ok(Vec::new());
        }
        let unowned = unowned_file_hashes(&tx, file_hashes)?;
        tx.commit().map_err(|e| {
            StagingError::Internal(format!(
                "failed to commit open push snapshot discard {snapshot_id}: {e}"
            ))
        })?;
        Ok(unowned)
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

    /// Release any non-canonical published leases and return completed files
    /// that have no path head/lease or immutable push-snapshot pin.
    pub fn reclaim_superseded_ownership(&self) -> Result<Vec<[u8; 32]>> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!(
                "failed to begin superseded ownership reclamation: {e}"
            ))
        })?;
        tx.execute(
            "DELETE FROM path_leases AS lease
             WHERE EXISTS (
                 SELECT 1 FROM staging_batches AS batch
                 WHERE batch.batch_id = lease.batch_id
                   AND batch.state = 'published'
             )
               AND NOT EXISTS (
                   SELECT 1 FROM path_heads AS head
                   WHERE head.batch_id = lease.batch_id
                     AND head.path_bytes = lease.path_bytes
                     AND head.recipe_hash = lease.recipe_hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM publication_intent_entries AS pending
                   WHERE pending.batch_id = lease.batch_id
                     AND pending.path_bytes = lease.path_bytes
                     AND pending.recipe_hash = lease.recipe_hash
               )",
            [],
        )
        .map_err(|e| {
            StagingError::Internal(format!(
                "failed to release reclaimable superseded leases: {e}"
            ))
        })?;
        remove_empty_published_batches(&tx)?;

        let candidates = {
            let mut statement = tx
                .prepare_cached(
                    "SELECT DISTINCT recipe.file_hash
                     FROM file_recipes AS recipe
                     WHERE NOT EXISTS (
                         SELECT 1 FROM path_leases AS lease
                         WHERE lease.file_hash = recipe.file_hash
                     )",
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to prepare reclaimable staged files: {e}"
                    ))
                })?;
            statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| {
                    StagingError::Internal(format!("failed to query reclaimable staged files: {e}"))
                })?
                .map(|row| {
                    row.map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to collect reclaimable staged file: {e}"
                        ))
                    })
                    .and_then(|hash| decode_hash_blob("reclaimable staged file hash", hash))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let unowned = unowned_file_hashes(&tx, candidates)?;
        tx.commit().map_err(|e| {
            StagingError::Internal(format!(
                "failed to commit superseded ownership reclamation: {e}"
            ))
        })?;
        Ok(unowned)
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
        let file_hash_array: [u8; 32] = recipe.file_hash().into();
        let file_hash: &[u8] = &file_hash_array;
        tx.execute(
            "INSERT OR IGNORE INTO file_recipes
             (recipe_hash, file_hash, file_size, chunk_count, sequence_hash,
              page_count, page_root_hash, policy_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                recipe_hash,
                file_hash,
                sqlite_i64("recipe file size", recipe.file_size())?,
                sqlite_i64("recipe chunk count", recipe.chunk_count())?,
                recipe.sequence_hash().as_slice(),
                sqlite_i64("recipe page count", recipe.page_count())?,
                recipe.page_root_hash().as_slice(),
                recipe.policy().as_str()
            ],
        )
        .map_err(|e| StagingError::Internal(format!("failed to insert file recipe: {e}")))?;
        let stored_recipe: (Vec<u8>, i64, i64, Vec<u8>, i64, Vec<u8>, String) = tx
            .query_row(
                "SELECT file_hash, file_size, chunk_count, sequence_hash,
                        page_count, page_root_hash, policy_id
                 FROM file_recipes WHERE recipe_hash = ?1",
                params![recipe_hash],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to validate stored file recipe: {e}"))
            })?;
        if stored_recipe.0.as_slice() != file_hash
            || stored_recipe.1 != sqlite_i64("recipe file size", recipe.file_size())?
            || stored_recipe.2 != sqlite_i64("recipe chunk count", recipe.chunk_count())?
            || stored_recipe.3.as_slice() != recipe.sequence_hash()
            || stored_recipe.4 != sqlite_i64("recipe page count", recipe.page_count())?
            || stored_recipe.5.as_slice() != recipe.page_root_hash()
            || stored_recipe.6 != recipe.policy().as_str()
        {
            return Err(StagingError::StagingCorrupt(
                "stored recipe identity collides with different file metadata".to_owned(),
            ));
        }

        tx.execute(
            "INSERT INTO temp.incoming_recipe_occurrences
             (occurrence, chunk_hash, chunk_offset, chunk_size)
             SELECT occurrence, chunk_hash, chunk_offset, chunk_size
             FROM recipe_recording_terms
             WHERE batch_id = ?1
             ORDER BY occurrence",
            params![batch_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to load recorded recipe terms: {e}"))
        })?;
        let mut incoming_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM temp.incoming_recipe_occurrences",
                [],
                |row| row.get(0),
            )
            .map_err(|e| StagingError::Internal(format!("failed to count recipe terms: {e}")))?;
        if incoming_count == 0 && recipe.chunk_count() > 0 {
            tx.execute(
                "INSERT INTO temp.incoming_recipe_occurrences
                 (occurrence, chunk_hash, chunk_offset, chunk_size)
                 SELECT occurrence, chunk_hash, chunk_offset, chunk_size
                 FROM recipe_occurrences
                 WHERE recipe_hash = ?1
                 ORDER BY occurrence",
                params![recipe_hash],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to reuse indexed recipe terms: {e}"))
            })?;
            incoming_count = tx
                .query_row(
                    "SELECT COUNT(*) FROM temp.incoming_recipe_occurrences",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to recount recipe terms: {e}"))
                })?;
        }
        if incoming_count == 0 && recipe.chunk_count() > 0 {
            tx.execute(
                "WITH combined AS (
                     SELECT chunk_hash, chunk_index, size, 0 AS priority, rowid
                     FROM chunks WHERE file_hash = ?1
                     UNION ALL
                     SELECT chunk_hash, chunk_index, size, 1 AS priority, rowid
                     FROM pending_chunks WHERE file_hash = ?1
                 ), ranked AS (
                     SELECT chunk_hash, chunk_index, size,
                            ROW_NUMBER() OVER (
                                PARTITION BY chunk_index ORDER BY priority, rowid
                            ) AS authority_rank
                     FROM combined
                 )
                 INSERT INTO temp.incoming_recipe_occurrences
                 (occurrence, chunk_hash, chunk_offset, chunk_size)
                 SELECT chunk_index, chunk_hash,
                        COALESCE(SUM(size) OVER (
                            ORDER BY chunk_index ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
                        ), 0),
                        size
                 FROM ranked
                 WHERE authority_rank = 1
                 ORDER BY chunk_index",
                params![file_hash],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to derive indexed recipe terms: {e}"))
            })?;
        }

        let occurrence_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM temp.incoming_recipe_occurrences",
                [],
                |row| row.get(0),
            )
            .map_err(|e| StagingError::Internal(format!("failed to count recipe terms: {e}")))?;
        if occurrence_count != sqlite_i64("recipe chunk count", recipe.chunk_count())? {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe has {occurrence_count} indexed terms, expected {}",
                recipe.chunk_count()
            )));
        }
        let discontinuity: Option<i64> = tx
            .query_row(
                "WITH ordered AS (
                     SELECT occurrence, chunk_offset,
                            ROW_NUMBER() OVER (ORDER BY occurrence) - 1
                                AS expected_occurrence,
                            COALESCE(SUM(chunk_size) OVER (
                                ORDER BY occurrence
                                ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
                            ), 0) AS expected_offset
                     FROM temp.incoming_recipe_occurrences
                 )
                 SELECT occurrence
                 FROM ordered
                 WHERE occurrence != expected_occurrence
                    OR chunk_offset != expected_offset
                 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to validate recipe continuity: {e}"))
            })?;
        if let Some(occurrence) = discontinuity {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe occurrence {occurrence} is not contiguous"
            )));
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

        let remote_collision: Option<Vec<u8>> = tx
            .query_row(
                "SELECT recording.chunk_hash
                 FROM recording_remote_chunks AS recording
                 JOIN recipe_remote_chunks AS stored
                   ON stored.recipe_hash = ?1
                  AND stored.chunk_hash = recording.chunk_hash
                 WHERE recording.batch_id = ?2
                   AND (stored.xorb_hash != recording.xorb_hash
                     OR stored.chunk_index != recording.chunk_index
                     OR stored.uncompressed_size != recording.uncompressed_size
                     OR stored.placement_id != recording.placement_id
                     OR stored.origin_proof_id != recording.origin_proof_id)
                 LIMIT 1",
                params![recipe_hash, batch_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to validate indexed remote authority: {error}"
                ))
            })?;
        if let Some(chunk_hash) = remote_collision {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe remote authority for chunk {} changed",
                crab_xet::hash::MerkleHash::from(decode_hash_blob(
                    "remote authority chunk hash",
                    chunk_hash,
                )?)
                .hex()
            )));
        }
        tx.execute(
            "INSERT OR IGNORE INTO recipe_remote_chunks
             (recipe_hash, chunk_hash, xorb_hash, chunk_index,
              uncompressed_size, placement_id, origin_proof_id)
             SELECT ?1, chunk_hash, xorb_hash, chunk_index,
                    uncompressed_size, placement_id, origin_proof_id
             FROM recording_remote_chunks
             WHERE batch_id = ?2",
            params![recipe_hash, batch_id],
        )
        .map_err(|error| {
            StagingError::Internal(format!("failed to seal recipe remote authority: {error}"))
        })?;
        tx.execute(
            "DELETE FROM recording_remote_chunks WHERE batch_id = ?1",
            params![batch_id],
        )
        .map_err(|error| {
            StagingError::Internal(format!(
                "failed to retire sealed remote authority recording: {error}"
            ))
        })?;

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
                       FROM prepared_payload_chunks AS prepared
                       JOIN prepared_payloads AS xorb USING (xorb_hash)
                       WHERE prepared.chunk_hash = incoming.chunk_hash
                         AND prepared.uncompressed_size = incoming.chunk_size
                   )
                   AND NOT EXISTS (
                       SELECT 1
                       FROM recipe_remote_chunks AS existing
                       WHERE existing.recipe_hash = ?1
                         AND existing.chunk_hash = incoming.chunk_hash
                         AND existing.uncompressed_size = incoming.chunk_size
                   )
                 LIMIT 1",
                params![recipe_hash],
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

        let mut sequence_hasher = crate::recipe::new_sequence_hasher();
        let mut page_root_hasher = crate::recipe::new_page_root_hasher();
        let mut page_index = 0u64;
        let mut page_start = 0u64;
        let mut page_start_offset = 0u64;
        while page_start < recipe.chunk_count() {
            let page_end = page_start
                .saturating_add(crate::recipe::RECIPE_PAGE_ENTRIES as u64)
                .min(recipe.chunk_count());
            let page_rows = {
                let mut statement = tx
                    .prepare_cached(
                        "SELECT chunk_hash, chunk_size
                         FROM temp.incoming_recipe_occurrences
                         WHERE occurrence >= ?1 AND occurrence < ?2
                         ORDER BY occurrence",
                    )
                    .map_err(|e| {
                        StagingError::Internal(format!("failed to prepare recipe page seal: {e}"))
                    })?;
                statement
                    .query_map(
                        params![
                            sqlite_i64("recipe page start", page_start)?,
                            sqlite_i64("recipe page end", page_end)?
                        ],
                        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .map_err(|e| {
                        StagingError::Internal(format!("failed to query recipe page seal: {e}"))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| {
                        StagingError::Internal(format!("failed to collect recipe page seal: {e}"))
                    })?
            };
            let mut page_terms = Vec::with_capacity(page_rows.len());
            let mut page_bytes = 0u64;
            for (raw_hash, raw_size) in page_rows {
                let chunk_hash = crab_xet::hash::MerkleHash::from(decode_hash_blob(
                    "recipe seal chunk hash",
                    raw_hash,
                )?);
                let size = nonnegative_count("recipe seal chunk size", raw_size)?;
                crate::recipe::update_sequence_hasher(&mut sequence_hasher, chunk_hash, size);
                page_bytes = page_bytes.checked_add(size).ok_or_else(|| {
                    StagingError::StagingCorrupt("recipe page byte overflow".to_owned())
                })?;
                page_terms.push((chunk_hash, size));
            }
            let page_hash = crate::recipe::page_hash(page_start, page_start_offset, &page_terms)?;
            page_root_hasher.update(&page_hash);
            let stored_page: Option<(i64, i64, i64, i64, Vec<u8>)> = tx
                .query_row(
                    "SELECT start_occurrence, start_offset, occurrence_count,
                            page_bytes, page_hash
                     FROM recipe_pages
                     WHERE recipe_hash = ?1 AND page_index = ?2",
                    params![recipe_hash, sqlite_i64("recipe page index", page_index)?],
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
                .map_err(|e| {
                    StagingError::Internal(format!("failed to inspect stored recipe page: {e}"))
                })?;
            let page_count = page_end - page_start;
            if let Some(stored) = stored_page {
                if stored.0 != sqlite_i64("recipe page start", page_start)?
                    || stored.1 != sqlite_i64("recipe page offset", page_start_offset)?
                    || stored.2 != sqlite_i64("recipe page count", page_count)?
                    || stored.3 != sqlite_i64("recipe page bytes", page_bytes)?
                    || stored.4.as_slice() != page_hash
                {
                    return Err(StagingError::StagingCorrupt(
                        "stored recipe page collides with different metadata".to_owned(),
                    ));
                }
            } else {
                tx.execute(
                    "INSERT INTO recipe_pages
                     (recipe_hash, page_index, start_occurrence, start_offset,
                      occurrence_count, page_bytes, page_hash)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        recipe_hash,
                        sqlite_i64("recipe page index", page_index)?,
                        sqlite_i64("recipe page start", page_start)?,
                        sqlite_i64("recipe page offset", page_start_offset)?,
                        sqlite_i64("recipe page count", page_count)?,
                        sqlite_i64("recipe page bytes", page_bytes)?,
                        page_hash.as_slice(),
                    ],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to insert recipe page: {e}"))
                })?;
            }
            page_index = page_index.checked_add(1).ok_or_else(|| {
                StagingError::StagingCorrupt("recipe page count overflow".to_owned())
            })?;
            page_start = page_end;
            page_start_offset = page_start_offset.checked_add(page_bytes).ok_or_else(|| {
                StagingError::StagingCorrupt("recipe page offset overflow".to_owned())
            })?;
        }
        if page_index != recipe.page_count()
            || page_start_offset != recipe.file_size()
            || sequence_hasher.finalize().as_bytes() != &recipe.sequence_hash()
            || page_root_hasher.finalize().as_bytes() != &recipe.page_root_hash()
        {
            return Err(StagingError::StagingCorrupt(
                "indexed recipe terms do not match the sealed root".to_owned(),
            ));
        }

        tx.execute(
            "INSERT OR IGNORE INTO prepared_leases (recipe_hash, xorb_hash)
             SELECT DISTINCT ?1, prepared.xorb_hash
             FROM temp.incoming_recipe_occurrences AS incoming
             JOIN prepared_payload_chunks AS prepared
               ON prepared.chunk_hash = incoming.chunk_hash
              AND prepared.uncompressed_size = incoming.chunk_size
             JOIN prepared_payloads AS xorb USING (xorb_hash)",
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
                     FROM prepared_payload_chunks AS prepared
                     JOIN prepared_leases AS lease
                       ON lease.xorb_hash = prepared.xorb_hash
                      AND lease.recipe_hash = ?1
                     WHERE prepared.chunk_hash = incoming.chunk_hash
                       AND prepared.uncompressed_size = incoming.chunk_size
                 ) OR EXISTS (
                     SELECT 1
                     FROM recipe_remote_chunks AS existing
                     WHERE existing.recipe_hash = ?2
                       AND existing.chunk_hash = incoming.chunk_hash
                       AND existing.uncompressed_size = incoming.chunk_size
                 )",
                params![recipe_hash, recipe_hash],
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
                     VALUES ('recipe_payload_validation_pending', '1')
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
        tx.execute(
            "DELETE FROM recipe_recording_terms WHERE batch_id = ?1",
            params![batch_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to release recipe recording terms: {e}"))
        })?;

        tx.commit()
            .map_err(|e| StagingError::Internal(format!("failed to commit recipe lease: {e}")))?;
        Ok(())
    }

    pub fn has_file_owner(&self, file_hash: &[u8; 32]) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM path_leases WHERE file_hash = ?1
                 ) OR EXISTS(
                     SELECT 1
                     FROM push_snapshot_recipes AS pin
                     JOIN file_recipes AS recipe USING (recipe_hash)
                     WHERE recipe.file_hash = ?1
                 )",
                params![file_hash.as_slice()],
                |row| row.get(0),
            )
            .map_err(|e| StagingError::Internal(format!("failed to check file owners: {e}")))
    }

    pub fn rollback_batch(&self, batch_id: &str) -> Result<Vec<[u8; 32]>> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| StagingError::Internal(format!("failed to begin batch rollback: {e}")))?;
        let state = tx
            .query_row(
                "SELECT state FROM staging_batches WHERE batch_id = ?1",
                params![batch_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to validate rollback batch: {e}"))
            })?;
        match state.as_deref() {
            None => return Ok(Vec::new()),
            Some("open") => {}
            Some(_) => {
                return Err(StagingError::StagingCorrupt(format!(
                    "published staging batch {batch_id} cannot be rolled back"
                )));
            }
        }
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

        let unleased = unowned_file_hashes(&tx, file_hashes)?;
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
    pub fn remove_file(&self, file_hash: &[u8; 32]) -> Result<(Vec<u64>, Vec<[u8; 32]>)> {
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

        let reclaimable_payloads = {
            let mut statement = tx
                .prepare_cached(
                    "SELECT xorb_hash
                     FROM prepared_payloads AS payload
                     WHERE NOT EXISTS (
                               SELECT 1 FROM prepared_leases AS lease
                               WHERE lease.xorb_hash = payload.xorb_hash
                           )
                       AND NOT EXISTS (
                               SELECT 1 FROM preparation_payloads AS preparation
                               WHERE preparation.xorb_hash = payload.xorb_hash
                           )
                     ORDER BY xorb_hash",
                )
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to prepare removed payload query: {error}"
                    ))
                })?;
            statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to query removed prepared payloads: {error}"
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to collect removed prepared payloads: {error}"
                    ))
                })?
                .into_iter()
                .map(|hash| decode_hash_blob("removed prepared payload", hash))
                .collect::<Result<Vec<_>>>()?
        };

        tx.execute(
            "DELETE FROM prepared_payloads
             WHERE NOT EXISTS (
                       SELECT 1 FROM prepared_leases
                       WHERE prepared_leases.xorb_hash = prepared_payloads.xorb_hash
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM preparation_payloads
                       WHERE preparation_payloads.xorb_hash = prepared_payloads.xorb_hash
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

        Ok((affected, reclaimable_payloads))
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

    pub fn chunk_payload_exists(&self, chunk_hash: &[u8; 32], size: u64) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM chunk_payloads
                    WHERE chunk_hash = ?1 AND size = ?2
                 )",
                params![
                    chunk_hash.as_slice(),
                    sqlite_i64("chunk payload size", size)?
                ],
                |row| row.get(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to inspect staged chunk payload: {error}"))
            })
    }

    /// Replace one recipe's normalized remote and prepared authority.
    pub fn insert_file_push_plan(&self, write: FilePushPlanWrite<'_>) -> Result<Vec<[u8; 32]>> {
        let FilePushPlanWrite {
            file_hash,
            recipe_hash,
            recording_batch_id,
            existing_chunks,
            prepared_xorbs,
        } = write;
        let fh: &[u8] = file_hash;
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            StagingError::Internal(format!("failed to begin file push plan tx: {e}"))
        })?;
        let recipe_is_indexed: bool = tx
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM file_recipes
                     WHERE recipe_hash = ?1 AND file_hash = ?2
                 )",
                params![recipe_hash.as_slice(), fh],
                |row| row.get(0),
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to resolve prepared recipe: {e}"))
            })?;
        if !recipe_is_indexed {
            let Some(batch_id) = recording_batch_id else {
                return Err(StagingError::StagingCorrupt(format!(
                    "cannot persist prepared authority before recipe {} is indexed",
                    crab_xet::hash::MerkleHash::from(*recipe_hash).hex()
                )));
            };
            let recording_is_open: bool = tx
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM staging_batches
                         WHERE batch_id = ?1 AND state = 'open'
                     )",
                    params![batch_id],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to resolve prepared recipe recording: {e}"
                    ))
                })?;
            if !recording_is_open {
                return Err(StagingError::NotFound {
                    path: format!("open staging batch {batch_id}"),
                });
            }
        }

        let recipe_owner = recipe_hash.as_slice();
        let recording_owner = recording_batch_id.unwrap_or_default();
        let (table, owner_column, owner, coverage_sql): (&str, &str, &dyn ToSql, &str) =
            if recipe_is_indexed {
                (
                    "recipe_remote_chunks",
                    "recipe_hash",
                    &recipe_owner,
                    "SELECT EXISTS(
                         SELECT 1 FROM recipe_occurrences
                         WHERE recipe_hash = ?1
                           AND chunk_hash = ?2
                           AND chunk_size = ?3
                     )",
                )
            } else {
                (
                    "recording_remote_chunks",
                    "batch_id",
                    &recording_owner,
                    "SELECT EXISTS(
                         SELECT 1 FROM recipe_recording_terms
                         WHERE batch_id = ?1
                           AND chunk_hash = ?2
                           AND chunk_size = ?3
                     )",
                )
            };
        let insert_sql = format!(
            "INSERT OR IGNORE INTO {table}
             ({owner_column}, chunk_hash, xorb_hash, chunk_index,
              uncompressed_size, placement_id, origin_proof_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        );
        let verify_sql = format!(
            "SELECT xorb_hash = ?3
                    AND chunk_index = ?4
                    AND uncompressed_size = ?5
                    AND placement_id = ?6
                    AND origin_proof_id = ?7
             FROM {table}
             WHERE {owner_column} = ?1 AND chunk_hash = ?2"
        );
        let mut coverage_statement = tx.prepare_cached(coverage_sql).map_err(|error| {
            StagingError::Internal(format!(
                "failed to prepare planned existing coverage: {error}"
            ))
        })?;
        let mut insert_statement = tx.prepare_cached(&insert_sql).map_err(|error| {
            StagingError::Internal(format!(
                "failed to prepare planned existing insert: {error}"
            ))
        })?;
        let mut verify_statement = tx.prepare_cached(&verify_sql).map_err(|error| {
            StagingError::Internal(format!(
                "failed to prepare planned existing verification: {error}"
            ))
        })?;
        for existing in existing_chunks {
            if existing.placement_id == [0; 32] || existing.origin_proof_id == [0; 32] {
                return Err(StagingError::StagingCorrupt(
                    "planned existing chunk has an empty placement or origin proof id".to_owned(),
                ));
            }
            let chunk_hash: &[u8] = &existing.chunk_hash;
            let covers_recipe = coverage_statement
                .query_row(
                    params![owner, chunk_hash, i64::from(existing.uncompressed_size)],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to validate planned existing recipe coverage: {error}"
                    ))
                })?;
            if !covers_recipe {
                return Err(StagingError::StagingCorrupt(format!(
                    "planned existing chunk {} does not cover file {}",
                    crab_xet::hash::MerkleHash::from(existing.chunk_hash).hex(),
                    crab_xet::hash::MerkleHash::from(*file_hash).hex()
                )));
            }
            insert_statement
                .execute(params![
                    owner,
                    chunk_hash,
                    existing.xorb_hash.as_slice(),
                    i64::from(existing.chunk_index),
                    i64::from(existing.uncompressed_size),
                    existing.placement_id.as_slice(),
                    existing.origin_proof_id.as_slice(),
                ])
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to store planned existing chunk: {error}"
                    ))
                })?;
            let matches: bool = verify_statement
                .query_row(
                    params![
                        owner,
                        chunk_hash,
                        existing.xorb_hash.as_slice(),
                        i64::from(existing.chunk_index),
                        i64::from(existing.uncompressed_size),
                        existing.placement_id.as_slice(),
                        existing.origin_proof_id.as_slice(),
                    ],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to verify planned existing chunk: {error}"
                    ))
                })?;
            if !matches {
                return Err(StagingError::StagingCorrupt(format!(
                    "planned existing chunk {} has conflicting proof authority",
                    crab_xet::hash::MerkleHash::from(existing.chunk_hash).hex()
                )));
            }
        }
        drop(coverage_statement);
        drop(insert_statement);
        drop(verify_statement);

        let mut retired_payloads = Vec::new();
        if recipe_is_indexed {
            tx.execute(
                "DELETE FROM prepared_leases WHERE recipe_hash = ?1",
                params![recipe_hash.as_slice()],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to replace prepared leases: {e}"))
            })?;
            retired_payloads = {
                let mut statement = tx
                    .prepare_cached(
                        "SELECT xorb_hash
                         FROM prepared_payloads AS payload
                         WHERE NOT EXISTS (
                                   SELECT 1 FROM prepared_leases AS lease
                                   WHERE lease.xorb_hash = payload.xorb_hash
                               )
                           AND NOT EXISTS (
                                   SELECT 1 FROM preparation_payloads AS preparation
                                   WHERE preparation.xorb_hash = payload.xorb_hash
                               )
                         ORDER BY xorb_hash",
                    )
                    .map_err(|error| {
                        StagingError::Internal(format!(
                            "failed to prepare replaced payload query: {error}"
                        ))
                    })?;
                statement
                    .query_map([], |row| row.get::<_, Vec<u8>>(0))
                    .map_err(|error| {
                        StagingError::Internal(format!(
                            "failed to query replaced prepared payloads: {error}"
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| {
                        StagingError::Internal(format!(
                            "failed to collect replaced prepared payloads: {error}"
                        ))
                    })?
                    .into_iter()
                    .map(|hash| decode_hash_blob("replaced prepared payload", hash))
                    .collect::<Result<Vec<_>>>()?
            };
            tx.execute(
                "DELETE FROM prepared_payloads
                 WHERE NOT EXISTS (
                           SELECT 1 FROM prepared_leases
                           WHERE prepared_leases.xorb_hash = prepared_payloads.xorb_hash
                       )
                   AND NOT EXISTS (
                           SELECT 1 FROM preparation_payloads
                           WHERE preparation_payloads.xorb_hash = prepared_payloads.xorb_hash
                       )",
                [],
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to retire replaced prepared payloads: {error}"
                ))
            })?;
        }

        for prepared in prepared_xorbs {
            let xorb_hash: &[u8] = &prepared.xorb_hash;
            let payload_hash: &[u8] = &prepared.payload_hash;
            let bytes = sqlite_i64("prepared xorb bytes", prepared.bytes)?;
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
            let mut covers_recipe = false;
            for placement in &prepared.placements {
                let chunk_hash: &[u8] = &placement.chunk_hash;
                if !covers_recipe {
                    covers_recipe = if recipe_is_indexed {
                        tx.query_row(
                            "SELECT EXISTS(
                                 SELECT 1 FROM recipe_occurrences
                                 WHERE recipe_hash = ?1
                                   AND chunk_hash = ?2
                                   AND chunk_size = ?3
                             )",
                            params![
                                recipe_hash.as_slice(),
                                chunk_hash,
                                i64::from(placement.uncompressed_size)
                            ],
                            |row| row.get::<_, bool>(0),
                        )
                        .map_err(|error| {
                            StagingError::Internal(format!(
                                "failed to validate prepared xorb recipe coverage: {error}"
                            ))
                        })?
                    } else {
                        tx.query_row(
                            "SELECT EXISTS(
                                 SELECT 1 FROM recipe_recording_terms
                                 WHERE batch_id = ?1
                                   AND chunk_hash = ?2
                                   AND chunk_size = ?3
                             )",
                            params![
                                recording_batch_id,
                                chunk_hash,
                                i64::from(placement.uncompressed_size)
                            ],
                            |row| row.get::<_, bool>(0),
                        )
                        .map_err(|error| {
                            StagingError::Internal(format!(
                                "failed to validate prepared xorb recipe coverage: {error}"
                            ))
                        })?
                    };
                }
                tx.execute(
                    "INSERT OR IGNORE INTO prepared_payload_chunks
                     (xorb_hash, chunk_index, chunk_hash, uncompressed_size)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        xorb_hash,
                        i64::from(placement.chunk_index),
                        chunk_hash,
                        i64::from(placement.uncompressed_size),
                    ],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to store prepared payload chunk: {e}"))
                })?;
                let stored: (Vec<u8>, i64, i64) = tx
                    .query_row(
                        "SELECT xorb_hash, chunk_index, uncompressed_size
                         FROM prepared_payload_chunks WHERE chunk_hash = ?1",
                        params![chunk_hash],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(|e| {
                        StagingError::Internal(format!(
                            "failed to verify canonical prepared chunk placement: {e}"
                        ))
                    })?;
                if stored.0.as_slice() != xorb_hash
                    || stored.1 != i64::from(placement.chunk_index)
                    || stored.2 != i64::from(placement.uncompressed_size)
                {
                    return Err(StagingError::StagingCorrupt(format!(
                        "prepared chunk {} already belongs to another canonical xorb",
                        crab_xet::hash::MerkleHash::from(placement.chunk_hash).hex()
                    )));
                }
            }
            if !covers_recipe {
                return Err(StagingError::StagingCorrupt(format!(
                    "prepared xorb {} does not cover file {}",
                    crab_xet::hash::MerkleHash::from(prepared.xorb_hash).hex(),
                    crab_xet::hash::MerkleHash::from(*file_hash).hex()
                )));
            }
            if recipe_is_indexed {
                tx.execute(
                    "INSERT OR IGNORE INTO prepared_leases (recipe_hash, xorb_hash)
                     VALUES (?1, ?2)",
                    params![recipe_hash.as_slice(), xorb_hash],
                )
                .map_err(|e| {
                    StagingError::Internal(format!("failed to store prepared lease: {e}"))
                })?;
            }
        }

        let mut removed_payloads = Vec::new();
        for xorb_hash in retired_payloads {
            let exists = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM prepared_payloads WHERE xorb_hash = ?1)",
                    params![xorb_hash.as_slice()],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to verify replaced prepared payload: {error}"
                    ))
                })?;
            if !exists {
                removed_payloads.push(xorb_hash);
            }
        }
        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit file push plan tx: {e}"))
        })?;
        Ok(removed_payloads)
    }

    pub fn recipe_remote_chunk_page(
        &self,
        recipe_hash: &[u8; 32],
        start_occurrence: u64,
    ) -> Result<Vec<ExistingChunkWrite>> {
        let end_occurrence = start_occurrence
            .checked_add(crate::recipe::RECIPE_PAGE_ENTRIES as u64)
            .ok_or_else(|| {
                StagingError::StagingCorrupt("remote authority page range overflow".to_owned())
            })?;
        let start_occurrence = i64::try_from(start_occurrence).map_err(|_| {
            StagingError::StagingCorrupt("remote authority page start is too large".to_owned())
        })?;
        let end_occurrence = i64::try_from(end_occurrence).map_err(|_| {
            StagingError::StagingCorrupt("remote authority page end is too large".to_owned())
        })?;
        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT remote.chunk_hash, remote.xorb_hash, remote.chunk_index,
                        remote.uncompressed_size, remote.placement_id,
                        remote.origin_proof_id
                 FROM recipe_occurrences AS occurrence
                 JOIN recipe_remote_chunks AS remote
                   ON remote.recipe_hash = occurrence.recipe_hash
                  AND remote.chunk_hash = occurrence.chunk_hash
                 WHERE occurrence.recipe_hash = ?1
                   AND occurrence.occurrence >= ?2
                   AND occurrence.occurrence < ?3
                 ORDER BY occurrence.occurrence",
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to prepare recipe remote authority: {error}"
                ))
            })?;
        statement
            .query_map(
                params![recipe_hash.as_slice(), start_occurrence, end_occurrence],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to query recipe remote authority: {error}"))
            })?
            .map(|row| {
                let (
                    chunk_hash,
                    xorb_hash,
                    chunk_index,
                    uncompressed_size,
                    placement_id,
                    origin_proof_id,
                ) = row.map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to read recipe remote authority: {error}"
                    ))
                })?;
                Ok(ExistingChunkWrite {
                    chunk_hash: decode_hash_blob("remote chunk hash", chunk_hash)?,
                    xorb_hash: decode_hash_blob("remote xorb hash", xorb_hash)?,
                    chunk_index: u32::try_from(chunk_index).map_err(|_| {
                        StagingError::StagingCorrupt(format!(
                            "remote chunk index is invalid: {chunk_index}"
                        ))
                    })?,
                    uncompressed_size: u32::try_from(uncompressed_size).map_err(|_| {
                        StagingError::StagingCorrupt(format!(
                            "remote chunk size is invalid: {uncompressed_size}"
                        ))
                    })?,
                    placement_id: decode_hash_blob("remote placement id", placement_id)?,
                    origin_proof_id: decode_hash_blob("remote origin proof id", origin_proof_id)?,
                })
            })
            .collect()
    }

    pub fn recipe_remote_chunk_count(&self, recipe_hash: &[u8; 32]) -> Result<u64> {
        let count = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM recipe_remote_chunks WHERE recipe_hash = ?1",
                params![recipe_hash.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to count recipe remote authority: {error}"))
            })?;
        u64::try_from(count).map_err(|_| {
            StagingError::StagingCorrupt(format!(
                "recipe remote authority count is invalid: {count}"
            ))
        })
    }

    /// Load prepared xorb candidates that cover any of the requested chunks.
    pub fn prepared_xorbs_for_chunks(
        &self,
        chunk_hashes: &[[u8; 32]],
    ) -> Result<Vec<StoredPreparedXorb>> {
        if chunk_hashes.is_empty() {
            return Ok(Vec::new());
        }

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for batch in chunk_hashes.chunks(PREPARED_XORB_QUERY_CHUNK_BATCH) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let sql = format!(
                "SELECT DISTINCT px.xorb_hash, px.payload_hash, px.bytes
                 FROM prepared_payload_chunks pc
                 INNER JOIN prepared_payloads px USING (xorb_hash)
                 WHERE pc.chunk_hash IN ({placeholders})
                 ORDER BY px.xorb_hash"
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
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .map_err(|e| StagingError::Internal(format!("query prepared xorb lookup: {e}")))?;

            for row in rows {
                let (xorb_hash, payload_hash, bytes) = row
                    .map_err(|e| StagingError::Internal(format!("read prepared xorb row: {e}")))?;
                let xorb_hash = decode_hash_blob("prepared xorb hash", xorb_hash)?;
                if !seen.insert(xorb_hash) {
                    continue;
                }
                out.push(StoredPreparedXorb {
                    xorb_hash,
                    payload_hash: decode_hash_blob("prepared xorb payload hash", payload_hash)?,
                    bytes: nonnegative_count("prepared xorb bytes", bytes)?,
                    placements: self.prepared_payload_placements(&xorb_hash)?,
                });
            }
        }

        Ok(out)
    }

    pub fn prepared_payload_exclusive_to_recipe(
        &self,
        xorb_hash: &[u8; 32],
        recipe_hash: &[u8; 32],
    ) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM prepared_leases
                     WHERE xorb_hash = ?1 AND recipe_hash = ?2
                 )
                 AND NOT EXISTS(
                     SELECT 1 FROM prepared_leases
                     WHERE xorb_hash = ?1 AND recipe_hash != ?2
                 )
                 AND NOT EXISTS(
                     SELECT 1 FROM preparation_payloads WHERE xorb_hash = ?1
                 )",
                params![xorb_hash.as_slice(), recipe_hash.as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to inspect prepared payload ownership: {error}"
                ))
            })
    }

    /// List raw prepared xorb rows for staging diagnostics.
    pub fn raw_prepared_xorb_rows(&self) -> Result<Vec<RawPreparedXorbRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT xorb_hash, payload_hash, bytes
                 FROM prepared_payloads
                 ORDER BY xorb_hash",
            )
            .map_err(|e| StagingError::Internal(format!("prepare prepared xorb rows: {e}")))?;
        stmt.query_map([], |row| {
            Ok(RawPreparedXorbRow {
                xorb_hash: row.get(0)?,
                payload_hash: row.get(1)?,
                bytes: row.get(2)?,
            })
        })
        .map_err(|e| StagingError::Internal(format!("query prepared xorb rows: {e}")))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| StagingError::Internal(format!("collect prepared xorb rows: {e}")))
    }

    pub fn prepared_payload_hashes(&self) -> Result<Vec<[u8; 32]>> {
        let mut statement = self
            .conn
            .prepare("SELECT xorb_hash FROM prepared_payloads ORDER BY xorb_hash")
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to prepare prepared payload inventory: {error}"
                ))
            })?;
        statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to query prepared payload inventory: {error}"
                ))
            })?
            .map(|row| {
                row.map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to read prepared payload inventory: {error}"
                    ))
                })
                .and_then(|hash| decode_hash_blob("prepared payload inventory", hash))
            })
            .collect()
    }

    pub fn prepared_xorbs_for_recipe(
        &self,
        recipe_hash: &[u8; 32],
    ) -> Result<Vec<StoredPreparedXorb>> {
        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT payload.xorb_hash, payload.payload_hash, payload.bytes
                 FROM prepared_leases AS lease
                 JOIN prepared_payloads AS payload USING (xorb_hash)
                 WHERE lease.recipe_hash = ?1
                 ORDER BY payload.xorb_hash",
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to prepare recipe prepared xorbs: {error}"))
            })?;
        let rows = statement
            .query_map(params![recipe_hash.as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|error| {
                StagingError::Internal(format!("failed to query recipe prepared xorbs: {error}"))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                StagingError::Internal(format!("failed to collect recipe prepared xorbs: {error}"))
            })?;
        drop(statement);

        let mut out = Vec::with_capacity(rows.len());
        for (xorb_hash, payload_hash, bytes) in rows {
            let xorb_hash = decode_hash_blob("recipe prepared xorb hash", xorb_hash)?;
            out.push(StoredPreparedXorb {
                xorb_hash,
                payload_hash: decode_hash_blob("recipe prepared payload hash", payload_hash)?,
                bytes: nonnegative_count("recipe prepared payload bytes", bytes)?,
                placements: self.prepared_payload_placements(&xorb_hash)?,
            });
        }
        Ok(out)
    }

    fn prepared_payload_placements(
        &self,
        xorb_hash: &[u8; 32],
    ) -> Result<Vec<PreparedXorbPlacementWrite>> {
        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT chunk_hash, chunk_index, uncompressed_size
                 FROM prepared_payload_chunks
                 WHERE xorb_hash = ?1
                 ORDER BY chunk_index",
            )
            .map_err(|error| {
                StagingError::Internal(format!("failed to prepare payload placements: {error}"))
            })?;
        statement
            .query_map(params![xorb_hash.as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|error| {
                StagingError::Internal(format!("failed to query payload placements: {error}"))
            })?
            .map(|row| {
                let (chunk_hash, chunk_index, uncompressed_size) = row.map_err(|error| {
                    StagingError::Internal(format!("failed to read payload placement: {error}"))
                })?;
                Ok(PreparedXorbPlacementWrite {
                    chunk_hash: decode_hash_blob("prepared placement chunk hash", chunk_hash)?,
                    chunk_index: u32::try_from(chunk_index).map_err(|_| {
                        StagingError::StagingCorrupt(
                            "prepared placement chunk index is invalid".to_owned(),
                        )
                    })?,
                    uncompressed_size: u32::try_from(uncompressed_size).map_err(|_| {
                        StagingError::StagingCorrupt(
                            "prepared placement size is invalid".to_owned(),
                        )
                    })?,
                })
            })
            .collect()
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
            "SELECT chunk_hash, xorb_hash, payload_hash, bytes,
                    chunk_index, uncompressed_size
             FROM prepared_payload_chunks AS chunk
             JOIN prepared_payloads AS xorb USING (xorb_hash)
             WHERE chunk.chunk_hash IN ({placeholders})"
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
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .map_err(|error| {
                StagingError::Internal(format!("query prepared chunk batch lookup: {error}"))
            })?;

        let mut out = vec![None; hashes.len()];
        for row in rows {
            let (chunk_hash, xorb_hash, payload_hash, bytes, chunk_index, size) =
                row.map_err(|error| {
                    StagingError::Internal(format!("read prepared chunk batch lookup row: {error}"))
                })?;
            let chunk_hash = decode_hash_blob("prepared chunk hash", chunk_hash)?;
            let locator = PreparedChunkLocator {
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

    /// Start one connection-local, disk-backed residual read plan.
    pub fn begin_coalesced_read_plan(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS coalesced_read_requests (
                    sequence             INTEGER PRIMARY KEY,
                    chunk_hash          BLOB NOT NULL,
                    expected_size       INTEGER NOT NULL,
                    context             INTEGER NOT NULL,
                    authority           INTEGER NOT NULL CHECK (authority IN (0, 1)),
                    segment_id          INTEGER,
                    segment_offset      INTEGER,
                    segment_length      INTEGER,
                    prepared_xorb_hash  BLOB,
                    payload_hash        BLOB,
                    xorb_bytes          INTEGER,
                    prepared_chunk_index INTEGER,
                    prepared_size       INTEGER
                ) WITHOUT ROWID;
                DELETE FROM temp.coalesced_read_requests;",
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to initialize coalesced residual read plan: {error}"
                ))
            })
    }

    /// Append one bounded request page to the disk-backed residual plan.
    pub fn append_coalesced_read_requests(
        &self,
        requests: &[(u64, [u8; 32], u64, u64)],
    ) -> Result<()> {
        if requests.is_empty() {
            return Ok(());
        }
        let hashes = requests
            .iter()
            .map(|(_, hash, _, _)| *hash)
            .collect::<Vec<_>>();
        let segment_locators = self.locate_batch(&hashes)?;
        let prepared_locators = self.locate_prepared_batch(&hashes)?;
        let tx = self.conn.unchecked_transaction().map_err(|error| {
            StagingError::Internal(format!(
                "failed to begin coalesced residual request append: {error}"
            ))
        })?;
        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT INTO temp.coalesced_read_requests (
                         sequence, chunk_hash, expected_size, context, authority,
                         segment_id, segment_offset, segment_length,
                         prepared_xorb_hash, payload_hash,
                         xorb_bytes, prepared_chunk_index, prepared_size
                     ) VALUES (
                         ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13
                     )",
                )
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to prepare coalesced residual request append: {error}"
                    ))
                })?;
            for (((sequence, hash, expected_size, context), segment), prepared) in
                requests.iter().zip(segment_locators).zip(prepared_locators)
            {
                if let Some(locator) = segment {
                    let actual_size = u64::from(locator.length);
                    if actual_size != *expected_size {
                        return Err(StagingError::StagingCorrupt(format!(
                            "staged chunk {} changed size: expected {expected_size}, found {actual_size}",
                            crab_xet::hash::MerkleHash::from(*hash).hex()
                        )));
                    }
                    insert
                        .execute(params![
                            sqlite_i64("coalesced request sequence", *sequence)?,
                            hash.as_slice(),
                            sqlite_i64("coalesced request size", *expected_size)?,
                            sqlite_i64("coalesced request context", *context)?,
                            0i64,
                            locator.segment_id,
                            locator.offset,
                            locator.length,
                            Option::<&[u8]>::None,
                            Option::<&[u8]>::None,
                            Option::<i64>::None,
                            Option::<i64>::None,
                            Option::<i64>::None,
                        ])
                        .map_err(|error| {
                            StagingError::Internal(format!(
                                "failed to append segment residual request: {error}"
                            ))
                        })?;
                    continue;
                }
                let Some(locator) = prepared else {
                    return Err(StagingError::ChunkNotFound {
                        hash: crab_xet::hash::MerkleHash::from(*hash).hex(),
                    });
                };
                if u64::from(locator.size) != *expected_size {
                    return Err(StagingError::StagingCorrupt(format!(
                        "prepared chunk {} changed size: expected {expected_size}, found {}",
                        crab_xet::hash::MerkleHash::from(*hash).hex(),
                        locator.size
                    )));
                }
                insert
                    .execute(params![
                        sqlite_i64("coalesced request sequence", *sequence)?,
                        hash.as_slice(),
                        sqlite_i64("coalesced request size", *expected_size)?,
                        sqlite_i64("coalesced request context", *context)?,
                        1i64,
                        Option::<i64>::None,
                        Option::<i64>::None,
                        Option::<i64>::None,
                        locator.xorb_hash.as_slice(),
                        locator.payload_hash.as_slice(),
                        sqlite_i64("prepared xorb bytes", locator.xorb_bytes)?,
                        i64::from(locator.chunk_index),
                        i64::from(locator.size),
                    ])
                    .map_err(|error| {
                        StagingError::Internal(format!(
                            "failed to append prepared residual request: {error}"
                        ))
                    })?;
            }
        }
        tx.commit().map_err(|error| {
            StagingError::Internal(format!(
                "failed to commit coalesced residual request append: {error}"
            ))
        })
    }

    /// Remove and return the next bounded residual read group.
    pub fn take_coalesced_read_group(
        &self,
        max_segment_chunks: usize,
        max_segment_bytes: u64,
    ) -> Result<Option<IndexedCoalescedReadGroup>> {
        let first: Option<ResidualAuthorityRow> = self
            .conn
            .query_row(
                "SELECT authority, prepared_xorb_hash, payload_hash, xorb_bytes
                 FROM temp.coalesced_read_requests
                 ORDER BY sequence
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to inspect coalesced residual read plan: {error}"
                ))
            })?;
        let Some((authority, raw_xorb_hash, raw_payload_hash, raw_xorb_bytes)) = first else {
            return Ok(None);
        };

        if authority == 0 {
            let mut statement = self
                .conn
                .prepare_cached(
                    "SELECT sequence, context, chunk_hash, expected_size,
                            segment_id, segment_offset, segment_length
                     FROM temp.coalesced_read_requests
                     WHERE authority = 0
                     ORDER BY sequence",
                )
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to prepare segment residual group: {error}"
                    ))
                })?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, u64>(4)?,
                        row.get::<_, u64>(5)?,
                        row.get::<_, u32>(6)?,
                    ))
                })
                .map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to query segment residual group: {error}"
                    ))
                })?;
            let mut sequences = Vec::new();
            let mut chunks = Vec::new();
            let mut bytes = 0u64;
            for row in rows {
                let (
                    raw_sequence,
                    raw_context,
                    raw_chunk_hash,
                    raw_expected_size,
                    segment_id,
                    segment_offset,
                    length,
                ) = row.map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to read segment residual group: {error}"
                    ))
                })?;
                let sequence = nonnegative_count("segment residual sequence", raw_sequence)?;
                let context = nonnegative_count("segment residual context", raw_context)?;
                let chunk_hash = decode_hash_blob("segment residual chunk hash", raw_chunk_hash)?;
                let expected_size =
                    nonnegative_count("segment residual chunk size", raw_expected_size)?;
                if u64::from(length) != expected_size {
                    return Err(StagingError::StagingCorrupt(
                        "segment residual locator size changed".to_owned(),
                    ));
                }
                let exceeds = !chunks.is_empty()
                    && (chunks.len() >= max_segment_chunks.max(1)
                        || bytes.saturating_add(expected_size) > max_segment_bytes.max(1));
                if exceeds {
                    break;
                }
                sequences.push(sequence);
                chunks.push((
                    context,
                    FileChunkLocator {
                        chunk_hash,
                        size: expected_size,
                        locator: ChunkLocator {
                            segment_id,
                            offset: segment_offset,
                            length,
                        },
                    },
                ));
                bytes = bytes.saturating_add(expected_size);
            }
            drop(statement);
            let tx = self.conn.unchecked_transaction().map_err(|error| {
                StagingError::Internal(format!("failed to begin segment residual dequeue: {error}"))
            })?;
            {
                let mut delete = tx
                    .prepare_cached("DELETE FROM temp.coalesced_read_requests WHERE sequence = ?1")
                    .map_err(|error| {
                        StagingError::Internal(format!(
                            "failed to prepare segment residual dequeue: {error}"
                        ))
                    })?;
                for sequence in sequences {
                    delete
                        .execute(params![sqlite_i64("segment residual sequence", sequence)?])
                        .map_err(|error| {
                            StagingError::Internal(format!(
                                "failed to dequeue segment residual request: {error}"
                            ))
                        })?;
                }
            }
            tx.commit().map_err(|error| {
                StagingError::Internal(format!(
                    "failed to commit segment residual dequeue: {error}"
                ))
            })?;
            return Ok(Some(IndexedCoalescedReadGroup::Segments(chunks)));
        }

        if authority != 1 {
            return Err(StagingError::StagingCorrupt(format!(
                "unknown coalesced residual authority {authority}"
            )));
        }
        let xorb_hash = decode_hash_blob(
            "prepared residual xorb hash",
            raw_xorb_hash.ok_or_else(|| {
                StagingError::StagingCorrupt(
                    "prepared residual request lacks an xorb hash".to_owned(),
                )
            })?,
        )?;
        let payload_hash = decode_hash_blob(
            "prepared residual payload hash",
            raw_payload_hash.ok_or_else(|| {
                StagingError::StagingCorrupt(
                    "prepared residual request lacks a payload hash".to_owned(),
                )
            })?,
        )?;
        let xorb_bytes = nonnegative_count(
            "prepared residual xorb bytes",
            raw_xorb_bytes.ok_or_else(|| {
                StagingError::StagingCorrupt(
                    "prepared residual request lacks a payload size".to_owned(),
                )
            })?,
        )?;
        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT context, chunk_hash, expected_size,
                        prepared_chunk_index, prepared_size
                 FROM temp.coalesced_read_requests
                 WHERE authority = 1
                   AND prepared_xorb_hash = ?1
                   AND payload_hash = ?2
                   AND xorb_bytes = ?3
                 ORDER BY sequence",
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to prepare source-xorb residual group: {error}"
                ))
            })?;
        let rows = statement
            .query_map(
                params![
                    xorb_hash.as_slice(),
                    payload_hash.as_slice(),
                    sqlite_i64("prepared residual xorb bytes", xorb_bytes)?
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to query source-xorb residual group: {error}"
                ))
            })?;
        let mut chunks = Vec::new();
        for row in rows {
            let (context, chunk_hash, expected_size, chunk_index, prepared_size) =
                row.map_err(|error| {
                    StagingError::Internal(format!(
                        "failed to read source-xorb residual group: {error}"
                    ))
                })?;
            let expected_size = nonnegative_count("prepared residual chunk size", expected_size)?;
            let prepared_size = u32::try_from(prepared_size).map_err(|_| {
                StagingError::StagingCorrupt("invalid prepared residual chunk size".to_owned())
            })?;
            if u64::from(prepared_size) != expected_size {
                return Err(StagingError::StagingCorrupt(
                    "prepared residual locator size changed".to_owned(),
                ));
            }
            let chunk_hash = decode_hash_blob("prepared residual chunk hash", chunk_hash)?;
            chunks.push((
                nonnegative_count("prepared residual context", context)?,
                chunk_hash,
                PreparedChunkLocator {
                    xorb_hash,
                    payload_hash,
                    xorb_bytes,
                    chunk_index: u32::try_from(chunk_index).map_err(|_| {
                        StagingError::StagingCorrupt(
                            "invalid prepared residual chunk index".to_owned(),
                        )
                    })?,
                    size: prepared_size,
                },
            ));
        }
        drop(statement);
        self.conn
            .execute(
                "DELETE FROM temp.coalesced_read_requests
                 WHERE authority = 1
                   AND prepared_xorb_hash = ?1
                   AND payload_hash = ?2
                   AND xorb_bytes = ?3",
                params![
                    xorb_hash.as_slice(),
                    payload_hash.as_slice(),
                    sqlite_i64("prepared residual xorb bytes", xorb_bytes)?
                ],
            )
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to dequeue source-xorb residual group: {error}"
                ))
            })?;
        Ok(Some(IndexedCoalescedReadGroup::Prepared(chunks)))
    }

    /// Discard any remaining connection-local residual requests.
    pub fn clear_coalesced_read_plan(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM temp.coalesced_read_requests", [])
            .map_err(|error| {
                StagingError::Internal(format!(
                    "failed to clear coalesced residual read plan: {error}"
                ))
            })?;
        Ok(())
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
        let bytes = |query: &str| -> Result<u64> {
            let value: i64 = self
                .conn
                .query_row(query, [], |row| row.get(0))
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed staging lifecycle byte query {query:?}: {e}"
                    ))
                })?;
            nonnegative_count("staging lifecycle bytes", value)
        };
        Ok(super::stats::StagingLifecycleHealth {
            layout_version,
            quarantined_entries: count("SELECT COUNT(*) FROM staging_quarantine")?,
            unresolved_publications: count("SELECT COUNT(*) FROM publication_intents")?,
            open_batches_without_publication: count(
                "SELECT COUNT(*)
                 FROM staging_batches AS batch
                 WHERE batch.state = 'open'
                   AND NOT EXISTS (
                       SELECT 1 FROM publication_intent_entries AS entry
                       WHERE entry.batch_id = batch.batch_id
                   )",
            )?,
            open_push_snapshots: count("SELECT COUNT(*) FROM push_snapshots WHERE state = 'open'")?,
            committed_push_snapshots: count(
                "SELECT COUNT(*) FROM push_snapshots WHERE state = 'committed'",
            )?,
            recipes: count("SELECT COUNT(*) FROM file_recipes")?,
            path_heads: count("SELECT COUNT(*) FROM path_heads")?,
            path_leases: count("SELECT COUNT(*) FROM path_leases")?,
            snapshot_pinned_superseded_leases: count(
                "SELECT COUNT(*)
                 FROM push_snapshot_leases AS pin
                 JOIN push_snapshots AS snapshot USING (snapshot_id)
                 WHERE snapshot.state IN ('open', 'committed')
                   AND NOT EXISTS (
                       SELECT 1 FROM path_heads AS head
                       WHERE head.batch_id = pin.batch_id
                         AND head.path_bytes = pin.path_bytes
                         AND head.recipe_hash = pin.recipe_hash
                   )",
            )?,
            reclaimable_superseded_leases: count(
                "SELECT COUNT(*)
                 FROM path_leases AS lease
                 JOIN staging_batches AS batch USING (batch_id)
                 WHERE batch.state = 'published'
                   AND NOT EXISTS (
                       SELECT 1 FROM path_heads AS head
                       WHERE head.batch_id = lease.batch_id
                         AND head.path_bytes = lease.path_bytes
                         AND head.recipe_hash = lease.recipe_hash
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM publication_intent_entries AS entry
                       WHERE entry.batch_id = lease.batch_id
                         AND entry.path_bytes = lease.path_bytes
                         AND entry.recipe_hash = lease.recipe_hash
                   )",
            )?,
            reclaimable_files: count(
                "SELECT COUNT(DISTINCT recipe.file_hash)
                 FROM file_recipes AS recipe
                 WHERE NOT EXISTS (
                     SELECT 1 FROM path_leases AS lease
                     WHERE lease.file_hash = recipe.file_hash
                 )
                   AND NOT EXISTS (
                     SELECT 1
                     FROM push_snapshot_recipes AS pin
                     JOIN file_recipes AS pinned USING (recipe_hash)
                     WHERE pinned.file_hash = recipe.file_hash
                 )",
            )?,
            payloads: count("SELECT COUNT(*) FROM chunk_payloads")?,
            current_head_segment_bytes: bytes(
                "SELECT COALESCE(SUM(payload.size), 0)
                 FROM chunk_payloads AS payload
                 WHERE EXISTS (
                     SELECT 1
                     FROM recipe_payload_leases AS lease
                     JOIN path_heads AS head USING (recipe_hash)
                     WHERE lease.chunk_hash = payload.chunk_hash
                 )",
            )?,
            snapshot_pinned_segment_bytes: bytes(
                "SELECT COALESCE(SUM(payload.size), 0)
                 FROM chunk_payloads AS payload
                 WHERE EXISTS (
                     SELECT 1
                     FROM recipe_payload_leases AS lease
                     JOIN push_snapshot_recipes AS pin USING (recipe_hash)
                     WHERE lease.chunk_hash = payload.chunk_hash
                 )
                   AND NOT EXISTS (
                     SELECT 1
                     FROM recipe_payload_leases AS lease
                     JOIN path_heads AS head USING (recipe_hash)
                     WHERE lease.chunk_hash = payload.chunk_hash
                 )",
            )?,
            current_head_prepared_bytes: bytes(
                "SELECT COALESCE(SUM(payload.bytes), 0)
                 FROM prepared_payloads AS payload
                 WHERE EXISTS (
                     SELECT 1
                     FROM prepared_leases AS lease
                     JOIN path_heads AS head USING (recipe_hash)
                     WHERE lease.xorb_hash = payload.xorb_hash
                 )",
            )?,
            snapshot_pinned_prepared_bytes: bytes(
                "SELECT COALESCE(SUM(payload.bytes), 0)
                 FROM prepared_payloads AS payload
                 WHERE EXISTS (
                     SELECT 1
                     FROM prepared_leases AS lease
                     JOIN push_snapshot_recipes AS pin USING (recipe_hash)
                     WHERE lease.xorb_hash = payload.xorb_hash
                 )
                   AND NOT EXISTS (
                     SELECT 1
                     FROM prepared_leases AS lease
                     JOIN path_heads AS head USING (recipe_hash)
                     WHERE lease.xorb_hash = payload.xorb_hash
                 )",
            )?,
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
    use proptest::prelude::*;

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

    fn assert_staging_corrupt_contains<T>(result: Result<T>, expected: &str) {
        match result {
            Err(StagingError::StagingCorrupt(reason)) => {
                assert!(
                    reason.contains(expected),
                    "expected staging corruption containing {expected:?}, got {reason:?}"
                );
            }
            Err(other) => panic!("expected staging corruption, got {other:?}"),
            Ok(_) => panic!("expected staging corruption, got success"),
        }
    }

    fn insert_test_recipe_lease(
        idx: &Index,
        batch_id: &str,
        path: &[u8],
        file_seed: u8,
        chunk_seed: u8,
    ) -> crate::recipe::FileRecipe {
        let segment_id = idx.allocate_segment_id().expect("allocate segment");
        let file_hash = test_hash(file_seed);
        let chunk_hash = test_hash(chunk_seed);
        insert_test_file(idx, &file_hash, 8);
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
        idx.insert_batch(batch_id).expect("batch");
        idx.insert_recipe_lease(batch_id, path, &recipe, RecipeVerification::CallerVerified)
            .expect("recipe lease");
        recipe
    }

    #[test]
    fn publication_intent_atomically_replaces_canonical_path_head() {
        let idx = open_in_memory();
        let path = b"models/large.bin";
        let first = insert_test_recipe_lease(&idx, "batch-a", path, 0xA1, 0xA2);
        idx.create_publication_intent(
            "intent-a",
            &[(
                "batch-a".to_owned(),
                path.to_vec(),
                "oid-a".to_owned(),
                "old-a".to_owned(),
            )],
        )
        .expect("create first intent");
        let unresolved = idx.unresolved_publication_intents().expect("list intents");
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].entries[0].previous_index_state, "old-a");
        assert!(
            idx.publish_publication_intent("intent-a")
                .expect("publish first")
                .is_empty()
        );
        let first_file_hash: [u8; 32] = first.file_hash().into();
        assert_eq!(
            idx.published_recipe_for_file(&first_file_hash)
                .expect("first lookup"),
            Some(first.clone())
        );

        let second = insert_test_recipe_lease(&idx, "batch-b", path, 0xB1, 0xB2);
        idx.create_publication_intent(
            "intent-b",
            &[(
                "batch-b".to_owned(),
                path.to_vec(),
                "oid-b".to_owned(),
                "old-b".to_owned(),
            )],
        )
        .expect("create second intent");
        let unleased = idx
            .publish_publication_intent("intent-b")
            .expect("publish second");
        assert_eq!(unleased, vec![first_file_hash]);
        assert_eq!(
            idx.published_recipe_for_file(&first_file_hash)
                .expect("superseded lookup"),
            None
        );
        let second_file_hash: [u8; 32] = second.file_hash().into();
        assert_eq!(
            idx.published_recipe_for_file(&second_file_hash)
                .expect("current lookup"),
            Some(second)
        );
        let heads: i64 = idx
            .conn
            .query_row("SELECT COUNT(*) FROM path_heads", [], |row| row.get(0))
            .expect("count heads");
        assert_eq!(heads, 1);
    }

    #[test]
    fn releasing_one_path_preserves_shared_file_authority() {
        let idx = open_in_memory();
        let first_path = b"models/first.bin";
        let second_path = b"models/second.bin";
        let recipe = insert_test_recipe_lease(&idx, "batch-a", first_path, 0x91, 0x92);
        idx.mark_batch_published("batch-a").expect("publish first");
        idx.insert_batch("batch-b").expect("second batch");
        idx.insert_recipe_lease(
            "batch-b",
            second_path,
            &recipe,
            RecipeVerification::CallerVerified,
        )
        .expect("second recipe lease");
        idx.mark_batch_published("batch-b").expect("publish second");
        let file_hash: [u8; 32] = recipe.file_hash().into();

        assert_eq!(
            idx.release_path_head(first_path, &file_hash)
                .expect("release first"),
            Some(Vec::new())
        );
        assert_eq!(
            idx.published_recipe_for_file(&file_hash)
                .expect("shared file remains published"),
            Some(recipe)
        );
        assert_eq!(
            idx.release_path_head(second_path, &file_hash)
                .expect("release second"),
            Some(vec![file_hash])
        );
    }

    #[test]
    fn preparation_finalization_rejects_member_without_recipe_lease() {
        let idx = open_in_memory();
        idx.insert_add_preparation("preparation-a")
            .expect("preparation");
        idx.insert_batch("batch-a").expect("batch");
        idx.attach_add_preparation_batch("preparation-a", "batch-a")
            .expect("attach batch");

        assert_staging_corrupt_contains(
            idx.finalize_add_preparation("preparation-a"),
            "has no sealed recipe lease",
        );
    }

    #[test]
    fn repeated_chunk_claim_has_one_winning_occurrence() {
        let idx = open_in_memory();
        idx.insert_add_preparation("preparation-a")
            .expect("preparation");
        idx.insert_batch("batch-a").expect("batch");
        idx.attach_add_preparation_batch("preparation-a", "batch-a")
            .expect("attach batch");
        let chunk_hash = test_hash(0x93);

        assert_eq!(
            idx.claim_prepared_chunks(
                "preparation-a",
                "batch-a",
                &[(chunk_hash, 8), (chunk_hash, 8)],
            )
            .expect("claim repeated chunk"),
            vec![PreparedChunkClaim::Claimed, PreparedChunkClaim::Pending]
        );
    }

    #[test]
    fn unattached_batch_cannot_claim_prepared_chunks() {
        let idx = open_in_memory();
        idx.insert_add_preparation("preparation-a")
            .expect("preparation");
        idx.insert_batch("batch-a").expect("batch");

        assert!(matches!(
            idx.claim_prepared_chunks("preparation-a", "batch-a", &[(test_hash(0x94), 8)],),
            Err(StagingError::NotFound { .. })
        ));
    }

    proptest! {
        #[test]
        fn overlap_graph_claims_each_unique_chunk_once(
            files in prop::collection::vec(
                prop::collection::vec(0_u8..24, 1..32),
                1..8,
            )
        ) {
            let idx = open_in_memory();
            idx.insert_add_preparation("preparation-a").expect("preparation");
            let mut winners = HashMap::<u8, usize>::new();
            let mut expected = std::collections::HashSet::new();
            for (file_index, chunks) in files.iter().enumerate() {
                let batch_id = format!("batch-{file_index}");
                idx.insert_batch(&batch_id).expect("batch");
                idx.attach_add_preparation_batch("preparation-a", &batch_id)
                    .expect("attach batch");
                let claims = chunks
                    .iter()
                    .map(|seed| {
                        expected.insert(*seed);
                        (test_hash(*seed), 8)
                    })
                    .collect::<Vec<_>>();
                let outcomes = idx
                    .claim_prepared_chunks("preparation-a", &batch_id, &claims)
                    .expect("claim file chunks");
                for (seed, outcome) in chunks.iter().zip(outcomes) {
                    if outcome == PreparedChunkClaim::Claimed {
                        *winners.entry(*seed).or_default() += 1;
                    }
                }
            }

            prop_assert_eq!(winners.len(), expected.len());
            prop_assert!(winners.values().all(|count| *count == 1));
        }
    }

    #[test]
    fn open_push_snapshot_pins_superseded_path_until_retirement() {
        let idx = open_in_memory();
        let path = b"models/snapshot.bin";
        let first = insert_test_recipe_lease(&idx, "batch-a", path, 0xC1, 0xC2);
        idx.mark_batch_published("batch-a").expect("publish first");
        idx.create_push_snapshot("push-a", std::slice::from_ref(&first))
            .expect("snapshot first");

        let second = insert_test_recipe_lease(&idx, "batch-b", path, 0xD1, 0xD2);
        assert!(
            idx.mark_batch_published("batch-b")
                .expect("publish second")
                .is_empty(),
            "the open snapshot must retain the superseded file"
        );
        let old_lease: bool = idx
            .conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM path_leases WHERE recipe_hash = ?1
                )",
                params![first.hash().as_slice()],
                |row| row.get(0),
            )
            .expect("old lease");
        assert!(!old_lease);
        let snapshot_pin: bool = idx
            .conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM push_snapshot_recipes WHERE recipe_hash = ?1
                )",
                params![first.hash().as_slice()],
                |row| row.get(0),
            )
            .expect("snapshot recipe pin");
        assert!(snapshot_pin);
        let health = idx.lifecycle_health().expect("snapshot lifecycle health");
        assert_eq!(health.path_heads, 1);
        assert_eq!(health.path_leases, 1);
        assert_eq!(health.snapshot_pinned_superseded_leases, 1);
        assert_eq!(health.reclaimable_superseded_leases, 0);
        assert_eq!(health.reclaimable_files, 0);

        idx.commit_push_snapshot("push-a").expect("commit snapshot");
        let first_file_hash: [u8; 32] = first.file_hash().into();
        assert_eq!(
            idx.retire_push_snapshot("push-a").expect("retire snapshot"),
            vec![first_file_hash]
        );
        let second_file_hash: [u8; 32] = second.file_hash().into();
        assert_eq!(
            idx.published_recipe_for_file(&second_file_hash)
                .expect("current lookup"),
            Some(second)
        );
    }

    #[test]
    fn discarding_open_snapshot_releases_superseded_path_lease() {
        let idx = open_in_memory();
        let path = b"models/cancelled.bin";
        let first = insert_test_recipe_lease(&idx, "batch-a", path, 0xE1, 0xE2);
        idx.mark_batch_published("batch-a").expect("publish first");
        idx.create_push_snapshot("push-a", std::slice::from_ref(&first))
            .expect("snapshot first");

        let second = insert_test_recipe_lease(&idx, "batch-b", path, 0xF1, 0xF2);
        assert!(
            idx.mark_batch_published("batch-b")
                .expect("publish second")
                .is_empty()
        );

        let first_file_hash: [u8; 32] = first.file_hash().into();
        assert_eq!(
            idx.discard_open_push_snapshot("push-a")
                .expect("discard cancelled snapshot"),
            vec![first_file_hash]
        );
        let second_file_hash: [u8; 32] = second.file_hash().into();
        assert_eq!(
            idx.published_recipe_for_file(&second_file_hash)
                .expect("current lookup"),
            Some(second)
        );
        let old_lease: bool = idx
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM path_leases WHERE recipe_hash = ?1)",
                params![first.hash().as_slice()],
                |row| row.get(0),
            )
            .expect("old lease");
        assert!(!old_lease);
        assert_eq!(
            idx.published_recipe_for_file(&first_file_hash)
                .expect("superseded lookup"),
            None
        );
    }

    #[test]
    fn repeated_path_publication_keeps_one_head_and_one_ordinary_lease() {
        let idx = open_in_memory();
        let path = b"models/repeated.bin";
        let mut previous_file_hash: Option<[u8; 32]> = None;
        let mut current_recipe = None;

        for generation in 0..=250_u16 {
            let file_seed = u8::try_from(generation % 251).expect("bounded seed");
            let recipe_seed = file_seed.wrapping_add(1);
            let batch_id = format!("batch-{generation}");
            let recipe = insert_test_recipe_lease(&idx, &batch_id, path, file_seed, recipe_seed);
            let unowned = idx
                .mark_batch_published(&batch_id)
                .expect("publish replacement");
            if let Some(previous) = previous_file_hash {
                assert_eq!(unowned, vec![previous]);
            } else {
                assert!(unowned.is_empty());
            }
            previous_file_hash = Some(recipe.file_hash().into());
            current_recipe = Some(recipe);
        }

        let current_recipe = current_recipe.expect("current recipe");
        for repetition in 0..1_000_u16 {
            let batch_id = format!("batch-identical-{repetition}");
            idx.insert_batch(&batch_id).expect("identical batch");
            idx.insert_recipe_lease(
                &batch_id,
                path,
                &current_recipe,
                RecipeVerification::CallerVerified,
            )
            .expect("identical recipe lease");
            assert!(
                idx.mark_batch_published(&batch_id)
                    .expect("publish identical replacement")
                    .is_empty(),
                "an identical re-add transfers ownership without reclaiming shared payload"
            );
        }

        let counts: (i64, i64, i64) = idx
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM path_heads),
                        (SELECT COUNT(*) FROM path_leases),
                        (SELECT COUNT(*) FROM staging_batches)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("count canonical ownership");
        assert_eq!(counts, (1, 1, 1));
    }

    #[test]
    fn published_replacement_removes_superseded_empty_open_batch() {
        let idx = open_in_memory();
        let path = b"models/prepared.bin";
        let prepared = insert_test_recipe_lease(&idx, "batch-prepared", path, 0x31, 0x32);
        idx.insert_batch("batch-published")
            .expect("published batch");
        idx.insert_recipe_lease(
            "batch-published",
            path,
            &prepared,
            RecipeVerification::CallerVerified,
        )
        .expect("published lease");

        assert!(
            idx.mark_batch_published("batch-published")
                .expect("publish prepared replacement")
                .is_empty(),
            "the published owner keeps the same prepared recipe live"
        );

        let prepared_batch_exists: bool = idx
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM staging_batches WHERE batch_id = 'batch-prepared')",
                [],
                |row| row.get(0),
            )
            .expect("prepared batch existence");
        assert!(!prepared_batch_exists);
        let published_file_hash: [u8; 32] = prepared.file_hash().into();
        assert_eq!(
            idx.published_recipe_for_file(&published_file_hash)
                .expect("published recipe"),
            Some(prepared)
        );
    }

    #[test]
    fn rollback_preserves_snapshot_pin_and_rejects_published_batch() {
        let idx = open_in_memory();
        let path = b"models/rollback.bin";
        let first = insert_test_recipe_lease(&idx, "batch-first", path, 0x41, 0x42);
        idx.mark_batch_published("batch-first")
            .expect("publish first");
        idx.create_push_snapshot("push-first", std::slice::from_ref(&first))
            .expect("snapshot first");

        insert_test_recipe_lease(&idx, "batch-current", path, 0x51, 0x52);
        idx.mark_batch_published("batch-current")
            .expect("publish replacement");
        idx.insert_batch("batch-retry").expect("retry batch");
        idx.insert_recipe_lease(
            "batch-retry",
            b"models/retry.bin",
            &first,
            RecipeVerification::CallerVerified,
        )
        .expect("retry recipe lease");
        assert!(
            idx.rollback_batch("batch-retry")
                .expect("rollback open retry")
                .is_empty(),
            "the immutable snapshot recipe pin remains an owner"
        );

        idx.insert_batch("batch-intent-retry")
            .expect("intent retry batch");
        idx.insert_recipe_lease(
            "batch-intent-retry",
            b"models/intent-retry.bin",
            &first,
            RecipeVerification::CallerVerified,
        )
        .expect("intent retry recipe lease");
        idx.create_publication_intent(
            "intent-retry",
            &[(
                "batch-intent-retry".to_owned(),
                b"models/intent-retry.bin".to_vec(),
                "pointer".to_owned(),
                "absent".to_owned(),
            )],
        )
        .expect("retry publication intent");
        assert!(
            idx.rollback_publication_intent("intent-retry")
                .expect("rollback publication retry")
                .is_empty(),
            "publication rollback must also preserve the snapshot pin"
        );

        let error = idx
            .rollback_batch("batch-current")
            .expect_err("published ownership cannot roll back");
        assert!(matches!(error, StagingError::StagingCorrupt(_)));
        let first_file_hash: [u8; 32] = first.file_hash().into();
        assert_eq!(
            idx.discard_open_push_snapshot("push-first")
                .expect("release snapshot"),
            vec![first_file_hash]
        );
    }

    #[test]
    fn canonical_schema_reopens_without_change() {
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
    fn canonical_schema_rejects_missing_required_index() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path();

        let idx = Index::open(path).expect("first open");
        idx.conn
            .execute("DROP INDEX IF EXISTS recipe_payload_leases_by_chunk", [])
            .expect("drop chunk lease index");
        drop(idx);

        assert_staging_corrupt_contains(Index::open(path), "canonical v1");
    }

    #[test]
    fn canonical_schema_rejects_retired_publication_intent_shape() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path();

        let idx = Index::open(path).expect("first open");
        idx.conn
            .execute_batch(
                "DROP TABLE publication_intent_entries;
                 CREATE TABLE publication_intent_entries (
                    intent_id             TEXT NOT NULL,
                    batch_id              TEXT NOT NULL,
                    path_bytes            BLOB NOT NULL,
                    recipe_hash           BLOB NOT NULL,
                    expected_pointer_oid  TEXT NOT NULL,
                    PRIMARY KEY (intent_id, path_bytes)
                 );
                 CREATE INDEX publication_entries_by_batch
                    ON publication_intent_entries(batch_id, path_bytes);",
            )
            .expect("install retired publication table");
        drop(idx);

        assert_staging_corrupt_contains(Index::open(path), "canonical v1");
    }

    #[test]
    fn canonical_schema_rejects_remote_authority_without_placement_identity() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path();

        let idx = Index::open(path).expect("first open");
        idx.conn
            .execute(
                "ALTER TABLE recording_remote_chunks DROP COLUMN placement_id",
                [],
            )
            .expect("install retired remote authority table");
        drop(idx);

        assert_staging_corrupt_contains(Index::open(path), "canonical v1");
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
                .expect("headless lookup")
                .is_none(),
            "file/chunk rows without a published recipe lease cannot publish"
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
        let mut occurrence = 0u64;
        let mut offset = 0u64;
        for page in chunks.chunks(crate::stream::STAGE_BATCH_CHUNKS) {
            idx.append_recipe_recording_terms("batch-large", occurrence, offset, page)
                .expect("record repeated recipe terms");
            occurrence += u64::try_from(page.len()).expect("page length");
            offset += page.iter().map(|(_, size)| size).sum::<u64>();
        }

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
        idx.append_recipe_recording_terms(
            "batch-missing",
            0,
            0,
            &[(crab_xet::hash::MerkleHash::from(missing_chunk), 8)],
        )
        .expect("record missing recipe term");

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
    fn retired_schema_is_rejected_without_mutation() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        {
            let connection = Connection::open(&path).expect("open retired db");
            connection
                .execute("CREATE TABLE files (file_hash BLOB PRIMARY KEY)", [])
                .expect("create retired schema");
        }

        assert_staging_corrupt_contains(Index::open(&path), "canonical v1");

        let connection = Connection::open(&path).expect("reopen retired db");
        let tables: Vec<String> = connection
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .expect("prepare table inventory")
            .query_map([], |row| row.get(0))
            .expect("query table inventory")
            .collect::<std::result::Result<_, _>>()
            .expect("collect table inventory");
        assert_eq!(tables, vec!["files"]);
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
