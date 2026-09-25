//! Models which host-file bytes, names, and parent directories survive a crash.
//!
//! SQLite's VFS is deliberately outside this model; the tests below only
//! qualify LTX artifacts and checksum sidecars supplied by `FileSystem`.

use super::*;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
struct State {
    next: u64,
    live: HashMap<PathBuf, u64>,
    durable: HashMap<PathBuf, u64>,
    live_dirs: HashSet<PathBuf>,
    durable_dirs: HashSet<PathBuf>,
    synced: HashMap<u64, Vec<u8>>,
}

#[derive(Default)]
struct VolatileFs {
    state: Arc<Mutex<State>>,
    fail_parent: AtomicBool,
    fail_parent_path: Mutex<Option<PathBuf>>,
}

struct VolatileFile {
    inner: Box<dyn FileIo>,
    id: Option<u64>,
    path: PathBuf,
    state: Arc<Mutex<State>>,
}

impl VolatileFs {
    fn tracked(path: &Path) -> bool {
        let name = path.as_os_str().to_string_lossy();
        name.contains("-crab-ltx") || name.contains(".crab-ltx-")
    }

    fn id(&self, path: &Path) -> Option<u64> {
        if !Self::tracked(path) {
            return None;
        }
        let mut state = self.state.lock().unwrap();
        if let Some(id) = state.live.get(path) {
            return Some(*id);
        }
        state.next += 1;
        let id = state.next;
        state.live.insert(path.to_owned(), id);
        Some(id)
    }

    fn wrap(&self, path: &Path, inner: Box<dyn FileIo>) -> Box<dyn FileIo> {
        Box::new(VolatileFile {
            inner,
            id: self.id(path),
            path: path.to_owned(),
            state: self.state.clone(),
        })
    }

    fn moved(&self, from: &Path, to: &Path) {
        let mut state = self.state.lock().unwrap();
        if let Some(id) = state.live.remove(from) {
            state.live.insert(to.to_owned(), id);
        }
    }

    fn directory_barrier(&self, path: &Path) -> io::Result<()> {
        if self.fail_parent.load(Ordering::Relaxed)
            || self.fail_parent_path.lock().unwrap().as_deref() == Some(path)
        {
            return Err(io::Error::other("modeled parent sync failure"));
        }
        DirectFileSystem.sync_parent(path)?;
        let parent = path.parent();
        let mut state = self.state.lock().unwrap();
        state.durable.retain(|name, _| name.parent() != parent);
        let entries: Vec<_> = state
            .live
            .iter()
            .filter(|(name, _)| name.parent() == parent)
            .map(|(name, id)| (name.clone(), *id))
            .collect();
        state.durable.extend(entries);
        state.durable_dirs.retain(|name| name.parent() != parent);
        let directories: Vec<_> = state
            .live_dirs
            .iter()
            .filter(|name| name.parent() == parent)
            .cloned()
            .collect();
        state.durable_dirs.extend(directories);
        Ok(())
    }

    fn crash(&self) {
        let mut state = self.state.lock().unwrap();
        let directory_survives = |directory: &Path| {
            directory.ancestors().all(|ancestor| {
                !state.live_dirs.contains(ancestor) || state.durable_dirs.contains(ancestor)
            })
        };
        let surviving_dirs: Vec<_> = state
            .durable_dirs
            .iter()
            .filter(|directory| directory_survives(directory))
            .cloned()
            .collect();
        let surviving_files: Vec<_> = state
            .durable
            .iter()
            .filter(|(path, _)| path.parent().is_some_and(directory_survives))
            .map(|(path, id)| (path.clone(), *id))
            .collect();
        let paths: HashSet<_> = state
            .live
            .keys()
            .chain(state.durable.keys())
            .cloned()
            .collect();
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
        let mut directories: Vec<_> = state.live_dirs.iter().collect();
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for directory in directories {
            let _ = std::fs::remove_dir_all(directory);
        }
        for directory in &surviving_dirs {
            std::fs::create_dir_all(directory).unwrap();
        }
        for (path, id) in &surviving_files {
            if let Some(bytes) = state.synced.get(id) {
                std::fs::write(path, bytes).unwrap();
            }
        }
        state.live = surviving_files.into_iter().collect();
        state.live_dirs = surviving_dirs.into_iter().collect();
    }
}

impl FileIo for VolatileFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_all(bytes)
    }
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_all_at(offset, bytes)
    }
    fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read_exact_at(offset, len)
    }
    fn sync_all(&mut self) -> io::Result<()> {
        self.inner.sync_all()?;
        if let Some(id) = self.id {
            let bytes = std::fs::read(&self.path)?;
            self.state.lock().unwrap().synced.insert(id, bytes);
        }
        Ok(())
    }
    fn file_len(&self) -> io::Result<u64> {
        self.inner.file_len()
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
}

impl FileSystem for VolatileFs {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        Ok(self.wrap(path, DirectFileSystem.open(path)?))
    }
    fn open_rw(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        Ok(self.wrap(path, DirectFileSystem.open_rw(path)?))
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        Ok(self.wrap(path, DirectFileSystem.create(path)?))
    }
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        DirectFileSystem.file_len(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut created = Vec::new();
        let mut ancestor = path;
        while !DirectFileSystem.exists(ancestor)? {
            created.push(ancestor.to_owned());
            ancestor = ancestor
                .parent()
                .ok_or_else(|| io::Error::other("missing directory parent"))?;
        }
        DirectFileSystem.create_dir_all(path)?;
        self.state.lock().unwrap().live_dirs.extend(created);
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        DirectFileSystem.rename_uncommitted(from, to)?;
        self.moved(from, to);
        self.directory_barrier(to)
    }
    fn rename_uncommitted(&self, from: &Path, to: &Path) -> io::Result<()> {
        DirectFileSystem.rename_uncommitted(from, to)?;
        self.moved(from, to);
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        DirectFileSystem.remove_file(path)?;
        self.state.lock().unwrap().live.remove(path);
        Ok(())
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        DirectFileSystem.canonicalize(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        DirectFileSystem.exists(path)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        DirectFileSystem.create_dir(path)?;
        self.state.lock().unwrap().live_dirs.insert(path.to_owned());
        Ok(())
    }
    fn sync_parent(&self, path: &Path) -> io::Result<()> {
        self.directory_barrier(path)
    }
    fn persist_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        DirectFileSystem.persist_new(path, bytes)?;
        if let Some(id) = self.id(path) {
            self.state.lock().unwrap().synced.insert(id, bytes.to_vec());
        }
        self.directory_barrier(path)
    }
    fn persist_file_new(&self, source: &Path, destination: &Path) -> io::Result<()> {
        DirectFileSystem.persist_file_new(source, destination)?;
        self.moved(source, destination);
        self.directory_barrier(destination)
    }
}

#[test]
fn only_synced_ltx_bytes_and_parent_synced_names_survive_modeled_crash() {
    for barrier in ["none", "file", "file_parent", "ancestors"] {
        let directory = tempfile::TempDir::new().unwrap();
        let fs = Arc::new(VolatileFs::default());
        let host = Host::default().with_filesystem(fs.clone());
        let path = directory.path().join("source.sqlite");
        let mut db = Db::open_with_host(&path, Limits::default(), host).unwrap();
        db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(7)"))
            .unwrap();
        let cut = db.capture_deferred().unwrap();
        let segment = cut.segments[0].path().to_owned();
        if barrier != "none" {
            fs.open_rw(&segment).unwrap().sync_all().unwrap();
        }
        if barrier == "file_parent" || barrier == "ancestors" {
            fs.sync_parent(&segment).unwrap();
        }
        if barrier == "ancestors" {
            let l0 = segment.parent().unwrap();
            let ltx = l0.parent().unwrap();
            let session = ltx.parent().unwrap();
            for directory in [l0, ltx, session] {
                fs.sync_parent(directory).unwrap();
            }
        }
        drop(db);
        fs.crash();
        if barrier == "ancestors" {
            let plan = crab_ltx::VerifiedPlan::new(&cut.segments, cut.position, Limits::default())
                .unwrap();
            let restored = directory.path().join("restored.sqlite");
            crab_ltx::restore_exact(&plan, &restored).unwrap();
            let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
            let value: i64 = connection
                .query_row("SELECT v FROM t", [], |row| row.get(0))
                .unwrap();
            assert_eq!(value, 7);
        } else {
            assert!(
                !segment.exists(),
                "{barrier} must not preserve the complete cut path"
            );
        }
    }
}

#[test]
fn deferred_barrier_parent_failure_does_not_preserve_a_named_cut() {
    let directory = tempfile::TempDir::new().unwrap();
    let fs = Arc::new(VolatileFs::default());
    let host = Host::default().with_filesystem(fs.clone());
    let mut db = Db::open_with_host(
        &directory.path().join("source.sqlite"),
        Limits::default(),
        host,
    )
    .unwrap();
    db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(7)"))
        .unwrap();
    let cut = db.capture_deferred().unwrap();
    let segment = cut.segments[0].path().to_owned();

    fs.fail_parent.store(true, Ordering::Relaxed);
    assert!(db.durability_barrier().is_err());
    assert!(matches!(db.capture(), Err(CrabError::Fenced)));
    drop(db);
    fs.fail_parent.store(false, Ordering::Relaxed);
    fs.crash();
    assert!(!segment.exists());
}

#[test]
fn immediate_capture_survives_modeled_crash_at_its_return_boundary() {
    let directory = tempfile::TempDir::new().unwrap();
    let fs = Arc::new(VolatileFs::default());
    let host = Host::default().with_filesystem(fs.clone());
    let mut db = Db::open_with_host(
        &directory.path().join("source.sqlite"),
        Limits::default(),
        host,
    )
    .unwrap();
    db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(7)"))
        .unwrap();
    let cut = db.capture().unwrap();
    drop(db);
    fs.crash();

    let plan = crab_ltx::VerifiedPlan::new(&cut.segments, cut.position, Limits::default()).unwrap();
    let restored = directory.path().join("restored.sqlite");
    crab_ltx::restore_exact(&plan, &restored).unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    let value: i64 = connection
        .query_row("SELECT v FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 7);
}

#[test]
fn missing_session_directory_barrier_fences_local_acknowledgement() {
    for deferred in [false, true] {
        let directory = tempfile::TempDir::new().unwrap();
        let fs = Arc::new(VolatileFs::default());
        let host = Host::default().with_filesystem(fs.clone());
        let source = directory.path().join("source.sqlite");
        let mut db = Db::open_with_host(&source, Limits::default(), host).unwrap();
        let session = fs
            .state
            .lock()
            .unwrap()
            .live_dirs
            .iter()
            .find(|path| path.to_string_lossy().ends_with("-crab-ltx"))
            .cloned()
            .unwrap();
        db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(7)"))
            .unwrap();
        *fs.fail_parent_path.lock().unwrap() = Some(session.clone());
        if deferred {
            db.capture_deferred().unwrap();
            assert!(db.durability_barrier().is_err());
        } else {
            assert!(db.capture().is_err());
        }
        assert!(matches!(db.capture(), Err(CrabError::Fenced)));
        *fs.fail_parent_path.lock().unwrap() = None;
        drop(db);
        fs.crash();
        assert!(!session.exists(), "deferred={deferred}");
    }
}

#[test]
fn deferred_batch_survives_only_after_its_shared_barrier() {
    let directory = tempfile::TempDir::new().unwrap();
    let fs = Arc::new(VolatileFs::default());
    let host = Host::default().with_filesystem(fs.clone());
    let mut db = Db::open_with_host(
        &directory.path().join("source.sqlite"),
        Limits::default(),
        host,
    )
    .unwrap();
    db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(1)"))
        .unwrap();
    let first = db.capture_deferred().unwrap();
    db.transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
        .unwrap();
    let second = db.capture_deferred().unwrap();
    db.durability_barrier().unwrap();
    drop(db);
    fs.crash();

    let mut segments = first.segments;
    segments.extend(second.segments);
    let plan = crab_ltx::VerifiedPlan::new(&segments, second.position, Limits::default()).unwrap();
    let restored = directory.path().join("restored.sqlite");
    crab_ltx::restore_exact(&plan, &restored).unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    let count: i64 = connection
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn checkpoint_restart_preserves_the_acknowledged_cut_chain() {
    let directory = tempfile::TempDir::new().unwrap();
    let fs = Arc::new(VolatileFs::default());
    let host = Host::default().with_filesystem(fs.clone());
    let mut db = Db::open_with_host(
        &directory.path().join("source.sqlite"),
        Limits::default(),
        host,
    )
    .unwrap();
    db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(1)"))
        .unwrap();
    let first = db.capture().unwrap();
    db.transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
        .unwrap();
    let second = db.checkpoint(CheckpointMode::Truncate).unwrap();
    drop(db);
    fs.crash();

    let mut segments = first.segments;
    segments.extend(second.segments);
    let plan = crab_ltx::VerifiedPlan::new(&segments, second.position, Limits::default()).unwrap();
    let restored = directory.path().join("restored.sqlite");
    crab_ltx::restore_exact(&plan, &restored).unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    let count: i64 = connection
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn mutable_sidecar_and_unsynced_prune_revert_at_modeled_crash() {
    let directory = tempfile::TempDir::new().unwrap();
    let fs = VolatileFs::default();
    let sidecar = directory.path().join("source.sqlite.crab-ltx-checksums");
    let mut file = fs.create(&sidecar).unwrap();
    file.write_all(&[1]).unwrap();
    file.sync_all().unwrap();
    fs.sync_parent(&sidecar).unwrap();
    file.write_all_at(0, &[2]).unwrap();
    drop(file);
    fs.crash();
    assert_eq!(std::fs::read(&sidecar).unwrap(), vec![1]);

    fs.remove_file(&sidecar).unwrap();
    fs.crash();
    assert_eq!(std::fs::read(&sidecar).unwrap(), vec![1]);
    fs.remove_file(&sidecar).unwrap();
    fs.sync_parent(&sidecar).unwrap();
    fs.crash();
    assert!(!sidecar.exists());
}
