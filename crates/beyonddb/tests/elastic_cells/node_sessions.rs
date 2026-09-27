use crate::*;

const SESSION: SessionId = SessionId::from_bytes([90; 16]);

async fn starting_node() -> (crab_cell_host::CellNode, NodeDirectory, CellStorageLayout) {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "node-session-restart".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("node-session-restart"),
        [42; 16],
    );
    let directory = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([80; 32]),
        Digest::from_bytes([81; 32]),
        Digest::from_bytes([82; 32]),
    );
    let node = CellNodeBuilder::new(application)
        .with_runtime(SqlWorkerPool::new(1, 16).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(SESSION)
        .build()
        .unwrap();
    let cancellation = CancellationToken::new();
    let tasks = node
        .install_task_group(CancellationToken::new(), cancellation.clone())
        .unwrap();
    let published = published_test_node_lease(&layout, SESSION).await;
    node.install_node_lease_for_startup(published.guard())
        .unwrap();
    tasks
        .spawn_lease_maintenance(async move { published.run(&cancellation).await })
        .unwrap();
    (node, directory, layout)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_shutdown_retires_session_before_same_node_restarts() {
    let (node, directory, layout) = starting_node().await;
    node.start().unwrap();
    beyonddb::shutdown_serving_node(&node, &directory, SESSION)
        .await
        .unwrap();
    // Do not wait out the old lease: placement must accept the immediate next
    // boot on this node after a successful graceful shutdown.
    let next = SessionId::from_bytes([91; 16]);
    let replacement = published_test_node_lease(&layout, next).await;
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let live = directory.live(now, 10).await.unwrap();
    assert_eq!(
        live.iter()
            .map(NodeAdvertisement::session)
            .collect::<Vec<_>>(),
        vec![next]
    );
    assert!(directory.is_retired(SESSION).await.unwrap());
    drop(replacement);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_drain_does_not_retire_session() {
    let (node, directory, _) = starting_node().await;
    node.install_facility(
        crab_cell_host::CellNodeFacility::new("failed-drain", || async {
            Err(Box::new(std::io::Error::other("injected drain failure"))
                as Box<dyn std::error::Error + Send + Sync>)
        })
        .unwrap(),
    )
    .unwrap();
    node.start().unwrap();
    assert!(matches!(
        beyonddb::shutdown_serving_node(&node, &directory, SESSION).await,
        Err(crab_cell_runtime::Error::Facility {
            name: "failed-drain",
            ..
        })
    ));
    let observed = directory
        .inspect_advertisement(
            SESSION,
            i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(observed.is_some());
}
