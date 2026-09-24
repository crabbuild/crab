//! Node startup, deadline, shutdown, and scale-down lifecycle.

use super::*;

#[tokio::test]
async fn node_shutdown_is_idempotent_and_returns_stopped_state() {
    let pool = SqlWorkerPool::new(1, 1).unwrap();
    let host = ReplicaHost::default();
    let node = CellNodeBuilder::new(application())
        .with_runtime(pool, 16 * 1024 * 1024)
        .with_replica_host(host)
        .with_session(SessionId::from_bytes([4; 16]))
        .build()
        .unwrap();
    assert_eq!(node.state(), NodeState::Starting);
    assert!(!node.is_ready());
    let starting = node.status();
    assert_eq!(starting.state(), NodeState::Starting);
    assert!(!starting.is_shutting_down());
    assert_eq!(starting.stats(), node.stats());
    assert!(node.start().is_err());
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    assert_eq!(node.state(), NodeState::Ready);
    assert!(node.is_ready());
    node.shutdown().await.unwrap();
    assert_eq!(node.state(), NodeState::Stopped);
    let stopped = node.status();
    assert_eq!(stopped.state(), NodeState::Stopped);
    assert!(stopped.is_shutting_down());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_lease_does_not_open_readiness_before_host_startup_finishes() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([19; 16]))
        .build()
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    assert_eq!(node.state(), NodeState::Starting);
    assert!(!node.is_ready());
    assert!(node.start().is_err());
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn node_deadline_returns_after_a_stalled_facility() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([21; 16]))
        .build()
        .unwrap();
    node.install_facility(
        CellNodeFacility::new("stalled", || async {
            std::future::pending::<FacilityResult>().await
        })
        .unwrap(),
    )
    .unwrap();

    let result = node
        .shutdown_until(Instant::now() + std::time::Duration::from_millis(10))
        .await;

    assert!(result.is_err());
    assert_eq!(node.state(), NodeState::Draining);
}

#[tokio::test]
async fn node_deadline_bounds_a_stalled_coordination_task() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([23; 16]))
        .build()
        .unwrap();
    let tasks = node
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    tasks
        .spawn(async {
            std::future::pending::<()>().await;
            Ok::<(), Error>(())
        })
        .unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        node.shutdown_until(Instant::now() + std::time::Duration::from_millis(10)),
    )
    .await
    .expect("deadline-aware shutdown must return");

    assert!(result.is_err());
    assert_eq!(node.state(), NodeState::Draining);
}

#[tokio::test]
async fn node_cancels_admission_before_draining_provider_facilities() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([27; 16]))
        .build()
        .unwrap();
    let cancellation = CancellationToken::new();
    node.install_task_group(cancellation.clone(), CancellationToken::new())
        .unwrap();
    let observed = Arc::new(AtomicBool::new(false));
    let facility_observed = Arc::clone(&observed);
    node.install_facility(
        CellNodeFacility::new("provider", move || {
            let cancellation = cancellation.clone();
            let facility_observed = facility_observed.clone();
            async move {
                cancellation.cancelled().await;
                facility_observed.store(true, Ordering::Release);
                Ok(())
            }
        })
        .unwrap(),
    )
    .unwrap();

    node.shutdown_until(Instant::now() + std::time::Duration::from_secs(1))
        .await
        .unwrap();

    assert!(observed.load(Ordering::Acquire));
}

#[tokio::test]
async fn scale_down_stops_acquisition_without_stopping_the_host() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([97; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    node.start().unwrap();
    let status = node
        .drain_for_scale_down(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert!(status.ready_to_stop());
    assert_eq!(node.state(), NodeState::Stopped);
    assert!(!node.is_ready());
    assert!(!node.runtime().is_acquiring());
    let retry = node
        .drain_for_scale_down(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert!(retry.ready_to_stop());
    node.drain().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_shutdown_waits_for_the_single_runtime_drain() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([5; 16]))
        .build()
        .unwrap();
    let first = node.shutdown();
    let second = node.shutdown();
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    assert_eq!(node.state(), NodeState::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_nodes_have_independent_lifecycle_and_resource_ledgers() {
    let mut nodes = Vec::new();
    for index in 0..3_u8 {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([index + 10; 16]))
            .build()
            .unwrap();
        assert_eq!(node.state(), NodeState::Starting);
        assert!(!node.is_ready());
        nodes.push(node);
    }

    for node in &nodes {
        node.shutdown().await.unwrap();
        assert_eq!(node.state(), NodeState::Stopped);
        assert!(node.is_shutting_down());
    }
}
