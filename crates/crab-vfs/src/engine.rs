//! VFS engine: read/write dispatch and copy-on-write promotion.
//!
//! The engine sits between protocol adapters and the lower components
//! (resolver, hydration service, overlay). It dispatches
//! reads to either the overlay backing file or the hydration service,
//! and routes writes through the copy-on-write overlay.
//!
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, RwLock as AsyncRwLock,
};
use tracing::{debug, trace, warn};

use crate::blob::OdbReader;
use crate::blob::{FilePointer, PointerIdentity};
use crate::core::error::{CrabError, Result};
use crate::hydration::{HydrationReadStatsSnapshot, HydrationService};
use crate::resolver::{FuseResolver, OverlayEntry, ResolvedNode, SnapshotView};
use crate::snapshot::{BaseNode, NodeType, SnapshotStore};

#[cfg(feature = "fuse")]
mod open_file;
#[cfg(feature = "fuse")]
pub(crate) use open_file::VfsOpenFile;

// ---------------------------------------------------------------------------
// OverlayWriter trait
// ---------------------------------------------------------------------------

/// Trait for overlay write operations.
pub trait OverlayWriter: Send + Sync {
    /// Look up a single path in the overlay (read side, for checking
    /// whether a file is already promoted).
    fn get(&self, path: &str) -> Option<OverlayEntry>;

    /// Get the local backing file path for an overlay entry.
    /// Returns `None` if the path is not in the overlay.
    fn get_backing_path(&self, path: &str) -> Option<PathBuf>;

    /// Create a new file in the overlay (empty backing file).
    fn create_file(&self, path: &str, mode: u32) -> Result<OverlayEntry>;

    /// Write data to an overlay backing file at the given offset.
    /// The file must already exist in the overlay (via `create_file`
    /// or `promote`).
    fn write_file(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize>;

    /// Promote a base file into the overlay with initial content.
    /// Creates the backing file and writes `content` into it.
    /// `source_oid` records the git blob OID at promotion time for
    /// OID-aware reconciliation.
    fn promote(
        &self,
        path: &str,
        mode: u32,
        content: &[u8],
        source_oid: Option<&str>,
    ) -> Result<OverlayEntry>;

    /// Mark a path as deleted in the overlay.
    fn remove(&self, path: &str) -> Result<()>;

    /// Rename a path in the overlay.
    fn rename(&self, old_path: &str, new_path: &str) -> Result<()>;

    /// Record a metadata-only rename for paths that still live in the
    /// base snapshot. This preserves large-file laziness: unchanged Crab
    /// pointer blobs move in Git, while content is hydrated only if a
    /// later write promotes the file.
    fn rename_base_subtree(&self, entries: &[BaseRenameEntry]) -> Result<()>;

    /// Create a directory in the overlay.
    fn mkdir(&self, path: &str, mode: u32) -> Result<()>;

    /// Remove a directory from the overlay (must be empty).
    fn rmdir(&self, path: &str) -> Result<()>;

    /// Update the modification time of an overlay entry.
    fn set_mtime(&self, path: &str, mtime_ns: i64) -> Result<()>;

    /// Update permission bits of an overlay entry.
    fn set_mode(&self, path: &str, mode: u32) -> Result<()>;

    /// Mutate descriptor content and record its metadata under one publication lease.
    /// A detached file has no path and must not create overlay metadata.
    fn mutate_content(
        &self,
        path: Option<&str>,
        file: &std::fs::File,
        mtime_ns: i64,
        mutation: &mut dyn FnMut(&std::fs::File) -> Result<()>,
    ) -> Result<()>;

    /// Record an overlay entry after the backing file has already been
    /// written to disk (e.g. by streaming promotion). Unlike `promote`,
    /// this does NOT write file content — the caller is responsible for
    /// placing the backing file at `backing_path(path)` before calling.
    fn promote_from_file(
        &self,
        path: &str,
        mode: u32,
        size: u64,
        source_oid: Option<&str>,
    ) -> Result<OverlayEntry>;

    /// Return the final backing file path for a given overlay path.
    fn backing_path_for(&self, path: &str) -> PathBuf;

    /// Create a symlink in the overlay.
    fn create_symlink(&self, path: &str, target: &str, mode: u32) -> Result<OverlayEntry>;

    /// Flush a path's local backing content to stable storage.
    fn sync_path(&self, _path: &str) -> Result<()> {
        Ok(())
    }

    /// Flush overlay metadata needed to rediscover synced content.
    fn checkpoint(&self) -> Result<()> {
        Ok(())
    }
}

/// A base snapshot node moved into a new overlay path without copying content.
#[derive(Debug, Clone)]
pub struct BaseRenameEntry {
    pub old_path: String,
    pub new_path: String,
    pub node_type: NodeType,
    pub mode: u32,
    pub size: u64,
    pub source_oid: Option<String>,
}

// ---------------------------------------------------------------------------
// VfsEngine
// ---------------------------------------------------------------------------

const STALE_READ_LEASE_GENERATION: &str = "stale VFS read lease generation";
const STALE_READ_LEASE_OVERLAY_VIEW: &str = "stale VFS read lease overlay view";
const STALE_READ_LEASE_OVERLAY_FILE: &str = "stale VFS read lease overlay file";
const DEFAULT_READ_SOURCE_CACHE_MAX_ENTRIES: usize = 1024;
const DEFAULT_READ_SOURCE_CACHE_MAX_ESTIMATED_BYTES: usize = 16 * 1024 * 1024;
const MAX_OVERLAY_INVALIDATION_ENTRIES: usize = 4096;

/// Identity of a read source served by the VFS engine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReadSourceKey {
    BasePointer {
        generation: i64,
        overlay_version: u64,
        file_hash: PointerIdentity,
        size: u64,
    },
    BaseBlob {
        generation: i64,
        overlay_version: u64,
        object_oid: String,
        known_size: Option<u64>,
    },
    BaseEmpty {
        generation: i64,
        overlay_version: u64,
        path: String,
    },
    OverlayFile {
        path: String,
        overlay_version: u64,
        size: u64,
        mtime_ns: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsInvalidation {
    PathChanged {
        path: String,
    },
    SubtreeRemoved {
        path: String,
    },
    SubtreeRenamed {
        old_path: String,
        new_path: String,
    },
    SnapshotGenerationChanged {
        old_generation: Option<i64>,
        new_generation: i64,
    },
    OverlayReset,
}

impl ReadSourceKey {
    /// Known size of this source, if reading it can decide EOF without probing.
    pub fn known_size(&self) -> Option<u64> {
        match self {
            Self::BasePointer { size, .. } | Self::OverlayFile { size, .. } => Some(*size),
            Self::BaseBlob { known_size, .. } => *known_size,
            Self::BaseEmpty { .. } => Some(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadSourceKind {
    BasePointer,
    BaseBlob,
    BaseEmpty,
    OverlayFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdaptiveReadClass {
    First,
    Sequential,
    Strided,
    Repeated,
    Random,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdaptiveReadDecision {
    class: AdaptiveReadClass,
    prefetch: Option<AdaptivePrefetch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdaptivePrefetch {
    NextWindow,
    TargetWindow { offset: u64, size: u32 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VfsAdaptiveReadMetricsSnapshot {
    pub first: u64,
    pub sequential: u64,
    pub strided: u64,
    pub repeated: u64,
    pub random: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VfsSourceReadMetricsSnapshot {
    pub reads: u64,
    pub bytes: u64,
    pub adaptive: VfsAdaptiveReadMetricsSnapshot,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VfsReadMetricsSnapshot {
    pub open_read_calls: u64,
    pub read_at_calls: u64,
    pub returned_bytes: u64,
    pub stale_generation_rejections: u64,
    pub stale_overlay_view_rejections: u64,
    pub stale_overlay_file_rejections: u64,
    pub source_cache_entries: usize,
    pub source_cache_max_entries: usize,
    pub source_cache_estimated_bytes: usize,
    pub source_cache_max_estimated_bytes: usize,
    pub source_cache_hits: u64,
    pub resolver_calls_avoided: u64,
    pub source_cache_misses: u64,
    pub source_cache_evictions: u64,
    pub source_cache_invalidations: u64,
    pub source_cache_stale_evictions: u64,
    pub invalidation_path_events: u64,
    pub invalidation_subtree_events: u64,
    pub invalidation_rename_events: u64,
    pub invalidation_generation_events: u64,
    pub invalidation_overlay_reset_events: u64,
    pub invalidation_compacted_full_resets: u64,
    pub base_pointer: VfsSourceReadMetricsSnapshot,
    pub base_blob: VfsSourceReadMetricsSnapshot,
    pub base_empty: VfsSourceReadMetricsSnapshot,
    pub overlay_file: VfsSourceReadMetricsSnapshot,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct VfsReadSourceCacheSnapshot {
    entries: usize,
    max_entries: usize,
    estimated_bytes: usize,
    max_estimated_bytes: usize,
    hits: u64,
    resolver_calls_avoided: u64,
    misses: u64,
    evictions: u64,
    invalidations: u64,
    stale_evictions: u64,
}

#[derive(Default)]
struct VfsReadMetrics {
    open_read_calls: AtomicU64,
    read_at_calls: AtomicU64,
    returned_bytes: AtomicU64,
    stale_generation_rejections: AtomicU64,
    stale_overlay_view_rejections: AtomicU64,
    stale_overlay_file_rejections: AtomicU64,
    invalidation_path_events: AtomicU64,
    invalidation_subtree_events: AtomicU64,
    invalidation_rename_events: AtomicU64,
    invalidation_generation_events: AtomicU64,
    invalidation_overlay_reset_events: AtomicU64,
    invalidation_compacted_full_resets: AtomicU64,
    base_pointer: VfsSourceReadMetrics,
    base_blob: VfsSourceReadMetrics,
    base_empty: VfsSourceReadMetrics,
    overlay_file: VfsSourceReadMetrics,
}

#[derive(Default)]
struct VfsSourceReadMetrics {
    reads: AtomicU64,
    bytes: AtomicU64,
    first: AtomicU64,
    sequential: AtomicU64,
    strided: AtomicU64,
    repeated: AtomicU64,
    random: AtomicU64,
}

#[derive(Default)]
struct ReadPatternState {
    cursor: Mutex<Option<ReadPatternCursor>>,
}

#[derive(Debug, Clone, Copy)]
struct ReadPatternCursor {
    offset: u64,
    end: u64,
    size: u32,
    delta: Option<i128>,
}

/// A validated read source opened through the VFS engine.
#[derive(Clone)]
pub struct VfsReadLease {
    key: ReadSourceKey,
    source: Arc<ReadSource>,
    pattern: Arc<ReadPatternState>,
    // A lease can outlive publication and the source cache that first held it.
    _snapshot_view: Option<Arc<SnapshotView>>,
}

impl VfsReadLease {
    fn new(
        key: ReadSourceKey,
        source: ReadSource,
        snapshot_view: Option<Arc<SnapshotView>>,
    ) -> Self {
        Self {
            key,
            source: Arc::new(source),
            pattern: Arc::new(ReadPatternState::default()),
            _snapshot_view: snapshot_view,
        }
    }

    /// Return the content identity that proves what bytes this lease serves.
    pub fn key(&self) -> &ReadSourceKey {
        &self.key
    }

    /// Known size of the leased source, if available.
    pub fn known_size(&self) -> Option<u64> {
        self.key.known_size()
    }

    /// Estimated pool memory used by this lease and its source identity.
    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + std::mem::size_of::<ReadSourceKey>()
            + read_source_key_heap_bytes(&self.key)
            + std::mem::size_of::<ReadSource>()
            + read_source_heap_bytes(self.source.as_ref())
            + std::mem::size_of::<ReadPatternState>()
    }

    pub(crate) async fn pin_staged_file(&self) -> Result<Option<crab_staging::StagedFilePin>> {
        match self.source.as_ref() {
            ReadSource::BasePointer { pointer, .. } => match self
                ._snapshot_view
                .as_ref()
                .and_then(|view| view.hydration.as_ref())
            {
                Some(hydration) => hydration.pin_file(pointer).await,
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn record_read_pattern(&self, offset: u64, size: u32) -> AdaptiveReadDecision {
        self.pattern.record_read(offset, size)
    }

    fn path(&self) -> &str {
        match self.source.as_ref() {
            ReadSource::BasePointer { path, .. }
            | ReadSource::BaseBlob { path, .. }
            | ReadSource::OverlayFile { path, .. } => path,
            ReadSource::BaseEmpty => match &self.key {
                ReadSourceKey::BaseEmpty { path, .. } => path,
                _ => "",
            },
        }
    }

    #[cfg(test)]
    pub fn for_test(key: ReadSourceKey) -> Self {
        Self::new(key, ReadSource::BaseEmpty, None)
    }
}

#[derive(Debug)]
enum ReadSource {
    BasePointer { path: String, pointer: FilePointer },
    BaseBlob { path: String, oid: String },
    BaseEmpty,
    OverlayFile { path: String, backing: PathBuf },
}

impl ReadSource {
    fn kind(&self) -> ReadSourceKind {
        match self {
            Self::BasePointer { .. } => ReadSourceKind::BasePointer,
            Self::BaseBlob { .. } => ReadSourceKind::BaseBlob,
            Self::BaseEmpty => ReadSourceKind::BaseEmpty,
            Self::OverlayFile { .. } => ReadSourceKind::OverlayFile,
        }
    }
}

impl ReadPatternState {
    fn record_read(&self, offset: u64, size: u32) -> AdaptiveReadDecision {
        let end = offset.saturating_add(u64::from(size));
        let mut cursor = self
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (class, prefetch) = match *cursor {
            None => (AdaptiveReadClass::First, None),
            Some(previous) if previous.offset == offset && previous.size == size => {
                (AdaptiveReadClass::Repeated, None)
            }
            Some(previous) if previous.end == offset => (
                AdaptiveReadClass::Sequential,
                Some(AdaptivePrefetch::NextWindow),
            ),
            Some(previous) => {
                let delta = i128::from(offset) - i128::from(previous.offset);
                if delta > 0
                    && previous.delta == Some(delta)
                    && previous.size == size
                    && offset != previous.end
                {
                    (
                        AdaptiveReadClass::Strided,
                        u64::try_from(delta)
                            .ok()
                            .map(|stride| AdaptivePrefetch::TargetWindow {
                                offset: offset.saturating_add(stride),
                                size,
                            }),
                    )
                } else {
                    (AdaptiveReadClass::Random, None)
                }
            }
        };

        let delta = cursor.map(|previous| i128::from(offset) - i128::from(previous.offset));
        *cursor = Some(ReadPatternCursor {
            offset,
            end,
            size,
            delta,
        });
        AdaptiveReadDecision { class, prefetch }
    }
}

impl VfsReadMetrics {
    fn record_open_read(&self) {
        self.open_read_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn record_read_at_call(&self) {
        self.read_at_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn record_stale_generation(&self) {
        self.stale_generation_rejections
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_stale_overlay_view(&self) {
        self.stale_overlay_view_rejections
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_stale_overlay_file(&self) {
        self.stale_overlay_file_rejections
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_invalidation(&self, invalidation: &VfsInvalidation) {
        match invalidation {
            VfsInvalidation::PathChanged { .. } => &self.invalidation_path_events,
            VfsInvalidation::SubtreeRemoved { .. } => &self.invalidation_subtree_events,
            VfsInvalidation::SubtreeRenamed { .. } => &self.invalidation_rename_events,
            VfsInvalidation::SnapshotGenerationChanged { .. } => {
                &self.invalidation_generation_events
            }
            VfsInvalidation::OverlayReset => &self.invalidation_overlay_reset_events,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    fn record_compacted_full_reset(&self) {
        self.invalidation_compacted_full_resets
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_read_success(
        &self,
        source: ReadSourceKind,
        class: AdaptiveReadClass,
        returned_bytes: usize,
    ) {
        self.returned_bytes
            .fetch_add(returned_bytes as u64, Ordering::Relaxed);
        self.source(source)
            .record_read_success(class, returned_bytes);
    }

    fn source(&self, source: ReadSourceKind) -> &VfsSourceReadMetrics {
        match source {
            ReadSourceKind::BasePointer => &self.base_pointer,
            ReadSourceKind::BaseBlob => &self.base_blob,
            ReadSourceKind::BaseEmpty => &self.base_empty,
            ReadSourceKind::OverlayFile => &self.overlay_file,
        }
    }

    fn snapshot(&self) -> VfsReadMetricsSnapshot {
        VfsReadMetricsSnapshot {
            open_read_calls: self.open_read_calls.load(Ordering::Relaxed),
            read_at_calls: self.read_at_calls.load(Ordering::Relaxed),
            returned_bytes: self.returned_bytes.load(Ordering::Relaxed),
            stale_generation_rejections: self.stale_generation_rejections.load(Ordering::Relaxed),
            stale_overlay_view_rejections: self
                .stale_overlay_view_rejections
                .load(Ordering::Relaxed),
            stale_overlay_file_rejections: self
                .stale_overlay_file_rejections
                .load(Ordering::Relaxed),
            source_cache_entries: 0,
            source_cache_max_entries: 0,
            source_cache_estimated_bytes: 0,
            source_cache_max_estimated_bytes: 0,
            source_cache_hits: 0,
            resolver_calls_avoided: 0,
            source_cache_misses: 0,
            source_cache_evictions: 0,
            source_cache_invalidations: 0,
            source_cache_stale_evictions: 0,
            invalidation_path_events: self.invalidation_path_events.load(Ordering::Relaxed),
            invalidation_subtree_events: self.invalidation_subtree_events.load(Ordering::Relaxed),
            invalidation_rename_events: self.invalidation_rename_events.load(Ordering::Relaxed),
            invalidation_generation_events: self
                .invalidation_generation_events
                .load(Ordering::Relaxed),
            invalidation_overlay_reset_events: self
                .invalidation_overlay_reset_events
                .load(Ordering::Relaxed),
            invalidation_compacted_full_resets: self
                .invalidation_compacted_full_resets
                .load(Ordering::Relaxed),
            base_pointer: self.base_pointer.snapshot(),
            base_blob: self.base_blob.snapshot(),
            base_empty: self.base_empty.snapshot(),
            overlay_file: self.overlay_file.snapshot(),
        }
    }
}

impl VfsSourceReadMetrics {
    fn record_read_success(&self, class: AdaptiveReadClass, returned_bytes: usize) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes
            .fetch_add(returned_bytes as u64, Ordering::Relaxed);
        match class {
            AdaptiveReadClass::First => &self.first,
            AdaptiveReadClass::Sequential => &self.sequential,
            AdaptiveReadClass::Strided => &self.strided,
            AdaptiveReadClass::Repeated => &self.repeated,
            AdaptiveReadClass::Random => &self.random,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> VfsSourceReadMetricsSnapshot {
        VfsSourceReadMetricsSnapshot {
            reads: self.reads.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            adaptive: VfsAdaptiveReadMetricsSnapshot {
                first: self.first.load(Ordering::Relaxed),
                sequential: self.sequential.load(Ordering::Relaxed),
                strided: self.strided.load(Ordering::Relaxed),
                repeated: self.repeated.load(Ordering::Relaxed),
                random: self.random.load(Ordering::Relaxed),
            },
        }
    }
}

struct VfsReadSourceCache {
    state: Mutex<VfsReadSourceCacheState>,
    max_entries: usize,
    max_estimated_bytes: usize,
}

struct VfsReadSourceCacheState {
    entries: HashMap<String, CachedVfsReadSource>,
    estimated_bytes: usize,
    access_clock: u64,
    hits: u64,
    resolver_calls_avoided: u64,
    misses: u64,
    evictions: u64,
    invalidations: u64,
    stale_evictions: u64,
}

struct CachedVfsReadSource {
    lease: VfsReadLease,
    last_access: u64,
    estimated_bytes: usize,
}

impl VfsReadSourceCache {
    fn new(max_entries: usize, max_estimated_bytes: usize) -> Self {
        Self {
            state: Mutex::new(VfsReadSourceCacheState {
                entries: HashMap::new(),
                estimated_bytes: 0,
                access_clock: 0,
                hits: 0,
                resolver_calls_avoided: 0,
                misses: 0,
                evictions: 0,
                invalidations: 0,
                stale_evictions: 0,
            }),
            max_entries: max_entries.max(1),
            max_estimated_bytes: max_estimated_bytes.max(1),
        }
    }

    fn candidate(&self, path: &str) -> Option<VfsReadLease> {
        let mut state = self.lock_state();
        let last_access = state.next_access();
        let entry = state.entries.get_mut(path)?;
        entry.last_access = last_access;
        Some(entry.lease.clone())
    }

    fn record_hit(&self) {
        let mut state = self.lock_state();
        state.hits = state.hits.saturating_add(1);
        state.resolver_calls_avoided = state.resolver_calls_avoided.saturating_add(1);
    }

    fn record_miss(&self) {
        let mut state = self.lock_state();
        state.misses = state.misses.saturating_add(1);
    }

    fn insert(&self, path: String, lease: VfsReadLease) {
        let mut state = self.lock_state();
        let estimated_bytes = lease.estimated_bytes();
        let last_access = state.next_access();
        if let Some(entry) = state.entries.get_mut(&path) {
            let previous_estimated_bytes = entry.estimated_bytes;
            entry.lease = lease;
            entry.last_access = last_access;
            entry.estimated_bytes = estimated_bytes;
            let _ = entry;
            state.estimated_bytes = state
                .estimated_bytes
                .saturating_sub(previous_estimated_bytes)
                .saturating_add(estimated_bytes);
        } else {
            state.estimated_bytes = state.estimated_bytes.saturating_add(estimated_bytes);
            state.entries.insert(
                path,
                CachedVfsReadSource {
                    lease,
                    last_access,
                    estimated_bytes,
                },
            );
        }
        state.shrink(self.max_entries, self.max_estimated_bytes);
    }

    fn evict_stale(&self, path: &str) {
        let mut state = self.lock_state();
        if state.remove_entry(path, true) {
            state.stale_evictions = state.stale_evictions.saturating_add(1);
        }
    }

    fn invalidate_path(&self, path: &str) {
        let mut state = self.lock_state();
        state.remove_entry(path, false);
        state.invalidations = state.invalidations.saturating_add(1);
    }

    fn invalidate_subtree(&self, path: &str) {
        let mut state = self.lock_state();
        let stale_paths = state
            .entries
            .keys()
            .filter(|entry_path| path_is_at_or_under(entry_path, path))
            .cloned()
            .collect::<Vec<_>>();
        for stale_path in stale_paths {
            state.remove_entry(&stale_path, false);
        }
        state.invalidations = state.invalidations.saturating_add(1);
    }

    fn invalidate_all(&self) {
        let mut state = self.lock_state();
        if !state.entries.is_empty() {
            state.entries.clear();
            state.estimated_bytes = 0;
        }
        state.invalidations = state.invalidations.saturating_add(1);
    }

    fn snapshot(&self) -> VfsReadSourceCacheSnapshot {
        let state = self.lock_state();
        VfsReadSourceCacheSnapshot {
            entries: state.entries.len(),
            max_entries: self.max_entries,
            estimated_bytes: state.estimated_bytes,
            max_estimated_bytes: self.max_estimated_bytes,
            hits: state.hits,
            resolver_calls_avoided: state.resolver_calls_avoided,
            misses: state.misses,
            evictions: state.evictions,
            invalidations: state.invalidations,
            stale_evictions: state.stale_evictions,
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, VfsReadSourceCacheState> {
        self.state.lock().unwrap_or_else(|error| {
            warn!("VFS read source cache mutex was poisoned; recovering");
            error.into_inner()
        })
    }
}

impl VfsReadSourceCacheState {
    fn next_access(&mut self) -> u64 {
        self.access_clock = self.access_clock.saturating_add(1);
        self.access_clock
    }

    fn remove_entry(&mut self, path: &str, count_eviction: bool) -> bool {
        let Some(entry) = self.entries.remove(path) else {
            return false;
        };
        self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.estimated_bytes);
        if count_eviction {
            self.evictions = self.evictions.saturating_add(1);
        }
        true
    }

    fn shrink(&mut self, max_entries: usize, max_estimated_bytes: usize) {
        while self.entries.len() > max_entries || self.estimated_bytes > max_estimated_bytes {
            if self.entries.len() == 1 && self.estimated_bytes > max_estimated_bytes {
                return;
            }
            let Some(evict_path) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(path, _)| path.clone())
            else {
                return;
            };
            self.remove_entry(&evict_path, true);
        }
    }
}

fn read_source_key_heap_bytes(key: &ReadSourceKey) -> usize {
    match key {
        ReadSourceKey::BasePointer { .. } => 0,
        ReadSourceKey::BaseBlob { object_oid, .. } => object_oid.len(),
        ReadSourceKey::BaseEmpty { path, .. } => path.len(),
        ReadSourceKey::OverlayFile { path, .. } => path.len(),
    }
}

fn read_source_heap_bytes(source: &ReadSource) -> usize {
    match source {
        ReadSource::BasePointer { path, .. } => path.len() + std::mem::size_of::<FilePointer>(),
        ReadSource::BaseBlob { path, oid, .. } => path.len() + oid.len(),
        ReadSource::BaseEmpty => 0,
        ReadSource::OverlayFile { path, backing } => {
            path.len() + backing.as_os_str().to_string_lossy().len()
        }
    }
}

/// Read/write dispatch engine for the virtual filesystem.
///
/// Reads check the overlay first (local writes take precedence), then
/// fall back to chunk-level hydration from object storage. Writes go
/// through the copy-on-write overlay: the file is promoted from the
/// base snapshot on first write.
pub struct VfsEngine {
    resolver: Arc<FuseResolver>,
    overlay: Option<Arc<dyn OverlayWriter>>,
    hydration: Arc<HydrationService>,
    /// ODB reader for small-file blobs stored in git packs.
    /// `None` in tests without a real git directory.
    odb_reader: Option<Arc<OdbReader>>,
    /// Snapshot store for size-backfill after hydration.
    /// `None` in tests without a real snapshot store.
    snapshot: Option<Arc<SnapshotStore>>,
    /// Serializes overlay mutations per path so first-write promotion,
    /// truncate, and rename cannot race on the same backing file.
    overlay_locks: DashMap<String, Arc<AsyncMutex<()>>>,
    /// Drains all overlay mutations while a live overlay reset clears state.
    overlay_reset_gate: Arc<AsyncRwLock<()>>,
    native_writeback_gate: Arc<AsyncRwLock<()>>,
    overlay_reset_epoch: AtomicU64,
    /// Monotonic clock for overlay invalidation events.
    overlay_version: AtomicU64,
    /// Lowest valid read-source version after whole-overlay changes.
    overlay_reset_version: AtomicU64,
    /// Exact-path overlay invalidation versions.
    overlay_path_versions: DashMap<String, u64>,
    /// Subtree overlay invalidation versions.
    overlay_subtree_versions: DashMap<String, u64>,
    read_metrics: VfsReadMetrics,
    read_source_cache: VfsReadSourceCache,
    control_accepting: Mutex<bool>,
    control_tasks: tokio_util::task::TaskTracker,
    control_permits: Arc<tokio::sync::Semaphore>,
    #[cfg(all(test, target_os = "linux", feature = "fuse", feature = "nfs"))]
    writeback_pause: Mutex<std::sync::Weak<crate::test_support::writeback::State>>,
    #[cfg(feature = "fuse")]
    open_files: open_file::OpenFileRegistry,
}

struct OverlayLockSet {
    paths: Vec<String>,
    locks: Vec<Arc<AsyncMutex<()>>>,
}

struct OverlayMutationGuard {
    _guard: OwnedRwLockReadGuard<()>,
}

pub struct OverlayResetGuard<'a> {
    engine: &'a VfsEngine,
    epoch: &'a AtomicU64,
    _guard: (OwnedRwLockWriteGuard<()>, OwnedRwLockWriteGuard<()>),
}

impl Drop for OverlayResetGuard<'_> {
    fn drop(&mut self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.engine
            .apply_invalidation(VfsInvalidation::OverlayReset);
    }
}

impl VfsEngine {
    /// Create a new engine.
    ///
    /// `overlay` may be `None` for a read-only mount. `odb_reader` may be
    /// `None` in tests without a real git directory. `snapshot` may be
    /// `None` in tests; when present, enables size-backfill after ODB reads.
    pub fn new(
        resolver: Arc<FuseResolver>,
        overlay: Option<Arc<dyn OverlayWriter>>,
        hydration: Arc<HydrationService>,
        odb_reader: Option<OdbReader>,
        snapshot: Option<Arc<SnapshotStore>>,
    ) -> Self {
        Self {
            resolver,
            overlay,
            hydration,
            odb_reader: odb_reader.map(Arc::new),
            snapshot,
            overlay_locks: DashMap::new(),
            overlay_reset_gate: Arc::new(AsyncRwLock::new(())),
            native_writeback_gate: Arc::new(AsyncRwLock::new(())),
            overlay_reset_epoch: AtomicU64::new(0),
            overlay_version: AtomicU64::new(0),
            overlay_reset_version: AtomicU64::new(0),
            overlay_path_versions: DashMap::new(),
            overlay_subtree_versions: DashMap::new(),
            read_metrics: VfsReadMetrics::default(),
            control_accepting: Mutex::new(true),
            control_tasks: tokio_util::task::TaskTracker::new(),
            control_permits: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(all(test, target_os = "linux", feature = "fuse", feature = "nfs"))]
            writeback_pause: Mutex::new(std::sync::Weak::new()),
            #[cfg(feature = "fuse")]
            open_files: open_file::OpenFileRegistry::default(),
            read_source_cache: VfsReadSourceCache::new(
                DEFAULT_READ_SOURCE_CACHE_MAX_ENTRIES,
                DEFAULT_READ_SOURCE_CACHE_MAX_ESTIMATED_BYTES,
            ),
        }
    }

    /// Return true when an error means the caller should reopen a read lease.
    pub fn is_stale_read_lease_error(error: &CrabError) -> bool {
        matches!(
            error,
            CrabError::Internal(message)
                if message == STALE_READ_LEASE_GENERATION
                    || message == STALE_READ_LEASE_OVERLAY_VIEW
                    || message == STALE_READ_LEASE_OVERLAY_FILE
        )
    }

    /// Current overlay view version used to stale read leases and write journal entries.
    #[cfg(feature = "nfs")]
    pub fn overlay_view_version(&self) -> u64 {
        self.overlay_version.load(Ordering::Acquire)
    }

    /// Access the overlay writer, if one is configured.
    /// `None` for read-only mounts.
    pub fn overlay(&self) -> &Option<Arc<dyn OverlayWriter>> {
        &self.overlay
    }

    /// Snapshot shared VFS read counters for mount diagnostics.
    pub fn read_metrics_snapshot(&self) -> VfsReadMetricsSnapshot {
        let mut snapshot = self.read_metrics.snapshot();
        let source_cache = self.read_source_cache.snapshot();
        snapshot.source_cache_entries = source_cache.entries;
        snapshot.source_cache_max_entries = source_cache.max_entries;
        snapshot.source_cache_estimated_bytes = source_cache.estimated_bytes;
        snapshot.source_cache_max_estimated_bytes = source_cache.max_estimated_bytes;
        snapshot.source_cache_hits = source_cache.hits;
        snapshot.resolver_calls_avoided = source_cache.resolver_calls_avoided;
        snapshot.source_cache_misses = source_cache.misses;
        snapshot.source_cache_evictions = source_cache.evictions;
        snapshot.source_cache_invalidations = source_cache.invalidations;
        snapshot.source_cache_stale_evictions = source_cache.stale_evictions;
        snapshot
    }

    /// Clear cached read sources after refresh or mutation changes path meaning.
    pub fn invalidate_read_source_cache(&self) {
        self.apply_invalidation(VfsInvalidation::SnapshotGenerationChanged {
            old_generation: None,
            new_generation: self.resolver.generation(),
        });
    }

    pub(crate) async fn run_control<T: Send + 'static>(
        self: &Arc<Self>,
        operation: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let permit = Arc::clone(&self.control_permits)
            .acquire_owned()
            .await
            .map_err(|_| CrabError::Cancelled)?;
        #[cfg(all(test, target_os = "linux", feature = "fuse", feature = "nfs"))]
        self.observe_control_start();
        let task = {
            let accepting = self
                .control_accepting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !*accepting {
                return Err(CrabError::Cancelled);
            }
            // The tracked task owns its guards and blocking jobs. Dropping the
            // caller's waiter cannot release a transition while mutation continues.
            self.control_tasks.spawn(async move {
                let _permit = permit;
                operation.await
            })
        };
        task.await.map_err(CrabError::ReadTask)?
    }

    /// Drain admitted Git reads/copies and hydration after backend requests stop.
    ///
    /// Foreground requests remain owned by the backend. Abandoned blocking work,
    /// queued workers and admitted prefetch retain ownership through completion.
    pub async fn shutdown_background(&self) {
        self.drain_controls().await;
        self.request_shutdown();
        if let Some(reader) = &self.odb_reader {
            reader.shutdown().await;
        }
        self.hydration.shutdown().await;
    }

    /// Close read admission and request worker shutdown without waiting for completion.
    pub fn request_shutdown(&self) {
        self.close_control_admission();
        if let Some(reader) = &self.odb_reader {
            reader.request_shutdown();
        }
        self.hydration.request_shutdown();
    }

    pub(crate) async fn drain_controls(&self) {
        self.close_control_admission();
        self.control_tasks.wait().await;
    }

    pub(crate) fn close_control_admission(&self) {
        let mut accepting = self
            .control_accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *accepting = false;
        self.control_permits.close();
        self.control_tasks.close();
    }

    /// Snapshot hydration read counters for mount diagnostics.
    pub fn hydration_read_stats_snapshot(&self) -> HydrationReadStatsSnapshot {
        self.hydration.read_stats_snapshot()
    }

    /// Resolve exact attributes from one retained snapshot view.
    pub fn getattr(&self, path: &str) -> Result<(u32, u64, NodeType, i64)> {
        let view = self.resolver.capture_view();
        let node = match self.resolver.resolve_path_in(&view, path)? {
            ResolvedNode::Base(base) => {
                let base = self.classify_unknown_base_node(view.generation, path, base)?;
                if base.pointer.is_none()
                    && base.size == 0
                    && base.node_type == NodeType::File
                    && base.object_oid.is_some()
                    && self.odb_reader.is_none()
                {
                    return Err(CrabError::Internal(format!(
                        "no ODB reader configured for size lookup of {path}"
                    )));
                }
                ResolvedNode::Base(base)
            }
            node @ ResolvedNode::Overlay(_) => node,
        };
        Ok(view.attributes(&node))
    }

    fn classify_unknown_base_node(
        &self,
        generation: i64,
        path: &str,
        mut base: BaseNode,
    ) -> Result<BaseNode> {
        if base.node_type != NodeType::File
            || base.size != 0
            || base.pointer.is_some()
            || base.object_oid.is_none()
        {
            return Ok(base);
        }
        let oid = base.object_oid.as_deref().ok_or_else(|| {
            CrabError::Internal(format!(
                "missing OID while classifying fetched blob at {path}"
            ))
        })?;
        let Some(reader) = self.odb_reader.as_ref() else {
            return Ok(base);
        };
        let blob = reader.read_blob(oid)?;
        let (size, pointer) = if let Some(snapshot) = &self.snapshot {
            snapshot.update_node_from_blob(generation, path, &blob)?
        } else {
            crate::snapshot::classify_blob(&blob)?
        };
        base.size = size;
        base.pointer = pointer;
        Ok(base)
    }

    /// Flush overlay data and metadata for protocols with explicit commit semantics.
    pub fn sync_overlay_path(&self, path: &str) -> Result<()> {
        if let Some(overlay) = &self.overlay {
            overlay.sync_path(path)?;
            overlay.checkpoint()?;
        }
        Ok(())
    }

    /// Flush overlay metadata without requiring a regular backing file.
    pub fn checkpoint_overlay(&self) -> Result<()> {
        if let Some(overlay) = &self.overlay {
            overlay.checkpoint()?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Read
    // -----------------------------------------------------------------------

    /// Open a read lease for a regular file.
    pub fn open_read(&self, path: &str) -> Result<VfsReadLease> {
        self.read_metrics.record_open_read();
        let path = clean_lock_path(path);
        if let Some(lease) = self.cached_read_source(&path)? {
            return Ok(lease);
        }
        let lease = self.open_read_uncached(path.clone())?;
        self.read_source_cache.insert(path, lease.clone());
        Ok(lease)
    }

    fn cached_read_source(&self, path: &str) -> Result<Option<VfsReadLease>> {
        let Some(lease) = self.read_source_cache.candidate(path) else {
            self.read_source_cache.record_miss();
            return Ok(None);
        };
        match self.validate_read_lease(&lease) {
            Ok(()) => {
                self.read_source_cache.record_hit();
                Ok(Some(lease))
            }
            Err(error) if Self::is_stale_read_lease_error(&error) => {
                self.read_source_cache.evict_stale(path);
                self.read_source_cache.record_miss();
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn open_read_uncached(&self, path: String) -> Result<VfsReadLease> {
        let view = self.resolver.capture_view();
        if let Some(ref ov) = self.overlay
            && let Some(backing) = ov.get_backing_path(&path)
        {
            let node = self.resolver.resolve_path_in(&view, &path)?;
            let ResolvedNode::Overlay(entry) = node else {
                return Err(CrabError::Internal(format!(
                    "overlay backing exists for non-overlay path {path}"
                )));
            };
            ensure_readable_file(entry.node_type, &path)?;
            let key = ReadSourceKey::OverlayFile {
                path: path.clone(),
                overlay_version: self.overlay_view_version_for_path(&path),
                size: entry.size,
                mtime_ns: entry.mtime_ns,
            };
            let source = ReadSource::OverlayFile { path, backing };
            return Ok(VfsReadLease::new(key, source, None));
        }

        let node = self.resolver.resolve_path_in(&view, &path)?;
        ensure_readable_file(node.node_type(), &path)?;
        let overlay_version = self.overlay_view_version_for_path(&path);
        match node {
            ResolvedNode::Base(base) => {
                let base = self.classify_unknown_base_node(view.generation, &path, base)?;
                Ok(self.open_base_read(path, view, overlay_version, base))
            }
            ResolvedNode::Overlay(_) => {
                warn!(
                    path,
                    "overlay entry resolved but no backing path found for read lease"
                );
                Ok(VfsReadLease::new(
                    ReadSourceKey::OverlayFile {
                        overlay_version: self.overlay_view_version_for_path(&path),
                        path,
                        size: 0,
                        mtime_ns: 0,
                    },
                    ReadSource::BaseEmpty,
                    None,
                ))
            }
        }
    }

    fn open_base_read(
        &self,
        path: String,
        view: Arc<SnapshotView>,
        overlay_version: u64,
        base: BaseNode,
    ) -> VfsReadLease {
        let generation = view.generation;
        if let Some(pointer) = base.pointer {
            let key = ReadSourceKey::BasePointer {
                generation,
                overlay_version,
                file_hash: pointer.identity(),
                size: pointer.size(),
            };
            let source = ReadSource::BasePointer { path, pointer };
            return VfsReadLease::new(key, source, Some(view));
        }

        if let Some(oid) = base.object_oid {
            let known_size = (base.size != 0).then_some(base.size);
            let key = ReadSourceKey::BaseBlob {
                generation,
                overlay_version,
                object_oid: oid.clone(),
                known_size,
            };
            let source = ReadSource::BaseBlob { path, oid };
            return VfsReadLease::new(key, source, Some(view));
        }

        VfsReadLease::new(
            ReadSourceKey::BaseEmpty {
                generation,
                overlay_version,
                path,
            },
            ReadSource::BaseEmpty,
            Some(view),
        )
    }

    /// Read bytes from a file at the given offset.
    ///
    /// Overlay files are read from their local backing file. Base files
    /// are hydrated on demand via the hydration service (chunk-level).
    pub async fn read(&self, path: &str, offset: u64, size: u32) -> Result<Bytes> {
        loop {
            let lease = self.open_read(path)?;
            match self.read_at(&lease, offset, size).await {
                Err(error) if Self::is_stale_read_lease_error(&error) => {
                    // A path read owns admission, so reacquire if refresh won
                    // before I/O. Explicit lease callers keep the stale error.
                    tokio::task::yield_now().await;
                }
                result => return result,
            }
        }
    }

    /// Read bytes from a previously opened VFS read lease.
    pub async fn read_at(&self, lease: &VfsReadLease, offset: u64, size: u32) -> Result<Bytes> {
        self.read_metrics.record_read_at_call();
        self.validate_read_lease(lease)?;
        self.read_captured_at(lease, offset, size).await
    }

    async fn read_captured_at(
        &self,
        lease: &VfsReadLease,
        offset: u64,
        size: u32,
    ) -> Result<Bytes> {
        let source_kind = lease.source.kind();
        let read_decision = lease.record_read_pattern(offset, size);
        let read_class = read_decision.class;
        let mut prefetch = None;
        let result = match lease.source.as_ref() {
            ReadSource::BasePointer { path, pointer } => {
                trace!(path, offset, size, "hydrating base file (pointer)");
                let hydration = lease
                    ._snapshot_view
                    .as_ref()
                    .and_then(|view| view.hydration.as_ref())
                    .ok_or_else(|| {
                        CrabError::Internal("pointer read view has no hydration service".into())
                    })?;
                let result = hydration.read_range(pointer, offset, size).await;
                if result.is_ok() {
                    prefetch = read_decision
                        .prefetch
                        .map(|prefetch| (Arc::clone(hydration), pointer.clone(), prefetch));
                }
                result
            }
            ReadSource::BaseBlob { path, oid } => {
                let Some(ref reader) = self.odb_reader else {
                    return Err(CrabError::Internal(format!(
                        "no ODB reader configured for small-file read of {path}"
                    )));
                };
                trace!(
                    path,
                    oid = oid.as_str(),
                    offset,
                    size,
                    "reading small file from ODB"
                );
                let data = reader.read_blob_range(oid, offset, size).await?;
                Ok(data)
            }
            ReadSource::BaseEmpty => Ok(Bytes::new()),
            ReadSource::OverlayFile { path, backing } => {
                trace!(path, offset, size, "reading from overlay backing file");
                read_file_range(backing, offset, size)
            }
        };
        if let Ok(bytes) = &result {
            self.read_metrics
                .record_read_success(source_kind, read_class, bytes.len());
        }
        if let Some((hydration, pointer, prefetch)) = prefetch {
            match prefetch {
                AdaptivePrefetch::NextWindow => {
                    hydration.prefetch_next_read_window(pointer, offset, size);
                }
                AdaptivePrefetch::TargetWindow { offset, size } => {
                    hydration.prefetch_read_window(pointer, offset, size);
                }
            }
        }
        result
    }

    fn validate_read_lease(&self, lease: &VfsReadLease) -> Result<()> {
        match lease.key() {
            ReadSourceKey::BasePointer {
                generation,
                overlay_version,
                ..
            }
            | ReadSourceKey::BaseBlob {
                generation,
                overlay_version,
                ..
            }
            | ReadSourceKey::BaseEmpty {
                generation,
                overlay_version,
                ..
            } => {
                if self.resolver.generation() != *generation {
                    self.read_metrics.record_stale_generation();
                    return Err(CrabError::Internal(STALE_READ_LEASE_GENERATION.into()));
                }
                if self.overlay_view_version_for_path(lease.path()) != *overlay_version {
                    self.read_metrics.record_stale_overlay_view();
                    return Err(CrabError::Internal(STALE_READ_LEASE_OVERLAY_VIEW.into()));
                }
            }
            ReadSourceKey::OverlayFile {
                overlay_version, ..
            } => {
                if self.overlay_view_version_for_path(lease.path()) != *overlay_version {
                    self.read_metrics.record_stale_overlay_file();
                    return Err(CrabError::Internal(STALE_READ_LEASE_OVERLAY_FILE.into()));
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Write
    // -----------------------------------------------------------------------

    /// Write data to a file at the given offset.
    ///
    /// Ensures the file is in the overlay (copy-on-write promotion) before
    /// writing. Returns the number of bytes written.
    pub async fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
        let ov = self.require_overlay()?;
        #[cfg(all(test, target_os = "linux", feature = "fuse", feature = "nfs"))]
        self.pause_writeback().await?;
        let mutation = self.begin_overlay_mutation().await?;
        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = async {
            self.ensure_overlay_backing_locked(path, ov).await?;
            ov.write_file(path, offset, data)
        }
        .await;
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_path(path);
        }
        result
    }

    // -----------------------------------------------------------------------
    // Mutation operations
    // -----------------------------------------------------------------------

    /// Create a new file in the overlay.
    pub async fn create(&self, path: &str, mode: u32) -> Result<OverlayEntry> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;
        debug!(path, mode, "create");
        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = ov.create_file(path, mode);
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_path(path);
        }
        result
    }

    /// Create a symlink in the overlay.
    pub async fn create_symlink(
        &self,
        path: &str,
        target: &str,
        mode: u32,
    ) -> Result<OverlayEntry> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;
        debug!(path, target, mode, "create_symlink");
        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = ov.create_symlink(path, target, mode);
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_path(path);
        }
        result
    }

    /// Delete a file (mark as deleted in the overlay).
    pub async fn unlink(&self, path: &str) -> Result<()> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;
        debug!(path, "unlink");
        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = self.unlink_locked(path, ov);
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_path(path);
        }
        result
    }

    /// Create a directory in the overlay.
    pub async fn mkdir(&self, path: &str, mode: u32) -> Result<()> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;
        debug!(path, mode, "mkdir");
        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = ov.mkdir(path, mode);
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_path(path);
        }
        result
    }

    /// Remove a directory from the overlay (must be empty).
    pub async fn rmdir(&self, path: &str) -> Result<()> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;
        debug!(path, "rmdir");
        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = self.rmdir_locked(path, ov);
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_subtree(path);
        }
        result
    }

    /// Rename a file or directory in the overlay.
    pub async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        self.rename_entry(old_path, new_path, true).await
    }

    #[cfg(feature = "fuse")]
    pub(crate) async fn rename_no_replace(&self, old_path: &str, new_path: &str) -> Result<()> {
        self.rename_entry(old_path, new_path, false).await
    }

    async fn rename_entry(&self, old_path: &str, new_path: &str, replace: bool) -> Result<()> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;
        debug!(old_path, new_path, "rename");
        let old_path = clean_lock_path(old_path);
        let new_path = clean_lock_path(new_path);

        if is_descendant_path(&new_path, &old_path) {
            return Err(CrabError::Forbidden {
                path: format!("cannot rename directory into itself: {old_path} -> {new_path}"),
            });
        }

        let lock_set = self.overlay_locks_for(rename_lock_paths(&old_path, &new_path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = async {
            let view = self.resolver.capture_view();
            let node = self.resolver.resolve_path_in(&view, &old_path)?;
            // Destination admission shares mutation/path locks with publication
            // and all engine writers; checking in FUSE alone would race refresh.
            if !replace {
                match self.resolver.resolve_path_in(&view, &new_path) {
                    Ok(_) => {
                        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into());
                    }
                    Err(CrabError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
            if old_path == new_path {
                return Ok(());
            }
            match node {
                ResolvedNode::Base(base) => {
                    let entries =
                        self.base_rename_entries(view.generation, &base, &old_path, &new_path)?;
                    ov.rename_base_subtree(&entries)
                }
                ResolvedNode::Overlay(_) => ov.rename(&old_path, &new_path),
            }
        }
        .await;
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_rename(&old_path, &new_path);
        }
        result
    }

    /// Update the modification time of a file.
    pub async fn set_mtime(&self, path: &str, mtime_ns: i64) -> Result<()> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;
        trace!(path, mtime_ns, "set_mtime");
        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = async {
            self.ensure_overlay_locked(path, ov).await?;
            ov.set_mtime(path, mtime_ns)
        }
        .await;
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_path(path);
        }
        result
    }

    /// Update permission bits for a file or directory.
    pub async fn set_mode(&self, path: &str, mode: u32) -> Result<()> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;
        trace!(path, mode, "set_mode");
        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = async {
            self.ensure_overlay_locked(path, ov).await?;
            ov.set_mode(path, mode)
        }
        .await;
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_path(path);
        }
        result
    }

    /// Truncate a file to the given size.
    ///
    /// Ensures the file is in the overlay (copy-on-write promotion if
    /// needed), truncates the backing file via `File::set_len`, and
    /// updates the overlay metadata. `set_len` implements POSIX
    /// `ftruncate` semantics: extending fills with zero bytes,
    /// truncating removes bytes beyond `new_size`.
    pub async fn truncate(&self, path: &str, new_size: u64) -> Result<()> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;
        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = async {
            if new_size == 0
                && ov.get_backing_path(path).is_none()
                && let ResolvedNode::Base(base) = self.resolver.resolve_path(path)?
            {
                Self::promote_empty_base_file_to_overlay(&base, path, ov)?;
                return Ok(());
            }

            self.ensure_overlay_backing_locked(path, ov).await?;
            let backing = ov
                .get_backing_path(path)
                .ok_or_else(|| CrabError::NotFound { path: path.into() })?;
            let file = std::fs::File::options().write(true).open(&backing)?;
            let now_ns = now_nanos();
            ov.mutate_content(Some(path), &file, now_ns, &mut |file| {
                file.set_len(new_size)?;
                Ok(())
            })?;
            Ok(())
        }
        .await;
        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_path(path);
        }
        result
    }

    // -----------------------------------------------------------------------
    // Symlink support
    // -----------------------------------------------------------------------

    pub(crate) fn symlink_target(&self, path: &str) -> Result<String> {
        let view = self.resolver.capture_view();
        match self.resolver.resolve_path_in(&view, path)? {
            ResolvedNode::Base(base) => {
                let oid = base.object_oid.ok_or_else(|| {
                    CrabError::Internal(format!("symlink has no object identity: {path}"))
                })?;
                self.read_symlink_target(&oid)
            }
            ResolvedNode::Overlay(_) => {
                let backing = self
                    .overlay
                    .as_ref()
                    .and_then(|overlay| overlay.get_backing_path(path))
                    .ok_or_else(|| {
                        CrabError::Internal(format!("symlink has no backing: {path}"))
                    })?;
                Ok(std::fs::read_to_string(backing)?)
            }
        }
    }

    /// Read the target of a symlink stored as a git blob.
    ///
    /// The blob content IS the symlink target path (git stores symlink
    /// targets as-is, no newline stripping needed).
    pub fn read_symlink_target(&self, oid: &str) -> Result<String> {
        if let Some(ref reader) = self.odb_reader {
            let blob = reader.read_blob(oid)?;
            Ok(String::from_utf8_lossy(&blob).into_owned())
        } else {
            Err(CrabError::Internal(
                "no ODB reader for symlink target".into(),
            ))
        }
    }

    // -----------------------------------------------------------------------
    // Speculative prefetch
    // -----------------------------------------------------------------------

    /// Enqueue file children for background hydration at lower priority.
    ///
    /// Called from `opendir` in the FUSE layer. Resolves each path to a
    /// pointer (skipping non-pointer files) and delegates to the hydration
    /// service's `prefetch_dir`.
    pub fn prefetch_dir(&self, paths: &[String]) -> Result<()> {
        let view = self.resolver.capture_view();
        let mut to_prefetch = Vec::new();
        for path in paths {
            if let Ok(ResolvedNode::Base(base)) = self.resolver.resolve_path_in(&view, path)
                && let Some(FilePointer::Crab(pointer)) = base.pointer
            {
                to_prefetch.push((path.clone(), pointer));
            }
        }
        if !to_prefetch.is_empty() {
            let hydration = view.hydration.as_ref().ok_or_else(|| {
                CrabError::Internal("prefetch view has no hydration service".into())
            })?;
            hydration.prefetch_dir(to_prefetch);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // ensure_overlay — copy-on-write promotion
    // -----------------------------------------------------------------------

    /// Ensure a file is in the overlay, promoting from the base snapshot
    /// if necessary (copy-on-write).
    ///
    /// If the path is already in the overlay, this is a no-op. Otherwise:
    /// - For pointer-backed files: hydrate the full content via the
    ///   hydration service, then write it to the overlay backing file.
    /// - For small git-pack files: read the full blob from the ODB via
    ///   `odb_reader`, then promote to the overlay.
    /// - For directories: no-op (directories don't need content promotion).
    ///
    /// Mirrors artifact-fs's `EnsureCopyOnWrite` pattern.
    pub async fn ensure_overlay(&self, path: &str) -> Result<()> {
        let ov = self.require_overlay()?;
        let mutation = self.begin_overlay_mutation().await?;

        // Already in the overlay? Nothing to do.
        if ov.get(path).is_some() {
            drop(mutation);
            return Ok(());
        }

        let lock_set = self.overlay_locks_for(mutation_lock_paths(path));
        let guards = self.acquire_overlay_locks(&lock_set).await;
        let result = async { self.ensure_overlay_locked(path, ov).await }.await;

        drop(guards);
        self.remove_idle_overlay_locks(&lock_set);
        drop(mutation);
        if result.is_ok() {
            self.invalidate_overlay_path(path);
        }
        result
    }

    async fn ensure_overlay_locked(&self, path: &str, ov: &dyn OverlayWriter) -> Result<()> {
        // Another writer may have completed promotion while we waited.
        if ov.get(path).is_some() {
            return Ok(());
        }

        let view = self.resolver.capture_view();
        let node = self.resolver.resolve_path_in(&view, path)?;
        match node {
            ResolvedNode::Base(base) => self.promote_base_to_overlay(&view, &base, path, ov).await,
            ResolvedNode::Overlay(_) => Ok(()),
        }
    }

    async fn ensure_overlay_backing_locked(
        &self,
        path: &str,
        ov: &dyn OverlayWriter,
    ) -> Result<()> {
        if ov.get_backing_path(path).is_some() {
            return Ok(());
        }

        let view = self.resolver.capture_view();
        let node = self.resolver.resolve_path_in(&view, path)?;
        match node {
            ResolvedNode::Base(base) => self.promote_base_to_overlay(&view, &base, path, ov).await,
            ResolvedNode::Overlay(entry) if entry.node_type == NodeType::Dir => Ok(()),
            ResolvedNode::Overlay(_) => Err(CrabError::NotFound { path: path.into() }),
        }
    }

    fn unlink_locked(&self, path: &str, ov: &dyn OverlayWriter) -> Result<()> {
        let node = self.resolver.resolve_path(path)?;
        if node.node_type() == NodeType::Dir {
            return Err(CrabError::Forbidden {
                path: format!("cannot unlink directory: {path}"),
            });
        }
        ov.remove(path)
    }

    fn rmdir_locked(&self, path: &str, ov: &dyn OverlayWriter) -> Result<()> {
        let node = self.resolver.resolve_path(path)?;
        if node.node_type() != NodeType::Dir {
            return Err(CrabError::NotFound { path: path.into() });
        }
        let children = self.resolver.readdir(path)?;
        if !children.is_empty() {
            return Err(CrabError::Forbidden {
                path: format!("directory not empty: {path}"),
            });
        }
        ov.rmdir(path)
    }

    fn overlay_locks_for<I, P>(&self, paths: I) -> OverlayLockSet
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        let mut deduped = BTreeSet::new();
        for path in paths {
            deduped.insert(clean_lock_path(path.as_ref()));
        }

        let paths = deduped.into_iter().collect::<Vec<_>>();
        let locks = paths.iter().map(|path| self.overlay_lock(path)).collect();
        OverlayLockSet { paths, locks }
    }

    async fn acquire_overlay_locks(&self, lock_set: &OverlayLockSet) -> Vec<OwnedMutexGuard<()>> {
        let mut guards = Vec::with_capacity(lock_set.locks.len());
        for lock in &lock_set.locks {
            guards.push(lock.clone().lock_owned().await);
        }
        guards
    }

    fn overlay_lock(&self, path: &str) -> Arc<AsyncMutex<()>> {
        self.overlay_locks
            .entry(path.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn remove_idle_overlay_locks(&self, lock_set: &OverlayLockSet) {
        for (path, lock) in lock_set.paths.iter().zip(&lock_set.locks) {
            self.remove_idle_overlay_lock(path, lock);
        }
    }

    fn remove_idle_overlay_lock(&self, path: &str, lock: &Arc<AsyncMutex<()>>) {
        self.overlay_locks.remove_if(path, |_, current| {
            Arc::ptr_eq(current, lock) && Arc::strong_count(current) == 2
        });
    }

    async fn begin_overlay_mutation(&self) -> Result<OverlayMutationGuard> {
        let epoch = self.overlay_reset_epoch.load(Ordering::Acquire);
        let guard = self.overlay_reset_gate.clone().read_owned().await;
        let current = self.overlay_reset_epoch.load(Ordering::Acquire);
        if epoch != current || !current.is_multiple_of(2) {
            return Err(CrabError::Forbidden {
                path: "overlay reset raced with write operation".into(),
            });
        }
        Ok(OverlayMutationGuard { _guard: guard })
    }

    #[cfg(all(test, target_os = "linux", feature = "fuse", feature = "nfs"))]
    pub(crate) fn pause_next_writeback(&self) -> crate::test_support::writeback::Pause {
        let pause = crate::test_support::writeback::Pause::new();
        *self
            .writeback_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(&pause.state);
        pause
    }

    #[cfg(all(test, target_os = "linux", feature = "fuse", feature = "nfs"))]
    async fn pause_writeback(&self) -> Result<()> {
        let pause = self
            .writeback_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade();
        if let Some(pause) = pause {
            pause.wait().await?;
        }
        Ok(())
    }

    #[cfg(all(test, target_os = "linux", feature = "fuse", feature = "nfs"))]
    fn observe_control_start(&self) {
        if let Some(pause) = self
            .writeback_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade()
        {
            pause.control_started.notify_one();
        }
    }

    #[cfg(feature = "fuse")]
    pub(crate) async fn namespace_read(&self) -> OwnedRwLockReadGuard<()> {
        self.overlay_reset_gate.clone().read_owned().await
    }

    pub(crate) async fn begin_native_writeback(&self) -> OwnedRwLockReadGuard<()> {
        self.native_writeback_gate.clone().read_owned().await
    }

    pub(crate) async fn quiesce_overlay(
        &self,
    ) -> (OwnedRwLockWriteGuard<()>, OwnedRwLockWriteGuard<()>) {
        // Entered native WRITEs can still be waiting for inode/namespace locks.
        // Drain them before blocking engine mutations, or they cannot finish.
        let writeback = self.native_writeback_gate.clone().write_owned().await;
        let overlay = self.overlay_reset_gate.clone().write_owned().await;
        (overlay, writeback)
    }

    pub async fn begin_overlay_reset(&self) -> OverlayResetGuard<'_> {
        let guard = self.quiesce_overlay().await;
        let previous = self.overlay_reset_epoch.fetch_add(1, Ordering::AcqRel);
        if !previous.is_multiple_of(2) {
            warn!(epoch = previous, "overlay reset epoch was already active");
        }
        OverlayResetGuard {
            engine: self,
            epoch: &self.overlay_reset_epoch,
            _guard: guard,
        }
    }

    /// Promote a base snapshot node into the overlay.
    async fn promote_base_to_overlay(
        &self,
        view: &SnapshotView,
        base: &BaseNode,
        path: &str,
        ov: &dyn OverlayWriter,
    ) -> Result<()> {
        let base = self.classify_unknown_base_node(view.generation, path, base.clone())?;
        match base.node_type {
            NodeType::Dir => {
                // Directories don't need content promotion.
                trace!(path, "directory — no content promotion needed");
                Ok(())
            }
            NodeType::File => {
                if let Some(ref pointer) = base.pointer {
                    // Large file: stream chunks directly to disk.
                    let source_oid = base_source_oid(&base);
                    debug!(
                        path,
                        size = pointer.size(),
                        "promoting pointer-backed file via streaming"
                    );
                    let hydration = view.hydration.as_ref().ok_or_else(|| {
                        CrabError::Internal(
                            "pointer promotion view has no hydration service".into(),
                        )
                    })?;
                    self.promote_pointer_streaming(
                        hydration,
                        path,
                        pointer,
                        base.mode,
                        source_oid.as_deref(),
                        ov,
                    )
                    .await
                } else if let Some(ref oid) = base.object_oid {
                    // Ordinary Git content uses the same verified bytes as range reads.
                    if let Some(ref reader) = self.odb_reader {
                        debug!(path, oid = oid.as_str(), "promoting verified Git blob");
                        self.promote_from_blob(path, oid, base.mode, reader, ov)
                            .await
                    } else {
                        Err(CrabError::Internal(format!(
                            "no ODB reader configured for small-file promotion of {path}"
                        )))
                    }
                } else {
                    // Empty file (no pointer, no OID).
                    ov.promote(path, base.mode, &[], None)?;
                    Ok(())
                }
            }
            NodeType::Symlink => {
                // Symlinks are metadata-only; promote with empty content.
                ov.promote(path, base.mode, &[], None)?;
                Ok(())
            }
        }
    }

    fn promote_empty_base_file_to_overlay(
        base: &BaseNode,
        path: &str,
        ov: &dyn OverlayWriter,
    ) -> Result<()> {
        if base.node_type != NodeType::File {
            return Err(CrabError::NotFound { path: path.into() });
        }

        // Truncation creates locally owned content, not a disposable copy of
        // the old base. Record ownership atomically with the empty promotion.
        ov.promote(path, base.mode, &[], None)?;
        Ok(())
    }

    fn base_rename_entries(
        &self,
        generation: i64,
        base: &BaseNode,
        old_path: &str,
        new_path: &str,
    ) -> Result<Vec<BaseRenameEntry>> {
        let mut entries = vec![base_rename_entry(base, old_path, new_path)];
        if base.node_type != NodeType::Dir {
            return Ok(entries);
        }

        let snapshot = self.snapshot.as_ref().ok_or_else(|| {
            CrabError::Internal("base directory rename requires a snapshot store".into())
        })?;
        let mut stack = vec![(old_path.to_owned(), new_path.to_owned())];
        while let Some((old_parent, new_parent)) = stack.pop() {
            for child in snapshot.list_children(generation, &old_parent)? {
                let name = child
                    .path
                    .rsplit_once('/')
                    .map_or(child.path.as_str(), |(_, name)| name);
                let moved_path = if new_parent.is_empty() {
                    name.to_owned()
                } else {
                    format!("{new_parent}/{name}")
                };
                entries.push(base_rename_entry(&child, &child.path, &moved_path));
                if child.node_type == NodeType::Dir {
                    stack.push((child.path.clone(), moved_path));
                }
            }
        }
        Ok(entries)
    }

    /// Stream chunks from the hydration service directly to a temp file
    /// on disk, then atomic-rename to the final backing path.
    ///
    /// This avoids buffering the entire file in memory — critical for
    /// large pointer-tracked files (10 GB+).
    async fn promote_pointer_streaming(
        &self,
        hydration: &Arc<HydrationService>,
        path: &str,
        pointer: &FilePointer,
        mode: u32,
        source_oid: Option<&str>,
        ov: &dyn OverlayWriter,
    ) -> Result<()> {
        let backing_final = ov.backing_path_for(path);
        let mut temporary = promotion_file(&backing_final)?;
        if let Some(size) = hydration
            .reconstruct_to_path(pointer, temporary.path())
            .await?
        {
            temporary
                .persist(&backing_final)
                .map_err(|error| CrabError::Io(error.error))?;
            ov.promote_from_file(path, mode, size, source_oid)?;
            return Ok(());
        }
        let FilePointer::Crab(pointer) = pointer else {
            return Err(CrabError::Internal(
                "LFS pointer has no configured reader".into(),
            ));
        };
        let terms = hydration.resolve_terms(pointer)?;
        for term in &terms {
            let chunk = hydration.fetch_chunk(term).await?;
            std::io::Write::write_all(temporary.as_file_mut(), &chunk)?;
        }
        temporary.as_file().sync_all()?;
        temporary
            .persist(&backing_final)
            .map_err(|error| CrabError::Io(error.error))?;
        ov.promote_from_file(path, mode, pointer.size, source_oid)?;
        Ok(())
    }

    async fn promote_from_blob(
        &self,
        path: &str,
        oid: &str,
        mode: u32,
        reader: &Arc<OdbReader>,
        ov: &dyn OverlayWriter,
    ) -> Result<()> {
        let backing_final = ov.backing_path_for(path);
        let temporary = promotion_file(&backing_final)?;
        let (temporary, size) = reader.copy_blob(oid, temporary).await?;
        // Publish without another await: cancellation cannot leave a visible
        // backing file between its installation and overlay metadata recording.
        temporary
            .persist(&backing_final)
            .map_err(|error| CrabError::Io(error.error))?;
        ov.promote_from_file(path, mode, size, Some(oid))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Return a reference to the overlay, or error if writes are not
    /// supported (read-only mount).
    fn require_overlay(&self) -> Result<&dyn OverlayWriter> {
        self.overlay.as_deref().ok_or_else(|| CrabError::Forbidden {
            path: "write operations require an overlay (read-only mount)".into(),
        })
    }

    fn next_overlay_version(&self) -> u64 {
        self.overlay_version.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn overlay_view_version_for_path(&self, path: &str) -> u64 {
        let path = clean_lock_path(path);
        let mut version = self.overlay_reset_version.load(Ordering::Acquire);
        if let Some(path_version) = self.overlay_path_versions.get(&path) {
            version = version.max(*path_version);
        }
        for ancestor in path_and_ancestors(&path) {
            if let Some(subtree_version) = self.overlay_subtree_versions.get(&ancestor) {
                version = version.max(*subtree_version);
            }
        }
        version
    }

    fn invalidate_overlay_path(&self, path: &str) {
        self.apply_invalidation(VfsInvalidation::PathChanged {
            path: path.to_owned(),
        });
    }

    fn invalidate_overlay_subtree(&self, path: &str) {
        self.apply_invalidation(VfsInvalidation::SubtreeRemoved {
            path: path.to_owned(),
        });
    }

    fn invalidate_overlay_rename(&self, old_path: &str, new_path: &str) {
        self.apply_invalidation(VfsInvalidation::SubtreeRenamed {
            old_path: old_path.to_owned(),
            new_path: new_path.to_owned(),
        });
    }

    pub fn apply_invalidation(&self, invalidation: VfsInvalidation) {
        self.read_metrics.record_invalidation(&invalidation);
        match invalidation {
            VfsInvalidation::PathChanged { path } => {
                trace!(path, "applying VFS path invalidation");
                self.apply_path_invalidation(&path);
            }
            VfsInvalidation::SubtreeRemoved { path } => {
                trace!(path, "applying VFS subtree invalidation");
                self.apply_subtree_invalidation(&path);
            }
            VfsInvalidation::SubtreeRenamed { old_path, new_path } => {
                trace!(old_path, new_path, "applying VFS rename invalidation");
                self.apply_rename_invalidation(&old_path, &new_path);
            }
            VfsInvalidation::SnapshotGenerationChanged {
                old_generation,
                new_generation,
            } => {
                trace!(
                    old_generation = ?old_generation,
                    new_generation,
                    "applying VFS generation invalidation"
                );
                self.invalidate_all_read_sources();
            }
            VfsInvalidation::OverlayReset => {
                trace!("applying VFS overlay reset invalidation");
                self.invalidate_all_read_sources();
            }
        }
    }

    fn apply_path_invalidation(&self, path: &str) {
        let path = clean_lock_path(path);
        let version = self.next_overlay_version();
        self.overlay_path_versions.insert(path.clone(), version);
        self.read_source_cache.invalidate_path(&path);
        self.compact_overlay_invalidation_maps();
    }

    fn apply_subtree_invalidation(&self, path: &str) {
        let path = clean_lock_path(path);
        let version = self.next_overlay_version();
        self.overlay_subtree_versions.insert(path.clone(), version);
        self.read_source_cache.invalidate_subtree(&path);
        self.compact_overlay_invalidation_maps();
    }

    fn apply_rename_invalidation(&self, old_path: &str, new_path: &str) {
        let old_path = clean_lock_path(old_path);
        let new_path = clean_lock_path(new_path);
        let version = self.next_overlay_version();
        self.overlay_subtree_versions
            .insert(old_path.clone(), version);
        self.overlay_subtree_versions
            .insert(new_path.clone(), version);
        self.read_source_cache.invalidate_subtree(&old_path);
        self.read_source_cache.invalidate_subtree(&new_path);
        self.compact_overlay_invalidation_maps();
    }

    fn invalidate_all_read_sources(&self) {
        let version = self.next_overlay_version();
        self.overlay_reset_version.store(version, Ordering::Release);
        self.overlay_path_versions.clear();
        self.overlay_subtree_versions.clear();
        self.read_source_cache.invalidate_all();
    }

    fn compact_overlay_invalidation_maps(&self) {
        if self
            .overlay_path_versions
            .len()
            .saturating_add(self.overlay_subtree_versions.len())
            <= MAX_OVERLAY_INVALIDATION_ENTRIES
        {
            return;
        }

        self.read_metrics.record_compacted_full_reset();
        self.invalidate_all_read_sources();
    }
}

// ---------------------------------------------------------------------------
// File I/O helper
// ---------------------------------------------------------------------------

/// Read a byte range from a local file.
///
/// Returns up to `size` bytes starting at `offset`. If the file is shorter
/// than `offset + size`, returns whatever is available past `offset`.
fn promotion_file(backing_final: &Path) -> Result<tempfile::NamedTempFile> {
    let parent = backing_final
        .parent()
        .ok_or_else(|| CrabError::Internal("overlay backing path has no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    // A cancelled writer can still own pending I/O. Unique temporary paths
    // prevent its late writes from touching a later promotion of the same file.
    Ok(tempfile::NamedTempFile::new_in(parent)?)
}

fn read_file_range(path: &Path, offset: u64, size: u32) -> Result<Bytes> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CrabError::NotFound {
                path: path.display().to_string(),
            }
        } else {
            CrabError::Io(e)
        }
    })?;

    let metadata = file.metadata()?;
    let file_size = metadata.len();

    if offset >= file_size {
        return Ok(Bytes::new());
    }

    file.seek(SeekFrom::Start(offset))?;

    let available = (file_size - offset).min(u64::from(size));
    let mut buf = vec![0u8; available as usize];
    let n = file.read(&mut buf)?;
    buf.truncate(n);

    Ok(Bytes::from(buf))
}

/// Saturating cast from u64 to u32 (caps at `u32::MAX`).
#[cfg(test)]
fn saturating_u32(v: u64) -> u32 {
    if v > u64::from(u32::MAX) {
        u32::MAX
    } else {
        v as u32
    }
}

/// Current time in nanoseconds since the Unix epoch.
fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

fn mutation_lock_paths(path: &str) -> Vec<String> {
    let path = clean_lock_path(path);
    path_and_ancestors(&path)
}

fn rename_lock_paths(old_path: &str, new_path: &str) -> Vec<String> {
    let old_path = clean_lock_path(old_path);
    let new_path = clean_lock_path(new_path);
    let mut paths = path_and_ancestors(&old_path);
    paths.extend(path_and_ancestors(&new_path));
    paths
}

fn path_and_ancestors(path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }

    let mut paths = Vec::new();
    let mut current = String::new();
    for part in path.split('/') {
        if part.is_empty() {
            continue;
        }
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(part);
        paths.push(current.clone());
    }
    paths
}

fn clean_lock_path(path: &str) -> String {
    path.trim_matches('/').to_owned()
}

fn is_descendant_path(path: &str, parent: &str) -> bool {
    if parent.is_empty() {
        return !path.is_empty();
    }
    path.strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn path_is_at_or_under(path: &str, parent: &str) -> bool {
    path == parent || is_descendant_path(path, parent)
}

fn base_rename_entry(base: &BaseNode, old_path: &str, new_path: &str) -> BaseRenameEntry {
    BaseRenameEntry {
        old_path: old_path.to_owned(),
        new_path: new_path.to_owned(),
        node_type: base.node_type,
        mode: base.mode,
        size: base.size,
        source_oid: base_source_oid(base),
    }
}

fn base_source_oid(base: &BaseNode) -> Option<String> {
    base.object_oid.clone()
}

fn ensure_readable_file(node_type: NodeType, path: &str) -> Result<()> {
    if node_type == NodeType::File {
        return Ok(());
    }
    Err(CrabError::Forbidden {
        path: format!("cannot read non-file path: {path}"),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
#[expect(
    clippy::panic,
    reason = "test assertions reject unexpected result variants"
)]
mod tests {
    use super::*;
    use crate::data_plane::{FileIndexResolver, ReconstructionTerm, ShardLoader, XorbFetcher};
    use crate::overlay::OverlayStore;
    use crate::resolver::OverlayLookup;
    use crate::test_support::{create_git_repo_with_blob, write_git_blob};
    use crate::verified_set::VerifiedSet;
    use std::ops::Range;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    struct NoopFileIndexResolver;

    impl FileIndexResolver for NoopFileIndexResolver {
        fn resolve_file_index(
            &self,
            _file_hash: &[u8; 32],
            _shard_hint: Option<&[u8; 32]>,
        ) -> Result<Option<[u8; 32]>> {
            Ok(None)
        }

        fn scan_shard_list_for_file(&self, _file_hash: &[u8; 32]) -> Result<Option<[u8; 32]>> {
            Ok(None)
        }
    }

    struct NoopShardLoader;

    impl ShardLoader for NoopShardLoader {
        fn load_reconstruction_terms(
            &self,
            _shard_hash: &[u8; 32],
            _file_hash: &[u8; 32],
        ) -> Result<Vec<ReconstructionTerm>> {
            Ok(Vec::new())
        }
    }

    struct NoopXorbFetcher;

    impl XorbFetcher for NoopXorbFetcher {
        fn fetch_range(&self, _xorb_hash: &[u8; 32], _range: Range<u64>) -> Result<Vec<u8>> {
            Err(CrabError::NotFound {
                path: "noop xorb fetcher".into(),
            })
        }
    }

    struct CountingOverlay {
        store: OverlayStore,
        promote_count: AtomicUsize,
        promote_delay: Duration,
    }

    impl CountingOverlay {
        fn new(store: OverlayStore, promote_delay: Duration) -> Self {
            Self {
                store,
                promote_count: AtomicUsize::new(0),
                promote_delay,
            }
        }
    }

    impl OverlayLookup for CountingOverlay {
        fn get(&self, path: &str) -> Option<OverlayEntry> {
            OverlayLookup::get(&self.store, path)
        }

        fn list_by_prefix(&self, parent_path: &str) -> Vec<OverlayEntry> {
            self.store.list_by_prefix(parent_path)
        }
    }

    impl OverlayWriter for CountingOverlay {
        fn get(&self, path: &str) -> Option<OverlayEntry> {
            OverlayLookup::get(self, path)
        }

        fn get_backing_path(&self, path: &str) -> Option<PathBuf> {
            self.store.get_backing_path(path)
        }

        fn create_file(&self, path: &str, mode: u32) -> Result<OverlayEntry> {
            self.store.create_file(path, mode)
        }

        fn write_file(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
            self.store.write_file(path, offset, data)
        }

        fn promote(
            &self,
            path: &str,
            mode: u32,
            content: &[u8],
            source_oid: Option<&str>,
        ) -> Result<OverlayEntry> {
            self.promote_count.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.promote_delay);
            self.store.promote(path, mode, content, source_oid)
        }

        fn remove(&self, path: &str) -> Result<()> {
            self.store.remove(path)
        }

        fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
            self.store.rename(old_path, new_path)
        }

        fn rename_base_subtree(&self, entries: &[BaseRenameEntry]) -> Result<()> {
            self.store.rename_base_subtree(entries)
        }

        fn mkdir(&self, path: &str, mode: u32) -> Result<()> {
            self.store.mkdir(path, mode)
        }

        fn rmdir(&self, path: &str) -> Result<()> {
            self.store.rmdir(path)
        }

        fn set_mtime(&self, path: &str, mtime_ns: i64) -> Result<()> {
            self.store.set_mtime(path, mtime_ns)
        }

        fn set_mode(&self, path: &str, mode: u32) -> Result<()> {
            self.store.set_mode(path, mode)
        }

        fn mutate_content(
            &self,
            path: Option<&str>,
            file: &std::fs::File,
            mtime_ns: i64,
            mutation: &mut dyn FnMut(&std::fs::File) -> Result<()>,
        ) -> Result<()> {
            self.store.mutate_content(path, file, mtime_ns, mutation)
        }

        fn promote_from_file(
            &self,
            path: &str,
            mode: u32,
            size: u64,
            _source_oid: Option<&str>,
        ) -> Result<OverlayEntry> {
            self.promote_count.fetch_add(1, Ordering::SeqCst);
            self.store.promote_from_file(path, mode, size, _source_oid)
        }

        fn backing_path_for(&self, path: &str) -> PathBuf {
            self.store.backing_path_for(path)
        }

        fn create_symlink(&self, path: &str, target: &str, mode: u32) -> Result<OverlayEntry> {
            self.store.create_symlink(path, target, mode)
        }
    }

    struct TestEngine {
        engine: Arc<VfsEngine>,
        overlay: Arc<CountingOverlay>,
        _root: tempfile::TempDir,
        _cache: tempfile::TempDir,
    }

    fn test_engine_with_base_file(path: &str) -> TestEngine {
        test_engine_with_nodes(vec![BaseNode {
            path: path.to_owned(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: None,
            pointer: None,
            size: 0,
        }])
    }

    fn test_engine_with_nodes(nodes: Vec<BaseNode>) -> TestEngine {
        let root = tempfile::tempdir().unwrap();
        let snapshot =
            Arc::new(SnapshotStore::open_or_create(&root.path().join("snapshot.sqlite")).unwrap());
        snapshot
            .publish_generation("abc123", "refs/heads/main", &nodes)
            .unwrap();

        let overlay_store =
            OverlayStore::open(&root.path().join("overlay.db"), &root.path().join("upper"))
                .unwrap();
        let overlay = Arc::new(CountingOverlay::new(
            overlay_store,
            Duration::from_millis(50),
        ));
        let overlay_lookup: Arc<dyn OverlayLookup> = overlay.clone();
        let overlay_writer: Arc<dyn OverlayWriter> = overlay.clone();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(
            crate::ChunkCache::open(cache_dir.path().join("chunks"), Some(1024 * 1024)).unwrap(),
        );
        let hydration = HydrationService::new(
            cache,
            Arc::new(VerifiedSet::new(16)),
            Arc::new(NoopFileIndexResolver),
            Arc::new(NoopShardLoader),
            Arc::new(NoopXorbFetcher),
            None,
            Some(1),
            CancellationToken::new(),
        );
        let resolver = Arc::new(
            FuseResolver::new(
                Arc::clone(&snapshot),
                Some(overlay_lookup),
                1,
                0,
                Some(Arc::clone(&hydration)),
                None,
            )
            .unwrap(),
        );
        let engine = Arc::new(VfsEngine::new(
            resolver,
            Some(overlay_writer),
            hydration,
            None,
            Some(snapshot),
        ));

        TestEngine {
            engine,
            overlay,
            _root: root,
            _cache: cache_dir,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn native_writeback_drains_before_engine_mutations_are_blocked() {
        let fixture = test_engine_with_base_file("file.txt");
        let writeback = fixture.engine.begin_native_writeback().await;
        let freeze = fixture.engine.quiesce_overlay();
        tokio::pin!(freeze);
        tokio::time::timeout(Duration::from_millis(10), freeze.as_mut())
            .await
            .unwrap_err();
        tokio::time::timeout(
            Duration::from_secs(5),
            fixture.engine.write("file.txt", 0, b"accepted write"),
        )
        .await
        .unwrap()
        .unwrap();
        drop(writeback);
        let _freeze = tokio::time::timeout(Duration::from_secs(5), freeze)
            .await
            .unwrap();
        let backing = fixture.overlay.get_backing_path("file.txt").unwrap();
        assert_eq!(std::fs::read(backing).unwrap(), b"accepted write");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_read_reacquires_a_lease_invalidated_before_io() {
        let fixture = test_engine_with_base_file("empty.txt");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let start = Arc::new(std::sync::Barrier::new(2));
        let writer = {
            let engine = Arc::clone(&fixture.engine);
            let stop = Arc::clone(&stop);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                while !stop.load(Ordering::Acquire) {
                    engine.invalidate_overlay_path("empty.txt");
                    std::thread::yield_now();
                }
            })
        };
        start.wait();
        let mut failure = None;
        for _ in 0..1000 {
            if let Err(error) = fixture.engine.read("empty.txt", 0, 10).await {
                failure = Some(error);
                break;
            }
        }
        stop.store(true, Ordering::Release);
        writer.join().unwrap();
        assert!(
            failure.is_none(),
            "path read exposed its internal lease race: {failure:?}"
        );
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn open_file_base_reads_count_attempts() {
        let fixture = test_engine_with_base_file("empty.txt");
        let held = fixture
            .engine
            .open_file("empty.txt", true, false)
            .await
            .unwrap();
        fixture.engine.read_open_file(&held, 0, 10).await.unwrap();
        let metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!((metrics.read_at_calls, metrics.base_empty.reads), (1, 1));
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn truncate_to_zero_keeps_local_ownership_after_reconciliation() {
        for descriptor in [false, true] {
            let (_odb, git_dir, oid) = create_git_repo_with_blob(b"before");
            let mut fixture = test_engine_with_nodes(vec![BaseNode {
                path: "file.txt".into(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: Some(oid),
                pointer: None,
                size: 0,
            }]);
            Arc::get_mut(&mut fixture.engine).unwrap().odb_reader =
                Some(Arc::new(OdbReader::new(&git_dir).unwrap()));
            if descriptor {
                let mut held = fixture
                    .engine
                    .open_file("file.txt", true, true)
                    .await
                    .unwrap();
                fixture
                    .engine
                    .truncate_open_file(&mut held, Some("file.txt"), 0)
                    .await
                    .unwrap();
            } else {
                fixture.engine.truncate("file.txt", 0).await.unwrap();
            }
            fixture.overlay.store.reconcile(|_| None).unwrap();
            assert!(
                fixture
                    .engine
                    .read("file.txt", 0, 100)
                    .await
                    .unwrap()
                    .is_empty(),
                "descriptor={descriptor}"
            );
        }
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn open_file_access_upgrade_rejects_replaced_backing() {
        let fixture = test_engine_with_base_file("base.txt");
        fixture.engine.create("held.txt", 0o100_644).await.unwrap();
        fixture.engine.write("held.txt", 0, b"old").await.unwrap();
        let mut held = fixture
            .engine
            .open_file("held.txt", true, false)
            .await
            .unwrap();
        fixture.engine.unlink("held.txt").await.unwrap();
        fixture.engine.create("held.txt", 0o100_644).await.unwrap();
        assert!(matches!(
            held.enable_access(&fixture.engine, Some("held.txt"), true, true)
                .await,
            Err(CrabError::NotFound { .. })
        ));
        assert_eq!(
            fixture.engine.read_open_file(&held, 0, 100).await.unwrap(),
            b"old".as_slice()
        );
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn rename_no_replace_preserves_both_existing_files() {
        let fixture = test_engine_with_base_file("base.txt");
        for (path, content) in [
            ("from.txt", b"source".as_slice()),
            ("to.txt", b"target".as_slice()),
        ] {
            fixture.engine.create(path, 0o100_644).await.unwrap();
            fixture.engine.write(path, 0, content).await.unwrap();
        }
        assert!(
            matches!(fixture.engine.rename_no_replace("from.txt", "to.txt").await,
            Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(
            (
                fixture.engine.read("from.txt", 0, 100).await.unwrap(),
                fixture.engine.read("to.txt", 0, 100).await.unwrap()
            ),
            (Bytes::from_static(b"source"), Bytes::from_static(b"target"))
        );
        fixture
            .engine
            .rename_no_replace("from.txt", "absent.txt")
            .await
            .unwrap();
        assert_eq!(
            fixture.engine.read("absent.txt", 0, 100).await.unwrap(),
            b"source".as_slice()
        );
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn open_file_does_not_write_a_reused_path() {
        let fixture = test_engine_with_base_file("base.txt");
        fixture.engine.create("held.txt", 0o100_644).await.unwrap();
        fixture.engine.write("held.txt", 0, b"old").await.unwrap();
        let mut held = fixture
            .engine
            .open_file("held.txt", true, true)
            .await
            .unwrap();
        fixture.engine.unlink("held.txt").await.unwrap();
        fixture.engine.create("held.txt", 0o100_644).await.unwrap();
        fixture.engine.write("held.txt", 0, b"new").await.unwrap();
        fixture
            .engine
            .write_open_file(&mut held, Some("held.txt"), 0, b"OLD")
            .await
            .unwrap();
        assert_eq!(
            (
                fixture.engine.read_open_file(&held, 0, 100).await.unwrap(),
                fixture.engine.read("held.txt", 0, 100).await.unwrap()
            ),
            (Bytes::from_static(b"OLD"), Bytes::from_static(b"new")),
        );
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn open_file_keeps_git_bytes_after_snapshot_replacement() {
        let (_odb, git_dir, old_oid) = create_git_repo_with_blob(b"old");
        let new_oid = write_git_blob(&git_dir, b"new longer bytes");
        let node = BaseNode {
            path: "file.txt".into(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some(old_oid),
            pointer: None,
            size: 0,
        };
        let mut fixture = test_engine_with_nodes(vec![node.clone()]);
        Arc::get_mut(&mut fixture.engine).unwrap().odb_reader =
            Some(Arc::new(OdbReader::new(&git_dir).unwrap()));
        let mut held = fixture
            .engine
            .open_file("file.txt", true, true)
            .await
            .unwrap();
        fixture
            .engine
            .snapshot
            .as_ref()
            .unwrap()
            .publish_generation(
                "next",
                "refs/heads/main",
                &[BaseNode {
                    object_oid: Some(new_oid),
                    ..node
                }],
            )
            .unwrap();
        fixture
            .engine
            .resolver
            .set_generation(2, 0, Some(Arc::clone(&fixture.engine.hydration)), None)
            .unwrap();
        assert_eq!(
            fixture.engine.read_open_file(&held, 0, 100).await.unwrap(),
            b"old".as_slice()
        );
        fixture
            .engine
            .write_open_file(&mut held, Some("file.txt"), 0, b"OLD")
            .await
            .unwrap();
        assert_eq!(
            (
                fixture.engine.read_open_file(&held, 0, 100).await.unwrap(),
                fixture.engine.read("file.txt", 0, 100).await.unwrap()
            ),
            (
                Bytes::from_static(b"OLD"),
                Bytes::from_static(b"new longer bytes")
            ),
        );
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn open_file_write_survives_base_reconciliation() {
        let (_odb, git_dir, oid) = create_git_repo_with_blob(b"before");
        let mut fixture = test_engine_with_nodes(vec![BaseNode {
            path: "file.txt".into(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some(oid),
            pointer: None,
            size: 0,
        }]);
        Arc::get_mut(&mut fixture.engine).unwrap().odb_reader =
            Some(Arc::new(OdbReader::new(&git_dir).unwrap()));
        let mut held = fixture
            .engine
            .open_file("file.txt", true, true)
            .await
            .unwrap();
        fixture
            .engine
            .write_open_file(&mut held, Some("file.txt"), 0, b"edited")
            .await
            .unwrap();
        fixture.overlay.store.reconcile(|_| None).unwrap();
        assert_eq!(
            fixture.engine.read("file.txt", 0, 100).await.unwrap(),
            b"edited".as_slice()
        );
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn publication_freeze_rejects_content_changes_before_file_io() {
        for operation in 0..3 {
            let fixture = test_engine_with_base_file("base.txt");
            fixture.engine.create("held.txt", 0o100_644).await.unwrap();
            fixture
                .engine
                .write("held.txt", 0, b"before")
                .await
                .unwrap();
            let mut held = fixture
                .engine
                .open_file("held.txt", true, true)
                .await
                .unwrap();
            let backing = fixture.overlay.get_backing_path("held.txt").unwrap();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let overlay = OverlayStore::open(
                &fixture._root.path().join("overlay.db"),
                &fixture._root.path().join("upper"),
            )
            .unwrap();
            let freezer = std::thread::spawn(move || {
                let _freeze = overlay.freeze_writes().unwrap();
                ready_tx.send(()).unwrap();
                let _ = release_rx.recv();
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let result = match operation {
                0 => fixture
                    .engine
                    .write_open_file(&mut held, Some("held.txt"), 0, b"WRONG!")
                    .await
                    .map(|_| ()),
                1 => {
                    fixture
                        .engine
                        .truncate_open_file(&mut held, Some("held.txt"), 0)
                        .await
                }
                _ => fixture.engine.truncate("held.txt", 0).await,
            };
            drop(release_tx);
            freezer.join().unwrap();
            assert_eq!(
                (result.is_err(), std::fs::read(&backing).unwrap()),
                (true, b"before".to_vec()),
                "operation={operation}"
            );
        }
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn open_file_sync_returns_the_descriptor_error() {
        let fixture = test_engine_with_base_file("base.txt");
        fixture.engine.create("fault.txt", 0o100_644).await.unwrap();
        let backing = fixture.overlay.get_backing_path("fault.txt").unwrap();
        std::fs::remove_file(&backing).unwrap();
        std::os::unix::fs::symlink("/dev/null", &backing).unwrap();
        let held = fixture
            .engine
            .open_file("fault.txt", true, false)
            .await
            .unwrap();
        assert!(matches!(
            fixture.engine.sync_open_file(&held, false),
            Err(CrabError::Io(_))
        ));
    }

    // --- read_file_range tests ---

    #[test]
    fn mutation_locks_include_all_ancestors() {
        assert_eq!(
            mutation_lock_paths("/models/checkpoints/model.bin"),
            vec![
                "models".to_owned(),
                "models/checkpoints".to_owned(),
                "models/checkpoints/model.bin".to_owned()
            ]
        );
    }

    #[test]
    fn rename_locks_intersect_descendant_mutations() {
        let rename_locks = rename_lock_paths("models", "renamed-models")
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let write_locks = mutation_lock_paths("models/checkpoints/model.bin")
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert!(rename_locks.contains("models"));
        assert!(write_locks.contains("models"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_control_waiter_retains_transition_until_owned_work_finishes() {
        let fixture = test_engine_with_base_file("file.txt");
        let engine = Arc::clone(&fixture.engine);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let notify = Arc::clone(&entered);
        let resume = Arc::clone(&release);
        let owner = Arc::clone(&engine);
        let caller = tokio::spawn(async move {
            engine
                .run_control(async move {
                    let _reset = owner.begin_overlay_reset().await;
                    notify.notify_one();
                    resume.notified().await;
                    Ok(())
                })
                .await
        });
        entered.notified().await;
        caller.abort();
        let _ = caller.await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                fixture.engine.shutdown_background()
            )
            .await
            .is_err()
        );
        assert!(matches!(
            fixture.engine.run_control(async { Ok(()) }).await,
            Err(CrabError::Cancelled)
        ));
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), fixture.engine.shutdown_background())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_first_writes_promote_base_file_once() {
        let fixture = test_engine_with_base_file("large.bin");

        let first = fixture.engine.write("large.bin", 0, b"A");
        let second = fixture.engine.write("large.bin", 1, b"B");
        let (first, second) = tokio::join!(first, second);

        assert_eq!(first.unwrap(), 1);
        assert_eq!(second.unwrap(), 1);
        assert_eq!(fixture.overlay.promote_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overlay_reset_rejects_mutation_started_during_reset() {
        let fixture = test_engine_with_base_file("large.bin");
        let reset = fixture.engine.begin_overlay_reset().await;
        let mutation = fixture.engine.begin_overlay_mutation();
        tokio::pin!(mutation);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), mutation.as_mut())
                .await
                .is_err()
        );

        drop(reset);
        let result = mutation.await;
        assert!(
            matches!(result, Err(CrabError::Forbidden { path }) if path.contains("overlay reset raced"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rmdir_rejects_non_empty_base_directory() {
        let fixture = test_engine_with_nodes(vec![
            BaseNode {
                path: "dir".to_owned(),
                node_type: NodeType::Dir,
                mode: 0o040_755,
                object_oid: None,
                pointer: None,
                size: 0,
            },
            BaseNode {
                path: "dir/file.txt".to_owned(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: None,
                pointer: None,
                size: 0,
            },
        ]);

        let err = fixture.engine.rmdir("dir").await.unwrap_err();

        assert!(
            matches!(err, CrabError::Forbidden { path } if path.starts_with("directory not empty:"))
        );
        assert!(fixture.engine.resolver.resolve_path("dir").is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_mtime_promotes_base_file() {
        let fixture = test_engine_with_base_file("touch.bin");
        let mtime_ns = 1_800_000_123_456_789_000;

        fixture
            .engine
            .set_mtime("touch.bin", mtime_ns)
            .await
            .unwrap();

        let entry = OverlayLookup::get(fixture.overlay.as_ref(), "touch.bin").unwrap();
        assert_eq!(entry.mtime_ns, mtime_ns);
        assert_eq!(fixture.overlay.promote_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_mode_promotes_base_file() {
        let fixture = test_engine_with_base_file("script.sh");

        fixture.engine.set_mode("script.sh", 0o755).await.unwrap();

        let entry = OverlayLookup::get(fixture.overlay.as_ref(), "script.sh").unwrap();
        assert_eq!(entry.mode & 0o777, 0o755);
        assert_eq!(fixture.overlay.promote_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn base_pointer_read_lease_uses_content_identity() {
        let pointer = crab_types::pointer::Pointer {
            file_hash: [9; 32],
            size: 64 * 1024 * 1024,
            shard_hint: Some([4; 32]),
        };
        let fixture = test_engine_with_nodes(vec![BaseNode {
            path: "model.bin".to_owned(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some("pointer-blob-oid".to_owned()),
            pointer: Some(crate::blob::FilePointer::Crab(pointer.clone())),
            size: pointer.size,
        }]);

        let lease = fixture.engine.open_read("model.bin").unwrap();

        assert_eq!(
            lease.key(),
            &ReadSourceKey::BasePointer {
                generation: 1,
                overlay_version: 0,
                file_hash: crate::blob::PointerIdentity::Crab(pointer.file_hash),
                size: pointer.size,
            }
        );
        assert_eq!(lease.known_size(), Some(pointer.size));
    }

    #[test]
    fn unknown_pointer_blob_is_classified_before_direct_read() {
        let pointer = crab_types::pointer::Pointer {
            file_hash: [9; 32],
            size: 64 * 1024 * 1024,
            shard_hint: Some([4; 32]),
        };
        let (_odb_root, git_dir, oid) = create_git_repo_with_blob(&pointer.serialize());
        let mut fixture = test_engine_with_nodes(vec![BaseNode {
            path: "model.bin".to_owned(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some(oid),
            pointer: None,
            size: 0,
        }]);
        let reader = OdbReader::new(&git_dir).unwrap();
        Arc::get_mut(&mut fixture.engine).unwrap().odb_reader = Some(Arc::new(reader));

        let lease = fixture.engine.open_read("model.bin").unwrap();

        assert_eq!(
            lease.key(),
            &ReadSourceKey::BasePointer {
                generation: 1,
                overlay_version: 0,
                file_hash: crate::blob::PointerIdentity::Crab(pointer.file_hash),
                size: pointer.size,
            }
        );
        let classified = fixture
            .engine
            .snapshot
            .as_ref()
            .unwrap()
            .get_node(1, "model.bin")
            .unwrap()
            .unwrap();
        assert_eq!(classified.pointer, Some(pointer.into()));
    }

    #[test]
    fn empty_base_file_read_lease_has_zero_size_identity() {
        let fixture = test_engine_with_base_file("empty.txt");

        let lease = fixture.engine.open_read("empty.txt").unwrap();

        assert_eq!(
            lease.key(),
            &ReadSourceKey::BaseEmpty {
                generation: 1,
                overlay_version: 0,
                path: "empty.txt".to_owned(),
            }
        );
        assert_eq!(lease.known_size(), Some(0));
    }

    #[test]
    fn base_blob_read_lease_uses_oid_identity() {
        let fixture = test_engine_with_nodes(vec![BaseNode {
            path: "small.bin".to_owned(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some("0123456789abcdef".to_owned()),
            pointer: None,
            size: 0,
        }]);

        let lease = fixture.engine.open_read("small.bin").unwrap();

        assert_eq!(
            lease.key(),
            &ReadSourceKey::BaseBlob {
                generation: 1,
                overlay_version: 0,
                object_oid: "0123456789abcdef".to_owned(),
                known_size: None,
            }
        );
        assert_eq!(lease.known_size(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overlay_file_read_lease_uses_overlay_identity() {
        let fixture = test_engine_with_base_file("mutable.bin");

        fixture
            .engine
            .write("mutable.bin", 0, b"overlay")
            .await
            .unwrap();
        let entry = OverlayLookup::get(fixture.overlay.as_ref(), "mutable.bin").unwrap();
        let lease = fixture.engine.open_read("mutable.bin").unwrap();

        assert_eq!(
            lease.key(),
            &ReadSourceKey::OverlayFile {
                path: "mutable.bin".to_owned(),
                overlay_version: 1,
                size: entry.size,
                mtime_ns: entry.mtime_ns,
            }
        );
        assert_eq!(lease.known_size(), Some(7));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_metrics_classify_per_lease_read_patterns() {
        let fixture = test_engine_with_base_file("empty.txt");
        let lease = fixture.engine.open_read("empty.txt").unwrap();

        fixture.engine.read_at(&lease, 0, 1).await.unwrap();
        fixture.engine.read_at(&lease, 1, 1).await.unwrap();
        fixture.engine.read_at(&lease, 3, 1).await.unwrap();
        fixture.engine.read_at(&lease, 5, 1).await.unwrap();
        fixture.engine.read_at(&lease, 5, 1).await.unwrap();

        let metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(metrics.open_read_calls, 1);
        assert_eq!(metrics.read_at_calls, 5);
        assert_eq!(metrics.base_empty.reads, 5);
        assert_eq!(metrics.base_empty.adaptive.first, 1);
        assert_eq!(metrics.base_empty.adaptive.sequential, 1);
        assert_eq!(metrics.base_empty.adaptive.random, 1);
        assert_eq!(metrics.base_empty.adaptive.strided, 1);
        assert_eq!(metrics.base_empty.adaptive.repeated, 1);
    }

    #[test]
    fn read_pattern_prefetches_sequential_next_window() {
        let pattern = ReadPatternState::default();

        assert_eq!(
            pattern.record_read(0, 4096),
            AdaptiveReadDecision {
                class: AdaptiveReadClass::First,
                prefetch: None,
            }
        );
        assert_eq!(
            pattern.record_read(4096, 4096),
            AdaptiveReadDecision {
                class: AdaptiveReadClass::Sequential,
                prefetch: Some(AdaptivePrefetch::NextWindow),
            }
        );
    }

    #[test]
    fn read_pattern_prefetches_one_strided_target_window() {
        let pattern = ReadPatternState::default();

        assert_eq!(pattern.record_read(0, 4096).class, AdaptiveReadClass::First);
        assert_eq!(
            pattern.record_read(16 * 1024, 4096).class,
            AdaptiveReadClass::Random
        );
        assert_eq!(
            pattern.record_read(32 * 1024, 4096),
            AdaptiveReadDecision {
                class: AdaptiveReadClass::Strided,
                prefetch: Some(AdaptivePrefetch::TargetWindow {
                    offset: 48 * 1024,
                    size: 4096,
                }),
            }
        );
    }

    #[test]
    fn read_pattern_keeps_repeated_and_random_reads_unspeculative() {
        let pattern = ReadPatternState::default();

        assert_eq!(pattern.record_read(0, 4096).class, AdaptiveReadClass::First);
        assert_eq!(
            pattern.record_read(0, 4096),
            AdaptiveReadDecision {
                class: AdaptiveReadClass::Repeated,
                prefetch: None,
            }
        );
        assert_eq!(
            pattern.record_read(24 * 1024, 4096),
            AdaptiveReadDecision {
                class: AdaptiveReadClass::Random,
                prefetch: None,
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_cache_reuses_read_lease_state_across_opens() {
        let fixture = test_engine_with_base_file("empty.txt");

        let first = fixture.engine.open_read("empty.txt").unwrap();
        fixture.engine.read_at(&first, 0, 1).await.unwrap();
        let second = fixture.engine.open_read("empty.txt").unwrap();
        fixture.engine.read_at(&second, 1, 1).await.unwrap();

        let metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(metrics.open_read_calls, 2);
        assert_eq!(metrics.source_cache_entries, 1);
        assert_eq!(metrics.source_cache_hits, 1);
        assert_eq!(metrics.resolver_calls_avoided, 1);
        assert_eq!(metrics.source_cache_misses, 1);
        assert_eq!(metrics.base_empty.adaptive.first, 1);
        assert_eq!(metrics.base_empty.adaptive.sequential, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_cache_invalidates_after_overlay_mutation() {
        let fixture = test_engine_with_base_file("mutable.bin");

        let lease = fixture.engine.open_read("mutable.bin").unwrap();
        assert_eq!(
            fixture.engine.read_metrics_snapshot().source_cache_entries,
            1
        );

        fixture.engine.write("mutable.bin", 0, b"A").await.unwrap();

        let metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(metrics.source_cache_entries, 0);
        assert_eq!(metrics.source_cache_invalidations, 1);
        let err = fixture.engine.read_at(&lease, 0, 1).await.unwrap_err();
        assert!(
            matches!(err, CrabError::Internal(message) if message.contains("stale VFS read lease overlay view"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_cache_keeps_unrelated_read_source_after_overlay_mutation() {
        let fixture = test_engine_with_nodes(vec![
            BaseNode {
                path: "changed.bin".to_owned(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: None,
                pointer: None,
                size: 0,
            },
            BaseNode {
                path: "unchanged.bin".to_owned(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: None,
                pointer: None,
                size: 0,
            },
        ]);

        let changed = fixture.engine.open_read("changed.bin").unwrap();
        let unchanged = fixture.engine.open_read("unchanged.bin").unwrap();

        fixture.engine.write("changed.bin", 0, b"A").await.unwrap();

        assert_stale_read_lease(fixture.engine.read_at(&changed, 0, 1).await);
        fixture.engine.read_at(&unchanged, 0, 1).await.unwrap();
        fixture.engine.open_read("unchanged.bin").unwrap();

        let metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(metrics.source_cache_entries, 1);
        assert_eq!(metrics.source_cache_hits, 1);
        assert_eq!(metrics.source_cache_invalidations, 1);
        assert_eq!(metrics.source_cache_stale_evictions, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_cache_invalidates_renamed_subtree_without_dropping_sibling() {
        let fixture = test_engine_with_nodes(vec![
            BaseNode {
                path: "models".to_owned(),
                node_type: NodeType::Dir,
                mode: 0o040_755,
                object_oid: None,
                pointer: None,
                size: 0,
            },
            BaseNode {
                path: "models/model.bin".to_owned(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: None,
                pointer: None,
                size: 0,
            },
            BaseNode {
                path: "sibling.bin".to_owned(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: None,
                pointer: None,
                size: 0,
            },
        ]);

        let moved = fixture.engine.open_read("models/model.bin").unwrap();
        fixture.engine.open_read("sibling.bin").unwrap();

        fixture
            .engine
            .rename("models", "renamed-models")
            .await
            .unwrap();

        assert_stale_read_lease(fixture.engine.read_at(&moved, 0, 1).await);
        fixture.engine.open_read("sibling.bin").unwrap();

        let metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(metrics.source_cache_entries, 1);
        assert_eq!(metrics.source_cache_hits, 1);
        assert_eq!(metrics.source_cache_invalidations, 2);
    }

    #[test]
    fn overlay_invalidation_events_compact_to_full_reset_when_map_fills() {
        let fixture = test_engine_with_base_file("seed.bin");

        for i in 0..=MAX_OVERLAY_INVALIDATION_ENTRIES {
            fixture
                .engine
                .invalidate_overlay_path(&format!("file-{i}.bin"));
        }

        assert_eq!(fixture.engine.overlay_path_versions.len(), 0);
        assert_eq!(fixture.engine.overlay_subtree_versions.len(), 0);
        assert!(fixture.engine.overlay_reset_version.load(Ordering::Acquire) > 0);
        assert_eq!(
            fixture
                .engine
                .read_metrics_snapshot()
                .invalidation_compacted_full_resets,
            1
        );
    }

    #[test]
    fn vfs_invalidation_events_drive_cache_and_metrics() {
        let fixture = test_engine_with_nodes(vec![
            BaseNode {
                path: "dir".to_owned(),
                node_type: NodeType::Dir,
                mode: 0o040_755,
                object_oid: None,
                pointer: None,
                size: 0,
            },
            BaseNode {
                path: "dir/a.bin".to_owned(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: None,
                pointer: None,
                size: 0,
            },
            BaseNode {
                path: "other.bin".to_owned(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: None,
                pointer: None,
                size: 0,
            },
        ]);

        fixture.engine.open_read("dir/a.bin").unwrap();
        fixture.engine.open_read("other.bin").unwrap();

        fixture
            .engine
            .apply_invalidation(VfsInvalidation::PathChanged {
                path: "dir/a.bin".to_owned(),
            });
        let path_metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(path_metrics.invalidation_path_events, 1);
        assert_eq!(path_metrics.source_cache_entries, 1);

        fixture.engine.open_read("dir/a.bin").unwrap();
        fixture
            .engine
            .apply_invalidation(VfsInvalidation::SubtreeRenamed {
                old_path: "dir".to_owned(),
                new_path: "renamed-dir".to_owned(),
            });
        let rename_metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(rename_metrics.invalidation_rename_events, 1);
        assert_eq!(rename_metrics.source_cache_entries, 1);

        fixture
            .engine
            .apply_invalidation(VfsInvalidation::SnapshotGenerationChanged {
                old_generation: Some(1),
                new_generation: 2,
            });
        let generation_metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(generation_metrics.invalidation_generation_events, 1);
        assert_eq!(generation_metrics.source_cache_entries, 0);

        fixture
            .engine
            .apply_invalidation(VfsInvalidation::SubtreeRemoved {
                path: "dir".to_owned(),
            });
        fixture
            .engine
            .apply_invalidation(VfsInvalidation::OverlayReset);
        let metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(metrics.invalidation_subtree_events, 1);
        assert_eq!(metrics.invalidation_overlay_reset_events, 1);
    }

    #[test]
    fn source_cache_evicts_stale_generation_on_open() {
        let fixture = test_engine_with_base_file("empty.txt");

        fixture.engine.open_read("empty.txt").unwrap();
        fixture
            .engine
            .snapshot
            .as_ref()
            .unwrap()
            .publish_generation("next", "refs/heads/main", &[])
            .unwrap();
        fixture
            .engine
            .resolver
            .set_generation(2, 0, Some(Arc::clone(&fixture.engine.hydration)), None)
            .unwrap();
        let Err(err) = fixture.engine.open_read("empty.txt") else {
            panic!("stale generation unexpectedly opened a cached read lease");
        };

        assert!(matches!(err, CrabError::NotFound { .. }));
        let metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(metrics.stale_generation_rejections, 1);
        assert_eq!(metrics.source_cache_entries, 0);
        assert_eq!(metrics.source_cache_misses, 2);
        assert_eq!(metrics.source_cache_stale_evictions, 1);
    }

    async fn read_cached_lease_after_signal(
        engine: Arc<VfsEngine>,
        lease: VfsReadLease,
        signal: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<Bytes> {
        signal.await.unwrap();
        engine.read_at(&lease, 0, 8).await
    }

    fn assert_stale_read_lease(result: Result<Bytes>) {
        let Err(error) = result else {
            panic!("cached read lease remained readable after mutation");
        };
        assert!(VfsEngine::is_stale_read_lease_error(&error));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_read_lease_stales_after_concurrent_write() {
        let fixture = test_engine_with_base_file("race.bin");
        let lease = fixture.engine.open_read("race.bin").unwrap();
        let engine = Arc::clone(&fixture.engine);
        let (signal, wait_for_mutation) = tokio::sync::oneshot::channel();

        let read = tokio::spawn(read_cached_lease_after_signal(
            Arc::clone(&engine),
            lease,
            wait_for_mutation,
        ));
        assert_eq!(engine.write("race.bin", 0, b"A").await.unwrap(), 1);
        assert!(signal.send(()).is_ok());

        assert_stale_read_lease(read.await.unwrap());
        assert_eq!(&engine.read("race.bin", 0, 8).await.unwrap()[..], b"A");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_read_lease_stales_after_concurrent_truncate() {
        let fixture = test_engine_with_base_file("race.bin");
        fixture
            .engine
            .write("race.bin", 0, b"abcdef")
            .await
            .unwrap();
        let lease = fixture.engine.open_read("race.bin").unwrap();
        let engine = Arc::clone(&fixture.engine);
        let (signal, wait_for_mutation) = tokio::sync::oneshot::channel();

        let read = tokio::spawn(read_cached_lease_after_signal(
            Arc::clone(&engine),
            lease,
            wait_for_mutation,
        ));
        engine.truncate("race.bin", 2).await.unwrap();
        assert!(signal.send(()).is_ok());

        assert_stale_read_lease(read.await.unwrap());
        assert_eq!(&engine.read("race.bin", 0, 8).await.unwrap()[..], b"ab");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_read_lease_stales_after_concurrent_rename() {
        let fixture = test_engine_with_base_file("old.bin");
        let lease = fixture.engine.open_read("old.bin").unwrap();
        let engine = Arc::clone(&fixture.engine);
        let (signal, wait_for_mutation) = tokio::sync::oneshot::channel();

        let read = tokio::spawn(read_cached_lease_after_signal(
            Arc::clone(&engine),
            lease,
            wait_for_mutation,
        ));
        engine.rename("old.bin", "new.bin").await.unwrap();
        assert!(signal.send(()).is_ok());

        assert_stale_read_lease(read.await.unwrap());
        assert!(engine.resolver.resolve_path("old.bin").is_err());
        assert!(engine.resolver.resolve_path("new.bin").is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_read_lease_stales_after_concurrent_remove() {
        let fixture = test_engine_with_base_file("remove.bin");
        let lease = fixture.engine.open_read("remove.bin").unwrap();
        let engine = Arc::clone(&fixture.engine);
        let (signal, wait_for_mutation) = tokio::sync::oneshot::channel();

        let read = tokio::spawn(read_cached_lease_after_signal(
            Arc::clone(&engine),
            lease,
            wait_for_mutation,
        ));
        engine.unlink("remove.bin").await.unwrap();
        assert!(signal.send(()).is_ok());

        assert_stale_read_lease(read.await.unwrap());
        assert!(engine.resolver.resolve_path("remove.bin").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overlay_read_lease_stales_after_write() {
        let fixture = test_engine_with_base_file("mutable.bin");

        fixture.engine.write("mutable.bin", 0, b"A").await.unwrap();
        let lease = fixture.engine.open_read("mutable.bin").unwrap();
        let before = lease.key().clone();

        fixture.engine.write("mutable.bin", 1, b"B").await.unwrap();

        let err = fixture.engine.read_at(&lease, 0, 2).await.unwrap_err();
        assert!(
            matches!(err, CrabError::Internal(message) if message.contains("stale VFS read lease"))
        );
        assert_eq!(
            fixture
                .engine
                .read_metrics_snapshot()
                .stale_overlay_file_rejections,
            1
        );

        let after = fixture.engine.open_read("mutable.bin").unwrap();
        assert_ne!(&before, after.key());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn base_read_lease_stales_after_overlay_promotion() {
        let fixture = test_engine_with_base_file("mutable.bin");

        let lease = fixture.engine.open_read("mutable.bin").unwrap();

        fixture.engine.write("mutable.bin", 0, b"A").await.unwrap();

        let err = fixture.engine.read_at(&lease, 0, 1).await.unwrap_err();
        assert!(
            matches!(err, CrabError::Internal(message) if message.contains("stale VFS read lease overlay view"))
        );
        assert_eq!(
            fixture
                .engine
                .read_metrics_snapshot()
                .stale_overlay_view_rejections,
            1
        );
    }

    #[test]
    fn stale_read_lease_errors_are_retryable() {
        assert!(VfsEngine::is_stale_read_lease_error(&CrabError::Internal(
            STALE_READ_LEASE_GENERATION.into()
        )));
        assert!(VfsEngine::is_stale_read_lease_error(&CrabError::Internal(
            STALE_READ_LEASE_OVERLAY_VIEW.into()
        )));
        assert!(VfsEngine::is_stale_read_lease_error(&CrabError::Internal(
            STALE_READ_LEASE_OVERLAY_FILE.into()
        )));
        assert!(!VfsEngine::is_stale_read_lease_error(&CrabError::Internal(
            "other".into()
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn truncate_zero_pointer_base_file_skips_hydration() {
        let fixture = test_engine_with_nodes(vec![BaseNode {
            path: "model.bin".to_owned(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some("pointer-blob-oid".to_owned()),
            pointer: Some(crate::blob::FilePointer::Crab(
                crab_types::pointer::Pointer {
                    file_hash: [42; 32],
                    size: 8 * 1024 * 1024 * 1024,
                    shard_hint: None,
                },
            )),
            size: 8 * 1024 * 1024 * 1024,
        }]);

        fixture.engine.truncate("model.bin", 0).await.unwrap();

        let entry = OverlayLookup::get(fixture.overlay.as_ref(), "model.bin").unwrap();
        let backing = fixture.overlay.get_backing_path("model.bin").unwrap();
        assert_eq!(entry.size, 0);
        assert!(std::fs::read(backing).unwrap().is_empty());

        fixture
            .overlay
            .store
            .reconcile(|path| {
                (path == "model.bin").then(|| crate::overlay::ReconcileBaseInfo {
                    is_dir: false,
                    object_oid: Some("pointer-blob-oid".to_owned()),
                })
            })
            .unwrap();
        assert!(OverlayLookup::get(fixture.overlay.as_ref(), "model.bin").is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_after_truncate_zero_pointer_base_file_uses_empty_overlay() {
        let fixture = test_engine_with_nodes(vec![BaseNode {
            path: "model.bin".to_owned(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: None,
            pointer: Some(crate::blob::FilePointer::Crab(
                crab_types::pointer::Pointer {
                    file_hash: [7; 32],
                    size: 4 * 1024 * 1024 * 1024,
                    shard_hint: None,
                },
            )),
            size: 4 * 1024 * 1024 * 1024,
        }]);

        fixture.engine.truncate("model.bin", 0).await.unwrap();
        fixture
            .engine
            .write("model.bin", 0, b"replacement")
            .await
            .unwrap();

        let backing = fixture.overlay.get_backing_path("model.bin").unwrap();
        assert_eq!(std::fs::read(backing).unwrap(), b"replacement");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rename_base_directory_records_metadata_without_promoting_children() {
        let fixture = test_engine_with_nodes(vec![
            BaseNode {
                path: "models".to_owned(),
                node_type: NodeType::Dir,
                mode: 0o040_755,
                object_oid: None,
                pointer: None,
                size: 0,
            },
            BaseNode {
                path: "models/model.bin".to_owned(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: None,
                pointer: None,
                size: 1024,
            },
        ]);

        fixture
            .engine
            .rename("models", "renamed-models")
            .await
            .unwrap();

        assert_eq!(fixture.overlay.promote_count.load(Ordering::SeqCst), 0);
        assert!(fixture.engine.resolver.resolve_path("models").is_err());
        assert!(
            fixture
                .engine
                .resolver
                .resolve_path("renamed-models/model.bin")
                .is_ok()
        );
        assert!(
            fixture
                .overlay
                .store
                .get_backing_path("renamed-models/model.bin")
                .is_none()
        );
    }

    #[test]
    fn read_file_range_full_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, b"hello world").unwrap();

        let data = read_file_range(&path, 0, 100).unwrap();
        assert_eq!(&data[..], b"hello world");
    }

    #[test]
    fn read_file_range_with_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, b"hello world").unwrap();

        let data = read_file_range(&path, 6, 100).unwrap();
        assert_eq!(&data[..], b"world");
    }

    #[test]
    fn read_file_range_partial() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, b"hello world").unwrap();

        let data = read_file_range(&path, 0, 5).unwrap();
        assert_eq!(&data[..], b"hello");
    }

    #[test]
    fn read_file_range_offset_past_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, b"hello").unwrap();

        let data = read_file_range(&path, 100, 10).unwrap();
        assert!(data.is_empty());
    }

    #[test]
    fn read_file_range_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.txt");

        let err = read_file_range(&path, 0, 10).unwrap_err();
        assert!(matches!(err, CrabError::NotFound { .. }));
    }

    // --- saturating_u32 tests ---

    #[test]
    fn saturating_u32_within_range() {
        assert_eq!(saturating_u32(42), 42);
        assert_eq!(saturating_u32(u64::from(u32::MAX)), u32::MAX);
    }

    #[test]
    fn saturating_u32_overflow() {
        assert_eq!(saturating_u32(u64::from(u32::MAX) + 1), u32::MAX);
        assert_eq!(saturating_u32(u64::MAX), u32::MAX);
    }

    #[test]
    fn exact_attributes_keep_one_generation_during_concurrent_swaps() {
        let (_root, git_dir, old_oid) = create_git_repo_with_blob(b"old");
        let new_oid = write_git_blob(&git_dir, b"new longer bytes");
        let node = BaseNode {
            path: "file.txt".into(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some(old_oid),
            pointer: None,
            size: 0,
        };
        let mut fixture = test_engine_with_nodes(vec![node.clone()]);
        Arc::get_mut(&mut fixture.engine).unwrap().odb_reader =
            Some(Arc::new(OdbReader::new(&git_dir).unwrap()));
        fixture
            .engine
            .resolver
            .set_generation(1, 100, Some(Arc::clone(&fixture.engine.hydration)), None)
            .unwrap();
        let _first_view = fixture.engine.resolver.capture_view();
        fixture
            .engine
            .snapshot
            .as_ref()
            .unwrap()
            .publish_generation(
                "next",
                "refs/heads/main",
                &[BaseNode {
                    object_oid: Some(new_oid),
                    ..node
                }],
            )
            .unwrap();
        fixture
            .engine
            .resolver
            .set_generation(2, 200, Some(Arc::clone(&fixture.engine.hydration)), None)
            .unwrap();
        let _second_view = fixture.engine.resolver.capture_view();
        let resolver = Arc::clone(&fixture.engine.resolver);
        let hydration = Arc::clone(&fixture.engine.hydration);
        let ready = Arc::new(std::sync::Barrier::new(2));
        let writer_ready = Arc::clone(&ready);
        let writer = std::thread::spawn(move || {
            writer_ready.wait();
            for _ in 0..5_000 {
                resolver
                    .set_generation(1, 100, Some(Arc::clone(&hydration)), None)
                    .unwrap();
                resolver
                    .set_generation(2, 200, Some(Arc::clone(&hydration)), None)
                    .unwrap();
            }
        });
        ready.wait();
        for _ in 0..5_000 {
            let (_, size, _, timestamp) = fixture.engine.getattr("file.txt").unwrap();
            assert!(matches!((size, timestamp), (3, 100) | (16, 200)));
        }
        writer.join().unwrap();
    }

    #[test]
    fn late_blob_classification_preserves_newer_snapshot_metadata() {
        let (_root, git_dir, old_oid) = create_git_repo_with_blob(b"old");
        let new_oid = write_git_blob(&git_dir, b"new longer bytes");
        let old_node = BaseNode {
            path: "file.txt".into(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some(old_oid),
            pointer: None,
            size: 0,
        };
        let mut fixture = test_engine_with_nodes(vec![old_node.clone()]);
        Arc::get_mut(&mut fixture.engine).unwrap().odb_reader =
            Some(Arc::new(OdbReader::new(&git_dir).unwrap()));
        let snapshot = fixture.engine.snapshot.as_ref().unwrap();
        snapshot
            .publish_generation(
                "next",
                "refs/heads/main",
                &[BaseNode {
                    object_oid: Some(new_oid),
                    ..old_node.clone()
                }],
            )
            .unwrap();
        fixture
            .engine
            .resolver
            .set_generation(2, 2, Some(Arc::clone(&fixture.engine.hydration)), None)
            .unwrap();
        fixture
            .engine
            .classify_unknown_base_node(1, "file.txt", old_node)
            .unwrap();
        assert_eq!(snapshot.get_node(2, "file.txt").unwrap().unwrap().size, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn odb_promotion_uses_verified_bytes_through_copy_completion() {
        let (root, git_dir, oid) = create_git_repo_with_blob(b"abcdefghij");
        let cache = root.path().join("blob_cache");
        let reader = OdbReader::new(&git_dir).unwrap();
        reader.read_blob(&oid).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(&oid), b"XXXXXXXXXX").unwrap();
        let mut fixture = test_engine_with_nodes(vec![BaseNode {
            path: "file.txt".into(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some(oid),
            pointer: None,
            size: 10,
        }]);
        Arc::get_mut(&mut fixture.engine).unwrap().odb_reader = Some(Arc::new(reader));
        fixture.engine.write("file.txt", 3, b"!").await.unwrap();
        assert_eq!(
            fixture
                .engine
                .read("file.txt", 0, 20)
                .await
                .unwrap()
                .as_ref(),
            b"abc!efghij"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_pointer_promotion_discards_output_before_retry() {
        struct Gate {
            first: std::sync::atomic::AtomicBool,
            entered: tokio::sync::Notify,
            release: tokio::sync::Notify,
        }
        #[async_trait::async_trait]
        impl crab_read::XorbAvailability for Gate {
            async fn ensure_available(
                &self,
                _path: &object_store::path::Path,
            ) -> crab_read::Result<()> {
                if self.first.swap(false, Ordering::AcqRel) {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                Ok(())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let stored = crate::test_support::StoredPointer::new(
            &root.path().join("shared"),
            Bytes::from_static(b"remote pointer bytes"),
        )
        .await;
        let gate = Arc::new(Gate {
            first: std::sync::atomic::AtomicBool::new(true),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let hydrator = stored
            .context
            .hydrator
            .as_ref()
            .clone()
            .with_availability(gate.clone());
        let hydration = crate::pipeline::create_hydration(
            Arc::new(crate::ChunkCache::open(root.path().join("chunks"), None).unwrap()),
            Arc::new(VerifiedSet::default()),
            CancellationToken::new(),
            Some(stored.context.store_layout),
            Some(Arc::new(hydrator)),
            None,
            None,
        )
        .unwrap();
        let mut fixture = test_engine_with_nodes(vec![BaseNode {
            path: "file.bin".into(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: None,
            pointer: Some(crate::blob::FilePointer::Crab(stored.pointer.clone())),
            size: stored.pointer.size,
        }]);
        fixture
            .engine
            .resolver
            .set_generation(1, 0, Some(Arc::clone(&hydration)), None)
            .unwrap();
        Arc::get_mut(&mut fixture.engine).unwrap().hydration = hydration;
        let active = Arc::clone(&fixture.engine);
        let write = tokio::spawn(async move { active.write("file.bin", 0, b"R").await });
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        write.abort();
        let _ = write.await;
        let final_path = fixture.overlay.backing_path_for("file.bin");
        assert!(
            std::fs::read_dir(final_path.parent().unwrap())
                .unwrap()
                .next()
                .is_none()
        );
        // Let any abandoned source future finish while a new promotion starts.
        gate.release.notify_one();
        fixture.engine.write("file.bin", 0, b"R").await.unwrap();
        assert_eq!(
            fixture
                .engine
                .read("file.bin", 0, 100)
                .await
                .unwrap()
                .as_ref(),
            b"Remote pointer bytes"
        );
        fixture.engine.shutdown_background().await;
    }
}
