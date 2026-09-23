//! Injectable local I/O, clocks and jobs adapted from Celld host.rs.
//!
//! Object-store-only resources, telemetry, directory cache, and executors are
//! gated at the module boundary instead of per item.

#[cfg(feature = "replica")]
pub mod directory_cache;
#[cfg(feature = "replica")]
pub mod executor;
pub mod host;
#[cfg(feature = "replica")]
pub mod resources;
#[cfg(feature = "replica")]
pub mod telemetry;

#[cfg(feature = "replica")]
pub use directory_cache::DirectoryCacheStats;
#[cfg(feature = "replica")]
pub use executor::{Executor, Worker};
pub use host::{
    Clock, DirectFileSystem, DiskBudget, DiskBudgetAdmission, DiskReservation, FileIo, FileSystem,
    Host, SystemClock,
};
#[cfg(feature = "replica")]
pub use resources::{HostResourceAdmission, HostResourceKind, HostResourcePermit};
#[cfg(feature = "replica")]
pub use telemetry::{LtxPhase, LtxReadOrigin, LtxRequestOutcome, LtxTelemetry, ScratchMonitor};

#[cfg(test)]
mod tests {
    #[cfg(feature = "replica")]
    use std::sync::atomic::AtomicU64;

    use std::{
        io,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use super::*;
    #[cfg(feature = "replica")]
    use crate::environment::directory_cache::DirectoryCache;
    #[cfg(feature = "replica")]
    use crate::environment::executor::TokioExecutor;

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
        let mut db = crate::Db::open_with_host(
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
