//! WAL-mode SQLite journal for crash-safe restripe operations.
//!
//! The journal tracks per-source-xorb progress so that a crash or
//! SIGTERM at any point allows the next invocation to resume from
//! where it left off. An exclusive file lock ensures only one
//! restripe runs at a time per repository.
//!
//! Schema:
//! ```sql
//! CREATE TABLE runs (
//!   run_id       TEXT PRIMARY KEY,
//!   started_at   INTEGER NOT NULL,
//!   profile      TEXT NOT NULL,
//!   completed_at INTEGER,
//!   aborted      INTEGER NOT NULL DEFAULT 0,
//!   pid          INTEGER NOT NULL,
//!   schema_ver   INTEGER NOT NULL
//! );
//!
//! CREATE TABLE sources (
//!   run_id       TEXT NOT NULL,
//!   src_xorb     TEXT NOT NULL,
//!   status       TEXT NOT NULL,
//!   dest_xorbs   TEXT,
//!   started_at   INTEGER,
//!   completed_at INTEGER,
//!   err_kind     TEXT,
//!   err_msg      TEXT,
//!   PRIMARY KEY (run_id, src_xorb),
//!   FOREIGN KEY (run_id) REFERENCES runs(run_id)
//! );
//! ```

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use tracing::{debug, info};

use crate::core::error::{CrabError, Result};

/// Current on-disk schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Status values for source xorb entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    Pending,
    Staged,
    Done,
    Corrupt,
    Skipped,
}

impl SourceStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Staged => "staged",
            Self::Done => "done",
            Self::Corrupt => "corrupt",
            Self::Skipped => "skipped",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "staged" => Some(Self::Staged),
            "done" => Some(Self::Done),
            "corrupt" => Some(Self::Corrupt),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// A row from the `runs` table.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub run_id: String,
    pub started_at: i64,
    pub profile: String,
    pub completed_at: Option<i64>,
    pub aborted: bool,
    pub pid: u32,
    pub schema_ver: u32,
}

/// A row from the `sources` table.
#[derive(Debug, Clone)]
pub struct SourceRow {
    pub run_id: String,
    pub src_xorb: String,
    pub status: SourceStatus,
    pub dest_xorbs: Option<String>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub err_kind: Option<String>,
    pub err_msg: Option<String>,
}

/// Restripe journal handle.
///
/// Opens (or creates) the SQLite database at the given path in WAL
/// mode with an exclusive lock. If another process holds the lock,
/// returns `RestripeAlreadyInProgress`.
pub struct RestripeJournal {
    conn: Connection,
    path: PathBuf,
}

impl RestripeJournal {
    /// Open or create the journal database.
    ///
    /// Acquires an exclusive lock. Returns `RestripeAlreadyInProgress`
    /// if another process holds the lock.
    pub fn open(path: &Path) -> Result<Self> {
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CrabError::Io(e))?;
        }

        let conn = Connection::open(path).map_err(|e| {
            CrabError::Internal(format!(
                "failed to open restripe journal at {}: {e}",
                path.display()
            ))
        })?;

        // WAL mode for concurrent reads during writes.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| CrabError::Internal(format!("failed to set WAL mode: {e}")))?;

        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| CrabError::Internal(format!("failed to set synchronous mode: {e}")))?;

        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(|e| CrabError::Internal(format!("failed to set busy timeout: {e}")))?;

        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| CrabError::Internal(format!("failed to enable foreign keys: {e}")))?;

        // Exclusive locking mode — prevents concurrent restripe runs.
        conn.pragma_update(None, "locking_mode", "EXCLUSIVE")
            .map_err(|e| CrabError::Internal(format!("failed to set exclusive locking: {e}")))?;

        // Force the exclusive lock by writing to the database.
        // If another process holds the lock, this will fail.
        let lock_result = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _lock_probe (x INTEGER); \
             DROP TABLE IF EXISTS _lock_probe;",
        );

        if let Err(e) = lock_result {
            let err_str = e.to_string();
            if err_str.contains("locked") || err_str.contains("busy") {
                // Try to read the active run's PID for the error message.
                return Err(CrabError::RestripeAlreadyInProgress {
                    pid: 0,
                    started_at: "unknown".to_string(),
                });
            }
            return Err(CrabError::Internal(format!(
                "failed to acquire restripe journal lock: {e}"
            )));
        }

        // Create schema.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                run_id       TEXT PRIMARY KEY,
                started_at   INTEGER NOT NULL,
                profile      TEXT NOT NULL,
                completed_at INTEGER,
                aborted      INTEGER NOT NULL DEFAULT 0,
                pid          INTEGER NOT NULL,
                schema_ver   INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sources (
                run_id       TEXT NOT NULL,
                src_xorb     TEXT NOT NULL,
                status       TEXT NOT NULL,
                dest_xorbs   TEXT,
                started_at   INTEGER,
                completed_at INTEGER,
                err_kind     TEXT,
                err_msg      TEXT,
                PRIMARY KEY (run_id, src_xorb),
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
            );

            CREATE INDEX IF NOT EXISTS idx_sources_status
                ON sources(run_id, status);",
        )
        .map_err(|e| {
            CrabError::Internal(format!("failed to create restripe journal schema: {e}"))
        })?;

        info!(path = %path.display(), "restripe journal opened");

        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// Path to the journal database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start a new restripe run. Returns the run ID.
    pub fn start_run(&self, run_id: &str, profile_json: &str) -> Result<()> {
        let now = epoch_secs();
        let pid = std::process::id();

        self.conn
            .execute(
                "INSERT INTO runs (run_id, started_at, profile, pid, schema_ver)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run_id, now, profile_json, pid, SCHEMA_VERSION],
            )
            .map_err(|e| CrabError::Internal(format!("failed to insert run: {e}")))?;

        debug!(run_id, pid, "restripe run started");
        Ok(())
    }

    /// Mark a run as completed.
    pub fn complete_run(&self, run_id: &str) -> Result<()> {
        let now = epoch_secs();
        self.conn
            .execute(
                "UPDATE runs SET completed_at = ?1 WHERE run_id = ?2",
                params![now, run_id],
            )
            .map_err(|e| CrabError::Internal(format!("failed to complete run: {e}")))?;
        Ok(())
    }

    /// Mark a run as aborted.
    pub fn abort_run(&self, run_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET aborted = 1 WHERE run_id = ?1",
                params![run_id],
            )
            .map_err(|e| CrabError::Internal(format!("failed to abort run: {e}")))?;
        Ok(())
    }

    /// Get the most recent incomplete (non-completed, non-aborted) run.
    pub fn active_run(&self) -> Result<Option<RunRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT run_id, started_at, profile, completed_at, aborted, pid, schema_ver
                 FROM runs
                 WHERE completed_at IS NULL AND aborted = 0
                 ORDER BY started_at DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok(RunRow {
                        run_id: row.get(0)?,
                        started_at: row.get(1)?,
                        profile: row.get(2)?,
                        completed_at: row.get(3)?,
                        aborted: row.get::<_, i64>(4)? != 0,
                        pid: row.get::<_, u32>(5)?,
                        schema_ver: row.get::<_, u32>(6)?,
                    })
                },
            )
            .optional()
            .map_err(|e| CrabError::Internal(format!("failed to query active run: {e}")))?;
        Ok(row)
    }

    /// Insert a source xorb entry with `pending` status.
    pub fn insert_source(&self, run_id: &str, src_xorb: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO sources (run_id, src_xorb, status)
                 VALUES (?1, ?2, 'pending')",
                params![run_id, src_xorb],
            )
            .map_err(|e| CrabError::Internal(format!("failed to insert source: {e}")))?;
        Ok(())
    }

    /// Update a source xorb's status.
    pub fn update_source_status(
        &self,
        run_id: &str,
        src_xorb: &str,
        status: SourceStatus,
        dest_xorbs: Option<&str>,
    ) -> Result<()> {
        let now = epoch_secs();
        self.conn
            .execute(
                "UPDATE sources
                 SET status = ?1, dest_xorbs = ?2, completed_at = ?3
                 WHERE run_id = ?4 AND src_xorb = ?5",
                params![status.as_str(), dest_xorbs, now, run_id, src_xorb],
            )
            .map_err(|e| CrabError::Internal(format!("failed to update source status: {e}")))?;
        Ok(())
    }

    /// Mark a source as corrupt with an error message.
    pub fn mark_corrupt(
        &self,
        run_id: &str,
        src_xorb: &str,
        err_kind: &str,
        err_msg: &str,
    ) -> Result<()> {
        let now = epoch_secs();
        self.conn
            .execute(
                "UPDATE sources
                 SET status = 'corrupt', err_kind = ?1, err_msg = ?2, completed_at = ?3
                 WHERE run_id = ?4 AND src_xorb = ?5",
                params![err_kind, err_msg, now, run_id, src_xorb],
            )
            .map_err(|e| CrabError::Internal(format!("failed to mark source corrupt: {e}")))?;
        Ok(())
    }

    /// Get all sources with a given status for a run.
    pub fn sources_by_status(&self, run_id: &str, status: SourceStatus) -> Result<Vec<SourceRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT run_id, src_xorb, status, dest_xorbs, started_at,
                        completed_at, err_kind, err_msg
                 FROM sources
                 WHERE run_id = ?1 AND status = ?2",
            )
            .map_err(|e| CrabError::Internal(format!("failed to prepare sources query: {e}")))?;

        let rows = stmt
            .query_map(params![run_id, status.as_str()], |row| {
                let status_str: String = row.get(2)?;
                Ok(SourceRow {
                    run_id: row.get(0)?,
                    src_xorb: row.get(1)?,
                    status: SourceStatus::from_str(&status_str).unwrap_or(SourceStatus::Pending),
                    dest_xorbs: row.get(3)?,
                    started_at: row.get(4)?,
                    completed_at: row.get(5)?,
                    err_kind: row.get(6)?,
                    err_msg: row.get(7)?,
                })
            })
            .map_err(|e| CrabError::Internal(format!("failed to query sources: {e}")))?;

        let mut result = Vec::new();
        for row in rows {
            result.push(
                row.map_err(|e| CrabError::Internal(format!("failed to read source row: {e}")))?,
            );
        }
        Ok(result)
    }

    /// Count sources by status for a run.
    pub fn count_by_status(&self, run_id: &str) -> Result<StatusCounts> {
        let mut counts = StatusCounts::default();
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM sources WHERE run_id = ?1 GROUP BY status")
            .map_err(|e| CrabError::Internal(format!("failed to prepare count query: {e}")))?;

        let rows = stmt
            .query_map(params![run_id], |row| {
                let status: String = row.get(0)?;
                let count: u64 = row.get(1)?;
                Ok((status, count))
            })
            .map_err(|e| CrabError::Internal(format!("failed to count sources: {e}")))?;

        for row in rows {
            let (status, count) =
                row.map_err(|e| CrabError::Internal(format!("failed to read count row: {e}")))?;
            match status.as_str() {
                "pending" => counts.pending = count,
                "staged" => counts.staged = count,
                "done" => counts.done = count,
                "corrupt" => counts.corrupt = count,
                "skipped" => counts.skipped = count,
                _ => {}
            }
        }
        Ok(counts)
    }

    /// Drop the journal database file. Requires explicit confirmation.
    pub fn drop_journal(path: &Path) -> Result<()> {
        for suffix in &["", "-wal", "-shm"] {
            let p = path.with_extension(
                path.extension()
                    .map(|e| format!("{}{suffix}", e.to_string_lossy()))
                    .unwrap_or_else(|| suffix.to_string()),
            );
            if p.exists() {
                std::fs::remove_file(&p).map_err(|e| CrabError::Io(e))?;
            }
        }
        // Also try the base path directly.
        if path.exists() {
            std::fs::remove_file(path).map_err(|e| CrabError::Io(e))?;
        }
        info!(path = %path.display(), "restripe journal dropped");
        Ok(())
    }

    /// Check if a journal file exists at the given path.
    pub fn exists(path: &Path) -> bool {
        path.exists()
    }
}

/// Counts of source xorbs by status.
#[derive(Debug, Clone, Default)]
pub struct StatusCounts {
    pub pending: u64,
    pub staged: u64,
    pub done: u64,
    pub corrupt: u64,
    pub skipped: u64,
}

impl StatusCounts {
    /// Total number of sources.
    pub fn total(&self) -> u64 {
        self.pending + self.staged + self.done + self.corrupt + self.skipped
    }
}

/// Current epoch time in seconds.
fn epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_journal() -> (tempfile::TempDir, RestripeJournal) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = RestripeJournal::open(&path).unwrap();
        (dir, journal)
    }

    #[test]
    fn open_creates_schema() {
        let (_dir, journal) = temp_journal();
        // Verify tables exist by querying them.
        assert!(journal.active_run().unwrap().is_none());
    }

    #[test]
    fn start_and_complete_run() {
        let (_dir, journal) = temp_journal();

        journal.start_run("run-001", r#"{"profile":"ml"}"#).unwrap();

        let run = journal.active_run().unwrap().unwrap();
        assert_eq!(run.run_id, "run-001");
        assert!(!run.aborted);
        assert!(run.completed_at.is_none());

        journal.complete_run("run-001").unwrap();
        // No active run after completion.
        assert!(journal.active_run().unwrap().is_none());
    }

    #[test]
    fn abort_run() {
        let (_dir, journal) = temp_journal();

        journal.start_run("run-002", "{}").unwrap();
        journal.abort_run("run-002").unwrap();

        // Aborted runs are not active.
        assert!(journal.active_run().unwrap().is_none());
    }

    #[test]
    fn source_lifecycle() {
        let (_dir, journal) = temp_journal();

        journal.start_run("run-003", "{}").unwrap();
        journal.insert_source("run-003", "xorb-aaa").unwrap();
        journal.insert_source("run-003", "xorb-bbb").unwrap();

        let pending = journal
            .sources_by_status("run-003", SourceStatus::Pending)
            .unwrap();
        assert_eq!(pending.len(), 2);

        journal
            .update_source_status(
                "run-003",
                "xorb-aaa",
                SourceStatus::Done,
                Some(r#"["dest-001"]"#),
            )
            .unwrap();

        journal
            .mark_corrupt("run-003", "xorb-bbb", "hash_mismatch", "bad hash")
            .unwrap();

        let counts = journal.count_by_status("run-003").unwrap();
        assert_eq!(counts.done, 1);
        assert_eq!(counts.corrupt, 1);
        assert_eq!(counts.pending, 0);
        assert_eq!(counts.total(), 2);
    }

    #[test]
    fn insert_source_is_idempotent() {
        let (_dir, journal) = temp_journal();

        journal.start_run("run-004", "{}").unwrap();
        journal.insert_source("run-004", "xorb-aaa").unwrap();
        // Second insert is a no-op (INSERT OR IGNORE).
        journal.insert_source("run-004", "xorb-aaa").unwrap();

        let pending = journal
            .sources_by_status("run-004", SourceStatus::Pending)
            .unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn schema_version_stored() {
        let (_dir, journal) = temp_journal();

        journal.start_run("run-005", "{}").unwrap();
        let run = journal.active_run().unwrap().unwrap();
        assert_eq!(run.schema_ver, SCHEMA_VERSION);
    }

    #[test]
    fn concurrent_run_detection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");

        let _j1 = RestripeJournal::open(&path).unwrap();

        // Second open should fail with RestripeAlreadyInProgress.
        let result = RestripeJournal::open(&path);
        assert!(result.is_err(), "second journal open should fail");
    }
}
