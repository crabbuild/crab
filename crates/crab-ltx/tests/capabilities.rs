#![cfg(feature = "replica")]

use crab_ltx::{
    CaptureBatch, CompactionSchedule, DiskBudget, Host, Limits, ManagedDb, Replica,
    bundle::{Bundle, BundleEntry},
};
use crab_storage::{Store, StoreLayout};
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
    memory::InMemory,
    path::Path,
    throttle::{ThrottleConfig, ThrottledStore},
};
use std::{
    fmt,
    sync::Arc,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

#[derive(Debug)]
struct FailFirstPutStore {
    inner: Arc<InMemory>,
    failures: AtomicUsize,
}

impl FailFirstPutStore {
    fn new(inner: Arc<InMemory>) -> Self {
        Self {
            inner,
            failures: AtomicUsize::new(0),
        }
    }

    fn fail_next_put(&self) {
        self.failures.fetch_add(1, Ordering::Release);
    }
}

impl fmt::Display for FailFirstPutStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fail-first-put-store")
    }
}

#[async_trait::async_trait]
impl ObjectStore for FailFirstPutStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if self
            .failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(object_store::Error::PermissionDenied {
                path: location.to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected immutable PUT failure",
                )),
            });
        }
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn immutable_provider_failure_preserves_head_and_retries_exact_source() {
    let (directory, batches) = captures();
    let backend = Arc::new(FailFirstPutStore::new(Arc::new(InMemory::new())));
    let replica = Replica::new(
        StoreLayout::new(Store::new(backend.clone()), "provider-failure".into()),
        "epoch",
        Limits::default(),
    )
    .unwrap();

    backend.fail_next_put();
    assert!(replica.replicate(&batches[0], None).await.is_err());
    assert!(replica.head().await.unwrap().is_none());

    let head = replica.replicate(&batches[0], None).await.unwrap();
    let restored = directory.path().join("provider-failure-restored.sqlite");
    replica.restore(&head, &restored).await.unwrap();
    let database = crab_ltx::rusqlite::Connection::open(&restored).unwrap();
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM t", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_page_materialization_obeys_shared_local_disk_admission() {
    let (directory, batches) = captures();
    let budget = DiskBudget::new(4095);
    let replica = Replica::new(
        StoreLayout::new(Store::new(Arc::new(InMemory::new())), "paged-disk".into()),
        "epoch",
        Limits::default(),
    )
    .unwrap()
    .with_host(Host::default().with_local_disk_budget(budget.clone()));
    let head = replica.replicate(&batches[0], None).await.unwrap();
    let paged = replica.paged(&head).await.unwrap();
    assert_eq!(paged.page_size(), 4096);

    let result = paged.open_writable(&directory.path().join("disk-limited.sqlite"));

    assert!(matches!(
        result,
        Err(crab_ltx::CrabError::Limit("local disk bytes"))
    ));
    assert_eq!(budget.used(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_sqlite_fault_honors_its_callers_deadline() {
    let (_directory, batches) = captures();
    let store = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig::default(),
    ));
    let replica = Replica::new(
        StoreLayout::new(Store::new(store.clone()), "paged-deadline".into()),
        "epoch",
        Limits::default(),
    )
    .unwrap();
    let head = replica.replicate(&batches[0], None).await.unwrap();
    let paged = replica.paged(&head).await.unwrap();
    store.config_mut(|config| config.wait_get_per_call = Duration::from_secs(1));

    let started = Instant::now();
    let error = tokio::task::spawn_blocking(move || {
        crab_ltx::with_paged_io_deadline(Instant::now() + Duration::from_millis(20), || match paged
            .open_sqlite()
        {
            Ok(_) => panic!("delayed page read must exceed the scoped deadline"),
            Err(error) => error,
        })
    })
    .await
    .unwrap();

    assert!(matches!(error, crab_ltx::CrabError::Deadline));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_paged_worker_keeps_provider_runtime_tasks_alive() {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    let (_directory, batches) = captures();
    let armed = Arc::new(AtomicBool::new(false));
    let observed = armed.clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Mutex::new(Some(send));
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |_| {
            if observed.swap(false, Ordering::SeqCst) {
                let send = send.lock().unwrap().take().unwrap();
                // A pooled HTTP connection's driver is also spawned on the runtime
                // executing the initial fetch and must progress after that fetch.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let _ = send.send(());
                });
            }
        }));
    let replica = Replica::new(
        StoreLayout::new(store, "idle-runtime".into()),
        "epoch",
        Limits::default(),
    )
    .unwrap();
    let head = replica.replicate(&batches[0], None).await.unwrap();
    let paged = replica.paged(&head).await.unwrap();
    armed.store(true, Ordering::SeqCst);
    let sql = tokio::task::spawn_blocking(move || paged.open_sqlite().unwrap())
        .await
        .unwrap();
    let progress = tokio::time::timeout(Duration::from_secs(2), receive).await;
    drop(sql);
    assert!(
        progress.is_ok(),
        "provider runtime must progress while SQLite is idle"
    );
}

fn captures() -> (tempfile::TempDir, Vec<CaptureBatch>) {
    let directory = tempfile::TempDir::new().unwrap();
    let mut db = ManagedDb::open(&directory.path().join("db.sqlite"), Limits::default()).unwrap();
    let mut batches = Vec::new();
    for _ in 0..4 {
        db.transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS t(v); INSERT INTO t VALUES(randomblob(20000))",
            )
        })
        .unwrap();
        batches.push(db.capture().unwrap());
    }
    db.close().unwrap();
    (directory, batches)
}

#[test]
fn bundle_envelope_checks_identity_extents_and_all_inner_bytes() {
    let (_directory, batches) = captures();
    let segment = &batches[0].segments[0];
    let entry = |repository: &str| BundleEntry {
        repository: repository.into(),
        epoch: "epoch".into(),
        info: segment.info().clone(),
        bytes: std::fs::read(segment.path()).unwrap(),
    };
    assert!(Bundle::encode(vec![entry("repo"), entry("repo")], Limits::default()).is_err());
    let bundle = Bundle::encode(vec![entry("one"), entry("two")], Limits::default()).unwrap();
    assert_eq!(
        bundle.segment(1).unwrap(),
        std::fs::read(segment.path()).unwrap()
    );
    assert!(bundle.segment(2).is_err());
    let original = bundle.bytes();
    for length in [0, 1, 7, 8, 100, original.len() - 1] {
        assert!(Bundle::decode(original[..length].to_vec(), Limits::default()).is_err());
    }
    for at in [0, 105, original.len() - 5, original.len() - 1] {
        let mut changed = original.to_vec();
        changed[at] ^= 1;
        assert!(Bundle::decode(changed, Limits::default()).is_err());
    }
    let limits = Limits {
        max_segments: 1,
        ..Limits::default()
    };
    assert!(Bundle::decode(original.to_vec(), limits).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn aggregated_bundle_rows_publish_only_to_their_matching_repository_and_epoch() {
    let (_directory, batches) = captures();
    let segment = &batches[0].segments[0];
    let bundle = Bundle::encode(
        ["one", "two"]
            .into_iter()
            .map(|repository| BundleEntry {
                repository: repository.into(),
                epoch: "epoch".into(),
                info: segment.info().clone(),
                bytes: std::fs::read(segment.path()).unwrap(),
            })
            .collect(),
        Limits::default(),
    )
    .unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let destination = tempfile::TempDir::new().unwrap();
    for name in ["one", "two", "wrong"] {
        let replica = Replica::new(
            StoreLayout::new(store.clone(), name.into()),
            "epoch",
            Limits::default(),
        )
        .unwrap();
        let result = replica.replicate_bundle(&bundle, None).await;
        if name == "wrong" {
            assert!(result.is_err());
            assert!(replica.head().await.unwrap().is_none());
            continue;
        }
        let head = result.unwrap();
        replica
            .restore(&head, &destination.path().join(name))
            .await
            .unwrap();
        assert!(
            replica
                .replicate_bundle(&bundle, Some(&head))
                .await
                .is_err(),
            "overlapping captures cannot be republished as new transactions"
        );
    }
    assert_eq!(
        std::fs::read(destination.path().join("one")).unwrap(),
        std::fs::read(destination.path().join("two")).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduled_compaction_is_bounded_exact_and_retains_historical_roots() {
    let (_directory, batches) = captures();
    let replica = Replica::new(
        StoreLayout::new(Store::new(Arc::new(InMemory::new())), "schedule".into()),
        "epoch",
        Limits::default(),
    )
    .unwrap();
    let mut head = replica.replicate(&batches[0], None).await.unwrap();
    for batch in &batches[1..] {
        head = replica.replicate(batch, Some(&head)).await.unwrap();
    }
    let mut schedule = CompactionSchedule::default();
    assert_eq!(schedule.next_due(), Duration::from_secs(30));
    assert!(
        schedule
            .run_due(&replica, &head, Duration::from_secs(29))
            .await
            .unwrap()
            .is_none()
    );
    let compacted = schedule
        .run_due(&replica, &head, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(compacted.segment_count(), 1);
    assert!(
        schedule
            .run_due(&replica, &compacted, Duration::ZERO)
            .await
            .is_err()
    );
    let destination = tempfile::TempDir::new().unwrap();
    for (name, selected) in [("old", &head), ("new", &compacted)] {
        replica
            .restore(selected, &destination.path().join(name))
            .await
            .unwrap();
    }
    assert_eq!(
        std::fs::read(destination.path().join("old")).unwrap(),
        std::fs::read(destination.path().join("new")).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bundle_transport_corruption_does_not_fall_back_to_native_objects() {
    let (_directory, batches) = captures();
    let store = Store::new(Arc::new(InMemory::new()));
    let replica = Replica::new(
        StoreLayout::new(store.clone(), "corrupt-bundle".into()),
        "epoch",
        Limits::default(),
    )
    .unwrap();
    let native = replica.replicate(&batches[0], None).await.unwrap();
    let bundled = replica.bundle(&native).await.unwrap();
    let (head, _) = store
        .get_with_etag(&object_store::path::Path::from(
            "corrupt-bundle/ltx/epoch/head.json",
        ))
        .await
        .unwrap();
    let head: serde_json::Value = serde_json::from_slice(&head).unwrap();
    let hash: [u8; 32] =
        serde_json::from_value(head["segments"][0]["bundle"]["hash"].clone()).unwrap();
    let hash = blake3::Hash::from_bytes(hash).to_hex();
    let key =
        object_store::path::Path::from(format!("corrupt-bundle/ltx/epoch/objects/{hash}.bundle"));
    let (bytes, _) = store.get_with_etag(&key).await.unwrap();
    let mut bytes = bytes.to_vec();
    bytes[105] ^= 1;
    store.put_overwrite(&key, bytes.into()).await.unwrap();
    let destination = tempfile::TempDir::new().unwrap();
    assert!(
        replica
            .restore(&bundled, &destination.path().join("broken"))
            .await
            .is_err()
    );
    assert!(
        replica
            .paged(&bundled)
            .await
            .unwrap()
            .read_page(1)
            .await
            .is_err()
    );
    replica
        .restore(&native, &destination.path().join("native"))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn delayed_sparse_fault_cannot_overwrite_a_checkpointed_page() {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    let directory = tempfile::TempDir::new().unwrap();
    let mut source =
        ManagedDb::open(&directory.path().join("source.sqlite"), Limits::default()).unwrap();
    source
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE payload(v); INSERT INTO payload VALUES(randomblob(1200000))",
            )
        })
        .unwrap();
    let batch = source.capture().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let observe = armed.clone();
    let (started, waiting) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let released = Mutex::new(released);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |_| {
            if observe.swap(false, Ordering::SeqCst) {
                started.send(()).unwrap();
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
        }));
    let replica = Replica::new(
        StoreLayout::new(store, "fault-race".into()),
        "epoch",
        Limits::default(),
    )
    .unwrap();
    let head = replica.replicate(&batch, None).await.unwrap();
    let paged = replica.paged(&head).await.unwrap();
    let page = paged.page_count();
    let size = paged.page_size();
    let mut expected = paged.read_page(page).await.unwrap();
    expected[size as usize - 1] ^= 0x55;
    let path = directory.path().join("sparse.sqlite");
    let writer = tokio::task::spawn_blocking(move || paged.open_writable(&path).unwrap())
        .await
        .unwrap();
    let open = || {
        crab_ltx::rusqlite::Connection::open_with_flags_and_vfs(
            writer.path(),
            crab_ltx::rusqlite::OpenFlags::default(),
            "crab-ltx-writable-v1",
        )
        .unwrap()
    };
    let reader = open();
    let checkpoint = open();
    let offset = u64::from(page - 1) * u64::from(size);
    armed.store(true, Ordering::SeqCst);
    let thread = std::thread::spawn(move || raw_io(&reader, offset, size, None));
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    // Emulate SQLite's full-page checkpoint below the pager while a remote
    // fault is outstanding. Both operations enter the production VFS methods.
    raw_io(&checkpoint, offset, size, Some(&expected));
    release.send(()).unwrap();
    assert_eq!(thread.join().unwrap(), expected);
    drop(checkpoint);
    writer.close().unwrap();
    source.close().unwrap();
}

fn raw_io(
    connection: &crab_ltx::rusqlite::Connection,
    offset: u64,
    size: u32,
    write: Option<&[u8]>,
) -> Vec<u8> {
    use crab_ltx::rusqlite::ffi;
    let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
    let mut bytes = vec![0; size as usize];
    // SAFETY: connection is exclusively owned on this thread; FILE_POINTER
    // remains live, buffers cover a page, and all callbacks retain SQLite ABI.
    unsafe {
        assert_eq!(
            ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_FILE_POINTER,
                (&mut file as *mut *mut ffi::sqlite3_file).cast()
            ),
            ffi::SQLITE_OK
        );
        assert!(!file.is_null());
        let offset = i64::try_from(offset).unwrap();
        if let Some(data) = write {
            assert_eq!(data.len(), size as usize);
            assert_eq!(
                (*(*file).pMethods).xWrite.unwrap()(
                    file,
                    data.as_ptr().cast(),
                    size as i32,
                    offset
                ),
                ffi::SQLITE_OK
            );
        } else {
            assert_eq!(
                (*(*file).pMethods).xRead.unwrap()(
                    file,
                    bytes.as_mut_ptr().cast(),
                    size as i32,
                    offset
                ),
                ffi::SQLITE_OK
            );
        }
    }
    bytes
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_partial_cross_page_writes_preserve_untouched_bytes_at_all_page_sizes() {
    for size in [512u32, 4096, 65536] {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("source");
        let db = crab_ltx::rusqlite::Connection::open(&path).unwrap();
        db.pragma_update(None, "page_size", size).unwrap();
        // Persist a header before closing; an empty connection's page_size
        // setting alone is not carried into the managed connection.
        db.execute_batch("VACUUM").unwrap();
        drop(db);
        let mut writer = ManagedDb::open(&path, Limits::default()).unwrap();
        writer
            .transaction(|tx| {
                tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(1200000))")
            })
            .unwrap();
        let replica = Replica::new(
            StoreLayout::new(Store::new(Arc::new(InMemory::new())), "partial".into()),
            "epoch",
            Limits::default(),
        )
        .unwrap();
        let head = replica
            .replicate(&writer.capture().unwrap(), None)
            .await
            .unwrap();
        let paged = replica.paged(&head).await.unwrap();
        assert_eq!(
            paged.page_size(),
            size,
            "fixture must persist its requested SQLite page size"
        );
        let first = paged.page_count() - 1;
        let mut expected = paged.read_page(first).await.unwrap();
        expected.extend(paged.read_page(first + 1).await.unwrap());
        let offset = u64::from(first - 1) * u64::from(size);
        let sparse = paged
            .open_writable(&directory.path().join("sparse"))
            .unwrap();
        let connection = crab_ltx::rusqlite::Connection::open_with_flags_and_vfs(
            sparse.path(),
            crab_ltx::rusqlite::OpenFlags::default(),
            "crab-ltx-writable-v1",
        )
        .unwrap();
        let partial = vec![0x5a; 17];
        let write_offset = offset + u64::from(size) - 7;
        raw_io(
            &connection,
            write_offset,
            partial.len() as u32,
            Some(&partial),
        );
        expected[size as usize - 7..size as usize + 10].copy_from_slice(&partial);
        assert_eq!(
            raw_io(&connection, offset, 2 * size, None),
            expected,
            "page size {size}"
        );
        drop(connection);
        sparse.close().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn multiple_compaction_levels_and_bundle_overlays_continue_across_three_epochs() {
    let (directory, batches) = captures();
    let layout = StoreLayout::new(Store::new(Arc::new(InMemory::new())), "levels".into());
    let source = Replica::new(layout.clone(), "one", Limits::default()).unwrap();
    let mut head = source.replicate(&batches[0], None).await.unwrap();
    for batch in &batches[1..] {
        head = source.replicate(batch, Some(&head)).await.unwrap();
    }
    let original = head.clone();
    head = source.compact_range(&head, 0..2, 1).await.unwrap();
    head = source
        .compact_range(&head, 1..head.segment_count(), 1)
        .await
        .unwrap();
    let mut schedule = CompactionSchedule::default();
    head = schedule
        .run_due(&source, &head, Duration::from_secs(300))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.segment_count(), 1);
    assert!(source.compact_range(&head, 0..1, 1).await.is_err());
    head = source.bundle(&head).await.unwrap();
    let second = Replica::new(layout.clone(), "two", Limits::default()).unwrap();
    let inherited = second.inherit(&source, &head).await.unwrap();
    let mut writer = second
        .resume(&inherited, &directory.path().join("second"))
        .await
        .unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(5)"))
        .unwrap();
    let captured = writer.capture().unwrap();
    let next = second.replicate(&captured, Some(&inherited)).await.unwrap();
    let third = Replica::new(layout, "three", Limits::default()).unwrap();
    let inherited = third.inherit(&second, &next).await.unwrap();
    // The final flattened plan mixes an epoch-one bundle and epoch-two L0.
    let pinned = third.open_exact(inherited.manifest_digest()).await.unwrap();
    let path = directory.path().join("third");
    third.restore(&pinned, &path).await.unwrap();
    let sql = crab_ltx::rusqlite::Connection::open(path).unwrap();
    let count: usize = sql
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 5);
    let before = directory.path().join("before");
    let after = directory.path().join("after");
    source.restore(&original, &before).await.unwrap();
    source.restore(&head, &after).await.unwrap();
    assert_eq!(
        std::fs::read(before).unwrap(),
        std::fs::read(after).unwrap()
    );
}
