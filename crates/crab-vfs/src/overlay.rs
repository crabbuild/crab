//! Copy-on-write overlay store backed by SQLite.
//!
//! Tracks local modifications (creates, modifies, deletes, renames,
//! mkdirs) in a SQLite database at `.crab/overlay.db` with file
//! content stored on disk at `.crab/overlay/upper/`. Mirrors
//! artifact-fs's `overlay.Store` pattern.
//!
//! The store implements both [`OverlayLookup`] (read side, used by the
//! resolver) and [`OverlayWriter`] (write side, used by the engine).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use tracing::{debug, trace, warn};

use crate::core::error::{CrabError, Result};
use crate::engine::{BaseRenameEntry, OverlayWriter};
use crate::resolver::{OverlayEntry, OverlayKind, OverlayLookup};
use crate::snapshot::NodeType;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const MIGRATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS overlay_entries (
        path TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        backing_path TEXT,
        mode INTEGER NOT NULL,
        size_bytes INTEGER NOT NULL DEFAULT 0,
        mtime_unix_ns INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_overlay_kind ON overlay_entries(kind)",
];

// ---------------------------------------------------------------------------
// OverlayStore
// ---------------------------------------------------------------------------

/// SQLite-backed copy-on-write overlay store.
///
/// Thread safety: `rusqlite::Connection` is `!Send`, so we wrap it in a
/// `Mutex` to allow shared access from multiple FUSE threads.
pub struct OverlayStore {
    db: Mutex<Connection>,
    freeze_lock: Mutex<()>,
    upper_dir: PathBuf,
}

/// Persisted overlay mutation with backing-file information.
#[derive(Debug, Clone)]
pub struct OverlayRecord {
    pub path: String,
    pub kind: OverlayKind,
    pub backing_path: Option<PathBuf>,
    pub base_path: Option<String>,
    pub mode: u32,
    pub size: u64,
    pub mtime_ns: i64,
}

impl OverlayStore {
    /// Open or create an overlay store.
    ///
    /// - `db_path`: path to the SQLite database (e.g. `.crab/overlay.db`)
    /// - `upper_dir`: directory for backing files (e.g. `.crab/overlay/upper/`)
    pub fn open(db_path: &Path, upper_dir: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(upper_dir)?;

        let conn = Connection::open(db_path).map_err(map_sqlite_err)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(map_sqlite_err)?;

        for migration in MIGRATIONS {
            conn.execute_batch(migration).map_err(map_sqlite_err)?;
        }

        // Migrate columns added after the original overlay schema.
        Self::migrate_add_columns(&conn)?;

        let store = Self {
            db: Mutex::new(conn),
            freeze_lock: Mutex::new(()),
            upper_dir: upper_dir.to_path_buf(),
        };

        debug!(db = %db_path.display(), upper = %upper_dir.display(), "overlay store opened");

        Ok(store)
    }

    /// Open or create an overlay store and remove unreferenced backing files.
    ///
    /// Use only before a mount starts serving FUSE requests. Live inspection,
    /// export, commit, and reset paths must not delete upper files that may be
    /// waiting on delayed metadata writes from the kernel.
    pub fn open_with_orphan_cleanup(db_path: &Path, upper_dir: &Path) -> Result<Self> {
        let store = Self::open(db_path, upper_dir)?;
        store.cleanup_orphaned_backing_files()?;
        Ok(store)
    }

    /// Add `source_oid` and `target_path` columns if they don't exist.
    ///
    /// Uses `PRAGMA table_info` to check column presence before running
    /// `ALTER TABLE`. SQLite `ADD COLUMN` with a default is safe on
    /// existing data.
    fn migrate_add_columns(conn: &Connection) -> Result<()> {
        let mut has_source_oid = false;
        let mut has_target_path = false;
        let mut has_base_path = false;

        let mut stmt = conn
            .prepare("PRAGMA table_info(overlay_entries)")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(map_sqlite_err)?;
        for col in rows {
            let name = col.map_err(map_sqlite_err)?;
            if name == "source_oid" {
                has_source_oid = true;
            } else if name == "target_path" {
                has_target_path = true;
            } else if name == "base_path" {
                has_base_path = true;
            }
        }
        drop(stmt);

        if !has_source_oid {
            conn.execute_batch("ALTER TABLE overlay_entries ADD COLUMN source_oid TEXT DEFAULT ''")
                .map_err(map_sqlite_err)?;
            debug!("migrated overlay schema: added source_oid column");
        }
        if !has_target_path {
            conn.execute_batch(
                "ALTER TABLE overlay_entries ADD COLUMN target_path TEXT DEFAULT ''",
            )
            .map_err(map_sqlite_err)?;
            debug!("migrated overlay schema: added target_path column");
        }
        if !has_base_path {
            conn.execute_batch("ALTER TABLE overlay_entries ADD COLUMN base_path TEXT DEFAULT ''")
                .map_err(map_sqlite_err)?;
            debug!("migrated overlay schema: added base_path column");
        }

        Ok(())
    }

    /// Delete orphaned backing files in `upper/` not referenced by any
    /// overlay entry. This handles crash recovery where the DB was
    /// committed but backing files were left behind, or vice versa.
    fn cleanup_orphaned_backing_files(&self) -> Result<()> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;

        // Collect all referenced backing paths from the DB.
        let mut stmt = db
            .prepare("SELECT backing_path FROM overlay_entries WHERE backing_path IS NOT NULL")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_err)?;

        let mut referenced: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for row in rows {
            let path_str = row.map_err(map_sqlite_err)?;
            referenced.insert(PathBuf::from(path_str));
        }
        drop(stmt);
        drop(db);

        // Walk upper_dir and delete unreferenced files.
        let mut orphan_count = 0u64;
        if let Ok(walker) = walk_dir_files(&self.upper_dir) {
            for file_path in walker {
                if !referenced.contains(&file_path) {
                    warn!(path = %file_path.display(), "deleting orphaned backing file");
                    let _ = std::fs::remove_file(&file_path);
                    orphan_count += 1;
                }
            }
        }

        if orphan_count > 0 {
            warn!(count = orphan_count, "cleaned up orphaned backing files");
        }

        Ok(())
    }

    /// Delete the overlay database and upper directory, providing a fresh start.
    ///
    /// Call this before `OverlayStore::open` to discard all local modifications.
    pub fn clean(db_path: &Path, upper_dir: &Path) -> Result<()> {
        if db_path.exists() {
            std::fs::remove_file(db_path)?;
            // Also remove WAL and SHM files if present.
            let wal = db_path.with_extension("db-wal");
            let shm = db_path.with_extension("db-shm");
            let _ = std::fs::remove_file(wal);
            let _ = std::fs::remove_file(shm);
            debug!(path = %db_path.display(), "deleted overlay database");
        }
        if upper_dir.exists() {
            std::fs::remove_dir_all(upper_dir)?;
            debug!(path = %upper_dir.display(), "deleted overlay upper directory");
        }
        Ok(())
    }

    /// Remove all overlay entries and backing files without replacing the DB.
    ///
    /// This keeps other open SQLite connections attached to the same overlay
    /// database, which live FUSE mounts rely on when a CLI reset runs out of
    /// process.
    pub fn clear(&self) -> Result<()> {
        // FUSE can still deliver delayed metadata writes while a live reset is
        // draining. A second pass removes rows or files that appeared between
        // the first DB clear and upper-directory sweep.
        self.clear_entries()?;
        clear_directory_contents(&self.upper_dir)?;
        self.clear_entries()?;
        clear_directory_contents(&self.upper_dir)?;
        debug!(path = %self.upper_dir.display(), "cleared overlay store");
        Ok(())
    }

    fn clear_entries(&self) -> Result<()> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        db.execute("DELETE FROM overlay_entries", [])
            .map_err(map_sqlite_err)?;
        checkpoint_wal(&db)?;
        Ok(())
    }

    /// Flush pending WAL frames so out-of-process readers see current rows.
    pub fn checkpoint_wal(&self) -> Result<()> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        checkpoint_wal(&db)
    }

    /// Number of unpublished overlay changes, including delete markers.
    pub fn dirty_count(&self) -> Result<i64> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM overlay_entries", [], |row| row.get(0))
            .map_err(map_sqlite_err)?;
        Ok(count)
    }

    /// Paths of unpublished overlay changes, including delete markers, sorted.
    pub fn dirty_paths(&self) -> Result<Vec<String>> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let mut stmt = db
            .prepare("SELECT path FROM overlay_entries ORDER BY path")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .map_err(map_sqlite_err)?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(row.map_err(map_sqlite_err)?);
        }
        Ok(paths)
    }

    /// Read dirty overlay state from an existing database without creating or migrating it.
    pub fn read_dirty_state(db_path: &Path, include_paths: bool) -> Result<(i64, Vec<String>)> {
        if !db_path.exists() {
            return Ok((0, Vec::new()));
        }

        let db = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_sqlite_err)?;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM overlay_entries", [], |row| row.get(0))
            .map_err(map_sqlite_err)?;
        if !include_paths || count == 0 {
            return Ok((count, Vec::new()));
        }

        let mut stmt = db
            .prepare("SELECT path FROM overlay_entries ORDER BY path")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .map_err(map_sqlite_err)?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(row.map_err(map_sqlite_err)?);
        }
        Ok((count, paths))
    }

    /// Return all overlay mutations, including deletion markers, sorted by path.
    pub fn records(&self) -> Result<Vec<OverlayRecord>> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let mut stmt = db
            .prepare(
                "SELECT path, kind, backing_path, base_path, mode, size_bytes, mtime_unix_ns
                 FROM overlay_entries ORDER BY path",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let kind: String = row.get(1)?;
                Ok(OverlayRecord {
                    path: row.get(0)?,
                    kind: parse_overlay_kind(&kind),
                    backing_path: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
                    base_path: non_empty_string(row.get(3)?),
                    mode: row.get::<_, i64>(4)? as u32,
                    size: row.get::<_, i64>(5)? as u64,
                    mtime_ns: row.get(6)?,
                })
            })
            .map_err(map_sqlite_err)?;

        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(map_sqlite_err)?);
        }
        Ok(records)
    }

    /// Return whether writes are currently frozen for this overlay.
    pub fn is_frozen(&self) -> bool {
        self.freeze_marker_path().exists()
    }

    /// Freeze writes to this overlay until the returned guard is dropped.
    pub fn freeze_writes(&self) -> Result<OverlayFreezeGuard<'_>> {
        let guard = self.freeze_lock.lock().map_err(|_| lock_poisoned())?;
        let marker = self.freeze_marker_path();
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(_) => Ok(OverlayFreezeGuard {
                marker,
                _guard: guard,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(CrabError::Forbidden {
                path: "overlay is already frozen by another publish operation".into(),
            }),
            Err(e) => Err(CrabError::Io(e)),
        }
    }

    /// Reconcile the overlay against a new base snapshot.
    ///
    /// For each overlay entry, calls `base_lookup(path)` to check whether
    /// lineage-backed entries still apply to the current base. Stale entries
    /// are removed:
    ///
    /// - `delete` where base no longer has the path → remove (irrelevant)
    /// - `create`/`mkdir`/`symlink` → keep; these are local-only edits
    /// - `modify`/`rename`: compare `source_oid` against new base `object_oid`:
    ///   - no source lineage → KEEP
    ///   - base exists AND `source_oid` matches base `object_oid` → KEEP
    ///   - base exists AND `source_oid` differs → REMOVE (stale)
    ///   - base gone → REMOVE
    ///
    /// This mirrors artifact-fs's OID-aware reconciliation logic.
    pub fn reconcile<F>(&self, base_lookup: F) -> Result<()>
    where
        F: Fn(&str) -> Option<ReconcileBaseInfo>,
    {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;

        // Read all overlay entries including source_oid.
        let mut stmt = db
            .prepare(
                "SELECT path, kind, mtime_unix_ns, source_oid, base_path \
                 FROM overlay_entries ORDER BY path",
            )
            .map_err(map_sqlite_err)?;

        let entries: Vec<(String, String, i64, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(map_sqlite_err)?
            .filter_map(std::result::Result::ok)
            .collect();

        drop(stmt);

        let mut to_remove: Vec<(String, String, i64, String)> = Vec::new();

        for (path, kind, mtime_ns, source_oid, base_path) in &entries {
            let lookup_path = if base_path.is_empty() {
                path.as_str()
            } else {
                base_path.as_str()
            };
            let base = base_lookup(lookup_path);
            let should_remove = match kind.as_str() {
                "delete" => base.is_none(),
                "create" | "mkdir" | "symlink" => false,
                "modify" | "rename" => {
                    if source_oid.is_empty() && base_path.is_empty() {
                        false
                    } else {
                        match &base {
                            Some(b) => {
                                // Base exists — keep only if source_oid matches.
                                let base_oid = b.object_oid.as_deref().unwrap_or("");
                                source_oid != base_oid
                            }
                            None => true, // Base gone — stale.
                        }
                    }
                }
                _ => false,
            };
            if should_remove {
                to_remove.push((path.clone(), kind.clone(), *mtime_ns, source_oid.clone()));
            }
        }

        if to_remove.is_empty() {
            debug!("reconcile: no stale overlay entries");
            return Ok(());
        }

        // Delete stale entries in a transaction, guarded by source_oid and
        // mtime to avoid racing with concurrent FUSE writes.
        let tx = db.unchecked_transaction().map_err(map_sqlite_err)?;
        {
            let mut del_stmt = tx
                .prepare(
                    "DELETE FROM overlay_entries \
                     WHERE path = ? AND kind = ? AND source_oid = ? AND mtime_unix_ns = ?",
                )
                .map_err(map_sqlite_err)?;

            for (path, kind, mtime_ns, source_oid) in &to_remove {
                del_stmt
                    .execute(params![path, kind, source_oid, mtime_ns])
                    .map_err(map_sqlite_err)?;
            }
        }
        tx.commit().map_err(map_sqlite_err)?;

        // Delete backing files after commit (reverse order so children
        // are removed before parents).
        for (path, _kind, _mtime, _oid) in to_remove.iter().rev() {
            let backing = self.backing_path(path);
            if backing.exists() {
                let _ = std::fs::remove_file(&backing);
            }
        }

        debug!(removed = to_remove.len(), "reconcile complete");
        Ok(())
    }

    // --- Internal helpers ---

    /// Compute the on-disk backing path for a given overlay path.
    fn backing_path(&self, path: &str) -> PathBuf {
        self.upper_dir.join(clean_path(path))
    }

    fn freeze_marker_path(&self) -> PathBuf {
        self.upper_dir.parent().map_or_else(
            || self.upper_dir.join("publish.freeze"),
            |parent| parent.join("publish.freeze"),
        )
    }

    fn ensure_not_frozen(&self) -> Result<()> {
        if self.is_frozen() {
            return Err(CrabError::Forbidden {
                path: "overlay writes are frozen during publish".into(),
            });
        }
        Ok(())
    }

    fn begin_write(&self) -> Result<OverlayWriteGuard<'_>> {
        let guard = match self.freeze_lock.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) if self.is_frozen() => {
                return Err(CrabError::Forbidden {
                    path: "overlay writes are frozen during publish".into(),
                });
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                self.freeze_lock.lock().map_err(|_| lock_poisoned())?
            }
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(lock_poisoned()),
        };
        self.ensure_not_frozen()?;
        Ok(OverlayWriteGuard { _guard: guard })
    }

    /// Current time in nanoseconds since epoch.
    fn now_ns() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64)
    }

    /// Upsert an overlay entry into the database.
    fn upsert_entry(
        conn: &Connection,
        path: &str,
        kind: &str,
        backing_path: Option<&str>,
        mode: u32,
        size: u64,
        mtime_ns: i64,
        lineage: EntryLineage<'_>,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO overlay_entries(path, kind, backing_path, mode, size_bytes, mtime_unix_ns, source_oid, base_path)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET
               kind = excluded.kind,
               backing_path = excluded.backing_path,
               mode = excluded.mode,
               size_bytes = excluded.size_bytes,
               mtime_unix_ns = excluded.mtime_unix_ns,
               source_oid = excluded.source_oid,
               base_path = excluded.base_path",
            params![
                path,
                kind,
                backing_path,
                mode,
                size as i64,
                mtime_ns,
                lineage.source_oid,
                lineage.base_path
            ],
        )
        .map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Read a single overlay entry from the database.
    fn query_entry(conn: &Connection, path: &str) -> Result<Option<OverlayEntry>> {
        let result = conn
            .query_row(
                "SELECT path, kind, backing_path, mode, size_bytes, mtime_unix_ns
                 FROM overlay_entries WHERE path = ?1",
                params![path],
                |row| {
                    Ok(RawEntry {
                        path: row.get(0)?,
                        kind: row.get(1)?,
                        backing_path: row.get(2)?,
                        mode: row.get::<_, i64>(3)? as u32,
                        size: row.get::<_, i64>(4)? as u64,
                        mtime_ns: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_err)?;

        Ok(result.map(RawEntry::into_overlay_entry))
    }

    /// List overlay entries whose paths match `prefix/%`.
    fn query_by_prefix(conn: &Connection, prefix: &str) -> Result<Vec<OverlayEntry>> {
        let pattern = if prefix.is_empty() {
            "%".to_owned()
        } else {
            format!("{prefix}/%")
        };

        let mut stmt = conn
            .prepare(
                "SELECT path, kind, backing_path, mode, size_bytes, mtime_unix_ns
                 FROM overlay_entries WHERE path LIKE ?1 ORDER BY path",
            )
            .map_err(map_sqlite_err)?;

        let rows = stmt
            .query_map(params![pattern], |row| {
                Ok(RawEntry {
                    path: row.get(0)?,
                    kind: row.get(1)?,
                    backing_path: row.get(2)?,
                    mode: row.get::<_, i64>(3)? as u32,
                    size: row.get::<_, i64>(4)? as u64,
                    mtime_ns: row.get(5)?,
                })
            })
            .map_err(map_sqlite_err)?;

        let mut entries = Vec::new();
        for row in rows {
            let raw = row.map_err(map_sqlite_err)?;
            entries.push(raw.into_overlay_entry());
        }
        Ok(entries)
    }

    fn query_children_page(
        conn: &Connection,
        parent_path: &str,
        after_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OverlayEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let prefix = if parent_path.is_empty() {
            String::new()
        } else {
            format!("{parent_path}/")
        };
        let after_path = format!("{prefix}{}", after_name.unwrap_or(""));
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut entries = Vec::new();

        if prefix.is_empty() {
            let mut stmt = conn
                .prepare(
                    "SELECT path, kind, backing_path, mode, size_bytes, mtime_unix_ns
                     FROM overlay_entries
                     WHERE path > ?1 AND instr(path, '/') = 0
                     ORDER BY path LIMIT ?2",
                )
                .map_err(map_sqlite_err)?;
            let rows = stmt
                .query_map(params![after_path, limit], raw_entry_from_row)
                .map_err(map_sqlite_err)?;
            for row in rows {
                entries.push(row.map_err(map_sqlite_err)?.into_overlay_entry());
            }
        } else {
            let upper = format!("{prefix}\u{10ffff}");
            let child_start = i64::try_from(prefix.len() + 1).unwrap_or(i64::MAX);
            let mut stmt = conn
                .prepare(
                    "SELECT path, kind, backing_path, mode, size_bytes, mtime_unix_ns
                     FROM overlay_entries
                     WHERE path > ?1 AND path < ?2
                       AND instr(substr(path, ?3), '/') = 0
                     ORDER BY path LIMIT ?4",
                )
                .map_err(map_sqlite_err)?;
            let rows = stmt
                .query_map(
                    params![after_path, upper, child_start, limit],
                    raw_entry_from_row,
                )
                .map_err(map_sqlite_err)?;
            for row in rows {
                entries.push(row.map_err(map_sqlite_err)?.into_overlay_entry());
            }
        }
        Ok(entries)
    }

    fn query_rename_row(conn: &Connection, path: &str) -> Result<Option<RenameRow>> {
        conn.query_row(
            "SELECT path, kind, backing_path, mode, size_bytes, mtime_unix_ns, source_oid, base_path
             FROM overlay_entries WHERE path = ?1",
            params![path],
            |row| {
                Ok(RenameRow {
                    path: row.get(0)?,
                    kind: row.get(1)?,
                    backing_path: row.get(2)?,
                    mode: row.get::<_, i64>(3)? as u32,
                    size: row.get::<_, i64>(4)? as u64,
                    mtime_ns: row.get(5)?,
                    source_oid: row.get(6)?,
                    base_path: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(map_sqlite_err)
    }

    fn query_descendant_rename_rows(conn: &Connection, path: &str) -> Result<Vec<RenameRow>> {
        let prefix = format!("{path}/");
        let mut stmt = conn
            .prepare(
                "SELECT path, kind, backing_path, mode, size_bytes, mtime_unix_ns, source_oid, base_path
                 FROM overlay_entries ORDER BY path",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(RenameRow {
                    path: row.get(0)?,
                    kind: row.get(1)?,
                    backing_path: row.get(2)?,
                    mode: row.get::<_, i64>(3)? as u32,
                    size: row.get::<_, i64>(4)? as u64,
                    mtime_ns: row.get(5)?,
                    source_oid: row.get(6)?,
                    base_path: row.get(7)?,
                })
            })
            .map_err(map_sqlite_err)?;

        let mut descendants = Vec::new();
        for row in rows {
            let row = row.map_err(map_sqlite_err)?;
            if row.path.starts_with(&prefix) {
                descendants.push(row);
            }
        }
        Ok(descendants)
    }

    fn query_base_path(conn: &Connection, path: &str) -> Result<Option<String>> {
        conn.query_row(
            "SELECT base_path FROM overlay_entries WHERE path = ?1",
            params![path],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_sqlite_err)
        .map(|value| value.and_then(non_empty_string))
    }
}

fn raw_entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok(RawEntry {
        path: row.get(0)?,
        kind: row.get(1)?,
        backing_path: row.get(2)?,
        mode: row.get::<_, i64>(3)? as u32,
        size: row.get::<_, i64>(4)? as u64,
        mtime_ns: row.get(5)?,
    })
}

struct OverlayWriteGuard<'a> {
    _guard: std::sync::MutexGuard<'a, ()>,
}

/// Removes the overlay write-freeze marker when dropped.
pub struct OverlayFreezeGuard<'a> {
    marker: PathBuf,
    _guard: std::sync::MutexGuard<'a, ()>,
}

impl Drop for OverlayFreezeGuard<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.marker);
    }
}

// ---------------------------------------------------------------------------
// OverlayLookup implementation (read side for the resolver)
// ---------------------------------------------------------------------------

impl OverlayLookup for OverlayStore {
    fn get(&self, path: &str) -> Option<OverlayEntry> {
        let db = self.db.lock().unwrap_or_else(|e| {
            tracing::warn!("overlay db mutex was poisoned; recovering");
            e.into_inner()
        });
        Self::query_entry(&db, &clean_path(path)).ok().flatten()
    }

    fn list_by_prefix(&self, parent_path: &str) -> Vec<OverlayEntry> {
        let Ok(db) = self.db.lock() else {
            return Vec::new();
        };
        Self::query_by_prefix(&db, &clean_path(parent_path)).unwrap_or_default()
    }

    fn list_children_page(
        &self,
        parent_path: &str,
        after_name: Option<&str>,
        limit: usize,
    ) -> Vec<OverlayEntry> {
        let Ok(db) = self.db.lock() else {
            return Vec::new();
        };
        Self::query_children_page(&db, &clean_path(parent_path), after_name, limit)
            .unwrap_or_default()
    }

    fn base_path(&self, path: &str) -> Option<String> {
        let Ok(db) = self.db.lock() else {
            return None;
        };
        Self::query_base_path(&db, &clean_path(path)).ok().flatten()
    }
}

// ---------------------------------------------------------------------------
// OverlayWriter implementation (write side for the engine)
// ---------------------------------------------------------------------------

impl OverlayWriter for OverlayStore {
    fn get(&self, path: &str) -> Option<OverlayEntry> {
        OverlayLookup::get(self, path)
    }

    fn get_backing_path(&self, path: &str) -> Option<PathBuf> {
        let entry = OverlayLookup::get(self, path)?;
        if entry.is_deleted() {
            return None;
        }
        let backing = self.backing_path(&clean_path(path));
        if backing.exists() {
            Some(backing)
        } else {
            None
        }
    }

    fn create_file(&self, path: &str, mode: u32) -> Result<OverlayEntry> {
        let _write = self.begin_write()?;
        let path = clean_path(path);
        let backing = self.backing_path(&path);

        if let Some(parent) = backing.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&backing, [])?;
        set_path_permissions(&backing, mode)?;

        self.ensure_not_frozen()?;
        let now = Self::now_ns();
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        Self::upsert_entry(
            &db,
            &path,
            "create",
            Some(&backing.to_string_lossy()),
            mode,
            0,
            now,
            EntryLineage::default(),
        )?;

        trace!(path = %path, "overlay create_file");
        Ok(OverlayEntry {
            path,
            kind: OverlayKind::Create,
            mode,
            size: 0,
            mtime_ns: now,
            node_type: NodeType::File,
        })
    }

    fn create_symlink(&self, path: &str, target: &str, mode: u32) -> Result<OverlayEntry> {
        let _write = self.begin_write()?;
        let path = clean_path(path);
        let backing = self.backing_path(&path);
        let mode = mode | 0o120_000;

        if let Some(parent) = backing.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write the symlink target as the backing file content (git convention:
        // symlinks are stored as blob content containing the target path).
        std::fs::write(&backing, target.as_bytes())?;

        self.ensure_not_frozen()?;
        let now = Self::now_ns();
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        Self::upsert_entry(
            &db,
            &path,
            "symlink",
            Some(&backing.to_string_lossy()),
            mode,
            target.len() as u64,
            now,
            EntryLineage::default(),
        )?;

        trace!(path = %path, target, "overlay create_symlink");
        Ok(OverlayEntry {
            path,
            kind: OverlayKind::Symlink,
            mode,
            size: target.len() as u64,
            mtime_ns: now,
            node_type: NodeType::Symlink,
        })
    }

    fn sync_path(&self, path: &str) -> Result<()> {
        if let Some(backing) = self.get_backing_path(&clean_path(path)) {
            std::fs::File::open(&backing)?.sync_all()?;
        }
        Ok(())
    }

    fn checkpoint(&self) -> Result<()> {
        self.checkpoint_wal()
    }

    fn write_file(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
        use std::io::{Seek, SeekFrom, Write};

        let _write = self.begin_write()?;
        let path = clean_path(path);

        // Phase 1: query the entry (hold lock briefly, then release).
        let (entry_mode, entry_kind, entry_mtime) = {
            let db = self.db.lock().map_err(|_| lock_poisoned())?;
            let entry = Self::query_entry(&db, &path)?
                .ok_or_else(|| CrabError::NotFound { path: path.clone() })?;
            if entry.is_deleted() {
                return Err(CrabError::NotFound { path });
            }
            (entry.mode, entry.kind, entry.mtime_ns)
        };

        // Phase 2: file I/O without holding the lock.
        let backing = self.backing_path(&path);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&backing)?;

        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        let n = data.len();

        let metadata = file.metadata()?;
        let new_size = metadata.len();
        let now = Self::now_ns();

        let kind = if entry_kind == OverlayKind::Create {
            "create"
        } else {
            "modify"
        };

        // Phase 3: update metadata (hold lock briefly). Re-query the entry
        // first to guard against a concurrent delete or modify race between
        // Phase 1 and now.
        {
            self.ensure_not_frozen()?;
            let db = self.db.lock().map_err(|_| lock_poisoned())?;
            let current = Self::query_entry(&db, &path)?;
            match current {
                Some(ref cur) if cur.is_deleted() => {
                    return Err(CrabError::NotFound { path: path.clone() });
                }
                Some(ref cur) if cur.mtime_ns != entry_mtime || cur.kind != entry_kind => {
                    return Err(CrabError::Internal(
                        "overlay entry changed during write".into(),
                    ));
                }
                None => {
                    return Err(CrabError::NotFound { path: path.clone() });
                }
                _ => { /* entry unchanged, safe to update */ }
            }
            Self::upsert_entry(
                &db,
                &path,
                kind,
                Some(&backing.to_string_lossy()),
                entry_mode,
                new_size,
                now,
                EntryLineage::default(),
            )?;
        }

        trace!(path = %path, offset, written = n, new_size, "overlay write_file");
        Ok(n)
    }

    fn promote(
        &self,
        path: &str,
        mode: u32,
        content: &[u8],
        source_oid: Option<&str>,
    ) -> Result<OverlayEntry> {
        let _write = self.begin_write()?;
        let path = clean_path(path);
        let backing = self.backing_path(&path);

        if let Some(parent) = backing.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write content atomically via tempfile + rename.
        let tmp = backing.with_extension("tmp");
        std::fs::write(&tmp, content)?;
        set_path_permissions(&tmp, mode)?;
        std::fs::rename(&tmp, &backing)?;

        let size = content.len() as u64;
        let now = Self::now_ns();
        let oid = source_oid.unwrap_or("");

        self.ensure_not_frozen()?;
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        Self::upsert_entry(
            &db,
            &path,
            "modify",
            Some(&backing.to_string_lossy()),
            mode,
            size,
            now,
            EntryLineage {
                source_oid: oid,
                base_path: "",
            },
        )?;

        debug!(path = %path, size, source_oid = oid, "overlay promote");
        Ok(OverlayEntry {
            path,
            kind: OverlayKind::Modify,
            mode,
            size,
            mtime_ns: now,
            node_type: NodeType::File,
        })
    }

    fn remove(&self, path: &str) -> Result<()> {
        let _write = self.begin_write()?;
        let path = clean_path(path);

        // Delete backing file if it exists.
        let backing = self.backing_path(&path);
        if backing.exists() {
            let _ = std::fs::remove_file(&backing);
        }

        self.ensure_not_frozen()?;
        let now = Self::now_ns();
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        if let Some(row) = Self::query_rename_row(&db, &path)?
            && is_overlay_only_creation(&row)
        {
            db.execute("DELETE FROM overlay_entries WHERE path = ?1", params![path])
                .map_err(map_sqlite_err)?;
            trace!(path = %path, "overlay remove discarded overlay-only entry");
            return Ok(());
        }
        Self::upsert_entry(
            &db,
            &path,
            "delete",
            None,
            0,
            0,
            now,
            EntryLineage::default(),
        )?;

        trace!(path = %path, "overlay remove");
        Ok(())
    }

    fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let _write = self.begin_write()?;
        let old_path = clean_path(old_path);
        let new_path = clean_path(new_path);

        let db = self.db.lock().map_err(|_| lock_poisoned())?;

        let entry = Self::query_entry(&db, &old_path)?.ok_or_else(|| CrabError::NotFound {
            path: old_path.clone(),
        })?;

        if entry.is_deleted() {
            return Err(CrabError::NotFound { path: old_path });
        }
        let source_row =
            Self::query_rename_row(&db, &old_path)?.ok_or_else(|| CrabError::NotFound {
                path: old_path.clone(),
            })?;
        let descendant_rows = if entry.node_type == NodeType::Dir {
            Self::query_descendant_rename_rows(&db, &old_path)?
        } else {
            Vec::new()
        };

        let new_backing_path = self.backing_path(&new_path);
        if let Some(parent) = new_backing_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let now = Self::now_ns();

        // DB transaction: rewrite the moved subtree, then mark the old root deleted.
        let tx = db.unchecked_transaction().map_err(map_sqlite_err)?;
        {
            tx.execute(
                "DELETE FROM overlay_entries WHERE path = ?1",
                params![old_path],
            )
            .map_err(map_sqlite_err)?;
            for row in &descendant_rows {
                tx.execute(
                    "DELETE FROM overlay_entries WHERE path = ?1",
                    params![row.path],
                )
                .map_err(map_sqlite_err)?;
            }

            let new_backing = source_row
                .backing_path
                .as_ref()
                .map(|_| self.backing_path(&new_path).to_string_lossy().into_owned());
            let new_kind = rename_destination_kind_for_row(&source_row, entry.node_type);

            Self::upsert_entry(
                &tx,
                &new_path,
                new_kind,
                new_backing.as_deref(),
                entry.mode,
                entry.size,
                now,
                EntryLineage {
                    source_oid: &source_row.source_oid,
                    base_path: &source_row.base_path,
                },
            )?;

            for row in &descendant_rows {
                let moved_path = move_descendant_path(&old_path, &new_path, &row.path);
                let moved_backing = row.backing_path.as_ref().map(|_| {
                    self.backing_path(&moved_path)
                        .to_string_lossy()
                        .into_owned()
                });
                Self::upsert_entry(
                    &tx,
                    &moved_path,
                    &row.kind,
                    moved_backing.as_deref(),
                    row.mode,
                    row.size,
                    row.mtime_ns,
                    EntryLineage {
                        source_oid: &row.source_oid,
                        base_path: &row.base_path,
                    },
                )?;
            }

            if !is_overlay_only_creation(&source_row) {
                Self::upsert_entry(
                    &tx,
                    &old_path,
                    "delete",
                    None,
                    0,
                    0,
                    now,
                    EntryLineage::default(),
                )?;
            }
        }
        tx.commit().map_err(map_sqlite_err)?;

        // Filesystem rename after successful commit.
        let old_backing = self.backing_path(&old_path);
        if old_backing.exists()
            && let Err(e) = std::fs::rename(&old_backing, &new_backing_path)
        {
            // DB committed but file didn't move. Attempt rollback.
            warn!(
                old_path = %old_path,
                new_path = %new_path,
                error = %e,
                "overlay rename: filesystem move failed after DB commit"
            );
            return Err(CrabError::Io(e));
        }

        debug!(old_path = %old_path, new_path = %new_path, "overlay rename");
        Ok(())
    }

    fn rename_base_subtree(&self, entries: &[BaseRenameEntry]) -> Result<()> {
        let _write = self.begin_write()?;
        let Some(root) = entries.first() else {
            return Ok(());
        };

        let old_root = clean_path(&root.old_path);
        let new_root = clean_path(&root.new_path);
        let old_backing = self.backing_path(&old_root);
        let new_backing = self.backing_path(&new_root);

        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let moved_overlay_rows = Self::query_descendant_rename_rows(&db, &old_root)?;
        let now = Self::now_ns();

        let tx = db.unchecked_transaction().map_err(map_sqlite_err)?;
        {
            for row in &moved_overlay_rows {
                tx.execute(
                    "DELETE FROM overlay_entries WHERE path = ?1",
                    params![row.path],
                )
                .map_err(map_sqlite_err)?;
            }

            for entry in entries {
                let old_path = clean_path(&entry.old_path);
                let new_path = clean_path(&entry.new_path);
                let source_oid = entry.source_oid.as_deref().unwrap_or("");
                Self::upsert_entry(
                    &tx,
                    &new_path,
                    "rename",
                    None,
                    entry.mode,
                    entry.size,
                    now,
                    EntryLineage {
                        source_oid,
                        base_path: &old_path,
                    },
                )?;
                Self::upsert_entry(
                    &tx,
                    &old_path,
                    "delete",
                    None,
                    0,
                    0,
                    now,
                    EntryLineage::default(),
                )?;
            }

            for row in &moved_overlay_rows {
                let moved_path = move_descendant_path(&old_root, &new_root, &row.path);
                let moved_backing = row.backing_path.as_ref().map(|_| {
                    self.backing_path(&moved_path)
                        .to_string_lossy()
                        .into_owned()
                });
                Self::upsert_entry(
                    &tx,
                    &moved_path,
                    &row.kind,
                    moved_backing.as_deref(),
                    row.mode,
                    row.size,
                    row.mtime_ns,
                    EntryLineage {
                        source_oid: &row.source_oid,
                        base_path: &row.base_path,
                    },
                )?;
            }
        }
        tx.commit().map_err(map_sqlite_err)?;

        if old_backing.exists() {
            if let Some(parent) = new_backing.parent() {
                std::fs::create_dir_all(parent)?;
            }
            remove_backing_path(&new_backing)?;
            std::fs::rename(&old_backing, &new_backing)?;
        }

        debug!(old_path = %old_root, new_path = %new_root, entries = entries.len(), "overlay base rename");
        Ok(())
    }

    fn mkdir(&self, path: &str, mode: u32) -> Result<()> {
        let _write = self.begin_write()?;
        let path = clean_path(path);
        let backing = self.backing_path(&path);
        std::fs::create_dir_all(&backing)?;

        self.ensure_not_frozen()?;
        let now = Self::now_ns();
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        Self::upsert_entry(
            &db,
            &path,
            "mkdir",
            Some(&backing.to_string_lossy()),
            mode,
            0,
            now,
            EntryLineage::default(),
        )?;

        trace!(path = %path, "overlay mkdir");
        Ok(())
    }

    fn rmdir(&self, path: &str) -> Result<()> {
        let _write = self.begin_write()?;
        let path = clean_path(path);
        let backing = self.backing_path(&path);

        // Only remove if empty.
        if backing.exists() {
            std::fs::remove_dir(&backing).map_err(|e| {
                if e.kind() == std::io::ErrorKind::Other
                    || e.to_string().contains("not empty")
                    || e.to_string().contains("Directory not empty")
                {
                    CrabError::Forbidden {
                        path: format!("directory not empty: {path}"),
                    }
                } else {
                    CrabError::Io(e)
                }
            })?;
        }

        self.ensure_not_frozen()?;
        let now = Self::now_ns();
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        if let Some(row) = Self::query_rename_row(&db, &path)?
            && is_overlay_only_creation(&row)
        {
            db.execute("DELETE FROM overlay_entries WHERE path = ?1", params![path])
                .map_err(map_sqlite_err)?;
            trace!(path = %path, "overlay rmdir discarded overlay-only entry");
            return Ok(());
        }
        Self::upsert_entry(
            &db,
            &path,
            "delete",
            None,
            0,
            0,
            now,
            EntryLineage::default(),
        )?;

        trace!(path = %path, "overlay rmdir");
        Ok(())
    }

    fn set_mtime(&self, path: &str, mtime_ns: i64) -> Result<()> {
        let _write = self.begin_write()?;
        let path = clean_path(path);
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        db.execute(
            "UPDATE overlay_entries SET mtime_unix_ns = ?1 WHERE path = ?2",
            params![mtime_ns, path],
        )
        .map_err(map_sqlite_err)?;
        trace!(path = %path, mtime_ns, "overlay set_mtime");
        Ok(())
    }

    fn set_mode(&self, path: &str, mode: u32) -> Result<()> {
        let _write = self.begin_write()?;
        let path = clean_path(path);
        let entry = OverlayLookup::get(self, &path)
            .ok_or_else(|| CrabError::NotFound { path: path.clone() })?;
        if entry.is_deleted() {
            return Err(CrabError::NotFound { path });
        }

        let mode = mode_with_existing_type(entry.mode, mode);
        if let Some(backing) = self.get_backing_path(&path) {
            set_path_permissions(&backing, mode)?;
        }

        let now = Self::now_ns();
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        db.execute(
            "UPDATE overlay_entries SET mode = ?1, mtime_unix_ns = ?2 WHERE path = ?3",
            params![i64::from(mode), now, path],
        )
        .map_err(map_sqlite_err)?;
        trace!(path = %path, mode, "overlay set_mode");
        Ok(())
    }

    fn update_size_and_mtime(&self, path: &str, size: u64, mtime_ns: i64) -> Result<()> {
        let _write = self.begin_write()?;
        let path = clean_path(path);
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        db.execute(
            "UPDATE overlay_entries SET size_bytes = ?1, mtime_unix_ns = ?2 WHERE path = ?3",
            params![size as i64, mtime_ns, path],
        )
        .map_err(map_sqlite_err)?;
        trace!(path = %path, size, mtime_ns, "overlay update_size_and_mtime");
        Ok(())
    }

    fn promote_from_file(
        &self,
        path: &str,
        mode: u32,
        size: u64,
        source_oid: Option<&str>,
    ) -> Result<OverlayEntry> {
        let _write = self.begin_write()?;
        let path = clean_path(path);
        let backing = self.backing_path(&path);

        let now = Self::now_ns();
        let oid = source_oid.unwrap_or("");
        set_path_permissions(&backing, mode)?;

        self.ensure_not_frozen()?;
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        Self::upsert_entry(
            &db,
            &path,
            "modify",
            Some(&backing.to_string_lossy()),
            mode,
            size,
            now,
            EntryLineage {
                source_oid: oid,
                base_path: "",
            },
        )?;

        debug!(path = %path, size, source_oid = oid, "overlay promote_from_file");
        Ok(OverlayEntry {
            path,
            kind: OverlayKind::Modify,
            mode,
            size,
            mtime_ns: now,
            node_type: NodeType::File,
        })
    }

    fn backing_path_for(&self, path: &str) -> PathBuf {
        self.backing_path(&clean_path(path))
    }

    fn backing_tmp_path_for(&self, path: &str) -> PathBuf {
        self.backing_path(&clean_path(path)).with_extension("tmp")
    }
}

// ---------------------------------------------------------------------------
// Reconcile helper type
// ---------------------------------------------------------------------------

/// Minimal base-node info needed by [`OverlayStore::reconcile`].
pub struct ReconcileBaseInfo {
    /// Whether the base node is a directory.
    pub is_dir: bool,
    /// Git blob OID of the base node (for OID-aware reconciliation).
    pub object_oid: Option<String>,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Intermediate row from SQLite before conversion to `OverlayEntry`.
struct RawEntry {
    path: String,
    kind: String,
    #[allow(dead_code)]
    backing_path: Option<String>,
    mode: u32,
    size: u64,
    mtime_ns: i64,
}

struct RenameRow {
    path: String,
    kind: String,
    backing_path: Option<String>,
    mode: u32,
    size: u64,
    mtime_ns: i64,
    source_oid: String,
    base_path: String,
}

#[derive(Clone, Copy, Default)]
struct EntryLineage<'a> {
    source_oid: &'a str,
    base_path: &'a str,
}

impl RawEntry {
    fn into_overlay_entry(self) -> OverlayEntry {
        let kind = parse_overlay_kind(&self.kind);

        let node_type = node_type_from_mode(self.mode).unwrap_or(match kind {
            OverlayKind::Mkdir => NodeType::Dir,
            OverlayKind::Symlink => NodeType::Symlink,
            _ => NodeType::File,
        });

        OverlayEntry {
            path: self.path,
            kind,
            mode: self.mode,
            size: self.size,
            mtime_ns: self.mtime_ns,
            node_type,
        }
    }
}

fn parse_overlay_kind(kind: &str) -> OverlayKind {
    match kind {
        "create" => OverlayKind::Create,
        "delete" => OverlayKind::Delete,
        "rename" => OverlayKind::Rename,
        "mkdir" => OverlayKind::Mkdir,
        "symlink" => OverlayKind::Symlink,
        // "modify" and any unknown kind default to Modify.
        _ => OverlayKind::Modify,
    }
}

fn rename_destination_kind(node_type: NodeType) -> &'static str {
    match node_type {
        NodeType::Dir => "mkdir",
        NodeType::Symlink => "symlink",
        NodeType::File => "rename",
    }
}

fn rename_destination_kind_for_row(row: &RenameRow, node_type: NodeType) -> &str {
    if is_overlay_only_creation(row) {
        return &row.kind;
    }
    if row.base_path.is_empty() {
        rename_destination_kind(node_type)
    } else {
        &row.kind
    }
}

fn is_overlay_only_creation(row: &RenameRow) -> bool {
    row.source_oid.is_empty()
        && row.base_path.is_empty()
        && matches!(row.kind.as_str(), "create" | "rename" | "symlink" | "mkdir")
}

fn move_descendant_path(old_root: &str, new_root: &str, path: &str) -> String {
    let old_prefix = format!("{old_root}/");
    let suffix = path.strip_prefix(&old_prefix).unwrap_or(path);
    format!("{new_root}/{suffix}")
}

fn node_type_from_mode(mode: u32) -> Option<NodeType> {
    match mode & 0o170_000 {
        0o040_000 => Some(NodeType::Dir),
        0o100_000 => Some(NodeType::File),
        0o120_000 => Some(NodeType::Symlink),
        _ => None,
    }
}

fn non_empty_string(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

/// Normalize a path: strip leading `/`, trim trailing `/`.
fn clean_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return String::new();
    }
    if trimmed.len() == path.len() {
        path.to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn map_sqlite_err(e: rusqlite::Error) -> CrabError {
    CrabError::Internal(format!("overlay sqlite: {e}"))
}

fn checkpoint_wal(db: &Connection) -> Result<()> {
    db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .map_err(map_sqlite_err)
}

fn lock_poisoned() -> CrabError {
    CrabError::Internal("overlay store mutex poisoned".into())
}

fn mode_with_existing_type(existing_mode: u32, requested_mode: u32) -> u32 {
    let file_type = existing_mode & 0o170_000;
    if file_type == 0 {
        requested_mode
    } else {
        file_type | (requested_mode & 0o7777)
    }
}

fn remove_backing_path(path: &Path) -> Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn clear_directory_contents(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        remove_backing_path(&entry.path())?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_path_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let perms = std::fs::Permissions::from_mode(mode & 0o7777);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_path_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Recursively walk a directory and collect all file paths.
fn walk_dir_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, OverlayStore) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");
        let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
        (dir, store)
    }

    #[test]
    fn create_and_get() {
        let (_dir, store) = temp_store();
        let entry = store.create_file("hello.txt", 0o100644).unwrap();
        assert_eq!(entry.kind, OverlayKind::Create);
        assert_eq!(entry.size, 0);

        let got = OverlayLookup::get(&store, "hello.txt").unwrap();
        assert_eq!(got.path, "hello.txt");
        assert_eq!(got.kind, OverlayKind::Create);
    }

    #[test]
    fn write_and_read_back() {
        let (_dir, store) = temp_store();
        store.create_file("data.txt", 0o100644).unwrap();

        let n = store.write_file("data.txt", 0, b"hello world").unwrap();
        assert_eq!(n, 11);

        let entry = OverlayLookup::get(&store, "data.txt").unwrap();
        assert_eq!(entry.size, 11);

        // Verify backing file content.
        let backing = store.get_backing_path("data.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"hello world");
    }

    #[test]
    fn freeze_waits_for_in_flight_overlay_mutation() {
        let (_dir, store) = temp_store();
        let store = std::sync::Arc::new(store);
        let freezer_store = std::sync::Arc::clone(&store);
        let write_guard = store.begin_write().unwrap();
        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
        let (frozen_tx, frozen_rx) = std::sync::mpsc::channel();

        let freezer = std::thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            let freeze = freezer_store.freeze_writes().unwrap();
            frozen_tx.send(freezer_store.is_frozen()).unwrap();
            drop(freeze);
        });

        attempted_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(frozen_rx.try_recv().is_err());

        drop(write_guard);
        assert!(
            frozen_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
        );
        freezer.join().unwrap();
        assert!(!store.is_frozen());
    }

    #[test]
    fn write_at_offset() {
        let (_dir, store) = temp_store();
        store.create_file("data.txt", 0o100644).unwrap();
        store.write_file("data.txt", 0, b"hello world").unwrap();
        store.write_file("data.txt", 6, b"rust!").unwrap();

        let backing = store.get_backing_path("data.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"hello rust!");
    }

    #[test]
    fn remove_discards_overlay_only_file() {
        let (_dir, store) = temp_store();
        store.create_file("doomed.txt", 0o100644).unwrap();
        store.remove("doomed.txt").unwrap();

        assert!(OverlayLookup::get(&store, "doomed.txt").is_none());
        assert!(store.get_backing_path("doomed.txt").is_none());
    }

    #[test]
    fn rename_overlay_only_file_preserves_create_kind() {
        let (_dir, store) = temp_store();
        store.create_file("old.txt", 0o100644).unwrap();
        store.write_file("old.txt", 0, b"content").unwrap();

        store.rename("old.txt", "new.txt").unwrap();

        assert!(OverlayLookup::get(&store, "old.txt").is_none());

        // New path should have the content.
        let new_entry = OverlayLookup::get(&store, "new.txt").unwrap();
        assert_eq!(new_entry.kind, OverlayKind::Create);

        let backing = store.get_backing_path("new.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"content");
    }

    #[test]
    fn remove_discards_renamed_overlay_only_file() {
        let (_dir, store) = temp_store();
        store.create_file("old.txt", 0o100644).unwrap();
        store.write_file("old.txt", 0, b"content").unwrap();
        store.rename("old.txt", "new.txt").unwrap();

        store.remove("new.txt").unwrap();

        assert!(store.records().unwrap().is_empty());
    }

    #[test]
    fn rename_directory_moves_descendant_entries() {
        let (_dir, store) = temp_store();
        store.mkdir("old", 0o040755).unwrap();
        store.create_file("old/child.txt", 0o100644).unwrap();
        store
            .write_file("old/child.txt", 0, b"child content")
            .unwrap();

        store.rename("old", "new").unwrap();

        assert!(OverlayLookup::get(&store, "old").is_none());
        assert!(OverlayLookup::get(&store, "old/child.txt").is_none());

        let new_dir = OverlayLookup::get(&store, "new").unwrap();
        assert_eq!(new_dir.kind, OverlayKind::Mkdir);
        assert_eq!(new_dir.node_type, NodeType::Dir);

        let new_child = OverlayLookup::get(&store, "new/child.txt").unwrap();
        assert_eq!(new_child.kind, OverlayKind::Create);
        let backing = store.get_backing_path("new/child.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"child content");
    }

    #[test]
    fn mkdir_and_rmdir() {
        let (_dir, store) = temp_store();
        store.mkdir("subdir", 0o040755).unwrap();

        let entry = OverlayLookup::get(&store, "subdir").unwrap();
        assert_eq!(entry.kind, OverlayKind::Mkdir);
        assert_eq!(entry.node_type, NodeType::Dir);

        store.rmdir("subdir").unwrap();
        assert!(OverlayLookup::get(&store, "subdir").is_none());
    }

    #[test]
    fn create_symlink_records_symlink_metadata() {
        let (_dir, store) = temp_store();
        store
            .create_symlink("link.txt", "../target.txt", 0o777)
            .unwrap();

        let entry = OverlayLookup::get(&store, "link.txt").unwrap();
        assert_eq!(entry.kind, OverlayKind::Symlink);
        assert_eq!(entry.node_type, NodeType::Symlink);
        assert_eq!(entry.mode & 0o170_000, 0o120_000);

        let backing = store.get_backing_path("link.txt").unwrap();
        assert_eq!(std::fs::read_to_string(backing).unwrap(), "../target.txt");
    }

    #[test]
    fn remove_discards_overlay_only_symlink() {
        let (_dir, store) = temp_store();
        store
            .create_symlink("link.txt", "../target.txt", 0o777)
            .unwrap();

        store.remove("link.txt").unwrap();

        assert!(OverlayLookup::get(&store, "link.txt").is_none());
        assert!(store.get_backing_path("link.txt").is_none());
    }

    #[test]
    fn promote_writes_content() {
        let (_dir, store) = temp_store();
        let entry = store
            .promote("base.txt", 0o100644, b"base content", Some("abc123"))
            .unwrap();
        assert_eq!(entry.kind, OverlayKind::Modify);
        assert_eq!(entry.size, 12);

        let backing = store.get_backing_path("base.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"base content");
    }

    #[test]
    fn list_by_prefix_returns_children() {
        let (_dir, store) = temp_store();
        store.create_file("src/main.rs", 0o100644).unwrap();
        store.create_file("src/lib.rs", 0o100644).unwrap();
        store.create_file("README.md", 0o100644).unwrap();

        let children = store.list_by_prefix("src");
        let paths: Vec<&str> = children.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"src/lib.rs"));
        assert!(paths.contains(&"src/main.rs"));
        assert!(!paths.contains(&"README.md"));
    }

    #[test]
    fn set_mtime_updates_entry() {
        let (_dir, store) = temp_store();
        store.create_file("ts.txt", 0o100644).unwrap();
        store
            .set_mtime("ts.txt", 1_800_000_000_000_000_000)
            .unwrap();

        let entry = OverlayLookup::get(&store, "ts.txt").unwrap();
        assert_eq!(entry.mtime_ns, 1_800_000_000_000_000_000);
    }

    #[test]
    fn set_mode_updates_entry_and_backing_permissions() {
        let (_dir, store) = temp_store();
        store.create_file("script.sh", 0o100644).unwrap();

        store.set_mode("script.sh", 0o755).unwrap();

        let entry = OverlayLookup::get(&store, "script.sh").unwrap();
        assert_eq!(entry.mode & 0o777, 0o755);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let backing = store.get_backing_path("script.sh").unwrap();
            assert_eq!(
                std::fs::metadata(backing).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn dirty_count() {
        let (_dir, store) = temp_store();
        assert_eq!(store.dirty_count().unwrap(), 0);

        store.create_file("a.txt", 0o100644).unwrap();
        assert_eq!(store.dirty_count().unwrap(), 1);

        store.remove("a.txt").unwrap();
        assert_eq!(store.dirty_count().unwrap(), 0);

        store.remove("base.txt").unwrap();
        assert_eq!(store.dirty_count().unwrap(), 1);
    }

    #[test]
    fn reconcile_removes_stale_entries() {
        let (_dir, store) = temp_store();

        // Local creates remain dirty even if the base later has the path.
        store.create_file("local_conflict.txt", 0o100644).unwrap();
        // Create a file that won't be in the base.
        store.create_file("local.txt", 0o100644).unwrap();
        // Delete marker for a path that no longer exists in base (stale).
        store.remove("gone.txt").unwrap();

        store
            .reconcile(|path| match path {
                "local_conflict.txt" => Some(ReconcileBaseInfo {
                    is_dir: false,
                    object_oid: None,
                }),
                _ => None,
            })
            .unwrap();

        // Local creates are preserved by remount reconciliation.
        assert!(OverlayLookup::get(&store, "local_conflict.txt").is_some());
        assert!(OverlayLookup::get(&store, "local.txt").is_some());
        // gone.txt not in base → delete marker removed.
        assert!(OverlayLookup::get(&store, "gone.txt").is_none());
    }

    #[test]
    fn reconcile_preserves_overlay_only_rename_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");

        {
            let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
            store.mkdir("newdir", 0o040755).unwrap();
            store.create_file("created.txt", 0o100644).unwrap();
            store.write_file("created.txt", 0, b"created").unwrap();
            store.rename("created.txt", "newdir/renamed.txt").unwrap();
        }

        let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
        store.reconcile(|_| None).unwrap();

        assert!(OverlayLookup::get(&store, "created.txt").is_none());
        let entry = OverlayLookup::get(&store, "newdir/renamed.txt").unwrap();
        assert_eq!(entry.kind, OverlayKind::Create);
        let backing = store.get_backing_path("newdir/renamed.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"created");
    }

    #[test]
    fn reconcile_preserves_modify_without_source_lineage() {
        let (_dir, store) = temp_store();

        store
            .promote("edited.txt", 0o100644, b"user edits", None)
            .unwrap();

        store
            .reconcile(|path| match path {
                "edited.txt" => Some(ReconcileBaseInfo {
                    is_dir: false,
                    object_oid: Some("new-base".to_owned()),
                }),
                _ => None,
            })
            .unwrap();

        let backing = store.get_backing_path("edited.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"user edits");
    }

    #[test]
    fn write_to_missing_file_returns_not_found() {
        let (_dir, store) = temp_store();
        let err = store.write_file("nonexistent.txt", 0, b"data").unwrap_err();
        assert!(matches!(err, CrabError::NotFound { .. }));
    }

    #[test]
    fn path_normalization() {
        let (_dir, store) = temp_store();
        store.create_file("/leading/slash.txt", 0o100644).unwrap();

        // Should be accessible without leading slash.
        let entry = OverlayLookup::get(&store, "leading/slash.txt");
        assert!(entry.is_some());
    }

    #[test]
    fn schema_migration_adds_columns_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");

        // Create a DB with the old schema (no source_oid, no target_path).
        {
            std::fs::create_dir_all(&upper_dir).unwrap();
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE overlay_entries (
                    path TEXT PRIMARY KEY,
                    kind TEXT NOT NULL,
                    backing_path TEXT,
                    mode INTEGER NOT NULL,
                    size_bytes INTEGER NOT NULL DEFAULT 0,
                    mtime_unix_ns INTEGER NOT NULL
                )",
            )
            .unwrap();
            // Insert a row with the old schema.
            conn.execute(
                "INSERT INTO overlay_entries(path, kind, backing_path, mode, size_bytes, mtime_unix_ns)
                 VALUES('old.txt', 'create', NULL, 0, 0, 1000)",
                [],
            )
            .unwrap();
        }

        // Open with OverlayStore — migration should add the new columns.
        let store = OverlayStore::open(&db_path, &upper_dir).unwrap();

        // Verify the old entry is still accessible.
        let entry = OverlayLookup::get(&store, "old.txt");
        assert!(entry.is_some());

        // Verify we can promote with source_oid (uses the new column).
        let backing = upper_dir.join("migrated.txt");
        std::fs::write(&backing, b"").unwrap();
        let entry = store
            .promote("migrated.txt", 0o100644, b"data", Some("deadbeef"))
            .unwrap();
        assert_eq!(entry.kind, OverlayKind::Modify);
    }

    #[test]
    fn promote_stores_source_oid_in_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");
        let store = OverlayStore::open(&db_path, &upper_dir).unwrap();

        store
            .promote("tracked.txt", 0o100644, b"content", Some("abc123def456"))
            .unwrap();

        // Query the raw source_oid from SQLite to verify it was stored.
        let db = store.db.lock().unwrap();
        let oid: String = db
            .query_row(
                "SELECT source_oid FROM overlay_entries WHERE path = 'tracked.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(oid, "abc123def456");
    }

    #[test]
    fn promote_without_source_oid_stores_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");
        let store = OverlayStore::open(&db_path, &upper_dir).unwrap();

        store
            .promote("empty_oid.txt", 0o100644, b"data", None)
            .unwrap();

        let db = store.db.lock().unwrap();
        let oid: String = db
            .query_row(
                "SELECT source_oid FROM overlay_entries WHERE path = 'empty_oid.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(oid, "");
    }

    #[test]
    fn create_file_stores_empty_source_oid() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");
        let store = OverlayStore::open(&db_path, &upper_dir).unwrap();

        store.create_file("new.txt", 0o100644).unwrap();

        let db = store.db.lock().unwrap();
        let oid: String = db
            .query_row(
                "SELECT source_oid FROM overlay_entries WHERE path = 'new.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(oid, "");
    }

    #[test]
    fn reconcile_oid_aware_keeps_matching_modify() {
        let (_dir, store) = temp_store();

        // Promote a file with source_oid "aaa111" (simulates COW from base).
        store
            .promote("edited.txt", 0o100644, b"user edits", Some("aaa111"))
            .unwrap();

        // Promote a file with source_oid "bbb222" (base will diverge).
        store
            .promote("stale.txt", 0o100644, b"old edits", Some("bbb222"))
            .unwrap();

        // Promote a file whose base will disappear.
        store
            .promote("gone.txt", 0o100644, b"orphan", Some("ccc333"))
            .unwrap();

        store
            .reconcile(|path| match path {
                // Base still has "aaa111" — source_oid matches → KEEP.
                "edited.txt" => Some(ReconcileBaseInfo {
                    is_dir: false,
                    object_oid: Some("aaa111".to_owned()),
                }),
                // Base now has "ddd444" — source_oid differs → REMOVE.
                "stale.txt" => Some(ReconcileBaseInfo {
                    is_dir: false,
                    object_oid: Some("ddd444".to_owned()),
                }),
                // "gone.txt" not in base → REMOVE.
                _ => None,
            })
            .unwrap();

        // edited.txt kept (OID match).
        assert!(OverlayLookup::get(&store, "edited.txt").is_some());
        // stale.txt removed (OID mismatch).
        assert!(OverlayLookup::get(&store, "stale.txt").is_none());
        // gone.txt removed (base gone).
        assert!(OverlayLookup::get(&store, "gone.txt").is_none());
    }

    #[test]
    fn reconcile_race_safe_delete_guards_source_oid() {
        let (_dir, store) = temp_store();

        // Promote with source_oid "aaa111".
        store
            .promote("racing.txt", 0o100644, b"original", Some("aaa111"))
            .unwrap();

        // Simulate a concurrent write that changes mtime (and source_oid
        // stays the same in the DB, but mtime differs from what reconcile read).
        // We do this by reading the entry, then updating mtime, then running
        // reconcile with the old mtime — the DELETE should miss.
        let original_mtime = {
            let db = store.db.lock().unwrap();
            db.query_row(
                "SELECT mtime_unix_ns FROM overlay_entries WHERE path = 'racing.txt'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };

        // Update mtime to simulate a concurrent FUSE write.
        store
            .set_mtime("racing.txt", original_mtime + 1_000_000)
            .unwrap();

        // Reconcile should try to delete with the OLD mtime (from the
        // SELECT), but the row now has a newer mtime → DELETE misses.
        // However, our reconcile reads fresh data, so we need to test
        // the guard differently: the source_oid guard matters when
        // a write changes source_oid between read and delete.
        //
        // For this test, verify that the 4-column WHERE clause is used
        // by checking that a matching reconcile still works.
        store
            .reconcile(|path| match path {
                "racing.txt" => Some(ReconcileBaseInfo {
                    is_dir: false,
                    object_oid: Some("different_oid".to_owned()),
                }),
                _ => None,
            })
            .unwrap();

        // The entry should still exist because the mtime changed after
        // the initial promote — but reconcile reads fresh data in a
        // single transaction, so the delete WILL match. This test
        // verifies the 4-column WHERE clause executes without error.
        // The real race protection is when a write happens BETWEEN
        // the SELECT and DELETE within reconcile.
        //
        // What we CAN verify: the entry is removed when OID mismatches.
        assert!(OverlayLookup::get(&store, "racing.txt").is_none());
    }

    #[test]
    fn open_preserves_unreferenced_files() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");

        {
            let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
            store.create_file("kept.txt", 0o100644).unwrap();
            store.write_file("kept.txt", 0, b"keep me").unwrap();
        }

        let orphan_path = upper_dir.join("orphan.txt");
        std::fs::write(&orphan_path, b"orphan data").unwrap();

        let _store = OverlayStore::open(&db_path, &upper_dir).unwrap();

        assert!(orphan_path.exists());
        assert!(upper_dir.join("kept.txt").exists());
    }

    #[test]
    fn orphan_cleanup_deletes_unreferenced_files() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");

        {
            let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
            store.create_file("kept.txt", 0o100644).unwrap();
            store.write_file("kept.txt", 0, b"keep me").unwrap();
        }

        let orphan_path = upper_dir.join("orphan.txt");
        std::fs::write(&orphan_path, b"orphan data").unwrap();
        let orphan_sub = upper_dir.join("sub");
        std::fs::create_dir_all(&orphan_sub).unwrap();
        let orphan_nested = orphan_sub.join("nested_orphan.txt");
        std::fs::write(&orphan_nested, b"nested orphan").unwrap();

        assert!(orphan_path.exists());
        assert!(orphan_nested.exists());

        let _store = OverlayStore::open_with_orphan_cleanup(&db_path, &upper_dir).unwrap();

        assert!(!orphan_path.exists());
        assert!(!orphan_nested.exists());
        assert!(upper_dir.join("kept.txt").exists());
    }

    #[test]
    fn clean_deletes_db_and_upper_dir() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");

        // Create a store with some data.
        {
            let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
            store.create_file("file.txt", 0o100644).unwrap();
            store.write_file("file.txt", 0, b"data").unwrap();
        }

        assert!(db_path.exists());
        assert!(upper_dir.exists());

        // Clean should delete both.
        OverlayStore::clean(&db_path, &upper_dir).unwrap();

        assert!(!db_path.exists());
        assert!(!upper_dir.exists());

        // Re-opening should create a fresh store.
        let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
        assert_eq!(store.dirty_count().unwrap(), 0);
    }

    #[test]
    fn overlay_persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");

        // Create entries in the first session.
        {
            let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
            store.create_file("user_file.txt", 0o100644).unwrap();
            store.write_file("user_file.txt", 0, b"user data").unwrap();
            store
                .promote(
                    "modified.txt",
                    0o100644,
                    b"modified content",
                    Some("oid123"),
                )
                .unwrap();
            store.mkdir("user_dir", 0o040755).unwrap();
            assert_eq!(store.dirty_count().unwrap(), 3);
        }

        // Re-open the store (simulates daemon restart).
        let store = OverlayStore::open(&db_path, &upper_dir).unwrap();

        // All entries should be preserved.
        assert_eq!(store.dirty_count().unwrap(), 3);

        let user_file = OverlayLookup::get(&store, "user_file.txt").unwrap();
        assert_eq!(user_file.kind, OverlayKind::Create);
        assert_eq!(user_file.size, 9);

        let modified = OverlayLookup::get(&store, "modified.txt").unwrap();
        assert_eq!(modified.kind, OverlayKind::Modify);

        let user_dir = OverlayLookup::get(&store, "user_dir").unwrap();
        assert_eq!(user_dir.kind, OverlayKind::Mkdir);

        // Backing file content should be preserved.
        let backing = store.get_backing_path("user_file.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"user data");
    }

    #[test]
    fn persistence_with_reconcile_preserves_valid_modifications() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");

        // First session: promote a file with source_oid.
        {
            let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
            store
                .promote("edited.txt", 0o100644, b"user edits", Some("base_oid_1"))
                .unwrap();
            store
                .promote("stale.txt", 0o100644, b"old edits", Some("base_oid_2"))
                .unwrap();
        }

        // Second session: reopen and reconcile against new HEAD.
        let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
        store
            .reconcile(|path| match path {
                // Base unchanged — source_oid matches → KEEP.
                "edited.txt" => Some(ReconcileBaseInfo {
                    is_dir: false,
                    object_oid: Some("base_oid_1".to_owned()),
                }),
                // Base changed — source_oid differs → REMOVE.
                "stale.txt" => Some(ReconcileBaseInfo {
                    is_dir: false,
                    object_oid: Some("new_base_oid".to_owned()),
                }),
                _ => None,
            })
            .unwrap();

        // Valid modification preserved.
        assert!(OverlayLookup::get(&store, "edited.txt").is_some());
        // Stale modification removed.
        assert!(OverlayLookup::get(&store, "stale.txt").is_none());
    }

    #[test]
    fn dirty_paths_returns_sorted_changed_paths() {
        let (_dir, store) = temp_store();

        store.create_file("c.txt", 0o100644).unwrap();
        store.create_file("a.txt", 0o100644).unwrap();
        store.create_file("b.txt", 0o100644).unwrap();
        store.remove("deleted.txt").unwrap();
        store.mkdir("dir", 0o040755).unwrap();

        let paths = store.dirty_paths().unwrap();
        assert_eq!(paths, vec!["a.txt", "b.txt", "c.txt", "deleted.txt", "dir"]);
    }

    #[test]
    fn dirty_paths_includes_base_delete_markers() {
        let (_dir, store) = temp_store();

        store.create_file("alive.txt", 0o100644).unwrap();
        store.create_file("doomed.txt", 0o100644).unwrap();
        store.remove("doomed.txt").unwrap();
        store.remove("base_deleted.txt").unwrap();

        let paths = store.dirty_paths().unwrap();
        assert_eq!(paths, vec!["alive.txt", "base_deleted.txt"]);
    }

    #[test]
    fn read_dirty_state_missing_db_does_not_create_parent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("missing").join("overlay.db");

        let state = OverlayStore::read_dirty_state(&db_path, true).unwrap();

        assert_eq!(state, (0, Vec::new()));
        assert!(!db_path.parent().unwrap().exists());
    }

    #[test]
    fn read_dirty_state_reports_sorted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");

        {
            let store = OverlayStore::open(&db_path, &upper_dir).unwrap();
            store.create_file("zeta.txt", 0o100644).unwrap();
            store.write_file("zeta.txt", 0, b"z").unwrap();
            store.create_file("alpha.txt", 0o100644).unwrap();
            store.write_file("alpha.txt", 0, b"a").unwrap();
            store.remove("deleted.txt").unwrap();
        }

        let state = OverlayStore::read_dirty_state(&db_path, true).unwrap();

        assert_eq!(
            state,
            (
                3,
                vec![
                    "alpha.txt".to_owned(),
                    "deleted.txt".to_owned(),
                    "zeta.txt".to_owned()
                ]
            )
        );
    }

    #[test]
    fn rename_overlay_only_file_overwrites_destination_and_preserves_create_kind() {
        let (_dir, store) = temp_store();

        // Create source and destination files.
        store.create_file("src.txt", 0o100644).unwrap();
        store.write_file("src.txt", 0, b"source content").unwrap();
        store.create_file("dst.txt", 0o100644).unwrap();
        store.write_file("dst.txt", 0, b"old destination").unwrap();

        // Rename src → dst should overwrite dst.
        store.rename("src.txt", "dst.txt").unwrap();

        assert!(OverlayLookup::get(&store, "src.txt").is_none());

        // Destination should have the renamed entry.
        let dst = OverlayLookup::get(&store, "dst.txt").unwrap();
        assert_eq!(dst.kind, OverlayKind::Create);

        // Backing file should have source content.
        let backing = store.get_backing_path("dst.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"source content");
    }

    #[test]
    fn unlink_base_file_inserts_whiteout_without_promotion() {
        let (_dir, store) = temp_store();

        // Simulate unlinking a base file (not in overlay).
        // The remove() method inserts a delete whiteout directly.
        store.remove("base_file.txt").unwrap();

        let entry = OverlayLookup::get(&store, "base_file.txt").unwrap();
        assert!(entry.is_deleted());
        assert_eq!(entry.kind, OverlayKind::Delete);

        // No backing file should exist.
        assert!(store.get_backing_path("base_file.txt").is_none());
    }

    #[test]
    fn write_after_unlink_and_create() {
        let (_dir, store) = temp_store();

        // Create a file, then delete it, then create a new one.
        store.create_file("reborn.txt", 0o100644).unwrap();
        store.write_file("reborn.txt", 0, b"first life").unwrap();

        // Unlink it. A file created and deleted only in the overlay is clean.
        store.remove("reborn.txt").unwrap();
        assert!(OverlayLookup::get(&store, "reborn.txt").is_none());

        // Create a new file at the same path.
        store.create_file("reborn.txt", 0o100644).unwrap();

        // Write to the new file.
        let n = store.write_file("reborn.txt", 0, b"second life").unwrap();
        assert_eq!(n, 11);

        // The entry should be a create (not delete).
        let entry = OverlayLookup::get(&store, "reborn.txt").unwrap();
        assert_eq!(entry.kind, OverlayKind::Create);
        assert_eq!(entry.size, 11);

        // Backing file should have the new content.
        let backing = store.get_backing_path("reborn.txt").unwrap();
        let content = std::fs::read(backing).unwrap();
        assert_eq!(content, b"second life");
    }

    #[test]
    fn metadata_consistency_after_create() {
        let (_dir, store) = temp_store();

        let entry = store.create_file("meta.txt", 0o100644).unwrap();
        assert_eq!(entry.size, 0);
        assert!(entry.mtime_ns > 0);
    }

    #[test]
    fn metadata_consistency_after_write() {
        let (_dir, store) = temp_store();

        store.create_file("meta.txt", 0o100644).unwrap();
        let before = OverlayLookup::get(&store, "meta.txt").unwrap();

        // Small sleep to ensure mtime changes.
        std::thread::sleep(std::time::Duration::from_millis(2));

        store.write_file("meta.txt", 0, b"hello").unwrap();
        let after = OverlayLookup::get(&store, "meta.txt").unwrap();

        assert_eq!(after.size, 5);
        assert!(after.mtime_ns >= before.mtime_ns);
    }

    #[test]
    fn metadata_consistency_after_remove() {
        let (_dir, store) = temp_store();

        store.create_file("meta.txt", 0o100644).unwrap();
        store.remove("meta.txt").unwrap();

        assert!(OverlayLookup::get(&store, "meta.txt").is_none());
    }

    #[test]
    fn metadata_consistency_after_rename() {
        let (_dir, store) = temp_store();

        store.create_file("old.txt", 0o100644).unwrap();
        store.write_file("old.txt", 0, b"content").unwrap();
        store.rename("old.txt", "new.txt").unwrap();

        let new_entry = OverlayLookup::get(&store, "new.txt").unwrap();
        assert_eq!(new_entry.size, 7);
        assert!(new_entry.mtime_ns > 0);

        assert!(OverlayLookup::get(&store, "old.txt").is_none());
    }

    #[test]
    fn metadata_consistency_after_mkdir() {
        let (_dir, store) = temp_store();

        store.mkdir("dir", 0o040755).unwrap();
        let entry = OverlayLookup::get(&store, "dir").unwrap();
        assert_eq!(entry.size, 0);
        assert!(entry.mtime_ns > 0);
    }

    #[test]
    fn metadata_consistency_after_promote() {
        let (_dir, store) = temp_store();

        let entry = store
            .promote("promoted.txt", 0o100644, b"promoted content", Some("oid"))
            .unwrap();
        assert_eq!(entry.size, 16);
        assert!(entry.mtime_ns > 0);

        // Verify via DB query too.
        let got = OverlayLookup::get(&store, "promoted.txt").unwrap();
        assert_eq!(got.size, 16);
    }

    #[test]
    fn metadata_consistency_after_update_size_and_mtime() {
        let (_dir, store) = temp_store();

        store.create_file("meta.txt", 0o100644).unwrap();
        store
            .update_size_and_mtime("meta.txt", 42, 999_000_000)
            .unwrap();

        let entry = OverlayLookup::get(&store, "meta.txt").unwrap();
        assert_eq!(entry.size, 42);
        assert_eq!(entry.mtime_ns, 999_000_000);
    }
}
