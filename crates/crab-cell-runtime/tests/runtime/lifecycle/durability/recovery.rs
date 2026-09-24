//! Recovery inventory reads and lost-acknowledgement recovery.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn recovery_inventory_reads_catalog_heads_concurrently() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let object_store: Arc<dyn ObjectStore> = pausing.clone();
    let fixture = fixture_with_limits_and_store(
        b"parallel-recovery-inventory",
        Limits::default(),
        Store::new(object_store),
    );
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let authority = CellAuthority::new(fixture.layout.clone());
    pausing.require_parallel_catalog_heads();

    let inventory = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        crab_cell_runtime::node::log_recovery::recoverable_cells(
            &catalog,
            &authority,
            SessionId::from_bytes([99; 16]),
            10,
        ),
    )
    .await
    .expect("catalog head reads remained serial")
    .unwrap();

    assert!(inventory.is_empty());
}
#[tokio::test(flavor = "multi_thread")]
async fn lost_ack_suffix_recovers_an_ambiguous_command_without_reexecution() {
    let store = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let object_store: Arc<dyn ObjectStore> = store.clone();
    let fixture = fixture_with_limits_and_store(
        b"lost-ack-recovery",
        Limits::default(),
        Store::new(object_store),
    );
    let leader = SessionId::from_bytes([126; 16]);
    let follower = SessionId::from_bytes([127; 16]);
    let member = NodeId::from_bytes(*follower.as_bytes());
    let successor = SessionId::from_bytes([128; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        leader,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let follower_directory = tempfile::TempDir::new().unwrap();
    let follower_store = crab_cell_runtime::FollowerStore::open(
        follower_directory.path().to_owned(),
        Limits::default(),
        DiskBudget::new(1 << 30),
    )
    .unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(LostAckFollowerTransport {
        inner: crab_cell_runtime::node::log_transport::LocalFollowerTransport::new(
            member,
            follower_store.clone(),
        ),
        acknowledged_once: AtomicBool::new(false),
    });
    let gate =
        DurabilityGate::new(leader, NodeId::from_bytes(*leader.as_bytes()), 1, [member]).unwrap();
    let shipper =
        NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
    runtime
        .install_node_durability(
            fixture.target.application(),
            Arc::new(NodeDurability::new(
                gate,
                shipper,
                Arc::new(TestNodeAuthority::default()),
                Arc::clone(&transport),
                lease,
            )),
        )
        .unwrap();
    let handle = bootstrap_on(&runtime, &fixture, leader).await;
    let first = handle
        .execute(
            identity(126),
            Digest::from_bytes([126; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"published".to_vec()))
            },
        )
        .await
        .unwrap();
    assert_eq!(first.commit_sequence(), 1);
    let authority = CellAuthority::new(fixture.layout.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current = authority
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap();
            if current.value().root.as_ref().unwrap().commit_sequence == 1
                && runtime.stats().unpublished_node_log_bytes() == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let request_identity = identity(127);
    let operation_digest = Digest::from_bytes([127; 32]);
    store.fail_puts();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle.execute(
            request_identity,
            operation_digest,
            21,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"ambiguous".to_vec()))
            },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(crab_cell_runtime::Error::OutcomeUnknown {
            request_id,
            operation_digest: digest,
            ..
        }) if request_id == request_identity.request_id && digest == operation_digest
    ));
    tokio::time::timeout(std::time::Duration::from_secs(2), store.wait_until_failed())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while runtime.stats().active_cells() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(follower_store.retained_bytes() > 0);
    store.allow_puts();
    assert!(runtime.shutdown().await.is_err());

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let stale = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stale.value().root.as_ref().unwrap().commit_sequence, 1);
    let fenced = fence_log_session(&fixture.layout, leader, successor, follower, 1).await;
    let recovery = crab_cell_runtime::node::log_recovery::NodeLogRecovery::from_fenced(
        Arc::clone(&transport),
        &fenced,
        Limits::default(),
    )
    .unwrap();
    let manifests = crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
        fixture.layout.clone(),
        Limits::default(),
    );
    let coordinator = crab_cell_runtime::node::log_recovery::RecoveryCoordinator::new(
        recovery,
        manifests.clone(),
    );
    let inventory =
        crab_cell_runtime::node::log_recovery::recoverable_cells(&catalog, &authority, leader, 10)
            .await
            .unwrap();
    let directory = crab_cell_runtime::node::NodeDirectory::new(
        fixture.layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        Digest::from_bytes([92; 32]),
    );
    let completed = coordinator
        .recover_and_seal(&directory, fenced, inventory, 10_002)
        .await
        .unwrap();
    let attached = completed.controls.into_iter().next().unwrap();
    let successor_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = successor_runtime
        .takeover_restored(
            proof,
            fixture.replica.clone(),
            authority,
            attached,
            completed.takeover,
            manifests,
            fixture._directory.path().join("lost-ack-successor.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://lost-ack-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    let replayed = restored
        .execute(
            request_identity,
            operation_digest,
            30,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"executed-twice".to_vec()))
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        replayed,
        StoredOutcome::Success {
            ref result,
            commit_sequence: 2
        } if result == b"ambiguous"
    ));
    let value = restored
        .query(64, 64, |connection| {
            let value = connection
                .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
            Ok(value.to_be_bytes().to_vec())
        })
        .await
        .unwrap();
    assert_eq!(i64::from_be_bytes(value.try_into().unwrap()), 2);
    restored.drain().await.unwrap();
    successor_runtime.shutdown().await.unwrap();
}
