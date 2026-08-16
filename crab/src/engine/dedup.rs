//! Dedup planner: A/B/C chunk classifier and per-file/push statistics.
//!
//! Three chunk classes:
//! - **A** (`Existing`): already in remote storage (three-tier lookup hit).
//! - **B** (`Staged`): already in local staging (session-seen set).
//! - **C** (`New`): needs to be staged and uploaded.
//!
//! Three-tier dedup lookup chain (first hit wins):
//! 1. In-memory `ChunkIndex` HashMap — O(1) lookup
//! 2. `PersistentChunkIndex` (SQLite) — indexed lookup
//! 3. On-disk `MDBShardFile` handles — O(1)-amortized via interpolation search
//!
//! Lookup order is remote-first, then session set — this is a
//! non-negotiable invariant ensuring remote-first precedence.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{trace, warn};

use crab_metadata::chunk_index::ChunkIndex;
use crab_metadata::persistent_chunk_index::PersistentChunkIndex;
use crab_xet::shard::MDBShardFile;
use crab_xet::xorb::format::{MerkleHash, XorbRef};

/// Chunk classification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkClass {
    /// Already in remote storage (three-tier lookup hit).
    Existing(XorbRef),
    /// Already seen in this session (staging hit).
    Staged,
    /// New chunk — needs to be staged and uploaded.
    New,
}

/// Bundles the three dedup lookup tiers for classification.
///
/// Tier 1: in-memory `ChunkIndex` (O(1) HashMap lookup).
/// Tier 2: `PersistentChunkIndex` (indexed SQLite lookup).
/// Tier 3: on-disk `MDBShardFile` handles (O(1)-amortized interpolation search).
///
/// First hit wins — lower tiers are not checked once a match is found.
pub struct DedupContext<'a> {
    /// Tier 1: in-memory chunk index.
    pub chunk_index: &'a ChunkIndex,
    /// Tier 2: persistent on-disk index (optional).
    pub persistent_index: Option<&'a PersistentChunkIndex>,
    /// Tier 3: on-disk shard file handles (optional).
    pub shard_files: &'a [Arc<MDBShardFile>],
}

/// Classifies chunks for dedup decisions.
///
/// Maintains a session-scoped set of chunk hashes that have been seen
/// during this push. Classification checks the [`ChunkIndex`] first
/// (repo-first precedence), then the session set.
pub struct Classifier {
    session_seen: HashSet<MerkleHash>,
}

impl Default for Classifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Classifier {
    /// Create a new classifier with an empty session set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            session_seen: HashSet::new(),
        }
    }

    /// Classify a chunk using only the in-memory ChunkIndex (tier 1).
    ///
    /// This is the legacy single-tier path. Prefer `classify_with_context`
    /// for the full three-tier lookup chain.
    #[must_use]
    pub fn classify(&self, hash: &MerkleHash, chunk_index: &ChunkIndex) -> ChunkClass {
        // Class A: already in remote storage.
        if let Some(xorb_ref) = chunk_index.get(hash) {
            trace!(chunk = %hash, "class A: existing in ChunkIndex");
            return ChunkClass::Existing(*xorb_ref);
        }

        // Class B: already seen in this session.
        if self.session_seen.contains(hash) {
            trace!(chunk = %hash, "class B: staged (session seen)");
            return ChunkClass::Staged;
        }

        // Class C: new chunk.
        trace!(chunk = %hash, "class C: new");
        ChunkClass::New
    }

    /// Classify a chunk using the three-tier dedup lookup chain.
    ///
    /// Checks tiers in order (first hit wins):
    /// 1. In-memory `ChunkIndex` — O(1) HashMap lookup
    /// 2. `PersistentChunkIndex` — indexed SQLite lookup
    /// 3. On-disk `MDBShardFile` handles — O(1)-amortized interpolation search
    ///
    /// Falls back to session-seen check (class B) and finally class C (new).
    #[must_use]
    pub fn classify_with_context(&self, hash: &MerkleHash, ctx: &DedupContext<'_>) -> ChunkClass {
        // Tier 1: in-memory ChunkIndex.
        if let Some(xorb_ref) = ctx.chunk_index.get(hash) {
            trace!(chunk = %hash, tier = 1, "class A: existing in ChunkIndex");
            return ChunkClass::Existing(*xorb_ref);
        }

        // Tier 2: persistent on-disk index.
        if let Some(pi) = ctx.persistent_index {
            match pi.get(hash) {
                Ok(Some(xorb_ref)) => {
                    trace!(chunk = %hash, tier = 2, "class A: existing in PersistentChunkIndex");
                    return ChunkClass::Existing(xorb_ref);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(chunk = %hash, error = %e, "persistent index lookup failed, continuing to tier 3");
                }
            }
        }

        // Tier 3: on-disk MDBShardFile handles via interpolation search.
        if !ctx.shard_files.is_empty() {
            let query = &[*hash];
            for shard_file in ctx.shard_files {
                match shard_file.chunk_hash_dedup_query(query) {
                    Ok(Some((_count, entry))) => {
                        let xorb_ref = XorbRef {
                            xorb_hash: entry.xorb_hash,
                            chunk_index: entry.chunk_index_start,
                            uncompressed_size: entry.unpacked_segment_bytes,
                        };
                        trace!(chunk = %hash, tier = 3, "class A: existing in on-disk MDBShardFile");
                        return ChunkClass::Existing(xorb_ref);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(
                            chunk = %hash,
                            shard = %shard_file.path.display(),
                            error = %e,
                            "on-disk shard query failed, trying next shard"
                        );
                    }
                }
            }
        }

        // Class B: already seen in this session.
        if self.session_seen.contains(hash) {
            trace!(chunk = %hash, "class B: staged (session seen)");
            return ChunkClass::Staged;
        }

        // Class C: new chunk.
        trace!(chunk = %hash, "class C: new");
        ChunkClass::New
    }

    /// Mark a chunk as seen in this session (for session dedup).
    ///
    /// After marking, subsequent [`classify`](Self::classify) calls for
    /// the same hash will return [`ChunkClass::Staged`] (unless the
    /// `ChunkIndex` also contains it, in which case `Existing` wins).
    pub fn mark_seen(&mut self, hash: MerkleHash) {
        self.session_seen.insert(hash);
    }

    /// Number of unique chunks seen in this session.
    #[must_use]
    pub fn session_seen_count(&self) -> usize {
        self.session_seen.len()
    }
}

/// Perform a three-tier lookup for a single chunk hash without
/// classification (no session-seen check).
///
/// Returns the `XorbRef` if found in any tier, `None` otherwise.
/// Used by the shard builder (step 8) to resolve existing chunk
/// placements for reconstruction terms.
pub fn lookup_three_tier(hash: &MerkleHash, ctx: &DedupContext<'_>) -> Option<XorbRef> {
    // Tier 1: in-memory ChunkIndex.
    if let Some(xorb_ref) = ctx.chunk_index.get(hash) {
        return Some(*xorb_ref);
    }

    // Tier 2: persistent on-disk index.
    if let Some(pi) = ctx.persistent_index {
        match pi.get(hash) {
            Ok(Some(xorb_ref)) => return Some(xorb_ref),
            Ok(None) => {}
            Err(e) => {
                warn!(chunk = %hash, error = %e, "persistent index lookup failed in placement merge");
            }
        }
    }

    // Tier 3: on-disk MDBShardFile handles.
    if !ctx.shard_files.is_empty() {
        let query = &[*hash];
        for shard_file in ctx.shard_files {
            match shard_file.chunk_hash_dedup_query(query) {
                Ok(Some((_count, entry))) => {
                    return Some(XorbRef {
                        xorb_hash: entry.xorb_hash,
                        chunk_index: entry.chunk_index_start,
                        uncompressed_size: entry.unpacked_segment_bytes,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        chunk = %hash,
                        shard = %shard_file.path.display(),
                        error = %e,
                        "on-disk shard query failed in placement merge"
                    );
                }
            }
        }
    }

    None
}

/// Per-file dedup plan summarizing classification results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlan {
    /// Content hash of the file.
    pub file_hash: MerkleHash,
    /// Total number of chunks in the file.
    pub total_chunks: u64,
    /// Chunks classified as Existing (class A).
    pub existing_chunks: u64,
    /// Chunks classified as Staged (class B).
    pub staged_chunks: u64,
    /// Chunks classified as New (class C).
    pub new_chunks: u64,
    /// Total uncompressed bytes across all chunks.
    pub total_bytes: u64,
}

impl FilePlan {
    /// Create a new empty plan for a file.
    #[must_use]
    pub fn new(file_hash: MerkleHash) -> Self {
        Self {
            file_hash,
            total_chunks: 0,
            existing_chunks: 0,
            staged_chunks: 0,
            new_chunks: 0,
            total_bytes: 0,
        }
    }

    /// Record a classified chunk into this plan.
    pub fn record(&mut self, class: &ChunkClass, chunk_bytes: u64) {
        self.total_chunks += 1;
        self.total_bytes += chunk_bytes;
        match class {
            ChunkClass::Existing(_) => self.existing_chunks += 1,
            ChunkClass::Staged => self.staged_chunks += 1,
            ChunkClass::New => self.new_chunks += 1,
        }
    }

    /// Dedup ratio for this file: fraction of chunks that are not new.
    #[must_use]
    pub fn dedup_ratio(&self) -> f64 {
        if self.total_chunks == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "chunk counts are well under 2^53"
        )]
        let ratio = (self.existing_chunks + self.staged_chunks) as f64 / self.total_chunks as f64;
        ratio
    }
}

/// Aggregate statistics across all files in a push.
#[derive(Debug, Clone, PartialEq)]
pub struct FileStats {
    /// Total number of files processed.
    pub total_files: u64,
    /// Total number of chunks across all files.
    pub total_chunks: u64,
    /// Chunks classified as Existing (class A).
    pub existing_chunks: u64,
    /// Chunks classified as New (class C).
    pub new_chunks: u64,
    /// Overall dedup ratio (fraction of chunks that are not new).
    pub dedup_ratio: f64,
}

impl Default for FileStats {
    fn default() -> Self {
        Self::new()
    }
}

impl FileStats {
    /// Create empty aggregate stats.
    #[must_use]
    pub fn new() -> Self {
        Self {
            total_files: 0,
            total_chunks: 0,
            existing_chunks: 0,
            new_chunks: 0,
            dedup_ratio: 0.0,
        }
    }

    /// Fold a [`FilePlan`] into the aggregate.
    pub fn add(&mut self, plan: &FilePlan) {
        self.total_files += 1;
        self.total_chunks += plan.total_chunks;
        self.existing_chunks += plan.existing_chunks;
        self.new_chunks += plan.new_chunks;
        self.recompute_ratio();
    }

    /// Recompute the dedup ratio from current totals.
    fn recompute_ratio(&mut self) {
        if self.total_chunks == 0 {
            self.dedup_ratio = 0.0;
        } else {
            #[expect(
                clippy::cast_precision_loss,
                reason = "chunk counts are well under 2^53"
            )]
            {
                self.dedup_ratio =
                    (self.total_chunks - self.new_chunks) as f64 / self.total_chunks as f64;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed, seed, seed])
    }

    fn xorb_ref(xorb_seed: u64, idx: u32) -> XorbRef {
        XorbRef {
            xorb_hash: hash(xorb_seed),
            chunk_index: idx,
            uncompressed_size: 0,
        }
    }

    // --- Classifier tests ---

    #[test]
    fn new_classifier_has_empty_session() {
        let c = Classifier::new();
        assert_eq!(c.session_seen_count(), 0);
    }

    #[test]
    fn classify_new_chunk_returns_new() {
        let c = Classifier::new();
        let idx = ChunkIndex::new();
        let h = hash(1);
        assert_eq!(c.classify(&h, &idx), ChunkClass::New);
    }

    #[test]
    fn classify_existing_chunk_returns_existing() {
        let c = Classifier::new();
        let mut idx = ChunkIndex::new();
        let h = hash(1);
        let xr = xorb_ref(100, 0);
        idx.insert(h, xr);

        assert_eq!(c.classify(&h, &idx), ChunkClass::Existing(xr));
    }

    #[test]
    fn classify_staged_chunk_returns_staged() {
        let mut c = Classifier::new();
        let idx = ChunkIndex::new();
        let h = hash(1);

        c.mark_seen(h);
        assert_eq!(c.classify(&h, &idx), ChunkClass::Staged);
    }

    #[test]
    fn repo_first_precedence_existing_over_staged() {
        let mut c = Classifier::new();
        let mut idx = ChunkIndex::new();
        let h = hash(1);
        let xr = xorb_ref(100, 0);

        // Mark as both staged and existing in ChunkIndex.
        c.mark_seen(h);
        idx.insert(h, xr);

        // ChunkIndex hit (class A) must win over session seen (class B).
        assert_eq!(c.classify(&h, &idx), ChunkClass::Existing(xr));
    }

    #[test]
    fn session_dedup_prevents_duplicate_new() {
        let mut c = Classifier::new();
        let idx = ChunkIndex::new();
        let h = hash(42);

        // First time: New.
        assert_eq!(c.classify(&h, &idx), ChunkClass::New);

        // Mark as seen.
        c.mark_seen(h);

        // Second time: Staged.
        assert_eq!(c.classify(&h, &idx), ChunkClass::Staged);
    }

    #[test]
    fn session_seen_count_tracks_unique_hashes() {
        let mut c = Classifier::new();
        c.mark_seen(hash(1));
        c.mark_seen(hash(2));
        c.mark_seen(hash(1)); // duplicate
        assert_eq!(c.session_seen_count(), 2);
    }

    // --- FilePlan tests ---

    #[test]
    fn file_plan_records_classifications() {
        let mut plan = FilePlan::new(hash(10));
        plan.record(&ChunkClass::Existing(xorb_ref(100, 0)), 1024);
        plan.record(&ChunkClass::Staged, 2048);
        plan.record(&ChunkClass::New, 4096);

        assert_eq!(plan.total_chunks, 3);
        assert_eq!(plan.existing_chunks, 1);
        assert_eq!(plan.staged_chunks, 1);
        assert_eq!(plan.new_chunks, 1);
        assert_eq!(plan.total_bytes, 7168);
    }

    #[test]
    fn file_plan_dedup_ratio() {
        let mut plan = FilePlan::new(hash(10));
        plan.record(&ChunkClass::Existing(xorb_ref(100, 0)), 100);
        plan.record(&ChunkClass::Existing(xorb_ref(100, 1)), 100);
        plan.record(&ChunkClass::New, 100);

        let ratio = plan.dedup_ratio();
        assert!((ratio - 2.0 / 3.0).abs() < 1e-10);
    }

    #[test]
    fn file_plan_empty_dedup_ratio_is_zero() {
        let plan = FilePlan::new(hash(10));
        assert_eq!(plan.dedup_ratio(), 0.0);
    }

    // --- FileStats tests ---

    #[test]
    fn file_stats_aggregates_plans() {
        let mut stats = FileStats::new();

        let mut plan1 = FilePlan::new(hash(1));
        plan1.record(&ChunkClass::Existing(xorb_ref(100, 0)), 100);
        plan1.record(&ChunkClass::New, 100);

        let mut plan2 = FilePlan::new(hash(2));
        plan2.record(&ChunkClass::Existing(xorb_ref(200, 0)), 200);
        plan2.record(&ChunkClass::Existing(xorb_ref(200, 1)), 200);
        plan2.record(&ChunkClass::New, 200);

        stats.add(&plan1);
        stats.add(&plan2);

        assert_eq!(stats.total_files, 2);
        assert_eq!(stats.total_chunks, 5);
        assert_eq!(stats.existing_chunks, 3);
        assert_eq!(stats.new_chunks, 2);
        assert!((stats.dedup_ratio - 0.6).abs() < 1e-10);
    }

    #[test]
    fn file_stats_empty_ratio_is_zero() {
        let stats = FileStats::new();
        assert_eq!(stats.dedup_ratio, 0.0);
    }

    #[test]
    fn file_stats_all_existing_ratio_is_one() {
        let mut stats = FileStats::new();
        let mut plan = FilePlan::new(hash(1));
        plan.record(&ChunkClass::Existing(xorb_ref(100, 0)), 100);
        plan.record(&ChunkClass::Existing(xorb_ref(100, 1)), 100);
        stats.add(&plan);

        assert!((stats.dedup_ratio - 1.0).abs() < 1e-10);
    }

    // --- Three-tier classify_with_context tests ---

    #[test]
    fn classify_with_context_tier1_hit() {
        let c = Classifier::new();
        let mut idx = ChunkIndex::new();
        let h = hash(1);
        let xr = xorb_ref(100, 0);
        idx.insert(h, xr);

        let ctx = DedupContext {
            chunk_index: &idx,
            persistent_index: None,
            shard_files: &[],
        };
        assert_eq!(c.classify_with_context(&h, &ctx), ChunkClass::Existing(xr));
    }

    #[test]
    fn classify_with_context_tier2_hit() {
        let c = Classifier::new();
        let idx = ChunkIndex::new();

        let dir = tempfile::TempDir::new().unwrap();
        let pi = PersistentChunkIndex::open_or_create(&dir.path().join("test.sqlite")).unwrap();
        let h = hash(1);
        let xr = xorb_ref(100, 0);
        pi.install_shard(hash(999), &[(h, xr)]).unwrap();

        let ctx = DedupContext {
            chunk_index: &idx,
            persistent_index: Some(&pi),
            shard_files: &[],
        };
        assert_eq!(c.classify_with_context(&h, &ctx), ChunkClass::Existing(xr));
    }

    #[test]
    fn classify_with_context_tier1_wins_over_tier2() {
        let c = Classifier::new();
        let mut idx = ChunkIndex::new();
        let h = hash(1);
        let xr_tier1 = xorb_ref(100, 0);
        let xr_tier2 = xorb_ref(200, 0);
        idx.insert(h, xr_tier1);

        let dir = tempfile::TempDir::new().unwrap();
        let pi = PersistentChunkIndex::open_or_create(&dir.path().join("test.sqlite")).unwrap();
        pi.install_shard(hash(999), &[(h, xr_tier2)]).unwrap();

        let ctx = DedupContext {
            chunk_index: &idx,
            persistent_index: Some(&pi),
            shard_files: &[],
        };
        // Tier 1 should win.
        assert_eq!(
            c.classify_with_context(&h, &ctx),
            ChunkClass::Existing(xr_tier1)
        );
    }

    #[test]
    fn classify_with_context_falls_through_to_new() {
        let c = Classifier::new();
        let idx = ChunkIndex::new();

        let ctx = DedupContext {
            chunk_index: &idx,
            persistent_index: None,
            shard_files: &[],
        };
        assert_eq!(c.classify_with_context(&hash(42), &ctx), ChunkClass::New);
    }

    #[test]
    fn classify_with_context_session_seen_returns_staged() {
        let mut c = Classifier::new();
        let idx = ChunkIndex::new();
        let h = hash(1);
        c.mark_seen(h);

        let ctx = DedupContext {
            chunk_index: &idx,
            persistent_index: None,
            shard_files: &[],
        };
        assert_eq!(c.classify_with_context(&h, &ctx), ChunkClass::Staged);
    }

    #[test]
    fn classify_with_context_remote_wins_over_session() {
        let mut c = Classifier::new();
        let mut idx = ChunkIndex::new();
        let h = hash(1);
        let xr = xorb_ref(100, 0);
        c.mark_seen(h);
        idx.insert(h, xr);

        let ctx = DedupContext {
            chunk_index: &idx,
            persistent_index: None,
            shard_files: &[],
        };
        assert_eq!(c.classify_with_context(&h, &ctx), ChunkClass::Existing(xr));
    }

    // --- lookup_three_tier tests ---

    #[test]
    fn lookup_three_tier_tier1_hit() {
        let mut idx = ChunkIndex::new();
        let h = hash(1);
        let xr = xorb_ref(100, 0);
        idx.insert(h, xr);

        let ctx = DedupContext {
            chunk_index: &idx,
            persistent_index: None,
            shard_files: &[],
        };
        assert_eq!(lookup_three_tier(&h, &ctx), Some(xr));
    }

    #[test]
    fn lookup_three_tier_tier2_hit() {
        let idx = ChunkIndex::new();

        let dir = tempfile::TempDir::new().unwrap();
        let pi = PersistentChunkIndex::open_or_create(&dir.path().join("test.sqlite")).unwrap();
        let h = hash(1);
        let xr = xorb_ref(100, 0);
        pi.install_shard(hash(999), &[(h, xr)]).unwrap();

        let ctx = DedupContext {
            chunk_index: &idx,
            persistent_index: Some(&pi),
            shard_files: &[],
        };
        assert_eq!(lookup_three_tier(&h, &ctx), Some(xr));
    }

    #[test]
    fn lookup_three_tier_miss_returns_none() {
        let idx = ChunkIndex::new();

        let ctx = DedupContext {
            chunk_index: &idx,
            persistent_index: None,
            shard_files: &[],
        };
        assert_eq!(lookup_three_tier(&hash(42), &ctx), None);
    }
}
