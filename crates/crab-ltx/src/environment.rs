//! Injectable local I/O, clocks and jobs adapted from Celld host.rs.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// An open local artifact/WAL handle supplied by a host filesystem.
pub trait FileIo: Send {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    fn sync_all(&mut self) -> io::Result<()>;
    fn file_len(&self) -> io::Result<u64>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
}

/// Local filesystem boundary; SQLite pager I/O remains under its selected VFS.
///
/// `create` must exclusively create a new file. `rename` must sync the destination
/// parent before succeeding. Implementations must preserve underlying I/O errors.
/// `exists` must detect dangling symlinks. `create_dir` is an exclusive claim.
/// `persist_new` atomically installs fully synced bytes without replacing any
/// destination and syncs its parent; an error after installation is ambiguous.
pub trait FileSystem: Send + Sync {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FileIo>>;
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
        self
    }

    #[cfg(feature = "replica")]
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> T + Send + 'static,
    ) -> crate::Result<T> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.executor.dispatch(Box::new(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
                .map_err(|_| crate::CrabError::InvalidState("host job panicked"));
            let _ = send.send(result);
        }))?;
        receive
            .await
            .map_err(|error| crate::CrabError::Other(Box::new(error)))?
    }
}

impl Default for Host {
    fn default() -> Self {
        Self {
            filesystem: Arc::new(DirectFileSystem),
            clock: Arc::new(SystemClock),
            sqlite_vfs: None,
            #[cfg(feature = "replica")]
            executor: Arc::new(TokioExecutor),
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
}
