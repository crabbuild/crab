//! Conditional desired-reader policy storage tests.

use std::sync::Arc;

use crab_cell_runtime::{CellId, identity::IncarnationId, read_policy::ReadPolicyStore};
use crab_ltx::CellStorageLayout;
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

#[tokio::test]
async fn desired_reader_updates_require_the_observed_etag() {
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        [1; 16],
    );
    let store = ReadPolicyStore::new(layout);
    let cell = CellId::from_bytes([2; 32]);
    let incarnation = IncarnationId::from_bytes([3; 16]);
    assert!(store.load(cell).await.unwrap().is_none());

    let first = store.create(cell, incarnation, 0).await.unwrap();
    assert_eq!(first.value().desired_readers(), 0);
    let second = store.update(&first, 2).await.unwrap();
    assert_eq!(second.value().revision(), 2);
    let current = store.update(&second, 4).await.unwrap();
    assert_eq!(current.value().desired_readers(), 4);
    assert!(store.update(&first, 1).await.is_err());
    assert_eq!(
        store.load(cell).await.unwrap().unwrap().value(),
        current.value()
    );
    assert!(store.create(cell, incarnation, 1).await.is_err());
    let next_incarnation = IncarnationId::from_bytes([4; 16]);
    let replacement = store
        .replace_incarnation(&current, next_incarnation, 1)
        .await
        .unwrap();
    assert_eq!(replacement.value().incarnation(), next_incarnation);
    assert_eq!(replacement.value().revision(), 4);
    assert!(store.update(&current, 5).await.is_err());
}
