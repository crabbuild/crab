//! Cell node builder validation and configuration retention.

use super::*;

#[test]
fn builder_rejects_missing_owners_before_starting() {
    let error = match CellNodeBuilder::new(application()).build() {
        Ok(_) => panic!("missing node owners must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::Control(_)));
}

#[test]
fn builder_rejects_zero_node_session_before_starting() {
    let result = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([0; 16]))
        .build();
    let error = match result {
        Ok(_) => panic!("zero node sessions must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::Control(_)));
}

#[tokio::test]
async fn builder_retains_configured_follower_store_as_an_owned_component() {
    let data_dir = tempfile::tempdir().unwrap();
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([3; 16]))
        .with_follower_store(
            data_dir.path().join("followers"),
            ReplicaLimits::default(),
            DiskBudget::new(1 << 20),
        )
        .build()
        .unwrap();

    assert!(
        node.owned_component::<FollowerStore>(FOLLOWER_STORE_COMPONENT)
            .is_some()
    );
}
