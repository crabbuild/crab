use crab_ltx::rusqlite::Connection;
use crab_ltx::{
    CrabError, Limits, LocalSegment, ManagedDb, VerifiedLocalPlan, compact_exact, restore_exact,
};
use tempfile::TempDir;

fn insert(db: &mut ManagedDb, value: &str) {
    db.transaction(|tx| {
        tx.execute(
            "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
            [],
        )?;
        tx.execute("INSERT INTO messages(body) VALUES (?1)", [value])?;
        Ok(())
    })
    .unwrap();
}

fn rows(path: &std::path::Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    conn.prepare("SELECT body FROM messages ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn committed_sql_survives_cold_restore_and_compaction() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();
    let restore = TempDir::new().unwrap();
    let limits = Limits::default();
    let mut db = ManagedDb::open(&source.path().join("repo.sqlite"), limits).unwrap();
    let mut files = Vec::new();
    let mut position = Default::default();
    for text in ["initial", "second", "Unicode: 🦀 中文"] {
        insert(&mut db, text);
        let batch = db.capture().unwrap();
        assert!(
            batch
                .segments
                .iter()
                .all(|file| file.info().post_checksum != 0)
        );
        position = batch.position;
        for file in batch.segments {
            let path = remote.path().join(file.path().file_name().unwrap());
            std::fs::copy(file.path(), &path).unwrap();
            files.push(LocalSegment::new(path, file.info().clone()));
        }
    }
    db.close().unwrap();
    source.close().unwrap();
    let plan = VerifiedLocalPlan::new(&files, position, limits).unwrap();
    let direct = restore.path().join("direct.sqlite");
    restore_exact(&plan, &direct).unwrap();
    let compacted = compact_exact(&plan, &remote.path().join("snapshot.ltx")).unwrap();
    let compact_plan = VerifiedLocalPlan::new(&[compacted], position, limits).unwrap();
    let compact_db = restore.path().join("compact.sqlite");
    restore_exact(&compact_plan, &compact_db).unwrap();
    assert_eq!(
        std::fs::read(&direct).unwrap(),
        std::fs::read(&compact_db).unwrap()
    );
    assert_eq!(rows(&direct), ["initial", "second", "Unicode: 🦀 中文"]);
}

#[test]
fn snapshot_matches_delta_restore_byte_for_byte() {
    let temp = TempDir::new().unwrap();
    let mut db = ManagedDb::open(&temp.path().join("repo.sqlite"), Limits::default()).unwrap();
    insert(&mut db, "one");
    let mut batch = db.capture().unwrap();
    insert(&mut db, "two");
    let next = db.capture().unwrap();
    batch.segments.extend(next.segments);
    let (snapshot, _) = db.snapshot(&temp.path().join("snapshot.ltx")).unwrap();
    assert_eq!(snapshot.info().position(), next.position);
    let plan = VerifiedLocalPlan::new(&batch.segments, next.position, Limits::default()).unwrap();
    let snapshot_plan =
        VerifiedLocalPlan::new(&[snapshot], next.position, Limits::default()).unwrap();
    let a = temp.path().join("a.sqlite");
    let b = temp.path().join("b.sqlite");
    restore_exact(&plan, &a).unwrap();
    restore_exact(&snapshot_plan, &b).unwrap();
    assert_eq!(std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
}

#[test]
fn rolled_back_sql_is_not_restored() {
    let temp = TempDir::new().unwrap();
    let mut db = ManagedDb::open(&temp.path().join("repo.sqlite"), Limits::default()).unwrap();
    insert(&mut db, "keep");
    let result = db.transaction(|tx| {
        tx.execute("INSERT INTO messages(body) VALUES ('rollback')", [])?;
        Err::<(), _>(crab_ltx::rusqlite::Error::InvalidQuery)
    });
    assert!(result.is_err());
    let batch = db.capture().unwrap();
    let plan = VerifiedLocalPlan::new(&batch.segments, batch.position, Limits::default()).unwrap();
    let path = temp.path().join("restored.sqlite");
    restore_exact(&plan, &path).unwrap();
    assert_eq!(rows(&path), ["keep"]);
}

#[test]
fn checkpoint_threshold_captures_every_cut_and_growth_page() {
    let temp = TempDir::new().unwrap();
    let mut db = ManagedDb::open(&temp.path().join("repo.sqlite"), Limits::default()).unwrap();
    let mut segments = Vec::new();
    let mut target = Default::default();
    for round in 0..4 {
        db.transaction(|tx| {
            tx.execute(
                "CREATE TABLE IF NOT EXISTS blobs (id INTEGER PRIMARY KEY, payload BLOB)",
                [],
            )?;
            for _ in 0..400 {
                tx.execute("INSERT INTO blobs(payload) VALUES (zeroblob(12000))", [])?;
            }
            if round % 2 == 1 {
                tx.execute("DELETE FROM blobs WHERE id % 2 = 0", [])?;
            }
            Ok(())
        })
        .unwrap();
        let batch = db.capture().unwrap();
        segments.extend(batch.segments);
        target = batch.position;
    }
    let plan = VerifiedLocalPlan::new(&segments, target, Limits::default()).unwrap();
    let path = temp.path().join("restored.sqlite");
    restore_exact(&plan, &path).unwrap();
    let conn = Connection::open(path).unwrap();
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM blobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 800);
}

#[test]
fn exact_plan_rejects_gap_overlap_wrong_target_and_manifest_mutation() {
    let temp = TempDir::new().unwrap();
    let limits = Limits::default();
    let mut db = ManagedDb::open(&temp.path().join("repo.sqlite"), limits).unwrap();
    let mut segments = Vec::new();
    let mut target = Default::default();
    for value in ["a", "b", "c"] {
        insert(&mut db, value);
        let batch = db.capture().unwrap();
        segments.extend(batch.segments);
        target = batch.position;
    }
    let mut gap = segments.clone();
    gap.remove(1);
    assert!(VerifiedLocalPlan::new(&gap, target, limits).is_err());
    let mut overlap = segments.clone();
    overlap.insert(1, segments[0].clone());
    assert!(VerifiedLocalPlan::new(&overlap, target, limits).is_err());
    let mut wrong_target = target;
    wrong_target.txid += 1;
    assert!(VerifiedLocalPlan::new(&segments, wrong_target, limits).is_err());
    let mut info = segments[0].info().clone();
    info.post_checksum ^= 1;
    let mut wrong_info = segments.clone();
    wrong_info[0] = LocalSegment::new(segments[0].path().to_owned(), info);
    assert!(VerifiedLocalPlan::new(&wrong_info, target, limits).is_err());
    let mut corrupted = std::fs::read(segments[0].path()).unwrap();
    corrupted[120] ^= 1;
    std::fs::write(segments[0].path(), corrupted).unwrap();
    assert!(VerifiedLocalPlan::new(&segments, target, limits).is_err());
}

#[test]
fn verified_plan_owns_bytes_and_never_overwrites_destination() {
    let temp = TempDir::new().unwrap();
    let mut db = ManagedDb::open(&temp.path().join("repo.sqlite"), Limits::default()).unwrap();
    insert(&mut db, "retained");
    let batch = db.capture().unwrap();
    let plan = VerifiedLocalPlan::new(&batch.segments, batch.position, Limits::default()).unwrap();
    for file in batch.segments {
        std::fs::remove_file(file.path()).unwrap();
    }
    let destination = temp.path().join("restore.sqlite");
    restore_exact(&plan, &destination).unwrap();
    assert!(restore_exact(&plan, &destination).is_err());
    assert_eq!(rows(&destination), ["retained"]);
}

#[test]
fn capture_failure_fences_writer_and_stale_sessions_are_refused() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("repo.sqlite");
    let mut db = ManagedDb::open(
        &path,
        Limits {
            max_capture_bytes: 32768,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(ManagedDb::open(&path, Limits::default()).is_err());
    db.transaction(|tx| {
        tx.execute("CREATE TABLE large (body BLOB)", [])?;
        tx.execute("INSERT INTO large VALUES (randomblob(65536))", [])?;
        Ok(())
    })
    .unwrap();
    assert!(db.capture().is_err());
    assert!(matches!(db.transaction(|_| Ok(())), Err(CrabError::Fenced)));
    db.close().unwrap();
    assert!(ManagedDb::open(&path, Limits::default()).is_err());
}

#[test]
fn restore_admission_rejects_database_and_chain_byte_limits() {
    let temp = TempDir::new().unwrap();
    let mut db = ManagedDb::open(&temp.path().join("repo.sqlite"), Limits::default()).unwrap();
    insert(&mut db, "bounded");
    let batch = db.capture().unwrap();
    let small = Limits {
        max_database_bytes: 512,
        ..Limits::default()
    };
    assert!(VerifiedLocalPlan::new(&batch.segments, batch.position, small).is_err());
    let small = Limits {
        max_capture_bytes: 128,
        max_file_bytes: 128,
        max_plan_bytes: 128,
        ..Limits::default()
    };
    assert!(VerifiedLocalPlan::new(&batch.segments, batch.position, small).is_err());
}

#[test]
fn corrupt_committed_wal_cannot_acknowledge_an_earlier_valid_cut() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("repo.sqlite");
    let mut db = ManagedDb::open(&path, Limits::default()).unwrap();
    insert(&mut db, "already captured");
    db.capture().unwrap();
    insert(&mut db, "valid but not captured yet");
    insert(&mut db, "damaged commit");
    let wal_path = temp.path().join("repo.sqlite-wal");
    let mut wal = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(wal_path)
        .unwrap();
    let len = wal.metadata().unwrap().len();
    wal.seek(SeekFrom::Start(len - 1)).unwrap();
    let mut byte = [0];
    wal.read_exact(&mut byte).unwrap();
    byte[0] ^= 1;
    wal.seek(SeekFrom::Start(len - 1)).unwrap();
    wal.write_all(&byte).unwrap();
    wal.sync_all().unwrap();
    assert!(db.capture().is_err());
    assert!(matches!(db.transaction(|_| Ok(())), Err(CrabError::Fenced)));
}

#[test]
fn recovered_database_starts_a_new_epoch_and_can_capture_again() {
    let temp = TempDir::new().unwrap();
    let mut db = ManagedDb::open(&temp.path().join("old.sqlite"), Limits::default()).unwrap();
    insert(&mut db, "before takeover");
    let batch = db.capture().unwrap();
    let plan = VerifiedLocalPlan::new(&batch.segments, batch.position, Limits::default()).unwrap();
    db.close().unwrap();
    let path = temp.path().join("new.sqlite");
    restore_exact(&plan, &path).unwrap();
    let mut new = ManagedDb::open(&path, Limits::default()).unwrap();
    insert(&mut new, "after takeover");
    let batch = new.capture().unwrap();
    assert_eq!(batch.segments[0].info().min_txid, 1);
    let plan = VerifiedLocalPlan::new(&batch.segments, batch.position, Limits::default()).unwrap();
    let path = temp.path().join("restored.sqlite");
    restore_exact(&plan, &path).unwrap();
    assert_eq!(rows(&path), ["before takeover", "after takeover"]);
}

#[test]
fn sqlite_sidecars_prevent_restore() {
    let temp = TempDir::new().unwrap();
    let mut db = ManagedDb::open(&temp.path().join("repo.sqlite"), Limits::default()).unwrap();
    insert(&mut db, "safe");
    let batch = db.capture().unwrap();
    let plan = VerifiedLocalPlan::new(&batch.segments, batch.position, Limits::default()).unwrap();
    let path = temp.path().join("restored.sqlite");
    std::fs::write(temp.path().join("restored.sqlite-wal"), b"stale").unwrap();
    assert!(restore_exact(&plan, &path).is_err());
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn database_symlink_cannot_claim_a_second_capture_session() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("repo.sqlite");
    let db = ManagedDb::open(&path, Limits::default()).unwrap();
    let alias = temp.path().join("alias.sqlite");
    std::os::unix::fs::symlink(db.path(), &alias).unwrap();
    assert!(ManagedDb::open(&alias, Limits::default()).is_err());
}
