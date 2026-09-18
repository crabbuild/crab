//! Injectable local I/O, clocks and jobs adapted from Celld host.rs.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "replica")]
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Admission hook used by an embedding runtime to charge local bytes to its
/// node-wide resource ledger.
///
/// The hook is called synchronously with the exact aggregate budget usage for
/// every reserve, resize, release, and late installation operation.
pub trait DiskBudgetAdmission: Send + Sync {
    /// Reconciles the exact aggregate bytes currently reserved by this budget.
    fn reconcile(&self, bytes: u64) -> crate::Result<()>;

    /// Reports whether this admission owner is still alive.
    fn is_live(&self) -> bool {
        true
    }
}

/// Resource class charged by an embedding runtime for replica-host work.
#[cfg(feature = "replica")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostResourceKind {
    /// One bounded object-store or immutable-file I/O operation.
    Io,
    /// One blocking host job dispatched to the replica executor.
    BlockingJob,
    /// One full recovery or restore cohort.
    Recovery,
    /// One capture/compaction dirty-memory cohort.
    Dirty,
    /// One MiB of temporary scratch admission.
    Scratch,
}

/// Admission hook used by an embedding runtime to charge host work to its
/// node-wide resource ledger. The returned permit owns the charge until drop.
#[cfg(feature = "replica")]
pub trait HostResourceAdmission: Send + Sync {
    /// Reserves `units` of one host resource without waiting.
    fn reserve(
        &self,
        kind: HostResourceKind,
        units: u32,
    ) -> crate::Result<Box<dyn HostResourcePermit>>;
}

/// Opaque lifetime token returned by [`HostResourceAdmission::reserve`].
#[cfg(feature = "replica")]
pub trait HostResourcePermit: Send + Sync {}

type DiskAdmissions = Vec<Arc<dyn DiskBudgetAdmission>>;

/// Shared byte-precise admission for local files owned by active database work.
#[derive(Clone)]
pub struct DiskBudget {
    inner: Arc<DiskBudgetInner>,
}

struct DiskBudgetInner {
    capacity: u64,
    used: AtomicU64,
    has_admissions: AtomicBool,
    admissions: Mutex<DiskAdmissions>,
}

impl fmt::Debug for DiskBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiskBudget")
            .field("capacity", &self.capacity())
            .field("used", &self.used())
            .finish_non_exhaustive()
    }
}

impl DiskBudget {
    /// Creates a budget. A zero capacity rejects every non-empty reservation.
    #[must_use]
    pub fn new(capacity: u64) -> Self {
        Self {
            inner: Arc::new(DiskBudgetInner {
                capacity,
                used: AtomicU64::new(0),
                has_admissions: AtomicBool::new(false),
                admissions: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Installs one embedding ledger and immediately reconciles existing bytes.
    ///
    /// All clones of this budget observe the same hook. Multiple live runtimes
    /// may observe the same process-wide budget; dead hooks are removed before
    /// the new hook is registered.
    pub fn install_admission(&self, admission: Arc<dyn DiskBudgetAdmission>) -> crate::Result<()> {
        let mut current = self
            .inner
            .admissions
            .lock()
            .map_err(|_| crate::CrabError::InvalidState("disk admission lock poisoned"))?;
        current.retain(|admission| admission.is_live());
        let had_admissions = !current.is_empty();
        self.inner.has_admissions.store(true, Ordering::Release);
        current.push(admission);
        let result = current.last().map_or_else(
            || {
                Err(crate::CrabError::InvalidState(
                    "disk admission was not installed",
                ))
            },
            |admission| admission.reconcile(self.used()),
        );
        if let Err(error) = result {
            current.pop();
            self.inner
                .has_admissions
                .store(had_admissions, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    /// Reserves bytes without waiting or overcommitting the configured capacity.
    pub fn try_reserve(&self, bytes: u64) -> crate::Result<DiskReservation> {
        self.add(bytes)?;
        if let Err(error) = self.reconcile_admissions(self.used()) {
            let _ = self.remove(bytes);
            let _ = self.reconcile_admissions(self.used());
            return Err(error);
        }
        Ok(DiskReservation {
            budget: self.clone(),
            bytes: Mutex::new(bytes),
        })
    }

    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.inner.capacity
    }

    #[must_use]
    pub fn used(&self) -> u64 {
        self.inner.used.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn available(&self) -> u64 {
        self.capacity().saturating_sub(self.used())
    }

    fn add(&self, bytes: u64) -> crate::Result<()> {
        self.inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= self.inner.capacity)
            })
            .map(|_| ())
            .map_err(|_| crate::CrabError::Limit("local disk bytes"))
    }

    fn reconcile_admissions(&self, bytes: u64) -> crate::Result<()> {
        let Some(mut admissions) = self.live_admissions()? else {
            return Ok(());
        };
        Self::reconcile_admissions_locked(&mut admissions, bytes)
    }

    fn live_admissions(&self) -> crate::Result<Option<MutexGuard<'_, DiskAdmissions>>> {
        if !self.inner.has_admissions.load(Ordering::Acquire) {
            return Ok(None);
        }
        let mut admissions = self
            .inner
            .admissions
            .lock()
            .map_err(|_| crate::CrabError::InvalidState("disk admission lock poisoned"))?;
        admissions.retain(|admission| admission.is_live());
        if admissions.is_empty() {
            self.inner.has_admissions.store(false, Ordering::Release);
            return Ok(None);
        }
        Ok(Some(admissions))
    }

    fn reconcile_admissions_locked(
        admissions: &mut DiskAdmissions,
        bytes: u64,
    ) -> crate::Result<()> {
        for admission in admissions {
            admission.reconcile(bytes)?;
        }
        Ok(())
    }

    fn remove(&self, bytes: u64) -> crate::Result<()> {
        self.inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_sub(bytes)
            })
            .map(|_| ())
            .map_err(|_| crate::CrabError::InvalidState("local disk reservation underflow"))
    }
}

/// Owned local-disk admission released when its owner drops it.
pub struct DiskReservation {
    budget: DiskBudget,
    bytes: Mutex<u64>,
}

impl fmt::Debug for DiskReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiskReservation")
            .field("bytes", &self.bytes())
            .finish_non_exhaustive()
    }
}

impl DiskReservation {
    /// Adds bytes to this reservation without exceeding the shared budget.
    pub fn try_grow(&self, bytes: u64) -> crate::Result<()> {
        let mut held = match self.bytes.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        let next = held
            .checked_add(bytes)
            .ok_or(crate::CrabError::Limit("local disk bytes"))?;
        self.budget.add(bytes)?;
        if let Err(error) = self.budget.reconcile_admissions(self.budget.used()) {
            let _ = self.budget.remove(bytes);
            let _ = self.budget.reconcile_admissions(self.budget.used());
            return Err(error);
        }
        *held = next;
        Ok(())
    }

    /// Changes the exact held byte count, releasing capacity when it shrinks.
    pub fn resize(&self, bytes: u64) -> crate::Result<()> {
        let mut held = match self.bytes.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        let current = *held;
        if bytes > current {
            let added = bytes - current;
            self.budget.add(added)?;
            if let Err(error) = self.budget.reconcile_admissions(self.budget.used()) {
                let _ = self.budget.remove(added);
                let _ = self.budget.reconcile_admissions(self.budget.used());
                return Err(error);
            }
            *held = bytes;
            return Ok(());
        }
        let released = current - bytes;
        *held = bytes;
        if let Err(error) = self.budget.remove(released) {
            *held = current;
            return Err(error);
        }
        if let Err(error) = self.budget.reconcile_admissions(self.budget.used()) {
            self.budget.add(released)?;
            *held = current;
            let _ = self.budget.reconcile_admissions(self.budget.used());
            return Err(error);
        }
        Ok(())
    }

    fn release(&self) {
        let mut held = match self.bytes.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        let released = *held;
        *held = 0;
        let _ = self.budget.remove(released);
        let _ = self.budget.reconcile_admissions(self.budget.used());
    }

    #[must_use]
    pub fn bytes(&self) -> u64 {
        match self.bytes.lock() {
            Ok(held) => *held,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        self.release();
    }
}

/// An open local artifact/WAL handle supplied by a host filesystem.
///
/// Positional reads and writes use their explicit offsets. A handle returned by
/// `FileSystem::open_rw` supports both operations.
pub trait FileIo: Send {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    fn sync_all(&mut self) -> io::Result<()>;
    fn file_len(&self) -> io::Result<u64>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
}

/// Local filesystem boundary; SQLite pager I/O remains under its selected VFS.
///
/// `create` must exclusively create a new file. `open_rw` must not create.
/// `rename` must sync the destination parent before succeeding. Implementations
/// must preserve underlying I/O errors.
/// `exists` must detect dangling symlinks. `create_dir` is an exclusive claim.
/// `persist_new` atomically installs fully synced bytes without replacing any
/// destination and syncs its parent; `persist_file_new` does the same for an
/// already synced same-directory scratch file. An error after installation is
/// ambiguous.
pub trait FileSystem: Send + Sync {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FileIo>>;
    fn open_rw(&self, path: &Path) -> io::Result<Box<dyn FileIo>>;
    fn create(&self, path: &Path) -> io::Result<Box<dyn FileIo>>;
    fn file_len(&self, path: &Path) -> io::Result<u64>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn exists(&self, path: &Path) -> io::Result<bool>;
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    fn sync_parent(&self, path: &Path) -> io::Result<()>;
    fn persist_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn persist_file_new(&self, source: &Path, destination: &Path) -> io::Result<()>;

    /// Removes abandoned private cache temporaries below `root`.
    ///
    /// Host filesystems that cannot enumerate a private directory may leave
    /// this as a no-op; the cache remains fail-closed because only indexed,
    /// canonical entries are ever read.
    fn cleanup_private_temporaries(&self, _root: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "replica")]
#[derive(serde::Serialize, serde::Deserialize)]
struct DirectoryCacheIndex {
    version: u8,
    entries: BTreeMap<String, u64>,
}

#[cfg(feature = "replica")]
struct DirectoryCacheState {
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
struct DirectoryCache {
    filesystem: Arc<dyn FileSystem>,
    budget: DiskBudget,
    root: PathBuf,
    index: PathBuf,
    max_bytes: u64,
    state: Mutex<DirectoryCacheState>,
    fills: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[cfg(feature = "replica")]
const MAX_DIRECTORY_CACHE_ENTRIES: usize = 16_384;

#[cfg(feature = "replica")]
impl DirectoryCache {
    #[cfg(test)]
    fn new(filesystem: Arc<dyn FileSystem>, root: PathBuf, max_bytes: u64) -> Self {
        Self::with_budget(filesystem, root, max_bytes, DiskBudget::new(max_bytes))
    }

    fn with_budget(
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

    fn key_path(&self, key: &str) -> PathBuf {
        let digest = blake3::hash(key.as_bytes());
        self.root.join(hex_digest(digest.as_bytes()))
    }

    fn stats(&self) -> DirectoryCacheStats {
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

    fn fill_lock(&self, key: &str) -> Arc<Mutex<()>> {
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

    fn get(&self, key: &str, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
        let lock = self.fill_lock(key);
        let _guard = lock
            .lock()
            .map_err(|_| io::Error::other("cache fill lock poisoned"))?;
        let path = self.key_path(key);
        if !self.filesystem.exists(&path)? {
            self.remove_entry(key);
            self.persist_index();
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
        self.persist_index();
        Ok(Some(bytes))
    }

    fn put(&self, key: &str, bytes: &[u8], max_entry: u64) -> io::Result<()> {
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

    fn invalidate(&self, key: &str) -> io::Result<()> {
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

    fn touch_entry(&self, key: &str, length: u64) {
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

    fn touch_entry_with_reservation(&self, key: &str, length: u64, reservation: DiskReservation) {
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

    fn remove_entry(&self, key: &str) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = state.entries.remove(key) {
            state.bytes = state.bytes.saturating_sub(previous);
        }
        state.reservations.remove(key);
        state.order.retain(|entry| entry != key);
    }

    fn make_room(&self, required: u64) -> io::Result<()> {
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

    fn evict(&self) -> io::Result<()> {
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

    fn persist_index(&self) {
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
static NEXT_CACHE_TEMP: AtomicU64 = AtomicU64::new(1);

#[cfg(feature = "replica")]
fn hex_digest(bytes: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

/// Rechecks host disk pressure after full-job scratch admission.
///
/// `reserved_bytes` is the process-wide scratch reservation, including the
/// current job. An embedding service can combine it with other local-disk
/// reservations and an operator reserve before allowing remote downloads.
#[cfg(feature = "replica")]
pub trait ScratchMonitor: Send + Sync {
    fn ensure_available(&self, reserved_bytes: u64) -> io::Result<()>;
}

#[cfg(feature = "replica")]
struct UnlimitedScratch;

#[cfg(feature = "replica")]
impl ScratchMonitor for UnlimitedScratch {
    fn ensure_available(&self, _: u64) -> io::Result<()> {
        Ok(())
    }
}

/// Wall-clock observations used in LTX timestamps and checkpoint eligibility.
pub trait Clock: Send + Sync {
    fn unix_millis(&self) -> i64;
    fn file_age(&self, path: &Path) -> io::Result<Duration>;
}

/// Blocking dispatch boundary; success means the job was accepted for execution.
///
/// The dispatcher must eventually run or drop the job. Dropped jobs and panics
/// become errors to the awaiting operation; cancellation does not undo side effects.
#[cfg(feature = "replica")]
pub trait Executor: Send + Sync {
    fn dispatch(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<()>;
    /// Starts a long-lived worker independently of the caller and dispatch pool.
    ///
    /// Must not queue behind the blocking caller: SQLite waits synchronously
    /// for this worker. The worker drives its own Tokio I/O runtime until closed.
    fn start_worker(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<Box<dyn Worker>>;
}

/// An independently progressing worker joined after its input queue closes.
#[cfg(feature = "replica")]
pub trait Worker: Send + Sync {
    fn join(self: Box<Self>) -> io::Result<()>;
}

/// Cloneable host facilities; defaults retain standard filesystem and Tokio behavior.
///
/// The filesystem and selected SQLite VFS must address the same namespace.
/// Injecting local facilities does not replace object-store transport.
#[derive(Clone)]
pub struct Host {
    pub(crate) filesystem: Arc<dyn FileSystem>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) sqlite_vfs: Option<String>,
    pub(crate) local_disk: DiskBudget,
    #[cfg(feature = "replica")]
    pub(crate) executor: Arc<dyn Executor>,
    #[cfg(feature = "replica")]
    pub(crate) paged_driver: crate::paged_io::DriverSlot,
    #[cfg(feature = "replica")]
    io_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    io_capacity: usize,
    #[cfg(feature = "replica")]
    job_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    job_capacity: usize,
    #[cfg(feature = "replica")]
    recovery_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    recovery_capacity: usize,
    #[cfg(feature = "replica")]
    dirty_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    dirty_capacity: usize,
    #[cfg(feature = "replica")]
    scratch_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    scratch_capacity: u32,
    #[cfg(feature = "replica")]
    scratch_monitor: Arc<dyn ScratchMonitor>,
    #[cfg(feature = "replica")]
    directory_cache: Option<Arc<DirectoryCache>>,
    #[cfg(feature = "replica")]
    resource_admission: Option<Arc<dyn HostResourceAdmission>>,
    #[cfg(feature = "replica")]
    recovery: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    #[cfg(feature = "replica")]
    dirty: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    #[cfg(feature = "replica")]
    scratch: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    #[cfg(feature = "replica")]
    recovery_resource: Option<Arc<dyn HostResourcePermit>>,
    #[cfg(feature = "replica")]
    dirty_resource: Option<Arc<dyn HostResourcePermit>>,
    #[cfg(feature = "replica")]
    scratch_resource: Option<Arc<dyn HostResourcePermit>>,
}

#[cfg(feature = "replica")]
pub(crate) struct HostIoPermit {
    _semaphore: tokio::sync::OwnedSemaphorePermit,
    _resource: Option<Arc<dyn HostResourcePermit>>,
}

impl Host {
    /// Charges local-disk reservations to one embedding runtime ledger.
    pub fn install_disk_admission(
        &self,
        admission: Arc<dyn DiskBudgetAdmission>,
    ) -> crate::Result<()> {
        self.local_disk.install_admission(admission)
    }

    /// Verifies named local artifacts using this host's bounded filesystem reads.
    pub fn verify(
        &self,
        segments: &[crate::LocalSegment],
        target: crate::Position,
        limits: crate::Limits,
    ) -> crate::Result<crate::VerifiedLocalPlan> {
        crate::VerifiedLocalPlan::with_host(segments, target, limits, self)
    }

    /// Restores a verified cut through this host's atomic new-file installation.
    pub fn restore(
        &self,
        plan: &crate::VerifiedLocalPlan,
        destination: &Path,
    ) -> crate::Result<crate::Position> {
        crate::recovery::reject_sidecars(destination, self)?;
        self.filesystem.persist_new(destination, &plan.image()?)?;
        Ok(plan.position())
    }

    /// Installs a verified full-chain compaction through this host's filesystem.
    pub fn compact(
        &self,
        plan: &crate::VerifiedLocalPlan,
        destination: &Path,
    ) -> crate::Result<crate::LocalSegment> {
        let (bytes, info) = crate::recovery::compact_bytes(plan)?;
        self.filesystem.persist_new(destination, &bytes)?;
        Ok(crate::LocalSegment::new(destination.to_owned(), info))
    }

    /// Selects an already registered SQLite VFS for local and sparse databases.
    ///
    /// The host must keep the registration alive for the process lifetime and
    /// make its file namespace agree with `FileSystem`. Unknown names fail closed.
    #[must_use]
    pub fn with_sqlite_vfs(mut self, name: &str) -> Self {
        self.sqlite_vfs = Some(name.to_owned());
        self
    }

    /// Shares byte-precise admission across WAL, LTX, sparse pages and staging.
    #[must_use]
    pub fn with_local_disk_budget(mut self, budget: DiskBudget) -> Self {
        self.local_disk = budget;
        self
    }

    /// Enables the verified immutable directory-node cache below `root`.
    ///
    /// The cache is an acceleration layer only; directory reachability still
    /// reads canonical objects when collecting retention roots. Its byte bound
    /// is derived from one eighth of the shared local-disk envelope.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn with_directory_cache(mut self, root: PathBuf) -> Self {
        let capacity = (self.local_disk.capacity() / 8).clamp(1, 8 << 30);
        self.directory_cache = Some(Arc::new(DirectoryCache::with_budget(
            Arc::clone(&self.filesystem),
            root,
            capacity,
            self.local_disk.clone(),
        )));
        self
    }

    /// Installs one embedding runtime ledger for bounded replica-host work.
    #[cfg(feature = "replica")]
    pub fn install_resource_admission(&mut self, admission: Arc<dyn HostResourceAdmission>) {
        self.resource_admission = Some(admission);
    }

    /// Returns the currently configured object-store I/O capacity.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn io_capacity(&self) -> usize {
        self.io_capacity
    }

    /// Returns the currently configured blocking-job capacity.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn job_capacity(&self) -> usize {
        self.job_capacity
    }

    /// Returns the currently configured recovery-job capacity.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn recovery_capacity(&self) -> usize {
        self.recovery_capacity
    }

    /// Returns the currently configured dirty-memory capacity.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn dirty_capacity(&self) -> usize {
        self.dirty_capacity
    }

    /// Returns the currently configured scratch capacity in MiB units.
    #[cfg(feature = "replica")]
    #[must_use]
    pub const fn scratch_capacity(&self) -> u32 {
        self.scratch_capacity
    }

    /// Returns the configured byte ceiling shared by local replica artifacts.
    #[must_use]
    pub fn local_disk_capacity(&self) -> u64 {
        self.local_disk.capacity()
    }

    /// Returns bytes currently reserved by local replica artifacts.
    #[must_use]
    pub fn local_disk_used(&self) -> u64 {
        self.local_disk.used()
    }

    /// Returns verified directory-cache usage when the cache is enabled.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn directory_cache_stats(&self) -> Option<DirectoryCacheStats> {
        self.directory_cache.as_ref().map(|cache| cache.stats())
    }

    pub(crate) fn reserve_local_disk(&self, bytes: u64) -> crate::Result<DiskReservation> {
        self.local_disk.try_reserve(bytes)
    }

    pub(crate) fn read(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
        let host = crate::host::LtxHost {
            facilities: self.clone(),
            max_database_bytes: limit,
            max_file_bytes: limit,
        };
        host.read(path)
    }
    #[must_use]
    pub fn with_filesystem(mut self, filesystem: Arc<dyn FileSystem>) -> Self {
        self.filesystem = filesystem;
        self
    }
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn with_executor(mut self, executor: Arc<dyn Executor>) -> Self {
        self.executor = executor;
        self.paged_driver = Arc::new(std::sync::Mutex::new(std::sync::Weak::new()));
        self
    }

    /// Shares an object-store request ceiling across hosts and databases.
    ///
    /// Defaults share 32 permits process-wide. Closing the semaphore rejects
    /// new I/O; permits are held across provider retries and released on drop.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn with_io_slots(mut self, slots: Arc<tokio::sync::Semaphore>) -> Self {
        self.io_capacity = slots.available_permits();
        self.io_slots = slots;
        self
    }

    /// Shares a blocking-job ceiling, including jobs whose callers cancel.
    ///
    /// Defaults share up to 16 jobs process-wide, capped by available CPUs.
    /// Independent paged workers do not consume these slots, avoiding deadlock.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn with_job_slots(mut self, slots: Arc<tokio::sync::Semaphore>) -> Self {
        self.job_capacity = slots.available_permits();
        self.job_slots = slots;
        self
    }

    /// Bounds simultaneous full restore, resume, bundling and remote compaction.
    ///
    /// Defaults share two slots process-wide. Admission precedes body downloads
    /// and stays with non-cancellable jobs. This bounds cohorts, not process RSS;
    /// size per-database `Limits` and these slots to the service memory budget.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn with_recovery_slots(mut self, slots: Arc<tokio::sync::Semaphore>) -> Self {
        self.recovery_capacity = slots.available_permits();
        self.recovery_slots = slots;
        self
    }

    /// Shares memory admission for capture, recovery and compaction jobs.
    ///
    /// One permit represents the embedding service's fixed per-job dirty-memory
    /// reservation. The permit follows dispatched work after caller cancellation.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn with_dirty_slots(mut self, slots: Arc<tokio::sync::Semaphore>) -> Self {
        self.dirty_capacity = slots.available_permits();
        self.dirty_slots = slots;
        self
    }

    /// Shares temporary local-disk admission in one-MiB permit units.
    ///
    /// Configure an unused semaphore before cloning the host. Requests larger
    /// than its initial capacity fail instead of waiting forever.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn with_scratch_slots(mut self, slots: Arc<tokio::sync::Semaphore>) -> Self {
        self.scratch_capacity = u32::try_from(slots.available_permits()).unwrap_or(u32::MAX);
        self.scratch_slots = slots;
        self
    }

    /// Rechecks actual host capacity whenever a full scratch job is admitted.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn with_scratch_monitor(mut self, monitor: Arc<dyn ScratchMonitor>) -> Self {
        self.scratch_monitor = monitor;
        self
    }

    #[cfg(feature = "replica")]
    fn reserve_resource(
        &self,
        kind: HostResourceKind,
        units: u32,
    ) -> crate::Result<Option<Arc<dyn HostResourcePermit>>> {
        self.resource_admission
            .as_ref()
            .map(|admission| admission.reserve(kind, units).map(Arc::from))
            .transpose()
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn for_dirty(&self) -> crate::Result<Self> {
        let mut host = self.clone();
        if host.dirty.is_none() {
            let permit = self
                .dirty_slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| crate::CrabError::Other(Box::new(e)))?;
            host.dirty_resource = self.reserve_resource(HostResourceKind::Dirty, 1)?;
            host.dirty = Some(Arc::new(permit));
        }
        Ok(host)
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn for_recovery(&self) -> crate::Result<Self> {
        let mut host = self.for_dirty().await?;
        if host.recovery.is_none() {
            let permit = self
                .recovery_slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| crate::CrabError::Other(Box::new(e)))?;
            host.recovery_resource = self.reserve_resource(HostResourceKind::Recovery, 1)?;
            host.recovery = Some(Arc::new(permit));
        }
        Ok(host)
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn for_scratch(&self, bytes: u64) -> crate::Result<Self> {
        const MIB: u64 = 1 << 20;
        let units = bytes
            .checked_add(MIB - 1)
            .ok_or(crate::CrabError::Limit("scratch disk bytes"))?
            / MIB;
        let units =
            u32::try_from(units).map_err(|_| crate::CrabError::Limit("scratch disk bytes"))?;
        if units == 0 || units > self.scratch_capacity {
            return Err(crate::CrabError::Limit("scratch disk bytes"));
        }
        let mut host = self.clone();
        if let Some(permit) = &host.scratch {
            if permit.num_permits() < units as usize {
                return Err(crate::CrabError::Limit("scratch disk bytes"));
            }
            return Ok(host);
        }
        let permit = self
            .scratch_slots
            .clone()
            .acquire_many_owned(units)
            .await
            .map_err(|e| crate::CrabError::Other(Box::new(e)))?;
        let capacity = self.scratch_capacity as usize;
        let reserved_units = capacity.saturating_sub(self.scratch_slots.available_permits());
        let reserved_bytes = u64::try_from(reserved_units)
            .ok()
            .and_then(|units| units.checked_mul(MIB))
            .ok_or(crate::CrabError::Limit("scratch disk bytes"))?;
        self.scratch_monitor
            .ensure_available(reserved_bytes)
            .map_err(crate::CrabError::Io)?;
        host.scratch_resource = self.reserve_resource(HostResourceKind::Scratch, units)?;
        host.scratch = Some(Arc::new(permit));
        Ok(host)
    }

    #[cfg(feature = "replica")]
    pub(crate) fn without_recovery(mut self) -> Self {
        self.recovery = None;
        self.recovery_resource = None;
        self
    }

    #[cfg(feature = "replica")]
    pub(crate) fn without_dirty(mut self) -> Self {
        self.dirty = None;
        self.dirty_resource = None;
        self
    }

    #[cfg(feature = "replica")]
    pub(crate) fn without_scratch(mut self) -> Self {
        self.scratch = None;
        self.scratch_resource = None;
        self
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn io_permit(&self) -> crate::Result<HostIoPermit> {
        let permit = self
            .io_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| crate::CrabError::Other(Box::new(e)))?;
        let resource = self.reserve_resource(HostResourceKind::Io, 1)?;
        Ok(HostIoPermit {
            _semaphore: permit,
            _resource: resource,
        })
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn directory_cache_get(
        &self,
        key: String,
        max_bytes: u64,
    ) -> crate::Result<Option<Vec<u8>>> {
        let Some(cache) = &self.directory_cache else {
            return Ok(None);
        };
        let cache = Arc::clone(cache);
        self.run(move || cache.get(&key, max_bytes))
            .await?
            .map_err(crate::CrabError::Io)
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn directory_cache_put(
        &self,
        key: String,
        bytes: Vec<u8>,
        max_bytes: u64,
    ) -> crate::Result<()> {
        let Some(cache) = &self.directory_cache else {
            return Ok(());
        };
        let cache = Arc::clone(cache);
        self.run(move || cache.put(&key, &bytes, max_bytes))
            .await?
            .map_err(crate::CrabError::Io)
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn directory_cache_invalidate(&self, key: String) -> crate::Result<()> {
        let Some(cache) = &self.directory_cache else {
            return Ok(());
        };
        let cache = Arc::clone(cache);
        self.run(move || cache.invalidate(&key))
            .await?
            .map_err(crate::CrabError::Io)
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> T + Send + 'static,
    ) -> crate::Result<T> {
        let permit = self
            .job_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| crate::CrabError::Other(Box::new(e)))?;
        let resource = self.reserve_resource(HostResourceKind::BlockingJob, 1)?;
        let (send, receive) = tokio::sync::oneshot::channel();
        let recovery = self.recovery.clone();
        let dirty = self.dirty.clone();
        let scratch = self.scratch.clone();
        self.executor.dispatch(Box::new(move || {
            // Dispatched work can outlive its future. Keep admission with the
            // job, not the waiter, so cancellation cannot oversubscribe the pool.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
                .map_err(|_| crate::CrabError::InvalidState("host job panicked"));
            // Result delivery is the operation's completion boundary. Release
            // admission first so returned long-lived handles cannot appear to
            // retain capacity while this closure is still being torn down.
            drop(recovery);
            drop(dirty);
            drop(scratch);
            drop(resource);
            drop(permit);
            let _ = send.send(result);
        }))?;
        receive
            .await
            .map_err(|error| crate::CrabError::Other(Box::new(error)))?
    }
}

impl Default for Host {
    fn default() -> Self {
        #[cfg(feature = "replica")]
        static IO: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
        #[cfg(feature = "replica")]
        static JOBS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
        #[cfg(feature = "replica")]
        static RECOVERY: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        #[cfg(feature = "replica")]
        static DIRTY: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
        #[cfg(feature = "replica")]
        static SCRATCH: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        static LOCAL_DISK: std::sync::OnceLock<DiskBudget> = std::sync::OnceLock::new();
        Self {
            filesystem: Arc::new(DirectFileSystem),
            clock: Arc::new(SystemClock),
            sqlite_vfs: None,
            local_disk: LOCAL_DISK
                .get_or_init(|| DiskBudget::new(64 * 1024 * 1024 * 1024))
                .clone(),
            #[cfg(feature = "replica")]
            executor: Arc::new(TokioExecutor),
            #[cfg(feature = "replica")]
            paged_driver: crate::paged_io::default_slot(),
            #[cfg(feature = "replica")]
            io_slots: IO
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(32)))
                .clone(),
            #[cfg(feature = "replica")]
            io_capacity: 32,
            #[cfg(feature = "replica")]
            job_slots: JOBS
                .get_or_init(|| {
                    Arc::new(tokio::sync::Semaphore::new(
                        std::thread::available_parallelism().map_or(1, |n| n.get().min(16)),
                    ))
                })
                .clone(),
            #[cfg(feature = "replica")]
            job_capacity: std::thread::available_parallelism().map_or(1, |n| n.get().min(16)),
            #[cfg(feature = "replica")]
            recovery_slots: RECOVERY
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
                .clone(),
            #[cfg(feature = "replica")]
            recovery_capacity: 2,
            #[cfg(feature = "replica")]
            dirty_slots: DIRTY
                .get_or_init(|| {
                    Arc::new(tokio::sync::Semaphore::new(
                        std::thread::available_parallelism().map_or(1, |n| n.get().min(16)),
                    ))
                })
                .clone(),
            #[cfg(feature = "replica")]
            dirty_capacity: std::thread::available_parallelism().map_or(1, |n| n.get().min(16)),
            #[cfg(feature = "replica")]
            scratch_slots: SCRATCH
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(64 * 1024)))
                .clone(),
            #[cfg(feature = "replica")]
            scratch_capacity: 64 * 1024,
            #[cfg(feature = "replica")]
            scratch_monitor: Arc::new(UnlimitedScratch),
            #[cfg(feature = "replica")]
            directory_cache: None,
            #[cfg(feature = "replica")]
            resource_admission: None,
            #[cfg(feature = "replica")]
            recovery: None,
            #[cfg(feature = "replica")]
            dirty: None,
            #[cfg(feature = "replica")]
            scratch: None,
            #[cfg(feature = "replica")]
            recovery_resource: None,
            #[cfg(feature = "replica")]
            dirty_resource: None,
            #[cfg(feature = "replica")]
            scratch_resource: None,
        }
    }
}

/// Standard local filesystem with exclusive private artifact creation.
#[derive(Default)]
pub struct DirectFileSystem;

impl FileIo for std::fs::File {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        io::Write::write_all(self, bytes)
    }
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        io::Seek::seek(self, io::SeekFrom::Start(offset))?;
        io::Write::write_all(self, bytes)
    }
    fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        io::Seek::seek(self, io::SeekFrom::Start(offset))?;
        let mut bytes = vec![0; len];
        io::Read::read_exact(self, &mut bytes)?;
        Ok(bytes)
    }
    fn sync_all(&mut self) -> io::Result<()> {
        std::fs::File::sync_all(self)
    }
    fn file_len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        std::fs::File::set_len(self, len)
    }
}

impl FileSystem for DirectFileSystem {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }
    fn open_rw(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        Ok(Box::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?,
        ))
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(Box::new(options.open(path)?))
    }
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.len())
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)?;
        crate::host::sync_parent(to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        path.canonicalize()
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }
    fn sync_parent(&self, path: &Path) -> io::Result<()> {
        crate::host::sync_parent(path)
    }
    fn persist_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        io::Write::write_all(&mut file, bytes)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(path).map_err(|error| error.error)?;
        self.sync_parent(path)
    }
    fn persist_file_new(&self, source: &Path, destination: &Path) -> io::Result<()> {
        if source.parent() != destination.parent() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scratch and destination must share a directory",
            ));
        }
        std::fs::hard_link(source, destination)?;
        self.sync_parent(destination)?;
        std::fs::remove_file(source)?;
        self.sync_parent(destination)
    }

    fn cleanup_private_temporaries(&self, root: &Path) -> io::Result<()> {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if file_type.is_file() && (name.starts_with(".tmp-") || name.starts_with(".index-tmp-"))
            {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
}

/// Operating-system wall clock; future mtimes have age zero.
#[derive(Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_millis(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
    fn file_age(&self, path: &Path) -> io::Result<Duration> {
        Ok(SystemTime::now()
            .duration_since(std::fs::metadata(path)?.modified()?)
            .unwrap_or_default())
    }
}

#[cfg(feature = "replica")]
struct TokioExecutor;
#[cfg(feature = "replica")]
impl Executor for TokioExecutor {
    fn dispatch(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<()> {
        tokio::runtime::Handle::try_current()
            .map_err(io::Error::other)?
            .spawn_blocking(job);
        Ok(())
    }
    fn start_worker(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<Box<dyn Worker>> {
        Ok(Box::new(
            std::thread::Builder::new()
                .name("crab-ltx-paged".into())
                .spawn(job)?,
        ))
    }
}

#[cfg(feature = "replica")]
impl Worker for std::thread::JoinHandle<()> {
    fn join(self: Box<Self>) -> io::Result<()> {
        (*self)
            .join()
            .map_err(|_| io::Error::other("host worker panicked"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "replica")]
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn disk_budget_reservations_resize_and_release_exact_bytes() {
        let budget = DiskBudget::new(10);
        let first = budget.try_reserve(4).unwrap();
        let second = budget.try_reserve(6).unwrap();
        assert_eq!(budget.available(), 0);
        assert!(matches!(
            budget.try_reserve(1),
            Err(crate::CrabError::Limit("local disk bytes"))
        ));

        first.resize(2).unwrap();
        assert_eq!(budget.available(), 2);
        drop(second);
        assert_eq!(budget.available(), 8);
        drop(first);
        assert_eq!(budget.available(), 10);
    }

    #[cfg(feature = "replica")]
    #[test]
    fn directory_cache_survives_restart_and_evicts_by_bytes() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path().join("directory-cache");
        let filesystem: Arc<dyn FileSystem> = Arc::new(DirectFileSystem);
        let cache = DirectoryCache::new(Arc::clone(&filesystem), root.clone(), 5);
        cache.put("first", b"1234", 64).unwrap();
        assert_eq!(cache.budget.used(), 4);
        drop(cache);

        let cache = DirectoryCache::new(Arc::clone(&filesystem), root, 5);
        assert_eq!(cache.get("first", 64).unwrap(), Some(b"1234".to_vec()));
        assert_eq!(cache.stats().entries(), 1);
        assert_eq!(cache.stats().bytes(), 4);
        cache.put("second", b"abcde", 64).unwrap();
        assert!(cache.get("first", 64).unwrap().is_none());
        assert_eq!(cache.get("second", 64).unwrap(), Some(b"abcde".to_vec()));
        assert_eq!(cache.stats().entries(), 1);
        assert_eq!(cache.budget.used(), 5);
    }

    #[cfg(feature = "replica")]
    #[test]
    fn directory_cache_discards_truncated_and_symlink_entries() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path().join("directory-cache");
        let filesystem: Arc<dyn FileSystem> = Arc::new(DirectFileSystem);
        let cache = DirectoryCache::new(Arc::clone(&filesystem), root.clone(), 64);
        cache.put("entry", b"verified", 64).unwrap();
        let path = cache.key_path("entry");
        std::fs::write(&path, b"short").unwrap();
        assert!(cache.get("entry", 64).unwrap().is_none());

        let target = directory.path().join("outside");
        std::fs::write(&target, b"outside").unwrap();
        let symlink = cache.key_path("symlink");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &symlink).unwrap();
        #[cfg(unix)]
        assert!(cache.get("symlink", 64).unwrap().is_none());
    }

    #[cfg(feature = "replica")]
    #[test]
    fn directory_cache_cleans_abandoned_private_temporaries_on_restart() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path().join("directory-cache");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".tmp-old"), b"partial").unwrap();
        std::fs::write(root.join(".index-tmp-old"), b"partial").unwrap();
        std::fs::write(root.join("unrelated"), b"keep").unwrap();
        let filesystem: Arc<dyn FileSystem> = Arc::new(DirectFileSystem);
        let _cache = DirectoryCache::new(filesystem, root.clone(), 64);
        assert!(!root.join(".tmp-old").exists());
        assert!(!root.join(".index-tmp-old").exists());
        assert!(root.join("unrelated").exists());
    }

    #[cfg(feature = "replica")]
    #[test]
    fn directory_cache_serializes_concurrent_fills_for_one_key() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path().join("directory-cache");
        let filesystem: Arc<dyn FileSystem> = Arc::new(DirectFileSystem);
        let cache = Arc::new(DirectoryCache::new(Arc::clone(&filesystem), root, 64));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = Arc::clone(&cache);
                scope.spawn(move || {
                    cache.put("same-key", b"verified", 64).unwrap();
                    assert_eq!(
                        cache.get("same-key", 64).unwrap(),
                        Some(b"verified".to_vec())
                    );
                });
            }
        });
        assert_eq!(cache.stats().entries(), 1);
        assert_eq!(cache.stats().bytes(), 8);
        assert_eq!(cache.budget.used(), 8);
    }

    struct TestClock;
    impl Clock for TestClock {
        fn unix_millis(&self) -> i64 {
            123456789
        }
        fn file_age(&self, _: &Path) -> io::Result<Duration> {
            Ok(Duration::ZERO)
        }
    }

    struct FaultFs(AtomicBool);
    impl FileSystem for FaultFs {
        fn open(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
            DirectFileSystem.open(path)
        }
        fn open_rw(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
            DirectFileSystem.open_rw(path)
        }
        fn create(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
            if self.0.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "injected artifact failure",
                ));
            }
            DirectFileSystem.create(path)
        }
        fn file_len(&self, path: &Path) -> io::Result<u64> {
            DirectFileSystem.file_len(path)
        }
        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            DirectFileSystem.create_dir_all(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            DirectFileSystem.rename(from, to)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            DirectFileSystem.remove_file(path)
        }
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            DirectFileSystem.canonicalize(path)
        }
        fn exists(&self, path: &Path) -> io::Result<bool> {
            DirectFileSystem.exists(path)
        }
        fn create_dir(&self, path: &Path) -> io::Result<()> {
            DirectFileSystem.create_dir(path)
        }
        fn sync_parent(&self, path: &Path) -> io::Result<()> {
            DirectFileSystem.sync_parent(path)
        }
        fn persist_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            DirectFileSystem.persist_new(path, bytes)
        }
        fn persist_file_new(&self, source: &Path, destination: &Path) -> io::Result<()> {
            DirectFileSystem.persist_file_new(source, destination)
        }
    }

    #[test]
    fn injected_clock_and_capture_filesystem_reach_real_sqlite_transactions() {
        let directory = tempfile::TempDir::new().unwrap();
        let filesystem = Arc::new(FaultFs(AtomicBool::new(false)));
        let host = Host::default()
            .with_clock(Arc::new(TestClock))
            .with_filesystem(filesystem.clone());
        let mut db = crate::ManagedDb::open_with_host(
            &directory.path().join("db.sqlite"),
            crate::Limits::default(),
            host,
        )
        .unwrap();
        db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(1)"))
            .unwrap();
        let batch = db.capture().unwrap();
        let bytes = std::fs::read(batch.segments[0].path()).unwrap();
        assert_eq!(
            crate::ltx::Header::parse(&bytes).unwrap().timestamp,
            123456789
        );
        db.transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
            .unwrap();
        filesystem.0.store(true, Ordering::SeqCst);
        assert!(
            matches!(db.capture(), Err(crate::CrabError::Io(error)) if error.kind() == io::ErrorKind::StorageFull)
        );
        assert!(matches!(
            db.transaction(|_| Ok(())),
            Err(crate::CrabError::Fenced)
        ));
    }

    #[test]
    fn synced_scratch_install_never_replaces_a_destination() {
        let directory = tempfile::TempDir::new().unwrap();
        let destination = directory.path().join("database.sqlite");
        let first = directory.path().join("first.scratch");
        let mut file = DirectFileSystem.create(&first).unwrap();
        file.write_all(b"first").unwrap();
        file.sync_all().unwrap();
        drop(file);
        DirectFileSystem
            .persist_file_new(&first, &destination)
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");
        assert!(!first.exists());

        let second = directory.path().join("second.scratch");
        let mut file = DirectFileSystem.create(&second).unwrap();
        file.write_all(b"second").unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert_eq!(
            DirectFileSystem
                .persist_file_new(&second, &destination)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(destination).unwrap(), b"first");
        assert_eq!(std::fs::read(second).unwrap(), b"second");
    }

    #[cfg(feature = "replica")]
    #[tokio::test]
    async fn dropped_executor_jobs_return_errors_without_hanging() {
        struct DroppingExecutor;
        impl Executor for DroppingExecutor {
            fn dispatch(&self, _: Box<dyn FnOnce() + Send>) -> io::Result<()> {
                Ok(())
            }
            fn start_worker(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<Box<dyn Worker>> {
                TokioExecutor.start_worker(job)
            }
        }
        let host = Host::default().with_executor(Arc::new(DroppingExecutor));
        assert!(host.run(|| 42).await.is_err());
        assert_eq!(Host::default().run(|| 42).await.unwrap(), 42);
    }

    #[cfg(feature = "replica")]
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_waiters_do_not_release_running_job_or_recovery_admission() {
        let jobs = Arc::new(tokio::sync::Semaphore::new(1));
        let recovery = Arc::new(tokio::sync::Semaphore::new(1));
        let dirty = Arc::new(tokio::sync::Semaphore::new(1));
        let scratch = Arc::new(tokio::sync::Semaphore::new(1));
        let host = Host::default()
            .with_job_slots(jobs.clone())
            .with_recovery_slots(recovery.clone())
            .with_dirty_slots(dirty.clone())
            .with_scratch_slots(scratch.clone());
        let scope = host
            .for_recovery()
            .await
            .unwrap()
            .for_scratch(1 << 20)
            .await
            .unwrap();
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            scope
                .run(move || {
                    let _ = started.send(());
                    let _ = blocked.recv();
                })
                .await
        });
        entered.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(jobs.available_permits(), 0);
        assert_eq!(recovery.available_permits(), 0);
        assert_eq!(dirty.available_permits(), 0);
        assert_eq!(scratch.available_permits(), 0);
        release.send(()).unwrap();
        let _job = tokio::time::timeout(Duration::from_secs(2), jobs.acquire())
            .await
            .unwrap()
            .unwrap();
        let _recovery = tokio::time::timeout(Duration::from_secs(2), recovery.acquire())
            .await
            .unwrap()
            .unwrap();
        let _dirty = tokio::time::timeout(Duration::from_secs(2), dirty.acquire())
            .await
            .unwrap()
            .unwrap();
        let _scratch = tokio::time::timeout(Duration::from_secs(2), scratch.acquire())
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(feature = "replica")]
    #[tokio::test]
    async fn closed_admission_returns_errors_instead_of_panicking() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        slots.close();
        let host = Host::default()
            .with_io_slots(slots.clone())
            .with_job_slots(slots.clone())
            .with_recovery_slots(slots.clone())
            .with_scratch_slots(slots);
        assert!(host.run(|| 1).await.is_err());
        assert!(host.io_permit().await.is_err());
        assert!(host.for_recovery().await.is_err());
        assert!(host.for_scratch(1 << 20).await.is_err());
    }

    #[cfg(feature = "replica")]
    #[tokio::test]
    async fn scratch_monitor_rechecks_total_reservation_and_releases_rejection() {
        struct RecordingScratch {
            bytes: AtomicU64,
            reject: AtomicBool,
        }
        impl ScratchMonitor for RecordingScratch {
            fn ensure_available(&self, reserved_bytes: u64) -> io::Result<()> {
                self.bytes.store(reserved_bytes, Ordering::Release);
                if self.reject.load(Ordering::Acquire) {
                    return Err(io::Error::from(io::ErrorKind::StorageFull));
                }
                Ok(())
            }
        }

        let slots = Arc::new(tokio::sync::Semaphore::new(3));
        let monitor = Arc::new(RecordingScratch {
            bytes: AtomicU64::new(0),
            reject: AtomicBool::new(false),
        });
        let host = Host::default()
            .with_scratch_slots(slots.clone())
            .with_scratch_monitor(monitor.clone());
        let first = host.for_scratch(1 << 20).await.unwrap();
        assert_eq!(monitor.bytes.load(Ordering::Acquire), 1 << 20);
        let second = host.for_scratch(2 << 20).await.unwrap();
        assert_eq!(monitor.bytes.load(Ordering::Acquire), 3 << 20);
        drop((first, second));

        monitor.reject.store(true, Ordering::Release);

        assert!(matches!(
            host.for_scratch(1 << 20).await,
            Err(crate::CrabError::Io(error)) if error.kind() == io::ErrorKind::StorageFull
        ));
        assert_eq!(monitor.bytes.load(Ordering::Acquire), 1 << 20);
        assert_eq!(slots.available_permits(), 3);
    }
}
