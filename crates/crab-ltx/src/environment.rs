//! Injectable local I/O, clocks and jobs adapted from Celld host.rs.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
    #[cfg(feature = "replica")]
    pub(crate) executor: Arc<dyn Executor>,
    #[cfg(feature = "replica")]
    pub(crate) paged_driver: crate::paged_io::DriverSlot,
    #[cfg(feature = "replica")]
    io_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    job_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    recovery_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    dirty_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    scratch_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(feature = "replica")]
    scratch_capacity: u32,
    #[cfg(feature = "replica")]
    recovery: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    #[cfg(feature = "replica")]
    dirty: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    #[cfg(feature = "replica")]
    scratch: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
}

impl Host {
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

    #[cfg(feature = "replica")]
    pub(crate) async fn for_dirty(&self) -> crate::Result<Self> {
        let mut host = self.clone();
        if host.dirty.is_none() {
            host.dirty = Some(Arc::new(
                self.dirty_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| crate::CrabError::Other(Box::new(e)))?,
            ));
        }
        Ok(host)
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn for_recovery(&self) -> crate::Result<Self> {
        let mut host = self.for_dirty().await?;
        if host.recovery.is_none() {
            host.recovery = Some(Arc::new(
                self.recovery_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| crate::CrabError::Other(Box::new(e)))?,
            ));
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
        host.scratch = Some(Arc::new(
            self.scratch_slots
                .clone()
                .acquire_many_owned(units)
                .await
                .map_err(|e| crate::CrabError::Other(Box::new(e)))?,
        ));
        Ok(host)
    }

    #[cfg(feature = "replica")]
    pub(crate) fn without_recovery(mut self) -> Self {
        self.recovery = None;
        self
    }

    #[cfg(feature = "replica")]
    pub(crate) fn without_dirty(mut self) -> Self {
        self.dirty = None;
        self
    }

    #[cfg(feature = "replica")]
    pub(crate) fn without_scratch(mut self) -> Self {
        self.scratch = None;
        self
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn io_permit(&self) -> crate::Result<tokio::sync::OwnedSemaphorePermit> {
        self.io_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| crate::CrabError::Other(Box::new(e)))
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
        Self {
            filesystem: Arc::new(DirectFileSystem),
            clock: Arc::new(SystemClock),
            sqlite_vfs: None,
            #[cfg(feature = "replica")]
            executor: Arc::new(TokioExecutor),
            #[cfg(feature = "replica")]
            paged_driver: crate::paged_io::default_slot(),
            #[cfg(feature = "replica")]
            io_slots: IO
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(32)))
                .clone(),
            #[cfg(feature = "replica")]
            job_slots: JOBS
                .get_or_init(|| {
                    Arc::new(tokio::sync::Semaphore::new(
                        std::thread::available_parallelism().map_or(1, |n| n.get().min(16)),
                    ))
                })
                .clone(),
            #[cfg(feature = "replica")]
            recovery_slots: RECOVERY
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
                .clone(),
            #[cfg(feature = "replica")]
            dirty_slots: DIRTY
                .get_or_init(|| {
                    Arc::new(tokio::sync::Semaphore::new(
                        std::thread::available_parallelism().map_or(1, |n| n.get().min(16)),
                    ))
                })
                .clone(),
            #[cfg(feature = "replica")]
            scratch_slots: SCRATCH
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(64 * 1024)))
                .clone(),
            #[cfg(feature = "replica")]
            scratch_capacity: 64 * 1024,
            #[cfg(feature = "replica")]
            recovery: None,
            #[cfg(feature = "replica")]
            dirty: None,
            #[cfg(feature = "replica")]
            scratch: None,
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
    use std::sync::atomic::{AtomicBool, Ordering};

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
}
