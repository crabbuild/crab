//! Prepare transfer bounds, upload overlap, and cancellation cleanup.

use super::*;

async fn writable_fixture(
    extra_bytes: i64,
) -> (tempfile::TempDir, Arc<Faults>, crab_ltx::CellPagedDatabase) {
    let (directory, faults, host, mut source) = fixture();
    source
        .transaction(|tx| {
            tx.execute("INSERT INTO t VALUES(zeroblob(?1))", [extra_bytes])?;
            Ok(())
        })
        .unwrap();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("async-checksums"),
            [4; 16],
        ),
        [5; 32],
        [6; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host.with_job_slots(Arc::new(tokio::sync::Semaphore::new(1))));
    let root = replica
        .prepare(None, &source.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    source.close().unwrap();
    let paged = replica.open_root(&root).await.unwrap().paged();
    (directory, faults, paged)
}

#[tokio::test]
async fn writable_preparation_dispatches_filesystem_work_off_the_async_thread() {
    // More than 8,192 pages exercises streamed checksum writes as well as the
    // final durability barrier, with only one available host executor slot.
    let (directory, faults, paged) = writable_fixture(35_000_000).await;
    let destination = directory.path().join("active.sqlite");
    *faults.forbidden_thread.lock().unwrap() = Some(std::thread::current().id());
    let prepared = paged.prepare_writable(&destination).await;
    *faults.forbidden_thread.lock().unwrap() = None;
    let mut writer = prepared.unwrap().open_writable(&destination).unwrap();
    let size: i64 = writer
        .transaction(|connection| {
            connection.query_row("SELECT sum(length(v)) FROM t", [], |row| row.get(0))
        })
        .unwrap();
    assert_eq!(size, 35_020_000);
    writer.close().unwrap();
}

#[tokio::test]
async fn failed_checksum_preparation_cleans_up_without_blocking_the_async_thread() {
    let (directory, faults, paged) = writable_fixture(0).await;
    for operation in ["write_all", "sync_all", "sync_parent"] {
        let destination = directory.path().join(format!("{operation}.sqlite"));
        faults.arm(Some(operation));
        *faults.forbidden_thread.lock().unwrap() = Some(std::thread::current().id());
        let result = paged.clone().prepare_writable(&destination).await;
        *faults.forbidden_thread.lock().unwrap() = None;
        faults.arm(None);
        assert!(
            matches!(result, Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::StorageFull)
        );
        assert!(
            !directory
                .path()
                .join(format!("{operation}.sqlite.crab-ltx-checksums"))
                .exists()
        );
    }
}

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_prepare_bounds_source_transfers_without_local_writes() {
    let (directory, faults, host, mut writer) = fixture();
    writer
        .transaction(|tx| {
            tx.execute("INSERT INTO t VALUES(randomblob(10000000))", [])?;
            Ok(())
        })
        .unwrap();
    let captured = writer.capture().unwrap();
    faults.track_all.store(true, Ordering::Relaxed);
    faults.largest_read.store(0, Ordering::Relaxed);
    faults.read_calls.store(0, Ordering::Relaxed);
    faults.largest_write.store(0, Ordering::Relaxed);
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-streaming-bound"),
            [31; 16],
        ),
        [32; 32],
        [33; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);

    replica.prepare(None, &captured, 1, 1).await.unwrap();

    assert!(
        faults.largest_read.load(Ordering::Relaxed) <= 8 * 1024 * 1024,
        "largest source read was {} bytes",
        faults.largest_read.load(Ordering::Relaxed)
    );
    assert_eq!(faults.largest_write.load(Ordering::Relaxed), 0);
    let expected_reads: usize = captured
        .segments
        .iter()
        .map(|segment| segment.info().size_bytes.div_ceil(8 << 20) as usize)
        .sum();
    assert_eq!(faults.read_calls.load(Ordering::Relaxed), expected_reads);
    writer.close().unwrap();
    drop(directory);
}
#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn caller_constructed_segment_uses_full_inspection_fallback() {
    let (_directory, faults, host, mut writer) = fixture();
    let mut captured = writer.capture().unwrap();
    captured.segments[0] = LocalSegment::new(
        captured.segments[0].path().to_owned(),
        captured.segments[0].info().clone(),
    );
    faults.track_all.store(true, Ordering::Relaxed);
    faults.read_calls.store(0, Ordering::Relaxed);
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-external-segment"),
            [34; 16],
        ),
        [35; 32],
        [36; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);

    replica.prepare(None, &captured, 1, 1).await.unwrap();

    assert!(faults.read_calls.load(Ordering::Relaxed) > 1);
    writer.close().unwrap();
}
#[cfg(feature = "replica")]
#[tokio::test(start_paused = true)]
async fn prepare_overlaps_independent_immutable_uploads() {
    let (_directory, _faults, _host, mut writer) = fixture();
    let first = writer.capture_deferred().unwrap();
    let backend = InMemory::new();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(backend.clone())),
        ObjectPath::from("parallel-preparation"),
        [71; 16],
    );
    let initial = CellReplica::new(layout, [72; 32], [73; 16], Limits::default()).unwrap();
    let root = initial.prepare(None, &first, 1, 1).await.unwrap().root();
    writer.prune_captured(&first).unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(randomblob(4096))"))
        .unwrap();
    let captured = writer.capture_deferred().unwrap();

    let delay = Duration::from_millis(100);
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(ThrottledStore::new(
                backend,
                ThrottleConfig {
                    wait_put_per_call: delay,
                    ..ThrottleConfig::default()
                },
            ))),
            ObjectPath::from("parallel-preparation"),
            [71; 16],
        ),
        [72; 32],
        [73; 16],
        Limits::default(),
    )
    .unwrap();
    let started = tokio::time::Instant::now();

    replica.prepare(Some(&root), &captured, 2, 1).await.unwrap();

    assert_eq!(started.elapsed(), delay);
    writer.close().unwrap();
}
#[cfg(feature = "replica")]
#[tokio::test(start_paused = true)]
async fn warm_append_reuses_its_authenticated_root_metadata() {
    let (_directory, _faults, _host, mut writer) = fixture();
    let first = writer.capture_deferred().unwrap();
    let delay = Duration::from_millis(100);
    let backend = InMemory::new();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(ThrottledStore::new(
                backend.clone(),
                ThrottleConfig {
                    wait_get_per_call: delay,
                    wait_put_per_call: delay,
                    ..ThrottleConfig::default()
                },
            ))),
            ObjectPath::from("cached-root-metadata"),
            [74; 16],
        ),
        [75; 32],
        [76; 16],
        Limits::default(),
    )
    .unwrap();
    let root = replica.prepare(None, &first, 1, 1).await.unwrap().root();
    writer.prune_captured(&first).unwrap();
    writer
        .transaction(|transaction| transaction.execute_batch("INSERT INTO t VALUES(2)"))
        .unwrap();
    let second = writer.capture_deferred().unwrap();
    let started = tokio::time::Instant::now();

    let prepared = replica.prepare(Some(&root), &second, 2, 1).await.unwrap();

    // The store throttles HEAD with the PUT delay: one parallel presence
    // wave plus one overlapping immutable-upload wave, with no serial
    // metadata GETs or directory-before-root upload dependency.
    assert_eq!(started.elapsed(), delay * 2);
    assert_eq!(prepared.root().position, second.position);
    let independent = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(ThrottledStore::new(
                backend,
                ThrottleConfig {
                    wait_get_per_call: delay,
                    ..ThrottleConfig::default()
                },
            ))),
            ObjectPath::from("cached-root-metadata"),
            [74; 16],
        ),
        [75; 32],
        [76; 16],
        Limits::default(),
    )
    .unwrap();
    let cold_started = tokio::time::Instant::now();
    independent.open_root(&prepared.root()).await.unwrap();
    assert!(cold_started.elapsed() >= delay * 2);
    writer.close().unwrap();
}
#[cfg(feature = "replica")]
#[tokio::test(start_paused = true)]
async fn prepare_opens_captured_segments_concurrently() {
    let (_directory, _faults, _host, mut writer) = fixture();
    let mut captured = writer.capture_deferred().unwrap();
    for value in [2, 3] {
        writer
            .transaction(|tx| tx.execute("INSERT INTO t VALUES(?1)", [value]))
            .unwrap();
        let next = writer.capture_deferred().unwrap();
        captured.segments.extend(next.segments);
        captured.position = next.position;
    }
    assert_eq!(captured.segments.len(), 3);

    let delay = Duration::from_millis(100);
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("parallel-capture-inputs"),
            [81; 16],
        ),
        [82; 32],
        [83; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(Host::default().with_executor(Arc::new(DelayedExecutor { delay })));
    let started = tokio::time::Instant::now();

    replica.prepare(None, &captured, 1, 1).await.unwrap();

    // One open wave plus size/read/size upload jobs. Serial opens would add
    // two more delay intervals before the already-concurrent uploads begin.
    assert_eq!(started.elapsed(), delay * 4);
    writer.close().unwrap();
}
#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_prepare_needs_no_scratch_or_local_durability_barrier() {
    let (_directory, faults, host, mut writer) = fixture();
    let captured = writer.capture_deferred().unwrap();
    let scratch_slots = Arc::new(tokio::sync::Semaphore::new(0));
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-ephemeral-scratch"),
            [41; 16],
        ),
        [42; 32],
        [43; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(
        host.with_scratch_slots(scratch_slots)
            .with_local_disk_budget(crab_ltx::DiskBudget::new(0)),
    );
    faults.calls.lock().unwrap().clear();
    faults.file_syncs.store(0, Ordering::Relaxed);
    faults.parent_syncs.store(0, Ordering::Relaxed);

    replica.prepare(None, &captured, 1, 1).await.unwrap();

    let calls = faults.calls.lock().unwrap();
    assert!(!calls.contains("create"));
    assert!(!calls.contains("open_rw"));
    assert!(!calls.contains("write_all"));
    assert!(!calls.contains("sync_all"));
    assert!(!calls.contains("sync_parent"));
    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);
    drop(calls);
    writer.close().unwrap();
}
#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_cell_prepare_releases_pinned_source_without_publishing_a_root() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("source.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction
                .execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(2000000))")
        })
        .unwrap();
    let captures = writer.capture().unwrap();
    let backend = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: Duration::from_secs(5),
            ..ThrottleConfig::default()
        },
    ));
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(backend),
            ObjectPath::from("cell-cancelled-prepare"),
            [21; 16],
        ),
        [22; 32],
        [23; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(Host::default());

    let result = tokio::time::timeout(
        Duration::from_millis(250),
        replica.prepare(None, &captures, 1, 1),
    )
    .await;
    assert!(
        result.is_err(),
        "the throttled immutable upload must be cancelled"
    );
    writer.prune_captured(&captures).unwrap();
    assert!(
        captures
            .segments
            .iter()
            .all(|segment| !segment.path().exists())
    );
    writer.close().unwrap();
}
