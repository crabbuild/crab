//! Models which host-file bytes and names survive a crash.
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
    synced: HashMap<u64, Vec<u8>>,
}

#[derive(Default)]
struct VolatileFs {
    state: Arc<Mutex<State>>,
    fail_parent: AtomicBool,
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
        if self.fail_parent.load(Ordering::Relaxed) {
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
        Ok(())
    }

    fn crash(&self) {
        let mut state = self.state.lock().unwrap();
        let paths: HashSet<_> = state
            .live
            .keys()
            .chain(state.durable.keys())
            .cloned()
            .collect();
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
        for (path, id) in &state.durable {
            if let Some(bytes) = state.synced.get(id) {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, bytes).unwrap();
            }
        }
        state.live = state.durable.clone();
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
        DirectFileSystem.create_dir_all(path)
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
        DirectFileSystem.create_dir(path)
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
    for barrier in ["none", "file", "parent"] {
        let directory = tempfile::TempDir::new().unwrap();
        let fs = Arc::new(VolatileFs::default());
        let host = Host::default().with_filesystem(fs.clone());
        let path = directory.path().join("source.sqlite");
        let mut db = Db::open_with_host(&path, Limits::default(), host).unwrap();
        db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(7)"))
            .unwrap();
        let cut = db.capture_deferred().unwrap();
        let segment = cut.segments[0].path().to_owned();
        if barrier == "file" || barrier == "parent" {
            fs.open_rw(&segment).unwrap().sync_all().unwrap();
        }
        if barrier == "parent" {
            fs.sync_parent(&segment).unwrap();
        }
        drop(db);
        fs.crash();
        if barrier == "parent" {
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
                "{barrier} must not preserve the cut name"
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
