//! Per-repo local cache with hash-verified reads and LRU eviction.
//!
//! Chunks and shards are stored under `{dir}/{hash[:2]}/{hash}` and
//! verified via `compute_data_hash` on every read. A mismatch evicts
//! the stale entry and refetches via the caller-supplied closure.
//!
//! Xorbs also use the two-level layout, but validate their aggregate xorb
//! identity from serialized metadata instead of hashing the whole object.
//!
//! LRU eviction uses file modification time (mtime) as the access
//! timestamp — every successful read touches the file. Prune sorts
//! by mtime ascending and removes the oldest entries until the cache
//! fits within its configured byte budget.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use rusqlite::{Connection, OptionalExtension, params};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, warn};

use crate::error::{CacheError, Result};
use crate::key::CacheKey;
use crab_xet::hash::{HashedWrite, compute_data_hash, xorb_hash};
use crab_xet::xorb::builder::FOOTER_SIZE;
use crab_xet::xorb::format::{ChunkMeta, ChunkPlacement, MAX_XORB_SIZE, MerkleHash};
use crab_xet::xorb::parser::{
    XorbParser, verify_compressed_chunk, xorb_chunks_from_metadata, xorb_hash_from_metadata,
    xorb_metadata_region,
};

/// Default chunk cache ceiling: 10 GiB.
const DEFAULT_CHUNK_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// Maximum metadata-shard body retained in or read from the local cache.
pub const MAX_CACHE_SHARD_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum uncompressed chunk body retained in or read from the local cache.
pub const MAX_CACHE_CHUNK_BYTES: u64 = MAX_XORB_SIZE as u64;
/// Maximum workflow stage entry body retained in or read from the local cache.
pub const MAX_CACHE_STAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum metadata manifest body retained in or read from the local cache.
pub const MAX_CACHE_MANIFEST_BYTES: u64 = 256 * 1024 * 1024;
const XORB_INDEX_DIR: &str = "xorb-index";
const XORB_INDEX_DB: &str = "index.db";
const HASH_BYTES: usize = 32;
const XORB_INDEX_BUSY_TIMEOUT: Duration = Duration::from_secs(30);
const XORB_INDEX_OPEN_RETRY_DELAYS_MS: [u64; 4] = [5, 20, 50, 100];
const CACHE_FILL_LOCK_STRIPES: usize = 256;
pub const XORB_INDEX_SCHEMA_VERSION: i64 = 1;
const MAX_CACHE_LRU_ENTRIES: usize = 1_000_000;

fn new_fill_locks() -> Box<[tokio::sync::Mutex<()>]> {
    std::iter::repeat_with(|| tokio::sync::Mutex::new(()))
        .take(CACHE_FILL_LOCK_STRIPES)
        .collect()
}

/// Statistics returned by [`LocalCache::prune`].
#[derive(Debug, Clone, Default)]
pub struct PruneStats {
    /// Number of chunk files evicted.
    pub chunks_evicted: u64,
    /// Number of shard files evicted.
    pub shards_evicted: u64,
    /// Number of xorb files evicted.
    pub xorbs_evicted: u64,
    /// Total bytes freed across all evictions.
    pub bytes_freed: u64,
    /// Cache objects pruned or selected for pruning.
    pub entries: Vec<PrunedCacheObject>,
}

/// Options for local cache LRU pruning.
#[derive(Debug, Clone, Copy, Default)]
pub struct PruneOptions {
    /// Return the prune plan without deleting files.
    pub dry_run: bool,
    /// Include each pruned object in [`PruneStats::entries`].
    pub record_entries: bool,
}

/// Cache object type selected by LRU pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneObjectKind {
    Chunk,
    Shard,
    Xorb,
}

impl PruneObjectKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chunk => "chunk",
            Self::Shard => "shard",
            Self::Xorb => "xorb",
        }
    }
}

/// One cache object pruned or selected for pruning.
#[derive(Debug, Clone)]
pub struct PrunedCacheObject {
    pub kind: PruneObjectKind,
    pub path: PathBuf,
    pub bytes: u64,
}

impl PruneStats {
    /// Total object count pruned or selected for pruning.
    #[must_use]
    pub fn objects_evicted(&self) -> u64 {
        self.chunks_evicted + self.shards_evicted + self.xorbs_evicted
    }
}

/// Report returned by [`LocalCache::verify`].
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    /// Total objects checked.
    pub total: u64,
    /// Objects whose hash matched.
    pub valid: u64,
    /// Objects whose hash did not match (evicted).
    pub corrupt: u64,
}

/// Aggregate cache statistics.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Total bytes used by cached chunks.
    pub chunk_bytes: u64,
    /// Number of cached chunk files.
    pub chunk_count: u64,
    /// Total bytes used by cached shards.
    pub shard_bytes: u64,
    /// Number of cached shard files.
    pub shard_count: u64,
    /// Total bytes used by cached xorbs.
    pub xorb_bytes: u64,
    /// Number of cached xorb files.
    pub xorb_count: u64,
    /// Total bytes used by cached workflow stage entries.
    pub stage_bytes: u64,
    /// Number of cached workflow stage entries.
    pub stage_count: u64,
    /// Number of cached manifests.
    pub manifest_count: u64,
}

/// Verified local xorb candidate that may cover one or more requested chunks.
#[derive(Debug, Clone)]
pub struct CachedXorbCandidate {
    pub xorb_hash: MerkleHash,
    pub path: PathBuf,
    pub bytes: u64,
    pub payload_hash: [u8; 32],
    pub placements: Vec<ChunkPlacement>,
}

/// Parsed remote xorb metadata proven against a provider object identity token.
#[derive(Debug, Clone)]
pub struct CachedRemoteXorbIndex {
    pub xorb_hash: MerkleHash,
    pub payload_digest: [u8; 32],
    pub chunks: Vec<ChunkMeta>,
}

/// Per-repo local cache with hash-verified reads and LRU eviction.
///
/// All I/O is async via `tokio::fs`. Writes use atomic
/// tempfile-then-rename to avoid partial reads.
pub struct LocalCache {
    root: PathBuf,
    /// Shared byte ceiling for large data objects: chunk fragments and xorbs.
    chunk_max_bytes: u64,
    shard_max_bytes: Option<u64>,
    xorb_index_write_lock: Arc<std::sync::Mutex<()>>,
    fill_locks: Box<[tokio::sync::Mutex<()>]>,
}

impl LocalCache {
    /// Create a cache rooted at `root` with default limits.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            chunk_max_bytes: DEFAULT_CHUNK_MAX_BYTES,
            shard_max_bytes: None,
            xorb_index_write_lock: Arc::new(std::sync::Mutex::new(())),
            fill_locks: new_fill_locks(),
        }
    }

    /// Create a cache with explicit byte budgets.
    ///
    /// `chunk_max` is the shared ceiling for chunk fragments and xorbs.
    #[must_use]
    pub fn with_limits(root: PathBuf, chunk_max: u64, shard_max: Option<u64>) -> Self {
        Self {
            root,
            chunk_max_bytes: chunk_max,
            shard_max_bytes: shard_max,
            xorb_index_write_lock: Arc::new(std::sync::Mutex::new(())),
            fill_locks: new_fill_locks(),
        }
    }

    /// Root directory of this cache.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get a cached object, or fetch and cache it.
    ///
    /// Hash-verifies on read; evicts and refetches on mismatch.
    /// For manifests, `ETag` freshness is checked instead of content hash.
    ///
    /// # Errors
    ///
    /// Returns the fetch closure's error on remote failure, or
    /// [`CacheError::Io`] on local write failure.
    pub async fn get_or_fetch<F, Fut>(&self, key: &CacheKey, fetch: F) -> Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Bytes>>,
    {
        self.get_or_fetch_with(key, fetch).await
    }

    /// Get a cached object, allowing the fetch closure to use an outer error.
    ///
    /// This keeps cache ownership in `crab-cache` while allowing upper crates
    /// to compose origin fetches that return their own domain errors.
    pub async fn get_or_fetch_with<F, Fut, E>(
        &self,
        key: &CacheKey,
        fetch: F,
    ) -> std::result::Result<Bytes, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<Bytes, E>>,
        E: From<CacheError>,
    {
        self.get_or_fetch_with_limit(key, None, fetch).await
    }

    /// Get a cached object while bounding every cache and fetch body read.
    pub async fn get_or_fetch_bounded_with<F, Fut, E>(
        &self,
        key: &CacheKey,
        max_bytes: u64,
        fetch: F,
    ) -> std::result::Result<Bytes, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<Bytes, E>>,
        E: From<CacheError>,
    {
        self.get_or_fetch_with_limit(key, Some(max_bytes), fetch)
            .await
    }

    async fn get_or_fetch_with_limit<F, Fut, E>(
        &self,
        key: &CacheKey,
        max_bytes: Option<u64>,
        fetch: F,
    ) -> std::result::Result<Bytes, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<Bytes, E>>,
        E: From<CacheError>,
    {
        let max_bytes = effective_read_limit(key, max_bytes);
        if let Some(data) = self.try_read_key_limited(key, max_bytes).await {
            return Ok(data);
        }

        let fill_path = match key {
            CacheKey::Manifest { name, .. } => self.manifest_data_path(name),
            _ => self.hash_path(key),
        };
        let _fill_guard = self.fill_lock(&fill_path).lock().await;
        if let Some(data) = self.try_read_key_limited(key, max_bytes).await {
            return Ok(data);
        }

        match key {
            CacheKey::Chunk(hash) | CacheKey::Shard(hash) => {
                let path = self.hash_path(key);
                let data = fetch().await?;
                enforce_size_limit(key, &data, max_bytes).map_err(E::from)?;
                verify_data_hash(&data, hash).map_err(E::from)?;
                self.atomic_write(&path, &data).await.map_err(E::from)?;
                Ok(data)
            }
            CacheKey::Xorb(hash) => {
                let path = self.hash_path(key);
                let data = fetch().await?;
                enforce_size_limit(key, &data, max_bytes).map_err(E::from)?;
                verify_xorb_payload(&data, hash).map_err(E::from)?;
                self.atomic_write(&path, &data).await.map_err(E::from)?;
                let payload_hash = *blake3::hash(&data).as_bytes();
                self.index_cached_xorb_file_best_effort(
                    hash,
                    &path,
                    data.len() as u64,
                    payload_hash,
                )
                .await;
                Ok(data)
            }
            CacheKey::Stage(_) => {
                // Stage entries are keyed by the `StageHash` of their
                // inputs, not a hash of the entry bytes. Integrity is
                // enforced by `workflow::cache` via JSON deserialization.
                let path = self.hash_path(key);
                let data = fetch().await?;
                enforce_size_limit(key, &data, max_bytes).map_err(E::from)?;
                self.atomic_write(&path, &data).await.map_err(E::from)?;
                Ok(data)
            }
            CacheKey::Manifest { name, etag } => {
                let data_path = self.manifest_data_path(name);
                let etag_path = self.manifest_etag_path(name);
                let data = fetch().await?;
                enforce_size_limit(key, &data, max_bytes).map_err(E::from)?;
                self.atomic_write(&data_path, &data)
                    .await
                    .map_err(E::from)?;
                if let Some(tag) = etag {
                    self.atomic_write(&etag_path, tag.as_bytes())
                        .await
                        .map_err(E::from)?;
                }
                Ok(data)
            }
        }
    }

    /// Get a committed remote xorb without publishing it as an add-side candidate.
    ///
    /// Remote xorbs are already discoverable through the canonical global chunk
    /// index. Avoiding the local placement index keeps bulk reads from serializing
    /// thousands of redundant SQLite writes while retaining the full-xorb cache.
    pub async fn get_or_fetch_read_xorb_with<F, Fut, E>(
        &self,
        hash: &MerkleHash,
        fetch: F,
    ) -> std::result::Result<Bytes, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<Bytes, E>>,
        E: From<CacheError>,
    {
        let key = CacheKey::Xorb(*hash);
        let path = self.hash_path(&key);
        if let Some(data) = self.try_read_xorb_bytes(&path, hash).await {
            return Ok(data);
        }

        let _fill_guard = self.fill_lock(&path).lock().await;
        if let Some(data) = self.try_read_xorb_bytes(&path, hash).await {
            return Ok(data);
        }

        let data = fetch().await?;
        enforce_size_limit(&key, &data, Some(MAX_XORB_SIZE as u64)).map_err(E::from)?;
        verify_xorb_serialized_payload(&data, hash).map_err(E::from)?;
        self.atomic_write(&path, &data).await.map_err(E::from)?;
        Ok(data)
    }

    /// Read an existing complete xorb without creating a cache entry on miss.
    ///
    /// Metadata identity is checked here; callers decoding ranges remain
    /// responsible for verifying the selected chunk payloads.
    ///
    /// # Errors
    ///
    /// Returns an I/O or corruption error when an existing entry cannot be
    /// read as the requested content-addressed xorb. Invalid entries are
    /// evicted before the error is returned.
    pub async fn get_read_xorb_if_present(&self, hash: &MerkleHash) -> Result<Option<Bytes>> {
        let key = CacheKey::Xorb(*hash);
        let path = self.hash_path(&key);
        let Some(data) = read_file_bounded_result(&path, MAX_XORB_SIZE as u64).await? else {
            return Ok(None);
        };
        if let Err(error) = verify_xorb_identity(&data, hash) {
            warn!(
                path = %path.display(),
                expected = %hash.hex(),
                error = %error,
                "cached xorb identity mismatch — evicting"
            );
            let _ = self.evict(&key).await;
            return Err(error);
        }
        touch_mtime(&path).await;
        Ok(Some(data))
    }

    async fn try_read_key_limited(&self, key: &CacheKey, max_bytes: Option<u64>) -> Option<Bytes> {
        match key {
            CacheKey::Chunk(hash) | CacheKey::Shard(hash) => {
                self.try_read_verified_limited(&self.hash_path(key), hash, max_bytes)
                    .await
            }
            CacheKey::Xorb(hash) => {
                self.try_read_xorb_limited(&self.hash_path(key), hash, max_bytes)
                    .await
            }
            CacheKey::Stage(_) => {
                let path = self.hash_path(key);
                let data = match max_bytes {
                    Some(max_bytes) => read_file_bounded(&path, max_bytes).await?,
                    None => Bytes::from(tokio::fs::read(&path).await.ok()?),
                };
                touch_mtime(&path).await;
                Some(data)
            }
            CacheKey::Manifest { name, etag } => {
                let want_etag = etag.as_deref()?;
                let cached_etag = read_string_if_exists(&self.manifest_etag_path(name)).await?;
                if cached_etag.trim() != want_etag {
                    return None;
                }
                let path = self.manifest_data_path(name);
                let data = match max_bytes {
                    Some(max_bytes) => read_file_bounded(&path, max_bytes).await?,
                    None => Bytes::from(tokio::fs::read(&path).await.ok()?),
                };
                debug!(manifest = %name, "manifest cache hit (ETag match)");
                Some(data)
            }
        }
    }

    fn fill_lock(&self, path: &Path) -> &tokio::sync::Mutex<()> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        let stripe_count = u64::try_from(self.fill_locks.len()).unwrap_or(1);
        let index = usize::try_from(hasher.finish() % stripe_count).unwrap_or(0);
        &self.fill_locks[index]
    }

    /// Put an object into the cache directly.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on write failure.
    pub async fn put(&self, key: &CacheKey, data: &[u8]) -> Result<()> {
        Self::validate(key, data)?;
        match key {
            CacheKey::Chunk(_) | CacheKey::Shard(_) | CacheKey::Xorb(_) | CacheKey::Stage(_) => {
                let path = self.hash_path(key);
                self.atomic_write(&path, data).await?;
                if let CacheKey::Xorb(hash) = key {
                    let payload_hash = *blake3::hash(data).as_bytes();
                    self.index_cached_xorb_file_best_effort(
                        hash,
                        &path,
                        data.len() as u64,
                        payload_hash,
                    )
                    .await;
                }
                Ok(())
            }
            CacheKey::Manifest { name, etag } => {
                let data_path = self.manifest_data_path(name);
                self.atomic_write(&data_path, data).await?;
                if let Some(tag) = etag {
                    let etag_path = self.manifest_etag_path(name);
                    self.atomic_write(&etag_path, tag.as_bytes()).await?;
                }
                Ok(())
            }
        }
    }

    /// Put an object into the cache without copying an existing [`Bytes`] body.
    ///
    /// # Errors
    ///
    /// Returns a hash/corruption error when `data` does not match `key`, or
    /// [`CacheError::Io`] on write failure.
    pub async fn put_bytes(&self, key: &CacheKey, data: Bytes) -> Result<()> {
        Self::validate_bytes(key, &data)?;
        match key {
            CacheKey::Chunk(_) | CacheKey::Shard(_) | CacheKey::Xorb(_) | CacheKey::Stage(_) => {
                let path = self.hash_path(key);
                self.atomic_write(&path, &data).await?;
                if let CacheKey::Xorb(hash) = key {
                    let payload_hash = *blake3::hash(&data).as_bytes();
                    self.index_cached_xorb_file_best_effort(
                        hash,
                        &path,
                        data.len() as u64,
                        payload_hash,
                    )
                    .await;
                }
                Ok(())
            }
            CacheKey::Manifest { name, etag } => {
                let data_path = self.manifest_data_path(name);
                self.atomic_write(&data_path, &data).await?;
                if let Some(tag) = etag {
                    let etag_path = self.manifest_etag_path(name);
                    self.atomic_write(&etag_path, tag.as_bytes()).await?;
                }
                Ok(())
            }
        }
    }

    /// Put an existing xorb file into the cache without loading it whole.
    ///
    /// The copied tempfile is payload-verified before it becomes visible,
    /// so readers never observe partially-copied or corrupt cache entries.
    pub async fn put_xorb_file(
        &self,
        hash: &MerkleHash,
        source: &Path,
        expected_len: u64,
    ) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

        let path = self.hash_path(&CacheKey::Xorb(*hash));
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = path.with_extension(format!("tmp.{pid}.{seq}"));

        let install_result = async {
            let payload_hash = copy_xorb_temp_file_with_blake3(source, &tmp, expected_len).await?;
            verify_xorb_file_payload(&tmp, expected_len, hash).await?;
            tokio::fs::rename(&tmp, &path).await?;
            self.index_cached_xorb_file_best_effort(hash, &path, expected_len, payload_hash)
                .await;
            Ok(())
        }
        .await;
        if install_result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        install_result
    }

    /// Put an already verified xorb file into the cache without reparsing every chunk.
    ///
    /// `expected_blake3` must come from bytes that were already payload-verified
    /// against `hash`. This method still copies into an unpublished temp file,
    /// checks the copied bytes against that digest, and verifies xorb metadata
    /// identity before publishing the cache entry.
    pub async fn put_preverified_xorb_file(
        &self,
        hash: &MerkleHash,
        source: &Path,
        expected_len: u64,
        expected_blake3: [u8; 32],
    ) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

        let path = self.hash_path(&CacheKey::Xorb(*hash));
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = path.with_extension(format!("tmp.{pid}.{seq}"));

        let install_result = async {
            let actual_blake3 = copy_xorb_temp_file_with_blake3(source, &tmp, expected_len).await?;
            if actual_blake3 != expected_blake3 {
                return Err(CacheError::CorruptObject {
                    path: source.display().to_string(),
                    reason: "copied xorb payload digest did not match preverified digest"
                        .to_owned(),
                });
            }
            verify_xorb_file_identity(&tmp, expected_len, hash).await?;
            tokio::fs::rename(&tmp, &path).await?;
            self.index_cached_xorb_file_best_effort(hash, &path, expected_len, expected_blake3)
                .await;
            Ok(())
        }
        .await;
        if install_result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        install_result
    }

    /// Seed cache bytes without integrity checks for disk-corruption tests.
    #[doc(hidden)]
    pub async fn put_unchecked_for_test(&self, key: &CacheKey, data: &[u8]) -> Result<()> {
        let path = match key {
            CacheKey::Chunk(_) | CacheKey::Shard(_) | CacheKey::Xorb(_) | CacheKey::Stage(_) => {
                self.hash_path(key)
            }
            CacheKey::Manifest { name, .. } => self.manifest_data_path(name),
        };
        self.atomic_write(&path, data).await
    }

    /// Verify that `data` is valid for `key` without writing it.
    ///
    /// Manifest and stage entries are keyed by logical identities rather
    /// than body hashes, so this is a no-op for them.
    pub fn validate(key: &CacheKey, data: &[u8]) -> Result<()> {
        enforce_key_size(key, data.len() as u64)?;
        match key {
            CacheKey::Chunk(hash) | CacheKey::Shard(hash) => verify_data_hash(data, hash),
            CacheKey::Xorb(hash) => verify_xorb_payload(&Bytes::copy_from_slice(data), hash),
            CacheKey::Stage(_) | CacheKey::Manifest { .. } => Ok(()),
        }
    }

    /// Verify that `data` is valid for `key` without copying an existing body.
    pub fn validate_bytes(key: &CacheKey, data: &Bytes) -> Result<()> {
        enforce_key_size(key, data.len() as u64)?;
        match key {
            CacheKey::Chunk(hash) | CacheKey::Shard(hash) => verify_data_hash(data, hash),
            CacheKey::Xorb(hash) => verify_xorb_payload(data, hash),
            CacheKey::Stage(_) | CacheKey::Manifest { .. } => Ok(()),
        }
    }

    /// Check if a key exists in cache (does not verify hash).
    pub async fn contains(&self, key: &CacheKey) -> bool {
        let path = match key {
            CacheKey::Chunk(_) | CacheKey::Shard(_) | CacheKey::Xorb(_) | CacheKey::Stage(_) => {
                self.hash_path(key)
            }
            CacheKey::Manifest { name, .. } => self.manifest_data_path(name),
        };
        tokio::fs::metadata(&path).await.is_ok()
    }

    /// Return the size of an existing cache entry without reading its body.
    pub async fn cached_size(&self, key: &CacheKey) -> Result<Option<u64>> {
        let path = match key {
            CacheKey::Chunk(_) | CacheKey::Shard(_) | CacheKey::Xorb(_) | CacheKey::Stage(_) => {
                self.hash_path(key)
            }
            CacheKey::Manifest { name, .. } => self.manifest_data_path(name),
        };
        match tokio::fs::metadata(path).await {
            Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Check whether a cache entry exists and matches its key.
    ///
    /// For xorbs this verifies every compressed chunk payload, so callers
    /// should reserve it for paths where avoiding a stale cache entry matters
    /// more than the extra local read.
    pub async fn contains_verified(&self, key: &CacheKey) -> bool {
        match key {
            CacheKey::Chunk(hash) => self
                .try_read_verified_limited(&self.hash_path(key), hash, Some(MAX_CACHE_CHUNK_BYTES))
                .await
                .is_some(),
            CacheKey::Shard(hash) => self
                .try_read_verified_limited(&self.hash_path(key), hash, Some(MAX_CACHE_SHARD_BYTES))
                .await
                .is_some(),
            CacheKey::Xorb(hash) => {
                let path = self.hash_path(key);
                self.try_verify_xorb_payload_file(&path, hash).await
            }
            CacheKey::Stage(_) | CacheKey::Manifest { .. } => self.contains(key).await,
        }
    }

    /// Ensure an existing cached xorb has a local placement-index entry.
    ///
    /// Returns `false` when the xorb file is absent. This is an optimization
    /// hint for future adds; callers must still validate the xorb before
    /// referencing it in pushed metadata.
    pub async fn index_xorb_if_present(&self, hash: &MerkleHash) -> Result<bool> {
        let path = self.hash_path(&CacheKey::Xorb(*hash));
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !metadata.is_file() {
            return Ok(false);
        }

        verify_xorb_file_identity(&path, metadata.len(), hash).await?;
        let payload_hash = hash_file_blake3(&path).await?;
        self.index_cached_xorb_file(hash, &path, metadata.len(), payload_hash)
            .await?;
        Ok(true)
    }

    /// Return locally cached xorbs that may cover at least one requested chunk.
    ///
    /// The lookup is bounded by `chunk_hashes`; it does not scan the xorb cache.
    /// Stale index entries are ignored, and corrupt candidate files are evicted
    /// and treated as misses.
    pub async fn cached_xorb_candidates_for_chunks(
        &self,
        chunk_hashes: &[MerkleHash],
    ) -> Result<Vec<CachedXorbCandidate>> {
        if chunk_hashes.is_empty() {
            return Ok(Vec::new());
        }

        let records = query_xorb_index(&self.xorb_index_path(), chunk_hashes)?;
        let mut candidates = Vec::new();
        for record in records {
            match self.cached_xorb_candidate_from_record(record).await {
                Ok(Some(candidate)) => candidates.push(candidate),
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, "cached xorb candidate skipped");
                }
            }
        }
        Ok(candidates)
    }

    /// Check whether a previous origin payload proof still matches object metadata.
    ///
    /// The proof is usable only when the provider gives an immutable object
    /// identity token (`ETag` or version). Size must match as well, and any
    /// token recorded with the proof must still match the current HEAD.
    pub fn remote_xorb_proof_matches(
        &self,
        hash: &MerkleHash,
        payload_digest: &[u8; 32],
        xorb_bytes: u64,
        e_tag: Option<&str>,
        version: Option<&str>,
    ) -> Result<bool> {
        remote_xorb_proof_matches(
            &self.xorb_index_path(),
            hash,
            payload_digest,
            xorb_bytes,
            e_tag,
            version,
        )
    }

    /// Record a successful origin payload proof for a whole xorb.
    ///
    /// Returns `false` when the provider did not return an object identity
    /// token, because a size-only proof is not strong enough to reuse.
    pub fn record_remote_xorb_proof(
        &self,
        hash: &MerkleHash,
        payload_digest: &[u8; 32],
        xorb_bytes: u64,
        e_tag: Option<&str>,
        version: Option<&str>,
    ) -> Result<bool> {
        record_remote_xorb_proof(
            &self.xorb_index_path(),
            hash,
            payload_digest,
            xorb_bytes,
            e_tag,
            version,
        )
    }

    /// Return parsed remote xorb metadata when the object identity still matches.
    ///
    /// This is an optimization only. A miss means callers should read and parse
    /// the remote xorb metadata through the authoritative object store.
    pub fn cached_remote_xorb_index(
        &self,
        hash: &MerkleHash,
        xorb_bytes: u64,
        e_tag: Option<&str>,
        version: Option<&str>,
    ) -> Result<Option<CachedRemoteXorbIndex>> {
        cached_remote_xorb_index(&self.xorb_index_path(), hash, xorb_bytes, e_tag, version)
    }

    /// Record parsed remote xorb metadata under a strong object identity token.
    ///
    /// Returns `false` when the provider did not return an ETag or version.
    pub fn record_remote_xorb_index(
        &self,
        hash: &MerkleHash,
        payload_digest: &[u8; 32],
        xorb_bytes: u64,
        e_tag: Option<&str>,
        version: Option<&str>,
        chunks: &[ChunkMeta],
    ) -> Result<bool> {
        record_remote_xorb_index(
            &self.xorb_index_path(),
            hash,
            payload_digest,
            xorb_bytes,
            e_tag,
            version,
            chunks,
        )
    }

    /// Evict a cache entry if present.
    ///
    /// Missing entries are treated as success so verifier callsites can
    /// invalidate stale data without racing other cleanup paths.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] when the entry exists but cannot be removed.
    pub async fn evict(&self, key: &CacheKey) -> Result<()> {
        let path = match key {
            CacheKey::Chunk(_) | CacheKey::Shard(_) | CacheKey::Xorb(_) | CacheKey::Stage(_) => {
                self.hash_path(key)
            }
            CacheKey::Manifest { name, .. } => self.manifest_data_path(name),
        };
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if let CacheKey::Xorb(hash) = key
            && let Err(e) = remove_xorb_index_entries(&self.xorb_index_path(), hash)
        {
            warn!(
                xorb = %hash.hex(),
                error = %e,
                "local xorb cache index eviction failed"
            );
        }
        Ok(())
    }

    /// Return a byte range from a cached xorb without reading the whole file.
    ///
    /// Returns `None` on miss, invalid range, or local I/O error. Callers
    /// treat this as a cache miss and fall back to the remote object store.
    pub async fn get_xorb_range_if_present(
        &self,
        hash: &MerkleHash,
        range: std::ops::Range<u64>,
    ) -> Option<Bytes> {
        self.get_xorb_range_with_size_if_present(hash, range)
            .await
            .map(|(data, _)| data)
    }

    /// Return a byte range plus total size from a cached xorb.
    ///
    /// Returns `None` on miss, invalid range, or local I/O error. Callers
    /// treat this as a cache miss and fall back to the remote object store.
    pub async fn get_xorb_range_with_size_if_present(
        &self,
        hash: &MerkleHash,
        range: std::ops::Range<u64>,
    ) -> Option<(Bytes, u64)> {
        if range.start > range.end {
            return None;
        }
        let len = usize::try_from(range.end.checked_sub(range.start)?).ok()?;
        let path = self.hash_path(&CacheKey::Xorb(*hash));
        let meta = tokio::fs::metadata(&path).await.ok()?;
        if !meta.is_file() || meta.len() > MAX_XORB_SIZE as u64 || range.end > meta.len() {
            return None;
        }
        if let Err(e) = verify_xorb_file_identity(&path, meta.len(), hash).await {
            warn!(
                path = %path.display(),
                expected = %hash.hex(),
                error = %e,
                "cached xorb range identity check failed — evicting"
            );
            let _ = tokio::fs::remove_file(&path).await;
            return None;
        }
        let mut file = tokio::fs::File::open(&path).await.ok()?;
        file.seek(std::io::SeekFrom::Start(range.start))
            .await
            .ok()?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).await.ok()?;
        touch_mtime(&path).await;
        Some((Bytes::from(buf), meta.len()))
    }

    /// Return verified chunk metadata and payload length for a cached xorb.
    ///
    /// Invalid entries are evicted and returned as errors so callers can
    /// distinguish corruption repair from an ordinary cold miss.
    ///
    /// # Errors
    ///
    /// Returns an I/O or corruption error when an existing cache entry cannot
    /// provide valid metadata for the requested content address.
    pub async fn get_xorb_metadata_if_present(
        &self,
        hash: &MerkleHash,
    ) -> Result<Option<(Vec<ChunkMeta>, u64)>> {
        let path = self.hash_path(&CacheKey::Xorb(*hash));
        let meta = match tokio::fs::metadata(&path).await {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !meta.is_file() {
            return Err(CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: "cached xorb path is not a file".to_owned(),
            });
        }
        match read_xorb_file_metadata(&path, meta.len()).await {
            Ok((chunks, actual)) if actual == *hash => {
                let payload_len = chunks.last().map_or(0, |chunk| {
                    u64::from(chunk.offset) + u64::from(chunk.compressed_len)
                });
                touch_mtime(&path).await;
                Ok(Some((chunks, payload_len)))
            }
            Ok((_, actual)) => {
                warn!(
                    path = %path.display(),
                    expected = %hash.hex(),
                    actual = %actual.hex(),
                    "cached xorb metadata identity mismatch — evicting"
                );
                let _ = self.evict(&CacheKey::Xorb(*hash)).await;
                Err(CacheError::HashMismatch {
                    requested: hash.hex(),
                    actual: actual.hex(),
                })
            }
            Err(error) => {
                warn!(
                    path = %path.display(),
                    expected = %hash.hex(),
                    error = %error,
                    "cached xorb metadata validation failed — evicting"
                );
                let _ = self.evict(&CacheKey::Xorb(*hash)).await;
                Err(error)
            }
        }
    }

    /// Prune cache to configured limits using LRU eviction.
    ///
    /// Sorts cached files by mtime ascending and removes the oldest
    /// until the total size is within the configured byte budget.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on filesystem failure during eviction.
    pub async fn prune(&self) -> Result<PruneStats> {
        self.prune_with_options(PruneOptions::default()).await
    }

    /// Evict oldest cache objects until at least `target_bytes` have
    /// been reclaimed, returning the actual bytes removed.
    ///
    /// Eligible objects are chunks, xorbs, and shards. The returned
    /// byte count is based on file sizes observed immediately before
    /// deletion, so callers can compute exact before/after cache usage
    /// deltas.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on filesystem failure during
    /// eviction.
    pub async fn evict_bytes(&self, target_bytes: u64) -> Result<PruneStats> {
        if target_bytes == 0 {
            return Ok(PruneStats::default());
        }

        let mut entries: Vec<TargetLruEntry> = collect_hash_entries(&self.root.join("chunks"))
            .await?
            .into_iter()
            .map(|entry| TargetLruEntry {
                entry,
                kind: PruneObjectKind::Chunk,
            })
            .collect();
        entries.extend(
            collect_hash_entries(&self.root.join("xorbs"))
                .await?
                .into_iter()
                .map(|entry| TargetLruEntry {
                    entry,
                    kind: PruneObjectKind::Xorb,
                }),
        );
        entries.extend(
            collect_hash_entries(&self.root.join("shards"))
                .await?
                .into_iter()
                .map(|entry| TargetLruEntry {
                    entry,
                    kind: PruneObjectKind::Shard,
                }),
        );

        entries.sort_by_key(|entry| entry.entry.mtime);

        let mut stats = PruneStats::default();
        for entry in entries {
            if stats.bytes_freed >= target_bytes {
                break;
            }

            match tokio::fs::remove_file(&entry.entry.path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }

            match entry.kind {
                PruneObjectKind::Chunk => stats.chunks_evicted += 1,
                PruneObjectKind::Shard => stats.shards_evicted += 1,
                PruneObjectKind::Xorb => {
                    stats.xorbs_evicted += 1;
                    if let Some(hash) = merkle_hash_from_path(&entry.entry.path)
                        && let Err(e) = remove_xorb_index_entries(&self.xorb_index_path(), &hash)
                    {
                        warn!(
                            xorb = %hash.hex(),
                            error = %e,
                            "local xorb cache index targeted-evict cleanup failed"
                        );
                    }
                }
            }
            stats.bytes_freed = stats.bytes_freed.saturating_add(entry.entry.size);
        }

        Ok(stats)
    }

    /// Prune cache to configured limits using LRU eviction.
    ///
    /// When `dry_run` is true, returns the objects that would be evicted
    /// without deleting files.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on filesystem failure during eviction.
    pub async fn prune_with_options(&self, options: PruneOptions) -> Result<PruneStats> {
        let mut stats = PruneStats::default();

        let large_evicted = lru_evict_large_objects(
            &self.root.join("chunks"),
            &self.root.join("xorbs"),
            &self.xorb_index_path(),
            self.chunk_max_bytes,
            options,
        )
        .await?;
        stats.chunks_evicted = large_evicted.chunks;
        stats.xorbs_evicted = large_evicted.xorbs;
        stats.bytes_freed += large_evicted.bytes;
        stats.entries.extend(large_evicted.entries);

        if let Some(shard_max) = self.shard_max_bytes {
            let shard_evicted = lru_evict(
                &self.root.join("shards"),
                shard_max,
                PruneObjectKind::Shard,
                options,
            )
            .await?;
            stats.shards_evicted = shard_evicted.count;
            stats.bytes_freed += shard_evicted.bytes;
            stats.entries.extend(shard_evicted.entries);
        }

        debug!(
            chunks_evicted = stats.chunks_evicted,
            shards_evicted = stats.shards_evicted,
            xorbs_evicted = stats.xorbs_evicted,
            bytes_freed = stats.bytes_freed,
            "cache prune complete"
        );
        Ok(stats)
    }

    /// Verify all cached chunks, shards, and xorbs.
    ///
    /// Corrupt entries are evicted. Manifests are skipped (no content hash).
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on filesystem failure.
    pub async fn verify(&self) -> Result<VerifyReport> {
        let mut report = VerifyReport::default();
        verify_dir(
            &self.root.join("chunks"),
            &mut report,
            Some(MAX_CACHE_CHUNK_BYTES),
        )
        .await?;
        verify_dir(
            &self.root.join("shards"),
            &mut report,
            Some(MAX_CACHE_SHARD_BYTES),
        )
        .await?;
        verify_xorb_dir(
            &self.root.join("xorbs"),
            &self.xorb_index_path(),
            &mut report,
        )
        .await?;
        debug!(
            total = report.total,
            valid = report.valid,
            corrupt = report.corrupt,
            "cache verify complete"
        );
        Ok(report)
    }

    /// Get cache statistics.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on filesystem failure.
    pub async fn stats(&self) -> Result<CacheStats> {
        let mut s = CacheStats::default();
        let (cb, cc) = dir_size(&self.root.join("chunks")).await?;
        s.chunk_bytes = cb;
        s.chunk_count = cc;
        let (sb, sc) = dir_size(&self.root.join("shards")).await?;
        s.shard_bytes = sb;
        s.shard_count = sc;
        let (xb, xc) = dir_size(&self.root.join("xorbs")).await?;
        s.xorb_bytes = xb;
        s.xorb_count = xc;
        let (tb, tc) = dir_size(&self.root.join("stages")).await?;
        s.stage_bytes = tb;
        s.stage_count = tc;
        s.manifest_count = count_manifests(&self.root.join("manifests")).await?;
        Ok(s)
    }

    /// Remove all cached data.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] on filesystem failure.
    pub async fn clean(&self) -> Result<()> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                tokio::fs::remove_dir_all(&path).await?;
            } else {
                tokio::fs::remove_file(&path).await?;
            }
        }
        debug!("cache cleaned");
        Ok(())
    }

    async fn index_cached_xorb_file(
        &self,
        hash: &MerkleHash,
        path: &Path,
        expected_len: u64,
        payload_hash: [u8; 32],
    ) -> Result<()> {
        let (chunks, actual) = read_xorb_file_metadata(path, expected_len).await?;
        if actual != *hash {
            return Err(CacheError::HashMismatch {
                requested: hash.hex(),
                actual: actual.hex(),
            });
        }

        let mut entries = Vec::with_capacity(chunks.len());
        let xorb_hash: [u8; 32] = (*hash).into();
        for (idx, chunk) in chunks.iter().enumerate() {
            let chunk_index = u32::try_from(idx).map_err(|_| CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: "xorb chunk index does not fit u32".to_owned(),
            })?;
            entries.push(XorbIndexEntry {
                chunk_hash: chunk.hash.into(),
                xorb_hash,
                chunk_index,
                uncompressed_size: chunk.uncompressed_len,
                xorb_bytes: expected_len,
                payload_hash,
            });
        }
        let index_path = self.xorb_index_path();
        let write_lock = Arc::clone(&self.xorb_index_write_lock);
        tokio::task::spawn_blocking(move || {
            let _guard = write_lock.lock().map_err(|_| {
                CacheError::Internal("local xorb index writer lock poisoned".to_owned())
            })?;
            write_xorb_index_entries(&index_path, &entries)
        })
        .await
        .map_err(|e| CacheError::Internal(format!("local xorb index writer task failed: {e}")))?
    }

    async fn index_cached_xorb_file_best_effort(
        &self,
        hash: &MerkleHash,
        path: &Path,
        expected_len: u64,
        payload_hash: [u8; 32],
    ) {
        if let Err(e) = self
            .index_cached_xorb_file(hash, path, expected_len, payload_hash)
            .await
        {
            warn!(
                xorb = %hash.hex(),
                error = %e,
                "local xorb cache index update failed"
            );
        }
    }

    async fn cached_xorb_candidate_from_record(
        &self,
        record: IndexedXorbRecord,
    ) -> Result<Option<CachedXorbCandidate>> {
        let path = self.hash_path(&CacheKey::Xorb(record.xorb_hash));
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !metadata.is_file() || metadata.len() != record.xorb_bytes {
            return Ok(None);
        }

        let (chunks, actual) = match read_xorb_file_metadata(&path, metadata.len()).await {
            Ok(parsed) => parsed,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "cached xorb metadata check failed — evicting"
                );
                let _ = tokio::fs::remove_file(&path).await;
                remove_xorb_index_entries(&self.xorb_index_path(), &record.xorb_hash)?;
                return Ok(None);
            }
        };
        if actual != record.xorb_hash {
            warn!(
                path = %path.display(),
                expected = %record.xorb_hash.hex(),
                actual = %actual.hex(),
                "cached xorb identity mismatch — evicting"
            );
            let _ = tokio::fs::remove_file(&path).await;
            remove_xorb_index_entries(&self.xorb_index_path(), &record.xorb_hash)?;
            return Ok(None);
        }

        let mut placements = Vec::with_capacity(chunks.len());
        for (idx, chunk) in chunks.iter().enumerate() {
            placements.push(ChunkPlacement {
                chunk_hash: chunk.hash,
                xorb_hash: record.xorb_hash,
                chunk_index: u32::try_from(idx).map_err(|_| CacheError::CorruptObject {
                    path: path.display().to_string(),
                    reason: "xorb chunk index does not fit u32".to_owned(),
                })?,
                uncompressed_size: chunk.uncompressed_len,
            });
        }

        touch_mtime(&path).await;
        Ok(Some(CachedXorbCandidate {
            xorb_hash: record.xorb_hash,
            path,
            bytes: record.xorb_bytes,
            payload_hash: record.payload_hash,
            placements,
        }))
    }

    // --- private helpers ---

    fn xorb_index_path(&self) -> PathBuf {
        self.root.join(XORB_INDEX_DIR).join(XORB_INDEX_DB)
    }

    /// Resolve the on-disk path for a hash-keyed entry.
    fn hash_path(&self, key: &CacheKey) -> PathBuf {
        match key {
            CacheKey::Chunk(h) => self.merkle_path("chunks", h),
            CacheKey::Shard(h) => self.merkle_path("shards", h),
            CacheKey::Xorb(h) => self.merkle_path("xorbs", h),
            CacheKey::Stage(h) => {
                let hex = h.as_hex();
                self.root.join("stages").join(&hex[..2]).join(&hex)
            }
            CacheKey::Manifest { name, .. } => self.manifest_data_path(name),
        }
    }

    fn merkle_path(&self, dir: &str, hash: &MerkleHash) -> PathBuf {
        let hex = hash.hex();
        self.root.join(dir).join(&hex[..2]).join(&hex)
    }

    fn manifest_data_path(&self, name: &str) -> PathBuf {
        self.root.join("manifests").join(format!("{name}.json"))
    }

    fn manifest_etag_path(&self, name: &str) -> PathBuf {
        self.root.join("manifests").join(format!("{name}.etag"))
    }

    /// Read the cached ETag for a manifest, if any.
    pub async fn cached_manifest_etag(&self, name: &str) -> Option<String> {
        read_string_if_exists(&self.manifest_etag_path(name)).await
    }

    async fn try_read_verified_limited(
        &self,
        path: &Path,
        expected: &MerkleHash,
        max_bytes: Option<u64>,
    ) -> Option<Bytes> {
        let max_bytes = max_bytes?;
        let data = read_file_bounded(path, max_bytes).await?;
        if compute_data_hash(&data) == *expected {
            touch_mtime(path).await;
            return Some(data);
        }

        warn!(
            path = %path.display(),
            expected = %expected.hex(),
            "cache hash mismatch — evicting"
        );
        let _ = tokio::fs::remove_file(path).await;
        None
    }

    async fn try_read_xorb_limited(
        &self,
        path: &Path,
        expected: &MerkleHash,
        max_bytes: Option<u64>,
    ) -> Option<Bytes> {
        let data = self
            .try_read_xorb_bytes_limited(path, expected, max_bytes)
            .await?;
        self.finish_xorb_cache_read(path, expected, data).await
    }

    async fn finish_xorb_cache_read(
        &self,
        path: &Path,
        expected: &MerkleHash,
        data: Bytes,
    ) -> Option<Bytes> {
        let index_path = self.xorb_index_path();
        let expected_hash = *expected;
        let expected_len = data.len() as u64;
        let indexed = tokio::task::spawn_blocking(move || {
            indexed_xorb_install_matches(&index_path, &expected_hash, expected_len)
        })
        .await
        .ok()
        .and_then(std::result::Result::ok)
        .unwrap_or(false);
        if indexed {
            return Some(data);
        }
        let payload_hash = *blake3::hash(&data).as_bytes();
        self.index_cached_xorb_file_best_effort(expected, path, data.len() as u64, payload_hash)
            .await;
        Some(data)
    }

    async fn try_read_xorb_bytes(&self, path: &Path, expected: &MerkleHash) -> Option<Bytes> {
        self.try_read_xorb_bytes_limited(path, expected, Some(MAX_XORB_SIZE as u64))
            .await
    }

    async fn try_read_xorb_bytes_limited(
        &self,
        path: &Path,
        expected: &MerkleHash,
        max_bytes: Option<u64>,
    ) -> Option<Bytes> {
        let max_bytes = max_bytes
            .unwrap_or(MAX_XORB_SIZE as u64)
            .min(MAX_XORB_SIZE as u64);
        let data = read_file_bounded(path, max_bytes).await?;

        if verify_xorb_identity(&data, expected).is_ok() {
            touch_mtime(path).await;
            return Some(data);
        }

        warn!(
            path = %path.display(),
            expected = %expected.hex(),
            "cached xorb identity mismatch — evicting"
        );
        let _ = tokio::fs::remove_file(path).await;
        let _ = remove_xorb_index_entries(&self.xorb_index_path(), expected);
        None
    }

    async fn try_verify_xorb_payload_file(&self, path: &Path, expected: &MerkleHash) -> bool {
        let Ok(meta) = tokio::fs::metadata(path).await else {
            return false;
        };
        if verify_xorb_file_payload(path, meta.len(), expected)
            .await
            .is_ok()
        {
            touch_mtime(path).await;
            return true;
        }

        warn!(
            path = %path.display(),
            expected = %expected.hex(),
            "cached xorb payload check failed — evicting"
        );
        let _ = tokio::fs::remove_file(path).await;
        let _ = remove_xorb_index_entries(&self.xorb_index_path(), expected);
        false
    }

    /// Atomically write `data` to `path` via tempfile + rename.
    ///
    /// Uses a unique per-call tempfile suffix (PID + monotonic counter)
    /// so concurrent writers for the same cache key don't clobber each
    /// other's in-flight tempfile. One writer wins the rename; the
    /// losers' tempfiles are cleaned up (best-effort) and the file on
    /// disk reflects the last successful rename.
    async fn atomic_write(&self, path: &Path, data: &[u8]) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = path.with_extension(format!("tmp.{pid}.{seq}"));
        let mut f = tokio::fs::File::create(&tmp).await?;
        let write_result = async {
            f.write_all(data).await?;
            f.flush().await?;
            // Drop the handle before renaming so any pending writes are
            // flushed to the OS on all supported platforms.
            drop(f);
            tokio::fs::rename(&tmp, path).await?;
            Ok::<_, std::io::Error>(())
        }
        .await;
        if let Err(e) = write_result {
            // Best-effort cleanup; ignore secondary errors.
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        Ok(())
    }
}

// --- free-standing helpers ---

fn enforce_size_limit(
    key: &CacheKey,
    data: &Bytes,
    max_bytes: Option<u64>,
) -> std::result::Result<(), CacheError> {
    let Some(max_bytes) = max_bytes else {
        return Ok(());
    };
    let actual = data.len() as u64;
    if actual <= max_bytes {
        return Ok(());
    }
    Err(CacheError::CorruptObject {
        path: format!("{key:?}"),
        reason: format!(
            "object is {actual} bytes; bounded read supports at most {max_bytes} bytes"
        ),
    })
}

fn enforce_key_size(key: &CacheKey, actual: u64) -> std::result::Result<(), CacheError> {
    let Some(max_bytes) = effective_read_limit(key, None) else {
        return Ok(());
    };
    if actual <= max_bytes {
        return Ok(());
    }
    Err(CacheError::CorruptObject {
        path: format!("{key:?}"),
        reason: format!(
            "object is {actual} bytes; cache format supports at most {max_bytes} bytes"
        ),
    })
}

fn effective_read_limit(key: &CacheKey, requested: Option<u64>) -> Option<u64> {
    let format_limit = match key {
        CacheKey::Shard(_) => Some(MAX_CACHE_SHARD_BYTES),
        CacheKey::Xorb(_) => Some(MAX_XORB_SIZE as u64),
        CacheKey::Chunk(_) => Some(MAX_CACHE_CHUNK_BYTES),
        CacheKey::Stage(_) => Some(MAX_CACHE_STAGE_BYTES),
        CacheKey::Manifest { .. } => Some(MAX_CACHE_MANIFEST_BYTES),
    };
    match (requested, format_limit) {
        (Some(requested), Some(format_limit)) => Some(requested.min(format_limit)),
        (None, Some(format_limit)) => Some(format_limit),
        (requested, None) => requested,
    }
}

async fn read_file_bounded(path: &Path, max_bytes: u64) -> Option<Bytes> {
    read_file_bounded_result(path, max_bytes)
        .await
        .ok()
        .flatten()
}

async fn read_file_bounded_result(path: &Path, max_bytes: u64) -> Result<Option<Bytes>> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() > max_bytes {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!(
                "file is {} bytes; bounded read supports at most {max_bytes} bytes",
                metadata.len()
            ),
        });
    }
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut data = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut data)
        .await?;
    if data.len() as u64 > max_bytes {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!(
                "file grew beyond the bounded read limit of {max_bytes} bytes while reading"
            ),
        });
    }
    Ok(Some(Bytes::from(data)))
}

fn verify_data_hash(data: &[u8], expected: &MerkleHash) -> Result<()> {
    let actual = compute_data_hash(data);
    if actual == *expected {
        return Ok(());
    }
    Err(CacheError::HashMismatch {
        requested: expected.hex(),
        actual: actual.hex(),
    })
}

async fn copy_xorb_temp_file_with_blake3(
    source: &Path,
    tmp: &Path,
    expected_len: u64,
) -> Result<[u8; 32]> {
    if expected_len > MAX_XORB_SIZE as u64 {
        return Err(CacheError::CorruptObject {
            path: source.display().to_string(),
            reason: format!("xorb is {expected_len} bytes; format limit is {MAX_XORB_SIZE} bytes"),
        });
    }
    let mut source_file = tokio::fs::File::open(source).await?;
    let mut tmp_file = tokio::fs::File::create(tmp).await?;
    let mut hasher = blake3::Hasher::new();
    let mut copied = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = source_file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        tmp_file.write_all(chunk).await?;
        hasher.update(chunk);
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| CacheError::CorruptObject {
                path: source.display().to_string(),
                reason: "copied xorb byte count overflowed".to_owned(),
            })?;
    }
    tmp_file.flush().await?;
    drop(tmp_file);

    if copied == expected_len {
        return Ok(*hasher.finalize().as_bytes());
    }
    Err(CacheError::CorruptObject {
        path: source.display().to_string(),
        reason: format!("expected {expected_len} bytes, copied {copied}"),
    })
}

fn verify_xorb_identity(data: &Bytes, expected: &MerkleHash) -> Result<()> {
    let parser = XorbParser::parse(data.clone())?;
    let actual = parser.hash();
    if actual == *expected {
        return Ok(());
    }
    Err(CacheError::HashMismatch {
        requested: expected.hex(),
        actual: actual.hex(),
    })
}

fn verify_xorb_payload(data: &Bytes, expected: &MerkleHash) -> Result<()> {
    let parser = verify_xorb_serialized_payload(data, expected)?;
    Ok(parser.verify_all_chunks()?)
}

fn verify_xorb_serialized_payload(data: &Bytes, expected: &MerkleHash) -> Result<XorbParser> {
    let parser = XorbParser::parse(data.clone())?;
    let actual = parser.hash();
    if actual != *expected {
        return Err(CacheError::HashMismatch {
            requested: expected.hex(),
            actual: actual.hex(),
        });
    }
    parser.verify_payload_digest()?;
    Ok(parser)
}

async fn verify_xorb_file_identity(
    path: &Path,
    file_len: u64,
    expected: &MerkleHash,
) -> Result<()> {
    if file_len > MAX_XORB_SIZE as u64 {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!("xorb is {file_len} bytes; format limit is {MAX_XORB_SIZE} bytes"),
        });
    }
    let file_len = usize::try_from(file_len).map_err(|_| CacheError::CorruptObject {
        path: path.display().to_string(),
        reason: "xorb file length does not fit usize".to_string(),
    })?;
    if file_len < FOOTER_SIZE {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: "xorb too small for footer".to_string(),
        });
    }

    let mut file = tokio::fs::File::open(path).await?;
    let footer_start =
        u64::try_from(file_len - FOOTER_SIZE).map_err(|_| CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: "xorb footer offset does not fit u64".to_string(),
        })?;
    file.seek(std::io::SeekFrom::Start(footer_start)).await?;
    let mut footer = vec![0u8; FOOTER_SIZE];
    file.read_exact(&mut footer).await?;

    let region = xorb_metadata_region(file_len, &footer)?;
    let offset = u64::try_from(region.offset).map_err(|_| CacheError::CorruptObject {
        path: path.display().to_string(),
        reason: "xorb metadata offset does not fit u64".to_string(),
    })?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut metadata = vec![0u8; region.len];
    file.read_exact(&mut metadata).await?;

    let actual = xorb_hash_from_metadata(file_len, &footer, &metadata)?;
    if actual == *expected {
        return Ok(());
    }

    Err(CacheError::HashMismatch {
        requested: expected.hex(),
        actual: actual.hex(),
    })
}

async fn verify_xorb_file_payload(path: &Path, file_len: u64, expected: &MerkleHash) -> Result<()> {
    let current_len = tokio::fs::metadata(path).await?.len();
    if current_len != file_len {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!(
                "xorb changed during verification: indexed size {file_len}, current size {current_len}"
            ),
        });
    }
    let (chunks, actual) = read_xorb_file_metadata(path, file_len).await?;
    if actual != *expected {
        return Err(CacheError::HashMismatch {
            requested: expected.hex(),
            actual: actual.hex(),
        });
    }

    let mut file = tokio::fs::File::open(path).await?;
    for chunk in &chunks {
        let compressed = read_compressed_xorb_chunk(&mut file, chunk).await?;
        verify_compressed_chunk(chunk, &compressed)?;
    }
    let current_len = tokio::fs::metadata(path).await?.len();
    if current_len != file_len {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!(
                "xorb changed during verification: indexed size {file_len}, current size {current_len}"
            ),
        });
    }
    Ok(())
}

async fn read_xorb_file_metadata(
    path: &Path,
    file_len: u64,
) -> Result<(Vec<ChunkMeta>, MerkleHash)> {
    if file_len > MAX_XORB_SIZE as u64 {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!("xorb is {file_len} bytes; format limit is {MAX_XORB_SIZE} bytes"),
        });
    }
    let file_len = usize::try_from(file_len).map_err(|_| CacheError::CorruptObject {
        path: path.display().to_string(),
        reason: "xorb file length does not fit usize".to_string(),
    })?;
    if file_len < FOOTER_SIZE {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: "xorb too small for footer".to_string(),
        });
    }

    let mut file = tokio::fs::File::open(path).await?;
    let footer_start =
        u64::try_from(file_len - FOOTER_SIZE).map_err(|_| CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: "xorb footer offset does not fit u64".to_string(),
        })?;
    file.seek(std::io::SeekFrom::Start(footer_start)).await?;
    let mut footer = vec![0u8; FOOTER_SIZE];
    file.read_exact(&mut footer).await?;

    let region = xorb_metadata_region(file_len, &footer)?;
    let offset = u64::try_from(region.offset).map_err(|_| CacheError::CorruptObject {
        path: path.display().to_string(),
        reason: "xorb metadata offset does not fit u64".to_string(),
    })?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut metadata = vec![0u8; region.len];
    file.read_exact(&mut metadata).await?;

    Ok(xorb_chunks_from_metadata(file_len, &footer, &metadata)?)
}

async fn read_compressed_xorb_chunk(
    file: &mut tokio::fs::File,
    chunk: &ChunkMeta,
) -> Result<Vec<u8>> {
    file.seek(std::io::SeekFrom::Start(u64::from(chunk.offset)))
        .await?;
    let mut compressed = vec![0u8; chunk.compressed_len as usize];
    file.read_exact(&mut compressed).await?;
    Ok(compressed)
}

async fn hash_file_blake3(path: &Path) -> Result<[u8; 32]> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(*hasher.finalize().as_bytes())
}

#[derive(Debug, Clone)]
struct XorbIndexEntry {
    chunk_hash: [u8; 32],
    xorb_hash: [u8; 32],
    chunk_index: u32,
    uncompressed_size: u32,
    xorb_bytes: u64,
    payload_hash: [u8; 32],
}

#[derive(Debug, Clone)]
struct IndexedXorbRecord {
    xorb_hash: MerkleHash,
    xorb_bytes: u64,
    payload_hash: [u8; 32],
}

fn write_xorb_index_entries(index_path: &Path, entries: &[XorbIndexEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut conn = open_xorb_index(index_path)?;
    let tx = conn
        .transaction()
        .map_err(|source| cache_index_error(index_path, source))?;
    {
        let mut stmt = tx
            .prepare(
                "INSERT OR REPLACE INTO xorb_index
                 (chunk_hash, xorb_hash, chunk_index, uncompressed_size, xorb_bytes, payload_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|source| cache_index_error(index_path, source))?;
        for entry in entries {
            let xorb_bytes =
                i64::try_from(entry.xorb_bytes).map_err(|_| CacheError::CorruptObject {
                    path: index_path.display().to_string(),
                    reason: "cached xorb byte count does not fit sqlite integer".to_owned(),
                })?;
            stmt.execute(params![
                entry.chunk_hash.as_slice(),
                entry.xorb_hash.as_slice(),
                i64::from(entry.chunk_index),
                i64::from(entry.uncompressed_size),
                xorb_bytes,
                entry.payload_hash.as_slice(),
            ])
            .map_err(|source| cache_index_error(index_path, source))?;
        }
    }
    tx.commit()
        .map_err(|source| cache_index_error(index_path, source))?;
    Ok(())
}

fn query_xorb_index(
    index_path: &Path,
    chunk_hashes: &[MerkleHash],
) -> Result<Vec<IndexedXorbRecord>> {
    if !index_path.exists() {
        return Ok(Vec::new());
    }

    let conn = open_xorb_index(index_path)?;
    let mut stmt = conn
        .prepare(
            "SELECT xorb_hash, xorb_bytes, payload_hash
             FROM xorb_index
             WHERE chunk_hash = ?1",
        )
        .map_err(|source| cache_index_error(index_path, source))?;
    let mut seen_xorbs = HashSet::new();
    let mut records = Vec::new();
    for chunk_hash in chunk_hashes {
        let chunk_hash_bytes: [u8; 32] = (*chunk_hash).into();
        let row = stmt
            .query_row(params![chunk_hash_bytes.as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .optional()
            .map_err(|source| cache_index_error(index_path, source))?;
        let Some((xorb_hash, xorb_bytes, payload_hash)) = row else {
            continue;
        };
        let Some(xorb_hash) = decode_fixed_hash(&xorb_hash) else {
            continue;
        };
        let Some(payload_hash) = decode_fixed_hash(&payload_hash) else {
            continue;
        };
        let Ok(xorb_bytes) = u64::try_from(xorb_bytes) else {
            continue;
        };
        if seen_xorbs.insert(xorb_hash) {
            records.push(IndexedXorbRecord {
                xorb_hash: MerkleHash::from(xorb_hash),
                xorb_bytes,
                payload_hash,
            });
        }
    }
    Ok(records)
}

fn indexed_xorb_install_matches(
    index_path: &Path,
    xorb_hash: &MerkleHash,
    expected_bytes: u64,
) -> Result<bool> {
    if !index_path.exists() {
        return Ok(false);
    }
    let conn = open_xorb_index(index_path)?;
    let xorb_hash: [u8; 32] = (*xorb_hash).into();
    let stored_bytes = conn
        .query_row(
            "SELECT xorb_bytes FROM xorb_index WHERE xorb_hash = ?1 LIMIT 1",
            params![xorb_hash.as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|source| cache_index_error(index_path, source))?;
    Ok(stored_bytes.and_then(|bytes| u64::try_from(bytes).ok()) == Some(expected_bytes))
}

fn remove_xorb_index_entries(index_path: &Path, xorb_hash: &MerkleHash) -> Result<()> {
    if !index_path.exists() {
        return Ok(());
    }
    let conn = open_xorb_index(index_path)?;
    let xorb_hash: [u8; 32] = (*xorb_hash).into();
    conn.execute(
        "DELETE FROM xorb_index WHERE xorb_hash = ?1",
        params![xorb_hash.as_slice()],
    )
    .map_err(|source| cache_index_error(index_path, source))?;
    Ok(())
}

fn record_remote_xorb_proof(
    index_path: &Path,
    hash: &MerkleHash,
    payload_digest: &[u8; 32],
    xorb_bytes: u64,
    e_tag: Option<&str>,
    version: Option<&str>,
) -> Result<bool> {
    if e_tag.is_none() && version.is_none() {
        return Ok(false);
    }

    let xorb_bytes = i64::try_from(xorb_bytes).map_err(|_| CacheError::CorruptObject {
        path: index_path.display().to_string(),
        reason: "remote xorb proof byte count does not fit sqlite integer".to_owned(),
    })?;
    let conn = open_xorb_index(index_path)?;
    let xorb_hash: [u8; 32] = (*hash).into();
    conn.execute(
        "INSERT OR REPLACE INTO remote_xorb_proof
         (xorb_hash, payload_digest, xorb_bytes, e_tag, version)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            xorb_hash.as_slice(),
            payload_digest.as_slice(),
            xorb_bytes,
            e_tag,
            version
        ],
    )
    .map_err(|source| cache_index_error(index_path, source))?;
    Ok(true)
}

fn remote_xorb_proof_matches(
    index_path: &Path,
    hash: &MerkleHash,
    payload_digest: &[u8; 32],
    xorb_bytes: u64,
    e_tag: Option<&str>,
    version: Option<&str>,
) -> Result<bool> {
    if e_tag.is_none() && version.is_none() {
        return Ok(false);
    }
    if !index_path.exists() {
        return Ok(false);
    }

    let conn = open_xorb_index(index_path)?;
    let xorb_hash: [u8; 32] = (*hash).into();
    let row = conn
        .query_row(
            "SELECT payload_digest, xorb_bytes, e_tag, version
             FROM remote_xorb_proof
             WHERE xorb_hash = ?1",
            params![xorb_hash.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|source| cache_index_error(index_path, source))?;
    let Some((stored_digest, stored_bytes, stored_e_tag, stored_version)) = row else {
        return Ok(false);
    };
    if stored_digest.as_slice() != payload_digest {
        return Ok(false);
    }
    let Ok(stored_bytes) = u64::try_from(stored_bytes) else {
        return Ok(false);
    };
    if stored_bytes != xorb_bytes {
        return Ok(false);
    }

    if let Some(stored) = stored_e_tag.as_deref()
        && e_tag != Some(stored)
    {
        return Ok(false);
    }
    if let Some(stored) = stored_version.as_deref()
        && version != Some(stored)
    {
        return Ok(false);
    }

    Ok(stored_e_tag.is_some() || stored_version.is_some())
}

fn record_remote_xorb_index(
    index_path: &Path,
    hash: &MerkleHash,
    payload_digest: &[u8; 32],
    xorb_bytes: u64,
    e_tag: Option<&str>,
    version: Option<&str>,
    chunks: &[ChunkMeta],
) -> Result<bool> {
    if e_tag.is_none() && version.is_none() {
        return Ok(false);
    }

    let xorb_bytes = i64::try_from(xorb_bytes).map_err(|_| CacheError::CorruptObject {
        path: index_path.display().to_string(),
        reason: "remote xorb index byte count does not fit sqlite integer".to_owned(),
    })?;
    let mut conn = open_xorb_index(index_path)?;
    let tx = conn
        .transaction()
        .map_err(|source| cache_index_error(index_path, source))?;
    let xorb_hash: [u8; 32] = (*hash).into();
    tx.execute(
        "DELETE FROM remote_xorb_index WHERE xorb_hash = ?1",
        params![xorb_hash.as_slice()],
    )
    .map_err(|source| cache_index_error(index_path, source))?;
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO remote_xorb_index
                 (xorb_hash, payload_digest, xorb_bytes, e_tag, version, chunk_index, chunk_hash,
                  chunk_offset, compressed_len, uncompressed_len, scheme)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .map_err(|source| cache_index_error(index_path, source))?;
        for (chunk_index, chunk) in chunks.iter().enumerate() {
            let chunk_index =
                i64::try_from(chunk_index).map_err(|_| CacheError::CorruptObject {
                    path: index_path.display().to_string(),
                    reason: "remote xorb index chunk count does not fit sqlite integer".to_owned(),
                })?;
            let chunk_hash: [u8; 32] = chunk.hash.into();
            stmt.execute(params![
                xorb_hash.as_slice(),
                payload_digest.as_slice(),
                xorb_bytes,
                e_tag,
                version,
                chunk_index,
                chunk_hash.as_slice(),
                i64::from(chunk.offset),
                i64::from(chunk.compressed_len),
                i64::from(chunk.uncompressed_len),
                i64::from(chunk.scheme as u8),
            ])
            .map_err(|source| cache_index_error(index_path, source))?;
        }
    }
    tx.commit()
        .map_err(|source| cache_index_error(index_path, source))?;
    Ok(true)
}

fn cached_remote_xorb_index(
    index_path: &Path,
    hash: &MerkleHash,
    xorb_bytes: u64,
    e_tag: Option<&str>,
    version: Option<&str>,
) -> Result<Option<CachedRemoteXorbIndex>> {
    if e_tag.is_none() && version.is_none() {
        return Ok(None);
    }
    if !index_path.exists() {
        return Ok(None);
    }

    let conn = open_xorb_index(index_path)?;
    let xorb_hash_bytes: [u8; 32] = (*hash).into();
    let mut stmt = conn
        .prepare(
            "SELECT xorb_bytes, e_tag, version, chunk_index, chunk_hash,
                    chunk_offset, compressed_len, uncompressed_len, scheme, payload_digest
             FROM remote_xorb_index
             WHERE xorb_hash = ?1
             ORDER BY chunk_index ASC",
        )
        .map_err(|source| cache_index_error(index_path, source))?;
    let mut rows = stmt
        .query(params![xorb_hash_bytes.as_slice()])
        .map_err(|source| cache_index_error(index_path, source))?;

    let mut chunks = Vec::new();
    let mut expected_index = 0i64;
    let mut stored_payload_digest = None;
    while let Some(row) = rows
        .next()
        .map_err(|source| cache_index_error(index_path, source))?
    {
        let stored_bytes = row
            .get::<_, i64>(0)
            .map_err(|source| cache_index_error(index_path, source))?;
        let stored_e_tag = row
            .get::<_, Option<String>>(1)
            .map_err(|source| cache_index_error(index_path, source))?;
        let stored_version = row
            .get::<_, Option<String>>(2)
            .map_err(|source| cache_index_error(index_path, source))?;
        let chunk_index = row
            .get::<_, i64>(3)
            .map_err(|source| cache_index_error(index_path, source))?;
        let chunk_hash = row
            .get::<_, Vec<u8>>(4)
            .map_err(|source| cache_index_error(index_path, source))?;
        let offset = row
            .get::<_, i64>(5)
            .map_err(|source| cache_index_error(index_path, source))?;
        let compressed_len = row
            .get::<_, i64>(6)
            .map_err(|source| cache_index_error(index_path, source))?;
        let uncompressed_len = row
            .get::<_, i64>(7)
            .map_err(|source| cache_index_error(index_path, source))?;
        let scheme = row
            .get::<_, i64>(8)
            .map_err(|source| cache_index_error(index_path, source))?;
        let payload_digest = row
            .get::<_, Vec<u8>>(9)
            .map_err(|source| cache_index_error(index_path, source))?;

        let Ok(stored_bytes) = u64::try_from(stored_bytes) else {
            return Ok(None);
        };
        if stored_bytes != xorb_bytes
            || !identity_tokens_match(
                stored_e_tag.as_deref(),
                stored_version.as_deref(),
                e_tag,
                version,
            )
            || chunk_index != expected_index
        {
            return Ok(None);
        }
        let Some(chunk_hash) = decode_fixed_hash(&chunk_hash) else {
            return Ok(None);
        };
        let Some(payload_digest) = decode_fixed_hash(&payload_digest) else {
            return Ok(None);
        };
        let (Ok(offset), Ok(compressed_len), Ok(uncompressed_len), Ok(scheme)) = (
            u32::try_from(offset),
            u32::try_from(compressed_len),
            u32::try_from(uncompressed_len),
            u8::try_from(scheme),
        ) else {
            return Ok(None);
        };
        let Ok(scheme) = crab_xet::xorb::format::CompressionScheme::try_from(scheme) else {
            return Ok(None);
        };
        if stored_payload_digest.is_some_and(|stored| stored != payload_digest) {
            return Ok(None);
        }
        stored_payload_digest = Some(payload_digest);
        chunks.push(ChunkMeta {
            hash: MerkleHash::from(chunk_hash),
            offset,
            compressed_len,
            uncompressed_len,
            scheme,
        });
        expected_index += 1;
    }

    if chunks.is_empty() {
        return Ok(None);
    }
    let Some(payload_digest) = stored_payload_digest else {
        return Ok(None);
    };
    let hash_pairs: Vec<(MerkleHash, u64)> = chunks
        .iter()
        .map(|chunk| (chunk.hash, u64::from(chunk.uncompressed_len)))
        .collect();
    if xorb_hash(&hash_pairs) != *hash {
        return Ok(None);
    }

    Ok(Some(CachedRemoteXorbIndex {
        xorb_hash: *hash,
        payload_digest,
        chunks,
    }))
}

fn identity_tokens_match(
    stored_e_tag: Option<&str>,
    stored_version: Option<&str>,
    e_tag: Option<&str>,
    version: Option<&str>,
) -> bool {
    if stored_e_tag.is_none() && stored_version.is_none() {
        return false;
    }
    if let Some(stored) = stored_e_tag
        && e_tag != Some(stored)
    {
        return false;
    }
    if let Some(stored) = stored_version
        && version != Some(stored)
    {
        return false;
    }
    true
}

fn open_xorb_index(index_path: &Path) -> Result<Connection> {
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut last_error = None;
    for delay_ms in [0].into_iter().chain(XORB_INDEX_OPEN_RETRY_DELAYS_MS) {
        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }

        match open_xorb_index_once(index_path) {
            Ok(conn) => return Ok(conn),
            Err(error) if is_retryable_xorb_index_error(&error) => {
                debug!(
                    path = %index_path.display(),
                    error = %error,
                    "retrying xorb index open"
                );
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }

    if let Some(error) = last_error {
        return Err(error);
    }

    Err(CacheError::CorruptObject {
        path: index_path.display().to_string(),
        reason: "xorb index open retry loop exited without a sqlite error".to_owned(),
    })
}

fn open_xorb_index_once(index_path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(index_path).map_err(|source| cache_index_error(index_path, source))?;
    conn.busy_timeout(XORB_INDEX_BUSY_TIMEOUT)
        .map_err(|source| cache_index_error(index_path, source))?;

    // Schema creation and validation share the same write transaction. A
    // concurrent opener therefore waits for initialization instead of
    // observing SQLite's transient version-0 file.
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|source| cache_index_error(index_path, source))?;
    let schema_version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|source| cache_index_error(index_path, source))?;
    if schema_version == XORB_INDEX_SCHEMA_VERSION {
        validate_xorb_index_schema(&conn, index_path)?;
        conn.execute_batch("COMMIT")
            .map_err(|source| cache_index_error(index_path, source))?;
        return Ok(conn);
    }

    let objects = xorb_index_schema_objects(&conn, index_path)?;
    if schema_version != 0 || !objects.is_empty() {
        return Err(noncanonical_xorb_index_error(index_path, schema_version));
    }
    conn.execute_batch(
        "CREATE TABLE xorb_index (
            chunk_hash BLOB PRIMARY KEY NOT NULL,
            xorb_hash BLOB NOT NULL,
            chunk_index INTEGER NOT NULL,
            uncompressed_size INTEGER NOT NULL,
            xorb_bytes INTEGER NOT NULL,
            payload_hash BLOB NOT NULL
        );
        CREATE INDEX idx_xorb_index_xorb_hash
            ON xorb_index (xorb_hash);
        CREATE TABLE remote_xorb_proof (
            xorb_hash BLOB PRIMARY KEY NOT NULL,
            payload_digest BLOB NOT NULL,
            xorb_bytes INTEGER NOT NULL,
            e_tag TEXT,
            version TEXT
        );
        CREATE TABLE remote_xorb_index (
            xorb_hash BLOB NOT NULL,
            payload_digest BLOB NOT NULL,
            xorb_bytes INTEGER NOT NULL,
            e_tag TEXT,
            version TEXT,
            chunk_index INTEGER NOT NULL,
            chunk_hash BLOB NOT NULL,
            chunk_offset INTEGER NOT NULL,
            compressed_len INTEGER NOT NULL,
            uncompressed_len INTEGER NOT NULL,
            scheme INTEGER NOT NULL,
            PRIMARY KEY (xorb_hash, chunk_index)
        );
        PRAGMA user_version = 1;
        COMMIT;",
    )
    .map_err(|source| cache_index_error(index_path, source))?;
    Ok(conn)
}

fn validate_xorb_index_schema(conn: &Connection, index_path: &Path) -> Result<()> {
    let version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|source| cache_index_error(index_path, source))?;
    let objects = xorb_index_schema_objects(conn, index_path)?;
    let expected = vec![
        ("index".to_owned(), "idx_xorb_index_xorb_hash".to_owned()),
        ("table".to_owned(), "remote_xorb_index".to_owned()),
        ("table".to_owned(), "remote_xorb_proof".to_owned()),
        ("table".to_owned(), "xorb_index".to_owned()),
    ];
    if version != XORB_INDEX_SCHEMA_VERSION || objects != expected {
        return Err(noncanonical_xorb_index_error(index_path, version));
    }
    Ok(())
}

fn xorb_index_schema_objects(
    conn: &Connection,
    index_path: &Path,
) -> Result<Vec<(String, String)>> {
    let mut statement = conn
        .prepare(
            "SELECT type, name FROM sqlite_schema
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .map_err(|source| cache_index_error(index_path, source))?;
    let objects = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|source| cache_index_error(index_path, source))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|source| cache_index_error(index_path, source))?;
    Ok(objects)
}

fn noncanonical_xorb_index_error(index_path: &Path, version: i64) -> CacheError {
    CacheError::CorruptObject {
        path: index_path.display().to_string(),
        reason: format!(
            "xorb index is not canonical v1; delete this cache file and retry (version={version})"
        ),
    }
}

fn is_retryable_xorb_index_error(error: &CacheError) -> bool {
    matches!(
        error,
        CacheError::Index { source, .. }
            if matches!(source.sqlite_error_code(),
        Some(
            rusqlite::ffi::ErrorCode::CannotOpen
                | rusqlite::ffi::ErrorCode::DatabaseBusy
                | rusqlite::ffi::ErrorCode::DatabaseLocked
        )
        )
    )
}

fn cache_index_error(index_path: &Path, source: rusqlite::Error) -> CacheError {
    CacheError::Index {
        path: index_path.display().to_string(),
        source,
    }
}

fn decode_fixed_hash(value: &[u8]) -> Option<[u8; HASH_BYTES]> {
    value.try_into().ok()
}

/// Read a file to a string, returning `None` on any error.
async fn read_string_if_exists(path: &Path) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}

/// Update a file's mtime to now for LRU tracking.
async fn touch_mtime(path: &Path) {
    // Best-effort; failure doesn't affect correctness.
    let path = path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        filetime::set_file_mtime(&path, filetime::FileTime::now())
    })
    .await;
}

/// Entry with its size and mtime, used for LRU sorting.
struct LruEntry {
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
    regular: bool,
}

struct EvictResult {
    count: u64,
    bytes: u64,
    entries: Vec<PrunedCacheObject>,
}

struct LargeEvictResult {
    chunks: u64,
    xorbs: u64,
    bytes: u64,
    entries: Vec<PrunedCacheObject>,
}

#[derive(Clone, Copy)]
enum LargeObjectKind {
    Chunk,
    Xorb,
}

struct LargeLruEntry {
    entry: LruEntry,
    kind: LargeObjectKind,
}

struct TargetLruEntry {
    entry: LruEntry,
    kind: PruneObjectKind,
}

/// Evict chunks and xorbs under one shared LRU budget.
async fn lru_evict_large_objects(
    chunks_dir: &Path,
    xorbs_dir: &Path,
    xorb_index_path: &Path,
    max_bytes: u64,
    options: PruneOptions,
) -> Result<LargeEvictResult> {
    let mut entries: Vec<LargeLruEntry> = collect_hash_entries(chunks_dir)
        .await?
        .into_iter()
        .map(|entry| LargeLruEntry {
            entry,
            kind: LargeObjectKind::Chunk,
        })
        .collect();
    entries.extend(
        collect_hash_entries(xorbs_dir)
            .await?
            .into_iter()
            .map(|entry| LargeLruEntry {
                entry,
                kind: LargeObjectKind::Xorb,
            }),
    );

    let total = checked_sum(entries.iter().map(|e| e.entry.size))?;
    if total <= max_bytes {
        return Ok(LargeEvictResult {
            chunks: 0,
            xorbs: 0,
            bytes: 0,
            entries: Vec::new(),
        });
    }

    entries.sort_by_key(|e| e.entry.mtime);

    let mut freed: u64 = 0;
    let mut chunks: u64 = 0;
    let mut xorbs: u64 = 0;
    let mut pruned_entries = Vec::new();
    let target = total - max_bytes;

    for entry in &entries {
        if freed >= target {
            break;
        }
        if !options.dry_run {
            match tokio::fs::remove_file(&entry.entry.path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }

        let kind = match entry.kind {
            LargeObjectKind::Chunk => {
                chunks += 1;
                PruneObjectKind::Chunk
            }
            LargeObjectKind::Xorb => {
                xorbs += 1;
                if !options.dry_run
                    && let Some(hash) = merkle_hash_from_path(&entry.entry.path)
                    && let Err(e) = remove_xorb_index_entries(xorb_index_path, &hash)
                {
                    warn!(
                        xorb = %hash.hex(),
                        error = %e,
                        "local xorb cache index prune cleanup failed"
                    );
                }
                PruneObjectKind::Xorb
            }
        };
        freed += entry.entry.size;
        if options.record_entries {
            pruned_entries.push(PrunedCacheObject {
                kind,
                path: entry.entry.path.clone(),
                bytes: entry.entry.size,
            });
        }
    }

    Ok(LargeEvictResult {
        chunks,
        xorbs,
        bytes: freed,
        entries: pruned_entries,
    })
}

/// Walk a two-level hash directory (`{prefix}/{hash}`) and evict oldest
/// entries until total size ≤ `max_bytes`.
async fn lru_evict(
    dir: &Path,
    max_bytes: u64,
    kind: PruneObjectKind,
    options: PruneOptions,
) -> Result<EvictResult> {
    let entries = collect_hash_entries(dir).await?;
    let total = checked_sum(entries.iter().map(|e| e.size))?;
    if total <= max_bytes {
        return Ok(EvictResult {
            count: 0,
            bytes: 0,
            entries: Vec::new(),
        });
    }

    // Sort oldest-first (ascending mtime).
    let mut sorted = entries;
    sorted.sort_by_key(|e| e.mtime);

    let mut freed: u64 = 0;
    let mut evicted: u64 = 0;
    let mut pruned_entries = Vec::new();
    let target = total - max_bytes;

    for entry in &sorted {
        if freed >= target {
            break;
        }
        if !options.dry_run {
            match tokio::fs::remove_file(&entry.path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }

        freed += entry.size;
        evicted += 1;
        if options.record_entries {
            pruned_entries.push(PrunedCacheObject {
                kind,
                path: entry.path.clone(),
                bytes: entry.size,
            });
        }
    }

    Ok(EvictResult {
        count: evicted,
        bytes: freed,
        entries: pruned_entries,
    })
}

fn merkle_hash_from_path(path: &Path) -> Option<MerkleHash> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|hex| MerkleHash::from_hex(hex).ok())
}

/// Collect all files under a two-level hash directory.
async fn collect_hash_entries(dir: &Path) -> Result<Vec<LruEntry>> {
    let mut entries = Vec::new();
    let mut prefixes = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => return Err(error.into()),
    };
    while let Some(prefix_entry) = prefixes.next_entry().await? {
        let prefix_path = prefix_entry.path();
        if !prefix_entry.file_type().await?.is_dir() {
            continue;
        }
        let mut files = tokio::fs::read_dir(&prefix_path).await?;
        while let Some(file_entry) = files.next_entry().await? {
            let path = file_entry.path();
            let file_type = file_entry.file_type().await?;
            if file_type.is_dir() {
                continue;
            }
            let meta = tokio::fs::symlink_metadata(&path).await?;
            if entries.len() >= MAX_CACHE_LRU_ENTRIES {
                return Err(CacheError::Internal(format!(
                    "cache directory {} contains more than {MAX_CACHE_LRU_ENTRIES} entries; refusing an unbounded LRU scan",
                    dir.display()
                )));
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push(LruEntry {
                path,
                size: meta.len(),
                mtime,
                regular: file_type.is_file(),
            });
        }
    }
    Ok(entries)
}

/// Verify all hash-keyed files in a two-level directory.
async fn verify_dir(dir: &Path, report: &mut VerifyReport, max_bytes: Option<u64>) -> Result<()> {
    let entries = collect_hash_entries(dir).await?;
    for entry in &entries {
        report.total += 1;
        if !entry.regular {
            report.corrupt += 1;
            remove_discovered_entry(&entry.path).await?;
            continue;
        }
        // The filename is the expected hex hash.
        let Some(filename) = entry.path.file_name().and_then(|f| f.to_str()) else {
            report.corrupt += 1;
            remove_discovered_entry(&entry.path).await?;
            continue;
        };
        let Ok(expected) = MerkleHash::from_hex(filename) else {
            report.corrupt += 1;
            warn!(
                path = %entry.path.display(),
                "invalid cache entry name — removing"
            );
            remove_discovered_entry(&entry.path).await?;
            continue;
        };
        if max_bytes.is_some_and(|limit| entry.size > limit) {
            report.corrupt += 1;
            warn!(
                path = %entry.path.display(),
                bytes = entry.size,
                limit = max_bytes.unwrap_or(0),
                "oversized cache entry — removing"
            );
            remove_discovered_entry(&entry.path).await?;
            continue;
        }
        match verify_data_file(&entry.path, expected).await {
            Ok(true) => report.valid += 1,
            Ok(false) => {
                report.corrupt += 1;
                warn!(
                    path = %entry.path.display(),
                    "corrupt cache entry — removing"
                );
                remove_discovered_entry(&entry.path).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.total -= 1,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Verify all xorb files in a two-level directory.
async fn verify_xorb_dir(
    dir: &Path,
    xorb_index_path: &Path,
    report: &mut VerifyReport,
) -> Result<()> {
    let entries = collect_hash_entries(dir).await?;
    for entry in &entries {
        report.total += 1;
        if !entry.regular {
            report.corrupt += 1;
            remove_discovered_entry(&entry.path).await?;
            if let Some(hash) = merkle_hash_from_path(&entry.path) {
                remove_xorb_index_entries(xorb_index_path, &hash)?;
            }
            continue;
        }
        let Some(filename) = entry.path.file_name().and_then(|f| f.to_str()) else {
            report.corrupt += 1;
            remove_discovered_entry(&entry.path).await?;
            continue;
        };
        let Ok(expected) = MerkleHash::from_hex(filename) else {
            report.corrupt += 1;
            warn!(
                path = %entry.path.display(),
                "invalid cached xorb name — removing"
            );
            remove_discovered_entry(&entry.path).await?;
            continue;
        };
        match verify_xorb_file_payload(&entry.path, entry.size, &expected).await {
            Ok(()) => report.valid += 1,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                report.total -= 1;
            }
            Err(_) => {
                report.corrupt += 1;
                warn!(
                    path = %entry.path.display(),
                    "corrupt cached xorb — removing"
                );
                remove_discovered_entry(&entry.path).await?;
                remove_xorb_index_entries(xorb_index_path, &expected)?;
            }
        }
    }
    Ok(())
}

async fn verify_data_file(path: &Path, expected: MerkleHash) -> std::io::Result<bool> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut source = std::fs::File::open(path)?;
        let mut hashed = HashedWrite::new(std::io::sink());
        std::io::copy(&mut source, &mut hashed)?;
        Ok(hashed.hash() == expected)
    })
    .await
    .map_err(|error| std::io::Error::other(format!("cache verification worker failed: {error}")))?
}

/// Sum file sizes and count in a two-level hash directory.
async fn dir_size(dir: &Path) -> Result<(u64, u64)> {
    let entries = collect_hash_entries(dir).await?;
    let bytes = checked_sum(entries.iter().map(|e| e.size))?;
    let count = entries.len() as u64;
    Ok((bytes, count))
}

fn checked_sum(mut values: impl Iterator<Item = u64>) -> Result<u64> {
    values.try_fold(0u64, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| CacheError::Internal("cache byte total overflow".to_owned()))
    })
}

async fn remove_discovered_entry(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Count manifest data files (*.json) in the manifests directory.
async fn count_manifests(dir: &Path) -> Result<u64> {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut count = 0u64;
    while let Some(entry) = rd.next_entry().await? {
        if entry.file_type().await?.is_file()
            && entry.path().extension().is_some_and(|ext| ext == "json")
        {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    fn temp_cache() -> (tempfile::TempDir, LocalCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(dir.path().to_path_buf());
        (dir, cache)
    }

    fn test_xorb(data: &[u8]) -> (MerkleHash, Bytes) {
        let chunk = Chunk::new(Bytes::copy_from_slice(data));
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0)).unwrap();
        let mut xorbs = builder.finalize().unwrap();
        let xorb = xorbs.pop().unwrap();
        (xorb.hash, xorb.bytes)
    }

    fn test_xorb_with_chunks(chunks: &[&[u8]]) -> (Vec<MerkleHash>, MerkleHash, Bytes) {
        let mut builder = XorbBuilder::new();
        let mut chunk_hashes = Vec::new();
        for data in chunks {
            let chunk = Chunk::new(Bytes::copy_from_slice(data));
            chunk_hashes.push(chunk.hash);
            builder.push(&chunk, RunId(0)).unwrap();
        }
        let mut xorbs = builder.finalize().unwrap();
        let xorb = xorbs.pop().unwrap();
        (chunk_hashes, xorb.hash, xorb.bytes)
    }

    #[tokio::test]
    async fn put_and_get_chunk() {
        let (_dir, cache) = temp_cache();
        let data = b"hello chunk";
        let hash = compute_data_hash(data);
        let key = CacheKey::Chunk(hash);

        cache.put(&key, data).await.unwrap();
        assert!(cache.contains(&key).await);

        let fetched = cache
            .get_or_fetch(&key, || async { panic!("should not fetch") })
            .await
            .unwrap();
        assert_eq!(&fetched[..], data);
    }

    #[tokio::test]
    async fn put_and_get_shard() {
        let (_dir, cache) = temp_cache();
        let data = b"shard payload";
        let hash = compute_data_hash(data);
        let key = CacheKey::Shard(hash);

        cache.put(&key, data).await.unwrap();
        let fetched = cache
            .get_or_fetch(&key, || async { panic!("should not fetch") })
            .await
            .unwrap();
        assert_eq!(&fetched[..], data);
    }

    #[tokio::test]
    async fn get_or_fetch_calls_fetch_on_miss() {
        let (_dir, cache) = temp_cache();
        let data = b"fetched data";
        let hash = compute_data_hash(data);
        let key = CacheKey::Chunk(hash);

        let result = cache
            .get_or_fetch(&key, || async { Ok(Bytes::from_static(data)) })
            .await
            .unwrap();
        assert_eq!(&result[..], data);

        // Second call should hit cache.
        let result2 = cache
            .get_or_fetch(&key, || async { panic!("should not fetch again") })
            .await
            .unwrap();
        assert_eq!(&result2[..], data);
    }

    #[tokio::test]
    async fn bounded_read_does_not_serve_an_oversized_cached_body() {
        let (_dir, cache) = temp_cache();
        let data = Bytes::from_static(b"oversized cached body");
        let key = CacheKey::Shard(compute_data_hash(&data));
        let path = cache.hash_path(&key);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, &data).await.unwrap();

        let error = cache
            .get_or_fetch_bounded_with(&key, 4, || async {
                Err::<Bytes, CacheError>(CacheError::CorruptObject {
                    path: "fetch sentinel".to_owned(),
                    reason: "oversized cache must not be served".to_owned(),
                })
            })
            .await
            .unwrap_err();

        assert!(matches!(error, CacheError::CorruptObject { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_cold_misses_fetch_one_object_once() {
        let (_dir, cache) = temp_cache();
        let cache = Arc::new(cache);
        let data = Bytes::from_static(b"single flight payload");
        let key = CacheKey::Chunk(compute_data_hash(&data));
        let fetches = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let key = key.clone();
            let data = data.clone();
            let fetches = Arc::clone(&fetches);
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_fetch(&key, || async move {
                        fetches.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        Ok(data)
                    })
                    .await
            }));
        }

        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), data);
        }
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn get_or_fetch_rejects_hash_mismatch_on_miss() {
        let (_dir, cache) = temp_cache();
        let good_data = b"correct fetched data";
        let bad_data = b"wrong fetched data";
        let key = CacheKey::Shard(compute_data_hash(good_data));

        let err = cache
            .get_or_fetch(&key, || async { Ok(Bytes::from_static(bad_data)) })
            .await
            .unwrap_err();

        assert!(matches!(err, CacheError::HashMismatch { .. }));
        assert!(!cache.contains(&key).await);
    }

    #[tokio::test]
    async fn put_rejects_hash_mismatch_for_hash_verified_entries() {
        let (_dir, cache) = temp_cache();
        let key = CacheKey::Shard(compute_data_hash(b"expected shard bytes"));

        let err = cache.put(&key, b"different shard bytes").await.unwrap_err();

        assert!(matches!(err, CacheError::HashMismatch { .. }));
        assert!(!cache.contains(&key).await);
    }

    #[tokio::test]
    async fn hash_mismatch_evicts_and_refetches() {
        let (_dir, cache) = temp_cache();
        let good_data = b"correct bytes";
        let hash = compute_data_hash(good_data);
        let key = CacheKey::Chunk(hash);

        // Write wrong data directly to the cache path.
        let path = cache.hash_path(&key);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, b"corrupted").await.unwrap();

        // get_or_fetch should detect mismatch, evict, and refetch.
        let result = cache
            .get_or_fetch(&key, || async { Ok(Bytes::from_static(good_data)) })
            .await
            .unwrap();
        assert_eq!(&result[..], good_data);
    }

    #[tokio::test]
    async fn manifest_etag_cache() {
        let (_dir, cache) = temp_cache();
        let data = b"manifest content";
        let key = CacheKey::Manifest {
            name: "pack-list".to_string(),
            etag: Some("etag-v1".to_string()),
        };

        cache.put(&key, data).await.unwrap();
        assert!(cache.contains(&key).await);

        // Same ETag → cache hit.
        let result = cache
            .get_or_fetch(&key, || async { panic!("should not fetch") })
            .await
            .unwrap();
        assert_eq!(&result[..], data);

        // Different ETag → refetch.
        let key_v2 = CacheKey::Manifest {
            name: "pack-list".to_string(),
            etag: Some("etag-v2".to_string()),
        };
        let new_data = b"updated manifest";
        let result = cache
            .get_or_fetch(&key_v2, || async { Ok(Bytes::from_static(new_data)) })
            .await
            .unwrap();
        assert_eq!(&result[..], new_data);
        assert_eq!(cache.stats().await.unwrap().manifest_count, 1);
    }

    #[tokio::test]
    async fn prune_evicts_oldest_chunks() {
        let (_dir, cache) = temp_cache();
        // Set a tiny limit so we can trigger eviction.
        let cache = LocalCache::with_limits(cache.root.clone(), 100, None);

        // Write several chunks totaling more than 100 bytes.
        let mut keys = Vec::new();
        for i in 0u8..5 {
            let data = vec![i; 50]; // 50 bytes each = 250 total
            let hash = compute_data_hash(&data);
            let key = CacheKey::Chunk(hash);
            cache.put(&key, &data).await.unwrap();
            keys.push(key);
            // Small sleep to ensure distinct mtimes.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let stats = cache.prune().await.unwrap();
        assert!(stats.chunks_evicted > 0);
        assert!(stats.bytes_freed > 0);

        // After prune, total should be ≤ 100.
        let cache_stats = cache.stats().await.unwrap();
        assert!(cache_stats.chunk_bytes <= 100);
    }

    #[tokio::test]
    async fn shard_lru_eviction() {
        let (_dir, cache) = temp_cache();
        let cache = LocalCache::with_limits(cache.root.clone(), DEFAULT_CHUNK_MAX_BYTES, Some(80));

        for i in 0u8..4 {
            let data = vec![i; 40]; // 40 bytes each = 160 total
            let hash = compute_data_hash(&data);
            let key = CacheKey::Shard(hash);
            cache.put(&key, &data).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let stats = cache.prune().await.unwrap();
        assert!(stats.shards_evicted > 0);
        assert!(stats.bytes_freed > 0);
    }

    #[tokio::test]
    async fn xorb_lru_eviction() {
        let (_dir, cache) = temp_cache();
        let cache = LocalCache::with_limits(cache.root.clone(), 1, None);

        for i in 0u8..4 {
            let data = vec![i; 40];
            let (hash, data) = test_xorb(&data);
            let key = CacheKey::Xorb(hash);
            cache.put(&key, data.as_ref()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let stats = cache.prune().await.unwrap();
        assert!(stats.xorbs_evicted > 0);
        assert!(stats.bytes_freed > 0);

        let cache_stats = cache.stats().await.unwrap();
        assert!(cache_stats.xorb_bytes <= 1);
    }

    #[tokio::test]
    async fn xorb_lru_eviction_removes_index_candidate() {
        let (_dir, cache) = temp_cache();
        let cache = LocalCache::with_limits(cache.root.clone(), 1, None);
        let (chunk_hashes, xorb_hash, xorb_data) = test_xorb_with_chunks(&[b"chunk-a", b"chunk-b"]);
        cache
            .put(&CacheKey::Xorb(xorb_hash), xorb_data.as_ref())
            .await
            .unwrap();
        assert!(
            !cache
                .cached_xorb_candidates_for_chunks(&chunk_hashes)
                .await
                .unwrap()
                .is_empty()
        );

        let stats = cache.prune().await.unwrap();

        assert_eq!(stats.xorbs_evicted, 1);
        let candidates = cache
            .cached_xorb_candidates_for_chunks(&chunk_hashes)
            .await
            .unwrap();
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn evict_bytes_removes_oldest_entries_until_target_is_met() {
        let (_dir, cache) = temp_cache();

        let chunk_data = vec![1; 30];
        let chunk_key = CacheKey::Chunk(compute_data_hash(&chunk_data));
        cache.put(&chunk_key, &chunk_data).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let shard_data = vec![2; 40];
        let shard_key = CacheKey::Shard(compute_data_hash(&shard_data));
        cache.put(&shard_key, &shard_data).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (xorb_hash, xorb_data) = test_xorb(&vec![3; 50]);
        let xorb_key = CacheKey::Xorb(xorb_hash);
        cache.put(&xorb_key, xorb_data.as_ref()).await.unwrap();

        let target = u64::try_from(chunk_data.len() + shard_data.len() + 1).unwrap();
        let stats = cache.evict_bytes(target).await.unwrap();

        assert_eq!(stats.chunks_evicted, 1);
        assert_eq!(stats.shards_evicted, 1);
        assert_eq!(stats.xorbs_evicted, 1);
        assert_eq!(
            stats.bytes_freed,
            u64::try_from(chunk_data.len() + shard_data.len() + xorb_data.len()).unwrap()
        );
        assert!(!cache.contains(&chunk_key).await);
        assert!(!cache.contains(&shard_key).await);
        assert!(!cache.contains(&xorb_key).await);
    }

    #[tokio::test]
    async fn chunk_and_xorb_lru_share_large_object_budget() {
        let (_dir, cache) = temp_cache();
        let cache = LocalCache::with_limits(cache.root.clone(), 120, None);

        let chunk_data = vec![1u8; 80];
        let chunk_hash = compute_data_hash(&chunk_data);
        cache
            .put(&CacheKey::Chunk(chunk_hash), &chunk_data)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (xorb_hash, xorb_data) = test_xorb(&vec![2u8; 80]);
        let max_bytes = xorb_data.len() as u64 + 1;
        let cache = LocalCache::with_limits(cache.root.clone(), max_bytes, None);
        cache
            .put(&CacheKey::Xorb(xorb_hash), xorb_data.as_ref())
            .await
            .unwrap();

        let stats = cache.prune().await.unwrap();
        assert_eq!(stats.chunks_evicted, 1);
        assert_eq!(stats.xorbs_evicted, 0);

        let cache_stats = cache.stats().await.unwrap();
        assert!(cache_stats.chunk_bytes + cache_stats.xorb_bytes <= max_bytes);
    }

    #[tokio::test]
    async fn verify_detects_corruption() {
        let (_dir, cache) = temp_cache();
        let data = b"valid data";
        let hash = compute_data_hash(data);
        let key = CacheKey::Chunk(hash);
        cache.put(&key, data).await.unwrap();

        // Corrupt the file in place.
        let path = cache.hash_path(&key);
        tokio::fs::write(&path, b"bad").await.unwrap();

        let report = cache.verify().await.unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.corrupt, 1);
        assert_eq!(report.valid, 0);

        // Corrupt file should have been removed.
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn verify_accepts_valid_xorb_identity() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"valid cached xorb");
        let key = CacheKey::Xorb(hash);
        cache.put(&key, data.as_ref()).await.unwrap();

        let report = cache.verify().await.unwrap();

        assert_eq!(report.total, 1);
        assert_eq!(report.valid, 1);
        assert_eq!(report.corrupt, 0);
    }

    #[tokio::test]
    async fn verify_evicts_corrupt_xorb_payload() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"corrupt cached xorb");
        let key = CacheKey::Xorb(hash);
        cache.put(&key, data.as_ref()).await.unwrap();

        let path = cache.hash_path(&key);
        let mut corrupt = data.to_vec();
        corrupt[0] ^= 0xFF;
        tokio::fs::write(&path, corrupt).await.unwrap();

        let report = cache.verify().await.unwrap();

        assert_eq!(report.total, 1);
        assert_eq!(report.valid, 0);
        assert_eq!(report.corrupt, 1);
        assert!(!path.exists());
        assert!(
            cache
                .cached_xorb_candidates_for_chunks(&[compute_data_hash(b"corrupt cached xorb")])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn verify_evicts_invalid_hash_entry_names() {
        let (_dir, cache) = temp_cache();
        let invalid = cache.root().join("chunks").join("xx").join("not-a-hash");
        tokio::fs::create_dir_all(invalid.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&invalid, b"orphan temp data")
            .await
            .unwrap();

        let report = cache.verify().await.unwrap();

        assert_eq!(report.total, 1);
        assert_eq!(report.valid, 0);
        assert_eq!(report.corrupt, 1);
        assert!(!invalid.exists());
    }

    #[tokio::test]
    async fn clean_removes_all() {
        let (_dir, cache) = temp_cache();
        let data = b"some data";
        let hash = compute_data_hash(data);
        let (xorb_hash, xorb_data) = test_xorb(data);
        cache.put(&CacheKey::Chunk(hash), data).await.unwrap();
        cache
            .put(&CacheKey::Xorb(xorb_hash), xorb_data.as_ref())
            .await
            .unwrap();
        cache
            .put(
                &CacheKey::Stage(crab_types::workflow::StageHash([3u8; 32])),
                data,
            )
            .await
            .unwrap();
        cache
            .put(
                &CacheKey::Manifest {
                    name: "test".to_string(),
                    etag: None,
                },
                data,
            )
            .await
            .unwrap();
        let repo_index = cache.root().join("repos/test/chunk-index.sqlite");
        tokio::fs::create_dir_all(repo_index.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&repo_index, b"sqlite").await.unwrap();
        tokio::fs::write(cache.root().join("bloom.bin"), b"bloom")
            .await
            .unwrap();

        cache.clean().await.unwrap();

        let stats = cache.stats().await.unwrap();
        assert_eq!(stats.chunk_bytes, 0);
        assert_eq!(stats.xorb_bytes, 0);
        assert_eq!(stats.stage_bytes, 0);
        assert_eq!(stats.manifest_count, 0);
        assert!(
            tokio::fs::read_dir(cache.root())
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn stats_counts_correctly() {
        let (_dir, cache) = temp_cache();

        for i in 0u8..3 {
            let data = vec![i; 100];
            let hash = compute_data_hash(&data);
            cache.put(&CacheKey::Chunk(hash), &data).await.unwrap();
        }
        for i in 10u8..12 {
            let data = vec![i; 200];
            let hash = compute_data_hash(&data);
            cache.put(&CacheKey::Shard(hash), &data).await.unwrap();
        }
        for i in 20u8..22 {
            let data = vec![i; 150];
            let (hash, xorb_data) = test_xorb(&data);
            cache
                .put(&CacheKey::Xorb(hash), xorb_data.as_ref())
                .await
                .unwrap();
        }
        cache
            .put(
                &CacheKey::Stage(crab_types::workflow::StageHash([0xef; 32])),
                b"stage",
            )
            .await
            .unwrap();
        cache
            .put(
                &CacheKey::Manifest {
                    name: "m1".to_string(),
                    etag: None,
                },
                b"{}",
            )
            .await
            .unwrap();

        let stats = cache.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 3);
        assert_eq!(stats.chunk_bytes, 300);
        assert_eq!(stats.shard_count, 2);
        assert_eq!(stats.shard_bytes, 400);
        assert_eq!(stats.xorb_count, 2);
        assert!(stats.xorb_bytes > 0);
        assert_eq!(stats.stage_count, 1);
        assert_eq!(stats.stage_bytes, 5);
        assert_eq!(stats.manifest_count, 1);
    }

    #[tokio::test]
    async fn contains_returns_false_for_missing() {
        let (_dir, cache) = temp_cache();
        let hash = compute_data_hash(b"nonexistent");
        assert!(!cache.contains(&CacheKey::Chunk(hash)).await);
    }

    #[tokio::test]
    async fn xorb_hash_path_uses_xorbs_directory() {
        let (_dir, cache) = temp_cache();
        let hash = compute_data_hash(b"xorb bytes");
        let hex = hash.hex();
        let key = CacheKey::Xorb(hash);

        let path = cache.hash_path(&key);
        let expected = cache.root().join("xorbs").join(&hex[..2]).join(&hex);
        assert_eq!(path, expected);
    }

    #[tokio::test]
    async fn stage_hash_path_uses_stages_directory() {
        let (_dir, cache) = temp_cache();
        let stage_hash = crab_types::workflow::StageHash([0xab; 32]);
        let hex = stage_hash.as_hex();
        let key = CacheKey::Stage(stage_hash);

        let path = cache.hash_path(&key);
        let expected = cache.root().join("stages").join(&hex[..2]).join(&hex);
        assert_eq!(path, expected);
    }

    #[tokio::test]
    async fn put_and_contains_stage_roundtrip() {
        let (_dir, cache) = temp_cache();
        let stage_hash = crab_types::workflow::StageHash([0xcd; 32]);
        let key = CacheKey::Stage(stage_hash);

        assert!(!cache.contains(&key).await);
        cache.put(&key, br#"{"schema_version":1}"#).await.unwrap();
        assert!(cache.contains(&key).await);
    }

    #[tokio::test]
    async fn put_and_get_xorb_roundtrip() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"packed xorb payload");
        let key = CacheKey::Xorb(hash);

        assert!(!cache.contains(&key).await);
        cache.put(&key, data.as_ref()).await.unwrap();
        assert!(cache.contains(&key).await);
        assert!(cache.contains_verified(&key).await);

        let fetched = cache
            .get_or_fetch(&key, || async { panic!("should not fetch") })
            .await
            .unwrap();
        assert_eq!(fetched, data);
    }

    #[tokio::test]
    async fn warm_xorb_read_does_not_rewrite_install_index() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"warm indexed xorb payload");
        let key = CacheKey::Xorb(hash);
        cache.put(&key, data.as_ref()).await.unwrap();

        let index_path = cache.xorb_index_path();
        let conn = open_xorb_index(&index_path).expect("open xorb index");
        conn.execute_batch(
            "CREATE TABLE xorb_index_write_audit (writes INTEGER NOT NULL);
             INSERT INTO xorb_index_write_audit VALUES (0);
             CREATE TRIGGER audit_xorb_index_insert
             AFTER INSERT ON xorb_index BEGIN
                 UPDATE xorb_index_write_audit SET writes = writes + 1;
             END;",
        )
        .expect("install xorb index audit");
        let fetched = cache
            .get_or_fetch(&key, || async { panic!("warm xorb should not fetch") })
            .await
            .expect("read warm xorb");

        assert_eq!(fetched, data);
        let writes: i64 = conn
            .query_row("SELECT writes FROM xorb_index_write_audit", [], |row| {
                row.get(0)
            })
            .expect("read xorb index audit");
        assert_eq!(writes, 0);
    }

    #[tokio::test]
    async fn xorb_index_rejects_non_v1_schema_without_mutation() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"strict v1 xorb index");
        cache
            .put(&CacheKey::Xorb(hash), data.as_ref())
            .await
            .unwrap();
        let index_path = cache.xorb_index_path();
        let conn = Connection::open(&index_path).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        drop(conn);

        assert!(open_xorb_index(&index_path).is_err());
        let conn = Connection::open(&index_path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let rows: i64 = conn
            .query_row("SELECT COUNT(1) FROM xorb_index", [], |row| row.get(0))
            .unwrap();
        assert!(rows > 0);
    }

    #[tokio::test]
    async fn put_xorb_indexes_candidate_by_chunk() {
        let (_dir, cache) = temp_cache();
        let (chunk_hashes, xorb_hash, data) =
            test_xorb_with_chunks(&[b"indexed chunk one", b"indexed chunk two"]);

        cache
            .put(&CacheKey::Xorb(xorb_hash), data.as_ref())
            .await
            .unwrap();

        let candidates = cache
            .cached_xorb_candidates_for_chunks(&[chunk_hashes[1]])
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].xorb_hash, xorb_hash);
        assert_eq!(candidates[0].bytes, data.len() as u64);
        assert_eq!(candidates[0].payload_hash, *blake3::hash(&data).as_bytes());
        assert_eq!(candidates[0].placements.len(), 2);
        assert_eq!(candidates[0].placements[1].chunk_hash, chunk_hashes[1]);
    }

    #[tokio::test]
    async fn xorb_index_lock_wait_does_not_block_async_cache_work() {
        let (_dir, cache) = temp_cache();
        let index_path = cache.xorb_index_path();
        drop(open_xorb_index(&index_path).expect("initialize xorb index"));

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let locked_index_path = index_path.clone();
        let lock_thread = std::thread::spawn(move || {
            let conn = Connection::open(locked_index_path).expect("open lock connection");
            conn.execute_batch("BEGIN EXCLUSIVE")
                .expect("lock xorb index");
            ready_tx.send(()).expect("signal lock");
            std::thread::sleep(Duration::from_millis(500));
            conn.execute_batch("COMMIT").expect("release xorb index");
        });
        ready_rx.recv().expect("wait for xorb index lock");

        let (xorb_hash, data) = test_xorb(b"runtime responsiveness while sqlite is locked");
        let key = CacheKey::Xorb(xorb_hash);
        let started = std::time::Instant::now();
        let put = cache.put(&key, data.as_ref());
        let heartbeat = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            started.elapsed()
        };
        let (put_result, heartbeat_elapsed) = tokio::join!(put, heartbeat);
        lock_thread.join().expect("join lock thread");

        put_result.expect("cache xorb");
        assert!(
            heartbeat_elapsed < Duration::from_millis(250),
            "sqlite lock wait blocked the async runtime for {heartbeat_elapsed:?}"
        );
    }

    #[tokio::test]
    async fn evict_xorb_removes_index_candidate() {
        let (_dir, cache) = temp_cache();
        let (chunk_hashes, xorb_hash, data) = test_xorb_with_chunks(&[b"evicted indexed chunk"]);
        let key = CacheKey::Xorb(xorb_hash);

        cache.put(&key, data.as_ref()).await.unwrap();
        assert_eq!(
            cache
                .cached_xorb_candidates_for_chunks(&[chunk_hashes[0]])
                .await
                .unwrap()
                .len(),
            1
        );

        cache.evict(&key).await.unwrap();

        assert!(
            cache
                .cached_xorb_candidates_for_chunks(&[chunk_hashes[0]])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn remote_xorb_proof_requires_matching_identity_token() {
        let (_dir, cache) = temp_cache();
        let hash = MerkleHash::from([0x42; 32]);
        let payload_digest = [0x24; 32];

        assert!(
            !cache
                .record_remote_xorb_proof(&hash, &payload_digest, 100, None, None)
                .unwrap()
        );
        assert!(
            !cache
                .remote_xorb_proof_matches(&hash, &payload_digest, 100, Some("etag-a"), None)
                .unwrap()
        );

        assert!(
            cache
                .record_remote_xorb_proof(&hash, &payload_digest, 100, Some("etag-a"), None,)
                .unwrap()
        );
        assert!(
            cache
                .remote_xorb_proof_matches(&hash, &payload_digest, 100, Some("etag-a"), None)
                .unwrap()
        );
        assert!(
            !cache
                .remote_xorb_proof_matches(&hash, &payload_digest, 101, Some("etag-a"), None)
                .unwrap()
        );
        assert!(
            !cache
                .remote_xorb_proof_matches(&hash, &payload_digest, 100, Some("etag-b"), None)
                .unwrap()
        );
        assert!(
            !cache
                .remote_xorb_proof_matches(&hash, &payload_digest, 100, None, None)
                .unwrap()
        );
    }

    #[test]
    fn remote_xorb_proof_cache_tolerates_concurrent_open() {
        let (_dir, cache) = temp_cache();
        let cache = std::sync::Arc::new(cache);
        let handles = (0u8..64)
            .map(|seed| {
                let cache = std::sync::Arc::clone(&cache);
                std::thread::spawn(move || {
                    let hash = MerkleHash::from([seed; 32]);
                    let payload_digest = [seed.wrapping_add(1); 32];
                    let e_tag = format!("etag-{seed}");
                    for round in 0u64..8 {
                        let xorb_bytes = 1024 + round;
                        assert!(
                            cache
                                .record_remote_xorb_proof(
                                    &hash,
                                    &payload_digest,
                                    xorb_bytes,
                                    Some(e_tag.as_str()),
                                    None
                                )
                                .unwrap()
                        );
                        assert!(
                            cache
                                .remote_xorb_proof_matches(
                                    &hash,
                                    &payload_digest,
                                    xorb_bytes,
                                    Some(e_tag.as_str()),
                                    None
                                )
                                .unwrap()
                        );
                    }
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[tokio::test]
    async fn remote_xorb_index_requires_matching_identity_token() {
        let (dir, cache) = temp_cache();
        let (_chunk_hashes, xorb_hash, data) =
            test_xorb_with_chunks(&[b"remote index one", b"remote index two"]);
        let path = dir.path().join("remote-index.xorb");
        tokio::fs::write(&path, &data).await.unwrap();
        let (chunks, actual_hash) = read_xorb_file_metadata(&path, data.len() as u64)
            .await
            .unwrap();
        assert_eq!(actual_hash, xorb_hash);
        let payload_digest = XorbParser::parse(data.clone()).unwrap().payload_digest();

        assert!(
            !cache
                .record_remote_xorb_index(
                    &xorb_hash,
                    &payload_digest,
                    data.len() as u64,
                    None,
                    None,
                    &chunks,
                )
                .unwrap()
        );
        assert!(
            cache
                .cached_remote_xorb_index(&xorb_hash, data.len() as u64, Some("etag-a"), None)
                .unwrap()
                .is_none()
        );

        assert!(
            cache
                .record_remote_xorb_index(
                    &xorb_hash,
                    &payload_digest,
                    data.len() as u64,
                    Some("etag-a"),
                    None,
                    &chunks,
                )
                .unwrap()
        );
        let cached = cache
            .cached_remote_xorb_index(&xorb_hash, data.len() as u64, Some("etag-a"), None)
            .unwrap()
            .unwrap();
        assert_eq!(cached.xorb_hash, xorb_hash);
        assert_eq!(cached.payload_digest, payload_digest);
        assert_eq!(cached.chunks, chunks);
        assert!(
            cache
                .cached_remote_xorb_index(&xorb_hash, data.len() as u64 + 1, Some("etag-a"), None)
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .cached_remote_xorb_index(&xorb_hash, data.len() as u64, Some("etag-b"), None)
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .cached_remote_xorb_index(&xorb_hash, data.len() as u64, None, None)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn put_bytes_and_get_xorb_roundtrip() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"bytes-backed xorb payload");
        let key = CacheKey::Xorb(hash);

        cache.put_bytes(&key, data.clone()).await.unwrap();
        assert!(cache.contains_verified(&key).await);

        let fetched = cache
            .get_or_fetch(&key, || async { panic!("should not fetch") })
            .await
            .unwrap();
        assert_eq!(fetched, data);
    }

    #[tokio::test]
    async fn read_through_xorb_cache_does_not_publish_add_candidate() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"remote read-through xorb payload");
        let chunk_hash = XorbParser::parse(data.clone())
            .unwrap()
            .chunk_meta(0)
            .unwrap()
            .hash;
        let fetches = AtomicUsize::new(0);

        let fetched = cache
            .get_or_fetch_read_xorb_with(&hash, || async {
                fetches.fetch_add(1, Ordering::Relaxed);
                Ok::<_, CacheError>(data.clone())
            })
            .await
            .unwrap();
        assert_eq!(fetched, data);
        assert!(
            cache
                .cached_xorb_candidates_for_chunks(&[chunk_hash])
                .await
                .unwrap()
                .is_empty()
        );

        let warm = cache
            .get_or_fetch_read_xorb_with::<_, _, CacheError>(&hash, || async {
                panic!("warm read-through xorb should not fetch")
            })
            .await
            .unwrap();
        assert_eq!(warm, data);
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
        assert!(
            cache
                .cached_xorb_candidates_for_chunks(&[chunk_hash])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn read_through_xorb_defers_chunk_validation_to_reconstruction() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"deferred read-through chunk validation");
        let mut corrupt = data.to_vec();
        let footer_start = corrupt.len() - FOOTER_SIZE;
        let payload_len = u64::from_le_bytes(
            corrupt[footer_start + 4..footer_start + 12]
                .try_into()
                .unwrap(),
        ) as usize;
        corrupt[0] ^= 0xFF;
        let payload_digest = blake3::hash(&corrupt[..payload_len]);
        corrupt[footer_start + 12..footer_start + 44].copy_from_slice(payload_digest.as_bytes());
        let corrupt = Bytes::from(corrupt);

        let fetched = cache
            .get_or_fetch_read_xorb_with(&hash, || async { Ok::<_, CacheError>(corrupt.clone()) })
            .await
            .unwrap();

        assert_eq!(fetched, corrupt);
        assert!(
            XorbParser::parse(fetched)
                .unwrap()
                .verify_all_chunks()
                .is_err()
        );
    }

    #[tokio::test]
    async fn put_xorb_file_and_get_roundtrip() {
        let (dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"file-backed xorb payload");
        let key = CacheKey::Xorb(hash);
        let source = dir.path().join("prepared.xorb");
        tokio::fs::write(&source, &data).await.unwrap();

        cache
            .put_xorb_file(&hash, &source, data.len() as u64)
            .await
            .unwrap();
        tokio::fs::remove_file(&source).await.unwrap();
        assert!(cache.contains_verified(&key).await);

        let fetched = cache
            .get_or_fetch(&key, || async { panic!("should not fetch") })
            .await
            .unwrap();
        assert_eq!(fetched, data);
    }

    #[tokio::test]
    async fn put_preverified_xorb_file_and_get_roundtrip() {
        let (dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"preverified file-backed xorb payload");
        let key = CacheKey::Xorb(hash);
        let source = dir.path().join("preverified.xorb");
        tokio::fs::write(&source, &data).await.unwrap();
        let payload_hash = *blake3::hash(&data).as_bytes();

        cache
            .put_preverified_xorb_file(&hash, &source, data.len() as u64, payload_hash)
            .await
            .unwrap();
        tokio::fs::remove_file(&source).await.unwrap();
        assert!(cache.contains_verified(&key).await);

        let fetched = cache
            .get_or_fetch(&key, || async { panic!("should not fetch") })
            .await
            .unwrap();
        assert_eq!(fetched, data);
    }

    #[tokio::test]
    async fn put_preverified_xorb_file_rejects_payload_digest_mismatch() {
        let (dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"preverified digest mismatch");
        let source = dir.path().join("preverified-mismatch.xorb");
        tokio::fs::write(&source, &data).await.unwrap();
        let wrong_payload_hash = [0x42u8; 32];

        let err = cache
            .put_preverified_xorb_file(&hash, &source, data.len() as u64, wrong_payload_hash)
            .await
            .unwrap_err();

        assert!(matches!(err, CacheError::CorruptObject { .. }));
        assert!(!cache.contains(&CacheKey::Xorb(hash)).await);
    }

    #[tokio::test]
    async fn put_preverified_xorb_file_rejects_wrong_xorb_hash() {
        let (dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"preverified wrong xorb hash");
        let source = dir.path().join("preverified-wrong-hash.xorb");
        tokio::fs::write(&source, &data).await.unwrap();
        let payload_hash = *blake3::hash(&data).as_bytes();
        let wrong_hash = MerkleHash::from([0x77u8; 32]);
        assert_ne!(wrong_hash, hash);

        let err = cache
            .put_preverified_xorb_file(&wrong_hash, &source, data.len() as u64, payload_hash)
            .await
            .unwrap_err();

        assert!(matches!(err, CacheError::HashMismatch { .. }));
        assert!(!cache.contains(&CacheKey::Xorb(wrong_hash)).await);
    }

    #[tokio::test]
    async fn put_xorb_file_rejects_corrupt_payload() {
        let (dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"corrupt file-backed xorb payload");
        let source = dir.path().join("corrupt-prepared.xorb");
        let mut corrupt = data.to_vec();
        corrupt[0] ^= 0xFF;
        tokio::fs::write(&source, corrupt).await.unwrap();

        let err = cache
            .put_xorb_file(&hash, &source, data.len() as u64)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            CacheError::CorruptObject { .. }
                | CacheError::HashMismatch { .. }
                | CacheError::Xet { .. }
        ));
        assert!(!cache.contains(&CacheKey::Xorb(hash)).await);
    }

    #[tokio::test]
    async fn contains_verified_evicts_corrupt_xorb_payload() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"packed xorb payload");
        let key = CacheKey::Xorb(hash);
        cache.put(&key, data.as_ref()).await.unwrap();

        let path = cache.hash_path(&key);
        let mut corrupt = data.to_vec();
        corrupt[0] ^= 0xFF;
        tokio::fs::write(&path, corrupt).await.unwrap();

        assert!(cache.contains(&key).await);
        assert!(!cache.contains_verified(&key).await);
        assert!(!cache.contains(&key).await);
    }

    #[tokio::test]
    async fn put_rejects_invalid_xorb_body() {
        let (_dir, cache) = temp_cache();
        let key = CacheKey::Xorb(MerkleHash::from([0x42u8; 32]));

        let err = cache.put(&key, b"not a serialized xorb").await.unwrap_err();

        assert!(matches!(err, CacheError::CorruptObject { .. }));
        assert!(!cache.contains(&key).await);
    }

    #[tokio::test]
    async fn put_rejects_corrupt_xorb_payload() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"valid xorb payload");
        let key = CacheKey::Xorb(hash);
        let mut corrupt = data.to_vec();
        corrupt[0] ^= 0xFF;

        let err = cache.put(&key, &corrupt).await.unwrap_err();

        assert!(matches!(
            err,
            CacheError::CorruptObject { .. } | CacheError::Xet { .. }
        ));
        assert!(!cache.contains(&key).await);
    }

    #[tokio::test]
    async fn corrupt_cached_xorb_evicts_and_refetches() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"fresh xorb payload");
        let key = CacheKey::Xorb(hash);
        let path = cache.hash_path(&key);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, b"corrupt xorb").await.unwrap();

        let fetched = cache
            .get_or_fetch(&key, || {
                let data = data.clone();
                async move { Ok(data) }
            })
            .await
            .unwrap();

        assert_eq!(fetched, data);
    }

    #[tokio::test]
    async fn get_xorb_range_if_present_reads_requested_bytes() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"packed xorb payload");
        let key = CacheKey::Xorb(hash);
        cache.put(&key, data.as_ref()).await.unwrap();

        let fetched = cache.get_xorb_range_if_present(&hash, 7..11).await.unwrap();
        assert_eq!(fetched.as_ref(), &data[7..11]);
    }

    #[tokio::test]
    async fn corrupt_cached_xorb_range_evicts_and_misses() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"packed xorb payload");
        let key = CacheKey::Xorb(hash);
        cache.put(&key, data.as_ref()).await.unwrap();
        let path = cache.hash_path(&key);
        tokio::fs::write(&path, b"bad cached xorb bytes")
            .await
            .unwrap();

        assert!(cache.get_xorb_range_if_present(&hash, 0..3).await.is_none());
        assert!(!cache.contains(&key).await);
    }

    /// Xorb hashes are aggregated node hashes over the chunk summary,
    /// not `compute_data_hash(xorb_bytes)`. The cache must not content-
    /// hash xorbs on read, otherwise every warmed entry would evict
    /// itself on the first lookup — defeating push-time cache warming.
    #[tokio::test]
    async fn xorb_cache_hit_does_not_verify_data_hash() {
        let (_dir, cache) = temp_cache();
        let (xorb_hash, bytes) = test_xorb(b"serialized xorb payload");
        assert_ne!(
            compute_data_hash(&bytes),
            xorb_hash,
            "test precondition: aggregated hash must differ from data hash",
        );

        let key = CacheKey::Xorb(xorb_hash);
        cache.put(&key, bytes.as_ref()).await.unwrap();

        // get_or_fetch should serve from disk WITHOUT calling the fetch
        // closure, even though compute_data_hash(bytes) != fake_aggregated_hash.
        let got = cache
            .get_or_fetch(&key, || async {
                panic!("should not fetch - cache should hit")
            })
            .await
            .unwrap();
        assert_eq!(got, bytes);

        // Second call also hits (exercises touch_mtime path).
        let got2 = cache
            .get_or_fetch(&key, || async {
                panic!("should not fetch - cache should hit")
            })
            .await
            .unwrap();
        assert_eq!(got2, bytes);
    }
}
