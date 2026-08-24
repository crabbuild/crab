//! Shared handle over xet-core's on-disk xorb-range cache.
//!
//! The cache stores contiguous chunk ranges keyed by xorb hash and is shared
//! across every reconstructor instance opened in a process. Callers provide the
//! resolved directory and byte budget; owner-specific config stays above this
//! Module.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use base64::Engine as _;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use xet_client::chunk_cache::{CacheConfig, ChunkCache, get_cache};
use xet_runtime::config::XetConfig;

use crate::{CacheError, Result};

const MAX_XET_CHUNK_CACHE_ENTRIES: usize = 1_000_000;

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

/// Result of explicitly pruning xet-core range-cache files to a byte budget.
#[derive(Debug, Clone, Default)]
pub struct XetChunkCachePruneStats {
    pub entries_evicted: u64,
    pub bytes_freed: u64,
    pub entries: Vec<(PathBuf, u64)>,
}

/// Integrity report for xet-core range-cache files.
#[derive(Debug, Clone, Default)]
pub struct XetChunkCacheVerifyStats {
    pub total: u64,
    pub valid: u64,
    pub corrupt: u64,
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

        let xet_config = XetConfig::new();
        let cache =
            get_cache(&xet_config, &cache_config).map_err(|source| CacheError::XetChunkCache {
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
    /// Scans the xet-core cache layout without opening or repairing it.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] when an existing cache directory cannot be
    /// read. A missing directory is an empty cache.
    pub async fn stats(&self) -> Result<XetChunkCacheStats> {
        xet_chunk_cache_stats(&self.directory).await
    }

    /// Snapshot the cache directory while honoring a caller cancellation.
    pub async fn stats_with_cancel(
        &self,
        cancel: &CancellationToken,
    ) -> Result<XetChunkCacheStats> {
        xet_chunk_cache_stats_with_cancel(&self.directory, cancel).await
    }
}

/// Snapshot an xet-core range-cache directory without mutating it.
pub async fn xet_chunk_cache_stats(directory: &std::path::Path) -> Result<XetChunkCacheStats> {
    xet_chunk_cache_stats_with_cancel(directory, &CancellationToken::new()).await
}

/// Snapshot the xet-core range-cache directory while honoring cancellation.
pub async fn xet_chunk_cache_stats_with_cancel(
    directory: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<XetChunkCacheStats> {
    run_blocking_cache_scan(directory, cancel, |directory, cancel| {
        let mut stats = XetChunkCacheStats {
            entries: 0,
            total_bytes: 0,
        };
        visit_xet_chunk_cache_entries(directory, cancel, |entry| {
            stats.entries = stats.entries.saturating_add(1);
            stats.total_bytes = stats.total_bytes.saturating_add(entry.bytes);
            Ok(())
        })?;
        Ok(stats)
    })
    .await
}

/// Evict oldest xet-core range files until `max_bytes` is satisfied.
pub async fn prune_xet_chunk_cache(
    directory: &Path,
    max_bytes: u64,
    dry_run: bool,
    record_paths: bool,
) -> Result<XetChunkCachePruneStats> {
    prune_xet_chunk_cache_with_cancel(
        directory,
        max_bytes,
        dry_run,
        record_paths,
        &CancellationToken::new(),
    )
    .await
}

/// Evict xet-core range files while honoring a caller cancellation.
pub async fn prune_xet_chunk_cache_with_cancel(
    directory: &Path,
    max_bytes: u64,
    dry_run: bool,
    record_paths: bool,
    cancel: &CancellationToken,
) -> Result<XetChunkCachePruneStats> {
    run_blocking_cache_scan(directory, cancel, move |directory, cancel| {
        let mut entries = collect_xet_chunk_cache_entries(directory, cancel)?;
        let total = entries
            .iter()
            .fold(0u64, |total, entry| total.saturating_add(entry.bytes));
        if total <= max_bytes {
            return Ok(XetChunkCachePruneStats::default());
        }
        entries.sort_by_key(|entry| entry.modified);
        let target = total - max_bytes;
        let mut stats = XetChunkCachePruneStats::default();
        for entry in entries {
            check_cancelled(cancel)?;
            if stats.bytes_freed >= target {
                break;
            }
            if !dry_run {
                match fs::remove_file(&entry.path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            stats.entries_evicted = stats.entries_evicted.saturating_add(1);
            stats.bytes_freed = stats.bytes_freed.saturating_add(entry.bytes);
            if record_paths {
                stats.entries.push((entry.path, entry.bytes));
            }
        }
        Ok(stats)
    })
    .await
}

/// Validate xet-core range-cache filenames, lengths, headers, and CRCs.
pub async fn verify_xet_chunk_cache(directory: &Path) -> Result<XetChunkCacheVerifyStats> {
    verify_xet_chunk_cache_with_cancel(directory, &CancellationToken::new()).await
}

/// Validate xet-core range files while honoring a caller cancellation.
pub async fn verify_xet_chunk_cache_with_cancel(
    directory: &Path,
    cancel: &CancellationToken,
) -> Result<XetChunkCacheVerifyStats> {
    run_blocking_cache_scan(directory, cancel, |directory, cancel| {
        let mut stats = XetChunkCacheVerifyStats::default();
        visit_xet_chunk_cache_entries(directory, cancel, |entry| {
            let valid = match verify_xet_chunk_cache_entry(&entry.path, entry.bytes, cancel) {
                Ok(valid) => valid,
                Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            stats.total = stats.total.saturating_add(1);
            if valid {
                stats.valid = stats.valid.saturating_add(1);
                return Ok(());
            }
            stats.corrupt = stats.corrupt.saturating_add(1);
            check_cancelled(cancel)?;
            match fs::remove_file(&entry.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        })?;
        Ok(stats)
    })
    .await
}

async fn run_blocking_cache_scan<T, F>(
    directory: &Path,
    cancel: &CancellationToken,
    scan: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Path, &CancellationToken) -> Result<T> + Send + 'static,
{
    let directory = directory.to_owned();
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        check_cancelled(&cancel)?;
        scan(&directory, &cancel)
    })
    .await
    .map_err(|error| CacheError::Internal(format!("xet cache scan task failed: {error}")))?
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(CacheError::Cancelled);
    }
    Ok(())
}

fn verify_xet_chunk_cache_entry(
    path: &Path,
    bytes: u64,
    cancel: &CancellationToken,
) -> Result<bool> {
    check_cancelled(cancel)?;
    const ITEM_NAME_BYTES: usize = 20;
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    let Ok(encoded) = base64::engine::general_purpose::URL_SAFE.decode(name) else {
        return Ok(false);
    };
    let Ok(encoded): std::result::Result<[u8; ITEM_NAME_BYTES], _> = encoded.try_into() else {
        return Ok(false);
    };
    let start = u32::from_le_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
    let end = u32::from_le_bytes([encoded[4], encoded[5], encoded[6], encoded[7]]);
    let expected_bytes = u64::from_le_bytes([
        encoded[8],
        encoded[9],
        encoded[10],
        encoded[11],
        encoded[12],
        encoded[13],
        encoded[14],
        encoded[15],
    ]);
    let expected_crc = u32::from_le_bytes([encoded[16], encoded[17], encoded[18], encoded[19]]);
    if start >= end || expected_bytes != bytes || bytes < 8 {
        return Ok(false);
    }

    let mut file = fs::File::open(path)?;
    let mut count_bytes = [0u8; 4];
    if !read_exact_or_corrupt(&mut file, &mut count_bytes)? {
        return Ok(false);
    }
    let count = u32::from_le_bytes(count_bytes);
    let expected_count = u64::from(end) - u64::from(start) + 1;
    if u64::from(count) != expected_count {
        return Ok(false);
    }
    let header_bytes = u64::from(count).saturating_add(1).saturating_mul(4);
    if header_bytes > bytes {
        return Ok(false);
    }
    let mut previous = None;
    let mut offset_bytes = [0u8; 4];
    for _ in 0..count {
        check_cancelled(cancel)?;
        if !read_exact_or_corrupt(&mut file, &mut offset_bytes)? {
            return Ok(false);
        }
        let offset = u32::from_le_bytes(offset_bytes);
        if previous.is_none() && offset != 0 || previous.is_some_and(|previous| previous >= offset)
        {
            return Ok(false);
        }
        previous = Some(offset);
    }
    if u64::from(previous.unwrap_or(0)) != bytes - header_bytes {
        return Ok(false);
    }

    let mut file = fs::File::open(path)?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        check_cancelled(cancel)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize() == expected_crc)
}

fn read_exact_or_corrupt(file: &mut fs::File, buffer: &mut [u8]) -> Result<bool> {
    match file.read_exact(buffer) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error.into()),
    }
}

struct XetChunkCacheEntry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

fn collect_xet_chunk_cache_entries(
    directory: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<XetChunkCacheEntry>> {
    let mut entries = Vec::new();
    visit_xet_chunk_cache_entries(directory, cancel, |entry| {
        if entries.len() >= MAX_XET_CHUNK_CACHE_ENTRIES {
            return Err(CacheError::Internal(format!(
                "xet chunk cache contains more than {MAX_XET_CHUNK_CACHE_ENTRIES} entries; refusing an unbounded LRU scan"
            )));
        }
        entries.push(entry);
        Ok(())
    })?;
    Ok(entries)
}

fn visit_xet_chunk_cache_entries(
    directory: &Path,
    cancel: &CancellationToken,
    mut visit: impl FnMut(XetChunkCacheEntry) -> Result<()>,
) -> Result<()> {
    check_cancelled(cancel)?;
    let prefixes = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for prefix in prefixes {
        check_cancelled(cancel)?;
        let prefix = match prefix {
            Ok(prefix) => prefix,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let Some(prefix_name) = prefix.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let prefix_type = match prefix.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if prefix_name.len() != 2 || !prefix_name.is_ascii() || !prefix_type.is_dir() {
            continue;
        }
        let keys = match fs::read_dir(prefix.path()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for key in keys {
            check_cancelled(cancel)?;
            let key = match key {
                Ok(key) => key,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let Some(key_name) = key.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let key_type = match key.file_type() {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !key_name
                .as_bytes()
                .get(..2)
                .is_some_and(|key_prefix| key_prefix.eq_ignore_ascii_case(prefix_name.as_bytes()))
                || !key_type.is_dir()
            {
                continue;
            }
            let items = match fs::read_dir(key.path()) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for item in items {
                check_cancelled(cancel)?;
                let item = match item {
                    Ok(item) => item,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                let item_type = match item.file_type() {
                    Ok(file_type) => file_type,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if !item_type.is_file() {
                    continue;
                }
                let metadata = match item.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                visit(XetChunkCacheEntry {
                    path: item.path(),
                    bytes: metadata.len(),
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    async fn write_xet_range(cache_dir: &Path, payload: &[u8]) -> PathBuf {
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        body.extend_from_slice(payload);
        let mut name = Vec::new();
        name.extend_from_slice(&0u32.to_le_bytes());
        name.extend_from_slice(&1u32.to_le_bytes());
        name.extend_from_slice(&u64::try_from(body.len()).unwrap().to_le_bytes());
        name.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        let name = base64::engine::general_purpose::URL_SAFE.encode(name);
        let dir = cache_dir.join("ab").join("ab-key");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join(name);
        tokio::fs::write(&path, body).await.unwrap();
        path
    }

    #[tokio::test]
    async fn open_reads_empty_cache_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("chunks");
        let handle =
            XetChunkCacheHandle::open(cache_dir.clone(), 64 * 1024).expect("should open cache");

        assert_eq!(handle.directory, cache_dir);
        assert_eq!(handle.size_bytes, 64 * 1024);

        let stats = handle.stats().await.unwrap();
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

    #[tokio::test]
    async fn stats_is_read_only_and_counts_range_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("chunks");
        let item_dir = cache_dir.join("ab").join("ab-key");
        tokio::fs::create_dir_all(&item_dir).await.unwrap();
        tokio::fs::write(item_dir.join("range"), b"12345")
            .await
            .unwrap();
        tokio::fs::write(cache_dir.join("unknown"), b"ignored")
            .await
            .unwrap();

        let stats = xet_chunk_cache_stats(&cache_dir).await.unwrap();

        assert_eq!(stats.entries, 1);
        assert_eq!(stats.total_bytes, 5);
        assert!(cache_dir.join("unknown").exists());
    }

    #[tokio::test]
    async fn stats_does_not_create_a_missing_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("missing");

        let stats = xet_chunk_cache_stats(&cache_dir).await.unwrap();

        assert_eq!(stats.entries, 0);
        assert_eq!(stats.total_bytes, 0);
        assert!(!cache_dir.exists());
    }

    #[tokio::test]
    async fn scans_honor_cancellation_before_worker_start() {
        let tmp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = xet_chunk_cache_stats_with_cancel(&tmp.path().join("missing"), &cancel)
            .await
            .unwrap_err();
        assert!(matches!(error, CacheError::Cancelled));
    }

    #[tokio::test]
    async fn prune_dry_run_and_apply_cover_xet_range_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("chunks");
        let item_dir = cache_dir.join("ab").join("ab-key");
        tokio::fs::create_dir_all(&item_dir).await.unwrap();
        tokio::fs::write(item_dir.join("old"), vec![1u8; 8])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::fs::write(item_dir.join("new"), vec![2u8; 8])
            .await
            .unwrap();

        let plan = prune_xet_chunk_cache(&cache_dir, 8, true, true)
            .await
            .unwrap();
        assert_eq!(plan.entries_evicted, 1);
        assert_eq!(plan.bytes_freed, 8);
        assert_eq!(xet_chunk_cache_stats(&cache_dir).await.unwrap().entries, 2);

        let applied = prune_xet_chunk_cache(&cache_dir, 8, false, true)
            .await
            .unwrap();
        assert_eq!(applied.entries, plan.entries);
        assert_eq!(xet_chunk_cache_stats(&cache_dir).await.unwrap().entries, 1);
    }

    #[tokio::test]
    async fn verify_accepts_valid_ranges_and_evicts_crc_mismatches() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("chunks");
        let valid = write_xet_range(&cache_dir, b"valid").await;

        let report = verify_xet_chunk_cache(&cache_dir).await.unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.valid, 1);

        let mut corrupt = tokio::fs::read(&valid).await.unwrap();
        *corrupt.last_mut().unwrap() ^= 0xff;
        tokio::fs::write(&valid, corrupt).await.unwrap();
        let report = verify_xet_chunk_cache(&cache_dir).await.unwrap();

        assert_eq!(report.corrupt, 1);
        assert!(!valid.exists());
    }

    #[tokio::test]
    async fn stats_and_verify_stream_large_range_file_inventories() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("chunks");
        for index in 0..1_001u32 {
            write_xet_range(&cache_dir, &index.to_le_bytes()).await;
        }

        let stats = xet_chunk_cache_stats(&cache_dir).await.unwrap();
        let report = verify_xet_chunk_cache(&cache_dir).await.unwrap();

        assert_eq!(stats.entries, 1_001);
        assert_eq!(report.total, 1_001);
        assert_eq!(report.valid, 1_001);
    }

    #[tokio::test]
    async fn verify_accepts_single_chunk_ranges_and_evicts_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("chunks");
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&7u32.to_le_bytes());
        body.extend_from_slice(b"payload");
        let mut name = Vec::new();
        name.extend_from_slice(&7u32.to_le_bytes());
        name.extend_from_slice(&8u32.to_le_bytes());
        name.extend_from_slice(&u64::try_from(body.len()).unwrap().to_le_bytes());
        name.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        let name = base64::engine::general_purpose::URL_SAFE.encode(name);
        let dir = cache_dir.join("ab").join("ab-key");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join(name);
        tokio::fs::write(&path, &body).await.unwrap();

        let report = verify_xet_chunk_cache(&cache_dir).await.unwrap();
        assert_eq!(report.valid, 1);

        tokio::fs::write(&path, &body[..3]).await.unwrap();
        let report = verify_xet_chunk_cache(&cache_dir).await.unwrap();
        assert_eq!(report.corrupt, 1);
        assert!(!path.exists());
    }
}
