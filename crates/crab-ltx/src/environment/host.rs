//! Disk admission, filesystem, clock, and host contracts.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "replica")]
use crate::environment::directory_cache::{DirectoryCache, DirectoryCacheStats};
#[cfg(feature = "replica")]
use crate::environment::executor::{Executor, TokioExecutor};
#[cfg(feature = "replica")]
use crate::environment::resources::{HostResourceAdmission, HostResourceKind, HostResourcePermit};
#[cfg(feature = "replica")]
use crate::environment::telemetry::{LtxPhase, LtxReadOrigin, LtxRequestOutcome, LtxTelemetry};
#[cfg(feature = "replica")]
use crate::environment::telemetry::{ScratchMonitor, UnlimitedScratch};

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

pub(crate) type DiskAdmissions = Vec<Arc<dyn DiskBudgetAdmission>>;

/// Shared byte-precise admission for local files owned by active database work.
#[derive(Clone)]
pub struct DiskBudget {
    inner: Arc<DiskBudgetInner>,
}

pub(crate) struct DiskBudgetInner {
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
            .map_err(|_| crate::CrabError::Limit(crate::LimitKind::LocalDiskBytes))
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
            .ok_or(crate::CrabError::Limit(crate::LimitKind::LocalDiskBytes))?;
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
/// `FileSystem::open_rw` supports both operations. An open handle remains bound
/// to the selected artifact even if its namespace path is later replaced.
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
/// `rename` must sync the destination parent before succeeding. The opt-in
/// `rename_uncommitted` variant may install a file before its contents or name
/// are durable; the caller must sync the file and then its parent directory.
/// Implementations must preserve underlying I/O errors.
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

    /// Atomically renames a file without requiring the destination directory
    /// to be durable yet. The default preserves the synchronous `rename`
    /// contract for host filesystems that do not support batching.
    fn rename_uncommitted(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn exists(&self, path: &Path) -> io::Result<bool>;
    fn create_dir(&self, path: &Path) -> io::Result<()>;

    /// Syncs every named file before a shared directory barrier.
    ///
    /// Hosts may coalesce or parallelize these independent flushes. Success
    /// must still mean that every file's contents are durable.
    fn sync_files(&self, paths: &[PathBuf]) -> io::Result<()> {
        for path in paths {
            self.open_rw(path)?.sync_all()?;
        }
        Ok(())
    }

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

/// Wall-clock observations used in LTX timestamps and checkpoint eligibility.
pub trait Clock: Send + Sync {
    fn unix_millis(&self) -> i64;
    fn file_age(&self, path: &Path) -> io::Result<Duration>;

    /// Returns a monotonic instant for observational duration measurements.
    ///
    /// Implementors that only provide wall-clock behavior can keep this
    /// default; tests may override it with a deterministic clock.
    fn monotonic(&self) -> Instant {
        Instant::now()
    }
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
    telemetry: Option<Arc<dyn LtxTelemetry>>,
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
    ) -> crate::Result<crate::VerifiedPlan> {
        crate::VerifiedPlan::with_host(segments, target, limits, self)
    }

    /// Restores a verified cut through this host's atomic new-file installation.
    pub fn restore(
        &self,
        plan: &crate::VerifiedPlan,
        destination: &Path,
    ) -> crate::Result<crate::Position> {
        self.restore_materialized(plan.materialize(), destination)
    }

    pub(crate) fn restore_materialized(
        &self,
        materialized: &crate::recovery::MaterializedPlan,
        destination: &Path,
    ) -> crate::Result<crate::Position> {
        crate::recovery::reject_sidecars(destination, self)?;
        self.filesystem
            .persist_new(destination, &materialized.image)?;
        Ok(materialized.position)
    }

    /// Installs a verified full-chain compaction through this host's filesystem.
    pub fn compact(
        &self,
        plan: &crate::VerifiedPlan,
        destination: &Path,
    ) -> crate::Result<crate::LocalSegment> {
        crate::recovery::compact_to_file(self, plan, destination)
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

    /// Sends finite replica observations to one embedding runtime.
    #[cfg(feature = "replica")]
    #[must_use]
    pub fn with_ltx_telemetry(mut self, telemetry: Arc<dyn LtxTelemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    #[cfg(feature = "replica")]
    pub(crate) fn observe_ltx_phase(&self, phase: LtxPhase, started: Instant, succeeded: bool) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.phase(
                phase,
                self.now_monotonic().saturating_duration_since(started),
                succeeded,
            );
        }
    }

    #[cfg(feature = "replica")]
    pub(crate) fn observe_ltx_logical_read(&self, origin: LtxReadOrigin) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.logical_read(origin);
        }
    }

    #[cfg(feature = "replica")]
    pub(crate) fn observe_ltx_origin_request(
        &self,
        origin: LtxReadOrigin,
        succeeded: bool,
        bytes: usize,
    ) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.origin_request(
                origin,
                if succeeded {
                    LtxRequestOutcome::Succeeded
                } else {
                    LtxRequestOutcome::Failed
                },
                bytes as u64,
            );
        }
    }

    #[cfg(feature = "replica")]
    pub(crate) fn observe_ltx_capture(&self, timing: &crate::CaptureTiming, succeeded: bool) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.capture(timing, succeeded);
        }
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

    /// Returns the shared local-disk budget used by replica artifacts.
    #[must_use]
    pub fn local_disk_budget(&self) -> DiskBudget {
        self.local_disk.clone()
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

    pub(crate) fn now_monotonic(&self) -> Instant {
        self.clock.monotonic()
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
            .ok_or(crate::CrabError::Limit(crate::LimitKind::ScratchDiskBytes))?
            / MIB;
        let units = u32::try_from(units)
            .map_err(|_| crate::CrabError::Limit(crate::LimitKind::ScratchDiskBytes))?;
        if units == 0 || units > self.scratch_capacity {
            return Err(crate::CrabError::Limit(crate::LimitKind::ScratchDiskBytes));
        }
        let mut host = self.clone();
        if let Some(permit) = &host.scratch {
            if permit.num_permits() < units as usize {
                return Err(crate::CrabError::Limit(crate::LimitKind::ScratchDiskBytes));
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
            .ok_or(crate::CrabError::Limit(crate::LimitKind::ScratchDiskBytes))?;
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
            telemetry: None,
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
    fn rename_uncommitted(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
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
    fn sync_files(&self, paths: &[PathBuf]) -> io::Result<()> {
        let workers = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(paths.len());
        if workers <= 1 {
            for path in paths {
                self.open_rw(path)?.sync_all()?;
            }
            return Ok(());
        }
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let handles = (0..workers)
                .map(|_| {
                    let next = &next;
                    scope.spawn(move || {
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(path) = paths.get(index) else {
                                break;
                            };
                            std::fs::OpenOptions::new()
                                .read(true)
                                .write(true)
                                .open(path)?
                                .sync_all()?;
                        }
                        Ok::<(), io::Error>(())
                    })
                })
                .collect::<Vec<_>>();
            let mut first_error = None;
            for handle in handles {
                let result = handle
                    .join()
                    .map_err(|_| io::Error::other("file sync worker panicked"))
                    .and_then(|result| result);
                if let Err(error) = result
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
            Ok(())
        })
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
        // Both names share one directory, so one barrier after link + unlink
        // makes the no-clobber install and scratch cleanup durable together.
        // A barrier error is ambiguous because the destination may now exist.
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
