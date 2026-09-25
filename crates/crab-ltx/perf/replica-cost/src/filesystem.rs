use crab_ltx::environment::{DirectFileSystem, FileIo, FileSystem};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Default)]
pub(super) struct ChecksumSyncs {
    calls: AtomicU64,
    nanos: AtomicU64,
}

impl ChecksumSyncs {
    pub(super) fn take(&self) -> (u64, u64) {
        (
            self.calls.swap(0, Ordering::Relaxed),
            self.nanos.swap(0, Ordering::Relaxed) / 1_000,
        )
    }
}

pub(super) struct MeasuredFileSystem {
    pub(super) syncs: std::sync::Arc<ChecksumSyncs>,
}

struct MeasuredFile {
    inner: Box<dyn FileIo>,
    checksum: bool,
    syncs: std::sync::Arc<ChecksumSyncs>,
}

impl MeasuredFileSystem {
    fn wrap(&self, path: &Path, file: Box<dyn FileIo>) -> Box<dyn FileIo> {
        Box::new(MeasuredFile {
            inner: file,
            checksum: path
                .as_os_str()
                .to_string_lossy()
                .ends_with(".crab-ltx-checksums"),
            syncs: self.syncs.clone(),
        })
    }
}

impl FileIo for MeasuredFile {
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
        let started = Instant::now();
        let result = self.inner.sync_all();
        if self.checksum {
            self.syncs.calls.fetch_add(1, Ordering::Relaxed);
            self.syncs.nanos.fetch_add(
                started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
        }
        result
    }
    fn file_len(&self) -> io::Result<u64> {
        self.inner.file_len()
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
}

impl FileSystem for MeasuredFileSystem {
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
        DirectFileSystem.rename(from, to)
    }
    fn rename_uncommitted(&self, from: &Path, to: &Path) -> io::Result<()> {
        DirectFileSystem.rename_uncommitted(from, to)
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
    fn cleanup_private_temporaries(&self, root: &Path) -> io::Result<()> {
        DirectFileSystem.cleanup_private_temporaries(root)
    }
}
