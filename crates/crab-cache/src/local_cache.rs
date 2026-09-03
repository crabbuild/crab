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

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use rusqlite::{Connection, OptionalExtension, params};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, warn};

use crate::catalog::PayloadRead;
use crate::error::{CacheError, Result};
use crate::key::CacheKey;
use crab_xet::hash::{compute_data_hash, xorb_hash};
#[cfg(test)]
use crab_xet::xorb::builder::FOOTER_SIZE;
use crab_xet::xorb::format::{ChunkMeta, MAX_XORB_SIZE, MerkleHash};
use crab_xet::xorb::parser::XorbParser;

mod maintenance;
mod xorb_file;
use xorb_file::{read_xorb_file_metadata, verify_xorb_file_identity, verify_xorb_file_payload};

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

/// Parsed remote xorb metadata proven against a provider object identity token.
#[derive(Debug, Clone)]
pub struct CachedRemoteXorbIndex {
    pub xorb_hash: MerkleHash,
    pub payload_digest: [u8; 32],
    pub chunks: Vec<ChunkMeta>,
}

/// Per-repo local cache with hash-verified reads and LRU eviction.
///
/// Async entry points delegate scans and file verification to cancellable
/// blocking workers. Writes publish complete private files atomically.
pub struct LocalCache {
    root: PathBuf,
    catalog: crate::catalog::CacheCatalog,
    /// Shared byte ceiling for large data objects: chunk fragments and xorbs.
    chunk_max_bytes: u64,
    shard_max_bytes: Option<u64>,
    fill_locks: Box<[tokio::sync::Mutex<()>]>,
}

impl LocalCache {
    /// Create a cache rooted at `root` with default limits.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            catalog: crate::catalog::CacheCatalog::new(root.clone(), DEFAULT_CHUNK_MAX_BYTES),
            root,
            chunk_max_bytes: DEFAULT_CHUNK_MAX_BYTES,
            shard_max_bytes: None,
            fill_locks: new_fill_locks(),
        }
    }

    /// Create a cache with explicit byte budgets.
    ///
    /// `chunk_max` is the shared ceiling for chunk fragments and xorbs.
    #[must_use]
    pub fn with_limits(root: PathBuf, chunk_max: u64, shard_max: Option<u64>) -> Self {
        Self {
            catalog: crate::catalog::CacheCatalog::new(root.clone(), chunk_max),
            root,
            chunk_max_bytes: chunk_max,
            shard_max_bytes: shard_max,
            fill_locks: new_fill_locks(),
        }
    }

    /// Root directory of this cache.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Product byte budget shared by object and decoded-range caches.
    #[must_use]
    pub fn max_bytes(&self) -> u64 {
        self.catalog.max_bytes()
    }

    /// Read advisory clean-filter bloom bytes without creating a missing cache.
    ///
    /// The caller validates the bloom encoding and treats cache errors as misses.
    pub fn read_clean_bloom(&self) -> Result<Option<Vec<u8>>> {
        let path = self.root.join("hints/clean-bloom.bin");
        match crate::private_fs::read_bounded_sync(&self.root, &path, 1024 * 1024) {
            Ok(data) => Ok(Some(data)),
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Publish advisory clean-filter bloom bytes through private, budgeted storage.
    ///
    /// Concurrent sessions may replace this hint; a hit still needs remote proof.
    pub fn put_clean_bloom(&self, data: &[u8]) -> Result<()> {
        let path = self.root.join("hints/clean-bloom.bin");
        if data.len() > 1024 * 1024 {
            return Err(CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: "clean bloom exceeds the 1 MiB safety limit".to_owned(),
            });
        }
        self.catalog.write_sync(&path, "bloom", data)
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

    /// Cache a committed remote xorb after verifying its identity and serialized digest.
    ///
    /// Read-through consumers verify the requested decoded chunks separately;
    /// this path does not decompress unrelated chunks merely to install the file.
    pub async fn put_read_xorb(&self, hash: &MerkleHash, data: Bytes) -> Result<()> {
        let key = CacheKey::Xorb(*hash);
        let path = self.hash_path(&key);
        enforce_size_limit(&key, &data, Some(MAX_XORB_SIZE as u64))?;
        verify_xorb_serialized_payload(&data, hash)?;
        self.atomic_write(&path, &data).await
    }

    /// Read an existing complete xorb without creating a cache entry on miss.
    ///
    /// Metadata identity is checked here; callers decoding ranges remain
    /// responsible for verifying the selected chunk payloads.
    ///
    /// # Errors
    ///
    /// Returns an I/O or corruption error when an existing entry cannot be
    /// read as the requested content-addressed xorb. Repair targets only the
    /// failed read's file; busy entries and later replacements are retained.
    pub async fn get_read_xorb_if_present(&self, hash: &MerkleHash) -> Result<Option<Bytes>> {
        let key = CacheKey::Xorb(*hash);
        let path = self.hash_path(&key);
        let Some(data) =
            read_file_bounded_result(&self.root, &path, MAX_XORB_SIZE as u64, |data| {
                verify_xorb_identity(data, hash)
            })
            .await?
        else {
            return Ok(None);
        };
        crate::private_fs::touch(&self.root, &path).await;
        Ok(Some(data))
    }

    async fn try_read_key_limited(&self, key: &CacheKey, max_bytes: Option<u64>) -> Option<Bytes> {
        if !crate::root::private_cache_directory_is_safe(&self.root) {
            warn!(
                family = cache_family_for_path(&self.root, &self.hash_path(key)),
                operation = "validate-root",
                path = %self.root.display(),
                recovery = "disable-cache-and-use-origin",
                "unsafe local cache root"
            );
            return None;
        }
        match key {
            CacheKey::Chunk(hash) | CacheKey::Shard(hash) => {
                self.try_read_verified_limited(&self.hash_path(key), hash, max_bytes)
                    .await
            }
            CacheKey::Xorb(hash) => {
                self.try_read_xorb_bytes_limited(&self.hash_path(key), hash, max_bytes)
                    .await
            }
            CacheKey::Stage(_) => {
                let path = self.hash_path(key);
                let limit = max_bytes.unwrap_or(MAX_CACHE_STAGE_BYTES);
                let data = read_file_bounded_result(&self.root, &path, limit, |_| Ok(()))
                    .await
                    .ok()??;
                crate::private_fs::touch(&self.root, &path).await;
                Some(data)
            }
            CacheKey::Manifest { name, etag } => {
                let want_etag = etag.as_deref()?;
                let cached_etag =
                    read_string_if_exists(&self.root, &self.manifest_etag_path(name)).await?;
                if cached_etag.trim() != want_etag {
                    return None;
                }
                let path = self.manifest_data_path(name);
                // A body-read failure cannot authorize deleting a later ETag
                // publication. Missing bodies are misses regardless of ETag.
                let limit = max_bytes.unwrap_or(MAX_CACHE_MANIFEST_BYTES);
                let data = read_file_bounded_result(&self.root, &path, limit, |_| Ok(()))
                    .await
                    .ok()??;
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
        if expected_len > self.catalog.max_bytes() {
            return Ok(());
        }

        let path = self.hash_path(&CacheKey::Xorb(*hash));
        let Some(reservation) = self.catalog.reserve(&path, expected_len).await? else {
            return Ok(());
        };
        let temporary = reservation.pending_file().await?;
        let mut file = temporary.file()?;
        copy_xorb_temp_file_with_blake3(source, &mut file, expected_len).await?;
        verify_xorb_file_payload(file, &path, expected_len, hash).await?;
        let reservation = temporary.commit().await?;
        self.record_completed_file("xorb", &path, hash.hex(), expected_len, reservation)
            .await;
        Ok(())
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
        if expected_len > self.catalog.max_bytes() {
            return Ok(());
        }

        let path = self.hash_path(&CacheKey::Xorb(*hash));
        let Some(reservation) = self.catalog.reserve(&path, expected_len).await? else {
            return Ok(());
        };
        let temporary = reservation.pending_file().await?;
        let mut file = temporary.file()?;
        let actual_blake3 =
            copy_xorb_temp_file_with_blake3(source, &mut file, expected_len).await?;
        if actual_blake3 != expected_blake3 {
            return Err(CacheError::CorruptObject {
                path: source.display().to_string(),
                reason: "copied xorb payload digest did not match preverified digest".to_owned(),
            });
        }
        drop(verify_xorb_file_identity(file, &path, expected_len, hash).await?);
        let reservation = temporary.commit().await?;
        self.record_completed_file("xorb", &path, hash.hex(), expected_len, reservation)
            .await;
        Ok(())
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
        private_cache_file_metadata(&self.root, &path)
            .await
            .ok()
            .flatten()
            .is_some()
    }

    /// Return the size of an existing cache entry without reading its body.
    pub async fn cached_size(&self, key: &CacheKey) -> Result<Option<u64>> {
        let path = match key {
            CacheKey::Chunk(_) | CacheKey::Shard(_) | CacheKey::Xorb(_) | CacheKey::Stage(_) => {
                self.hash_path(key)
            }
            CacheKey::Manifest { name, .. } => self.manifest_data_path(name),
        };
        Ok(private_cache_file_metadata(&self.root, &path)
            .await?
            .map(|metadata| metadata.len()))
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
        crate::catalog::remove_file(&self.root, &path).await?;
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
        let (entry, file) = PayloadRead::open(&self.root, &path).await.ok()?;
        let result: Result<_> = async {
            let bytes = file.metadata().await?.len();
            // Request bounds do not prove the cached object is corrupt.
            if bytes > MAX_XORB_SIZE as u64 || range.end > bytes {
                return Ok(None);
            }
            let mut file = verify_xorb_file_identity(file, &path, bytes, hash).await?;
            file.seek(std::io::SeekFrom::Start(range.start)).await?;
            let mut buf = vec![0u8; len];
            file.read_exact(&mut buf).await?;
            Ok(Some((Bytes::from(buf), bytes)))
        }
        .await;
        let data = entry.finish(result).await.ok()??;
        crate::private_fs::touch(&self.root, &path).await;
        Some(data)
    }

    /// Return verified chunk metadata and payload length for a cached xorb.
    ///
    /// Invalid entries return errors and repair targets the failed read's file,
    /// retaining busy entries and later replacements.
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
        let (entry, file) = match PayloadRead::open(&self.root, &path).await {
            Ok(opened) => opened,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let result = async {
            let bytes = file.metadata().await?.len();
            let (chunks, actual) = read_xorb_file_metadata(file, &path, bytes).await?;
            if actual != *hash {
                return Err(CacheError::HashMismatch {
                    requested: hash.hex(),
                    actual: actual.hex(),
                });
            }
            let payload_len = chunks.last().map_or(0, |chunk| {
                u64::from(chunk.offset) + u64::from(chunk.compressed_len)
            });
            Ok((chunks, payload_len))
        }
        .await;
        let metadata = entry.finish(result).await?;
        crate::private_fs::touch(&self.root, &path).await;
        Ok(Some(metadata))
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
        read_string_if_exists(&self.root, &self.manifest_etag_path(name)).await
    }

    async fn try_read_verified_limited(
        &self,
        path: &Path,
        expected: &MerkleHash,
        max_bytes: Option<u64>,
    ) -> Option<Bytes> {
        let max_bytes = max_bytes?;
        let data = read_file_bounded_result(&self.root, path, max_bytes, |data| {
            verify_data_hash(data, expected)
        })
        .await
        .ok()??;
        crate::private_fs::touch(&self.root, path).await;
        Some(data)
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
        let data = read_file_bounded_result(&self.root, path, max_bytes, |data| {
            verify_xorb_identity(data, expected)
        })
        .await
        .ok()??;
        crate::private_fs::touch(&self.root, path).await;
        Some(data)
    }

    async fn try_verify_xorb_payload_file(&self, path: &Path, expected: &MerkleHash) -> bool {
        let Ok((entry, file)) = PayloadRead::open(&self.root, path).await else {
            return false;
        };
        let result = async {
            let bytes = file.metadata().await?.len();
            verify_xorb_file_payload(file, path, bytes, expected).await
        }
        .await;
        if entry.finish(result).await.is_ok() {
            crate::private_fs::touch(&self.root, path).await;
            return true;
        }
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
        if data.len() as u64 > self.catalog.max_bytes() {
            debug!(
                family = cache_family_for_path(&self.root, path),
                bytes = data.len(),
                max_bytes = self.catalog.max_bytes(),
                "cache entry exceeds product budget; serving without caching"
            );
            return Ok(());
        }
        let Some(reservation) = self.catalog.reserve(path, data.len() as u64).await? else {
            debug!(
                family = cache_family_for_path(&self.root, path),
                bytes = data.len(),
                max_bytes = self.catalog.max_bytes(),
                "cache budget has no room for entry; serving without caching"
            );
            return Ok(());
        };
        let reservation = reservation.write(data).await?;
        self.record_completed_file(
            cache_family_for_path(&self.root, path),
            path,
            path.file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            data.len() as u64,
            reservation,
        )
        .await;
        Ok(())
    }

    async fn record_completed_file(
        &self,
        family: &'static str,
        path: &Path,
        logical_key: String,
        size: u64,
        reservation: crate::catalog::CacheReservation,
    ) {
        if let Err(error) = self
            .catalog
            .record_and_maintain(family, logical_key, size, reservation)
            .await
        {
            warn!(
                family,
                operation = "record-and-maintain",
                path = %path.display(),
                recovery = "retain-entry-and-continue",
                %error,
                "cache maintenance failed"
            );
        }
    }
}

// --- free-standing helpers ---

fn cache_family_for_path(root: &Path, path: &Path) -> &'static str {
    match path
        .strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
    {
        Some("chunks") => "chunk",
        Some("xorbs") => "xorb",
        Some("shards") => "shard",
        Some("manifests") => "manifest",
        Some("stages") => "stage",
        _ => "other",
    }
}

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

async fn read_file_bounded_result(
    root: &Path,
    path: &Path,
    max_bytes: u64,
    validate: impl FnOnce(&Bytes) -> Result<()>,
) -> Result<Option<Bytes>> {
    let (entry, file) = match PayloadRead::open(root, path).await {
        Ok(opened) => opened,
        Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let result = async {
        let metadata = file.metadata().await?;
        if metadata.len() > max_bytes {
            return Err(CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: format!(
                    "file is {} bytes; bounded read supports at most {max_bytes} bytes",
                    metadata.len()
                ),
            });
        }
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
        let data = Bytes::from(data);
        validate(&data)?;
        Ok(data)
    }
    .await;
    entry.finish(result).await.map(Some)
}

async fn private_cache_file_metadata(
    root: &Path,
    path: &Path,
) -> Result<Option<std::fs::Metadata>> {
    let file = match crate::private_fs::open_read(root, path).await {
        Ok(file) => file,
        Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    Ok(Some(file.metadata().await?))
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
    tmp_file: &mut tokio::fs::File,
    expected_len: u64,
) -> Result<[u8; 32]> {
    if expected_len > MAX_XORB_SIZE as u64 {
        return Err(CacheError::CorruptObject {
            path: source.display().to_string(),
            reason: format!("xorb is {expected_len} bytes; format limit is {MAX_XORB_SIZE} bytes"),
        });
    }
    let mut source_file = tokio::fs::File::open(source).await?.take(expected_len + 1);
    let mut hasher = blake3::Hasher::new();
    let mut copied = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = source_file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| CacheError::CorruptObject {
                path: source.display().to_string(),
                reason: "copied xorb byte count overflowed".to_owned(),
            })?;
        if copied > expected_len {
            return Err(CacheError::CorruptObject {
                path: source.display().to_string(),
                reason: format!("source exceeds its expected {expected_len} bytes"),
            });
        }
        tmp_file.write_all(chunk).await?;
        hasher.update(chunk);
    }
    tmp_file.sync_all().await?;

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

fn open_xorb_index(index_path: &Path) -> Result<crate::private_fs::Database> {
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

fn open_xorb_index_once(index_path: &Path) -> Result<crate::private_fs::Database> {
    let root =
        index_path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| CacheError::UnsafeRoot {
                path: index_path.display().to_string(),
                reason: "xorb index has no cache root".into(),
            })?;
    let conn = crate::private_fs::open_database(
        root,
        index_path,
        crate::private_fs::DatabaseMode::Create,
        XORB_INDEX_BUSY_TIMEOUT,
    )?;

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
        // v1.0.1 shipped this table beside the live remote-proof tables. It
        // remains only until an explicit migration can preserve those proofs;
        // runtime code no longer reads or writes local placement rows.
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
async fn read_string_if_exists(root: &Path, path: &Path) -> Option<String> {
    let bytes = read_file_bounded_result(root, path, 16 * 1024, |_| Ok(()))
        .await
        .ok()??;
    std::str::from_utf8(&bytes).ok().map(str::to_owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    #[cfg(unix)]
    mod maintenance;
    #[cfg(unix)]
    mod read_repair;

    fn temp_cache() -> (tempfile::TempDir, LocalCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(dir.path().join("cache"));
        (dir, cache)
    }

    #[tokio::test]
    async fn clean_bloom_is_private_budgeted_and_disposable() {
        let (_dir, cache) = temp_cache();
        assert!(cache.read_clean_bloom().unwrap().is_none());
        assert!(!cache.root.exists());
        cache.put_clean_bloom(b"hint").unwrap();
        let reopened = LocalCache::new(cache.root.clone());
        assert_eq!(reopened.read_clean_bloom().unwrap().unwrap(), b"hint");
        assert_eq!(
            crate::CacheCatalog::read_only_stats(&cache.root)
                .unwrap()
                .total_bytes,
            4
        );
        let report = crate::clean_cache(
            &cache.root,
            false,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(report.bytes_reclaimed, 4);
        assert!(reopened.read_clean_bloom().unwrap().is_none());
    }

    #[test]
    fn clean_bloom_over_budget_does_not_create_a_root() {
        let (dir, _cache) = temp_cache();
        let cache = LocalCache::with_limits(dir.path().join("tiny"), 1, None);
        cache.put_clean_bloom(b"hint").unwrap();
        assert!(!cache.root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn clean_bloom_never_follows_unsafe_roots_or_leaf_links() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let (dir, cache) = temp_cache();
        let target = dir.path().join("target");
        std::fs::write(&target, b"retained").unwrap();
        symlink(dir.path(), &cache.root).unwrap();
        assert!(cache.put_clean_bloom(b"hint").is_err());
        assert!(cache.read_clean_bloom().is_err());
        std::fs::remove_file(&cache.root).unwrap();

        std::fs::create_dir(&cache.root).unwrap();
        std::fs::set_permissions(&cache.root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(cache.put_clean_bloom(b"hint").is_err());
        assert!(cache.read_clean_bloom().is_err());
        assert_eq!(
            std::fs::metadata(&cache.root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        std::fs::remove_dir(&cache.root).unwrap();

        cache.put_clean_bloom(b"hint").unwrap();
        let path = cache.root.join("hints/clean-bloom.bin");
        std::fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        assert!(cache.read_clean_bloom().is_err());
        // Descriptor-relative rename replaces the link itself, never its
        // target. Reads reject the link; a fresh private publication is safe.
        cache.put_clean_bloom(b"replacement").unwrap();
        assert_eq!(cache.read_clean_bloom().unwrap().unwrap(), b"replacement");
        assert_eq!(std::fs::read(&target).unwrap(), b"retained");
    }

    #[test]
    fn clean_bloom_rejects_oversized_reads_and_writes() {
        let (_dir, cache) = temp_cache();
        let oversized = vec![0; 1024 * 1024 + 1];
        assert!(cache.put_clean_bloom(&oversized).is_err());
        assert!(!cache.root.exists());
        cache.put_clean_bloom(b"hint").unwrap();
        let path = cache.root.join("hints/clean-bloom.bin");
        std::fs::write(path, &oversized).unwrap();
        assert!(matches!(
            cache.read_clean_bloom(),
            Err(CacheError::CorruptObject { .. })
        ));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn put_creates_private_directories_and_file() {
        use std::os::unix::fs::MetadataExt as _;

        let (_dir, cache) = temp_cache();
        let data = b"private cache bytes";
        let key = CacheKey::Chunk(compute_data_hash(data));

        cache.put(&key, data).await.unwrap();

        let path = cache.hash_path(&key);
        assert_eq!(
            std::fs::symlink_metadata(path.parent().unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(path).unwrap().mode() & 0o777,
            0o600
        );
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
        cache
            .put_unchecked_for_test(&key, b"corrupted")
            .await
            .unwrap();

        // get_or_fetch should detect mismatch, evict, and refetch.
        let result = cache
            .get_or_fetch(&key, || async { Ok(Bytes::from_static(good_data)) })
            .await
            .unwrap();
        assert_eq!(&result[..], good_data);
    }

    #[tokio::test]
    async fn copied_xorb_cannot_exceed_its_reserved_bytes() {
        let (dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"source is longer than its advertised size");
        let source = dir.path().join("source.xorb");
        tokio::fs::write(&source, &data).await.unwrap();
        let result = cache.put_xorb_file(&hash, &source, 8).await;
        assert!(matches!(result, Err(CacheError::CorruptObject { .. })));
        let destination = cache.hash_path(&CacheKey::Xorb(hash));
        assert_eq!(
            std::fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .count(),
            0
        );
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

        // Write-time maintenance keeps the product budget bounded; an
        // explicit prune is idempotent after quiescence.
        let _ = cache.prune().await.unwrap();
        let cache_stats = cache.stats().await.unwrap();
        assert!(cache_stats.chunk_bytes <= 100);
        assert!(!cache.contains(&keys[0]).await);
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

        for i in 0u8..4 {
            let data = vec![i; 40];
            let (hash, data) = test_xorb(&data);
            let key = CacheKey::Xorb(hash);
            cache.put(&key, data.as_ref()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let cache = LocalCache::with_limits(cache.root.clone(), 1, None);
        let stats = cache.prune().await.unwrap();
        assert!(stats.xorbs_evicted > 0);
        assert!(stats.bytes_freed > 0);

        let cache_stats = cache.stats().await.unwrap();
        assert!(cache_stats.xorb_bytes <= 1);
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

        let (xorb_hash, xorb_data) = test_xorb(&[3; 50]);
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

        let chunk_data = vec![1u8; 80];
        let chunk_hash = compute_data_hash(&chunk_data);
        cache
            .put(&CacheKey::Chunk(chunk_hash), &chunk_data)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (xorb_hash, xorb_data) = test_xorb(&[2u8; 80]);
        let max_bytes = xorb_data.len() as u64 + 1;
        cache
            .put(&CacheKey::Xorb(xorb_hash), xorb_data.as_ref())
            .await
            .unwrap();

        let cache = LocalCache::with_limits(cache.root.clone(), max_bytes, None);
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
    }

    #[tokio::test]
    async fn clean_removes_payloads_but_preserves_other_owners() {
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

        let report = crate::clean_cache(
            cache.root(),
            false,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(report.files_removed, 4);

        let stats = cache.stats().await.unwrap();
        assert_eq!(stats.chunk_bytes, 0);
        assert_eq!(stats.xorb_bytes, 0);
        assert_eq!(stats.stage_bytes, 0);
        assert_eq!(stats.manifest_count, 0);
        assert_eq!(tokio::fs::read(repo_index).await.unwrap(), b"sqlite");
        assert_eq!(
            tokio::fs::read(cache.root().join("bloom.bin"))
                .await
                .unwrap(),
            b"bloom"
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
    async fn xorb_index_rejects_non_v1_schema_without_mutation() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"strict v1 xorb index");
        cache
            .record_remote_xorb_proof(
                &hash,
                blake3::hash(&data).as_bytes(),
                data.len() as u64,
                Some("origin-etag"),
                None,
            )
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
            .query_row("SELECT COUNT(1) FROM remote_xorb_proof", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(rows > 0);
    }

    #[tokio::test]
    async fn every_xorb_writer_leaves_local_placements_unused_and_remote_proofs_reusable() {
        let (temp, cache) = temp_cache();
        let proof_hash = MerkleHash::from([0x42; 32]);
        let proof_digest = [0x24; 32];
        assert!(
            cache
                .record_remote_xorb_proof(
                    &proof_hash,
                    &proof_digest,
                    41,
                    Some("origin-etag"),
                    None,
                )
                .unwrap()
        );
        let conn = open_xorb_index(&cache.xorb_index_path()).unwrap();
        let placements = || {
            conn.query_row("SELECT COUNT(*) FROM xorb_index", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(placements(), 0);

        let (put_hash, put) = test_xorb(b"put");
        cache.put(&CacheKey::Xorb(put_hash), &put).await.unwrap();
        let (bytes_hash, bytes) = test_xorb(b"put bytes");
        cache
            .put_bytes(&CacheKey::Xorb(bytes_hash), bytes)
            .await
            .unwrap();
        let (fetch_hash, fetch) = test_xorb(b"fetch");
        cache
            .get_or_fetch(&CacheKey::Xorb(fetch_hash), || async { Ok(fetch) })
            .await
            .unwrap();
        let (file_hash, file) = test_xorb(b"file");
        let file_path = temp.path().join("file.xorb");
        std::fs::write(&file_path, &file).unwrap();
        cache
            .put_xorb_file(&file_hash, &file_path, file.len() as u64)
            .await
            .unwrap();
        let (verified_hash, verified) = test_xorb(b"preverified file");
        let verified_path = temp.path().join("verified.xorb");
        std::fs::write(&verified_path, &verified).unwrap();
        cache
            .put_preverified_xorb_file(
                &verified_hash,
                &verified_path,
                verified.len() as u64,
                *blake3::hash(&verified).as_bytes(),
            )
            .await
            .unwrap();
        let (read_hash, read) = test_xorb(b"read-through");
        cache.put_read_xorb(&read_hash, read).await.unwrap();

        assert_eq!(placements(), 0);
        assert!(
            cache
                .remote_xorb_proof_matches(
                    &proof_hash,
                    &proof_digest,
                    41,
                    Some("origin-etag"),
                    None,
                )
                .unwrap()
        );
    }

    #[tokio::test]
    async fn concurrent_xorb_writers_without_placement_metadata_remain_present() {
        let (_temp, cache) = temp_cache();
        let xorbs = (0..4)
            .map(|index| test_xorb(format!("concurrent xorb {index}").as_bytes()))
            .collect::<Vec<_>>();
        let keys = xorbs
            .iter()
            .map(|(hash, _)| CacheKey::Xorb(*hash))
            .collect::<Vec<_>>();

        tokio::try_join!(
            cache.put_bytes(&keys[0], xorbs[0].1.clone()),
            cache.put_bytes(&keys[1], xorbs[1].1.clone()),
            cache.put_bytes(&keys[2], xorbs[2].1.clone()),
            cache.put_bytes(&keys[3], xorbs[3].1.clone()),
        )
        .unwrap();

        for (hash, _) in xorbs {
            assert!(cache.contains(&CacheKey::Xorb(hash)).await);
        }
    }

    #[cfg(unix)]
    #[test]
    fn xorb_index_rejects_database_links_without_changing_the_target() {
        use std::os::unix::fs::PermissionsExt as _;
        let (temp, cache) = temp_cache();
        let index = cache.xorb_index_path();
        crate::ensure_private_cache_directory(index.parent().unwrap()).unwrap();
        let target = temp.path().join("sentinel");
        std::fs::write(&target, b"not disposable").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&target, &index).unwrap();
        assert!(matches!(
            open_xorb_index(&index),
            Err(CacheError::UnsafeRoot { .. })
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"not disposable");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
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
        let file = tokio::fs::File::open(&path).await.unwrap();
        let (chunks, actual_hash) = read_xorb_file_metadata(file, &path, data.len() as u64)
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
    async fn read_xorb_cache_does_not_initialize_proof_database() {
        let (_dir, cache) = temp_cache();
        let (hash, data) = test_xorb(b"remote read-through xorb payload");
        cache.put_read_xorb(&hash, data.clone()).await.unwrap();

        let warm = cache
            .get_read_xorb_if_present(&hash)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(warm, data);
        assert!(!cache.xorb_index_path().exists());
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

        cache.put_read_xorb(&hash, corrupt.clone()).await.unwrap();
        let fetched = cache
            .get_read_xorb_if_present(&hash)
            .await
            .unwrap()
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
        cache
            .put_unchecked_for_test(&key, b"corrupt xorb")
            .await
            .unwrap();

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
