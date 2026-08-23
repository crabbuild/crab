//! Clean pipeline: streaming chunker + blake3 + staging + index.db + pointer.
//!
//! The clean path runs at `git add` time. It hashes file content via blake3,
//! chunks it via CDC, stages chunks locally, records them in index.db, and
//! emits a pointer blob. The slow path is local-only; the fast path
//! performs bounded remote metadata/object HEADs before it skips staging.
//!
//! The fast path skips staging for files already known to
//! the file-index: when the file size exceeds `fastpath_min_size` and the
//! session-scoped bloom filter reports a hit, a HEAD request to the
//! file-index confirms existence and the referenced shard object exists.
//! On success the pointer is emitted directly with a `shard-hint`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cache::ShardHintCache;
use crate::core::context::AppContext;
use crate::core::error::{CrabError, Result};
use crate::core::pattern::PatternFilter;
use crate::storage::StoreLayout;
use bytes::Bytes;
use crab_git::lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE};
use crab_git::pointer_detect::{PointerKind, classify};
use crab_lfs::LfsObjectStore;
use crab_staging::StagingArea;
use crab_staging::recipe::{ChunkingPolicyId, FileRecipe, RecipeRecorder};
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use gix_object::Find;

/// Default capacity for the in-memory chunk-buffer ring (32 MiB).
pub const DEFAULT_CHUNK_BUFFER_CAP: usize = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Session-scoped bloom filter
// ---------------------------------------------------------------------------

/// Lightweight bloom filter tracking known `file_hash` values from the local
/// file-index. Populated at session start and consulted on each clean to
/// decide whether the fast path is worth attempting.
///
/// Uses a simple bit-vector with k=3 hash functions derived from the
/// blake3 file hash. False positives are expected and tracked via the
/// `clean_fastpath_false_positives` counter.
#[derive(Debug)]
struct FileHashBloom {
    bits: Vec<u64>,
    num_bits: u64,
}

impl FileHashBloom {
    /// Create a bloom filter sized for `expected_items` with ~1% FPR.
    fn new(expected_items: usize) -> Self {
        // ~10 bits per item for ~1% FPR with k=3.
        let num_bits = ((expected_items as u64).max(64)) * 10;
        let words = num_bits.div_ceil(64) as usize;
        Self {
            bits: vec![0u64; words],
            num_bits,
        }
    }

    /// Insert a file hash into the bloom filter.
    fn insert(&mut self, file_hash: &[u8; 32]) {
        for idx in self.hash_indices(file_hash) {
            let word = (idx / 64) as usize;
            let bit = idx % 64;
            if word < self.bits.len() {
                self.bits[word] |= 1 << bit;
            }
        }
    }

    /// Check if a file hash might be in the bloom filter.
    ///
    /// Returns `false` for definite misses, `true` for possible hits
    /// (which may be false positives).
    fn maybe_contains(&self, file_hash: &[u8; 32]) -> bool {
        for idx in self.hash_indices(file_hash) {
            let word = (idx / 64) as usize;
            let bit = idx % 64;
            if word >= self.bits.len() || (self.bits[word] & (1 << bit)) == 0 {
                return false;
            }
        }
        true
    }

    /// Derive k=3 bit indices from the 32-byte hash.
    fn hash_indices(&self, hash: &[u8; 32]) -> [u64; 3] {
        // Use non-overlapping 8-byte slices from the hash as independent
        // hash values, then reduce modulo num_bits.
        let h1 = u64::from_le_bytes(hash[0..8].try_into().unwrap_or([0; 8]));
        let h2 = u64::from_le_bytes(hash[8..16].try_into().unwrap_or([0; 8]));
        let h3 = u64::from_le_bytes(hash[16..24].try_into().unwrap_or([0; 8]));
        [h1 % self.num_bits, h2 % self.num_bits, h3 % self.num_bits]
    }

    /// Persist the bloom filter to disk so subsequent filter-process
    /// sessions can reuse it without rebuilding from the file-index.
    fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        let parent = path.parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        // Header: 8 bytes for num_bits.
        f.write_all(&self.num_bits.to_le_bytes())?;
        // Body: raw u64 words.
        for word in &self.bits {
            f.write_all(&word.to_le_bytes())?;
        }
        f.flush()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load a previously persisted bloom filter from disk.
    ///
    /// Returns `None` on any I/O or format error — the caller falls
    /// back to an empty bloom.
    fn load(path: &std::path::Path) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        if data.len() < 8 {
            return None;
        }
        let num_bits = u64::from_le_bytes(data[0..8].try_into().ok()?);
        if num_bits == 0 {
            return None;
        }
        let words_bytes = &data[8..];
        if words_bytes.len() % 8 != 0 {
            return None;
        }
        let bits: Vec<u64> = words_bytes
            .chunks_exact(8)
            .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])))
            .collect();
        let expected_words = num_bits.div_ceil(64) as usize;
        if bits.len() != expected_words {
            return None;
        }
        Some(Self { bits, num_bits })
    }
}

// ---------------------------------------------------------------------------
// File-index checker
// ---------------------------------------------------------------------------

/// Result of a file-index lookup for a given file hash.
#[derive(Debug)]
pub struct FileIndexHeadResult {
    /// Whether the file hash exists in the file-index.
    pub exists: bool,
    /// Shard hash hint from the response headers, if available.
    pub shard_hint: Option<[u8; 32]>,
}

/// Trait abstracting the file-index fast-path check for testability.
///
/// The real implementation resolves `file_hash -> shard_hash` through
/// `file_index_db` and verifies the referenced shard object exists.
pub trait FileIndexChecker: Send {
    /// Check whether the file is already reconstructable from remote storage.
    ///
    /// Returns existence + optional shard-hint on success.
    fn head_file_index(&self, file_hash: &[u8; 32]) -> Result<FileIndexHeadResult>;
}

/// No-op checker that always reports files as unknown (disables fast path).
#[derive(Debug, Default)]
struct NoopFileIndexChecker;

impl FileIndexChecker for NoopFileIndexChecker {
    fn head_file_index(&self, _file_hash: &[u8; 32]) -> Result<FileIndexHeadResult> {
        Ok(FileIndexHeadResult {
            exists: false,
            shard_hint: None,
        })
    }
}

/// File-index checker backed by a [`StoreLayout`].
///
/// Routes file-hash existence checks through the per-repo
/// `file_index_db` via
/// [`crab_metadata::file_index_lookup::resolve_file_hash_to_shard`].
/// Uses `Handle::block_on` to bridge the sync [`FileIndexChecker`]
/// trait with the async SlateDB access path — safe because the
/// filter-process loop invokes the checker inside `spawn_blocking`.
pub struct StoreFileIndexChecker {
    router: StoreLayout,
    handle: tokio::runtime::Handle,
    session: std::sync::Mutex<Option<crab_metadata::file_index_lookup::FileIndexLookupSession>>,
}

impl StoreFileIndexChecker {
    /// Create a checker that routes lookups via the given layout.
    ///
    /// `handle` must be a handle to the tokio runtime that owns the store.
    pub fn new(router: StoreLayout, handle: tokio::runtime::Handle) -> Self {
        Self {
            router,
            handle,
            session: std::sync::Mutex::new(None),
        }
    }
}

impl Drop for StoreFileIndexChecker {
    fn drop(&mut self) {
        let session = self
            .session
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(session) = session {
            let handle = self.handle.clone();
            let close = std::thread::Builder::new()
                .name("crab-file-index-close".to_owned())
                .spawn(move || handle.block_on(session.close()));
            match close {
                Ok(thread) => match thread.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(error = %error, "clean file-index reader close failed");
                    }
                    Err(_) => tracing::warn!("clean file-index close worker panicked"),
                },
                Err(error) => {
                    tracing::warn!(error = %error, "failed to start clean file-index close worker");
                }
            }
        }
    }
}

// SAFETY: StoreLayout is Send (Arc-wrapped internals), and Handle is Send+Sync.
unsafe impl Send for StoreFileIndexChecker {}

impl FileIndexChecker for StoreFileIndexChecker {
    fn head_file_index(&self, file_hash: &[u8; 32]) -> Result<FileIndexHeadResult> {
        // Convert the raw 32-byte file hash to the MerkleHash the helper
        // expects. The codec on the metadb side matches this layout.
        let merkle = MerkleHash::from(*file_hash);
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if session.is_none() {
            *session = Some(self.handle.block_on(
                crab_metadata::file_index_lookup::FileIndexLookupSession::open(
                    Arc::clone(self.router.store().inner()),
                    self.router.repo_prefix(),
                ),
            )?);
        }
        let shard_hash = self.handle.block_on(
            session
                .as_ref()
                .ok_or_else(|| CrabError::Internal("file-index session missing".to_owned()))?
                .lookup(&merkle),
        )?;

        Ok(match shard_hash {
            Some(hash) => {
                let shard_path = self.router.shard_path(&hash);
                match self.handle.block_on(self.router.store().head(&shard_path)) {
                    Ok(_) => FileIndexHeadResult {
                        exists: true,
                        shard_hint: Some(hash.into()),
                    },
                    Err(CrabError::NotFound { .. }) => {
                        tracing::warn!(
                            file_hash = %merkle.hex(),
                            shard_hash = %hash.hex(),
                            "clean fast path: file-index hit points at missing shard"
                        );
                        FileIndexHeadResult {
                            exists: false,
                            shard_hint: None,
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            None => FileIndexHeadResult {
                exists: false,
                shard_hint: None,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Staging trait
// ---------------------------------------------------------------------------

/// Trait abstracting chunk staging for testability.
///
/// The real implementation writes chunks to the segment-based staging area
/// and records them in index.db within a transaction.
pub trait ChunkStager: Send {
    /// Stage a batch of chunks transactionally.
    ///
    /// On success, all chunks are recorded in index.db. Callers must
    /// flush the stager before emitting the corresponding pointer so
    /// recovery preserves the durable segment boundary. On failure,
    /// callers must not emit that pointer; a later retry will retire
    /// stale rows before re-staging.
    ///
    /// Chunks are passed as `(hash, data)` pairs where `data` is `Bytes`
    /// (zero-copy reference-counted) to avoid copying chunk data out of
    /// the CDC output.
    ///
    /// `chunk_index_offset` is the number of chunks of this file that
    /// have already been staged by previous calls. The implementation
    /// must assign `chunk_index = offset + i` to each chunk so
    /// consecutive batches produce non-overlapping
    /// `(file_hash, chunk_index)` keys. Pass `0` for the first batch.
    fn stage_chunks(
        &self,
        chunks: &[([u8; 32], Bytes)],
        file_hash: &[u8; 32],
        file_size: u64,
        chunk_index_offset: u64,
    ) -> Result<()>;

    /// Persist the canonical recipe/path lease before emitting its pointer.
    fn publish_recipe(&self, _path: &Path, _recipe: &FileRecipe) -> Result<()> {
        Ok(())
    }

    /// Return whether the exact recipe already has durable local payload rows.
    fn has_recipe(&self, _recipe: &FileRecipe) -> Result<bool> {
        Ok(false)
    }

    /// Adopt chunks staged under a provisional hash into the final file hash.
    ///
    /// Streaming clean callers use this after EOF, once Blake3 has
    /// finalized the real file hash. The default no-op keeps unit-test
    /// stagers lightweight; production stagers must make the rename
    /// durable before the pointer is emitted.
    fn adopt_staged_file(
        &self,
        _source_file_hash: &[u8; 32],
        _target_file_hash: &[u8; 32],
        _file_size: u64,
    ) -> Result<()> {
        Ok(())
    }

    /// Drop a provisional file and its staged rows when the clean fast path wins.
    fn discard_staged_file(&self, _file_hash: &[u8; 32]) -> Result<()> {
        Ok(())
    }

    /// Flush any staged bytes that must be durable before the pointer
    /// is emitted to Git.
    ///
    /// Implementations that only record in-memory/test state may leave
    /// this as a no-op.
    fn flush_pending(&self) -> Result<()> {
        Ok(())
    }
}

/// No-op stager that accepts all chunks for tests and dependency injection.
#[derive(Debug, Default)]
struct NoopChunkStager;

impl ChunkStager for NoopChunkStager {
    fn stage_chunks(
        &self,
        _chunks: &[([u8; 32], Bytes)],
        _file_hash: &[u8; 32],
        _file_size: u64,
        _chunk_index_offset: u64,
    ) -> Result<()> {
        Ok(())
    }
}

/// Real chunk stager backed by the segment-based [`StagingArea`].
///
/// Bridges the synchronous [`ChunkStager`] trait with the async
/// `StagingArea` API by using `Handle::block_on`. This is safe because
/// the filter-process loop runs inside `spawn_blocking`, so blocking on
/// async calls won't starve the tokio runtime.
pub struct StagingChunkStager {
    staging: Arc<StagingArea>,
    handle: tokio::runtime::Handle,
}

impl StagingChunkStager {
    /// Create a new stager wrapping the given staging area.
    ///
    /// `handle` must be a handle to the tokio runtime that owns the
    /// staging area's async locks.
    pub fn new(staging: Arc<StagingArea>, handle: tokio::runtime::Handle) -> Self {
        Self { staging, handle }
    }
}

// SAFETY: StagingArea is Send (Arc-wrapped internals), and Handle is Send+Sync.
unsafe impl Send for StagingChunkStager {}

impl ChunkStager for StagingChunkStager {
    fn stage_chunks(
        &self,
        chunks: &[([u8; 32], Bytes)],
        file_hash: &[u8; 32],
        file_size: u64,
        chunk_index_offset: u64,
    ) -> Result<()> {
        let file_merkle = MerkleHash::from(*file_hash);

        // Pre-insert the file row so the staging index can attach every
        // pending row to the final file hash before the pointer is emitted.
        self.staging.pre_register_file(&file_merkle, file_size)?;

        // Build the batch: convert hashes and borrow data slices.
        let batch: Vec<(MerkleHash, &[u8])> = chunks
            .iter()
            .map(|(hash, data)| (MerkleHash::from(*hash), data.as_ref()))
            .collect();

        let refs: Vec<(&MerkleHash, &[u8])> =
            batch.iter().map(|(hash, data)| (hash, *data)).collect();

        // Stage all chunks in a single batch — one writer lock, one
        // SQLite transaction, and the staging layer's own threshold
        // flush check. The clean checkpoint performs the final flush
        // before emitting the pointer, so mid-stream fsyncs are only a
        // crash-recovery boundary.
        self.handle.block_on(self.staging.stage_chunks_batch(
            &refs,
            &file_merkle,
            chunk_index_offset,
        ))?;

        tracing::debug!(
            file_hash = %file_merkle.hex(),
            chunks = chunks.len(),
            size = file_size,
            "staged chunks for file (batch)"
        );

        Ok(())
    }

    fn publish_recipe(&self, path: &Path, recipe: &FileRecipe) -> Result<()> {
        let batch_id = self.staging.create_batch()?;
        let file_hash = recipe.sequence().file_hash;
        let publish = (|| -> Result<()> {
            self.staging
                .pre_register_file(&file_hash, recipe.sequence().file_size)?;
            self.staging
                .record_verified_recipe_lease(&batch_id, path, recipe)?;
            self.staging
                .record_file_path(&file_hash, &path.to_string_lossy())?;
            self.staging.mark_batch_published(&batch_id)?;
            Ok(())
        })();
        if let Err(error) = publish {
            let _ = self.staging.rollback_batch(&batch_id);
            let _ = self.staging.retire_file_if_unleased(&file_hash);
            return Err(error);
        }
        Ok(())
    }

    fn has_recipe(&self, recipe: &FileRecipe) -> Result<bool> {
        Ok(self
            .staging
            .published_recipe_for_file(&recipe.sequence().file_hash)?
            .is_some_and(|published| published == *recipe))
    }

    fn adopt_staged_file(
        &self,
        source_file_hash: &[u8; 32],
        target_file_hash: &[u8; 32],
        file_size: u64,
    ) -> Result<()> {
        let source = MerkleHash::from(*source_file_hash);
        let target = MerkleHash::from(*target_file_hash);
        let adopted = self
            .staging
            .adopt_staged_file(&source, &target, file_size)?;

        tracing::debug!(
            source_file_hash = %source.hex(),
            target_file_hash = %target.hex(),
            rows = adopted,
            size = file_size,
            "clean filter: adopted provisional staged file"
        );

        Ok(())
    }

    fn discard_staged_file(&self, file_hash: &[u8; 32]) -> Result<()> {
        let file_merkle = MerkleHash::from(*file_hash);
        let removed = self.staging.unregister_file(&file_merkle)?;
        let abandoned = self.handle.block_on(self.staging.clean_abandoned(true));

        tracing::debug!(
            file_hash = %file_merkle.hex(),
            removed,
            "clean filter: discarded provisional staged file"
        );

        match abandoned {
            Ok((segments_removed, bytes_reclaimed, pending_removed)) => {
                if segments_removed > 0 || bytes_reclaimed > 0 || pending_removed > 0 {
                    tracing::debug!(
                        file_hash = %file_merkle.hex(),
                        segments_removed,
                        bytes_reclaimed,
                        pending_removed,
                        "clean filter: reclaimed discarded provisional staging"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    file_hash = %file_merkle.hex(),
                    error = %e,
                    "clean filter: discarded provisional rows but could not reclaim abandoned staging bytes"
                );
            }
        }

        Ok(())
    }

    fn flush_pending(&self) -> Result<()> {
        self.handle.block_on(self.staging.flush_pending())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BatchedChunkStager
// ---------------------------------------------------------------------------

/// Maximum number of accumulated chunks before sealing a staging batch.
/// Keeps each SQLite transaction modest without fsyncing per chunk.
const STAGER_BATCH_THRESHOLD: usize = 64;

/// Buffers CDC chunks until the file hash is known or the memory cap is reached.
///
/// Small files keep the old no-I/O fast path: staging can be skipped after
/// EOF when the file-index already knows the hash. Larger streams spill into
/// [`ProvisionalChunkStager`] so chunk payload memory stays bounded.
struct BatchedChunkStager {
    /// Accumulated chunk batches, each ready for a single `stage_chunks` call.
    batches: Vec<Vec<([u8; 32], Bytes)>>,
    /// Current batch being filled during the CDC loop.
    current_batch: Vec<([u8; 32], Bytes)>,
    /// Total chunks accumulated across all batches.
    total_chunks: usize,
    /// Total chunk payload bytes currently buffered in memory.
    total_bytes: usize,
}

impl BatchedChunkStager {
    /// Create a new stager with pre-allocated capacity.
    fn new(estimated_chunks: usize) -> Self {
        Self {
            batches: Vec::new(),
            current_batch: Vec::with_capacity(estimated_chunks.min(STAGER_BATCH_THRESHOLD)),
            total_chunks: 0,
            total_bytes: 0,
        }
    }

    /// Add CDC chunks to the current batch. When the batch reaches the
    /// threshold, it is sealed and a new batch starts.
    fn add_chunks(&mut self, chunks: Vec<crab_xet::chunker::Chunk>) {
        for c in chunks {
            let hash_bytes: [u8; 32] = c.hash.into();
            self.total_bytes = self.total_bytes.saturating_add(c.data.len());
            self.current_batch.push((hash_bytes, c.data));
            self.total_chunks += 1;

            if self.current_batch.len() >= STAGER_BATCH_THRESHOLD {
                let full_batch = std::mem::replace(
                    &mut self.current_batch,
                    Vec::with_capacity(STAGER_BATCH_THRESHOLD),
                );
                self.batches.push(full_batch);
            }
        }
    }

    /// Total number of chunks accumulated so far.
    fn chunk_count(&self) -> usize {
        self.total_chunks
    }

    /// Buffered payload bytes currently held in memory.
    fn buffered_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Move all buffered chunks into provisional staging.
    fn spill_into(
        &mut self,
        provisional: &mut ProvisionalChunkStager,
        stager: &mut Box<dyn ChunkStager>,
    ) -> Result<()> {
        if !self.current_batch.is_empty() {
            let full_batch = std::mem::replace(
                &mut self.current_batch,
                Vec::with_capacity(STAGER_BATCH_THRESHOLD),
            );
            self.batches.push(full_batch);
        }

        let batches = std::mem::take(&mut self.batches);
        for batch in batches {
            provisional.stage_batch(batch, stager)?;
        }

        self.total_chunks = 0;
        self.total_bytes = 0;
        Ok(())
    }

    #[cfg(test)]
    fn checkpoint(
        &mut self,
        stager: &mut Box<dyn ChunkStager>,
        file_hash: &[u8; 32],
        file_size: u64,
    ) -> Result<()> {
        if !self.current_batch.is_empty() {
            let full_batch = std::mem::replace(
                &mut self.current_batch,
                Vec::with_capacity(STAGER_BATCH_THRESHOLD),
            );
            self.batches.push(full_batch);
        }
        let batches = std::mem::take(&mut self.batches);
        if batches.is_empty() {
            self.total_bytes = 0;
            return Ok(());
        }
        Self::stage_batches(batches, stager, file_hash, file_size)?;
        self.total_bytes = 0;
        stager.flush_pending()
    }

    #[cfg(test)]
    fn stage_batches(
        batches: Vec<Vec<([u8; 32], Bytes)>>,
        stager: &mut Box<dyn ChunkStager>,
        file_hash: &[u8; 32],
        file_size: u64,
    ) -> Result<()> {
        let mut offset = 0u64;
        for batch in batches {
            let batch_len = u64::try_from(batch.len()).map_err(|_| {
                CrabError::StagingCorrupt("clean staging batch length overflow".to_owned())
            })?;
            stager.stage_chunks(&batch, file_hash, file_size, offset)?;
            offset = offset.checked_add(batch_len).ok_or_else(|| {
                CrabError::StagingCorrupt("clean staging chunk index overflow".to_owned())
            })?;
        }
        Ok(())
    }
}

/// Stages chunks under a provisional file hash until the final hash is known.
struct ProvisionalChunkStager {
    file_hash: [u8; 32],
    current_batch: Vec<([u8; 32], Bytes)>,
    staged_chunks: u64,
}

impl ProvisionalChunkStager {
    fn new(pathname: &str) -> Self {
        Self {
            file_hash: provisional_file_hash(pathname),
            current_batch: Vec::with_capacity(STAGER_BATCH_THRESHOLD),
            staged_chunks: 0,
        }
    }

    fn add_chunks(
        &mut self,
        chunks: Vec<crab_xet::chunker::Chunk>,
        stager: &mut Box<dyn ChunkStager>,
    ) -> Result<()> {
        for c in chunks {
            let hash_bytes: [u8; 32] = c.hash.into();
            self.current_batch.push((hash_bytes, c.data));

            if self.current_batch.len() >= STAGER_BATCH_THRESHOLD {
                self.flush_current_batch(stager)?;
            }
        }
        Ok(())
    }

    fn chunk_count(&self) -> Result<usize> {
        let current = u64::try_from(self.current_batch.len()).map_err(|_| {
            CrabError::StagingCorrupt("provisional clean batch length overflow".to_owned())
        })?;
        let total = self.staged_chunks.checked_add(current).ok_or_else(|| {
            CrabError::StagingCorrupt("provisional clean chunk count overflow".to_owned())
        })?;
        usize::try_from(total).map_err(|_| {
            CrabError::StagingCorrupt("provisional clean chunk count cannot be represented".into())
        })
    }

    fn stage_batch(
        &mut self,
        batch: Vec<([u8; 32], Bytes)>,
        stager: &mut Box<dyn ChunkStager>,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let batch_len = u64::try_from(batch.len()).map_err(|_| {
            CrabError::StagingCorrupt(format!(
                "provisional clean batch length {} cannot be represented",
                batch.len()
            ))
        })?;
        stager.stage_chunks(&batch, &self.file_hash, 0, self.staged_chunks)?;
        self.staged_chunks = self.staged_chunks.checked_add(batch_len).ok_or_else(|| {
            CrabError::StagingCorrupt(format!(
                "provisional clean chunk index overflow at offset {}",
                self.staged_chunks
            ))
        })?;
        Ok(())
    }

    fn flush_current_batch(&mut self, stager: &mut Box<dyn ChunkStager>) -> Result<()> {
        if self.current_batch.is_empty() {
            return Ok(());
        }
        let batch = std::mem::replace(
            &mut self.current_batch,
            Vec::with_capacity(STAGER_BATCH_THRESHOLD),
        );
        self.stage_batch(batch, stager)
    }

    fn checkpoint(
        &mut self,
        stager: &mut Box<dyn ChunkStager>,
        final_file_hash: &[u8; 32],
        file_size: u64,
    ) -> Result<()> {
        self.flush_current_batch(stager)?;
        stager.adopt_staged_file(&self.file_hash, final_file_hash, file_size)?;
        stager.flush_pending()
    }

    fn discard(&mut self, stager: &mut Box<dyn ChunkStager>) -> Result<()> {
        self.current_batch.clear();
        if let Err(e) = stager.discard_staged_file(&self.file_hash) {
            tracing::warn!(
                provisional_file_hash = %hex_encode(&self.file_hash),
                error = %e,
                "clean filter: failed to discard provisional staging after fast path"
            );
            return Err(e);
        }
        Ok(())
    }
}

fn discard_provisional_stager(
    provisional: &mut Option<ProvisionalChunkStager>,
    stager: &mut Box<dyn ChunkStager>,
) -> Result<()> {
    if let Some(mut provisional) = provisional.take() {
        provisional.discard(stager)?;
    }
    Ok(())
}

fn provisional_file_hash(pathname: &str) -> [u8; 32] {
    static NEXT_PROVISIONAL_FILE_ID: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(1);

    let id = NEXT_PROVISIONAL_FILE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab provisional clean staging v1\0");
    hasher.update(pathname.as_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&id.to_le_bytes());
    *hasher.finalize().as_bytes()
}

fn buffer_or_stage_clean_chunks(
    chunks: Vec<crab_xet::chunker::Chunk>,
    buffered: &mut BatchedChunkStager,
    provisional: &mut Option<ProvisionalChunkStager>,
    stager: &mut Box<dyn ChunkStager>,
    recipe_recorder: &mut RecipeRecorder,
    pathname: &str,
    buffer_cap: usize,
) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }

    record_clean_chunks(recipe_recorder, &chunks)?;

    if let Some(provisional) = provisional.as_mut() {
        return provisional.add_chunks(chunks, stager);
    }

    buffered.add_chunks(chunks);
    if buffered.buffered_bytes() <= buffer_cap {
        return Ok(());
    }

    let mut next = ProvisionalChunkStager::new(pathname);
    buffered.spill_into(&mut next, stager)?;
    *provisional = Some(next);
    Ok(())
}

fn record_clean_chunks(
    recorder: &mut RecipeRecorder,
    chunks: &[crab_xet::chunker::Chunk],
) -> Result<()> {
    for chunk in chunks {
        recorder.record(
            MerkleHash::from(<[u8; 32]>::from(chunk.hash)),
            u64::try_from(chunk.data.len()).map_err(|_| {
                CrabError::StagingCorrupt("clean chunk length cannot be represented".to_owned())
            })?,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CleanSession
// ---------------------------------------------------------------------------

/// Per-session state for the clean pipeline.
///
/// Maintains the bloom filter, chunk buffer, and metrics across multiple
/// clean operations within a single filter-process invocation. Supports
/// both crab-native (Blake3 + CDC) and LFS (SHA-256) clean paths,
/// selected per-file based on `.gitattributes` `filter=` settings.
pub struct CleanSession {
    ctx: AppContext,
    bloom: FileHashBloom,
    /// File hashes confirmed via HEAD, with their shard hint if the file-index has one.
    confirmed_hashes: HashMap<[u8; 32], Option<[u8; 32]>>,
    file_index_checker: Box<dyn FileIndexChecker>,
    chunk_stager: Box<dyn ChunkStager>,
    chunk_buffer_cap: usize,
    fastpath_min_size: u64,
    /// Compiled include/exclude filter for auto-hydrate. `Some` when
    /// `hydrate.auto = true` and patterns compiled successfully.
    hydrate_filter: Option<PatternFilter>,
    /// LFS object store for staging content when cleaning LFS-tracked files.
    /// `None` when LFS support is not configured for this session.
    lfs_store: Option<Arc<LfsObjectStore>>,
    /// Git LFS fetch include/exclude filters used by smudge paths.
    lfs_fetch_filter: Option<crate::lfs::fetch_filter::FetchPathFilter>,
    /// Repository root directory, used to locate `.gitattributes`.
    /// `None` when the repo root could not be determined.
    repo_root: Option<PathBuf>,
    /// Cached LFS-tracked patterns from `.gitattributes`, parsed once at
    /// session start. Used by the legacy classifier and kept in both
    /// modes so the diagnostics in `set_repo_root` can report a count.
    lfs_patterns: Vec<String>,
    /// Consolidated attributes reader built by
    /// [`crate::core::attrs::AttrsReader`]. When `Some`, drives
    /// `is_lfs_tracked` in place of walking `lfs_patterns` with the
    /// hand-rolled glob matcher. Gated behind `gix-pathmatch` so the
    /// legacy classifier stays reachable until the flag flips
    /// default-on.
    ///
    /// Lazily initialized via `OnceLock`: the underlying tree walk +
    /// `gix_attributes` parse is only paid when `is_lfs_tracked` is
    /// actually called, which in current code paths is never (the
    /// filter-process dispatch uses `resolve_filter_for` →
    /// `FilterAttrCache` instead). Without this deferral the
    /// filter-process startup in `crab init` walks the entire working
    /// tree a third time for no benefit.
    #[cfg(feature = "gix-pathmatch")]
    lfs_attrs: std::sync::OnceLock<Option<Arc<crate::core::attrs::AttrsReader>>>,
    /// Persistent `file_hash → shard_hash` mapping used to populate the
    /// `shard-hint` field in emitted pointers. Loaded lazily via
    /// [`load_shard_hints_from_cache`](Self::load_shard_hints_from_cache);
    /// an empty cache (default) simply emits pointers without hints.
    shard_hints: ShardHintCache,
    /// Cached `.gitattributes` filter rules for LFS/XET dispatch.
    /// Built once at `set_repo_root` time and shared across all files
    /// in the session. Resolves `filter=lfs` vs `filter=crab` per file
    /// path with git's "last match wins" semantics.
    filter_attr_cache: Option<crate::git::filter_attr_cache::FilterAttrCache>,
    /// Per-worktree cache of `path → (mtime, size, pointer_bytes)` populated
    /// by `crab hydrate`. A cache hit lets the clean filter short-circuit
    /// CDC + staging for hydrated files — critical for `git status` /
    /// `git diff` / `git pull` not to either grind through multi-GiB
    /// content on every invocation or fail with `CRAB-E0081` when a
    /// concurrent crab process holds `.crab/staging`.
    hydrated_cache: Option<crate::cache::HydratedPointerCache>,
    /// Paths observed to have stale hydrated-cache entries during this
    /// session (stat mismatch). Flushed to disk at session teardown via
    /// [`persist_hydrated_cache_invalidations`](Self::persist_hydrated_cache_invalidations)
    /// so the next session doesn't re-do the lookup only to fall through.
    hydrated_cache_invalidations: Vec<String>,
    /// When `Some`, the backing staging area is unavailable for writes
    /// and the crab-native clean path must fail instead of producing
    /// a pointer that points at chunks we never staged. `None` means
    /// the stager is writable (or this is a unit-test session where
    /// staging simply isn't used — tests opt into that by leaving the
    /// flag unset).
    ///
    /// The inner `Option<u32>` carries the holder PID when known, so
    /// the surfaced `CRAB-E0081` error tells the user which process
    /// to resolve.
    #[expect(
        clippy::option_option,
        reason = "outer None means writable staging; inner None means blocked by an unknown holder"
    )]
    staging_unavailable: Option<Option<u32>>,
}

impl CleanSession {
    /// Create a new session with default configuration.
    pub fn new(ctx: AppContext) -> Self {
        let fastpath_min_size = ctx.perf().effective_fastpath_min_size();

        let hydrate_filter = Self::compile_hydrate_filter(&ctx);

        Self {
            ctx,
            bloom: FileHashBloom::new(1024),
            confirmed_hashes: HashMap::new(),
            file_index_checker: Box::new(NoopFileIndexChecker),
            chunk_stager: Box::new(NoopChunkStager),
            chunk_buffer_cap: DEFAULT_CHUNK_BUFFER_CAP,
            fastpath_min_size,
            hydrate_filter,
            lfs_store: None,
            lfs_fetch_filter: None,
            repo_root: None,
            lfs_patterns: Vec::new(),
            #[cfg(feature = "gix-pathmatch")]
            lfs_attrs: std::sync::OnceLock::new(),
            filter_attr_cache: None,
            shard_hints: ShardHintCache::new(),
            hydrated_cache: None,
            hydrated_cache_invalidations: Vec::new(),
            staging_unavailable: None,
        }
    }

    /// The application context for this session.
    #[must_use]
    pub fn ctx(&self) -> &AppContext {
        &self.ctx
    }

    /// Create a session with custom dependencies.
    pub fn with_deps(
        ctx: AppContext,
        checker: Box<dyn FileIndexChecker>,
        stager: Box<dyn ChunkStager>,
        bloom_items: &[[u8; 32]],
        fastpath_min_size: u64,
        chunk_buffer_cap: usize,
    ) -> Self {
        let mut bloom = FileHashBloom::new(bloom_items.len().max(64));
        for hash in bloom_items {
            bloom.insert(hash);
        }
        let hydrate_filter = Self::compile_hydrate_filter(&ctx);
        Self {
            ctx,
            bloom,
            confirmed_hashes: HashMap::new(),
            file_index_checker: checker,
            chunk_stager: stager,
            chunk_buffer_cap,
            fastpath_min_size,
            hydrate_filter,
            lfs_store: None,
            lfs_fetch_filter: None,
            repo_root: None,
            lfs_patterns: Vec::new(),
            #[cfg(feature = "gix-pathmatch")]
            lfs_attrs: std::sync::OnceLock::new(),
            filter_attr_cache: None,
            shard_hints: ShardHintCache::new(),
            hydrated_cache: None,
            hydrated_cache_invalidations: Vec::new(),
            staging_unavailable: None,
        }
    }

    /// Mark the session as having no writable staging area because
    /// another process holds the exclusive lock. The crab-native
    /// clean path will refuse with
    /// [`CrabError::StagingLocked`] instead of emitting a pointer
    /// against `NoopChunkStager`, which would produce a valid-looking
    /// pointer backed by no chunks — irrecoverably broken on push.
    pub fn set_staging_locked(&mut self, holder_pid: Option<u32>) {
        self.staging_unavailable = Some(holder_pid);
    }

    /// Mark the session as having no staging area at all (directory
    /// missing). Same effect on the clean path as
    /// [`set_staging_locked`](Self::set_staging_locked): refuse to
    /// emit crab-native pointers. Unit tests that exercise clean
    /// without a real staging area should not call this.
    pub fn set_staging_unavailable(&mut self) {
        self.staging_unavailable = Some(None);
    }

    /// Swap in a writable chunk stager after the session was created
    /// without one. The filter session defers staging-area open until
    /// a `clean` command actually arrives, so `git status` — which
    /// only issues `smudge` commands through the filter — never
    /// acquires `LOCK_EX` on the staging root and doesn't contend
    /// with a concurrent `crab add`.
    ///
    /// Clears any prior "staging unavailable" flag set at session
    /// construction: successful lazy-open means the crab-native
    /// clean path can now emit backed pointers. Callers that still
    /// want the session to refuse writes must re-assert via
    /// [`set_staging_locked`](Self::set_staging_locked) or
    /// [`set_staging_unavailable`](Self::set_staging_unavailable)
    /// after this call.
    pub fn set_chunk_stager(&mut self, stager: Box<dyn ChunkStager>) {
        self.chunk_stager = stager;
        self.staging_unavailable = None;
    }

    /// Install the remote file-index checker used by the clean fast path.
    pub fn set_file_index_checker(&mut self, checker: Box<dyn FileIndexChecker>) {
        self.file_index_checker = checker;
    }

    /// Peek the hydrated-pointer cache for `pathname` without touching
    /// staging. Returns `true` when the pathname has a live (stat
    /// matches on-disk file) cache entry with decodable pointer bytes,
    /// meaning the upcoming clean will be served from cache — so callers
    /// can skip acquiring the staging flock entirely.
    ///
    /// Non-mutating: if the entry turns out to be stale during the
    /// actual clean call, the normal invalidation path handles it.
    #[must_use]
    pub fn has_live_hydrated_entry(&self, pathname: &str) -> bool {
        let Some(cache) = self.hydrated_cache.as_ref() else {
            return false;
        };
        let Some(entry) = cache.get(pathname) else {
            return false;
        };
        let Some(root) = self.repo_root.as_ref() else {
            return false;
        };
        crate::cache::hydrated_pointer::matches_stat(&root.join(pathname), entry)
            && crate::cache::hydrated_pointer::decode_pointer(entry).is_some()
    }

    /// If the session knows staging isn't backing writes, return the
    /// appropriate `StagingLocked` error so callers can bail before
    /// running CDC. Returns `Ok(())` otherwise.
    fn check_staging_available(&self) -> Result<()> {
        match self.staging_unavailable {
            None => Ok(()),
            Some(holder_pid) => Err(CrabError::StagingLocked { holder_pid }),
        }
    }

    /// Seed the bloom filter with known file hashes from the local file-index.
    pub fn seed_bloom(&mut self, file_hashes: &[[u8; 32]]) {
        for hash in file_hashes {
            self.bloom.insert(hash);
        }
    }

    /// Load a previously persisted bloom filter from the cache directory.
    ///
    /// Falls back to an empty bloom if the file doesn't exist or is corrupt.
    pub fn load_bloom_from_cache(&mut self) {
        let path = crate::cache::default_cache_root().join("bloom.bin");
        if let Some(bloom) = FileHashBloom::load(&path) {
            tracing::debug!(
                num_bits = bloom.num_bits,
                "loaded persisted bloom filter from cache"
            );
            self.bloom = bloom;
        }
    }

    /// Persist the current bloom filter to the cache directory.
    pub fn save_bloom_to_cache(&self) {
        let path = crate::cache::default_cache_root().join("bloom.bin");
        if let Err(e) = self.bloom.save(&path) {
            tracing::debug!(error = %e, "failed to persist bloom filter (non-fatal)");
        }
    }

    /// Load the persisted `file_hash → shard_hash` mapping from the
    /// default cache location. Any error (missing, unreadable, or corrupt
    /// JSON) is logged at `warn!` and degrades to an empty cache —
    /// emitted pointers simply won't carry `shard-hint`, which falls
    /// back to the file-index path on hydrate.
    pub fn load_shard_hints_from_cache(&mut self) {
        let path = crate::cache::shard_hints::default_path();
        match ShardHintCache::load_sync(&path) {
            Ok(cache) => {
                tracing::debug!(
                    path = %path.display(),
                    entries = cache.len(),
                    "loaded shard-hints cache for clean session"
                );
                self.shard_hints = cache;
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to load shard-hints cache, proceeding without hints"
                );
            }
        }
    }

    /// Compile the persistent hydrate include/exclude patterns into a
    /// [`PatternFilter`] when `hydrate.auto` is enabled. Returns `None`
    /// when auto-hydrate is off or when pattern compilation fails (with a
    /// warning logged so the session can fall back gracefully).
    fn compile_hydrate_filter(ctx: &AppContext) -> Option<PatternFilter> {
        let hydrate = &ctx.config().hydrate;
        if !hydrate.auto {
            return None;
        }
        if hydrate.include.is_empty() {
            tracing::debug!("hydrate.auto enabled but no include patterns configured");
            return None;
        }
        match crate::core::pattern::build_filter(&hydrate.include, &hydrate.exclude) {
            Ok(filter) => {
                tracing::debug!(
                    include_count = hydrate.include.len(),
                    exclude_count = hydrate.exclude.len(),
                    "compiled auto-hydrate pattern filter"
                );
                Some(filter)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to compile auto-hydrate patterns, falling back to lazy for all files"
                );
                None
            }
        }
    }

    /// Check whether a file should be eagerly smudged despite lazy mode.
    ///
    /// Returns `true` when `hydrate.auto` is enabled and the file path
    /// matches the persistent include/exclude patterns.
    pub fn should_auto_hydrate(&self, pathname: &str) -> bool {
        match self.hydrate_filter {
            Some(ref filter) => filter.matches(pathname),
            None => false,
        }
    }

    /// Set the LFS object store for this session.
    ///
    /// When set, files with `filter=lfs` in `.gitattributes` will be
    /// cleaned via the LFS path (SHA-256 + LFS pointer) instead of the
    /// default crab path (Blake3 + CDC).
    pub fn set_lfs_store(&mut self, store: Arc<LfsObjectStore>) {
        self.lfs_store = Some(store);
    }

    /// Set the repository root directory for `.gitattributes` lookup.
    ///
    /// Walks the working tree **once** to collect both LFS patterns
    /// (legacy `is_lfs_tracked` fallback) and the
    /// [`FilterAttrCache`](crate::git::filter_attr_cache::FilterAttrCache)
    /// entries, avoiding the redundant second tree walk that the
    /// previous two-call approach incurred.
    pub fn set_repo_root(&mut self, root: PathBuf) {
        let (entries, root_mtime) = crate::git::filter_attr_cache::collect_all_entries(&root);
        self.lfs_fetch_filter = match crate::lfs::config::LfsConfig::resolve(&root)
            .and_then(|config| crate::lfs::fetch_filter::FetchPathFilter::from_config(&config))
        {
            Ok(filter) => filter,
            Err(e) => {
                tracing::warn!(
                    root = %root.display(),
                    error = %e,
                    "failed to compile LFS fetch filters; process smudge will not apply path filters"
                );
                None
            }
        };

        // Extract LFS-tracked patterns for the legacy `is_lfs_tracked`
        // fallback (used when `gix-pathmatch` is disabled or the
        // AttrsReader fails to open). Produces the same `Vec<String>`
        // that `parse_lfs_patterns` would have returned.
        self.lfs_patterns = entries
            .iter()
            .filter(|e| e.filter == Some(crate::git::filter_attr_cache::FilterKind::Lfs))
            .map(|e| e.pattern.clone())
            .collect();

        self.filter_attr_cache =
            Some(crate::git::filter_attr_cache::FilterAttrCache::from_entries(entries, root_mtime));

        // AttrsReader is lazily initialized by `is_lfs_tracked` on first
        // call — we don't pay the tree-walk + gix_attributes parse cost
        // unless the legacy LFS classifier path is actually exercised.
        // The filter-process dispatch uses `resolve_filter_for` →
        // `FilterAttrCache` instead, so the lazy init keeps `crab init`
        // fast on large working trees.

        // Load the hydrated-pointer cache so we can short-circuit
        // clean on already-hydrated files. Missing / corrupt caches
        // degrade to an empty map (no short-circuit) rather than
        // failing the session.
        match crate::cache::hydrated_pointer::cache_path_for_worktree_root(&root) {
            Ok(cache_path) => {
                let hydrated = crate::cache::HydratedPointerCache::load_sync(&cache_path);
                tracing::debug!(
                    path = %cache_path.display(),
                    entries = hydrated.len(),
                    "loaded hydrated-pointer cache for clean session"
                );
                self.hydrated_cache = Some(hydrated);
            }
            Err(e) => {
                tracing::debug!(
                    root = %root.display(),
                    error = %e,
                    "hydrated-pointer cache unavailable for clean session"
                );
                self.hydrated_cache = Some(crate::cache::HydratedPointerCache::new());
            }
        }

        self.repo_root = Some(root);
    }

    /// Return the repository root bound to this filter session, when known.
    #[must_use]
    pub fn repo_root(&self) -> Option<&Path> {
        self.repo_root.as_deref()
    }

    /// Check whether an LFS-tracked path should be smudged under LFS fetch filters.
    #[must_use]
    pub fn should_lfs_smudge(&self, pathname: &str) -> bool {
        self.lfs_fetch_filter
            .as_ref()
            .is_none_or(|filter| filter.allows(pathname))
    }

    /// Flush pending hydrated-cache invalidations to disk. Called at
    /// session teardown by the filter-process loop so stale entries
    /// don't linger across invocations. Non-fatal on failure — the
    /// cache is purely advisory.
    pub fn persist_hydrated_cache_invalidations(&mut self) {
        let paths = std::mem::take(&mut self.hydrated_cache_invalidations);
        if paths.is_empty() {
            return;
        }
        let Some(root) = self.repo_root.as_ref() else {
            return;
        };
        match crate::cache::hydrated_pointer::cache_path_for_worktree_root(root) {
            Ok(cache_path) => {
                if let Err(e) =
                    crate::cache::HydratedPointerCache::invalidate_on_disk(&cache_path, paths)
                {
                    tracing::debug!(
                        path = %cache_path.display(),
                        error = %e,
                        "failed to flush hydrated-pointer cache invalidations"
                    );
                }
            }
            Err(e) => {
                tracing::debug!(
                    root = %root.display(),
                    error = %e,
                    "hydrated-pointer cache unavailable for invalidation flush"
                );
            }
        }
    }

    /// Consult the hydrated-pointer cache for `pathname`. When the
    /// stat fingerprint still matches, returns the cached pointer
    /// bytes verbatim so the filter can skip CDC, hashing, and
    /// staging entirely. A stale entry is queued for invalidation
    /// and the caller falls back to the normal pipeline.
    ///
    /// Returns `None` when the cache is missing, the entry is absent,
    /// the fingerprint no longer matches, or the pointer bytes are
    /// corrupt — any of which mean the normal clean path must run.
    fn try_hydrated_cache_hit(&mut self, pathname: &str) -> Option<Vec<u8>> {
        let cache = self.hydrated_cache.as_ref()?;
        let entry = cache.get(pathname)?;
        let root = self.repo_root.as_ref()?;
        let full_path = root.join(pathname);

        if !crate::cache::hydrated_pointer::matches_stat(&full_path, entry) {
            // Fingerprint no longer matches — the user (or a tool)
            // touched the file after hydrate. Drop the entry so we
            // don't re-check on every clean in this session, and fall
            // through to the real pipeline.
            tracing::debug!(
                path = %pathname,
                "hydrated-pointer cache: stat mismatch, invalidating entry"
            );
            self.hydrated_cache_invalidations.push(pathname.to_owned());
            if let Some(cache) = self.hydrated_cache.as_mut() {
                cache.remove(pathname);
            }
            return None;
        }

        let Some(bytes) = crate::cache::hydrated_pointer::decode_pointer(entry) else {
            tracing::debug!(
                path = %pathname,
                "hydrated-pointer cache: corrupt pointer bytes, invalidating entry"
            );
            self.hydrated_cache_invalidations.push(pathname.to_owned());
            if let Some(cache) = self.hydrated_cache.as_mut() {
                cache.remove(pathname);
            }
            return None;
        };
        tracing::debug!(
            path = %pathname,
            size = entry.size,
            "hydrated-pointer cache hit: returning cached pointer without CDC"
        );
        Some(bytes)
    }

    /// Refresh the hydrated-pointer cache entry for `pathname` with
    /// the current file stat and `pointer_bytes`. Non-fatal on error —
    /// the cache is advisory and degrades to the slow path when
    /// absent. No-op when we don't know the repo root or the file is
    /// missing on disk.
    ///
    /// Called after a successful clean so subsequent invocations
    /// (e.g. the next `git status` in a shell prompt) short-circuit
    /// without re-running CDC over the same bytes. Also populated by
    /// `crab hydrate`, but clean-side updates cover the
    /// clone-then-hydrate-elsewhere path and keep the cache self-healing.
    fn remember_hydrated_pointer(&mut self, pathname: &str, pointer_bytes: &[u8]) {
        let Some(root) = self.repo_root.as_ref() else {
            return;
        };
        let full_path = root.join(pathname);
        let entry = match crate::cache::hydrated_pointer::entry_for_path(&full_path, pointer_bytes)
        {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(
                    path = %pathname,
                    error = %e,
                    "hydrated-pointer cache: stat failed, not recording entry"
                );
                return;
            }
        };

        match crate::cache::hydrated_pointer::cache_path_for_worktree_root(root) {
            Ok(cache_path) => {
                if let Err(e) = crate::cache::HydratedPointerCache::update_on_disk(
                    &cache_path,
                    [(pathname.to_owned(), entry.clone())],
                ) {
                    tracing::debug!(
                        path = %cache_path.display(),
                        error = %e,
                        "hydrated-pointer cache: failed to persist entry"
                    );
                    return;
                }
            }
            Err(e) => {
                tracing::debug!(
                    root = %root.display(),
                    error = %e,
                    "hydrated-pointer cache: unavailable for persist"
                );
                return;
            }
        }

        if let Some(cache) = self.hydrated_cache.as_mut() {
            cache.insert(pathname.to_owned(), entry);
        }
    }

    /// Check whether a file is LFS-tracked via `filter=lfs` in `.gitattributes`.
    ///
    /// Resolution order (first match wins):
    /// 1. [`crate::core::attrs::AttrsReader`] (when `gix-pathmatch` is enabled)
    /// 2. [`FilterAttrCache`](crate::git::filter_attr_cache::FilterAttrCache)
    ///    (same entries, O(n) scan with "last match wins" semantics)
    /// 3. Legacy `lfs_patterns` slice (hand-rolled glob match)
    ///
    /// Returns `false` when the repo root is unknown or `.gitattributes` is
    /// missing/unreadable.
    #[allow(dead_code)]
    fn is_lfs_tracked(&self, pathname: &str) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            // Lazily build the AttrsReader on first call. The underlying
            // tree walk + gix_attributes parse is expensive and never
            // needed by the filter-process clean dispatch (which uses
            // FilterAttrCache instead). Deferring keeps `crab init` fast
            // on large working trees.
            let reader_opt = self.lfs_attrs.get_or_init(|| {
                let root = self.repo_root.as_ref()?;
                match crate::core::attrs::AttrsReader::open(root) {
                    Ok(r) => {
                        tracing::debug!(
                            root = %root.display(),
                            "lazily initialized AttrsReader for is_lfs_tracked"
                        );
                        Some(Arc::new(r))
                    }
                    Err(err) => {
                        tracing::warn!(
                            root = %root.display(),
                            error = %err,
                            "failed to lazily open consolidated attributes reader; using legacy LFS patterns"
                        );
                        None
                    }
                }
            });
            if let Some(reader) = reader_opt.as_ref() {
                return reader.has_filter(pathname, "lfs");
            }
        }
        // Prefer the FilterAttrCache — it uses the same entries as
        // resolve_filter_for and has proper "last match wins" semantics.
        if let Some(ref cache) = self.filter_attr_cache {
            return cache.resolve_filter(pathname)
                == Some(crate::git::filter_attr_cache::FilterKind::Lfs);
        }
        self.lfs_patterns
            .iter()
            .any(|pattern| glob_matches(pattern, pathname))
    }

    /// Resolve the filter handler for a file path using the
    /// [`FilterAttrCache`](crate::git::filter_attr_cache::FilterAttrCache).
    ///
    /// Returns the filter kind from the last matching `.gitattributes` line.
    /// Returns `None` when no filter attribute matches or the cache hasn't
    /// been initialized (missing repo root).
    pub fn resolve_filter_for(
        &self,
        pathname: &str,
    ) -> Option<crate::git::filter_attr_cache::FilterKind> {
        let cache = self.filter_attr_cache.as_ref()?;
        let resolved = cache.resolve_filter(pathname);
        match resolved {
            Some(filter) => {
                tracing::debug!(
                    path = %pathname,
                    filter = ?filter,
                    "resolved filter for path"
                );
            }
            None => {
                tracing::trace!(path = %pathname, "no filter attribute matches, falling back to blob classification");
            }
        }
        resolved
    }

    /// Reset transient state after a failed operation.
    ///
    /// Called by the session-isolation wrapper to ensure a
    /// panic or error in one clean doesn't leave stale buffered data that
    /// could affect the next operation.
    pub fn reset_transient_state(&mut self) {
        // The chunk buffer is per-operation and created fresh each time,
        // so there's nothing to reset there. The bloom filter and
        // confirmed_hashes are session-scoped and remain valid.
        // This method exists as the hook point for any future transient
        // state that needs clearing.
        tracing::debug!("transient session state reset after operation failure");
    }

    /// Clean a file: hash, chunk, stage, and return the pointer bytes.
    ///
    /// Dispatches to the LFS path (SHA-256 + LFS pointer) when the file
    /// has `filter=lfs` in `.gitattributes` and an LFS object store is
    /// configured. Otherwise uses the crab path (Blake3 + CDC).
    ///
    /// Cancel-safe: if this function is interrupted (e.g. the filter process
    /// is killed), no partial index.db rows are left behind because staging
    /// uses a transaction that only commits on success.
    ///
    /// This is a thin wrapper that re-frames `content` as pkt-line data
    /// packets and delegates to [`clean_stream`](Self::clean_stream) so
    /// the buffered and streaming entry points share a single pipeline
    /// implementation. Output pointer bytes are byte-identical to the
    /// pre-wrapper behavior because CDC boundaries and blake3 hashing
    /// are both independent of input-feed granularity.
    pub fn clean_file(&mut self, pathname: &str, content: Vec<u8>) -> Result<Vec<u8>> {
        let framed = frame_as_pktlines(&content);
        let mut reader = crate::git::filter_process::PktLineReader::from_slice(&framed);
        self.clean_stream(pathname, &mut reader)
    }

    /// LFS clean path for callers that already materialized the input.
    fn lfs_clean_path(&self, pathname: &str, content: Vec<u8>) -> Result<Vec<u8>> {
        let lfs_dir = self.lfs_dir()?;
        let mut writer = crate::lfs::cache::ObjectWriter::new(&lfs_dir)?;
        std::io::Write::write_all(&mut writer, &content).map_err(CrabError::Io)?;
        let extensions = crate::lfs::extension::configured_extensions_sorted()?;
        let cleaned = crate::lfs::extension::clean_staged_with_extensions(
            writer.finish()?,
            &lfs_dir,
            pathname,
            &extensions,
        )?;
        self.finish_lfs_clean(pathname, cleaned.staged, cleaned.pointer_extensions)
    }

    fn finish_lfs_clean(
        &self,
        pathname: &str,
        staged: crate::lfs::cache::StagedObject,
        extensions: Vec<crab_git::lfs_pointer::LfsExtension>,
    ) -> Result<Vec<u8>> {
        let oid = *staged.oid();
        let size = staged.size();
        let lfs_dir = self.lfs_dir()?;
        let local_path = staged.install(&lfs_dir)?;
        let oid_hex = crab_git::lfs_pointer::hex_encode(&oid);
        tracing::debug!(
            oid = %oid_hex,
            path = %pathname,
            size,
            "lfs clean: object verified and installed locally"
        );

        if let Some(store) = self.lfs_store.as_ref() {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(store.put_stream(&oid, &local_path))?;
        }

        Ok(LfsPointer {
            oid,
            size,
            extensions,
        }
        .serialize())
    }

    fn lfs_dir(&self) -> Result<PathBuf> {
        let root = self
            .repo_root
            .as_ref()
            .ok_or_else(|| CrabError::Configuration {
                key: "lfs cache".to_owned(),
                origin: "repository root is unavailable".to_owned(),
            })?;
        let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(root)?;
        Ok(ctx.common_git_dir.join("lfs"))
    }

    /// Crab clean path: Blake3 hash + CDC chunk in a single pass, stage, return pointer.
    ///
    /// Uses [`BatchedChunkStager`] to preserve chunk order while keeping each
    /// staging transaction bounded.
    ///
    /// The file hash isn't known until all data is consumed (single-pass
    /// blake3+CDC), so chunks are buffered during the loop and staged
    /// in batches once `checkpoint()` is called with the hash.
    ///
    /// The staging-availability check runs *after* the fast-path lookup
    /// so a file already known to the file-index doesn't need a
    /// writable staging area. That lets `git status` / `git diff` on
    /// hydrated files succeed even while another crab process holds
    /// `.crab/staging`.
    fn crab_clean_path(&mut self, pathname: &str, content: Vec<u8>) -> Result<Vec<u8>> {
        use crab_xet::chunker::GearChunker;

        let file_size = content.len() as u64;

        // Single-pass: compute blake3 file hash and CDC chunk boundaries
        // simultaneously. This eliminates a separate full-file hash pass
        // (~300ms saved per GB on modern hardware).
        let mut file_hasher = blake3::Hasher::new();
        let mut chunker = GearChunker::new();
        let estimated_chunks = (file_size / 65536).max(1) as usize;

        let mut batch_stager = BatchedChunkStager::new(estimated_chunks);
        let mut recipe_recorder = RecipeRecorder::new(ChunkingPolicyId::XetGearV1_64KiB);

        // Hash in one adaptive pass (serial loop or rayon-parallel based
        // on size), then feed the CDC chunker block-by-block. The CDC
        // chunker is stateful and still needs incremental feeding; the
        // hasher doesn't.
        let use_rayon = file_size >= crate::engine::hashing::BLAKE3_RAYON_THRESHOLD;
        tracing::debug!(
            hash_rayon = use_rayon,
            file_size,
            reason = if use_rayon {
                "above_threshold"
            } else {
                "below_threshold"
            },
            "clean: blake3 hash path selected",
        );
        crate::engine::hashing::update_blake3_adaptive(&mut file_hasher, &content);

        for block in content.chunks(128 * 1024) {
            let new_chunks = chunker.feed(block);
            if !new_chunks.is_empty() {
                record_clean_chunks(&mut recipe_recorder, &new_chunks)?;
                batch_stager.add_chunks(new_chunks);
            }
        }
        if let Some(last) = chunker.finalize() {
            record_clean_chunks(&mut recipe_recorder, std::slice::from_ref(&last))?;
            batch_stager.add_chunks(vec![last]);
        }

        let file_hash: [u8; 32] = *file_hasher.finalize().as_bytes();
        let total_chunks = batch_stager.chunk_count();

        tracing::debug!(
            path = %pathname,
            size = file_size,
            file_hash = %hex_encode(&file_hash),
            chunks = total_chunks,
            "clean: hash + CDC complete (single pass)"
        );

        // Before the bloom/file-index fast path, consult the git
        // index directly. If the working-tree content hashes to the
        // same file_hash as a pointer blob already committed for this
        // path, the file is a hydrated copy — emit the committed
        // pointer verbatim, no staging needed. Works with zero
        // network traffic and regardless of bloom state.
        if let Some(pointer_bytes) = self.try_index_pointer_match(pathname, &file_hash) {
            self.remember_hydrated_pointer(pathname, &pointer_bytes);
            return Ok(pointer_bytes);
        }

        // Attempt fast path with the computed hash. A hit emits a
        // pointer whose chunks already exist remotely — no staging
        // needed, so this works even when `.crab/staging` is locked.
        if let Some(pointer_bytes) = self.try_fast_path(file_size, &file_hash)? {
            self.remember_hydrated_pointer(pathname, &pointer_bytes);
            return Ok(pointer_bytes);
        }

        // Slow path: we're about to stage chunks. Refuse now if the
        // stager is a no-op — otherwise we'd emit a correct-looking
        // pointer into git's index and lose every chunk byte, which
        // surfaces as "shard not found" the first time any other
        // clone tries to hydrate.
        self.check_staging_available()?;

        let recipe = recipe_recorder.seal(MerkleHash::from(file_hash), file_size)?;
        if self.chunk_stager.has_recipe(&recipe)? {
            self.chunk_stager
                .publish_recipe(Path::new(pathname), &recipe)?;
            let pointer = self.build_pointer(file_hash, file_size, None);
            let pointer_bytes = pointer.serialize();
            self.remember_hydrated_pointer(pathname, &pointer_bytes);
            return Ok(pointer_bytes);
        }
        let mut provisional = ProvisionalChunkStager::new(pathname);
        batch_stager.spill_into(&mut provisional, &mut self.chunk_stager)?;
        provisional.checkpoint(&mut self.chunk_stager, &file_hash, file_size)?;
        self.chunk_stager
            .publish_recipe(Path::new(pathname), &recipe)?;

        let pointer = self.build_pointer(file_hash, file_size, None);
        let pointer_bytes = pointer.serialize();
        self.remember_hydrated_pointer(pathname, &pointer_bytes);
        Ok(pointer_bytes)
    }

    /// Streaming variant of [`clean_file`] that feeds a pkt-line reader
    /// directly into the hasher and CDC chunker.
    ///
    /// On the crab path, peak chunk-payload memory is bounded by the
    /// session's `chunk_buffer_cap`: small files keep staging deferred
    /// until EOF, while larger files spill into provisional staging and
    /// later adopt those rows under the finalized Blake3 hash. For any
    /// given input, the output pointer bytes are identical to
    /// [`clean_file`] on the same content.
    ///
    /// Pointer passthrough is decided from the complete input only while it
    /// remains below Git LFS's pointer-size cutoff. Unextended LFS content
    /// streams to a temporary cache file while hashing, so memory use is
    /// independent of object size.
    pub fn clean_stream<R: std::io::Read>(
        &mut self,
        pathname: &str,
        reader: &mut crate::git::filter_process::PktLineReader<R>,
    ) -> Result<Vec<u8>> {
        // Fast-fast path: the file was hydrated in a previous session
        // and its stat fingerprint still matches. Emit the cached
        // pointer verbatim without reading any content from the
        // stream — no CDC, no hashing, no staging lock. Makes
        // `git status` / `git diff` / `git pull` instant on hydrated
        // files, even when another crab process holds the staging
        // lock.
        //
        // We must still drain the reader to honor the filter-process
        // protocol (git expects us to consume the content pkt-lines
        // up to and including flush).
        if let Some(bytes) = self.try_hydrated_cache_hit(pathname) {
            while reader.read_packet()?.is_some() {
                // discard — we already know the answer
            }
            return Ok(bytes);
        }

        // Resolve the filter handler for this path from .gitattributes.
        // This runs BEFORE blob classification so explicit user rules
        // always take precedence over pointer format detection.
        let resolved_filter = self.resolve_filter_for(pathname);

        // Buffer only the bounded pointer probe. A pointer may span packets,
        // and a pointer-shaped first packet may be followed by real content;
        // classification before either condition is known can bypass cleaning
        // or grow an unbounded passthrough Vec.
        let Some(mut prefix) = reader.read_packet()?.map(<[u8]>::to_vec) else {
            // Empty stream: mirror `clean_file(vec![])`.
            match resolved_filter {
                Some(crate::git::filter_attr_cache::FilterKind::Lfs) => {
                    return self.lfs_clean_path(pathname, Vec::new());
                }
                _ => {
                    return self.crab_clean_path(pathname, Vec::new());
                }
            }
        };

        let mut reached_flush = false;
        while prefix.len() < MAX_LFS_POINTER_SIZE {
            let Some(packet) = reader.read_packet()? else {
                reached_flush = true;
                break;
            };
            prefix.extend_from_slice(packet);
        }

        // Pointer passthrough with filter-awareness: if the complete content
        // is a pointer and it matches the resolved filter, pass it through.
        // If the stream exceeded the pointer cutoff or the format disagrees
        // with the filter rule, re-process it with the selected pipeline.
        let ptr_kind = if reached_flush {
            classify(&prefix)
        } else {
            PointerKind::NotAPointer
        };
        let should_passthrough = match (resolved_filter, &ptr_kind) {
            (Some(crate::git::filter_attr_cache::FilterKind::Lfs), PointerKind::Lfs(_)) => {
                !lfs_extensions_configured_for_clean()
            }
            (Some(crate::git::filter_attr_cache::FilterKind::Crab), PointerKind::Crab(_))
            | (None, PointerKind::Lfs(_) | PointerKind::Crab(_)) => true,
            // Filter disagrees with pointer → warn and re-process
            (Some(filter), kind) => {
                tracing::warn!(
                    path = %pathname,
                    resolved = ?filter,
                    blob = ?kind,
                    "pointer format mismatch: .gitattributes filter disagrees with blob content; re-processing"
                );
                false
            }
            _ => false,
        };

        if should_passthrough {
            tracing::debug!(
                path = %pathname,
                size = prefix.len(),
                "clean_stream: content is already a matching pointer, passing through"
            );
            return Ok(prefix);
        }

        // Explicit attributes are the routing contract. Unmatched paths
        // remain on Crab/XET because the filter does not yet have complete
        // file size and version-history evidence for an automatic decision.
        match resolved_filter {
            Some(crate::git::filter_attr_cache::FilterKind::Lfs) => {
                let extensions = crate::lfs::extension::configured_extensions_sorted()?;
                let lfs_dir = self.lfs_dir()?;
                let mut writer = crate::lfs::cache::ObjectWriter::new(&lfs_dir)?;
                std::io::Write::write_all(&mut writer, &prefix).map_err(CrabError::Io)?;
                if !reached_flush {
                    while let Some(pkt) = reader.read_packet()? {
                        std::io::Write::write_all(&mut writer, pkt).map_err(CrabError::Io)?;
                    }
                }
                let cleaned = crate::lfs::extension::clean_staged_with_extensions(
                    writer.finish()?,
                    &lfs_dir,
                    pathname,
                    &extensions,
                )?;
                self.finish_lfs_clean(pathname, cleaned.staged, cleaned.pointer_extensions)
            }
            _ => {
                // Crab/XET path: stream into the hasher and chunker.
                // This is the default when no filter matches (backward compat).
                self.crab_clean_stream(pathname, prefix, reader, reached_flush)
            }
        }
    }

    /// Crab streaming clean: drives [`GearChunker::feed`] off a
    /// pkt-line reader, buffers chunks up to `chunk_buffer_cap`, spills
    /// larger streams into provisional staging, runs [`try_fast_path`]
    /// at EOF, and adopts staged rows under the final file hash before
    /// emitting the pointer.
    ///
    /// `first` holds the bounded pointer probe that [`clean_stream`] already
    /// consumed. When that probe reached the flush packet, `reached_flush`
    /// prevents a second read past the command boundary.
    ///
    /// [`GearChunker::feed`]: crab_xet::chunker::GearChunker::feed
    /// [`try_fast_path`]: Self::try_fast_path
    fn crab_clean_stream<R: std::io::Read>(
        &mut self,
        pathname: &str,
        first: Vec<u8>,
        reader: &mut crate::git::filter_process::PktLineReader<R>,
        reached_flush: bool,
    ) -> Result<Vec<u8>> {
        use crab_xet::chunker::GearChunker;

        let staging_available = self.staging_unavailable.is_none();
        let mut file_hasher = blake3::Hasher::new();
        let mut chunker = staging_available.then(GearChunker::new);
        let mut buffered_stager = BatchedChunkStager::new(1);
        let mut provisional_stager: Option<ProvisionalChunkStager> = None;
        let mut recipe_recorder = RecipeRecorder::new(ChunkingPolicyId::XetGearV1_64KiB);
        let mut file_size: u64 = 0;

        let result = (|| -> Result<Vec<u8>> {
            // Feed the previously-peeked first packet.
            if !first.is_empty() {
                file_size += first.len() as u64;
                file_hasher.update(&first);
                if let Some(chunker) = chunker.as_mut() {
                    let new_chunks = chunker.feed(&first);
                    buffer_or_stage_clean_chunks(
                        new_chunks,
                        &mut buffered_stager,
                        &mut provisional_stager,
                        &mut self.chunk_stager,
                        &mut recipe_recorder,
                        pathname,
                        self.chunk_buffer_cap,
                    )?;
                }
            }

            // Drain the remaining packets.
            if !reached_flush {
                while let Some(pkt) = reader.read_packet()? {
                    file_size += pkt.len() as u64;
                    file_hasher.update(pkt);
                    if let Some(chunker) = chunker.as_mut() {
                        let new_chunks = chunker.feed(pkt);
                        buffer_or_stage_clean_chunks(
                            new_chunks,
                            &mut buffered_stager,
                            &mut provisional_stager,
                            &mut self.chunk_stager,
                            &mut recipe_recorder,
                            pathname,
                            self.chunk_buffer_cap,
                        )?;
                    }
                }
            }

            if let Some(chunker) = chunker.take()
                && let Some(last) = chunker.finalize()
            {
                buffer_or_stage_clean_chunks(
                    vec![last],
                    &mut buffered_stager,
                    &mut provisional_stager,
                    &mut self.chunk_stager,
                    &mut recipe_recorder,
                    pathname,
                    self.chunk_buffer_cap,
                )?;
            }

            let file_hash: [u8; 32] = *file_hasher.finalize().as_bytes();
            let total_chunks = match provisional_stager.as_ref() {
                Some(provisional) => provisional.chunk_count()?,
                None => buffered_stager.chunk_count(),
            };

            tracing::debug!(
                path = %pathname,
                size = file_size,
                file_hash = %hex_encode(&file_hash),
                chunks = total_chunks,
                "clean_stream: hash + CDC complete (streaming)"
            );

            // Before the bloom/file-index fast path, consult the git
            // index for a matching pointer. Hydrated files always
            // produce content whose blake3 hash equals the pointer's
            // file_hash in the index — emit the pointer verbatim with
            // zero network traffic and no staging lock needed.
            if let Some(pointer_bytes) = self.try_index_pointer_match(pathname, &file_hash) {
                discard_provisional_stager(&mut provisional_stager, &mut self.chunk_stager)?;
                self.remember_hydrated_pointer(pathname, &pointer_bytes);
                return Ok(pointer_bytes);
            }

            // Fast-path check using the computed hash. On hit, the chunks
            // accumulated in memory are dropped; provisional rows are
            // explicitly discarded. No final staging is needed, so this
            // succeeds even when `.crab/staging` is locked — the
            // hydrated-file workflow depends on this.
            if let Some(pointer_bytes) = self.try_fast_path(file_size, &file_hash)? {
                discard_provisional_stager(&mut provisional_stager, &mut self.chunk_stager)?;
                self.remember_hydrated_pointer(pathname, &pointer_bytes);
                return Ok(pointer_bytes);
            }

            // Slow path: we're about to stage chunks. Refuse now if the
            // stager is a no-op. See the mirror check in
            // [`crab_clean_path`] for the failure mode this prevents
            // (pointer emitted without backing chunks).
            self.check_staging_available()?;

            let recipe = recipe_recorder.seal(MerkleHash::from(file_hash), file_size)?;
            if self.chunk_stager.has_recipe(&recipe)? {
                discard_provisional_stager(&mut provisional_stager, &mut self.chunk_stager)?;
                self.chunk_stager
                    .publish_recipe(Path::new(pathname), &recipe)?;
                let pointer = self.build_pointer(file_hash, file_size, None);
                let pointer_bytes = pointer.serialize();
                self.remember_hydrated_pointer(pathname, &pointer_bytes);
                return Ok(pointer_bytes);
            }
            if provisional_stager.is_none() {
                let mut provisional = ProvisionalChunkStager::new(pathname);
                buffered_stager.spill_into(&mut provisional, &mut self.chunk_stager)?;
                provisional_stager = Some(provisional);
            }
            provisional_stager
                .as_mut()
                .ok_or_else(|| CrabError::Internal("missing clean staging attempt".to_owned()))?
                .checkpoint(&mut self.chunk_stager, &file_hash, file_size)?;
            self.chunk_stager
                .publish_recipe(Path::new(pathname), &recipe)?;

            let pointer = self.build_pointer(file_hash, file_size, None);
            let pointer_bytes = pointer.serialize();
            self.remember_hydrated_pointer(pathname, &pointer_bytes);
            Ok(pointer_bytes)
        })();

        if result.is_err()
            && let Err(e) =
                discard_provisional_stager(&mut provisional_stager, &mut self.chunk_stager)
        {
            tracing::warn!(error = %e, "clean filter: failed to discard provisional staging after error");
        }

        result
    }

    /// Construct a [`Pointer`], attaching a `shard-hint` when one is
    /// available. An explicit `shard_hint` (e.g. from a file-index HEAD
    /// response on the fast path) takes precedence; otherwise the local
    /// shard-hints cache is consulted. Missing entries are the common
    /// case on first push and simply produce a hint-less pointer.
    fn build_pointer(
        &self,
        file_hash: [u8; 32],
        file_size: u64,
        shard_hint: Option<[u8; 32]>,
    ) -> Pointer {
        let pointer = self.shard_hints.pointer_for(file_hash, file_size);
        match shard_hint {
            Some(hint) => pointer.with_shard_hint(hint),
            None => pointer,
        }
    }

    /// Attempt to resolve `pathname` to an existing pointer blob in
    /// the git index whose file-hash matches `computed_hash`. A match
    /// proves the working-tree content is a hydrated version of that
    /// pointer and lets the clean filter emit the index's pointer
    /// verbatim — no staging, no network.
    ///
    /// Returns `None` when:
    ///   - the repo root is unknown,
    ///   - the git index is missing or unreadable,
    ///   - the path has no entry,
    ///   - the blob isn't a parseable crab pointer, or
    ///   - the pointer's file_hash doesn't match.
    ///
    /// All failures degrade to the next fallback — the index lookup is
    /// strictly an optimization for the hydrated-file flow.
    fn try_index_pointer_match(&self, pathname: &str, computed_hash: &[u8; 32]) -> Option<Vec<u8>> {
        let root = self.repo_root.as_ref()?;
        let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(root).ok()?;
        let index_path = ctx.index_path();
        if !index_path.is_file() {
            return None;
        }

        let index = gix_index::File::at(
            index_path,
            gix_hash::Kind::Sha1,
            true,
            gix_index::decode::Options::default(),
        )
        .ok()?;

        // Find the entry matching this pathname. The index stores
        // paths as raw bytes; comparing against the pkt-line pathname
        // (UTF-8 forward-slashed) matches git's normal lookup.
        let entry = index
            .entries()
            .iter()
            .find(|e| e.path(&index) == pathname.as_bytes())?;

        let blob_id = entry.id;

        // Read the blob from the object DB. Pointers are tiny (<1 KiB)
        // so this is cheap.
        let odb = gix_odb::at(ctx.objects_dir()).ok()?;
        let mut buf = Vec::new();
        let data = odb.try_find(&blob_id, &mut buf).ok()??;
        if data.kind != gix_object::Kind::Blob {
            return None;
        }
        let blob_bytes = data.data.to_vec();

        // Parse as a crab pointer. Non-pointer blobs mean the
        // working-tree content shouldn't be cleaned to a pointer
        // anyway — fall through to the CDC path.
        let pointer = Pointer::parse(&blob_bytes).ok()?;
        if &pointer.file_hash != computed_hash {
            // Content hash doesn't match the pointer — user has
            // actually modified the file. Fall through so the
            // filter produces the new pointer (requiring staging).
            return None;
        }

        tracing::debug!(
            path = %pathname,
            file_hash = %hex_encode(computed_hash),
            "clean: content matches git-index pointer, emitting pointer verbatim"
        );
        Some(blob_bytes)
    }

    /// Attempt the fast path: skip staging if the file is already known.
    ///
    /// Eligible when:
    /// - `file_size >= fastpath_min_size`
    /// - Bloom filter reports a possible hit
    /// - HEAD to file-index confirms existence (200)
    ///
    /// On bloom hit + HEAD miss, increments the false-positive counter.
    fn try_fast_path(&mut self, file_size: u64, file_hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        // When staging is unavailable, the normal gates (size
        // threshold + bloom filter) are too strict: the bloom never
        // got seeded on fresh clones (it's populated by pushes), so
        // any hydrated file would fall through to the slow path and
        // fail with `StagingLocked`. In that state, issue an
        // unconditional HEAD against the file-index — paying one
        // extra HEAP per clean is cheap compared to failing the
        // user's `git status` / `git diff`.
        let staging_is_unavailable = self.staging_unavailable.is_some();

        // Gate: file must be large enough, unless we have to take
        // the fast path anyway.
        if !staging_is_unavailable && file_size < self.fastpath_min_size {
            return Ok(None);
        }

        // Gate: bloom filter must report a possible hit, unless we
        // have to take the fast path anyway.
        if !staging_is_unavailable && !self.bloom.maybe_contains(file_hash) {
            return Ok(None);
        }

        // Already confirmed in this session? Skip the HEAD.
        if let Some(shard_hint) = self.confirmed_hashes.get(file_hash).copied() {
            self.ctx.metrics().inc_clean_fastpath_taken();
            let pointer = self.build_pointer(*file_hash, file_size, shard_hint);
            return Ok(Some(pointer.serialize()));
        }

        // HEAD the file-index.
        let head_result = self.file_index_checker.head_file_index(file_hash)?;

        if head_result.exists {
            self.confirmed_hashes
                .insert(*file_hash, head_result.shard_hint);
            self.ctx.metrics().inc_clean_fastpath_taken();

            // Remember this hash so subsequent clean sessions also
            // fast-path it (seeds the persisted bloom filter).
            self.bloom.insert(file_hash);

            let pointer = self.build_pointer(*file_hash, file_size, head_result.shard_hint);

            tracing::debug!(
                file_hash = %hex_encode(file_hash),
                shard_hint = ?pointer.shard_hint.map(|h| hex_encode(&h)),
                staging_unavailable = staging_is_unavailable,
                "clean fast path: file already known"
            );

            return Ok(Some(pointer.serialize()));
        }

        // Bloom false positive: file not actually in the index.
        self.ctx.metrics().inc_clean_fastpath_false_positives();
        tracing::debug!(
            file_hash = %hex_encode(file_hash),
            "clean fast path: bloom false positive"
        );

        Ok(None)
    }
}

fn lfs_extensions_configured_for_clean() -> bool {
    crate::lfs::extension::configured_extensions()
        .map(|extensions| !extensions.is_empty())
        .unwrap_or(false)
}

/// Check whether a pathname matches an LFS track pattern in `.gitattributes`.
///
/// Reads `.gitattributes` from `root` and checks if any line with
/// `filter=lfs` has a glob pattern matching `pathname`. Returns `false`
/// when `.gitattributes` is missing or unreadable.
#[cfg(test)]
fn is_lfs_tracked_in(root: &std::path::Path, pathname: &str) -> bool {
    let ga_path = root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(_) => return false,
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !trimmed.contains("filter=lfs") {
            continue;
        }
        // Extract the glob pattern (first whitespace-delimited token).
        if let Some(pattern) = trimmed.split_whitespace().next() {
            if glob_matches(pattern, pathname) {
                return true;
            }
        }
    }

    false
}

/// Simple glob matching for `.gitattributes` patterns.
///
/// Supports `*` (matches any sequence except `/`), `**` (matches any
/// sequence including `/`), and `?` (matches any single character except `/`).
/// Literal characters are compared case-sensitively.
#[allow(dead_code)]
fn glob_matches(pattern: &str, path: &str) -> bool {
    // Use the `glob` crate's pattern matching if available, otherwise
    // fall back to a simple implementation that handles the common cases.
    glob_match_impl(pattern.as_bytes(), path.as_bytes())
}

/// Recursive glob matcher operating on byte slices.
#[allow(dead_code)]
fn glob_match_impl(pattern: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < text.len() {
        if pi < pattern.len() && pattern[pi] == b'*' {
            // Check for `**` (matches path separators too).
            if pi + 1 < pattern.len() && pattern[pi + 1] == b'*' {
                // `**` — try matching the rest of the pattern at every
                // position in the remaining text.
                let rest = &pattern[pi + 2..];
                // Skip optional `/` after `**`.
                let rest = if rest.first() == Some(&b'/') {
                    &rest[1..]
                } else {
                    rest
                };
                // Try matching from every position.
                for i in ti..=text.len() {
                    if glob_match_impl(rest, &text[i..]) {
                        return true;
                    }
                }
                return false;
            }
            // Single `*` — matches anything except `/`.
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if pi < pattern.len()
            && (pattern[pi] == b'?' && text[ti] != b'/' || pattern[pi] == text[ti])
        {
            pi += 1;
            ti += 1;
        } else if star_pi != usize::MAX {
            // Backtrack to the last `*`.
            pi = star_pi + 1;
            star_ti += 1;
            // `*` must not match `/`.
            if text[star_ti - 1] == b'/' {
                return false;
            }
            ti = star_ti;
        } else {
            return false;
        }
    }

    // Consume trailing `*` patterns.
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

/// Encode a 32-byte hash as lowercase hex.
fn hex_encode(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Wrap a raw byte slice as one or more pkt-line data packets followed by
/// a flush packet, suitable for feeding into
/// [`PktLineReader::from_slice`](crate::git::filter_process::PktLineReader::from_slice).
///
/// Chunks larger than the pkt-line body cap (65 516 B, per the git
/// protocol) are split across multiple packets. An empty input produces
/// just a flush packet, mirroring what git itself sends for empty files.
fn frame_as_pktlines(content: &[u8]) -> Vec<u8> {
    // The git protocol caps a pkt-line at 65 520 bytes total including the
    // 4-byte length header, so the body fits in 65 516 bytes.
    const PKT_LINE_MAX_BODY: usize = 65516;

    // Pre-size: 4-byte header per packet + 4-byte trailing flush.
    let header_overhead = content.len().div_ceil(PKT_LINE_MAX_BODY.max(1)) * 4 + 4;
    let mut out = Vec::with_capacity(content.len() + header_overhead);

    for chunk in content.chunks(PKT_LINE_MAX_BODY) {
        let len = chunk.len() + 4;
        // `len` <= 65 520, which fits in four hex digits.
        out.extend_from_slice(format!("{len:04x}").as_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(b"0000");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::AppContext;
    use sha2::Digest;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn git_repo_tempdir() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        let status = std::process::Command::new("git")
            .args(["init", "-q", dir.path().to_str()?])
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        Some(dir)
    }

    // --- Bloom filter tests ---

    #[test]
    fn bloom_insert_and_query() {
        let mut bloom = FileHashBloom::new(100);
        let hash = [0xABu8; 32];
        assert!(!bloom.maybe_contains(&hash));
        bloom.insert(&hash);
        assert!(bloom.maybe_contains(&hash));
    }

    #[test]
    fn bloom_definite_miss() {
        let bloom = FileHashBloom::new(100);
        let hash = [0x42u8; 32];
        assert!(!bloom.maybe_contains(&hash));
    }

    #[test]
    fn bloom_load_rejects_zero_bit_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bloom.bin");
        std::fs::write(&path, 0u64.to_le_bytes()).unwrap();

        assert!(
            FileHashBloom::load(&path).is_none(),
            "corrupt bloom cache must degrade to an empty bloom, not panic on modulo by zero"
        );
    }

    #[derive(Default)]
    struct CountingChunkStageStats {
        calls: std::sync::Mutex<Vec<(usize, u64)>>,
        flushes: std::sync::Mutex<u32>,
    }

    struct CountingChunkStager {
        stats: Arc<CountingChunkStageStats>,
    }

    impl ChunkStager for CountingChunkStager {
        fn stage_chunks(
            &self,
            chunks: &[([u8; 32], Bytes)],
            _file_hash: &[u8; 32],
            _file_size: u64,
            chunk_index_offset: u64,
        ) -> Result<()> {
            self.stats
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((chunks.len(), chunk_index_offset));
            Ok(())
        }

        fn flush_pending(&self) -> Result<()> {
            let mut flushes = self
                .stats
                .flushes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *flushes += 1;
            Ok(())
        }
    }

    struct FailAfterFirstChunkStager {
        calls: Arc<AtomicUsize>,
    }

    impl ChunkStager for FailAfterFirstChunkStager {
        fn stage_chunks(
            &self,
            _chunks: &[([u8; 32], Bytes)],
            _file_hash: &[u8; 32],
            _file_size: u64,
            _chunk_index_offset: u64,
        ) -> Result<()> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            if call > 1 {
                return Err(CrabError::Internal("injected staging failure".into()));
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingChunkStagerStats {
        staged_hashes: std::sync::Mutex<Vec<[u8; 32]>>,
        adopted: std::sync::Mutex<Vec<([u8; 32], [u8; 32], u64)>>,
        discarded: std::sync::Mutex<Vec<[u8; 32]>>,
        flushes: AtomicUsize,
    }

    struct RecordingChunkStager {
        stats: Arc<RecordingChunkStagerStats>,
    }

    impl ChunkStager for RecordingChunkStager {
        fn stage_chunks(
            &self,
            _chunks: &[([u8; 32], Bytes)],
            file_hash: &[u8; 32],
            _file_size: u64,
            _chunk_index_offset: u64,
        ) -> Result<()> {
            self.stats
                .staged_hashes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(*file_hash);
            Ok(())
        }

        fn adopt_staged_file(
            &self,
            source_file_hash: &[u8; 32],
            target_file_hash: &[u8; 32],
            file_size: u64,
        ) -> Result<()> {
            self.stats
                .adopted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((*source_file_hash, *target_file_hash, file_size));
            Ok(())
        }

        fn discard_staged_file(&self, file_hash: &[u8; 32]) -> Result<()> {
            self.stats
                .discarded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(*file_hash);
            Ok(())
        }

        fn flush_pending(&self) -> Result<()> {
            self.stats.flushes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    struct FailSecondRecordingChunkStager {
        stats: Arc<RecordingChunkStagerStats>,
        calls: AtomicUsize,
    }

    impl ChunkStager for FailSecondRecordingChunkStager {
        fn stage_chunks(
            &self,
            _chunks: &[([u8; 32], Bytes)],
            file_hash: &[u8; 32],
            _file_size: u64,
            _chunk_index_offset: u64,
        ) -> Result<()> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            if call > 1 {
                return Err(CrabError::Internal(
                    "injected provisional staging failure".into(),
                ));
            }
            self.stats
                .staged_hashes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(*file_hash);
            Ok(())
        }

        fn discard_staged_file(&self, file_hash: &[u8; 32]) -> Result<()> {
            self.stats
                .discarded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(*file_hash);
            Ok(())
        }
    }

    struct FailDiscardRecordingChunkStager {
        stats: Arc<RecordingChunkStagerStats>,
    }

    impl ChunkStager for FailDiscardRecordingChunkStager {
        fn stage_chunks(
            &self,
            _chunks: &[([u8; 32], Bytes)],
            file_hash: &[u8; 32],
            _file_size: u64,
            _chunk_index_offset: u64,
        ) -> Result<()> {
            self.stats
                .staged_hashes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(*file_hash);
            Ok(())
        }

        fn discard_staged_file(&self, file_hash: &[u8; 32]) -> Result<()> {
            self.stats
                .discarded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(*file_hash);
            Err(CrabError::Internal(
                "injected provisional discard failure".into(),
            ))
        }
    }

    #[test]
    fn batched_checkpoint_flushes_once_after_all_batches() {
        let mut batches = BatchedChunkStager {
            batches: vec![
                vec![([1; 32], Bytes::from_static(b"one"))],
                vec![
                    ([2; 32], Bytes::from_static(b"two")),
                    ([3; 32], Bytes::from_static(b"three")),
                ],
            ],
            current_batch: Vec::new(),
            total_chunks: 3,
            total_bytes: 11,
        };
        let stats = Arc::new(CountingChunkStageStats::default());
        let mut stager: Box<dyn ChunkStager> = Box::new(CountingChunkStager {
            stats: Arc::clone(&stats),
        });

        batches.checkpoint(&mut stager, &[9; 32], 11).unwrap();

        assert_eq!(
            *stats
                .flushes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            1
        );
        assert_eq!(
            *stats
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![(1, 0), (2, 1)]
        );
    }

    #[test]
    fn batched_stage_error_keeps_stager_in_place() {
        let batches = vec![
            vec![([1; 32], Bytes::from_static(b"one"))],
            vec![([2; 32], Bytes::from_static(b"two"))],
        ];
        let calls = Arc::new(AtomicUsize::new(0));
        let mut stager: Box<dyn ChunkStager> = Box::new(FailAfterFirstChunkStager {
            calls: Arc::clone(&calls),
        });

        let err = BatchedChunkStager::stage_batches(batches, &mut stager, &[9; 32], 11)
            .expect_err("second batch should fail");
        assert!(
            err.to_string().contains("injected staging failure"),
            "unexpected error: {err}"
        );

        let err = stager
            .stage_chunks(&[], &[9; 32], 11, 0)
            .expect_err("same stager should still fail, not accept as no-op");
        assert!(
            err.to_string().contains("injected staging failure"),
            "expected failing stager, got: {err}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    // --- Mock file-index checker ---

    struct MockFileIndexChecker {
        known_hashes: HashSet<[u8; 32]>,
        shard_hint: Option<[u8; 32]>,
    }

    impl FileIndexChecker for MockFileIndexChecker {
        fn head_file_index(&self, file_hash: &[u8; 32]) -> Result<FileIndexHeadResult> {
            Ok(FileIndexHeadResult {
                exists: self.known_hashes.contains(file_hash),
                shard_hint: if self.known_hashes.contains(file_hash) {
                    self.shard_hint
                } else {
                    None
                },
            })
        }
    }

    struct CountingFileIndexChecker {
        known_hashes: HashSet<[u8; 32]>,
        shard_hint: Option<[u8; 32]>,
        head_calls: Arc<AtomicUsize>,
    }

    impl FileIndexChecker for CountingFileIndexChecker {
        fn head_file_index(&self, file_hash: &[u8; 32]) -> Result<FileIndexHeadResult> {
            self.head_calls.fetch_add(1, Ordering::Relaxed);
            let exists = self.known_hashes.contains(file_hash);
            Ok(FileIndexHeadResult {
                exists,
                shard_hint: exists.then_some(self.shard_hint).flatten(),
            })
        }
    }

    // --- StoreFileIndexChecker tests ---

    fn seed_file_index(
        rt: &tokio::runtime::Runtime,
        store: Arc<dyn object_store::ObjectStore>,
        repo_prefix: &str,
        file_hash: &MerkleHash,
        shard_hash: &MerkleHash,
    ) {
        rt.block_on(async {
            let storage = crab_storage::Store::new(Arc::clone(&store));
            let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
            let (shard_index_hash, _, shard_write) = crab_metadata::manifests::append_shard_index(
                crab_metadata::segmented::SegmentIndex::default(),
                1,
                &[shard_hash.hex()],
            )
            .expect("build shard index");
            crab_metadata::manifest_store::upload_segmented_bulk(
                &storage,
                &router,
                &crab_metadata::manifests::BulkData {
                    shard_index: shard_write,
                    pack_index: crab_metadata::segmented::SegmentWrite::default(),
                },
            )
            .await
            .expect("upload shard index");
            let mut manifest =
                crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
            manifest.generation = 1;
            manifest.shard_index_hash = shard_index_hash.clone();
            manifest.seal_git_validation();
            crab_metadata::manifest_store::create_manifest(&storage, &router, &manifest)
                .await
                .expect("create manifest");

            let metadb = crate::metadata::MetaDb::new(
                store,
                repo_prefix.to_owned(),
                crate::metadata::MetaDbConfig::for_repo(repo_prefix),
            );
            let file_store = metadb.file_index().await.expect("file index store");
            let mut txn = metadb.new_transaction();
            file_store.save_committed_batch(
                &mut txn,
                &[(
                    *file_hash,
                    crab_metadata::value_codec::CommittedFileRecord {
                        recipe_hash: [0xC7; 32],
                        shard_hash: *shard_hash,
                        committed_generation: 1,
                        shard_index_hash: MerkleHash::from_hex(&shard_index_hash)
                            .expect("valid shard-index hash"),
                    },
                )],
            );
            metadb.commit(txn).await.expect("seed file-index");
            metadb.close_all().await.expect("close metadb");
        });
    }

    #[test]
    fn store_file_index_checker_requires_file_index_and_shard_body() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let repo_prefix = "clean-fast-path-existing";
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let store = crate::storage::store::Store::new(Arc::clone(&inner));
        let router = StoreLayout::new(store.clone(), repo_prefix.to_owned());
        let file_hash = MerkleHash::from([31_u64, 0, 0, 0]);
        let shard_hash = MerkleHash::from([32_u64, 0, 0, 0]);

        seed_file_index(
            &rt,
            Arc::clone(&inner),
            repo_prefix,
            &file_hash,
            &shard_hash,
        );
        rt.block_on(async {
            store
                .put(
                    &router.shard_path(&shard_hash),
                    Bytes::from_static(b"shard body"),
                )
                .await
                .expect("seed shard body");
        });

        let checker = StoreFileIndexChecker::new(router, rt.handle().clone());
        let file_hash_raw: [u8; 32] = file_hash.into();
        let result = checker
            .head_file_index(&file_hash_raw)
            .expect("file-index check");

        assert!(result.exists);
        assert_eq!(result.shard_hint, Some(shard_hash.into()));
    }

    #[test]
    fn store_file_index_checker_rejects_stale_file_index_missing_shard_body() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let repo_prefix = "clean-fast-path-stale";
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let store = crate::storage::store::Store::new(Arc::clone(&inner));
        let router = StoreLayout::new(store, repo_prefix.to_owned());
        let file_hash = MerkleHash::from([33_u64, 0, 0, 0]);
        let shard_hash = MerkleHash::from([34_u64, 0, 0, 0]);

        seed_file_index(
            &rt,
            Arc::clone(&inner),
            repo_prefix,
            &file_hash,
            &shard_hash,
        );

        let checker = StoreFileIndexChecker::new(router, rt.handle().clone());
        let file_hash_raw: [u8; 32] = file_hash.into();
        let result = checker
            .head_file_index(&file_hash_raw)
            .expect("file-index check");

        assert!(!result.exists);
        assert!(result.shard_hint.is_none());
    }

    // --- Clean pipeline tests ---

    #[test]
    fn clean_produces_valid_pointer() {
        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        let content = b"hello world";
        let pointer_bytes = session.clean_file("test.bin", content.to_vec()).unwrap();

        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        let expected_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        assert_eq!(pointer.file_hash, expected_hash);
        assert_eq!(pointer.size, content.len() as u64);
    }

    #[test]
    fn clean_empty_file() {
        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        let content = b"";
        let pointer_bytes = session.clean_file("empty.bin", content.to_vec()).unwrap();

        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.size, 0);
    }

    #[test]
    fn fast_path_taken_when_file_known() {
        let content = vec![0xAA; 128 * 1024 * 1024]; // 128 MiB
        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let shard_hint = [0xBB; 32];

        let mut known = HashSet::new();
        known.insert(file_hash);

        let checker = MockFileIndexChecker {
            known_hashes: known,
            shard_hint: Some(shard_hint),
        };

        let ctx = AppContext::default();
        let mut session = CleanSession::with_deps(
            ctx.clone(),
            Box::new(checker),
            Box::new(NoopChunkStager),
            &[file_hash],
            64 * 1024 * 1024, // 64 MiB threshold
            DEFAULT_CHUNK_BUFFER_CAP,
        );

        let pointer_bytes = session.clean_file("big.bin", content.clone()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();

        assert_eq!(pointer.file_hash, file_hash);
        assert_eq!(pointer.shard_hint, Some(shard_hint));
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_taken, 1);
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_false_positives, 0);
    }

    #[test]
    fn fast_path_bloom_false_positive_counted() {
        let content = vec![0xCC; 128 * 1024 * 1024]; // 128 MiB
        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();

        // Bloom seeded with this hash, but checker says it doesn't exist.
        let checker = MockFileIndexChecker {
            known_hashes: HashSet::new(),
            shard_hint: None,
        };

        let ctx = AppContext::default();
        let mut session = CleanSession::with_deps(
            ctx.clone(),
            Box::new(checker),
            Box::new(NoopChunkStager),
            &[file_hash],
            64 * 1024 * 1024,
            DEFAULT_CHUNK_BUFFER_CAP,
        );

        let _pointer_bytes = session.clean_file("big2.bin", content).unwrap();

        assert_eq!(ctx.metrics().snapshot().clean_fastpath_taken, 0);
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_false_positives, 1);
    }

    #[test]
    fn fast_path_skipped_for_small_files() {
        let content = b"small file";
        let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();

        let mut known = HashSet::new();
        known.insert(file_hash);

        let checker = MockFileIndexChecker {
            known_hashes: known,
            shard_hint: None,
        };

        let ctx = AppContext::default();
        let mut session = CleanSession::with_deps(
            ctx.clone(),
            Box::new(checker),
            Box::new(NoopChunkStager),
            &[file_hash],
            64 * 1024 * 1024,
            DEFAULT_CHUNK_BUFFER_CAP,
        );

        let _pointer_bytes = session.clean_file("small.bin", content.to_vec()).unwrap();

        // Fast path not taken because file is too small.
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_taken, 0);
    }

    #[test]
    fn slow_path_pointer_carries_cached_shard_hint() {
        // When the shard-hint cache has an entry for the computed file
        // hash, the emitted pointer (slow path — no HEAD confirmation)
        // should include the hint so hydrate can skip the file-index GET.
        let content = b"hello world";
        let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        let shard_hint: [u8; 32] = [0xCD; 32];

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session
            .shard_hints
            .insert(MerkleHash::from(file_hash), MerkleHash::from(shard_hint));

        let pointer_bytes = session.clean_file("test.bin", content.to_vec()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();

        assert_eq!(pointer.file_hash, file_hash);
        assert_eq!(pointer.shard_hint, Some(shard_hint));
    }

    #[test]
    fn slow_path_pointer_has_no_hint_when_cache_miss() {
        // With an empty shard-hint cache, the emitted pointer should have
        // no hint — this is the common case on first push.
        let content = b"hello world";

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);

        let pointer_bytes = session.clean_file("test.bin", content.to_vec()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();

        assert!(pointer.shard_hint.is_none());
    }

    #[test]
    fn crab_clean_refuses_when_staging_is_locked() {
        // Regression guard for the silent-pointer-without-chunks bug:
        // when staging is unavailable, the clean path must return
        // `StagingLocked` instead of producing a valid-looking pointer
        // backed by `NoopChunkStager`. Emitting such a pointer advances
        // git's index past content that will never reach object storage,
        // which breaks hydration on every other clone of the repo.
        let content = b"large file content we would have chunked";

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_staging_locked(Some(12345));

        let result = session.clean_file("big.bin", content.to_vec());

        match result {
            Err(CrabError::StagingLocked { holder_pid }) => {
                assert_eq!(holder_pid, Some(12345));
            }
            other => panic!("expected StagingLocked, got {other:?}"),
        }
    }

    #[test]
    fn crab_clean_refuses_when_staging_is_unavailable() {
        // Same failure mode as the locked variant, but without a known
        // holder PID (e.g. staging directory simply doesn't exist).
        let content = b"another would-be chunked payload";

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_staging_unavailable();

        let result = session.clean_file("big.bin", content.to_vec());

        match result {
            Err(CrabError::StagingLocked { holder_pid }) => {
                assert!(holder_pid.is_none());
            }
            other => panic!("expected StagingLocked, got {other:?}"),
        }
    }

    #[test]
    fn session_isolation_after_reset() {
        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);

        // Simulate a failure + reset.
        session.reset_transient_state();

        // Next clean should still work.
        let content = b"after reset";
        let pointer_bytes = session.clean_file("ok.bin", content.to_vec()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.size, content.len() as u64);
    }

    #[test]
    fn hydrated_cache_short_circuits_clean_when_staging_locked() {
        // Regression guard for the hydrated-file UX bug: once a file
        // has been hydrated and its pointer recorded in the cache,
        // subsequent clean-filter invocations (git status, git diff,
        // git pull) must return the cached pointer without touching
        // staging — even when staging is locked by another process.
        use crate::cache::{HydratedPointerCache, hydrated_pointer};

        let Some(dir) = git_repo_tempdir() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        let root = dir.path();
        let rel = "big.zip";
        let full = root.join(rel);
        std::fs::write(&full, b"hydrated file content that matches the pointer").unwrap();

        // Seed a cached pointer entry for this file.
        let fake_pointer = b"\
version crab/1\nfile-hash 00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\n\
size 45\n";
        let entry = hydrated_pointer::entry_for_path(&full, fake_pointer).unwrap();
        let cache_path = hydrated_pointer::cache_path_for_worktree_root(root).expect("cache path");
        HydratedPointerCache::update_on_disk(&cache_path, [(rel.to_owned(), entry)]).unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(root.to_path_buf());
        // Simulate "another process holds the staging lock". Without
        // the short-circuit this would immediately return
        // StagingLocked.
        session.set_staging_locked(Some(99999));

        let bytes = session
            .clean_file(
                rel,
                b"hydrated file content that matches the pointer".to_vec(),
            )
            .expect("clean must succeed from cached pointer even with staging locked");
        assert_eq!(bytes, fake_pointer);
    }

    #[test]
    fn hydrated_cache_corrupt_pointer_does_not_skip_staging_acquire() {
        use crate::cache::{HydratedPointerCache, hydrated_pointer};

        let Some(dir) = git_repo_tempdir() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        let root = dir.path();
        let rel = "corrupt-cache.bin";
        let full = root.join(rel);
        let content = b"hydrated file content with corrupt cached pointer";
        std::fs::write(&full, content).unwrap();

        let mut entry = hydrated_pointer::entry_for_path(
            &full,
            b"version crab/1\nfile-hash 00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\nsize 48\n",
        )
        .unwrap();
        entry.pointer_hex = "not hex".to_owned();
        let cache_path = hydrated_pointer::cache_path_for_worktree_root(root).expect("cache path");
        HydratedPointerCache::update_on_disk(&cache_path, [(rel.to_owned(), entry)]).unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(root.to_path_buf());
        assert!(
            !session.has_live_hydrated_entry(rel),
            "dispatch must acquire staging when the live cache entry cannot decode"
        );

        session.set_staging_locked(Some(99999));
        let err = session.clean_file(rel, content.to_vec()).unwrap_err();
        assert!(matches!(
            err,
            CrabError::StagingLocked {
                holder_pid: Some(99999)
            }
        ));

        session.persist_hydrated_cache_invalidations();
        let reloaded = HydratedPointerCache::load_sync(&cache_path);
        assert!(
            reloaded.get(rel).is_none(),
            "corrupt live entry should be invalidated after the failed slow-path attempt"
        );
    }

    #[test]
    fn hydrated_cache_falls_through_when_file_modified_after_hydrate() {
        // A hydrated-then-touched file has a stat-mismatched cache
        // entry. The clean filter must drop the entry and run the
        // normal pipeline, not hand back a stale pointer.
        use crate::cache::{HydratedPointerCache, hydrated_pointer};

        let Some(dir) = git_repo_tempdir() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        let root = dir.path();
        let rel = "file.bin";
        let full = root.join(rel);
        std::fs::write(&full, b"original hydrated content").unwrap();

        let fake_pointer = b"version crab/1\nfile-hash abc\nsize 25\n";
        let entry = hydrated_pointer::entry_for_path(&full, fake_pointer).unwrap();
        let cache_path = hydrated_pointer::cache_path_for_worktree_root(root).expect("cache path");
        HydratedPointerCache::update_on_disk(&cache_path, [(rel.to_owned(), entry)]).unwrap();

        // Modify the file so the stat fingerprint no longer matches.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&full, b"modified content has a different size").unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(root.to_path_buf());

        // Clean must go through the crab path (not return the cached
        // pointer verbatim). The exact output isn't the point — it's
        // that it's *not* `fake_pointer`.
        let bytes = session
            .clean_file(rel, b"modified content has a different size".to_vec())
            .unwrap();
        assert_ne!(bytes, fake_pointer);
    }

    #[test]
    fn set_repo_root_uses_linked_worktree_attributes_and_hydrated_cache() {
        use crate::cache::{HydratedPointerCache, hydrated_pointer};
        use crate::git::filter_attr_cache::FilterKind;
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("linked");
        if !Command::new("git")
            .args(["init", "-q", repo.to_str().unwrap()])
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&repo)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&repo)
            .status();
        std::fs::write(repo.join("README.md"), "initial\n").unwrap();
        let _ = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&repo)
            .status();
        if !Command::new("git")
            .args(["commit", "-q", "-m", "initial"])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git commit unavailable or fixture setup failed");
            return;
        }
        if !Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git worktree fixture setup failed");
            return;
        }

        std::fs::write(
            repo.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        std::fs::write(
            linked.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        let rel = "model.bin";
        let hydrated = b"linked worktree hydrated bytes";
        let full_path = linked.join(rel);
        std::fs::write(&full_path, hydrated).unwrap();
        let pointer = b"version crab/1\nfile-hash abc\nsize 28\n";
        let entry = hydrated_pointer::entry_for_path(&full_path, pointer).unwrap();
        let cache_path =
            hydrated_pointer::cache_path_for_worktree_root(&linked).expect("cache path");
        HydratedPointerCache::update_on_disk(&cache_path, [(rel.to_owned(), entry)]).unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(linked);
        session.set_staging_locked(Some(12345));

        assert_eq!(session.resolve_filter_for(rel), Some(FilterKind::Lfs));
        assert_eq!(session.clean_file(rel, hydrated.to_vec()).unwrap(), pointer);
    }

    #[test]
    fn linked_worktree_index_pointer_match_uses_private_index_and_common_objects() {
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("linked");
        if !Command::new("git")
            .args(["init", "-q", repo.to_str().unwrap()])
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&repo)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&repo)
            .status();

        let hydrated = b"linked hydrated content for index fast path".to_vec();
        let pointer = Pointer {
            file_hash: *blake3::hash(&hydrated).as_bytes(),
            size: hydrated.len() as u64,
            shard_hint: None,
        };
        let pointer_bytes = pointer.serialize();
        let rel = "big.bin";
        std::fs::write(repo.join(rel), &pointer_bytes).unwrap();
        let _ = Command::new("git")
            .args(["add", rel])
            .current_dir(&repo)
            .status();
        if !Command::new("git")
            .args(["commit", "-q", "-m", "add pointer"])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git commit unavailable or fixture setup failed");
            return;
        }
        if !Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git worktree fixture setup failed");
            return;
        }

        std::fs::write(linked.join(rel), &hydrated).unwrap();

        let mut session = CleanSession::new(AppContext::default());
        session.set_repo_root(linked);
        session.set_staging_locked(Some(12345));

        let cleaned = session
            .clean_file(rel, hydrated)
            .expect("clean should use linked worktree index fast path");
        assert_eq!(cleaned, pointer_bytes);
    }

    #[test]
    fn lfs_clean_in_linked_worktree_caches_in_common_git_dir() {
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("linked");
        if !Command::new("git")
            .args(["init", "-q", repo.to_str().unwrap()])
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&repo)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&repo)
            .status();
        std::fs::write(repo.join("README.md"), "initial\n").unwrap();
        let _ = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&repo)
            .status();
        if !Command::new("git")
            .args(["commit", "-q", "-m", "initial"])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git commit unavailable or fixture setup failed");
            return;
        }
        if !Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git worktree fixture setup failed");
            return;
        }

        std::fs::write(
            linked.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        let content = b"linked lfs content";
        let mut session = CleanSession::new(AppContext::default());
        session.set_repo_root(linked.clone());

        let pointer_bytes = session.clean_file("model.bin", content.to_vec()).unwrap();
        let pointer = crab_git::lfs_pointer::LfsPointer::parse(&pointer_bytes).unwrap();
        let oid_hex = crab_git::lfs_pointer::hex_encode(&pointer.oid);
        let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(&linked).unwrap();
        let cached_path = ctx
            .lfs_objects_dir()
            .join(&oid_hex[..2])
            .join(&oid_hex[2..4])
            .join(&oid_hex);

        assert_eq!(std::fs::read(cached_path).unwrap(), content);
    }

    #[test]
    fn git_index_pointer_match_short_circuits_clean_without_staging() {
        // Regression guard for the hydrated-file UX: when the working
        // tree has a hydrated file and git's index has its committed
        // pointer blob, the clean filter must reconstruct the pointer
        // from the index (not the staging area). Exercises the
        // `try_index_pointer_match` path: no bloom hit, no hydrated
        // cache, no staging — just the git index + blake3 hash check.
        use std::process::Command;

        // Create a real git repo so gix_index + gix_odb work against it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status();
        let Ok(s) = status else {
            // Git not available in test env — skip rather than fail.
            return;
        };
        if !s.success() {
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(root)
            .status();

        // Commit a crab pointer blob at a known path. The pointer's
        // file_hash must match the blake3 of the hydrated content we
        // later "clean" from.
        let hydrated = b"the hydrated content that matches the pointer".to_vec();
        let file_hash: [u8; 32] = *blake3::hash(&hydrated).as_bytes();
        let pointer = Pointer {
            file_hash,
            size: hydrated.len() as u64,
            shard_hint: None,
        };
        let pointer_bytes = pointer.serialize();

        let rel = "big.bin";
        std::fs::write(root.join(rel), &pointer_bytes).unwrap();
        let _ = Command::new("git")
            .args(["add", rel])
            .current_dir(root)
            .status();
        let _ = Command::new("git")
            .args(["commit", "-q", "-m", "add pointer"])
            .current_dir(root)
            .status();

        // Now replace the working-tree file with hydrated content
        // (simulating `crab hydrate`).
        std::fs::write(root.join(rel), &hydrated).unwrap();

        // Clean with staging locked — must still succeed via the
        // index-pointer-match path.
        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(root.to_path_buf());
        session.set_staging_locked(Some(12345));

        let cleaned = session
            .clean_file(rel, hydrated)
            .expect("clean must succeed via git-index pointer match");

        // The cleaned bytes must parse as a pointer with the same
        // file_hash, proving we reproduced the committed pointer.
        let reparsed = Pointer::parse(&cleaned).expect("cleaned output is a valid pointer");
        assert_eq!(reparsed.file_hash, file_hash);
    }

    #[test]
    fn fast_path_second_call_uses_confirmed_cache() {
        let content = b"duplicate content already present remotely".to_vec();
        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let shard_hint = [0xEE; 32];

        let mut known = HashSet::new();
        known.insert(file_hash);

        let head_calls = Arc::new(AtomicUsize::new(0));
        let checker = CountingFileIndexChecker {
            known_hashes: known,
            shard_hint: Some(shard_hint),
            head_calls: Arc::clone(&head_calls),
        };

        let ctx = AppContext::default();
        let mut session = CleanSession::with_deps(
            ctx.clone(),
            Box::new(checker),
            Box::new(NoopChunkStager),
            &[file_hash],
            0,
            DEFAULT_CHUNK_BUFFER_CAP,
        );

        session.clean_file("big.bin", content.clone()).unwrap();
        let pointer_bytes = session.clean_file("copy.bin", content).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();

        assert_eq!(pointer.file_hash, file_hash);
        assert_eq!(pointer.shard_hint, Some(shard_hint));
        assert_eq!(head_calls.load(Ordering::Relaxed), 1);
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_taken, 2);
    }

    #[test]
    fn should_auto_hydrate_returns_false_when_auto_disabled() {
        let ctx = AppContext::default();
        let session = CleanSession::new(ctx);
        assert!(!session.should_auto_hydrate("models/big.safetensors"));
    }

    #[test]
    fn should_auto_hydrate_returns_true_when_path_matches_patterns() {
        use crate::core::config::{Config, HydrateConfig};
        use tokio_util::sync::CancellationToken;

        let config = Config {
            hydrate: HydrateConfig {
                include: vec!["*.safetensors".to_owned()],
                exclude: Vec::new(),
                auto: true,
                ..HydrateConfig::default()
            },
            ..Config::default()
        };
        let ctx = AppContext::new(config, CancellationToken::new());
        let session = CleanSession::new(ctx);
        assert!(session.should_auto_hydrate("models/big.safetensors"));
        assert!(!session.should_auto_hydrate("models/big.bin"));
    }

    // --- Glob matching tests ---

    #[test]
    fn glob_matches_extension_wildcard() {
        assert!(glob_matches("*.bin", "model.bin"));
        assert!(glob_matches("*.bin", "a.bin"));
        assert!(!glob_matches("*.bin", "model.txt"));
        // Single `*` does not cross directory boundaries.
        assert!(!glob_matches("*.bin", "dir/model.bin"));
    }

    #[test]
    fn glob_matches_double_star() {
        assert!(glob_matches("**/*.bin", "model.bin"));
        assert!(glob_matches("**/*.bin", "dir/model.bin"));
        assert!(glob_matches("**/*.bin", "a/b/c/model.bin"));
        assert!(!glob_matches("**/*.bin", "model.txt"));
    }

    #[test]
    fn glob_matches_exact_filename() {
        assert!(glob_matches("model.bin", "model.bin"));
        assert!(!glob_matches("model.bin", "other.bin"));
    }

    #[test]
    fn glob_matches_question_mark() {
        assert!(glob_matches("?.bin", "a.bin"));
        assert!(!glob_matches("?.bin", "ab.bin"));
        // `?` does not match `/`.
        assert!(!glob_matches("?.bin", "/.bin"));
    }

    // --- .gitattributes LFS tracking tests ---

    #[test]
    fn is_lfs_tracked_matches_extension_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        assert!(is_lfs_tracked_in(dir.path(), "model.bin"));
        assert!(!is_lfs_tracked_in(dir.path(), "readme.txt"));
    }

    #[test]
    fn is_lfs_tracked_ignores_crab_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        assert!(!is_lfs_tracked_in(dir.path(), "model.bin"));
    }

    #[test]
    fn is_lfs_tracked_handles_mixed_filters() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n\
             *.safetensors filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        assert!(is_lfs_tracked_in(dir.path(), "model.bin"));
        assert!(!is_lfs_tracked_in(dir.path(), "weights.safetensors"));
    }

    #[test]
    fn is_lfs_tracked_returns_false_when_no_gitattributes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_lfs_tracked_in(dir.path(), "model.bin"));
    }

    #[test]
    fn is_lfs_tracked_ignores_comments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "# *.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        assert!(!is_lfs_tracked_in(dir.path(), "model.bin"));
    }

    #[cfg(feature = "gix-pathmatch")]
    #[test]
    fn set_repo_root_defers_attrs_reader_until_legacy_lfs_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());

        assert!(
            session.lfs_attrs.get().is_none(),
            "set_repo_root must not initialize AttrsReader"
        );
        assert_eq!(
            session.resolve_filter_for("model.bin"),
            Some(crate::git::filter_attr_cache::FilterKind::Lfs)
        );
        assert!(
            session.lfs_attrs.get().is_none(),
            "filter-process cache lookup must not initialize AttrsReader"
        );

        assert!(session.is_lfs_tracked("model.bin"));
        assert!(
            session.lfs_attrs.get().is_some(),
            "legacy LFS classifier is the first AttrsReader user"
        );
    }

    // --- LFS clean path tests ---

    #[test]
    fn lfs_clean_produces_valid_lfs_pointer() {
        let Some(dir) = git_repo_tempdir() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());
        // No remote LFS store — content is cached locally in .git/lfs/objects/
        // and an LFS pointer is still produced.
        let content = b"hello lfs world";
        let pointer_bytes = session.clean_file("model.bin", content.to_vec()).unwrap();
        // Should produce a valid LFS pointer even without a remote store.
        let lfs_pointer = crab_git::lfs_pointer::LfsPointer::parse(&pointer_bytes).unwrap();
        assert_eq!(lfs_pointer.size, content.len() as u64);

        // Verify the content was cached locally.
        let oid_hex = crab_git::lfs_pointer::hex_encode(&lfs_pointer.oid);
        let worktree_ctx =
            crate::git::worktree::WorktreeContext::resolve_from_path(dir.path()).unwrap();
        let cached_path = worktree_ctx
            .lfs_objects_dir()
            .join(&oid_hex[..2])
            .join(&oid_hex[2..4])
            .join(&oid_hex);
        assert!(
            cached_path.is_file(),
            "LFS content should be cached locally"
        );
        assert_eq!(std::fs::read(&cached_path).unwrap(), content);
    }

    #[test]
    fn lfs_clean_without_lfs_pattern_uses_crab_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.safetensors filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());

        let content = b"crab content";
        let pointer_bytes = session
            .clean_file("weights.safetensors", content.to_vec())
            .unwrap();
        // Should produce a crab pointer (not LFS).
        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        let expected_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        assert_eq!(pointer.file_hash, expected_hash);
    }

    #[test]
    fn unmatched_path_uses_crab_path() {
        let dir = tempfile::tempdir().unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());

        let content = b"small unmatched candidate";
        let pointer_bytes = session.clean_file("model.bin", content.to_vec()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        let expected_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        assert_eq!(pointer.file_hash, expected_hash);
    }

    #[tokio::test]
    async fn lfs_clean_with_store_produces_lfs_pointer_and_stages_content() {
        use crab_git::lfs_pointer::LfsPointer;
        use crab_storage::{RetryPolicy, Store};
        use object_store::memory::InMemory;

        let Some(dir) = git_repo_tempdir() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        // Set up an in-memory LFS object store.
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        let store = Store::with_retry(inner, policy);
        let lfs_store = Arc::new(LfsObjectStore::new(store, "repo"));

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());
        session.set_lfs_store(Arc::clone(&lfs_store));

        let content = b"hello lfs world";
        let expected_oid: [u8; 32] = {
            let hash = sha2::Sha256::digest(content);
            let mut oid = [0u8; 32];
            oid.copy_from_slice(&hash);
            oid
        };

        // Run clean_file inside spawn_blocking since it uses block_on internally.
        let pointer_bytes = tokio::task::spawn_blocking(move || {
            session.clean_file("model.bin", content.to_vec()).unwrap()
        })
        .await
        .unwrap();

        // Verify the pointer is a valid LFS pointer.
        let pointer = LfsPointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.oid, expected_oid);
        assert_eq!(pointer.size, content.len() as u64);

        // Verify the content was staged in the LFS object store.
        let stored = lfs_store.get(&expected_oid).await.unwrap();
        assert_eq!(stored.as_ref(), content);
    }

    #[tokio::test]
    async fn lfs_clean_empty_file_produces_empty_pointer() {
        use crab_git::lfs_pointer::LfsPointer;
        use crab_storage::{RetryPolicy, Store};
        use object_store::memory::InMemory;

        let Some(dir) = git_repo_tempdir() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        let store = Store::with_retry(inner, policy);
        let lfs_store = Arc::new(LfsObjectStore::new(store, "repo"));

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());
        session.set_lfs_store(Arc::clone(&lfs_store));

        // Run clean_file inside spawn_blocking since it uses block_on internally.
        let pointer_bytes = tokio::task::spawn_blocking(move || {
            session.clean_file("empty.bin", Vec::new()).unwrap()
        })
        .await
        .unwrap();

        // Empty content produces a zero-size LFS pointer (serialized as empty bytes).
        let pointer = LfsPointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.size, 0);
    }

    // --- clean_stream tests ---

    /// Encode a single pkt-line data packet: `{len:04x}{body}`.
    ///
    /// Body must be at most 65 516 bytes (pkt-line max body size).
    fn encode_pktline(body: &[u8]) -> Vec<u8> {
        let len = body.len() + 4;
        assert!(len <= 0xffff, "pkt-line body too large for test helper");
        let mut out = format!("{len:04x}").into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// Encode a pkt-line data packet followed by a flush.
    fn encode_pktline_then_flush(body: &[u8]) -> Vec<u8> {
        let mut out = encode_pktline(body);
        out.extend_from_slice(b"0000");
        out
    }

    #[test]
    fn clean_stream_matches_clean_file_for_same_content() {
        use crate::git::filter_process::PktLineReader;

        let content = b"hello streaming world".to_vec();

        let ctx = AppContext::default();
        let mut session_a = CleanSession::new(ctx.clone());
        let via_file = session_a.clean_file("test.bin", content.clone()).unwrap();

        let framed = frame_as_pktlines(&content);
        let mut session_b = CleanSession::new(ctx);
        let mut reader = PktLineReader::from_slice(&framed);
        let via_stream = session_b.clean_stream("test.bin", &mut reader).unwrap();

        assert_eq!(via_file, via_stream);
    }

    #[test]
    fn clean_stream_pointer_passthrough() {
        use crate::git::filter_process::PktLineReader;

        // Build a real crab pointer payload; classify() must recognize it.
        let file_hash = [0xA5u8; 32];
        let pointer = Pointer {
            file_hash,
            size: 123,
            shard_hint: None,
        };
        let pointer_bytes = pointer.serialize();

        let framed = encode_pktline_then_flush(&pointer_bytes);

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        let mut reader = PktLineReader::from_slice(&framed);
        let out = session.clean_stream("test.bin", &mut reader).unwrap();

        assert_eq!(out, pointer_bytes);
    }

    #[test]
    fn clean_stream_recognizes_pointer_split_across_packets() {
        use crate::git::filter_process::PktLineReader;

        let pointer_bytes = Pointer {
            file_hash: [0xA5u8; 32],
            size: 123,
            shard_hint: None,
        }
        .serialize();
        let split = pointer_bytes.len() / 2;
        let mut framed = encode_pktline(&pointer_bytes[..split]);
        framed.extend_from_slice(&encode_pktline(&pointer_bytes[split..]));
        framed.extend_from_slice(b"0000");

        let mut session = CleanSession::new(AppContext::default());
        let mut reader = PktLineReader::from_slice(&framed);
        let out = session.clean_stream("test.bin", &mut reader).unwrap();

        assert_eq!(out, pointer_bytes);
    }

    #[test]
    fn clean_stream_pointer_prefix_does_not_bypass_cleaning() {
        use crate::git::filter_process::PktLineReader;

        let pointer_bytes = Pointer {
            file_hash: [0xA5u8; 32],
            size: 123,
            shard_hint: None,
        }
        .serialize();
        let tail = vec![0x5a; 2 * 1024];
        let mut content = pointer_bytes.clone();
        content.extend_from_slice(&tail);

        let mut expected_session = CleanSession::new(AppContext::default());
        let expected = expected_session.clean_file("test.bin", content).unwrap();

        let mut framed = encode_pktline(&pointer_bytes);
        framed.extend_from_slice(&encode_pktline(&tail));
        framed.extend_from_slice(b"0000");
        let mut session = CleanSession::new(AppContext::default());
        let mut reader = PktLineReader::from_slice(&framed);
        let out = session.clean_stream("test.bin", &mut reader).unwrap();

        assert_eq!(out, expected);
    }

    #[test]
    fn clean_stream_empty_content() {
        use crate::git::filter_process::PktLineReader;

        let flush_only = b"0000".to_vec();

        let ctx = AppContext::default();
        let mut session_a = CleanSession::new(ctx.clone());
        let via_file = session_a.clean_file("empty.bin", Vec::new()).unwrap();

        let mut session_b = CleanSession::new(ctx);
        let mut reader = PktLineReader::from_slice(&flush_only);
        let via_stream = session_b.clean_stream("empty.bin", &mut reader).unwrap();

        let pointer = Pointer::parse(&via_stream).unwrap();
        assert_eq!(pointer.size, 0);
        assert_eq!(via_file, via_stream);
    }

    #[test]
    fn clean_stream_spills_buffered_chunks_to_provisional_staging() {
        use crate::git::filter_process::PktLineReader;

        let content = b"streaming spill adoption".repeat(1024);
        let expected_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let stats = Arc::new(RecordingChunkStagerStats::default());
        let stager: Box<dyn ChunkStager> = Box::new(RecordingChunkStager {
            stats: Arc::clone(&stats),
        });
        let mut session = CleanSession::with_deps(
            AppContext::default(),
            Box::new(NoopFileIndexChecker),
            stager,
            &[],
            u64::MAX,
            1,
        );
        let framed = frame_as_pktlines(&content);
        let mut reader = PktLineReader::from_slice(&framed);

        let pointer_bytes = session.clean_stream("large.bin", &mut reader).unwrap();

        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.file_hash, expected_hash);
        assert_eq!(pointer.size, content.len() as u64);

        let adopted = stats
            .adopted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0].1, expected_hash);
        assert_eq!(adopted[0].2, content.len() as u64);
        assert_ne!(adopted[0].0, expected_hash);

        let staged_hashes = stats
            .staged_hashes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!staged_hashes.is_empty());
        assert!(staged_hashes.iter().all(|hash| *hash == adopted[0].0));
        assert_eq!(stats.flushes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn clean_stream_discards_provisional_rows_when_fast_path_wins() {
        use crate::git::filter_process::PktLineReader;

        let content = b"streaming fast path discard".repeat(1024);
        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let stats = Arc::new(RecordingChunkStagerStats::default());
        let stager: Box<dyn ChunkStager> = Box::new(RecordingChunkStager {
            stats: Arc::clone(&stats),
        });
        let checker = MockFileIndexChecker {
            known_hashes: HashSet::from([file_hash]),
            shard_hint: None,
        };
        let mut session = CleanSession::with_deps(
            AppContext::default(),
            Box::new(checker),
            stager,
            &[file_hash],
            0,
            1,
        );
        let framed = encode_pktline_then_flush(&content);
        let mut reader = PktLineReader::from_slice(&framed);

        let pointer_bytes = session.clean_stream("known.bin", &mut reader).unwrap();

        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.file_hash, file_hash);

        let adopted = stats
            .adopted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(adopted.is_empty());

        let discarded = stats
            .discarded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(discarded.len(), 1);

        let staged_hashes = stats
            .staged_hashes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!staged_hashes.is_empty());
        assert!(staged_hashes.iter().all(|hash| *hash == discarded[0]));
        assert_eq!(stats.flushes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn clean_stream_fails_fast_path_when_provisional_discard_fails() {
        use crate::git::filter_process::PktLineReader;

        let content = b"streaming discard failure".repeat(1024);
        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let stats = Arc::new(RecordingChunkStagerStats::default());
        let stager: Box<dyn ChunkStager> = Box::new(FailDiscardRecordingChunkStager {
            stats: Arc::clone(&stats),
        });
        let checker = MockFileIndexChecker {
            known_hashes: HashSet::from([file_hash]),
            shard_hint: None,
        };
        let mut session = CleanSession::with_deps(
            AppContext::default(),
            Box::new(checker),
            stager,
            &[file_hash],
            0,
            1,
        );
        let framed = frame_as_pktlines(&content);
        let mut reader = PktLineReader::from_slice(&framed);

        let err = session
            .clean_stream("known-discard-fails.bin", &mut reader)
            .expect_err("fast path must not publish a pointer when provisional discard fails");
        assert!(
            err.to_string()
                .contains("injected provisional discard failure"),
            "unexpected error: {err}"
        );

        let staged_hashes = stats
            .staged_hashes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !staged_hashes.is_empty(),
            "test should stage provisional rows before the fast path wins"
        );

        let discarded = stats
            .discarded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(discarded.len(), 1);
        assert!(staged_hashes.iter().all(|hash| *hash == discarded[0]));
        assert!(
            stats
                .adopted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn clean_stream_discards_provisional_rows_on_stage_error() {
        use crate::git::filter_process::PktLineReader;

        let mut content = Vec::with_capacity(9 * 1024 * 1024);
        for i in 0..content.capacity() {
            content.push((u64::try_from(i).unwrap().wrapping_mul(2_654_435_761) >> 24) as u8);
        }

        let stats = Arc::new(RecordingChunkStagerStats::default());
        let stager: Box<dyn ChunkStager> = Box::new(FailSecondRecordingChunkStager {
            stats: Arc::clone(&stats),
            calls: AtomicUsize::new(0),
        });
        let mut session = CleanSession::with_deps(
            AppContext::default(),
            Box::new(NoopFileIndexChecker),
            stager,
            &[],
            u64::MAX,
            1,
        );
        let framed = frame_as_pktlines(&content);
        let mut reader = PktLineReader::from_slice(&framed);

        let err = session
            .clean_stream("large-fails.bin", &mut reader)
            .expect_err("second provisional staging batch should fail");
        assert!(
            err.to_string()
                .contains("injected provisional staging failure"),
            "unexpected error: {err}"
        );

        let staged_hashes = stats
            .staged_hashes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !staged_hashes.is_empty(),
            "test should stage at least one provisional batch before failing"
        );
        let discarded = stats
            .discarded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            discarded.len(),
            1,
            "failed streaming clean should discard its provisional rows"
        );
        assert!(staged_hashes.iter().all(|hash| *hash == discarded[0]));
        assert!(
            stats
                .adopted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn clean_fast_path_discards_real_provisional_staging_bytes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let staging = Arc::new(
            rt.block_on(crab_staging::StagingArea::open(tmp.path().join("staging")))
                .expect("open staging"),
        );

        let mut state = 0xC0FF_EE42_D15C_A11Du64;
        let mut content = Vec::with_capacity(9 * 1024 * 1024);
        for _ in 0..content.capacity() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            content.push((state >> 33) as u8);
        }

        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let checker = MockFileIndexChecker {
            known_hashes: HashSet::from([file_hash]),
            shard_hint: None,
        };
        let stager: Box<dyn ChunkStager> = Box::new(StagingChunkStager::new(
            Arc::clone(&staging),
            rt.handle().clone(),
        ));
        let mut session = CleanSession::with_deps(
            AppContext::default(),
            Box::new(checker),
            stager,
            &[file_hash],
            0,
            1,
        );

        let pointer_bytes = session.clean_file("known-large.bin", content).unwrap();

        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.file_hash, file_hash);

        let stats = staging.stats().expect("staging stats");
        assert_eq!(stats.file_count, 0);
        assert_eq!(stats.current_segment_bytes, 0);
        assert_eq!(stats.total_staged_bytes, 0);
        assert_eq!(stats.live_bytes, 0);
        assert_eq!(stats.chunk_count, 0);
    }

    #[test]
    fn clean_slow_path_persists_recipe_and_reuses_identical_payloads() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let staging = Arc::new(
            rt.block_on(crab_staging::StagingArea::open(tmp.path().join("staging")))
                .expect("open staging"),
        );
        let content = b"shared clean recipe payload".repeat(64 * 1024);
        let expected_hash = *blake3::hash(&content).as_bytes();
        let stager: Box<dyn ChunkStager> = Box::new(StagingChunkStager::new(
            Arc::clone(&staging),
            rt.handle().clone(),
        ));
        let mut session = CleanSession::with_deps(
            AppContext::default(),
            Box::new(NoopFileIndexChecker),
            stager,
            &[],
            u64::MAX,
            DEFAULT_CHUNK_BUFFER_CAP,
        );

        let first = session.clean_file("first.bin", content.clone()).unwrap();
        let after_first = staging.stats().unwrap();
        let second = session.clean_file("second.bin", content).unwrap();
        let after_second = staging.stats().unwrap();

        assert_eq!(Pointer::parse(&first).unwrap().file_hash, expected_hash);
        assert_eq!(first, second);
        assert_eq!(after_first.file_count, 1);
        assert_eq!(after_second.file_count, 1);
        assert_eq!(after_second.chunk_count, after_first.chunk_count);
        assert_eq!(
            after_second.current_segment_bytes, after_first.current_segment_bytes,
            "an exact recipe hit must not rewrite identical chunk payloads"
        );
    }
}
