use std::cell::Cell;
use std::ffi::{CStr, CString, OsStr, c_char, c_int};
use std::fs::File;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::ffi;

use super::super::{Directory, validate_metadata};
use super::generation::Generation;
use super::locking::DatabaseLock;
use super::shm::SharedMemory;
use crate::private_fs::DatabaseMode;
use crate::{CacheError, Result};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub(super) const VIRTUAL_DATABASE: &str = "/crab.sqlite";

pub(super) struct Context {
    pub(super) directory: Directory,
    name: CString,
    native: *mut ffi::sqlite3_vfs,
    busy_timeout: Duration,
    generation: Arc<Generation>,
    pub(super) read_only: bool,
    pub(super) exclusive: Cell<bool>,
}

impl Context {
    fn mutation(&self) -> std::result::Result<super::super::MutationGuard<'_>, i32> {
        let started = Instant::now();
        loop {
            match self.directory.mutation() {
                Ok(guard) => return Ok(guard),
                Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let remaining = self.busy_timeout.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(ffi::SQLITE_BUSY);
                    }
                    std::thread::sleep(Duration::from_millis(1).min(remaining));
                }
                Err(error) => return Err(cache_code(error)),
            }
        }
    }

    pub(super) fn component(&self, virtual_name: &CStr) -> std::result::Result<CString, i32> {
        let suffix = virtual_name
            .to_bytes()
            .strip_prefix(VIRTUAL_DATABASE.as_bytes())
            .ok_or(ffi::SQLITE_CANTOPEN)?;
        if ![b"".as_slice(), b"-journal", b"-wal", b"-shm"].contains(&suffix) {
            return Err(ffi::SQLITE_CANTOPEN);
        }
        let mut name = self.name.as_bytes().to_vec();
        name.extend_from_slice(suffix);
        CString::new(name).map_err(|_| ffi::SQLITE_CANTOPEN)
    }

    pub(super) fn open(
        &self,
        name: &CStr,
        create: bool,
        exclusive: bool,
        read_only: bool,
    ) -> std::result::Result<Option<File>, i32> {
        // Serialize namespace changes with other Crab openers/removers. macOS
        // openat(O_CREAT) can report ENOENT when the leaf is concurrently
        // unlinked even though the pinned parent still exists.
        let _mutation = self.mutation()?;
        self.validate_generation()?;
        if name == self.name.as_c_str() {
            // SQLite must use the inode bound under the initialization lock.
            // Reopening the pathname here could select another generation.
            return self.generation.main.try_clone().map(Some).map_err(io_code);
        }
        let path = self.directory.path.join(OsStr::from_bytes(name.to_bytes()));
        validate_metadata(
            &self.directory.file.metadata().map_err(io_code)?,
            &self.directory.path,
            true,
        )
        .map_err(cache_code)?;
        let read_only = read_only || self.read_only;
        let flags = if read_only {
            libc::O_RDONLY
        } else {
            libc::O_RDWR
        };
        let flags = flags
            | if create && !read_only {
                libc::O_CREAT
            } else {
                0
            }
            | if exclusive { libc::O_EXCL } else { 0 };
        let file = match self.directory.open_component(name, flags, &path) {
            Ok(file) => file,
            Err(CacheError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound
                    && self.read_only
                    && self.exclusive.get()
                    && name == self.side_name(b"-wal")?.as_c_str() =>
            {
                // A quiet WAL-mode main still makes SQLite request a WAL.
                // Its proven absence under EXCLUSIVE is an empty read-only
                // handle, never permission to create or ignore an existing WAL.
                return Ok(None);
            }
            Err(error) => {
                // SQLite retries a disappeared hot journal under EXCLUSIVE
                // only for CANTOPEN. A generic IOERR makes that normal unlink
                // race fatal; match the native VFS's open-error contract.
                tracing::debug!(%error, "private SQLite open failed");
                return Err(ffi::SQLITE_CANTOPEN);
            }
        };
        validate_metadata(&file.metadata().map_err(io_code)?, &path, false).map_err(cache_code)?;
        Ok(Some(file))
    }

    pub(super) fn side_name(&self, suffix: &[u8]) -> std::result::Result<CString, i32> {
        let mut name = self.name.as_bytes().to_vec();
        name.extend_from_slice(suffix);
        CString::new(name).map_err(|_| ffi::SQLITE_CANTOPEN)
    }

    pub(super) fn validate_generation(&self) -> std::result::Result<(), i32> {
        self.generation.validate(&self.directory, &self.name)
    }

    pub(super) fn main_lock_changed(&self, name: &CStr, level: i32) {
        if name == self.name.as_c_str() {
            self.exclusive.set(level == ffi::SQLITE_LOCK_EXCLUSIVE);
        }
    }

    pub(super) fn unlink(&self, name: &CString) -> std::result::Result<(), i32> {
        if self.read_only {
            return Err(ffi::SQLITE_READONLY);
        }
        // Never wait for SQLite while holding this short namespace lock.
        // Bounded waiting uses the owning caller's existing busy policy.
        let _mutation = self.mutation()?;
        self.validate_generation()?;
        self.directory.remove(name).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ffi::SQLITE_IOERR_DELETE_NOENT
            } else {
                ffi::SQLITE_IOERR_DELETE
            }
        })
    }
}

pub(super) struct Registration {
    vfs: Box<ffi::sqlite3_vfs>,
    _name: CString,
    _context: Box<Context>,
}

// SAFETY: the boxes and name retain stable addresses when moved. SQLite calls
// this connection's callbacks synchronously; Connection is Send but not Sync.
// Registration/unregistration use SQLite's mutex-protected public registry.
unsafe impl Send for Registration {}

impl Registration {
    pub(super) fn generation(&self) -> Arc<Generation> {
        Arc::clone(&self._context.generation)
    }

    pub(super) fn new(
        directory: Directory,
        name: CString,
        generation: Arc<Generation>,
        busy_timeout: Duration,
        mode: DatabaseMode,
    ) -> Result<Self> {
        // SAFETY: SQLite initializes and serializes its built-in VFS registry.
        let native = unsafe { ffi::sqlite3_vfs_find(c"unix".as_ptr()) };
        if native.is_null() {
            return Err(CacheError::Io(std::io::Error::other(
                "SQLite has no native VFS",
            )));
        }
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name_string = format!("crab-private-{}-{sequence}", std::process::id());
        let vfs_name = CString::new(name_string)
            .map_err(|error| CacheError::Io(std::io::Error::other(error)))?;
        let mut context = Box::new(Context {
            directory,
            name,
            native,
            busy_timeout,
            generation,
            read_only: mode == DatabaseMode::ReadOnly,
            exclusive: Cell::new(false),
        });
        let mut vfs = Box::new(ffi::sqlite3_vfs {
            iVersion: 2,
            szOsFile: std::mem::size_of::<SqlFile>() as c_int,
            mxPathname: 512,
            pNext: ptr::null_mut(),
            zName: vfs_name.as_ptr(),
            pAppData: ptr::from_mut(context.as_mut()).cast(),
            xOpen: Some(open),
            xDelete: Some(delete),
            xAccess: Some(access),
            xFullPathname: Some(full_path),
            xDlOpen: None,
            xDlError: None,
            xDlSym: None,
            xDlClose: None,
            xRandomness: Some(randomness),
            xSleep: Some(sleep),
            xCurrentTime: Some(current_time),
            xGetLastError: None,
            xCurrentTimeInt64: Some(current_time_int64),
            xSetSystemCall: None,
            xGetSystemCall: None,
            xNextSystemCall: None,
        });
        // SAFETY: every callback, name, and context remains live until the
        // connection has closed; zero leaves the process default unchanged.
        let code = unsafe { ffi::sqlite3_vfs_register(vfs.as_mut(), 0) };
        if code != ffi::SQLITE_OK {
            return Err(index_error(&context.directory.path, code));
        }
        Ok(Self {
            vfs,
            _name: vfs_name,
            _context: context,
        })
    }

    pub(super) fn name(&self) -> Result<&str> {
        self._name
            .to_str()
            .map_err(|error| CacheError::Io(std::io::Error::other(error)))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: Database closes its connection before dropping this owner.
        // The registry owns no allocation and cannot call an unregistered VFS.
        unsafe { ffi::sqlite3_vfs_unregister(self.vfs.as_mut()) };
    }
}

#[repr(C)]
pub(super) struct SqlFile {
    base: ffi::sqlite3_file,
    state: *mut FileState,
}

pub(super) struct FileState {
    // None is exclusively a missing WAL proven under the main EXCLUSIVE lock.
    pub(super) file: Option<File>,
    pub(super) context: *const Context,
    pub(super) name: CString,
    pub(super) locks: DatabaseLock,
    pub(super) shm: Option<SharedMemory>,
    pub(super) read_only: bool,
    pub(super) persist_wal: bool,
    pub(super) sync_directory: bool,
}

pub(super) fn with_file(
    raw: *mut ffi::sqlite3_file,
    operation: impl FnOnce(&mut FileState) -> std::result::Result<(), i32>,
) -> i32 {
    boundary(|| {
        // SAFETY: SQLite provides the live SqlFile storage initialized by
        // xOpen and serializes callbacks for this NO_MUTEX connection.
        let state = unsafe { &mut *(*raw.cast::<SqlFile>()).state };
        operation(state)
    })
}

pub(super) fn context(raw: *mut ffi::sqlite3_vfs) -> &'static Context {
    // SAFETY: callbacks run only while Database retains the registration.
    // The reference never escapes a callback or its owned file state.
    unsafe { &*(*raw).pAppData.cast::<Context>() }
}

pub(super) fn file_context(state: &FileState) -> &Context {
    // SAFETY: every FileState closes before its registration is released.
    unsafe { &*state.context }
}

pub(super) fn boundary(operation: impl FnOnce() -> std::result::Result<(), i32>) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
        Ok(Ok(())) => ffi::SQLITE_OK,
        Ok(Err(code)) => code,
        Err(_) => ffi::SQLITE_IOERR,
    }
}

pub(super) fn io_code(error: std::io::Error) -> i32 {
    match error.raw_os_error() {
        Some(libc::ENOSPC | libc::EDQUOT) => ffi::SQLITE_FULL,
        _ => ffi::SQLITE_IOERR,
    }
}

fn cache_code(error: CacheError) -> i32 {
    match error {
        CacheError::Io(error) if error.kind() == std::io::ErrorKind::WouldBlock => ffi::SQLITE_BUSY,
        CacheError::Io(error) => io_code(error),
        _ => ffi::SQLITE_CANTOPEN,
    }
}

fn index_error(path: &Path, code: i32) -> CacheError {
    CacheError::Index {
        path: path.display().to_string(),
        source: rusqlite::Error::SqliteFailure(ffi::Error::new(code), None),
    }
}

unsafe extern "C" fn open(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    raw: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    boundary(|| {
        let context = context(vfs);
        let temporary = name.is_null();
        let name = if temporary {
            if flags & ffi::SQLITE_OPEN_DELETEONCLOSE == 0 {
                return Err(ffi::SQLITE_CANTOPEN);
            }
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            CString::new(format!(".sqlite-temp-{}-{sequence}", std::process::id()))
                .map_err(|_| ffi::SQLITE_CANTOPEN)?
        } else {
            // SAFETY: SQLite supplies a NUL-terminated filename for named opens.
            context.component(unsafe { CStr::from_ptr(name) })?
        };
        if temporary && context.read_only {
            return Err(ffi::SQLITE_READONLY);
        }
        let create = flags & ffi::SQLITE_OPEN_CREATE != 0 && !context.read_only;
        let read_only = flags & ffi::SQLITE_OPEN_READONLY != 0 || context.read_only;
        let file = context.open(
            &name,
            create,
            temporary || flags & ffi::SQLITE_OPEN_EXCLUSIVE != 0,
            read_only,
        )?;
        if temporary {
            // Unnamed temporary state cannot be redirected during close and
            // is reclaimed by the OS even when its process is killed.
            context.unlink(&name)?;
        }
        let state = Box::new(FileState {
            file,
            context,
            name,
            locks: DatabaseLock::default(),
            shm: None,
            read_only,
            persist_wal: false,
            sync_directory: create && !temporary,
        });
        // SAFETY: SQLite allocated szOsFile aligned storage. pMethods becomes
        // non-null only after all fallible initialization has succeeded.
        unsafe {
            ptr::write(
                raw.cast::<SqlFile>(),
                SqlFile {
                    base: ffi::sqlite3_file {
                        pMethods: &super::file::METHODS,
                    },
                    state: Box::into_raw(state),
                },
            );
            if !out_flags.is_null() {
                *out_flags = if context.read_only {
                    (flags & !(ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE))
                        | ffi::SQLITE_OPEN_READONLY
                } else {
                    flags
                };
            }
        }
        Ok(())
    })
}

pub(super) unsafe extern "C" fn close(raw: *mut ffi::sqlite3_file) -> c_int {
    boundary(|| {
        // SAFETY: xOpen transferred this Box to SQLite; xClose runs once and
        // clears its pointer before reclaiming all file/mapping ownership.
        let state = unsafe {
            let raw = &mut *raw.cast::<SqlFile>();
            raw.base.pMethods = ptr::null();
            Box::from_raw(ptr::replace(&mut raw.state, ptr::null_mut()))
        };
        // A generation lease can retain the main description after xClose.
        // Release its OFD byte locks explicitly, including partial acquisitions;
        // closing this cloned fd alone no longer releases them.
        let result = state.file.as_ref().map_or(Ok(()), |file| {
            super::locking::lock(file, super::locking::UNLOCK, 0, 0)
        });
        file_context(&state).main_lock_changed(&state.name, ffi::SQLITE_LOCK_NONE);
        drop(state);
        result
    })
}

unsafe extern "C" fn delete(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    sync_directory: c_int,
) -> c_int {
    boundary(|| {
        let context = context(vfs);
        // SAFETY: SQLite supplies a live NUL-terminated name to xDelete.
        let component = context.component(unsafe { CStr::from_ptr(name) })?;
        context.unlink(&component)?;
        if sync_directory != 0 {
            context.directory.file.sync_all().map_err(io_code)?;
        }
        Ok(())
    })
}

unsafe extern "C" fn access(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    flags: c_int,
    result: *mut c_int,
) -> c_int {
    boundary(|| {
        // SAFETY: SQLite provides an output integer and terminated name.
        unsafe {
            *result = 0;
        }
        let context = context(vfs);
        let component = context.component(unsafe { CStr::from_ptr(name) })?;
        context.validate_generation()?;
        let stat = match context.directory.stat_component(&component) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_code(error)),
        };
        super::super::validate_permissions(stat.st_mode, stat.st_uid, &context.directory.path)
            .map_err(cache_code)?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_nlink > 1 {
            return Err(ffi::SQLITE_CANTOPEN);
        }
        let exists =
            stat.st_nlink == 1 && (flags != ffi::SQLITE_ACCESS_EXISTS || stat.st_size != 0);
        // SAFETY: result is SQLite's valid output pointer for this callback.
        unsafe {
            *result = c_int::from(exists);
        }
        Ok(())
    })
}

unsafe extern "C" fn full_path(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    capacity: c_int,
    output: *mut c_char,
) -> c_int {
    boundary(|| {
        // SAFETY: SQLite supplies a terminated name and capacity-byte output.
        let name = unsafe { CStr::from_ptr(name) };
        context(vfs).component(name)?;
        if capacity < 0 || name.to_bytes_with_nul().len() > capacity as usize {
            return Err(ffi::SQLITE_CANTOPEN);
        }
        // SAFETY: checked output size; SQLite's input and output do not overlap.
        unsafe {
            ptr::copy_nonoverlapping(name.as_ptr(), output, name.to_bytes_with_nul().len());
        }
        Ok(())
    })
}

unsafe extern "C" fn randomness(
    vfs: *mut ffi::sqlite3_vfs,
    length: c_int,
    output: *mut c_char,
) -> c_int {
    let native = context(vfs).native;
    // SAFETY: the built-in VFS lives for SQLite's process lifetime, and the
    // delegated callback receives its original VFS and SQLite output buffer.
    unsafe {
        (*native)
            .xRandomness
            .map_or(0, |call| call(native, length, output))
    }
}

unsafe extern "C" fn sleep(vfs: *mut ffi::sqlite3_vfs, microseconds: c_int) -> c_int {
    let native = context(vfs).native;
    // SAFETY: delegate only time behavior to the original built-in VFS.
    unsafe {
        (*native)
            .xSleep
            .map_or(0, |call| call(native, microseconds))
    }
}

unsafe extern "C" fn current_time(vfs: *mut ffi::sqlite3_vfs, output: *mut f64) -> c_int {
    let native = context(vfs).native;
    // SAFETY: SQLite's output pointer matches the built-in callback contract.
    unsafe {
        (*native)
            .xCurrentTime
            .map_or(ffi::SQLITE_ERROR, |call| call(native, output))
    }
}

unsafe extern "C" fn current_time_int64(vfs: *mut ffi::sqlite3_vfs, output: *mut i64) -> c_int {
    let native = context(vfs).native;
    // SAFETY: the selected built-in VFS is version 2+ in pinned SQLite.
    unsafe {
        (*native)
            .xCurrentTimeInt64
            .map_or(ffi::SQLITE_ERROR, |call| call(native, output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_rejects_writes_and_requires_exclusion_for_an_absent_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let path = root.join("entry");
        let writer =
            super::super::open_database(&root, &path, DatabaseMode::Create, Duration::ZERO)
                .unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE entries(value); INSERT INTO entries VALUES(1)").unwrap();
        drop(writer);
        let reader =
            super::super::open_database(&root, &path, DatabaseMode::ReadOnly, Duration::ZERO)
                .unwrap();
        let context = &reader.registration._context;
        assert!(context.open(c"entry-wal", true, false, false).is_err());
        assert_eq!(
            reader
                .query_row("SELECT value FROM entries", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(
            context
                .open(c"entry-wal", true, false, false)
                .unwrap()
                .is_none()
        );
        assert!(context.open(c"entry-journal", true, false, false).is_err());
        assert_eq!(
            context.unlink(&c"entry".to_owned()),
            Err(ffi::SQLITE_READONLY)
        );
        assert!(matches!(
            SharedMemory::open(context),
            Err(ffi::SQLITE_READONLY)
        ));
        for sql in ["INSERT INTO entries VALUES(2)", "PRAGMA user_version=7"] {
            assert_eq!(
                reader.execute_batch(sql).unwrap_err().sqlite_error_code(),
                Some(rusqlite::ErrorCode::ReadOnly)
            );
        }

        let mut file: *mut ffi::sqlite3_file = ptr::null_mut();
        // SAFETY: FILE_POINTER returns this live connection's main sqlite3_file;
        // it stays owned by reader throughout the synchronous callback probes.
        assert_eq!(
            unsafe {
                ffi::sqlite3_file_control(
                    reader.handle(),
                    c"main".as_ptr(),
                    ffi::SQLITE_FCNTL_FILE_POINTER,
                    ptr::from_mut(&mut file).cast(),
                )
            },
            ffi::SQLITE_OK
        );
        assert!(!file.is_null());
        // SAFETY: SQLite returned its valid file and method table; both input
        // buffers have the lengths required by these callbacks.
        unsafe {
            let methods = &*(*file).pMethods;
            assert_eq!(
                methods.xWrite.unwrap()(file, b"x".as_ptr().cast(), 1, 0),
                ffi::SQLITE_READONLY
            );
            assert_eq!(methods.xTruncate.unwrap()(file, 0), ffi::SQLITE_READONLY);
        }
        drop(reader);
        assert!(!root.join("entry-wal").exists());
        assert!(!root.join("entry-shm").exists());
    }

    #[test]
    fn close_releases_sqlite_locks_even_when_generation_retains_the_main_description() {
        use super::super::locking::{PENDING, UNLOCK, WRITE_LOCK, conflicting_lock, lock};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let path = root.join("entry");
        let connection = super::super::open_database(
            &root,
            &path,
            crate::private_fs::DatabaseMode::Create,
            Duration::ZERO,
        )
        .unwrap();
        let context = &connection.registration._context;
        let file = context
            .open(c"entry", false, false, false)
            .unwrap()
            .unwrap();
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        lock(&file, WRITE_LOCK, PENDING, 1).unwrap();
        assert_eq!(conflicting_lock(&probe, PENDING, 1).unwrap(), WRITE_LOCK);
        let state = Box::new(FileState {
            file: Some(file),
            context: ptr::from_ref(context.as_ref()),
            name: c"entry".to_owned(),
            locks: DatabaseLock::default(),
            shm: None,
            read_only: false,
            persist_wal: false,
            sync_directory: false,
        });
        let mut raw = SqlFile {
            base: ffi::sqlite3_file {
                pMethods: &super::super::file::METHODS,
            },
            state: Box::into_raw(state),
        };
        // SAFETY: the fixture transfers one complete FileState to the ordinary
        // close callback; the registration/context and retained main stay live.
        assert_eq!(
            unsafe { close(ptr::from_mut(&mut raw).cast()) },
            ffi::SQLITE_OK
        );
        assert_eq!(conflicting_lock(&probe, PENDING, 1).unwrap(), UNLOCK);
    }

    #[test]
    fn private_namespace_creation_and_removal_share_the_same_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        Directory::root(&root, true).unwrap();
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let start = &start;
                let root = &root;
                scope.spawn(move || {
                    let connection = super::super::open_database(
                        root,
                        &root.join("entry"),
                        crate::private_fs::DatabaseMode::Create,
                        Duration::from_secs(5),
                    )
                    .unwrap();
                    let context = &connection.registration._context;
                    start.wait();
                    for _ in 0..100 {
                        let result = context.open(c"entry-journal", true, false, false);
                        assert!(result.is_ok(), "{result:?}");
                        let _ = context.unlink(&c"entry-journal".to_owned());
                    }
                });
            }
        });
    }
}
