//! Injected host hooks, plans, and VFS selection.

use super::*;

#[test]
fn committed_wal_observation_uses_the_injected_reader() {
    let (_directory, faults, _host, mut writer) = fixture();
    faults.arm(Some("read_exact_at"));
    injected(writer.transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)")));
    faults.arm(None);
    assert!(matches!(writer.capture(), Err(CrabError::Fenced)));
}
#[test]
fn fresh_session_claims_do_not_bypass_the_host() {
    for operation in ["canonicalize", "file_len", "create_dir"] {
        let directory = tempfile::TempDir::new().unwrap();
        let faults = Arc::new(Faults::default());
        faults.arm(Some(operation));
        injected(Db::open_with_host(
            &directory.path().join("db"),
            Limits::default(),
            Host::default().with_filesystem(faults),
        ));
        assert!(!directory.path().join("db").exists(), "{operation}");
    }
}
#[test]
fn exact_local_resume_and_all_checkpoints_preserve_the_injected_plan() {
    let (directory, faults, host, mut writer) = fixture();
    let first = writer.capture().unwrap();
    faults.arm(Some("open"));
    injected(host.verify(&first.segments, first.position, Limits::default()));
    faults.arm(None);
    let plan = host
        .verify(&first.segments, first.position, Limits::default())
        .unwrap();
    writer.close().unwrap();
    let destination = directory.path().join("resumed");
    for operation in ["exists", "persist_new"] {
        faults.arm(Some(operation));
        injected(Db::resume_with_host(
            &plan,
            &destination,
            Limits::default(),
            host.clone(),
        ));
        assert!(!destination.exists(), "{operation}");
    }
    faults.arm(None);
    let mut writer =
        Db::resume_with_host(&plan, &destination, Limits::default(), host.clone()).unwrap();
    assert_eq!(writer.position(), first.position);
    let mut segments = first.segments;
    for (index, mode) in [
        CheckpointMode::Passive,
        CheckpointMode::Full,
        CheckpointMode::Restart,
        CheckpointMode::Truncate,
    ]
    .into_iter()
    .enumerate()
    {
        writer
            .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(1)"))
            .unwrap();
        let batch = writer.checkpoint(mode).unwrap();
        assert_eq!(batch.timing.checkpoint_runs, 1);
        assert!(batch.timing.checkpoint_frames >= batch.timing.checkpoint_backfilled);
        assert_eq!(batch.timing.checkpoint_busy_errors, 0);
        segments.extend(batch.segments);
        let plan = host
            .verify(&segments, batch.position, Limits::default())
            .unwrap();
        let restored = directory.path().join(format!("check-{index}"));
        host.restore(&plan, &restored).unwrap();
        let db = crab_ltx::rusqlite::Connection::open(restored).unwrap();
        let count: usize = db
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, index + 2);
    }
    let calls = faults.calls.lock().unwrap();
    for operation in [
        "canonicalize",
        "create_dir",
        "read_exact_at",
        "exists",
        "persist_new",
    ] {
        assert!(calls.contains(operation), "{operation}");
    }
}
#[test]
fn snapshot_and_compaction_installation_are_injectable_and_never_clobber() {
    let (directory, faults, host, mut writer) = fixture();
    let batch = writer.capture().unwrap();
    let plan = host
        .verify(&batch.segments, batch.position, Limits::default())
        .unwrap();
    let destination = directory.path().join("snapshot.ltx");
    for operation in [
        "create",
        "write_all",
        "sync_all",
        "open",
        "persist_file_new",
    ] {
        faults.arm(Some(operation));
        injected(host.compact(&plan, &destination));
        assert!(!destination.exists(), "{operation}");
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp-crab-ltx-compaction-")),
            "{operation} left compaction scratch"
        );
    }
    faults.arm(Some("persist_file_new"));
    injected(writer.snapshot(&destination));
    assert!(!destination.exists());
    faults.arm(None);
    faults.track_all.store(true, Ordering::Relaxed);
    faults.largest_write.store(0, Ordering::Relaxed);
    host.compact(&plan, &destination).unwrap();
    assert!(faults.largest_write.load(Ordering::Relaxed) < 128 * 1024);
    assert!(!faults.calls.lock().unwrap().contains("persist_new"));
    let before = std::fs::read(&destination).unwrap();
    assert!(host.compact(&plan, &destination).is_err());
    assert_eq!(std::fs::read(destination).unwrap(), before);
}
#[test]
fn unknown_sqlite_vfs_does_not_fall_back_to_the_platform_vfs() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("db");
    assert!(
        Db::open_with_host(
            &path,
            Limits::default(),
            Host::default().with_sqlite_vfs("absent-test-vfs")
        )
        .is_err()
    );
    assert!(!path.exists());
}
