//! Object-store-only verified directory cache.

use super::*;
use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(feature = "replica")]
use std::collections::{BTreeMap, HashMap, VecDeque};

#[cfg(feature = "replica")]
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct DirectoryCacheIndex {
    version: u8,
    entries: BTreeMap<String, u64>,
}

#[cfg(feature = "replica")]
pub(crate) struct DirectoryCacheState {
    entries: BTreeMap<String, u64>,
    order: VecDeque<String>,
    bytes: u64,
    reservations: BTreeMap<String, DiskReservation>,
}

#[cfg(feature = "replica")]
/// Point-in-time usage of the verified immutable directory-node cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryCacheStats {
    entries: usize,
    bytes: u64,
    capacity_bytes: u64,
}

#[cfg(feature = "replica")]
impl DirectoryCacheStats {
    /// Returns the number of indexed cache entries.
    #[must_use]
    pub const fn entries(self) -> usize {
        self.entries
    }

    /// Returns bytes occupied by verified cache entries.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.bytes
    }

    /// Returns the cache's byte ceiling.
    #[must_use]
    pub const fn capacity_bytes(self) -> u64 {
        self.capacity_bytes
    }
}

#[cfg(feature = "replica")]
pub(crate) struct DirectoryCache {
    filesystem: Arc<dyn FileSystem>,
    pub(super) budget: DiskBudget,
    root: PathBuf,
    index: PathBuf,
    max_bytes: u64,
    state: Mutex<DirectoryCacheState>,
    fills: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[cfg(feature = "replica")]
pub(crate) const MAX_DIRECTORY_CACHE_ENTRIES: usize = 16_384;

#[cfg(feature = "replica")]
impl DirectoryCache {
    #[cfg(test)]
    pub(crate) fn new(filesystem: Arc<dyn FileSystem>, root: PathBuf, max_bytes: u64) -> Self {
        Self::with_budget(filesystem, root, max_bytes, DiskBudget::new(max_bytes))
    }

    pub(crate) fn with_budget(
        filesystem: Arc<dyn FileSystem>,
        root: PathBuf,
        max_bytes: u64,
        budget: DiskBudget,
    ) -> Self {
        let _ = filesystem.cleanup_private_temporaries(&root);
        let index = root.join("index-v1.json");
        let entries = filesystem
            .open(&index)
            .and_then(|mut file| {
                let length = file.file_len()?;
                let length = usize::try_from(length).map_err(io::Error::other)?;
                let bytes = file.read_exact_at(0, length)?;
                let index: DirectoryCacheIndex =
                    serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if index.version != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsupported Cell directory cache index",
                    ));
                }
                Ok(index.entries)
            })
            .unwrap_or_default();
        let entries = entries
            .into_iter()
            .filter_map(|(key, length)| {
                let path = root.join(hex_digest(blake3::hash(key.as_bytes()).as_bytes()));
                let valid = length != 0
                    && length <= max_bytes
                    && filesystem.exists(&path).ok() == Some(true)
                    && filesystem.file_len(&path).ok() == Some(length)
                    && safe_cache_entry(&filesystem, &root, &path).ok() == Some(true);
                if !valid {
                    let _ = filesystem.remove_file(&path);
                }
                valid.then_some((key, length))
            })
            .collect::<BTreeMap<_, _>>();
        let mut retained = BTreeMap::new();
        let mut reservations = BTreeMap::new();
        for (key, length) in entries {
            let within_limits = length <= max_bytes
                && retained.len() < MAX_DIRECTORY_CACHE_ENTRIES
                && retained.values().copied().fold(0_u64, u64::saturating_add)
                    <= max_bytes.saturating_sub(length);
            let reservation = within_limits
                .then(|| budget.try_reserve(length).ok())
                .flatten();
            if let Some(reservation) = reservation {
                retained.insert(key.clone(), length);
                reservations.insert(key, reservation);
            } else {
                let path = root.join(hex_digest(blake3::hash(key.as_bytes()).as_bytes()));
                let _ = filesystem.remove_file(&path);
            }
        }
        let bytes = retained.values().copied().sum();
        let order = retained.keys().cloned().collect();
        Self {
            filesystem,
            budget,
            root,
            index,
            max_bytes,
            state: Mutex::new(DirectoryCacheState {
                entries: retained,
                order,
                bytes,
                reservations,
            }),
            fills: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn key_path(&self, key: &str) -> PathBuf {
        let digest = blake3::hash(key.as_bytes());
        self.root.join(hex_digest(digest.as_bytes()))
    }

    pub(crate) fn stats(&self) -> DirectoryCacheStats {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        DirectoryCacheStats {
            entries: state.entries.len(),
            bytes: state.bytes,
            capacity_bytes: self.max_bytes,
        }
    }

    pub(crate) fn fill_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut fills = match self.fills.lock() {
            Ok(fills) => fills,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(lock) = fills.get(key) {
            return Arc::clone(lock);
        }
        let lock = Arc::new(Mutex::new(()));
        if fills.len() < MAX_DIRECTORY_CACHE_ENTRIES {
            fills.insert(key.to_owned(), Arc::clone(&lock));
        }
        lock
    }

    pub(crate) fn get(&self, key: &str, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
        let lock = self.fill_lock(key);
        let _guard = lock
            .lock()
            .map_err(|_| io::Error::other("cache fill lock poisoned"))?;
        let path = self.key_path(key);
        if !self.filesystem.exists(&path)? {
            if self.remove_entry(key) {
                self.persist_index();
            }
            return Ok(None);
        }
        if !safe_cache_entry(&self.filesystem, &self.root, &path)? {
            let _ = self.filesystem.remove_file(&path);
            self.remove_entry(key);
            self.persist_index();
            return Ok(None);
        }
        let length = self.filesystem.file_len(&path)?;
        if length == 0 || length > max_bytes || length > self.max_bytes {
            let _ = self.filesystem.remove_file(&path);
            self.remove_entry(key);
            self.persist_index();
            return Ok(None);
        }
        let indexed_length = match self.state.lock() {
            Ok(state) => state.entries.get(key).copied(),
            Err(poisoned) => poisoned.into_inner().entries.get(key).copied(),
        };
        if indexed_length != Some(length) {
            let _ = self.filesystem.remove_file(&path);
            self.remove_entry(key);
            self.persist_index();
            return Ok(None);
        }
        let mut file = match self.filesystem.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let bytes = match file.read_exact_at(0, usize::try_from(length).map_err(io::Error::other)?)
        {
            Ok(bytes) => bytes,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::InvalidData
                        | io::ErrorKind::NotFound
                ) =>
            {
                let _ = self.filesystem.remove_file(&path);
                self.remove_entry(key);
                self.persist_index();
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if bytes.len() as u64 != length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "directory cache entry was truncated",
            ));
        }
        self.touch_entry(key, length);
        // Recency lives only in memory. Persisting an unchanged membership map
        // turns a verified disk-cache hit into an unnecessary durable write.
        Ok(Some(bytes))
    }

    pub(crate) fn put(&self, key: &str, bytes: &[u8], max_entry: u64) -> io::Result<()> {
        if bytes.is_empty() || bytes.len() as u64 > max_entry || bytes.len() as u64 > self.max_bytes
        {
            return Ok(());
        }
        let lock = self.fill_lock(key);
        let _guard = lock
            .lock()
            .map_err(|_| io::Error::other("cache fill lock poisoned"))?;
        self.filesystem.create_dir_all(&self.root)?;
        let path = self.key_path(key);
        if self.filesystem.exists(&path)? {
            let length = self.filesystem.file_len(&path)?;
            if length == bytes.len() as u64 {
                self.touch_entry(key, length);
                self.persist_index();
                return Ok(());
            }
            let _ = self.filesystem.remove_file(&path);
            self.remove_entry(key);
        }
        self.make_room(bytes.len() as u64)?;
        let reservation = self
            .budget
            .try_reserve(bytes.len() as u64)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let temporary = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            NEXT_CACHE_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = match self.filesystem.create(&temporary) {
            Ok(file) => file,
            Err(error) => {
                drop(reservation);
                return Err(error);
            }
        };
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _ = self.filesystem.remove_file(&temporary);
            drop(reservation);
            return Err(error);
        }
        drop(file);
        if let Err(error) = self.filesystem.rename(&temporary, &path) {
            let _ = self.filesystem.remove_file(&temporary);
            drop(reservation);
            return Err(error);
        }
        self.touch_entry_with_reservation(key, bytes.len() as u64, reservation);
        self.evict()?;
        self.persist_index();
        Ok(())
    }

    pub(crate) fn invalidate(&self, key: &str) -> io::Result<()> {
        let lock = self.fill_lock(key);
        let _guard = lock
            .lock()
            .map_err(|_| io::Error::other("cache fill lock poisoned"))?;
        let path = self.key_path(key);
        match self.filesystem.remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.remove_entry(key);
        self.persist_index();
        Ok(())
    }

    pub(crate) fn touch_entry(&self, key: &str, length: u64) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = state.entries.insert(key.to_owned(), length) {
            state.bytes = state.bytes.saturating_sub(previous);
            state.order.retain(|entry| entry != key);
        }
        state.bytes = state.bytes.saturating_add(length);
        state.order.push_back(key.to_owned());
    }

    pub(crate) fn touch_entry_with_reservation(
        &self,
        key: &str,
        length: u64,
        reservation: DiskReservation,
    ) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = state.entries.insert(key.to_owned(), length) {
            state.bytes = state.bytes.saturating_sub(previous);
            state.order.retain(|entry| entry != key);
        }
        state.bytes = state.bytes.saturating_add(length);
        state.order.push_back(key.to_owned());
        state.reservations.insert(key.to_owned(), reservation);
    }

    pub(crate) fn remove_entry(&self, key: &str) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let previous = state.entries.remove(key);
        if let Some(previous) = previous {
            state.bytes = state.bytes.saturating_sub(previous);
        }
        state.reservations.remove(key);
        state.order.retain(|entry| entry != key);
        previous.is_some()
    }

    pub(crate) fn make_room(&self, required: u64) -> io::Result<()> {
        loop {
            let victim = {
                let state = match self.state.lock() {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                };
                (state.bytes.saturating_add(required) > self.max_bytes
                    || state.entries.len() >= MAX_DIRECTORY_CACHE_ENTRIES)
                    .then(|| state.order.front().cloned())
            };
            let Some(Some(key)) = victim else {
                return Ok(());
            };
            let path = self.key_path(&key);
            match self.filesystem.remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            self.remove_entry(&key);
        }
    }

    pub(crate) fn evict(&self) -> io::Result<()> {
        loop {
            let victim = {
                let state = match self.state.lock() {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                };
                (state.bytes > self.max_bytes || state.entries.len() > MAX_DIRECTORY_CACHE_ENTRIES)
                    .then(|| state.order.front().cloned())
            };
            let Some(Some(key)) = victim else {
                return Ok(());
            };
            let path = self.key_path(&key);
            match self.filesystem.remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            self.remove_entry(&key);
        }
    }

    pub(crate) fn persist_index(&self) {
        let entries = match self.state.lock() {
            Ok(state) => state.entries.clone(),
            Err(poisoned) => poisoned.into_inner().entries.clone(),
        };
        if self.filesystem.create_dir_all(&self.root).is_err() {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(&DirectoryCacheIndex {
            version: 1,
            entries,
        }) else {
            return;
        };
        let temporary = self.root.join(format!(
            ".index-tmp-{}-{}",
            std::process::id(),
            NEXT_CACHE_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let Ok(mut file) = self.filesystem.create(&temporary) else {
            return;
        };
        if file.write_all(&bytes).is_err() || file.sync_all().is_err() {
            let _ = self.filesystem.remove_file(&temporary);
            return;
        }
        drop(file);
        let _ = self.filesystem.rename(&temporary, &self.index);
    }
}

#[cfg(feature = "replica")]
fn safe_cache_entry(
    filesystem: &Arc<dyn FileSystem>,
    root: &Path,
    path: &Path,
) -> io::Result<bool> {
    let canonical_root = filesystem.canonicalize(root)?;
    let canonical_path = filesystem.canonicalize(path)?;
    Ok(canonical_path.parent() == Some(canonical_root.as_path())
        && canonical_path.file_name() == path.file_name())
}

#[cfg(feature = "replica")]
pub(crate) static NEXT_CACHE_TEMP: AtomicU64 = AtomicU64::new(1);

#[cfg(feature = "replica")]
fn hex_digest(bytes: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}
