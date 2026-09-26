//! Small-body transport selection, exact restore, and ambiguous upload retries.

use super::*;

#[tokio::test]
async fn native_compacted_and_bundled_uploads_use_single_put_only_for_small_bodies() {
    for (payload, expected_multipart) in [(4096, 0), (300_000, 1)] {
        let directory = tempfile::TempDir::new().unwrap();
        let mut writer = Db::open(&directory.path().join("source"), Limits::default()).unwrap();
        writer
            .transaction(|tx| {
                tx.execute_batch("CREATE TABLE t(value BLOB)")?;
                tx.execute("INSERT INTO t VALUES(randomblob(?1))", [payload])?;
                Ok(())
            })
            .unwrap();
        let batch = writer.capture_deferred().unwrap();
        let store = InstrumentedStore::new(Arc::new(InMemory::new()), Duration::ZERO);
        let replica = cell_replica(
            Store::new(store.clone()),
            [151; 32],
            [152; 16],
            Host::default(),
        );
        let prepared = replica.prepare(None, &batch, 1, 1).await.unwrap();
        assert_eq!(store.multipart.load(Ordering::SeqCst), expected_multipart);
        let cost = replica.take_publication_cost();
        assert_eq!(
            store.puts.load(Ordering::SeqCst) + expected_multipart,
            cost.objects as usize
        );

        let compacted = replica
            .prepare_compaction(
                &prepared.root(),
                0..prepared.verified().segment_count(),
                9,
                directory.path(),
            )
            .await
            .unwrap();
        assert_eq!(
            store.multipart.load(Ordering::SeqCst),
            expected_multipart * 2
        );
        let before = directory.path().join("before");
        let after = directory.path().join("after");
        prepared.verified().restore(&before).await.unwrap();
        compacted.verified().restore(&after).await.unwrap();
        assert_eq!(
            std::fs::read(before).unwrap(),
            std::fs::read(&after).unwrap()
        );
        let bundle = crab_ltx::bundle::Bundle::encode(
            batch
                .segments
                .iter()
                .map(|segment| {
                    crab_ltx::bundle::BundleEntry::for_cell(
                        [151; 32],
                        [152; 16],
                        segment.info().clone(),
                        std::fs::read(segment.path()).unwrap(),
                    )
                })
                .collect(),
            Limits::default(),
        )
        .unwrap();
        let bundled = replica.prepare_bundle(None, &bundle, 1, 1).await.unwrap();
        assert_eq!(
            store.multipart.load(Ordering::SeqCst),
            expected_multipart * 3
        );
        let bundle_restore = directory.path().join("bundle");
        bundled.verified().restore(&bundle_restore).await.unwrap();
        assert_eq!(
            std::fs::read(bundle_restore).unwrap(),
            std::fs::read(after).unwrap()
        );
        writer.close().unwrap();
    }
}

#[tokio::test]
async fn small_body_upload_reconciles_response_loss_without_changing_the_root() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("source"), Limits::default()).unwrap();
    writer
        .transaction(|tx| tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(7)"))
        .unwrap();
    let batch = writer.capture_deferred().unwrap();
    let store = InstrumentedStore::new(Arc::new(InMemory::new()), Duration::ZERO);
    store.arm(PUT_RESPONSE_LOST);
    let replica = cell_replica(
        Store::new(store.clone()),
        [153; 32],
        [154; 16],
        Host::default(),
    );
    let prepared = replica.prepare(None, &batch, 1, 1).await.unwrap();
    assert_eq!(store.fault.load(Ordering::SeqCst), NO_FAULT);
    assert_eq!(store.multipart.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.puts.load(Ordering::SeqCst) as u64,
        replica.take_publication_cost().objects + 1
    );
    let restored = directory.path().join("restored");
    prepared.verified().restore(&restored).await.unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT v FROM t", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        7
    );
    writer.close().unwrap();
}
