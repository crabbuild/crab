//! Compaction transfer overlap, local write coalescing, and scratch cleanup.

use super::*;

#[cfg(feature = "replica")]
#[tokio::test(start_paused = true)]
async fn compaction_overlaps_independent_remote_transfers() {
    let (_directory, _faults, _host, mut writer) = fixture();
    let mut captured = writer.capture().unwrap();
    for value in [2, 3, 4] {
        writer
            .transaction(|tx| tx.execute("INSERT INTO t VALUES(?1)", [value]))
            .unwrap();
        let next = writer.capture().unwrap();
        captured.segments.extend(next.segments);
        captured.position = next.position;
    }
    assert_eq!(captured.segments.len(), 4);
    let delay = Duration::from_millis(100);
    let backend = InMemory::new();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(ThrottledStore::new(
                backend,
                ThrottleConfig {
                    wait_get_per_call: delay,
                    wait_put_per_call: delay,
                    ..ThrottleConfig::default()
                },
            ))),
            ObjectPath::from("parallel-compaction-downloads"),
            [84; 16],
        ),
        [85; 32],
        [86; 16],
        Limits::default(),
    )
    .unwrap();
    let root = replica.prepare(None, &captured, 1, 1).await.unwrap().root();
    let scratch = tempfile::TempDir::new().unwrap();
    let started = tokio::time::Instant::now();

    replica
        .prepare_compaction(&root, 0..4, 9, scratch.path())
        .await
        .unwrap();

    // Cached root metadata is presence-checked in one parallel HEAD wave.
    // Streaming directory construction still precedes the final root upload.
    assert_eq!(started.elapsed(), delay * 5);
    writer.close().unwrap();
}
#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_compaction_coalesces_local_output_writes() {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let mut writer = Db::open_with_host(
        &directory.path().join("source.sqlite"),
        Limits::default(),
        host.clone(),
    )
    .unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(3000000))",
            )
        })
        .unwrap();
    let captured = writer.capture().unwrap();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("buffered-compaction-output"),
            [66; 16],
        ),
        [67; 32],
        [68; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica.prepare(None, &captured, 1, 1).await.unwrap().root();
    faults.track_all.store(true, Ordering::Relaxed);
    faults.read_calls.store(0, Ordering::Relaxed);
    faults.write_calls.store(0, Ordering::Relaxed);
    let scratch = tempfile::TempDir::new().unwrap();

    let compacted = replica
        .prepare_compaction(&root, 0..1, 9, scratch.path())
        .await
        .unwrap();

    assert_eq!(compacted.root().position, root.position);
    assert!(
        faults.write_calls.load(Ordering::Relaxed) <= 1_000,
        "compaction used {} local writes",
        faults.write_calls.load(Ordering::Relaxed)
    );
    assert!(
        faults.read_calls.load(Ordering::Relaxed) < 128,
        "compaction used {} local reads",
        faults.read_calls.load(Ordering::Relaxed)
    );
    writer.close().unwrap();
}

#[tokio::test]
async fn cell_compaction_dispatches_local_io_with_one_job_slot() {
    let (directory, faults, host, mut writer) = fixture();
    let jobs = Arc::new(tokio::sync::Semaphore::new(1));
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("compaction-job-admission"),
            [91; 16],
        ),
        [92; 32],
        [93; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host.with_job_slots(jobs.clone()));
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    *faults.forbidden_thread.lock().unwrap() = Some(std::thread::current().id());

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        replica.prepare_compaction(&root, 0..1, 9, directory.path()),
    )
    .await;

    *faults.forbidden_thread.lock().unwrap() = None;
    let compacted = result.unwrap().unwrap();
    assert_eq!(compacted.root().position, root.position);
    assert_eq!(jobs.available_permits(), 1);
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".crab-compaction-")
    }));
}
#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_compaction_uses_injected_filesystem_and_cleans_failed_scratch() {
    let (directory, faults, host, mut writer) = fixture();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-compaction"),
            [11; 16],
        ),
        [12; 32],
        [13; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();

    for operation in [
        "create",
        "open_rw",
        "read_exact_at",
        "write_all_at",
        "write_all",
        "sync_all",
    ] {
        faults.arm(Some(operation));
        injected(
            replica
                .prepare_compaction(&root, 0..1, 9, directory.path())
                .await,
        );
        assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".crab-compaction-")
        }));
    }

    faults.arm(None);
    let compacted = replica
        .prepare_compaction(&root, 0..1, 9, directory.path())
        .await
        .unwrap();
    assert_eq!(compacted.root().position, root.position);
    assert!(faults.calls.lock().unwrap().contains("open_rw"));
}

#[tokio::test]
async fn canceled_compaction_retains_files_and_admission_through_cleanup() {
    for operation in [
        "create",
        "open_rw",
        "read_exact_at",
        "write_all_at",
        "write_all",
        "sync_all",
    ] {
        let (directory, faults, host, mut writer) = fixture();
        let jobs = Arc::new(tokio::sync::Semaphore::new(1));
        let dirty = Arc::new(tokio::sync::Semaphore::new(1));
        let recovery = Arc::new(tokio::sync::Semaphore::new(1));
        let scratch = Arc::new(tokio::sync::Semaphore::new(128));
        let replica = CellReplica::new(
            CellStorageLayout::new(
                Store::new(Arc::new(InMemory::new())),
                ObjectPath::from("canceled-compaction"),
                [94; 16],
            ),
            [95; 32],
            [96; 16],
            Limits::default(),
        )
        .unwrap()
        .with_host(
            host.with_job_slots(jobs.clone())
                .with_dirty_slots(dirty.clone())
                .with_recovery_slots(recovery.clone())
                .with_scratch_slots(scratch.clone()),
        );
        let root = replica
            .prepare(None, &writer.capture().unwrap(), 1, 1)
            .await
            .unwrap()
            .root();
        writer.close().unwrap();
        let pause = Arc::new(Pause {
            operation,
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        let release = Release(pause.clone());
        *faults.pause.lock().unwrap() = Some(pause.clone());
        *faults.forbidden_thread.lock().unwrap() = Some(std::thread::current().id());
        let task_replica = replica.clone();
        let destination = directory.path().to_owned();
        let task = tokio::spawn(async move {
            task_replica
                .prepare_compaction(&root, 0..1, 9, &destination)
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), pause.entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert_eq!(jobs.available_permits(), 0, "{operation}");
        assert_eq!(dirty.available_permits(), 0, "{operation}");
        assert_eq!(recovery.available_permits(), 0, "{operation}");
        assert!(scratch.available_permits() < 128, "{operation}");

        let cleanup_pause = Arc::new(Pause {
            operation: "remove_file",
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        let cleanup_release = Release(cleanup_pause.clone());
        *faults.pause.lock().unwrap() = Some(cleanup_pause.clone());
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), cleanup_pause.entered.notified())
            .await
            .unwrap();
        assert_eq!(jobs.available_permits(), 0, "cleanup after {operation}");
        assert_eq!(dirty.available_permits(), 0, "cleanup after {operation}");
        assert_eq!(recovery.available_permits(), 0, "cleanup after {operation}");
        assert!(
            scratch.available_permits() < 128,
            "cleanup after {operation}"
        );
        *faults.pause.lock().unwrap() = None;
        drop(cleanup_release);
        tokio::time::timeout(Duration::from_secs(2), async {
            let _job = jobs.acquire().await.unwrap();
            let _dirty = dirty.acquire().await.unwrap();
            let _recovery = recovery.acquire().await.unwrap();
            let _scratch = scratch.acquire_many(128).await.unwrap();
        })
        .await
        .unwrap();
        *faults.forbidden_thread.lock().unwrap() = None;
        assert!(
            !std::fs::read_dir(directory.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".crab-compaction-")
            }),
            "{operation}"
        );
        let compacted = replica
            .prepare_compaction(&root, 0..1, 9, directory.path())
            .await
            .unwrap();
        assert_eq!(compacted.root().position, root.position);
    }
}
