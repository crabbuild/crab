//! Managed database over the local replica: admissions, checkpoints, and cut publishing.
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
    /// Publication is the durability proof for selected deferred captures, so
    /// they are reverified and unlinked without a local durability barrier.
    /// Every session owns a fresh metadata directory, so a crash-resurrected
    /// local name remains quarantined rather than becoming acknowledged state.
    /// An error retains unfinished accounting so the owner can retry or discard
    /// the complete local session.
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
            let bytes = self.host.read(segment.path(), segment.info().size_bytes)?;
            crate::recovery::verify_segment(&bytes, segment.info(), self.limits)?;
            // The published immutable root, not durable local deletion, releases
            // the result. A fresh session never adopts crash-resurrected residue.
            self.host.filesystem.remove_file(segment.path())?;
            self.pending_durability
                .retain(|pending| pending != segment.path());
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
        let materialized = plan.materialize();
        let limits = limits.validate()?;
        if u64::from(materialized.database_pages) * u64::from(materialized.page_size)
            > limits.max_database_bytes
        {
            return Err(CrabError::Limit(crate::LimitKind::DatabaseBytes));
        }
        let database_bytes =
            u64::from(materialized.database_pages) * u64::from(materialized.page_size);
        let local_disk = host.reserve_local_disk(database_bytes)?;
        host.restore_materialized(materialized, destination)?;
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
        let capture = CaptureEngine::open_with_host(path, host, vfs, limits.max_capture_bytes)?;
        let writer = open_connection(path, vfs)?;
        writer.busy_timeout(std::time::Duration::from_secs(1))?;
        writer.pragma_update(None, "wal_autocheckpoint", 0)?;
        writer.pragma_update(None, "synchronous", "FULL")?;
        writer.pragma_update(None, "foreign_keys", true)?;
        let page_size: u32 = writer.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let max_pages = limits.max_database_bytes / u64::from(page_size);
        if max_pages == 0 {
            return Err(CrabError::Limit(crate::LimitKind::DatabasePageSize));
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
    ///
    /// A commit larger than `Limits::max_capture_bytes` is not refused: the
    /// later capture represents it as a full database image, which is bounded
    /// by `Limits::max_file_bytes`, so a large write can never leave a local
    /// commit that the session cannot capture.
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
            .map_err(crate::TransactionError::Admission)?;
        let disk_before = self.local_disk.bytes();
        let write_bytes = self.limits.max_capture_bytes.checked_mul(2).ok_or(
            crate::TransactionError::Admission(CrabError::Limit(crate::LimitKind::LocalDiskBytes)),
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
                // FULL, interrupt, and ROLLBACK constraints can end the whole
                // transaction. A second rollback would falsely fence the writer;
                // autocommit proves rollback only if no WAL commit was observed.
                let rollback = if tx.is_autocommit() && self.observer.frames() == 0 {
                    drop(tx);
                    Ok(())
                } else {
                    tx.rollback()
                };
                if let Err(rollback) = rollback {
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
    /// A failure that can leave partial local state fences further use. A
    /// declared capacity refusal happens before the cut is written, so the
    /// session stays active with [`Self::has_pending_capture`] reporting the
    /// uncaptured commit; the host may clear the obstruction and retry, or
    /// discard the session and restore authoritative state. Retain returned
    /// files until the canonical Cell root publishes; `prune_captured` can then
    /// release one exact acknowledged batch.
    pub fn capture(&mut self) -> Result<CaptureBatch> {
        self.ensure_active()?;
        self.flush_pending_durability()?;
        let (result, timing, wrote_cut) = self.capture_inner(false);
        #[cfg(feature = "replica")]
        self.host.observe_ltx_capture(&timing, result.is_ok());
        let result = result.map(|mut batch| {
            batch.timing = timing;
            batch
        });
        // A failure after the cut writer started can leave partial local state,
        // so the session fences. A refusal raised before the writer starts only
        // declined work, and the pending WAL cut stays recoverable.
        if result.is_err() && wrote_cut {
            self.fenced = true;
        }
        result
    }

    /// Captures committed WAL pages while deferring their durability barrier.
    ///
    /// LTX files are complete and readable when this returns, but their contents
    /// and names are not durable until [`Self::durability_barrier`] succeeds.
    /// This permits a host to group several captures behind one storage flush;
    /// callers must complete the barrier before acknowledging a locally durable
    /// batch. A higher-level protocol may instead publish the exact bytes to its
    /// own durability boundary, then pass that published batch to
    /// `prune_captured`. A failed local barrier fences the session.
    ///
    /// As with [`Self::capture`], a declared capacity refusal that happens
    /// before any cut is written leaves the session active with
    /// [`Self::has_pending_capture`] set.
    pub fn capture_deferred(&mut self) -> Result<CaptureBatch> {
        self.ensure_active()?;
        let (result, timing, wrote_cut) = self.capture_inner(true);
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
        if result.is_err() && wrote_cut {
            self.fenced = true;
        }
        result
    }

    /// Makes all files published by deferred captures durable as one barrier.
    ///
    /// The barrier syncs each completed file, each destination directory once,
    /// and the new LTX directory chain once per session. If any step fails, the
    /// session is fenced; no caller may acknowledge the pending paths.
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
        .map_err(CrabError::from)
        // A new LTX parent name cannot survive merely because its own contents did.
        .and_then(|()| self.capture.sync_l0_ancestors());
        if result.is_ok() {
            self.pending_durability.clear();
        } else {
            self.fenced = true;
        }
        result
    }

    /// Runs one capture attempt.
    ///
    /// The returned flag reports whether the attempt could have written local
    /// cut state: a refusal raised before the writer starts leaves the session
    /// intact, while any later failure requires fencing.
    fn capture_inner(
        &mut self,
        defer_durability: bool,
    ) -> (Result<CaptureBatch>, crate::CaptureTiming, bool) {
        self.capture.start_timing(self.host.now_monotonic());
        self.capture
            .timing_begin(crate::capture::TimingPhase::Preparation);
        if let Err(error) = self.ensure_capacity() {
            self.capture
                .timing_end(crate::capture::TimingPhase::Preparation);
            let timing = self.capture.finish_timing(self.host.now_monotonic());
            return (Err(error), timing, false);
        }
        let result = (|| {
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
        (result, timing, true)
    }

    fn collect_cuts(&mut self, before: crate::Pos) -> Result<CaptureBatch> {
        let after = self.capture.pos();
        let mut segments = Vec::new();
        if after.txid.0 > before.txid.0 {
            for txid in before.txid.0 + 1..=after.txid.0 {
                let path = PathBuf::from(self.capture.ltx_path(0, Txid(txid), Txid(txid)));
                let info = if let Some(info) = self.capture.take_sealed_l0_segment(Txid(txid)) {
                    info
                } else {
                    // The inspection limit is the full-file bound: a captured
                    // cut may be a full database image, which the writer already
                    // bounded by `max_file_bytes`.
                    let file = crate::LtxHost {
                        facilities: self.host.clone(),
                        max_database_bytes: self.limits.max_database_bytes,
                        max_file_bytes: self.limits.max_file_bytes,
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
                // The capture writer already enforced the tighter incremental
                // bound for delta cuts, so retention accounting only has to
                // honor the file bound shared by every cut.
                self.account(&info)?;
                let segment = LocalSegment::new(path, info);
                #[cfg(feature = "replica")]
                let segment = match self.capture.take_sealed_l0_captured_index(Txid(txid)) {
                    Some(index) => segment.with_captured_index(index),
                    None => segment,
                };
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
        let (initial, mut timing, _) = self.capture_inner(false);
        let result = (|| {
            let mut batch = initial?;
            self.local_disk.try_grow(
                self.limits
                    .max_capture_bytes
                    .checked_mul(2)
                    .ok_or(CrabError::Limit(crate::LimitKind::LocalDiskBytes))?,
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

    /// Writes the continuation a later [`Db::open_resumed`] continues from.
    ///
    /// The session must be drained: a pending capture means a local commit has
    /// no published cut, so the continuation would name a position no root
    /// owns. A sparse activation must be fully materialized, because its
    /// local file holds zeros where no page was ever faulted in. The record
    /// authorizes nothing by itself — the caller still has to prove that the
    /// file holds one authoritative root before opening it.
    #[cfg(feature = "replica")]
    pub fn persist_continuation(&self) -> Result<()> {
        self.ensure_active()?;
        if self.has_pending_capture() {
            return Err(CrabError::InvalidState(
                "capture continuation requires a drained database",
            ));
        }
        if self.sparse && !self.hydration()?.is_some_and(crate::Hydration::complete) {
            return Err(CrabError::InvalidState(
                "capture continuation requires a fully materialized activation",
            ));
        }
        let position = self.position();
        let pages = self.capture.checksums().count();
        let page_size = self.capture.page_size();
        if position.txid == 0 || pages == 0 || position.checksum & crate::CHECKSUM_FLAG == 0 {
            return Err(CrabError::InvalidState(
                "capture continuation requires a committed position",
            ));
        }
        crate::resume::write_checksums(&self.host, &self.path, self.capture.checksums())?;
        crate::resume::write_continuation(
            &self.host,
            &self.path,
            &crate::resume::Continuation {
                position,
                page_size,
                pages,
            },
        )
    }

    /// Opens a database that a clean close left resumable at the same path.
    ///
    /// Reads the continuation and dense page checksums recorded beside the file,
    /// requires the file to be exactly the recorded image, and seeds the capture
    /// session from them. No origin object is read, so the caller must have
    /// matched its own resume record against the authoritative control first.
    #[cfg(feature = "replica")]
    pub fn open_resumed(path: &Path, limits: Limits) -> Result<Self> {
        Self::open_resumed_with_host(path, limits, crate::Host::default())
    }

    /// Opens a resumable database using the host's filesystem, SQLite VFS, and clock.
    #[cfg(feature = "replica")]
    pub fn open_resumed_with_host(path: &Path, limits: Limits, host: crate::Host) -> Result<Self> {
        let limits = limits.validate()?;
        let continuation = crate::resume::read_continuation(&host, path)?;
        let database_bytes = u64::from(continuation.pages) * u64::from(continuation.page_size);
        if database_bytes > limits.max_database_bytes {
            return Err(CrabError::Limit(crate::LimitKind::DatabaseBytes));
        }
        if host.filesystem.file_len(path)? != database_bytes {
            return Err(CrabError::InvalidState(
                "resumed database length does not match its continuation",
            ));
        }
        let ltx_host = crate::LtxHost {
            facilities: host.clone(),
            max_database_bytes: limits.max_database_bytes,
            max_file_bytes: limits.max_database_bytes,
        };
        let checksums = crate::pages::PageChecksums::from_file(
            crate::LtxHost {
                facilities: host.clone(),
                max_database_bytes: limits.max_database_bytes,
                max_file_bytes: limits.max_database_bytes,
            },
            &crate::resume::checksum_path(path),
            continuation.page_size,
            continuation.pages,
            continuation.position.checksum,
        )?;
        // The continuation and sidecar name a published image; a same-length
        // local corruption must fall back to the authoritative root.
        checksums.verify_database(&ltx_host, path, continuation.page_size)?;
        let vfs = host.sqlite_vfs.clone();
        let local_disk = host.reserve_local_disk(database_bytes)?;
        let mut db = Self::open_inner(path, limits, vfs.as_deref(), host, false, Some(local_disk))?;
        db.capture.seed_continuation(
            continuation.position,
            checksums,
            continuation.page_size,
            continuation.pages,
        )?;
        Ok(db)
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
        let (batch, timing, _) = self.capture_inner(false);
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
            return Err(CrabError::Limit(crate::LimitKind::RetainedCaptureArtifacts));
        }
        Ok(())
    }

    /// Reports whether a committed WAL cut is waiting for capture.
    ///
    /// While this is true the local database holds a commit that no LTX file
    /// covers, so the host must not acknowledge it, serve it, or reuse the
    /// session's local state. Retry [`Db::capture`] after clearing the
    /// obstruction, or discard the session and restore authoritative state.
    #[must_use]
    pub fn has_pending_capture(&self) -> bool {
        self.required_cut.is_some()
    }

    fn account(&mut self, info: &SegmentInfo) -> Result<()> {
        if info.size_bytes > self.limits.max_file_bytes {
            return Err(CrabError::Limit(crate::LimitKind::LtxFileBytes));
        }
        self.retained_bytes = self
            .retained_bytes
            .checked_add(info.size_bytes)
            .ok_or(CrabError::Limit(crate::LimitKind::RetainedBytes))?;
        self.retained_segments += 1;
        if self.retained_segments > self.limits.max_segments
            || self.retained_bytes > self.limits.max_plan_bytes
        {
            return Err(CrabError::Limit(crate::LimitKind::RetainedCaptureArtifacts));
        }
        Ok(())
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
            .ok_or(CrabError::Limit(crate::LimitKind::LocalDiskBytes))?
            .checked_add(wal_bytes)
            .ok_or(CrabError::Limit(crate::LimitKind::LocalDiskBytes))?;
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
mod tests;
