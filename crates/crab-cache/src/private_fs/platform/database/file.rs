use std::ffi::{c_int, c_void};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{FileExt as _, MetadataExt as _};
use std::ptr;
use std::sync::atomic::{Ordering, fence};

use rusqlite::ffi;

use super::shm::SharedMemory;
use super::vfs::{file_context, io_code, with_file};

pub(super) static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 2,
    xClose: Some(super::vfs::close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(reserved),
    xFileControl: Some(control),
    xSectorSize: Some(sector_size),
    xDeviceCharacteristics: Some(characteristics),
    xShmMap: Some(shm_map),
    xShmLock: Some(shm_lock),
    xShmBarrier: Some(shm_barrier),
    xShmUnmap: Some(shm_unmap),
    xFetch: None,
    xUnfetch: None,
};

unsafe extern "C" fn read(
    raw: *mut ffi::sqlite3_file,
    output: *mut c_void,
    length: c_int,
    offset: i64,
) -> c_int {
    with_file(raw, |state| {
        if length < 0 || offset < 0 {
            return Err(ffi::SQLITE_IOERR_READ);
        }
        // SAFETY: SQLite supplies length writable bytes for this read.
        let output =
            unsafe { std::slice::from_raw_parts_mut(output.cast::<u8>(), length as usize) };
        let mut read = 0;
        while read < output.len() {
            match state
                .file
                .read_at(&mut output[read..], offset as u64 + read as u64)
            {
                Ok(0) => {
                    // SQLite requires every unread byte to be zero even on
                    // SQLITE_IOERR_SHORT_READ; pages may be newly allocated.
                    output[read..].fill(0);
                    return Err(ffi::SQLITE_IOERR_SHORT_READ);
                }
                Ok(count) => read += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(ffi::SQLITE_IOERR_READ),
            }
        }
        Ok(())
    })
}

unsafe extern "C" fn write(
    raw: *mut ffi::sqlite3_file,
    input: *const c_void,
    length: c_int,
    offset: i64,
) -> c_int {
    with_file(raw, |state| {
        if state.read_only {
            return Err(ffi::SQLITE_READONLY);
        }
        if length < 0 || offset < 0 {
            return Err(ffi::SQLITE_IOERR_WRITE);
        }
        // SAFETY: SQLite supplies length readable bytes for this write.
        let input = unsafe { std::slice::from_raw_parts(input.cast::<u8>(), length as usize) };
        state
            .file
            .write_all_at(input, offset as u64)
            .map_err(io_code)
    })
}

unsafe extern "C" fn truncate(raw: *mut ffi::sqlite3_file, size: i64) -> c_int {
    with_file(raw, |state| {
        if state.read_only {
            return Err(ffi::SQLITE_READONLY);
        }
        let size = u64::try_from(size).map_err(|_| ffi::SQLITE_IOERR_TRUNCATE)?;
        state.file.set_len(size).map_err(io_code)
    })
}

unsafe extern "C" fn sync(raw: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    with_file(raw, |state| {
        if state.read_only {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            let full_sync = flags & 0x0f == ffi::SQLITE_SYNC_FULL && {
                // SAFETY: F_FULLFSYNC operates on this live owned descriptor.
                // Pinned SQLite also uses fsync when this is unsupported.
                unsafe { libc::fcntl(state.file.as_raw_fd(), libc::F_FULLFSYNC) == 0 }
            };
            if !full_sync {
                state.file.sync_all().map_err(|_| ffi::SQLITE_IOERR_FSYNC)?;
            }
        }
        #[cfg(target_os = "linux")]
        {
            let result = if flags & ffi::SQLITE_SYNC_DATAONLY != 0 {
                state.file.sync_data()
            } else {
                state.file.sync_all()
            };
            result.map_err(|_| ffi::SQLITE_IOERR_FSYNC)?;
        }
        if state.sync_directory {
            file_context(state)
                .directory
                .file
                .sync_all()
                .map_err(|_| ffi::SQLITE_IOERR_DIR_FSYNC)?;
            state.sync_directory = false;
        }
        Ok(())
    })
}

unsafe extern "C" fn size(raw: *mut ffi::sqlite3_file, output: *mut i64) -> c_int {
    with_file(raw, |state| {
        let size = i64::try_from(state.file.metadata().map_err(io_code)?.len())
            .map_err(|_| ffi::SQLITE_IOERR_FSTAT)?;
        // SAFETY: SQLite provides a valid i64 output pointer.
        unsafe {
            *output = size;
        }
        Ok(())
    })
}

unsafe extern "C" fn lock(raw: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    with_file(raw, |state| state.locks.acquire(&state.file, level))
}

unsafe extern "C" fn unlock(raw: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    with_file(raw, |state| state.locks.release(&state.file, level))
}

unsafe extern "C" fn reserved(raw: *mut ffi::sqlite3_file, output: *mut c_int) -> c_int {
    with_file(raw, |state| {
        let reserved = state.locks.reserved(&state.file)?;
        // SAFETY: SQLite provides a valid integer output pointer.
        unsafe {
            *output = c_int::from(reserved);
        }
        Ok(())
    })
}

unsafe extern "C" fn control(
    raw: *mut ffi::sqlite3_file,
    operation: c_int,
    argument: *mut c_void,
) -> c_int {
    with_file(raw, |state| {
        match operation {
            ffi::SQLITE_FCNTL_LOCKSTATE => {
                // SAFETY: LOCKSTATE's argument is a writable integer.
                unsafe {
                    *argument.cast::<c_int>() = state.locks.level;
                }
            }
            ffi::SQLITE_FCNTL_HAS_MOVED => {
                let metadata = state.file.metadata().map_err(io_code)?;
                let moved = match file_context(state).directory.stat_component(&state.name) {
                    Ok(stat) => {
                        #[cfg(target_os = "macos")]
                        let device = stat.st_dev as u64;
                        #[cfg(target_os = "linux")]
                        let device = stat.st_dev;
                        device != metadata.dev() || stat.st_ino != metadata.ino()
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(error) => return Err(io_code(error)),
                };
                // SAFETY: HAS_MOVED's argument is a writable integer. Compare
                // within the pinned parent, never re-resolve the ambient root.
                unsafe {
                    *argument.cast::<c_int>() = c_int::from(moved);
                }
            }
            ffi::SQLITE_FCNTL_PERSIST_WAL => {
                // SAFETY: PERSIST_WAL uses a read/write integer, negative for query.
                let value = unsafe { &mut *argument.cast::<c_int>() };
                if *value >= 0 {
                    state.persist_wal = *value != 0;
                }
                *value = c_int::from(state.persist_wal);
            }
            _ => return Err(ffi::SQLITE_NOTFOUND),
        }
        Ok(())
    })
}

unsafe extern "C" fn sector_size(_raw: *mut ffi::sqlite3_file) -> c_int {
    4096
}

unsafe extern "C" fn characteristics(_raw: *mut ffi::sqlite3_file) -> c_int {
    // No filesystem atomicity or power-safe-overwrite assumption is needed.
    0
}

unsafe extern "C" fn shm_map(
    raw: *mut ffi::sqlite3_file,
    region: c_int,
    size: c_int,
    extend: c_int,
    output: *mut *mut c_void,
) -> c_int {
    // SAFETY: SQLite always supplies a writable mapping pointer.
    unsafe {
        *output = ptr::null_mut();
    }
    with_file(raw, |state| {
        if state.shm.is_none() {
            state.shm = Some(SharedMemory::open(file_context(state))?);
        }
        let shm = state.shm.as_mut().ok_or(ffi::SQLITE_IOERR_SHMOPEN)?;
        let mapping = shm.map(region, size, extend != 0)?;
        // SAFETY: the mapping remains owned until xShmUnmap or xClose.
        unsafe {
            *output = mapping;
        }
        Ok(())
    })
}

unsafe extern "C" fn shm_lock(
    raw: *mut ffi::sqlite3_file,
    offset: c_int,
    count: c_int,
    flags: c_int,
) -> c_int {
    with_file(raw, |state| {
        state
            .shm
            .as_mut()
            .ok_or(ffi::SQLITE_IOERR_SHMLOCK)?
            .lock(offset, count, flags)
    })
}

unsafe extern "C" fn shm_barrier(_raw: *mut ffi::sqlite3_file) {
    fence(Ordering::SeqCst);
}

unsafe extern "C" fn shm_unmap(raw: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    with_file(raw, |state| {
        if let Some(shm) = state.shm.take() {
            shm.close(file_context(state), delete != 0 && !state.persist_wal)?;
        }
        Ok(())
    })
}
