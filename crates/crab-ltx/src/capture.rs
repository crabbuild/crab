// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.

use crate::error::{CrabError, Result};
use crate::ltx::{self, lock_pgno};
use crate::wal::WalReader;
use crate::{
    CHECKPOINT_MODE_PASSIVE, CHECKPOINT_MODE_TRUNCATE, META_DIR_SUFFIX, Pos, Txid,
    WAL_FRAME_HEADER_SIZE, WAL_HEADER_SIZE, ltx_file_path,
};
use rusqlite::Connection;

mod checkpoint;
mod verify;
mod wal;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointMode {
    Passive,
    Full,
    Restart,
    Truncate,
}

#[derive(Clone, Copy, Debug)]
struct CheckpointPragma {
    busy: bool,
    wal_frames: i64,
    backfilled: i64,
}

impl std::fmt::Display for CheckpointMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CheckpointMode::Passive => CHECKPOINT_MODE_PASSIVE,
            CheckpointMode::Full => "FULL",
            CheckpointMode::Restart => "RESTART",
            CheckpointMode::Truncate => CHECKPOINT_MODE_TRUNCATE,
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Default)]
struct SyncInfo {
    offset: i64,
    salt1: u32,
    salt2: u32,
    prev_commit: u32,
    snapshotting: bool,
}

const RELATIVE_TRUNCATE_PAGES: u32 = 1024;

#[derive(Clone, Debug)]
struct LastL0Header {
    wal_offset: i64,
    wal_size: i64,
    wal_salt1: u32,
    wal_salt2: u32,
    commit: u32,
    final_pgno: u32,
    final_page: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimingPhase {
    Preparation,
    SchemaCheck,
    WalExistence,
    PositionResolution,
    WalRead,
    PageCollection,
    Verification,
    Encode,
    LocalWrite,
    Fsync,
    ParentSync,
    Checkpoint,
}

pub(crate) struct TimingRecorder {
    started: Instant,
    active: Option<(TimingPhase, Instant)>,
    timing: crate::CaptureTiming,
}

impl TimingRecorder {
    pub(crate) fn new(started: Instant) -> Self {
        Self {
            started,
            active: None,
            timing: crate::CaptureTiming::default(),
        }
    }

    pub(crate) fn begin(&mut self, phase: TimingPhase, now: Instant) {
        if let Some((active, started)) = self.active.take() {
            self.add_phase(active, now.saturating_duration_since(started));
        }
        self.active = Some((phase, now));
    }

    pub(crate) fn end(&mut self, phase: TimingPhase, now: Instant) {
        if let Some((active, started)) = self.active
            && active == phase
        {
            self.add_phase(active, now.saturating_duration_since(started));
            self.active = None;
        }
    }

    pub(crate) fn add_wal_bytes(&mut self, bytes: u64) {
        self.timing.wal_bytes = self.timing.wal_bytes.saturating_add(bytes);
    }

    pub(crate) fn add_database_bytes(&mut self, bytes: u64) {
        self.timing.database_bytes = self.timing.database_bytes.saturating_add(bytes);
    }

    pub(crate) fn add_ltx_bytes(&mut self, bytes: u64) {
        self.timing.ltx_bytes = self.timing.ltx_bytes.saturating_add(bytes);
    }

    pub(crate) fn add_segment(&mut self) {
        self.timing.segment_count = self.timing.segment_count.saturating_add(1);
    }

    pub(crate) fn add_phase_nanos(&mut self, phase: TimingPhase, elapsed: u64) {
        self.add_phase(phase, Duration::from_nanos(elapsed));
    }

    pub(crate) fn observe_wal_image(&mut self, sparse: bool, fallback: bool, bytes: usize) {
        if sparse {
            self.timing.wal_sparse_reads = self.timing.wal_sparse_reads.saturating_add(1);
        } else {
            self.timing.wal_full_reads = self.timing.wal_full_reads.saturating_add(1);
        }
        if fallback {
            self.timing.wal_fallback_reads = self.timing.wal_fallback_reads.saturating_add(1);
        }
        self.timing.wal_image_bytes = self.timing.wal_image_bytes.max(bytes as u64);
    }

    pub(crate) fn observe_wal_transfer(&mut self, file_bytes: u64, read_bytes: u64) {
        self.timing.wal_file_bytes = self.timing.wal_file_bytes.max(file_bytes);
        self.timing.wal_read_bytes = self.timing.wal_read_bytes.saturating_add(read_bytes);
    }

    pub(crate) fn observe_wal_snapshot(&mut self) {
        self.timing.wal_snapshot_reads = self.timing.wal_snapshot_reads.saturating_add(1);
    }

    pub(crate) fn checkpoint_run(&mut self) {
        self.timing.checkpoint_runs = self.timing.checkpoint_runs.saturating_add(1);
    }

    pub(crate) fn checkpoint_result(&mut self, busy: bool, frames: i64, backfilled: i64) {
        self.timing.checkpoint_busy = self.timing.checkpoint_busy.saturating_add(u32::from(busy));
        self.timing.checkpoint_frames = self
            .timing
            .checkpoint_frames
            .saturating_add(u64::try_from(frames.max(0)).unwrap_or_default());
        self.timing.checkpoint_backfilled = self
            .timing
            .checkpoint_backfilled
            .saturating_add(u64::try_from(backfilled.max(0)).unwrap_or_default());
    }

    pub(crate) fn checkpoint_busy_error(&mut self) {
        self.timing.checkpoint_busy_errors = self.timing.checkpoint_busy_errors.saturating_add(1);
    }

    pub(crate) fn checkpoint_restart(&mut self) {
        self.timing.checkpoint_restarts = self.timing.checkpoint_restarts.saturating_add(1);
    }

    pub(crate) fn finish(mut self, now: Instant) -> crate::CaptureTiming {
        if let Some((active, started)) = self.active.take() {
            self.add_phase(active, now.saturating_duration_since(started));
        }
        self.timing.total_nanos = nanos(now.saturating_duration_since(self.started));
        self.timing
    }

    fn add_phase(&mut self, phase: TimingPhase, elapsed: Duration) {
        let target = match phase {
            TimingPhase::Preparation => &mut self.timing.preparation_nanos,
            TimingPhase::SchemaCheck => &mut self.timing.schema_check_nanos,
            TimingPhase::WalExistence => &mut self.timing.wal_existence_nanos,
            TimingPhase::PositionResolution => &mut self.timing.position_resolution_nanos,
            TimingPhase::WalRead => &mut self.timing.wal_read_nanos,
            TimingPhase::PageCollection => &mut self.timing.page_collection_nanos,
            TimingPhase::Verification => &mut self.timing.verification_nanos,
            TimingPhase::Encode => &mut self.timing.encode_nanos,
            TimingPhase::LocalWrite => &mut self.timing.local_write_nanos,
            TimingPhase::Fsync => &mut self.timing.fsync_nanos,
            TimingPhase::ParentSync => &mut self.timing.parent_sync_nanos,
            TimingPhase::Checkpoint => &mut self.timing.checkpoint_nanos,
        };
        *target = target.saturating_add(nanos(elapsed));
    }
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

pub(crate) struct CaptureEngine {
    checksums: crate::pages::PageChecksums,
    host: crate::LtxHost,
    path: PathBuf,
    meta_path: PathBuf,
    conn: Connection,
    rtx_conn: Connection,
    page_size: u32,

    read_lock_held: bool,

    pub min_checkpoint_page_n: u32,
    pub truncate_page_n: u32,
    pub checkpoint_interval: Duration,

    synced_since_checkpoint: bool,
    synced_to_wal_end: bool,
    last_synced_wal_offset: i64,
    last_db_pages: u32,
    checkpointed_wal_offset: i64,
    verified_schema_version: Option<i64>,
    last_l0_header: Option<(Txid, LastL0Header)>,
    last_l0_segment: Option<crate::SegmentInfo>,

    position: Pos,
    l0_dir_ready: bool,
    wal_file: Option<crate::HostFile>,
    timing: Option<TimingRecorder>,
    defer_durability: bool,
}

const CONTROL_TABLES_DDL: &str = "CREATE TABLE IF NOT EXISTS _litestream_seq (id INTEGER PRIMARY KEY, seq INTEGER);\
     CREATE TABLE IF NOT EXISTS _litestream_lock (id INTEGER);";

impl CaptureEngine {
    pub const DEFAULT_MIN_CHECKPOINT_PAGE_N: u32 = 1000;
    pub const DEFAULT_TRUNCATE_PAGE_N: u32 = 121_359;

    pub const DEFAULT_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);
    pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

    pub fn open_with_host(
        path: impl AsRef<Path>,
        host: crate::LtxHost,
        vfs: Option<&str>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let meta_path = Self::meta_path_for(&path);

        let open = |path: &Path| crate::db::open_connection(path, vfs);
        let conn = open(&path).map_err(CrabError::Sqlite)?;

        // All managed writers disable autocheckpoint. The separate long-lived
        // read mark protects the WAL until the capture/checkpoint barrier runs.
        conn.busy_timeout(Self::DEFAULT_BUSY_TIMEOUT)
            .map_err(CrabError::Sqlite)?;
        conn.pragma_update(None, "wal_autocheckpoint", 0)
            .map_err(CrabError::Sqlite)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(CrabError::Sqlite)?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(CrabError::Sqlite)?;

        // Enable WAL; SQLite returns the new mode on success (db.go:849-853).
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(CrabError::Sqlite)?;
        if mode != "wal" {
            return Err(CrabError::Other(
                format!("enable wal failed, mode={mode:?}").into(),
            ));
        }

        conn.execute_batch(CONTROL_TABLES_DDL)
            .map_err(CrabError::Sqlite)?;

        // Dedicated read-lock connection (mirrors a second pooled connection).
        let rtx_conn = open(&path).map_err(CrabError::Sqlite)?;
        rtx_conn
            .busy_timeout(Self::DEFAULT_BUSY_TIMEOUT)
            .map_err(CrabError::Sqlite)?;
        rtx_conn
            .pragma_update(None, "wal_autocheckpoint", 0)
            .map_err(CrabError::Sqlite)?;
        rtx_conn
            .pragma_update(None, "synchronous", "FULL")
            .map_err(CrabError::Sqlite)?;
        rtx_conn
            .pragma_update(None, "foreign_keys", true)
            .map_err(CrabError::Sqlite)?;

        let mut db = Self {
            checksums: crate::pages::PageChecksums::default(),
            host,
            path,
            meta_path,
            conn,
            rtx_conn,
            page_size: 0,
            read_lock_held: false,
            min_checkpoint_page_n: Self::DEFAULT_MIN_CHECKPOINT_PAGE_N,
            truncate_page_n: Self::DEFAULT_TRUNCATE_PAGE_N,
            checkpoint_interval: Self::DEFAULT_CHECKPOINT_INTERVAL,
            synced_since_checkpoint: false,
            synced_to_wal_end: false,
            last_synced_wal_offset: 0,
            last_db_pages: 0,
            checkpointed_wal_offset: 0,
            verified_schema_version: None,
            last_l0_header: None,
            last_l0_segment: None,
            position: Pos::ZERO,
            l0_dir_ready: false,
            wal_file: None,
            timing: None,
            defer_durability: false,
        };

        // Start the long-running read transaction (db.go:867-871).
        db.acquire_read_lock()?;

        // Read page size (db.go:874-878).
        let page_size: i64 = db
            .conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .map_err(CrabError::Sqlite)?;
        if !ltx::is_valid_page_size(page_size as u32) {
            return Err(CrabError::Other(
                format!("invalid db page size: {page_size}").into(),
            ));
        }
        // Validated > 0 above; SQLite page sizes are <= 65536.
        db.page_size = page_size as u32;
        let pages: u32 = db
            .conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .map_err(CrabError::Sqlite)?;
        db.host
            .check_database_size(u64::from(pages) * u64::from(db.page_size))?;

        // Ensure the meta directory exists (db.go:880-883).
        db.host.create_dir_all(&db.meta_path)?;

        // Ensure the WAL has at least one frame (db.go:886-888).
        db.ensure_wal_exists()?;

        Ok(db)
    }

    pub(crate) fn meta_path_for(path: &Path) -> PathBuf {
        // Go: filepath.Join(dir, "."+file+MetaDirSuffix) (db.go:206).
        let dir = path.parent();
        let file = path.file_name().map(|s| s.to_owned()).unwrap_or_default();
        let mut name = std::ffi::OsString::from(".");
        name.push(&file);
        name.push(META_DIR_SUFFIX);
        match dir {
            Some(d) if !d.as_os_str().is_empty() => d.join(name),
            _ => PathBuf::from(name),
        }
    }

    pub fn wal_path(&self) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push("-wal");
        PathBuf::from(s)
    }

    pub fn ltx_path(&self, level: u32, min_txid: Txid, max_txid: Txid) -> String {
        ltx_file_path(&self.meta_path.to_string_lossy(), level, min_txid, max_txid)
    }

    fn acquire_read_lock(&mut self) -> Result<()> {
        if self.read_lock_held {
            return Ok(());
        }
        self.rtx_conn
            .prepare_cached("BEGIN")
            .and_then(|mut statement| statement.execute([]))
            .map_err(CrabError::Sqlite)?;
        // Execute a read query to obtain the read lock. On failure, roll back.
        if let Err(e) = self
            .rtx_conn
            .query_row("SELECT COUNT(1) FROM _litestream_seq", [], |r| {
                r.get::<_, i64>(0)
            })
        {
            let _ = self.rtx_conn.execute_batch("ROLLBACK");
            return Err(CrabError::Sqlite(e));
        }
        self.read_lock_held = true;
        Ok(())
    }

    fn release_read_lock(&mut self) -> Result<()> {
        if !self.read_lock_held {
            return Ok(());
        }
        self.read_lock_held = false;
        rollback(&self.rtx_conn)
    }

    fn ensure_control_tables(&mut self) -> Result<()> {
        // A swept control table is a schema change, and every schema change
        // bumps SQLite's schema version — so an unchanged version proves
        // the last verification still holds and the DDL (a full parse and
        // execute per statement) can be skipped on the hot capture path.
        // Through the statement cache: this guard runs on every sync, and a
        // fresh `PRAGMA` per sync was one SQL compilation per capture on the
        // fleet profile.
        let version: i64 = self
            .conn
            .prepare_cached("PRAGMA schema_version")
            .and_then(|mut statement| statement.query_row([], |row| row.get(0)))
            .map_err(CrabError::Sqlite)?;
        if self.verified_schema_version == Some(version) {
            return Ok(());
        }
        self.conn
            .execute_batch(CONTROL_TABLES_DDL)
            .map_err(CrabError::Sqlite)?;
        let verified: i64 = self
            .conn
            .prepare_cached("PRAGMA schema_version")
            .and_then(|mut statement| statement.query_row([], |row| row.get(0)))
            .map_err(CrabError::Sqlite)?;
        self.verified_schema_version = Some(verified);
        Ok(())
    }

    fn with_wal_file<T>(
        &mut self,
        op: impl Fn(&mut crate::HostFile) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let mut attempt = 0;
        loop {
            if self.wal_file.is_none() {
                self.wal_file = Some(self.host.open(&self.wal_path())?);
            }
            let file = self
                .wal_file
                .as_mut()
                .ok_or_else(|| std::io::Error::other("WAL handle missing"))?;
            match op(file) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    self.wal_file = None;
                    let retry = attempt == 0
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::UnexpectedEof
                        );
                    if !retry {
                        return Err(error);
                    }
                    attempt += 1;
                }
            }
        }
    }

    fn wal_header_bytes(&mut self) -> Result<[u8; WAL_HEADER_SIZE]> {
        let bytes = self.with_wal_file(|file| file.read_exact_at(0, WAL_HEADER_SIZE))?;
        self.timing_observe_wal_transfer(0, bytes.len() as u64);
        bytes.try_into().map_err(|_| CrabError::LTXCorrupted)
    }

    fn wal_bytes_at(&mut self, offset: i64, n: i64) -> Result<Vec<u8>> {
        let bytes = self.with_wal_file(|file| file.read_exact_at(offset as u64, n as usize))?;
        self.timing_observe_wal_transfer(0, bytes.len() as u64);
        Ok(bytes)
    }

    fn read_whole_wal(&mut self) -> Result<Vec<u8>> {
        let bytes = self.host.read(&self.wal_path())?;
        self.timing_observe_wal_transfer(bytes.len() as u64, bytes.len() as u64);
        Ok(bytes)
    }

    fn ensure_wal_exists(&mut self) -> Result<()> {
        if self.wal_file_size()? >= WAL_HEADER_SIZE as i64 {
            return Ok(());
        }
        self.conn
            .execute_batch(
                "INSERT INTO _litestream_seq (id, seq) VALUES (1, 1) \
                 ON CONFLICT (id) DO UPDATE SET seq = seq + 1",
            )
            .map_err(CrabError::Sqlite)?;
        Ok(())
    }

    fn wal_file_size(&mut self) -> Result<i64> {
        match self.with_wal_file(|file| file.file_len()) {
            Ok(len) => {
                self.timing_observe_wal_transfer(len, 0);
                Ok(len as i64)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    fn db_file_size(&self) -> Result<i64> {
        match self.host.metadata(&self.path) {
            Ok(md) => Ok(md.len as i64),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    pub fn pos(&self) -> Pos {
        self.position
    }

    pub(crate) fn sealed_l0_segment(&self, txid: Txid) -> Option<crate::SegmentInfo> {
        self.last_l0_segment
            .as_ref()
            .filter(|info| info.max_txid == txid.0)
            .cloned()
    }

    pub(crate) fn seed_continuation(
        &mut self,
        position: crate::Position,
        checksums: crate::pages::PageChecksums,
        page_size: u32,
        count: u32,
    ) -> Result<()> {
        if self.position != Pos::ZERO
            || page_size != self.page_size
            || checksums.checksum() != position.checksum
            || position.txid == 0
        {
            return Err(CrabError::ChecksumMismatch);
        }
        let wal = self.wal_header_bytes()?;
        self.last_l0_segment = None;
        self.last_l0_header = Some((
            Txid(position.txid),
            LastL0Header {
                wal_offset: WAL_HEADER_SIZE as i64,
                wal_size: 0,
                wal_salt1: be_u32(&wal[16..]),
                wal_salt2: be_u32(&wal[20..]),
                commit: count,
                final_pgno: 0,
                final_page: Vec::new(),
            },
        ));
        self.position = Pos::new(Txid(position.txid), position.checksum);
        self.checksums = checksums;
        self.last_db_pages = count;
        Ok(())
    }

    pub(crate) fn start_timing(&mut self, now: Instant) {
        self.timing = Some(TimingRecorder::new(now));
    }

    pub(crate) fn finish_timing(&mut self, now: Instant) -> crate::CaptureTiming {
        self.timing
            .take()
            .map(|recorder| recorder.finish(now))
            .unwrap_or_default()
    }

    pub(crate) fn timing_begin(&mut self, phase: TimingPhase) {
        if self.timing.is_some() {
            let now = self.host.now_monotonic();
            if let Some(recorder) = &mut self.timing {
                recorder.begin(phase, now);
            }
        }
    }

    pub(crate) fn timing_end(&mut self, phase: TimingPhase) {
        if self.timing.is_some() {
            let now = self.host.now_monotonic();
            if let Some(recorder) = &mut self.timing {
                recorder.end(phase, now);
            }
        }
    }

    pub(crate) fn timing_add_wal_bytes(&mut self, bytes: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_wal_bytes(bytes);
        }
    }

    pub(crate) fn timing_add_database_bytes(&mut self, bytes: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_database_bytes(bytes);
        }
    }

    pub(crate) fn timing_add_ltx_bytes(&mut self, bytes: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_ltx_bytes(bytes);
        }
    }

    pub(crate) fn timing_add_segment(&mut self) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_segment();
        }
    }

    pub(crate) fn timing_add_phase_nanos(&mut self, phase: TimingPhase, elapsed: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_phase_nanos(phase, elapsed);
        }
    }

    pub(crate) fn timing_observe_wal_image(&mut self, sparse: bool, fallback: bool, bytes: usize) {
        if let Some(recorder) = &mut self.timing {
            recorder.observe_wal_image(sparse, fallback, bytes);
        }
    }

    pub(crate) fn timing_observe_wal_transfer(&mut self, file_bytes: u64, read_bytes: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.observe_wal_transfer(file_bytes, read_bytes);
        }
    }

    pub(crate) fn timing_observe_wal_snapshot(&mut self) {
        if let Some(recorder) = &mut self.timing {
            recorder.observe_wal_snapshot();
        }
    }

    pub fn sync(&mut self, required: Option<crate::commit::WalCut>) -> Result<()> {
        self.sync_impl(required)
    }

    pub(crate) fn sync_deferred(&mut self, required: Option<crate::commit::WalCut>) -> Result<()> {
        let previous = self.defer_durability;
        self.defer_durability = true;
        let result = self.sync(required);
        self.defer_durability = previous;
        result
    }

    fn sync_impl(&mut self, required: Option<crate::commit::WalCut>) -> Result<()> {
        // Self-heal: recreate the control tables if something swept them out
        // of `sqlite_schema` from under the replicator — without them every
        // capture fails until the database is reopened. A no-op when the
        // tables exist (no schema change, no WAL write).
        self.timing_begin(TimingPhase::SchemaCheck);
        let schema_result = self.ensure_control_tables();
        self.timing_end(TimingPhase::SchemaCheck);
        schema_result?;

        // Ensure the WAL has at least one frame (db.go:1017-1020).
        self.timing_begin(TimingPhase::WalExistence);
        let wal_result = self.ensure_wal_exists();
        self.timing_end(TimingPhase::WalExistence);
        wal_result?;

        let (orig_wal_size, new_wal_size, synced) = self.verify_and_sync()?;

        // Validate the application's WAL hook boundary BEFORE any checkpoint
        // can replace its salts or write control frames. A valid earlier cut
        // alone does not prove the last committed transaction was captured.
        if let Some(required) = required {
            let (_, header) = self
                .last_l0_header
                .as_ref()
                .ok_or(CrabError::LTXCorrupted)?;
            let end = WAL_HEADER_SIZE as i64
                + i64::from(required.frames)
                    * (i64::from(self.page_size) + WAL_FRAME_HEADER_SIZE as i64);
            if header.wal_salt1 != required.salt1
                || header.wal_salt2 != required.salt2
                || self.last_synced_wal_offset < end
            {
                return Err(CrabError::LTXCorrupted);
            }
        }

        // Track that data was synced for time-based checkpoint decisions.
        if synced {
            self.synced_since_checkpoint = true;
        }

        self.checkpoint_if_needed(orig_wal_size, new_wal_size)?;

        Ok(())
    }

    fn verify_and_sync(&mut self) -> Result<(i64, i64, bool)> {
        // Use the last synced WAL offset as the logical size for checkpoint
        // decisions; on the first sync fall back to file size (db.go:1062-1069).
        let mut orig_wal_size = self.last_synced_wal_offset;
        if orig_wal_size == 0 {
            orig_wal_size = self.wal_file_size()?;
        }

        self.timing_begin(TimingPhase::PositionResolution);
        let info_result = self.verify();
        self.timing_end(TimingPhase::PositionResolution);
        let info = info_result?;

        let synced = self.sync_inner(info)?;

        let new_wal_size = self.last_synced_wal_offset;
        Ok((orig_wal_size, new_wal_size, synced))
    }

    pub fn snapshot_to_writer<W: std::io::Write>(&mut self, w: &mut W) -> Result<Pos> {
        if self.page_size == 0 {
            return Err(CrabError::Other(
                "db not ready: page size not initialized".into(),
            ));
        }

        let pos = self.position;

        let db_size = self.db_file_size()?;
        let mut commit = (db_size / self.page_size as i64) as u32;

        let wal = WalImage::whole(self.read_whole_wal()?);
        let mut rd = WalReader::new(&wal.bytes).map_err(CrabError::from)?;
        let (page_map, max_offset, wal_commit) = rd.page_map().map_err(CrabError::from)?;
        if wal_commit > 0 {
            commit = wal_commit;
        }
        let wal_offset = rd.offset();
        let sz = if max_offset > 0 {
            max_offset - wal_offset
        } else {
            0
        };
        let (salt1, salt2) = rd.salt();

        self.host
            .check_database_size(u64::from(commit) * u64::from(self.page_size))?;
        let header = ltx::Header {
            version: ltx::VERSION,
            flags: 0,
            page_size: self.page_size,
            commit,
            min_txid: Txid(1),
            max_txid: pos.txid,
            timestamp: self.host.now_unix_millis(),
            pre_apply_checksum: 0,
            wal_offset,
            wal_size: sz,
            wal_salt1: salt1,
            wal_salt2: salt2,
            node_id: 0,
        };

        // A snapshot tracks the rolling post-apply checksum (MinTXID==1, no
        // NoChecksum flag). Encode each page as it is read so callers can back
        // the writer with a bounded scratch file instead of a database-sized
        // resident buffer.
        let lock = lock_pgno(self.page_size);
        let mut rolling: crate::Checksum = crate::CHECKSUM_FLAG;
        let mut encoder = crate::codec::Encoder::new_block(w);
        encoder.encode_header(header)?;
        for pgno in (1..=commit).filter(|page| *page != lock) {
            let data = self.capture_page(&wal, &page_map, pgno)?;
            rolling = crate::CHECKSUM_FLAG | (rolling ^ ltx::checksum_page(pgno, &data));
            encoder.encode_page(ltx::PageHeader { pgno, flags: 0 }, &data)?;
        }
        encoder.close(rolling)?;

        Ok(Pos::new(pos.txid, rolling))
    }

    pub fn close(mut self) -> Result<()> {
        self.release_read_lock()?;
        // The connection is dropped here, closing it.
        Ok(())
    }
}

fn calc_wal_size(page_size: u32, page_n: u32) -> i64 {
    WAL_HEADER_SIZE as i64 + (WAL_FRAME_HEADER_SIZE as i64 + page_size as i64) * page_n as i64
}

fn rollback(conn: &Connection) -> Result<()> {
    // SQLite can auto-rollback on I/O failure. Check native state instead of
    // swallowing arbitrary errors whose text happens to mention rollback.
    if conn.is_autocommit() {
        return Ok(());
    }
    conn.execute_batch("ROLLBACK").map_err(CrabError::Sqlite)
}

struct WalImage {
    bytes: Vec<u8>,
    tail_base: usize,
}

impl WalImage {
    fn whole(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            tail_base: 0,
        }
    }

    fn reader_at(
        &self,
        offset: i64,
        salt1: u32,
        salt2: u32,
    ) -> std::result::Result<WalReader<'_>, crate::wal::WalError> {
        if self.tail_base == 0 {
            WalReader::new_with_offset(&self.bytes, offset, salt1, salt2)
        } else {
            WalReader::new_with_offset_over_tail(
                &self.bytes,
                self.tail_base as i64,
                offset,
                salt1,
                salt2,
            )
        }
    }

    fn slice(&self, offset: i64, n: usize) -> Option<&[u8]> {
        if offset < 0 {
            return None;
        }
        let offset = offset as usize;
        let start = if self.tail_base == 0 || offset < WAL_HEADER_SIZE {
            offset
        } else {
            WAL_HEADER_SIZE + offset.checked_sub(self.tail_base)?
        };
        let end = start.checked_add(n)?;
        (end <= self.bytes.len()).then(|| &self.bytes[start..end])
    }

    fn page(&self, offset: i64, page_size: u32) -> Result<Vec<u8>> {
        self.slice(offset + WAL_FRAME_HEADER_SIZE as i64, page_size as usize)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                CrabError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("short read wal page @ {offset}"),
                ))
            })
    }
}

#[inline]
fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn recorder_uses_monotonic_instants_without_affecting_capture_state() {
        let start = Instant::now();
        let mut recorder = TimingRecorder::new(start);
        recorder.begin(TimingPhase::Preparation, start + Duration::from_millis(1));
        recorder.end(TimingPhase::Preparation, start + Duration::from_millis(3));
        recorder.begin(TimingPhase::WalRead, start + Duration::from_millis(4));
        recorder.end(TimingPhase::WalRead, start + Duration::from_millis(9));
        recorder.add_wal_bytes(11);
        recorder.add_database_bytes(22);
        recorder.add_ltx_bytes(33);
        recorder.add_segment();

        let timing = recorder.finish(start + Duration::from_millis(10));
        assert_eq!(timing.total_nanos, 10_000_000);
        assert_eq!(timing.preparation_nanos, 2_000_000);
        assert_eq!(timing.wal_read_nanos, 5_000_000);
        assert_eq!(timing.wal_bytes, 11);
        assert_eq!(timing.database_bytes, 22);
        assert_eq!(timing.ltx_bytes, 33);
        assert_eq!(timing.segment_count, 1);
    }

    #[test]
    fn recorder_closes_an_incomplete_phase_and_saturates_counters() {
        let start = Instant::now();
        let mut recorder = TimingRecorder::new(start);
        recorder.begin(TimingPhase::Encode, start);
        recorder.add_wal_bytes(u64::MAX);
        recorder.add_wal_bytes(1);
        recorder.add_segment();
        recorder.add_segment();

        let timing = recorder.finish(start + Duration::from_nanos(7));
        assert_eq!(timing.encode_nanos, 7);
        assert_eq!(timing.wal_bytes, u64::MAX);
        assert_eq!(timing.segment_count, 2);
    }
}
