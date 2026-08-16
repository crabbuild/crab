//! Bounded set of chunk hashes verified during this process lifetime.
//!
//! Prevents redundant blake3 re-verification for chunks that have already
//! been verified since the daemon started. Uses a `DashSet` for lock-free
//! concurrent reads and a `Mutex<VecDeque>` for FIFO eviction ordering.

use std::collections::VecDeque;
use std::sync::Mutex;

use dashmap::DashSet;

use crab_xet::xorb::format::MerkleHash;

/// Default capacity: 1 million entries (~32 MB for MerkleHash keys).
const DEFAULT_CAPACITY: usize = 1_000_000;

/// Bounded set of chunk hashes that have passed blake3 verification.
///
/// Thread-safe for concurrent reads (`contains`) via `DashSet`. Insertions
/// acquire the `order` mutex briefly to maintain FIFO eviction order.
/// When the set reaches capacity, the oldest entry is evicted on each insert.
pub struct VerifiedSet {
    set: DashSet<MerkleHash>,
    order: Mutex<VecDeque<MerkleHash>>,
    capacity: usize,
}

impl VerifiedSet {
    /// Create a new verified set with the given capacity.
    ///
    /// Use `0` for `capacity` to get the default (1 million entries).
    pub fn new(capacity: usize) -> Self {
        let cap = if capacity == 0 {
            DEFAULT_CAPACITY
        } else {
            capacity
        };
        Self {
            set: DashSet::with_capacity(cap),
            order: Mutex::new(VecDeque::with_capacity(cap)),
            capacity: cap,
        }
    }

    /// Check whether a hash has been verified.
    pub fn contains(&self, hash: &MerkleHash) -> bool {
        self.set.contains(hash)
    }

    /// Record a hash as verified. If the set is at capacity, the oldest
    /// entry is evicted first (FIFO).
    pub fn insert(&self, hash: MerkleHash) {
        // Quick check without lock — skip if already present.
        if self.set.contains(&hash) {
            return;
        }

        let mut order = self
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Recheck after acquiring the lock to close the TOCTOU window
        // between the lock-free check above and the mutex acquisition.
        if self.set.contains(&hash) {
            return;
        }

        // Evict oldest if at capacity.
        while order.len() >= self.capacity {
            if let Some(oldest) = order.pop_front() {
                self.set.remove(&oldest);
            }
        }

        self.set.insert(hash);
        order.push_back(hash);
    }

    /// Number of entries currently in the set.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.set.len()
    }
}

impl Default for VerifiedSet {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl std::fmt::Debug for VerifiedSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let order_len = self.order.lock().map_or(0, |o| o.len());
        f.debug_struct("VerifiedSet")
            .field("capacity", &self.capacity)
            .field("size", &self.set.len())
            .field("order_len", &order_len)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn make_hash(val: u8) -> MerkleHash {
        MerkleHash::from([val; 32])
    }

    #[test]
    fn insert_and_contains() {
        let vs = VerifiedSet::new(10);
        let h = make_hash(0x01);
        assert!(!vs.contains(&h));
        vs.insert(h);
        assert!(vs.contains(&h));
    }

    #[test]
    fn duplicate_insert_is_idempotent() {
        let vs = VerifiedSet::new(10);
        let h = make_hash(0x02);
        vs.insert(h);
        vs.insert(h);
        assert_eq!(vs.len(), 1);
    }

    #[test]
    fn fifo_eviction_when_full() {
        let vs = VerifiedSet::new(3);
        let h1 = make_hash(0x01);
        let h2 = make_hash(0x02);
        let h3 = make_hash(0x03);
        let h4 = make_hash(0x04);

        vs.insert(h1);
        vs.insert(h2);
        vs.insert(h3);
        assert_eq!(vs.len(), 3);

        // Inserting h4 should evict h1 (oldest).
        vs.insert(h4);
        assert_eq!(vs.len(), 3);
        assert!(!vs.contains(&h1), "h1 should be evicted");
        assert!(vs.contains(&h2));
        assert!(vs.contains(&h3));
        assert!(vs.contains(&h4));
    }

    #[test]
    fn size_never_exceeds_capacity() {
        let cap = 5;
        let vs = VerifiedSet::new(cap);
        for i in 0u8..20 {
            vs.insert(make_hash(i));
            assert!(vs.len() <= cap);
        }
    }

    #[test]
    fn default_uses_million_capacity() {
        let vs = VerifiedSet::default();
        assert_eq!(vs.capacity, DEFAULT_CAPACITY);
    }
}
