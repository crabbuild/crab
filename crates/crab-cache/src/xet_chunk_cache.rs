//! Shared handle over xet-core's on-disk xorb-range cache.
//!
//! The cache stores contiguous chunk ranges keyed by xorb hash and is shared
//! across every reconstructor instance opened in a process. Callers provide the
//! resolved directory and byte budget; owner-specific config stays above this
//! Module.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use base64::Engine as _;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use xet_client::cas_types::{ChunkRange, Key};
use xet_client::chunk_cache::error::ChunkCacheError;
use xet_client::chunk_cache::{CacheRange, ChunkCache};

use crate::private_fs::{PinnedRoot, check_cancelled, with_pinned_root};
use crate::{CacheError, Result};

const MAX_XET_CHUNK_CACHE_ENTRIES: usize = 1_000_000;
const RANGE_ITEM_NAME_BYTES: usize = 20;
const MAX_DECODED_RANGE_BYTES: u64 = 256 * 1024 * 1024;
static RANGE_CACHE_HANDLES: OnceLock<Mutex<HashMap<PathBuf, Weak<CrabRangeCache>>>> =
    OnceLock::new();

/// Namespace for one-chunk entries whose key is the decoded content's Blake3 hash.
pub const CHUNK_HASH_PREFIX: &str = "crab-chunk";

struct CrabRangeCache {
    root: PathBuf,
    capacity: u64,
    catalog: crate::catalog::CacheCatalog,
}

impl CrabRangeCache {
    fn key_directory(&self, key: &Key) -> PathBuf {
        let mut bytes = Vec::with_capacity(key.hash.as_bytes().len() + key.prefix.len());
        bytes.extend_from_slice(key.hash.as_bytes());
        bytes.extend_from_slice(key.prefix.as_bytes());
        let encoded = base64::engine::general_purpose::URL_SAFE.encode(bytes);
        self.root.join(&encoded[..2]).join(encoded)
    }

    async fn read_entry(
        &self,
        path: &Path,
        item: (u32, u32, u64, u32),
        key: &Key,
        requested: &ChunkRange,
    ) -> std::result::Result<CacheRange, ChunkCacheError> {
        let (item_start, item_end, expected_len, _) = item;
        if item_start >= item_end
            || requested.start < item_start
            || requested.end > item_end
            || expected_len > self.capacity.min(MAX_DECODED_RANGE_BYTES)
        {
            return Err(ChunkCacheError::InvalidArguments);
        }
        let (entry, file) = crate::catalog::PayloadRead::open(self.catalog.root(), path)
            .await
            .map_err(std::io::Error::other)?;
        let result = Self::read_open_entry(file, item, requested)
            .await
            .and_then(|hit| {
                // Xorb keys name the encoded object, not these decoded bytes.
                // Only the chunk namespace supplies a decoded content hash;
                // CRC consistency alone cannot establish that identity.
                if key.prefix == CHUNK_HASH_PREFIX
                    && blake3::hash(&hit.data).as_bytes() != key.hash.as_bytes()
                {
                    return Err(ChunkCacheError::Parse(
                        "decoded chunk identity mismatch".into(),
                    ));
                }
                Ok(hit)
            });
        let (hit, entry) = entry.finish(result).await?;
        entry.touch().await;
        Ok(hit)
    }

    async fn read_open_entry(
        file: tokio::fs::File,
        item: (u32, u32, u64, u32),
        requested: &ChunkRange,
    ) -> std::result::Result<CacheRange, ChunkCacheError> {
        let (item_start, item_end, expected_len, expected_crc) = item;
        let metadata = file.metadata().await?;
        if metadata.len() != expected_len {
            return Err(ChunkCacheError::Parse(
                "decoded-range length mismatch".into(),
            ));
        }
        let mut body = Vec::new();
        file.take(expected_len.saturating_add(1))
            .read_to_end(&mut body)
            .await?;
        if body.len() as u64 != expected_len || crc32fast::hash(&body) != expected_crc {
            return Err(ChunkCacheError::Parse(
                "decoded-range checksum mismatch".into(),
            ));
        }
        let count = usize::try_from(u64::from(item_end) - u64::from(item_start) + 1)
            .map_err(ChunkCacheError::parse)?;
        let header_len = 4usize
            .checked_add(
                count
                    .checked_mul(4)
                    .ok_or(ChunkCacheError::InvalidArguments)?,
            )
            .ok_or(ChunkCacheError::InvalidArguments)?;
        if body.len() < header_len {
            return Err(ChunkCacheError::Parse(
                "decoded-range header truncated".into(),
            ));
        }
        let stored_count =
            u32::from_le_bytes(body[..4].try_into().map_err(ChunkCacheError::parse)?);
        if usize::try_from(stored_count).map_err(ChunkCacheError::parse)? != count {
            return Err(ChunkCacheError::Parse(
                "decoded-range offset count mismatch".into(),
            ));
        }
        let mut offsets = Vec::with_capacity(count);
        for bytes in body[4..header_len].as_chunks::<4>().0 {
            offsets.push(u32::from_le_bytes(*bytes));
        }
        if offsets.first() != Some(&0)
            || offsets
                .windows(2)
                .any(|pair| pair.first().is_none_or(|first| *first >= pair[1]))
            || offsets.last().copied().map(u64::from) != Some((body.len() - header_len) as u64)
        {
            return Err(ChunkCacheError::Parse(
                "decoded-range offsets invalid".into(),
            ));
        }
        let first =
            usize::try_from(requested.start - item_start).map_err(ChunkCacheError::parse)?;
        let last = usize::try_from(requested.end - item_start).map_err(ChunkCacheError::parse)?;
        let data_start = usize::try_from(offsets[first]).map_err(ChunkCacheError::parse)?;
        let data_end = usize::try_from(offsets[last]).map_err(ChunkCacheError::parse)?;
        let data = body[header_len + data_start..header_len + data_end].to_vec();
        let base = offsets[first];
        let selected_offsets = offsets[first..=last]
            .iter()
            .map(|offset| offset - base)
            .collect();
        Ok(CacheRange {
            offsets: selected_offsets,
            data,
            range: *requested,
        })
    }
}

#[async_trait::async_trait]
impl ChunkCache for CrabRangeCache {
    async fn get(
        &self,
        key: &Key,
        range: &ChunkRange,
    ) -> std::result::Result<Option<CacheRange>, ChunkCacheError> {
        if range.start >= range.end
            || (key.prefix == CHUNK_HASH_PREFIX && (range.start != 0 || range.end != 1))
        {
            return Err(ChunkCacheError::InvalidArguments);
        }
        let key_directory = self.key_directory(key);
        let entries = match crate::private_fs::entry_names(
            self.catalog.root(),
            &key_directory,
            4_096,
        )
        .await
        {
            Ok(entries) => entries,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => {
                warn!(family = "decoded-range", operation = "read-dir", %error, "cache miss");
                return Ok(None);
            }
        };
        for name in entries {
            let Some((start, end, len, crc)) = decode_range_item_name(&name) else {
                continue;
            };
            if start > range.start || end < range.end {
                continue;
            }
            let path = key_directory.join(name);
            match self
                .read_entry(&path, (start, end, len, crc), key, range)
                .await
            {
                Ok(hit) => return Ok(Some(hit)),
                Err(error) => {
                    warn!(family = "decoded-range", operation = "read", path = %path.display(),
                        recovery = "use-origin", %error, "local cache read failed");
                }
            }
        }
        Ok(None)
    }

    async fn put(
        &self,
        key: &Key,
        range: &ChunkRange,
        chunk_byte_indices: &[u32],
        data: &[u8],
    ) -> std::result::Result<(), ChunkCacheError> {
        if range.start >= range.end
            || chunk_byte_indices.len() as u64 != u64::from(range.end) - u64::from(range.start) + 1
            || chunk_byte_indices.first() != Some(&0)
            || chunk_byte_indices.last().copied().map(u64::from) != Some(data.len() as u64)
            || chunk_byte_indices.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(ChunkCacheError::InvalidArguments);
        }
        let body_len = (chunk_byte_indices.len() as u64)
            .checked_mul(4)
            .and_then(|len| len.checked_add(4))
            .and_then(|len| len.checked_add(data.len() as u64))
            .ok_or(ChunkCacheError::InvalidArguments)?;
        if body_len > self.capacity.min(MAX_DECODED_RANGE_BYTES) {
            return Ok(());
        }
        if self.get(key, range).await?.is_some() {
            return Ok(());
        }
        let mut body =
            Vec::with_capacity(usize::try_from(body_len).map_err(ChunkCacheError::parse)?);
        body.extend_from_slice(&(chunk_byte_indices.len() as u32).to_le_bytes());
        for offset in chunk_byte_indices {
            body.extend_from_slice(&offset.to_le_bytes());
        }
        body.extend_from_slice(data);
        let name = encode_range_item_name(range, body.len() as u64, crc32fast::hash(&body));
        let directory = self.key_directory(key);
        let final_path = directory.join(name);
        let Some(reservation) = self
            .catalog
            .reserve(&final_path, body.len() as u64)
            .await
            .map_err(ChunkCacheError::general)?
        else {
            return Ok(());
        };
        let reservation = reservation
            .write(&body)
            .await
            .map_err(ChunkCacheError::general)?;
        if let Err(error) = self
            .catalog
            .record_and_maintain(
                "decoded-range",
                format!("{}:{}-{}", key.hash, range.start, range.end),
                body.len() as u64,
                reservation,
            )
            .await
        {
            warn!(
                family = "decoded-range",
                operation = "record-and-maintain",
                recovery = "retain-entry-and-continue",
                %error,
                "cache maintenance failed"
            );
        }
        Ok(())
    }
}

pub(crate) fn decode_range_item_name(name: &std::ffi::OsStr) -> Option<(u32, u32, u64, u32)> {
    let encoded = base64::engine::general_purpose::URL_SAFE
        .decode(name.to_str()?)
        .ok()?;
    let bytes: [u8; RANGE_ITEM_NAME_BYTES] = encoded.try_into().ok()?;
    Some((
        u32::from_le_bytes(bytes[0..4].try_into().ok()?),
        u32::from_le_bytes(bytes[4..8].try_into().ok()?),
        u64::from_le_bytes(bytes[8..16].try_into().ok()?),
        u32::from_le_bytes(bytes[16..20].try_into().ok()?),
    ))
}

fn encode_range_item_name(range: &ChunkRange, len: u64, crc: u32) -> String {
    let mut bytes = Vec::with_capacity(RANGE_ITEM_NAME_BYTES);
    bytes.extend_from_slice(&range.start.to_le_bytes());
    bytes.extend_from_slice(&range.end.to_le_bytes());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&crc.to_le_bytes());
    base64::engine::general_purpose::URL_SAFE.encode(bytes)
}

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    /// Opens Crab's decoded-range cache at `directory` with `size_bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] when the directory cannot be created, or
    /// [`CacheError::BudgetConflict`] when a live handle already owns the same
    /// canonical directory with a different byte budget.
    pub fn open(directory: impl Into<PathBuf>, size_bytes: u64) -> Result<Self> {
        let directory = directory.into();
        if let Some(cache_root) = directory.parent() {
            crate::root::ensure_private_cache_directory(cache_root)?;
        }
        crate::root::ensure_private_cache_directory(&directory)?;
        let directory = directory.canonicalize()?;
        let handles = RANGE_CACHE_HANDLES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut handles = handles
            .lock()
            .map_err(|_| CacheError::Internal("range-cache handle registry poisoned".into()))?;
        let cache = if let Some(cache) = handles.get(&directory).and_then(Weak::upgrade) {
            if cache.capacity != size_bytes {
                return Err(CacheError::BudgetConflict {
                    path: directory.display().to_string(),
                    active_bytes: cache.capacity,
                    requested_bytes: size_bytes,
                });
            }
            cache
        } else {
            let cache_root = directory
                .parent()
                .map_or_else(|| directory.clone(), Path::to_path_buf);
            let cache = Arc::new(CrabRangeCache {
                root: directory.clone(),
                capacity: size_bytes,
                catalog: crate::catalog::CacheCatalog::new(cache_root, size_bytes),
            });
            handles.insert(directory.clone(), Arc::downgrade(&cache));
            cache
        };

        debug!(
            directory = %directory.display(),
            size_bytes,
            "opened Crab decoded-range cache"
        );

        Ok(Self {
            cache: cache as Arc<dyn ChunkCache>,
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
    /// Returns [`CacheError::Io`] for filesystem failures or
    /// [`CacheError::UnsafeRoot`] for unsafe recognized paths. A missing
    /// directory is an empty cache; unknown paths are not traversed.
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
    with_pinned_root(directory, cancel, |root, cancel| {
        let mut stats = XetChunkCacheStats {
            entries: 0,
            total_bytes: 0,
        };
        visit_xet_chunk_cache_entries(root, cancel, |entry| {
            stats.entries = stats.entries.saturating_add(1);
            stats.total_bytes = stats.total_bytes.saturating_add(entry.bytes);
            Ok(())
        })?;
        Ok(stats)
    })
    .await
}

/// Evict eligible oldest range files toward `max_bytes`, skipping busy entries.
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
    let report_directory = directory.to_owned();
    let parent = directory.parent().ok_or_else(|| CacheError::UnsafeRoot {
        path: directory.display().to_string(),
        reason: "range directory has no cache parent".into(),
    })?;
    let name = directory
        .file_name()
        .ok_or_else(|| CacheError::UnsafeRoot {
            path: directory.display().to_string(),
            reason: "range directory has no name".into(),
        })?
        .to_owned();
    let display_root = parent.to_owned();
    crate::private_fs::run_blocking(cancel, move |cancel| {
        let (root, catalog_root) = match PinnedRoot::open_with_private_parent(&report_directory) {
            Ok(roots) => roots,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(XetChunkCachePruneStats::default());
            }
            Err(error) => return Err(error),
        };
        let mut entries = collect_xet_chunk_cache_entries(&root, cancel)?;
        let total = entries
            .iter()
            .fold(0u64, |total, entry| total.saturating_add(entry.bytes));
        if total <= max_bytes {
            return Ok(XetChunkCachePruneStats::default());
        }
        entries.sort_by_key(|entry| entry.modified);
        let target = total - max_bytes;
        let mut stats = XetChunkCachePruneStats::default();
        let mut removal =
            crate::catalog::PayloadRemoval::open(catalog_root.as_ref(), &display_root, dry_run)?;
        for entry in entries {
            check_cancelled(cancel)?;
            if stats.bytes_freed >= target {
                break;
            }
            let relative = Path::new(&name).join(&entry.path);
            let removed = removal.remove(&relative, || {
                root.remove_file_if(&entry.path, dry_run, &mut |_| {
                    check_cancelled(cancel)?;
                    Ok(true)
                })
            });
            let bytes = match removed {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(CacheError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            stats.entries_evicted = stats.entries_evicted.saturating_add(1);
            stats.bytes_freed = stats.bytes_freed.saturating_add(bytes);
            if record_paths {
                stats
                    .entries
                    .push((report_directory.join(entry.path), bytes));
            }
        }
        Ok(stats)
    })
    .await
}

/// Validate private range records and chunk identities, evicting corrupt entries.
///
/// Unknown paths and busy entries are retained and excluded from checked totals.
pub async fn verify_xet_chunk_cache(directory: &Path) -> Result<XetChunkCacheVerifyStats> {
    verify_xet_chunk_cache_with_cancel(directory, &CancellationToken::new()).await
}

/// Validate xet-core range files while honoring a caller cancellation.
pub async fn verify_xet_chunk_cache_with_cancel(
    directory: &Path,
    cancel: &CancellationToken,
) -> Result<XetChunkCacheVerifyStats> {
    let parent = directory.parent().ok_or_else(|| CacheError::UnsafeRoot {
        path: directory.display().to_string(),
        reason: "range directory has no cache parent".into(),
    })?;
    let name = directory
        .file_name()
        .ok_or_else(|| CacheError::UnsafeRoot {
            path: directory.display().to_string(),
            reason: "range directory has no name".into(),
        })?
        .to_owned();
    let display_root = parent.to_owned();
    let directory = directory.to_owned();
    crate::private_fs::run_blocking(cancel, move |cancel| {
        let (root, catalog_root) = match PinnedRoot::open_with_private_parent(&directory) {
            Ok(roots) => roots,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(XetChunkCacheVerifyStats::default());
            }
            Err(error) => return Err(error),
        };
        let mut removal =
            crate::catalog::PayloadRemoval::open(catalog_root.as_ref(), &display_root, false)?;
        let mut stats = XetChunkCacheVerifyStats::default();
        visit_xet_chunk_cache_entries(&root, cancel, |entry| {
            let relative = Path::new(&name).join(&entry.path);
            let result = removal.remove(&relative, || {
                root.remove_file_if(&entry.path, false, &mut |file| {
                    let valid = verify_xet_chunk_cache_entry(file, &entry.path, cancel)?;
                    check_cancelled(cancel)?;
                    Ok(!valid)
                })
            });
            let corrupt = match result {
                Ok(removed) => removed.is_some(),
                Err(CacheError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            stats.total = stats.total.saturating_add(1);
            if corrupt {
                stats.corrupt = stats.corrupt.saturating_add(1);
            } else {
                stats.valid = stats.valid.saturating_add(1);
            }
            Ok(())
        })?;
        Ok(stats)
    })
    .await
}

fn verify_xet_chunk_cache_entry(
    file: &mut fs::File,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<bool> {
    check_cancelled(cancel)?;
    let Some((start, end, expected_bytes, expected_crc)) =
        path.file_name().and_then(decode_range_item_name)
    else {
        return Ok(false);
    };
    let bytes = file.metadata()?.len();
    if start >= end || expected_bytes != bytes || bytes < 8 {
        return Ok(false);
    }

    let mut count_bytes = [0u8; 4];
    if !read_exact_or_corrupt(file, &mut count_bytes)? {
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
    let mut crc = crc32fast::Hasher::new();
    crc.update(&count_bytes);
    for _ in 0..count {
        check_cancelled(cancel)?;
        if !read_exact_or_corrupt(file, &mut offset_bytes)? {
            return Ok(false);
        }
        crc.update(&offset_bytes);
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

    let mut hasher = blake3::Hasher::new();
    let mut remaining = bytes - header_bytes;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        check_cancelled(cancel)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let Some(rest) = remaining.checked_sub(read as u64) else {
            return Ok(false);
        };
        remaining = rest;
        crc.update(&buffer[..read]);
        hasher.update(&buffer[..read]);
    }
    if remaining != 0 || crc.finalize() != expected_crc {
        return Ok(false);
    }
    let key = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .and_then(|name| base64::engine::general_purpose::URL_SAFE.decode(name).ok());
    if let Some(key) = key.filter(|key| key.get(32..) == Some(CHUNK_HASH_PREFIX.as_bytes())) {
        return Ok(start == 0 && end == 1 && key[..32] == hasher.finalize().as_bytes()[..]);
    }
    Ok(true)
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
    modified: u64,
}

fn collect_xet_chunk_cache_entries(
    root: &PinnedRoot,
    cancel: &CancellationToken,
) -> Result<Vec<XetChunkCacheEntry>> {
    let mut entries = Vec::new();
    visit_xet_chunk_cache_entries(root, cancel, |entry| {
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
    root: &PinnedRoot,
    cancel: &CancellationToken,
    mut visit: impl FnMut(XetChunkCacheEntry) -> Result<()>,
) -> Result<()> {
    check_cancelled(cancel)?;
    root.visit_selected_files(
        &|relative| {
            check_cancelled(cancel)?;
            Ok(!matches!(
                range_entry_kind(relative),
                crate::clean::EntryKind::Retain
            ))
        },
        &mut |relative, metadata| {
            if matches!(range_entry_kind(relative), crate::clean::EntryKind::Payload) {
                visit(XetChunkCacheEntry {
                    path: relative.to_owned(),
                    bytes: metadata.size,
                    modified: metadata.modified_ns,
                })?;
            }
            Ok(())
        },
    )
}

fn range_entry_kind(relative: &Path) -> crate::clean::EntryKind {
    let Some(parts) = std::iter::once(Some("chunks"))
        .chain(relative.iter().map(|part| part.to_str()))
        .collect::<Option<Vec<_>>>()
    else {
        return crate::clean::EntryKind::Retain;
    };
    crate::clean::range_entry_kind(&parts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod maintenance;

    #[cfg(unix)]
    mod read_repair;

    fn test_key() -> Key {
        Key {
            prefix: "repo".to_owned(),
            hash: Default::default(),
        }
    }

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
        let dir = cache_dir
            .join("AA")
            .join(base64::engine::general_purpose::URL_SAFE.encode([0u8; 32]));
        let path = dir.join(name);
        crate::private_fs::atomic_write(cache_dir, &path, &body)
            .await
            .unwrap();
        path
    }

    #[tokio::test]
    async fn range_hits_update_recency_but_missing_ranges_do_not() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache/chunks");
        let path = write_xet_range(&root, b"data").await;
        let cache = XetChunkCacheHandle::open(&root, 1024 * 1024).unwrap();
        let key = Key {
            prefix: String::new(),
            hash: Default::default(),
        };
        let file = std::fs::File::open(path).unwrap();
        let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(60);
        file.set_modified(old_time).unwrap();
        assert!(
            cache
                .cache
                .get(&key, &ChunkRange::new(0, 2))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(file.metadata().unwrap().modified().unwrap(), old_time);
        assert_eq!(
            cache
                .cache
                .get(&key, &ChunkRange::new(0, 1))
                .await
                .unwrap()
                .unwrap()
                .data,
            b"data"
        );
        assert!(file.metadata().unwrap().modified().unwrap() > old_time);
    }

    #[tokio::test]
    async fn open_reads_empty_cache_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache").join("chunks");
        let handle =
            XetChunkCacheHandle::open(cache_dir.clone(), 64 * 1024).expect("should open cache");

        assert_eq!(handle.directory, std::fs::canonicalize(cache_dir).unwrap());
        assert_eq!(handle.size_bytes, 64 * 1024);

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.total_bytes, 0);
    }

    #[tokio::test]
    async fn range_publication_lease_survives_all_range_maintenance() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let ranges = root.join("chunks");
        let path = write_xet_range(&ranges, b"range payload").await;
        let body = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let catalog = crate::catalog::CacheCatalog::new(root.clone(), 1024 * 1024);
        let reservation = catalog
            .reserve(&path, body.len() as u64)
            .await
            .unwrap()
            .unwrap();
        let reservation = reservation.write(&body).await.unwrap();
        for dry_run in [false, true] {
            let pruned = prune_xet_chunk_cache(&ranges, 0, dry_run, true)
                .await
                .unwrap();
            assert_eq!(pruned.entries_evicted, 0);
            let clean = crate::clean_cache(&root, dry_run, &CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(clean.files_removed, 0);
            assert_eq!(clean.busy_entries, 1);
        }
        assert_eq!(verify_xet_chunk_cache(&ranges).await.unwrap().total, 0);
        assert_eq!(std::fs::read(&path).unwrap(), body);
        drop(reservation);
        assert_eq!(verify_xet_chunk_cache(&ranges).await.unwrap().valid, 1);
        assert_eq!(
            prune_xet_chunk_cache(&ranges, 0, false, false)
                .await
                .unwrap()
                .entries_evicted,
            1
        );
    }

    #[tokio::test]
    async fn crab_range_cache_serves_exact_and_contained_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let handle =
            XetChunkCacheHandle::open(tmp.path().join("cache").join("chunks"), 64 * 1024).unwrap();
        let key = test_key();
        let stored = ChunkRange::new(2, 5);
        handle
            .cache
            .put(&key, &stored, &[0, 3, 7, 9], b"abcdefghi")
            .await
            .unwrap();

        let exact = handle.cache.get(&key, &stored).await.unwrap().unwrap();
        assert_eq!(exact.data, b"abcdefghi");
        assert_eq!(exact.offsets, vec![0, 3, 7, 9]);

        let contained = handle
            .cache
            .get(&key, &ChunkRange::new(3, 5))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(contained.data, b"defghi");
        assert_eq!(contained.offsets, vec![0, 4, 6]);
    }

    #[tokio::test]
    async fn chunk_identity_checks_do_not_change_xorb_ranges_or_evict_on_bad_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = XetChunkCacheHandle::open(tmp.path().join("cache/chunks"), 64 * 1024).unwrap();
        let content = b"chunk bytes";
        let chunk = Key {
            prefix: CHUNK_HASH_PREFIX.into(),
            hash: (*blake3::hash(content).as_bytes()).into(),
        };
        let whole = ChunkRange::new(0, 1);
        handle
            .cache
            .put(&chunk, &whole, &[0, 11], content)
            .await
            .unwrap();

        assert!(matches!(
            handle.cache.get(&chunk, &ChunkRange::new(0, 2)).await,
            Err(ChunkCacheError::InvalidArguments)
        ));
        assert_eq!(
            handle
                .cache
                .get(&chunk, &whole)
                .await
                .unwrap()
                .unwrap()
                .data,
            content
        );

        // An xorb key is not the hash of its decoded bytes. The same bytes
        // under another namespace retain the normal partial-range contract.
        let xorb = test_key();
        handle
            .cache
            .put(&xorb, &whole, &[0, 11], content)
            .await
            .unwrap();
        assert_eq!(
            handle.cache.get(&xorb, &whole).await.unwrap().unwrap().data,
            content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn range_cache_rechecks_product_root_privacy_on_read() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let handle = XetChunkCacheHandle::open(root.join("chunks"), 64 * 1024).unwrap();
        let range = ChunkRange::new(0, 1);
        handle
            .cache
            .put(&test_key(), &range, &[0, 7], b"payload")
            .await
            .unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            handle
                .cache
                .get(&test_key(), &range)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn range_filename_cannot_authorize_an_unbounded_allocation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        crate::root::ensure_private_cache_directory(&root).unwrap();
        let range_root = root.join("chunks");
        crate::root::ensure_private_cache_directory(&range_root).unwrap();
        let cache = CrabRangeCache {
            root: range_root,
            capacity: 64 * 1024,
            catalog: crate::catalog::CacheCatalog::new(root.clone(), 64 * 1024),
        };
        let range = ChunkRange::new(0, u32::MAX);
        let path = cache
            .key_directory(&test_key())
            .join(encode_range_item_name(&range, u64::MAX, 0));
        crate::private_fs::atomic_write(&root, &path, b"invalid")
            .await
            .unwrap();
        assert!(
            cache
                .get(&test_key(), &ChunkRange::new(0, 1))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn range_cache_creates_private_roots_and_files() {
        use std::os::unix::fs::MetadataExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache").join("chunks");
        let handle = XetChunkCacheHandle::open(&cache_dir, 64 * 1024).unwrap();
        handle
            .cache
            .put(&test_key(), &ChunkRange::new(0, 1), &[0, 7], b"payload")
            .await
            .unwrap();

        assert_eq!(
            std::fs::symlink_metadata(&cache_dir).unwrap().mode() & 0o777,
            0o700
        );
        let mut prefix_dirs = std::fs::read_dir(&cache_dir).unwrap();
        let prefix = prefix_dirs.next().unwrap().unwrap().path();
        let key_dir = std::fs::read_dir(prefix)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let entry = std::fs::read_dir(key_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::symlink_metadata(entry).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn range_cache_rejects_unsafe_root() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache").join("chunks");
        crate::root::ensure_private_cache_directory(cache_dir.parent().unwrap()).unwrap();
        std::fs::create_dir(&cache_dir).unwrap();
        std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            XetChunkCacheHandle::open(cache_dir, 64 * 1024),
            Err(CacheError::UnsafeRoot { .. })
        ));
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
        let cache_dir = tmp.path().join("cache").join("chunks");

        let first = XetChunkCacheHandle::open(cache_dir.clone(), 64 * 1024).unwrap();
        let second = XetChunkCacheHandle::open(cache_dir, 64 * 1024).unwrap();

        assert!(Arc::ptr_eq(&first.cache, &second.cache));
    }

    #[tokio::test]
    async fn live_same_root_handles_reject_conflicting_budgets() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache").join("chunks");
        let first = XetChunkCacheHandle::open(cache_dir.clone(), 64 * 1024).unwrap();

        let error = XetChunkCacheHandle::open(cache_dir.clone(), 32 * 1024).unwrap_err();

        assert!(matches!(
            error,
            CacheError::BudgetConflict {
                path,
                active_bytes: 65_536,
                requested_bytes: 32_768,
            } if path == cache_dir.canonicalize().unwrap().display().to_string()
        ));
        drop(first);
    }

    #[tokio::test]
    async fn root_accepts_a_new_budget_after_last_handle_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache").join("chunks");
        let first = XetChunkCacheHandle::open(cache_dir.clone(), 64 * 1024).unwrap();
        drop(first);

        let reopened = XetChunkCacheHandle::open(cache_dir, 32 * 1024).unwrap();

        assert_eq!(reopened.size_bytes, 32 * 1024);
    }

    #[tokio::test]
    async fn stats_is_read_only_and_counts_range_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("chunks");
        let path = write_xet_range(&cache_dir, b"12345").await;
        tokio::fs::write(cache_dir.join("unknown"), b"ignored")
            .await
            .unwrap();

        let stats = xet_chunk_cache_stats(&cache_dir).await.unwrap();

        assert_eq!(stats.entries, 1);
        assert_eq!(stats.total_bytes, 17);
        assert_eq!(std::fs::metadata(path).unwrap().len(), 17);
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
        let old = write_xet_range(&cache_dir, &[1u8; 8]).await;
        write_xet_range(&cache_dir, &[2u8; 8]).await;
        fs::File::open(&old)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();

        let plan = prune_xet_chunk_cache(&cache_dir, 20, true, true)
            .await
            .unwrap();
        assert_eq!(plan.entries_evicted, 1);
        assert_eq!(plan.entries, [(old, 20)]);
        assert_eq!(plan.bytes_freed, 20);
        assert_eq!(xet_chunk_cache_stats(&cache_dir).await.unwrap().entries, 2);

        let applied = prune_xet_chunk_cache(&cache_dir, 20, false, true)
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
        let dir = cache_dir
            .join("AA")
            .join(base64::engine::general_purpose::URL_SAFE.encode([0u8; 32]));
        let path = dir.join(name);
        crate::private_fs::atomic_write(&cache_dir, &path, &body)
            .await
            .unwrap();

        let report = verify_xet_chunk_cache(&cache_dir).await.unwrap();
        assert_eq!(report.valid, 1);

        tokio::fs::write(&path, &body[..3]).await.unwrap();
        let report = verify_xet_chunk_cache(&cache_dir).await.unwrap();
        assert_eq!(report.corrupt, 1);
        assert!(!path.exists());
    }
}
