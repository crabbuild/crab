//! Bounded read-lease pool for protocol adapters without an open/close lifecycle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::warn;

use crate::engine::VfsReadLease;

/// Bounded LRU pool for VFS read leases keyed by protocol file id.
pub struct ReadLeasePool {
    state: Mutex<ReadLeasePoolState>,
    max_entries: usize,
    max_estimated_bytes: usize,
}

struct ReadLeasePoolState {
    entries: HashMap<u64, PooledReadLease>,
    estimated_bytes: usize,
    access_clock: u64,
    temporary_overflows: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
    stale_retries: u64,
}

struct PooledReadLease {
    lease: VfsReadLease,
    pin_count: u32,
    last_access: u64,
    estimated_bytes: usize,
}

/// Read lease pinned for one in-flight protocol operation.
pub struct ReadLeasePin {
    key: u64,
    lease: VfsReadLease,
    pool: Arc<ReadLeasePool>,
}

impl ReadLeasePool {
    pub fn new(max_entries: usize, max_estimated_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ReadLeasePoolState {
                entries: HashMap::new(),
                estimated_bytes: 0,
                access_clock: 0,
                temporary_overflows: 0,
                hits: 0,
                misses: 0,
                evictions: 0,
                stale_retries: 0,
            }),
            max_entries: max_entries.max(1),
            max_estimated_bytes: max_estimated_bytes.max(1),
        })
    }

    pub fn pin(self: &Arc<Self>, key: u64) -> Option<ReadLeasePin> {
        let mut state = self.lock_state();
        let last_access = state.next_access();
        let Some(entry) = state.entries.get_mut(&key) else {
            state.misses = state.misses.saturating_add(1);
            return None;
        };
        entry.pin_count = entry.pin_count.saturating_add(1);
        entry.last_access = last_access;
        let lease = entry.lease.clone();
        state.hits = state.hits.saturating_add(1);
        Some(ReadLeasePin {
            key,
            lease,
            pool: Arc::clone(self),
        })
    }

    pub fn insert_and_pin(self: &Arc<Self>, key: u64, lease: VfsReadLease) -> ReadLeasePin {
        let mut state = self.lock_state();
        let estimated_bytes = lease.estimated_bytes();
        let last_access = state.next_access();

        if let Some(previous_estimated_bytes) = state.entries.get_mut(&key).map(|entry| {
            let previous_estimated_bytes = entry.estimated_bytes;
            entry.lease = lease.clone();
            entry.pin_count = entry.pin_count.saturating_add(1);
            entry.last_access = last_access;
            entry.estimated_bytes = estimated_bytes;
            previous_estimated_bytes
        }) {
            state.estimated_bytes = state
                .estimated_bytes
                .saturating_sub(previous_estimated_bytes)
                .saturating_add(estimated_bytes);
        } else {
            state.estimated_bytes = state.estimated_bytes.saturating_add(estimated_bytes);
            state.entries.insert(
                key,
                PooledReadLease {
                    lease: lease.clone(),
                    pin_count: 1,
                    last_access,
                    estimated_bytes,
                },
            );
        }

        state.shrink_unpinned(self.max_entries, self.max_estimated_bytes, Some(key));
        ReadLeasePin {
            key,
            lease,
            pool: Arc::clone(self),
        }
    }

    pub fn evict(&self, key: u64) {
        let mut state = self.lock_state();
        state.remove_entry(key);
    }

    pub fn invalidate_all(&self) {
        let mut state = self.lock_state();
        let removed = state.entries.len() as u64;
        state.entries.clear();
        state.estimated_bytes = 0;
        state.evictions = state.evictions.saturating_add(removed);
    }

    #[cfg(feature = "nfs")]
    pub fn evict_many<I>(&self, keys: I)
    where
        I: IntoIterator<Item = u64>,
    {
        let mut state = self.lock_state();
        for key in keys {
            state.remove_entry(key);
        }
    }

    pub fn record_stale_retry(&self) {
        let mut state = self.lock_state();
        state.stale_retries = state.stale_retries.saturating_add(1);
    }

    fn unpin(&self, key: u64) {
        let mut state = self.lock_state();
        if let Some(entry) = state.entries.get_mut(&key) {
            entry.pin_count = entry.pin_count.saturating_sub(1);
        }
        state.shrink_unpinned(self.max_entries, self.max_estimated_bytes, None);
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ReadLeasePoolState> {
        self.state.lock().unwrap_or_else(|error| {
            warn!("read lease pool mutex was poisoned; recovering");
            error.into_inner()
        })
    }

    pub fn snapshot(&self) -> ReadLeasePoolSnapshot {
        let state = self.lock_state();
        let pinned_entries = state
            .entries
            .values()
            .filter(|entry| entry.pin_count > 0)
            .count();
        let active_pins = state
            .entries
            .values()
            .map(|entry| u64::from(entry.pin_count))
            .sum();
        ReadLeasePoolSnapshot {
            entries: state.entries.len(),
            max_entries: self.max_entries,
            estimated_bytes: state.estimated_bytes,
            max_estimated_bytes: self.max_estimated_bytes,
            pinned_entries,
            active_pins,
            temporary_overflows: state.temporary_overflows,
            hits: state.hits,
            misses: state.misses,
            evictions: state.evictions,
            stale_retries: state.stale_retries,
        }
    }

    #[cfg(test)]
    fn contains(&self, key: u64) -> bool {
        self.lock_state().entries.contains_key(&key)
    }
}

impl ReadLeasePin {
    pub fn lease(&self) -> &VfsReadLease {
        &self.lease
    }
}

impl Drop for ReadLeasePin {
    fn drop(&mut self) {
        self.pool.unpin(self.key);
    }
}

impl ReadLeasePoolState {
    fn next_access(&mut self) -> u64 {
        self.access_clock = self.access_clock.saturating_add(1);
        self.access_clock
    }

    fn remove_entry(&mut self, key: u64) {
        if let Some(entry) = self.entries.remove(&key) {
            self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.estimated_bytes);
            self.evictions = self.evictions.saturating_add(1);
        }
    }

    fn shrink_unpinned(
        &mut self,
        max_entries: usize,
        max_estimated_bytes: usize,
        protected: Option<u64>,
    ) {
        loop {
            let over_budget =
                self.entries.len() > max_entries || self.estimated_bytes > max_estimated_bytes;
            if !over_budget {
                return;
            }

            if self.entries.len() == 1 && self.estimated_bytes > max_estimated_bytes {
                // One lease can exceed the byte budget by itself. Keep it rather
                // than turning every unpin into a guaranteed cache miss.
                return;
            }

            let Some(evict_key) = self
                .entries
                .iter()
                .filter(|(key, entry)| Some(**key) != protected && entry.pin_count == 0)
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| *key)
            else {
                self.temporary_overflows = self.temporary_overflows.saturating_add(1);
                return;
            };

            self.remove_entry(evict_key);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadLeasePoolSnapshot {
    pub entries: usize,
    pub max_entries: usize,
    pub estimated_bytes: usize,
    pub max_estimated_bytes: usize,
    pub pinned_entries: usize,
    pub active_pins: u64,
    pub temporary_overflows: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub stale_retries: u64,
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used)]

    use super::*;
    use crate::engine::{ReadSourceKey, VfsReadLease};

    fn lease(path: &str) -> VfsReadLease {
        VfsReadLease::for_test(ReadSourceKey::BaseEmpty {
            generation: 1,
            overlay_version: 0,
            path: path.to_owned(),
        })
    }

    #[test]
    fn pin_reuses_existing_lease() {
        let pool = ReadLeasePool::new(2, usize::MAX);
        drop(pool.insert_and_pin(1, lease("a.bin")));

        let pin = pool.pin(1).unwrap();

        assert_eq!(pin.lease().known_size(), Some(0));
        assert_eq!(pool.snapshot().hits, 1);
    }

    #[test]
    fn pin_miss_does_not_insert() {
        let pool = ReadLeasePool::new(2, usize::MAX);

        assert!(pool.pin(1).is_none());

        assert_eq!(pool.snapshot().misses, 1);
        assert_eq!(pool.snapshot().entries, 0);
    }

    #[test]
    fn insert_evicts_least_recent_unpinned_entry() {
        let pool = ReadLeasePool::new(2, usize::MAX);
        drop(pool.insert_and_pin(1, lease("a.bin")));
        drop(pool.insert_and_pin(2, lease("b.bin")));
        drop(pool.pin(1).unwrap());

        drop(pool.insert_and_pin(3, lease("c.bin")));

        assert!(pool.contains(1));
        assert!(!pool.contains(2));
        assert!(pool.contains(3));
    }

    #[test]
    fn pinned_entries_can_temporarily_overflow_then_shrink_on_unpin() {
        let pool = ReadLeasePool::new(1, usize::MAX);
        let first = pool.insert_and_pin(1, lease("a.bin"));
        let second = pool.insert_and_pin(2, lease("b.bin"));

        assert_eq!(pool.snapshot().entries, 2);
        assert_eq!(pool.snapshot().temporary_overflows, 1);

        drop(first);
        drop(second);

        assert_eq!(pool.snapshot().entries, 1);
    }

    #[test]
    fn explicit_evict_removes_entry_even_while_pin_is_alive() {
        let pool = ReadLeasePool::new(2, usize::MAX);
        let pin = pool.insert_and_pin(1, lease("a.bin"));

        pool.evict(1);

        assert!(!pool.contains(1));
        assert_eq!(pin.lease().known_size(), Some(0));
    }

    #[test]
    fn invalidate_all_removes_cached_entries_even_while_pinned() {
        let pool = ReadLeasePool::new(4, usize::MAX);
        let first = pool.insert_and_pin(1, lease("a.bin"));
        drop(pool.insert_and_pin(2, lease("b.bin")));

        pool.invalidate_all();

        assert_eq!(first.lease().known_size(), Some(0));
        assert_eq!(
            pool.snapshot(),
            ReadLeasePoolSnapshot {
                entries: 0,
                max_entries: 4,
                estimated_bytes: 0,
                max_estimated_bytes: usize::MAX,
                pinned_entries: 0,
                active_pins: 0,
                temporary_overflows: 0,
                hits: 0,
                misses: 0,
                evictions: 2,
                stale_retries: 0,
            }
        );
    }

    #[test]
    fn memory_budget_evicts_unpinned_entries() {
        let first = lease("a.bin");
        let pool = ReadLeasePool::new(10, first.estimated_bytes() + 1);
        drop(pool.insert_and_pin(1, first));

        drop(pool.insert_and_pin(2, lease("b.bin")));

        assert_eq!(pool.snapshot().entries, 1);
    }

    #[test]
    fn snapshot_reports_runtime_counters_and_pins() {
        let pool = ReadLeasePool::new(2, 4096);
        let first = pool.insert_and_pin(1, lease("a.bin"));

        drop(pool.pin(1).unwrap());
        assert!(pool.pin(2).is_none());
        pool.record_stale_retry();

        let pinned = pool.snapshot();
        assert_eq!(pinned.entries, 1);
        assert_eq!(pinned.max_entries, 2);
        assert_eq!(pinned.max_estimated_bytes, 4096);
        assert_eq!(pinned.pinned_entries, 1);
        assert_eq!(pinned.active_pins, 1);
        assert_eq!(pinned.hits, 1);
        assert_eq!(pinned.misses, 1);
        assert_eq!(pinned.stale_retries, 1);

        drop(first);
        pool.evict(1);

        let evicted = pool.snapshot();
        assert_eq!(evicted.entries, 0);
        assert_eq!(evicted.evictions, 1);
    }
}
