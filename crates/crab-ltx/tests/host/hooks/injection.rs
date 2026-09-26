//! Injected host hooks, plans, and VFS selection.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn directory_cache_fill_releases_origin_admission_before_local_sync() {
    let backend = Arc::new(InMemory::new());
    verify_cache_fill_admission(|| Store::new(backend.clone()), "cache-admission").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the RustFS environment documented in examples/README.md"]
async fn rustfs_directory_cache_fill_releases_origin_admission_before_local_sync() {
    let bucket = std::env::var("CRAB_LTX_TEST_BUCKET").unwrap();
    let endpoint = std::env::var("CRAB_LTX_TEST_ENDPOINT").unwrap();
    let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").unwrap();
    let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap();
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prefix = format!(
        "crab-ltx-tests/cache-admission/{run}-{}",
        std::process::id()
    );
    let store = || {
        crab_storage::build_explicit_store(
            &bucket,
            crab_storage::ObjectStoreCredentials::Aws {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: None,
                region: "us-east-1".into(),
            },
            Some(&endpoint),
            endpoint.starts_with("http://"),
        )
        .unwrap()
    };
    verify_cache_fill_admission(store, &prefix).await;
    eprintln!("RustFS cache admission and exact SQLite restore passed: {prefix}");
}

async fn verify_cache_fill_admission(store: impl Fn() -> Store, prefix: &str) {
    let (directory, faults, host, mut writer) = fixture();
    let replica = |cell| {
        CellReplica::new(
            CellStorageLayout::new(store(), ObjectPath::from(prefix), [231; 16]),
            [cell; 32],
            [233; 16],
            Limits::default(),
        )
        .unwrap()
    };
    let captured = writer.capture().unwrap();
    let root = replica(232)
        .prepare(None, &captured, 1, 1)
        .await
        .unwrap()
        .root();
    let other_root = replica(234)
        .prepare(None, &captured, 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    let io = Arc::new(tokio::sync::Semaphore::new(1));
    let jobs = Arc::new(tokio::sync::Semaphore::new(1));
    let reader = replica(232).with_host(
        host.with_io_slots(io.clone())
            .with_job_slots(jobs.clone())
            .with_directory_cache(directory.path().join("cache"))
            .await
            .unwrap(),
    );
    let pause = Arc::new(Pause {
        operation: "sync_all",
        entered: tokio::sync::Notify::new(),
        released: Mutex::new(false),
        wake: std::sync::Condvar::new(),
    });
    *faults.pause.lock().unwrap() = Some(pause.clone());
    let release = Release(pause.clone());
    let read = tokio::spawn(async move { reader.open_root(&root).await });
    tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
        .await
        .unwrap();
    assert!(!read.is_finished());

    // Another Cell with a fresh store identity bypasses the process byte cache.
    // It shares origin admission, but does not use the paused disk cache.
    let other = replica(234).with_host(Host::default().with_io_slots(io.clone()));
    let verified = tokio::time::timeout(Duration::from_secs(1), other.open_root(&other_root)).await;
    read.abort();
    assert!(matches!(read.await, Err(error) if error.is_cancelled()));
    assert_eq!(jobs.available_permits(), 0);
    drop(release);
    let finished = tokio::time::timeout(Duration::from_secs(5), jobs.acquire())
        .await
        .unwrap()
        .unwrap();
    drop(finished);
    let verified = verified
        .expect("cache persistence must not hold origin admission")
        .unwrap();
    let destination = directory.path().join("restored.sqlite");
    verified.restore(&destination).await.unwrap();
    let db = crab_ltx::rusqlite::Connection::open(destination).unwrap();
    let count: u64 = db
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

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
