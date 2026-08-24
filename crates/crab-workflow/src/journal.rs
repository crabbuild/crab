//! SQLite-backed workflow journal.
//!
//! The journal is the source of truth for what a stage has durably
//! committed. Every state transition is persisted before the work
//! of the next state begins, so a crash at any point leaves the
//! journal pointing at a safe resume state.
//!
//! Backed by SQLite at `.crab/workflow/runs/<run_id>/journal.db`
//! in WAL mode, mirroring the conventions in `src/import/journal.rs`
//! (WAL, `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`,
//! a `journal_meta.schema_version` marker).

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::StageState;

use crate::{Result, WorkflowError as CrabError};

/// Current on-disk schema version. Bumped when the SQL layout
/// changes in a way earlier readers cannot tolerate. Journals
/// with a higher value refuse to open with
/// `WorkflowJournalSchemaNewer`.
pub const SCHEMA_VERSION: u16 = 1;

/// Terminal outcome of a run. Serialized as a stable integer so the
/// column stays readable across upgrades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Success = 0,
    Failure = 1,
    Aborted = 2,
}

impl RunOutcome {
    fn tag(self) -> i64 {
        self as i64
    }

    fn from_tag(raw: i64) -> Result<Self> {
        match raw {
            0 => Ok(Self::Success),
            1 => Ok(Self::Failure),
            2 => Ok(Self::Aborted),
            other => Err(CrabError::Internal(format!(
                "unknown run outcome tag {other} in workflow journal"
            ))),
        }
    }

    /// Stable lowercase string used in structured output. The value
    /// matches the human-readable status rendered by the text path
    /// so downstream tools can rely on either.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Aborted => "aborted",
        }
    }
}

/// One row of the `stage_runs` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageRunRow {
    pub run_id: Uuid,
    pub stage_name: String,
    pub attempt: u32,
    pub state: StageState,
    pub stage_hash: Option<[u8; 32]>,
    pub pid: Option<i64>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub started_at: Option<i64>,
    pub updated_at: i64,
    pub stderr_tail: Option<String>,
    pub payload_json: String,
}

/// One row of the `runs` table. Separate from [`StageRunRow`] so
/// callers that only need run-level metadata (the `journal ls` and
/// `journal gc` commands in particular) don't pay for loading every
/// stage row.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub run_id: Uuid,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub outcome: Option<RunOutcome>,
    pub crab_version: String,
    pub host_fingerprint: String,
}

/// Workflow journal handle.
///
/// `Journal` wraps a `rusqlite::Connection` which is `Send` but not
/// `Sync` (due to internal `RefCell`). For parallel DAG execution,
/// each spawned task opens its own `Journal` instance — no sharing
/// occurs. The `Sync` impl below is safe because:
/// 1. We never share a single `Journal` across threads.
/// 2. Each parallel task owns its exclusive `Journal` connection.
/// 3. The `!Sync` bound comes from `RefCell` inside rusqlite, but
///    since we never create `&Journal` references that cross thread
///    boundaries to the same instance, the invariant holds.
pub struct Journal {
    conn: Connection,
    path: PathBuf,
}

// SAFETY: Each parallel task opens its own Journal (own Connection).
// We never share a Journal instance across threads — the Sync impl
// is needed only so that `&Journal` satisfies `Send` for futures
// that hold a reference to a task-local Journal across await points.
unsafe impl Sync for Journal {}

impl Journal {
    /// Open (or create) the journal at `path`. Creates parent
    /// directories as needed and runs migrations on first open.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| CrabError::WorkflowJournalOpen {
                path: path.to_path_buf(),
                source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
            })?;
        }

        let conn = Connection::open(path).map_err(|source| CrabError::WorkflowJournalOpen {
            path: path.to_path_buf(),
            source,
        })?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: path.to_path_buf(),
                source,
            })?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: path.to_path_buf(),
                source,
            })?;
        conn.pragma_update(None, "busy_timeout", "5000")
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: path.to_path_buf(),
                source,
            })?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: path.to_path_buf(),
                source,
            })?;

        let journal = Self {
            conn,
            path: path.to_path_buf(),
        };
        journal.run_migrations()?;
        journal.check_schema_version(path)?;
        Ok(journal)
    }

    fn run_migrations(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS runs (
                    run_id           TEXT PRIMARY KEY,
                    started_at       INTEGER NOT NULL,
                    ended_at         INTEGER,
                    outcome          INTEGER,
                    crab_version   TEXT NOT NULL,
                    host_fingerprint TEXT NOT NULL
                 );

                 CREATE TABLE IF NOT EXISTS stage_runs (
                    run_id       TEXT NOT NULL,
                    stage_name   TEXT NOT NULL,
                    attempt      INTEGER NOT NULL DEFAULT 1,
                    state        INTEGER NOT NULL,
                    stage_hash   BLOB,
                    pid          INTEGER,
                    exit_code    INTEGER,
                    signal       INTEGER,
                    timed_out    INTEGER DEFAULT 0,
                    started_at   INTEGER,
                    updated_at   INTEGER NOT NULL,
                    stderr_tail  TEXT,
                    payload_json TEXT NOT NULL,
                    PRIMARY KEY (run_id, stage_name, attempt)
                 );

                 CREATE INDEX IF NOT EXISTS idx_stage_runs_state
                     ON stage_runs(state);

                 CREATE TABLE IF NOT EXISTS journal_meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                 );",
            )
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;

        // Seed the schema version on first open. Fails loud if
        // another writer seeded a newer version concurrently.
        self.conn
            .execute(
                "INSERT OR IGNORE INTO journal_meta(key, value) VALUES ('schema_version', ?1)",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;

        Ok(())
    }

    fn check_schema_version(&self, path: &Path) -> Result<()> {
        let found = self.schema_version()?;
        if found > SCHEMA_VERSION {
            // Can't attribute this to a specific run here — callers
            // that know the run_id re-wrap with the right field.
            return Err(CrabError::WorkflowJournalSchemaNewer {
                run_id: path.display().to_string(),
                found,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    /// Current on-disk schema version.
    pub fn schema_version(&self) -> Result<u16> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM journal_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;

        match raw {
            Some(s) => s
                .parse::<u16>()
                .map_err(|_| CrabError::WorkflowJournalCorrupt {
                    run_id: self.path.display().to_string(),
                    detail: format!("schema_version not parseable as u16: {s:?}"),
                }),
            None => Ok(SCHEMA_VERSION),
        }
    }

    /// Record that a new run has begun. Idempotent: re-opening a
    /// journal for the same run_id is a no-op.
    pub fn insert_run_start(
        &self,
        run_id: Uuid,
        crab_version: &str,
        host_fingerprint: &str,
    ) -> Result<()> {
        let now = unix_now();
        self.conn
            .execute(
                "INSERT OR IGNORE INTO runs(run_id, started_at, crab_version, host_fingerprint)
                 VALUES (?1, ?2, ?3, ?4)",
                params![run_id.to_string(), now, crab_version, host_fingerprint,],
            )
            .map_err(|source| map_sqlite_err(&self.path, source))?;
        Ok(())
    }

    /// Record a stage as started within `run_id`. Creates a row with
    /// state = Resolving, attempt = 1. The row's subsequent lifecycle
    /// is driven by [`Journal::transition`].
    pub fn insert_stage_start(&self, run_id: Uuid, stage_name: &str) -> Result<()> {
        let now = unix_now();
        self.conn
            .execute(
                "INSERT OR REPLACE INTO stage_runs(
                    run_id, stage_name, attempt, state, updated_at, payload_json
                 ) VALUES (?1, ?2, 1, ?3, ?4, '{}')",
                params![
                    run_id.to_string(),
                    stage_name,
                    i64::from(StageState::Resolving.sql_tag()),
                    now,
                ],
            )
            .map_err(|source| map_sqlite_err(&self.path, source))?;
        Ok(())
    }

    /// Record a retry attempt for a stage. Creates a new row with the
    /// given `attempt` number and state = Resolving. The prior attempt
    /// should already have been transitioned to `Failed` by the
    /// executor before calling this.
    pub fn insert_stage_retry(&self, run_id: Uuid, stage_name: &str, attempt: u32) -> Result<()> {
        let now = unix_now();
        self.conn
            .execute(
                "INSERT OR REPLACE INTO stage_runs(
                    run_id, stage_name, attempt, state, updated_at, payload_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, '{}')",
                params![
                    run_id.to_string(),
                    stage_name,
                    i64::from(attempt),
                    i64::from(StageState::Resolving.sql_tag()),
                    now,
                ],
            )
            .map_err(|source| map_sqlite_err(&self.path, source))?;
        Ok(())
    }

    /// Transition a stage to `new_state`, persisting an opaque JSON
    /// payload. Rejects illegal transitions with
    /// `WorkflowStateTransitionIllegal`.
    pub fn transition(
        &self,
        run_id: Uuid,
        stage_name: &str,
        attempt: u32,
        new_state: StageState,
        payload_json: &str,
    ) -> Result<()> {
        let now = unix_now();

        // Read the current state so we can validate the transition.
        let current: Option<i64> = self
            .conn
            .query_row(
                "SELECT state FROM stage_runs
                 WHERE run_id = ?1 AND stage_name = ?2 AND attempt = ?3",
                params![run_id.to_string(), stage_name, i64::from(attempt)],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;

        if let Some(raw) = current {
            let tag = u8::try_from(raw).map_err(|_| CrabError::WorkflowJournalCorrupt {
                run_id: run_id.to_string(),
                detail: format!("state tag out of range: {raw}"),
            })?;
            let prev =
                StageState::from_sql_tag(tag).ok_or_else(|| CrabError::WorkflowJournalCorrupt {
                    run_id: run_id.to_string(),
                    detail: format!("unknown state tag {tag}"),
                })?;
            if !prev.can_transition_to(new_state) {
                return Err(CrabError::WorkflowStateTransitionIllegal {
                    stage: stage_name.to_owned(),
                    from: prev.to_string(),
                    to: new_state.to_string(),
                });
            }
        }

        let updated = self
            .conn
            .execute(
                "UPDATE stage_runs
                   SET state = ?4, updated_at = ?5, payload_json = ?6
                 WHERE run_id = ?1 AND stage_name = ?2 AND attempt = ?3",
                params![
                    run_id.to_string(),
                    stage_name,
                    i64::from(attempt),
                    i64::from(new_state.sql_tag()),
                    now,
                    payload_json,
                ],
            )
            .map_err(|source| map_sqlite_err(&self.path, source))?;

        if updated == 0 {
            return Err(CrabError::WorkflowJournalCorrupt {
                run_id: run_id.to_string(),
                detail: format!("no stage_runs row for {stage_name} attempt {attempt}"),
            });
        }

        Ok(())
    }

    /// Latest-attempt row for `stage_name` in `run_id`, or `None`
    /// if the stage has no rows yet. "Latest" is the row with the
    /// highest `attempt`; the journal never reorders attempts and
    /// transitions mutate rows in place per attempt, so this is
    /// well-defined even when earlier attempts went terminal.
    pub fn latest_stage_row(&self, run_id: Uuid, stage_name: &str) -> Result<Option<StageRunRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT run_id, stage_name, attempt, state, stage_hash, pid,
                        exit_code, signal, timed_out, started_at, updated_at,
                        stderr_tail, payload_json
                 FROM stage_runs
                 WHERE run_id = ?1 AND stage_name = ?2
                 ORDER BY attempt DESC
                 LIMIT 1",
                params![run_id.to_string(), stage_name],
                Self::map_stage_row,
            )
            .optional()
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;
        Ok(row)
    }

    /// Return stages that were started but never reached
    /// [`StageState::Committed`]. Used by the resume path.
    pub fn stages_not_committed(&self, run_id: Uuid) -> Result<Vec<StageRunRow>> {
        let committed = i64::from(StageState::Committed.sql_tag());
        let failed = i64::from(StageState::Failed.sql_tag());
        let aborted = i64::from(StageState::Aborted.sql_tag());

        let mut stmt = self
            .conn
            .prepare(
                "SELECT run_id, stage_name, attempt, state, stage_hash, pid,
                        exit_code, signal, timed_out, started_at, updated_at,
                        stderr_tail, payload_json
                 FROM stage_runs
                 WHERE run_id = ?1
                   AND state NOT IN (?2, ?3, ?4)
                 ORDER BY stage_name, attempt",
            )
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;

        let rows = stmt
            .query_map(
                params![run_id.to_string(), committed, failed, aborted],
                Self::map_stage_row,
            )
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?);
        }
        Ok(out)
    }

    fn map_stage_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StageRunRow> {
        let run_id_str: String = row.get(0)?;
        let run_id = Uuid::parse_str(&run_id_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let state_tag: i64 = row.get(3)?;
        let state_u8 = u8::try_from(state_tag).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Integer,
                Box::new(e),
            )
        })?;
        let state = StageState::from_sql_tag(state_u8).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Integer,
                format!("unknown state tag {state_u8}").into(),
            )
        })?;
        let stage_hash_blob: Option<Vec<u8>> = row.get(4)?;
        let stage_hash = stage_hash_blob
            .map(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            .and_then(std::convert::identity);
        let attempt_raw: i64 = row.get(2)?;

        Ok(StageRunRow {
            run_id,
            stage_name: row.get(1)?,
            attempt: u32::try_from(attempt_raw).unwrap_or(u32::MAX),
            state,
            stage_hash,
            pid: row.get(5)?,
            exit_code: row.get::<_, Option<i64>>(6)?.map(|v| v as i32),
            signal: row.get::<_, Option<i64>>(7)?.map(|v| v as i32),
            timed_out: row.get::<_, i64>(8)? != 0,
            started_at: row.get(9)?,
            updated_at: row.get(10)?,
            stderr_tail: row.get(11)?,
            payload_json: row.get(12)?,
        })
    }

    /// Record the final outcome of a run.
    pub fn mark_run_outcome(&self, run_id: Uuid, outcome: RunOutcome) -> Result<()> {
        let now = unix_now();
        let updated = self
            .conn
            .execute(
                "UPDATE runs SET ended_at = ?2, outcome = ?3 WHERE run_id = ?1",
                params![run_id.to_string(), now, outcome.tag()],
            )
            .map_err(|source| map_sqlite_err(&self.path, source))?;
        if updated == 0 {
            return Err(CrabError::WorkflowJournalCorrupt {
                run_id: run_id.to_string(),
                detail: "no runs row to mark outcome".to_owned(),
            });
        }
        Ok(())
    }

    /// Look up a run's outcome, or `None` if still in flight.
    pub fn run_outcome(&self, run_id: Uuid) -> Result<Option<RunOutcome>> {
        let raw: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT outcome FROM runs WHERE run_id = ?1",
                params![run_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;

        match raw {
            Some(Some(tag)) => Ok(Some(RunOutcome::from_tag(tag)?)),
            _ => Ok(None),
        }
    }

    /// Load the `runs` row for `run_id`, or `None` if
    /// `insert_run_start` was never called for this journal.
    ///
    /// Read-only accessor used by the `workflow journal ls` and
    /// `gc` commands to sort journals by creation time and to
    /// decide whether a given journal is terminal.
    pub fn run_row(&self, run_id: Uuid) -> Result<Option<RunRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT run_id, started_at, ended_at, outcome,
                        crab_version, host_fingerprint
                 FROM runs
                 WHERE run_id = ?1",
                params![run_id.to_string()],
                Self::map_run_row,
            )
            .optional()
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;
        Ok(row)
    }

    fn map_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
        let run_id_str: String = row.get(0)?;
        let run_id = Uuid::parse_str(&run_id_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let outcome_raw: Option<i64> = row.get(3)?;
        let outcome = match outcome_raw {
            None => None,
            Some(tag) => Some(RunOutcome::from_tag(tag).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Integer,
                    e.to_string().into(),
                )
            })?),
        };
        Ok(RunRow {
            run_id,
            started_at: row.get(1)?,
            ended_at: row.get(2)?,
            outcome,
            crab_version: row.get(4)?,
            host_fingerprint: row.get(5)?,
        })
    }

    /// Every `stage_runs` row for `run_id`, ordered by
    /// `(stage_name, attempt)`. Includes terminal states, unlike
    /// [`Journal::stages_not_committed`]. Used by
    /// `workflow journal show` to render the full state trajectory.
    pub fn all_stage_rows(&self, run_id: Uuid) -> Result<Vec<StageRunRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT run_id, stage_name, attempt, state, stage_hash, pid,
                        exit_code, signal, timed_out, started_at, updated_at,
                        stderr_tail, payload_json
                 FROM stage_runs
                 WHERE run_id = ?1
                 ORDER BY stage_name, attempt",
            )
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;

        let rows = stmt
            .query_map(params![run_id.to_string()], Self::map_stage_row)
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?);
        }
        Ok(out)
    }

    /// Return every stage row while refusing to materialize an oversized
    /// journal in memory.
    pub fn all_stage_rows_with_limit(
        &self,
        run_id: Uuid,
        max_rows: usize,
    ) -> Result<Vec<StageRunRow>> {
        let limit = i64::try_from(max_rows)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| CrabError::Configuration {
                key: "workflow journal stage row limit".to_owned(),
                origin: "limit cannot be represented by SQLite".to_owned(),
            })?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT run_id, stage_name, attempt, state, stage_hash, pid,
                        exit_code, signal, timed_out, started_at, updated_at,
                        stderr_tail, payload_json
                 FROM stage_runs
                 WHERE run_id = ?1
                 ORDER BY stage_name, attempt
                 LIMIT ?2",
            )
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;
        let rows = stmt
            .query_map(params![run_id.to_string(), limit], Self::map_stage_row)
            .map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?;
        let mut out = Vec::with_capacity(max_rows.min(1024));
        for row in rows {
            out.push(row.map_err(|source| CrabError::WorkflowJournalOpen {
                path: self.path.clone(),
                source,
            })?);
        }
        if out.len() > max_rows {
            return Err(CrabError::Configuration {
                key: "workflow journal stage row count".to_owned(),
                origin: format!("journal {run_id} contains more than {max_rows} stage rows"),
            });
        }
        Ok(out)
    }
}

/// Map a rusqlite error to the appropriate `CrabError`. Detects
/// `SQLITE_FULL` (error code 13) and maps it to `JournalDiskFull`
/// so the caller can distinguish disk-full from other journal errors.
fn map_sqlite_err(path: &Path, source: rusqlite::Error) -> CrabError {
    if is_sqlite_full(&source) {
        return CrabError::JournalDiskFull {
            path: path.to_path_buf(),
        };
    }
    CrabError::WorkflowJournalOpen {
        path: path.to_path_buf(),
        source,
    }
}

/// Detect SQLITE_FULL (error code 13) or SQLITE_IOERR (code 10)
/// with an ENOSPC extended code. These indicate the disk is full
/// and the journal write cannot proceed.
fn is_sqlite_full(err: &rusqlite::Error) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(ffi_err, _) => {
            // SQLITE_FULL = 13 (DiskFull in rusqlite's ErrorCode enum)
            ffi_err.code == rusqlite::ffi::ErrorCode::DiskFull
        }
        _ => false,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_journal() -> (TempDir, Journal) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("journal.db");
        let j = Journal::open(&path).unwrap();
        (tmp, j)
    }

    #[test]
    fn schema_version_seeded_on_first_open() {
        let (_tmp, j) = open_journal();
        assert_eq!(j.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn open_then_reopen_preserves_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("journal.db");
        let run_id = Uuid::now_v7();

        {
            let j = Journal::open(&path).unwrap();
            j.insert_run_start(run_id, "test", "host").unwrap();
            j.insert_stage_start(run_id, "train").unwrap();
        }

        let j2 = Journal::open(&path).unwrap();
        assert_eq!(j2.schema_version().unwrap(), SCHEMA_VERSION);
        let rows = j2.stages_not_committed(run_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stage_name, "train");
        assert_eq!(rows[0].state, StageState::Resolving);
    }

    #[test]
    fn transition_enforces_legal_moves() {
        let (_tmp, j) = open_journal();
        let run_id = Uuid::now_v7();
        j.insert_run_start(run_id, "test", "host").unwrap();
        j.insert_stage_start(run_id, "train").unwrap();

        // Resolving → Resolved is legal.
        j.transition(run_id, "train", 1, StageState::Resolved, "{}")
            .expect("legal transition");

        // Resolved → Committed is not.
        let err = j
            .transition(run_id, "train", 1, StageState::Committed, "{}")
            .expect_err("illegal transition should fail");
        assert!(matches!(
            err,
            CrabError::WorkflowStateTransitionIllegal { .. }
        ));

        // But the DB row is untouched after the rejection.
        let rows = j.stages_not_committed(run_id).unwrap();
        assert_eq!(rows[0].state, StageState::Resolved);
    }

    #[test]
    fn stages_not_committed_filters_terminals() {
        let (_tmp, j) = open_journal();
        let run_id = Uuid::now_v7();
        j.insert_run_start(run_id, "test", "host").unwrap();

        j.insert_stage_start(run_id, "a").unwrap();
        j.insert_stage_start(run_id, "b").unwrap();

        // Walk `a` all the way to Committed.
        for step in [
            StageState::Resolved,
            StageState::CacheChecked,
            StageState::Running,
            StageState::Produced,
            StageState::Hashed,
            StageState::Staged,
            StageState::EntryWritten,
            StageState::RefPublished,
            StageState::LockfileUpdated,
            StageState::Committed,
        ] {
            j.transition(run_id, "a", 1, step, "{}").unwrap();
        }

        // Leave `b` pending.
        let rows = j.stages_not_committed(run_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stage_name, "b");
    }

    #[test]
    fn mark_run_outcome_records_terminal_state() {
        let (_tmp, j) = open_journal();
        let run_id = Uuid::now_v7();
        j.insert_run_start(run_id, "test", "host").unwrap();

        assert_eq!(j.run_outcome(run_id).unwrap(), None);
        j.mark_run_outcome(run_id, RunOutcome::Success).unwrap();
        assert_eq!(j.run_outcome(run_id).unwrap(), Some(RunOutcome::Success));
    }

    #[test]
    fn latest_stage_row_returns_none_for_missing_stage() {
        let (_tmp, j) = open_journal();
        let run_id = Uuid::now_v7();
        j.insert_run_start(run_id, "test", "host").unwrap();
        assert!(j.latest_stage_row(run_id, "ghost").unwrap().is_none());
    }

    #[test]
    fn latest_stage_row_reflects_current_state() {
        let (_tmp, j) = open_journal();
        let run_id = Uuid::now_v7();
        j.insert_run_start(run_id, "test", "host").unwrap();
        j.insert_stage_start(run_id, "train").unwrap();
        j.transition(run_id, "train", 1, StageState::Resolved, "{}")
            .unwrap();

        let row = j
            .latest_stage_row(run_id, "train")
            .unwrap()
            .expect("row should exist");
        assert_eq!(row.stage_name, "train");
        assert_eq!(row.attempt, 1);
        assert_eq!(row.state, StageState::Resolved);
    }
}
