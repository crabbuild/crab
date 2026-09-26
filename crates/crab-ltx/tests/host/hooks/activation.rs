use super::*;

async fn prepared_root(host: Host, writer: &mut Db, store: Store) -> crab_ltx::CellPagedDatabase {
    let replica = CellReplica::new(
        CellStorageLayout::new(store, ObjectPath::from("activation-admission"), [81; 16]),
        [82; 32],
        [83; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let captured = writer.capture().unwrap();
    let root = replica.prepare(None, &captured, 1, 1).await.unwrap().root();
    replica.open_root(&root).await.unwrap().paged()
}

fn checksum_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(".crab-ltx-checksums");
    PathBuf::from(path)
}

#[tokio::test]
async fn writable_activation_dispatches_filesystem_work_with_one_job_slot() {
    let (directory, faults, host, mut writer) = fixture();
    let jobs = Arc::new(tokio::sync::Semaphore::new(1));
    let dirty = Arc::new(tokio::sync::Semaphore::new(1));
    let host = host
        .with_job_slots(jobs.clone())
        .with_dirty_slots(dirty.clone())
        .with_directory_cache(directory.path().join("cache"));
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(zeroblob(40000000))"))
        .unwrap();
    let paged = prepared_root(host, &mut writer, Store::new(Arc::new(InMemory::new()))).await;
    faults.track_all.store(true, Ordering::Relaxed);
    faults.largest_write.store(0, Ordering::Relaxed);
    *faults.forbidden_thread.lock().unwrap() = Some(std::thread::current().id());

    let prepared = tokio::time::timeout(
        Duration::from_secs(5),
        paged.prepare_writable(&directory.path().join("active.sqlite")),
    )
    .await
    .unwrap();

    *faults.forbidden_thread.lock().unwrap() = None;
    assert!(prepared.is_ok(), "{:?}", prepared.err());
    assert_eq!(jobs.available_permits(), 1);
    assert_eq!(dirty.available_permits(), 1);
    assert_eq!(faults.largest_write.load(Ordering::Relaxed), 64 << 10);
    writer.close().unwrap();
}

#[tokio::test]
async fn canceled_activation_retains_admission_until_file_cleanup_finishes() {
    for operation in ["create", "write_all", "sync_all", "sync_parent", "file_len"] {
        let (directory, faults, host, mut writer) = fixture();
        let jobs = Arc::new(tokio::sync::Semaphore::new(1));
        let dirty = Arc::new(tokio::sync::Semaphore::new(1));
        let host = host
            .with_job_slots(jobs.clone())
            .with_dirty_slots(dirty.clone());
        let paged = prepared_root(host, &mut writer, Store::new(Arc::new(InMemory::new()))).await;
        let destination = directory.path().join("active.sqlite");
        let task_destination = destination.clone();
        let task_paged = paged.clone();
        let pause = Arc::new(Pause {
            operation,
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        let release = Release(pause.clone());
        *faults.pause.lock().unwrap() = Some(pause.clone());
        *faults.forbidden_thread.lock().unwrap() = Some(std::thread::current().id());
        let task =
            tokio::spawn(async move { task_paged.prepare_writable(&task_destination).await });
        tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
            .await
            .unwrap();
        // This task runs on the same single-thread runtime as activation. Reaching
        // here while the filesystem is paused proves unrelated async progress.
        task.abort();
        assert!(task.await.err().unwrap().is_cancelled());
        assert_eq!(dirty.available_permits(), 0, "{operation}");
        assert_eq!(jobs.available_permits(), 0, "{operation}");
        let cleanup_pause = Arc::new(Pause {
            operation: "remove_file",
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        let cleanup_release = Release(cleanup_pause.clone());
        *faults.pause.lock().unwrap() = Some(cleanup_pause.clone());
        drop(release);
        tokio::time::timeout(Duration::from_secs(5), cleanup_pause.entered.notified())
            .await
            .unwrap();
        assert_eq!(dirty.available_permits(), 0, "cleanup after {operation}");
        assert_eq!(jobs.available_permits(), 0, "cleanup after {operation}");
        drop(cleanup_release);
        let permit = tokio::time::timeout(Duration::from_secs(5), dirty.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert!(!checksum_path(&destination).exists(), "{operation}");
        assert_eq!(jobs.available_permits(), 1, "{operation}");
        *faults.pause.lock().unwrap() = None;
        *faults.forbidden_thread.lock().unwrap() = None;
        let prepared = paged.prepare_writable(&destination).await.unwrap();
        let mut restored = prepared.open_writable(&destination).unwrap();
        let count: u64 = restored
            .query_with(|db| db.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)))
            .unwrap();
        assert_eq!(count, 1, "{operation}");
        restored.close().unwrap();
        writer.close().unwrap();
    }
}

#[tokio::test]
async fn activation_file_failures_cleanup_before_retry_and_preserve_existing_destinations() {
    let (directory, faults, host, mut writer) = fixture();
    let paged = prepared_root(host, &mut writer, Store::new(Arc::new(InMemory::new()))).await;
    let destination = directory.path().join("active.sqlite");
    for operation in ["create", "write_all", "sync_all", "sync_parent", "file_len"] {
        faults.plan([operation]);
        injected(paged.clone().prepare_writable(&destination).await);
        assert!(!checksum_path(&destination).exists(), "{operation}");
    }
    for existing in [&destination, &checksum_path(&destination)] {
        std::fs::write(existing, b"existing").unwrap();
        assert!(matches!(
            paged.clone().prepare_writable(&destination).await,
            Err(CrabError::InvalidState(_))
        ));
        assert_eq!(std::fs::read(existing).unwrap(), b"existing");
        std::fs::remove_file(existing).unwrap();
    }
    paged.prepare_writable(&destination).await.unwrap();
    writer.close().unwrap();
}

#[tokio::test(start_paused = true)]
async fn cold_activation_overlaps_leaf_reads_within_shared_io_admission() {
    let (directory, _faults, _host, mut writer) = fixture();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(zeroblob(10000000))"))
        .unwrap();
    let backend = InMemory::new();
    let layout =
        |store| CellStorageLayout::new(store, ObjectPath::from("activation-leaf-reads"), [84; 16]);
    let replica = CellReplica::new(
        layout(Store::new(Arc::new(backend.clone()))),
        [85; 32],
        [86; 16],
        Limits::default(),
    )
    .unwrap();
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    let delay = Duration::from_millis(100);
    for slots in [1, 4, 16] {
        let io = Arc::new(tokio::sync::Semaphore::new(slots));
        let replica = CellReplica::new(
            layout(Store::new(Arc::new(ThrottledStore::new(
                backend.clone(),
                ThrottleConfig {
                    wait_get_per_call: delay,
                    ..ThrottleConfig::default()
                },
            )))),
            [85; 32],
            [86; 16],
            Limits::default(),
        )
        .unwrap()
        .with_host(
            Host::default()
                .with_io_slots(io.clone())
                .with_job_slots(Arc::new(tokio::sync::Semaphore::new(1))),
        );
        let verified = replica.open_root(&root).await.unwrap();
        assert_eq!(verified.directory_height(), 1);
        let paged = verified.paged();
        let leaves = paged.page_count().div_ceil(256);
        let destination = directory.path().join(format!("parallel-{slots}.sqlite"));
        let started = tokio::time::Instant::now();
        let prepared = paged.prepare_writable(&destination).await.unwrap();
        let elapsed = started.elapsed();
        // open_root already loaded the authenticated parent. Four shared slots
        // overlap waits, while excess slots cannot exceed the eight-read ceiling.
        assert_eq!(
            elapsed,
            delay * leaves.div_ceil(slots.min(8) as u32),
            "slots={slots}, leaves={leaves}"
        );
        assert_eq!(io.available_permits(), slots);
        let mut restored = prepared.open_writable(&destination).unwrap();
        let count: u64 = restored
            .query_with(|db| db.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)))
            .unwrap();
        assert_eq!(count, 2);
        restored.close().unwrap();
    }
    writer.close().unwrap();
}

struct ActivationExecutor(Arc<Faults>);

impl Executor for ActivationExecutor {
    fn dispatch(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<()> {
        tokio::task::spawn_blocking(job);
        Ok(())
    }

    fn start_worker(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<Box<dyn Worker>> {
        self.0.check("start_worker")?;
        Ok(Box::new(TestWorker(std::thread::spawn(job))))
    }
}

async fn prepared_activation(
    store: Store,
    value: i64,
) -> (
    tempfile::TempDir,
    Arc<Faults>,
    crab_ltx::CellWritableDatabase,
    PathBuf,
) {
    let (directory, faults, host, mut writer) = fixture();
    writer
        .transaction(|tx| tx.execute("UPDATE t SET v = ?1", [value]))
        .unwrap();
    let host = host.with_executor(Arc::new(ActivationExecutor(faults.clone())));
    let paged = prepared_root(host, &mut writer, store).await;
    writer.close().unwrap();
    let destination = directory.path().join("active.sqlite");
    let prepared = paged.prepare_writable(&destination).await.unwrap();
    (directory, faults, prepared, destination)
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_activation_io_does_not_block_other_cells() {
    verify_registry_isolation(Store::new(Arc::new(InMemory::new()))).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the RustFS environment documented in examples/README.md"]
async fn rustfs_sparse_activation_io_does_not_block_other_cells() {
    let endpoint = std::env::var("CRAB_LTX_TEST_ENDPOINT").unwrap();
    let store = crab_storage::build_explicit_store(
        &std::env::var("CRAB_LTX_TEST_BUCKET").unwrap(),
        crab_storage::ObjectStoreCredentials::Aws {
            access_key_id: std::env::var("AWS_ACCESS_KEY_ID").unwrap(),
            secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").unwrap(),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&endpoint),
        endpoint.starts_with("http://"),
    )
    .unwrap();
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prefix = format!(
        "crab-ltx-tests/activation-registry/{run}-{}",
        std::process::id()
    );
    let store = Store::new(Arc::new(object_store::prefix::PrefixStore::new(
        store.inner().clone(),
        ObjectPath::from(prefix.clone()),
    )));
    verify_registry_isolation(store).await;
    eprintln!("RustFS activation registry isolation passed: {prefix}");
}

async fn verify_registry_isolation(store: Store) {
    for operation in ["sync_all", "sync_parent", "start_worker"] {
        let (_slow_directory, faults, slow, slow_path) =
            prepared_activation(store.clone(), 1).await;
        let (_opening_directory, _, opening, opening_path) =
            prepared_activation(store.clone(), 2).await;
        let (_closing_directory, _, closing, closing_path) =
            prepared_activation(store.clone(), 3).await;
        let closing = closing.open_writable(&closing_path).unwrap();
        let pause = Arc::new(Pause {
            operation,
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        let release = Release(pause.clone());
        *faults.pause.lock().unwrap() = Some(pause.clone());
        let slow = tokio::task::spawn_blocking(move || {
            read_and_close(slow.open_writable(&slow_path).unwrap())
        });
        tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
            .await
            .unwrap();
        let mut opening = tokio::task::spawn_blocking(move || {
            read_and_close(opening.open_writable(&opening_path).unwrap())
        });
        let mut closing = tokio::task::spawn_blocking(move || read_and_close(closing));
        let (opened, closed) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(1), &mut opening),
            tokio::time::timeout(Duration::from_secs(1), &mut closing),
        );
        let progressed = (opened.is_ok(), closed.is_ok());
        // Always drain blocked jobs before asserting the isolation property.
        // The regression must fail cleanly against the original global lock.
        drop(release);
        *faults.pause.lock().unwrap() = None;
        assert_eq!(slow.await.unwrap(), 1);
        let count = match opened {
            Ok(result) => result.unwrap(),
            Err(_) => opening.await.unwrap(),
        };
        let closed_value = match closed {
            Ok(result) => result.unwrap(),
            Err(_) => closing.await.unwrap(),
        };
        assert_eq!((count, closed_value), (2, 3));
        assert_eq!(progressed, (true, true), "paused {operation}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_sparse_open_preserves_the_in_progress_path_claim() {
    let (_directory, faults, prepared, destination) =
        prepared_activation(Store::new(Arc::new(InMemory::new())), 1).await;
    let pause = Arc::new(Pause {
        operation: "create",
        entered: tokio::sync::Notify::new(),
        released: Mutex::new(false),
        wake: std::sync::Condvar::new(),
    });
    let release = Release(pause.clone());
    *faults.pause.lock().unwrap() = Some(pause.clone());
    let first = {
        let prepared = prepared.clone();
        let destination = destination.clone();
        tokio::task::spawn_blocking(move || prepared.open_writable(&destination))
    };
    tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
        .await
        .unwrap();
    assert!(!destination.exists());
    // Rejected attempts must neither enter file creation nor remove the first
    // attempt's claim. Exercise another attempt after the first refusal.
    let mut conflicts = {
        let prepared = prepared.clone();
        let destination = destination.clone();
        tokio::task::spawn_blocking(move || {
            for _ in 0..2 {
                assert!(matches!(
                    prepared.clone().open_writable(&destination),
                    Err(CrabError::InvalidState(_))
                ));
            }
        })
    };
    let refused = tokio::time::timeout(Duration::from_secs(1), &mut conflicts).await;
    let prompt = refused.is_ok();
    drop(release);
    *faults.pause.lock().unwrap() = None;
    let db = first.await.unwrap().unwrap();
    match refused {
        Ok(result) => result.unwrap(),
        Err(_) => conflicts.await.unwrap(),
    }
    assert_eq!((prompt, read_and_close(db)), (true, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_sparse_open_releases_its_claim_without_deleting_files() {
    for operation in [
        "create",
        "set_len",
        "sync_all",
        "sync_parent",
        "start_worker",
        "create_dir",
        "existing",
    ] {
        let (_directory, faults, prepared, destination) =
            prepared_activation(Store::new(Arc::new(InMemory::new())), 1).await;
        if operation == "existing" {
            std::fs::write(&destination, b"existing").unwrap();
            assert!(matches!(
                prepared.clone().open_writable(&destination),
                Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists
            ));
            assert_eq!(std::fs::read(&destination).unwrap(), b"existing");
        } else {
            faults.plan([operation]);
            injected(prepared.clone().open_writable(&destination));
        }
        if operation != "create" {
            assert!(
                destination.exists(),
                "failed {operation} must leave its file quarantined"
            );
            assert!(matches!(
                prepared.clone().open_writable(&destination),
                Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists
            ));
            // Only the test's owner discards this failed local artifact. The
            // library must not turn an interrupted sparse file into a fresh one.
            std::fs::remove_file(&destination).unwrap();
        }
        let db = prepared.open_writable(&destination).unwrap();
        assert_eq!(read_and_close(db), 1, "retry after {operation}");
    }
}

fn read_and_close(mut db: Db) -> i64 {
    let value = db
        .query_with(|db| db.query_row("SELECT v FROM t", [], |row| row.get(0)))
        .unwrap();
    db.close().unwrap();
    value
}
