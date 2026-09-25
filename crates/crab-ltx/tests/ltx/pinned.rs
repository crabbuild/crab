//! A reader that pins the WAL must not stall capture, fence the session, or
//! leave a chain that cannot be restored exactly.

use crab_ltx::rusqlite::Connection;
use crab_ltx::{CheckpointMode, Db, Limits, VerifiedPlan, restore_exact};

fn wal_bytes(database: &std::path::Path) -> u64 {
    let mut path = database.as_os_str().to_owned();
    path.push("-wal");
    std::fs::metadata(std::path::Path::new(&path))
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

#[test]
fn a_pinned_wal_reader_keeps_capture_progress_and_restores_exactly() {
    let temp = tempfile::TempDir::new().unwrap();
    let database = temp.path().join("pinned.sqlite");
    let limits = Limits::default();
    let mut db = Db::open(&database, limits).unwrap();
    db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v)"))
        .unwrap();
    let mut segments = db.capture().unwrap().segments;
    let pinned_wal = wal_bytes(&database);

    // A second connection holds a read mark, so SQLite cannot backfill the WAL
    // past it and every checkpoint stays busy.
    let reader = Connection::open(&database).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM t;")
        .unwrap();

    for _ in 0..64 {
        db.transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(randomblob(4096))"))
            .unwrap();
        segments.extend(db.capture().unwrap().segments);
    }
    assert!(
        !db.has_pending_capture(),
        "capture must keep sealing cuts while a reader pins the WAL"
    );
    assert!(
        wal_bytes(&database) > pinned_wal,
        "a pinned checkpoint is expected to leave the WAL growing"
    );

    // A passive checkpoint reports its busy outcome instead of failing, so the
    // session stays usable and the caller keeps every cut.
    let batch = db.checkpoint(CheckpointMode::Passive).unwrap();
    segments.extend(batch.segments);
    assert_eq!(batch.position, db.position());

    // Releasing the reader lets a truncate checkpoint backfill the whole WAL.
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    let batch = db.checkpoint(CheckpointMode::Truncate).unwrap();
    segments.extend(batch.segments);
    let position = batch.position;
    assert_eq!(position, db.position());
    db.close().unwrap();

    // The complete chain still restores the exact database.
    let plan = VerifiedPlan::new(&segments, position, limits).unwrap();
    let restored = temp.path().join("restored.sqlite");
    assert_eq!(restore_exact(&plan, &restored).unwrap(), position);
    let connection = Connection::open(&restored).unwrap();
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 64);
}

#[test]
fn checkpoint_returns_every_sealed_cut_without_reopening_it_during_collection() {
    let temp = tempfile::TempDir::new().unwrap();
    let database = temp.path().join("checkpoint.sqlite");
    let limits = Limits::default();
    let mut db = Db::open(&database, limits).unwrap();
    db.transaction(|tx| tx.execute_batch("CREATE TABLE t(v BLOB)"))
        .unwrap();
    let first = db.capture().unwrap();
    db.transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(randomblob(4096))"))
        .unwrap();

    let batch = db.checkpoint(CheckpointMode::Truncate).unwrap();
    assert_eq!(batch.segments.len(), 2);
    assert_eq!(batch.timing.verification_nanos, 0);
    let mut segments = first.segments;
    segments.extend(batch.segments);
    let plan = VerifiedPlan::new(&segments, batch.position, limits).unwrap();
    db.close().unwrap();

    let restored = temp.path().join("checkpoint-restored.sqlite");
    restore_exact(&plan, &restored).unwrap();
    let connection = Connection::open(&restored).unwrap();
    let length: i64 = connection
        .query_row("SELECT length(v) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(length, 4096);
}
