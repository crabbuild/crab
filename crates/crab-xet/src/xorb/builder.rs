//! Run-continuity xorb builder.
//!
//! [`XorbBuilder`] accumulates chunks into xorbs while keeping chunks from
//! the same source file (identified by [`RunId`]) together. A xorb is
//! finalized when it reaches [`TARGET_XORB_SIZE`] bytes of compressed data.
//!
//! Per-chunk compression is selected by a [`CompressionPolicy`]. The default
//! policy uses LZ4 for all chunks. The xorb content hash is the
//! [`MerkleHash`] over the ordered chunk-hash/chunk-size sequence,
//! computed via [`xorb_hash`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use xet_core_structures::merklehash::{MerkleHash, xorb_hash};
use xet_core_structures::xorb_object::Chunk;
use xet_core_structures::xorb_object::byte_grouping::BG4Predictor;
use xet_core_structures::xorb_object::constants::MAX_XORB_CHUNKS;

use bytes::Bytes;

use crate::defrag::DefragPrevention;
use crate::entropy::entropy_ratio;
use crate::error::{Result, XetError};
use crate::xorb::format::{ChunkPlacement, CompressionScheme};

pub use crate::xorb::format::{
    CHUNK_META_ENTRY_SIZE, FOOTER_SIZE, MAX_XORB_SIZE, MIN_XORB_SIZE, TARGET_XORB_SIZE, XORB_MAGIC,
};

/// Minimum accumulated run size before a run break is allowed (1 MiB).
pub const MIN_RUN_SIZE: usize = 1024 * 1024;

/// Dedup threshold ratio — a chunk is deduped only when savings exceed this.
pub const DEDUP_THRESHOLD_RATIO: f64 = 0.25;

fn xorb_layout_overflow(field: &str, value: impl std::fmt::Display) -> XetError {
    XetError::Layout {
        field: field.to_string(),
        value: value.to_string(),
    }
}

/// Identifies a run of chunks from the same source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(pub u64);

/// Outcome of pushing a chunk into the builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackOutcome {
    /// Chunk was packed into the current xorb.
    Packed,
    /// Current xorb was full; chunk starts a new xorb.
    RolledOver,
    /// Chunk was already known (deduped); not packed.
    Deduped,
}

/// Per-chunk compression scheme selection policy.
///
/// Implementations inspect chunk data and return the compression scheme
/// to use. The builder calls [`CompressionPolicy::select`] before
/// compressing each chunk.
pub trait CompressionPolicy: Send + Sync {
    /// Choose a compression scheme for the given chunk data.
    fn select(&self, chunk_data: &[u8]) -> CompressionScheme;
}

/// Receives compression outcome counters from xorb packing.
pub trait CompressionMetrics: Send + Sync {
    /// Records chunks compressed with plain LZ4.
    fn add_chunks_compressed(&self, n: u64);

    /// Records chunks transformed with byte-grouping before LZ4.
    fn add_chunks_bg4_transformed(&self, n: u64);

    /// Records chunks stored without compression.
    fn add_chunks_stored_raw(&self, n: u64);

    /// Records bytes saved by compression.
    fn add_compression_bytes_saved(&self, n: u64);
}

/// A compression policy that always returns the same scheme.
///
/// This is the default policy, preserving the current fixed-compression
/// behavior. The scheme defaults to [`CompressionScheme::LZ4`].
pub struct FixedCompression {
    scheme: CompressionScheme,
}

impl FixedCompression {
    /// Creates a policy that always returns the given scheme.
    #[must_use]
    pub fn new(scheme: CompressionScheme) -> Self {
        Self { scheme }
    }
}

impl Default for FixedCompression {
    fn default() -> Self {
        Self {
            scheme: CompressionScheme::LZ4,
        }
    }
}

impl CompressionPolicy for FixedCompression {
    fn select(&self, _chunk_data: &[u8]) -> CompressionScheme {
        self.scheme
    }
}

/// Adaptive compression policy using xet-core's BG4 byte-grouping predictor.
///
/// Three-tier decision per chunk:
/// 1. If `BG4Predictor` recommends byte grouping → `ByteGrouping4LZ4`
/// 2. If Shannon entropy ratio < threshold → `LZ4`
/// 3. Otherwise → `None` (store raw)
///
/// BG4 is particularly effective for structured binary data like ML model
/// weights (float32 arrays), where byte-grouping exposes redundancy that
/// plain LZ4 misses.
pub struct AdaptiveCompression {
    entropy_threshold: f32,
}

impl AdaptiveCompression {
    /// Creates an adaptive policy with the given entropy threshold.
    ///
    /// Chunks with entropy ratio above the threshold are stored raw.
    /// A typical value is 0.95 (less than 5% expected savings from LZ4).
    #[must_use]
    pub fn new(entropy_threshold: f32) -> Self {
        Self { entropy_threshold }
    }
}

impl Default for AdaptiveCompression {
    fn default() -> Self {
        Self {
            entropy_threshold: 0.95,
        }
    }
}

impl CompressionPolicy for AdaptiveCompression {
    fn select(&self, chunk_data: &[u8]) -> CompressionScheme {
        if chunk_data.is_empty() {
            return CompressionScheme::None;
        }

        // Sample the first 4 KiB for both BG4 prediction and entropy estimation.
        let sample = &chunk_data[..chunk_data.len().min(4096)];

        // Tier 1: BG4 predictor — fast KL-divergence heuristic on popcount
        // histograms. Effective for structured binary (ML weights, float arrays).
        let mut predictor = BG4Predictor::default();
        predictor.add_data(0, sample);
        if predictor.bg4_recommended() {
            return CompressionScheme::ByteGrouping4LZ4;
        }

        // Tier 2: Shannon entropy check — if data is compressible, use plain LZ4.
        if entropy_ratio(sample) < self.entropy_threshold {
            return CompressionScheme::LZ4;
        }

        // Tier 3: incompressible data — store raw.
        CompressionScheme::None
    }
}

/// Compresses chunk data according to the given scheme.
///
/// - `None`: returns raw bytes (no compression).
/// - `LZ4`: LZ4 frame compression via xet-core.
/// - `ByteGrouping4LZ4`: BG4 byte-grouping pre-transform then LZ4.
/// - `Auto`: resolves to a concrete scheme via `CompressionScheme::resolve_for_data`.
///
/// `Auto` is resolved before compression so serialized xorbs always carry a
/// concrete scheme.
pub fn compress_chunk(data: &[u8], scheme: CompressionScheme) -> Result<Vec<u8>> {
    match scheme {
        CompressionScheme::None => Ok(data.to_vec()),
        CompressionScheme::LZ4 | CompressionScheme::ByteGrouping4LZ4 => {
            let compressed =
                scheme
                    .compress_from_slice(data)
                    .map_err(|source| XetError::Compress {
                        scheme: scheme.into(),
                        source,
                    })?;
            Ok(compressed.into_owned())
        }
        CompressionScheme::Auto => {
            let resolved = scheme.resolve_for_data(data);
            compress_chunk(data, resolved)
        }
    }
}

/// Result of finalizing an xorb, containing both the serialized bytes
/// and the placement information for each chunk.
pub struct XorbResult {
    /// Serialized xorb bytes ready for upload.
    pub bytes: Bytes,
    /// Content hash of the xorb.
    pub hash: MerkleHash,
    /// BLAKE3 of the exact serialized compressed-payload region.
    pub payload_digest: [u8; 32],
    /// Placement info for each chunk, in xorb order.
    pub placements: Vec<ChunkPlacement>,
}

impl XorbResult {
    /// Number of chunks in this xorb.
    #[must_use]
    pub fn num_chunks(&self) -> u32 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "placement count bounded by xorb chunk count, well under u32::MAX"
        )]
        let n = self.placements.len() as u32;
        n
    }
}

impl std::fmt::Debug for XorbResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XorbResult")
            .field("hash", &self.hash)
            .field("num_chunks", &self.placements.len())
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

/// Bounded pool of serialized xorb allocations shared by cooperating builders.
#[derive(Clone)]
pub struct SerializedPayloadPool {
    buffers: Arc<Mutex<Vec<Vec<u8>>>>,
    max_buffers: usize,
}

impl SerializedPayloadPool {
    /// Creates a pool retaining at most `max_buffers` allocations.
    #[must_use]
    pub fn new(max_buffers: usize) -> Self {
        Self {
            buffers: Arc::new(Mutex::new(Vec::with_capacity(max_buffers.max(1)))),
            max_buffers: max_buffers.max(1),
        }
    }

    fn lock_buffers(&self) -> std::sync::MutexGuard<'_, Vec<Vec<u8>>> {
        match self.buffers.lock() {
            Ok(buffers) => buffers,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn take(&self) -> Option<Vec<u8>> {
        self.lock_buffers().pop()
    }

    fn recycle_vec(&self, mut payload: Vec<u8>) {
        payload.clear();
        let mut buffers = self.lock_buffers();
        if buffers.len() < self.max_buffers {
            buffers.push(payload);
        }
    }

    /// Returns uniquely owned serialized bytes to the pool without copying.
    pub fn recycle_serialized_bytes(&self, bytes: Bytes) -> bool {
        let Ok(bytes) = bytes.try_into_mut() else {
            return false;
        };
        self.recycle_vec(Vec::from(bytes));
        true
    }
}

/// Internal record for a chunk that has been compressed and staged.
struct StagedChunk {
    hash: MerkleHash,
    uncompressed_len: u64,
    compressed_len: usize,
    scheme: CompressionScheme,
}

type CompressedBatchChunk = (usize, CompressionScheme, Option<Vec<u8>>);

/// Builds xorbs from a stream of chunks, maintaining run-continuity.
///
/// Chunks from the same [`RunId`] are kept together unless the current
/// run has accumulated at least [`MIN_RUN_SIZE`] bytes. When the xorb
/// reaches [`TARGET_XORB_SIZE`] compressed bytes, it is finalized and
/// a new one begins.
pub struct XorbBuilder {
    /// Compressed chunks in the current xorb.
    staged: Vec<StagedChunk>,
    /// Contiguous compressed payload for the current xorb.
    payload: Vec<u8>,
    /// Allocation owner shared across related builders.
    serialized_payload_pool: SerializedPayloadPool,
    /// Total compressed bytes in the current xorb.
    current_size: usize,
    /// Total original bytes in the current xorb.
    current_uncompressed_size: u64,
    /// Run id of the most recently pushed chunk.
    current_run: Option<RunId>,
    /// Compressed bytes accumulated for the current run.
    current_run_size: usize,
    /// Number of chunks in the current run (for fragmentation estimation).
    current_run_chunks: usize,
    /// Hashes of all chunks seen so far (for dedup).
    seen: HashSet<MerkleHash>,
    /// Completed xorbs with placement info.
    completed: Vec<XorbResult>,
    /// Whether this builder has sealed its initial xorb.
    /// The first payload reserves once; successors wait for pooled allocation.
    has_finalized_xorb: bool,
    /// Placement records for chunks in the current xorb, filled with a
    /// placeholder xorb_hash until `finalize_current()` computes the real one.
    pending_placements: Vec<ChunkPlacement>,
    /// Dynamic target size for the current xorb (bytes of compressed data).
    /// Defaults to [`TARGET_XORB_SIZE`]. Updated via [`set_target_size`].
    target_size: usize,
    /// Minimum allowed target size (bytes). Clamps `set_target_size` from below.
    min_size: usize,
    /// Maximum allowed target size (bytes). Clamps `set_target_size` from above.
    max_size: usize,
    /// Per-chunk compression scheme selection policy.
    compression_policy: Arc<dyn CompressionPolicy>,
    /// Optional shared metrics for recording compression outcomes.
    metrics: Option<Arc<dyn CompressionMetrics>>,
    /// Rolling fragmentation estimator for run-continuity-aware splitting.
    defrag_prevention: DefragPrevention,
    /// Maximum bytes the xorb may grow beyond `target_size` when
    /// extending for run continuity (default: 10% of target).
    max_xorb_overshoot: usize,
    /// True when the xorb has passed `target_size` but is continuing
    /// to pack chunks from the current run to reduce fragmentation.
    in_extension: bool,
}

impl Default for XorbBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl XorbBuilder {
    /// Creates a new empty builder with the default compression policy (LZ4).
    #[must_use]
    pub fn new() -> Self {
        Self::with_policy(Arc::new(FixedCompression::default()) as Arc<dyn CompressionPolicy>)
    }

    /// Creates a new empty builder with the given compression policy.
    ///
    /// Accepts anything convertible into an `Arc<dyn CompressionPolicy>` so
    /// callers can pass either `Box<dyn CompressionPolicy>` (which converts
    /// via the standard `From<Box<T>> for Arc<T>` impl) or an `Arc` directly.
    #[must_use]
    pub fn with_policy<P: Into<Arc<dyn CompressionPolicy>>>(policy: P) -> Self {
        Self {
            staged: Vec::new(),
            payload: Vec::new(),
            serialized_payload_pool: SerializedPayloadPool::new(1),
            current_size: 0,
            current_uncompressed_size: 0,
            current_run: None,
            current_run_size: 0,
            current_run_chunks: 0,
            seen: HashSet::new(),
            completed: Vec::new(),
            has_finalized_xorb: false,
            pending_placements: Vec::new(),
            target_size: TARGET_XORB_SIZE,
            min_size: MIN_XORB_SIZE,
            max_size: MAX_XORB_SIZE,
            compression_policy: policy.into(),
            metrics: None,
            defrag_prevention: DefragPrevention::default(),
            max_xorb_overshoot: TARGET_XORB_SIZE / 10,
            in_extension: false,
        }
    }

    /// Attaches shared metrics for recording compression outcomes.
    #[must_use]
    pub fn with_metrics<M>(mut self, metrics: Arc<M>) -> Self
    where
        M: CompressionMetrics + 'static,
    {
        self.metrics = Some(metrics);
        self
    }

    /// Uses a caller-owned pool for serialized xorb allocations.
    #[must_use]
    pub fn with_serialized_payload_pool(mut self, pool: SerializedPayloadPool) -> Self {
        self.serialized_payload_pool = pool;
        self
    }

    /// Sets the maximum overshoot beyond `target_size` allowed when
    /// extending a xorb for run continuity.
    #[must_use]
    pub fn with_max_overshoot(mut self, max_overshoot: usize) -> Self {
        self.max_xorb_overshoot = max_overshoot;
        self
    }

    /// Configures the minimum and maximum bounds for adaptive target sizing.
    ///
    /// `set_target_size` clamps to these bounds. Defaults are
    /// [`MIN_XORB_SIZE`] (16 MiB) and [`MAX_XORB_SIZE`] (256 MiB).
    #[must_use]
    pub fn with_size_bounds(mut self, min: usize, max: usize) -> Self {
        self.min_size = min;
        self.max_size = max.max(min);
        // Re-clamp the current target in case it's now out of bounds.
        self.target_size = self.target_size.clamp(self.min_size, self.max_size);
        self.max_xorb_overshoot = self.target_size / 10;
        self
    }

    /// Updates the target xorb size, clamped to `[min_size, max_size]`.
    ///
    /// Takes effect on the next xorb — the xorb currently being built
    /// (if any) is unaffected because `push()` checks `would_exceed`
    /// before adding each chunk, and `finalize_current()` resets state.
    pub fn set_target_size(&mut self, target: usize) {
        self.target_size = target.clamp(self.min_size, self.max_size);
        self.max_xorb_overshoot = self.target_size / 10;
    }

    /// Configures the `DefragPrevention` rolling estimator parameters.
    ///
    /// - `window_size`: number of ranges in the sliding window
    /// - `min_chunks_per_range`: base threshold for fragmentation detection
    /// - `hysteresis`: low-threshold multiplier (0.0, 1.0]
    #[must_use]
    pub fn with_defrag_params(
        mut self,
        window_size: usize,
        min_chunks_per_range: f32,
        hysteresis: f32,
    ) -> Self {
        self.defrag_prevention =
            DefragPrevention::new(window_size, min_chunks_per_range, hysteresis);
        self
    }

    /// Pushes a chunk into the builder.
    ///
    /// Returns [`PackOutcome::Packed`] if the chunk was added to the current
    /// xorb, [`PackOutcome::RolledOver`] if the current xorb was finalized
    /// first, or [`PackOutcome::Deduped`] if the chunk was already seen.
    ///
    /// # Errors
    ///
    /// Returns [`XetError::Compress`] if compression fails.
    pub fn push(&mut self, chunk: &Chunk, run_id: RunId) -> Result<PackOutcome> {
        // Dedup: skip chunks we've already seen.
        if self.seen.contains(&chunk.hash) {
            return Ok(PackOutcome::Deduped);
        }

        // Select compression scheme via the policy.
        let scheme = self.compression_policy.select(&chunk.data);

        if scheme == CompressionScheme::None {
            return self.push_precompressed_with_reserve(
                chunk,
                scheme,
                chunk.data.as_ref(),
                run_id,
                true,
            );
        }

        let compressed = compress_chunk(&chunk.data, scheme)?;
        self.push_precompressed_with_reserve(chunk, scheme, &compressed, run_id, true)
    }

    /// Ingests an already-compressed chunk into the builder.
    ///
    /// This is the state-mutating half of [`push`](Self::push): scheme
    /// selection and compression are assumed to have already run on the
    /// caller's thread (or a rayon worker). Boundary decisions, placement
    /// tracking, seen-set updates, and run-continuity logic are identical
    /// to [`push`](Self::push), so output is byte-for-byte equivalent for
    /// any given chunk sequence.
    ///
    /// Returns [`PackOutcome::Deduped`] if the chunk hash is already in
    /// the seen-set — the caller's compression work is simply discarded
    /// in that case. This matches [`push`](Self::push)'s semantics.
    ///
    /// # Errors
    ///
    /// Currently infallible but returns `Result` for forward-compatibility
    /// with future boundary-decision failures.
    pub fn push_precompressed(
        &mut self,
        chunk: &Chunk,
        scheme: CompressionScheme,
        compressed: Vec<u8>,
        run_id: RunId,
    ) -> Result<PackOutcome> {
        self.push_precompressed_with_reserve(chunk, scheme, &compressed, run_id, true)
    }

    fn push_precompressed_with_reserve(
        &mut self,
        chunk: &Chunk,
        scheme: CompressionScheme,
        compressed: &[u8],
        run_id: RunId,
        reserve_full_payload: bool,
    ) -> Result<PackOutcome> {
        self.push_precompressed_with_reserve_and_admission(
            chunk,
            scheme,
            compressed,
            run_id,
            reserve_full_payload,
            &mut || Ok::<_, XetError>(()),
        )
    }

    fn push_precompressed_with_reserve_and_admission<F, E>(
        &mut self,
        chunk: &Chunk,
        scheme: CompressionScheme,
        compressed: &[u8],
        run_id: RunId,
        reserve_full_payload: bool,
        before_rollover: &mut F,
    ) -> std::result::Result<PackOutcome, E>
    where
        F: FnMut() -> std::result::Result<(), E>,
        E: From<XetError>,
    {
        // Dedup: skip chunks we've already seen. `push_batch`'s outer
        // filter is an optimization; the authoritative check lives here.
        if self.seen.contains(&chunk.hash) {
            return Ok(PackOutcome::Deduped);
        }

        let compressed_len = compressed.len();
        let uncompressed_len = u64::try_from(chunk.data.len())
            .map_err(|_| xorb_layout_overflow("chunk length", chunk.data.len()))?;
        let uncompressed_size = u32::try_from(uncompressed_len)
            .map_err(|_| xorb_layout_overflow("uncompressed chunk length", uncompressed_len))?;

        let next_uncompressed = self
            .current_uncompressed_size
            .checked_add(uncompressed_len)
            .ok_or_else(|| xorb_layout_overflow("uncompressed xorb size", uncompressed_len))?;
        let mut rolled = false;
        if next_uncompressed > u64::from(u32::MAX) && !self.staged.is_empty() {
            self.finalize_current_after_admission(before_rollover)?;
            rolled = true;
        }

        let next_compressed_size = self
            .current_size
            .checked_add(compressed_len)
            .ok_or_else(|| xorb_layout_overflow("compressed xorb size", compressed_len))?;

        // Check if adding this chunk would exceed the target size.
        let would_exceed = next_compressed_size > self.target_size && !self.staged.is_empty();

        // Check for run break: different run_id and current run is large enough.
        let run_break = match self.current_run {
            Some(cur) if cur != run_id => self.current_run_size >= MIN_RUN_SIZE,
            _ => false,
        };

        // Decide whether to seal the xorb or extend for run continuity.
        //
        // When the xorb exceeds the target size, we consult DefragPrevention:
        // if fragmentation is too high (dedup suppressed) and we're still
        // within the overshoot budget, we defer the seal to keep the current
        // run together. A run break always forces a seal since there's no
        // continuity benefit in extending across files.
        if would_exceed {
            let hard_limit = self
                .target_size
                .checked_add(self.max_xorb_overshoot)
                .ok_or_else(|| {
                    xorb_layout_overflow("compressed xorb hard limit", self.max_xorb_overshoot)
                })?;
            let hard_limit_exceeded = next_compressed_size > hard_limit;

            // During extension, a run break means the run we were preserving
            // has ended — seal now.
            let run_ended_during_extension = self.in_extension && run_break;

            if hard_limit_exceeded || run_ended_during_extension {
                self.finalize_current_after_admission(before_rollover)?;
                self.in_extension = false;
                rolled = true;
            } else {
                // Ask DefragPrevention whether fragmentation is acceptable.
                // If dedup is allowed (fragmentation OK), seal at the target
                // boundary. If dedup is suppressed (fragmentation high),
                // extend the xorb to preserve run continuity.
                //
                // We pass 1 as the range size to represent the worst case:
                // sealing now would create a small tail fragment in the next
                // xorb. When fragmentation is already high, even a tiny
                // fragment triggers extension.
                let same_run = self.current_run == Some(run_id);
                let should_extend =
                    same_run && !self.defrag_prevention.allow_dedup_on_next_range(1);

                if should_extend {
                    self.in_extension = true;
                } else {
                    self.finalize_current_after_admission(before_rollover)?;
                    self.in_extension = false;
                    rolled = true;
                }
            }
        }

        // Update run tracking: allow a run break only when the current run
        // has accumulated enough bytes, or when this is the first chunk.
        if self.current_run != Some(run_id)
            && (run_break || self.current_run.is_none() || self.staged.is_empty())
        {
            // Record the completed run's chunk count for fragmentation estimation.
            if self.current_run.is_some() && self.current_run_chunks > 0 {
                self.defrag_prevention
                    .add_range_to_fragmentation_estimate(self.current_run_chunks);
            }
            self.current_run = Some(run_id);
            self.current_run_size = 0;
            self.current_run_chunks = 0;
        }

        // Record placement before staging so chunk_index matches the staged position.
        let chunk_index = u32::try_from(self.staged.len())
            .map_err(|_| xorb_layout_overflow("chunk index", self.staged.len()))?;

        self.pending_placements.push(ChunkPlacement {
            chunk_hash: chunk.hash,
            xorb_hash: MerkleHash::default(),
            chunk_index,
            uncompressed_size,
        });

        self.seen.insert(chunk.hash);
        self.current_size = self
            .current_size
            .checked_add(compressed_len)
            .ok_or_else(|| xorb_layout_overflow("compressed xorb size", compressed_len))?;
        self.current_uncompressed_size = self
            .current_uncompressed_size
            .checked_add(uncompressed_len)
            .ok_or_else(|| xorb_layout_overflow("uncompressed xorb size", uncompressed_len))?;
        self.current_run_size = self
            .current_run_size
            .checked_add(compressed_len)
            .ok_or_else(|| xorb_layout_overflow("compressed run size", compressed_len))?;
        self.current_run_chunks += 1;
        if self.payload.is_empty() {
            if self.payload.capacity() == 0
                && let Some(recycled) = self.serialized_payload_pool.take()
            {
                self.payload = recycled;
            }
            let metadata_reserve = (*MAX_XORB_CHUNKS)
                .checked_mul(CHUNK_META_ENTRY_SIZE)
                .and_then(|bytes| bytes.checked_add(FOOTER_SIZE))
                .ok_or_else(|| xorb_layout_overflow("xorb payload capacity", self.target_size))?;
            let payload_capacity = self
                .target_size
                .checked_add(self.max_xorb_overshoot)
                .and_then(|bytes| bytes.checked_add(metadata_reserve))
                .ok_or_else(|| xorb_layout_overflow("xorb payload capacity", self.target_size))?;
            let reserve = if reserve_full_payload || !self.has_finalized_xorb {
                payload_capacity
            } else {
                compressed_len
            };
            self.payload.reserve(reserve);
        }
        self.payload.extend_from_slice(compressed);
        self.staged.push(StagedChunk {
            hash: chunk.hash,
            uncompressed_len,
            compressed_len,
            scheme,
        });

        // Record compression metrics.
        if let Some(ref m) = self.metrics {
            match scheme {
                CompressionScheme::LZ4 => m.add_chunks_compressed(1),
                CompressionScheme::ByteGrouping4LZ4 => m.add_chunks_bg4_transformed(1),
                CompressionScheme::None => m.add_chunks_stored_raw(1),
                _ => {}
            }
            let uncompressed = chunk.data.len() as u64;
            if uncompressed > compressed_len as u64 {
                m.add_compression_bytes_saved(uncompressed - compressed_len as u64);
            }
        }

        if rolled {
            Ok(PackOutcome::RolledOver)
        } else {
            Ok(PackOutcome::Packed)
        }
    }

    /// Pushes a batch of chunks, parallelising compression across rayon workers.
    ///
    /// Produces the same completed xorbs, placements, and run-continuity
    /// state as feeding the batch one-by-one to [`push`](Self::push).
    /// Scheme selection and compression run inside rayon workers; state
    /// mutation (boundary decisions, seen-set, placement tracking) runs
    /// serially in input order inside [`push_precompressed`](Self::push_precompressed).
    ///
    /// Falls back to the serial path when the batch has fewer than 4 new
    /// chunks or when the rayon thread pool has fewer than 2 worker
    /// threads — parallel dispatch doesn't pay for its overhead at those
    /// sizes.
    ///
    /// # Errors
    ///
    /// Propagates [`XetError::Compress`] from compression failures and
    /// from [`push_precompressed`](Self::push_precompressed).
    #[tracing::instrument(
        level = "info",
        name = "xorb.pack",
        skip_all,
        fields(parallel_compress, reason)
    )]
    pub fn push_batch(&mut self, batch: &[(Chunk, RunId)]) -> Result<()> {
        for (i, scheme, compressed) in self.compress_batch(batch)? {
            let (chunk, run_id) = &batch[i];
            let compressed = compressed.as_deref().unwrap_or(chunk.data.as_ref());
            self.push_precompressed_with_reserve(chunk, scheme, compressed, *run_id, true)?;
        }

        Ok(())
    }

    /// Pushes a compressed batch with bounded handoff around each rollover.
    ///
    /// `admit` returns an ownership token immediately before sealing. `consume`
    /// receives that token and completed xorb immediately after sealing, before
    /// the next chunk is processed. This prevents one batch from retaining
    /// multiple admitted serialized results.
    ///
    /// # Errors
    ///
    /// Propagates admission, compression, handoff, and xorb-layout failures.
    /// Returns an internal error if the caller left an older completed xorb
    /// undrained.
    pub fn push_batch_with_rollover_admission<T, E, A, C>(
        &mut self,
        batch: &[(Chunk, RunId)],
        mut admit: A,
        mut consume: C,
    ) -> std::result::Result<(), E>
    where
        A: FnMut() -> std::result::Result<T, E>,
        C: FnMut(XorbResult, T) -> std::result::Result<(), E>,
        E: From<XetError>,
    {
        if !self.completed.is_empty() {
            return Err(E::from(XetError::Internal(
                "rollover admission requires previously completed xorbs to be drained".to_owned(),
            )));
        }
        self.adopt_recycled_payload();
        for (i, scheme, compressed) in self.compress_batch(batch)? {
            let (chunk, run_id) = &batch[i];
            let compressed = compressed.as_deref().unwrap_or(chunk.data.as_ref());
            let mut admission = None;
            let mut before_rollover = || -> std::result::Result<(), E> {
                if admission.is_some() {
                    return Err(E::from(XetError::Internal(
                        "one chunk attempted more than one admitted rollover".to_owned(),
                    )));
                }
                admission = Some(admit()?);
                Ok(())
            };
            self.push_precompressed_with_reserve_and_admission(
                chunk,
                scheme,
                compressed,
                *run_id,
                false,
                &mut before_rollover,
            )?;
            match (self.completed.pop(), admission) {
                (Some(result), Some(token)) if self.completed.is_empty() => {
                    consume(result, token)?;
                }
                (None, None) => {}
                (result, token) => {
                    return Err(E::from(XetError::Internal(format!(
                        "rollover admission/result mismatch: result={}, admission={}, remaining={}",
                        result.is_some(),
                        token.is_some(),
                        self.completed.len()
                    ))));
                }
            }
        }
        Ok(())
    }

    fn finalize_current_after_admission<F, E>(
        &mut self,
        before_rollover: &mut F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut() -> std::result::Result<(), E>,
        E: From<XetError>,
    {
        before_rollover()?;
        // The writer releases admission only after pool return. Recheck here
        // so a stalled builder cannot miss the allocation it just waited for.
        self.adopt_recycled_payload();
        self.finalize_current()?;
        Ok(())
    }

    fn adopt_recycled_payload(&mut self) {
        if self.payload.is_empty() {
            return;
        }
        let Some(mut recycled) = self.serialized_payload_pool.take() else {
            return;
        };
        if recycled.capacity() <= self.payload.capacity() {
            self.serialized_payload_pool.recycle_vec(recycled);
            return;
        }
        recycled.extend_from_slice(&self.payload);
        let displaced = std::mem::replace(&mut self.payload, recycled);
        self.serialized_payload_pool.recycle_vec(displaced);
    }

    fn compress_batch(&self, batch: &[(Chunk, RunId)]) -> Result<Vec<CompressedBatchChunk>> {
        use rayon::prelude::*;

        if batch.is_empty() {
            let span = tracing::Span::current();
            span.record("parallel_compress", false);
            span.record("reason", "empty_batch");
            return Ok(Vec::new());
        }

        let mut batch_seen = HashSet::with_capacity(batch.len());
        let kept = batch
            .iter()
            .enumerate()
            .filter_map(|(i, (chunk, _))| {
                if self.seen.contains(&chunk.hash) || !batch_seen.insert(chunk.hash) {
                    None
                } else {
                    Some(i)
                }
            })
            .collect::<Vec<_>>();

        let rayon_threads = rayon::current_num_threads();
        let parallel = kept.len() >= 4 && rayon_threads >= 2;
        let span = tracing::Span::current();
        span.record("parallel_compress", parallel);
        span.record(
            "reason",
            if parallel {
                "parallel"
            } else if rayon_threads < 2 {
                "rayon_unavailable"
            } else {
                "batch_too_small"
            },
        );

        let policy = Arc::clone(&self.compression_policy);
        let compress = |&i: &usize| {
            let chunk = &batch[i].0;
            let scheme = policy.select(&chunk.data);
            let data = match scheme {
                CompressionScheme::None => None,
                _ => Some(compress_chunk(&chunk.data, scheme)?),
            };
            Ok::<_, XetError>((i, scheme, data))
        };
        if parallel {
            kept.par_iter().map(compress).collect()
        } else {
            kept.iter().map(compress).collect()
        }
    }

    /// Finalizes the builder, returning all completed xorbs with placement info.
    ///
    /// Any remaining staged chunks are packed into a final xorb.
    ///
    /// # Errors
    ///
    /// Returns [`XetError::Layout`] if chunk metadata cannot be
    /// represented by the current xorb layout.
    pub fn finalize(mut self) -> Result<Vec<XorbResult>> {
        self.adopt_recycled_payload();
        if !self.staged.is_empty() {
            self.finalize_current()?;
        }
        Ok(self.completed)
    }

    /// Returns the number of completed xorbs so far (not counting staged chunks).
    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.completed.len()
    }

    /// Returns the number of chunks currently staged (not yet finalized).
    #[must_use]
    pub fn staged_count(&self) -> usize {
        self.staged.len()
    }

    /// Returns `true` if there is at least one completed xorb ready to take.
    ///
    /// Used by the streaming pipeline to check after each [`push`](Self::push)
    /// whether a xorb rolled over and can be sent to the upload channel.
    #[must_use]
    pub fn has_completed_xorb(&self) -> bool {
        !self.completed.is_empty()
    }

    /// Returns a mutable reference to the defrag prevention estimator.
    ///
    /// Used by the split decision logic to query whether dedup should be
    /// allowed on the next range.
    pub fn defrag_prevention_mut(&mut self) -> &mut DefragPrevention {
        &mut self.defrag_prevention
    }

    /// Returns `true` if the builder is in extension mode — past the target
    /// size but continuing to pack chunks for run continuity.
    #[must_use]
    pub fn in_extension(&self) -> bool {
        self.in_extension
    }

    /// Removes and returns the oldest completed xorb, if any.
    ///
    /// This is non-destructive to the builder's ongoing state — staged chunks
    /// and dedup tracking are unaffected. The existing [`finalize`](Self::finalize)
    /// method continues to work after completed xorbs have been taken.
    pub fn take_completed(&mut self) -> Option<XorbResult> {
        if self.completed.is_empty() {
            None
        } else {
            Some(self.completed.remove(0))
        }
    }

    /// Finalizes the current staged chunks into a completed xorb.
    ///
    /// The serialized format is:
    /// ```text
    /// [compressed_chunk_0][compressed_chunk_1]...[compressed_chunk_N]
    /// [chunk_meta_0: hash(32) + offset(4) + compressed_len(4) + uncompressed_len(4) + scheme(1)]...
    /// [footer: num_chunks(4) + meta_offset(8) + payload_digest(32) + magic(4)]
    /// ```
    fn finalize_current(&mut self) -> Result<()> {
        if self.staged.is_empty() {
            return Ok(());
        }

        let num_meta_entries = self.staged.len();
        let num_chunks = u32::try_from(num_meta_entries)
            .map_err(|_| xorb_layout_overflow("chunk count", num_meta_entries))?;
        let metadata_size = num_meta_entries
            .checked_mul(CHUNK_META_ENTRY_SIZE)
            .and_then(|size| size.checked_add(FOOTER_SIZE))
            .ok_or_else(|| xorb_layout_overflow("metadata size", num_meta_entries))?;
        let total_compressed = self.staged.iter().try_fold(0usize, |total, chunk| {
            total
                .checked_add(chunk.compressed_len)
                .ok_or_else(|| xorb_layout_overflow("compressed data size", chunk.compressed_len))
        })?;
        let _total_uncompressed = self.staged.iter().try_fold(0u32, |total, chunk| {
            let len = u32::try_from(chunk.uncompressed_len).map_err(|_| {
                xorb_layout_overflow("uncompressed chunk length", chunk.uncompressed_len)
            })?;
            total.checked_add(len).ok_or_else(|| {
                xorb_layout_overflow("uncompressed xorb size", chunk.uncompressed_len)
            })
        })?;
        let data_capacity = total_compressed
            .checked_add(metadata_size)
            .ok_or_else(|| xorb_layout_overflow("xorb object size", total_compressed))?;

        let mut offset: u32 = 0;
        let mut chunk_metas: Vec<(MerkleHash, u32, u32, u32, CompressionScheme)> =
            Vec::with_capacity(num_meta_entries);
        for chunk in &self.staged {
            let compressed_len = u32::try_from(chunk.compressed_len).map_err(|_| {
                xorb_layout_overflow("compressed chunk length", chunk.compressed_len)
            })?;
            let uncompressed_len = u32::try_from(chunk.uncompressed_len).map_err(|_| {
                xorb_layout_overflow("uncompressed chunk length", chunk.uncompressed_len)
            })?;
            chunk_metas.push((
                chunk.hash,
                offset,
                compressed_len,
                uncompressed_len,
                chunk.scheme,
            ));
            let next_offset = u64::from(offset) + u64::from(compressed_len);
            offset = offset
                .checked_add(compressed_len)
                .ok_or_else(|| xorb_layout_overflow("compressed data offset", next_offset))?;
        }

        // Compute xorb hash from chunk (hash, size) pairs.
        let hash_pairs: Vec<(MerkleHash, u64)> = self
            .staged
            .iter()
            .map(|c| (c.hash, c.uncompressed_len))
            .collect();
        let hash = xorb_hash(&hash_pairs);

        if self.payload.len() != total_compressed {
            return Err(XetError::Internal(format!(
                "xorb compressed payload length {} does not match staged metadata {total_compressed}",
                self.payload.len()
            )));
        }
        self.payload.reserve(data_capacity - total_compressed);
        let mut data = std::mem::take(&mut self.payload);
        self.staged.clear();
        let payload_digest = *blake3::hash(&data).as_bytes();

        // Write chunk metadata entries.
        let meta_offset = data.len() as u64;
        for (hash_val, off, comp_len, uncomp_len, scheme) in &chunk_metas {
            let hash_bytes: [u8; 32] = (*hash_val).into();
            data.extend_from_slice(&hash_bytes);
            data.extend_from_slice(&off.to_le_bytes());
            data.extend_from_slice(&comp_len.to_le_bytes());
            data.extend_from_slice(&uncomp_len.to_le_bytes());
            data.push(*scheme as u8);
        }

        // Write footer with the digest of the exact serialized payload region.
        data.extend_from_slice(&num_chunks.to_le_bytes());
        data.extend_from_slice(&meta_offset.to_le_bytes());
        data.extend_from_slice(&payload_digest);
        data.extend_from_slice(XORB_MAGIC);

        self.current_size = 0;
        self.current_uncompressed_size = 0;
        self.current_run = None;
        self.current_run_size = 0;
        self.in_extension = false;
        // Record the final run's chunk count before resetting.
        if self.current_run_chunks > 0 {
            self.defrag_prevention
                .add_range_to_fragmentation_estimate(self.current_run_chunks);
        }
        self.current_run_chunks = 0;

        // Fill in the real xorb hash for all pending placements.
        let mut placements = std::mem::take(&mut self.pending_placements);
        for p in &mut placements {
            p.xorb_hash = hash;
        }

        self.has_finalized_xorb = true;
        self.completed.push(XorbResult {
            bytes: Bytes::from(data),
            hash,
            payload_digest,
            placements,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    use bytes::Bytes;

    use super::*;

    #[derive(Default)]
    struct TestCompressionMetrics {
        chunks_compressed: AtomicU64,
        chunks_bg4_transformed: AtomicU64,
        chunks_stored_raw: AtomicU64,
        compression_bytes_saved: AtomicU64,
    }

    #[derive(Debug, Clone, Copy)]
    struct TestCompressionMetricsSnapshot {
        chunks_compressed: u64,
        chunks_bg4_transformed: u64,
        chunks_stored_raw: u64,
        compression_bytes_saved: u64,
    }

    impl TestCompressionMetrics {
        fn snapshot(&self) -> TestCompressionMetricsSnapshot {
            TestCompressionMetricsSnapshot {
                chunks_compressed: self.chunks_compressed.load(Relaxed),
                chunks_bg4_transformed: self.chunks_bg4_transformed.load(Relaxed),
                chunks_stored_raw: self.chunks_stored_raw.load(Relaxed),
                compression_bytes_saved: self.compression_bytes_saved.load(Relaxed),
            }
        }
    }

    impl CompressionMetrics for TestCompressionMetrics {
        fn add_chunks_compressed(&self, n: u64) {
            self.chunks_compressed.fetch_add(n, Relaxed);
        }

        fn add_chunks_bg4_transformed(&self, n: u64) {
            self.chunks_bg4_transformed.fetch_add(n, Relaxed);
        }

        fn add_chunks_stored_raw(&self, n: u64) {
            self.chunks_stored_raw.fetch_add(n, Relaxed);
        }

        fn add_compression_bytes_saved(&self, n: u64) {
            self.compression_bytes_saved.fetch_add(n, Relaxed);
        }
    }

    #[derive(Default)]
    struct CountingPolicy {
        calls: AtomicU64,
    }

    impl CompressionPolicy for CountingPolicy {
        fn select(&self, _chunk_data: &[u8]) -> CompressionScheme {
            self.calls.fetch_add(1, Relaxed);
            CompressionScheme::None
        }
    }

    /// Helper: create a chunk with deterministic data from a seed.
    fn make_chunk(seed: u32, size: usize) -> Chunk {
        let data: Vec<u8> = (0..size as u32)
            .map(|i| (i.wrapping_mul(seed.wrapping_mul(2654435761))) as u8)
            .collect();
        Chunk::new(Bytes::from(data))
    }

    fn push_batch_and_collect(
        builder: &mut XorbBuilder,
        batch: &[(Chunk, RunId)],
    ) -> Result<Vec<XorbResult>> {
        let mut completed = Vec::new();
        builder.push_batch_with_rollover_admission(
            batch,
            || Ok::<_, XetError>(()),
            |result, ()| {
                completed.push(result);
                Ok(())
            },
        )?;
        Ok(completed)
    }

    /// Helper: create a high-entropy chunk that resists compression.
    fn make_incompressible_chunk(seed: u32, size: usize) -> Chunk {
        let data: Vec<u8> = (0..size)
            .map(|j| {
                let v = (j as u32).wrapping_add(seed.wrapping_mul(0x9E3779B9));
                let v = v ^ (v >> 16);
                let v = v.wrapping_mul(0x45d9f3b);
                let v = v ^ (v >> 16);
                v as u8
            })
            .collect();
        Chunk::new(Bytes::from(data))
    }

    #[test]
    fn single_chunk_produces_one_xorb() {
        let mut builder = XorbBuilder::new();
        let chunk = make_chunk(1, 1024);
        let outcome = builder.push(&chunk, RunId(0)).unwrap();
        assert_eq!(outcome, PackOutcome::Packed);

        let xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1);
        assert_eq!(xorbs[0].placements.len(), 1);
        assert!(!xorbs[0].bytes.is_empty());
    }

    #[test]
    fn duplicate_chunk_is_deduped() {
        let mut builder = XorbBuilder::new();
        let chunk = make_chunk(42, 2048);

        assert_eq!(builder.push(&chunk, RunId(0)).unwrap(), PackOutcome::Packed);
        assert_eq!(
            builder.push(&chunk, RunId(0)).unwrap(),
            PackOutcome::Deduped
        );

        let xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1);
        assert_eq!(xorbs[0].placements.len(), 1);
    }

    #[test]
    fn push_batch_compresses_duplicate_hash_once() {
        let policy = Arc::new(CountingPolicy::default());
        let builder_policy: Arc<dyn CompressionPolicy> = policy.clone();
        let mut builder = XorbBuilder::with_policy(builder_policy);
        let repeated = make_chunk(42, 2048);
        let other = make_chunk(43, 2048);
        let batch = vec![
            (repeated.clone(), RunId(0)),
            (repeated.clone(), RunId(0)),
            (other, RunId(0)),
            (repeated.clone(), RunId(0)),
            (repeated, RunId(0)),
        ];

        builder.push_batch(&batch).expect("push batch");
        assert_eq!(policy.calls.load(Relaxed), 2);

        let xorbs = builder.finalize().expect("finalize");
        assert_eq!(xorbs.len(), 1);
        assert_eq!(xorbs[0].placements.len(), 2);
    }

    #[test]
    fn raw_batch_reuses_source_bytes_without_compression_allocations() {
        let policy =
            Arc::new(FixedCompression::new(CompressionScheme::None)) as Arc<dyn CompressionPolicy>;
        let builder = XorbBuilder::with_policy(policy);
        let batch = (1_u32..=4)
            .map(|seed| (make_chunk(seed, 1024), RunId(0)))
            .collect::<Vec<_>>();

        let compressed = builder.compress_batch(&batch).unwrap();

        assert!(
            compressed
                .iter()
                .all(|(_, scheme, payload)| *scheme == CompressionScheme::None && payload.is_none())
        );
    }

    #[test]
    fn rollover_when_target_exceeded() {
        let mut builder = XorbBuilder::new();
        let run = RunId(1);
        let chunk_size = 1024 * 1024;
        let mut rolled = false;

        for i in 0..200u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            let outcome = builder.push(&chunk, run).unwrap();
            if outcome == PackOutcome::RolledOver {
                rolled = true;
                break;
            }
        }
        assert!(rolled, "expected a rollover before 200 chunks");

        let xorbs = builder.finalize().unwrap();
        assert!(xorbs.len() >= 2);
    }

    #[test]
    fn xorb_hash_is_deterministic() {
        let build = || {
            let mut b = XorbBuilder::new();
            for i in 0..5u32 {
                b.push(&make_chunk(i, 4096), RunId(0)).unwrap();
            }
            b.finalize().unwrap()
        };

        let xorbs_a = build();
        let xorbs_b = build();
        assert_eq!(xorbs_a.len(), xorbs_b.len());
        for (a, b) in xorbs_a.iter().zip(xorbs_b.iter()) {
            assert_eq!(a.hash, b.hash);
            assert_eq!(a.placements.len(), b.placements.len());
        }
    }

    #[test]
    fn serialized_xorb_fixture_is_stable() {
        let policy: Arc<dyn CompressionPolicy> =
            Arc::new(FixedCompression::new(CompressionScheme::None));
        let mut builder = XorbBuilder::with_policy(policy);
        for i in 0..20u32 {
            builder
                .push(&make_incompressible_chunk(i, 32 * 1024), RunId(7))
                .unwrap();
        }

        let xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1);
        assert_eq!(
            blake3::hash(&xorbs[0].bytes).to_hex().to_string(),
            "a20b0fba10c3a7c73f9e0305f7ceff63d87ce05d4366e1721a0ef6a2a9b703a4"
        );
    }

    #[test]
    fn compressed_data_decompresses_via_parser() {
        let original_data = vec![42u8; 8192];
        let chunk = Chunk::new(Bytes::from(original_data.clone()));

        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0)).unwrap();
        let xorbs = builder.finalize().unwrap();

        // The xorb now includes metadata footer, so we parse it properly.
        let parsed = crate::xorb::parser::XorbParser::parse(xorbs[0].bytes.clone())
            .expect("parse should succeed");
        let recovered = parsed.get_chunk(0).expect("get_chunk should succeed");
        assert_eq!(recovered.data.as_ref(), &original_data[..]);
    }

    #[test]
    fn push_records_pending_placements() {
        let mut builder = XorbBuilder::new();
        let c0 = make_chunk(10, 2048);
        let c1 = make_chunk(11, 4096);
        let c2 = make_chunk(12, 1024);

        builder.push(&c0, RunId(0)).unwrap();
        builder.push(&c1, RunId(0)).unwrap();
        builder.push(&c2, RunId(1)).unwrap();

        assert_eq!(builder.pending_placements.len(), 3);

        // Verify chunk_index is 0-based and sequential.
        for (i, p) in builder.pending_placements.iter().enumerate() {
            assert_eq!(p.chunk_index, i as u32);
            assert_eq!(p.xorb_hash, MerkleHash::default());
        }

        // Verify chunk hashes match.
        assert_eq!(builder.pending_placements[0].chunk_hash, c0.hash);
        assert_eq!(builder.pending_placements[1].chunk_hash, c1.hash);
        assert_eq!(builder.pending_placements[2].chunk_hash, c2.hash);

        // Verify uncompressed sizes.
        assert_eq!(builder.pending_placements[0].uncompressed_size, 2048);
        assert_eq!(builder.pending_placements[1].uncompressed_size, 4096);
        assert_eq!(builder.pending_placements[2].uncompressed_size, 1024);
    }

    #[test]
    fn deduped_chunk_does_not_record_placement() {
        let mut builder = XorbBuilder::new();
        let chunk = make_chunk(42, 2048);

        builder.push(&chunk, RunId(0)).unwrap();
        builder.push(&chunk, RunId(0)).unwrap(); // deduped

        assert_eq!(builder.pending_placements.len(), 1);
    }

    #[test]
    fn finalize_fills_xorb_hash_in_placements() {
        let mut builder = XorbBuilder::new();
        let c0 = make_chunk(10, 2048);
        let c1 = make_chunk(11, 4096);
        let c2 = make_chunk(12, 1024);

        builder.push(&c0, RunId(0)).unwrap();
        builder.push(&c1, RunId(0)).unwrap();
        builder.push(&c2, RunId(1)).unwrap();

        let xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1);

        let result = &xorbs[0];
        assert_ne!(result.hash, MerkleHash::default());
        assert_eq!(result.placements.len(), 3);

        // Every placement should have the real xorb hash, not the placeholder.
        for p in &result.placements {
            assert_eq!(p.xorb_hash, result.hash);
        }

        // Verify chunk hashes and indices are preserved.
        assert_eq!(result.placements[0].chunk_hash, c0.hash);
        assert_eq!(result.placements[1].chunk_hash, c1.hash);
        assert_eq!(result.placements[2].chunk_hash, c2.hash);
        for (i, p) in result.placements.iter().enumerate() {
            assert_eq!(p.chunk_index, i as u32);
        }
    }

    #[test]
    fn rollover_clears_pending_placements() {
        let mut builder = XorbBuilder::new();
        let run = RunId(1);
        let chunk_size = 1024 * 1024;

        for i in 0..200u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            let outcome = builder.push(&chunk, run).unwrap();
            if outcome == PackOutcome::RolledOver {
                // After rollover, pending_placements should only contain
                // the chunk that started the new xorb.
                assert_eq!(builder.pending_placements.len(), 1);
                assert_eq!(builder.pending_placements[0].chunk_index, 0);
                return;
            }
        }
        panic!("expected a rollover before 200 chunks");
    }

    #[test]
    fn rollover_produces_xorb_results_with_correct_placements() {
        let mut builder = XorbBuilder::new();
        let run = RunId(1);
        let chunk_size = 1024 * 1024;

        for i in 0..200u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            let outcome = builder.push(&chunk, run).unwrap();
            if outcome == PackOutcome::RolledOver {
                // The completed xorb should have placements with the real hash.
                let taken = builder.take_completed().unwrap();
                assert_ne!(taken.hash, MerkleHash::default());
                assert!(!taken.placements.is_empty());
                for (idx, p) in taken.placements.iter().enumerate() {
                    assert_eq!(p.xorb_hash, taken.hash);
                    assert_eq!(p.chunk_index, idx as u32);
                }

                // Push a couple more and finalize.
                for j in 200..203u32 {
                    builder
                        .push(&make_incompressible_chunk(j, chunk_size), run)
                        .unwrap();
                }
                let remaining = builder.finalize().unwrap();
                assert!(!remaining.is_empty());
                for result in &remaining {
                    assert_ne!(result.hash, MerkleHash::default());
                    for (idx, p) in result.placements.iter().enumerate() {
                        assert_eq!(p.xorb_hash, result.hash);
                        assert_eq!(p.chunk_index, idx as u32);
                    }
                }
                return;
            }
        }
        panic!("expected a rollover before 200 chunks");
    }

    #[test]
    fn empty_builder_produces_no_xorbs() {
        let builder = XorbBuilder::new();
        let xorbs = builder.finalize().unwrap();
        assert!(xorbs.is_empty());
    }

    #[test]
    fn run_continuity_keeps_small_runs_together() {
        let mut builder = XorbBuilder::new();

        // Small run A: 10 KiB total (well under 1 MiB MIN_RUN_SIZE).
        for i in 0..10u32 {
            let outcome = builder.push(&make_chunk(i, 1024), RunId(1)).unwrap();
            assert_eq!(outcome, PackOutcome::Packed);
        }

        // Switch to run B — should still pack (run A was small).
        let outcome = builder.push(&make_chunk(100, 1024), RunId(2)).unwrap();
        assert_eq!(outcome, PackOutcome::Packed);

        let xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1);
        assert_eq!(xorbs[0].placements.len(), 11);
    }

    #[test]
    fn multiple_runs_in_single_xorb() {
        let mut builder = XorbBuilder::new();

        for i in 0..3u32 {
            builder.push(&make_chunk(i, 4096), RunId(1)).unwrap();
        }
        for i in 10..13u32 {
            builder.push(&make_chunk(i, 4096), RunId(2)).unwrap();
        }

        let xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1);
        assert_eq!(xorbs[0].placements.len(), 6);
    }

    #[test]
    fn has_completed_xorb_false_when_no_rollover() {
        let mut builder = XorbBuilder::new();
        assert!(!builder.has_completed_xorb());

        builder.push(&make_chunk(1, 1024), RunId(0)).unwrap();
        assert!(!builder.has_completed_xorb());
    }

    #[test]
    fn take_completed_returns_none_when_empty() {
        let mut builder = XorbBuilder::new();
        assert!(builder.take_completed().is_none());

        builder.push(&make_chunk(1, 1024), RunId(0)).unwrap();
        assert!(builder.take_completed().is_none());
    }

    #[test]
    fn take_completed_returns_xorb_after_rollover() {
        let mut builder = XorbBuilder::new();
        let run = RunId(1);
        let chunk_size = 1024 * 1024;

        // Push incompressible chunks until rollover.
        for i in 0..200u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            let outcome = builder.push(&chunk, run).unwrap();
            if outcome == PackOutcome::RolledOver {
                break;
            }
        }

        assert!(builder.has_completed_xorb());
        let xorb = builder.take_completed().unwrap();
        assert!(!xorb.placements.is_empty());
        assert!(!xorb.bytes.is_empty());

        // After taking, no more completed xorbs.
        assert!(!builder.has_completed_xorb());
        assert!(builder.take_completed().is_none());
    }

    #[test]
    fn serialized_payload_pool_reuses_allocation_across_builders() {
        let pool = SerializedPayloadPool::new(1);
        let mut first = XorbBuilder::new()
            .with_size_bounds(8, 8)
            .with_max_overshoot(0)
            .with_serialized_payload_pool(pool.clone());
        let chunk = Chunk {
            hash: MerkleHash::from([4; 32]),
            data: Bytes::from(vec![4; 8]),
        };
        first
            .push_precompressed(&chunk, CompressionScheme::None, vec![4; 8], RunId(0))
            .unwrap();
        let serialized_capacity = first.payload.capacity();
        let completed = first.finalize().unwrap().pop().unwrap();
        assert!(pool.recycle_serialized_bytes(completed.bytes));

        let mut second = XorbBuilder::new()
            .with_size_bounds(8, 8)
            .with_max_overshoot(0)
            .with_serialized_payload_pool(pool);
        second
            .push_precompressed(&chunk, CompressionScheme::None, vec![4; 8], RunId(0))
            .unwrap();

        assert!(second.payload.capacity() >= serialized_capacity);
    }

    #[test]
    fn batch_boundary_adopts_larger_recycled_payload() {
        let pool = SerializedPayloadPool::new(1);
        let policy =
            Arc::new(FixedCompression::new(CompressionScheme::None)) as Arc<dyn CompressionPolicy>;
        let mut builder = XorbBuilder::with_policy(policy)
            .with_size_bounds(8, 8)
            .with_max_overshoot(0)
            .with_serialized_payload_pool(pool.clone());
        let batch = (1_u8..=2)
            .map(|byte| {
                (
                    Chunk {
                        hash: MerkleHash::from([byte; 32]),
                        data: Bytes::from(vec![byte; 8]),
                    },
                    RunId(0),
                )
            })
            .collect::<Vec<_>>();

        let completed = push_batch_and_collect(&mut builder, &batch)
            .unwrap()
            .pop()
            .unwrap();
        let partial_capacity = builder.payload.capacity();
        let partial_ptr = builder.payload.as_ptr();
        assert!(pool.recycle_serialized_bytes(completed.bytes));

        assert!(
            push_batch_and_collect(&mut builder, &[])
                .unwrap()
                .is_empty()
        );
        assert!(builder.payload.capacity() > partial_capacity);
        assert_eq!(builder.payload, vec![2; 8]);
        let displaced = pool.take().unwrap();
        assert_eq!(displaced.as_ptr(), partial_ptr);
        assert_eq!(displaced.capacity(), partial_capacity);
    }

    #[test]
    fn batch_drain_matches_serial_xorb_bytes_and_placements() {
        let policy =
            Arc::new(FixedCompression::new(CompressionScheme::None)) as Arc<dyn CompressionPolicy>;
        let batch = (1_u8..=3)
            .map(|byte| {
                (
                    Chunk {
                        hash: MerkleHash::from([byte; 32]),
                        data: Bytes::from(vec![byte; 8]),
                    },
                    RunId(0),
                )
            })
            .collect::<Vec<_>>();
        let mut drained = XorbBuilder::with_policy(Arc::clone(&policy))
            .with_size_bounds(8, 8)
            .with_max_overshoot(0);
        let mut serial = XorbBuilder::with_policy(policy)
            .with_size_bounds(8, 8)
            .with_max_overshoot(0);

        let mut actual = push_batch_and_collect(&mut drained, &batch).unwrap();
        actual.extend(drained.finalize().unwrap());
        for (chunk, run_id) in &batch {
            serial.push(chunk, *run_id).unwrap();
        }
        let expected = serial.finalize().unwrap();

        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected.iter()) {
            assert_eq!(actual.hash, expected.hash);
            assert_eq!(actual.bytes, expected.bytes);
            assert_eq!(actual.payload_digest, expected.payload_digest);
            assert_eq!(actual.placements.len(), expected.placements.len());
            for (actual, expected) in actual.placements.iter().zip(&expected.placements) {
                assert_eq!(actual.chunk_hash, expected.chunk_hash);
                assert_eq!(actual.xorb_hash, expected.xorb_hash);
                assert_eq!(actual.chunk_index, expected.chunk_index);
                assert_eq!(actual.uncompressed_size, expected.uncompressed_size);
            }
        }
    }

    #[test]
    fn rollover_admission_tokens_stay_paired_with_completed_xorbs() {
        let policy =
            Arc::new(FixedCompression::new(CompressionScheme::None)) as Arc<dyn CompressionPolicy>;
        let batch = (1_u8..=3)
            .map(|byte| {
                (
                    Chunk {
                        hash: MerkleHash::from([byte; 32]),
                        data: Bytes::from(vec![byte; 8]),
                    },
                    RunId(0),
                )
            })
            .collect::<Vec<_>>();
        let mut builder = XorbBuilder::with_policy(policy)
            .with_size_bounds(8, 8)
            .with_max_overshoot(0);
        let mut next_token = 0_u8;
        let permit_available = Cell::new(true);

        let mut admitted = Vec::new();
        builder
            .push_batch_with_rollover_admission(
                &batch,
                || {
                    assert!(permit_available.replace(false));
                    next_token += 1;
                    Ok::<_, XetError>(next_token)
                },
                |result, token| {
                    assert!(!permit_available.replace(true));
                    admitted.push((result, token));
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(
            admitted.iter().map(|(_, token)| *token).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(permit_available.get());
        assert_eq!(builder.completed_count(), 0);
        assert_eq!(builder.staged_count(), 1);
        assert_eq!(builder.payload.capacity(), 8);
    }

    #[test]
    fn rollover_admission_reserves_initial_payload_once() {
        let policy =
            Arc::new(FixedCompression::new(CompressionScheme::None)) as Arc<dyn CompressionPolicy>;
        let mut builder = XorbBuilder::with_policy(policy)
            .with_size_bounds(64, 64)
            .with_max_overshoot(0);
        let batch = [(make_chunk(1, 8), RunId(0))];

        let completed = push_batch_and_collect(&mut builder, &batch).unwrap();

        assert!(completed.is_empty());
        assert!(builder.payload.capacity() >= 64);
        assert!(!builder.has_finalized_xorb);
    }

    #[test]
    fn rollover_admission_rechecks_pool_after_wait() {
        let pool = SerializedPayloadPool::new(1);
        let policy =
            Arc::new(FixedCompression::new(CompressionScheme::None)) as Arc<dyn CompressionPolicy>;
        let mut builder = XorbBuilder::with_policy(policy)
            .with_size_bounds(8, 8)
            .with_max_overshoot(0)
            .with_serialized_payload_pool(pool.clone());
        let chunks = (1_u8..=3)
            .map(|byte| {
                (
                    Chunk {
                        hash: MerkleHash::from([byte; 32]),
                        data: Bytes::from(vec![byte; 8]),
                    },
                    RunId(0),
                )
            })
            .collect::<Vec<_>>();

        push_batch_and_collect(&mut builder, &chunks[..1]).unwrap();
        let initial_capacity = builder.payload.capacity();
        let mut first = push_batch_and_collect(&mut builder, &chunks[1..2])
            .unwrap()
            .pop();
        let mut second = Vec::new();
        builder
            .push_batch_with_rollover_admission(
                &chunks[2..],
                || {
                    assert!(pool.recycle_serialized_bytes(first.take().unwrap().bytes));
                    Ok::<_, XetError>(())
                },
                |result, ()| {
                    second.push(result);
                    Ok(())
                },
            )
            .unwrap();

        let serialized = second.pop().unwrap().bytes.try_into_mut().unwrap();
        assert!(serialized.capacity() >= initial_capacity);
    }

    #[test]
    fn rollover_admission_failure_leaves_current_xorb_unsealed() {
        let policy =
            Arc::new(FixedCompression::new(CompressionScheme::None)) as Arc<dyn CompressionPolicy>;
        let batch = (1_u8..=2)
            .map(|byte| {
                (
                    Chunk {
                        hash: MerkleHash::from([byte; 32]),
                        data: Bytes::from(vec![byte; 8]),
                    },
                    RunId(0),
                )
            })
            .collect::<Vec<_>>();
        let mut builder = XorbBuilder::with_policy(policy)
            .with_size_bounds(8, 8)
            .with_max_overshoot(0);

        let error = builder
            .push_batch_with_rollover_admission(
                &batch,
                || Err::<(), _>(XetError::Internal("admission denied".to_owned())),
                |_, _| Ok(()),
            )
            .unwrap_err();

        assert!(matches!(error, XetError::Internal(message) if message == "admission denied"));
        assert_eq!(builder.completed_count(), 0);
        assert_eq!(builder.staged_count(), 1);
        let completed = push_batch_and_collect(&mut builder, &batch[1..]).unwrap();
        assert_eq!(completed.len(), 1);
    }

    #[test]
    fn finalize_works_after_take_completed() {
        let mut builder = XorbBuilder::new();
        let run = RunId(1);
        let chunk_size = 1024 * 1024;

        // Push until rollover, then push a few more.
        let mut rolled = false;
        for i in 0..200u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            let outcome = builder.push(&chunk, run).unwrap();
            if outcome == PackOutcome::RolledOver {
                rolled = true;
                // Push a couple more chunks after rollover.
                for j in 200..203u32 {
                    builder
                        .push(&make_incompressible_chunk(j, chunk_size), run)
                        .unwrap();
                }
                break;
            }
        }
        assert!(rolled);

        // Take the completed xorb.
        let taken = builder.take_completed().unwrap();
        assert!(!taken.placements.is_empty());

        // Finalize should produce the remaining staged chunks.
        let remaining = builder.finalize().unwrap();
        assert!(!remaining.is_empty());
        assert!(!remaining[0].placements.is_empty());
    }

    #[test]
    fn fixed_compression_default_returns_lz4() {
        let policy = FixedCompression::default();
        let data = vec![0u8; 4096];
        assert_eq!(policy.select(&data), CompressionScheme::LZ4);
    }

    #[test]
    fn fixed_compression_ignores_chunk_content() {
        let policy = FixedCompression::new(CompressionScheme::None);
        assert_eq!(policy.select(&[0u8; 100]), CompressionScheme::None);
        assert_eq!(policy.select(&[0xFF; 8192]), CompressionScheme::None);
        assert_eq!(policy.select(&[]), CompressionScheme::None);
    }

    #[test]
    fn fixed_compression_custom_scheme() {
        let policy = FixedCompression::new(CompressionScheme::ByteGrouping4LZ4);
        assert_eq!(
            policy.select(&[1, 2, 3]),
            CompressionScheme::ByteGrouping4LZ4
        );
    }

    #[test]
    fn adaptive_compression_empty_data_returns_none() {
        let policy = AdaptiveCompression::default();
        assert_eq!(policy.select(&[]), CompressionScheme::None);
    }

    #[test]
    fn adaptive_compression_compressible_data_returns_lz4() {
        let policy = AdaptiveCompression::default();
        // All-zeros: very low entropy, highly compressible → LZ4.
        let data = vec![0u8; 4096];
        assert_eq!(policy.select(&data), CompressionScheme::LZ4);
    }

    #[test]
    fn adaptive_compression_incompressible_data_returns_none() {
        let policy = AdaptiveCompression::default();
        // Uniform byte distribution: entropy ≈ 1.0 → None.
        let mut data = Vec::with_capacity(4096);
        for _ in 0..16 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        assert_eq!(policy.select(&data), CompressionScheme::None);
    }

    #[test]
    fn adaptive_compression_structured_floats_returns_bg4() {
        let policy = AdaptiveCompression::default();
        // Float32 array with values in a narrow range — BG4 predictor should
        // detect the structured byte pattern and recommend byte grouping.
        let data: Vec<u8> = (0..1024u32)
            .flat_map(|i| {
                let f = (i as f32) * 0.001;
                f.to_le_bytes()
            })
            .collect();
        assert_eq!(policy.select(&data), CompressionScheme::ByteGrouping4LZ4);
    }

    #[test]
    fn adaptive_compression_custom_threshold() {
        // With a very low threshold, even moderately compressible data is stored raw.
        let policy = AdaptiveCompression::new(0.2);
        // Two-value alternating pattern: entropy ≈ 0.125 → below 0.2 → LZ4.
        let mut data = Vec::with_capacity(4096);
        for _ in 0..2048 {
            data.push(0x00);
            data.push(0xFF);
        }
        assert_eq!(policy.select(&data), CompressionScheme::LZ4);

        // Slightly higher entropy data that exceeds the low threshold → None.
        let mut data = Vec::with_capacity(4096);
        for i in 0..4096u16 {
            data.push((i % 8) as u8);
        }
        // 8 distinct values, entropy = 3/8 = 0.375 > 0.2 → None.
        assert_eq!(policy.select(&data), CompressionScheme::None);
    }

    #[test]
    fn adaptive_compression_default_threshold_is_0_95() {
        let policy = AdaptiveCompression::default();
        assert!((policy.entropy_threshold - 0.95).abs() < f32::EPSILON);
    }

    #[test]
    fn compression_metrics_recorded_for_lz4_chunks() {
        let metrics = Arc::new(TestCompressionMetrics::default());
        let mut builder = XorbBuilder::new().with_metrics(Arc::clone(&metrics));

        // All-zeros: highly compressible → LZ4.
        let chunk = Chunk::new(Bytes::from(vec![0u8; 4096]));
        builder.push(&chunk, RunId(0)).unwrap();
        builder.finalize().unwrap();

        let snap = metrics.snapshot();
        assert_eq!(snap.chunks_compressed, 1);
        assert_eq!(snap.chunks_bg4_transformed, 0);
        assert_eq!(snap.chunks_stored_raw, 0);
        assert!(snap.compression_bytes_saved > 0);
    }

    #[test]
    fn compression_metrics_recorded_for_raw_chunks() {
        let metrics = Arc::new(TestCompressionMetrics::default());
        let policy: Box<dyn CompressionPolicy> =
            Box::new(FixedCompression::new(CompressionScheme::None));
        let mut builder = XorbBuilder::with_policy(policy).with_metrics(Arc::clone(&metrics));

        let chunk = make_chunk(1, 4096);
        builder.push(&chunk, RunId(0)).unwrap();
        builder.finalize().unwrap();

        let snap = metrics.snapshot();
        assert_eq!(snap.chunks_compressed, 0);
        assert_eq!(snap.chunks_stored_raw, 1);
        assert_eq!(snap.compression_bytes_saved, 0);
    }

    #[test]
    fn compression_metrics_recorded_for_bg4_chunks() {
        let metrics = Arc::new(TestCompressionMetrics::default());
        let policy: Box<dyn CompressionPolicy> = Box::new(AdaptiveCompression::default());
        let mut builder = XorbBuilder::with_policy(policy).with_metrics(Arc::clone(&metrics));

        // Float32 array with values in a narrow range — BG4 predictor should
        // detect the structured byte pattern.
        let data: Vec<u8> = (0..1024u32)
            .flat_map(|i| {
                let f = (i as f32) * 0.001;
                f.to_le_bytes()
            })
            .collect();
        let chunk = Chunk::new(Bytes::from(data));
        builder.push(&chunk, RunId(0)).unwrap();
        builder.finalize().unwrap();

        let snap = metrics.snapshot();
        assert_eq!(snap.chunks_bg4_transformed, 1);
        assert_eq!(snap.chunks_compressed, 0);
        assert_eq!(snap.chunks_stored_raw, 0);
        assert!(snap.compression_bytes_saved > 0);
    }

    #[test]
    fn compression_metrics_not_recorded_without_metrics() {
        // Builder without metrics should not panic.
        let mut builder = XorbBuilder::new();
        let chunk = make_chunk(1, 4096);
        builder.push(&chunk, RunId(0)).unwrap();
        builder.finalize().unwrap();
    }

    #[test]
    fn defrag_prevention_tracks_run_chunks_on_run_break() {
        let mut builder = XorbBuilder::new();

        // Push chunks in run A with enough compressed bytes to exceed MIN_RUN_SIZE.
        // MIN_RUN_SIZE is 1 MiB, so we need ~1 MiB of compressed data.
        // Use incompressible 256 KiB chunks — 5 of them gives ~1.25 MiB.
        for i in 0..5u32 {
            builder
                .push(&make_incompressible_chunk(i, 256 * 1024), RunId(1))
                .unwrap();
        }
        assert_eq!(builder.current_run_chunks, 5);
        assert!(builder.current_run_size >= MIN_RUN_SIZE);

        // Switch to run B — run A's chunk count should be recorded and counter reset.
        builder
            .push(&make_incompressible_chunk(100, 4096), RunId(2))
            .unwrap();
        assert_eq!(builder.current_run_chunks, 1);
    }

    #[test]
    fn defrag_prevention_records_final_run_on_finalize() {
        let mut builder = XorbBuilder::new();

        // Push 5 chunks in a single run.
        for i in 0..5u32 {
            builder.push(&make_chunk(i, 4096), RunId(1)).unwrap();
        }
        assert_eq!(builder.current_run_chunks, 5);

        // Finalize should record the run and reset.
        let xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1);
    }

    #[test]
    fn defrag_prevention_not_recorded_for_empty_run() {
        let mut builder = XorbBuilder::new();

        // The first push sets current_run from None — no previous run to record.
        builder
            .push(&make_incompressible_chunk(0, 256 * 1024), RunId(1))
            .unwrap();
        assert_eq!(builder.current_run_chunks, 1);

        // Small runs (under MIN_RUN_SIZE) don't trigger a run break, so the
        // builder keeps accumulating into the same logical run even when
        // RunId changes. This is by design — run-continuity keeps small runs
        // together.
        builder
            .push(&make_incompressible_chunk(10, 4096), RunId(2))
            .unwrap();
        // No run break occurred, so chunks accumulate in the same run.
        assert_eq!(builder.current_run_chunks, 2);
    }

    #[test]
    fn defrag_prevention_accessible_via_mut_accessor() {
        let mut builder = XorbBuilder::new();

        // Fill the defrag window so allow_dedup_on_next_range returns a meaningful result.
        for i in 0..10u32 {
            builder
                .defrag_prevention_mut()
                .add_range_to_fragmentation_estimate(20);
            let _ = i;
        }

        // With avg = 20 and default min_chunks_per_range = 16, dedup should be allowed.
        assert!(builder.defrag_prevention_mut().allow_dedup_on_next_range(5));
    }

    #[test]
    fn extension_mode_defers_rollover_when_fragmented() {
        // When DefragPrevention says fragmentation is too high, the builder
        // should extend past TARGET_XORB_SIZE instead of sealing immediately.
        //
        // We use a small overshoot to make the test tractable: set overshoot
        // to 2 MiB so we can observe extension within a few extra chunks.
        let mut builder = XorbBuilder::new().with_max_overshoot(2 * 1024 * 1024);

        // Prime DefragPrevention with small ranges to trigger fragmentation.
        // Window size is 10, min_chunks_per_range = 16, low threshold = 8.
        // Filling with ranges of 2 chunks each → avg = 2, well below threshold.
        for _ in 0..10 {
            builder
                .defrag_prevention_mut()
                .add_range_to_fragmentation_estimate(2);
        }

        let run = RunId(1);
        let chunk_size = 1024 * 1024; // 1 MiB incompressible chunks

        // Push chunks until we exceed TARGET_XORB_SIZE.
        let mut exceeded_target = false;
        for i in 0..200u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            let outcome = builder.push(&chunk, run).unwrap();
            if builder.in_extension() {
                exceeded_target = true;
                // In extension mode — the builder deferred the seal.
                assert_eq!(outcome, PackOutcome::Packed);
                assert!(builder.current_size > TARGET_XORB_SIZE);
                break;
            }
            if outcome == PackOutcome::RolledOver {
                // Shouldn't happen with fragmented defrag state and same run.
                panic!("expected extension mode, got rollover");
            }
        }
        assert!(
            exceeded_target,
            "builder should have entered extension mode"
        );
    }

    #[test]
    fn extension_mode_bounded_by_max_overshoot() {
        // Extension cannot exceed TARGET_XORB_SIZE + max_xorb_overshoot.
        let overshoot = 2 * 1024 * 1024; // 2 MiB
        let mut builder = XorbBuilder::new().with_max_overshoot(overshoot);

        // Prime DefragPrevention with small ranges to trigger fragmentation.
        for _ in 0..10 {
            builder
                .defrag_prevention_mut()
                .add_range_to_fragmentation_estimate(2);
        }

        let run = RunId(1);
        let chunk_size = 1024 * 1024;

        let mut rolled = false;
        for i in 0..200u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            let outcome = builder.push(&chunk, run).unwrap();
            if outcome == PackOutcome::RolledOver {
                rolled = true;
                break;
            }
        }
        assert!(rolled, "builder should eventually seal at the hard limit");

        // The completed xorb should be within the overshoot budget.
        let taken = builder.take_completed().unwrap();
        let max_allowed = TARGET_XORB_SIZE + overshoot;
        assert!(
            taken.bytes.len()
                <= max_allowed + FOOTER_SIZE + taken.placements.len() * CHUNK_META_ENTRY_SIZE,
            "xorb data ({}) should be within overshoot budget",
            taken.bytes.len()
        );
    }

    #[test]
    fn no_extension_when_fragmentation_acceptable() {
        // When DefragPrevention says fragmentation is OK, the builder seals
        // at the target boundary without entering extension mode.
        let mut builder = XorbBuilder::new();

        // Prime DefragPrevention with large ranges — avg = 50, well above threshold.
        for _ in 0..10 {
            builder
                .defrag_prevention_mut()
                .add_range_to_fragmentation_estimate(50);
        }

        let run = RunId(1);
        let chunk_size = 1024 * 1024;

        let mut rolled = false;
        for i in 0..200u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            let outcome = builder.push(&chunk, run).unwrap();
            if outcome == PackOutcome::RolledOver {
                rolled = true;
                assert!(!builder.in_extension());
                break;
            }
            // Should never enter extension mode with healthy fragmentation.
            assert!(!builder.in_extension());
        }
        assert!(rolled, "builder should seal at target boundary");
    }

    #[test]
    fn extension_ends_on_run_break() {
        // When in extension mode and the run changes, the builder should seal.
        let overshoot = 4 * 1024 * 1024; // 4 MiB — generous overshoot
        let mut builder = XorbBuilder::new().with_max_overshoot(overshoot);

        // Prime DefragPrevention with small ranges to trigger fragmentation.
        for _ in 0..10 {
            builder
                .defrag_prevention_mut()
                .add_range_to_fragmentation_estimate(2);
        }

        let chunk_size = 256 * 1024; // 256 KiB

        // Push chunks in run A until we enter extension mode.
        let mut entered_extension = false;
        for i in 0..500u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            builder.push(&chunk, RunId(1)).unwrap();
            if builder.in_extension() {
                entered_extension = true;
                break;
            }
        }
        assert!(entered_extension, "should enter extension mode");

        // Now push enough chunks in run A to exceed MIN_RUN_SIZE so a run
        // break is allowed, then switch to run B.
        // current_run_size is already > MIN_RUN_SIZE since we're past target.
        assert!(builder.current_run_size >= MIN_RUN_SIZE);

        // Switch to run B — should trigger a seal.
        let outcome = builder
            .push(&make_incompressible_chunk(999, chunk_size), RunId(2))
            .unwrap();
        assert_eq!(outcome, PackOutcome::RolledOver);
        assert!(!builder.in_extension());
    }

    #[test]
    fn with_max_overshoot_sets_value() {
        let builder = XorbBuilder::new().with_max_overshoot(1234);
        assert_eq!(builder.max_xorb_overshoot, 1234);
    }

    #[test]
    fn default_max_overshoot_is_ten_percent_of_target() {
        let builder = XorbBuilder::new();
        assert_eq!(builder.max_xorb_overshoot, TARGET_XORB_SIZE / 10);
    }

    #[test]
    fn in_extension_false_initially() {
        let builder = XorbBuilder::new();
        assert!(!builder.in_extension());
    }

    #[test]
    fn default_target_size_is_target_xorb_size() {
        let builder = XorbBuilder::new();
        assert_eq!(builder.target_size, TARGET_XORB_SIZE);
    }

    #[test]
    fn default_size_bounds() {
        let builder = XorbBuilder::new();
        assert_eq!(builder.min_size, MIN_XORB_SIZE);
        assert_eq!(builder.max_size, MAX_XORB_SIZE);
    }

    #[test]
    fn set_target_size_within_bounds() {
        let mut builder = XorbBuilder::new();
        builder.set_target_size(128 * 1024 * 1024);
        assert_eq!(builder.target_size, 128 * 1024 * 1024);
        assert_eq!(builder.max_xorb_overshoot, 128 * 1024 * 1024 / 10);
    }

    #[test]
    fn set_target_size_clamps_below_min() {
        let mut builder = XorbBuilder::new();
        builder.set_target_size(1); // way below MIN_XORB_SIZE
        assert_eq!(builder.target_size, MIN_XORB_SIZE);
        assert_eq!(builder.max_xorb_overshoot, MIN_XORB_SIZE / 10);
    }

    #[test]
    fn set_target_size_clamps_above_max() {
        let mut builder = XorbBuilder::new();
        builder.set_target_size(1024 * 1024 * 1024); // 1 GiB, above MAX_XORB_SIZE
        assert_eq!(builder.target_size, MAX_XORB_SIZE);
        assert_eq!(builder.max_xorb_overshoot, MAX_XORB_SIZE / 10);
    }

    #[test]
    fn with_size_bounds_configures_bounds() {
        let builder = XorbBuilder::new().with_size_bounds(8 * 1024 * 1024, 512 * 1024 * 1024);
        assert_eq!(builder.min_size, 8 * 1024 * 1024);
        assert_eq!(builder.max_size, 512 * 1024 * 1024);
    }

    #[test]
    fn with_size_bounds_reclamps_target() {
        // Start with default target (64 MiB), then set bounds that exclude it.
        let builder = XorbBuilder::new().with_size_bounds(128 * 1024 * 1024, 256 * 1024 * 1024);
        // Target was 64 MiB, now clamped up to min of 128 MiB.
        assert_eq!(builder.target_size, 128 * 1024 * 1024);
        assert_eq!(builder.max_xorb_overshoot, 128 * 1024 * 1024 / 10);
    }

    #[test]
    fn with_size_bounds_min_exceeds_max_uses_min() {
        let builder = XorbBuilder::new().with_size_bounds(200 * 1024 * 1024, 100 * 1024 * 1024);
        // max is clamped to at least min.
        assert_eq!(builder.min_size, 200 * 1024 * 1024);
        assert_eq!(builder.max_size, 200 * 1024 * 1024);
    }

    #[test]
    fn set_target_size_respects_custom_bounds() {
        let mut builder = XorbBuilder::new().with_size_bounds(32 * 1024 * 1024, 128 * 1024 * 1024);
        builder.set_target_size(10 * 1024 * 1024); // below custom min
        assert_eq!(builder.target_size, 32 * 1024 * 1024);

        builder.set_target_size(200 * 1024 * 1024); // above custom max
        assert_eq!(builder.target_size, 128 * 1024 * 1024);

        builder.set_target_size(64 * 1024 * 1024); // within bounds
        assert_eq!(builder.target_size, 64 * 1024 * 1024);
    }

    #[test]
    fn target_size_affects_rollover_threshold() {
        // With a small target size, rollover should happen sooner.
        let small_target = MIN_XORB_SIZE; // 16 MiB
        let mut builder = XorbBuilder::new();
        builder.set_target_size(small_target);

        let run = RunId(1);
        let chunk_size = 1024 * 1024; // 1 MiB incompressible chunks
        let mut rollover_at = None;

        for i in 0..200u32 {
            let chunk = make_incompressible_chunk(i, chunk_size);
            let outcome = builder.push(&chunk, run).unwrap();
            if outcome == PackOutcome::RolledOver {
                rollover_at = Some(i);
                break;
            }
        }

        // With 16 MiB target and 1 MiB chunks, rollover should happen
        // around chunk 16-18 (accounting for overshoot).
        let idx = rollover_at.expect("expected rollover with small target");
        assert!(
            idx < 30,
            "rollover at chunk {idx}, expected sooner with 16 MiB target"
        );
    }

    #[test]
    fn push_rolls_over_before_uncompressed_xorb_size_exceeds_shard_limit() {
        let mut builder = XorbBuilder::new();
        let first = make_chunk(41, 16);

        builder.staged.push(StagedChunk {
            hash: first.hash,
            uncompressed_len: u64::from(u32::MAX),
            compressed_len: 1,
            scheme: CompressionScheme::None,
        });
        builder.payload.push(0xAA);
        builder.current_size = 1;
        builder.current_uncompressed_size = u64::from(u32::MAX);
        builder.current_run = Some(RunId(1));
        builder.current_run_size = 1;
        builder.current_run_chunks = 1;
        builder.seen.insert(first.hash);
        builder.pending_placements.push(ChunkPlacement {
            chunk_hash: first.hash,
            xorb_hash: MerkleHash::default(),
            chunk_index: 0,
            uncompressed_size: u32::MAX,
        });

        let next = make_chunk(42, 16);
        let outcome = builder
            .push_precompressed(&next, CompressionScheme::None, vec![0xBB], RunId(1))
            .expect("push should roll over instead of overflowing metadata");

        assert_eq!(outcome, PackOutcome::RolledOver);
        assert_eq!(builder.completed_count(), 1);
        assert_eq!(builder.staged_count(), 1);
        assert_eq!(builder.pending_placements[0].chunk_index, 0);
        assert_eq!(
            builder.current_uncompressed_size,
            u64::try_from(next.data.len()).unwrap()
        );
    }

    #[test]
    fn finalize_rejects_unrepresentable_chunk_metadata_without_dropping_state() {
        let mut builder = XorbBuilder::new();
        let chunk = make_chunk(42, 16);

        builder.staged.push(StagedChunk {
            hash: chunk.hash,
            uncompressed_len: u64::from(u32::MAX) + 1,
            compressed_len: 1,
            scheme: CompressionScheme::None,
        });
        builder.payload.push(0xAA);
        builder.pending_placements.push(ChunkPlacement {
            chunk_hash: chunk.hash,
            xorb_hash: MerkleHash::default(),
            chunk_index: 0,
            uncompressed_size: 16,
        });

        let err = builder
            .finalize_current()
            .expect_err("oversized chunk metadata should fail");

        assert!(err.to_string().contains("uncompressed chunk length"));
        assert_eq!(builder.staged.len(), 1);
        assert_eq!(builder.pending_placements.len(), 1);
        assert!(builder.completed.is_empty());
    }
}
