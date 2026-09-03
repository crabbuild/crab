//! Chunk-hash adapter over Crab's shared decoded-range cache.
//!
//! Directory validation, disk layout, and capacity belong to `crab-cache`.
//! An unavailable cache stores nothing and always misses; mounted reads must
//! not depend on disposable storage. Whole-pointer reads use `crab-read`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use tokio::runtime::{Handle, RuntimeFlavor};
use tracing::{debug, warn};
use xet_client::cas_types::{ChunkRange, Key};
use xet_client::chunk_cache::error::ChunkCacheError;
use xet_client::chunk_cache::{CacheRange, ChunkCache as XetChunkCacheTrait};

use crate::core::error::Result;
use crab_xet::xorb::format::MerkleHash;

/// Default chunk cache ceiling: 4 GiB.
///
/// Kept identical to the prior self-written cache so existing callers
/// behave the same when they pass `None`.
const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Optional chunk storage using the shared Xet cache contract.
pub struct ChunkCache {
    dir: PathBuf,
    max_bytes: u64,
    inner: Arc<dyn XetChunkCacheTrait>,
}

// Xet allows a miss immediately after a put. Keeping this stateless handle
// preserves the tagged xet_handle API without giving cache failure authority
// over reads or creating a replacement cache directory.
struct UnavailableCache;

#[async_trait::async_trait]
impl XetChunkCacheTrait for UnavailableCache {
    async fn get(
        &self,
        _key: &Key,
        _range: &ChunkRange,
    ) -> std::result::Result<Option<CacheRange>, ChunkCacheError> {
        Ok(None)
    }

    async fn put(
        &self,
        _key: &Key,
        _range: &ChunkRange,
        _chunk_byte_indices: &[u32],
        _data: &[u8],
    ) -> std::result::Result<(), ChunkCacheError> {
        Ok(())
    }
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
    /// Open optional chunk storage, bypassing an unsafe or unavailable cache.
    ///
    /// The `Result` signature is retained from release tags v1.0.1/v1.1.0;
    /// cache-only initialization failures now return a non-storing handle.
    pub fn open(dir: PathBuf, max_bytes: Option<u64>) -> Result<Self> {
        let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

        let inner: Arc<dyn XetChunkCacheTrait> =
            match crab_cache::XetChunkCacheHandle::open(dir.clone(), max_bytes) {
                Ok(handle) => {
                    debug!(dir = %dir.display(), max_bytes, "chunk cache opened");
                    handle.cache
                }
                Err(error) => {
                    warn!(
                        family = "decoded-range",
                        operation = "open",
                        path = %dir.display(),
                        recovery = "use-verified-origin",
                        %error,
                        "VFS chunk cache unavailable"
                    );
                    Arc::new(UnavailableCache)
                }
            };

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
        let Ok(len) = u32::try_from(data.len()) else {
            return;
        };
        let indices = [0, len];

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

    /// Return the shared cache handle, which always misses if storage is unavailable.
    pub fn xet_handle(&self) -> Arc<dyn XetChunkCacheTrait> {
        Arc::clone(&self.inner)
    }
}

/// Build a crab-prefixed xet-core key for a chunk hash.
fn make_key(hash: &MerkleHash) -> Key {
    Key {
        prefix: crab_cache::xet_chunk_cache::CHUNK_HASH_PREFIX.to_owned(),
        hash: *hash,
    }
}

/// Run an async future to completion from a sync context.
///
/// Requires a multi-thread runtime; other contexts bypass this optional cache.
fn block_on_async<F, T>(fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    let Ok(handle) = Handle::try_current() else {
        return None;
    };
    if handle.runtime_flavor() != RuntimeFlavor::MultiThread {
        return None;
    }
    Some(tokio::task::block_in_place(|| handle.block_on(fut)))
}

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
        let cache = ChunkCache::open(dir.path().join("cache/chunks"), Some(max_bytes)).unwrap();
        (dir, cache)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn put_and_get_round_trip() {
        let (_dir, cache) = temp_cache(1024 * 1024);
        let data = b"hello chunk cache";
        let hash = make_hash(data);

        cache.put(hash, Bytes::from_static(data));
        assert_eq!(cache.get(&hash).as_deref(), Some(&data[..]));
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
        assert_eq!(cache.get(&hash).as_deref(), Some(&data[..]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache/chunks");

        let data = b"persistent chunk";
        let hash = make_hash(data);

        {
            let cache = ChunkCache::open(cache_dir.clone(), Some(1024 * 1024)).unwrap();
            cache.put(hash, Bytes::from_static(data));
            assert_eq!(cache.get(&hash).as_deref(), Some(&data[..]));
        }

        // Reopening after the final handle drops must reuse persisted bytes,
        // not depend on a live process-local handle.
        {
            let cache = ChunkCache::open(cache_dir, Some(1024 * 1024)).unwrap();
            assert_eq!(cache.get(&hash).as_deref(), Some(&data[..]));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shared_singleton_for_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache/chunks");

        let first = ChunkCache::open(cache_dir.clone(), Some(1024 * 1024)).unwrap();
        let second = ChunkCache::open(cache_dir, Some(1024 * 1024)).unwrap();

        assert!(Arc::ptr_eq(&first.xet_handle(), &second.xet_handle()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn valid_record_checksum_does_not_authorize_wrong_chunk_bytes() {
        let (_dir, cache) = temp_cache(1024 * 1024);
        let hash = make_hash(b"expected");
        cache
            .xet_handle()
            .put(&make_key(&hash), &ChunkRange::new(0, 1), &[0, 5], b"wrong")
            .await
            .unwrap();

        assert!(cache.get(&hash).is_none());
        cache.put(hash, Bytes::from_static(b"expected"));
        assert_eq!(cache.get(&hash).unwrap().as_ref(), b"expected");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn unavailable_storage_is_non_storing_and_preserves_outside_state() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for kind in ["file", "symlink", "permissions"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let outside = tmp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("sentinel"), b"unchanged").unwrap();
            match kind {
                "file" => std::fs::write(&root, b"not a directory").unwrap(),
                "symlink" => symlink(&outside, &root).unwrap(),
                _ => {
                    std::fs::create_dir(&root).unwrap();
                    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                }
            }

            let cache = ChunkCache::open(root.join("chunks"), Some(1024)).unwrap();
            let hash = make_hash(b"value");
            cache.put(hash, Bytes::from_static(b"value"));
            assert!(cache.get(&hash).is_none(), "{kind}");
            let handle = cache.xet_handle();
            let key = make_key(&hash);
            let range = ChunkRange::new(0, 1);
            handle.put(&key, &range, &[0, 5], b"value").await.unwrap();
            assert!(handle.get(&key, &range).await.unwrap().is_none());
            assert_eq!(
                std::fs::read(outside.join("sentinel")).unwrap(),
                b"unchanged"
            );
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
            assert!(!root.join("chunks").exists());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_thread_runtime_bypasses_sync_cache_without_panicking() {
        let (_dir, cache) = temp_cache(1024);
        let hash = make_hash(b"value");
        cache.put(hash, Bytes::from_static(b"value"));
        assert!(cache.get(&hash).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn private_creation_ignores_permissive_umask() {
        use std::os::unix::fs::PermissionsExt;

        const CHILD: &str = "CRAB_VFS_PRIVATE_CACHE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "chunk_cache::tests::private_creation_ignores_permissive_umask",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        // SAFETY: only this test runs in the dedicated child process.
        unsafe { libc::umask(0) };
        let (_dir, cache) = temp_cache(1024);
        for path in [cache.dir(), cache.dir().parent().unwrap()] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
