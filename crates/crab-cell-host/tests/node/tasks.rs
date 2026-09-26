//! Node task groups, supervision, and readiness reporting.

use super::*;

struct NoopNodeDurabilityProvider;

impl NodeDurabilityProvider for NoopNodeDurabilityProvider {
    fn recruit(
        self: Arc<Self>,
        _limits: ReplicaLimits,
        _required_follower_bytes: u64,
        _live_node_limit: usize,
    ) -> Pin<Box<dyn Future<Output = FacilityResult<Option<NodeDurabilityConfig>>> + Send>> {
        Box::pin(async { Ok(None) })
    }
}

#[tokio::test]
async fn node_durability_supervisor_is_host_owned_and_joined() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([28; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    let provider = Arc::new(NoopNodeDurabilityProvider);
    node.install_node_durability_provider(
        Arc::clone(&provider),
        NodeDurabilitySupervisorConfig::new(
            ApplicationId::from_bytes([29; 16]),
            ReplicaLimits::default(),
            1,
            1,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
            1,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        node.owned_component::<NoopNodeDurabilityProvider>(NODE_DURABILITY_PROVIDER_COMPONENT)
            .is_some()
    );
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_node_task_removes_readiness_and_is_reported_during_drain() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([30; 16]))
        .build()
        .unwrap();
    let task_group = node
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());
    task_group
        .spawn(async { Err::<(), _>(std::io::Error::other("supervisor failed")) })
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    assert!(!node.is_ready());
    let error = node.drain().await.unwrap_err();
    assert!(matches!(
        error,
        Error::Facility {
            name: "cell-coordination-tasks",
            ..
        }
    ));
}

#[tokio::test]
async fn completed_node_task_removes_readiness() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([33; 16]))
        .build()
        .unwrap();
    let task_group = node
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());

    task_group.spawn(async { Ok::<(), Error>(()) }).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while node.is_ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!node.is_ready());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn panicked_node_task_removes_readiness_and_is_reported_during_drain() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([31; 16]))
        .build()
        .unwrap();
    let task_group = node
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());
    task_group
        .spawn_boxed(async {
            panic!("supervisor panicked");
        })
        .unwrap();
    tokio::task::yield_now().await;
    assert!(!node.is_ready());
    let error = node.drain().await.unwrap_err();
    assert!(matches!(
        error,
        Error::Facility {
            name: "cell-coordination-tasks",
            ..
        }
    ));
}

#[tokio::test]
async fn readiness_requires_the_node_task_group() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([22; 16]))
        .build()
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();

    let error = node.start().unwrap_err();
    assert!(matches!(error, Error::Control(_)));
    assert_eq!(node.state(), NodeState::Starting);

    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_group_cancels_both_tokens_and_joins_tasks() {
    let cancellation = CancellationToken::new();
    let node_shutdown = CancellationToken::new();
    let finished = Arc::new(AtomicBool::new(false));
    let task_cancellation = cancellation.clone();
    let task_shutdown = node_shutdown.clone();
    let task_finished = Arc::clone(&finished);
    let tasks = CellNodeTaskGroup::new(cancellation.clone(), node_shutdown.clone());
    tasks
        .spawn(async move {
            task_cancellation.cancelled().await;
            task_shutdown.cancelled().await;
            task_finished.store(true, Ordering::Release);
            Ok::<(), Error>(())
        })
        .unwrap();

    tasks.drain().await.unwrap();

    assert!(cancellation.is_cancelled());
    assert!(node_shutdown.is_cancelled());
    assert!(finished.load(Ordering::Acquire));
}

#[tokio::test]
async fn task_group_rejects_new_tasks_after_drain_starts() {
    let cancellation = CancellationToken::new();
    let node_shutdown = CancellationToken::new();
    let task_shutdown = node_shutdown.clone();
    let tasks = CellNodeTaskGroup::new(cancellation, node_shutdown);
    tasks
        .spawn(async move {
            task_shutdown.cancelled().await;
            Ok::<(), Error>(())
        })
        .unwrap();
    tasks.drain().await.unwrap();

    assert!(matches!(
        tasks.spawn(async { Ok::<(), Error>(()) }),
        Err(Error::CellDraining)
    ));
    assert!(matches!(
        tasks.spawn_boxed(async { Ok(()) }),
        Err(Error::CellDraining)
    ));
}

#[tokio::test]
async fn task_group_deadline_aborts_unfinished_tasks() {
    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let tasks = CellNodeTaskGroup::new(CancellationToken::new(), CancellationToken::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let task_dropped = Arc::clone(&dropped);
    tasks
        .spawn(async move {
            let _probe = DropProbe(task_dropped);
            std::future::pending::<()>().await;
            Ok::<(), Error>(())
        })
        .unwrap();

    let result = tasks
        .drain_until(Some(Instant::now() + std::time::Duration::from_millis(10)))
        .await;

    assert!(result.is_err());
    tokio::task::yield_now().await;
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn dropping_task_group_aborts_unjoined_tasks() {
    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    {
        let tasks = CellNodeTaskGroup::new(CancellationToken::new(), CancellationToken::new());
        let task_dropped = Arc::clone(&dropped);
        let task_started = Arc::clone(&started);
        tasks
            .spawn(async move {
                let _probe = DropProbe(task_dropped);
                task_started.notify_one();
                std::future::pending::<()>().await;
                Ok::<(), Error>(())
            })
            .unwrap();
        started.notified().await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn node_owns_one_task_group_and_drains_it() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([20; 16]))
        .build()
        .unwrap();
    let cancellation = CancellationToken::new();
    let node_shutdown = CancellationToken::new();
    let tasks = node
        .install_task_group(cancellation.clone(), node_shutdown.clone())
        .unwrap();
    assert!(
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .is_err()
    );
    let finished = Arc::new(AtomicBool::new(false));
    let task_finished = Arc::clone(&finished);
    let task_shutdown = node_shutdown.clone();
    tasks
        .spawn_lease_maintenance(async move {
            task_shutdown.cancelled().await;
            task_finished.store(true, Ordering::Release);
            Ok::<(), Error>(())
        })
        .unwrap();

    node.shutdown().await.unwrap();

    assert!(cancellation.is_cancelled());
    assert!(node_shutdown.is_cancelled());
    assert!(finished.load(Ordering::Acquire));
}

#[tokio::test]
async fn task_group_rejects_tasks_above_bound() {
    let cancellation = CancellationToken::new();
    let node_shutdown = CancellationToken::new();
    let tasks = CellNodeTaskGroup::new(cancellation, node_shutdown.clone());
    for _ in 0..MAX_NODE_TASKS {
        let node_shutdown = node_shutdown.clone();
        tasks
            .spawn(async move {
                node_shutdown.cancelled().await;
                Ok::<(), Error>(())
            })
            .unwrap();
    }
    let node_shutdown = node_shutdown.clone();
    let error = tasks
        .spawn(async move {
            node_shutdown.cancelled().await;
            Ok::<(), Error>(())
        })
        .unwrap_err();
    assert!(matches!(error, Error::Capacity(_)));

    assert!(matches!(
        tasks.spawn_lease_maintenance(async { Ok::<(), Error>(()) }),
        Err(Error::Capacity(_))
    ));

    tasks.drain().await.unwrap();
}
