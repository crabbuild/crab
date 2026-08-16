//! Chunk-hash keyed cache, backed by xet-core's on-disk `DiskCache`.
//!
//! This is an adapter, not a standalone LRU. The caller-facing API is
//! still keyed by a single chunk's [`MerkleHash`] and returns `Bytes`,
//! but every entry is stored in xet-core's `DiskCache` as a one-chunk
//! [`ChunkRange`] under the prefix `CRAB_CHUNK_PREFIX`. Sharing the
//! same on-disk directory as the xorb-range reconstruction cache means:
//!
//! - one eviction budget (`chunk_cache_bytes`) covers every cached byte;
//! - the `CacheManager` singleton hands out the same `Arc<dyn ChunkCache>`
//!   for a given directory, so every call site that opens the cache at
//!   the same path shares state.
//!
//! The old self-written LRU lived here with ~600 lines of index scan,
//! blake3 verify-once, and LRU bookkeeping. All of that is replaced by
//! `DiskCache`'s own CRC32 + range-indexed on-disk layout — simpler, and
//! proven by the reconstruction path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use tokio::runtime::Handle;
use tracing::{debug, warn};
use xet_client::cas_types::{ChunkRange, Key};
use xet_client::chunk_cache::ChunkCache as XetChunkCacheTrait;

use crate::core::error::{CrabError, Result};
use crab_xet::xorb::format::MerkleHash;

/// Prefix used for crab chunk-hash keyed entries in xet-core's cache.
///
/// The `Key` type in `xet_client::cas_types` carries both a `prefix` and
/// a `hash`. Reconstruction-path entries use xorb hashes keyed under the
/// empty-or-xorb prefix used by `FileReconstructor`; we use a distinct
/// prefix so the two keyspaces don't collide even when their hashes
/// happen to match.
const CRAB_CHUNK_PREFIX: &str = "crab-chunk";

/// Default chunk cache ceiling: 4 GiB.
///
/// Kept identical to the prior self-written cache so existing callers
/// behave the same when they pass `None`.
const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Chunk-hash keyed cache backed by xet-core's [`DiskCache`].
///
/// Thread-safe — the inner `Arc<dyn ChunkCache>` handles all locking.
/// Calls bridge sync to async via `block_in_place` + `block_on`; this
/// is safe on crab's multi-thread tokio runtime. See
/// [`crate::core::context::AppContext`] for the runtime setup.
pub struct ChunkCache {
    /// Directory backing the underlying `DiskCache`.
    dir: PathBuf,
    /// Configured eviction budget.
    max_bytes: u64,
    /// Shared trait-object handle. Same `Arc` as the one returned to
    /// any other subsystem opening the same directory.
    inner: Arc<dyn XetChunkCacheTrait>,
}

impl std::fmt::Debug for ChunkCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkCache")
            .field("dir", &self.dir)
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

impl ChunkCache {
    /// Open (or create) the chunk cache at `dir` with the given byte budget.
    ///
    /// Goes through the xet-core `CacheManager` singleton so callers that
    /// open the same directory share the same underlying `DiskCache`.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Internal`] if the directory cannot be
    /// created or the underlying cache fails to initialize (invalid
    /// size, disk errors during index rebuild).
    pub fn open(dir: PathBuf, max_bytes: Option<u64>) -> Result<Self> {
        let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

        let inner = crab_cache::XetChunkCacheHandle::open(dir.clone(), max_bytes)
            .map_err(|e| {
                CrabError::Internal(format!(
                    "failed to initialize chunk cache at {}: {e}",
                    dir.display(),
                ))
            })?
            .cache;

        debug!(
            dir = %dir.display(),
            max_bytes,
            "chunk cache opened (xet-core backed)"
        );

        Ok(Self {
            dir,
            max_bytes,
            inner,
        })
    }

    /// Fetch a cached chunk by its blake3 hash.
    ///
    /// Returns `None` on miss, on a backend error, or when the caller is
    /// not inside a tokio runtime (no `Handle::try_current`). Errors are
    /// logged at `warn!` — callers treat any `None` as "fetch it again",
    /// matching the previous contract.
    pub fn get(&self, hash: &MerkleHash) -> Option<Bytes> {
        let key = make_key(hash);
        let range = ChunkRange::new(0, 1);

        let result = block_on_async(async { self.inner.get(&key, &range).await })?;

        match result {
            Ok(Some(cache_range)) => Some(Bytes::from(cache_range.data)),
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, "chunk cache get failed");
                None
            }
        }
    }

    /// Store a chunk in the cache, keyed by its blake3 hash.
    ///
    /// Silently drops on backend error so the caller's reconstruction
    /// path is not blocked by cache-layer problems — matches prior
    /// semantics. The `put` is async under the hood; callers are on
    /// the multi-thread runtime, so blocking briefly is acceptable.
    pub fn put(&self, hash: MerkleHash, data: Bytes) {
        let key = make_key(&hash);
        let range = ChunkRange::new(0, 1);
        // xet-core expects offsets[0] = 0 and offsets[last] = data.len().
        let indices: [u32; 2] = [0, data.len() as u32];

        let Some(result) =
            block_on_async(async { self.inner.put(&key, &range, &indices, &data).await })
        else {
            return;
        };

        if let Err(e) = result {
            warn!(error = %e, "chunk cache put failed");
        }
    }

    /// Return `true` if the chunk is present in the cache.
    ///
    /// Implemented as a probing `get` — xet-core does not expose a
    /// cheaper metadata lookup on the trait. `contains` callers in the
    /// hydration path only use this to skip already-cached entries
    /// during prefetch, so the extra read is not hot.
    pub fn contains(&self, hash: &MerkleHash) -> bool {
        self.get(hash).is_some()
    }

    /// Cache directory path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Configured maximum cache size in bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Shared trait-object handle — useful for subsystems that want to
    /// pass the same cache to `FileReconstructor::with_chunk_cache`.
    ///
    /// Because `CacheManager` dedupes by directory, this `Arc` points
    /// at the same `DiskCache` any other caller gets for the same path.
    pub fn xet_handle(&self) -> Arc<dyn XetChunkCacheTrait> {
        Arc::clone(&self.inner)
    }
}

/// Build a crab-prefixed xet-core key for a chunk hash.
fn make_key(hash: &MerkleHash) -> Key {
    Key {
        prefix: CRAB_CHUNK_PREFIX.to_owned(),
        hash: *hash,
    }
}

/// Run an async future to completion from a sync context.
///
/// Assumes a multi-thread tokio runtime (verified at
/// [`crate::main`] runtime construction). Returns `None` if called
/// outside any runtime — callers treat that as a cache miss and fall
/// back to the network path rather than panicking.
fn block_on_async<F, T>(fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    let Ok(handle) = Handle::try_current() else {
        // No runtime (should not happen in production paths, but keep
        // the cache layer non-fatal for unit tests outside #[tokio::test]).
        return None;
    };
    Some(tokio::task::block_in_place(|| handle.block_on(fut)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn make_hash(data: &[u8]) -> MerkleHash {
        let b3 = blake3::hash(data);
        MerkleHash::from_slice(b3.as_bytes()).unwrap()
    }

    fn temp_cache(max_bytes: u64) -> (tempfile::TempDir, ChunkCache) {
        let dir = tempfile::tempdir().unwrap();
        // A fresh subdir per test so `CacheManager` doesn't hand us a
        // stale singleton from a prior test in the same process.
        let cache = ChunkCache::open(dir.path().join("chunks"), Some(max_bytes)).unwrap();
        (dir, cache)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn put_and_get_round_trip() {
        let (_dir, cache) = temp_cache(1024 * 1024);
        let data = b"hello chunk cache";
        let hash = make_hash(data);

        cache.put(hash, Bytes::from_static(data));
        let got = cache.get(&hash).expect("cached");
        assert_eq!(&got[..], data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_returns_none_on_miss() {
        let (_dir, cache) = temp_cache(1024 * 1024);
        let hash = make_hash(b"nonexistent");
        assert!(cache.get(&hash).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn contains_reflects_state() {
        let (_dir, cache) = temp_cache(1024 * 1024);
        let data = b"check me";
        let hash = make_hash(data);

        assert!(!cache.contains(&hash));
        cache.put(hash, Bytes::from_static(data));
        assert!(cache.contains(&hash));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn put_is_idempotent() {
        let (_dir, cache) = temp_cache(1024 * 1024);
        let data = b"idempotent";
        let hash = make_hash(data);

        cache.put(hash, Bytes::from_static(data));
        cache.put(hash, Bytes::from_static(data));

        // No panic, and the entry is still retrievable.
        let got = cache.get(&hash).expect("still cached");
        assert_eq!(&got[..], data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("chunks");

        let data = b"persistent chunk";
        let hash = make_hash(data);

        {
            let cache = ChunkCache::open(cache_dir.clone(), Some(1024 * 1024)).unwrap();
            cache.put(hash, Bytes::from_static(data));
            let got = cache.get(&hash).expect("first read");
            assert_eq!(&got[..], data);
        }

        // Dropping the first handle decrements the CacheManager weak
        // refcount; reopening via the same path rebuilds the on-disk
        // index and should find the previously stored chunk.
        {
            let cache = ChunkCache::open(cache_dir, Some(1024 * 1024)).unwrap();
            let got = cache.get(&hash).expect("second read after reopen");
            assert_eq!(&got[..], data);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shared_singleton_for_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("shared");

        let first = ChunkCache::open(cache_dir.clone(), Some(1024 * 1024)).unwrap();
        let second = ChunkCache::open(cache_dir, Some(1024 * 1024)).unwrap();

        // Both handles should point at the same underlying DiskCache.
        assert!(Arc::ptr_eq(&first.xet_handle(), &second.xet_handle()));
    }
}
