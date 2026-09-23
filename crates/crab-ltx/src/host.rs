//! Bounded synchronous filesystem operations; execution scheduling belongs to the caller.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) struct LtxHost {
    pub facilities: crate::Host,
    pub max_database_bytes: u64,
    pub max_file_bytes: u64,
}

pub(crate) struct HostFile {
    file: Box<dyn crate::environment::FileIo>,
    limit: u64,
    read_offset: u64,
    sequential_write_bytes: Option<u64>,
}

impl HostFile {
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(written) = self.sequential_write_bytes else {
            check_size(
                self.file.file_len()?.saturating_add(bytes.len() as u64),
                self.limit,
            )?;
            return self.file.write_all(bytes);
        };
        let end = written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("LTX local file byte limit exceeded"))?;
        check_size(end, self.limit)?;
        self.file.write_all(bytes)?;
        self.sequential_write_bytes = Some(end);
        Ok(())
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

    #[cfg(feature = "replica")]
    pub fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("local file offset overflow"))?;
        check_size(end, self.limit)?;
        self.file.write_all_at(offset, bytes)
    }

    pub fn sync_all(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
    pub fn file_len(&mut self) -> io::Result<u64> {
        let len = self.file.file_len()?;
        check_size(len, self.limit)?;
        Ok(len)
    }

    #[cfg(feature = "replica")]
    pub fn set_len(&mut self, len: u64) -> io::Result<()> {
        check_size(len, self.limit)?;
        self.file.set_len(len)
    }
}

impl Read for HostFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let remaining = self.file_len()?.saturating_sub(self.read_offset);
        let length =
            usize::try_from(remaining.min(bytes.len() as u64)).map_err(io::Error::other)?;
        if length == 0 {
            return Ok(0);
        }
        let read = self.file.read_exact_at(self.read_offset, length)?;
        bytes[..length].copy_from_slice(&read);
        self.read_offset += length as u64;
        Ok(length)
    }
}

impl Write for HostFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct HostMetadata {
    pub len: u64,
}

impl LtxHost {
    pub fn check_database_size(&self, size: u64) -> crate::Result<()> {
        if size > self.max_database_bytes {
            return Err(crate::CrabError::Limit(crate::LimitKind::DatabaseBytes));
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
            read_offset: 0,
            sequential_write_bytes: None,
        })
    }
    #[cfg(feature = "replica")]
    pub fn open_rw(&self, path: &Path) -> io::Result<HostFile> {
        Ok(HostFile {
            file: self.facilities.filesystem.open_rw(path)?,
            limit: self.max_file_bytes,
            read_offset: 0,
            sequential_write_bytes: None,
        })
    }
    pub fn create(&self, path: &Path) -> io::Result<HostFile> {
        Ok(HostFile {
            file: self.facilities.filesystem.create(path)?,
            limit: self.max_file_bytes,
            read_offset: 0,
            sequential_write_bytes: Some(0),
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
    pub fn rename_uncommitted(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.facilities.filesystem.rename_uncommitted(from, to)
    }
    pub fn now_unix_millis(&self) -> i64 {
        self.facilities.clock.unix_millis()
    }
    pub fn now_monotonic(&self) -> Instant {
        self.facilities.clock.monotonic()
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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    struct CountingFile {
        inner: File,
        file_len_calls: Arc<AtomicUsize>,
    }

    impl crate::environment::FileIo for CountingFile {
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            crate::environment::FileIo::write_all(&mut self.inner, bytes)
        }

        fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
            crate::environment::FileIo::write_all_at(&mut self.inner, offset, bytes)
        }

        fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            crate::environment::FileIo::read_exact_at(&mut self.inner, offset, len)
        }

        fn sync_all(&mut self) -> io::Result<()> {
            crate::environment::FileIo::sync_all(&mut self.inner)
        }

        fn file_len(&self) -> io::Result<u64> {
            self.file_len_calls.fetch_add(1, Ordering::Relaxed);
            crate::environment::FileIo::file_len(&self.inner)
        }

        fn set_len(&mut self, len: u64) -> io::Result<()> {
            crate::environment::FileIo::set_len(&mut self.inner, len)
        }
    }

    #[test]
    fn created_output_tracks_its_extent_without_metadata_queries() {
        let file_len_calls = Arc::new(AtomicUsize::new(0));
        let mut file = HostFile {
            file: Box::new(CountingFile {
                inner: tempfile::tempfile().unwrap(),
                file_len_calls: Arc::clone(&file_len_calls),
            }),
            limit: 4,
            read_offset: 0,
            sequential_write_bytes: Some(0),
        };

        file.write_all(&[1, 2]).unwrap();
        file.write_all(&[3, 4]).unwrap();
        assert_eq!(
            file.write_all(&[5]).unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert_eq!(file_len_calls.load(Ordering::Relaxed), 0);
    }
}
