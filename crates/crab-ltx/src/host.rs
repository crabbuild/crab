//! Bounded synchronous filesystem operations; execution scheduling belongs to the caller.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) struct LtxHost {
    pub max_database_bytes: u64,
    pub max_file_bytes: u64,
}

pub(crate) struct HostFile {
    file: File,
    limit: u64,
}

impl HostFile {
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        check_size(bytes.len() as u64, self.limit)?;
        self.file.write_all(bytes)
    }

    pub fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        check_size(len as u64, self.limit)?;
        check_size(self.file.metadata()?.len(), self.limit)?;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; len];
        self.file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    pub fn sync_all(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
    pub fn file_len(&mut self) -> io::Result<u64> {
        let len = self.file.metadata()?.len();
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
        read_bounded(path, self.max_file_bytes)
    }
    pub fn open(&self, path: &Path) -> io::Result<HostFile> {
        Ok(HostFile {
            file: File::open(path)?,
            limit: self.max_file_bytes,
        })
    }
    pub fn create(&self, path: &Path) -> io::Result<HostFile> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(HostFile {
            file: options.open(path)?,
            limit: self.max_file_bytes,
        })
    }
    pub fn metadata(&self, path: &Path) -> io::Result<HostMetadata> {
        Ok(HostMetadata {
            len: std::fs::metadata(path)?.len(),
        })
    }

    pub fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
    pub fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        // A fresh session owns this directory. Directory fsync seals the new name.
        std::fs::rename(from, to)?;
        sync_parent(to)
    }
    pub fn now_unix_millis(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
    pub fn file_age(&self, path: &Path) -> io::Result<Duration> {
        Ok(SystemTime::now()
            .duration_since(std::fs::metadata(path)?.modified()?)
            .unwrap_or_default())
    }
}

pub(crate) fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    check_size(file.metadata()?.len(), limit)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    check_size(bytes.len() as u64, limit)?;
    Ok(bytes)
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
