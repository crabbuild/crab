#[cfg(feature = "replica")]
use crab_ltx::environment::{Executor, Worker};
#[cfg(feature = "replica")]
use crab_ltx::{CellReplica, CellStorageLayout, LocalSegment};
use crab_ltx::{
    CheckpointMode, CrabError, Db, Host, Limits,
    environment::{DirectFileSystem, FileIo, FileSystem},
};
#[cfg(feature = "replica")]
use crab_storage::Store;
#[cfg(feature = "replica")]
use object_store::throttle::{ThrottleConfig, ThrottledStore};
#[cfg(feature = "replica")]
use object_store::{memory::InMemory, path::Path as ObjectPath};
#[cfg(feature = "replica")]
use std::time::Duration;
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[cfg(feature = "replica")]
struct DelayedExecutor {
    delay: Duration,
}

#[cfg(feature = "replica")]
struct TestWorker(std::thread::JoinHandle<()>);

#[cfg(feature = "replica")]
impl Worker for TestWorker {
    fn join(self: Box<Self>) -> io::Result<()> {
        self.0
            .join()
            .map_err(|_| io::Error::other("test worker panicked"))
    }
}

#[cfg(feature = "replica")]
impl Executor for DelayedExecutor {
    fn dispatch(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<()> {
        let delay = self.delay;
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tokio::task::spawn_blocking(job).await;
        });
        Ok(())
    }

    fn start_worker(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<Box<dyn Worker>> {
        Ok(Box::new(TestWorker(std::thread::spawn(job))))
    }
}

#[derive(Clone, Default)]
struct Faults {
    failure: Arc<Mutex<Option<&'static str>>>,
    planned: Arc<Mutex<Vec<&'static str>>>,
    calls: Arc<Mutex<BTreeSet<&'static str>>>,
    largest_read: Arc<AtomicUsize>,
    read_calls: Arc<AtomicUsize>,
    largest_write: Arc<AtomicUsize>,
    write_calls: Arc<AtomicUsize>,
    file_syncs: Arc<AtomicUsize>,
    parent_syncs: Arc<AtomicUsize>,
    track_all: Arc<AtomicBool>,
    forbidden_thread: Arc<Mutex<Option<std::thread::ThreadId>>>,
    pause: Arc<Mutex<Option<Arc<activation::Pause>>>>,
}

impl Faults {
    fn arm(&self, operation: Option<&'static str>) {
        *self.failure.lock().unwrap() = operation;
    }

    /// Arms an ordered list of operations to fail, one injection per match.
    ///
    /// A plan models a sequence of failures across seams — a torn write, then a
    /// failed rename — instead of one armed operation at a time.
    fn plan(&self, operations: impl IntoIterator<Item = &'static str>) {
        *self.planned.lock().unwrap() = operations.into_iter().collect();
    }

    fn check(&self, operation: &'static str) -> io::Result<()> {
        self.calls.lock().unwrap().insert(operation);
        if *self.forbidden_thread.lock().unwrap() == Some(std::thread::current().id()) {
            return Err(io::Error::other("filesystem work on async thread"));
        }
        let pause = self.pause.lock().unwrap().clone();
        if let Some(pause) = pause {
            pause.wait(operation);
        }
        if *self.failure.lock().unwrap() == Some(operation) {
            return Err(io::Error::new(io::ErrorKind::StorageFull, operation));
        }
        let mut planned = self.planned.lock().unwrap();
        if planned.first() == Some(&operation) {
            planned.remove(0);
            return Err(io::Error::new(io::ErrorKind::StorageFull, operation));
        }
        Ok(())
    }
}

struct File {
    inner: Box<dyn FileIo>,
    faults: Faults,
    track: bool,
}

impl FileIo for File {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.track {
            self.faults
                .largest_write
                .fetch_max(bytes.len(), Ordering::Relaxed);
            self.faults.write_calls.fetch_add(1, Ordering::Relaxed);
        }
        if let Err(error) = self.faults.check("write_all") {
            self.inner.write_all(&bytes[..bytes.len() / 2])?;
            return Err(error);
        }
        self.inner.write_all(bytes)
    }
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        if self.track {
            self.faults
                .largest_write
                .fetch_max(bytes.len(), Ordering::Relaxed);
            self.faults.write_calls.fetch_add(1, Ordering::Relaxed);
        }
        if let Err(error) = self.faults.check("write_all_at") {
            self.inner.write_all_at(offset, &bytes[..bytes.len() / 2])?;
            return Err(error);
        }
        self.inner.write_all_at(offset, bytes)
    }
    fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if self.track {
            self.faults.largest_read.fetch_max(len, Ordering::Relaxed);
            self.faults.read_calls.fetch_add(1, Ordering::Relaxed);
        }
        self.faults.check("read_exact_at")?;
        self.inner.read_exact_at(offset, len)
    }
    fn sync_all(&mut self) -> io::Result<()> {
        self.faults.check("sync_all")?;
        self.faults.file_syncs.fetch_add(1, Ordering::Relaxed);
        self.inner.sync_all()
    }
    fn file_len(&self) -> io::Result<u64> {
        self.inner.file_len()
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.faults.check("set_len")?;
        self.inner.set_len(len)
    }
}

macro_rules! filesystem_operation {
    ($name:ident($($arg:ident: $ty:ty),*) -> $result:ty) => {
        fn $name(&self, $($arg: $ty),*) -> io::Result<$result> {
            self.check(stringify!($name))?;
            DirectFileSystem.$name($($arg),*)
        }
    };
}

impl FileSystem for Faults {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        self.check("open")?;
        Ok(Box::new(File {
            inner: DirectFileSystem.open(path)?,
            faults: self.clone(),
            track: self.track_all.load(Ordering::Relaxed)
                || path.to_string_lossy().contains(".ltx"),
        }))
    }
    fn open_rw(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        self.check("open_rw")?;
        Ok(Box::new(File {
            inner: DirectFileSystem.open_rw(path)?,
            faults: self.clone(),
            track: self.track_all.load(Ordering::Relaxed)
                || path.to_string_lossy().contains(".ltx"),
        }))
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        self.check("create")?;
        Ok(Box::new(File {
            inner: DirectFileSystem.create(path)?,
            faults: self.clone(),
            track: self.track_all.load(Ordering::Relaxed)
                || path.to_string_lossy().contains(".ltx"),
        }))
    }
    filesystem_operation!(file_len(path: &Path) -> u64);
    filesystem_operation!(canonicalize(path: &Path) -> PathBuf);
    filesystem_operation!(exists(path: &Path) -> bool);
    filesystem_operation!(create_dir(path: &Path) -> ());
    filesystem_operation!(create_dir_all(path: &Path) -> ());
    filesystem_operation!(remove_file(path: &Path) -> ());
    filesystem_operation!(rename(from: &Path, to: &Path) -> ());
    fn rename_uncommitted(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check("rename_uncommitted")?;
        DirectFileSystem.rename_uncommitted(from, to)
    }
    fn sync_parent(&self, path: &Path) -> io::Result<()> {
        self.check("sync_parent")?;
        self.parent_syncs.fetch_add(1, Ordering::Relaxed);
        DirectFileSystem.sync_parent(path)
    }
    filesystem_operation!(persist_new(path: &Path, bytes: &[u8]) -> ());
    filesystem_operation!(persist_file_new(source: &Path, destination: &Path) -> ());
}

fn fixture() -> (tempfile::TempDir, Arc<Faults>, Host, Db) {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default()
        .with_filesystem(faults.clone())
        .with_local_disk_budget(crab_ltx::DiskBudget::new(1 << 30));
    let mut writer = Db::open_with_host(
        &directory.path().join("source.sqlite"),
        Limits::default(),
        host.clone(),
    )
    .unwrap();
    writer
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(20000))")
        })
        .unwrap();
    (directory, faults, host, writer)
}

fn injected<T>(result: crab_ltx::Result<T>) {
    assert!(
        matches!(result, Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::StorageFull)
    );
}

#[cfg(feature = "replica")]
mod activation;
mod capture;
mod compaction;
mod injection;
mod matrix;
mod prepare;
mod restore;
mod volatile;
