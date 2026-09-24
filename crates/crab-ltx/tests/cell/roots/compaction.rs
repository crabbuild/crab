//! Scheduled compaction, large-frame streaming, and corruption refusal.

use super::*;

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
    let scratch_slots = Arc::new(tokio::sync::Semaphore::new(64));
    let limited = replica
        .clone()
        .with_host(Host::default().with_scratch_slots(scratch_slots.clone()));
    assert!(matches!(
        limited
            .prepare_scheduled_compaction(&root, scratch.path())
            .await,
        Err(crab_ltx::CrabError::Limit(
            crab_ltx::LimitKind::ScratchDiskBytes
        ))
    ));
    assert_eq!(scratch_slots.available_permits(), 64);
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
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
