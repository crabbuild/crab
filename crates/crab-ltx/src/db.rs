use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, Transaction};

use crate::{CaptureBatch, CrabError, Limits, LocalSegment, Position, Result, SegmentInfo};
use crate::{capture::CaptureEngine, host::LtxHost, ltx, types::Txid};

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
pub struct Db {
    capture: CaptureEngine,
    writer: Connection,
    observer: crate::commit::CommitObserver,
    required_cut: Option<crate::commit::WalCut>,
    limits: Limits,
    fenced: bool,
    retained_bytes: u64,
    retained_segments: usize,
    local_disk: crate::DiskReservation,
    sparse: bool,
    #[cfg(feature = "replica")]
    retained: Vec<LocalSegment>,
    path: PathBuf,
    host: crate::Host,
    pending_durability: Vec<PathBuf>,
    #[cfg(feature = "replica")]
    paged: Option<crate::writable_vfs::Registration>,
}

impl Db {
    /// Returns a thread-safe handle for interrupting the current SQLite operation.
    ///
    /// The handle becomes inert after the database closes. Calling it does not
    /// prove rollback or cancellation; the owner must still await the operation.
    #[must_use]
    pub fn interrupt_handle(&self) -> rusqlite::InterruptHandle {
        self.writer.get_interrupt_handle()
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
        let mut db = Self::open_inner(
            destination,
            limits,
            Some(registration.vfs()),
            host,
            true,
            None,
        )
        .map_err(|error| registration.take_error().unwrap_or(error))?;
        db.capture
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
        let paged = self
            .paged
            .as_mut()
            .ok_or(CrabError::InvalidState("not a sparse activation"))?;
        crate::paged_io::with_paged_io_origin(crate::LtxReadOrigin::Hydrating, || {
            paged.step(&self.writer, pages)
        })
    }

    /// Takes the provider/checksum source behind a sparse SQLite I/O error.
    #[cfg(feature = "replica")]
    pub fn take_io_error(&self) -> Option<CrabError> {
        self.paged.as_ref().and_then(|p| p.take_error())
    }

    /// Deletes this session's exact captured artifacts after their root publishes.
    ///
    /// Every selected file is reverified before deletion. An error retains its
    /// accounting so the owner can retry or discard the complete local session.
    #[cfg(feature = "replica")]
    pub fn prune_captured(&mut self, batch: &crate::CaptureBatch) -> Result<usize> {
        self.durability_barrier()?;
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
        self.reconcile_local_disk()?;
        Ok(removed)
    }

    /// Restores a pinned plan into a fresh session, preserving the parent position.
    ///
    /// Local leftovers are never used to infer acknowledged state. Any prior
    /// unacknowledged session remains quarantined for explicit reconciliation.
    pub fn resume(plan: &crate::VerifiedPlan, destination: &Path, limits: Limits) -> Result<Self> {
        Self::resume_with_host(plan, destination, limits, crate::Host::default())
    }

    /// Resumes an exact verified plan using the same host for installation and capture.
    pub fn resume_with_host(
        plan: &crate::VerifiedPlan,
        destination: &Path,
        limits: Limits,
        host: crate::Host,
    ) -> Result<Self> {
        let materialized = plan.materialize()?;
        let limits = limits.validate()?;
        if u64::from(materialized.database_pages) * u64::from(materialized.page_size)
            > limits.max_database_bytes
        {
            return Err(CrabError::Limit("database bytes"));
        }
        let database_bytes =
            u64::from(materialized.database_pages) * u64::from(materialized.page_size);
        let local_disk = host.reserve_local_disk(database_bytes)?;
        host.restore_materialized(&materialized, destination)?;
        let vfs = host.sqlite_vfs.clone();
        let mut db = Self::open_inner(
            destination,
            limits,
            vfs.as_deref(),
            host,
            false,
            Some(local_disk),
        )?;
        db.capture.seed_continuation(
            plan.position(),
            materialized.checksums.clone(),
            materialized.page_size,
            materialized.database_pages,
        )?;
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
        Self::open_inner(path, limits, vfs.as_deref(), host, false, None)
    }

    fn open_inner(
        path: &Path,
        limits: Limits,
        vfs: Option<&str>,
        facilities: crate::Host,
        sparse: bool,
        local_disk: Option<crate::DiskReservation>,
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
        let database_bytes = match facilities.filesystem.file_len(path) {
            Ok(len) => {
                host.check_database_size(len)?;
                len
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        let local_disk = match local_disk {
            Some(local_disk) => {
                local_disk.resize(if sparse { 0 } else { database_bytes })?;
                local_disk
            }
            None => facilities.reserve_local_disk(if sparse { 0 } else { database_bytes })?,
        };
        // Atomic directory creation fences concurrent handles and stale sessions.
        // Never unlink it on close: an old open file must not acquire a new epoch.
        facilities
            .filesystem
            .create_dir(&CaptureEngine::meta_path_for(path))?;
        let capture = CaptureEngine::open_with_host(path, host, vfs)?;
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
            capture,
            writer,
            observer,
            required_cut: None,
            limits,
            fenced: false,
            retained_bytes: 0,
            retained_segments: 0,
            local_disk,
            sparse,
            #[cfg(feature = "replica")]
            retained: Vec::new(),
            path: path.to_owned(),
            host: facilities,
            pending_durability: Vec::new(),
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
                crate::TransactionError::Admission(error) => error,
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
        let disk_before = self.local_disk.bytes();
        let write_bytes = self.limits.max_capture_bytes.checked_mul(2).ok_or(
            crate::TransactionError::Admission(CrabError::Limit("local disk bytes")),
        )?;
        self.local_disk
            .try_grow(write_bytes)
            .map_err(crate::TransactionError::Admission)?;
        self.observer.reset();
        let tx = match self
            .writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        {
            Ok(tx) => tx,
            Err(error) => {
                let _ = self.local_disk.resize(disk_before);
                return Err(crate::TransactionError::Sqlite(error));
            }
        };
        let value = match operation(&tx) {
            Ok(value) => value,
            Err(error) => {
                if let Err(rollback) = tx.rollback() {
                    self.fenced = true;
                    return Err(crate::TransactionError::Sqlite(rollback));
                }
                let _ = self.local_disk.resize(disk_before);
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
            Ok(None) => {
                let _ = self.local_disk.resize(disk_before);
            }
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
    /// Retain returned files until the canonical Cell root publishes;
    /// `prune_captured` can then release one exact acknowledged batch.
    pub fn capture(&mut self) -> Result<CaptureBatch> {
        self.ensure_active()?;
        self.flush_pending_durability()?;
        let (result, timing) = self.capture_inner(false);
        #[cfg(feature = "replica")]
        self.host.observe_ltx_capture(&timing, result.is_ok());
        let result = result.map(|mut batch| {
            batch.timing = timing;
            batch
        });
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    /// Captures committed WAL pages while deferring their durability barrier.
    ///
    /// LTX files are complete and readable when this returns, but their contents
    /// and names are not durable until [`Self::durability_barrier`] succeeds.
    /// This permits a host to group several captures behind one storage flush;
    /// callers must complete the barrier before acknowledging or pruning any
    /// returned batch. A failed barrier fences the session.
    pub fn capture_deferred(&mut self) -> Result<CaptureBatch> {
        self.ensure_active()?;
        let (result, timing) = self.capture_inner(true);
        #[cfg(feature = "replica")]
        self.host.observe_ltx_capture(&timing, result.is_ok());
        let result = result.map(|mut batch| {
            batch.timing = timing;
            self.pending_durability.extend(
                batch
                    .segments
                    .iter()
                    .map(|segment| segment.path().to_owned()),
            );
            batch
        });
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    /// Makes all files published by deferred captures durable as one barrier.
    ///
    /// The barrier syncs each completed file before syncing each destination
    /// directory once. If either step fails, the session is fenced and pending
    /// paths remain tracked for diagnostics; no caller may acknowledge them.
    pub fn durability_barrier(&mut self) -> Result<()> {
        self.ensure_active()?;
        self.flush_pending_durability()
    }

    fn flush_pending_durability(&mut self) -> Result<()> {
        if self.pending_durability.is_empty() {
            return Ok(());
        }
        let mut parents = BTreeMap::new();
        for path in &self.pending_durability {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_owned();
            parents.entry(parent).or_insert_with(|| path.clone());
        }
        let result = (|| {
            self.host.filesystem.sync_files(&self.pending_durability)?;
            parents
                .values()
                .try_for_each(|path| self.host.filesystem.sync_parent(path))?;
            Ok::<(), std::io::Error>(())
        })()
        .map_err(CrabError::from);
        if result.is_ok() {
            self.pending_durability.clear();
        } else {
            self.fenced = true;
        }
        result
    }

    fn capture_inner(
        &mut self,
        defer_durability: bool,
    ) -> (Result<CaptureBatch>, crate::CaptureTiming) {
        self.capture.start_timing(self.host.now_monotonic());
        self.capture
            .timing_begin(crate::capture::TimingPhase::Preparation);
        let result = (|| {
            self.ensure_capacity()?;
            self.capture
                .timing_end(crate::capture::TimingPhase::Preparation);
            let before = self.capture.pos();
            if defer_durability {
                self.capture.sync_deferred(self.required_cut)?;
            } else {
                self.capture.sync(self.required_cut)?;
            }
            self.required_cut = None;
            let batch = self.collect_cuts(before)?;
            self.reconcile_local_disk()?;
            Ok(batch)
        })();
        let timing = self.capture.finish_timing(self.host.now_monotonic());
        (result, timing)
    }

    fn collect_cuts(&mut self, before: crate::Pos) -> Result<CaptureBatch> {
        let after = self.capture.pos();
        let mut segments = Vec::new();
        if after.txid.0 > before.txid.0 {
            for txid in before.txid.0 + 1..=after.txid.0 {
                let path = PathBuf::from(self.capture.ltx_path(0, Txid(txid), Txid(txid)));
                let info = if let Some(info) = self.capture.sealed_l0_segment(Txid(txid)) {
                    info
                } else {
                    let file = crate::LtxHost {
                        facilities: self.host.clone(),
                        max_database_bytes: self.limits.max_database_bytes,
                        max_file_bytes: self.limits.max_capture_bytes,
                    }
                    .open(&path)?;
                    self.capture
                        .timing_begin(crate::capture::TimingPhase::Verification);
                    let inspected = ltx::inspect_reader(file);
                    self.capture
                        .timing_end(crate::capture::TimingPhase::Verification);
                    let (decoded, size, digest) = inspected?;
                    SegmentInfo::from_inspected(&decoded, size, digest)
                };
                self.capture.timing_add_ltx_bytes(info.size_bytes);
                self.capture.timing_add_segment();
                self.account_capture(&info)?;
                let segment = LocalSegment::new(path, info);
                #[cfg(feature = "replica")]
                self.retained.push(segment.clone());
                segments.push(segment);
            }
        }
        Ok(CaptureBatch {
            segments,
            position: after.into(),
            timing: crate::CaptureTiming::default(),
        })
    }

    /// Captures pending writes before a requested checkpoint and returns every cut.
    ///
    /// Failure fences this session, including failed forced checkpoints. All
    /// returned cuts must be published before acknowledging the operation.
    pub fn checkpoint(&mut self, mode: crate::CheckpointMode) -> Result<CaptureBatch> {
        self.ensure_active()?;
        self.flush_pending_durability()?;
        let (initial, mut timing) = self.capture_inner(false);
        let result = (|| {
            let mut batch = initial?;
            self.local_disk.try_grow(
                self.limits
                    .max_capture_bytes
                    .checked_mul(2)
                    .ok_or(CrabError::Limit("local disk bytes"))?,
            )?;
            let before = self.capture.pos();
            self.capture.start_timing(self.host.now_monotonic());
            let checkpoint_result = self.capture.checkpoint(mode);
            let extra_result = checkpoint_result.and_then(|()| self.collect_cuts(before));
            let checkpoint_timing = self.capture.finish_timing(self.host.now_monotonic());
            timing.merge(checkpoint_timing);
            let extra = extra_result?;
            batch.timing = timing;
            batch.segments.extend(extra.segments);
            batch.position = extra.position;
            self.reconcile_local_disk()?;
            Ok(batch)
        })();
        #[cfg(feature = "replica")]
        self.host.observe_ltx_capture(&timing, result.is_ok());
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    /// Returns the last sealed local position, not a remote durability receipt.
    #[must_use]
    pub fn position(&self) -> Position {
        self.capture.pos().into()
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
        self.flush_pending_durability()?;
        let (batch, timing) = self.capture_inner(false);
        #[cfg(feature = "replica")]
        self.host.observe_ltx_capture(&timing, batch.is_ok());
        let mut batch = batch?;
        batch.timing = timing;
        self.local_disk.try_grow(self.limits.max_file_bytes)?;
        let (mut scratch, mut output) =
            SnapshotScratch::create(&self.host, destination, self.limits.max_file_bytes)?;
        let pos: Position = self.capture.snapshot_to_writer(&mut output)?.into();
        if pos != batch.position {
            return Err(CrabError::ChecksumMismatch);
        }
        output.sync_all()?;
        drop(output);
        let file = crate::LtxHost {
            facilities: self.host.clone(),
            max_database_bytes: self.limits.max_database_bytes,
            max_file_bytes: self.limits.max_file_bytes,
        }
        .open(&scratch.path)?;
        let (decoded, size, digest) = ltx::inspect_reader(file)?;
        let info = SegmentInfo::from_inspected(&decoded, size, digest);
        self.account(&info)?;
        self.host
            .filesystem
            .persist_file_new(&scratch.path, destination)?;
        scratch.installed = true;
        let segment = LocalSegment::new(destination.to_owned(), info);
        #[cfg(feature = "replica")]
        self.retained.push(segment.clone());
        self.reconcile_local_disk()?;
        Ok((segment, batch))
    }

    /// Returns the local file path; it must not be independently mutated.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Releases the writer and checkpoint read lock without claiming publication.
    pub fn close(mut self) -> Result<()> {
        self.flush_pending_durability()?;
        drop(self.writer);
        self.capture.close()
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

    fn account_capture(&mut self, info: &SegmentInfo) -> Result<()> {
        if info.size_bytes > self.limits.max_capture_bytes {
            return Err(CrabError::Limit("captured LTX bytes"));
        }
        self.account(info)
    }

    fn reconcile_local_disk(&self) -> Result<()> {
        let database_bytes = if self.sparse {
            0
        } else {
            self.host.filesystem.file_len(&self.path)?
        };
        let wal_bytes = match self.host.filesystem.file_len(&self.capture.wal_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        let live = database_bytes
            .checked_add(self.retained_bytes)
            .ok_or(CrabError::Limit("local disk bytes"))?
            .checked_add(wal_bytes)
            .ok_or(CrabError::Limit("local disk bytes"))?;
        self.local_disk.resize(live)
    }
}

struct SnapshotScratch {
    filesystem: std::sync::Arc<dyn crate::environment::FileSystem>,
    path: PathBuf,
    installed: bool,
}

impl SnapshotScratch {
    fn create(
        host: &crate::Host,
        destination: &Path,
        max_file_bytes: u64,
    ) -> Result<(Self, crate::HostFile)> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let filename = destination
            .file_name()
            .ok_or(CrabError::InvalidState("missing snapshot filename"))?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        for _ in 0..16 {
            let mut scratch_name = filename.to_owned();
            scratch_name.push(format!(
                ".crab-snapshot-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let path = parent.join(scratch_name);
            let ltx_host = crate::LtxHost {
                facilities: host.clone(),
                max_database_bytes: max_file_bytes,
                max_file_bytes,
            };
            match ltx_host.create(&path) {
                Ok(file) => {
                    return Ok((
                        Self {
                            filesystem: host.filesystem.clone(),
                            path,
                            installed: false,
                        },
                        file,
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "snapshot scratch namespace exhausted",
        )
        .into())
    }
}

impl Drop for SnapshotScratch {
    fn drop(&mut self) {
        if !self.installed {
            let _ = self.filesystem.remove_file(&self.path);
        }
    }
}

pub(crate) fn open_connection(path: &Path, vfs: Option<&str>) -> rusqlite::Result<Connection> {
    let connection = match vfs {
        Some(vfs) => Connection::open_with_flags_and_vfs(path, rusqlite::OpenFlags::default(), vfs),
        None => Connection::open(path),
    }?;
    disable_lookaside(&connection)?;
    connection.pragma_update(None, "cache_size", -MANAGED_CONNECTION_PAGE_CACHE_KIB)?;
    Ok(connection)
}

fn disable_lookaside(connection: &Connection) -> rusqlite::Result<()> {
    use rusqlite::ffi;

    // SQLite's default lookaside arena reserves memory per connection. Managed
    // LTX connections use a small, stable statement vocabulary, so keeping
    // that arena only adds resident cost across a dense Cell fleet.
    // SAFETY: the connection was opened immediately above and no SQLite
    // operation has run, so no lookaside slot can be in use.
    let result = unsafe {
        ffi::sqlite3_db_config(
            connection.handle(),
            ffi::SQLITE_DBCONFIG_LOOKASIDE,
            std::ptr::null_mut::<std::ffi::c_void>(),
            0,
            0,
        )
    };
    if result != ffi::SQLITE_OK {
        return Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(result),
            None,
        ));
    }
    Ok(())
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
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    #[derive(Debug, thiserror::Error)]
    #[error("inventory rejected the command")]
    struct Rejected;

    struct TimingClock {
        origin: Instant,
        ticks: AtomicU64,
    }

    impl TimingClock {
        fn new() -> Self {
            Self {
                origin: Instant::now(),
                ticks: AtomicU64::new(0),
            }
        }
    }

    impl crate::environment::Clock for TimingClock {
        fn unix_millis(&self) -> i64 {
            123456789
        }

        fn file_age(&self, _: &Path) -> std::io::Result<Duration> {
            Ok(Duration::ZERO)
        }

        fn monotonic(&self) -> Instant {
            self.origin + Duration::from_micros(self.ticks.fetch_add(1, Ordering::Relaxed))
        }
    }

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
    fn capture_reports_deterministic_bounded_timing_for_real_ltx_work() {
        let temp = tempfile::TempDir::new().unwrap();
        let host = crate::Host::default().with_clock(Arc::new(TimingClock::new()));
        let mut db =
            Db::open_with_host(&temp.path().join("timed.sqlite"), Limits::default(), host).unwrap();
        db.transaction(|tx| {
            tx.execute_batch("CREATE TABLE events(value TEXT); INSERT INTO events VALUES ('ok')")
        })
        .unwrap();

        let batch = db.capture().unwrap();
        let phase_nanos = batch.timing.preparation_nanos
            + batch.timing.schema_check_nanos
            + batch.timing.wal_existence_nanos
            + batch.timing.position_resolution_nanos
            + batch.timing.wal_read_nanos
            + batch.timing.page_collection_nanos
            + batch.timing.verification_nanos
            + batch.timing.encode_nanos
            + batch.timing.local_write_nanos
            + batch.timing.fsync_nanos
            + batch.timing.parent_sync_nanos
            + batch.timing.checkpoint_nanos;
        let ltx_bytes = batch
            .segments
            .iter()
            .map(|segment| segment.info().size_bytes)
            .sum::<u64>();
        let segment = &batch.segments[0];
        let file = crate::LtxHost {
            facilities: crate::Host::default(),
            max_database_bytes: Limits::default().max_database_bytes,
            max_file_bytes: Limits::default().max_file_bytes,
        }
        .open(segment.path())
        .unwrap();
        let (decoded, size, digest) = crate::ltx::inspect_reader(file).unwrap();
        assert_eq!(
            segment.info(),
            &crate::SegmentInfo::from_inspected(&decoded, size, digest)
        );
        assert!(batch.timing.total_nanos > 0);
        assert!(phase_nanos <= batch.timing.total_nanos);
        assert_eq!(batch.timing.segment_count as usize, batch.segments.len());
        assert_eq!(batch.timing.ltx_bytes, ltx_bytes);
        assert!(batch.timing.wal_bytes > 0);
        assert!(batch.timing.database_bytes > 0);
        assert!(batch.timing.schema_check_nanos > 0);
        assert!(batch.timing.wal_existence_nanos > 0);
        assert!(batch.timing.position_resolution_nanos > 0);
        assert!(batch.timing.page_collection_nanos > 0);
        assert!(batch.timing.local_write_nanos > 0);
        assert!(batch.timing.fsync_nanos > 0);
        assert!(batch.timing.parent_sync_nanos > 0);
        assert_eq!(
            batch.timing.wal_sparse_reads + batch.timing.wal_full_reads,
            1
        );
        assert!(batch.timing.wal_image_bytes > 0);
        assert!(batch.timing.wal_file_bytes >= batch.timing.wal_read_bytes);
        assert!(batch.timing.wal_read_bytes > 0);
        assert_eq!(batch.timing.wal_snapshot_reads, 1);
    }

    #[cfg(feature = "replica")]
    #[test]
    fn failed_capture_emits_its_bounded_ledger() {
        #[derive(Default)]
        struct CaptureTelemetry(std::sync::Mutex<Vec<(crate::CaptureTiming, bool)>>);

        impl crate::LtxTelemetry for CaptureTelemetry {
            fn capture(&self, timing: &crate::CaptureTiming, succeeded: bool) {
                self.0.lock().unwrap().push((*timing, succeeded));
            }
        }

        let temp = tempfile::TempDir::new().unwrap();
        let telemetry = Arc::new(CaptureTelemetry::default());
        let host = crate::Host::default().with_ltx_telemetry(telemetry.clone());
        let limits = Limits {
            max_capture_bytes: 128,
            ..Limits::default()
        };
        let mut db = Db::open_with_host(&temp.path().join("failed.sqlite"), limits, host).unwrap();
        db.transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE events(value BLOB); INSERT INTO events VALUES(randomblob(4096))",
            )
        })
        .unwrap();

        assert!(db.capture().is_err());
        let attempts = telemetry.0.lock().unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(!attempts[0].1);
        assert!(attempts[0].0.total_nanos > 0);
        assert!(attempts[0].0.wal_read_bytes > 0);
    }

    #[test]
    fn managed_connections_disable_sqlite_lookaside() {
        use rusqlite::ffi;

        let temp = tempfile::TempDir::new().unwrap();
        for index in 0..MANAGED_SQLITE_CONNECTIONS {
            let connection =
                open_connection(&temp.path().join(format!("lookaside-{index}.sqlite")), None)
                    .unwrap();
            let _statement = connection.prepare("SELECT 1").unwrap();
            let mut current = 0;
            let mut highwater = 0;
            let result = unsafe {
                // SAFETY: the connection remains alive and is exclusively
                // borrowed for the duration of this status query.
                ffi::sqlite3_db_status(
                    connection.handle(),
                    ffi::SQLITE_DBSTATUS_LOOKASIDE_USED,
                    &mut current,
                    &mut highwater,
                    0,
                )
            };
            assert_eq!(result, ffi::SQLITE_OK);
            assert_eq!(current, 0);
            assert_eq!(highwater, 0);
        }
    }

    #[test]
    fn local_disk_admission_rejects_before_running_the_transaction() {
        let temp = tempfile::TempDir::new().unwrap();
        let budget = crate::DiskBudget::new(255);
        let host = crate::Host::default().with_local_disk_budget(budget.clone());
        let limits = Limits {
            max_capture_bytes: 128,
            ..Limits::default()
        };
        let mut db = Db::open_with_host(&temp.path().join("disk.sqlite"), limits, host).unwrap();
        let ran = std::cell::Cell::new(false);

        let result = db.transaction(|_| {
            ran.set(true);
            Ok(())
        });

        assert!(matches!(result, Err(CrabError::Limit("local disk bytes"))));
        assert!(!ran.get());
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn existing_database_disk_admission_precedes_session_claim() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("existing.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE t(v)").unwrap();
        drop(connection);
        let bytes = std::fs::metadata(&path).unwrap().len();
        let host = crate::Host::default()
            .with_local_disk_budget(crate::DiskBudget::new(bytes.saturating_sub(1)));

        let result = Db::open_with_host(&path, Limits::default(), host);

        assert!(matches!(result, Err(CrabError::Limit("local disk bytes"))));
        assert!(!CaptureEngine::meta_path_for(&path).exists());
    }

    #[test]
    fn resume_disk_admission_precedes_database_installation() {
        let temp = tempfile::TempDir::new().unwrap();
        let source_path = temp.path().join("source.sqlite");
        let limits = Limits::default();
        let mut source = Db::open(&source_path, limits).unwrap();
        source
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES (1)")
            })
            .unwrap();
        let batch = source.capture().unwrap();
        let plan = crate::VerifiedPlan::new(&batch.segments, batch.position, limits).unwrap();
        source.close().unwrap();
        let database_bytes = u64::from(batch.segments[0].info().database_pages)
            * u64::from(batch.segments[0].info().page_size);
        let host = crate::Host::default()
            .with_local_disk_budget(crate::DiskBudget::new(database_bytes.saturating_sub(1)));
        let destination = temp.path().join("destination.sqlite");

        let result = Db::resume_with_host(&plan, &destination, limits, host);

        assert!(matches!(result, Err(CrabError::Limit("local disk bytes"))));
        assert!(!destination.exists());
    }

    #[test]
    fn pending_wal_and_captured_segments_reconcile_and_release_disk_admission() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("accounted.sqlite");
        let budget = crate::DiskBudget::new(4 * 1024 * 1024);
        let host = crate::Host::default().with_local_disk_budget(budget.clone());
        let limits = Limits {
            max_capture_bytes: 1024 * 1024,
            ..Limits::default()
        };
        let mut db = Db::open_with_host(&path, limits, host).unwrap();

        db.transaction(|transaction| {
            transaction.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES (1)")
        })
        .unwrap();
        assert_eq!(budget.used(), 2 * limits.max_capture_bytes);

        let batch = db.capture().unwrap();
        let retained = batch
            .segments
            .iter()
            .map(|segment| segment.info().size_bytes)
            .sum::<u64>();
        let wal = std::fs::metadata(format!("{}-wal", path.display()))
            .unwrap()
            .len();
        let database = std::fs::metadata(&path).unwrap().len();
        assert_eq!(budget.used(), database + retained + wal);

        db.close().unwrap();
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn typed_operation_error_rolls_back_and_keeps_writer_usable() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut db = Db::open(&temp.path().join("typed.sqlite"), Limits::default()).unwrap();
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
        let mut db = Db::open(&path, Limits::default()).unwrap();
        db.capture.truncate_page_n = 20;
        db.capture.min_checkpoint_page_n = 10;
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
            let plan =
                crate::VerifiedPlan::new(&segments, batch.position, Limits::default()).unwrap();
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
