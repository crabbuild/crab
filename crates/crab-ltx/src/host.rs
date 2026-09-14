//! Bounded synchronous filesystem operations; execution scheduling belongs to the caller.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::Duration;

pub(crate) struct LtxHost {
    pub facilities: crate::Host,
    pub max_database_bytes: u64,
    pub max_file_bytes: u64,
}

pub(crate) struct HostFile {
    file: Box<dyn crate::environment::FileIo>,
    limit: u64,
}

impl HostFile {
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        check_size(
            self.file.file_len()?.saturating_add(bytes.len() as u64),
            self.limit,
        )?;
        self.file.write_all(bytes)
    }

    pub fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        check_size(len as u64, self.limit)?;
        check_size(self.file.file_len()?, self.limit)?;
        let bytes = self.file.read_exact_at(offset, len)?;
        if bytes.len() != len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(bytes)
    }

    pub fn sync_all(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
    pub fn file_len(&mut self) -> io::Result<u64> {
        let len = self.file.file_len()?;
        check_size(len, self.limit)?;
        Ok(len)
    }
}

pub(crate) struct HostMetadata {
    pub len: u64,
}

impl LtxHost {
    pub fn check_database_size(&self, size: u64) -> crate::Result<()> {
        if size > self.max_database_bytes {
            return Err(crate::CrabError::Limit("database bytes"));
        }
        Ok(())
    }
    pub fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let mut file = self.open(path)?;
        let len = file.file_len()?;
        file.read_exact_at(0, usize::try_from(len).map_err(io::Error::other)?)
    }
    pub fn open(&self, path: &Path) -> io::Result<HostFile> {
        Ok(HostFile {
            file: self.facilities.filesystem.open(path)?,
            limit: self.max_file_bytes,
        })
    }
    pub fn create(&self, path: &Path) -> io::Result<HostFile> {
        Ok(HostFile {
            file: self.facilities.filesystem.create(path)?,
            limit: self.max_file_bytes,
        })
    }
    pub fn metadata(&self, path: &Path) -> io::Result<HostMetadata> {
        Ok(HostMetadata {
            len: self.facilities.filesystem.file_len(path)?,
        })
    }

    pub fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.facilities.filesystem.create_dir_all(path)
    }
    pub fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.facilities.filesystem.remove_file(path)
    }
    pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        // A fresh session owns this directory. Directory fsync seals the new name.
        self.facilities.filesystem.rename(from, to)
    }
    pub fn now_unix_millis(&self) -> i64 {
        self.facilities.clock.unix_millis()
    }
    pub fn file_age(&self, path: &Path) -> io::Result<Duration> {
        self.facilities.clock.file_age(path)
    }
}

fn check_size(size: u64, limit: u64) -> io::Result<()> {
    if size > limit {
        return Err(io::Error::other("LTX local file byte limit exceeded"));
    }
    Ok(())
}

pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)?.sync_all()
}
