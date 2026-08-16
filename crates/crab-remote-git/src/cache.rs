use std::collections::HashMap;
use std::hash::Hash;

pub(crate) struct BoundedLru<K, V> {
    entries: HashMap<K, CacheEntry<V>>,
    max_entries: usize,
    max_bytes: usize,
    resident_bytes: usize,
    clock: u64,
}

struct CacheEntry<V> {
    value: V,
    bytes: usize,
    touched: u64,
}

impl<K, V> BoundedLru<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            max_bytes,
            resident_bytes: 0,
            clock: 0,
        }
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<V> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(key)?;
        entry.touched = self.clock;
        Some(entry.value.clone())
    }

    pub(crate) fn insert(&mut self, key: K, value: V, bytes: usize) -> usize {
        if self.max_entries == 0 || self.max_bytes == 0 || bytes > self.max_bytes {
            return 0;
        }
        self.clock = self.clock.wrapping_add(1);
        if let Some(replaced) = self.entries.remove(&key) {
            self.resident_bytes = self.resident_bytes.saturating_sub(replaced.bytes);
        }
        self.resident_bytes = self.resident_bytes.saturating_add(bytes);
        self.entries.insert(
            key,
            CacheEntry {
                value,
                bytes,
                touched: self.clock,
            },
        );

        let mut evicted = 0;
        while self.entries.len() > self.max_entries || self.resident_bytes > self.max_bytes {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.resident_bytes = self.resident_bytes.saturating_sub(entry.bytes);
                evicted += 1;
            }
        }
        evicted
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        let entry = self.entries.remove(key)?;
        self.resident_bytes = self.resident_bytes.saturating_sub(entry.bytes);
        Some(entry.value)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_is_byte_and_entry_bounded() {
        let mut cache = BoundedLru::new(2, 5);
        assert_eq!(cache.insert(1, "one", 3), 0);
        assert_eq!(cache.insert(2, "two", 2), 0);
        assert_eq!(cache.get(&1), Some("one"));
        assert_eq!(cache.insert(3, "three", 2), 1);
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.resident_bytes(), 5);
    }

    #[test]
    fn zero_capacity_disables_cache() {
        let mut cache = BoundedLru::new(0, 0);
        assert_eq!(cache.insert(1, "one", 1), 0);
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.resident_bytes(), 0);
    }
}
