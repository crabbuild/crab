//! Transactional storage-scoped `file_hash → shard_hash` hints.
//!
//! Push records hints after building shards. Clean and direct-index writers use
//! them to attach `shard-hint` fields to pointer blobs. Hints are advisory:
//! an unavailable, corrupt, missing, or stale entry must fall back to the
//! authoritative file-index path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crab_types::storage::{BucketIdentity, StorageProviderKind};
use rusqlite::{OptionalExtension as _, TransactionBehavior, params};
use tracing::debug;

use crate::private_fs::{Database, DatabaseMode, PinnedRoot, open_database};
use crate::{CacheError, Result};
use crab_types::pointer::Pointer;
use crab_xet::xorb::format::MerkleHash;

/// Relative path of the shard-hint database inside the Crab cache root.
pub const SHARD_HINTS_DATABASE: &str = "hints/shard-hints.sqlite";
const MAX_SHARD_HINTS_ENTRIES: usize = 1_000_000;
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const DATABASE_INSPECTION_BUSY_TIMEOUT: Duration = Duration::from_millis(250);
const DATABASE_INSPECTION_TIMEOUT: Duration = Duration::from_secs(5);
const DATABASE_INSPECTION_PROGRESS_OPS: i32 = 1_000;

/// Stable namespace for hints that address one physical global-content view.
///
/// The global prefix is part of the identity because managed repository views
/// can share a physical bucket while exposing distinct shard namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShardHintScope([u8; 32]);

impl ShardHintScope {
    /// Build a scope from a resolved store identity and global content prefix.
    #[must_use]
    pub fn new(identity: &BucketIdentity, global_prefix: &str) -> Self {
        let provider = match identity.cloud {
            StorageProviderKind::S3 => b"s3".as_slice(),
            StorageProviderKind::Gcs => b"gcs".as_slice(),
            StorageProviderKind::Azure => b"azure".as_slice(),
            StorageProviderKind::Local => b"local".as_slice(),
        };
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab-shard-hint-scope-v1");
        hash_field(&mut hasher, provider);
        hash_field(&mut hasher, identity.host.as_bytes());
        hash_field(&mut hasher, identity.container.as_bytes());
        hash_field(&mut hasher, global_prefix.as_bytes());
        Self(hasher.finalize().into())
    }
}

/// In-memory hints loaded for exactly one storage scope.
#[derive(Debug, Clone, Default)]
pub struct ShardHintCache {
    hints: HashMap<MerkleHash, MerkleHash>,
}

impl ShardHintCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every hint for `scope` without creating cache state.
    ///
    /// A missing root or database returns an empty cache. Other failures are
    /// returned so product callers can log them and use the advisory miss path.
    pub fn load_sync(root: &Path, scope: &ShardHintScope) -> Result<Self> {
        let path = database_path(root);
        let database =
            match open_database(root, &path, DatabaseMode::ReadOnly, DATABASE_BUSY_TIMEOUT) {
                Ok(database) => database,
                Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    debug!(path = %path.display(), "shard-hint database missing, starting empty");
                    return Ok(Self::new());
                }
                Err(error) => return Err(error),
            };
        load_scope(&database, &path, scope)
    }

    /// Transactionally merge `entries` into one storage scope.
    ///
    /// Concurrent writers serialize through SQLite. Unrelated committed rows
    /// are never replaced by a whole-file rewrite.
    pub async fn update(
        root: &Path,
        scope: &ShardHintScope,
        entries: Vec<(MerkleHash, MerkleHash)>,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        if entries.len() > MAX_SHARD_HINTS_ENTRIES {
            return Err(CacheError::CorruptObject {
                path: database_path(root).display().to_string(),
                reason: format!(
                    "update contains {} entries; limit is {MAX_SHARD_HINTS_ENTRIES}",
                    entries.len()
                ),
            });
        }
        let root = root.to_owned();
        let scope = *scope;
        tokio::task::spawn_blocking(move || update_sync(&root, &scope, &entries))
            .await
            .map_err(|error| {
                CacheError::Internal(format!("shard-hint update task failed: {error}"))
            })?
    }

    /// Look up the shard hash associated with `file_hash`, if any.
    #[must_use]
    pub fn get(&self, file_hash: &MerkleHash) -> Option<MerkleHash> {
        self.hints.get(file_hash).copied()
    }

    /// Insert or replace one in-memory mapping.
    pub fn insert(&mut self, file_hash: MerkleHash, shard_hash: MerkleHash) {
        self.hints.insert(file_hash, shard_hash);
    }

    /// Build a Crab pointer with a hint when this scoped cache has one.
    #[must_use]
    pub fn pointer_for(&self, file_hash: [u8; 32], size: u64) -> Pointer {
        let key = MerkleHash::from(file_hash);
        let shard_hint = self.get(&key).map(<[u8; 32]>::from);
        Pointer {
            file_hash,
            size,
            shard_hint,
        }
    }

    /// Number of entries loaded for the selected scope.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hints.len()
    }

    /// Whether the selected scope has any entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hints.is_empty()
    }
}

/// On-disk path for the transactional shard-hint database.
#[must_use]
pub fn database_path(root: &Path) -> PathBuf {
    root.join(SHARD_HINTS_DATABASE)
}

fn load_scope(database: &Database, path: &Path, scope: &ShardHintScope) -> Result<ShardHintCache> {
    validate_schema(database, path)?;
    let mut statement = database
        .prepare(
            "SELECT file_hash, shard_hash FROM shard_hints
             WHERE scope = ?1 ORDER BY file_hash LIMIT ?2",
        )
        .map_err(|source| index_error(path, source))?;
    let limit = i64::try_from(MAX_SHARD_HINTS_ENTRIES.saturating_add(1)).map_err(|_| {
        CacheError::Internal("shard-hint entry limit cannot fit SQLite integer".into())
    })?;
    let rows = statement
        .query_map(params![scope.0.as_slice(), limit], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|source| index_error(path, source))?;
    let mut hints = HashMap::new();
    for row in rows {
        let (file_hash, shard_hash) = row.map_err(|source| index_error(path, source))?;
        let file_hash: [u8; 32] = file_hash
            .try_into()
            .map_err(|_| CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: "shard-hint file hash has an invalid length".into(),
            })?;
        let shard_hash: [u8; 32] =
            shard_hash
                .try_into()
                .map_err(|_| CacheError::CorruptObject {
                    path: path.display().to_string(),
                    reason: "shard-hint shard hash has an invalid length".into(),
                })?;
        hints.insert(MerkleHash::from(file_hash), MerkleHash::from(shard_hash));
        if hints.len() > MAX_SHARD_HINTS_ENTRIES {
            return Err(CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: format!(
                    "storage scope exceeds the {MAX_SHARD_HINTS_ENTRIES} shard-hint entry limit"
                ),
            });
        }
    }
    debug!(path = %path.display(), entries = hints.len(), "loaded scoped shard hints");
    Ok(ShardHintCache { hints })
}

pub(crate) fn inspect_database_at(
    root: &PinnedRoot,
    display_root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    inspect_database_at_with_limits(
        root,
        display_root,
        cancel,
        DATABASE_INSPECTION_TIMEOUT,
        DATABASE_INSPECTION_PROGRESS_OPS,
    )
}

fn inspect_database_at_with_limits(
    root: &PinnedRoot,
    display_root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
    timeout: Duration,
    progress_ops: i32,
) -> Result<()> {
    let path = database_path(display_root);
    let mut database = match root.open_database(
        Path::new(SHARD_HINTS_DATABASE),
        DatabaseMode::ReadOnly,
        DATABASE_INSPECTION_BUSY_TIMEOUT,
    ) {
        Ok(database) => database,
        Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let deadline = Instant::now() + timeout;
    let interrupt_cancel = cancel.clone();
    // SQLite calls this between VM operations, not inside blocking VFS I/O.
    // Keep lock admission bounded separately; this is not a hard syscall deadline.
    database.progress_handler(
        progress_ops,
        Some(move || interrupt_cancel.is_cancelled() || Instant::now() >= deadline),
    );
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|source| inspection_error(&path, source, cancel, deadline, timeout))?;
    validate_schema(&transaction, &path)
        .map_err(|error| inspection_cache_error(&path, error, cancel, deadline, timeout))?;
    let quick_check = transaction
        .query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))
        .map_err(|source| inspection_error(&path, source, cancel, deadline, timeout))?;
    if quick_check != "ok" {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: "shard-hint database failed SQLite quick_check".into(),
        });
    }
    let (count, malformed): (i64, bool) = transaction
        .query_row(
            "SELECT COUNT(*), EXISTS(
               SELECT 1 FROM shard_hints
               WHERE typeof(scope) != 'blob' OR length(scope) != 32
                  OR typeof(file_hash) != 'blob' OR length(file_hash) != 32
                  OR typeof(shard_hash) != 'blob' OR length(shard_hash) != 32
               LIMIT 1
             )
             FROM shard_hints",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|source| inspection_error(&path, source, cancel, deadline, timeout))?;
    if count > MAX_SHARD_HINTS_ENTRIES as i64 {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!(
                "shard-hint database contains {count} entries; limit is {MAX_SHARD_HINTS_ENTRIES}"
            ),
        });
    }
    if malformed {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: "shard-hint database contains a malformed hash row".into(),
        });
    }
    Ok(())
}

fn inspection_error(
    path: &Path,
    source: rusqlite::Error,
    cancel: &tokio_util::sync::CancellationToken,
    deadline: Instant,
    timeout: Duration,
) -> CacheError {
    if source.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) {
        if cancel.is_cancelled() {
            return CacheError::Cancelled;
        }
        if Instant::now() >= deadline {
            return CacheError::InspectionTimeout {
                path: path.display().to_string(),
                timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
                source,
            };
        }
    }
    index_error(path, source)
}

fn inspection_cache_error(
    path: &Path,
    error: CacheError,
    cancel: &tokio_util::sync::CancellationToken,
    deadline: Instant,
    timeout: Duration,
) -> CacheError {
    match error {
        CacheError::Index { source, .. } => {
            inspection_error(path, source, cancel, deadline, timeout)
        }
        error => error,
    }
}

fn update_sync(
    root_path: &Path,
    scope: &ShardHintScope,
    entries: &[(MerkleHash, MerkleHash)],
) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        match update_once(root_path, scope, entries) {
            Err(error)
                if retryable_index_error(&error) && started.elapsed() < DATABASE_BUSY_TIMEOUT =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

fn update_once(
    root_path: &Path,
    scope: &ShardHintScope,
    entries: &[(MerkleHash, MerkleHash)],
) -> Result<()> {
    let root = PinnedRoot::create(root_path)?;
    let path = database_path(root_path);
    let mut database = root.open_database(
        Path::new(SHARD_HINTS_DATABASE),
        DatabaseMode::Create,
        DATABASE_BUSY_TIMEOUT,
    )?;
    configure_writer(&database, &path)?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| index_error(&path, source))?;
    {
        let mut statement = transaction
            .prepare_cached(
                "INSERT INTO shard_hints(scope, file_hash, shard_hash)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(scope, file_hash) DO UPDATE SET shard_hash = excluded.shard_hash",
            )
            .map_err(|source| index_error(&path, source))?;
        for (file_hash, shard_hash) in entries {
            let file_hash: [u8; 32] = (*file_hash).into();
            let shard_hash: [u8; 32] = (*shard_hash).into();
            statement
                .execute(params![
                    scope.0.as_slice(),
                    file_hash.as_slice(),
                    shard_hash.as_slice()
                ])
                .map_err(|source| index_error(&path, source))?;
        }
    }
    let count: i64 = transaction
        .query_row("SELECT COUNT(*) FROM shard_hints", [], |row| row.get(0))
        .map_err(|source| index_error(&path, source))?;
    if count > MAX_SHARD_HINTS_ENTRIES as i64 {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!(
                "shard-hint database contains {count} entries; limit is {MAX_SHARD_HINTS_ENTRIES}"
            ),
        });
    }
    transaction
        .commit()
        .map_err(|source| index_error(&path, source))?;
    debug!(path = %path.display(), entries = entries.len(), "updated scoped shard hints");
    Ok(())
}

fn configure_writer(database: &Database, path: &Path) -> Result<()> {
    let version = schema_version(database, path)?;
    let table_exists = table_sql(database, path)?.is_some();
    match (version, table_exists) {
        (0, false) => database
            .execute_batch(
                "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS shard_hints (
               scope BLOB NOT NULL CHECK(length(scope) = 32),
               file_hash BLOB NOT NULL CHECK(length(file_hash) = 32),
               shard_hash BLOB NOT NULL CHECK(length(shard_hash) = 32),
               PRIMARY KEY(scope, file_hash)
             ) WITHOUT ROWID;
             PRAGMA user_version = 1;
             COMMIT;",
            )
            .map_err(|source| index_error(path, source))?,
        (1, true) => database
            .execute_batch("PRAGMA synchronous = NORMAL;")
            .map_err(|source| index_error(path, source))?,
        _ => return unsupported_schema(path, version),
    }
    validate_schema(database, path)
}

fn validate_schema(database: &rusqlite::Connection, path: &Path) -> Result<()> {
    let version = schema_version(database, path)?;
    if version != 1 {
        return unsupported_schema(path, version);
    }
    let sql = table_sql(database, path)?;
    let expected = "CREATE TABLE shard_hints ( scope BLOB NOT NULL CHECK(length(scope) = 32), file_hash BLOB NOT NULL CHECK(length(file_hash) = 32), shard_hash BLOB NOT NULL CHECK(length(shard_hash) = 32), PRIMARY KEY(scope, file_hash) ) WITHOUT ROWID";
    let Some(sql) = sql else {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: "shard-hint table is missing".into(),
        });
    };
    if normalize_sql(&sql) != normalize_sql(expected) {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: "shard-hint table has an unsupported schema".into(),
        });
    }
    Ok(())
}

fn schema_version(database: &rusqlite::Connection, path: &Path) -> Result<i64> {
    database
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| index_error(path, source))
}

fn table_sql(database: &rusqlite::Connection, path: &Path) -> Result<Option<String>> {
    database
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'shard_hints'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|source| index_error(path, source))
}

fn unsupported_schema(path: &Path, version: i64) -> Result<()> {
    Err(CacheError::CorruptObject {
        path: path.display().to_string(),
        reason: format!("shard-hint database has unsupported schema version {version}"),
    })
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn hash_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn index_error(path: &Path, source: rusqlite::Error) -> CacheError {
    CacheError::Index {
        path: path.display().to_string(),
        source,
    }
}

fn retryable_index_error(error: &CacheError) -> bool {
    matches!(
        error,
        CacheError::Index { source, .. }
            if matches!(
                source.sqlite_error_code(),
                Some(
                    rusqlite::ffi::ErrorCode::DatabaseBusy
                        | rusqlite::ffi::ErrorCode::DatabaseLocked
                )
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use tempfile::TempDir;

    fn make_hash(seed: u64) -> MerkleHash {
        MerkleHash::from([
            seed,
            seed.wrapping_add(1),
            seed.wrapping_add(2),
            seed.wrapping_add(3),
        ])
    }

    fn scope(bucket: &str, global_prefix: &str) -> ShardHintScope {
        ShardHintScope::new(
            &BucketIdentity::new(StorageProviderKind::S3, bucket, bucket),
            global_prefix,
        )
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn cache_root(dir: &TempDir) -> PathBuf {
        dir.path().join("private-cache")
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn load_missing_root_returns_empty_without_creating_it() {
        let dir = TempDir::new().unwrap();
        let root = cache_root(&dir);

        let cache = ShardHintCache::load_sync(&root, &scope("bucket", ".crab")).unwrap();

        assert!(cache.is_empty());
        assert!(!root.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn update_then_load_roundtrips_entries() {
        let dir = TempDir::new().unwrap();
        let root = cache_root(&dir);
        let scope = scope("bucket", ".crab");
        let file_hash = make_hash(1);
        let shard_hash = make_hash(42);

        ShardHintCache::update(&root, &scope, vec![(file_hash, shard_hash)])
            .await
            .unwrap();

        let loaded = ShardHintCache::load_sync(&root, &scope).unwrap();
        assert_eq!(loaded.get(&file_hash), Some(shard_hash));
        assert_eq!(loaded.len(), 1);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn inspection_deadline_interrupts_sqlite_work() {
        let dir = TempDir::new().unwrap();
        let root_path = cache_root(&dir);
        ShardHintCache::update(
            &root_path,
            &scope("bucket", ".crab"),
            (0..2_048)
                .map(|seed| (make_hash(seed), make_hash(42)))
                .collect(),
        )
        .await
        .unwrap();
        let root = PinnedRoot::open(&root_path).unwrap();

        let error = inspect_database_at_with_limits(
            &root,
            &root_path,
            &tokio_util::sync::CancellationToken::new(),
            Duration::ZERO,
            DATABASE_INSPECTION_PROGRESS_OPS,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CacheError::InspectionTimeout { timeout_ms: 0, source, .. }
                if source.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn inspection_interrupt_preserves_cancellation_attribution() {
        let dir = TempDir::new().unwrap();
        let root_path = cache_root(&dir);
        ShardHintCache::update(
            &root_path,
            &scope("bucket", ".crab"),
            (0..2_048)
                .map(|seed| (make_hash(seed), make_hash(42)))
                .collect(),
        )
        .await
        .unwrap();
        let root = PinnedRoot::open(&root_path).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();

        let error = inspect_database_at_with_limits(
            &root,
            &root_path,
            &cancel,
            DATABASE_INSPECTION_TIMEOUT,
            DATABASE_INSPECTION_PROGRESS_OPS,
        )
        .unwrap_err();

        assert!(matches!(error, CacheError::Cancelled));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn interrupted_inspection_preserves_files_and_releases_reader_locks() {
        let dir = TempDir::new().unwrap();
        let root_path = cache_root(&dir);
        ShardHintCache::update(
            &root_path,
            &scope("bucket", ".crab"),
            (0..2_048)
                .map(|seed| (make_hash(seed), make_hash(42)))
                .collect(),
        )
        .await
        .unwrap();
        let root = PinnedRoot::open(&root_path).unwrap();
        let database = root
            .open_database(
                Path::new(SHARD_HINTS_DATABASE),
                DatabaseMode::Create,
                DATABASE_BUSY_TIMEOUT,
            )
            .unwrap();
        // In rollback mode a leaked read transaction prevents a native exclusive
        // writer. WAL writers alone would not prove that the reader was released.
        database
            .execute_batch("PRAGMA journal_mode = DELETE;")
            .unwrap();
        drop(database);
        let snapshot = || {
            std::fs::read_dir(root_path.join("hints"))
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), std::fs::read(entry.path()).unwrap())
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = snapshot();

        for cancelled in [false, true] {
            let cancel = tokio_util::sync::CancellationToken::new();
            if cancelled {
                cancel.cancel();
            }
            let error = inspect_database_at_with_limits(
                &root,
                &root_path,
                &cancel,
                Duration::ZERO,
                DATABASE_INSPECTION_PROGRESS_OPS,
            )
            .unwrap_err();
            assert!(matches!(error, CacheError::Cancelled) == cancelled);
            assert_eq!(snapshot(), before, "cancelled={cancelled}");

            let native = rusqlite::Connection::open_with_flags(
                database_path(&root_path),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )
            .unwrap();
            native.busy_timeout(Duration::ZERO).unwrap();
            native.execute_batch("BEGIN EXCLUSIVE; ROLLBACK;").unwrap();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_writers_retain_unrelated_updates() {
        let dir = TempDir::new().unwrap();
        let root = cache_root(&dir);
        let scope = scope("bucket", ".crab");
        let first = tokio::spawn({
            let root = root.clone();
            async move {
                ShardHintCache::update(&root, &scope, vec![(make_hash(1), make_hash(11))]).await
            }
        });
        let second = tokio::spawn({
            let root = root.clone();
            async move {
                ShardHintCache::update(&root, &scope, vec![(make_hash(2), make_hash(22))]).await
            }
        });

        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        let loaded = ShardHintCache::load_sync(&root, &scope).unwrap();
        assert_eq!(loaded.get(&make_hash(1)), Some(make_hash(11)));
        assert_eq!(loaded.get(&make_hash(2)), Some(make_hash(22)));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn concurrent_process_writers_retain_unrelated_updates() {
        const ROOT_ENV: &str = "CRAB_TEST_SHARD_HINT_ROOT";
        const SEED_ENV: &str = "CRAB_TEST_SHARD_HINT_SEED";
        if let (Some(root), Some(seed)) = (std::env::var_os(ROOT_ENV), std::env::var_os(SEED_ENV)) {
            let seed = seed.to_string_lossy().parse::<u64>().unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime
                .block_on(ShardHintCache::update(
                    Path::new(&root),
                    &scope("bucket", ".crab"),
                    vec![(make_hash(seed), make_hash(seed + 10))],
                ))
                .unwrap();
            return;
        }

        let dir = TempDir::new().unwrap();
        let root = cache_root(&dir);
        let executable = std::env::current_exe().unwrap();
        let test_name = "shard_hints::tests::concurrent_process_writers_retain_unrelated_updates";
        let mut first = std::process::Command::new(&executable)
            .args(["--exact", test_name])
            .env(ROOT_ENV, &root)
            .env(SEED_ENV, "1")
            .spawn()
            .unwrap();
        let mut second = std::process::Command::new(executable)
            .args(["--exact", test_name])
            .env(ROOT_ENV, &root)
            .env(SEED_ENV, "2")
            .spawn()
            .unwrap();

        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());

        let loaded = ShardHintCache::load_sync(&root, &scope("bucket", ".crab")).unwrap();
        assert_eq!(loaded.get(&make_hash(1)), Some(make_hash(11)));
        assert_eq!(loaded.get(&make_hash(2)), Some(make_hash(12)));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn scopes_never_return_each_others_hints() {
        let dir = TempDir::new().unwrap();
        let root = cache_root(&dir);
        let bucket_a = scope("bucket-a", ".crab");
        let bucket_b = scope("bucket-b", ".crab");
        let managed_view = scope("bucket-a", "views/team-b/.crab");
        let file_hash = make_hash(1);

        ShardHintCache::update(&root, &bucket_a, vec![(file_hash, make_hash(11))])
            .await
            .unwrap();
        ShardHintCache::update(&root, &bucket_b, vec![(file_hash, make_hash(22))])
            .await
            .unwrap();
        ShardHintCache::update(&root, &managed_view, vec![(file_hash, make_hash(33))])
            .await
            .unwrap();

        assert_eq!(
            ShardHintCache::load_sync(&root, &bucket_a)
                .unwrap()
                .get(&file_hash),
            Some(make_hash(11))
        );
        assert_eq!(
            ShardHintCache::load_sync(&root, &bucket_b)
                .unwrap()
                .get(&file_hash),
            Some(make_hash(22))
        );
        assert_eq!(
            ShardHintCache::load_sync(&root, &managed_view)
                .unwrap()
                .get(&file_hash),
            Some(make_hash(33))
        );
    }

    #[test]
    fn scope_normalizes_bucket_identity_and_isolates_global_prefixes() {
        let canonical = scope("bucket-a", ".crab");
        let normalized = ShardHintScope::new(
            &BucketIdentity::new(StorageProviderKind::S3, "BUCKET-A/", "bucket-a/"),
            ".crab",
        );
        let managed = scope("bucket-a", "views/team-b/.crab");

        assert_eq!(canonical, normalized);
        assert_ne!(canonical, managed);
    }

    #[test]
    fn pointer_for_attaches_loaded_hint() {
        let file_hash = make_hash(1);
        let shard_hash = make_hash(2);
        let cache = ShardHintCache {
            hints: HashMap::from([(file_hash, shard_hash)]),
        };

        let pointer = cache.pointer_for(file_hash.into(), 123);

        assert_eq!(pointer.file_hash, <[u8; 32]>::from(file_hash));
        assert_eq!(pointer.size, 123);
        assert_eq!(pointer.shard_hint, Some(shard_hash.into()));
    }
}
