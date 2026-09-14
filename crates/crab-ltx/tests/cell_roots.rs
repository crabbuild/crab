#![cfg(feature = "replica")]

use std::sync::Arc;

use crab_ltx::{CellReplica, Limits, ManagedDb, RootRef};
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
                 INSERT INTO messages(body) VALUES ('first')",
            )
        })
        .unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
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

    let reopened = replica.open_root(&prepared.root()).await.unwrap();
    assert_eq!(reopened.root(), prepared.root());
    assert_eq!(
        reopened.database_pages(),
        prepared.verified().database_pages()
    );
    assert!(reopened.page_size().is_power_of_two());
    writer.close().unwrap();
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
