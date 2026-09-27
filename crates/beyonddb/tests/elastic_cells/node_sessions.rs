use crate::*;

const SESSION: SessionId = SessionId::from_bytes([90; 16]);

async fn starting_node(
    store: Store,
) -> (crab_cell_host::CellNode, NodeDirectory, CellStorageLayout) {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "node-session-restart".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let layout = CellStorageLayout::new(
        store,
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
    let (node, directory, layout) = starting_node(Store::new(Arc::new(InMemory::new()))).await;
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
    let (node, directory, _) = starting_node(Store::new(Arc::new(InMemory::new()))).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_reconciles_a_heartbeat_that_commits_after_drain() {
    use crab_storage::test_support::CountingObjectStore;
    use object_store::throttle::{ThrottleConfig, ThrottledStore};

    let origin = Arc::new(InMemory::new());
    let delayed = Arc::new(ThrottledStore::new(
        origin.clone(),
        ThrottleConfig::default(),
    ));
    let counted = Arc::new(CountingObjectStore::new(delayed.clone()));
    let (node, directory, _) = starting_node(Store::new(counted.clone())).await;
    node.start().unwrap();
    let now = || {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    };
    let observed = directory.load(SESSION, now()).await.unwrap().unwrap();
    let before = counted.put_requests();
    delayed.config_mut(|config| config.wait_put_per_call = Duration::from_secs(1));
    let retiring = directory.clone();
    let shutdown =
        tokio::spawn(
            async move { beyonddb::shutdown_serving_node(&node, &retiring, SESSION).await },
        );
    tokio::time::timeout(Duration::from_secs(2), async {
        while counted.put_requests() == before {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Commit through the same provider after shutdown's GET and before its
    // delayed conditional PUT, as a canceled in-flight renewal could do.
    let direct = NodeDirectory::new(
        CellStorageLayout::new(
            Store::new(origin),
            object_store::path::Path::from("node-session-restart"),
            [42; 16],
        ),
        Digest::from_bytes([80; 32]),
        Digest::from_bytes([81; 32]),
        Digest::from_bytes([82; 32]),
    );
    let refreshed_at = now();
    direct
        .refresh(
            &observed,
            test_node_advertisement(SESSION, refreshed_at, refreshed_at + 15_000).unwrap(),
            refreshed_at,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(4), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(directory.is_retired(SESSION).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retirement_read_and_write_stalls_return_a_bounded_failure() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};

    for stall_read in [true, false] {
        let delayed = Arc::new(ThrottledStore::new(
            InMemory::new(),
            ThrottleConfig::default(),
        ));
        let (node, directory, _) = starting_node(Store::new(delayed.clone())).await;
        node.start().unwrap();
        delayed.config_mut(|config| {
            if stall_read {
                config.wait_get_per_call = Duration::from_secs(60);
            } else {
                config.wait_put_per_call = Duration::from_secs(60);
            }
        });
        let result = tokio::time::timeout(
            Duration::from_secs(17),
            beyonddb::shutdown_serving_node(&node, &directory, SESSION),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Err(crab_cell_runtime::Error::Deadline)),
            "read={stall_read}: {result:?}"
        );
        delayed.config_mut(|config| *config = ThrottleConfig::default());
        assert!(!directory.is_retired(SESSION).await.unwrap());
    }
}
