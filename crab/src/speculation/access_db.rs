//! SQLite-backed co-access database for speculative hydration.
//!
//! Tracks which files are accessed together within a time window so the
//! speculative hydration driver can pre-fetch likely neighbors. The DB
//! lives under the current worktree's Crab state directory (local to the
//! workspace, never pushed).
//!
//! [`AccessDb`] is the synchronous core; [`AsyncAccessDb`] wraps it in
//! `Arc<Mutex<>>` and dispatches through `spawn_blocking` so callers on
//! the tokio runtime never block.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, params};
use tracing::{debug, warn};

use crate::core::error::CrabError;

type Result<T> = std::result::Result<T, CrabError>;

pub const ACCESS_DB_FILENAME: &str = "access.db";

/// Build the canonical per-worktree access database path for a resolved context.
#[must_use]
pub fn path_for_context(ctx: &crate::git::worktree::WorktreeContext) -> PathBuf {
    ctx.per_worktree_crab_dir.join(ACCESS_DB_FILENAME)
}

/// Build the canonical per-worktree access database path for `worktree_root`.
pub fn path_for_worktree_root(worktree_root: &Path) -> Result<PathBuf> {
    let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(worktree_root)?;
    Ok(path_for_context(&ctx))
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single access event row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessEvent {
    pub path: String,
    pub ts_ms: i64,
    pub run_id: String,
}

// ---------------------------------------------------------------------------
// Synchronous core
// ---------------------------------------------------------------------------

/// Synchronous SQLite-backed co-access database.
///
/// All methods are blocking; async callers should use [`AsyncAccessDb`].
pub struct AccessDb {
    conn: Connection,
    db_path: PathBuf,
}

impl AccessDb {
    /// Open (or create) the access database at `db_path` and run migrations.
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path).map_err(|e| CrabError::SpeculationDb {
            path: db_path.to_path_buf(),
            source: e,
        })?;

        // WAL mode for concurrent readers + single writer.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| CrabError::SpeculationDb {
                path: db_path.to_path_buf(),
                source: e,
            })?;

        let db = Self {
            conn,
            db_path: db_path.to_path_buf(),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Run schema migrations (idempotent).
    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS access_events (
                    path     TEXT NOT NULL,
                    ts       INTEGER NOT NULL,
                    run_id   TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS co_access (
                    a        TEXT NOT NULL,
                    b        TEXT NOT NULL,
                    count    INTEGER NOT NULL DEFAULT 1,
                    last_ts  INTEGER NOT NULL,
                    PRIMARY KEY (a, b)
                );

                CREATE INDEX IF NOT EXISTS idx_co_a ON co_access(a);",
            )
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;
        Ok(())
    }

    /// Insert an access event.
    pub fn record_access(&self, path: &str, ts_ms: i64, run_id: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO access_events (path, ts, run_id) VALUES (?1, ?2, ?3)",
                params![path, ts_ms, run_id],
            )
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;
        Ok(())
    }

    /// Query access events within `window_ms` of the most recent event.
    pub fn get_recent_events(&self, window_ms: i64) -> Result<Vec<AccessEvent>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, ts, run_id FROM access_events
                 WHERE ts >= (SELECT COALESCE(MAX(ts), 0) FROM access_events) - ?1
                 ORDER BY ts ASC",
            )
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;

        let rows = stmt
            .query_map(params![window_ms], |row| {
                Ok(AccessEvent {
                    path: row.get(0)?,
                    ts_ms: row.get(1)?,
                    run_id: row.get(2)?,
                })
            })
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?);
        }
        Ok(events)
    }

    /// Increment the co-access count for `(a, b)`, or insert with count 1.
    pub fn upsert_co_access(&self, a: &str, b: &str, ts_ms: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO co_access (a, b, count, last_ts) VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT(a, b) DO UPDATE SET count = count + 1, last_ts = ?3",
                params![a, b, ts_ms],
            )
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;
        Ok(())
    }

    /// Return the top-K co-accessed paths for `path` with count ≥ `min_count`.
    pub fn top_k(&self, path: &str, k: usize, min_count: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT b FROM co_access
                 WHERE a = ?1 AND count >= ?2
                 ORDER BY count DESC
                 LIMIT ?3",
            )
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;

        let rows = stmt
            .query_map(params![path, min_count, k as i64], |row| row.get(0))
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?);
        }
        Ok(result)
    }

    /// Delete co-access and access_events entries older than `max_age_ms`.
    ///
    /// Returns the total number of rows deleted across both tables.
    pub fn decay(&self, max_age_ms: i64) -> Result<u64> {
        // Compute the cutoff timestamp: most-recent event minus max_age_ms.
        // Using a relative cutoff from the DB's own data keeps tests
        // deterministic without injecting wall-clock time.
        let cutoff: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(ts), 0) FROM access_events",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?
            - max_age_ms;

        let co_deleted = self
            .conn
            .execute("DELETE FROM co_access WHERE last_ts < ?1", params![cutoff])
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;

        let ev_deleted = self
            .conn
            .execute("DELETE FROM access_events WHERE ts < ?1", params![cutoff])
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;

        let total = (co_deleted + ev_deleted) as u64;
        debug!(co_deleted, ev_deleted, total, "speculation decay complete");
        Ok(total)
    }

    /// Wipe both tables.
    pub fn clear(&self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM access_events; DELETE FROM co_access;")
            .map_err(|e| CrabError::SpeculationDb {
                path: self.db_path.clone(),
                source: e,
            })?;
        debug!("speculation tables cleared");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Async wrapper
// ---------------------------------------------------------------------------

/// Async wrapper around [`AccessDb`].
///
/// Holds the synchronous DB in `Arc<std::sync::Mutex<>>` and dispatches
/// every call through `tokio::task::spawn_blocking` so the tokio runtime
/// is never blocked by SQLite I/O. Uses `std::sync::Mutex` (not tokio's)
/// because the lock is only held inside `spawn_blocking` closures that
/// need `'static`.
#[derive(Clone)]
pub struct AsyncAccessDb {
    inner: Arc<std::sync::Mutex<AccessDb>>,
}

impl AsyncAccessDb {
    /// Open (or create) the access database at `db_path`.
    pub async fn open(db_path: PathBuf) -> Result<Self> {
        let path = db_path.clone();
        let db = tokio::task::spawn_blocking(move || AccessDb::open(&path))
            .await
            .map_err(|e| CrabError::SpeculationDb {
                path: db_path,
                source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
            })??;
        Ok(Self {
            inner: Arc::new(std::sync::Mutex::new(db)),
        })
    }

    /// Fire-and-forget access recording.
    ///
    /// If the DB lock is contended, the event is silently dropped.
    /// Speculation bugs must never break smudge.
    pub fn record_access_fire_and_forget(&self, path: String, ts_ms: i64, run_id: String) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            // spawn_blocking so the std::sync::Mutex lock + SQLite I/O
            // don't block the async runtime.
            let result = tokio::task::spawn_blocking(move || {
                let Ok(guard) = inner.try_lock() else {
                    warn!("speculation DB busy, dropping access event for {path}");
                    return Ok(());
                };
                guard.record_access(&path, ts_ms, &run_id)
            })
            .await;
            match result {
                Err(e) => warn!("speculation record_access join error: {e}"),
                Ok(Err(e)) => warn!("speculation record_access DB error: {e}"),
                Ok(Ok(())) => {}
            }
        });
    }

    /// Record an access event (awaitable version).
    pub async fn record_access(&self, path: String, ts_ms: i64, run_id: String) -> Result<()> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner.lock().map_err(|_| CrabError::SpeculationDb {
                path: PathBuf::from("<lock-poisoned>"),
                source: rusqlite::Error::InvalidQuery,
            })?;
            guard.record_access(&path, ts_ms, &run_id)
        })
        .await
        .map_err(|e| CrabError::SpeculationDb {
            path: PathBuf::from("<async>"),
            source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
        })?
    }

    /// Query recent access events within `window_ms`.
    pub async fn get_recent_events(&self, window_ms: i64) -> Result<Vec<AccessEvent>> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner.lock().map_err(|_| CrabError::SpeculationDb {
                path: PathBuf::from("<lock-poisoned>"),
                source: rusqlite::Error::InvalidQuery,
            })?;
            guard.get_recent_events(window_ms)
        })
        .await
        .map_err(|e| CrabError::SpeculationDb {
            path: PathBuf::from("<async>"),
            source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
        })?
    }

    /// Upsert a co-access edge.
    pub async fn upsert_co_access(&self, a: String, b: String, ts_ms: i64) -> Result<()> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner.lock().map_err(|_| CrabError::SpeculationDb {
                path: PathBuf::from("<lock-poisoned>"),
                source: rusqlite::Error::InvalidQuery,
            })?;
            guard.upsert_co_access(&a, &b, ts_ms)
        })
        .await
        .map_err(|e| CrabError::SpeculationDb {
            path: PathBuf::from("<async>"),
            source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
        })?
    }

    /// Return the top-K co-accessed paths for `path`.
    pub async fn top_k(&self, path: String, k: usize, min_count: i64) -> Result<Vec<String>> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner.lock().map_err(|_| CrabError::SpeculationDb {
                path: PathBuf::from("<lock-poisoned>"),
                source: rusqlite::Error::InvalidQuery,
            })?;
            guard.top_k(&path, k, min_count)
        })
        .await
        .map_err(|e| CrabError::SpeculationDb {
            path: PathBuf::from("<async>"),
            source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
        })?
    }

    /// Run decay, deleting entries older than `max_age_ms`.
    pub async fn decay(&self, max_age_ms: i64) -> Result<u64> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner.lock().map_err(|_| CrabError::SpeculationDb {
                path: PathBuf::from("<lock-poisoned>"),
                source: rusqlite::Error::InvalidQuery,
            })?;
            guard.decay(max_age_ms)
        })
        .await
        .map_err(|e| CrabError::SpeculationDb {
            path: PathBuf::from("<async>"),
            source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
        })?
    }

    /// Wipe both tables.
    pub async fn clear(&self) -> Result<()> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner.lock().map_err(|_| CrabError::SpeculationDb {
                path: PathBuf::from("<lock-poisoned>"),
                source: rusqlite::Error::InvalidQuery,
            })?;
            guard.clear()
        })
        .await
        .map_err(|e| CrabError::SpeculationDb {
            path: PathBuf::from("<async>"),
            source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
        })?
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_temp_db() -> (TempDir, AccessDb) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("access.db");
        let db = AccessDb::open(&db_path).unwrap();
        (tmp, db)
    }

    #[test]
    fn schema_creation_is_idempotent() {
        let (_tmp, db) = open_temp_db();
        // Running migrate again should succeed without error.
        db.migrate().unwrap();
    }

    #[test]
    fn record_and_query_access_events() {
        let (_tmp, db) = open_temp_db();

        db.record_access("src/main.rs", 1000, "run-1").unwrap();
        db.record_access("src/lib.rs", 2000, "run-1").unwrap();
        db.record_access("README.md", 3000, "run-1").unwrap();

        // Window of 1500ms from the latest (3000) captures events at 2000 and 3000.
        let events = db.get_recent_events(1500).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, "src/lib.rs");
        assert_eq!(events[1].path, "README.md");

        // Window of 5000ms captures all three.
        let all = db.get_recent_events(5000).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn upsert_co_access_increments_count() {
        let (_tmp, db) = open_temp_db();

        db.upsert_co_access("a.rs", "b.rs", 1000).unwrap();
        db.upsert_co_access("a.rs", "b.rs", 2000).unwrap();
        db.upsert_co_access("a.rs", "b.rs", 3000).unwrap();

        let top = db.top_k("a.rs", 10, 1).unwrap();
        assert_eq!(top, vec!["b.rs"]);

        // Verify count is 3 by querying with min_count=3.
        let top_min3 = db.top_k("a.rs", 10, 3).unwrap();
        assert_eq!(top_min3, vec!["b.rs"]);

        // min_count=4 should return nothing.
        let top_min4 = db.top_k("a.rs", 10, 4).unwrap();
        assert!(top_min4.is_empty());
    }

    #[test]
    fn top_k_returns_ordered_by_count() {
        let (_tmp, db) = open_temp_db();

        // b.rs: count 5
        for i in 0..5 {
            db.upsert_co_access("a.rs", "b.rs", 1000 + i).unwrap();
        }
        // c.rs: count 3
        for i in 0..3 {
            db.upsert_co_access("a.rs", "c.rs", 1000 + i).unwrap();
        }
        // d.rs: count 1
        db.upsert_co_access("a.rs", "d.rs", 1000).unwrap();

        let top2 = db.top_k("a.rs", 2, 1).unwrap();
        assert_eq!(top2, vec!["b.rs", "c.rs"]);

        // k=1 returns only the highest.
        let top1 = db.top_k("a.rs", 1, 1).unwrap();
        assert_eq!(top1, vec!["b.rs"]);
    }

    #[test]
    fn top_k_filters_by_min_count() {
        let (_tmp, db) = open_temp_db();

        db.upsert_co_access("a.rs", "b.rs", 1000).unwrap();
        db.upsert_co_access("a.rs", "b.rs", 2000).unwrap();
        db.upsert_co_access("a.rs", "c.rs", 1000).unwrap();

        // min_count=2 should only return b.rs (count=2).
        let top = db.top_k("a.rs", 10, 2).unwrap();
        assert_eq!(top, vec!["b.rs"]);
    }

    #[test]
    fn decay_removes_old_entries() {
        let (_tmp, db) = open_temp_db();

        // Old events at ts=1000.
        db.record_access("old.rs", 1000, "run-1").unwrap();
        db.upsert_co_access("old.rs", "also_old.rs", 1000).unwrap();

        // Recent events at ts=100_000.
        db.record_access("new.rs", 100_000, "run-2").unwrap();
        db.upsert_co_access("new.rs", "also_new.rs", 100_000)
            .unwrap();

        // Decay with max_age_ms=50_000 — cutoff is 100_000 - 50_000 = 50_000.
        // Events at ts=1000 are older than cutoff.
        let deleted = db.decay(50_000).unwrap();
        assert!(deleted > 0);

        // Old events should be gone.
        let events = db.get_recent_events(200_000).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "new.rs");

        // Old co-access should be gone.
        let top = db.top_k("old.rs", 10, 1).unwrap();
        assert!(top.is_empty());

        // New co-access should remain.
        let top_new = db.top_k("new.rs", 10, 1).unwrap();
        assert_eq!(top_new, vec!["also_new.rs"]);
    }

    #[test]
    fn clear_wipes_both_tables() {
        let (_tmp, db) = open_temp_db();

        db.record_access("a.rs", 1000, "run-1").unwrap();
        db.upsert_co_access("a.rs", "b.rs", 1000).unwrap();

        db.clear().unwrap();

        let events = db.get_recent_events(100_000).unwrap();
        assert!(events.is_empty());

        let top = db.top_k("a.rs", 10, 1).unwrap();
        assert!(top.is_empty());
    }

    #[test]
    fn empty_db_queries_return_empty() {
        let (_tmp, db) = open_temp_db();

        let events = db.get_recent_events(100_000).unwrap();
        assert!(events.is_empty());

        let top = db.top_k("a.rs", 10, 1).unwrap();
        assert!(top.is_empty());
    }

    #[test]
    fn decay_on_empty_db_returns_zero() {
        let (_tmp, db) = open_temp_db();
        let deleted = db.decay(50_000).unwrap();
        assert_eq!(deleted, 0);
    }

    // --- Async tests ---

    #[tokio::test]
    async fn async_record_and_query() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("access.db");
        let db = AsyncAccessDb::open(db_path).await.unwrap();

        db.record_access("src/main.rs".into(), 1000, "run-1".into())
            .await
            .unwrap();
        db.record_access("src/lib.rs".into(), 2000, "run-1".into())
            .await
            .unwrap();

        let events = db.get_recent_events(5000).await.unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn async_upsert_and_top_k() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("access.db");
        let db = AsyncAccessDb::open(db_path).await.unwrap();

        for i in 0..5 {
            db.upsert_co_access("a.rs".into(), "b.rs".into(), 1000 + i)
                .await
                .unwrap();
        }

        let top = db.top_k("a.rs".into(), 10, 3).await.unwrap();
        assert_eq!(top, vec!["b.rs"]);
    }

    #[tokio::test]
    async fn async_decay_and_clear() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("access.db");
        let db = AsyncAccessDb::open(db_path).await.unwrap();

        db.record_access("old.rs".into(), 1000, "run-1".into())
            .await
            .unwrap();
        db.record_access("new.rs".into(), 100_000, "run-2".into())
            .await
            .unwrap();

        let deleted = db.decay(50_000).await.unwrap();
        assert!(deleted > 0);

        db.clear().await.unwrap();
        let events = db.get_recent_events(200_000).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn fire_and_forget_does_not_panic() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("access.db");
        let db = AsyncAccessDb::open(db_path).await.unwrap();

        db.record_access_fire_and_forget("test.rs".into(), 1000, "run-1".into());

        // Give the spawned task time to complete.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let events = db.get_recent_events(5000).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "test.rs");
    }
}
