//! Scheduled compaction, large-frame streaming, and corruption refusal.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn range_compaction_reads_and_reserves_only_the_selected_data() {
    verify_range_compaction(Arc::new(InMemory::new()), Path::from("runtime")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the RustFS environment documented in README.md"]
async fn rustfs_range_compaction_preserves_exact_native_and_bundled_roots() {
    let bucket = std::env::var("CRAB_LTX_TEST_BUCKET").unwrap();
    let endpoint = std::env::var("CRAB_LTX_TEST_ENDPOINT").unwrap();
    let store = crab_storage::build_explicit_store(
        &bucket,
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
    verify_range_compaction(
        store.inner().clone(),
        Path::from(format!(
            "crab-ltx-tests/range-compaction/{run}-{}",
            std::process::id()
        )),
    )
    .await;
}

async fn verify_range_compaction(backend: Arc<dyn object_store::ObjectStore>, prefix: Path) {
    for (page_size, rows, payload) in [(4096, 4096, 3000), (512, 70000, 400)] {
        let prefix = prefix.clone().join(page_size.to_string());
        let source = tempfile::TempDir::new().unwrap();
        let path = source.path().join("range.sqlite");
        let initial = crab_ltx::rusqlite::Connection::open(&path).unwrap();
        initial.execute_batch(&format!("PRAGMA page_size={page_size}; CREATE TABLE t(k INTEGER PRIMARY KEY, v BLOB NOT NULL)")).unwrap();
        drop(initial);
        let mut writer = Db::open(&path, Limits::default()).unwrap();
        writer
            .transaction(|tx| {
                tx.execute(
                    "WITH RECURSIVE n(k) AS (VALUES(1) UNION ALL SELECT k+1 FROM n WHERE k<?1) \
                 INSERT INTO t SELECT k, zeroblob(?2) FROM n",
                    (rows, payload),
                )?;
                Ok(())
            })
            .unwrap();
        let read_bytes = Arc::new(AtomicU64::new(0));
        let observed = read_bytes.clone();
        let store = Store::new(backend.clone()).with_read_byte_observer(Arc::new(move |bytes| {
            observed.fetch_add(bytes, Ordering::Relaxed);
        }));
        let replica = CellReplica::new(
            CellStorageLayout::new(store, prefix.clone(), [3; 16]),
            [233; 32],
            [234; 16],
            Limits::default(),
        )
        .unwrap();
        let first = writer.capture().unwrap();
        let mut root = replica.prepare(None, &first, 1, 1).await.unwrap().root();
        let start = replica.open_root(&root).await.unwrap().segment_count();
        let mut end = start;
        let mut segments = first.segments;
        // The later overwrite must remain newer than the selected pair of cuts.
        for (sequence, key, value) in [(2, 1, 11), (3, rows, 22), (4, 1, 33)] {
            writer
                .transaction(|tx| {
                    tx.execute(
                        "UPDATE t SET v=?1 WHERE k=?2",
                        (vec![value; payload as usize], key),
                    )?;
                    Ok(())
                })
                .unwrap();
            let captured = writer.capture().unwrap();
            root = replica
                .prepare(Some(&root), &captured, sequence, 1)
                .await
                .unwrap()
                .root();
            segments.extend(captured.segments);
            if sequence == 3 {
                end = replica.open_root(&root).await.unwrap().segment_count();
            }
        }
        writer.close().unwrap();
        let bundle = Bundle::encode(
            segments
                .iter()
                .map(|segment| {
                    BundleEntry::for_cell(
                        [233; 32],
                        [234; 16],
                        segment.info().clone(),
                        std::fs::read(segment.path()).unwrap(),
                    )
                })
                .collect(),
            Limits::default(),
        )
        .unwrap();
        let bundled = replica
            .prepare_bundle(None, &bundle, 4, 1)
            .await
            .unwrap()
            .root();
        let scratch = tempfile::TempDir::new().unwrap();
        let original = scratch.path().join("original.sqlite");
        replica
            .open_root(&root)
            .await
            .unwrap()
            .restore(&original)
            .await
            .unwrap();
        let expected = std::fs::read(original).unwrap();
        assert!(expected.len() > 16 << 20);

        for (representation, root) in [("native", root), ("bundle", bundled)] {
            let scratch_mib = 1;
            let slots = Arc::new(tokio::sync::Semaphore::new(scratch_mib));
            let observed = read_bytes.clone();
            let cold = CellReplica::new(
                CellStorageLayout::new(
                    Store::new(backend.clone()).with_read_byte_observer(Arc::new(move |bytes| {
                        observed.fetch_add(bytes, Ordering::Relaxed);
                    })),
                    prefix.clone(),
                    [3; 16],
                ),
                [233; 32],
                [234; 16],
                Limits::default(),
            )
            .unwrap();
            let limited = cold.with_host(Host::default().with_scratch_slots(slots.clone()));
            read_bytes.store(0, Ordering::Relaxed);
            let compacted = limited
                .prepare_compaction(&root, start..end, 1, scratch.path())
                .await
                .unwrap();
            let cost = limited.take_publication_cost();
            assert!(
                cost.objects <= 10,
                "range compaction rewrote {} objects",
                cost.objects
            );
            assert!(
                read_bytes.load(Ordering::Relaxed) < 128 << 10,
                "a tiny range downloaded {} bytes of unrelated metadata",
                read_bytes.load(Ordering::Relaxed)
            );
            assert_eq!(slots.available_permits(), scratch_mib);
            eprintln!(
                "range compaction: {page_size}-byte pages, {representation}, {} read bytes, {} uploaded objects, {scratch_mib} MiB scratch available",
                read_bytes.load(Ordering::Relaxed),
                cost.objects,
            );
            assert_eq!(compacted.predecessor(), Some(root));
            assert_eq!(compacted.root().position, root.position);
            assert_eq!(compacted.root().commit_sequence, root.commit_sequence);
            let destination = scratch
                .path()
                .join(format!("range-{representation}.sqlite"));
            replica
                .open_root(&compacted.root())
                .await
                .unwrap()
                .restore(&destination)
                .await
                .unwrap();
            assert_eq!(std::fs::read(destination).unwrap(), expected);
        }
    }
}

#[tokio::test]
async fn compaction_scratch_exhaustion_refuses_cleanly() {
    let source = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&source.path().join("scratch.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction
                .execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(1048576))")
        })
        .unwrap();
    let batch = writer.capture().unwrap();
    let replica = replica(Store::new(Arc::new(InMemory::new())), [231; 32], [232; 16]);
    let root = replica.prepare(None, &batch, 1, 1).await.unwrap().root();

    // One scratch MiB cannot hold this input and its output, so the request
    // must be refused as capacity instead of starting work it cannot finish.
    let constrained = replica
        .clone()
        .with_host(Host::default().with_scratch_slots(Arc::new(tokio::sync::Semaphore::new(1))));
    let scratch = tempfile::TempDir::new().unwrap();
    let error = match constrained
        .prepare_compaction(&root, 0..1, 9, scratch.path())
        .await
    {
        Ok(_) => panic!("an unadmitted compaction must not return a proposal"),
        Err(error) => error,
    };
    assert_eq!(error.classify(), crab_ltx::FailureClass::Capacity);
    assert_eq!(
        std::fs::read_dir(scratch.path()).unwrap().count(),
        0,
        "a refused compaction must not leave scratch files"
    );

    // The pinned root is untouched and still verifies.
    let verified = replica.open_root(&root).await.unwrap();
    assert_eq!(verified.root().position, batch.position);

    // The same request succeeds once the host can admit the scratch it needs.
    let compacted = replica
        .prepare_compaction(&root, 0..1, 9, scratch.path())
        .await
        .unwrap();
    assert_eq!(compacted.root().position, batch.position);
    writer.close().unwrap();
}

#[tokio::test]
async fn scheduled_cell_compaction_promotes_fanout_and_preserves_root() {
    let source = tempfile::TempDir::new().unwrap();
    let database = source.path().join("scheduled.sqlite");
    let mut writer = Db::open(&database, Limits::default()).unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let replica = replica(store, [41; 32], [42; 16]);
    let mut root = None;
    for sequence in 1_u64..=8 {
        writer
            .transaction(|transaction| {
                if sequence == 1 {
                    transaction.execute_batch(
                        "CREATE TABLE events(sequence INTEGER PRIMARY KEY, value TEXT NOT NULL)",
                    )?;
                }
                transaction.execute(
                    "INSERT INTO events(sequence, value) VALUES (?1, ?2)",
                    (sequence, format!("event-{sequence}")),
                )?;
                Ok(())
            })
            .unwrap();
        let batch = writer.capture().unwrap();
        root = Some(
            replica
                .prepare(root.as_ref(), &batch, sequence, 1)
                .await
                .unwrap()
                .root(),
        );
    }
    writer.close().unwrap();
    let root = root.unwrap();
    assert_eq!(replica.open_root(&root).await.unwrap().segment_count(), 8);

    let scratch = tempfile::TempDir::new().unwrap();
    let compacted = replica
        .prepare_scheduled_compaction(&root, scratch.path())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(compacted.predecessor(), Some(root));
    assert_eq!(compacted.root().position, root.position);
    assert_eq!(compacted.root().commit_sequence, root.commit_sequence);
    assert_eq!(compacted.verified().segment_count(), 1);
    assert!(
        replica
            .prepare_scheduled_compaction(&compacted.root(), scratch.path())
            .await
            .unwrap()
            .is_none()
    );

    let restored = tempfile::TempDir::new().unwrap();
    let before = restored.path().join("before.sqlite");
    let after = restored.path().join("after.sqlite");
    replica
        .open_root(&root)
        .await
        .unwrap()
        .restore(&before)
        .await
        .unwrap();
    replica
        .open_root(&compacted.root())
        .await
        .unwrap()
        .restore(&after)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(before).unwrap(),
        std::fs::read(after).unwrap()
    );
}
#[tokio::test(flavor = "multi_thread")]
async fn compaction_streams_large_frames_and_cleans_scratch() {
    let source = tempfile::TempDir::new().unwrap();
    let database = source.path().join("large.sqlite");
    let mut writer = Db::open(&database, Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(3000000))",
            )
        })
        .unwrap();
    let maximum_read = Arc::new(AtomicU64::new(0));
    let total_read = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&maximum_read);
    let observed_total = Arc::clone(&total_read);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |bytes| {
            observed.fetch_max(bytes, Ordering::SeqCst);
            observed_total.fetch_add(bytes, Ordering::SeqCst);
        }));
    let replica = replica(store, [91; 32], [92; 16]);
    let cuts = writer.capture().unwrap();
    let source_bytes = cuts
        .segments
        .iter()
        .map(|segment| segment.info().size_bytes)
        .sum::<u64>();
    let root = replica.prepare(None, &cuts, 1, 1).await.unwrap().root();
    writer.close().unwrap();
    let scratch = tempfile::TempDir::new().unwrap();
    maximum_read.store(0, Ordering::SeqCst);
    total_read.store(0, Ordering::SeqCst);

    let compacted = replica
        .prepare_compaction(&root, 0..1, 9, scratch.path())
        .await
        .unwrap();

    assert_eq!(compacted.root().position, root.position);
    assert!(
        maximum_read.load(Ordering::SeqCst) <= 1 << 20,
        "compaction must not download the complete LTX body"
    );
    assert!(
        total_read.load(Ordering::SeqCst) <= source_bytes.saturating_add(2 << 20),
        "compaction must fetch each selected body only once"
    );
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    let restored = scratch.path().join("restored.sqlite");
    compacted.verified().restore(&restored).await.unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT length(value) FROM payload", [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap(),
        3_000_000
    );
}
#[tokio::test]
async fn compaction_rejects_selected_body_corruption_outside_page_frames() {
    let source = tempfile::TempDir::new().unwrap();
    let database = source.path().join("corrupt.sqlite");
    let mut writer = Db::open(&database, Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE messages(body TEXT NOT NULL);\
                 INSERT INTO messages VALUES ('verified')",
            )
        })
        .unwrap();
    let batch = writer.capture().unwrap();
    let info = batch.segments[0].info().clone();
    let mut corrupted = std::fs::read(batch.segments[0].path()).unwrap();
    corrupted[0] ^= 1;
    let inner = Arc::new(InMemory::new());
    let store = Store::new(inner.clone());
    let cell = [93; 32];
    let incarnation = [94; 16];
    let layout = CellStorageLayout::new(store.clone(), Path::from("runtime"), [3; 16]);
    let replica = CellReplica::new(layout.clone(), cell, incarnation, Limits::default()).unwrap();
    let root = replica.prepare(None, &batch, 1, 1).await.unwrap().root();
    writer.close().unwrap();
    let object =
        layout.incarnation_object_path(&cell, &incarnation, &info.blake3, CellObjectKind::Ltx);
    inner
        .put(&object, Bytes::from(corrupted).into())
        .await
        .unwrap();
    let scratch = tempfile::TempDir::new().unwrap();

    let error = match replica
        .prepare_compaction(&root, 0..1, 9, scratch.path())
        .await
    {
        Ok(_) => panic!("corrupt selected LTX must not compact"),
        Err(error) => error,
    };

    assert!(matches!(error, crab_ltx::CrabError::ChecksumMismatch));
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
}
