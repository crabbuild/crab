use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;

use rusqlite::ffi;

pub(super) const PENDING: i64 = 0x4000_0000;
#[cfg(target_os = "macos")]
pub(super) const READ_LOCK: i32 = libc::F_RDLCK as i32;
#[cfg(target_os = "macos")]
pub(super) const WRITE_LOCK: i32 = libc::F_WRLCK as i32;
#[cfg(target_os = "macos")]
pub(super) const UNLOCK: i32 = libc::F_UNLCK as i32;
#[cfg(target_os = "linux")]
pub(super) const READ_LOCK: i32 = libc::F_RDLCK;
#[cfg(target_os = "linux")]
pub(super) const WRITE_LOCK: i32 = libc::F_WRLCK;
#[cfg(target_os = "linux")]
pub(super) const UNLOCK: i32 = libc::F_UNLCK;
const RESERVED: i64 = PENDING + 1;
const SHARED: i64 = PENDING + 2;
const SHARED_BYTES: i64 = 510;

pub(super) fn lock(file: &File, kind: i32, offset: i64, length: i64) -> Result<(), i32> {
    control(file, kind, offset, length, false).map(|_| ())
}

pub(super) fn conflicting_lock(file: &File, offset: i64, length: i64) -> Result<i32, i32> {
    control(file, WRITE_LOCK, offset, length, true)
}

fn control(file: &File, kind: i32, offset: i64, length: i64, query: bool) -> Result<i32, i32> {
    // SAFETY: flock is a C integer record; zero initializes unused fields,
    // including the pid required by open-file-description locks.
    let mut region: libc::flock = unsafe { std::mem::zeroed() };
    region.l_type = kind as _;
    region.l_whence = libc::SEEK_SET as _;
    region.l_start = offset;
    region.l_len = length;
    let command = if query {
        libc::F_OFD_GETLK
    } else {
        libc::F_OFD_SETLK
    };
    loop {
        // SAFETY: the file and initialized flock remain live for fcntl. These
        // nonblocking locks belong to this description, not every process fd.
        if unsafe { libc::fcntl(file.as_raw_fd(), command, &mut region) } == 0 {
            return Ok(i32::from(region.l_type));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(match error.raw_os_error() {
            Some(libc::EACCES | libc::EAGAIN) => ffi::SQLITE_BUSY,
            _ => ffi::SQLITE_IOERR_LOCK,
        });
    }
}

pub(super) struct DatabaseLock {
    pub(super) level: i32,
}

impl Default for DatabaseLock {
    fn default() -> Self {
        Self {
            level: ffi::SQLITE_LOCK_NONE,
        }
    }
}

impl DatabaseLock {
    pub(super) fn acquire(&mut self, file: &File, level: i32) -> Result<(), i32> {
        if level <= self.level {
            return Ok(());
        }
        match level {
            ffi::SQLITE_LOCK_SHARED if self.level == ffi::SQLITE_LOCK_NONE => {
                lock(file, READ_LOCK, PENDING, 1)?;
                let result = lock(file, READ_LOCK, SHARED, SHARED_BYTES);
                let release = lock(file, UNLOCK, PENDING, 1);
                result?;
                self.level = ffi::SQLITE_LOCK_SHARED;
                release?;
            }
            ffi::SQLITE_LOCK_RESERVED if self.level == ffi::SQLITE_LOCK_SHARED => {
                lock(file, WRITE_LOCK, RESERVED, 1)?;
                self.level = level;
            }
            ffi::SQLITE_LOCK_EXCLUSIVE if self.level >= ffi::SQLITE_LOCK_SHARED => {
                if self.level < ffi::SQLITE_LOCK_PENDING {
                    lock(file, WRITE_LOCK, PENDING, 1)?;
                    self.level = ffi::SQLITE_LOCK_PENDING;
                }
                // Keep PENDING on a busy upgrade. It excludes new readers
                // while the current readers finish, as SQLite requires.
                lock(file, WRITE_LOCK, SHARED, SHARED_BYTES)?;
                self.level = level;
            }
            _ => return Err(ffi::SQLITE_IOERR_LOCK),
        }
        Ok(())
    }

    pub(super) fn release(&mut self, file: &File, level: i32) -> Result<(), i32> {
        if level >= self.level {
            return Ok(());
        }
        match level {
            ffi::SQLITE_LOCK_NONE => lock(file, UNLOCK, PENDING, 512)?,
            ffi::SQLITE_LOCK_SHARED => {
                lock(file, READ_LOCK, SHARED, SHARED_BYTES)?;
                lock(file, UNLOCK, PENDING, 2)?;
            }
            _ => return Err(ffi::SQLITE_IOERR_UNLOCK),
        }
        self.level = level;
        Ok(())
    }

    pub(super) fn reserved(&self, file: &File) -> Result<bool, i32> {
        if self.level >= ffi::SQLITE_LOCK_RESERVED {
            return Ok(true);
        }
        Ok(conflicting_lock(file, RESERVED, 1)? != UNLOCK)
    }
}
