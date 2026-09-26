//! Directory growth, sharing, cache survival, and truncate/regrow fencing.

use super::*;

#[tokio::test]
async fn changed_cut_loads_only_touched_directory_nodes() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE counter(value INTEGER NOT NULL);\
                 INSERT INTO counter VALUES (0);\
                 CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(20000000))",
            )
        })
        .unwrap();
    let read_bytes = Arc::new(AtomicU64::new(0));
    let observed = read_bytes.clone();
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |bytes| {
            observed.fetch_add(bytes, Ordering::SeqCst);
        }));
    let replica = replica(store, [41; 32], [42; 16]);
    let first = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();

    writer
        .transaction(|transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(())
        })
        .unwrap();
    let next = writer.capture().unwrap();
    read_bytes.store(0, Ordering::SeqCst);
    let second = replica.prepare(Some(&first), &next, 2, 1).await.unwrap();

    assert!(
        read_bytes.load(Ordering::SeqCst) < 100_000,
        "an incremental root must not reload the full 20 MB snapshot index"
    );
    assert_eq!(second.root().position, next.position);
    writer.close().unwrap();
}
#[tokio::test]
async fn directory_nodes_are_shared_across_exact_root_views() {
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
    let metadata_reads = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&metadata_reads);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |kind| {
            if kind == StorageReadKind::Get {
                observed.fetch_add(1, Ordering::SeqCst);
            }
        }));
    let replica = replica(store, [81; 32], [82; 16]);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();

    metadata_reads.store(0, Ordering::SeqCst);
    let first = replica.open_root(&root).await.unwrap();
    assert_eq!(first.directory_height(), 1);
    first.paged().read_page(1).await.unwrap();
    let after_first_fault = metadata_reads.load(Ordering::SeqCst);
    first.paged().read_page(1).await.unwrap();
    assert_eq!(metadata_reads.load(Ordering::SeqCst), after_first_fault);

    let second = replica.clone().open_root(&root).await.unwrap();
    let before_second_fault = metadata_reads.load(Ordering::SeqCst);
    second.paged().read_page(1).await.unwrap();
    assert_eq!(metadata_reads.load(Ordering::SeqCst), before_second_fault);
}
#[tokio::test]
async fn directory_cache_survives_replica_restart_without_directory_origin_read() {
    let directory = tempfile::TempDir::new().unwrap();
    let cache_root = directory.path().join("directory-cache");
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
    let cell = [85; 32];
    let incarnation = [86; 16];
    let cache_host = Host::default()
        .with_local_disk_budget(DiskBudget::new(64 * 1024 * 1024))
        .with_directory_cache(cache_root.clone());
    let first =
        replica(Store::new(backend.clone()), cell, incarnation).with_host(cache_host.clone());
    let root = first
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    let warm_replica =
        replica(Store::new(backend.clone()), cell, incarnation).with_host(cache_host.clone());
    let warm = warm_replica.open_root(&root).await.unwrap();
    assert_eq!(warm.directory_height(), 1);
    warm.paged().read_page(1).await.unwrap();
    assert!(cache_host.directory_cache_stats().unwrap().entries() >= 1);
    drop(first);
    drop(warm_replica);

    let uncached_reads = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&uncached_reads);
    let uncached_store =
        Store::new(backend.clone()).with_read_byte_observer(Arc::new(move |bytes| {
            observed.fetch_add(bytes, Ordering::SeqCst);
        }));
    let uncached = replica(uncached_store, cell, incarnation);
    let uncached_root = uncached.open_root(&root).await.unwrap();
    uncached_reads.store(0, Ordering::SeqCst);
    uncached_root.paged().read_page(1).await.unwrap();
    let uncached_bytes = uncached_reads.load(Ordering::SeqCst);
    assert!(uncached_bytes > 0);

    let cached_reads = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&cached_reads);
    let cached_store = Store::new(backend).with_read_byte_observer(Arc::new(move |bytes| {
        observed.fetch_add(bytes, Ordering::SeqCst);
    }));
    let cached_host = Host::default()
        .with_local_disk_budget(DiskBudget::new(64 * 1024 * 1024))
        .with_directory_cache(cache_root);
    let cached = replica(cached_store, cell, incarnation).with_host(cached_host);
    let cached_root = cached.open_root(&root).await.unwrap();
    cached_reads.store(0, Ordering::SeqCst);
    cached_root.paged().read_page(1).await.unwrap();
    let cached_bytes = cached_reads.load(Ordering::SeqCst);

    assert!(
        cached_bytes < uncached_bytes,
        "a restarted replica must avoid the persisted directory-node origin read"
    );
}
#[tokio::test]
async fn directory_growth_adds_authenticated_parent_level() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(700000))",
            )
        })
        .unwrap();
    let replica = replica(Store::new(Arc::new(InMemory::new())), [61; 32], [62; 16]);
    let first = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap();
    assert_eq!(first.verified().directory_height(), 0);

    writer
        .transaction(|transaction| {
            transaction.execute("INSERT INTO payload VALUES(randomblob(700000))", [])?;
            Ok(())
        })
        .unwrap();
    let second = replica
        .prepare(Some(&first.root()), &writer.capture().unwrap(), 2, 1)
        .await
        .unwrap();
    assert_eq!(second.verified().directory_height(), 1);
    let last = second.verified().paged().page_count();
    assert_eq!(
        second
            .verified()
            .paged()
            .read_page(last)
            .await
            .unwrap()
            .len(),
        second.verified().page_size() as usize
    );
    writer.close().unwrap();
}
#[tokio::test]
async fn truncate_regrow_cannot_reuse_old_locator() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("cell.sqlite");
    let initial = crab_ltx::rusqlite::Connection::open(&path).unwrap();
    initial
        .execute_batch("PRAGMA auto_vacuum = FULL; VACUUM")
        .unwrap();
    drop(initial);
    let mut writer = Db::open(&path, Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(zeroblob(4000000))",
            )
        })
        .unwrap();
    let replica = replica(Store::new(Arc::new(InMemory::new())), [51; 32], [52; 16]);
    let first_batch = writer.capture().unwrap();
    let first_pages = first_batch.segments.last().unwrap().info().database_pages;
    let first = replica
        .prepare(None, &first_batch, 1, 1)
        .await
        .unwrap()
        .root();

    writer
        .transaction(|transaction| {
            transaction.execute("DELETE FROM payload", [])?;
            Ok(())
        })
        .unwrap();
    let truncated_batch = writer.capture().unwrap();
    let truncated_pages = truncated_batch
        .segments
        .last()
        .unwrap()
        .info()
        .database_pages;
    assert!(truncated_pages < first_pages);
    let truncated = replica
        .prepare(Some(&first), &truncated_batch, 2, 1)
        .await
        .unwrap()
        .root();

    writer
        .transaction(|transaction| {
            transaction.execute("INSERT INTO payload VALUES(randomblob(4000000))", [])?;
            Ok(())
        })
        .unwrap();
    let regrown_batch = writer.capture().unwrap();
    let regrown = replica
        .prepare(Some(&truncated), &regrown_batch, 3, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();

    let restored = directory.path().join("restored.sqlite");
    let writable = replica
        .open_root(&regrown)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&restored)
        .await
        .unwrap();
    let restored_for_open = restored.clone();
    let mut replacement =
        tokio::task::spawn_blocking(move || writable.open_writable(&restored_for_open))
            .await
            .unwrap()
            .unwrap();
    let (length, is_zero): (u32, bool) = replacement
        .transaction(|transaction| {
            transaction.query_row(
                "SELECT length(value), value = zeroblob(length(value)) FROM payload",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(length, 4_000_000);
    assert!(!is_zero);
    replacement.close().unwrap();
}
#[tokio::test]
async fn initial_streaming_directory_merges_truncation_and_regrowth() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("cell.sqlite");
    let initial = crab_ltx::rusqlite::Connection::open(&path).unwrap();
    initial
        .execute_batch("PRAGMA auto_vacuum = FULL; VACUUM")
        .unwrap();
    drop(initial);
    let mut writer = Db::open(&path, Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(zeroblob(4000000))",
            )
        })
        .unwrap();
    let first = writer.capture().unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute("DELETE FROM payload", [])?;
            Ok(())
        })
        .unwrap();
    let truncated = writer.capture().unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute("INSERT INTO payload VALUES(randomblob(4000000))", [])?;
            Ok(())
        })
        .unwrap();
    let regrown = writer.capture().unwrap();
    let mut segments = first.segments;
    segments.extend(truncated.segments);
    segments.extend(regrown.segments);
    let batch = CaptureBatch {
        segments,
        position: regrown.position,
        timing: CaptureTiming::default(),
    };
    let segment_count = batch.segments.len();

    let replica = replica(Store::new(Arc::new(InMemory::new())), [53; 32], [54; 16]);
    let prepared = replica.prepare(None, &batch, 1, 1).await.unwrap();
    assert_eq!(prepared.verified().segment_count(), segment_count);
    writer.close().unwrap();

    let restored = directory.path().join("streamed.sqlite");
    prepared.verified().restore(&restored).await.unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    let (length, is_zero): (u32, bool) = connection
        .query_row(
            "SELECT length(value), value = zeroblob(length(value)) FROM payload",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(length, 4_000_000);
    assert!(!is_zero);
}

#[tokio::test]
async fn writable_activation_preserves_leaf_order_across_parent_branches() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("source.sqlite");
    let initial = crab_ltx::rusqlite::Connection::open(&path).unwrap();
    initial
        .execute_batch("PRAGMA page_size=512; CREATE TABLE payload(value BLOB)")
        .unwrap();
    drop(initial);
    let mut writer = Db::open(&path, Limits::default()).unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO payload VALUES(zeroblob(40000000))"))
        .unwrap();
    let replica = replica(Store::new(Arc::new(InMemory::new())), [101; 32], [102; 16]);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    let verified = replica.open_root(&root).await.unwrap();
    assert_eq!(verified.directory_height(), 2);
    let destination = directory.path().join("restored.sqlite");
    let prepared = verified
        .paged()
        .prepare_writable(&destination)
        .await
        .unwrap();
    let mut restored = prepared.open_writable(&destination).unwrap();
    let length: u64 = restored
        .query_with(|db| db.query_row("SELECT length(value) FROM payload", [], |row| row.get(0)))
        .unwrap();
    assert_eq!(length, 40_000_000);
    restored.close().unwrap();
    writer.close().unwrap();
}

#[tokio::test]
async fn writable_activation_rejects_a_corrupt_late_leaf_and_cleans_its_file() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("source.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE payload(value); INSERT INTO payload VALUES(zeroblob(10000000))",
            )
        })
        .unwrap();
    let backend = Arc::new(InMemory::new());
    let layout = CellStorageLayout::new(
        Store::new(backend.clone()),
        Path::from("activation-corruption"),
        [103; 16],
    );
    let cell = [104; 32];
    let incarnation = [105; 16];
    let source = CellReplica::new(layout.clone(), cell, incarnation, Limits::default()).unwrap();
    let root = source
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    let mut leaves = Vec::new();
    for object in source.reachable_objects(&root).await.unwrap() {
        if object.kind == CellObjectKind::Directory {
            let path =
                layout.incarnation_object_path(&cell, &incarnation, &object.digest, object.kind);
            let bytes = backend.get(&path).await.unwrap().bytes().await.unwrap();
            // CRBDIR01 kind and first page locate a late leaf independently of
            // object digest order, so earlier prefetched leaves can succeed.
            if bytes[10] == 0 {
                let first = u32::from_be_bytes(bytes[32..36].try_into().unwrap());
                leaves.push((first, path, bytes));
            }
        }
    }
    let (first, path, original) = leaves
        .into_iter()
        .max_by_key(|(first, _, _)| *first)
        .unwrap();
    assert!(first > 256);
    let io = Arc::new(tokio::sync::Semaphore::new(4));
    let reader = CellReplica::new(
        CellStorageLayout::new(
            Store::new(backend.clone()),
            Path::from("activation-corruption"),
            [103; 16],
        ),
        cell,
        incarnation,
        Limits::default(),
    )
    .unwrap()
    .with_host(Host::default().with_io_slots(io.clone()));
    let paged = reader.open_root(&root).await.unwrap().paged();
    let mut corrupt = original.to_vec();
    *corrupt.last_mut().unwrap() ^= 1;
    backend
        .put(&path, Bytes::from(corrupt).into())
        .await
        .unwrap();
    let destination = directory.path().join("active.sqlite");
    assert!(matches!(
        paged.clone().prepare_writable(&destination).await,
        Err(crab_ltx::CrabError::ChecksumMismatch)
    ));
    assert!(!checksum_path(&destination).exists());
    assert_eq!(io.available_permits(), 4);
    backend.put(&path, original.into()).await.unwrap();
    paged.prepare_writable(&destination).await.unwrap();
    writer.close().unwrap();
}
