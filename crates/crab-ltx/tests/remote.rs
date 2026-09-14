#![cfg(feature = "replica")]

use crab_ltx::{CaptureBatch, CrabError, Limits, ManagedDb, Replica};
use crab_storage::{StorageError, Store, StoreLayout};
use object_store::{memory::InMemory, path::Path};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tempfile::TempDir;

fn replica(store: Store) -> Replica {
    Replica::new(
        StoreLayout::new(store, "test-repository".into()),
        "epoch-1",
        Limits::default(),
    )
    .unwrap()
}

fn history() -> (TempDir, Vec<CaptureBatch>) {
    let directory = TempDir::new().unwrap();
    let mut db =
        ManagedDb::open(&directory.path().join("source.sqlite"), Limits::default()).unwrap();
    let mut batches = Vec::new();
    for value in ["first", "second", "🦀 中文"] {
        db.transaction(|tx| {
            tx.execute_batch("CREATE TABLE IF NOT EXISTS messages(id INTEGER PRIMARY KEY, body TEXT, payload BLOB)")?;
            tx.execute("INSERT INTO messages(body, payload) VALUES (?1, zeroblob(20000))", [value])?;
            Ok(())
        }).unwrap();
        batches.push(db.capture().unwrap());
    }
    db.close().unwrap();
    (directory, batches)
}

#[tokio::test(flavor = "multi_thread")]
async fn inherited_bundles_sparse_writes_and_incremental_compaction_preserve_sql() {
    parity_roundtrip(Store::new(Arc::new(InMemory::new()))).await;
}

async fn parity_roundtrip(store: Store) {
    let layout = StoreLayout::new(store, "parity-repository".into());
    let remote = Replica::new(layout.clone(), "old", Limits::default()).unwrap();
    let source = TempDir::new().unwrap();
    let path = source.path().join("source.sqlite");
    let connection = crab_ltx::rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch("PRAGMA auto_vacuum=FULL; VACUUM")
        .unwrap();
    drop(connection);
    let mut writer = ManagedDb::open(&path, Limits::default()).unwrap();
    writer.transaction(|tx| {
        tx.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, data BLOB); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<200) INSERT INTO items SELECT i, randomblob(8000) FROM n")
    }).unwrap();
    let captured = writer.capture().unwrap();
    let entries = captured
        .segments
        .iter()
        .map(|segment| crab_ltx::bundle::BundleEntry {
            repository: "parity-repository".into(),
            epoch: "old".into(),
            info: segment.info().clone(),
            bytes: std::fs::read(segment.path()).unwrap(),
        })
        .collect();
    let bundle = crab_ltx::bundle::Bundle::encode(entries, Limits::default()).unwrap();
    let head = remote.replicate_bundle(&bundle, None).await.unwrap();
    let bundled = remote.bundle(&head).await.unwrap();
    let next = Replica::new(layout, "next", Limits::default()).unwrap();
    let inherited = next.inherit(&remote, &bundled).await.unwrap();
    // A late old owner can advance only its old epoch, not the pinned inheritance.
    writer
        .transaction(|tx| tx.execute_batch("DELETE FROM items"))
        .unwrap();
    remote
        .replicate(&writer.capture().unwrap(), Some(&bundled))
        .await
        .unwrap();
    writer.close().unwrap();
    source.close().unwrap();

    let local = TempDir::new().unwrap();
    let destination = local.path().join("sparse.sqlite");
    let paged = next.paged(&inherited).await.unwrap();
    let (mut writer, initial, first) = tokio::task::spawn_blocking(move || {
        let mut writer = paged.open_writable(&destination).unwrap();
        let initial = writer.hydration().unwrap().unwrap();
        assert!(
            !initial.complete(),
            "activation must not download the full database"
        );
        assert_eq!(writer.position(), inherited.position());
        writer
            .transaction(|tx| {
                assert_eq!(
                    tx.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, u32>(0))?,
                    200
                );
                tx.execute(
                    "UPDATE items SET data=?1 WHERE id=1",
                    [b"new committed bytes".as_slice()],
                )?;
                Ok(())
            })
            .unwrap();
        let first = writer.capture().unwrap();
        (writer, initial, first)
    })
    .await
    .unwrap();
    let inherited = next.head().await.unwrap().unwrap();
    assert_eq!(
        first.segments[0].info().min_txid,
        inherited.position().txid + 1
    );
    let mut head = next.replicate(&first, Some(&inherited)).await.unwrap();
    assert_eq!(writer.prune_published(&head).unwrap(), first.segments.len());
    assert_eq!(writer.prune_published(&head).unwrap(), 0);
    for mode in [
        crab_ltx::CheckpointMode::Passive,
        crab_ltx::CheckpointMode::Full,
        crab_ltx::CheckpointMode::Restart,
        crab_ltx::CheckpointMode::Truncate,
    ] {
        writer
            .transaction(|tx| tx.execute_batch("INSERT INTO items(data) VALUES(randomblob(10000))"))
            .unwrap();
        let batch = writer.checkpoint(mode).unwrap();
        head = next.replicate(&batch, Some(&head)).await.unwrap();
    }
    for sql in [
        "DELETE FROM items WHERE id>1",
        "WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i+1 FROM n WHERE i<150) INSERT INTO items SELECT i, randomblob(8000) FROM n",
    ] {
        writer.transaction(|tx| tx.execute_batch(sql)).unwrap();
        let batch = writer
            .checkpoint(crab_ltx::CheckpointMode::Truncate)
            .unwrap();
        head = next.replicate(&batch, Some(&head)).await.unwrap();
    }
    let before = local.path().join("before.sqlite");
    next.restore(&head, &before).await.unwrap();
    let partial = next
        .compact_range(&head, 1..head.segment_count(), 1)
        .await
        .unwrap();
    let after = local.path().join("after.sqlite");
    next.restore(&partial, &after).await.unwrap();
    assert_eq!(
        std::fs::read(before).unwrap(),
        std::fs::read(after).unwrap()
    );
    let bundled = next.bundle(&partial).await.unwrap();
    let output = local.path().join("bundle.sqlite");
    next.restore(&bundled, &output).await.unwrap();
    let sql = crab_ltx::rusqlite::Connection::open(output).unwrap();
    assert_eq!(
        sql.query_row("SELECT data FROM items WHERE id=1", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        b"new committed bytes"
    );
    assert_eq!(
        sql.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    while !writer.hydration().unwrap().unwrap().complete() {
        writer.hydrate_step(17).unwrap();
    }
    assert!(writer.hydration().unwrap().unwrap().resolved > initial.resolved);
    writer.close().unwrap();
    let recovered = local.path().join("resume.sqlite");
    let mut resumed = next.resume(&bundled, &recovered).await.unwrap();
    resumed
        .transaction(|tx| tx.execute_batch("INSERT INTO items(data) VALUES('resumed')"))
        .unwrap();
    let final_head = next
        .replicate(&resumed.capture().unwrap(), Some(&bundled))
        .await
        .unwrap();
    assert!(final_head.position().txid > bundled.position().txid);
    resumed.close().unwrap();
    println!(
        "Inherited bundle → sparse SQL write → checkpoint → partial compaction → exact resume verified"
    );
}

async fn roundtrip(store: Store) {
    let remote = replica(store);
    let (source, batches) = tokio::task::spawn_blocking(history).await.unwrap();
    let mut head = remote.replicate(&batches[0], None).await.unwrap();
    let original_head = head.clone();
    for batch in &batches[1..] {
        head = remote.replicate(batch, Some(&head)).await.unwrap();
    }
    source.close().unwrap();
    let destination = TempDir::new().unwrap();
    let original = destination.path().join("original.sqlite");
    remote.restore(&head, &original).await.unwrap();
    let bytes = std::fs::read(&original).unwrap();
    let paged = remote.paged(&head).await.unwrap();
    let mut from_pages = Vec::new();
    for page in 1..=paged.page_count() {
        from_pages.extend(paged.read_page(page).await.unwrap());
    }
    assert_eq!(
        from_pages, bytes,
        "ranged reconstruction must be byte-identical"
    );
    let witness = tokio::task::spawn_blocking(move || {
        let sql = paged.open_sqlite().unwrap();
        let connection = sql.connection();
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        assert!(
            connection
                .execute("INSERT INTO messages(body) VALUES ('forbidden')", [])
                .is_err()
        );
        assert!(
            connection
                .execute_batch("ATTACH 'another-cut' AS other")
                .is_err()
        );
        connection
            .query_row("SELECT group_concat(body, ',') FROM messages", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(witness, "first,second,🦀 中文");
    let compacted = remote.compact(&head).await.unwrap();
    assert_eq!(compacted.segment_count(), 1);
    let compact_path = destination.path().join("compact.sqlite");
    remote.restore(&compacted, &compact_path).await.unwrap();
    assert_eq!(std::fs::read(compact_path).unwrap(), bytes);
    // Retention is external: old exact heads remain restorable after compaction.
    let old_path = destination.path().join("old.sqlite");
    let historical = remote
        .open_exact(original_head.manifest_digest())
        .await
        .unwrap();
    remote.restore(&historical, &old_path).await.unwrap();
    assert!(
        remote.compact(&historical).await.is_err(),
        "historical views cannot mutate the head"
    );
    let count: u32 = crab_ltx::rusqlite::Connection::open(old_path)
        .unwrap()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        remote.head().await.unwrap().unwrap().position(),
        compacted.position()
    );
    println!("Restored and paged SQL witness: {witness}; compacted to one immutable snapshot");
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_publication_paged_sql_and_compaction_roundtrip() {
    roundtrip(Store::new(Arc::new(InMemory::new()))).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_one_concurrent_head_update_wins_and_stale_compaction_cannot_rewind() {
    cas_race(replica(Store::new(Arc::new(InMemory::new())))).await;
}

async fn cas_race(remote: Replica) {
    let (_source, batches) = tokio::task::spawn_blocking(history).await.unwrap();
    let head = remote.replicate(&batches[0], None).await.unwrap();
    let (a, b) = tokio::join!(
        remote.replicate(&batches[1], Some(&head)),
        remote.replicate(&batches[1], Some(&head))
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let loser = if a.is_err() {
        a.err().unwrap()
    } else {
        b.err().unwrap()
    };
    assert!(matches!(
        loser,
        CrabError::Storage(StorageError::StateConflict { .. })
    ));
    assert!(matches!(
        remote.compact(&head).await,
        Err(CrabError::Storage(StorageError::StateConflict { .. }))
    ));
    assert_eq!(
        remote.head().await.unwrap().unwrap().position(),
        batches[1].position
    );
}

#[tokio::test]
async fn invalid_capture_never_advances_the_remote_head() {
    let remote = replica(Store::new(Arc::new(InMemory::new())));
    let (_source, mut batches) = tokio::task::spawn_blocking(history).await.unwrap();
    let head = remote.replicate(&batches[0], None).await.unwrap();
    batches[1].position.checksum ^= 1;
    assert!(matches!(
        remote.replicate(&batches[1], Some(&head)).await,
        Err(CrabError::ChecksumMismatch)
    ));
    assert_eq!(
        remote.head().await.unwrap().unwrap().position(),
        head.position()
    );
    assert!(
        remote.replicate(&batches[2], Some(&head)).await.is_err(),
        "gaps must be rejected"
    );
}

#[tokio::test]
async fn paged_construction_fetches_indexes_only_and_corrupt_frames_fail_closed() {
    let fetched = Arc::new(AtomicU64::new(0));
    let counter = fetched.clone();
    let store = Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |n| {
        counter.fetch_add(n, Ordering::Relaxed);
    }));
    let remote = replica(store.clone());
    let (_source, batches) = tokio::task::spawn_blocking(history).await.unwrap();
    let head = remote.replicate(&batches[0], None).await.unwrap();
    let head_key = Path::from("test-repository/ltx/epoch-1/head.json");
    let (body, _) = store.get_with_etag(&head_key).await.unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let index_bytes: u64 = manifest["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["index_size"].as_u64().unwrap())
        .sum();
    fetched.store(0, Ordering::Relaxed);
    let paged = remote.paged(&head).await.unwrap();
    assert_eq!(fetched.load(Ordering::Relaxed), index_bytes);
    let info = &batches[0].segments.last().unwrap().info();
    let digest = blake3::Hash::from_bytes(info.blake3).to_hex();
    let key = Path::from(format!("test-repository/ltx/epoch-1/objects/{digest}.ltx"));
    let (body, _) = store.get_with_etag(&key).await.unwrap();
    let mut corrupt = body.to_vec();
    corrupt[110] ^= 1;
    store.put_overwrite(&key, corrupt.into()).await.unwrap();
    assert!(matches!(
        paged.read_page(1).await,
        Err(CrabError::ChecksumMismatch)
    ));
    let dest = TempDir::new().unwrap();
    assert!(
        remote
            .restore(&head, &dest.path().join("bad.sqlite"))
            .await
            .is_err()
    );
    assert!(!dest.path().join("bad.sqlite").exists());
}

#[tokio::test]
async fn filesystem_create_restore_and_paging_work_but_unsupported_cas_fails_closed() {
    let root = TempDir::new().unwrap();
    let store = Store::new(Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(root.path()).unwrap(),
    ));
    let remote = replica(store);
    let (source, batches) = tokio::task::spawn_blocking(history).await.unwrap();
    let head = remote.replicate(&batches[0], None).await.unwrap();
    assert!(remote.replicate(&batches[1], Some(&head)).await.is_err());
    source.close().unwrap();
    let paged = remote.paged(&head).await.unwrap();
    let count = tokio::task::spawn_blocking(move || {
        let sql = paged.open_sqlite().unwrap();
        sql.connection()
            .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, u32>(0))
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn corrupt_indexes_and_invalid_heads_are_rejected_without_latest_fallback() {
    let store = Store::new(Arc::new(InMemory::new()));
    let remote = replica(store.clone());
    let (_source, batches) = tokio::task::spawn_blocking(history).await.unwrap();
    let head = remote.replicate(&batches[0], None).await.unwrap();
    let key = Path::from("test-repository/ltx/epoch-1/head.json");
    let (body, _) = store.get_with_etag(&key).await.unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let hash: [u8; 32] =
        serde_json::from_value(manifest["segments"][0]["index_hash"].clone()).unwrap();
    let hash = blake3::Hash::from_bytes(hash).to_hex();
    let index_key = Path::from(format!("test-repository/ltx/epoch-1/objects/{hash}.idx"));
    let (index, _) = store.get_with_etag(&index_key).await.unwrap();
    let mut changed = index.to_vec();
    changed[0] ^= 1;
    store
        .put_overwrite(&index_key, changed.into())
        .await
        .unwrap();
    assert!(matches!(
        remote.paged(&head).await,
        Err(CrabError::ChecksumMismatch)
    ));
    store
        .put_overwrite(&key, bytes::Bytes::from_static(b"{}"))
        .await
        .unwrap();
    assert!(remote.head().await.is_err());
    // An external pin remains usable even when a mutable head is corrupt.
    let exact = remote.open_exact(head.manifest_digest()).await.unwrap();
    let destination = TempDir::new().unwrap();
    remote
        .restore(&exact, &destination.path().join("exact.sqlite"))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn paged_shrink_regrowth_and_sql_work_at_different_page_sizes() {
    for size in [512, 4096, 65536] {
        let source = TempDir::new().unwrap();
        let path = source.path().join("source.sqlite");
        let connection = crab_ltx::rusqlite::Connection::open(&path).unwrap();
        connection.pragma_update(None, "page_size", size).unwrap();
        connection
            .pragma_update(None, "auto_vacuum", "FULL")
            .unwrap();
        drop(connection);
        let mut db = ManagedDb::open(&path, Limits::default()).unwrap();
        let remote = replica(Store::new(Arc::new(InMemory::new())));
        let mut head = None;
        for sql in [
            "CREATE TABLE blobs(data BLOB); INSERT INTO blobs VALUES(zeroblob(200000))",
            "DELETE FROM blobs",
            "INSERT INTO blobs VALUES(randomblob(100000))",
        ] {
            db.transaction(|tx| tx.execute_batch(sql)).unwrap();
            let batch = db.capture().unwrap();
            head = Some(remote.replicate(&batch, head.as_ref()).await.unwrap());
            let view = remote.paged(head.as_ref().unwrap()).await.unwrap();
            assert_eq!(
                view.page_size(),
                size,
                "fixture must persist its requested page size"
            );
            let integrity = tokio::task::spawn_blocking(move || {
                let sql = view.open_sqlite().unwrap();
                sql.connection()
                    .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                    .unwrap()
            })
            .await
            .unwrap();
            assert_eq!(integrity, "ok", "SQLite page size {size}");
        }
        db.close().unwrap();
    }
}

#[tokio::test]
async fn vfs_short_reads_zero_fill_and_preserve_cross_page_bytes() {
    let remote = replica(Store::new(Arc::new(InMemory::new())));
    let (_source, batches) = tokio::task::spawn_blocking(history).await.unwrap();
    let head = remote.replicate(&batches[0], None).await.unwrap();
    let view = remote.paged(&head).await.unwrap();
    let size = view.page_size() as usize;
    let count = view.page_count() as usize;
    let first = view.read_page(1).await.unwrap();
    let second = view.read_page(2).await.unwrap();
    tokio::task::spawn_blocking(move || {
        use crab_ltx::rusqlite::ffi;
        let sql = view.open_sqlite().unwrap();
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        let mut output = [0xffu8; 20];
        // SAFETY: the connection/file remain open and each buffer is sized for
        // its xRead invocation. SQLite owns the returned file pointer.
        unsafe {
            assert_eq!(
                ffi::sqlite3_file_control(
                    sql.connection().handle(),
                    c"main".as_ptr(),
                    ffi::SQLITE_FCNTL_FILE_POINTER,
                    (&mut file as *mut *mut ffi::sqlite3_file).cast()
                ),
                ffi::SQLITE_OK
            );
            assert!(!file.is_null());
            let read = (*(*file).pMethods).xRead.unwrap();
            assert_eq!(
                read(file, output.as_mut_ptr().cast(), 20, (size - 10) as i64),
                ffi::SQLITE_OK
            );
            assert_eq!(&output[..10], &first[size - 10..]);
            assert_eq!(&output[10..], &second[..10]);
            assert_eq!(
                read(file, output.as_mut_ptr().cast(), 20, (size * count) as i64),
                ffi::SQLITE_IOERR_SHORT_READ
            );
            assert_eq!(output, [0; 20]);
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket and explicit test credentials"]
async fn rustfs_roundtrip() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let store = crab_storage::build_explicit_store(
        &required("CRAB_LTX_TEST_BUCKET"),
        crab_storage::ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_LTX_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    roundtrip(store.clone()).await;
    parity_roundtrip(store.clone()).await;
    let race = Replica::new(
        StoreLayout::new(store, "race-repository".into()),
        "epoch-1",
        Limits::default(),
    )
    .unwrap();
    cas_race(race).await;
}

#[tokio::test]
async fn independently_opened_sqlite_file_outlives_view_registration_safely() {
    let remote = replica(Store::new(Arc::new(InMemory::new())));
    let (_source, batches) = tokio::task::spawn_blocking(history).await.unwrap();
    let head = remote.replicate(&batches[0], None).await.unwrap();
    let view = remote.paged(&head).await.unwrap();
    tokio::task::spawn_blocking(move || {
        use crab_ltx::rusqlite::{Connection, OpenFlags};
        let sql = view.open_sqlite().unwrap();
        let path = sql.connection().path().unwrap().to_owned();
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY;
        let independent =
            Connection::open_with_flags_and_vfs(&path, flags, "crab-ltx-readonly-v1").unwrap();
        drop(sql);
        assert!(
            Connection::open_with_flags_and_vfs(&path, flags, "crab-ltx-readonly-v1").is_err(),
            "retired views cannot be newly opened"
        );
        let count: u32 = independent
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        independent.close().unwrap();
    })
    .await
    .unwrap();
}
