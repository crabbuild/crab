//! Owned read-only SQLite view of one verified immutable Cell root.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};

use rusqlite::{Connection, OpenFlags};

use super::{RootRef, VerifiedRoot};
use crate::{CrabError, Result, writable_vfs::Registration};

/// A read-only SQLite view backed by authenticated pages of one immutable root.
///
/// Queries hold the connection lock through execution. Page bodies use the
/// host's bounded cache; Cell ownership and freshness remain the caller's duty.
pub struct ReadOnlyRoot {
    root: RootRef,
    connection: Option<Mutex<Connection>>,
    registration: Registration,
}

impl ReadOnlyRoot {
    pub(super) fn open(root: &VerifiedRoot, destination: &Path) -> Result<Self> {
        let database = crate::paged_io::Database::Snapshot(root.pages.clone());
        let registration = Registration::new(database, destination)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = (|| -> Result<Connection> {
            let connection =
                Connection::open_with_flags_and_vfs(destination, flags, registration.vfs())?;
            crate::db::configure_managed_connection(&connection)?;
            connection.pragma_update(None, "query_only", true)?;
            Ok(connection)
        })()
        .map_err(|error| registration.take_error().unwrap_or(error))?;
        Ok(Self {
            root: root.root,
            connection: Some(Mutex::new(connection)),
            registration,
        })
    }

    /// Takes the provider/checksum source behind a sparse SQLite I/O error.
    pub fn take_io_error(&self) -> Option<CrabError> {
        self.registration.take_error()
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
        // SQLite must release its handle before registration removes the placeholder.
        self.connection.take();
    }
}
