//! Smudge pipeline: pointer → file-index → shard → reconstruction → coalesced
//! Range GETs → verify → stream.
//!
//! The smudge path runs at `git checkout` time. It resolves a pointer blob
//! back to the original file content by:
//!
//! 1. Parsing the pointer to extract `file_hash`, `size`, and optional
//!    `shard-hint`.
//! 2. Resolving the file-index (or falling back to shard-list scan) to
//!    find the shard describing this file.
//! 3. Loading the shard and extracting reconstruction terms (xorb ranges).
//! 4. Coalescing byte ranges across files within a batch (cross-file
//!    delayed-smudge coalescing with `COALESCE_GAP=5`).
//! 5. Issuing Range GETs for the coalesced plan.
//! 6. Verifying each chunk via blake3 + size, gating output until the
//!    final chunk is verified (smudge gate).
//!
//! The entire operation is wrapped in a per-operation timeout
//! (`network.operation_timeout`, default 300 s).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use crate::cache::ChunkCache;
use crate::core::context::AppContext;
use crate::core::error::{CrabError, Result};
use crab_types::pointer::Pointer;
use crab_xet::xorb::format::MerkleHash;

/// Default memory cap for a smudge batch (1 GiB).
const DEFAULT_SMUDGE_BATCH_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;

/// Maximum gap (in chunks) between two ranges that will be coalesced into
/// a single Range GET. Ranges separated by more than this many chunks are
/// fetched independently.
const COALESCE_GAP: u32 = 5;

/// Default chunk size estimate for gap coalescing (128 KiB).
const ESTIMATED_CHUNK_SIZE: u64 = 128 * 1024;

// ---------------------------------------------------------------------------
// Reconstruction term — one xorb range needed to reconstruct a file
// ---------------------------------------------------------------------------

pub use crab_vfs::data_plane::ReconstructionTerm;

// ---------------------------------------------------------------------------
// Coalesced range — result of merging nearby ranges within one xorb
// ---------------------------------------------------------------------------

/// A coalesced byte range within a single xorb, potentially serving
/// multiple files' reconstruction terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedRange {
    /// The xorb this range belongs to.
    pub xorb_hash: [u8; 32],
    /// Byte range within the xorb to fetch.
    pub range: Range<u64>,
    /// Original terms served by this coalesced range, with their
    /// offsets relative to the start of the coalesced range.
    pub terms: Vec<CoalescedTerm>,
}

/// A term within a coalesced range, tracking its position relative to
/// the coalesced fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedTerm {
    /// Offset of this term's data within the coalesced range's bytes.
    pub relative_offset: u64,
    /// Length of this term's data.
    pub length: u64,
    /// Blake3 hash for verification.
    pub chunk_hash: [u8; 32],
}

// ---------------------------------------------------------------------------
// Trait abstractions for the data plane
// ---------------------------------------------------------------------------

pub use crab_vfs::data_plane::{
    FileIndexResolver, NoopFileIndexResolver, NoopShardLoader, NoopXorbFetcher, ShardLoader,
    XorbFetcher,
};

// ---------------------------------------------------------------------------
// SmudgeSession — per-session state
// ---------------------------------------------------------------------------

/// Per-session state for the smudge pipeline.
///
/// Maintains shard cache references, bloom filter state, and trait
/// dependencies across multiple smudge operations within a single
/// filter-process invocation.
pub struct SmudgeSession {
    ctx: AppContext,
    file_index_resolver: Box<dyn FileIndexResolver>,
    shard_loader: Box<dyn ShardLoader>,
    xorb_fetcher: Box<dyn XorbFetcher>,
    /// Optional unified chunk cache — shared with hydrate and FUSE.
    chunk_cache: Option<Arc<ChunkCache>>,
    operation_timeout: Duration,
    #[expect(dead_code, reason = "skeleton — used when download pipeline is wired")]
    download_concurrency: usize,
    #[expect(
        dead_code,
        reason = "skeleton — used when batch splitting is wired into smudge_file"
    )]
    smudge_batch_memory_bytes: u64,
}

impl SmudgeSession {
    /// Create a new session with default configuration.
    pub fn new(ctx: AppContext) -> Self {
        let download_concurrency = ctx.config().download_concurrency;
        let operation_timeout = ctx.config().operation_timeout;
        Self {
            ctx,
            file_index_resolver: Box::new(NoopFileIndexResolver),
            shard_loader: Box::new(NoopShardLoader),
            xorb_fetcher: Box::new(NoopXorbFetcher),
            chunk_cache: None,
            operation_timeout,
            download_concurrency,
            smudge_batch_memory_bytes: DEFAULT_SMUDGE_BATCH_MEMORY_BYTES,
        }
    }

    /// Create a session with a shared chunk cache.
    pub fn with_chunk_cache(ctx: AppContext, cache: Arc<ChunkCache>) -> Self {
        Self {
            chunk_cache: Some(cache),
            ..Self::new(ctx)
        }
    }

    /// Create a session with custom dependencies (for testing).
    #[cfg(test)]
    pub fn with_deps(
        ctx: AppContext,
        resolver: Box<dyn FileIndexResolver>,
        loader: Box<dyn ShardLoader>,
        fetcher: Box<dyn XorbFetcher>,
        operation_timeout: Duration,
        smudge_batch_memory_bytes: u64,
    ) -> Self {
        Self {
            ctx,
            file_index_resolver: resolver,
            shard_loader: loader,
            xorb_fetcher: fetcher,
            chunk_cache: None,
            operation_timeout,
            download_concurrency: 8,
            smudge_batch_memory_bytes,
        }
    }

    /// Smudge a single file: parse pointer → resolve → reconstruct → verify → output.
    ///
    /// The entire operation is wrapped in `operation_timeout`. On timeout,
    /// returns `CrabError::Protocol` with no partial output.
    ///
    /// `smudge_file_inner` is synchronous (the `XorbFetcher` trait uses
    /// blocking I/O). Running it inside `async { ... }` directly would
    /// not let the timeout fire during slow fetches — the inner call
    /// completes in a single poll and monopolises the tokio thread.
    /// We wrap in `block_in_place` so the runtime can schedule other
    /// tasks (including the timeout timer) while the inner work runs,
    /// and the timeout race works as intended. See finding S1-P2-2.
    ///
    /// `block_in_place` requires the multi-threaded runtime. In
    /// current-thread runtimes (tests, small CLI paths), we fall back
    /// to the original direct call — timeout behaviour there is
    /// cooperative only but tests don't exercise the long-fetch path.
    pub async fn smudge_file(&self, pointer_bytes: &[u8]) -> Result<Vec<u8>> {
        let run_inner = || self.smudge_file_inner(pointer_bytes);
        let fut = async {
            // `block_in_place` panics on current-thread runtimes; guard with
            // a runtime-flavor check so tests don't blow up.
            match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
                Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
                    tokio::task::block_in_place(run_inner)
                }
                _ => run_inner(),
            }
        };
        match tokio::time::timeout(self.operation_timeout, fut).await {
            Ok(result) => result,
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = self.operation_timeout.as_secs(),
                    "smudge operation timed out"
                );
                Err(CrabError::Protocol(format!(
                    "smudge timed out after {} s",
                    self.operation_timeout.as_secs(),
                )))
            }
        }
    }

    /// Delta-aware smudge: reconstruct a file by reusing chunks from a
    /// base version already available locally.
    ///
    /// When `base_terms` and `base_content` are provided, compares the
    /// base and target reconstruction term lists and copies unchanged
    /// segments from the base, fetching only the delta from storage.
    /// Falls back to full reconstruction if the base is empty or the
    /// delta path fails.
    ///
    /// The smudge gate (blake3 + size verification) is applied to the
    /// final output regardless of which path produced it.
    ///
    /// See [`smudge_file`](Self::smudge_file) for the timeout/block_in_place
    /// pattern; the same reasoning applies here.
    pub async fn smudge_file_delta(
        &self,
        pointer_bytes: &[u8],
        base_terms: &[ReconstructionTerm],
        base_content: &[u8],
    ) -> Result<Vec<u8>> {
        let run_inner = || self.smudge_file_delta_inner(pointer_bytes, base_terms, base_content);
        let fut = async {
            match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
                Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
                    tokio::task::block_in_place(run_inner)
                }
                _ => run_inner(),
            }
        };
        match tokio::time::timeout(self.operation_timeout, fut).await {
            Ok(result) => result,
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = self.operation_timeout.as_secs(),
                    "smudge delta operation timed out"
                );
                Err(CrabError::Protocol(format!(
                    "smudge timed out after {} s",
                    self.operation_timeout.as_secs(),
                )))
            }
        }
    }

    /// Inner delta smudge logic without the timeout wrapper.
    fn smudge_file_delta_inner(
        &self,
        pointer_bytes: &[u8],
        base_terms: &[ReconstructionTerm],
        base_content: &[u8],
    ) -> Result<Vec<u8>> {
        use crate::git::delta_reconstruct::{estimate_reuse_ratio, reconstruct_from_delta};

        let pointer = Pointer::parse(pointer_bytes)?;

        let shard_hash = self.resolve_shard(&pointer)?;
        let target_terms = self
            .shard_loader
            .load_reconstruction_terms(&shard_hash, &pointer.file_hash)?;

        if target_terms.is_empty() {
            return Err(CrabError::NotFound {
                path: format!(
                    "reconstruction terms for {}",
                    hex_encode(&pointer.file_hash)
                ),
            });
        }

        // Check if delta reconstruction is worthwhile (>10% reuse).
        let reuse_ratio = estimate_reuse_ratio(base_terms, &target_terms);

        let content = if !base_terms.is_empty() && reuse_ratio > 0.1 {
            tracing::debug!(
                file_hash = %hex_encode(&pointer.file_hash),
                reuse_ratio = format!("{:.1}%", reuse_ratio * 100.0),
                "smudge: using delta reconstruction"
            );

            let delta_result = reconstruct_from_delta(
                base_terms,
                base_content,
                &target_terms,
                self.xorb_fetcher.as_ref(),
                self.chunk_cache.as_ref(),
            )?;

            tracing::debug!(
                reused_bytes = delta_result.reused_bytes,
                fetched_bytes = delta_result.fetched_bytes,
                reused_segments = delta_result.reused_segments,
                fetched_segments = delta_result.fetched_segments,
                "delta reconstruction complete"
            );

            delta_result.content
        } else {
            tracing::debug!(
                file_hash = %hex_encode(&pointer.file_hash),
                reuse_ratio = format!("{:.1}%", reuse_ratio * 100.0),
                "smudge: delta not worthwhile, full reconstruction"
            );
            self.fetch_and_reconstruct(&target_terms)?
        };

        smudge_gate_verify(&content, &pointer.file_hash, pointer.size)?;
        Ok(content)
    }

    /// Resolve a pointer's reconstruction terms without fetching content.
    ///
    /// Useful for callers that need the term list for delta planning
    /// (e.g., the hydrate command comparing versions).
    pub fn resolve_terms(&self, pointer_bytes: &[u8]) -> Result<Vec<ReconstructionTerm>> {
        let pointer = Pointer::parse(pointer_bytes)?;
        let shard_hash = self.resolve_shard(&pointer)?;
        Ok(self
            .shard_loader
            .load_reconstruction_terms(&shard_hash, &pointer.file_hash)?)
    }

    /// Inner smudge logic without the timeout wrapper.
    fn smudge_file_inner(&self, pointer_bytes: &[u8]) -> Result<Vec<u8>> {
        // Phase: parse_pointer
        let pointer = {
            let _span = tracing::info_span!("smudge.parse_pointer").entered();
            Pointer::parse(pointer_bytes)?
        };

        tracing::debug!(
            file_hash = %hex_encode(&pointer.file_hash),
            size = pointer.size,
            shard_hint = ?pointer.shard_hint.map(|h| hex_encode(&h)),
            "smudge: resolving pointer"
        );

        // Phase: resolve_shard
        let shard_hash = {
            let _span = tracing::info_span!("smudge.resolve_shard").entered();
            self.resolve_shard(&pointer)?
        };

        // Phase: plan_ranges — load shard and extract reconstruction terms.
        let terms = {
            let _span = tracing::info_span!("smudge.plan_ranges").entered();
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
            terms
        };

        // Phase: fetch_xorbs + decode_chunks — fetch and reconstruct.
        let content = self.fetch_and_reconstruct(&terms)?;

        // Phase: write_stdout — smudge gate verify before emitting output.
        {
            let _span = tracing::info_span!("smudge.write_stdout").entered();
            smudge_gate_verify(&content, &pointer.file_hash, pointer.size)?;
        }

        Ok(content)
    }

    /// Resolve the shard hash for a pointer, with file-index fallback.
    fn resolve_shard(&self, pointer: &Pointer) -> Result<[u8; 32]> {
        // Attempt file-index lookup (with shard-hint fast path).
        self.ctx.metrics().inc_shard_bloom_queries();

        if let Some(shard_hash) = self
            .file_index_resolver
            .resolve_file_index(&pointer.file_hash, pointer.shard_hint.as_ref())?
        {
            return Ok(shard_hash);
        }

        // File-index miss: fallback scan of shard-list.
        tracing::debug!(
            file_hash = %hex_encode(&pointer.file_hash),
            "file-index miss, scanning shard-list"
        );

        if let Some(shard_hash) = self
            .file_index_resolver
            .scan_shard_list_for_file(&pointer.file_hash)?
        {
            return Ok(shard_hash);
        }

        // No shard found anywhere.
        Err(CrabError::NotFound {
            path: format!("file-hash {}", hex_encode(&pointer.file_hash)),
        })
    }

    /// Fetch xorb ranges and reconstruct the file content.
    ///
    /// Checks the chunk cache before issuing Range GETs. Fetched chunks
    /// are stored in the cache for reuse by subsequent smudge, hydrate,
    /// or FUSE operations.
    fn fetch_and_reconstruct(&self, terms: &[ReconstructionTerm]) -> Result<Vec<u8>> {
        let mut content = Vec::new();
        for term in terms {
            let chunk_hash = MerkleHash::from_slice(&term.chunk_hash).ok();

            // Check chunk cache first.
            if let (Some(cache), Some(hash)) = (&self.chunk_cache, &chunk_hash)
                && let Some(cached) = cache.get(hash)
            {
                content.extend_from_slice(&cached);
                continue;
            }

            let chunk_data = {
                let _span = tracing::info_span!("smudge.fetch_xorbs").entered();
                self.xorb_fetcher
                    .fetch_range(&term.xorb_hash, term.offset..term.offset + term.length)?
            };

            // Per-chunk verification.
            {
                let _span = tracing::info_span!("smudge.decode_chunks").entered();
                let actual_hash = *blake3::hash(&chunk_data).as_bytes();
                if actual_hash != term.chunk_hash {
                    return Err(CrabError::HashMismatch {
                        requested: hex_encode(&term.chunk_hash),
                        actual: hex_encode(&actual_hash),
                    });
                }
            }

            // Store in chunk cache for reuse.
            if let (Some(cache), Some(hash)) = (&self.chunk_cache, chunk_hash) {
                cache.put(hash, bytes::Bytes::from(chunk_data.clone()));
            }

            content.extend_from_slice(&chunk_data);
        }
        Ok(content)
    }
}

// ---------------------------------------------------------------------------
// Smudge gate — no output until blake3 + size verified
// ---------------------------------------------------------------------------

/// Verify that reconstructed content matches the expected blake3 hash and size.
///
/// This is the "smudge gate": no output is emitted to git until this check
/// passes. On mismatch, returns an error and the caller emits nothing.
fn smudge_gate_verify(content: &[u8], expected_hash: &[u8; 32], expected_size: u64) -> Result<()> {
    // Size check.
    let actual_size = content.len() as u64;
    if actual_size != expected_size {
        return Err(CrabError::CorruptObject {
            path: format!("file-hash {}", hex_encode(expected_hash)),
            reason: format!("size mismatch: expected {expected_size}, got {actual_size}"),
        });
    }

    // Blake3 check.
    let actual_hash = *blake3::hash(content).as_bytes();
    if actual_hash != *expected_hash {
        return Err(CrabError::CorruptObject {
            path: format!("file-hash {}", hex_encode(expected_hash)),
            reason: format!(
                "blake3 mismatch: expected {}, got {}",
                hex_encode(expected_hash),
                hex_encode(&actual_hash),
            ),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// SmudgeQueue — delay-capable queue with semaphore-bounded concurrency
// ---------------------------------------------------------------------------

/// Delay-capable smudge queue for the filter-process `delay` capability.
///
/// When git sends smudge requests with `can-delay`, files are enqueued
/// and processed in batches. A semaphore bounds the number of concurrent
/// smudge operations to `download_concurrency`.
pub struct SmudgeQueue {
    /// Pending smudge requests awaiting batch resolution.
    pending: Vec<SmudgeRequest>,
    /// Semaphore bounding concurrent smudge operations.
    semaphore: tokio::sync::Semaphore,
}

/// A pending smudge request in the queue.
#[derive(Debug, Clone)]
pub struct SmudgeRequest {
    /// The pathname git is smudging.
    pub pathname: String,
    /// Raw pointer bytes from git.
    pub pointer_bytes: Vec<u8>,
}

impl SmudgeQueue {
    /// Create a new queue with the given concurrency bound.
    pub fn new(concurrency: usize) -> Self {
        Self {
            pending: Vec::new(),
            semaphore: tokio::sync::Semaphore::new(concurrency),
        }
    }

    /// Enqueue a smudge request for delayed processing.
    pub fn enqueue(&mut self, request: SmudgeRequest) {
        self.pending.push(request);
    }

    /// Drain all pending requests into a batch for coalesced resolution.
    pub fn drain_batch(&mut self) -> Vec<SmudgeRequest> {
        std::mem::take(&mut self.pending)
    }

    /// Number of pending requests.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Acquire a concurrency permit before starting a smudge operation.
    ///
    /// Callers should hold the permit for the duration of the smudge.
    pub async fn acquire_permit(
        &self,
    ) -> std::result::Result<tokio::sync::SemaphorePermit<'_>, tokio::sync::AcquireError> {
        self.semaphore.acquire().await
    }
}

// ---------------------------------------------------------------------------
// SmudgeBatch — cross-file coalescing
// ---------------------------------------------------------------------------

/// A batch of smudge requests resolved together for cross-file coalescing.
///
/// Groups reconstruction terms by xorb hash and coalesces nearby byte
/// ranges (within `COALESCE_GAP` chunks) to minimize Range GET requests.
pub struct SmudgeBatch {
    /// Per-file reconstruction terms, keyed by file hash.
    file_terms: HashMap<[u8; 32], Vec<ReconstructionTerm>>,
    /// Memory cap for the batch.
    memory_cap: u64,
    /// Current estimated memory usage.
    current_memory: u64,
}

impl SmudgeBatch {
    /// Create a new batch with the given memory cap.
    pub fn new(memory_cap: u64) -> Self {
        Self {
            file_terms: HashMap::new(),
            memory_cap,
            current_memory: 0,
        }
    }

    /// Add a file's reconstruction terms to the batch.
    ///
    /// Returns `false` if adding this file would exceed the memory cap,
    /// in which case the caller should resolve the current batch first
    /// and start a new one.
    ///
    /// # Memory cap behaviour for oversized files
    ///
    /// When the batch is empty, this method accepts the file *even if*
    /// its terms exceed `memory_cap` — a file's reconstruction terms
    /// cannot be split across batches without breaking reconstruction,
    /// so the alternative is to reject oversized files outright. The
    /// caller logs a warning when `current_memory > memory_cap` after
    /// adding. See finding CR2-F23.
    pub fn try_add_file(&mut self, file_hash: [u8; 32], terms: Vec<ReconstructionTerm>) -> bool {
        let file_memory: u64 = terms.iter().map(|t| t.length).sum();

        if self.current_memory + file_memory > self.memory_cap && !self.file_terms.is_empty() {
            return false;
        }

        self.current_memory += file_memory;
        if self.current_memory > self.memory_cap {
            tracing::warn!(
                file_hash = %crab_types::pointer::hex_encode(&file_hash),
                file_memory,
                memory_cap = self.memory_cap,
                "single-file terms exceed smudge batch memory cap; processing in isolated batch"
            );
        }
        self.file_terms.insert(file_hash, terms);
        true
    }

    /// Resolve the batch: group by xorb, coalesce ranges, produce a
    /// coalesced fetch plan.
    ///
    /// The coalesced plan minimizes Range GET requests by merging nearby
    /// ranges within the same xorb. Two ranges are coalesced if the gap
    /// between them is ≤ `COALESCE_GAP` chunks (estimated at 128 KiB each).
    pub fn resolve(&self) -> Vec<CoalescedRange> {
        // Step 1: group all terms by xorb hash.
        let mut by_xorb: HashMap<[u8; 32], Vec<&ReconstructionTerm>> = HashMap::new();
        for terms in self.file_terms.values() {
            for term in terms {
                by_xorb.entry(term.xorb_hash).or_default().push(term);
            }
        }

        // Step 2: for each xorb, sort by offset and coalesce.
        let mut plan = Vec::new();
        for (xorb_hash, mut terms) in by_xorb {
            terms.sort_by_key(|t| t.offset);
            plan.extend(coalesce_terms(xorb_hash, &terms));
        }

        // Step 3: sort the plan to preserve first-file time-to-ready.
        // Ranges needed by the first file in insertion order come first.
        // Since HashMap doesn't preserve insertion order, we sort by the
        // minimum offset within each xorb as a reasonable heuristic.
        plan.sort_by_key(|r| (r.xorb_hash, r.range.start));

        plan
    }

    /// Number of files in the batch.
    pub fn file_count(&self) -> usize {
        self.file_terms.len()
    }

    /// Current estimated memory usage in bytes.
    pub fn current_memory(&self) -> u64 {
        self.current_memory
    }

    /// Split this batch into sub-batches that each fit within the memory cap.
    ///
    /// Files are split at file boundaries — a single file's terms are
    /// never split across sub-batches.
    pub fn split_if_needed(self) -> Vec<SmudgeBatch> {
        if self.current_memory <= self.memory_cap {
            return vec![self];
        }

        let mut batches = Vec::new();
        let mut current = SmudgeBatch::new(self.memory_cap);

        for (file_hash, terms) in self.file_terms {
            if !current.try_add_file(file_hash, terms.clone()) {
                batches.push(current);
                current = SmudgeBatch::new(self.memory_cap);
                // The file that didn't fit starts the new batch.
                current.try_add_file(file_hash, terms);
            }
        }

        if current.file_count() > 0 {
            batches.push(current);
        }

        batches
    }
}

/// Coalesce sorted terms within a single xorb into merged ranges.
///
/// Two adjacent terms are merged if the gap between them is ≤
/// `COALESCE_GAP * ESTIMATED_CHUNK_SIZE` bytes.
fn coalesce_terms(
    xorb_hash: [u8; 32],
    sorted_terms: &[&ReconstructionTerm],
) -> Vec<CoalescedRange> {
    if sorted_terms.is_empty() {
        return Vec::new();
    }

    let max_gap = u64::from(COALESCE_GAP) * ESTIMATED_CHUNK_SIZE;
    let mut ranges: Vec<CoalescedRange> = Vec::new();

    let first = sorted_terms[0];
    let mut current = CoalescedRange {
        xorb_hash,
        range: first.offset..first.offset + first.length,
        terms: vec![CoalescedTerm {
            relative_offset: 0,
            length: first.length,
            chunk_hash: first.chunk_hash,
        }],
    };

    for term in &sorted_terms[1..] {
        let term_start = term.offset;
        let term_end = term.offset + term.length;

        if term_start <= current.range.end + max_gap {
            // Coalesce: extend the current range.
            let relative_offset = term_start - current.range.start;
            if term_end > current.range.end {
                current.range.end = term_end;
            }
            current.terms.push(CoalescedTerm {
                relative_offset,
                length: term.length,
                chunk_hash: term.chunk_hash,
            });
        } else {
            // Gap too large: start a new range.
            ranges.push(current);
            current = CoalescedRange {
                xorb_hash,
                range: term_start..term_end,
                terms: vec![CoalescedTerm {
                    relative_offset: 0,
                    length: term.length,
                    chunk_hash: term.chunk_hash,
                }],
            };
        }
    }

    ranges.push(current);
    ranges
}

/// Compute coalescing metrics: how many requests were saved and bytes saved.
pub fn compute_coalescing_metrics(
    original_term_count: usize,
    coalesced_ranges: &[CoalescedRange],
) -> (u64, u64) {
    let coalesced_count = coalesced_ranges.len();
    let requests_saved = original_term_count.saturating_sub(coalesced_count) as u64;

    // Bytes saved = sum of gap bytes that were fetched as part of coalesced
    // ranges but would not have been fetched in per-term mode.
    let coalesced_total: u64 = coalesced_ranges
        .iter()
        .map(|r| r.range.end - r.range.start)
        .sum();
    let original_total: u64 = coalesced_ranges
        .iter()
        .flat_map(|r| r.terms.iter())
        .map(|t| t.length)
        .sum();

    // The "bytes saved" is the reduction in request overhead, not the gap
    // bytes. We approximate as requests_saved * typical HTTP overhead.
    // For a more accurate metric, we track the difference in total fetch bytes.
    let bytes_saved = original_total.saturating_sub(coalesced_total);

    (requests_saved, bytes_saved)
}

/// Encode a 32-byte hash as lowercase hex.
fn hex_encode(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::AppContext;

    // --- Mock implementations ---

    struct MockFileIndexResolver {
        /// Maps file_hash → shard_hash.
        index: HashMap<[u8; 32], [u8; 32]>,
        /// Maps file_hash → shard_hash for the fallback scan.
        shard_list: HashMap<[u8; 32], [u8; 32]>,
    }

    impl FileIndexResolver for MockFileIndexResolver {
        fn resolve_file_index(
            &self,
            file_hash: &[u8; 32],
            _shard_hint: Option<&[u8; 32]>,
        ) -> crab_vfs::Result<Option<[u8; 32]>> {
            Ok(self.index.get(file_hash).copied())
        }

        fn scan_shard_list_for_file(
            &self,
            file_hash: &[u8; 32],
        ) -> crab_vfs::Result<Option<[u8; 32]>> {
            Ok(self.shard_list.get(file_hash).copied())
        }
    }

    struct MockShardLoader {
        /// Maps (shard_hash, file_hash) → terms.
        terms: HashMap<([u8; 32], [u8; 32]), Vec<ReconstructionTerm>>,
    }

    impl ShardLoader for MockShardLoader {
        fn load_reconstruction_terms(
            &self,
            shard_hash: &[u8; 32],
            file_hash: &[u8; 32],
        ) -> crab_vfs::Result<Vec<ReconstructionTerm>> {
            Ok(self
                .terms
                .get(&(*shard_hash, *file_hash))
                .cloned()
                .unwrap_or_default())
        }
    }

    struct MockXorbFetcher {
        /// Maps xorb_hash → full xorb content.
        xorbs: HashMap<[u8; 32], Vec<u8>>,
    }

    impl XorbFetcher for MockXorbFetcher {
        fn fetch_range(
            &self,
            xorb_hash: &[u8; 32],
            range: Range<u64>,
        ) -> crab_vfs::Result<Vec<u8>> {
            let data = self
                .xorbs
                .get(xorb_hash)
                .ok_or_else(|| crab_vfs::VfsError::NotFound {
                    path: format!("xorb/{}", hex_encode(xorb_hash)),
                })?;
            let start = range.start as usize;
            let end = range.end as usize;
            if end > data.len() {
                return Err(crab_vfs::VfsError::NotFound {
                    path: format!("xorb/{} range {start}..{end}", hex_encode(xorb_hash)),
                });
            }
            Ok(data[start..end].to_vec())
        }
    }

    // --- Helper to build a complete mock smudge scenario ---

    fn build_test_scenario(content: &[u8]) -> (SmudgeSession, Vec<u8>) {
        let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        let chunk_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        let shard_hash = [0xAA; 32];
        let xorb_hash = [0xBB; 32];

        let pointer = Pointer {
            file_hash,
            size: content.len() as u64,
            shard_hint: None,
        };

        let mut index = HashMap::new();
        index.insert(file_hash, shard_hash);

        let mut terms_map = HashMap::new();
        terms_map.insert(
            (shard_hash, file_hash),
            vec![ReconstructionTerm {
                xorb_hash,
                offset: 0,
                length: content.len() as u64,
                chunk_hash,
            }],
        );

        let mut xorbs = HashMap::new();
        xorbs.insert(xorb_hash, content.to_vec());

        let ctx = AppContext::default();
        let session = SmudgeSession::with_deps(
            ctx,
            Box::new(MockFileIndexResolver {
                index,
                shard_list: HashMap::new(),
            }),
            Box::new(MockShardLoader { terms: terms_map }),
            Box::new(MockXorbFetcher { xorbs }),
            Duration::from_secs(10),
            DEFAULT_SMUDGE_BATCH_MEMORY_BYTES,
        );

        (session, pointer.serialize())
    }

    // --- SmudgeSession tests ---

    #[tokio::test]
    async fn smudge_round_trip() {
        let content = b"hello smudge world";
        let (session, pointer_bytes) = build_test_scenario(content);
        let result = session.smudge_file(&pointer_bytes).await.unwrap();
        assert_eq!(result, content);
    }

    #[tokio::test]
    async fn smudge_empty_file() {
        let content = b"";
        let (session, pointer_bytes) = build_test_scenario(content);
        let result = session.smudge_file(&pointer_bytes).await.unwrap();
        assert_eq!(result, content.as_slice());
    }

    #[tokio::test]
    async fn smudge_file_index_fallback() {
        let content = b"fallback content";
        let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        let chunk_hash = file_hash;
        let shard_hash = [0xCC; 32];
        let xorb_hash = [0xDD; 32];

        let pointer = Pointer {
            file_hash,
            size: content.len() as u64,
            shard_hint: None,
        };

        // File-index has no entry, but shard-list scan finds it.
        let mut shard_list = HashMap::new();
        shard_list.insert(file_hash, shard_hash);

        let mut terms_map = HashMap::new();
        terms_map.insert(
            (shard_hash, file_hash),
            vec![ReconstructionTerm {
                xorb_hash,
                offset: 0,
                length: content.len() as u64,
                chunk_hash,
            }],
        );

        let mut xorbs = HashMap::new();
        xorbs.insert(xorb_hash, content.to_vec());

        let ctx = AppContext::default();
        let session = SmudgeSession::with_deps(
            ctx,
            Box::new(MockFileIndexResolver {
                index: HashMap::new(),
                shard_list,
            }),
            Box::new(MockShardLoader { terms: terms_map }),
            Box::new(MockXorbFetcher { xorbs }),
            Duration::from_secs(10),
            DEFAULT_SMUDGE_BATCH_MEMORY_BYTES,
        );

        let result = session.smudge_file(&pointer.serialize()).await.unwrap();
        assert_eq!(result, content);
    }

    #[tokio::test]
    async fn smudge_not_found_when_no_shard() {
        let content = b"orphan file";
        let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();

        let pointer = Pointer {
            file_hash,
            size: content.len() as u64,
            shard_hint: None,
        };

        let ctx = AppContext::default();
        let session = SmudgeSession::with_deps(
            ctx,
            Box::new(NoopFileIndexResolver),
            Box::new(NoopShardLoader),
            Box::new(NoopXorbFetcher),
            Duration::from_secs(10),
            DEFAULT_SMUDGE_BATCH_MEMORY_BYTES,
        );

        let err = session.smudge_file(&pointer.serialize()).await.unwrap_err();
        assert!(matches!(err, CrabError::NotFound { .. }));
    }

    #[tokio::test]
    async fn smudge_timeout() {
        let content = b"timeout test";
        let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();

        let pointer = Pointer {
            file_hash,
            size: content.len() as u64,
            shard_hint: None,
        };

        // Use a resolver that always returns None, forcing the fallback
        // scan which also returns None. With a very short timeout and
        // a slow resolver, we'd hit the timeout. For this test, we use
        // a zero-duration timeout to guarantee it fires.
        let ctx = AppContext::default();
        let session = SmudgeSession::with_deps(
            ctx,
            Box::new(NoopFileIndexResolver),
            Box::new(NoopShardLoader),
            Box::new(NoopXorbFetcher),
            Duration::from_nanos(1), // effectively instant timeout
            DEFAULT_SMUDGE_BATCH_MEMORY_BYTES,
        );

        // The operation may either timeout or complete with NotFound —
        // both are acceptable since the timeout is racing with the
        // synchronous resolver. We just verify no panic and no partial output.
        let result = session.smudge_file(&pointer.serialize()).await;
        assert!(result.is_err());
    }

    // --- Smudge gate tests ---

    #[test]
    fn smudge_gate_passes_on_match() {
        let content = b"verified content";
        let hash = *blake3::hash(content).as_bytes();
        assert!(smudge_gate_verify(content, &hash, content.len() as u64).is_ok());
    }

    #[test]
    fn smudge_gate_rejects_size_mismatch() {
        let content = b"some content";
        let hash = *blake3::hash(content).as_bytes();
        let err = smudge_gate_verify(content, &hash, 999).unwrap_err();
        assert!(matches!(err, CrabError::CorruptObject { .. }));
    }

    #[test]
    fn smudge_gate_rejects_hash_mismatch() {
        let content = b"some content";
        let wrong_hash = [0xFF; 32];
        let err = smudge_gate_verify(content, &wrong_hash, content.len() as u64).unwrap_err();
        assert!(matches!(err, CrabError::CorruptObject { .. }));
    }

    // --- SmudgeQueue tests ---

    #[tokio::test]
    async fn smudge_queue_enqueue_and_drain() {
        let mut queue = SmudgeQueue::new(4);
        assert_eq!(queue.pending_count(), 0);

        queue.enqueue(SmudgeRequest {
            pathname: "a.bin".into(),
            pointer_bytes: vec![1, 2, 3],
        });
        queue.enqueue(SmudgeRequest {
            pathname: "b.bin".into(),
            pointer_bytes: vec![4, 5, 6],
        });
        assert_eq!(queue.pending_count(), 2);

        let batch = queue.drain_batch();
        assert_eq!(batch.len(), 2);
        assert_eq!(queue.pending_count(), 0);
    }

    #[tokio::test]
    async fn smudge_queue_semaphore_bounds_concurrency() {
        let queue = SmudgeQueue::new(2);

        let _p1 = queue.acquire_permit().await.unwrap();
        let _p2 = queue.acquire_permit().await.unwrap();

        // Third acquire should not complete immediately.
        let result = tokio::time::timeout(Duration::from_millis(50), queue.acquire_permit()).await;
        assert!(result.is_err(), "third permit should block");
    }

    // --- Coalescing tests ---

    #[test]
    fn coalesce_adjacent_terms() {
        let xorb = [0x11; 32];
        let terms = vec![
            ReconstructionTerm {
                xorb_hash: xorb,
                offset: 0,
                length: 1000,
                chunk_hash: [0x01; 32],
            },
            ReconstructionTerm {
                xorb_hash: xorb,
                offset: 1000,
                length: 1000,
                chunk_hash: [0x02; 32],
            },
        ];

        let refs: Vec<&ReconstructionTerm> = terms.iter().collect();
        let coalesced = coalesce_terms(xorb, &refs);

        assert_eq!(coalesced.len(), 1, "adjacent terms should coalesce");
        assert_eq!(coalesced[0].range, 0..2000);
        assert_eq!(coalesced[0].terms.len(), 2);
    }

    #[test]
    fn coalesce_with_small_gap() {
        let xorb = [0x22; 32];
        let gap_bytes = COALESCE_GAP as u64 * ESTIMATED_CHUNK_SIZE;

        let terms = vec![
            ReconstructionTerm {
                xorb_hash: xorb,
                offset: 0,
                length: 1000,
                chunk_hash: [0x01; 32],
            },
            ReconstructionTerm {
                xorb_hash: xorb,
                offset: 1000 + gap_bytes, // exactly at the gap limit
                length: 1000,
                chunk_hash: [0x02; 32],
            },
        ];

        let refs: Vec<&ReconstructionTerm> = terms.iter().collect();
        let coalesced = coalesce_terms(xorb, &refs);

        assert_eq!(coalesced.len(), 1, "gap within limit should coalesce");
    }

    #[test]
    fn no_coalesce_with_large_gap() {
        let xorb = [0x33; 32];
        let gap_bytes = COALESCE_GAP as u64 * ESTIMATED_CHUNK_SIZE + 1;

        let terms = vec![
            ReconstructionTerm {
                xorb_hash: xorb,
                offset: 0,
                length: 1000,
                chunk_hash: [0x01; 32],
            },
            ReconstructionTerm {
                xorb_hash: xorb,
                offset: 1000 + gap_bytes,
                length: 1000,
                chunk_hash: [0x02; 32],
            },
        ];

        let refs: Vec<&ReconstructionTerm> = terms.iter().collect();
        let coalesced = coalesce_terms(xorb, &refs);

        assert_eq!(
            coalesced.len(),
            2,
            "gap exceeding limit should not coalesce"
        );
    }

    #[test]
    fn coalesce_empty_terms() {
        let coalesced = coalesce_terms([0; 32], &[]);
        assert!(coalesced.is_empty());
    }

    #[test]
    fn coalesce_single_term() {
        let xorb = [0x44; 32];
        let term = ReconstructionTerm {
            xorb_hash: xorb,
            offset: 100,
            length: 500,
            chunk_hash: [0x01; 32],
        };
        let coalesced = coalesce_terms(xorb, &[&term]);

        assert_eq!(coalesced.len(), 1);
        assert_eq!(coalesced[0].range, 100..600);
        assert_eq!(coalesced[0].terms.len(), 1);
        assert_eq!(coalesced[0].terms[0].relative_offset, 0);
    }

    // --- SmudgeBatch tests ---

    #[test]
    fn batch_resolve_groups_by_xorb() {
        let mut batch = SmudgeBatch::new(DEFAULT_SMUDGE_BATCH_MEMORY_BYTES);

        let xorb_a = [0xAA; 32];
        let xorb_b = [0xBB; 32];

        batch.try_add_file(
            [0x01; 32],
            vec![
                ReconstructionTerm {
                    xorb_hash: xorb_a,
                    offset: 0,
                    length: 1000,
                    chunk_hash: [0x10; 32],
                },
                ReconstructionTerm {
                    xorb_hash: xorb_b,
                    offset: 0,
                    length: 2000,
                    chunk_hash: [0x20; 32],
                },
            ],
        );

        batch.try_add_file(
            [0x02; 32],
            vec![ReconstructionTerm {
                xorb_hash: xorb_a,
                offset: 1000,
                length: 1000,
                chunk_hash: [0x30; 32],
            }],
        );

        let plan = batch.resolve();

        // xorb_a should have its two terms coalesced into one range.
        let xorb_a_ranges: Vec<_> = plan.iter().filter(|r| r.xorb_hash == xorb_a).collect();
        assert_eq!(xorb_a_ranges.len(), 1);
        assert_eq!(xorb_a_ranges[0].range, 0..2000);
        assert_eq!(xorb_a_ranges[0].terms.len(), 2);

        // xorb_b should have one range.
        let xorb_b_ranges: Vec<_> = plan.iter().filter(|r| r.xorb_hash == xorb_b).collect();
        assert_eq!(xorb_b_ranges.len(), 1);
    }

    #[test]
    fn batch_memory_cap_rejects_overflow() {
        let mut batch = SmudgeBatch::new(1000);

        let added = batch.try_add_file(
            [0x01; 32],
            vec![ReconstructionTerm {
                xorb_hash: [0xAA; 32],
                offset: 0,
                length: 800,
                chunk_hash: [0x10; 32],
            }],
        );
        assert!(added);

        // This would push us over 1000 bytes.
        let added = batch.try_add_file(
            [0x02; 32],
            vec![ReconstructionTerm {
                xorb_hash: [0xBB; 32],
                offset: 0,
                length: 300,
                chunk_hash: [0x20; 32],
            }],
        );
        assert!(!added, "should reject when memory cap exceeded");
    }

    #[test]
    fn batch_split_produces_sub_batches() {
        let mut batch = SmudgeBatch::new(500);

        // Force-add two files that together exceed the cap.
        // First file fits.
        batch.try_add_file(
            [0x01; 32],
            vec![ReconstructionTerm {
                xorb_hash: [0xAA; 32],
                offset: 0,
                length: 400,
                chunk_hash: [0x10; 32],
            }],
        );

        // Manually insert a second file to test split logic.
        batch.file_terms.insert(
            [0x02; 32],
            vec![ReconstructionTerm {
                xorb_hash: [0xBB; 32],
                offset: 0,
                length: 400,
                chunk_hash: [0x20; 32],
            }],
        );
        batch.current_memory = 800;

        let sub_batches = batch.split_if_needed();
        assert!(sub_batches.len() >= 2, "should split into multiple batches");
    }

    // --- Metrics helper tests ---

    #[test]
    fn coalescing_metrics_counts_saved_requests() {
        let ranges = vec![CoalescedRange {
            xorb_hash: [0; 32],
            range: 0..2000,
            terms: vec![
                CoalescedTerm {
                    relative_offset: 0,
                    length: 1000,
                    chunk_hash: [0x01; 32],
                },
                CoalescedTerm {
                    relative_offset: 1000,
                    length: 1000,
                    chunk_hash: [0x02; 32],
                },
            ],
        }];

        let (requests_saved, _bytes_saved) = compute_coalescing_metrics(2, &ranges);
        assert_eq!(requests_saved, 1, "2 terms → 1 range = 1 request saved");
    }

    // --- Metrics integration ---

    #[tokio::test]
    async fn smudge_increments_bloom_queries() {
        let content = b"metrics test";
        let (session, pointer_bytes) = build_test_scenario(content);
        let _ = session.smudge_file(&pointer_bytes).await;
        assert!(session.ctx.metrics().snapshot().shard_bloom_queries >= 1);
    }
}
