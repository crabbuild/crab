//! Shared handle over xet-core's on-disk xorb-range cache.
//!
//! The cache stores contiguous chunk ranges keyed by xorb hash and is shared
//! across every reconstructor instance opened in a process. Callers provide the
//! resolved directory and byte budget; owner-specific config stays above this
//! Module.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{debug, warn};
use xet_client::chunk_cache::{CacheConfig, ChunkCache, DiskCache, get_cache};

use crate::{CacheError, Result};

/// Resolved handle over the xet-core chunk cache.
///
/// Holds both the trait-object handle that reconstructors consume and the
/// configuration needed to re-open the same directory for stats queries.
#[derive(Clone)]
pub struct XetChunkCacheHandle {
    /// Trait-object view for `FileReconstructor::with_chunk_cache`.
    pub cache: Arc<dyn ChunkCache>,
    /// Directory backing the `DiskCache`.
    pub directory: PathBuf,
    /// Configured capacity in bytes.
    pub size_bytes: u64,
}

impl std::fmt::Debug for XetChunkCacheHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XetChunkCacheHandle")
            .field("directory", &self.directory)
            .field("size_bytes", &self.size_bytes)
            .finish_non_exhaustive()
    }
}

/// Point-in-time statistics scanned from the on-disk cache.
///
/// Produced by [`XetChunkCacheHandle::stats`]. Re-scans the directory, so
/// callers should treat this as an end-of-command summary rather than a hot-path
/// counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XetChunkCacheStats {
    /// Total number of cached xorb-range entries.
    pub entries: usize,
    /// Total resident bytes across all entries.
    pub total_bytes: u64,
}

impl XetChunkCacheHandle {
    /// Opens the shared xet-core chunk cache at `directory` with `size_bytes`.
    ///
    /// Uses xet-core's cache manager so a process has one `DiskCache` per
    /// directory even when hydrate, prefetch, and filter-process all open a
    /// handle.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] when the directory cannot be created and
    /// [`CacheError::XetChunkCache`] when xet-core cannot initialize the cache.
    pub fn open(directory: impl Into<PathBuf>, size_bytes: u64) -> Result<Self> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory)?;

        let cache_config = CacheConfig {
            cache_directory: directory.clone(),
            cache_size: size_bytes,
        };

        let cache = get_cache(&cache_config).map_err(|source| CacheError::XetChunkCache {
            path: directory.display().to_string(),
            source: Box::new(source),
        })?;

        debug!(
            directory = %directory.display(),
            size_bytes,
            "opened xet-core chunk cache"
        );

        Ok(Self {
            cache,
            directory,
            size_bytes,
        })
    }

    /// Snapshot the cache directory's entry count and resident bytes.
    ///
    /// Re-initializes a local `DiskCache` view so callers do not need to
    /// downcast the `Arc<dyn ChunkCache>`. Returns zeroed stats on failure
    /// because stats are diagnostic output and should not tear down commands.
    pub async fn stats(&self) -> XetChunkCacheStats {
        let config = CacheConfig {
            cache_directory: self.directory.clone(),
            cache_size: self.size_bytes,
        };

        match DiskCache::initialize(&config) {
            Ok(disk) => XetChunkCacheStats {
                entries: disk.num_items().await,
                total_bytes: disk.total_bytes().await,
            },
            Err(e) => {
                warn!(
                    directory = %self.directory.display(),
                    error = %e,
                    "failed to scan chunk cache for stats",
                );
                XetChunkCacheStats {
                    entries: 0,
                    total_bytes: 0,
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_reads_empty_cache_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("chunks");
        let handle =
            XetChunkCacheHandle::open(cache_dir.clone(), 64 * 1024).expect("should open cache");

        assert_eq!(handle.directory, cache_dir);
        assert_eq!(handle.size_bytes, 64 * 1024);

        let stats = handle.stats().await;
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.total_bytes, 0);
    }

    #[tokio::test]
    async fn open_creates_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("nested").join("chunks");
        assert!(!cache_dir.exists());

        let _handle = XetChunkCacheHandle::open(cache_dir.clone(), 64 * 1024)
            .expect("should create cache directory");

        assert!(cache_dir.is_dir());
    }

    #[tokio::test]
    async fn cache_manager_returns_singleton_arc() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("chunks");

        let first = XetChunkCacheHandle::open(cache_dir.clone(), 64 * 1024).unwrap();
        let second = XetChunkCacheHandle::open(cache_dir, 64 * 1024).unwrap();

        assert!(Arc::ptr_eq(&first.cache, &second.cache));
    }
}
