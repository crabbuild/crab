//! Records SQLite's committed WAL boundary so a torn/corrupt WAL cannot silently
//! turn a successful application commit into an older successful capture.

use std::ffi::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::Result;
use rusqlite::{Connection, ffi};

#[derive(Clone, Copy)]
pub(crate) struct WalCut {
    pub salt1: u32,
    pub salt2: u32,
    pub frames: u32,
}

pub(crate) struct CommitObserver {
    frames: Box<AtomicU32>,
}

impl CommitObserver {
    pub fn install(connection: &Connection) -> Self {
        let mut observer = Self {
            frames: Box::new(AtomicU32::new(0)),
        };
        // SAFETY: the boxed atomic has a stable address and outlives the writer.
        // The callback neither unwinds nor calls SQLite. Db drops its
        // writer before this observer, including on implicit drop.
        unsafe {
            ffi::sqlite3_wal_hook(
                connection.handle(),
                Some(committed),
                (&mut *observer.frames as *mut AtomicU32).cast(),
            );
        }
        observer
    }

    pub fn reset(&self) {
        self.frames.store(0, Ordering::Relaxed);
    }
    pub fn frames(&self) -> u32 {
        self.frames.load(Ordering::Relaxed)
    }

    pub fn cut(&self, path: &Path, host: &crate::Host) -> Result<Option<WalCut>> {
        let frames = self.frames();
        if frames == 0 {
            return Ok(None);
        }
        let mut wal_path = path.as_os_str().to_owned();
        wal_path.push("-wal");
        let header = host
            .filesystem
            .open(Path::new(&wal_path))?
            .read_exact_at(0, 32)?;
        if header.len() != 32 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        let salt1 = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
        let salt2 = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
        Ok(Some(WalCut {
            salt1,
            salt2,
            frames,
        }))
    }
}

unsafe extern "C" fn committed(
    data: *mut c_void,
    _db: *mut ffi::sqlite3,
    _name: *const c_char,
    frames: c_int,
) -> c_int {
    // SAFETY: only CommitObserver::install supplies this pointer; its allocation
    // remains live until after the connection closes. SQLite calls synchronously.
    let counter = unsafe { &*data.cast::<AtomicU32>() };
    counter.store(frames as u32, Ordering::Relaxed);
    ffi::SQLITE_OK
}
