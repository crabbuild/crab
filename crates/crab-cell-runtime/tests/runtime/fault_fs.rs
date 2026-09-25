//! Fault injection at local LTX capture and cleanup boundaries.

use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use crab_ltx::environment::{DirectFileSystem, FileIo, FileSystem};

pub struct FaultFileSystem {
    fail_next_capture: AtomicBool,
    fail_next_remove: AtomicBool,
}

impl FaultFileSystem {
    pub fn new() -> Self {
        Self {
            fail_next_capture: AtomicBool::new(false),
            fail_next_remove: AtomicBool::new(false),
        }
    }

    pub fn fail_next_capture(&self) {
        self.fail_next_capture.store(true, Ordering::SeqCst);
    }

    pub fn capture_failure_consumed(&self) -> bool {
        !self.fail_next_capture.load(Ordering::SeqCst)
    }

    pub fn fail_next_prune(&self) {
        self.fail_next_remove.store(true, Ordering::SeqCst);
    }

    pub fn prune_failure_consumed(&self) -> bool {
        !self.fail_next_remove.load(Ordering::SeqCst)
    }
}

impl FileSystem for FaultFileSystem {
    fn open(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        DirectFileSystem.open(path)
    }

    fn open_rw(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        DirectFileSystem.open_rw(path)
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn FileIo>> {
        if path.to_string_lossy().ends_with(".ltx.tmp")
            && self.fail_next_capture.swap(false, Ordering::SeqCst)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected capture failure after SQLite commit",
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
        if path.extension().is_some_and(|extension| extension == "ltx")
            && self.fail_next_remove.swap(false, Ordering::SeqCst)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected prune failure after root publication",
            ));
        }
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
