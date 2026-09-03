use std::ffi::c_void;
use std::fs::File;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::FileExt as _;
use std::ptr;

use rusqlite::ffi;

use super::locking::{READ_LOCK, UNLOCK, WRITE_LOCK, conflicting_lock, lock};
use super::vfs::{Context, io_code};

const LOCK_BASE: i64 = 120;
const DEADMAN: i64 = LOCK_BASE + 8;
const REGION_BYTES: usize = 32 * 1024;

pub(super) struct SharedMemory {
    mappings: Vec<Mapping>,
    file: File,
    mapping_bytes: usize,
}

struct Mapping {
    pointer: *mut c_void,
    bytes: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this owner retains exactly the successful mmap extent.
        unsafe {
            libc::munmap(self.pointer, self.bytes);
        }
    }
}

impl SharedMemory {
    pub(super) fn open(context: &Context) -> Result<Self, i32> {
        let name = context.side_name(b"-shm")?;
        // Match native SQLite's side-file contract even for a read-only main
        // connection. A non-mutating health view requires a separate policy;
        // READONLY on the main database alone does not establish that promise.
        let file = context.open(&name, true, false, false)?;
        // Match SQLite's dead-man-switch protocol: only the first owner may
        // reset abandoned shared memory. An exclusive initializer means BUSY,
        // not permission to proceed with possibly uninitialized contents.
        match conflicting_lock(&file, DEADMAN, 1)? {
            UNLOCK => {
                lock(&file, WRITE_LOCK, DEADMAN, 1)?;
                file.set_len(3).map_err(io_code)?;
            }
            READ_LOCK => {}
            _ => return Err(ffi::SQLITE_BUSY),
        }
        lock(&file, READ_LOCK, DEADMAN, 1)?;
        // SAFETY: sysconf reads the OS page-size constant without pointers.
        let page_size = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .map_err(|_| ffi::SQLITE_IOERR_SHMMAP)?;
        if !page_size.is_power_of_two() {
            return Err(ffi::SQLITE_IOERR_SHMMAP);
        }
        Ok(Self {
            file,
            mappings: Vec::new(),
            mapping_bytes: page_size.max(REGION_BYTES),
        })
    }

    pub(super) fn map(&mut self, region: i32, size: i32, extend: bool) -> Result<*mut c_void, i32> {
        if region < 0 || size as usize != REGION_BYTES {
            return Err(ffi::SQLITE_IOERR_SHMMAP);
        }
        let offset = (region as usize)
            .checked_mul(REGION_BYTES)
            .ok_or(ffi::SQLITE_IOERR_SHMSIZE)?;
        let index = offset / self.mapping_bytes;
        let end = index
            .checked_add(1)
            .and_then(|count| count.checked_mul(self.mapping_bytes))
            .ok_or(ffi::SQLITE_IOERR_SHMSIZE)?;
        if end > i64::MAX as usize {
            return Err(ffi::SQLITE_IOERR_SHMSIZE);
        }
        let current = self.file.metadata().map_err(io_code)?.len();
        if current < end as u64 {
            if !extend {
                return Ok(ptr::null_mut());
            }
            // Materialize each OS-independent 4 KiB block before mmap, matching
            // SQLite's avoidance of deferred disk-full SIGBUS during access.
            for page in current / 4096..end as u64 / 4096 {
                self.file
                    .write_all_at(&[0], page * 4096 + 4095)
                    .map_err(io_code)?;
            }
        }
        self.mappings
            .try_reserve(index.saturating_add(1).saturating_sub(self.mappings.len()))
            .map_err(|_| ffi::SQLITE_NOMEM)?;
        while self.mappings.len() <= index {
            let offset = self
                .mappings
                .len()
                .checked_mul(self.mapping_bytes)
                .ok_or(ffi::SQLITE_IOERR_SHMSIZE)?;
            let protection = libc::PROT_READ | libc::PROT_WRITE;
            // SAFETY: the checked extent is allocated, page-aligned, and mapped
            // shared from a live descriptor. SQLite owns synchronization of
            // access to its WAL-index memory through xShmLock and barriers.
            let pointer = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    self.mapping_bytes,
                    protection,
                    libc::MAP_SHARED,
                    self.file.as_raw_fd(),
                    offset as libc::off_t,
                )
            };
            if pointer == libc::MAP_FAILED {
                return Err(ffi::SQLITE_IOERR_SHMMAP);
            }
            self.mappings.push(Mapping {
                pointer,
                bytes: self.mapping_bytes,
            });
        }
        // SAFETY: offset within the mapped extent was computed from checked
        // region and mapping sizes; the map survives until this owner closes.
        Ok(unsafe {
            self.mappings[index]
                .pointer
                .cast::<u8>()
                .add(offset % self.mapping_bytes)
                .cast()
        })
    }

    pub(super) fn lock(&mut self, offset: i32, count: i32, flags: i32) -> Result<(), i32> {
        if offset < 0 || count <= 0 || offset.checked_add(count).is_none_or(|end| end > 8) {
            return Err(ffi::SQLITE_IOERR_SHMLOCK);
        }
        let kind = match flags {
            value if value == ffi::SQLITE_SHM_LOCK | ffi::SQLITE_SHM_SHARED => READ_LOCK,
            value if value == ffi::SQLITE_SHM_LOCK | ffi::SQLITE_SHM_EXCLUSIVE => WRITE_LOCK,
            value
                if value == ffi::SQLITE_SHM_UNLOCK | ffi::SQLITE_SHM_SHARED
                    || value == ffi::SQLITE_SHM_UNLOCK | ffi::SQLITE_SHM_EXCLUSIVE =>
            {
                UNLOCK
            }
            _ => return Err(ffi::SQLITE_IOERR_SHMLOCK),
        };
        lock(
            &self.file,
            kind,
            LOCK_BASE + i64::from(offset),
            i64::from(count),
        )
    }

    pub(super) fn close(self, context: &Context, delete: bool) -> Result<(), i32> {
        if delete {
            // SQLite requests deletion only after obtaining the database's
            // exclusive close/checkpoint lock. Also exclude another SHM owner
            // before unlink so that a mapped inode cannot split into two files.
            match lock(&self.file, WRITE_LOCK, DEADMAN, 1) {
                Ok(()) => context.unlink(&context.side_name(b"-shm")?)?,
                Err(ffi::SQLITE_BUSY) => {}
                Err(code) => return Err(code),
            }
        }
        Ok(())
    }
}
