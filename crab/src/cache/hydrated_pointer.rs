//! Per-worktree cache of verified hydrated files.
//!
//! Hydration records the exact no-follow filesystem stat captured from the
//! published file descriptor together with the Crab pointer that produced the
//! content. Sibling worktrees use these rows only as candidate locators and
//! still hash candidates before cloning them.
//!
//! Rows live in a v1 SQLite database. WAL transactions make concurrent
//! hydrators composable, and updates touch only changed rows instead of
//! rewriting the full cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crab_types::pointer::Pointer;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result};

/// Filename inside `.crab/` holding the hydrated-pointer cache.
pub const HYDRATED_POINTERS_FILENAME: &str = "hydrated-pointers-v1.sqlite";
const SCHEMA_VERSION: i64 = 1;
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const JOURNAL_MODE_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Exact stat proof plus the pointer blob for a hydrated working-tree file.
#[derive(Debug, Clone)]
pub struct HydratedEntry {
    stat_token: [u8; 32],
    /// File size captured from the verified published descriptor.
    pub size: u64,
    pointer_bytes: Vec<u8>,
}

/// In-memory snapshot used while discovering sibling-worktree candidates.
#[derive(Debug, Clone, Default)]
pub struct HydratedPointerCache {
    entries: HashMap<String, HydratedEntry>,
}

impl HydratedPointerCache {
    /// Create an empty cache snapshot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a cache snapshot. Missing or invalid databases degrade to empty.
    #[must_use]
    pub fn load_sync(path: &Path) -> Self {
        match load_entries(path) {
            Ok(entries) => Self { entries },
            Err(error) => {
                warn!(
                    path = %path.display(),
                    error = %error,
                    "hydrated-pointer cache unavailable, treating as empty"
                );
                Self::new()
            }
        }
    }

    /// Number of entries in this snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether this snapshot has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Count rows without loading pointer blobs.
    #[must_use]
    pub(crate) fn count_on_disk(path: &Path) -> usize {
        match count_entries(path) {
            Ok(entries) => entries,
            Err(error) => {
                warn!(
                    path = %path.display(),
                    error = %error,
                    "hydrated-pointer cache count unavailable"
                );
                0
            }
        }
    }

    /// Look up a snapshot entry.
    #[must_use]
    pub fn get(&self, rel_path: &str) -> Option<&HydratedEntry> {
        self.entries.get(rel_path)
    }

    /// Iterate over advisory sibling-worktree candidates.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&str, &HydratedEntry)> {
        self.entries
            .iter()
            .map(|(path, entry)| (path.as_str(), entry))
    }

    /// Transactionally upsert only the supplied rows.
    pub fn update_on_disk<I>(path: &Path, updates: I) -> Result<()>
    where
        I: IntoIterator<Item = (String, HydratedEntry)>,
    {
        let updates = updates.into_iter().collect::<Vec<_>>();
        if updates.is_empty() {
            return Ok(());
        }
        let mut connection = open_connection(path)?;
        let transaction = connection
            .transaction()
            .map_err(|error| database_error("begin hydrated-pointer cache update", error))?;
        {
            let mut statement = transaction
                .prepare_cached(
                    "INSERT INTO hydrated_pointers(path, stat_token, size, pointer)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(path) DO UPDATE SET
                         stat_token = excluded.stat_token,
                         size = excluded.size,
                         pointer = excluded.pointer",
                )
                .map_err(|error| database_error("prepare hydrated-pointer cache update", error))?;
            for (relative, entry) in &updates {
                let size = i64::try_from(entry.size).map_err(|error| {
                    CrabError::Internal(format!(
                        "hydrated-pointer cache size {} exceeds SQLite range: {error}",
                        entry.size
                    ))
                })?;
                statement
                    .execute(params![
                        relative,
                        entry.stat_token.as_slice(),
                        size,
                        &entry.pointer_bytes
                    ])
                    .map_err(|error| database_error("write hydrated-pointer cache row", error))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| database_error("commit hydrated-pointer cache update", error))?;
        debug!(
            path = %path.display(),
            entries = updates.len(),
            "updated hydrated-pointer cache"
        );
        Ok(())
    }

    /// Transactionally remove the supplied paths without rewriting other rows.
    pub fn invalidate_on_disk<I>(path: &Path, paths: I) -> Result<()>
    where
        I: IntoIterator<Item = String>,
    {
        let paths = paths.into_iter().collect::<Vec<_>>();
        if paths.is_empty() || !path.exists() {
            return Ok(());
        }
        let mut connection = open_connection(path)?;
        let transaction = connection
            .transaction()
            .map_err(|error| database_error("begin hydrated-pointer cache invalidation", error))?;
        {
            let mut statement = transaction
                .prepare_cached("DELETE FROM hydrated_pointers WHERE path = ?1")
                .map_err(|error| {
                    database_error("prepare hydrated-pointer cache invalidation", error)
                })?;
            for relative in &paths {
                statement
                    .execute([relative])
                    .map_err(|error| database_error("delete hydrated-pointer cache row", error))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| database_error("commit hydrated-pointer cache invalidation", error))
    }
}

/// Build the canonical per-worktree cache path for a resolved context.
#[must_use]
pub fn cache_path_for_context(ctx: &crate::git::worktree::WorktreeContext) -> PathBuf {
    ctx.per_worktree_crab_dir.join(HYDRATED_POINTERS_FILENAME)
}

/// Build the canonical per-worktree cache path for `worktree_root`.
pub fn cache_path_for_worktree_root(worktree_root: &Path) -> Result<PathBuf> {
    let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(worktree_root)?;
    Ok(cache_path_for_context(&ctx))
}

/// Construct an entry from a descriptor-safe post-publication stat.
#[must_use]
pub fn entry_for_verified_stat(
    index_stat: crate::cmd::stream_stage::VerifiedIndexStat,
    pointer_bytes: &[u8],
) -> Option<HydratedEntry> {
    let stat_token = stat_token(index_stat)?;
    let pointer = Pointer::parse(pointer_bytes).ok()?;
    if pointer.size != index_stat.len {
        return None;
    }
    Some(HydratedEntry {
        stat_token,
        size: index_stat.len,
        pointer_bytes: pointer_bytes.to_vec(),
    })
}

/// Construct an entry from the current no-follow path stat.
pub fn entry_for_path(path: &Path, pointer_bytes: &[u8]) -> Result<HydratedEntry> {
    let index_stat = crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(path)
        .ok_or_else(|| {
            CrabError::Internal(format!(
                "could not capture exact hydrated-file stat for {}",
                path.display()
            ))
        })?;
    entry_for_verified_stat(index_stat, pointer_bytes).ok_or_else(|| {
        CrabError::Internal(format!(
            "hydrated file or pointer is not cacheable for {}",
            path.display()
        ))
    })
}

/// Compare an entry with the current exact no-follow filesystem stat.
#[must_use]
pub fn matches_stat(path: &Path, entry: &HydratedEntry) -> bool {
    crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(path)
        .and_then(stat_token)
        .is_some_and(|token| token == entry.stat_token)
}

/// Return validated pointer bytes from an entry.
#[must_use]
pub fn decode_pointer(entry: &HydratedEntry) -> Option<Vec<u8>> {
    Pointer::parse(&entry.pointer_bytes)
        .ok()
        .filter(|pointer| pointer.size == entry.size)
        .map(|_| entry.pointer_bytes.clone())
}

fn stat_token(index_stat: crate::cmd::stream_stage::VerifiedIndexStat) -> Option<[u8; 32]> {
    if !crate::cache::add_validation::stat_is_cacheable(&index_stat.stat) {
        return None;
    }
    let stat = index_stat.stat;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab hydrated pointer stat v1\0");
    hasher.update(&stat.mtime.secs.to_le_bytes());
    hasher.update(&stat.mtime.nsecs.to_le_bytes());
    hasher.update(&stat.ctime.secs.to_le_bytes());
    hasher.update(&stat.ctime.nsecs.to_le_bytes());
    hasher.update(&stat.dev.to_le_bytes());
    hasher.update(&stat.ino.to_le_bytes());
    hasher.update(&stat.uid.to_le_bytes());
    hasher.update(&stat.gid.to_le_bytes());
    hasher.update(&stat.size.to_le_bytes());
    hasher.update(&index_stat.len.to_le_bytes());
    Some(*hasher.finalize().as_bytes())
}

fn load_entries(path: &Path) -> Result<HashMap<String, HydratedEntry>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let connection = open_read_connection(path)?;
    let mut statement = connection
        .prepare("SELECT path, stat_token, size, pointer FROM hydrated_pointers")
        .map_err(|error| database_error("prepare hydrated-pointer cache load", error))?;
    let mut rows = statement
        .query([])
        .map_err(|error| database_error("query hydrated-pointer cache", error))?;
    let mut entries = HashMap::new();
    while let Some(row) = rows
        .next()
        .map_err(|error| database_error("read hydrated-pointer cache row", error))?
    {
        let relative = row
            .get::<_, String>(0)
            .map_err(|error| database_error("read hydrated-pointer cache path", error))?;
        let token = row
            .get::<_, Vec<u8>>(1)
            .map_err(|error| database_error("read hydrated-pointer cache token", error))?;
        let stat_token = <[u8; 32]>::try_from(token.as_slice()).map_err(|_| {
            CrabError::Internal("hydrated-pointer cache contains an invalid stat token".to_owned())
        })?;
        let size = row
            .get::<_, i64>(2)
            .map_err(|error| database_error("read hydrated-pointer cache size", error))?;
        let size = u64::try_from(size).map_err(|error| {
            CrabError::Internal(format!(
                "hydrated-pointer cache contains an invalid size: {error}"
            ))
        })?;
        let pointer_bytes = row
            .get::<_, Vec<u8>>(3)
            .map_err(|error| database_error("read hydrated-pointer cache pointer", error))?;
        entries.insert(
            relative,
            HydratedEntry {
                stat_token,
                size,
                pointer_bytes,
            },
        );
    }
    Ok(entries)
}

fn count_entries(path: &Path) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let connection = open_read_connection(path)?;
    let count = connection
        .query_row("SELECT COUNT(*) FROM hydrated_pointers", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| database_error("count hydrated-pointer cache rows", error))?;
    usize::try_from(count).map_err(|error| {
        CrabError::Internal(format!(
            "hydrated-pointer cache row count exceeds memory range: {error}"
        ))
    })
}

fn open_read_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| database_error("open hydrated-pointer cache for reading", error))?;
    verify_schema(&connection)?;
    Ok(connection)
}

fn open_connection(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    let mut connection = Connection::open(path)
        .map_err(|error| database_error("open hydrated-pointer cache", error))?;
    connection
        .busy_timeout(DATABASE_BUSY_TIMEOUT)
        .map_err(|error| database_error("configure hydrated-pointer cache timeout", error))?;
    ensure_wal_mode(&connection)?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| database_error("configure hydrated-pointer cache", error))?;
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|error| database_error("read hydrated-pointer cache version", error))?;
    if version == 0 {
        initialize_schema(&mut connection)?;
    } else {
        verify_schema(&connection)?;
    }
    Ok(connection)
}

fn ensure_wal_mode(connection: &Connection) -> Result<()> {
    let deadline = Instant::now() + DATABASE_BUSY_TIMEOUT;
    let mut last_mode = None;
    loop {
        let result = connection
            .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
            .and_then(|mode| {
                if mode.eq_ignore_ascii_case("wal") {
                    return Ok(mode);
                }
                connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| {
                    row.get::<_, String>(0)
                })
            });
        match result {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(mode) => last_mode = Some(mode),
            Err(error) if is_lock_contention(&error) => {}
            Err(error) => {
                return Err(database_error(
                    "configure hydrated-pointer cache journal mode",
                    error,
                ));
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let detail = last_mode
                .map(|mode| format!("SQLite kept journal mode {mode}"))
                .unwrap_or_else(|| "database remained locked".to_owned());
            return Err(CrabError::Internal(format!(
                "configure hydrated-pointer cache journal mode: {detail}"
            )));
        }

        // SQLite may bypass the busy handler to break a lock-promotion deadlock.
        // Retry after the competing initializer has had a chance to release its lock.
        std::thread::sleep(JOURNAL_MODE_RETRY_DELAY.min(remaining));
    }
}

fn initialize_schema(connection: &mut Connection) -> Result<()> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| database_error("begin hydrated-pointer cache initialization", error))?;
    let version = transaction
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|error| database_error("read hydrated-pointer cache version", error))?;
    if version == 0 {
        transaction
            .execute_batch(
                "CREATE TABLE hydrated_pointers (
                     path TEXT PRIMARY KEY NOT NULL,
                     stat_token BLOB NOT NULL CHECK(length(stat_token) = 32),
                     size INTEGER NOT NULL CHECK(size >= 0),
                     pointer BLOB NOT NULL
                 ) WITHOUT ROWID;
                 PRAGMA user_version = 1;",
            )
            .map_err(|error| database_error("initialize hydrated-pointer cache", error))?;
    } else {
        verify_schema(&transaction)?;
    }
    transaction
        .commit()
        .map_err(|error| database_error("commit hydrated-pointer cache initialization", error))
}

fn is_lock_contention(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ffi::ErrorCode::DatabaseBusy | rusqlite::ffi::ErrorCode::DatabaseLocked)
    )
}

fn verify_schema(connection: &Connection) -> Result<()> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|error| database_error("read hydrated-pointer cache version", error))?;
    if version != SCHEMA_VERSION {
        return Err(CrabError::Internal(format!(
            "unsupported hydrated-pointer cache schema {version}; expected v{SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

fn database_error(action: &str, error: rusqlite::Error) -> CrabError {
    CrabError::Internal(format!("{action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_file(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::File::create(path).expect("create");
        file.write_all(bytes).expect("write");
        file.sync_all().expect("sync");
    }

    fn pointer_for(content: &[u8]) -> Vec<u8> {
        Pointer {
            file_hash: *blake3::hash(content).as_bytes(),
            size: content.len() as u64,
            shard_hint: None,
        }
        .serialize()
    }

    fn entry(path: &Path, content: &[u8]) -> Option<HydratedEntry> {
        entry_for_path(path, &pointer_for(content)).ok()
    }

    #[test]
    fn load_missing_returns_empty() {
        let dir = tempdir().unwrap();
        let cache = HydratedPointerCache::load_sync(&dir.path().join("missing.sqlite"));
        assert!(cache.is_empty());
    }

    #[test]
    fn load_corrupt_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt.sqlite");
        write_file(&path, b"not sqlite");
        assert!(HydratedPointerCache::load_sync(&path).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn update_load_roundtrip() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("content.bin");
        let path = dir.path().join("cache.sqlite");
        write_file(&file, b"content");
        let expected = entry(&file, b"content").unwrap();
        HydratedPointerCache::update_on_disk(&path, [("content.bin".to_owned(), expected.clone())])
            .unwrap();
        assert_eq!(HydratedPointerCache::count_on_disk(&path), 1);
        let loaded = HydratedPointerCache::load_sync(&path);
        let actual = loaded.get("content.bin").unwrap();
        assert_eq!(actual.stat_token, expected.stat_token);
        assert_eq!(decode_pointer(actual), decode_pointer(&expected));
    }

    #[cfg(unix)]
    #[test]
    fn update_on_disk_preserves_other_entries() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("content.bin");
        let path = dir.path().join("cache.sqlite");
        write_file(&file, b"content");
        let entry = entry(&file, b"content").unwrap();
        HydratedPointerCache::update_on_disk(&path, [("keep.bin".to_owned(), entry.clone())])
            .unwrap();
        HydratedPointerCache::update_on_disk(&path, [("new.bin".to_owned(), entry)]).unwrap();
        let loaded = HydratedPointerCache::load_sync(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.get("keep.bin").is_some());
        assert!(loaded.get("new.bin").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn entry_for_path_reads_exact_metadata() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("content.bin");
        write_file(&path, b"hello world");
        let entry = entry(&path, b"hello world").unwrap();
        assert_eq!(entry.size, 11);
        assert!(matches_stat(&path, &entry));
    }

    #[cfg(unix)]
    #[test]
    fn matches_stat_detects_size_change() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("content.bin");
        write_file(&path, b"hello");
        let entry = entry(&path, b"hello").unwrap();
        write_file(&path, b"hello world");
        assert!(!matches_stat(&path, &entry));
    }

    #[cfg(unix)]
    #[test]
    fn matches_stat_detects_same_size_rewrite_with_restored_mtime() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("content.bin");
        write_file(&path, b"original");
        let original_mtime =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&path).unwrap());
        let entry = entry(&path, b"original").unwrap();
        write_file(&path, b"modified");
        filetime::set_file_mtime(&path, original_mtime).unwrap();
        assert!(!matches_stat(&path, &entry));
    }

    #[cfg(unix)]
    #[test]
    fn matches_stat_returns_false_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("content.bin");
        write_file(&path, b"content");
        let entry = entry(&path, b"content").unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!matches_stat(&path, &entry));
    }

    #[cfg(unix)]
    #[test]
    fn decode_pointer_rejects_invalid_blob() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("content.bin");
        write_file(&path, b"content");
        let mut entry = entry(&path, b"content").unwrap();
        entry.pointer_bytes = b"not a pointer".to_vec();
        assert!(decode_pointer(&entry).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn invalidate_on_disk_removes_only_selected_entries() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("content.bin");
        let path = dir.path().join("cache.sqlite");
        write_file(&file, b"content");
        let entry = entry(&file, b"content").unwrap();
        HydratedPointerCache::update_on_disk(
            &path,
            [("a".to_owned(), entry.clone()), ("b".to_owned(), entry)],
        )
        .unwrap();
        HydratedPointerCache::invalidate_on_disk(&path, ["a".to_owned()]).unwrap();
        let loaded = HydratedPointerCache::load_sync(&path);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("a").is_none());
        assert!(loaded.get("b").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_updates_preserve_every_entry() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("content.bin");
        write_file(&file, b"content");
        let entry = entry(&file, b"content").unwrap();
        for iteration in 0..16 {
            let path = std::sync::Arc::new(dir.path().join(format!("cache-{iteration}.sqlite")));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
            let mut threads = Vec::new();
            for index in 0..16 {
                let path = std::sync::Arc::clone(&path);
                let barrier = std::sync::Arc::clone(&barrier);
                let entry = entry.clone();
                threads.push(std::thread::spawn(move || {
                    barrier.wait();
                    HydratedPointerCache::update_on_disk(
                        &path,
                        [(format!("file-{index}.bin"), entry)],
                    )
                }));
            }
            for thread in threads {
                thread.join().unwrap().unwrap();
            }
            assert_eq!(HydratedPointerCache::load_sync(&path).len(), 16);
        }
    }

    #[test]
    fn cache_rejects_non_v1_schema() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        drop(connection);
        assert!(HydratedPointerCache::load_sync(&path).is_empty());
    }
}
