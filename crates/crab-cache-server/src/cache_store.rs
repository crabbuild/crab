//! On-disk cache with LRU eviction metadata backed by SQLite.
//!
//! Objects are stored on disk using a `{type_dir}/{hash[:2]}/{hash}` sharding
//! layout. Per-object access metadata (size, last access, access count) lives
//! in SQLite so eviction decisions survive restarts.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use rusqlite::{Connection, OptionalExtension, params};
use tempfile::{NamedTempFile, TempPath};
use tracing::{debug, warn};

use crate::db::map_sqlite_err;
#[cfg(test)]
use crate::db::{CACHE_DB_FILE, CacheDb};
use crate::error::{CacheServiceError, Result};
use crate::metrics::record_cache_integrity_repairs;
use crab_xet::hash::compute_data_hash;
use crab_xet::xorb::format::{FOOTER_SIZE, MerkleHash};
use crab_xet::xorb::parser::{xorb_hash_from_metadata, xorb_metadata_region};

const META_KEY_LEN: usize = 33;
const META_VAL_LEN: usize = 32;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Identifies an object in the cache-server persistence layer.
///
/// This is intentionally separate from `crab_cache::CacheKey`, which keys
/// local/client cache entries by content identity.
pub struct ServerObjectKey {
    pub bucket: String,
    pub repo_path: String,
    pub object_type: ObjectType,
    pub hash: String,
}

/// Recovery handle for a failed temp-path commit.
pub enum TempPathCommitRecovery {
    TempPath(TempPath),
    CommittedObject,
}

/// Error from a temp-path commit with enough state for callers to serve data.
pub struct TempPathCommitError {
    error: CacheServiceError,
    recovery: TempPathCommitRecovery,
}

impl TempPathCommitError {
    fn with_temp_path(error: CacheServiceError, temp_path: TempPath) -> Self {
        Self {
            error,
            recovery: TempPathCommitRecovery::TempPath(temp_path),
        }
    }

    fn after_persist(error: CacheServiceError) -> Self {
        Self {
            error,
            recovery: TempPathCommitRecovery::CommittedObject,
        }
    }

    pub fn into_parts(self) -> (CacheServiceError, TempPathCommitRecovery) {
        (self.error, self.recovery)
    }

    pub fn into_error(self) -> CacheServiceError {
        self.error
    }
}

/// The kind of immutable object stored in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    Xorb,
    Shard,
    Pack,
    PackIndex,
    Metadata,
}

impl ObjectType {
    /// Subdirectory name under the cache root.
    pub fn dir_name(&self) -> &str {
        match self {
            Self::Xorb => "xorbs",
            Self::Shard => "shards",
            Self::Pack | Self::PackIndex => "packs",
            Self::Metadata => "metadata",
        }
    }

    pub fn metric_label(&self) -> &'static str {
        match self {
            Self::Xorb => "xorb",
            Self::Shard => "shard",
            Self::Pack => "pack",
            Self::PackIndex => "pack_index",
            Self::Metadata => "metadata",
        }
    }

    pub fn as_u8(&self) -> u8 {
        match self {
            Self::Xorb => 0,
            Self::Shard => 1,
            Self::Pack => 3,
            Self::PackIndex => 4,
            Self::Metadata => 5,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Xorb),
            1 => Some(Self::Shard),
            3 => Some(Self::Pack),
            4 => Some(Self::PackIndex),
            5 => Some(Self::Metadata),
            _ => None,
        }
    }

    /// Parse from a human-readable name (case-insensitive).
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "xorb" => Some(Self::Xorb),
            "shard" => Some(Self::Shard),
            "pack" => Some(Self::Pack),
            "pack-index" | "pack_index" | "packindex" => Some(Self::PackIndex),
            "metadata" => Some(Self::Metadata),
            _ => None,
        }
    }
}

/// Aggregate cache statistics returned by the admin stats endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheStats {
    pub total_bytes: u64,
    pub max_bytes: u64,
    pub xorb_count: u64,
    pub shard_count: u64,
    pub pack_count: u64,
    pub metadata_count: u64,
    pub eviction: CacheEvictionStats,
    pub startup_integrity: CacheIntegrityStats,
    pub runtime_integrity: CacheRuntimeIntegrityStats,
}

/// Cache objects removed by LRU, emergency, startup, or admin eviction.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheEvictionStats {
    pub total: u64,
    pub xorb: u64,
    pub shard: u64,
    pub pack: u64,
    pub pack_index: u64,
    pub metadata: u64,
}

#[derive(Default)]
struct CacheEvictionCounters {
    xorb: AtomicU64,
    shard: AtomicU64,
    pack: AtomicU64,
    pack_index: AtomicU64,
    metadata: AtomicU64,
}

impl CacheEvictionCounters {
    fn snapshot(&self) -> CacheEvictionStats {
        let xorb = self.xorb.load(Ordering::Relaxed);
        let shard = self.shard.load(Ordering::Relaxed);
        let pack = self.pack.load(Ordering::Relaxed);
        let pack_index = self.pack_index.load(Ordering::Relaxed);
        let metadata = self.metadata.load(Ordering::Relaxed);

        CacheEvictionStats {
            total: xorb
                .saturating_add(shard)
                .saturating_add(pack)
                .saturating_add(pack_index)
                .saturating_add(metadata),
            xorb,
            shard,
            pack,
            pack_index,
            metadata,
        }
    }

    fn record(&self, object_type: ObjectType) {
        let counter = match object_type {
            ObjectType::Xorb => &self.xorb,
            ObjectType::Shard => &self.shard,
            ObjectType::Pack => &self.pack,
            ObjectType::PackIndex => &self.pack_index,
            ObjectType::Metadata => &self.metadata,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Cache metadata repairs performed while opening the store.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheIntegrityStats {
    pub metadata_entries_removed: u64,
    pub metadata_size_corrections: u64,
    pub unindexed_objects_indexed: u64,
    pub unindexed_paths_removed: u64,
}

/// Cache metadata repairs performed after the store starts serving requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheRuntimeIntegrityStats {
    pub missing_files_repaired: u64,
    pub invalid_objects_evicted: u64,
    pub metadata_entries_recreated: u64,
}

#[derive(Default)]
struct CacheRuntimeIntegrityCounters {
    missing_files_repaired: AtomicU64,
    invalid_objects_evicted: AtomicU64,
    metadata_entries_recreated: AtomicU64,
}

impl CacheRuntimeIntegrityCounters {
    fn snapshot(&self) -> CacheRuntimeIntegrityStats {
        CacheRuntimeIntegrityStats {
            missing_files_repaired: self.missing_files_repaired.load(Ordering::Relaxed),
            invalid_objects_evicted: self.invalid_objects_evicted.load(Ordering::Relaxed),
            metadata_entries_recreated: self.metadata_entries_recreated.load(Ordering::Relaxed),
        }
    }

    fn record_missing_file_repair(&self) {
        self.missing_files_repaired.fetch_add(1, Ordering::Relaxed);
        record_cache_integrity_repairs("runtime", "missing_files_repaired", 1);
    }

    fn record_invalid_object_eviction(&self) {
        self.invalid_objects_evicted.fetch_add(1, Ordering::Relaxed);
        record_cache_integrity_repairs("runtime", "invalid_objects_evicted", 1);
    }

    fn record_metadata_entry_recreated(&self) {
        self.metadata_entries_recreated
            .fetch_add(1, Ordering::Relaxed);
        record_cache_integrity_repairs("runtime", "metadata_entries_recreated", 1);
    }
}

fn record_startup_integrity_repairs(stats: &CacheIntegrityStats) {
    record_cache_integrity_repairs(
        "startup",
        "metadata_entries_removed",
        stats.metadata_entries_removed,
    );
    record_cache_integrity_repairs(
        "startup",
        "metadata_size_corrections",
        stats.metadata_size_corrections,
    );
    record_cache_integrity_repairs(
        "startup",
        "unindexed_objects_indexed",
        stats.unindexed_objects_indexed,
    );
    record_cache_integrity_repairs(
        "startup",
        "unindexed_paths_removed",
        stats.unindexed_paths_removed,
    );
}

/// Per-object access metadata stored in SQLite.
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub object_type: ObjectType,
    pub size: u64,
    pub last_access: u64,
    pub access_count: u64,
    pub cached_at: u64,
}

/// Statistics from an eviction run.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EvictStats {
    pub evicted_count: u64,
    pub evicted_bytes: u64,
}

/// Filter criteria for admin-triggered eviction.
pub struct EvictFilter {
    pub object_type: Option<ObjectType>,
}

/// Byte-range data served from a cached object.
pub struct CachedRange {
    pub data: Bytes,
    pub range: Range<u64>,
    pub total_size: u64,
}

/// An open cached object file and its current byte length.
pub struct CachedFile {
    pub file: std::fs::File,
    pub size: u64,
}

/// Result of reading a range from an object already present on disk.
pub enum CacheRangeRead {
    Hit(CachedRange),
    Unsatisfiable { total_size: u64 },
}

// ---------------------------------------------------------------------------
// CacheStore
// ---------------------------------------------------------------------------

/// On-disk cache with LRU eviction metadata.
pub struct CacheStore {
    root: PathBuf,
    max_bytes: u64,
    conn: Mutex<Connection>,
    mutation_lock: Mutex<()>,
    current_bytes: AtomicU64,
    startup_integrity: CacheIntegrityStats,
    runtime_integrity: CacheRuntimeIntegrityCounters,
    eviction: CacheEvictionCounters,
}

struct PutBudgetPlan {
    meta_key: [u8; META_KEY_LEN],
    existing: Option<ObjectMeta>,
    old_size: u64,
    growth: u64,
}

impl PutBudgetPlan {
    fn exceeds_budget(&self, current_bytes: u64, max_bytes: u64) -> bool {
        current_bytes.saturating_add(self.growth) > max_bytes
    }
}

impl CacheStore {
    /// Open a cache store rooted at `root` with the given SQLite connection.
    ///
    /// Scans the metadata table to compute the initial `current_bytes`.
    pub fn open(root: PathBuf, max_bytes: u64, conn: Connection) -> Result<Self> {
        // Ensure the root directory exists.
        std::fs::create_dir_all(&root).map_err(|e| {
            CacheServiceError::InternalError(
                format!("failed to create cache root {}: {e}", root.display()).into(),
            )
        })?;

        let (initial_bytes, startup_integrity) = reconcile_metadata_with_files(&root, &conn)?;
        record_startup_integrity_repairs(&startup_integrity);

        debug!(
            initial_bytes,
            metadata_entries_removed = startup_integrity.metadata_entries_removed,
            metadata_size_corrections = startup_integrity.metadata_size_corrections,
            unindexed_objects_indexed = startup_integrity.unindexed_objects_indexed,
            unindexed_paths_removed = startup_integrity.unindexed_paths_removed,
            root = %root.display(),
            "cache store opened"
        );

        Ok(Self {
            root,
            max_bytes,
            conn: Mutex::new(conn),
            mutation_lock: Mutex::new(()),
            current_bytes: AtomicU64::new(initial_bytes),
            startup_integrity,
            runtime_integrity: CacheRuntimeIntegrityCounters::default(),
            eviction: CacheEvictionCounters::default(),
        })
    }

    /// Current cache size in bytes.
    pub fn current_bytes(&self) -> u64 {
        self.current_bytes.load(Ordering::Relaxed)
    }

    /// Maximum cache size in bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Cache objects removed by eviction paths since this process started.
    pub fn eviction_stats(&self) -> CacheEvictionStats {
        self.eviction.snapshot()
    }

    /// Compute the on-disk path for a cached object.
    pub fn object_path(&self, key: &ServerObjectKey) -> PathBuf {
        let storage_id = storage_id_hex(key).unwrap_or_else(|| key.hash.clone());
        let hash = &storage_id;
        let prefix = if hash.len() >= 2 {
            &hash[..2]
        } else {
            hash.as_str()
        };
        self.root
            .join(key.object_type.dir_name())
            .join(prefix)
            .join(hash)
    }

    /// Look up a cached object. Returns `None` on miss.
    ///
    /// On hit, updates `last_access` and increments `access_count` in SQLite.
    pub fn get(&self, key: &ServerObjectKey) -> Result<Option<Bytes>> {
        let path = self.object_path(key);

        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.remove_metadata_for_missing_file(key, &path)?;
                return Ok(None);
            }
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to read {}: {e}", path.display()).into(),
                ));
            }
        };

        // Update access metadata.
        self.touch_metadata(key)?;

        Ok(Some(Bytes::from(data)))
    }

    /// Open a cached object for streaming without reading the body into RAM.
    ///
    /// Opening the file before returning keeps an in-flight response valid if
    /// eviction removes the directory entry while the response is draining.
    pub fn get_file(&self, key: &ServerObjectKey) -> Result<Option<CachedFile>> {
        let path = self.object_path(key);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.remove_metadata_for_missing_file(key, &path)?;
                return Ok(None);
            }
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to open {}: {e}", path.display()).into(),
                ));
            }
        };
        let size = file
            .metadata()
            .map_err(|e| {
                CacheServiceError::InternalError(
                    format!("failed to stat cached file {}: {e}", path.display()).into(),
                )
            })?
            .len();
        self.touch_metadata(key)?;
        Ok(Some(CachedFile { file, size }))
    }

    /// Read a byte range from a cached file. Returns `None` on miss.
    pub fn get_range(
        &self,
        key: &ServerObjectKey,
        range: Range<u64>,
    ) -> Result<Option<CacheRangeRead>> {
        use std::io::{Read, Seek, SeekFrom};

        let path = self.object_path(key);

        let mut file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.remove_metadata_for_missing_file(key, &path)?;
                return Ok(None);
            }
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to open {}: {e}", path.display()).into(),
                ));
            }
        };

        let total_size = file.metadata().map_err(|e| {
            CacheServiceError::InternalError(
                format!("failed to stat cached file {}: {e}", path.display()).into(),
            )
        })?;
        let total_size = total_size.len();
        if range.start == range.end {
            self.touch_metadata(key)?;
            return Ok(Some(CacheRangeRead::Hit(CachedRange {
                data: Bytes::new(),
                range,
                total_size,
            })));
        }

        if range.start >= total_size {
            self.touch_metadata(key)?;
            return Ok(Some(CacheRangeRead::Unsatisfiable { total_size }));
        }

        let range = range.start..range.end.min(total_size);

        let len = usize::try_from(range.end.saturating_sub(range.start)).map_err(|e| {
            CacheServiceError::InternalError(
                format!(
                    "range length does not fit in memory for {}: {e}",
                    path.display()
                )
                .into(),
            )
        })?;
        file.seek(SeekFrom::Start(range.start)).map_err(|e| {
            CacheServiceError::InternalError(
                format!("seek failed on {}: {e}", path.display()).into(),
            )
        })?;

        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).map_err(|e| {
            CacheServiceError::InternalError(
                format!("range read failed on {}: {e}", path.display()).into(),
            )
        })?;

        self.touch_metadata(key)?;

        Ok(Some(CacheRangeRead::Hit(CachedRange {
            data: Bytes::from(buf),
            range,
            total_size,
        })))
    }

    /// Verify a cached xorb's aggregate identity before serving it.
    ///
    /// Returns `Ok(false)` after evicting stale/corrupt xorb bytes so the
    /// handler can treat the lookup as a cache miss and refill from origin.
    pub fn verify_cached_xorb_identity(&self, key: &ServerObjectKey) -> Result<bool> {
        if key.object_type != ObjectType::Xorb {
            return Ok(true);
        }

        let path = self.object_path(key);
        let Ok(expected) = MerkleHash::from_hex(&key.hash) else {
            return Ok(false);
        };
        let total_size = match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => meta.len(),
            Ok(_) => {
                self.evict_invalid_cache_file(key, &path)?;
                return Ok(false);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.remove_metadata_for_missing_file(key, &path)?;
                return Ok(false);
            }
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to stat cached xorb {}: {e}", path.display()).into(),
                ));
            }
        };

        match xorb_hash_from_file_metadata(&path, total_size) {
            Ok(actual) if actual == expected => Ok(true),
            Ok(actual) => {
                warn!(
                    path = %path.display(),
                    expected = %expected.hex(),
                    actual = %actual.hex(),
                    "cached xorb identity mismatch, evicting"
                );
                self.evict_invalid_cache_file(key, &path)?;
                Ok(false)
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    expected = %expected.hex(),
                    error = %e,
                    "cached xorb metadata invalid, evicting"
                );
                self.evict_invalid_cache_file(key, &path)?;
                Ok(false)
            }
        }
    }

    /// Verify cached immutable objects whose key is their content identity.
    ///
    /// Xorbs use aggregate xorb identity from metadata, while shards are keyed
    /// by the Blake3/Merkle content hash of their serialized bytes.
    pub fn verify_cached_object_identity(&self, key: &ServerObjectKey) -> Result<bool> {
        match key.object_type {
            ObjectType::Xorb => self.verify_cached_xorb_identity(key),
            ObjectType::Shard => self.verify_cached_shard_identity(key),
            _ => Ok(true),
        }
    }

    fn verify_cached_shard_identity(&self, key: &ServerObjectKey) -> Result<bool> {
        debug_assert_eq!(key.object_type, ObjectType::Shard);

        let path = self.object_path(key);
        let expected = match parse_hash_hex(&key.hash) {
            Some(hash) => hex::encode(&hash),
            None => return Ok(false),
        };
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.remove_metadata_for_missing_file(key, &path)?;
                return Ok(false);
            }
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to read cached shard {}: {e}", path.display()).into(),
                ));
            }
        };

        let actual = compute_data_hash(&data).hex();
        if actual == expected {
            return Ok(true);
        }

        warn!(
            path = %path.display(),
            expected = %expected,
            actual = %actual,
            "cached shard identity mismatch, evicting"
        );
        self.evict_invalid_cache_file(key, &path)?;
        Ok(false)
    }

    fn remove_metadata_for_missing_file(
        &self,
        key: &ServerObjectKey,
        path: &std::path::Path,
    ) -> Result<()> {
        self.remove_metadata_for_absent_file(key, path, true)
    }

    fn remove_metadata_after_invalid_eviction(
        &self,
        key: &ServerObjectKey,
        path: &std::path::Path,
    ) -> Result<()> {
        self.remove_metadata_for_absent_file(key, path, false)
    }

    fn remove_metadata_for_absent_file(
        &self,
        key: &ServerObjectKey,
        path: &std::path::Path,
        count_missing_file_repair: bool,
    ) -> Result<()> {
        let Some(storage_id) = storage_id_bytes(key) else {
            return Ok(());
        };
        let _mutation_guard = self.mutation_guard()?;

        let meta_key = make_meta_key(key.object_type, &storage_id);
        let conn = self.connection()?;
        match std::fs::metadata(path) {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to stat cached file {}: {e}", path.display()).into(),
                ));
            }
        }

        let existing = conn
            .query_row(
                "SELECT meta_value FROM object_meta WHERE meta_key = ?1",
                params![meta_key.as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(map_sqlite_err)?;
        let Some(meta) = existing
            .as_deref()
            .and_then(|val| decode_meta_value(key.object_type, val))
        else {
            return Ok(());
        };

        let removed = conn
            .execute(
                "DELETE FROM object_meta WHERE meta_key = ?1",
                params![meta_key.as_slice()],
            )
            .map_err(map_sqlite_err)?;
        if removed > 0 {
            self.current_bytes.fetch_sub(
                meta.size.min(self.current_bytes.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
            debug!(
                hash = key.hash,
                object_type = ?key.object_type,
                size = meta.size,
                "removed stale cache metadata for missing file"
            );
            if count_missing_file_repair {
                self.runtime_integrity.record_missing_file_repair();
            }
        }

        Ok(())
    }

    fn evict_invalid_cache_file(&self, key: &ServerObjectKey, path: &Path) -> Result<()> {
        let removed_invalid_file = match std::fs::remove_file(path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => Err(CacheServiceError::InternalError(
                format!(
                    "failed to remove invalid cache file {}: {e}",
                    path.display()
                )
                .into(),
            ))?,
        };

        self.remove_metadata_after_invalid_eviction(key, path)?;
        if removed_invalid_file {
            self.runtime_integrity.record_invalid_object_eviction();
        }
        Ok(())
    }

    /// Store an object after unkeyed Blake3 hash verification.
    ///
    /// Uses tempfile + rename for atomic writes. Inserts metadata into SQLite.
    /// Returns `DiskFull` when the net byte growth would exceed `max_bytes`.
    pub fn put(&self, key: &ServerObjectKey, data: Bytes, expected_hash: &[u8; 32]) -> Result<()> {
        // Git pack paths use ordinary body hashes.
        let actual_hash = blake3::hash(&data);
        if actual_hash.as_bytes() != expected_hash {
            return Err(CacheServiceError::HashMismatch {
                expected: hex::encode(expected_hash),
                actual: actual_hash.to_hex().to_string(),
            });
        }

        self.put_unverified(key, data)
    }

    /// Store an object under its canonical cache key without body-hash verification.
    ///
    /// Production xorb path hashes are aggregate content IDs, not
    /// `blake3(body)`, so HTTP warm/read-miss paths use this after parsing
    /// and authorizing the object-store key.
    pub fn put_unverified(&self, key: &ServerObjectKey, data: Bytes) -> Result<()> {
        let size = data.len() as u64;
        {
            let _mutation_guard = self.mutation_guard()?;
            let plan = self.put_budget_plan(key, size)?;
            let current_bytes = self.current_bytes();
            if plan.exceeds_budget(current_bytes, self.max_bytes) {
                return Err(Self::disk_full_error(
                    current_bytes,
                    plan.growth,
                    self.max_bytes,
                ));
            }
        }

        let temp_path = self.create_temp_object_path(key)?;
        std::fs::write(temp_path.as_ref() as &Path, &data).map_err(|e| {
            CacheServiceError::InternalError(format!("tempfile write failed: {e}").into())
        })?;
        self.put_unverified_temp_path(key, temp_path, size)
    }

    /// Create a same-directory temp path for streaming a validated object.
    pub fn create_temp_object_path(&self, key: &ServerObjectKey) -> Result<TempPath> {
        storage_id_bytes(key).ok_or_else(|| CacheServiceError::BadRequest {
            reason: format!("invalid cache key for {:?}: {}", key.object_type, key.hash),
        })?;

        let path = self.object_path(key);
        let Some(parent) = path.parent() else {
            return Err(CacheServiceError::InternalError(
                format!("cache object path {} has no parent", path.display()).into(),
            ));
        };
        std::fs::create_dir_all(parent).map_err(|e| {
            CacheServiceError::InternalError(
                format!("failed to create dir {}: {e}", parent.display()).into(),
            )
        })?;
        let tmp = NamedTempFile::new_in(parent).map_err(|e| {
            CacheServiceError::InternalError(
                format!("failed to create tempfile in {}: {e}", parent.display()).into(),
            )
        })?;
        Ok(tmp.into_temp_path())
    }

    /// Store a previously validated temp file under its canonical cache key.
    pub fn put_unverified_temp_path(
        &self,
        key: &ServerObjectKey,
        temp_path: TempPath,
        size: u64,
    ) -> Result<()> {
        self.put_unverified_temp_path_recoverable(key, temp_path, size)
            .map_err(TempPathCommitError::into_error)
    }

    /// Store a previously validated temp file, returning the temp path when
    /// commit fails before ownership moves to the canonical cache file.
    pub fn put_unverified_temp_path_recoverable(
        &self,
        key: &ServerObjectKey,
        temp_path: TempPath,
        size: u64,
    ) -> std::result::Result<(), TempPathCommitError> {
        let mut temp_path = Some(temp_path);
        let take_temp_path = |temp_path: &mut Option<TempPath>| {
            temp_path.take().ok_or_else(|| {
                TempPathCommitError::after_persist(CacheServiceError::InternalError(
                    "temp path missing before persist".into(),
                ))
            })
        };
        let now = epoch_millis();
        let _mutation_guard = match self.mutation_guard() {
            Ok(guard) => guard,
            Err(e) => {
                return Err(TempPathCommitError::with_temp_path(
                    e,
                    take_temp_path(&mut temp_path)?,
                ));
            }
        };

        let plan = match self.put_budget_plan(key, size) {
            Ok(plan) => plan,
            Err(e) => {
                return Err(TempPathCommitError::with_temp_path(
                    e,
                    take_temp_path(&mut temp_path)?,
                ));
            }
        };
        let current_bytes = self.current_bytes();

        if plan.exceeds_budget(current_bytes, self.max_bytes) {
            return Err(TempPathCommitError::with_temp_path(
                Self::disk_full_error(current_bytes, plan.growth, self.max_bytes),
                take_temp_path(&mut temp_path)?,
            ));
        }

        let path = self.object_path(key);
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Err(TempPathCommitError::with_temp_path(
                CacheServiceError::InternalError(
                    format!("failed to create dir {}: {e}", parent.display()).into(),
                ),
                take_temp_path(&mut temp_path)?,
            ));
        }

        let temp_path = take_temp_path(&mut temp_path)?;
        if let Err(e) = temp_path.persist(&path) {
            return Err(TempPathCommitError::with_temp_path(
                CacheServiceError::InternalError(
                    format!("failed to persist tempfile to {}: {e}", path.display()).into(),
                ),
                e.path,
            ));
        }

        let (access_count, cached_at) = match plan.existing {
            Some(meta) => (meta.access_count.saturating_add(1), meta.cached_at),
            None => (0, now),
        };
        let meta_val = encode_meta_value(size, now, access_count, cached_at);
        let conn = self
            .connection()
            .map_err(TempPathCommitError::after_persist)?;
        conn.execute(
            "INSERT OR REPLACE INTO object_meta (meta_key, meta_value) VALUES (?1, ?2)",
            params![plan.meta_key.as_slice(), meta_val.as_slice()],
        )
        .map_err(map_sqlite_err)
        .map_err(TempPathCommitError::after_persist)?;

        if size >= plan.old_size {
            self.current_bytes
                .fetch_add(size - plan.old_size, Ordering::Relaxed);
        } else {
            self.current_bytes
                .fetch_sub(plan.old_size - size, Ordering::Relaxed);
        }

        debug!(
            hash = %key.hash,
            object_type = ?key.object_type,
            size,
            old_size = plan.old_size,
            "cached object"
        );
        Ok(())
    }

    /// Return whether storing `size` bytes at `key` would exceed the cache budget.
    ///
    /// Existing metadata for the same key is counted as replacement, not
    /// growth. Push warming uses this before consuming a temp file so it
    /// can evict only when the authoritative commit would otherwise fail.
    pub fn would_exceed_budget_after_put(&self, key: &ServerObjectKey, size: u64) -> Result<bool> {
        let _mutation_guard = self.mutation_guard()?;
        let plan = self.put_budget_plan(key, size)?;
        Ok(plan.exceeds_budget(self.current_bytes(), self.max_bytes))
    }

    fn put_budget_plan(&self, key: &ServerObjectKey, size: u64) -> Result<PutBudgetPlan> {
        let storage_id = storage_id_bytes(key).ok_or_else(|| CacheServiceError::BadRequest {
            reason: format!("invalid cache key for {:?}: {}", key.object_type, key.hash),
        })?;
        let meta_key = make_meta_key(key.object_type, &storage_id);
        let existing = self.read_meta(&meta_key)?;
        let old_size = existing.as_ref().map_or(0, |m| m.size);
        let growth = size.saturating_sub(old_size);

        Ok(PutBudgetPlan {
            meta_key,
            existing,
            old_size,
            growth,
        })
    }

    fn disk_full_error(current_bytes: u64, growth: u64, max_bytes: u64) -> CacheServiceError {
        CacheServiceError::DiskFull {
            reason: format!("cache full: {current_bytes} + {growth} net growth > {max_bytes} max"),
        }
    }

    /// Create a stub `CacheStore` for tests using a temp directory and SQLite database.
    #[cfg(test)]
    #[expect(
        clippy::expect_used,
        reason = "test-only convenience constructor fails loudly on fixture setup errors"
    )]
    pub fn stub() -> Self {
        let dir = tempfile::tempdir().expect("failed to create temp dir for stub CacheStore");
        let root = dir.path().to_path_buf();
        let db = CacheDb::open_or_create(&root.join(CACHE_DB_FILE))
            .expect("failed to create stub SQLite");
        let conn = db.connect().expect("failed to open stub SQLite connection");

        // Leak the TempDir so it isn't cleaned up while the stub is alive.
        // For production use, `open()` is the proper constructor.
        std::mem::forget(dir);

        Self {
            root,
            max_bytes: 1_073_741_824, // 1 GiB default for stubs
            conn: Mutex::new(conn),
            mutation_lock: Mutex::new(()),
            current_bytes: AtomicU64::new(0),
            startup_integrity: CacheIntegrityStats::default(),
            runtime_integrity: CacheRuntimeIntegrityCounters::default(),
            eviction: CacheEvictionCounters::default(),
        }
    }

    /// Cache root directory.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Aggregate cache statistics: byte totals and per-type object counts.
    pub fn stats(&self) -> Result<CacheStats> {
        let mut xorb_count: u64 = 0;
        let mut shard_count: u64 = 0;
        let mut pack_count: u64 = 0;
        let mut metadata_count: u64 = 0;

        let conn = self.connection()?;
        let mut stmt = conn
            .prepare("SELECT meta_key FROM object_meta")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(map_sqlite_err)?;
        for row in rows {
            let key = row.map_err(map_sqlite_err)?;
            if key.is_empty() {
                continue;
            }

            match ObjectType::from_u8(key[0]) {
                Some(ObjectType::Xorb) => xorb_count += 1,
                Some(ObjectType::Shard) => shard_count += 1,
                Some(ObjectType::Pack | ObjectType::PackIndex) => pack_count += 1,
                Some(ObjectType::Metadata) => metadata_count += 1,
                None => {}
            }
        }

        Ok(CacheStats {
            total_bytes: self.current_bytes.load(Ordering::Relaxed),
            max_bytes: self.max_bytes,
            xorb_count,
            shard_count,
            pack_count,
            metadata_count,
            eviction: self.eviction.snapshot(),
            startup_integrity: self.startup_integrity.clone(),
            runtime_integrity: self.runtime_integrity.snapshot(),
        })
    }

    // -----------------------------------------------------------------------
    // Eviction
    // -----------------------------------------------------------------------

    /// Remove a single object from disk and metadata, returning freed bytes.
    ///
    /// The `meta_key` is the 33-byte metadata key (object_type + hash).
    /// Silently returns 0 if the object is already gone.
    pub fn remove_object(&self, meta_key: &[u8; META_KEY_LEN]) -> Result<u64> {
        let _mutation_guard = self.mutation_guard()?;

        // Read the metadata to get size and build the disk path.
        let Some(meta) = self.read_meta(meta_key)? else {
            return Ok(0);
        };
        let size = meta.size;
        let object_type = meta.object_type;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&meta_key[1..]);

        // Build the disk path and delete the file.
        let hash_hex = hex::encode(&hash);
        let prefix = &hash_hex[..2];
        let path = self
            .root
            .join(object_type.dir_name())
            .join(prefix)
            .join(&hash_hex);

        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to remove cached file");
            }
        }

        // Remove the metadata entry.
        let conn = self.connection()?;
        conn.execute(
            "DELETE FROM object_meta WHERE meta_key = ?1",
            params![meta_key.as_slice()],
        )
        .map_err(map_sqlite_err)?;

        self.current_bytes.fetch_sub(
            size.min(self.current_bytes.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
        self.eviction.record(object_type);

        debug!(
            object_type = ?object_type,
            size,
            "evicted object"
        );

        Ok(size)
    }

    /// Evict exactly one object by canonical cache key.
    ///
    /// Returns zero counts when the object is already absent.
    pub fn evict_key(&self, key: &ServerObjectKey) -> Result<EvictStats> {
        let Some(storage_id) = storage_id_bytes(key) else {
            return Ok(EvictStats::default());
        };
        let meta_key = make_meta_key(key.object_type, &storage_id);
        let freed = self.remove_object(&meta_key)?;
        if freed == 0 {
            return Ok(EvictStats::default());
        }

        Ok(EvictStats {
            evicted_count: 1,
            evicted_bytes: freed,
        })
    }

    /// Evict objects until `current_bytes <= max_bytes * low_water_ratio`.
    ///
    /// Sorts by `last_access` ascending (oldest first), then by type weight
    /// (xorbs evicted first at same recency). Only runs if current usage
    /// exceeds `max_bytes * high_water_ratio`.
    pub fn evict_to_budget(
        &self,
        high_water_ratio: f64,
        low_water_ratio: f64,
    ) -> Result<EvictStats> {
        let high_water = (self.max_bytes as f64 * high_water_ratio) as u64;
        let low_water = (self.max_bytes as f64 * low_water_ratio) as u64;

        if self.current_bytes() <= high_water {
            return Ok(EvictStats::default());
        }

        // Collect all entries with their sort keys.
        let mut candidates = self.collect_eviction_candidates()?;

        // Sort: oldest first, then large payloads before shard metadata.
        candidates.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)));

        let mut stats = EvictStats::default();
        for (mk, _, _) in &candidates {
            if self.current_bytes() <= low_water {
                break;
            }
            let freed = self.remove_object(mk)?;
            stats.evicted_count += 1;
            stats.evicted_bytes += freed;
        }

        debug!(
            evicted_count = stats.evicted_count,
            evicted_bytes = stats.evicted_bytes,
            current_bytes = self.current_bytes(),
            "eviction complete"
        );

        Ok(stats)
    }

    /// Emergency eviction: evict the oldest 10% of objects by count.
    ///
    /// Called when a `put` fails with `DiskFull`. Sorts all entries by
    /// `last_access` ascending and removes the first 10%.
    pub fn emergency_evict(&self) -> Result<EvictStats> {
        let mut candidates = self.collect_eviction_candidates()?;

        if candidates.is_empty() {
            return Ok(EvictStats::default());
        }

        // Sort oldest first, then by type weight.
        candidates.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)));

        // Evict the oldest 10% by count (at least 1).
        let evict_count = (candidates.len() / 10).max(1);

        let mut stats = EvictStats::default();
        for (mk, _, _) in candidates.iter().take(evict_count) {
            let freed = self.remove_object(mk)?;
            stats.evicted_count += 1;
            stats.evicted_bytes += freed;
        }

        debug!(
            evicted_count = stats.evicted_count,
            evicted_bytes = stats.evicted_bytes,
            current_bytes = self.current_bytes(),
            "emergency eviction complete"
        );

        Ok(stats)
    }

    /// Evict objects matching the given filter (object type).
    ///
    /// Scans all metadata entries and removes those whose type byte matches
    /// the filter. Returns aggregate stats for the evicted objects.
    pub fn evict_by_filter(&self, filter: &EvictFilter) -> Result<EvictStats> {
        let mut candidates: Vec<[u8; META_KEY_LEN]> = Vec::new();
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare("SELECT meta_key FROM object_meta")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(map_sqlite_err)?;
        for row in rows {
            let key = row.map_err(map_sqlite_err)?;
            if key.len() != META_KEY_LEN {
                continue;
            }

            let matches = match filter.object_type {
                Some(ot) => key[0] == ot.as_u8(),
                None => true,
            };

            if matches {
                let mut mk = [0u8; META_KEY_LEN];
                mk.copy_from_slice(&key);
                candidates.push(mk);
            }
        }
        drop(stmt);
        drop(conn);

        let mut stats = EvictStats::default();
        for mk in &candidates {
            let freed = self.remove_object(mk)?;
            stats.evicted_count += 1;
            stats.evicted_bytes += freed;
        }

        debug!(
            evicted_count = stats.evicted_count,
            evicted_bytes = stats.evicted_bytes,
            filter_type = ?filter.object_type,
            "admin eviction complete"
        );

        Ok(stats)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| {
            CacheServiceError::InternalError("cache service SQLite connection poisoned".into())
        })
    }

    fn mutation_guard(&self) -> Result<MutexGuard<'_, ()>> {
        self.mutation_lock.lock().map_err(|_| {
            CacheServiceError::InternalError("cache service mutation lock poisoned".into())
        })
    }

    fn collect_eviction_candidates(&self) -> Result<Vec<([u8; META_KEY_LEN], u64, u8)>> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare("SELECT meta_key, meta_value FROM object_meta")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(map_sqlite_err)?;

        let mut candidates = Vec::new();
        for row in rows {
            let (key, val) = row.map_err(map_sqlite_err)?;
            if key.len() != META_KEY_LEN || val.len() != META_VAL_LEN {
                continue;
            }
            let mut mk = [0u8; META_KEY_LEN];
            mk.copy_from_slice(&key);
            let Some(last_access) = read_u64_field(&val, 8..16) else {
                continue;
            };
            let type_weight = eviction_type_weight(mk[0]);
            candidates.push((mk, last_access, type_weight));
        }

        Ok(candidates)
    }

    /// Update `last_access` and increment `access_count` for an object.
    fn touch_metadata(&self, key: &ServerObjectKey) -> Result<()> {
        let Some(storage_id) = storage_id_bytes(key) else {
            warn!(
                hash = key.hash,
                object_type = ?key.object_type,
                "invalid cache key, skipping metadata update"
            );
            return Ok(());
        };

        let meta_key = make_meta_key(key.object_type, &storage_id);
        let now = epoch_millis();
        let _mutation_guard = self.mutation_guard()?;

        let conn = self.connection()?;
        let existing = conn
            .query_row(
                "SELECT meta_value FROM object_meta WHERE meta_key = ?1",
                params![meta_key.as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        if let Some(meta) = existing
            .as_deref()
            .and_then(|val| decode_meta_value(key.object_type, val))
        {
            let new_val = encode_meta_value(
                meta.size,
                now,
                meta.access_count.saturating_add(1),
                meta.cached_at,
            );
            conn.execute(
                "UPDATE object_meta SET meta_value = ?2 WHERE meta_key = ?1",
                params![meta_key.as_slice(), new_val.as_slice()],
            )
            .map_err(map_sqlite_err)?;
            return Ok(());
        }

        let path = self.object_path(key);
        let size = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to stat cached file {}: {e}", path.display()).into(),
                ));
            }
        };
        let new_val = encode_meta_value(size, now, 1, now);
        let changed = conn
            .execute(
                "INSERT OR REPLACE INTO object_meta (meta_key, meta_value) VALUES (?1, ?2)",
                params![meta_key.as_slice(), new_val.as_slice()],
            )
            .map_err(map_sqlite_err)?;
        if changed > 0 {
            let total_bytes = metadata_total_bytes(&conn)?;
            self.current_bytes.store(total_bytes, Ordering::Relaxed);
            self.runtime_integrity.record_metadata_entry_recreated();
        }
        Ok(())
    }

    fn read_meta(&self, meta_key: &[u8; META_KEY_LEN]) -> Result<Option<ObjectMeta>> {
        let conn = self.connection()?;
        let Some(val) = conn
            .query_row(
                "SELECT meta_value FROM object_meta WHERE meta_key = ?1",
                params![meta_key.as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(map_sqlite_err)?
        else {
            return Ok(None);
        };

        if val.len() != META_VAL_LEN {
            return Ok(None);
        }

        let object_type = ObjectType::from_u8(meta_key[0]).unwrap_or(ObjectType::Xorb);
        Ok(decode_meta_value(object_type, &val))
    }
}

fn metadata_total_bytes(conn: &Connection) -> Result<u64> {
    let mut stmt = conn
        .prepare("SELECT meta_key, meta_value FROM object_meta")
        .map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(map_sqlite_err)?;

    let mut total_bytes: u64 = 0;
    for row in rows {
        let (key, val) = row.map_err(map_sqlite_err)?;
        if key.len() != META_KEY_LEN || val.len() != META_VAL_LEN {
            continue;
        }
        let Some(object_type) = ObjectType::from_u8(key[0]) else {
            continue;
        };
        if let Some(meta) = decode_meta_value(object_type, &val) {
            total_bytes = total_bytes.saturating_add(meta.size);
        }
    }

    Ok(total_bytes)
}

fn xorb_hash_from_file_metadata(path: &Path, total_size: u64) -> Result<MerkleHash> {
    use std::io::{Read, Seek, SeekFrom};

    let total_size = usize::try_from(total_size).map_err(|_| {
        CacheServiceError::InternalError(
            format!("cached xorb {} is too large to index", path.display()).into(),
        )
    })?;
    if total_size < FOOTER_SIZE {
        return Err(CacheServiceError::InternalError(
            format!("cached xorb {} is too small for footer", path.display()).into(),
        ));
    }

    let mut file = std::fs::File::open(path).map_err(|e| {
        CacheServiceError::InternalError(
            format!("failed to open cached xorb {}: {e}", path.display()).into(),
        )
    })?;
    let footer_start = u64::try_from(total_size - FOOTER_SIZE).map_err(|_| {
        CacheServiceError::InternalError(
            format!("cached xorb {} footer offset overflow", path.display()).into(),
        )
    })?;
    file.seek(SeekFrom::Start(footer_start)).map_err(|e| {
        CacheServiceError::InternalError(
            format!("failed to seek cached xorb footer {}: {e}", path.display()).into(),
        )
    })?;
    let mut footer = vec![0u8; FOOTER_SIZE];
    file.read_exact(&mut footer).map_err(|e| {
        CacheServiceError::InternalError(
            format!("failed to read cached xorb footer {}: {e}", path.display()).into(),
        )
    })?;

    let region = xorb_metadata_region(total_size, &footer).map_err(|e| {
        CacheServiceError::InternalError(
            format!(
                "cached xorb metadata header invalid {}: {e}",
                path.display()
            )
            .into(),
        )
    })?;
    let offset = u64::try_from(region.offset).map_err(|_| {
        CacheServiceError::InternalError(
            format!("cached xorb {} metadata offset overflow", path.display()).into(),
        )
    })?;
    file.seek(SeekFrom::Start(offset)).map_err(|e| {
        CacheServiceError::InternalError(
            format!(
                "failed to seek cached xorb metadata {}: {e}",
                path.display()
            )
            .into(),
        )
    })?;
    let mut metadata = vec![0u8; region.len];
    file.read_exact(&mut metadata).map_err(|e| {
        CacheServiceError::InternalError(
            format!(
                "failed to read cached xorb metadata {}: {e}",
                path.display()
            )
            .into(),
        )
    })?;

    xorb_hash_from_metadata(total_size, &footer, &metadata).map_err(|e| {
        CacheServiceError::InternalError(
            format!("cached xorb metadata invalid {}: {e}", path.display()).into(),
        )
    })
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

fn reconcile_metadata_with_files(
    root: &Path,
    conn: &Connection,
) -> Result<(u64, CacheIntegrityStats)> {
    let mut stmt = conn
        .prepare("SELECT meta_key, meta_value FROM object_meta")
        .map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(map_sqlite_err)?;

    let mut initial_bytes: u64 = 0;
    let mut integrity = CacheIntegrityStats::default();
    let mut deletes: Vec<Vec<u8>> = Vec::new();
    let mut updates: Vec<([u8; META_KEY_LEN], [u8; META_VAL_LEN])> = Vec::new();
    let mut live_paths: HashSet<PathBuf> = HashSet::new();
    for row in rows {
        let (key, val) = row.map_err(map_sqlite_err)?;
        if key.len() != META_KEY_LEN || val.len() != META_VAL_LEN {
            deletes.push(key);
            continue;
        }

        let mut meta_key = [0u8; META_KEY_LEN];
        meta_key.copy_from_slice(&key);
        let Some(object_type) = ObjectType::from_u8(meta_key[0]) else {
            deletes.push(key);
            continue;
        };
        let Some(meta) = decode_meta_value(object_type, &val) else {
            deletes.push(key);
            continue;
        };

        let path = object_path_from_meta_key(root, &meta_key, object_type);
        let actual_size = match std::fs::metadata(&path) {
            Ok(file_meta) if file_meta.is_file() => file_meta.len(),
            Ok(_) => {
                deletes.push(key);
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                deletes.push(key);
                continue;
            }
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to stat cached file {}: {e}", path.display()).into(),
                ));
            }
        };

        initial_bytes = initial_bytes.saturating_add(actual_size);
        live_paths.insert(path);
        if actual_size != meta.size {
            updates.push((
                meta_key,
                encode_meta_value(
                    actual_size,
                    meta.last_access,
                    meta.access_count,
                    meta.cached_at,
                ),
            ));
        }
    }
    drop(stmt);

    for key in deletes {
        let removed = conn
            .execute(
                "DELETE FROM object_meta WHERE meta_key = ?1",
                params![key.as_slice()],
            )
            .map_err(map_sqlite_err)?;
        integrity.metadata_entries_removed += removed as u64;
    }
    for (key, val) in updates {
        let changed = conn
            .execute(
                "UPDATE object_meta SET meta_value = ?2 WHERE meta_key = ?1",
                params![key.as_slice(), val.as_slice()],
            )
            .map_err(map_sqlite_err)?;
        integrity.metadata_size_corrections += changed as u64;
    }

    reconcile_unindexed_files(root, conn, &live_paths, &mut initial_bytes, &mut integrity)?;

    Ok((initial_bytes, integrity))
}

fn reconcile_unindexed_files(
    root: &Path,
    conn: &Connection,
    live_paths: &HashSet<PathBuf>,
    initial_bytes: &mut u64,
    integrity: &mut CacheIntegrityStats,
) -> Result<()> {
    for dir_name in ["xorbs", "shards", "packs", "metadata"] {
        let recoverable_type = match dir_name {
            "xorbs" => Some(ObjectType::Xorb),
            "shards" => Some(ObjectType::Shard),
            "metadata" => Some(ObjectType::Metadata),
            _ => None,
        };
        let object_dir = root.join(dir_name);
        match std::fs::read_dir(&object_dir) {
            Ok(prefix_entries) => {
                for prefix_entry in prefix_entries {
                    let prefix_entry = prefix_entry.map_err(|e| {
                        CacheServiceError::InternalError(
                            format!("failed to read cache dir {}: {e}", object_dir.display())
                                .into(),
                        )
                    })?;
                    let prefix_path = prefix_entry.path();
                    let file_type = prefix_entry.file_type().map_err(|e| {
                        CacheServiceError::InternalError(
                            format!("failed to stat cache dir {}: {e}", prefix_path.display())
                                .into(),
                        )
                    })?;
                    if !file_type.is_dir() {
                        remove_unindexed_path(&prefix_path)?;
                        integrity.unindexed_paths_removed += 1;
                        continue;
                    }

                    let object_entries = std::fs::read_dir(&prefix_path).map_err(|e| {
                        CacheServiceError::InternalError(
                            format!("failed to read cache dir {}: {e}", prefix_path.display())
                                .into(),
                        )
                    })?;
                    for object_entry in object_entries {
                        let object_path = object_entry
                            .map_err(|e| {
                                CacheServiceError::InternalError(
                                    format!(
                                        "failed to read cache dir {}: {e}",
                                        prefix_path.display()
                                    )
                                    .into(),
                                )
                            })?
                            .path();
                        if live_paths.contains(&object_path) {
                            continue;
                        }
                        if let Some(object_type) = recoverable_type
                            && let Some(storage_id) =
                                storage_id_from_cache_file(&prefix_path, &object_path)
                        {
                            let file_meta = std::fs::metadata(&object_path).map_err(|e| {
                                CacheServiceError::InternalError(
                                    format!(
                                        "failed to stat cached file {}: {e}",
                                        object_path.display()
                                    )
                                    .into(),
                                )
                            })?;
                            if file_meta.is_file() {
                                let size = file_meta.len();
                                let now = epoch_millis();
                                let meta_key = make_meta_key(object_type, &storage_id);
                                let meta_val = encode_meta_value(size, now, 0, now);
                                conn.execute(
                                    "INSERT OR REPLACE INTO object_meta (meta_key, meta_value) VALUES (?1, ?2)",
                                    params![meta_key.as_slice(), meta_val.as_slice()],
                                )
                                .map_err(map_sqlite_err)?;
                                *initial_bytes = initial_bytes.saturating_add(size);
                                integrity.unindexed_objects_indexed += 1;
                                debug!(
                                    path = %object_path.display(),
                                    size,
                                    object_type = ?object_type,
                                    "indexed untracked cache object"
                                );
                                continue;
                            }
                        }
                        remove_unindexed_path(&object_path)?;
                        integrity.unindexed_paths_removed += 1;
                    }

                    let _ = std::fs::remove_dir(&prefix_path);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to read cache dir {}: {e}", object_dir.display()).into(),
                ));
            }
        }
    }

    Ok(())
}

fn remove_unindexed_path(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| {
        CacheServiceError::InternalError(format!("failed to stat {}: {e}", path.display()).into())
    })?;
    if metadata.is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| {
            CacheServiceError::InternalError(
                format!(
                    "failed to remove unindexed cache dir {}: {e}",
                    path.display()
                )
                .into(),
            )
        })?;
    } else {
        std::fs::remove_file(path).map_err(|e| {
            CacheServiceError::InternalError(
                format!(
                    "failed to remove unindexed cache file {}: {e}",
                    path.display()
                )
                .into(),
            )
        })?;
    }

    debug!(path = %path.display(), "removed unindexed cache object");
    Ok(())
}

fn storage_id_from_cache_file(prefix_path: &Path, object_path: &Path) -> Option<[u8; 32]> {
    let prefix = prefix_path.file_name()?.to_str()?;
    let file_name = object_path.file_name()?.to_str()?;
    if prefix.len() != 2 || file_name.get(..2)? != prefix {
        return None;
    }
    parse_hash_hex(file_name)
}

fn object_path_from_meta_key(
    root: &Path,
    meta_key: &[u8; META_KEY_LEN],
    object_type: ObjectType,
) -> PathBuf {
    let hash_hex = hex::encode(&meta_key[1..]);
    root.join(object_type.dir_name())
        .join(&hash_hex[..2])
        .join(hash_hex)
}

/// Build the 33-byte metadata key: `[object_type_u8, hash_bytes[0..32]]`.
fn make_meta_key(object_type: ObjectType, hash: &[u8; 32]) -> [u8; META_KEY_LEN] {
    let mut key = [0u8; META_KEY_LEN];
    key[0] = object_type.as_u8();
    key[1..].copy_from_slice(hash);
    key
}

fn storage_id_bytes(key: &ServerObjectKey) -> Option<[u8; 32]> {
    match key.object_type {
        ObjectType::Xorb | ObjectType::Shard => parse_hash_hex(&key.hash),
        ObjectType::Pack | ObjectType::PackIndex => {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&[key.object_type.as_u8()]);
            hasher.update(b"\0");
            hasher.update(key.hash.as_bytes());
            Some(*hasher.finalize().as_bytes())
        }
        ObjectType::Metadata => {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&[key.object_type.as_u8()]);
            hasher.update(b"\0");
            hasher.update(key.repo_path.as_bytes());
            hasher.update(b"\0");
            hasher.update(key.hash.as_bytes());
            Some(*hasher.finalize().as_bytes())
        }
    }
}

fn storage_id_hex(key: &ServerObjectKey) -> Option<String> {
    storage_id_bytes(key).map(|id| hex::encode(&id))
}

/// Encode metadata value as 32 bytes (4 × u64 LE).
fn encode_meta_value(
    size: u64,
    last_access: u64,
    access_count: u64,
    cached_at: u64,
) -> [u8; META_VAL_LEN] {
    let mut val = [0u8; META_VAL_LEN];
    val[..8].copy_from_slice(&size.to_le_bytes());
    val[8..16].copy_from_slice(&last_access.to_le_bytes());
    val[16..24].copy_from_slice(&access_count.to_le_bytes());
    val[24..32].copy_from_slice(&cached_at.to_le_bytes());
    val
}

fn decode_meta_value(object_type: ObjectType, val: &[u8]) -> Option<ObjectMeta> {
    if val.len() != META_VAL_LEN {
        return None;
    }
    Some(ObjectMeta {
        object_type,
        size: read_u64_field(val, 0..8)?,
        last_access: read_u64_field(val, 8..16)?,
        access_count: read_u64_field(val, 16..24)?,
        cached_at: read_u64_field(val, 24..32)?,
    })
}

fn read_u64_field(val: &[u8], range: Range<usize>) -> Option<u64> {
    let bytes: [u8; 8] = val.get(range)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// Current time as milliseconds since UNIX epoch.
fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Eviction weight by object type — lower weight = evicted first.
///
/// Xorbs are large and re-fetchable with a single GET, so they're evicted
/// first. Shards are small and critical for dedup index population, so
/// they're evicted last.
fn eviction_type_weight(type_byte: u8) -> u8 {
    match type_byte {
        3..=5 => 1, // Pack / PackIndex / Metadata
        1 => 3,     // Shard — evict last
        _ => 0,     // Xorb and unknown values — evict first
    }
}

/// Parse a hex-encoded hash string into 32 bytes.
pub fn parse_hash_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

mod hex {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push(HEX_CHARS[(b >> 4) as usize] as char);
            s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
        }
        s
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    fn test_store() -> CacheStore {
        test_store_with_budget(1_073_741_824)
    }

    fn test_store_with_budget(max_bytes: u64) -> CacheStore {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db = CacheDb::open_or_create(&root.join(CACHE_DB_FILE)).unwrap();
        let conn = db.connect().unwrap();
        std::mem::forget(dir);
        CacheStore::open(root, max_bytes, conn).unwrap()
    }

    fn test_key(hash: &str) -> ServerObjectKey {
        ServerObjectKey {
            bucket: "test-bucket".to_string(),
            repo_path: "org/repo".to_string(),
            object_type: ObjectType::Xorb,
            hash: hash.to_string(),
        }
    }

    fn pack_key(name: &str) -> ServerObjectKey {
        ServerObjectKey {
            bucket: String::new(),
            repo_path: "org/repo".to_string(),
            object_type: ObjectType::Pack,
            hash: name.to_string(),
        }
    }

    fn assert_clean_startup_integrity(stats: &CacheStats) {
        assert_eq!(stats.startup_integrity, CacheIntegrityStats::default());
    }

    fn assert_clean_runtime_integrity(stats: &CacheStats) {
        assert_eq!(
            stats.runtime_integrity,
            CacheRuntimeIntegrityStats::default()
        );
    }

    #[test]
    fn object_type_round_trip() {
        for ot in [
            ObjectType::Xorb,
            ObjectType::Shard,
            ObjectType::Pack,
            ObjectType::PackIndex,
            ObjectType::Metadata,
        ] {
            assert_eq!(ObjectType::from_u8(ot.as_u8()), Some(ot));
        }
        assert_eq!(ObjectType::from_u8(255), None);
    }

    #[test]
    fn object_type_dir_names() {
        assert_eq!(ObjectType::Xorb.dir_name(), "xorbs");
        assert_eq!(ObjectType::Shard.dir_name(), "shards");
        assert_eq!(ObjectType::Pack.dir_name(), "packs");
        assert_eq!(ObjectType::PackIndex.dir_name(), "packs");
        assert_eq!(ObjectType::Metadata.dir_name(), "metadata");
    }

    #[test]
    fn object_path_uses_sharding_layout() {
        let store = test_store();
        let key = test_key("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
        let path = store.object_path(&key);
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains(
                "xorbs/ab/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
            )
        );
    }

    #[test]
    fn put_and_get_round_trip() {
        let store = test_store();
        let data = Bytes::from_static(b"hello world");
        let hash = blake3::hash(&data);
        let hash_hex = hash.to_hex().to_string();
        let key = test_key(&hash_hex);

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();
        assert_eq!(store.current_bytes(), data.len() as u64);

        let got = store.get(&key).unwrap().expect("should be a hit");
        assert_eq!(got, data);
    }

    #[test]
    fn put_unverified_temp_path_commits_file_and_metadata() {
        let store = test_store();
        let key = pack_key("pack-streamed");
        let body = b"streamed pack bytes";
        let temp_path = store.create_temp_object_path(&key).unwrap();
        let temp_file_path = temp_path.to_path_buf();
        std::fs::write(&temp_file_path, body).unwrap();

        store
            .put_unverified_temp_path(&key, temp_path, body.len() as u64)
            .unwrap();

        assert!(!temp_file_path.exists());
        assert_eq!(store.current_bytes(), body.len() as u64);
        let got = store.get(&key).unwrap().expect("should be a hit");
        assert_eq!(got.as_ref(), body);
    }

    #[test]
    fn get_file_opens_cached_pack_without_reading_body() {
        use std::io::Read;

        let store = test_store();
        let key = pack_key("pack-file");
        let body = Bytes::from_static(b"file-backed pack bytes");
        store.put_unverified(&key, body.clone()).unwrap();

        let cached = store.get_file(&key).unwrap().expect("should be a hit");
        assert_eq!(cached.size, body.len() as u64);
        let mut file_body = Vec::new();
        let mut file = cached.file;
        file.read_to_end(&mut file_body).unwrap();
        assert_eq!(file_body, body);
    }

    #[test]
    fn oversized_temp_path_commit_keeps_existing_object() {
        let store = test_store_with_budget(10);
        let key = pack_key("pack-streamed");
        store
            .put_unverified(&key, Bytes::from_static(b"old"))
            .unwrap();

        let temp_path = store.create_temp_object_path(&key).unwrap();
        let temp_file_path = temp_path.to_path_buf();
        std::fs::write(&temp_file_path, b"this payload is too large").unwrap();

        let err = store
            .put_unverified_temp_path(&key, temp_path, 25)
            .unwrap_err();

        assert!(matches!(err, CacheServiceError::DiskFull { .. }));
        assert!(!temp_file_path.exists());
        assert_eq!(store.current_bytes(), 3);
        let got = store.get(&key).unwrap().expect("existing object remains");
        assert_eq!(got.as_ref(), b"old");
    }

    #[test]
    fn recoverable_temp_path_commit_returns_temp_path_when_over_budget() {
        let store = test_store_with_budget(10);
        let key = pack_key("pack-streamed");
        store
            .put_unverified(&key, Bytes::from_static(b"old"))
            .unwrap();

        let temp_path = store.create_temp_object_path(&key).unwrap();
        let temp_file_path = temp_path.to_path_buf();
        std::fs::write(&temp_file_path, b"this payload is too large").unwrap();

        let err = store
            .put_unverified_temp_path_recoverable(&key, temp_path, 25)
            .unwrap_err();
        let (error, recovery) = err.into_parts();
        let TempPathCommitRecovery::TempPath(returned_temp_path) = recovery else {
            panic!("temp path should be recoverable");
        };

        assert!(matches!(error, CacheServiceError::DiskFull { .. }));
        assert_eq!(
            std::fs::read(returned_temp_path.as_ref() as &Path).unwrap(),
            b"this payload is too large"
        );
        drop(returned_temp_path);
        assert!(!temp_file_path.exists());
        assert_eq!(store.current_bytes(), 3);
        let got = store.get(&key).unwrap().expect("existing object remains");
        assert_eq!(got.as_ref(), b"old");
    }

    #[test]
    fn budget_prediction_uses_net_growth_for_existing_key() {
        let store = test_store_with_budget(10);
        let key = pack_key("pack-streamed");
        store
            .put_unverified(&key, Bytes::from_static(b"12345678"))
            .unwrap();

        assert!(
            !store.would_exceed_budget_after_put(&key, 8).unwrap(),
            "same-size replacement should not force eviction"
        );
        assert!(
            store.would_exceed_budget_after_put(&key, 11).unwrap(),
            "replacement growth still counts against budget"
        );

        let other_key = pack_key("pack-other");
        assert!(
            store.would_exceed_budget_after_put(&other_key, 3).unwrap(),
            "new object growth must still be budgeted"
        );
    }

    #[test]
    fn get_miss_returns_none() {
        let store = test_store();
        let key = test_key("0000000000000000000000000000000000000000000000000000000000000000");
        assert!(store.get(&key).unwrap().is_none());
    }

    #[test]
    fn put_rejects_hash_mismatch() {
        let store = test_store();
        let data = Bytes::from_static(b"hello");
        let wrong_hash = [0u8; 32];
        let key = test_key("0000000000000000000000000000000000000000000000000000000000000000");

        let err = store.put(&key, data, &wrong_hash).unwrap_err();
        assert!(matches!(err, CacheServiceError::HashMismatch { .. }));
    }

    #[test]
    fn verify_cached_object_identity_evicts_corrupt_shard() {
        let store = test_store();
        let data = Bytes::from_static(b"valid shard bytes");
        let hash = compute_data_hash(&data);
        let key = ServerObjectKey {
            bucket: "b".into(),
            repo_path: "r".into(),
            object_type: ObjectType::Shard,
            hash: hash.hex(),
        };

        store.put_unverified(&key, data).unwrap();
        assert!(store.verify_cached_object_identity(&key).unwrap());

        std::fs::write(store.object_path(&key), b"corrupt shard bytes").unwrap();

        assert!(!store.verify_cached_object_identity(&key).unwrap());
        assert!(!store.object_path(&key).exists());
        assert_eq!(store.current_bytes(), 0);
        let stats = store.stats().unwrap();
        assert_eq!(stats.shard_count, 0);
        assert_eq!(stats.runtime_integrity.missing_files_repaired, 0);
        assert_eq!(stats.runtime_integrity.invalid_objects_evicted, 1);
        assert_eq!(stats.runtime_integrity.metadata_entries_recreated, 0);
    }

    #[test]
    fn get_range_returns_correct_slice() {
        let store = test_store();
        let data = Bytes::from_static(b"0123456789abcdef");
        let hash = blake3::hash(&data);
        let hash_hex = hash.to_hex().to_string();
        let key = test_key(&hash_hex);

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();

        let slice = store.get_range(&key, 4..10).unwrap().expect("should hit");
        let CacheRangeRead::Hit(slice) = slice else {
            panic!("range should be satisfiable");
        };
        assert_eq!(slice.data.as_ref(), b"456789");
        assert_eq!(slice.range, 4..10);
        assert_eq!(slice.total_size, data.len() as u64);
    }

    #[test]
    fn get_range_miss_returns_none() {
        let store = test_store();
        let key = test_key("0000000000000000000000000000000000000000000000000000000000000000");
        assert!(store.get_range(&key, 0..10).unwrap().is_none());
    }

    #[test]
    fn get_range_clips_end_past_object_size() {
        let store = test_store();
        let data = Bytes::from_static(b"0123456789abcdef");
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();

        let range = store
            .get_range(&key, 10..20)
            .unwrap()
            .expect("object should be cached");
        let CacheRangeRead::Hit(slice) = range else {
            panic!("range should be satisfiable");
        };
        assert_eq!(slice.data.as_ref(), b"abcdef");
        assert_eq!(slice.range, 10..data.len() as u64);
        assert_eq!(slice.total_size, data.len() as u64);
    }

    #[test]
    fn get_range_reports_unsatisfiable_when_start_is_past_object_size() {
        let store = test_store();
        let data = Bytes::from_static(b"0123456789abcdef");
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();

        let range = store
            .get_range(&key, data.len() as u64..data.len() as u64 + 4)
            .unwrap()
            .expect("object should be cached");
        assert!(matches!(
            range,
            CacheRangeRead::Unsatisfiable { total_size } if total_size == data.len() as u64
        ));
    }

    #[test]
    fn current_bytes_tracks_puts() {
        let store = test_store();
        assert_eq!(store.current_bytes(), 0);

        let data1 = Bytes::from_static(b"aaaa");
        let hash1 = blake3::hash(&data1);
        let key1 = test_key(&hash1.to_hex().to_string());
        store.put(&key1, data1.clone(), hash1.as_bytes()).unwrap();
        assert_eq!(store.current_bytes(), 4);

        let data2 = Bytes::from_static(b"bbbbbb");
        let hash2 = blake3::hash(&data2);
        let key2 = ServerObjectKey {
            bucket: "b".to_string(),
            repo_path: "r".to_string(),
            object_type: ObjectType::Shard,
            hash: hash2.to_hex().to_string(),
        };
        store.put(&key2, data2.clone(), hash2.as_bytes()).unwrap();
        assert_eq!(store.current_bytes(), 10);
    }

    #[test]
    fn duplicate_put_does_not_double_count_bytes() {
        let store = test_store();
        let data = Bytes::from_static(b"same object twice");
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();
        store.put(&key, data.clone(), hash.as_bytes()).unwrap();

        assert_eq!(store.current_bytes(), data.len() as u64);
        assert_eq!(store.stats().unwrap().total_bytes, data.len() as u64);
    }

    #[test]
    fn stable_key_replacements_apply_byte_deltas() {
        let store = test_store_with_budget(20);
        let key = pack_key("pack-abc");

        let small = Bytes::from_static(b"small");
        let small_hash = blake3::hash(&small);
        store
            .put(&key, small.clone(), small_hash.as_bytes())
            .unwrap();
        assert_eq!(store.current_bytes(), small.len() as u64);

        let larger = Bytes::from_static(b"larger pack");
        let larger_hash = blake3::hash(&larger);
        store
            .put(&key, larger.clone(), larger_hash.as_bytes())
            .unwrap();
        assert_eq!(store.current_bytes(), larger.len() as u64);

        let huge = Bytes::from(vec![0u8; 30]);
        let huge_hash = blake3::hash(&huge);
        let err = store.put(&key, huge, huge_hash.as_bytes()).unwrap_err();
        assert!(matches!(err, CacheServiceError::DiskFull { .. }));
        assert_eq!(store.current_bytes(), larger.len() as u64);

        store
            .put(&key, small.clone(), small_hash.as_bytes())
            .unwrap();
        assert_eq!(store.current_bytes(), small.len() as u64);
    }

    #[test]
    fn concurrent_puts_respect_budget() {
        let store = std::sync::Arc::new(test_store_with_budget(100));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let handles = [0xA0, 0xB0].map(|byte| {
            let store = std::sync::Arc::clone(&store);
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                let data = Bytes::from(vec![byte; 60]);
                let hash = blake3::hash(&data);
                let key = test_key(&hash.to_hex().to_string());
                barrier.wait();
                store.put(&key, data, hash.as_bytes())
            })
        });

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let successes = results.iter().filter(|result| result.is_ok()).count();
        let disk_full = results
            .iter()
            .filter(|result| matches!(result, Err(CacheServiceError::DiskFull { .. })))
            .count();

        assert_eq!(successes, 1);
        assert_eq!(disk_full, 1);
        assert!(store.current_bytes() <= store.max_bytes());
    }

    #[test]
    fn pack_put_get_remove_uses_canonical_storage_id() {
        let store = test_store();
        let key = pack_key("pack-abc");
        let data = Bytes::from_static(b"pack bytes");
        let hash = blake3::hash(&data);
        let storage_id = storage_id_bytes(&key).unwrap();
        let storage_hex = hex::encode(&storage_id);

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();

        let path = store.object_path(&key);
        assert!(path.ends_with(&storage_hex));
        assert!(!path.to_string_lossy().contains("pack-abc"));
        assert!(path.exists());
        assert_eq!(store.get(&key).unwrap().unwrap(), data);

        let meta_key = make_meta_key(ObjectType::Pack, &storage_id);
        let freed = store.remove_object(&meta_key).unwrap();
        assert_eq!(freed, data.len() as u64);
        assert!(!path.exists());
        assert!(store.get(&key).unwrap().is_none());
        assert_eq!(store.current_bytes(), 0);
    }

    #[test]
    fn open_computes_initial_bytes_from_db() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(CACHE_DB_FILE);

        // First open: put some data.
        {
            let db = CacheDb::open_or_create(&db_path).unwrap();
            let store = CacheStore::open(root.clone(), 1_000_000, db.connect().unwrap()).unwrap();
            let data = Bytes::from(vec![0u8; 100]);
            let hash = blake3::hash(&data);
            let key = test_key(&hash.to_hex().to_string());
            store.put(&key, data, hash.as_bytes()).unwrap();
            assert_eq!(store.current_bytes(), 100);
        }

        // Second open: should recover current_bytes from metadata scan.
        {
            let db = CacheDb::open_or_create(&db_path).unwrap();
            let store = CacheStore::open(root, 1_000_000, db.connect().unwrap()).unwrap();
            assert_eq!(store.current_bytes(), 100);
            assert_clean_startup_integrity(&store.stats().unwrap());
        }
    }

    #[test]
    fn open_removes_stale_metadata_for_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(CACHE_DB_FILE);
        let data = Bytes::from(vec![0xAB; 40]);
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());

        {
            let db = CacheDb::open_or_create(&db_path).unwrap();
            let store = CacheStore::open(root.clone(), 1_000_000, db.connect().unwrap()).unwrap();
            store.put(&key, data, hash.as_bytes()).unwrap();
            std::fs::remove_file(store.object_path(&key)).unwrap();
            assert_eq!(store.current_bytes(), 40);
        }

        let db = CacheDb::open_or_create(&db_path).unwrap();
        let store = CacheStore::open(root, 1_000_000, db.connect().unwrap()).unwrap();
        assert_eq!(store.current_bytes(), 0);
        let stats = store.stats().unwrap();
        assert_eq!(stats.xorb_count, 0);
        assert_eq!(stats.startup_integrity.metadata_entries_removed, 1);
        assert_eq!(stats.startup_integrity.metadata_size_corrections, 0);
        assert_eq!(stats.startup_integrity.unindexed_objects_indexed, 0);
        assert_eq!(stats.startup_integrity.unindexed_paths_removed, 0);
    }

    #[test]
    fn open_indexes_unindexed_content_addressed_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(CACHE_DB_FILE);
        let hash_hex = format!("ab{}", "0".repeat(62));
        let orphan = root.join("xorbs").join("ab").join(&hash_hex);
        let data = [0xAB; 24];

        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, data).unwrap();

        let db = CacheDb::open_or_create(&db_path).unwrap();
        let store = CacheStore::open(root, 1_000_000, db.connect().unwrap()).unwrap();

        assert_eq!(store.current_bytes(), data.len() as u64);
        assert!(orphan.exists());
        let stats = store.stats().unwrap();
        assert_eq!(stats.xorb_count, 1);
        assert_eq!(stats.startup_integrity.metadata_entries_removed, 0);
        assert_eq!(stats.startup_integrity.metadata_size_corrections, 0);
        assert_eq!(stats.startup_integrity.unindexed_objects_indexed, 1);
        assert_eq!(stats.startup_integrity.unindexed_paths_removed, 0);
        assert_eq!(
            store.get(&test_key(&hash_hex)).unwrap().unwrap().as_ref(),
            data
        );
    }

    #[test]
    fn open_removes_unrecoverable_unindexed_cache_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(CACHE_DB_FILE);
        let orphan = root.join("xorbs").join("ab").join("partial-write.tmp");

        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, [0xAB; 24]).unwrap();

        let db = CacheDb::open_or_create(&db_path).unwrap();
        let store = CacheStore::open(root, 1_000_000, db.connect().unwrap()).unwrap();

        assert_eq!(store.current_bytes(), 0);
        assert!(!orphan.exists());
        let stats = store.stats().unwrap();
        assert_eq!(stats.startup_integrity.metadata_entries_removed, 0);
        assert_eq!(stats.startup_integrity.metadata_size_corrections, 0);
        assert_eq!(stats.startup_integrity.unindexed_objects_indexed, 0);
        assert_eq!(stats.startup_integrity.unindexed_paths_removed, 1);
    }

    #[test]
    fn open_corrects_metadata_size_drift_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(CACHE_DB_FILE);
        let data = Bytes::from(vec![0xCD; 64]);
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());

        {
            let db = CacheDb::open_or_create(&db_path).unwrap();
            let store = CacheStore::open(root.clone(), 1_000_000, db.connect().unwrap()).unwrap();
            store.put(&key, data, hash.as_bytes()).unwrap();
            std::fs::write(store.object_path(&key), [0xEE; 12]).unwrap();
            assert_eq!(store.current_bytes(), 64);
        }

        let db = CacheDb::open_or_create(&db_path).unwrap();
        let store = CacheStore::open(root, 1_000_000, db.connect().unwrap()).unwrap();
        assert_eq!(store.current_bytes(), 12);
        let stats = store.stats().unwrap();
        assert_eq!(stats.startup_integrity.metadata_entries_removed, 0);
        assert_eq!(stats.startup_integrity.metadata_size_corrections, 1);
        assert_eq!(stats.startup_integrity.unindexed_objects_indexed, 0);
        assert_eq!(stats.startup_integrity.unindexed_paths_removed, 0);

        let meta_key = make_meta_key(ObjectType::Xorb, hash.as_bytes());
        let conn = store.connection().unwrap();
        let val: Vec<u8> = conn
            .query_row(
                "SELECT meta_value FROM object_meta WHERE meta_key = ?1",
                params![meta_key.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(read_u64_field(&val, 0..8), Some(12));
    }

    #[test]
    fn stub_works() {
        let store = CacheStore::stub();
        assert_eq!(store.current_bytes(), 0);
        let stats = store.stats().unwrap();
        assert_clean_startup_integrity(&stats);
        assert_clean_runtime_integrity(&stats);
        let key = test_key("0000000000000000000000000000000000000000000000000000000000000000");
        assert!(store.get(&key).unwrap().is_none());
    }

    #[test]
    fn stats_counts_objects_by_type() {
        let store = test_store();

        // Empty store.
        let s = store.stats().unwrap();
        assert_eq!(s.total_bytes, 0);
        assert_eq!(s.xorb_count, 0);
        assert_eq!(s.shard_count, 0);
        assert_eq!(s.pack_count, 0);
        assert_eq!(s.max_bytes, 1_073_741_824);
        assert_clean_startup_integrity(&s);
        assert_clean_runtime_integrity(&s);

        // Insert one xorb and one shard.
        let d1 = Bytes::from_static(b"xorb-data");
        let h1 = blake3::hash(&d1);
        let k1 = test_key(&h1.to_hex().to_string());
        store.put(&k1, d1.clone(), h1.as_bytes()).unwrap();

        let d2 = Bytes::from_static(b"shard-data!!");
        let h2 = blake3::hash(&d2);
        let k2 = ServerObjectKey {
            bucket: "b".into(),
            repo_path: "r".into(),
            object_type: ObjectType::Shard,
            hash: h2.to_hex().to_string(),
        };
        store.put(&k2, d2.clone(), h2.as_bytes()).unwrap();

        let s = store.stats().unwrap();
        assert_eq!(s.xorb_count, 1);
        assert_eq!(s.shard_count, 1);
        assert_eq!(s.pack_count, 0);
        assert_eq!(s.total_bytes, d1.len() as u64 + d2.len() as u64);
        assert_clean_runtime_integrity(&s);
    }

    #[test]
    fn touch_metadata_increments_access_count() {
        let store = test_store();
        let data = Bytes::from_static(b"test data");
        let hash = blake3::hash(&data);
        let hash_hex = hash.to_hex().to_string();
        let key = test_key(&hash_hex);

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();

        // First get bumps access_count from 0 to 1.
        let _ = store.get(&key).unwrap();
        // Second get bumps to 2.
        let _ = store.get(&key).unwrap();

        // Read the raw metadata to verify.
        let meta_key = make_meta_key(ObjectType::Xorb, hash.as_bytes());
        let conn = store.connection().unwrap();
        let val: Vec<u8> = conn
            .query_row(
                "SELECT meta_value FROM object_meta WHERE meta_key = ?1",
                params![meta_key.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let access_count = u64::from_le_bytes(val[16..24].try_into().unwrap());
        assert_eq!(access_count, 2);
    }

    #[test]
    fn runtime_get_recreates_missing_metadata_without_double_counting_bytes() {
        let store = test_store();
        let data = Bytes::from_static(b"cached while sqlite row vanishes");
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();
        let meta_key = make_meta_key(ObjectType::Xorb, hash.as_bytes());
        {
            let conn = store.connection().unwrap();
            conn.execute(
                "DELETE FROM object_meta WHERE meta_key = ?1",
                params![meta_key.as_slice()],
            )
            .unwrap();
        }

        assert_eq!(store.get(&key).unwrap().unwrap(), data);
        let stats = store.stats().unwrap();
        assert_eq!(stats.xorb_count, 1);
        assert_eq!(stats.total_bytes, data.len() as u64);
        assert_eq!(store.current_bytes(), data.len() as u64);
        assert_eq!(stats.runtime_integrity.missing_files_repaired, 0);
        assert_eq!(stats.runtime_integrity.invalid_objects_evicted, 0);
        assert_eq!(stats.runtime_integrity.metadata_entries_recreated, 1);
    }

    #[test]
    fn open_recreates_missing_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = root.join(CACHE_DB_FILE);
        let db = CacheDb::open_or_create(&db_path).unwrap();

        let data = Bytes::from_static(b"cached before metadata reset");
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());
        {
            let store = CacheStore::open(root.clone(), 1_000_000, db.connect().unwrap()).unwrap();
            store.put(&key, data.clone(), hash.as_bytes()).unwrap();
            let meta_key = make_meta_key(ObjectType::Xorb, hash.as_bytes());
            let conn = store.connection().unwrap();
            conn.execute(
                "DELETE FROM object_meta WHERE meta_key = ?1",
                params![meta_key.as_slice()],
            )
            .unwrap();
        }

        let db = CacheDb::open_or_create(&db_path).unwrap();
        let store = CacheStore::open(root, 1_000_000, db.connect().unwrap()).unwrap();
        assert_eq!(store.current_bytes(), data.len() as u64);
        let stats = store.stats().unwrap();
        assert_eq!(stats.xorb_count, 1);
        assert_eq!(stats.startup_integrity.metadata_entries_removed, 0);
        assert_eq!(stats.startup_integrity.metadata_size_corrections, 0);
        assert_eq!(stats.startup_integrity.unindexed_objects_indexed, 1);
        assert_eq!(stats.startup_integrity.unindexed_paths_removed, 0);

        let got = store.get(&key).unwrap().unwrap();
        assert_eq!(got, data);
        assert_eq!(store.current_bytes(), data.len() as u64);
    }

    #[test]
    fn cache_miss_removes_stale_metadata_for_missing_file() {
        let store = test_store_with_budget(32);
        let data = Bytes::from_static(b"cached object that vanished");
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();
        std::fs::remove_file(store.object_path(&key)).unwrap();

        assert!(store.get(&key).unwrap().is_none());
        assert_eq!(store.current_bytes(), 0);
        let stats = store.stats().unwrap();
        assert_eq!(stats.xorb_count, 0);
        assert_eq!(stats.runtime_integrity.missing_files_repaired, 1);
        assert_eq!(stats.runtime_integrity.invalid_objects_evicted, 0);
        assert_eq!(stats.runtime_integrity.metadata_entries_recreated, 0);

        let replacement = Bytes::from_static(b"replacement bytes fit");
        let replacement_hash = blake3::hash(&replacement);
        let replacement_key = test_key(&replacement_hash.to_hex().to_string());
        store
            .put(
                &replacement_key,
                replacement.clone(),
                replacement_hash.as_bytes(),
            )
            .unwrap();
        assert_eq!(store.current_bytes(), replacement.len() as u64);
    }

    #[test]
    fn cache_range_miss_removes_stale_metadata_for_missing_file() {
        let store = test_store();
        let data = Bytes::from_static(b"range object that vanished");
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());

        store.put(&key, data.clone(), hash.as_bytes()).unwrap();
        std::fs::remove_file(store.object_path(&key)).unwrap();

        assert!(store.get_range(&key, 0..4).unwrap().is_none());
        assert_eq!(store.current_bytes(), 0);
        let stats = store.stats().unwrap();
        assert_eq!(stats.xorb_count, 0);
        assert_eq!(stats.runtime_integrity.missing_files_repaired, 1);
        assert_eq!(stats.runtime_integrity.invalid_objects_evicted, 0);
        assert_eq!(stats.runtime_integrity.metadata_entries_recreated, 0);
    }

    #[test]
    fn put_returns_disk_full_when_over_budget() {
        // max_bytes = 50, so a 51-byte write should fail.
        let store = test_store_with_budget(50);

        let data = Bytes::from(vec![0xAA; 51]);
        let hash = blake3::hash(&data);
        let key = test_key(&hash.to_hex().to_string());

        let err = store.put(&key, data, hash.as_bytes()).unwrap_err();
        assert!(matches!(err, CacheServiceError::DiskFull { .. }));
    }

    #[test]
    fn emergency_evict_removes_oldest_ten_percent() {
        // max_bytes large enough to hold all objects.
        let store = test_store_with_budget(10_000);

        // Insert 20 objects of 10 bytes each = 200 bytes.
        for i in 0u8..20 {
            let data = Bytes::from(vec![i; 10]);
            let hash = blake3::hash(&data);
            let key = test_key(&hash.to_hex().to_string());
            store.put(&key, data, hash.as_bytes()).unwrap();
        }
        assert_eq!(store.current_bytes(), 200);

        let stats = store.emergency_evict().unwrap();
        // 10% of 20 = 2 objects evicted.
        assert_eq!(stats.evicted_count, 2);
        assert_eq!(stats.evicted_bytes, 20);
        assert_eq!(store.current_bytes(), 180);
        let eviction = store.eviction_stats();
        assert_eq!(eviction.total, 2);
        assert_eq!(eviction.xorb, 2);
    }

    #[test]
    fn emergency_evict_at_least_one_object() {
        let store = test_store_with_budget(10_000);

        // Insert 5 objects — 10% of 5 = 0, but we clamp to at least 1.
        for i in 0u8..5 {
            let data = Bytes::from(vec![i; 10]);
            let hash = blake3::hash(&data);
            let key = test_key(&hash.to_hex().to_string());
            store.put(&key, data, hash.as_bytes()).unwrap();
        }

        let stats = store.emergency_evict().unwrap();
        assert_eq!(stats.evicted_count, 1);
        assert_eq!(store.current_bytes(), 40);
    }

    #[test]
    fn emergency_evict_empty_cache_is_noop() {
        let store = test_store();
        let stats = store.emergency_evict().unwrap();
        assert_eq!(stats.evicted_count, 0);
        assert_eq!(stats.evicted_bytes, 0);
    }

    #[test]
    fn from_name_parses_known_types() {
        assert_eq!(ObjectType::from_name("xorb"), Some(ObjectType::Xorb));
        assert_eq!(ObjectType::from_name("Shard"), Some(ObjectType::Shard));
        assert_eq!(ObjectType::from_name("PACK"), Some(ObjectType::Pack));
        assert_eq!(
            ObjectType::from_name("pack-index"),
            Some(ObjectType::PackIndex)
        );
        assert_eq!(
            ObjectType::from_name("metadata"),
            Some(ObjectType::Metadata)
        );
        assert_eq!(ObjectType::from_name("unknown"), None);
    }

    #[test]
    fn evict_by_filter_removes_matching_type() {
        let store = test_store();

        // Insert 2 xorbs and 2 shards.
        let d1 = Bytes::from_static(b"xorb-one");
        let h1 = blake3::hash(&d1);
        let k1 = test_key(&h1.to_hex().to_string());
        store.put(&k1, d1.clone(), h1.as_bytes()).unwrap();

        let d2 = Bytes::from_static(b"xorb-two");
        let h2 = blake3::hash(&d2);
        let k2 = test_key(&h2.to_hex().to_string());
        store.put(&k2, d2.clone(), h2.as_bytes()).unwrap();

        let d3 = Bytes::from_static(b"shard-one");
        let h3 = blake3::hash(&d3);
        let k3 = ServerObjectKey {
            bucket: "b".into(),
            repo_path: "r".into(),
            object_type: ObjectType::Shard,
            hash: h3.to_hex().to_string(),
        };
        store.put(&k3, d3.clone(), h3.as_bytes()).unwrap();

        let d4 = Bytes::from_static(b"shard-two");
        let h4 = blake3::hash(&d4);
        let k4 = ServerObjectKey {
            bucket: "b".into(),
            repo_path: "r".into(),
            object_type: ObjectType::Shard,
            hash: h4.to_hex().to_string(),
        };
        store.put(&k4, d4.clone(), h4.as_bytes()).unwrap();

        let total = d1.len() + d2.len() + d3.len() + d4.len();
        assert_eq!(store.current_bytes(), total as u64);

        // Evict only xorbs.
        let filter = EvictFilter {
            object_type: Some(ObjectType::Xorb),
        };
        let stats = store.evict_by_filter(&filter).unwrap();
        assert_eq!(stats.evicted_count, 2);
        assert_eq!(stats.evicted_bytes, (d1.len() + d2.len()) as u64);
        let eviction = store.eviction_stats();
        assert_eq!(eviction.total, 2);
        assert_eq!(eviction.xorb, 2);
        assert_eq!(eviction.shard, 0);

        // Shards remain.
        assert_eq!(store.current_bytes(), (d3.len() + d4.len()) as u64);
        assert!(store.get(&k3).unwrap().is_some());
        assert!(store.get(&k4).unwrap().is_some());
        // Xorbs gone.
        assert!(store.get(&k1).unwrap().is_none());
        assert!(store.get(&k2).unwrap().is_none());
    }

    #[test]
    fn evict_by_filter_no_filter_evicts_all() {
        let store = test_store();

        let d1 = Bytes::from_static(b"data-a");
        let h1 = blake3::hash(&d1);
        let k1 = test_key(&h1.to_hex().to_string());
        store.put(&k1, d1, h1.as_bytes()).unwrap();

        let filter = EvictFilter { object_type: None };
        let stats = store.evict_by_filter(&filter).unwrap();
        assert_eq!(stats.evicted_count, 1);
        assert_eq!(store.current_bytes(), 0);
    }
}
