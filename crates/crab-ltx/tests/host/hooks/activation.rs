use super::*;

pub(super) struct Pause {
    operation: &'static str,
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: std::sync::Condvar,
}

impl Pause {
    pub(super) fn wait(&self, operation: &str) {
        if self.operation == operation {
            let mut released = self.released.lock().unwrap();
            self.entered.notify_one();
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
        }
    }
}

struct Release(Arc<Pause>);

impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}

async fn prepared_root(host: Host, writer: &mut Db) -> crab_ltx::CellPagedDatabase {
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("activation-admission"),
            [81; 16],
        ),
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
    let paged = prepared_root(host, &mut writer).await;
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
        let paged = prepared_root(host, &mut writer).await;
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
    let paged = prepared_root(host, &mut writer).await;
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
