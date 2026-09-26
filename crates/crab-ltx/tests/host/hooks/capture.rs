//! Capture transfer bounds, deferred barriers, fencing, and pruning.

use super::*;

#[test]
fn capture_and_inspection_bound_each_filesystem_transfer() {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let mut writer = Db::open_with_host(
        &directory.path().join("streamed.sqlite"),
        Limits::default(),
        host,
    )
    .unwrap();
    writer
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(2000000))")
        })
        .unwrap();

    let batch = writer.capture().unwrap();

    assert!(batch.segments[0].info().size_bytes > 1_000_000);
    assert!(faults.largest_write.load(Ordering::Relaxed) < 128 * 1024);
    assert!(faults.largest_read.load(Ordering::Relaxed) < 128 * 1024);

    faults.largest_read.store(0, Ordering::Relaxed);
    faults.read_calls.store(0, Ordering::Relaxed);
    faults.largest_write.store(0, Ordering::Relaxed);
    let (snapshot, _) = writer
        .snapshot(&directory.path().join("streamed-snapshot.ltx"))
        .unwrap();
    assert!(snapshot.info().size_bytes > 1_000_000);
    assert!(faults.largest_write.load(Ordering::Relaxed) < 128 * 1024);
    assert!(faults.largest_read.load(Ordering::Relaxed) < 128 * 1024);
}
#[test]
fn deferred_captures_share_one_directory_barrier() {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let mut writer = Db::open_with_host(
        &directory.path().join("deferred.sqlite"),
        Limits::default(),
        host,
    )
    .unwrap();

    writer
        .transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(1)"))
        .unwrap();
    let first = writer.capture_deferred().unwrap();
    writer
        .transaction(|tx| tx.execute("INSERT INTO t VALUES(2)", []))
        .unwrap();
    let second = writer.capture_deferred().unwrap();

    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);
    assert!(!first.segments.is_empty());
    assert!(!second.segments.is_empty());

    writer.durability_barrier().unwrap();
    assert_eq!(
        faults.file_syncs.load(Ordering::Relaxed),
        first.segments.len() + second.segments.len()
    );
    // The first local barrier also seals the new ltx/0, ltx, and session names.
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 4);
    writer
        .transaction(|tx| tx.execute("INSERT INTO t VALUES(3)", []))
        .unwrap();
    let third = writer.capture_deferred().unwrap();
    writer.durability_barrier().unwrap();
    assert_eq!(
        faults.file_syncs.load(Ordering::Relaxed),
        first.segments.len() + second.segments.len() + third.segments.len()
    );
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 5);
    writer.close().unwrap();
}
#[test]
fn failed_deferred_barrier_fences_before_acknowledgement() {
    for operation in ["sync_all", "sync_parent"] {
        let directory = tempfile::TempDir::new().unwrap();
        let faults = Arc::new(Faults::default());
        let host = Host::default().with_filesystem(faults.clone());
        let mut writer = Db::open_with_host(
            &directory.path().join("deferred-failure.sqlite"),
            Limits::default(),
            host,
        )
        .unwrap();
        writer
            .transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(1)"))
            .unwrap();
        let captured = writer.capture_deferred().unwrap();

        faults.arm(Some(operation));
        assert!(matches!(
            writer.durability_barrier(),
            Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::StorageFull
        ));
        assert!(matches!(writer.capture(), Err(CrabError::Fenced)));
        assert!(!captured.segments.is_empty());
    }
}
#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_checksum_write_failure_fences_after_sealing_the_cut() {
    let (directory, faults, host, mut source) = fixture();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-checksums"),
            [4; 16],
        ),
        [5; 32],
        [6; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica
        .prepare(None, &source.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    source.close().unwrap();

    let destination = directory.path().join("cell-active.sqlite");
    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&destination)
        .await
        .unwrap();
    let mut writer = writable.open_writable(&destination).unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
        .unwrap();
    let synced_before_cut = faults.file_syncs.load(Ordering::Relaxed);
    writer.capture_deferred().unwrap();
    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), synced_before_cut);
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(3)"))
        .unwrap();
    faults.arm(Some("write_all_at"));
    injected(writer.capture_deferred());
    faults.arm(None);
    assert!(matches!(writer.capture(), Err(CrabError::Fenced)));
}
#[test]
fn capture_partial_write_sync_and_rename_failures_fence_the_session() {
    for operation in ["write_all", "sync_all", "rename"] {
        let (_directory, faults, _host, mut writer) = fixture();
        faults.arm(Some(operation));
        injected(writer.capture());
        faults.arm(None);
        assert!(
            matches!(writer.transaction(|_| Ok(())), Err(CrabError::Fenced)),
            "{operation}"
        );
    }
}
#[cfg(feature = "replica")]
#[test]
fn published_cut_pruning_bounds_each_filesystem_transfer() {
    let (_directory, faults, host, mut writer) = fixture();
    writer
        .transaction(|tx| tx.execute("INSERT INTO t VALUES(randomblob(2000000))", []))
        .unwrap();
    let batch = writer.capture_deferred().unwrap();
    assert!(batch.segments[0].info().size_bytes > 1_000_000);
    let retained = host.local_disk_used();
    faults.largest_read.store(0, Ordering::Relaxed);

    assert_eq!(writer.prune_captured(&batch).unwrap(), batch.segments.len());

    assert!(faults.largest_read.load(Ordering::Relaxed) < 128 * 1024);
    assert!(host.local_disk_used() < retained);
    assert!(
        batch
            .segments
            .iter()
            .all(|segment| !segment.path().exists())
    );
    writer.close().unwrap();
}

#[cfg(feature = "replica")]
#[test]
fn published_cut_pruning_rejects_changed_bytes_without_releasing_accounting() {
    for damage in ["truncated", "extended", "corrupted", "replaced"] {
        let (_directory, _faults, host, mut writer) = fixture();
        let batch = writer.capture_deferred().unwrap();
        let path = batch.segments[0].path();
        let original = std::fs::read(path).unwrap();
        let retained = host.local_disk_used();
        let mut changed = original.clone();
        match damage {
            "truncated" => changed.truncate(changed.len() - 1),
            "extended" => changed.push(0),
            "corrupted" => {
                let middle = changed.len() / 2;
                changed[middle] ^= 1;
            }
            "replaced" => {
                // A separately valid cut must not satisfy this batch's identity.
                let (_other_directory, _faults, _host, mut other) = fixture();
                let other_batch = other.capture().unwrap();
                changed = std::fs::read(other_batch.segments[0].path()).unwrap();
            }
            _ => unreachable!(),
        }
        std::fs::write(path, &changed).unwrap();

        assert!(writer.prune_captured(&batch).is_err(), "{damage}");
        assert!(path.exists(), "{damage}");
        assert_eq!(host.local_disk_used(), retained, "{damage}");

        std::fs::write(path, &original).unwrap();
        assert_eq!(writer.prune_captured(&batch).unwrap(), batch.segments.len());
        assert_eq!(writer.prune_captured(&batch).unwrap(), 0);
        writer.close().unwrap();
    }
}

#[cfg(feature = "replica")]
#[test]
fn captured_pruning_retains_accounting_after_io_failure() {
    for operation in ["read_exact_at", "remove_file"] {
        let (_directory, faults, _host, mut writer) = fixture();
        let batch = writer.capture().unwrap();
        faults.arm(Some(operation));
        injected(writer.prune_captured(&batch));
        assert!(batch.segments[0].path().exists());
        faults.arm(None);
        assert_eq!(writer.prune_captured(&batch).unwrap(), batch.segments.len());
        assert_eq!(writer.prune_captured(&batch).unwrap(), 0);
    }
}
#[cfg(feature = "replica")]
#[test]
fn published_deferred_capture_is_pruned_without_a_local_durability_barrier() {
    let (_directory, faults, _host, mut writer) = fixture();
    let batch = writer.capture_deferred().unwrap();

    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);
    faults.arm(Some("sync_parent"));
    assert_eq!(writer.prune_captured(&batch).unwrap(), batch.segments.len());
    faults.arm(None);
    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);

    writer.close().unwrap();
    assert_eq!(faults.file_syncs.load(Ordering::Relaxed), 0);
    assert_eq!(faults.parent_syncs.load(Ordering::Relaxed), 0);
}

fn resumed_checksum_fixture() -> (tempfile::TempDir, Arc<Faults>, Db) {
    let (directory, faults, host, mut source) = fixture();
    source
        .transaction(|tx| tx.execute("INSERT INTO t VALUES(zeroblob(33554432))", []))
        .unwrap();
    source.capture().unwrap();
    source.persist_continuation().unwrap();
    source.close().unwrap();
    faults.track_all.store(true, Ordering::Relaxed);
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("checksum-handoff"),
            [4; 16],
        ),
        [5; 32],
        [6; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let resumed = replica
        .open_resumed(
            &directory.path().join("source.sqlite"),
            &directory.path().join("resumed.sqlite"),
        )
        .unwrap();
    (directory, faults, resumed)
}

#[test]
fn checksum_handoff_batches_reads_and_preserves_the_dense_sidecar() {
    let (directory, faults, resumed) = resumed_checksum_fixture();
    let sidecar = directory.path().join("resumed.sqlite.crab-ltx-checksums");
    let before = std::fs::read(&sidecar).unwrap();
    faults.read_calls.store(0, Ordering::Relaxed);
    faults.largest_read.store(0, Ordering::Relaxed);

    resumed.persist_continuation().unwrap();

    assert!(
        faults.read_calls.load(Ordering::Relaxed) <= before.len().div_ceil(64 * 1024),
        "checksum handoff made {} reads for {} bytes",
        faults.read_calls.load(Ordering::Relaxed),
        before.len()
    );
    assert!(faults.largest_read.load(Ordering::Relaxed) <= 64 * 1024);
    assert_eq!(std::fs::read(sidecar).unwrap(), before);
    resumed.close().unwrap();
}

#[test]
fn checksum_capture_batches_adjacent_updates() {
    let (_directory, faults, mut resumed) = resumed_checksum_fixture();
    resumed
        .transaction(|tx| tx.execute("UPDATE t SET v=randomblob(length(v))", []))
        .unwrap();
    faults.write_calls.store(0, Ordering::Relaxed);
    faults.largest_write.store(0, Ordering::Relaxed);

    let captured = resumed.capture_deferred().unwrap();

    assert!(
        faults.write_calls.load(Ordering::Relaxed) < 1024,
        "capture made {} writes for {} cut bytes",
        faults.write_calls.load(Ordering::Relaxed),
        captured
            .segments
            .iter()
            .map(|cut| cut.info().size_bytes)
            .sum::<u64>()
    );
    assert!(faults.largest_write.load(Ordering::Relaxed) <= 64 * 1024);
    // Re-read and fold the persisted blocks before allowing a clean handoff.
    resumed.persist_continuation().unwrap();
    resumed.close().unwrap();
}
