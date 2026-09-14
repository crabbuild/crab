use std::sync::Arc;

use crab_cell_runtime::{CellId, Control, Digest, IncarnationId, Owner, SessionId};
use crab_ltx::{CellReplica, Limits, ManagedDb};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

#[tokio::test]
async fn prepared_root_becomes_one_valid_control_successor() {
    let cell = CellId::from_bytes([1; 32]);
    let incarnation = IncarnationId::from_bytes([2; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("runtime"), [3; 16]);
    let replica = CellReplica::new(
        layout,
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer =
        ManagedDb::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(value)"))
        .unwrap();
    let cuts = writer.capture().unwrap();
    let prepared = replica.prepare(None, &cuts, 1, 1).await.unwrap();
    let control = Control::initial(
        cell,
        incarnation,
        Owner {
            session: SessionId::from_bytes([4; 16]),
            endpoint: "https://node.internal:8081".into(),
        },
        Digest::from_bytes([5; 32]),
        1,
    )
    .unwrap();
    let published = control.publish_prepared(&prepared, Some(42)).unwrap();
    assert_eq!(published.ltx_root(), Some(prepared.root()));
    assert_eq!(published.next_due_ms, Some(42));
    assert_eq!(published.revision, 2);
    writer.close().unwrap();
}
