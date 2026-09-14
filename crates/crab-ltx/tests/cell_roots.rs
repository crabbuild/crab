#![cfg(feature = "replica")]

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crab_ltx::{CellReplica, Limits, ManagedDb, RootRef, VerifiedLocalPlan, restore_exact};
use crab_storage::{CellStorageLayout, Store};
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
    let pages = reopened.paged();
    let mut restored = Vec::with_capacity(expected.len());
    for page in 1..=pages.page_count() {
        restored.extend(pages.read_page(page).await.unwrap());
    }
    assert_eq!(restored, expected);
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
