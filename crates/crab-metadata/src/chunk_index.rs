//! In-memory chunk index mapping chunk hashes to their xorb location.
//!
//! The `ChunkIndex` is the primary dedup lookup structure: given a chunk
//! hash, it returns the `XorbRef` (xorb hash + chunk offset) if the chunk
//! is already stored remotely. Populated from cached shards and the
//! persistent on-disk index on startup, then incrementally refreshed after
//! each successful push via `install_shard`.
//!
//! A memory ceiling (default 1 GiB) controls when the index should be
//! considered "over budget" — callers can check `over_ceiling()` to decide
//! whether to spill to the persistent disk index instead.

use std::collections::{HashMap, HashSet};

use crab_xet::xorb::format::{MerkleHash, XorbRef};
use tracing::debug;

/// Default memory ceiling: 1 GiB (~40 bytes/entry ~= ~26 M entries).
const DEFAULT_MEMORY_CEILING: u64 = 1024 * 1024 * 1024;

/// Approximate bytes per entry in the `HashMap` (key + value + overhead).
const BYTES_PER_ENTRY: u64 = 40;

/// In-memory chunk index mapping chunk hashes to their xorb location.
pub struct ChunkIndex {
    entries: HashMap<MerkleHash, XorbRef>,
    /// Set of shard hashes that have been installed into this index.
    installed_shards: HashSet<MerkleHash>,
    memory_ceiling: u64,
}

impl ChunkIndex {
    /// Create a new empty index with the default 1 GiB memory ceiling.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            installed_shards: HashSet::new(),
            memory_ceiling: DEFAULT_MEMORY_CEILING,
        }
    }

    /// Create a new empty index with a custom memory ceiling in bytes.
    #[must_use]
    pub fn with_ceiling(ceiling: u64) -> Self {
        Self {
            entries: HashMap::new(),
            installed_shards: HashSet::new(),
            memory_ceiling: ceiling,
        }
    }

    /// Look up a chunk hash. Returns the xorb reference if known.
    #[must_use]
    pub fn get(&self, hash: &MerkleHash) -> Option<&XorbRef> {
        self.entries.get(hash)
    }

    /// Insert a single chunk→xorb mapping.
    pub fn insert(&mut self, chunk_hash: MerkleHash, xorb_ref: XorbRef) {
        self.entries.insert(chunk_hash, xorb_ref);
    }

    /// Remove a stale acceleration entry. Missing hashes are a no-op.
    pub fn remove(&mut self, chunk_hash: &MerkleHash) {
        self.entries.remove(chunk_hash);
    }

    /// Install all chunks from a shard.
    ///
    /// Each entry maps a chunk hash to its xorb reference. The shard hash
    /// is recorded so duplicate installs are skipped.
    pub fn install_shard(&mut self, shard_hash: MerkleHash, entries: &[(MerkleHash, XorbRef)]) {
        if self.installed_shards.contains(&shard_hash) {
            debug!(shard = %shard_hash, "shard already installed, skipping");
            return;
        }

        for (chunk_hash, xorb_ref) in entries {
            self.entries.insert(*chunk_hash, *xorb_ref);
        }
        self.installed_shards.insert(shard_hash);

        debug!(
            shard = %shard_hash,
            new_entries = entries.len(),
            total = self.entries.len(),
            "installed shard into chunk index"
        );
    }

    /// Whether the index has exceeded its memory ceiling.
    #[must_use]
    pub fn over_ceiling(&self) -> bool {
        self.estimated_bytes() > self.memory_ceiling
    }

    /// Number of chunk entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index contains no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Estimated memory usage in bytes (~40 bytes per entry).
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        self.entries.len() as u64 * BYTES_PER_ENTRY
    }

    /// Number of installed shards.
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.installed_shards.len()
    }

    /// Whether a specific shard has already been installed.
    #[must_use]
    pub fn has_shard(&self, shard_hash: &MerkleHash) -> bool {
        self.installed_shards.contains(shard_hash)
    }

    /// Verify that all chunks from a shard list are present.
    ///
    /// Returns the list of shard hashes that are NOT installed in this index.
    /// An empty return means all shards are accounted for.
    #[must_use]
    pub fn verify_against_shard_list(&self, shard_hashes: &[MerkleHash]) -> Vec<MerkleHash> {
        shard_hashes
            .iter()
            .filter(|h| !self.installed_shards.contains(h))
            .copied()
            .collect()
    }
}

impl Default for ChunkIndex {
    fn default() -> Self {
        Self::new()
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

    #[test]
    fn new_index_is_empty() {
        let idx = ChunkIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert_eq!(idx.estimated_bytes(), 0);
        assert!(!idx.over_ceiling());
    }

    #[test]
    fn insert_and_get() {
        let mut idx = ChunkIndex::new();
        let ch = hash(1);
        let xr = xorb_ref(100, 0);
        idx.insert(ch, xr);

        assert_eq!(idx.get(&ch), Some(&xr));
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
    }

    #[test]
    fn get_missing_returns_none() {
        let idx = ChunkIndex::new();
        assert_eq!(idx.get(&hash(42)), None);
    }

    #[test]
    fn install_shard_adds_entries() {
        let mut idx = ChunkIndex::new();
        let shard = hash(999);
        let entries = vec![
            (hash(1), xorb_ref(100, 0)),
            (hash(2), xorb_ref(100, 1)),
            (hash(3), xorb_ref(101, 0)),
        ];

        idx.install_shard(shard, &entries);

        assert_eq!(idx.len(), 3);
        assert_eq!(idx.shard_count(), 1);
        assert!(idx.has_shard(&shard));
        assert_eq!(idx.get(&hash(1)), Some(&xorb_ref(100, 0)));
        assert_eq!(idx.get(&hash(2)), Some(&xorb_ref(100, 1)));
        assert_eq!(idx.get(&hash(3)), Some(&xorb_ref(101, 0)));
    }

    #[test]
    fn install_shard_skips_duplicate() {
        let mut idx = ChunkIndex::new();
        let shard = hash(999);
        let entries = vec![(hash(1), xorb_ref(100, 0))];

        idx.install_shard(shard, &entries);
        assert_eq!(idx.len(), 1);

        // Installing the same shard again should be a no-op.
        let entries2 = vec![(hash(2), xorb_ref(200, 0))];
        idx.install_shard(shard, &entries2);
        assert_eq!(idx.len(), 1); // Still 1, not 2.
        assert!(idx.get(&hash(2)).is_none());
    }

    #[test]
    fn over_ceiling_triggers_at_threshold() {
        // With a ceiling of 80 bytes, 3 entries (3*40=120) should exceed it.
        let mut idx = ChunkIndex::with_ceiling(80);
        idx.insert(hash(1), xorb_ref(100, 0));
        idx.insert(hash(2), xorb_ref(100, 1));
        assert!(!idx.over_ceiling()); // 80 bytes = ceiling, not over

        idx.insert(hash(3), xorb_ref(100, 2));
        assert!(idx.over_ceiling()); // 120 > 80
    }

    #[test]
    fn verify_against_shard_list_reports_missing() {
        let mut idx = ChunkIndex::new();
        let s1 = hash(1);
        let s2 = hash(2);
        let s3 = hash(3);

        idx.install_shard(s1, &[(hash(10), xorb_ref(100, 0))]);
        idx.install_shard(s3, &[(hash(30), xorb_ref(300, 0))]);

        let missing = idx.verify_against_shard_list(&[s1, s2, s3]);
        assert_eq!(missing, vec![s2]);
    }

    #[test]
    fn verify_against_shard_list_all_present() {
        let mut idx = ChunkIndex::new();
        let s1 = hash(1);
        let s2 = hash(2);

        idx.install_shard(s1, &[]);
        idx.install_shard(s2, &[]);

        let missing = idx.verify_against_shard_list(&[s1, s2]);
        assert!(missing.is_empty());
    }
}
