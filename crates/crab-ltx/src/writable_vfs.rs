//! Writable sparse-file adaptation of Celld paged_vfs.rs; see UPSTREAM.md.
//! A static VFS and per-open Arc ownership keep SQLite discovery memory-safe.

use crate::{CrabError, Result, paged_io::Io};
use rusqlite::{Connection, ffi};
use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

/// Progress resolving an inherited cut: locally materialized or superseded pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hydration {
    pub resolved: u32,
    pub total: u32,
    pub faults: u64,
}

impl Hydration {
    #[must_use]
    pub fn complete(self) -> bool {
        self.resolved == self.total
    }
}

struct State {
    present: Vec<bool>,
    ceiling: u32,
    resolved: u32,
    faults: u64,
}

struct App {
    io: Io,
    page_size: u32,
    count: u32,
    local_disk: crate::DiskReservation,
    state: Mutex<State>,
    error: Mutex<Option<CrabError>>,
}

fn views() -> &'static Mutex<HashMap<PathBuf, Arc<App>>> {
    static VIEWS: OnceLock<Mutex<HashMap<PathBuf, Arc<App>>>> = OnceLock::new();
    VIEWS.get_or_init(Mutex::default)
}

pub(crate) struct Registration {
    path: PathBuf,
    app: Arc<App>,
    cursor: u32,
    vfs: &'static str,
}

impl Registration {
    pub(crate) fn new(database: crate::paged_io::Database, path: &Path) -> Result<Self> {
        let host = database.host();
        let vfs = register_for(host.sqlite_vfs.as_deref())?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let path = host.filesystem.canonicalize(parent)?.join(
            path.file_name()
                .ok_or(CrabError::InvalidState("missing database filename"))?,
        );
        if path.to_str().is_none() {
            return Err(CrabError::InvalidState("database path must be UTF-8"));
        }
        crate::recovery::reject_sidecars(&path, &host)?;
        let count = database.page_count();
        let page_size = database.page_size();
        let mut registry = views()
            .lock()
            .map_err(|_| CrabError::InvalidState("sparse registry poisoned"))?;
        if registry.contains_key(&path) {
            return Err(CrabError::InvalidState(
                "sparse activation already registered",
            ));
        }
        // Only a fresh file can receive a cut's missing-page map. An interrupted
        // activation is quarantined, never reopened as though its holes were data.
        let mut file = host.filesystem.create(&path)?;
        file.set_len(u64::from(count) * u64::from(page_size))?;
        file.sync_all()?;
        host.filesystem.sync_parent(&path)?;
        let app = Arc::new(App {
            io: Io::new(database)?,
            page_size,
            count,
            local_disk: host.reserve_local_disk(0)?,
            state: Mutex::new(State {
                present: vec![false; count as usize],
                ceiling: count,
                resolved: 0,
                faults: 0,
            }),
            error: Mutex::new(None),
        });
        registry.insert(path.clone(), app.clone());
        Ok(Self {
            path,
            app,
            cursor: 1,
            vfs,
        })
    }

    pub(crate) fn vfs(&self) -> &'static str {
        self.vfs
    }

    pub(crate) fn hydration(&self) -> Result<Hydration> {
        let state = self
            .app
            .state
            .lock()
            .map_err(|_| CrabError::InvalidState("sparse state poisoned"))?;
        Ok(Hydration {
            resolved: state.resolved,
            total: self.app.count
                - u32::from(crate::ltx::lock_pgno(self.app.page_size) <= self.app.count),
            faults: state.faults,
        })
    }

    pub(crate) fn step(&mut self, connection: &Connection, pages: u32) -> Result<Hydration> {
        let start = self.hydration()?.resolved;
        while self.cursor <= self.app.count && self.hydration()?.resolved - start < pages {
            let page = self.cursor;
            let present = self
                .app
                .state
                .lock()
                .map_err(|_| CrabError::InvalidState("sparse state poisoned"))?
                .present[page as usize - 1];
            if !present && page != crate::ltx::lock_pgno(self.app.page_size) {
                crate::db::read_main(
                    connection,
                    u64::from(page - 1) * u64::from(self.app.page_size),
                    self.app.page_size as usize,
                )
                .map_err(|error| self.take_error().unwrap_or(error))?;
            }
            // Failed reads leave the cursor on the missing page, so retries
            // cannot falsely declare hydration complete.
            self.cursor += 1;
        }
        self.hydration()
    }

    pub(crate) fn take_error(&self) -> Option<CrabError> {
        self.app.error.lock().ok().and_then(|mut e| e.take())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut registry) = views().lock() {
            registry.remove(&self.path);
        }
    }
}

#[repr(C)]
struct File {
    methods: *const ffi::sqlite3_io_methods,
    base: *mut ffi::sqlite3_file,
    app: *const App,
}

fn sqlite(rc: c_int) -> Result<()> {
    if rc == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None).into())
    }
}

fn mark(app: &App, state: &mut State, page: u32) {
    if page == 0 || page > app.count || page == crate::ltx::lock_pgno(app.page_size) {
        return;
    }
    if !state.present[page as usize - 1] {
        state.present[page as usize - 1] = true;
        state.resolved += 1;
    }
}

unsafe fn hydrate(file: *mut File, first: u32, last: u32) -> Result<()> {
    // SAFETY: xOpen owns an Arc for this file until xClose. The base SQLite
    // file remains open and its callbacks use SQLite's exact ABI.
    unsafe {
        let app = &*(*file).app;
        for page in first..=last.min(app.count) {
            if page == crate::ltx::lock_pgno(app.page_size) {
                continue;
            }
            {
                let state = app
                    .state
                    .lock()
                    .map_err(|_| CrabError::InvalidState("sparse state poisoned"))?;
                if page > state.ceiling || state.present[page as usize - 1] {
                    continue;
                }
            }
            let bytes = app.io.page(page)?;
            let mut state = app
                .state
                .lock()
                .map_err(|_| CrabError::InvalidState("sparse state poisoned"))?;
            state.faults += 1;
            // A checkpoint or truncate may have won while the fetch ran. Keep
            // the recheck and local write under the same gate as xWrite/xTruncate.
            if page > state.ceiling || state.present[page as usize - 1] {
                continue;
            }
            if bytes.len() != app.page_size as usize {
                return Err(CrabError::LTXCorrupted);
            }
            app.local_disk.try_grow(u64::from(app.page_size))?;
            let base = (*file).base;
            let write = (*(*base).pMethods)
                .xWrite
                .ok_or(CrabError::InvalidState("base VFS lacks xWrite"))?;
            sqlite(write(
                base,
                bytes.as_ptr().cast(),
                bytes.len() as c_int,
                i64::from(page - 1) * i64::from(app.page_size),
            ))?;
            mark(app, &mut state, page);
        }
        Ok(())
    }
}

fn page_range(amount: c_int, offset: i64, page_size: u32) -> Result<(u32, u32)> {
    if amount <= 0 || offset < 0 {
        return Err(CrabError::LTXCorrupted);
    }
    let end = offset
        .checked_add(i64::from(amount) - 1)
        .ok_or(CrabError::LTXCorrupted)?;
    Ok((
        u32::try_from(offset / i64::from(page_size) + 1).map_err(|_| CrabError::LTXCorrupted)?,
        u32::try_from(end / i64::from(page_size) + 1).map_err(|_| CrabError::LTXCorrupted)?,
    ))
}

unsafe fn guarded(
    file: *mut File,
    operation: impl FnOnce() -> Result<()>,
    failure: c_int,
) -> c_int {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
        Ok(Ok(())) => ffi::SQLITE_OK,
        Ok(Err(error)) => {
            // SAFETY: the file's Arc is live throughout this callback.
            unsafe {
                if !(*file).app.is_null()
                    && let Ok(mut slot) = (*(*file).app).error.lock()
                {
                    *slot = Some(error);
                }
            }
            failure
        }
        Err(_) => failure,
    }
}

unsafe extern "C" fn x_read(
    file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    // SAFETY: SQLite supplies live file and amount bytes of writable storage.
    unsafe {
        let file = file.cast::<File>();
        if amount == 0 {
            return ffi::SQLITE_OK;
        }
        if !(*file).app.is_null() {
            let rc = guarded(
                file,
                || {
                    let (first, last) = page_range(amount, offset, (*(*file).app).page_size)?;
                    hydrate(file, first, last)
                },
                ffi::SQLITE_IOERR_READ,
            );
            if rc != ffi::SQLITE_OK {
                return rc;
            }
        }
        let base = (*file).base;
        match (*(*base).pMethods).xRead {
            Some(call) => call(base, buffer, amount, offset),
            None => ffi::SQLITE_IOERR_READ,
        }
    }
}

unsafe extern "C" fn x_write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    // SAFETY: SQLite supplies a live file and amount initialized input bytes.
    unsafe {
        let file = file.cast::<File>();
        let base = (*file).base;
        if (*file).app.is_null() {
            return match (*(*base).pMethods).xWrite {
                Some(call) => call(base, buffer, amount, offset),
                None => ffi::SQLITE_IOERR_WRITE,
            };
        }
        if amount == 0 {
            return ffi::SQLITE_OK;
        }
        guarded(
            file,
            || {
                let app = &*(*file).app;
                let (first, last) = page_range(amount, offset, app.page_size)?;
                // SQLite may update only part of page 1. Resolve its untouched bytes
                // before marking the page present; full-page writes need no fetch.
                if !(offset as u64).is_multiple_of(u64::from(app.page_size)) {
                    hydrate(file, first, first)?;
                }
                if !(offset as u64 + amount as u64).is_multiple_of(u64::from(app.page_size)) {
                    hydrate(file, last, last)?;
                }
                let mut state = app
                    .state
                    .lock()
                    .map_err(|_| CrabError::InvalidState("sparse state poisoned"))?;
                let missing = (first..=last.min(app.count))
                    .filter(|page| {
                        *page != crate::ltx::lock_pgno(app.page_size)
                            && !state.present[*page as usize - 1]
                    })
                    .count() as u64;
                app.local_disk.try_grow(
                    missing
                        .checked_mul(u64::from(app.page_size))
                        .ok_or(CrabError::Limit(crate::LimitKind::LocalDiskBytes))?,
                )?;
                let write = (*(*base).pMethods)
                    .xWrite
                    .ok_or(CrabError::InvalidState("base VFS lacks xWrite"))?;
                sqlite(write(base, buffer, amount, offset))?;
                for page in first..=last.min(app.count) {
                    mark(app, &mut state, page);
                }
                Ok(())
            },
            ffi::SQLITE_IOERR_WRITE,
        )
    }
}

unsafe extern "C" fn x_truncate(file: *mut ffi::sqlite3_file, size: i64) -> c_int {
    // SAFETY: SQLite owns the open wrapper and its base file.
    unsafe {
        let file = file.cast::<File>();
        let base = (*file).base;
        let Some(truncate) = (*(*base).pMethods).xTruncate else {
            return ffi::SQLITE_IOERR_TRUNCATE;
        };
        if (*file).app.is_null() {
            return truncate(base, size);
        }
        guarded(
            file,
            || {
                let app = &*(*file).app;
                if size < 0 || !(size as u64).is_multiple_of(u64::from(app.page_size)) {
                    return Err(CrabError::LTXCorrupted);
                }
                let keep = u32::try_from(size / i64::from(app.page_size))
                    .map_err(|_| CrabError::LTXCorrupted)?;
                let mut state = app
                    .state
                    .lock()
                    .map_err(|_| CrabError::InvalidState("sparse state poisoned"))?;
                sqlite(truncate(base, size))?;
                // Old cut pages beyond a successful truncate are permanently
                // superseded. Regrowth must never resurrect their old remote bytes.
                for page in keep.saturating_add(1)..=state.ceiling {
                    mark(app, &mut state, page);
                }
                state.ceiling = state.ceiling.min(keep);
                Ok(())
            },
            ffi::SQLITE_IOERR_TRUNCATE,
        )
    }
}

unsafe extern "C" fn x_close(file: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: xOpen allocated the base with sqlite3_malloc and transferred one
    // Arc into main files. SQLite invokes xClose once per successful open.
    unsafe {
        let file = file.cast::<File>();
        let base = (*file).base;
        let rc = match (*(*base).pMethods).xClose {
            Some(call) => call(base),
            None => ffi::SQLITE_OK,
        };
        ffi::sqlite3_free(base.cast());
        if !(*file).app.is_null() {
            drop(Arc::from_raw((*file).app));
        }
        (*file).methods = std::ptr::null();
        rc
    }
}

macro_rules! forward_io {
    ($name:ident, $field:ident, ($($arg:ident : $ty:ty),*), $failure:expr) => {
        unsafe extern "C" fn $name(file: *mut ffi::sqlite3_file, $($arg:$ty),*) -> c_int {
            // SAFETY: file wraps an open SQLite base; arguments preserve its ABI.
            unsafe { let base = (*file.cast::<File>()).base;
                match (*(*base).pMethods).$field { Some(call) => call(base, $($arg),*), None => $failure }
            }
        }
    };
}
forward_io!(x_sync, xSync, (flags: c_int), ffi::SQLITE_IOERR_FSYNC);
forward_io!(x_size, xFileSize, (size: *mut i64), ffi::SQLITE_IOERR_FSTAT);
forward_io!(x_lock, xLock, (lock: c_int), ffi::SQLITE_IOERR_LOCK);
forward_io!(x_unlock, xUnlock, (lock: c_int), ffi::SQLITE_IOERR_UNLOCK);
forward_io!(x_reserved, xCheckReservedLock, (out: *mut c_int), ffi::SQLITE_IOERR_CHECKRESERVEDLOCK);
forward_io!(x_control, xFileControl, (op: c_int, arg: *mut c_void), ffi::SQLITE_NOTFOUND);
forward_io!(x_sector, xSectorSize, (), 4096);
forward_io!(x_shm_map, xShmMap, (page: c_int, size: c_int, extend: c_int, out: *mut *mut c_void), ffi::SQLITE_IOERR_SHMMAP);
forward_io!(x_shm_lock, xShmLock, (offset: c_int, n: c_int, flags: c_int), ffi::SQLITE_IOERR_SHMLOCK);
forward_io!(x_shm_unmap, xShmUnmap, (delete: c_int), ffi::SQLITE_IOERR);

unsafe extern "C" fn x_characteristics(_: *mut ffi::sqlite3_file) -> c_int {
    // Sparse faults can write during reads. Do not inherit atomic/batch-write
    // optimizations that can bypass the wrapper's bookkeeping.
    0
}

unsafe extern "C" fn x_shm_barrier(file: *mut ffi::sqlite3_file) {
    // SAFETY: the wrapper's base is open and owns its shared-memory region.
    unsafe {
        let base = (*file.cast::<File>()).base;
        if let Some(call) = (*(*base).pMethods).xShmBarrier {
            call(base);
        }
    }
}

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 2,
    xClose: Some(x_close),
    xRead: Some(x_read),
    xWrite: Some(x_write),
    xTruncate: Some(x_truncate),
    xSync: Some(x_sync),
    xFileSize: Some(x_size),
    xLock: Some(x_lock),
    xUnlock: Some(x_unlock),
    xCheckReservedLock: Some(x_reserved),
    xFileControl: Some(x_control),
    xSectorSize: Some(x_sector),
    xDeviceCharacteristics: Some(x_characteristics),
    xShmMap: Some(x_shm_map),
    xShmLock: Some(x_shm_lock),
    xShmBarrier: Some(x_shm_barrier),
    xShmUnmap: Some(x_shm_unmap),
    xFetch: None,
    xUnfetch: None,
};

unsafe extern "C" fn x_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    // SAFETY: SQLite provides zeroed szOsFile storage and a valid nullable name.
    // The base VFS is a process-lifetime registration pinned in pAppData.
    unsafe {
        (*file).pMethods = std::ptr::null();
        let app = if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
            if name.is_null() || flags & ffi::SQLITE_OPEN_READWRITE == 0 {
                return ffi::SQLITE_CANTOPEN;
            }
            let Ok(path) = CStr::from_ptr(name).to_str() else {
                return ffi::SQLITE_CANTOPEN;
            };
            let Ok(registry) = views().lock() else {
                return ffi::SQLITE_CANTOPEN;
            };
            let Some(app) = registry.get(Path::new(path)).cloned() else {
                return ffi::SQLITE_CANTOPEN;
            };
            Some(app)
        } else {
            None
        };
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        let Some(open) = (*base).xOpen else {
            return ffi::SQLITE_CANTOPEN;
        };
        let base_file = ffi::sqlite3_malloc((*base).szOsFile).cast::<ffi::sqlite3_file>();
        if base_file.is_null() {
            return ffi::SQLITE_NOMEM;
        }
        std::ptr::write_bytes(base_file.cast::<u8>(), 0, (*base).szOsFile as usize);
        let rc = open(base, name, base_file, flags, out);
        if rc != ffi::SQLITE_OK {
            if !(*base_file).pMethods.is_null()
                && let Some(close) = (*(*base_file).pMethods).xClose
            {
                close(base_file);
            }
            ffi::sqlite3_free(base_file.cast());
            return rc;
        }
        let wrapper = file.cast::<File>();
        (*wrapper).base = base_file;
        (*wrapper).app = app.map_or(std::ptr::null(), Arc::into_raw);
        (*wrapper).methods = &METHODS;
        ffi::SQLITE_OK
    }
}

macro_rules! forward_vfs {
    ($name:ident, $field:ident, ($($arg:ident : $ty:ty),*), $failure:expr) => {
        unsafe extern "C" fn $name(vfs: *mut ffi::sqlite3_vfs, $($arg:$ty),*) -> c_int {
            // SAFETY: pAppData pins the host's process-lifetime base VFS; exact ABI.
            unsafe { let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
                match (*base).$field { Some(call) => call(base, $($arg),*), None => $failure }
            }
        }
    };
}
forward_vfs!(x_delete, xDelete, (name: *const c_char, sync: c_int), ffi::SQLITE_IOERR_DELETE);
forward_vfs!(x_access, xAccess, (name: *const c_char, flags: c_int, out: *mut c_int), ffi::SQLITE_IOERR_ACCESS);
forward_vfs!(x_full_path, xFullPathname, (name: *const c_char, size: c_int, out: *mut c_char), ffi::SQLITE_CANTOPEN);
forward_vfs!(x_randomness, xRandomness, (size: c_int, out: *mut c_char), 0);
forward_vfs!(x_sleep, xSleep, (micros: c_int), 0);
forward_vfs!(x_current_time, xCurrentTime, (out: *mut f64), ffi::SQLITE_ERROR);

fn register_for(base: Option<&str>) -> Result<&'static str> {
    static REGISTRATIONS: OnceLock<Mutex<HashMap<Option<String>, &'static str>>> = OnceLock::new();
    let mut registrations = REGISTRATIONS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| CrabError::InvalidState("sparse VFS registry poisoned"))?;
    let key = base.map(str::to_owned);
    if let Some(name) = registrations.get(&key) {
        return Ok(name);
    }
    let name = match base {
        None => "crab-ltx-writable-v1".to_owned(),
        Some(_) => format!("crab-ltx-writable-v1-{}", registrations.len()),
    };
    let c_name =
        CString::new(name.as_str()).map_err(|_| CrabError::InvalidState("invalid VFS name"))?;
    let c_base = base
        .map(CString::new)
        .transpose()
        .map_err(|_| CrabError::InvalidState("invalid base VFS name"))?;
    sqlite(register(c_base.as_deref(), &c_name))?;
    // SQLite stores zName, not a copy. One registration per host VFS remains
    // process-lifetime, so independent connections cannot outlive its memory.
    let _ = c_name.into_raw();
    let name = Box::leak(name.into_boxed_str());
    registrations.insert(key, name);
    Ok(name)
}

fn register(base_name: Option<&CStr>, name: &CStr) -> c_int {
    // SAFETY: optional callbacks are zeroed; mandatory version-1 callbacks are
    // installed before registration. Both VFS structs remain process-lifetime.
    unsafe {
        let rc = ffi::sqlite3_initialize();
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        let base = ffi::sqlite3_vfs_find(base_name.map_or(std::ptr::null(), CStr::as_ptr));
        if base.is_null() || !ffi::sqlite3_vfs_find(name.as_ptr()).is_null() {
            return ffi::SQLITE_ERROR;
        }
        let mut vfs: Box<ffi::sqlite3_vfs> = Box::new(std::mem::zeroed());
        vfs.iVersion = 1;
        vfs.szOsFile = std::mem::size_of::<File>() as c_int;
        vfs.mxPathname = (*base).mxPathname;
        vfs.pAppData = base.cast();
        vfs.zName = name.as_ptr();
        vfs.xOpen = Some(x_open);
        vfs.xDelete = Some(x_delete);
        vfs.xAccess = Some(x_access);
        vfs.xFullPathname = Some(x_full_path);
        vfs.xRandomness = Some(x_randomness);
        vfs.xSleep = Some(x_sleep);
        vfs.xCurrentTime = Some(x_current_time);
        let rc = ffi::sqlite3_vfs_register(&mut *vfs, 0);
        if rc == ffi::SQLITE_OK {
            let _ = Box::into_raw(vfs);
        }
        rc
    }
}

#[cfg(test)]
mod tests {
    use super::page_range;
    use crate::CrabError;

    #[test]
    fn page_range_rejects_non_positive_amounts() {
        assert!(matches!(
            page_range(0, 0, 4096),
            Err(CrabError::LTXCorrupted)
        ));
        assert!(matches!(
            page_range(-1, 0, 4096),
            Err(CrabError::LTXCorrupted)
        ));
    }

    #[test]
    fn page_range_rejects_negative_offsets() {
        assert!(matches!(
            page_range(1, -1, 4096),
            Err(CrabError::LTXCorrupted)
        ));
    }

    #[test]
    fn page_range_keeps_a_partial_range_inside_one_page() {
        assert_eq!(page_range(1, 0, 4096).unwrap(), (1, 1));
        assert_eq!(page_range(4096, 0, 4096).unwrap(), (1, 1));
    }

    #[test]
    fn page_range_includes_both_ends_of_a_crossing_range() {
        assert_eq!(page_range(1, 4096, 4096).unwrap(), (2, 2));
        assert_eq!(page_range(2, 4095, 4096).unwrap(), (1, 2));
        assert_eq!(page_range(4096, 4096, 4096).unwrap(), (2, 2));
    }

    #[test]
    fn page_range_rejects_a_range_that_overflows_the_offset() {
        assert!(matches!(
            page_range(2, i64::MAX, 4096),
            Err(CrabError::LTXCorrupted)
        ));
    }

    #[test]
    fn page_range_rejects_a_page_number_past_the_cartesian_ceiling() {
        let offset = 4096 * i64::from(u32::MAX);
        assert!(matches!(
            page_range(1, offset, 4096),
            Err(CrabError::LTXCorrupted)
        ));
    }
}
