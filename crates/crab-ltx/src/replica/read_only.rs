//! Owned read-only SQLite view of one verified immutable Cell root.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use rusqlite::{Connection, OpenFlags};

use super::{RootRef, VerifiedRoot};
use crate::{CrabError, DiskReservation, Host, Result};

/// A verified root restored to a private SQLite file opened without write access.
///
/// Queries hold the connection lock through execution. The file is disposable;
/// Cell ownership and root freshness remain the caller's responsibility.
pub struct ReadOnlyRoot {
    root: RootRef,
    connection: Option<Mutex<Connection>>,
    destination: PathBuf,
    host: Host,
    installed: bool,
    _disk: DiskReservation,
}

impl ReadOnlyRoot {
    pub(super) async fn open(root: &VerifiedRoot, destination: &Path) -> Result<Self> {
        let host = root.pages.replica.host.clone();
        crate::recovery::reject_sidecars(destination, &host)?;
        if host.filesystem.exists(destination)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "read-only root destination already exists",
            )
            .into());
        }
        let bytes = u64::from(root.database_pages)
            .checked_mul(u64::from(root.page_size))
            .ok_or(CrabError::LTXCorrupted)?;
        let disk = host.reserve_local_disk(bytes)?;
        let mut view = Self {
            root: root.root,
            connection: None,
            destination: destination.to_owned(),
            host,
            installed: false,
            _disk: disk,
        };
        root.restore(destination).await?;
        view.installed = true;
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = match view.host.sqlite_vfs.as_deref() {
            Some(vfs) => Connection::open_with_flags_and_vfs(destination, flags, vfs)?,
            None => Connection::open_with_flags(destination, flags)?,
        };
        crate::db::configure_managed_connection(&connection)?;
        connection.pragma_update(None, "query_only", true)?;
        view.connection = Some(Mutex::new(connection));
        Ok(view)
    }

    /// Returns the immutable root this view reads.
    #[must_use]
    pub fn root(&self) -> RootRef {
        self.root
    }

    /// Locks the read-only SQLite connection for one bounded query.
    pub fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .as_ref()
            .ok_or(CrabError::InvalidState("read-only root is closed"))?
            .lock()
            .map_err(|_| CrabError::InvalidState("read-only root connection poisoned"))
    }
}

impl Drop for ReadOnlyRoot {
    fn drop(&mut self) {
        // SQLite must release its handle before the private file is removed.
        self.connection.take();
        // A failed restore may have raced with another installer; only remove
        // a destination after this view installed it successfully.
        if self.installed {
            let _ = self.host.filesystem.remove_file(&self.destination);
        }
    }
}
