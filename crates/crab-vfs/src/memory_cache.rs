//! Bounded immutable decoded bytes shared by VFS read families.

use bytes::Bytes;
use lru::LruCache;

pub(crate) struct MemoryCache<K> {
    entries: LruCache<K, CachedBytes>,
    retained_bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

struct CachedBytes {
    bytes: Bytes,
    charge: usize,
}

impl<K: Eq + std::hash::Hash> MemoryCache<K> {
    pub(crate) fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            // This owner evicts before insertion so byte accounting observes
            // every removal; the library must not evict independently.
            entries: LruCache::unbounded(),
            retained_bytes: 0,
            max_bytes,
            max_entries,
        }
    }

    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<Bytes> {
        self.entries.get(key).map(|entry| entry.bytes.clone())
    }

    pub(crate) fn insert(&mut self, key: K, bytes: &Bytes) {
        if self.max_entries == 0 || bytes.len() > self.max_bytes {
            return;
        }
        let mut owned = Vec::new();
        if owned.try_reserve_exact(bytes.len()).is_err() || owned.capacity() > self.max_bytes {
            return;
        }
        // A verified value can borrow a larger allocation. Own and
        // charge precisely this value; mutable files never back cache hits.
        let charge = owned.capacity();
        if let Some(previous) = self.entries.pop(&key) {
            self.retained_bytes -= previous.charge;
        }
        while self.retained_bytes + charge > self.max_bytes
            || self.entries.len() >= self.max_entries
        {
            let Some((_, removed)) = self.entries.pop_lru() else {
                break;
            };
            self.retained_bytes -= removed.charge;
        }
        owned.extend_from_slice(bytes);
        self.retained_bytes += charge;
        self.entries.put(
            key,
            CachedBytes {
                bytes: owned.into(),
                charge,
            },
        );
    }
}
