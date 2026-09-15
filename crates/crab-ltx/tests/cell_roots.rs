#![cfg(feature = "replica")]

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crab_ltx::{
    CellReplica, Limits, ManagedDb, RootRef, VerifiedLocalPlan,
    bundle::{Bundle, BundleEntry},
    restore_exact,
};
use crab_storage::{CellStorageLayout, StorageReadKind, Store};
use object_store::{memory::InMemory, path::Path};

fn replica(store: Store, cell: [u8; 32], incarnation: [u8; 16]) -> CellReplica {
    CellReplica::new(
        CellStorageLayout::new(store, Path::from("runtime"), [3; 16]),
        cell,
        incarnation,
        Limits::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn prepared_root_reopens_without_a_mutable_head() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer =
        ManagedDb::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
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
    let plan = VerifiedLocalPlan::new(&segments, second.position, Limits::default()).unwrap();
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
}

#[tokio::test]
async fn root_scope_and_commit_sequence_are_fenced() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer =
        ManagedDb::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
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
async fn exact_cell_root_opens_sparse_writer_and_publishes_incrementally() {
    let source = tempfile::TempDir::new().unwrap();
    let source_path = source.path().join("source.sqlite");
    let mut writer = ManagedDb::open(&source_path, Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE messages(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO messages(body) VALUES ('first'), ('second');\
                 CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(2000000))",
            )
        })
        .unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let replica = replica(store, [8; 32], [9; 16]);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 0, 3)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    source.close().unwrap();

    let active = tempfile::TempDir::new().unwrap();
    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable()
        .await
        .unwrap();
    let active_path = active.path().join("active.sqlite");
    let mut writer = tokio::task::spawn_blocking(move || writable.open_writable(&active_path))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(writer.position(), root.position);
    assert!(!writer.hydration().unwrap().unwrap().complete());
    writer
        .transaction(|transaction| {
            assert_eq!(
                transaction.query_row("SELECT count(*) FROM messages", [], |row| row
                    .get::<_, u32>(0))?,
                2
            );
            transaction.execute("INSERT INTO messages(body) VALUES ('third')", [])?;
            Ok(())
        })
        .unwrap();
    let next = replica
        .prepare(Some(&root), &writer.capture().unwrap(), 1, 3)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    assert_eq!(next.commit_sequence, 1);

    let replacement = tempfile::TempDir::new().unwrap();
    let writable = replica
        .open_root(&next)
        .await
        .unwrap()
        .paged()
        .prepare_writable()
        .await
        .unwrap();
    let replacement_path = replacement.path().join("replacement.sqlite");
    let mut replacement =
        tokio::task::spawn_blocking(move || writable.open_writable(&replacement_path))
            .await
            .unwrap()
            .unwrap();
    let count = replacement
        .transaction(|transaction| {
            transaction.query_row("SELECT count(*) FROM messages", [], |row| {
                row.get::<_, u32>(0)
            })
        })
        .unwrap();
    assert_eq!(count, 3);
    replacement.close().unwrap();
}

#[tokio::test]
async fn changed_cut_loads_only_touched_directory_nodes() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer =
        ManagedDb::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
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
    let mut writer =
        ManagedDb::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
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

#[tokio::test(flavor = "multi_thread")]
async fn sparse_hydration_coalesces_contiguous_cell_frames() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer =
        ManagedDb::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(2000000))",
            )
        })
        .unwrap();
    let range_reads = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&range_reads);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |kind| {
            if kind == StorageReadKind::Range {
                observed.fetch_add(1, Ordering::SeqCst);
            }
        }));
    let replica = replica(store, [83; 32], [84; 16]);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();

    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable()
        .await
        .unwrap();
    range_reads.store(0, Ordering::SeqCst);
    let destination = directory.path().join("sparse.sqlite");
    let (before, after, reads) = tokio::task::spawn_blocking(move || {
        let mut writer = writable.open_writable(&destination).unwrap();
        let before = writer.hydration().unwrap().unwrap();
        let requests = range_reads.load(Ordering::SeqCst);
        let after = writer.hydrate_step(64).unwrap();
        let reads = range_reads.load(Ordering::SeqCst) - requests;
        writer.close().unwrap();
        (before, after, reads)
    })
    .await
    .unwrap();
    let hydrated = after.resolved - before.resolved;
    assert!(hydrated >= 32);
    assert!(reads < u64::from(hydrated));
}

#[tokio::test]
async fn directory_growth_adds_authenticated_parent_level() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer =
        ManagedDb::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
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
    let mut writer = ManagedDb::open(&path, Limits::default()).unwrap();
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
        .prepare_writable()
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

#[tokio::test(flavor = "multi_thread")]
async fn prepare_does_not_write_mutable_keys() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("cell.sqlite");
    let mut writer = ManagedDb::open(&path, Limits::default()).unwrap();
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

    let compacted = replica
        .prepare_compaction(&appended.root(), 0..2, 1)
        .await
        .unwrap();
    assert_eq!(compacted.predecessor(), Some(appended.root()));
    assert_eq!(compacted.root().position, appended.root().position);
    assert_eq!(compacted.root().commit_sequence, 3);
    assert_eq!(compacted.verified().segment_count(), 2);
    assert_ne!(compacted.root().digest, appended.root().digest);

    let snapshot = replica
        .prepare_compaction(&compacted.root(), 0..2, 9)
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
    let destination = tempfile::TempDir::new().unwrap();
    let writable = replica
        .open_root(&snapshot.root())
        .await
        .unwrap()
        .paged()
        .prepare_writable()
        .await
        .unwrap();
    let path = destination.path().join("restored.sqlite");
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
