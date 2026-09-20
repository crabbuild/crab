#[cfg(feature = "replica")]
use crab_ltx::{CellReplica, CellStorageLayout};
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

#[derive(Clone, Default)]
struct Faults {
    failure: Arc<Mutex<Option<&'static str>>>,
    calls: Arc<Mutex<BTreeSet<&'static str>>>,
    largest_read: Arc<AtomicUsize>,
    largest_write: Arc<AtomicUsize>,
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
        }
        self.faults.check("read_exact_at")?;
        self.inner.read_exact_at(offset, len)
    }
    fn sync_all(&mut self) -> io::Result<()> {
        self.faults.check("sync_all")?;
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
    filesystem_operation!(sync_parent(path: &Path) -> ());
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
    faults.largest_write.store(0, Ordering::Relaxed);
    let (snapshot, _) = writer
        .snapshot(&directory.path().join("streamed-snapshot.ltx"))
        .unwrap();
    assert!(snapshot.info().size_bytes > 1_000_000);
    assert!(faults.largest_write.load(Ordering::Relaxed) < 128 * 1024);
    assert!(faults.largest_read.load(Ordering::Relaxed) < 128 * 1024);
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_prepare_bounds_source_and_scratch_transfers() {
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
    assert!(
        faults.largest_write.load(Ordering::Relaxed) <= 1 << 20,
        "largest scratch write was {} bytes",
        faults.largest_write.load(Ordering::Relaxed)
    );
    writer.close().unwrap();
    drop(directory);
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

    faults.arm(Some("write_all"));
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
async fn cancelled_cell_prepare_releases_scratch_without_publishing_a_root() {
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
    let scratch_slots = Arc::new(tokio::sync::Semaphore::new(64));
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
    .with_host(
        Host::default()
            .with_scratch_slots(Arc::clone(&scratch_slots))
            .with_local_disk_budget(crab_ltx::DiskBudget::new(64 * 1024 * 1024)),
    );

    let result = tokio::time::timeout(
        Duration::from_millis(250),
        replica.prepare(None, &captures, 1, 1),
    )
    .await;
    assert!(
        result.is_err(),
        "the throttled immutable upload must be cancelled"
    );
    assert_eq!(scratch_slots.available_permits(), 64);
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".crab-cell-segment-")
    }));
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
    faults.arm(Some("persist_new"));
    injected(host.compact(&plan, &destination));
    faults.arm(Some("persist_file_new"));
    injected(writer.snapshot(&destination));
    assert!(!destination.exists());
    faults.arm(None);
    host.compact(&plan, &destination).unwrap();
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
fn captured_pruning_retries_after_removal_but_failed_parent_sync() {
    let (_directory, faults, _host, mut writer) = fixture();
    let batch = writer.capture().unwrap();
    faults.arm(Some("sync_parent"));
    injected(writer.prune_captured(&batch));
    assert!(!batch.segments[0].path().exists());
    faults.arm(None);
    assert_eq!(writer.prune_captured(&batch).unwrap(), batch.segments.len());
    assert_eq!(writer.prune_captured(&batch).unwrap(), 0);
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(3)"))
        .unwrap();
    writer.capture().unwrap();
}
