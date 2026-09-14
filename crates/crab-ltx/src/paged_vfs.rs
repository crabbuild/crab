//! Immutable SQLite VFS inspired by Celld's per-cut page source and registration.
//! Unlike Celld's writable sparse wrapper, this VFS never mutates a local file.

use crate::{CrabError, PagedDatabase, Result};
use rusqlite::{Connection, OpenFlags, ffi};
use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_int, c_void},
    sync::{Arc, Mutex, OnceLock},
};

use crate::paged_io::Io;

struct App {
    io: Io,
    page_size: u32,
    count: u32,
    // SQLite exposes only an I/O result code; retain the source for diagnostics.
    error: std::sync::Mutex<Option<CrabError>>,
}

struct Registration {
    app: Arc<App>,
    name: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // Remove discovery only. Every open SQLite file holds its own Arc,
        // including connections independently opened through the global VFS.
        if let Ok(mut views) = views().lock() {
            views.remove(&self.name);
        }
    }
}

/// A read-only SQL connection pinned to a replica head for its entire lifetime.
///
/// Queries block for remote faults. Own/use/drop on a blocking thread. The
/// view disappears from discovery on drop; open files retain their page source.
/// Unknown databases and disk-backed temporary files are refused; temp storage
/// is memory. Leaking SQL statements also leaks their SQLite/page-source state.
/// SQL is trusted; the process-global view registry is not an authorization boundary.
pub struct PagedConnection {
    connection: Connection,
    registration: Registration,
}

impl PagedConnection {
    /// Borrows the read-only connection without transferring its VFS lifetime.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Takes the underlying range/decode error from a failed SQLite read.
    pub fn take_read_error(&self) -> Result<Option<CrabError>> {
        self.registration
            .app
            .error
            .lock()
            .map_err(|_| CrabError::InvalidState("paged error lock poisoned"))
            .map(|mut error| error.take())
    }
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
const VFS_NAME: &str = "crab-ltx-readonly-v1";

fn views() -> &'static Mutex<HashMap<String, Arc<App>>> {
    static VIEWS: OnceLock<Mutex<HashMap<String, Arc<App>>>> = OnceLock::new();
    VIEWS.get_or_init(Mutex::default)
}

pub(crate) fn open(database: PagedDatabase) -> Result<PagedConnection> {
    static VFS: OnceLock<c_int> = OnceLock::new();
    let rc = *VFS.get_or_init(register_vfs);
    if rc != ffi::SQLITE_OK {
        return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None).into());
    }
    let name = format!(
        "crab-ltx-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let app = Arc::new(App {
        page_size: database.page_size(),
        count: database.page_count(),
        io: Io::new(crate::paged_io::Database::Replica(database))?,
        error: std::sync::Mutex::new(None),
    });
    views()
        .lock()
        .map_err(|_| CrabError::InvalidState("paged registry lock poisoned"))?
        .insert(name.clone(), app.clone());
    let registration = Registration { app, name };
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = match Connection::open_with_flags_and_vfs(&registration.name, flags, VFS_NAME)
    {
        Ok(connection) => connection,
        Err(error) => {
            let source = registration
                .app
                .error
                .lock()
                .ok()
                .and_then(|mut error| error.take());
            return Err(source.unwrap_or_else(|| error.into()));
        }
    };
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "query_only", true)?;
    Ok(PagedConnection {
        connection,
        registration,
    })
}

fn register_vfs() -> c_int {
    // SAFETY: zero initializes optional C callbacks/pointers. All version-1
    // required methods below are installed before registration.
    let mut vfs: Box<ffi::sqlite3_vfs> = Box::new(unsafe { std::mem::zeroed() });
    vfs.iVersion = 1;
    vfs.szOsFile = std::mem::size_of::<PagedFile>() as c_int;
    vfs.mxPathname = 128;
    vfs.zName = c"crab-ltx-readonly-v1".as_ptr();
    vfs.xOpen = Some(x_open);
    vfs.xDelete = Some(x_delete);
    vfs.xAccess = Some(x_access);
    vfs.xFullPathname = Some(x_full_pathname);
    vfs.xRandomness = Some(x_randomness);
    vfs.xSleep = Some(x_sleep);
    vfs.xCurrentTime = Some(x_current_time);
    // SAFETY: one process-lifetime registration avoids freeing a VFS concurrently
    // discovered by SQLite. Per-file Arc references own all non-static resources.
    let rc = unsafe {
        let initialized = ffi::sqlite3_initialize();
        if initialized != ffi::SQLITE_OK {
            return initialized;
        }
        if !ffi::sqlite3_vfs_find(vfs.zName).is_null() {
            return ffi::SQLITE_ERROR;
        }
        ffi::sqlite3_vfs_register(&mut *vfs, 0)
    };
    if rc == ffi::SQLITE_OK {
        let _ = Box::into_raw(vfs);
    }
    rc
}

#[repr(C)]
struct PagedFile {
    methods: *const ffi::sqlite3_io_methods,
    app: *const App,
}

unsafe extern "C" fn x_open(
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    // SAFETY: SQLite provides writable szOsFile storage and a valid filename.
    // Null methods on failure prevent SQLite from calling xClose on that file.
    unsafe {
        (*file).pMethods = std::ptr::null();
        if name.is_null()
            || flags & ffi::SQLITE_OPEN_MAIN_DB == 0
            || flags & ffi::SQLITE_OPEN_READWRITE != 0
        {
            return ffi::SQLITE_CANTOPEN;
        }
        let paged = file.cast::<PagedFile>();
        let Ok(name) = CStr::from_ptr(name).to_str() else {
            return ffi::SQLITE_CANTOPEN;
        };
        let Ok(views) = views().lock() else {
            return ffi::SQLITE_CANTOPEN;
        };
        let Some(app) = views.get(name).cloned() else {
            return ffi::SQLITE_CANTOPEN;
        };
        (*paged).app = Arc::into_raw(app);
        (*paged).methods = &METHODS;
        if !out.is_null() {
            *out = ffi::SQLITE_OPEN_READONLY;
        }
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_read(
    file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    if amount < 0 || offset < 0 {
        return ffi::SQLITE_IOERR_READ;
    }
    // SAFETY: SQLite supplies amount writable bytes and a live file. Its App
    // is owned by this file's Arc until xClose, independently of discovery.
    let (app, output) = unsafe {
        (
            &*(*file.cast::<PagedFile>()).app,
            std::slice::from_raw_parts_mut(buffer.cast::<u8>(), amount as usize),
        )
    };
    output.fill(0);
    // Never unwind across SQLite's C boundary, including a poisoned I/O path.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read(app, output, offset as u64)
    }));
    match result {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            if let Ok(mut slot) = app.error.lock() {
                *slot = Some(error);
            }
            ffi::SQLITE_IOERR_READ
        }
        Err(_) => ffi::SQLITE_IOERR_READ,
    }
}

fn read(app: &App, output: &mut [u8], offset: u64) -> Result<c_int> {
    let ps = u64::from(app.page_size);
    let length = ps * u64::from(app.count);
    let available = length.saturating_sub(offset).min(output.len() as u64) as usize;
    let mut copied = 0;
    while copied < available {
        let at = offset + copied as u64;
        let pgno = u32::try_from(at / ps + 1).map_err(|_| CrabError::LTXCorrupted)?;
        let page = app.io.page(pgno)?;
        if page.len() != ps as usize {
            return Err(CrabError::LTXCorrupted);
        }
        let start = (at % ps) as usize;
        let count = (page.len() - start).min(available - copied);
        output[copied..copied + count].copy_from_slice(&page[start..start + count]);
        copied += count;
    }
    Ok(if available < output.len() {
        ffi::SQLITE_IOERR_SHORT_READ
    } else {
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn x_close(file: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: xOpen transferred exactly one Arc into this file; SQLite calls
    // xClose once. Other files and the registry retain independent references.
    unsafe {
        drop(Arc::from_raw((*file.cast::<PagedFile>()).app));
        (*file).pMethods = std::ptr::null();
    }
    ffi::SQLITE_OK
}
unsafe extern "C" fn x_write(
    _: *mut ffi::sqlite3_file,
    _: *const c_void,
    _: c_int,
    _: i64,
) -> c_int {
    ffi::SQLITE_READONLY
}
unsafe extern "C" fn x_truncate(_: *mut ffi::sqlite3_file, _: i64) -> c_int {
    ffi::SQLITE_READONLY
}
unsafe extern "C" fn x_sync(_: *mut ffi::sqlite3_file, _: c_int) -> c_int {
    ffi::SQLITE_OK
}
unsafe extern "C" fn x_lock(_: *mut ffi::sqlite3_file, _: c_int) -> c_int {
    ffi::SQLITE_OK
}
unsafe extern "C" fn x_reserved(_: *mut ffi::sqlite3_file, out: *mut c_int) -> c_int {
    // SAFETY: SQLite supplies an integer output; immutable views have no writers.
    unsafe {
        *out = 0;
    }
    ffi::SQLITE_OK
}
unsafe extern "C" fn x_size(file: *mut ffi::sqlite3_file, out: *mut i64) -> c_int {
    // SAFETY: file and output are live SQLite pointers; App lifetime is pinned.
    unsafe {
        let app = &*(*file.cast::<PagedFile>()).app;
        *out = i64::from(app.page_size) * i64::from(app.count);
    }
    ffi::SQLITE_OK
}
unsafe extern "C" fn x_control(_: *mut ffi::sqlite3_file, _: c_int, _: *mut c_void) -> c_int {
    ffi::SQLITE_NOTFOUND
}
unsafe extern "C" fn x_sector(_: *mut ffi::sqlite3_file) -> c_int {
    4096
}
unsafe extern "C" fn x_characteristics(_: *mut ffi::sqlite3_file) -> c_int {
    ffi::SQLITE_IOCAP_IMMUTABLE
}

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(x_close),
    xRead: Some(x_read),
    xWrite: Some(x_write),
    xTruncate: Some(x_truncate),
    xSync: Some(x_sync),
    xFileSize: Some(x_size),
    xLock: Some(x_lock),
    xUnlock: Some(x_lock),
    xCheckReservedLock: Some(x_reserved),
    xFileControl: Some(x_control),
    xSectorSize: Some(x_sector),
    xDeviceCharacteristics: Some(x_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};

unsafe extern "C" fn x_delete(_: *mut ffi::sqlite3_vfs, _: *const c_char, _: c_int) -> c_int {
    ffi::SQLITE_READONLY
}
unsafe extern "C" fn x_access(
    _: *mut ffi::sqlite3_vfs,
    _: *const c_char,
    _: c_int,
    out: *mut c_int,
) -> c_int {
    // SAFETY: SQLite supplies output storage. No journal, WAL, or sidecars exist.
    unsafe {
        *out = 0;
    }
    ffi::SQLITE_OK
}
unsafe extern "C" fn x_full_pathname(
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    // SAFETY: SQLite supplies a NUL-terminated name and size bytes of output.
    unsafe {
        let bytes = CStr::from_ptr(name).to_bytes_with_nul();
        if size < 0 || bytes.len() > size as usize {
            return ffi::SQLITE_CANTOPEN;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast(), bytes.len());
    }
    ffi::SQLITE_OK
}

// Delegate only platform services, always with the base VFS pointer. Database
// opens and I/O never fall through to local files or another replica.
macro_rules! platform {
    ($name:ident, $field:ident, ($($arg:ident : $ty:ty),*), $failure:expr) => {
        unsafe extern "C" fn $name(_: *mut ffi::sqlite3_vfs, $($arg: $ty),*) -> c_int {
            // SAFETY: the default VFS belongs to SQLite and outlives this private
            // registration. Arguments retain the base callback's exact ABI.
            unsafe {
                let base = ffi::sqlite3_vfs_find(std::ptr::null());
                if base.is_null() { return $failure; }
                match (*base).$field { Some(call) => call(base, $($arg),*), None => $failure }
            }
        }
    };
}
platform!(x_randomness, xRandomness, (size: c_int, out: *mut c_char), 0);
platform!(x_sleep, xSleep, (micros: c_int), 0);
platform!(x_current_time, xCurrentTime, (out: *mut f64), ffi::SQLITE_ERROR);
