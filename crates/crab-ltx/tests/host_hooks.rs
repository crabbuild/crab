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
    calls: Arc<Mutex<BTreeSet<&'static str>>>,
    largest_read: Arc<AtomicUsize>,
    read_calls: Arc<AtomicUsize>,
    largest_write: Arc<AtomicUsize>,
    write_calls: Arc<AtomicUsize>,
    file_syncs: Arc<AtomicUsize>,
    parent_syncs: Arc<AtomicUsize>,
    track_all: Arc<AtomicBool>,
}

impl Faults {
    fn arm(&self, operation: Option<&'static str>) {
        *self.failure.lock().unwrap() = operation;
    }
    fn check(&self, operation: &'static str) -> io::Result<()> {
        self.calls.lock().unwrap().insert(operation);
        if *self.failure.lock().unwrap() == Some(operation) {
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
    let host = Host::default().with_filesystem(faults.clone());
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

#[test]
fn capture_and_inspection_bound_each_filesystem_transfer() {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let mut writer = Db::open_with_host(
        &directory.path().join("streamed.sqlite"),
        Limits::default(),
        host,
    )
    .unwrap();
    writer
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(2000000))")
        })
        .unwrap();

    let batch = writer.capture().unwrap();

    assert!(batch.segments[0].info().size_bytes > 1_000_000);
    assert!(faults.largest_write.load(Ordering::Relaxed) < 128 * 1024);
    assert!(faults.largest_read.load(Ordering::Relaxed) < 128 * 1024);

    faults.largest_read.store(0, Ordering::Relaxed);
    faults.read_calls.store(0, Ordering::Relaxed);
    faults.largest_write.store(0, Ordering::Relaxed);
    let (snapshot, _) = writer
        .snapshot(&directory.path().join("streamed-snapshot.ltx"))
        .unwrap();
    assert!(snapshot.info().size_bytes > 1_000_000);
    assert!(faults.largest_write.load(Ordering::Relaxed) < 128 * 1024);
    assert!(faults.largest_read.load(Ordering::Relaxed) < 128 * 1024);
}

#[test]
fn deferred_captures_share_one_directory_barrier() {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let mut writer = Db::open_with_host(
        &directory.path().join("deferred.sqlite"),
        Limits::default(),
        host,
    )
    .unwrap();

    writer
        .transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(1)"))
        .unwrap();
    let first = writer.capture_deferred().unwrap();
    writer
        .transaction(|tx| tx.execute("INSERT INTO t VALUES(2)", []))
        .unwrap();
    let second = writer.capture_deferred().unwrap();

    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);
    assert!(!first.segments.is_empty());
    assert!(!second.segments.is_empty());

    writer.durability_barrier().unwrap();
    assert_eq!(
        faults.file_syncs.load(Ordering::Relaxed),
        first.segments.len() + second.segments.len()
    );
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 1);
    writer.close().unwrap();
}

#[test]
fn failed_deferred_barrier_fences_before_acknowledgement() {
    for operation in ["sync_all", "sync_parent"] {
        let directory = tempfile::TempDir::new().unwrap();
        let faults = Arc::new(Faults::default());
        let host = Host::default().with_filesystem(faults.clone());
        let mut writer = Db::open_with_host(
            &directory.path().join("deferred-failure.sqlite"),
            Limits::default(),
            host,
        )
        .unwrap();
        writer
            .transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(1)"))
            .unwrap();
        let captured = writer.capture_deferred().unwrap();

        faults.arm(Some(operation));
        assert!(matches!(
            writer.durability_barrier(),
            Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::StorageFull
        ));
        assert!(matches!(writer.capture(), Err(CrabError::Fenced)));
        assert!(!captured.segments.is_empty());
    }
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_prepare_bounds_source_transfers_without_local_writes() {
    let (directory, faults, host, mut writer) = fixture();
    writer
        .transaction(|tx| {
            tx.execute("INSERT INTO t VALUES(randomblob(10000000))", [])?;
            Ok(())
        })
        .unwrap();
    let captured = writer.capture().unwrap();
    faults.track_all.store(true, Ordering::Relaxed);
    faults.largest_read.store(0, Ordering::Relaxed);
    faults.read_calls.store(0, Ordering::Relaxed);
    faults.largest_write.store(0, Ordering::Relaxed);
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-streaming-bound"),
            [31; 16],
        ),
        [32; 32],
        [33; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);

    replica.prepare(None, &captured, 1, 1).await.unwrap();

    assert!(
        faults.largest_read.load(Ordering::Relaxed) <= 8 * 1024 * 1024,
        "largest source read was {} bytes",
        faults.largest_read.load(Ordering::Relaxed)
    );
    assert_eq!(faults.largest_write.load(Ordering::Relaxed), 0);
    let expected_reads: usize = captured
        .segments
        .iter()
        .map(|segment| segment.info().size_bytes.div_ceil(8 << 20) as usize)
        .sum();
    assert_eq!(faults.read_calls.load(Ordering::Relaxed), expected_reads);
    writer.close().unwrap();
    drop(directory);
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn caller_constructed_segment_uses_full_inspection_fallback() {
    let (_directory, faults, host, mut writer) = fixture();
    let mut captured = writer.capture().unwrap();
    captured.segments[0] = LocalSegment::new(
        captured.segments[0].path().to_owned(),
        captured.segments[0].info().clone(),
    );
    faults.track_all.store(true, Ordering::Relaxed);
    faults.read_calls.store(0, Ordering::Relaxed);
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-external-segment"),
            [34; 16],
        ),
        [35; 32],
        [36; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);

    replica.prepare(None, &captured, 1, 1).await.unwrap();

    assert!(faults.read_calls.load(Ordering::Relaxed) > 1);
    writer.close().unwrap();
}

#[cfg(feature = "replica")]
#[tokio::test(start_paused = true)]
async fn prepare_overlaps_independent_immutable_uploads() {
    let (_directory, _faults, _host, mut writer) = fixture();
    let first = writer.capture_deferred().unwrap();
    let backend = InMemory::new();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(backend.clone())),
        ObjectPath::from("parallel-preparation"),
        [71; 16],
    );
    let initial = CellReplica::new(layout, [72; 32], [73; 16], Limits::default()).unwrap();
    let root = initial.prepare(None, &first, 1, 1).await.unwrap().root();
    writer.prune_captured(&first).unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(randomblob(4096))"))
        .unwrap();
    let captured = writer.capture_deferred().unwrap();

    let delay = Duration::from_millis(100);
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(ThrottledStore::new(
                backend,
                ThrottleConfig {
                    wait_put_per_call: delay,
                    ..ThrottleConfig::default()
                },
            ))),
            ObjectPath::from("parallel-preparation"),
            [71; 16],
        ),
        [72; 32],
        [73; 16],
        Limits::default(),
    )
    .unwrap();
    let started = tokio::time::Instant::now();

    replica.prepare(Some(&root), &captured, 2, 1).await.unwrap();

    assert_eq!(started.elapsed(), delay * 2);
    writer.close().unwrap();
}

#[cfg(feature = "replica")]
#[tokio::test(start_paused = true)]
async fn warm_append_reuses_its_authenticated_root_metadata() {
    let (_directory, _faults, _host, mut writer) = fixture();
    let first = writer.capture_deferred().unwrap();
    let delay = Duration::from_millis(100);
    let backend = InMemory::new();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(ThrottledStore::new(
                backend.clone(),
                ThrottleConfig {
                    wait_get_per_call: delay,
                    wait_put_per_call: delay,
                    ..ThrottleConfig::default()
                },
            ))),
            ObjectPath::from("cached-root-metadata"),
            [74; 16],
        ),
        [75; 32],
        [76; 16],
        Limits::default(),
    )
    .unwrap();
    let root = replica.prepare(None, &first, 1, 1).await.unwrap().root();
    writer.prune_captured(&first).unwrap();
    writer
        .transaction(|transaction| transaction.execute_batch("INSERT INTO t VALUES(2)"))
        .unwrap();
    let second = writer.capture_deferred().unwrap();
    let started = tokio::time::Instant::now();

    let prepared = replica.prepare(Some(&root), &second, 2, 1).await.unwrap();

    // The store throttles HEAD with the PUT delay: one parallel presence
    // wave plus the immutable upload waves, but no serial metadata GETs.
    assert_eq!(started.elapsed(), delay * 3);
    assert_eq!(prepared.root().position, second.position);
    let independent = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(ThrottledStore::new(
                backend,
                ThrottleConfig {
                    wait_get_per_call: delay,
                    ..ThrottleConfig::default()
                },
            ))),
            ObjectPath::from("cached-root-metadata"),
            [74; 16],
        ),
        [75; 32],
        [76; 16],
        Limits::default(),
    )
    .unwrap();
    let cold_started = tokio::time::Instant::now();
    independent.open_root(&prepared.root()).await.unwrap();
    assert!(cold_started.elapsed() >= delay * 2);
    writer.close().unwrap();
}

#[cfg(feature = "replica")]
#[tokio::test(start_paused = true)]
async fn prepare_opens_captured_segments_concurrently() {
    let (_directory, _faults, _host, mut writer) = fixture();
    let mut captured = writer.capture_deferred().unwrap();
    for value in [2, 3] {
        writer
            .transaction(|tx| tx.execute("INSERT INTO t VALUES(?1)", [value]))
            .unwrap();
        let next = writer.capture_deferred().unwrap();
        captured.segments.extend(next.segments);
        captured.position = next.position;
    }
    assert_eq!(captured.segments.len(), 3);

    let delay = Duration::from_millis(100);
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("parallel-capture-inputs"),
            [81; 16],
        ),
        [82; 32],
        [83; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(Host::default().with_executor(Arc::new(DelayedExecutor { delay })));
    let started = tokio::time::Instant::now();

    replica.prepare(None, &captured, 1, 1).await.unwrap();

    // One open wave plus size/read/size upload jobs. Serial opens would add
    // two more delay intervals before the already-concurrent uploads begin.
    assert_eq!(started.elapsed(), delay * 4);
    writer.close().unwrap();
}

#[cfg(feature = "replica")]
#[tokio::test(start_paused = true)]
async fn compaction_overlaps_independent_remote_transfers() {
    let (_directory, _faults, _host, mut writer) = fixture();
    let mut captured = writer.capture().unwrap();
    for value in [2, 3, 4] {
        writer
            .transaction(|tx| tx.execute("INSERT INTO t VALUES(?1)", [value]))
            .unwrap();
        let next = writer.capture().unwrap();
        captured.segments.extend(next.segments);
        captured.position = next.position;
    }
    assert_eq!(captured.segments.len(), 4);
    let delay = Duration::from_millis(100);
    let backend = InMemory::new();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(ThrottledStore::new(
                backend,
                ThrottleConfig {
                    wait_get_per_call: delay,
                    wait_put_per_call: delay,
                    ..ThrottleConfig::default()
                },
            ))),
            ObjectPath::from("parallel-compaction-downloads"),
            [84; 16],
        ),
        [85; 32],
        [86; 16],
        Limits::default(),
    )
    .unwrap();
    let root = replica.prepare(None, &captured, 1, 1).await.unwrap().root();
    let scratch = tempfile::TempDir::new().unwrap();
    let started = tokio::time::Instant::now();

    replica
        .prepare_compaction(&root, 0..4, 9, scratch.path())
        .await
        .unwrap();

    // Cached root metadata is presence-checked in one parallel HEAD wave.
    // Index/body downloads and the upload waves each add one interval.
    assert_eq!(started.elapsed(), delay * 5);
    writer.close().unwrap();
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_compaction_coalesces_local_output_writes() {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let mut writer = Db::open_with_host(
        &directory.path().join("source.sqlite"),
        Limits::default(),
        host.clone(),
    )
    .unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(3000000))",
            )
        })
        .unwrap();
    let captured = writer.capture().unwrap();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("buffered-compaction-output"),
            [66; 16],
        ),
        [67; 32],
        [68; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica.prepare(None, &captured, 1, 1).await.unwrap().root();
    faults.track_all.store(true, Ordering::Relaxed);
    faults.write_calls.store(0, Ordering::Relaxed);
    let scratch = tempfile::TempDir::new().unwrap();

    let compacted = replica
        .prepare_compaction(&root, 0..1, 9, scratch.path())
        .await
        .unwrap();

    assert_eq!(compacted.root().position, root.position);
    assert!(
        faults.write_calls.load(Ordering::Relaxed) <= 1_000,
        "compaction used {} local writes",
        faults.write_calls.load(Ordering::Relaxed)
    );
    writer.close().unwrap();
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_prepare_needs_no_scratch_or_local_durability_barrier() {
    let (_directory, faults, host, mut writer) = fixture();
    let captured = writer.capture_deferred().unwrap();
    let scratch_slots = Arc::new(tokio::sync::Semaphore::new(0));
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-ephemeral-scratch"),
            [41; 16],
        ),
        [42; 32],
        [43; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(
        host.with_scratch_slots(scratch_slots)
            .with_local_disk_budget(crab_ltx::DiskBudget::new(0)),
    );
    faults.calls.lock().unwrap().clear();
    faults.file_syncs.store(0, Ordering::Relaxed);
    faults.parent_syncs.store(0, Ordering::Relaxed);

    replica.prepare(None, &captured, 1, 1).await.unwrap();

    let calls = faults.calls.lock().unwrap();
    assert!(!calls.contains("create"));
    assert!(!calls.contains("open_rw"));
    assert!(!calls.contains("write_all"));
    assert!(!calls.contains("sync_all"));
    assert!(!calls.contains("sync_parent"));
    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);
    drop(calls);
    writer.close().unwrap();
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_restore_write_and_install_failures_clean_owned_scratch() {
    let (directory, faults, host, mut writer) = fixture();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-restore"),
            [1; 16],
        ),
        [2; 32],
        [3; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    let verified = replica.open_root(&root).await.unwrap();
    let destination = directory.path().join("cell-restored.sqlite");

    faults.arm(Some("write_all"));
    injected(verified.restore(&destination).await);
    assert!(!destination.exists());
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".crab-restore-")
    }));

    faults.arm(Some("persist_file_new"));
    injected(verified.restore(&destination).await);
    assert!(!destination.exists());
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".crab-restore-")
    }));

    faults.arm(None);
    assert_eq!(verified.restore(&destination).await.unwrap(), root.position);
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_restore_coalesces_page_writes_within_bounded_windows() {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let mut writer = Db::open_with_host(
        &directory.path().join("source.sqlite"),
        Limits::default(),
        host.clone(),
    )
    .unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(3000000))",
            )
        })
        .unwrap();
    let expected: Vec<u8> = writer
        .query_with(|connection| {
            connection.query_row("SELECT value FROM payload", [], |row| row.get(0))
        })
        .unwrap();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("coalesced-restore-output"),
            [69; 16],
        ),
        [70; 32],
        [71; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    let verified = replica.open_root(&root).await.unwrap();
    faults.track_all.store(true, Ordering::Relaxed);
    faults.write_calls.store(0, Ordering::Relaxed);
    let destination = directory.path().join("restored.sqlite");

    assert_eq!(verified.restore(&destination).await.unwrap(), root.position);
    let actual: Vec<u8> = crab_ltx::rusqlite::Connection::open(&destination)
        .unwrap()
        .query_row("SELECT value FROM payload", [], |row| row.get(0))
        .unwrap();
    assert_eq!(actual, expected);
    assert!(
        faults.write_calls.load(Ordering::Relaxed) <= 16,
        "restore used {} local writes",
        faults.write_calls.load(Ordering::Relaxed)
    );
    assert!(faults.largest_write.load(Ordering::Relaxed) <= 1 << 20);
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_compaction_uses_injected_filesystem_and_cleans_failed_scratch() {
    let (directory, faults, host, mut writer) = fixture();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-compaction"),
            [11; 16],
        ),
        [12; 32],
        [13; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();

    for operation in ["write_all_at", "write_all"] {
        faults.arm(Some(operation));
        injected(
            replica
                .prepare_compaction(&root, 0..1, 9, directory.path())
                .await,
        );
        assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".crab-compaction-")
        }));
    }

    faults.arm(None);
    let compacted = replica
        .prepare_compaction(&root, 0..1, 9, directory.path())
        .await
        .unwrap();
    assert_eq!(compacted.root().position, root.position);
    assert!(faults.calls.lock().unwrap().contains("open_rw"));
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_cell_prepare_releases_pinned_source_without_publishing_a_root() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("source.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction
                .execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(2000000))")
        })
        .unwrap();
    let captures = writer.capture().unwrap();
    let backend = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: Duration::from_secs(5),
            ..ThrottleConfig::default()
        },
    ));
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(backend),
            ObjectPath::from("cell-cancelled-prepare"),
            [21; 16],
        ),
        [22; 32],
        [23; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(Host::default());

    let result = tokio::time::timeout(
        Duration::from_millis(250),
        replica.prepare(None, &captures, 1, 1),
    )
    .await;
    assert!(
        result.is_err(),
        "the throttled immutable upload must be cancelled"
    );
    writer.prune_captured(&captures).unwrap();
    assert!(
        captures
            .segments
            .iter()
            .all(|segment| !segment.path().exists())
    );
    writer.close().unwrap();
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_checksum_write_failure_fences_after_sealing_the_cut() {
    let (directory, faults, host, mut source) = fixture();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-checksums"),
            [4; 16],
        ),
        [5; 32],
        [6; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica
        .prepare(None, &source.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    source.close().unwrap();

    let destination = directory.path().join("cell-active.sqlite");
    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&destination)
        .await
        .unwrap();
    let mut writer = writable.open_writable(&destination).unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
        .unwrap();
    faults.arm(Some("write_all_at"));
    injected(writer.capture());
    faults.arm(None);
    assert!(matches!(writer.capture(), Err(CrabError::Fenced)));
}

#[test]
fn capture_partial_write_sync_and_rename_failures_fence_the_session() {
    for operation in ["write_all", "sync_all", "rename"] {
        let (_directory, faults, _host, mut writer) = fixture();
        faults.arm(Some(operation));
        injected(writer.capture());
        faults.arm(None);
        assert!(
            matches!(writer.transaction(|_| Ok(())), Err(CrabError::Fenced)),
            "{operation}"
        );
    }
}

#[test]
fn committed_wal_observation_uses_the_injected_reader() {
    let (_directory, faults, _host, mut writer) = fixture();
    faults.arm(Some("read_exact_at"));
    injected(writer.transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)")));
    faults.arm(None);
    assert!(matches!(writer.capture(), Err(CrabError::Fenced)));
}

#[test]
fn fresh_session_claims_do_not_bypass_the_host() {
    for operation in ["canonicalize", "file_len", "create_dir"] {
        let directory = tempfile::TempDir::new().unwrap();
        let faults = Arc::new(Faults::default());
        faults.arm(Some(operation));
        injected(Db::open_with_host(
            &directory.path().join("db"),
            Limits::default(),
            Host::default().with_filesystem(faults),
        ));
        assert!(!directory.path().join("db").exists(), "{operation}");
    }
}

#[test]
fn exact_local_resume_and_all_checkpoints_preserve_the_injected_plan() {
    let (directory, faults, host, mut writer) = fixture();
    let first = writer.capture().unwrap();
    faults.arm(Some("open"));
    injected(host.verify(&first.segments, first.position, Limits::default()));
    faults.arm(None);
    let plan = host
        .verify(&first.segments, first.position, Limits::default())
        .unwrap();
    writer.close().unwrap();
    let destination = directory.path().join("resumed");
    for operation in ["exists", "persist_new"] {
        faults.arm(Some(operation));
        injected(Db::resume_with_host(
            &plan,
            &destination,
            Limits::default(),
            host.clone(),
        ));
        assert!(!destination.exists(), "{operation}");
    }
    faults.arm(None);
    let mut writer =
        Db::resume_with_host(&plan, &destination, Limits::default(), host.clone()).unwrap();
    assert_eq!(writer.position(), first.position);
    let mut segments = first.segments;
    for (index, mode) in [
        CheckpointMode::Passive,
        CheckpointMode::Full,
        CheckpointMode::Restart,
        CheckpointMode::Truncate,
    ]
    .into_iter()
    .enumerate()
    {
        writer
            .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(1)"))
            .unwrap();
        let batch = writer.checkpoint(mode).unwrap();
        assert_eq!(batch.timing.checkpoint_runs, 1);
        assert!(batch.timing.checkpoint_frames >= batch.timing.checkpoint_backfilled);
        assert_eq!(batch.timing.checkpoint_busy_errors, 0);
        segments.extend(batch.segments);
        let plan = host
            .verify(&segments, batch.position, Limits::default())
            .unwrap();
        let restored = directory.path().join(format!("check-{index}"));
        host.restore(&plan, &restored).unwrap();
        let db = crab_ltx::rusqlite::Connection::open(restored).unwrap();
        let count: usize = db
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, index + 2);
    }
    let calls = faults.calls.lock().unwrap();
    for operation in [
        "canonicalize",
        "create_dir",
        "read_exact_at",
        "exists",
        "persist_new",
    ] {
        assert!(calls.contains(operation), "{operation}");
    }
}

#[test]
fn snapshot_and_compaction_installation_are_injectable_and_never_clobber() {
    let (directory, faults, host, mut writer) = fixture();
    let batch = writer.capture().unwrap();
    let plan = host
        .verify(&batch.segments, batch.position, Limits::default())
        .unwrap();
    let destination = directory.path().join("snapshot.ltx");
    for operation in [
        "create",
        "write_all",
        "sync_all",
        "open",
        "persist_file_new",
    ] {
        faults.arm(Some(operation));
        injected(host.compact(&plan, &destination));
        assert!(!destination.exists(), "{operation}");
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp-crab-ltx-compaction-")),
            "{operation} left compaction scratch"
        );
    }
    faults.arm(Some("persist_file_new"));
    injected(writer.snapshot(&destination));
    assert!(!destination.exists());
    faults.arm(None);
    faults.track_all.store(true, Ordering::Relaxed);
    faults.largest_write.store(0, Ordering::Relaxed);
    host.compact(&plan, &destination).unwrap();
    assert!(faults.largest_write.load(Ordering::Relaxed) < 128 * 1024);
    assert!(!faults.calls.lock().unwrap().contains("persist_new"));
    let before = std::fs::read(&destination).unwrap();
    assert!(host.compact(&plan, &destination).is_err());
    assert_eq!(std::fs::read(destination).unwrap(), before);
}

#[test]
fn unknown_sqlite_vfs_does_not_fall_back_to_the_platform_vfs() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("db");
    assert!(
        Db::open_with_host(
            &path,
            Limits::default(),
            Host::default().with_sqlite_vfs("absent-test-vfs")
        )
        .is_err()
    );
    assert!(!path.exists());
}

#[cfg(feature = "replica")]
#[test]
fn captured_pruning_retains_accounting_after_io_failure() {
    for operation in ["read_exact_at", "remove_file"] {
        let (_directory, faults, _host, mut writer) = fixture();
        let batch = writer.capture().unwrap();
        faults.arm(Some(operation));
        injected(writer.prune_captured(&batch));
        assert!(batch.segments[0].path().exists());
        faults.arm(None);
        assert_eq!(writer.prune_captured(&batch).unwrap(), batch.segments.len());
        assert_eq!(writer.prune_captured(&batch).unwrap(), 0);
    }
}

#[cfg(feature = "replica")]
#[test]
fn published_deferred_capture_is_pruned_without_a_local_durability_barrier() {
    let (_directory, faults, _host, mut writer) = fixture();
    let batch = writer.capture_deferred().unwrap();

    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);
    faults.arm(Some("sync_parent"));
    assert_eq!(writer.prune_captured(&batch).unwrap(), batch.segments.len());
    faults.arm(None);
    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);

    writer.close().unwrap();
    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);
}
