use std::path::{Path, PathBuf};

use rusqlite::{Connection, Transaction};

use crate::{CaptureBatch, CrabError, Limits, LocalSegment, Position, Result, SegmentInfo};
use crate::{db::Db, host::LtxHost, ltx, types::Txid};

/// Number of SQLite connections retained by one open managed database.
pub const MANAGED_SQLITE_CONNECTIONS: u64 = 3;

/// Page-cache byte target budgeted for each retained SQLite connection.
pub const MANAGED_CONNECTION_PAGE_CACHE_BYTES: u64 = 64 * 1024;

const MANAGED_CONNECTION_PAGE_CACHE_KIB: i64 = 64;

/// One exclusive local capture session with a serialized SQLite writer.
///
/// The caller owns the database and its directory: no external writers, direct
/// checkpoints, control-table edits, or deletion of retained LTX files. Opening
/// claims a fresh metadata directory; reactivation requires exact restore into
/// a fresh directory, not reusing potentially unpublished local state.
pub struct ManagedDb {
    db: Db,
    writer: Connection,
    observer: crate::commit::CommitObserver,
    required_cut: Option<crate::commit::WalCut>,
    limits: Limits,
    fenced: bool,
    retained_bytes: u64,
    retained_segments: usize,
    #[cfg(feature = "replica")]
    retained: Vec<LocalSegment>,
    path: PathBuf,
    host: crate::Host,
    #[cfg(feature = "replica")]
    paged: Option<crate::writable_vfs::Registration>,
}

impl ManagedDb {
    /// Returns a thread-safe handle for interrupting the current SQLite operation.
    ///
    /// The handle becomes inert after the database closes. Calling it does not
    /// prove rollback or cancellation; the owner must still await the operation.
    #[must_use]
    pub fn interrupt_handle(&self) -> rusqlite::InterruptHandle {
        self.writer.get_interrupt_handle()
    }

    #[cfg(feature = "replica")]
    pub(crate) fn open_paged(database: crate::PagedDatabase, destination: &Path) -> Result<Self> {
        Self::open_sparse(crate::paged_io::Database::Replica(database), destination)
    }

    #[cfg(feature = "replica")]
    pub(crate) fn open_cell_paged(
        database: crate::CellWritableDatabase,
        destination: &Path,
    ) -> Result<Self> {
        Self::open_sparse(crate::paged_io::Database::Cell(database), destination)
    }

    #[cfg(feature = "replica")]
    fn open_sparse(database: crate::paged_io::Database, destination: &Path) -> Result<Self> {
        let limits = database.limits();
        let position = database.position();
        let page_size = database.page_size();
        let count = database.page_count();
        let checksums = database.checksums()?;
        let host = database.host();
        let registration = crate::writable_vfs::Registration::new(database, destination)?;
        let mut db = Self::open_inner(destination, limits, Some(registration.vfs()), host)
            .map_err(|error| registration.take_error().unwrap_or(error))?;
        db.db
            .seed_continuation(position, checksums, page_size, count)?;
        db.paged = Some(registration);
        Ok(db)
    }

    /// Reports resolved pages of the pinned cut for a sparse activation.
    #[cfg(feature = "replica")]
    pub fn hydration(&self) -> Result<Option<crate::Hydration>> {
        self.paged.as_ref().map(|p| p.hydration()).transpose()
    }

    /// Hydrates at most `pages` unresolved cut pages through the same VFS as SQL.
    ///
    /// Call periodically on the database worker; scheduling and cancellation
    /// belong to the owner. This operation never publishes or checkpoints WAL.
    #[cfg(feature = "replica")]
    pub fn hydrate_step(&mut self, pages: u32) -> Result<crate::Hydration> {
        self.ensure_active()?;
        self.paged
            .as_mut()
            .ok_or(CrabError::InvalidState("not a sparse activation"))?
            .step(&self.writer, pages)
    }

    /// Takes the provider/checksum source behind a sparse SQLite I/O error.
    #[cfg(feature = "replica")]
    pub fn take_io_error(&self) -> Option<CrabError> {
        self.paged.as_ref().and_then(|p| p.take_error())
    }

    /// Deletes only this session's exact artifacts present in a published head.
    ///
    /// Remote retention remains caller-owned. Prune before remote compaction:
    /// a replacement snapshot is not proof that a particular local cut was
    /// published. An I/O failure can leave a partially pruned set; retry safely.
    #[cfg(feature = "replica")]
    pub fn prune_published(&mut self, head: &crate::ReplicaHead) -> Result<usize> {
        self.prune_retained(|segment| head.segments().any(|info| info == segment.info()))
    }

    /// Deletes this session's exact captured artifacts after their root publishes.
    ///
    /// Every selected file is reverified before deletion. An error retains its
    /// accounting so the owner can retry or discard the complete local session.
    #[cfg(feature = "replica")]
    pub fn prune_captured(&mut self, batch: &crate::CaptureBatch) -> Result<usize> {
        self.prune_retained(|segment| {
            batch.segments.iter().any(|published| {
                published.path() == segment.path() && published.info() == segment.info()
            })
        })
    }

    #[cfg(feature = "replica")]
    fn prune_retained(
        &mut self,
        mut selected: impl FnMut(&crate::LocalSegment) -> bool,
    ) -> Result<usize> {
        let mut removed = 0;
        let mut index = 0;
        while index < self.retained.len() {
            let segment = &self.retained[index];
            if !selected(segment) {
                index += 1;
                continue;
            }
            match self.host.read(segment.path(), segment.info().size_bytes) {
                Ok(bytes) => {
                    crate::recovery::verify_segment(&bytes, segment.info(), self.limits)?;
                    self.host.filesystem.remove_file(segment.path())?;
                }
                // A previous removal may have succeeded before parent sync failed.
                // Keep accounting until sync succeeds, including on retry.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            self.host.filesystem.sync_parent(segment.path())?;
            let segment = self.retained.remove(index);
            self.retained_bytes -= segment.info().size_bytes;
            self.retained_segments -= 1;
            removed += 1;
        }
        Ok(removed)
    }

    /// Restores a pinned plan into a fresh session, preserving the parent position.
    ///
    /// Local leftovers are never used to infer acknowledged state. Any prior
    /// unacknowledged session remains quarantined for explicit reconciliation.
    pub fn resume(
        plan: &crate::VerifiedLocalPlan,
        destination: &Path,
        limits: Limits,
    ) -> Result<Self> {
        Self::resume_with_host(plan, destination, limits, crate::Host::default())
    }

    /// Resumes an exact local plan using the same host for installation and capture.
    pub fn resume_with_host(
        plan: &crate::VerifiedLocalPlan,
        destination: &Path,
        limits: Limits,
        host: crate::Host,
    ) -> Result<Self> {
        let (checksums, page_size, count) = crate::recovery::continuation(plan)?;
        let limits = limits.validate()?;
        if u64::from(count) * u64::from(page_size) > limits.max_database_bytes {
            return Err(CrabError::Limit("database bytes"));
        }
        host.restore(plan, destination)?;
        let mut db = Self::open_with_host(destination, limits, host)?;
        db.db
            .seed_continuation(plan.position(), checksums, page_size, count)?;
        Ok(db)
    }

    /// Opens a new capture session on a new or exactly restored SQLite file.
    ///
    /// The parent must exist and the local path must be UTF-8. An existing
    /// capture directory is refused, even after a clean close. Failed opens
    /// leave that directory quarantined for caller-owned cleanup.
    pub fn open(path: &Path, limits: Limits) -> Result<Self> {
        Self::open_with_host(path, limits, crate::Host::default())
    }

    /// Opens a fresh session using the host's filesystem, SQLite VFS and clock.
    pub fn open_with_host(path: &Path, limits: Limits, host: crate::Host) -> Result<Self> {
        let vfs = host.sqlite_vfs.clone();
        Self::open_inner(path, limits, vfs.as_deref(), host)
    }

    fn open_inner(
        path: &Path,
        limits: Limits,
        vfs: Option<&str>,
        facilities: crate::Host,
    ) -> Result<Self> {
        let limits = limits.validate()?;
        if path.to_str().is_none() || path.file_name().is_none() {
            return Err(CrabError::InvalidState(
                "database path must be a UTF-8 file path",
            ));
        }
        // Resolve aliases before claiming the session directory, and keep all
        // later WAL reads independent of the process working directory.
        let resolved = match facilities.filesystem.canonicalize(path) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                facilities.filesystem.canonicalize(parent)?.join(
                    path.file_name()
                        .ok_or(CrabError::InvalidState("missing database filename"))?,
                )
            }
            Err(error) => return Err(error.into()),
        };
        let path = resolved.as_path();
        if path.to_str().is_none() {
            return Err(CrabError::InvalidState(
                "resolved database path must be UTF-8",
            ));
        }
        let host = LtxHost {
            facilities: facilities.clone(),
            max_database_bytes: limits.max_database_bytes,
            max_file_bytes: limits.max_file_bytes,
        };
        match facilities.filesystem.file_len(path) {
            Ok(len) => host.check_database_size(len)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Atomic directory creation fences concurrent handles and stale sessions.
        // Never unlink it on close: an old open file must not acquire a new epoch.
        facilities.filesystem.create_dir(&Db::meta_path_for(path))?;
        let db = Db::open_with_host(path, host, vfs)?;
        let writer = open_connection(path, vfs)?;
        writer.busy_timeout(std::time::Duration::from_secs(1))?;
        writer.pragma_update(None, "wal_autocheckpoint", 0)?;
        writer.pragma_update(None, "synchronous", "FULL")?;
        writer.pragma_update(None, "foreign_keys", true)?;
        let page_size: u32 = writer.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let max_pages = limits.max_database_bytes / u64::from(page_size);
        if max_pages == 0 {
            return Err(CrabError::Limit("database page size"));
        }
        writer.pragma_update(None, "max_page_count", max_pages)?;
        let observer = crate::commit::CommitObserver::install(&writer);
        Ok(Self {
            db,
            writer,
            observer,
            required_cut: None,
            limits,
            fenced: false,
            retained_bytes: 0,
            retained_segments: 0,
            #[cfg(feature = "replica")]
            retained: Vec::new(),
            path: path.to_owned(),
            host: facilities,
            #[cfg(feature = "replica")]
            paged: None,
        })
    }

    /// Commits one local SQL transaction; call `capture` before publishing it.
    ///
    /// SQL is trusted: do not issue transaction-control statements or change
    /// pager pragmas through the callback. Success is NOT remote durability.
    pub fn transaction<T>(
        &mut self,
        operation: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T> {
        self.transaction_with(operation)
            .map_err(|error| match error {
                crate::TransactionError::Operation(error)
                | crate::TransactionError::Sqlite(error) => error.into(),
                crate::TransactionError::Capture(error) => error,
            })
    }

    /// Commits one transaction while preserving application-domain failures.
    ///
    /// An `Operation` result guarantees the transaction was rolled back and the
    /// writer remains reusable. SQLite commit/rollback ambiguity fences the
    /// writer. A successful return is still local-only until capture and remote
    /// publication complete.
    pub fn transaction_with<T, E>(
        &mut self,
        operation: impl FnOnce(&Transaction<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, crate::TransactionError<E>>
    where
        E: std::error::Error + 'static,
    {
        self.ensure_active()
            .map_err(crate::TransactionError::Capture)?;
        self.ensure_capacity()
            .map_err(crate::TransactionError::Capture)?;
        self.observer.reset();
        let tx = self
            .writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(crate::TransactionError::Sqlite)?;
        let value = match operation(&tx) {
            Ok(value) => value,
            Err(error) => {
                if let Err(rollback) = tx.rollback() {
                    self.fenced = true;
                    return Err(crate::TransactionError::Sqlite(rollback));
                }
                return Err(crate::TransactionError::Operation(error));
            }
        };
        if let Err(error) = tx.commit() {
            // Commit failure is potentially ambiguous even if SQLite did not
            // invoke the WAL hook. Never accept another mutation here.
            self.fenced = true;
            return Err(crate::TransactionError::Sqlite(error));
        }
        match self.observer.cut(&self.path, &self.host) {
            Ok(Some(cut)) => self.required_cut = Some(cut),
            Ok(None) => {}
            Err(error) => {
                self.fenced = true;
                return Err(crate::TransactionError::Capture(error));
            }
        }
        Ok(value)
    }

    /// Runs one synchronous callback with SQLite writes disabled.
    ///
    /// The callback must not change connection pragmas or retain borrowed SQLite
    /// values. Establishing or removing the read-only boundary failure fences
    /// this capture session; an application error leaves it reusable.
    pub fn query_with<T, E>(
        &mut self,
        operation: impl FnOnce(&Connection) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, crate::QueryError<E>>
    where
        E: std::error::Error + 'static,
    {
        self.ensure_active().map_err(crate::QueryError::State)?;
        if let Err(error) = self.writer.pragma_update(None, "query_only", true) {
            self.fenced = true;
            return Err(crate::QueryError::Sqlite(error));
        }
        let result = operation(&self.writer);
        if let Err(error) = self.writer.pragma_update(None, "query_only", false) {
            self.fenced = true;
            return Err(crate::QueryError::Sqlite(error));
        }
        result.map_err(crate::QueryError::Operation)
    }

    /// Captures committed WAL pages and all cuts made by checkpoint maintenance.
    ///
    /// Any failure fences further use, since some local cuts may already exist.
    /// Retain returned files until publication; `prune_captured` can release
    /// an exact acknowledged batch and `prune_published` can reconcile a head
    /// when the replica feature is enabled.
    pub fn capture(&mut self) -> Result<CaptureBatch> {
        self.ensure_active()?;
        let result = self.capture_inner();
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    fn capture_inner(&mut self) -> Result<CaptureBatch> {
        self.ensure_capacity()?;
        let before = self.db.pos();
        self.db.sync(self.required_cut)?;
        self.required_cut = None;
        self.collect_cuts(before)
    }

    fn collect_cuts(&mut self, before: crate::Pos) -> Result<CaptureBatch> {
        let after = self.db.pos();
        let mut segments = Vec::new();
        if after.txid.0 > before.txid.0 {
            for txid in before.txid.0 + 1..=after.txid.0 {
                let path = PathBuf::from(self.db.ltx_path(0, Txid(txid), Txid(txid)));
                let bytes = self.host.read(&path, self.limits.max_file_bytes)?;
                let file = ltx::decode_file(&bytes)?;
                let info = SegmentInfo::from_decoded(&bytes, &file);
                self.account(&info)?;
                let segment = LocalSegment::new(path, info);
                #[cfg(feature = "replica")]
                self.retained.push(segment.clone());
                segments.push(segment);
            }
        }
        Ok(CaptureBatch {
            segments,
            position: after.into(),
        })
    }

    /// Captures pending writes before a requested checkpoint and returns every cut.
    ///
    /// Failure fences this session, including failed forced checkpoints. All
    /// returned cuts must be published before acknowledging the operation.
    pub fn checkpoint(&mut self, mode: crate::CheckpointMode) -> Result<CaptureBatch> {
        self.ensure_active()?;
        let result = (|| {
            let mut batch = self.capture_inner()?;
            let before = self.db.pos();
            self.db.checkpoint(mode)?;
            let extra = self.collect_cuts(before)?;
            batch.segments.extend(extra.segments);
            batch.position = extra.position;
            Ok(batch)
        })();
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    /// Returns the last sealed local position, not a remote durability receipt.
    #[must_use]
    pub fn position(&self) -> Position {
        self.db.pos().into()
    }

    /// Captures pending commits, then writes a full checksum-bearing snapshot.
    ///
    /// Its range is `1..=position.txid`. The snapshot can replace all preceding
    /// cuts in a new manifest. The second return value owns every newly captured
    /// cut: publish it to continue an existing head. Neither output is published.
    pub fn snapshot(&mut self, destination: &Path) -> Result<(LocalSegment, CaptureBatch)> {
        self.ensure_active()?;
        let result = self.snapshot_inner(destination);
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    fn snapshot_inner(&mut self, destination: &Path) -> Result<(LocalSegment, CaptureBatch)> {
        let batch = self.capture_inner()?;
        let mut bytes = Vec::new();
        let pos: Position = self.db.snapshot_to_writer(&mut bytes)?.into();
        if pos != batch.position {
            return Err(CrabError::ChecksumMismatch);
        }
        let file = ltx::decode_file(&bytes)?;
        let info = SegmentInfo::from_decoded(&bytes, &file);
        self.account(&info)?;
        self.host.filesystem.persist_new(destination, &bytes)?;
        let segment = LocalSegment::new(destination.to_owned(), info);
        #[cfg(feature = "replica")]
        self.retained.push(segment.clone());
        Ok((segment, batch))
    }

    /// Returns the local file path; it must not be independently mutated.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Releases the writer and checkpoint read lock without claiming publication.
    pub fn close(self) -> Result<()> {
        drop(self.writer);
        self.db.close()
    }

    fn ensure_active(&self) -> Result<()> {
        if self.fenced {
            return Err(CrabError::Fenced);
        }
        Ok(())
    }

    fn ensure_capacity(&self) -> Result<()> {
        if self.retained_segments >= self.limits.max_segments
            || self.retained_bytes >= self.limits.max_plan_bytes
        {
            return Err(CrabError::Limit(
                "retained capture artifacts; rotate session",
            ));
        }
        Ok(())
    }

    fn account(&mut self, info: &SegmentInfo) -> Result<()> {
        if info.size_bytes > self.limits.max_file_bytes {
            return Err(CrabError::Limit("LTX file bytes"));
        }
        self.retained_bytes = self
            .retained_bytes
            .checked_add(info.size_bytes)
            .ok_or(CrabError::Limit("retained bytes"))?;
        self.retained_segments += 1;
        if self.retained_segments > self.limits.max_segments
            || self.retained_bytes > self.limits.max_plan_bytes
        {
            return Err(CrabError::Limit(
                "retained capture artifacts; rotate session",
            ));
        }
        Ok(())
    }
}

pub(crate) fn open_connection(path: &Path, vfs: Option<&str>) -> rusqlite::Result<Connection> {
    let connection = match vfs {
        Some(vfs) => Connection::open_with_flags_and_vfs(path, rusqlite::OpenFlags::default(), vfs),
        None => Connection::open(path),
    }?;
    connection.pragma_update(None, "cache_size", -MANAGED_CONNECTION_PAGE_CACHE_KIB)?;
    Ok(connection)
}

pub(crate) fn read_main(connection: &Connection, offset: u64, size: usize) -> Result<Vec<u8>> {
    use rusqlite::ffi;
    let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
    let mut data = vec![0u8; size];
    let offset = i64::try_from(offset).map_err(|_| CrabError::LTXCorrupted)?;
    let size = i32::try_from(size).map_err(|_| CrabError::LTXCorrupted)?;
    // SAFETY: the connection is exclusively borrowed on its database worker.
    // SQLite owns FILE_POINTER until this borrow ends; data holds size bytes.
    let rc = unsafe {
        let rc = ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_FILE_POINTER,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        );
        if rc != ffi::SQLITE_OK || file.is_null() || (*file).pMethods.is_null() {
            return Err(CrabError::InvalidState("SQLite main file unavailable"));
        }
        let read = (*(*file).pMethods)
            .xRead
            .ok_or(CrabError::InvalidState("SQLite main file cannot read"))?;
        read(file, data.as_mut_ptr().cast(), size, offset)
    };
    if rc != ffi::SQLITE_OK {
        return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None).into());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("inventory rejected the command")]
    struct Rejected;

    #[test]
    fn managed_connections_set_the_budgeted_page_cache() {
        let temp = tempfile::TempDir::new().unwrap();
        let connection = open_connection(&temp.path().join("cache.sqlite"), None).unwrap();
        let cache_kib: i64 = connection
            .query_row("PRAGMA cache_size", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cache_kib, -MANAGED_CONNECTION_PAGE_CACHE_KIB);
    }

    #[test]
    fn typed_operation_error_rolls_back_and_keeps_writer_usable() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut db = ManagedDb::open(&temp.path().join("typed.sqlite"), Limits::default()).unwrap();
        db.transaction(|tx| tx.execute_batch("CREATE TABLE inventory(value INTEGER NOT NULL)"))
            .unwrap();

        let rejected = db.transaction_with(|tx| {
            tx.execute("INSERT INTO inventory VALUES (1)", [])
                .map_err(|_| Rejected)?;
            Err::<(), _>(Rejected)
        });
        assert!(matches!(
            rejected,
            Err(crate::TransactionError::Operation(Rejected))
        ));

        db.transaction(|tx| {
            tx.execute("INSERT INTO inventory VALUES (2)", [])
                .map(|_| ())
        })
        .unwrap();
        let count = db
            .writer
            .query_row("SELECT count(*) FROM inventory", [], |row| {
                row.get::<_, u32>(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn truncate_checkpoint_and_auto_vacuum_preserve_every_cut() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("source.sqlite");
        let initial = Connection::open(&path).unwrap();
        initial
            .execute_batch("PRAGMA auto_vacuum=FULL; VACUUM;")
            .unwrap();
        drop(initial);
        let mut db = ManagedDb::open(&path, Limits::default()).unwrap();
        db.db.truncate_page_n = 20;
        db.db.min_checkpoint_page_n = 10;
        let mut segments = Vec::new();
        let mut last_pages = 0;
        let mut shrank = false;
        let mut multiple_cuts = false;
        for round in 0..6 {
            db.transaction(|tx| {
                tx.execute("CREATE TABLE IF NOT EXISTS t (data BLOB)", [])?;
                if round % 2 == 0 {
                    for _ in 0..80 {
                        tx.execute("INSERT INTO t VALUES (randomblob(8000))", [])?;
                    }
                } else {
                    tx.execute("DELETE FROM t", [])?;
                }
                Ok(())
            })
            .unwrap();
            let batch = db.capture().unwrap();
            multiple_cuts |= batch.segments.len() > 1;
            for segment in &batch.segments {
                shrank |= last_pages > segment.info().database_pages;
                last_pages = segment.info().database_pages;
            }
            segments.extend(batch.segments);
            let plan = crate::VerifiedLocalPlan::new(&segments, batch.position, Limits::default())
                .unwrap();
            let restored = temp.path().join(format!("restored-{round}.sqlite"));
            crate::restore_exact(&plan, &restored).unwrap();
            let conn = Connection::open(&restored).unwrap();
            let check: String = conn
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap();
            assert_eq!(check, "ok");
            let count: u32 = conn
                .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, if round % 2 == 0 { 80 } else { 0 });
            crate::compact_exact(&plan, &temp.path().join(format!("compacted-{round}.ltx")))
                .unwrap();
        }
        assert!(shrank);
        assert!(multiple_cuts);
    }
}
