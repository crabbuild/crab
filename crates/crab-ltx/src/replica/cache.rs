use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
};

use crate::{CellObjectKind, CellStorageLayout, CrabError, Result};

// Root metadata and directory nodes each have an independent process-wide cap.
const CACHE_BYTES: usize = 8 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    store: u64,
    path: String,
    digest: [u8; 32],
}

#[derive(Default)]
struct Cache {
    objects: HashMap<Key, Arc<[u8]>>,
    order: VecDeque<Key>,
    bytes: usize,
}

impl Cache {
    fn get(&self, key: &Key) -> Option<Arc<[u8]>> {
        self.objects.get(key).cloned()
    }

    fn insert(&mut self, key: Key, bytes: Arc<[u8]>) -> Arc<[u8]> {
        if let Some(existing) = self.objects.get(&key) {
            return Arc::clone(existing);
        }
        while self.bytes.saturating_add(bytes.len()) > CACHE_BYTES {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.objects.remove(&old) {
                self.bytes -= removed.len();
            }
        }
        self.bytes += bytes.len();
        self.order.push_back(key.clone());
        self.objects.insert(key, Arc::clone(&bytes));
        bytes
    }
}

fn band(kind: CellObjectKind) -> Result<&'static Mutex<Cache>> {
    static ROOTS: OnceLock<Mutex<Cache>> = OnceLock::new();
    static DIRECTORIES: OnceLock<Mutex<Cache>> = OnceLock::new();
    match kind {
        CellObjectKind::Root => Ok(ROOTS.get_or_init(|| Mutex::new(Cache::default()))),
        CellObjectKind::Directory => Ok(DIRECTORIES.get_or_init(|| Mutex::new(Cache::default()))),
        _ => Err(CrabError::InvalidState("unsupported immutable cache kind")),
    }
}

fn key(
    layout: &CellStorageLayout,
    cell: &[u8; 32],
    incarnation: &[u8; 16],
    digest: [u8; 32],
    kind: CellObjectKind,
) -> Key {
    Key {
        store: layout.immutable_cache_identity(),
        path: layout
            .incarnation_object_path(cell, incarnation, &digest, kind)
            .to_string(),
        digest,
    }
}

pub(super) fn get(
    layout: &CellStorageLayout,
    cell: &[u8; 32],
    incarnation: &[u8; 16],
    digest: [u8; 32],
    kind: CellObjectKind,
) -> Result<Option<Arc<[u8]>>> {
    band(kind)?
        .lock()
        .map_err(|_| CrabError::InvalidState("immutable object cache poisoned"))
        .map(|cache| cache.get(&key(layout, cell, incarnation, digest, kind)))
}

// Callers insert only digest-verified bytes after a successful immutable upload
// or authenticated read; backup inventory bypasses this process cache.
pub(super) fn insert(
    layout: &CellStorageLayout,
    cell: &[u8; 32],
    incarnation: &[u8; 16],
    digest: [u8; 32],
    kind: CellObjectKind,
    bytes: Arc<[u8]>,
) -> Result<Arc<[u8]>> {
    Ok(band(kind)?
        .lock()
        .map_err(|_| CrabError::InvalidState("immutable object cache poisoned"))?
        .insert(key(layout, cell, incarnation, digest, kind), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_byte_bounded_and_store_isolated() {
        let mut cache = Cache::default();
        for store in 0..10 {
            let key = Key {
                store,
                path: "same/path.root".into(),
                digest: [1; 32],
            };
            cache.insert(key, vec![store as u8; 1 << 20].into());
        }
        assert_eq!(cache.bytes, CACHE_BYTES);
        assert_eq!(cache.objects.len(), 8);
        assert!(!cache.objects.keys().any(|key| key.store < 2));
        for store in 2..10 {
            assert_eq!(
                cache.objects[&Key {
                    store,
                    path: "same/path.root".into(),
                    digest: [1; 32],
                }][0],
                store as u8
            );
        }
    }
}
