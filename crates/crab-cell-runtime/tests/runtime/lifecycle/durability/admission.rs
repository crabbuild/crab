//! Node durability byte admission and reservation bounds.

use super::*;

#[tokio::test]
async fn node_byte_reservation_rejects_overcommit_and_releases_capacity() {
    let session = SessionId::from_bytes([40; 16]);
    let local_disk = DiskBudget::new(4_096);
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(1, 1).unwrap(),
        1_024,
        session,
        ReplicaHost::default().with_local_disk_budget(local_disk.clone()),
    )
    .unwrap();
    let disk = local_disk.try_reserve(512).unwrap();
    let held = runtime.try_reserve_node_bytes(1_024).unwrap();

    let full = runtime.stats();
    assert_eq!(full.active_cells(), 0);
    assert_eq!(full.active_cell_capacity(), 1);
    assert_eq!(full.file_descriptors(), 0);
    assert_eq!(
        full.file_descriptor_capacity(),
        ACTIVE_CELL_FILE_DESCRIPTORS
    );
    assert_eq!(full.retained_bytes(), 1_024);
    assert_eq!(full.retained_capacity_bytes(), 1_024);
    assert_eq!(full.local_disk_reserved_bytes(), 512);
    assert_eq!(full.local_disk_capacity_bytes(), 4_096);

    assert!(matches!(
        runtime.try_reserve_node_bytes(1),
        Err(crab_cell_runtime::Error::Capacity("node retained bytes"))
    ));
    drop(held);
    drop(disk);
    let empty = runtime.stats();
    assert_eq!(empty.file_descriptors(), 0);
    assert_eq!(
        empty.file_descriptor_capacity(),
        ACTIVE_CELL_FILE_DESCRIPTORS
    );
    assert_eq!(empty.retained_bytes(), 0);
    assert_eq!(empty.local_disk_reserved_bytes(), 0);
    let released = runtime.try_reserve_node_bytes(1_024).unwrap();
    drop(released);

    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn node_byte_admission_rejects_before_sql_execution() {
    let fixture = fixture();
    let handle = activate(&fixture, 1024 * 1024).await;
    assert!(matches!(
        handle
            .execute(
                mutation_identity_window(12, 10, 10_000),
                Digest::from_bytes([13; 32]),
                20,
                1_025,
                1024 * 1024,
                |_| Ok(HandlerOutcome::Success(Vec::new())),
            )
            .await,
        Err(crab_cell_runtime::Error::Capacity(_))
    ));
    handle.drain().await.unwrap();
}
