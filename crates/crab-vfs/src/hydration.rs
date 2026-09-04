//! On-demand chunk-level hydration service for the FUSE mount.
//!
//! Manages a priority queue of hydration tasks processed by a configurable
//! worker pool. The key innovation over artifact-fs: hydration operates at
//! chunk granularity (≈64 KiB) rather than whole-blob level, so a `read()`
//! for 1 KB of a 10 GB file fetches only the covering chunks.
//!
//! Workers loop: wait for `work_ready` notification → pop highest-priority
//! task → fetch chunks via Range GET → cache → notify waiters.
//!
//! Inflight dedup ensures concurrent requests for the same chunk share a
//! single network fetch, inspired by artifact-fs's `inflight[T]` pattern.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, trace, warn};

use crate::ChunkCache;
use crate::StoreLayout;
use crate::core::error::{CrabError, Result};
use crate::data_plane::{FileIndexResolver, ReconstructionTerm, ShardLoader, XorbFetcher};
use crate::verified_set::VerifiedSet;
use crab_cache::LocalCache;
use crab_types::pointer::Pointer;
use crab_xet::xorb::format::MerkleHash;

// ---------------------------------------------------------------------------
// Priority levels
// ---------------------------------------------------------------------------

/// Explicit read — user is blocked waiting for data.
pub const PRIORITY_EXPLICIT_READ: u8 = 0;
/// Bootstrap manifests (Cargo.toml, package.json, go.mod, etc.).
pub const PRIORITY_BOOTSTRAP: u8 = 1;
/// Source code files (.rs, .go, .py, .ts, .js, etc.).
pub const PRIORITY_CODE: u8 = 2;
/// Nearby text files (default for unrecognized extensions).
pub const PRIORITY_NEARBY: u8 = 3;
/// Binary / ML model files (.safetensors, .gguf, .bin, .ckpt, etc.).
pub const PRIORITY_BINARY: u8 = 4;

/// Default number of hydration worker tasks.
const DEFAULT_CONCURRENCY: usize = 4;
/// Remote read path reconstructs at most this much extra data per cache miss.
const READ_THROUGH_WINDOW_SIZE: u64 = 8 * 1024 * 1024;
const MAX_READ_WINDOW_PREFETCH_KEYS: usize = 4096;
const MAX_VERIFIED_READ_WINDOW_KEYS: usize = 4096;

// ---------------------------------------------------------------------------
// HydrationTask
// ---------------------------------------------------------------------------

/// A pending hydration task in the priority queue.
#[derive(Debug, Clone)]
pub struct HydrationTask {
    /// Path of the file being hydrated (for logging / prefetch context).
    pub path: String,
    /// Crab pointer for the file.
    pub pointer: Pointer,
    /// Priority level (lower = higher priority).
    pub priority: u8,
    /// Monotonic sequence number for FIFO ordering within the same priority.
    pub seq: u64,
}

impl Eq for HydrationTask {}

impl PartialEq for HydrationTask {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}

impl Ord for HydrationTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Lower priority number = higher urgency → should come first.
        // BinaryHeap is a max-heap, so we reverse the priority comparison.
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for HydrationTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// Inflight entry — notify-based dedup without requiring Clone on errors
// ---------------------------------------------------------------------------

/// Tracks an in-progress chunk fetch. Waiters subscribe to the `done`
/// notification and then re-check the cache. If the fetch failed, the
/// entry is removed from the inflight map and waiters retry or fail.
struct InflightEntry {
    /// Notified once the fetch completes (success or failure).
    done: Notify,
    /// Set to `true` when the fetch succeeded and the chunk is in cache.
    success: Mutex<Option<bool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ReadWindowKey {
    file_hash: [u8; 32],
    start: u64,
    end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadWindow {
    request_start: u64,
    request_end: u64,
    window_start: u64,
    window_end: u64,
}

#[derive(Default)]
struct HydrationReadStats {
    read_range_requests: AtomicU64,
    read_range_requested_bytes: AtomicU64,
    read_range_returned_bytes: AtomicU64,
    read_window_cache_hits: AtomicU64,
    read_window_cache_misses: AtomicU64,
    read_window_inflight_waits: AtomicU64,
    read_window_remote_fetches: AtomicU64,
    read_window_remote_bytes: AtomicU64,
    read_window_prefetch_requests: AtomicU64,
    read_window_prefetch_scheduled: AtomicU64,
    read_window_prefetch_skipped: AtomicU64,
    read_window_prefetch_errors: AtomicU64,
    chunk_cache_hits: AtomicU64,
    chunk_cache_misses: AtomicU64,
    chunk_inflight_waits: AtomicU64,
    chunk_remote_fetches: AtomicU64,
    chunk_remote_bytes: AtomicU64,
}

/// Snapshot of hydration read-path pressure and remote bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HydrationReadStatsSnapshot {
    pub read_range_requests: u64,
    pub read_range_requested_bytes: u64,
    pub read_range_returned_bytes: u64,
    pub read_window_cache_hits: u64,
    pub read_window_cache_misses: u64,
    pub read_window_inflight_waits: u64,
    pub read_window_remote_fetches: u64,
    pub read_window_remote_bytes: u64,
    pub read_window_prefetch_requests: u64,
    pub read_window_prefetch_scheduled: u64,
    pub read_window_prefetch_skipped: u64,
    pub read_window_prefetch_errors: u64,
    pub chunk_cache_hits: u64,
    pub chunk_cache_misses: u64,
    pub chunk_inflight_waits: u64,
    pub chunk_remote_fetches: u64,
    pub chunk_remote_bytes: u64,
}

impl InflightEntry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: Notify::new(),
            success: Mutex::new(None),
        })
    }

    fn complete(&self, ok: bool) {
        if let Ok(mut guard) = self.success.lock() {
            *guard = Some(ok);
        }
        self.done.notify_waiters();
    }

    fn succeeded(&self) -> Option<bool> {
        *self
            .success
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// ---------------------------------------------------------------------------
// HydrationService
// ---------------------------------------------------------------------------

/// On-demand chunk-level hydration service.
///
/// Provides `read_range` for synchronous (priority-0) reads and a background
/// worker pool for prefetch tasks. Inflight dedup via `DashMap` ensures
/// concurrent requests for the same chunk share a single network fetch.
pub struct HydrationService {
    /// Priority queue of pending hydration tasks.
    queue: Mutex<BinaryHeap<HydrationTask>>,
    /// Monotonic counter for FIFO ordering within the same priority.
    seq: Mutex<u64>,
    /// Inflight dedup: chunk_hash → notify entry.
    /// Multiple waiters subscribe to the same notification.
    inflight: DashMap<MerkleHash, Arc<InflightEntry>>,
    /// Local chunk cache (LRU, bounded).
    cache: Arc<ChunkCache>,
    /// Chunk hashes that have passed blake3 verification this session.
    verified: Arc<VerifiedSet>,
    /// Resolves file_hash → shard_hash.
    file_index_resolver: Arc<dyn FileIndexResolver>,
    /// Loads shard → reconstruction terms for a file.
    shard_loader: Arc<dyn ShardLoader>,
    /// Fetches byte ranges from xorbs on object storage.
    xorb_fetcher: Arc<dyn XorbFetcher>,
    /// Shared read-side hydrator for full pointer reconstruction.
    read_hydrator: Option<Arc<crab_read::ShardHydrator>>,
    /// Disk cache for remote range windows reconstructed through read_hydrator.
    read_range_cache_dir: Option<PathBuf>,
    /// Inflight dedup for remote range windows.
    read_range_inflight: DashMap<ReadWindowKey, Arc<AsyncMutex<()>>>,
    /// Read windows whose persisted BLAKE3 sidecar was verified this session.
    read_range_verified: DashMap<ReadWindowKey, ()>,
    /// Read windows already requested speculatively this session.
    read_window_prefetch_seen: DashMap<ReadWindowKey, ()>,
    /// Counters for foreground range reads and cache pressure.
    read_stats: HydrationReadStats,
    /// Per-file reconstruction term cache: file_hash → terms.
    terms_cache: Mutex<HashMap<[u8; 32], Vec<ReconstructionTerm>>>,
    /// Worker count.
    concurrency: usize,
    /// Notify workers of new work.
    work_ready: Arc<Notify>,
    /// Cancellation token for graceful shutdown.
    cancel: CancellationToken,
    /// Guards against double-spawn: workers are started at most once.
    workers_spawned: AtomicBool,
}

impl std::fmt::Debug for HydrationService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let queue_depth = self.queue.lock().map_or(0, |q| q.len());
        f.debug_struct("HydrationService")
            .field("concurrency", &self.concurrency)
            .field("queue_depth", &queue_depth)
            .field("inflight_count", &self.inflight.len())
            .finish_non_exhaustive()
    }
}

impl HydrationService {
    /// Create a new hydration service.
    ///
    /// Call [`spawn_workers`] to start the background worker pool.
    #[expect(clippy::too_many_arguments, reason = "wires VFS dependency graph")]
    pub fn new(
        cache: Arc<ChunkCache>,
        verified: Arc<VerifiedSet>,
        file_index_resolver: Arc<dyn FileIndexResolver>,
        shard_loader: Arc<dyn ShardLoader>,
        xorb_fetcher: Arc<dyn XorbFetcher>,
        read_hydrator: Option<Arc<crab_read::ShardHydrator>>,
        read_range_cache_dir: Option<PathBuf>,
        concurrency: Option<usize>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(BinaryHeap::new()),
            seq: Mutex::new(0),
            inflight: DashMap::new(),
            cache,
            verified,
            file_index_resolver,
            shard_loader,
            xorb_fetcher,
            read_hydrator,
            read_range_cache_dir,
            read_range_inflight: DashMap::new(),
            read_range_verified: DashMap::new(),
            read_window_prefetch_seen: DashMap::new(),
            read_stats: HydrationReadStats::default(),
            terms_cache: Mutex::new(HashMap::new()),
            concurrency: concurrency.unwrap_or(DEFAULT_CONCURRENCY),
            work_ready: Arc::new(Notify::new()),
            cancel,
            workers_spawned: AtomicBool::new(false),
        })
    }

    /// Spawn background worker tasks that process the priority queue.
    ///
    /// Returns `JoinHandle`s for the workers. The caller should hold these
    /// and await them on shutdown.
    pub fn spawn_workers(self: &Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        // Prevent duplicate worker pools: if spawn_workers is called
        // multiple times, only the first invocation creates workers.
        if self
            .workers_spawned
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            debug!("spawn_workers called again; workers already running, skipping");
            return Vec::new();
        }

        let mut handles = Vec::with_capacity(self.concurrency);
        for worker_id in 0..self.concurrency {
            let svc = Arc::clone(self);
            handles.push(tokio::spawn(async move {
                svc.worker_loop(worker_id).await;
            }));
        }
        debug!(workers = self.concurrency, "hydration workers spawned");
        handles
    }

    /// Enqueue a hydration task into the priority queue.
    pub fn enqueue(&self, task: HydrationTask) {
        if let Ok(mut q) = self.queue.lock() {
            trace!(path = %task.path, priority = task.priority, "enqueuing hydration task");
            q.push(task);
        }
        self.work_ready.notify_one();
    }

    /// Current queue depth (for metrics / status reporting).
    pub fn queue_depth(&self) -> usize {
        self.queue.lock().map_or(0, |q| q.len())
    }

    /// Snapshot foreground hydration read counters for mount diagnostics.
    pub fn read_stats_snapshot(&self) -> HydrationReadStatsSnapshot {
        self.read_stats.snapshot()
    }

    /// Number of inflight chunk fetches.
    pub fn inflight_count(&self) -> usize {
        self.inflight.len()
    }

    /// Opportunistically cache the next read-through window after a sequential read.
    pub fn prefetch_next_read_window(
        self: &Arc<Self>,
        pointer: Pointer,
        offset: u64,
        size: u32,
    ) -> bool {
        let window = next_read_window_for_range(pointer.size, offset, size);
        self.prefetch_read_window_for(pointer, window)
    }

    pub fn prefetch_read_window(
        self: &Arc<Self>,
        pointer: Pointer,
        offset: u64,
        size: u32,
    ) -> bool {
        let window = read_window_for_range(pointer.size, offset, size);
        self.prefetch_read_window_for(pointer, window)
    }

    fn prefetch_read_window_for(
        self: &Arc<Self>,
        pointer: Pointer,
        window: Option<ReadWindow>,
    ) -> bool {
        self.read_stats.record_read_window_prefetch_request();

        let Some(hydrator) = self.read_hydrator.clone() else {
            self.read_stats.record_read_window_prefetch_skipped();
            return false;
        };
        let Some(cache_root) = self.read_range_cache_dir.clone() else {
            self.read_stats.record_read_window_prefetch_skipped();
            return false;
        };
        let Some(window) = window else {
            self.read_stats.record_read_window_prefetch_skipped();
            return false;
        };

        let key = ReadWindowKey {
            file_hash: pointer.file_hash,
            start: window.window_start,
            end: window.window_end,
        };
        let cache_path = read_window_cache_path(&cache_root, &key);
        if !self.claim_read_window_prefetch(key) {
            self.read_stats.record_read_window_prefetch_skipped();
            return false;
        }
        let service = Arc::clone(self);
        self.read_stats.record_read_window_prefetch_scheduled();
        tokio::spawn(async move {
            if let Err(error) = service
                .ensure_read_window_cached(&hydrator, &pointer, &key, &cache_path)
                .await
            {
                service.release_failed_read_window_prefetch(key);
                service.read_stats.record_read_window_prefetch_error();
                debug!(
                    error = %error,
                    window_start = key.start,
                    window_end = key.end,
                    "read-window prefetch failed"
                );
            }
        });
        true
    }

    fn claim_read_window_prefetch(&self, key: ReadWindowKey) -> bool {
        if self.read_window_prefetch_seen.len() >= MAX_READ_WINDOW_PREFETCH_KEYS {
            self.read_window_prefetch_seen.clear();
        }

        use dashmap::mapref::entry::Entry;
        match self.read_window_prefetch_seen.entry(key) {
            Entry::Occupied(_) => false,
            Entry::Vacant(vacant) => {
                vacant.insert(());
                true
            }
        }
    }

    fn release_failed_read_window_prefetch(&self, key: ReadWindowKey) {
        self.read_window_prefetch_seen.remove(&key);
    }

    // -----------------------------------------------------------------------
    // read_range — the core read path (priority 0, synchronous)
    // -----------------------------------------------------------------------

    /// Read a byte range from a pointer-tracked file.
    ///
    /// This is the hot path for FUSE `read()` calls. It bypasses the queue
    /// and fetches directly at priority 0 (user is blocked).
    ///
    /// 1. Resolve pointer → shard → reconstruction terms (cached per file_hash)
    /// 2. Map byte range `[offset, offset+size)` to chunk indices
    /// 3. For each chunk: cache → inflight → fetch via Range GET
    /// 4. Assemble requested byte range from chunk data, trim to exact range
    pub async fn read_range(&self, pointer: &Pointer, offset: u64, size: u32) -> Result<Bytes> {
        self.read_stats.record_read_range_request(size);
        let span = tracing::debug_span!(
            "hydration_read_range",
            offset,
            size,
            file_size = pointer.size,
        );

        let result = async move {
            if let Some(hydrator) = &self.read_hydrator {
                return self
                    .read_range_via_hydrator(hydrator, pointer, offset, size)
                    .await;
            }

            let terms = self.resolve_terms(pointer)?;

            if terms.is_empty() {
                return Err(CrabError::NotFound {
                    path: format!(
                        "reconstruction terms for {}",
                        hex_encode(&pointer.file_hash)
                    ),
                });
            }

            // Map byte range to the chunks that cover it.
            let end = offset.saturating_add(u64::from(size)).min(pointer.size);
            let needed = select_chunks_for_range(&terms, offset, end);

            if needed.is_empty() {
                return Ok(Bytes::new());
            }

            // Fetch all needed chunks concurrently (cache → inflight → network).
            // Sequential fetching would serialize network round-trips; concurrent
            // fetching lets multiple Range GETs overlap.
            let fetch_futures: Vec<_> = needed
                .iter()
                .map(|(chunk_offset, term)| {
                    let offset = *chunk_offset;
                    async move {
                        let data = self.fetch_chunk(term).await?;
                        Ok::<_, CrabError>((offset, data))
                    }
                })
                .collect();

            let mut chunk_data: Vec<(u64, Bytes)> = Vec::with_capacity(fetch_futures.len());
            for fut in fetch_futures {
                chunk_data.push(fut.await?);
            }

            // Assemble the requested byte range from chunk data.
            Ok(assemble_range(&chunk_data, offset, end))
        }
        .instrument(span)
        .await;

        if let Ok(bytes) = &result {
            self.read_stats.record_read_range_response(bytes.len());
        }
        result
    }

    async fn read_range_via_hydrator(
        &self,
        hydrator: &crab_read::ShardHydrator,
        pointer: &Pointer,
        offset: u64,
        size: u32,
    ) -> Result<Bytes> {
        let Some(window) = read_window_for_range(pointer.size, offset, size) else {
            return Ok(Bytes::new());
        };

        let Some(cache_root) = &self.read_range_cache_dir else {
            let bytes = hydrator
                .reconstruct_range_from_pointer(
                    &pointer.serialize(),
                    window.request_start,
                    window.request_end,
                )
                .await?;
            self.read_stats.record_read_window_remote_fetch(bytes.len());
            return Ok(Bytes::from(bytes));
        };

        let key = ReadWindowKey {
            file_hash: pointer.file_hash,
            start: window.window_start,
            end: window.window_end,
        };
        let cache_path = read_window_cache_path(cache_root, &key);

        self.ensure_read_window_cached(hydrator, pointer, &key, &cache_path)
            .await?;

        let offset_in_window = window.request_start - window.window_start;
        let request_len = usize::try_from(window.request_end - window.request_start)
            .map_err(|_| CrabError::Internal("read range is too large".into()))?;
        read_cached_window_slice(&cache_path, offset_in_window, request_len).await
    }

    async fn ensure_read_window_cached(
        &self,
        hydrator: &crab_read::ShardHydrator,
        pointer: &Pointer,
        key: &ReadWindowKey,
        cache_path: &Path,
    ) -> Result<()> {
        let expected_len = usize::try_from(key.end - key.start)
            .map_err(|_| CrabError::Internal("read cache window is too large".into()))?;

        if self
            .cached_read_window_complete(key, cache_path, expected_len)
            .await?
        {
            self.read_stats.record_read_window_cache_hit();
            return Ok(());
        }
        self.read_stats.record_read_window_cache_miss();

        use dashmap::mapref::entry::Entry;
        let (lock, joined_inflight) = match self.read_range_inflight.entry(*key) {
            Entry::Occupied(occupied) => (Arc::clone(occupied.get()), true),
            Entry::Vacant(vacant) => {
                let lock = Arc::new(AsyncMutex::new(()));
                vacant.insert(Arc::clone(&lock));
                (lock, false)
            }
        };
        if joined_inflight {
            self.read_stats.record_read_window_inflight_wait();
        }
        let _guard = lock.lock().await;

        let result = async {
            if self
                .cached_read_window_complete(key, cache_path, expected_len)
                .await?
            {
                return Ok(());
            }

            if self.cancel.is_cancelled() {
                return Err(CrabError::Cancelled);
            }

            let bytes = hydrator
                .reconstruct_range_from_pointer(&pointer.serialize(), key.start, key.end)
                .await?;
            self.read_stats.record_read_window_remote_fetch(bytes.len());
            if bytes.len() != expected_len {
                return Err(CrabError::CorruptObject {
                    path: format!("read cache window {}", hex_encode(&key.file_hash)),
                    reason: format!("expected {expected_len} bytes, got {}", bytes.len()),
                });
            }

            write_cached_window(cache_path, &bytes).await?;
            self.mark_read_window_verified(*key);
            Ok(())
        }
        .await;

        self.read_range_inflight.remove(key);
        result
    }

    async fn cached_read_window_complete(
        &self,
        key: &ReadWindowKey,
        cache_path: &Path,
        expected_len: usize,
    ) -> Result<bool> {
        if self.read_range_verified.contains_key(key) {
            if cached_window_length_matches(cache_path, expected_len).await? {
                return Ok(true);
            }
            self.read_range_verified.remove(key);
            return Ok(false);
        }

        if !cached_window_complete(cache_path, expected_len).await? {
            return Ok(false);
        }

        self.mark_read_window_verified(*key);
        Ok(true)
    }

    fn mark_read_window_verified(&self, key: ReadWindowKey) {
        if self.read_range_verified.len() >= MAX_VERIFIED_READ_WINDOW_KEYS {
            self.read_range_verified.clear();
        }
        self.read_range_verified.insert(key, ());
    }

    /// Reconstruct a complete pointer-backed file to `dest`.
    ///
    /// Returns `Ok(None)` when the legacy term-based path must be used.
    pub async fn reconstruct_to_path(&self, pointer: &Pointer, dest: &Path) -> Result<Option<u64>> {
        let Some(hydrator) = &self.read_hydrator else {
            return Ok(None);
        };
        let bytes = hydrator.reconstruct_to_path(pointer, dest).await?;
        Ok(Some(bytes))
    }

    // -----------------------------------------------------------------------
    // Speculative prefetch
    // -----------------------------------------------------------------------

    /// Enqueue file children of a directory for background hydration.
    ///
    /// Called from `opendir` in a background task (non-blocking). Each file
    /// is classified by `classify_priority` and enqueued at that level.
    pub fn prefetch_dir(&self, entries: Vec<(String, Pointer)>) {
        for (path, pointer) in entries {
            let priority = classify_priority(&path);
            let seq = self.next_seq();
            self.enqueue(HydrationTask {
                path,
                pointer,
                priority,
                seq,
            });
        }
    }

    // -----------------------------------------------------------------------
    // Internal: term resolution
    // -----------------------------------------------------------------------

    /// Resolve a pointer to its reconstruction terms, caching the result.
    pub fn resolve_terms(&self, pointer: &Pointer) -> Result<Vec<ReconstructionTerm>> {
        // Check the per-file cache first.
        if let Ok(cache) = self.terms_cache.lock()
            && let Some(terms) = cache.get(&pointer.file_hash)
        {
            return Ok(terms.clone());
        }

        // Resolve shard hash.
        let shard_hash = self.resolve_shard(pointer)?;

        // Load reconstruction terms from the shard.
        let terms = self
            .shard_loader
            .load_reconstruction_terms(&shard_hash, &pointer.file_hash)?;

        if terms.is_empty() {
            return Err(CrabError::NotFound {
                path: format!(
                    "reconstruction terms for {}",
                    hex_encode(&pointer.file_hash)
                ),
            });
        }

        // Cache for subsequent reads of the same file.
        if let Ok(mut cache) = self.terms_cache.lock() {
            cache.insert(pointer.file_hash, terms.clone());
        }

        Ok(terms)
    }

    /// Resolve the shard hash for a pointer (file-index with fallback).
    fn resolve_shard(&self, pointer: &Pointer) -> Result<[u8; 32]> {
        if let Some(shard_hash) = self
            .file_index_resolver
            .resolve_file_index(&pointer.file_hash, pointer.shard_hint.as_ref())?
        {
            return Ok(shard_hash);
        }

        if let Some(shard_hash) = self
            .file_index_resolver
            .scan_shard_list_for_file(&pointer.file_hash)?
        {
            return Ok(shard_hash);
        }

        Err(CrabError::NotFound {
            path: format!("shard for file-hash {}", hex_encode(&pointer.file_hash)),
        })
    }

    // -----------------------------------------------------------------------
    // Internal: chunk fetching with inflight dedup
    // -----------------------------------------------------------------------

    /// Fetch a single chunk: cache → inflight dedup → network.
    ///
    /// Inflight dedup uses `DashMap::entry()` for atomic check-and-insert,
    /// avoiding the TOCTOU race where two threads both see "no inflight"
    /// and both start fetching. The first thread to enter inserts the
    /// `InflightEntry`; subsequent threads find it and wait.
    pub async fn fetch_chunk(&self, term: &ReconstructionTerm) -> Result<Bytes> {
        use dashmap::mapref::entry::Entry;

        let chunk_hash = MerkleHash::from_slice(&term.chunk_hash)
            .map_err(|_| CrabError::Internal("invalid chunk hash length".into()))?;

        // 1. Check cache. If the hash is in the verified set, the cache's
        //    own blake3 check is redundant — the chunk was already verified
        //    during this process lifetime (V8.3). If not in the verified set,
        //    the cache verifies on first read and we record the result (V8.4).
        if let Some(cached) = self.cache.get(&chunk_hash) {
            if !self.verified.contains(&chunk_hash) {
                self.verified.insert(chunk_hash);
            }
            self.read_stats.record_chunk_cache_hit();
            trace!("cache hit for chunk");
            return Ok(cached);
        }

        self.read_stats.record_chunk_cache_miss();
        trace!("cache miss for chunk");

        // 2. Atomic check-and-insert via DashMap::entry().
        //    If another thread is already fetching, we get their entry and wait.
        //    If we're first, we insert a new entry and become the fetcher.
        let is_fetcher;
        let entry = match self.inflight.entry(chunk_hash) {
            Entry::Occupied(occ) => {
                is_fetcher = false;
                self.read_stats.record_chunk_inflight_wait();
                Arc::clone(occ.get())
            }
            Entry::Vacant(vac) => {
                is_fetcher = true;
                let e = InflightEntry::new();
                vac.insert(Arc::clone(&e));
                e
            }
        };

        if !is_fetcher {
            // Wait for the fetcher to complete.
            entry.done.notified().await;

            if entry.succeeded() == Some(true)
                && let Some(cached) = self.cache.get(&chunk_hash)
            {
                return Ok(cached);
            }
            // Fetcher failed — fall through and try ourselves.
            // Re-enter as a new fetcher (rare path).
            let result = self.do_fetch_chunk(term, &chunk_hash);
            return result;
        }

        // 3. We're the fetcher — do the actual network fetch.
        let result = self.do_fetch_chunk(term, &chunk_hash);

        let ok = result.is_ok();
        entry.complete(ok);
        self.inflight.remove(&chunk_hash);

        result
    }

    /// Actually fetch a chunk from the xorb via Range GET, verify, and cache.
    fn do_fetch_chunk(&self, term: &ReconstructionTerm, chunk_hash: &MerkleHash) -> Result<Bytes> {
        let raw = self
            .xorb_fetcher
            .fetch_range(&term.xorb_hash, term.offset..term.offset + term.length)?;
        self.read_stats.record_chunk_remote_fetch(raw.len());

        // Blake3 verification.
        let actual = *blake3::hash(&raw).as_bytes();
        if actual != term.chunk_hash {
            return Err(CrabError::HashMismatch {
                requested: hex_encode(&term.chunk_hash),
                actual: hex_encode(&actual),
            });
        }

        // Record as verified for this process lifetime (V8.2).
        self.verified.insert(*chunk_hash);

        let data = Bytes::from(raw);

        // Store in cache for reuse.
        self.cache.put(*chunk_hash, data.clone());

        Ok(data)
    }

    // -----------------------------------------------------------------------
    // Internal: worker loop
    // -----------------------------------------------------------------------

    /// Worker loop: wait for notification → pop from queue → process.
    async fn worker_loop(&self, worker_id: usize) {
        debug!(worker_id, "hydration worker started");
        loop {
            // Check cancellation between tasks.
            if self.cancel.is_cancelled() {
                debug!(worker_id, "hydration worker cancelled, draining queue");
                self.drain_queue();
                break;
            }

            tokio::select! {
                () = self.cancel.cancelled() => {
                    debug!(worker_id, "hydration worker cancelled");
                    self.drain_queue();
                    break;
                }
                () = self.work_ready.notified() => {
                    // Drain all available work before waiting again.
                    while self.step_one(worker_id) {
                        if self.cancel.is_cancelled() {
                            self.drain_queue();
                            return;
                        }
                        // Re-notify so other workers can help if items remain.
                        self.work_ready.notify_one();
                    }
                }
            }
        }
        debug!(worker_id, "hydration worker exited");
    }

    /// Pop and process one task from the queue. Returns `true` if work was done.
    fn step_one(&self, worker_id: usize) -> bool {
        let task = {
            let Ok(mut q) = self.queue.lock() else {
                return false;
            };
            q.pop()
        };

        let Some(task) = task else {
            return false;
        };

        trace!(
            worker_id,
            path = %task.path,
            priority = task.priority,
            "hydration worker processing task"
        );

        // Resolve and prefetch all chunks for this file.
        if let Err(e) = self.hydrate_file(&task) {
            warn!(
                path = %task.path,
                error = %e,
                "hydration worker: failed to hydrate file"
            );
        }

        true
    }

    /// Hydrate all chunks of a file into the cache (background prefetch path).
    fn hydrate_file(&self, task: &HydrationTask) -> Result<()> {
        if self.read_hydrator.is_some() {
            return Ok(());
        }

        let terms = self.resolve_terms(&task.pointer)?;

        for term in &terms {
            let Ok(chunk_hash) = MerkleHash::from_slice(&term.chunk_hash) else {
                continue;
            };

            // Skip if already cached.
            if self.cache.contains(&chunk_hash) {
                continue;
            }

            // Fetch and cache.
            if let Err(e) = self.do_fetch_chunk(term, &chunk_hash) {
                warn!(
                    chunk_hash = %chunk_hash.hex(),
                    error = %e,
                    "failed to prefetch chunk"
                );
            }
        }

        Ok(())
    }

    /// Drain the queue on cancellation.
    fn drain_queue(&self) {
        if let Ok(mut q) = self.queue.lock() {
            let count = q.len();
            q.clear();
            if count > 0 {
                debug!(drained = count, "hydration queue drained on cancel");
            }
        }
    }

    /// Get the next monotonic sequence number.
    fn next_seq(&self) -> u64 {
        let mut seq = self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *seq += 1;
        *seq
    }
}

impl HydrationReadStats {
    fn record_read_range_request(&self, requested_bytes: u32) {
        self.read_range_requests
            .fetch_add(1, AtomicOrdering::Relaxed);
        self.read_range_requested_bytes
            .fetch_add(u64::from(requested_bytes), AtomicOrdering::Relaxed);
    }

    fn record_read_range_response(&self, returned_bytes: usize) {
        self.read_range_returned_bytes
            .fetch_add(returned_bytes as u64, AtomicOrdering::Relaxed);
    }

    fn record_read_window_cache_hit(&self) {
        self.read_window_cache_hits
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_read_window_cache_miss(&self) {
        self.read_window_cache_misses
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_read_window_inflight_wait(&self) {
        self.read_window_inflight_waits
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_read_window_remote_fetch(&self, bytes: usize) {
        self.read_window_remote_fetches
            .fetch_add(1, AtomicOrdering::Relaxed);
        self.read_window_remote_bytes
            .fetch_add(bytes as u64, AtomicOrdering::Relaxed);
    }

    fn record_read_window_prefetch_request(&self) {
        self.read_window_prefetch_requests
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_read_window_prefetch_scheduled(&self) {
        self.read_window_prefetch_scheduled
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_read_window_prefetch_skipped(&self) {
        self.read_window_prefetch_skipped
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_read_window_prefetch_error(&self) {
        self.read_window_prefetch_errors
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_chunk_cache_hit(&self) {
        self.chunk_cache_hits.fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_chunk_cache_miss(&self) {
        self.chunk_cache_misses
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_chunk_inflight_wait(&self) {
        self.chunk_inflight_waits
            .fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn record_chunk_remote_fetch(&self, bytes: usize) {
        self.chunk_remote_fetches
            .fetch_add(1, AtomicOrdering::Relaxed);
        self.chunk_remote_bytes
            .fetch_add(bytes as u64, AtomicOrdering::Relaxed);
    }

    fn snapshot(&self) -> HydrationReadStatsSnapshot {
        HydrationReadStatsSnapshot {
            read_range_requests: self.read_range_requests.load(AtomicOrdering::Relaxed),
            read_range_requested_bytes: self
                .read_range_requested_bytes
                .load(AtomicOrdering::Relaxed),
            read_range_returned_bytes: self.read_range_returned_bytes.load(AtomicOrdering::Relaxed),
            read_window_cache_hits: self.read_window_cache_hits.load(AtomicOrdering::Relaxed),
            read_window_cache_misses: self.read_window_cache_misses.load(AtomicOrdering::Relaxed),
            read_window_inflight_waits: self
                .read_window_inflight_waits
                .load(AtomicOrdering::Relaxed),
            read_window_remote_fetches: self
                .read_window_remote_fetches
                .load(AtomicOrdering::Relaxed),
            read_window_remote_bytes: self.read_window_remote_bytes.load(AtomicOrdering::Relaxed),
            read_window_prefetch_requests: self
                .read_window_prefetch_requests
                .load(AtomicOrdering::Relaxed),
            read_window_prefetch_scheduled: self
                .read_window_prefetch_scheduled
                .load(AtomicOrdering::Relaxed),
            read_window_prefetch_skipped: self
                .read_window_prefetch_skipped
                .load(AtomicOrdering::Relaxed),
            read_window_prefetch_errors: self
                .read_window_prefetch_errors
                .load(AtomicOrdering::Relaxed),
            chunk_cache_hits: self.chunk_cache_hits.load(AtomicOrdering::Relaxed),
            chunk_cache_misses: self.chunk_cache_misses.load(AtomicOrdering::Relaxed),
            chunk_inflight_waits: self.chunk_inflight_waits.load(AtomicOrdering::Relaxed),
            chunk_remote_fetches: self.chunk_remote_fetches.load(AtomicOrdering::Relaxed),
            chunk_remote_bytes: self.chunk_remote_bytes.load(AtomicOrdering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// Store-backed trait implementations for global prefix routing
// ---------------------------------------------------------------------------

/// Xorb fetcher that reads byte ranges from the object store via
/// `StoreLayout::xorb_path()`, routing to the global `.crab/xorbs/` prefix.
///
/// Uses `tokio::task::block_in_place` to bridge the sync `XorbFetcher` trait
/// with the async `Store::range_get` call. This is safe because the VFS
/// hydration workers run on a multi-threaded tokio runtime.
pub struct StoreBackedXorbFetcher {
    router: StoreLayout,
    rt: tokio::runtime::Handle,
    cache: Arc<LocalCache>,
}

impl StoreBackedXorbFetcher {
    /// Create a new store-backed xorb fetcher.
    pub fn new(router: StoreLayout, rt: tokio::runtime::Handle) -> Self {
        Self::with_cache(
            router,
            rt,
            Arc::new(LocalCache::new(crab_cache::default_cache_root())),
        )
    }

    /// Create a store-backed xorb fetcher with an explicit local cache.
    #[must_use]
    pub fn with_cache(
        router: StoreLayout,
        rt: tokio::runtime::Handle,
        cache: Arc<LocalCache>,
    ) -> Self {
        Self { router, rt, cache }
    }
}

impl XorbFetcher for StoreBackedXorbFetcher {
    fn fetch_range(&self, xorb_hash: &[u8; 32], range: std::ops::Range<u64>) -> Result<Vec<u8>> {
        let hash = MerkleHash::from(*xorb_hash);
        let path = self.router.xorb_path(&hash);
        let store = self.router.store().clone();
        let cache = Arc::clone(&self.cache);

        let data = tokio::task::block_in_place(|| {
            self.rt.block_on(async move {
                if let Some(bytes) = cache.get_xorb_range_if_present(&hash, range.clone()).await {
                    return Ok::<Vec<u8>, CrabError>(bytes.to_vec());
                }

                let data = store.range_get(&path, range).await?;
                Ok::<Vec<u8>, CrabError>(data.to_vec())
            })
        })?;

        Ok(data)
    }
}

// ---------------------------------------------------------------------------
// Priority classification
// ---------------------------------------------------------------------------

/// Classify a file path into a hydration priority level.
///
/// Lower number = higher priority (fetched first).
///
/// - 0: explicit read (assigned directly, not by this function)
/// - 1: bootstrap manifests (Cargo.toml, package.json, etc.)
/// - 2: code files (.rs, .go, .py, .ts, .js, etc.)
/// - 3: nearby text files (default)
/// - 4: binary / ML model files (.safetensors, .gguf, .bin, .ckpt, etc.)
pub fn classify_priority(path: &str) -> u8 {
    let file_path = Path::new(path);
    let base = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

    if is_manifest(base) {
        return PRIORITY_BOOTSTRAP;
    }
    if is_code_extension(ext) {
        return PRIORITY_CODE;
    }
    if is_ml_binary(ext) {
        return PRIORITY_BINARY;
    }

    PRIORITY_NEARBY
}

fn is_manifest(name: &str) -> bool {
    matches!(
        name,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "go.mod"
            | "go.sum"
            | "pyproject.toml"
            | "requirements.txt"
            | "setup.py"
            | "Makefile"
            | "CMakeLists.txt"
            | "README.md"
            | "README"
            | "LICENSE"
            | ".gitignore"
            | ".gitattributes"
    )
}

fn is_code_extension(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "go"
            | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "java"
            | "c"
            | "cc"
            | "cpp"
            | "h"
            | "hpp"
            | "zig"
            | "rb"
            | "swift"
            | "kt"
            | "scala"
            | "sh"
            | "bash"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "md"
            | "html"
            | "css"
            | "scss"
            | "sql"
    )
}

fn is_ml_binary(ext: &str) -> bool {
    matches!(
        ext,
        "safetensors"
            | "gguf"
            | "bin"
            | "ckpt"
            | "pt"
            | "pth"
            | "h5"
            | "pb"
            | "onnx"
            | "parquet"
            | "tar"
            | "gz"
            | "zip"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "mp4"
            | "mov"
            | "avi"
            | "pdf"
    )
}

// ---------------------------------------------------------------------------
// Chunk range selection
// ---------------------------------------------------------------------------

/// Given reconstruction terms (ordered chunks composing the file) and a byte
/// range `[start, end)`, return the terms whose chunks overlap the range,
/// along with each chunk's starting byte offset within the file.
fn select_chunks_for_range(
    terms: &[ReconstructionTerm],
    start: u64,
    end: u64,
) -> Vec<(u64, ReconstructionTerm)> {
    if start >= end {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut file_offset: u64 = 0;

    for term in terms {
        let chunk_start = file_offset;
        let chunk_end = file_offset + term.length;

        // Does this chunk overlap [start, end)?
        if chunk_end > start && chunk_start < end {
            result.push((chunk_start, term.clone()));
        }

        // Past the requested range — no need to continue.
        if chunk_start >= end {
            break;
        }

        file_offset = chunk_end;
    }

    result
}

/// Assemble the requested byte range from fetched chunk data.
///
/// Each entry in `chunk_data` is `(chunk_file_offset, chunk_bytes)`.
/// Returns exactly the bytes in `[start, end)`.
fn assemble_range(chunk_data: &[(u64, Bytes)], start: u64, end: u64) -> Bytes {
    let total = (end - start) as usize;
    let mut buf = BytesMut::with_capacity(total);

    for (chunk_offset, data) in chunk_data {
        let chunk_start = *chunk_offset;
        let chunk_end = chunk_start + data.len() as u64;

        // Compute the overlap between this chunk and [start, end).
        let overlap_start = start.max(chunk_start);
        let overlap_end = end.min(chunk_end);

        if overlap_start >= overlap_end {
            continue;
        }

        let local_start = (overlap_start - chunk_start) as usize;
        let local_end = (overlap_end - chunk_start) as usize;
        buf.extend_from_slice(&data[local_start..local_end]);
    }

    buf.freeze()
}

fn read_window_for_range(file_size: u64, offset: u64, size: u32) -> Option<ReadWindow> {
    if size == 0 || offset >= file_size {
        return None;
    }

    let request_end = offset.saturating_add(u64::from(size)).min(file_size);
    if offset >= request_end {
        return None;
    }

    let window_start = (offset / READ_THROUGH_WINDOW_SIZE) * READ_THROUGH_WINDOW_SIZE;
    let default_window_end = window_start.saturating_add(READ_THROUGH_WINDOW_SIZE);
    let window_end = default_window_end.max(request_end).min(file_size);

    Some(ReadWindow {
        request_start: offset,
        request_end,
        window_start,
        window_end,
    })
}

fn next_read_window_for_range(file_size: u64, offset: u64, size: u32) -> Option<ReadWindow> {
    let current = read_window_for_range(file_size, offset, size)?;
    read_window_for_range(file_size, current.window_end, size.max(1))
}

fn read_window_cache_path(root: &Path, key: &ReadWindowKey) -> PathBuf {
    let file_hash = hex_encode(&key.file_hash);
    root.join(&file_hash[..2])
        .join(file_hash)
        .join(format!("{}-{}.bin", key.start, key.end))
}

async fn cached_window_complete(path: &Path, expected_len: usize) -> Result<bool> {
    if !cached_window_length_matches(path, expected_len).await? {
        return Ok(false);
    }

    let path = path.to_owned();
    tokio::task::spawn_blocking(move || verify_cached_window(&path))
        .await
        .map_err(|error| CrabError::Internal(format!("read cache verification failed: {error}")))?
}

async fn cached_window_length_matches(path: &Path, expected_len: usize) -> Result<bool> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.len() == expected_len as u64),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn verify_cached_window(path: &Path) -> Result<bool> {
    use std::io::Read;

    let digest_path = read_window_digest_path(path);
    let expected = match std::fs::read(digest_path) {
        Ok(expected) if expected.len() == blake3::OUT_LEN => expected,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };

    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize().as_bytes() == expected.as_slice())
}

async fn read_cached_window_slice(path: &Path, offset: u64, len: usize) -> Result<Bytes> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(SeekFrom::Start(offset)).await?;
    let mut buf = vec![0; len];
    file.read_exact(&mut buf).await?;
    Ok(Bytes::from(buf))
}

async fn write_cached_window(path: &Path, bytes: &[u8]) -> Result<()> {
    let path = path.to_owned();
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || write_cached_window_blocking(&path, &bytes))
        .await
        .map_err(|error| CrabError::Internal(format!("read cache write failed: {error}")))?
}

fn write_cached_window_blocking(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;

    let mut data = tempfile::NamedTempFile::new_in(parent)?;
    data.write_all(bytes)?;
    data.flush()?;

    let digest_path = read_window_digest_path(path);
    let mut digest = tempfile::NamedTempFile::new_in(parent)?;
    digest.write_all(blake3::hash(bytes).as_bytes())?;
    digest.flush()?;

    data.persist(path).map_err(|error| error.error)?;
    digest.persist(digest_path).map_err(|error| error.error)?;
    Ok(())
}

fn read_window_digest_path(path: &Path) -> PathBuf {
    path.with_extension("blake3")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encode a 32-byte hash as lowercase hex.
fn hex_encode(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crab_cache::CacheKey;
    use crab_storage::Store;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;
    use object_store::memory::InMemory;

    #[tokio::test(flavor = "multi_thread")]
    async fn canonical_ranges_survive_unavailable_chunk_storage_and_reuse_warm_bytes() {
        use crate::test_support::StoredPointer;

        let content = Bytes::from(
            (0..READ_THROUGH_WINDOW_SIZE as usize + 17)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>(),
        );
        for window_cache_enabled in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let stored =
                StoredPointer::new(&tmp.path().join("shared-cache"), content.clone()).await;
            let unavailable = tmp.path().join("unavailable");
            std::fs::write(&unavailable, b"preserve me").unwrap();
            let chunks =
                Arc::new(ChunkCache::open(unavailable.join("chunks"), Some(1024)).unwrap());
            let window_root = window_cache_enabled.then(|| tmp.path().join("windows"));
            let service = crate::pipeline::create_hydration(
                chunks,
                Arc::new(VerifiedSet::new(16)),
                CancellationToken::new(),
                Some(stored.context.store_layout.clone()),
                Some(stored.context.hydrator.clone()),
                window_root,
            )
            .unwrap();

            assert_eq!(
                service.read_range(&stored.pointer, 0, 1).await.unwrap(),
                content.slice(..1)
            );
            assert_eq!(stored.xorb_body_requests(), 1);
            stored.origin.block_body_reads_for(&stored.xorb_path);

            for (offset, size) in [
                (7, 17),
                (READ_THROUGH_WINDOW_SIZE - 4, 32),
                (0, content.len() as u32 + 7),
                (stored.pointer.size, 20),
            ] {
                let end = (offset + u64::from(size)).min(stored.pointer.size);
                let bytes = service
                    .read_range(&stored.pointer, offset, size)
                    .await
                    .unwrap();
                assert_eq!(bytes, content.slice(offset as usize..end as usize));
            }
            assert_eq!(stored.xorb_body_requests(), 1);
            assert_eq!(std::fs::read(&unavailable).unwrap(), b"preserve me");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn store_backed_xorb_fetcher_reads_warmed_local_xorb_range() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(cache_dir.path().join("cache")));
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repo".to_owned());

        let mut builder = XorbBuilder::new();
        builder
            .push(
                &Chunk::new(Bytes::from_static(b"cached range payload")),
                RunId(0),
            )
            .unwrap();
        let xorb = builder.finalize().unwrap().pop().unwrap();
        let hash = xorb.hash;
        let hash_bytes: [u8; 32] = hash.into();
        cache
            .put(&CacheKey::Xorb(hash), xorb.bytes.as_ref())
            .await
            .unwrap();

        let fetcher =
            StoreBackedXorbFetcher::with_cache(router, tokio::runtime::Handle::current(), cache);
        let data = fetcher.fetch_range(&hash_bytes, 2..7).unwrap();
        assert_eq!(data, xorb.bytes.slice(2..7).to_vec());
    }

    // --- classify_priority tests ---

    #[test]
    fn priority_explicit_read_is_zero() {
        assert_eq!(PRIORITY_EXPLICIT_READ, 0);
    }

    #[test]
    fn priority_bootstrap_manifests() {
        assert_eq!(classify_priority("Cargo.toml"), PRIORITY_BOOTSTRAP);
        assert_eq!(classify_priority("package.json"), PRIORITY_BOOTSTRAP);
        assert_eq!(classify_priority("go.mod"), PRIORITY_BOOTSTRAP);
        assert_eq!(classify_priority("pyproject.toml"), PRIORITY_BOOTSTRAP);
        assert_eq!(classify_priority("README.md"), PRIORITY_BOOTSTRAP);
        assert_eq!(classify_priority("Makefile"), PRIORITY_BOOTSTRAP);
        assert_eq!(classify_priority(".gitignore"), PRIORITY_BOOTSTRAP);
        assert_eq!(classify_priority("LICENSE"), PRIORITY_BOOTSTRAP);
    }

    #[test]
    fn priority_code_files() {
        assert_eq!(classify_priority("src/main.rs"), PRIORITY_CODE);
        assert_eq!(classify_priority("lib/utils.go"), PRIORITY_CODE);
        assert_eq!(classify_priority("app.py"), PRIORITY_CODE);
        assert_eq!(classify_priority("index.ts"), PRIORITY_CODE);
        assert_eq!(classify_priority("app.js"), PRIORITY_CODE);
    }

    #[test]
    fn priority_binary_ml_files() {
        assert_eq!(classify_priority("model.safetensors"), PRIORITY_BINARY);
        assert_eq!(classify_priority("weights.gguf"), PRIORITY_BINARY);
        assert_eq!(classify_priority("data.bin"), PRIORITY_BINARY);
        assert_eq!(classify_priority("checkpoint.ckpt"), PRIORITY_BINARY);
        assert_eq!(classify_priority("model.pt"), PRIORITY_BINARY);
        assert_eq!(classify_priority("data.parquet"), PRIORITY_BINARY);
    }

    #[test]
    fn priority_nearby_text_default() {
        assert_eq!(classify_priority("notes.txt"), PRIORITY_NEARBY);
        assert_eq!(classify_priority("data.csv"), PRIORITY_NEARBY);
        assert_eq!(classify_priority("unknown_file"), PRIORITY_NEARBY);
    }

    #[test]
    fn priority_nested_paths() {
        assert_eq!(
            classify_priority("deep/nested/Cargo.toml"),
            PRIORITY_BOOTSTRAP
        );
        assert_eq!(classify_priority("src/lib/mod.rs"), PRIORITY_CODE);
        assert_eq!(classify_priority("models/big.safetensors"), PRIORITY_BINARY);
    }

    // --- HydrationTask ordering tests ---

    #[test]
    fn task_ordering_lower_priority_number_first() {
        let mut heap = BinaryHeap::new();

        heap.push(HydrationTask {
            path: "binary.bin".into(),
            pointer: test_pointer(),
            priority: PRIORITY_BINARY,
            seq: 1,
        });
        heap.push(HydrationTask {
            path: "main.rs".into(),
            pointer: test_pointer(),
            priority: PRIORITY_CODE,
            seq: 2,
        });
        heap.push(HydrationTask {
            path: "read.rs".into(),
            pointer: test_pointer(),
            priority: PRIORITY_EXPLICIT_READ,
            seq: 3,
        });
        heap.push(HydrationTask {
            path: "Cargo.toml".into(),
            pointer: test_pointer(),
            priority: PRIORITY_BOOTSTRAP,
            seq: 4,
        });

        let first = heap.pop().unwrap();
        assert_eq!(first.priority, PRIORITY_EXPLICIT_READ);

        let second = heap.pop().unwrap();
        assert_eq!(second.priority, PRIORITY_BOOTSTRAP);

        let third = heap.pop().unwrap();
        assert_eq!(third.priority, PRIORITY_CODE);

        let fourth = heap.pop().unwrap();
        assert_eq!(fourth.priority, PRIORITY_BINARY);
    }

    #[test]
    fn task_ordering_fifo_within_same_priority() {
        let mut heap = BinaryHeap::new();

        heap.push(HydrationTask {
            path: "a.rs".into(),
            pointer: test_pointer(),
            priority: PRIORITY_CODE,
            seq: 1,
        });
        heap.push(HydrationTask {
            path: "b.rs".into(),
            pointer: test_pointer(),
            priority: PRIORITY_CODE,
            seq: 2,
        });
        heap.push(HydrationTask {
            path: "c.rs".into(),
            pointer: test_pointer(),
            priority: PRIORITY_CODE,
            seq: 3,
        });

        assert_eq!(heap.pop().unwrap().path, "a.rs");
        assert_eq!(heap.pop().unwrap().path, "b.rs");
        assert_eq!(heap.pop().unwrap().path, "c.rs");
    }

    // --- select_chunks_for_range tests ---

    #[test]
    fn select_chunks_single_chunk_covers_range() {
        let terms = vec![make_term(0, 1000, [0x01; 32])];
        let selected = select_chunks_for_range(&terms, 100, 500);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].0, 0); // chunk starts at file offset 0
    }

    #[test]
    fn select_chunks_spans_multiple() {
        let terms = vec![
            make_term(0, 1000, [0x01; 32]),
            make_term(0, 1000, [0x02; 32]),
            make_term(0, 1000, [0x03; 32]),
        ];
        // Range 500..2500 spans all three chunks.
        let selected = select_chunks_for_range(&terms, 500, 2500);
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn select_chunks_skips_non_overlapping() {
        let terms = vec![
            make_term(0, 1000, [0x01; 32]),
            make_term(0, 1000, [0x02; 32]),
            make_term(0, 1000, [0x03; 32]),
        ];
        // Range 1000..2000 only needs the second chunk.
        let selected = select_chunks_for_range(&terms, 1000, 2000);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].0, 1000);
    }

    #[test]
    fn select_chunks_empty_range() {
        let terms = vec![make_term(0, 1000, [0x01; 32])];
        let selected = select_chunks_for_range(&terms, 500, 500);
        assert!(selected.is_empty());
    }

    // --- assemble_range tests ---

    #[test]
    fn assemble_single_chunk_partial() {
        let data = Bytes::from(vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let chunks = vec![(0u64, data)];
        let result = assemble_range(&chunks, 3, 7);
        assert_eq!(&result[..], &[3, 4, 5, 6]);
    }

    #[test]
    fn assemble_multi_chunk() {
        let c1 = Bytes::from(vec![0u8, 1, 2, 3, 4]);
        let c2 = Bytes::from(vec![5u8, 6, 7, 8, 9]);
        let chunks = vec![(0u64, c1), (5u64, c2)];
        let result = assemble_range(&chunks, 2, 8);
        assert_eq!(&result[..], &[2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn assemble_exact_chunk_boundaries() {
        let c1 = Bytes::from(vec![10u8, 20, 30]);
        let c2 = Bytes::from(vec![40u8, 50, 60]);
        let chunks = vec![(0u64, c1), (3u64, c2)];
        let result = assemble_range(&chunks, 0, 6);
        assert_eq!(&result[..], &[10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn read_window_empty_for_zero_size_or_eof() {
        assert_eq!(read_window_for_range(1024, 0, 0), None);
        assert_eq!(read_window_for_range(1024, 1024, 1), None);
    }

    #[test]
    fn read_window_aligns_to_cache_window() {
        let window = read_window_for_range(
            READ_THROUGH_WINDOW_SIZE * 3,
            READ_THROUGH_WINDOW_SIZE + 128,
            4096,
        )
        .unwrap();

        assert_eq!(window.request_start, READ_THROUGH_WINDOW_SIZE + 128);
        assert_eq!(window.request_end, READ_THROUGH_WINDOW_SIZE + 4224);
        assert_eq!(window.window_start, READ_THROUGH_WINDOW_SIZE);
        assert_eq!(window.window_end, READ_THROUGH_WINDOW_SIZE * 2);
    }

    #[test]
    fn read_window_truncates_last_window_at_file_size() {
        let file_size = READ_THROUGH_WINDOW_SIZE + 512;
        let window =
            read_window_for_range(file_size, READ_THROUGH_WINDOW_SIZE + 128, 4096).unwrap();

        assert_eq!(window.request_start, READ_THROUGH_WINDOW_SIZE + 128);
        assert_eq!(window.request_end, file_size);
        assert_eq!(window.window_start, READ_THROUGH_WINDOW_SIZE);
        assert_eq!(window.window_end, file_size);
    }

    #[test]
    fn read_window_expands_when_request_exceeds_default_window() {
        let request_size = u32::try_from(READ_THROUGH_WINDOW_SIZE + 4096).unwrap();
        let window = read_window_for_range(READ_THROUGH_WINDOW_SIZE * 3, 0, request_size).unwrap();

        assert_eq!(window.request_start, 0);
        assert_eq!(window.request_end, READ_THROUGH_WINDOW_SIZE + 4096);
        assert_eq!(window.window_start, 0);
        assert_eq!(window.window_end, READ_THROUGH_WINDOW_SIZE + 4096);
    }

    #[test]
    fn next_read_window_advances_after_current_cache_window() {
        let window = next_read_window_for_range(READ_THROUGH_WINDOW_SIZE * 3, 128, 4096).unwrap();

        assert_eq!(window.request_start, READ_THROUGH_WINDOW_SIZE);
        assert_eq!(window.request_end, READ_THROUGH_WINDOW_SIZE + 4096);
        assert_eq!(window.window_start, READ_THROUGH_WINDOW_SIZE);
        assert_eq!(window.window_end, READ_THROUGH_WINDOW_SIZE * 2);
    }

    #[test]
    fn next_read_window_stops_at_eof() {
        assert_eq!(
            next_read_window_for_range(
                READ_THROUGH_WINDOW_SIZE * 2,
                READ_THROUGH_WINDOW_SIZE + 128,
                4096
            ),
            None
        );
    }

    #[tokio::test]
    async fn cached_window_rejects_same_length_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window.bin");
        write_cached_window(&path, b"original").await.unwrap();
        assert!(cached_window_complete(&path, 8).await.unwrap());

        tokio::fs::write(&path, b"corrupt!").await.unwrap();

        assert!(!cached_window_complete(&path, 8).await.unwrap());
    }

    #[test]
    fn hydration_read_stats_snapshot_counts_cache_and_remote_pressure() {
        let stats = HydrationReadStats::default();

        stats.record_read_range_request(4096);
        stats.record_read_range_response(1024);
        stats.record_read_window_cache_hit();
        stats.record_read_window_cache_miss();
        stats.record_read_window_inflight_wait();
        stats.record_read_window_remote_fetch(8 * 1024 * 1024);
        stats.record_read_window_prefetch_request();
        stats.record_read_window_prefetch_scheduled();
        stats.record_read_window_prefetch_skipped();
        stats.record_read_window_prefetch_error();
        stats.record_chunk_cache_hit();
        stats.record_chunk_cache_miss();
        stats.record_chunk_inflight_wait();
        stats.record_chunk_remote_fetch(65_536);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.read_range_requests, 1);
        assert_eq!(snapshot.read_range_requested_bytes, 4096);
        assert_eq!(snapshot.read_range_returned_bytes, 1024);
        assert_eq!(snapshot.read_window_cache_hits, 1);
        assert_eq!(snapshot.read_window_cache_misses, 1);
        assert_eq!(snapshot.read_window_inflight_waits, 1);
        assert_eq!(snapshot.read_window_remote_bytes, 8 * 1024 * 1024);
        assert_eq!(snapshot.read_window_prefetch_requests, 1);
        assert_eq!(snapshot.read_window_prefetch_scheduled, 1);
        assert_eq!(snapshot.read_window_prefetch_skipped, 1);
        assert_eq!(snapshot.read_window_prefetch_errors, 1);
        assert_eq!(snapshot.chunk_cache_hits, 1);
        assert_eq!(snapshot.chunk_cache_misses, 1);
        assert_eq!(snapshot.chunk_inflight_waits, 1);
        assert_eq!(snapshot.chunk_remote_bytes, 65_536);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prefetch_next_read_window_skips_when_window_cache_is_unavailable() {
        let (_cache_dir, service) = test_service_without_read_window_cache();
        let mut pointer = test_pointer();
        pointer.size = READ_THROUGH_WINDOW_SIZE * 2;

        assert!(!service.prefetch_next_read_window(pointer, 0, 4096));

        let snapshot = service.read_stats_snapshot();
        assert_eq!(snapshot.read_window_prefetch_requests, 1);
        assert_eq!(snapshot.read_window_prefetch_scheduled, 0);
        assert_eq!(snapshot.read_window_prefetch_skipped, 1);
    }

    #[test]
    fn read_window_prefetch_claims_each_window_once_until_failure() {
        let (_cache_dir, service) = test_service_without_read_window_cache();
        let key = ReadWindowKey {
            file_hash: [0xAB; 32],
            start: READ_THROUGH_WINDOW_SIZE,
            end: READ_THROUGH_WINDOW_SIZE * 2,
        };

        assert!(service.claim_read_window_prefetch(key));
        assert!(!service.claim_read_window_prefetch(key));

        service.release_failed_read_window_prefetch(key);

        assert!(service.claim_read_window_prefetch(key));
    }

    #[test]
    fn read_window_prefetch_claims_compact_when_key_set_fills() {
        let (_cache_dir, service) = test_service_without_read_window_cache();

        for i in 0..=MAX_READ_WINDOW_PREFETCH_KEYS {
            let key = ReadWindowKey {
                file_hash: [0xCD; 32],
                start: i as u64 * READ_THROUGH_WINDOW_SIZE,
                end: (i as u64 + 1) * READ_THROUGH_WINDOW_SIZE,
            };
            assert!(service.claim_read_window_prefetch(key));
        }

        assert_eq!(service.read_window_prefetch_seen.len(), 1);
    }

    // --- helpers ---

    fn test_pointer() -> Pointer {
        Pointer {
            file_hash: [0xAA; 32],
            size: 4096,
            shard_hint: None,
        }
    }

    fn test_service_without_read_window_cache() -> (tempfile::TempDir, Arc<HydrationService>) {
        let cache_dir = tempfile::tempdir().unwrap();
        let service = HydrationService::new(
            Arc::new(
                ChunkCache::open(cache_dir.path().join("cache/chunks"), Some(1024 * 1024)).unwrap(),
            ),
            Arc::new(VerifiedSet::new(16)),
            Arc::new(NoopFileIndexResolver),
            Arc::new(NoopShardLoader),
            Arc::new(NoopXorbFetcher),
            None,
            None,
            Some(1),
            CancellationToken::new(),
        );
        (cache_dir, service)
    }

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
        fn fetch_range(
            &self,
            _xorb_hash: &[u8; 32],
            _range: std::ops::Range<u64>,
        ) -> Result<Vec<u8>> {
            Err(CrabError::NotFound {
                path: "noop xorb fetcher".into(),
            })
        }
    }

    fn make_term(xorb_offset: u64, length: u64, chunk_hash: [u8; 32]) -> ReconstructionTerm {
        ReconstructionTerm {
            xorb_hash: [0xBB; 32],
            offset: xorb_offset,
            length,
            chunk_hash,
        }
    }
}
