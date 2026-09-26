//! Exact-root preparation, inventory verification, and overlay recovery.

use super::*;

#[tokio::test]
async fn read_only_roots_keep_exact_snapshots_without_materializing_pages() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("source.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction
                .execute_batch("CREATE TABLE counter(value INTEGER); INSERT INTO counter VALUES(1)")
        })
        .unwrap();
    let first = writer.capture().unwrap();
    let disk = DiskBudget::new(8 << 20);
    let replica = replica(Store::new(Arc::new(InMemory::new())), [41; 32], [42; 16])
        .with_host(Host::default().with_local_disk_budget(disk.clone()));
    let first_root = replica.prepare(None, &first, 1, 1).await.unwrap().root();
    let first_path = directory.path().join("first.sqlite");
    let first_view = replica
        .open_root(&first_root)
        .await
        .unwrap()
        .open_read_only(&first_path)
        .unwrap();
    assert_eq!(first_view.root(), first_root);
    assert_eq!(disk.used(), 0);
    assert_eq!(std::fs::metadata(&first_path).unwrap().len(), 0);
    assert!(
        replica
            .open_root(&first_root)
            .await
            .unwrap()
            .open_read_only(&first_path)
            .is_err()
    );
    assert!(first_path.exists());
    {
        let connection = first_view.connection().unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            -64
        );
        let value: i64 = connection
            .query_row("SELECT value FROM counter", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 1);
        connection.pragma_update(None, "query_only", false).unwrap();
        assert!(
            connection
                .is_readonly(rusqlite::DatabaseName::Main)
                .unwrap()
        );
        assert!(
            connection
                .execute("UPDATE counter SET value=99", [])
                .is_err()
        );
    }

    writer
        .transaction(|transaction| transaction.execute_batch("UPDATE counter SET value=2"))
        .unwrap();
    let second = writer.capture().unwrap();
    let second_root = replica
        .prepare(Some(&first_root), &second, 2, 1)
        .await
        .unwrap()
        .root();
    let second_path = directory.path().join("second.sqlite");
    let second_view = replica
        .open_root(&second_root)
        .await
        .unwrap()
        .open_read_only(&second_path)
        .unwrap();
    let old: i64 = first_view
        .connection()
        .unwrap()
        .query_row("SELECT value FROM counter", [], |row| row.get(0))
        .unwrap();
    let current: i64 = second_view
        .connection()
        .unwrap()
        .query_row("SELECT value FROM counter", [], |row| row.get(0))
        .unwrap();
    assert_eq!((old, current), (1, 2));

    for path in [&first_path, &second_path] {
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
        for suffix in ["-wal", "-shm", "-journal", ".crab-ltx"] {
            assert!(!std::path::PathBuf::from(format!("{}{suffix}", path.display())).exists());
        }
    }
    drop(first_view);
    drop(second_view);
    assert!(!first_path.exists());
    assert!(!second_path.exists());
    assert_eq!(disk.used(), 0);
    writer.close().unwrap();
}

#[tokio::test]
async fn small_appends_report_a_bounded_publication_cost() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(4096))")
        })
        .unwrap();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("runtime"),
            [3; 16],
        ),
        [211; 32],
        [212; 16],
        Limits::default(),
    )
    .unwrap();

    // A bootstrap root uploads one body, one index, the changed directory
    // nodes, and the root document. The bound documents the per-command
    // object-store amplification the runtime budgets against.
    let first = writer.capture().unwrap();
    let root = replica.prepare(None, &first, 1, 1).await.unwrap().root();
    let initial = replica.take_publication_cost();
    assert!(
        (4..=12).contains(&initial.objects),
        "bootstrap objects: {initial:?}"
    );
    assert!(
        initial.bytes >= first.segments[0].info().size_bytes,
        "{initial:?}"
    );

    // One appended command pays at least a body and index, and no more than the
    // same bounded set of metadata objects.
    writer
        .transaction(|transaction| {
            transaction.execute_batch("INSERT INTO t VALUES(randomblob(4096))")
        })
        .unwrap();
    let second = writer.capture().unwrap();
    replica.prepare(Some(&root), &second, 2, 1).await.unwrap();
    let append = replica.take_publication_cost();
    assert!(
        (3..=12).contains(&append.objects),
        "append objects: {append:?}"
    );
    assert!(
        append.bytes >= second.segments[0].info().size_bytes,
        "{append:?}"
    );
    assert_eq!(replica.publication_cost().objects, 0);
}

#[tokio::test]
async fn oversized_commit_publishes_as_a_full_image_root() {
    let directory = tempfile::TempDir::new().unwrap();
    // The incremental bound is far below the commit and the database, so the
    // capture escalates and the publication must admit the full image.
    let limits = Limits {
        max_capture_bytes: 8 * 1024,
        ..Limits::default()
    };
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), limits).unwrap();
    writer
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE payload(value BLOB)"))
        .unwrap();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("runtime"),
            [3; 16],
        ),
        [201; 32],
        [202; 16],
        limits,
    )
    .unwrap();
    let first = writer.capture().unwrap();
    let first_root = replica.prepare(None, &first, 1, 1).await.unwrap().root();

    writer
        .transaction(|transaction| {
            transaction.execute_batch("INSERT INTO payload VALUES(randomblob(65536))")
        })
        .unwrap();
    let second = writer.capture().unwrap();
    assert!(
        second.segments[0].info().size_bytes > limits.max_capture_bytes,
        "the escalated cut must exceed the incremental bound"
    );

    let prepared = replica
        .prepare(Some(&first_root), &second, 2, 1)
        .await
        .unwrap();
    let root = prepared.root();
    assert_eq!(prepared.verified().segment_count(), 2);
    assert_eq!(root.position, second.position);

    // The published root restores the escalated commit exactly.
    let restored = directory.path().join("restored.sqlite");
    replica
        .open_root(&root)
        .await
        .unwrap()
        .restore(&restored)
        .await
        .unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(&restored).unwrap();
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM payload", [], |row| row.get(0))
        .unwrap();
    let bytes: i64 = connection
        .query_row("SELECT length(value) FROM payload", [], |row| row.get(0))
        .unwrap();
    assert_eq!((rows, bytes), (1, 65536));
    writer.close().unwrap();
}

#[tokio::test]
async fn prepared_cell_handles_release_dirty_admission() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let dirty = Arc::new(tokio::sync::Semaphore::new(1));
    let host = Host::default().with_dirty_slots(dirty.clone());
    let replica =
        replica(Store::new(Arc::new(InMemory::new())), [101; 32], [102; 16]).with_host(host);

    let prepared = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap();
    assert_eq!(dirty.available_permits(), 1);
    let writable = prepared
        .verified()
        .paged()
        .prepare_writable(&directory.path().join("active.sqlite"))
        .await
        .unwrap();
    assert_eq!(dirty.available_permits(), 1);

    drop(writable);
    writer.close().unwrap();
}

#[tokio::test]
async fn prepared_root_reopens_without_a_mutable_head() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE messages(id INTEGER PRIMARY KEY, body TEXT);\
                 INSERT INTO messages(body) VALUES ('first');\
                 CREATE TABLE payload(value BLOB);\
                 INSERT INTO payload VALUES(randomblob(2000000))",
            )
        })
        .unwrap();
    let read_bytes = Arc::new(AtomicU64::new(0));
    let observed = read_bytes.clone();
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |bytes| {
            observed.fetch_add(bytes, Ordering::SeqCst);
        }));
    let replica = replica(store.clone(), [1; 32], [2; 16]);
    let first = writer.capture().unwrap();
    let prepared = replica.prepare(None, &first, 1, 7).await.unwrap();
    let first_root = prepared.root();
    assert_eq!(prepared.predecessor(), None);
    assert_eq!(prepared.verified().schema(), 7);

    writer
        .transaction(|transaction| {
            transaction.execute("INSERT INTO messages(body) VALUES ('second')", [])?;
            Ok(())
        })
        .unwrap();
    let second = writer.capture().unwrap();
    let prepared = replica
        .prepare(Some(&first_root), &second, 2, 7)
        .await
        .unwrap();
    assert_eq!(prepared.predecessor(), Some(first_root));
    assert_eq!(prepared.verified().segment_count(), 2);
    assert_eq!(prepared.root().position, second.position);

    let expected_dir = tempfile::TempDir::new().unwrap();
    let expected_path = expected_dir.path().join("expected.sqlite");
    let segments = first
        .segments
        .iter()
        .chain(&second.segments)
        .cloned()
        .collect::<Vec<_>>();
    let plan = VerifiedPlan::new(&segments, second.position, Limits::default()).unwrap();
    restore_exact(&plan, &expected_path).unwrap();
    let expected = std::fs::read(expected_path).unwrap();
    writer.close().unwrap();
    directory.close().unwrap();

    read_bytes.store(0, Ordering::SeqCst);
    let reopened = replica.open_root(&prepared.root()).await.unwrap();
    assert_eq!(reopened.root(), prepared.root());
    assert_eq!(
        reopened.database_pages(),
        prepared.verified().database_pages()
    );
    assert!(reopened.page_size().is_power_of_two());
    assert!(
        read_bytes.load(Ordering::SeqCst) < 100_000,
        "activation must not fetch the LTX bodies or every directory leaf"
    );
    let restored_path = expected_dir.path().join("streamed.sqlite");
    assert_eq!(
        reopened.restore(&restored_path).await.unwrap(),
        prepared.root().position
    );
    assert_eq!(std::fs::read(&restored_path).unwrap(), expected);
    read_bytes.store(0, Ordering::SeqCst);
    assert!(reopened.restore(&restored_path).await.is_err());
    assert_eq!(read_bytes.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(restored_path).unwrap(), expected);

    let scratch = Arc::new(tokio::sync::Semaphore::new(64));
    let limited = replica
        .clone()
        .with_host(Host::default().with_scratch_slots(scratch.clone()))
        .open_root(&prepared.root())
        .await
        .unwrap();
    read_bytes.store(0, Ordering::SeqCst);
    let rejected_path = expected_dir.path().join("scratch-rejected.sqlite");
    assert!(matches!(
        limited.restore(&rejected_path).await,
        Err(crab_ltx::CrabError::Limit(
            crab_ltx::LimitKind::ScratchDiskBytes
        ))
    ));
    assert_eq!(scratch.available_permits(), 64);
    assert_eq!(read_bytes.load(Ordering::SeqCst), 0);
    assert!(!rejected_path.exists());
}
#[tokio::test]
async fn exact_root_inventory_verifies_every_remote_dependency() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(2000000))",
            )
        })
        .unwrap();
    let backend = Arc::new(InMemory::new());
    let store = Store::new(backend.clone());
    let cell = [31; 32];
    let incarnation = [32; 16];
    let layout = CellStorageLayout::new(store.clone(), Path::from("runtime"), [3; 16]);
    let replica = CellReplica::new(layout.clone(), cell, incarnation, Limits::default()).unwrap();
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();

    let objects = replica.reachable_objects(&root).await.unwrap();
    assert!(objects.windows(2).all(|pair| pair[0] < pair[1]));
    for kind in [
        CellObjectKind::Ltx,
        CellObjectKind::Index,
        CellObjectKind::Directory,
        CellObjectKind::Root,
    ] {
        assert!(objects.iter().any(|object| object.kind == kind));
    }
    for object in &objects {
        let path = layout.incarnation_object_path(&cell, &incarnation, &object.digest, object.kind);
        store.head(&path).await.unwrap();
    }

    let missing = objects
        .iter()
        .find(|object| object.kind == CellObjectKind::Directory)
        .unwrap();
    let missing_path =
        layout.incarnation_object_path(&cell, &incarnation, &missing.digest, missing.kind);
    backend.delete(&missing_path).await.unwrap();
    assert!(replica.reachable_objects(&root).await.is_err());
}
#[tokio::test]
async fn warm_root_cache_does_not_mask_missing_metadata() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let backend = Arc::new(InMemory::new());
    let cell = [41; 32];
    let incarnation = [42; 16];
    let layout =
        CellStorageLayout::new(Store::new(backend.clone()), Path::from("runtime"), [3; 16]);
    let replica = CellReplica::new(layout.clone(), cell, incarnation, Limits::default()).unwrap();
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    let path =
        layout.incarnation_object_path(&cell, &incarnation, &root.digest, CellObjectKind::Root);
    let root_bytes = backend.get(&path).await.unwrap().bytes().await.unwrap();
    let segment_page = replica
        .reachable_objects(&root)
        .await
        .unwrap()
        .into_iter()
        .find(|object| object.kind == CellObjectKind::Root && object.digest != root.digest)
        .unwrap();
    backend.delete(&path).await.unwrap();

    assert!(replica.reachable_objects(&root).await.is_err());
    writer
        .transaction(|transaction| transaction.execute_batch("INSERT INTO values_ VALUES(2)"))
        .unwrap();
    let next = writer.capture().unwrap();
    assert!(replica.prepare(Some(&root), &next, 2, 1).await.is_err());
    backend.put(&path, root_bytes.into()).await.unwrap();
    let segment_path = layout.incarnation_object_path(
        &cell,
        &incarnation,
        &segment_page.digest,
        CellObjectKind::Root,
    );
    backend.delete(&segment_path).await.unwrap();
    assert!(replica.prepare(Some(&root), &next, 2, 1).await.is_err());
    writer.close().unwrap();
}
#[tokio::test]
async fn root_scope_and_commit_sequence_are_fenced() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let batch = writer.capture().unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let replica = replica(store, [1; 32], [2; 16]);
    let root = replica.prepare(None, &batch, 4, 1).await.unwrap().root();
    assert!(replica.prepare(Some(&root), &batch, 4, 1).await.is_err());
    let wrong = RootRef {
        cell: [9; 32],
        ..root
    };
    assert!(replica.open_root(&wrong).await.is_err());
    writer.close().unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn prepare_does_not_write_mutable_keys() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("cell.sqlite");
    let mut writer = Db::open(&path, Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE messages(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO messages(body) VALUES ('first')",
            )
        })
        .unwrap();
    let first = writer.capture().unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute("INSERT INTO messages(body) VALUES ('second')", [])?;
            Ok(())
        })
        .unwrap();
    let second = writer.capture().unwrap();

    let cell = [71; 32];
    let incarnation = [72; 16];
    let first_segment = first.segments.first().unwrap();
    let second_segment = second.segments.first().unwrap();
    let bundle = Bundle::encode(
        vec![
            BundleEntry::for_cell(
                cell,
                incarnation,
                first_segment.info().clone(),
                std::fs::read(first_segment.path()).unwrap(),
            ),
            BundleEntry::for_cell(
                [73; 32],
                incarnation,
                first_segment.info().clone(),
                std::fs::read(first_segment.path()).unwrap(),
            ),
            BundleEntry::for_cell(
                cell,
                incarnation,
                second_segment.info().clone(),
                std::fs::read(second_segment.path()).unwrap(),
            ),
        ],
        Limits::default(),
    )
    .unwrap();
    let bundle_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(bundle_file.path(), bundle.read_all().unwrap()).unwrap();
    let bundle = Bundle::decode_file(bundle_file.path(), Limits::default()).unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let replica = replica(store.clone(), cell, incarnation);
    let bundled = replica.prepare_bundle(None, &bundle, 2, 5).await.unwrap();
    assert_eq!(bundled.verified().segment_count(), 2);
    assert_eq!(bundled.root().position, second.position);

    writer
        .transaction(|transaction| {
            transaction.execute("INSERT INTO messages(body) VALUES ('third')", [])?;
            Ok(())
        })
        .unwrap();
    let third = writer.capture().unwrap();
    let appended = replica
        .prepare(Some(&bundled.root()), &third, 3, 5)
        .await
        .unwrap();
    writer.close().unwrap();
    directory.close().unwrap();
    let compaction_scratch = tempfile::TempDir::new().unwrap();

    let compacted = replica
        .prepare_compaction(&appended.root(), 0..2, 1, compaction_scratch.path())
        .await
        .unwrap();
    assert_eq!(compacted.predecessor(), Some(appended.root()));
    assert_eq!(compacted.root().position, appended.root().position);
    assert_eq!(compacted.root().commit_sequence, 3);
    assert_eq!(compacted.verified().segment_count(), 2);
    assert_ne!(compacted.root().digest, appended.root().digest);

    let snapshot = replica
        .prepare_compaction(&compacted.root(), 0..2, 9, compaction_scratch.path())
        .await
        .unwrap();
    assert_eq!(snapshot.verified().segment_count(), 1);
    assert_eq!(snapshot.root().position, appended.root().position);
    let written = store
        .list_prefix(&Path::from("runtime/cells/v1"))
        .await
        .unwrap();
    assert!(
        !written.is_empty()
            && written
                .iter()
                .all(|object| object.location.as_ref().contains("/objects/")),
        "root preparation must write only immutable dependency objects"
    );
    assert!(
        written
            .iter()
            .all(|object| !object.location.as_ref().contains("/.staging/")),
        "bundle staging objects must not remain after preparation"
    );
    let destination = tempfile::TempDir::new().unwrap();
    let path = destination.path().join("restored.sqlite");
    let writable = replica
        .open_root(&snapshot.root())
        .await
        .unwrap()
        .paged()
        .prepare_writable(&path)
        .await
        .unwrap();
    let mut restored = tokio::task::spawn_blocking(move || writable.open_writable(&path))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        restored
            .transaction(|transaction| {
                transaction.query_row("SELECT count(*) FROM messages", [], |row| {
                    row.get::<_, u32>(0)
                })
            })
            .unwrap(),
        3
    );
    restored.close().unwrap();
}
#[tokio::test]
async fn recovered_overlay_requires_exact_predecessor_and_final_position() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer =
        Db::open(&directory.path().join("recovery.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE events(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO events(body) VALUES ('published')",
            )
        })
        .unwrap();
    let first = writer.capture().unwrap();
    let cell = [81; 32];
    let incarnation = [82; 16];
    let replica = replica(Store::new(Arc::new(InMemory::new())), cell, incarnation);
    let base = replica.prepare(None, &first, 1, 6).await.unwrap().root();

    writer
        .transaction(|transaction| {
            transaction.execute("INSERT INTO events(body) VALUES ('fleet-only')", [])?;
            Ok(())
        })
        .unwrap();
    let tail = writer.capture().unwrap();
    let entries = tail
        .segments
        .iter()
        .map(|segment| {
            BundleEntry::for_cell(
                cell,
                incarnation,
                segment.info().clone(),
                std::fs::read(segment.path()).unwrap(),
            )
        })
        .collect();
    let bundle = Bundle::encode(entries, Limits::default()).unwrap();
    let overlay = RecoveryOverlay::new(base, bundle, tail.position, 2);
    let recovered = replica
        .prepare_recovered_overlay(&overlay, 6)
        .await
        .unwrap();

    assert_eq!(recovered.predecessor(), Some(base));
    assert_eq!(recovered.root().position, tail.position);
    assert_eq!(recovered.root().commit_sequence, 2);

    let invalid = RecoveryOverlay::new(
        RootRef {
            cell: [83; 32],
            ..base
        },
        Bundle::encode(
            tail.segments
                .iter()
                .map(|segment| {
                    BundleEntry::for_cell(
                        cell,
                        incarnation,
                        segment.info().clone(),
                        std::fs::read(segment.path()).unwrap(),
                    )
                })
                .collect(),
            Limits::default(),
        )
        .unwrap(),
        tail.position,
        2,
    );
    assert!(
        replica
            .prepare_recovered_overlay(&invalid, 6)
            .await
            .is_err()
    );
    writer.close().unwrap();
}
