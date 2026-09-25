use super::*;
use crate::{VerifiedPlan, restore_exact};
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
    // One retained cut fills the session plan budget, so the next capture is
    // refused before the writer starts and still reports its bounded ledger.
    let limits = Limits {
        max_segments: 1,
        ..Limits::default()
    };
    let mut db = Db::open_with_host(&temp.path().join("failed.sqlite"), limits, host).unwrap();
    db.transaction(|tx| {
        tx.execute_batch(
            "CREATE TABLE events(value BLOB); INSERT INTO events VALUES(randomblob(4096))",
        )
    })
    .unwrap();
    db.capture().unwrap();

    // A checkpoint captures first, so its refusal is raised before the writer
    // starts: the failed attempt still emits its bounded ledger.
    assert!(matches!(
        db.checkpoint(crate::CheckpointMode::Passive),
        Err(CrabError::Limit(crate::LimitKind::RetainedCaptureArtifacts))
    ));
    let attempts = telemetry.0.lock().unwrap();
    assert_eq!(attempts.len(), 2);
    assert!(attempts[0].1);
    assert!(!attempts[1].1);
    assert!(attempts[1].0.total_nanos > 0);
    assert_eq!(attempts[1].0.wal_read_bytes, 0);
    assert!(attempts[0].0.wal_read_bytes > 0);
}

#[cfg(feature = "replica")]
#[test]
fn captured_indexes_match_independent_ltx_inspection() {
    let temp = tempfile::TempDir::new().unwrap();
    let mut db = Db::open(&temp.path().join("indexed.sqlite"), Limits::default()).unwrap();
    db.transaction(|transaction| {
        transaction.execute_batch(
            "CREATE TABLE payload(value BLOB); \
             INSERT INTO payload VALUES(randomblob(200000))",
        )
    })
    .unwrap();

    let batch = db.capture().unwrap();

    for segment in &batch.segments {
        let file = std::fs::File::open(segment.path()).unwrap();
        let (decoded, size, digest, pages) = crate::ltx::inspect_reader_with_index(file).unwrap();
        assert_eq!(
            crate::SegmentInfo::from_inspected(&decoded, size, digest),
            *segment.info()
        );
        assert_eq!(
            segment.captured_index().unwrap().as_ref(),
            crate::paged::encode_index_from_pages(&pages)
                .unwrap()
                .as_slice()
        );
        let cloned = segment.clone();
        assert_eq!(
            segment.captured_index().unwrap().as_ptr(),
            cloned.captured_index().unwrap().as_ptr()
        );
    }
    db.close().unwrap();
}

#[test]
fn managed_connections_disable_sqlite_lookaside() {
    use rusqlite::ffi;

    let temp = tempfile::TempDir::new().unwrap();
    for index in 0..MANAGED_SQLITE_CONNECTIONS {
        let connection =
            open_connection(&temp.path().join(format!("lookaside-{index}.sqlite")), None).unwrap();
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
fn oversized_commit_captures_as_a_full_image_instead_of_fencing() {
    let temp = tempfile::TempDir::new().unwrap();
    let limits = Limits {
        max_capture_bytes: 8 * 1024,
        ..Limits::default()
    };
    let mut db = Db::open(&temp.path().join("oversized.sqlite"), limits).unwrap();
    db.transaction(|tx| tx.execute_batch("CREATE TABLE payload(value BLOB)"))
        .unwrap();
    let first = db.capture().unwrap();

    // A commit whose delta cannot fit the incremental bound must still be
    // captured: the writer escalates it to a full database image bounded by
    // the file bound instead of fencing the session.
    db.transaction(|tx| tx.execute_batch("INSERT INTO payload VALUES(randomblob(65536))"))
        .unwrap();
    let second = db.capture().unwrap();
    assert_eq!(second.segments.len(), 1);
    let escalated = &second.segments[0];
    assert!(
        escalated.info().size_bytes > limits.max_capture_bytes,
        "the escalated cut must exceed the incremental bound"
    );
    assert!(escalated.info().size_bytes <= limits.max_file_bytes);

    // The escalated cut stays a valid chain element: a plan over both cuts
    // restores the exact database.
    let mut segments = first.segments.clone();
    segments.extend(second.segments.iter().cloned());
    let plan = VerifiedPlan::new(&segments, second.position, limits).unwrap();
    let destination = temp.path().join("restored.sqlite");
    assert_eq!(restore_exact(&plan, &destination).unwrap(), second.position);
    let connection = Connection::open(&destination).unwrap();
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM payload", [], |row| row.get(0))
        .unwrap();
    let bytes: i64 = connection
        .query_row("SELECT length(value) FROM payload", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(bytes, 65536);
}

#[test]
fn truncate_boundary_image_may_exceed_the_incremental_bound() {
    let temp = tempfile::TempDir::new().unwrap();
    let limits = Limits {
        max_capture_bytes: 64 * 1024,
        ..Limits::default()
    };
    let mut db = Db::open(&temp.path().join("boundary.sqlite"), limits).unwrap();
    db.transaction(|tx| {
        tx.execute_batch(
            "CREATE TABLE payload(value BLOB); CREATE INDEX payload_len ON payload(length(value))",
        )
    })
    .unwrap();
    let mut segments = db.capture().unwrap().segments;
    for _ in 0..20 {
        db.transaction(|tx| tx.execute_batch("INSERT INTO payload VALUES(randomblob(8192))"))
            .unwrap();
        segments.extend(db.capture().unwrap().segments);
    }

    // A truncate checkpoint writes the whole database as one boundary image.
    // That image legitimately exceeds the incremental bound and is bounded by
    // the file bound instead of failing and fencing the session.
    let batch = db.checkpoint(crate::CheckpointMode::Truncate).unwrap();
    let position = batch.position;
    segments.extend(batch.segments);
    let largest = segments
        .iter()
        .map(|segment| segment.info().size_bytes)
        .max()
        .unwrap();
    assert!(
        largest > limits.max_capture_bytes,
        "the boundary image must exceed the incremental bound"
    );
    assert!(largest <= limits.max_file_bytes);

    let plan = VerifiedPlan::new(&segments, position, limits).unwrap();
    let destination = temp.path().join("boundary-restored.sqlite");
    assert_eq!(restore_exact(&plan, &destination).unwrap(), position);
    let connection = Connection::open(&destination).unwrap();
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM payload", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 20);
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

    assert!(matches!(
        result,
        Err(CrabError::Limit(crate::LimitKind::LocalDiskBytes))
    ));
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

    assert!(matches!(
        result,
        Err(CrabError::Limit(crate::LimitKind::LocalDiskBytes))
    ));
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

    assert!(matches!(
        result,
        Err(CrabError::Limit(crate::LimitKind::LocalDiskBytes))
    ));
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
        let plan = crate::VerifiedPlan::new(&segments, batch.position, Limits::default()).unwrap();
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
        crate::compact_exact(&plan, &temp.path().join(format!("compacted-{round}.ltx"))).unwrap();
    }
    assert!(shrank);
    assert!(multiple_cuts);
}

#[cfg(feature = "replica")]
fn committed_database(path: &Path, limits: Limits) -> Db {
    let mut db = Db::open(path, limits).unwrap();
    db.transaction(|tx| tx.execute_batch("CREATE TABLE payload(value INTEGER)"))
        .unwrap();
    db.transaction(|tx| tx.execute("INSERT INTO payload VALUES (7)", []))
        .unwrap();
    db.capture().unwrap();
    db
}

#[cfg(feature = "replica")]
fn resumed_payload(db: &mut Db) -> i64 {
    db.query_with(|connection| {
        connection.query_row("SELECT count(*) FROM payload", [], |row| row.get(0))
    })
    .unwrap()
}

#[cfg(feature = "replica")]
#[test]
fn a_recorded_continuation_continues_the_chain_after_a_move() {
    let temp = tempfile::TempDir::new().unwrap();
    let limits = Limits::default();
    let source = temp.path().join("session.sqlite");
    let db = committed_database(&source, limits);
    assert!(!db.has_pending_capture());
    db.persist_continuation().unwrap();
    let position = db.position();
    db.close().unwrap();

    // The capture session directory fences the original path, so a resumed
    // activation always installs the file somewhere nothing has claimed.
    let host = crate::Host::default();
    let destination = temp.path().join("warm.sqlite");
    crate::resume::move_resumed(&source, &destination, &host).unwrap();
    let mut resumed = Db::open_resumed_with_host(&destination, limits, host).unwrap();
    assert_eq!(resumed.position(), position);
    assert_eq!(resumed_payload(&mut resumed), 1);

    resumed
        .transaction(|tx| tx.execute("INSERT INTO payload VALUES (8)", []))
        .unwrap();
    let next = resumed.capture().unwrap();
    assert_eq!(next.position.txid, position.txid + 1);
    assert_eq!(resumed_payload(&mut resumed), 2);
    resumed.close().unwrap();
}

#[cfg(feature = "replica")]
#[test]
fn a_resume_refuses_a_continuation_that_does_not_match_the_file() {
    let temp = tempfile::TempDir::new().unwrap();
    let limits = Limits::default();
    let source = temp.path().join("mismatched.sqlite");
    let db = committed_database(&source, limits);
    db.persist_continuation().unwrap();
    db.close().unwrap();

    let host = crate::Host::default();
    let corrupt = temp.path().join("corrupt.sqlite");
    crate::resume::move_resumed(&source, &corrupt, &host).unwrap();
    let continuation = crate::resume::continuation_path(&corrupt);
    let mut bytes = std::fs::read(&continuation).unwrap();
    let recorded: u32 = bytes[32..36].try_into().map(u32::from_be_bytes).unwrap();
    bytes[32..36].copy_from_slice(&(recorded + 1).to_be_bytes());
    std::fs::write(&continuation, bytes).unwrap();
    assert!(Db::open_resumed_with_host(&corrupt, limits, host.clone()).is_err());

    // A file that no longer holds the recorded image is refused the same way.
    let short = temp.path().join("short.sqlite");
    crate::resume::discard_resumed(&corrupt, &host).unwrap();
    let source = temp.path().join("short-source.sqlite");
    let db = committed_database(&source, limits);
    db.persist_continuation().unwrap();
    db.close().unwrap();
    crate::resume::move_resumed(&source, &short, &host).unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&short)
        .unwrap();
    file.set_len(file.metadata().unwrap().len() - 4096).unwrap();
    drop(file);
    assert!(Db::open_resumed_with_host(&short, limits, host).is_err());
}

#[cfg(feature = "replica")]
#[test]
fn a_resume_refuses_a_database_that_is_not_checkpointed() {
    let temp = tempfile::TempDir::new().unwrap();
    let limits = Limits::default();
    let source = temp.path().join("dirty.sqlite");
    let db = committed_database(&source, limits);
    db.persist_continuation().unwrap();
    db.close().unwrap();

    // A database whose WAL still holds frames may sit behind the continuation,
    // so a resumed open must fall back to the authoritative root instead.
    std::fs::write(crate::resume::wal_path(&source), [0u8; 32]).unwrap();
    let host = crate::Host::default();
    let destination = temp.path().join("refused.sqlite");
    assert!(crate::resume::move_resumed(&source, &destination, &host).is_err());
    assert!(!destination.exists());
}

#[cfg(feature = "replica")]
#[test]
fn a_dense_checksum_copy_refuses_a_base_that_no_longer_folds_to_it() {
    let temp = tempfile::TempDir::new().unwrap();
    let limits = Limits::default();
    let source = temp.path().join("folding.sqlite");
    let db = committed_database(&source, limits);
    db.persist_continuation().unwrap();
    db.close().unwrap();

    let host = crate::Host::default();
    let destination = temp.path().join("reopened.sqlite");
    crate::resume::move_resumed(&source, &destination, &host).unwrap();
    let resumed = Db::open_resumed_with_host(&destination, limits, host).unwrap();
    let checksums = crate::resume::checksum_path(&destination);
    let mut bytes = std::fs::read(&checksums).unwrap();
    let wrong = u64::from_be_bytes(bytes[0..8].try_into().unwrap()) ^ 1;
    bytes[0..8].copy_from_slice(&wrong.to_be_bytes());
    std::fs::write(&checksums, bytes).unwrap();
    assert!(resumed.persist_continuation().is_err());
}
