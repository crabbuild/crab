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
    writer.close().unwrap();
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

    for operation in ["write_all_at", "write_all"] {
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
