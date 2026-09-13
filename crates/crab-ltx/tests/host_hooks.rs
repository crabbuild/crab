use crab_ltx::{
    CheckpointMode, CrabError, Host, Limits, ManagedDb,
    environment::{DirectFileSystem, FileIo, FileSystem},
};
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct Faults {
    failure: Arc<Mutex<Option<&'static str>>>,
    calls: Arc<Mutex<BTreeSet<&'static str>>>,
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
}

impl FileIo for File {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Err(error) = self.faults.check("write_all") {
            self.inner.write_all(&bytes[..bytes.len() / 2])?;
            return Err(error);
        }
        self.inner.write_all(bytes)
    }
    fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
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
        }))
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        self.check("create")?;
        Ok(Box::new(File {
            inner: DirectFileSystem.create(path)?,
            faults: self.clone(),
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
}

fn fixture() -> (tempfile::TempDir, Arc<Faults>, Host, ManagedDb) {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let mut writer = ManagedDb::open_with_host(
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
        injected(ManagedDb::open_with_host(
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
        injected(ManagedDb::resume_with_host(
            &plan,
            &destination,
            Limits::default(),
            host.clone(),
        ));
        assert!(!destination.exists(), "{operation}");
    }
    faults.arm(None);
    let mut writer =
        ManagedDb::resume_with_host(&plan, &destination, Limits::default(), host.clone()).unwrap();
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
        ManagedDb::open_with_host(
            &path,
            Limits::default(),
            Host::default().with_sqlite_vfs("absent-test-vfs")
        )
        .is_err()
    );
    assert!(!path.exists());
}

#[cfg(feature = "replica")]
mod remote {
    use super::*;
    use crab_ltx::{
        Replica,
        environment::{Executor, Worker},
    };
    use crab_storage::{Store, StoreLayout};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Jobs {
        started: Arc<AtomicUsize>,
        joined: Arc<AtomicUsize>,
        dispatches: AtomicUsize,
    }
    struct Join {
        thread: std::thread::JoinHandle<()>,
        joined: Arc<AtomicUsize>,
    }
    impl Worker for Join {
        fn join(self: Box<Self>) -> io::Result<()> {
            self.thread.join().unwrap();
            self.joined.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    impl Executor for Jobs {
        fn dispatch(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<()> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            std::thread::Builder::new().spawn(job)?;
            Ok(())
        }
        fn start_worker(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<Box<dyn Worker>> {
            self.started.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Join {
                thread: std::thread::Builder::new().spawn(job)?,
                joined: self.joined.clone(),
            }))
        }
    }

    fn replica(host: Host) -> Replica {
        Replica::new(
            StoreLayout::new(
                Store::new(Arc::new(object_store::memory::InMemory::new())),
                "hooks".into(),
            ),
            "epoch",
            Limits::default(),
        )
        .unwrap()
        .with_host(host)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_restore_sparse_activation_and_worker_lifetime_use_one_host() {
        let (directory, faults, host, mut writer) = fixture();
        let jobs = Arc::new(Jobs::default());
        // Selecting the named platform VFS exercises a non-default wrapper
        // registration without assuming Unix-specific VFS names in this test.
        let name = unsafe {
            // SAFETY: SQLite is initialized by fixture; its default registration
            // and NUL-terminated name live for the process lifetime.
            std::ffi::CStr::from_ptr(
                (*crab_ltx::rusqlite::ffi::sqlite3_vfs_find(std::ptr::null())).zName,
            )
            .to_str()
            .unwrap()
            .to_owned()
        };
        let replica = replica(host.with_executor(jobs.clone()).with_sqlite_vfs(&name));
        let batch = writer.capture().unwrap();
        let head = replica.replicate(&batch, None).await.unwrap();
        let restored = directory.path().join("restored");
        faults.arm(Some("persist_new"));
        injected(replica.restore(&head, &restored).await);
        assert!(!restored.exists());
        faults.arm(None);
        replica.restore(&head, &restored).await.unwrap();

        for operation in ["create", "set_len", "sync_all", "sync_parent"] {
            let paged = replica.paged(&head).await.unwrap();
            let path = directory.path().join(operation);
            faults.arm(Some(operation));
            injected(paged.open_writable(&path));
            faults.arm(None);
        }
        let paged = replica.paged(&head).await.unwrap();
        let mut sparse = paged
            .open_writable(&directory.path().join("sparse"))
            .unwrap();
        sparse
            .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(9)"))
            .unwrap();
        while !sparse.hydrate_step(2).unwrap().complete() {}
        let captured = sparse.checkpoint(CheckpointMode::Truncate).unwrap();
        let head = replica.replicate(&captured, Some(&head)).await.unwrap();
        sparse.close().unwrap();
        let paged = replica.paged(&head).await.unwrap();
        let view = paged.open_sqlite().unwrap();
        let count: usize = view
            .connection()
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        drop(view);
        assert_eq!(jobs.started.load(Ordering::SeqCst), 2);
        assert_eq!(jobs.joined.load(Ordering::SeqCst), 2);
        assert!(jobs.dispatches.load(Ordering::SeqCst) > 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn published_pruning_retries_after_removal_but_failed_parent_sync() {
        let (_directory, faults, host, mut writer) = fixture();
        let replica = replica(host);
        let batch = writer.capture().unwrap();
        let head = replica.replicate(&batch, None).await.unwrap();
        faults.arm(Some("sync_parent"));
        injected(writer.prune_published(&head));
        assert!(!batch.segments[0].path().exists());
        faults.arm(None);
        assert_eq!(writer.prune_published(&head).unwrap(), batch.segments.len());
        assert_eq!(writer.prune_published(&head).unwrap(), 0);
        writer
            .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(3)"))
            .unwrap();
        replica
            .replicate(&writer.capture().unwrap(), Some(&head))
            .await
            .unwrap();
    }
}
