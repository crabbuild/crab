//! Node-log durability, fleet proofs, and byte admission.

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

#[tokio::test(flavor = "multi_thread")]
async fn runtime_replaces_node_durability_binding_for_a_new_epoch() {
    let fixture = fixture_for(b"node-durability-rotation");
    let session = SessionId::from_bytes([60; 16]);
    let leader = NodeId::from_bytes([61; 16]);
    let follower = NodeId::from_bytes([62; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(TestNodeTransport(None));
    let make_durability = |log_epoch| {
        let gate = DurabilityGate::new(session, leader, log_epoch, [follower]).unwrap();
        let shipper =
            NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
        let authority: Arc<dyn NodeLogAuthority> = Arc::new(TestNodeAuthority::default());
        Arc::new(NodeDurability::new(
            gate,
            shipper,
            authority,
            Arc::clone(&transport),
            lease.clone(),
        ))
    };
    let first = make_durability(1);
    runtime
        .install_node_durability(fixture.target.application(), Arc::clone(&first))
        .unwrap();
    let second = make_durability(2);

    let previous = runtime
        .replace_node_durability(fixture.target.application(), Arc::clone(&second))
        .unwrap();

    assert!(Arc::ptr_eq(&previous, &first));
    let (application, current) = runtime.node_durability().unwrap();
    assert_eq!(application, fixture.target.application());
    assert!(Arc::ptr_eq(&current, &second));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn command_flows_through_follower_proof_object_coverage_and_clean_close() {
    let fixture = fixture_for(b"node-durability-command");
    let session = SessionId::from_bytes([46; 16]);
    let leader = NodeId::from_bytes([47; 16]);
    let follower = NodeId::from_bytes([48; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let gate = DurabilityGate::new(session, leader, 1, [follower]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(TestNodeTransport::default());
    let shipper =
        NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
    let authority = Arc::new(TestNodeAuthority::default());
    let node_authority: Arc<dyn NodeLogAuthority> = authority.clone();
    let durability = Arc::new(NodeDurability::new(
        gate,
        shipper,
        node_authority,
        transport,
        lease,
    ));
    runtime
        .install_node_durability(fixture.target.application(), durability)
        .unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;

    let outcome = handle
        .execute(
            identity(49),
            Digest::from_bytes([50; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"durable".to_vec()))
            },
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        StoredOutcome::Success {
            ref result,
            commit_sequence: 1
        } if result == b"durable"
    ));
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(*authority.coverage.lock().unwrap(), vec![(1, 1)]);
    assert_eq!(*authority.closes.lock().unwrap(), vec![1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn follower_proofs_advance_logical_head_and_bound_the_object_backlog() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let object_store: Arc<dyn ObjectStore> = pausing.clone();
    let fixture = fixture_with_limits_and_store(
        b"dual-head-command",
        Limits::default(),
        Store::new(object_store),
    );
    let session = SessionId::from_bytes([51; 16]);
    let leader = NodeId::from_bytes([52; 16]);
    let follower = NodeId::from_bytes([53; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let gate = DurabilityGate::new(session, leader, 1, [follower]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(TestNodeTransport::default());
    let shipper =
        NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
    let authority = Arc::new(TestNodeAuthority::default());
    let node_authority: Arc<dyn NodeLogAuthority> = authority.clone();
    runtime
        .install_node_durability(
            fixture.target.application(),
            Arc::new(NodeDurability::new(
                gate,
                shipper,
                node_authority,
                transport,
                lease,
            )),
        )
        .unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    pausing.arm();

    let first = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle.execute(
            identity(54),
            Digest::from_bytes([55; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"one".to_vec()))
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.commit_sequence(), 1);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        pausing.wait_until_blocked(),
    )
    .await
    .unwrap();

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle.execute(
            identity(56),
            Digest::from_bytes([57; 32]),
            21,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"two".to_vec()))
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.commit_sequence(), 2);
    let value = handle
        .query(64, 64, |connection| {
            let value = connection
                .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
            Ok(value.to_be_bytes().to_vec())
        })
        .await
        .unwrap();
    assert_eq!(i64::from_be_bytes(value.try_into().unwrap()), 2);

    for sequence in 3_u8..=64 {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            handle.execute(
                identity(sequence.saturating_add(54)),
                Digest::from_bytes([sequence.saturating_add(55); 32]),
                20 + i64::from(sequence),
                1_024,
                1_024,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(outcome.commit_sequence(), u64::from(sequence));
    }
    let sixty_fifth = handle.execute(
        identity(119),
        Digest::from_bytes([120; 32]),
        85,
        1_024,
        1_024,
        |transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(HandlerOutcome::Success(Vec::new()))
        },
    );
    tokio::pin!(sixty_fifth);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut sixty_fifth)
            .await
            .is_err()
    );
    let blocked = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blocked.value().root.as_ref().unwrap().commit_sequence, 0);
    assert!(runtime.stats().unpublished_node_log_bytes() > 0);

    pausing.release();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), &mut sixty_fifth)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.commit_sequence(), 65);
    tokio::time::timeout(std::time::Duration::from_secs(5), handle.drain())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runtime.stats().unpublished_node_log_bytes(), 0);
    runtime.shutdown().await.unwrap();
    let released = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(released.value().root.as_ref().unwrap().commit_sequence, 65);
    assert_eq!(*authority.activations.lock().unwrap(), vec![1]);
    assert!(
        authority
            .coverage
            .lock()
            .unwrap()
            .last()
            .is_some_and(|(epoch, covered)| *epoch == 1 && *covered >= 65)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fleet_proof_retains_owner_when_object_publication_fails_first() {
    let store = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let object_store: Arc<dyn ObjectStore> = store.clone();
    let fixture = fixture_with_limits_and_store(
        b"fleet-object-failure",
        Limits::default(),
        Store::new(object_store),
    );
    let session = SessionId::from_bytes([121; 16]);
    let leader = NodeId::from_bytes([122; 16]);
    let follower = NodeId::from_bytes([123; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let gate = DurabilityGate::new(session, leader, 1, [follower]).unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(TestNodeTransport(Some(
        std::time::Duration::from_millis(50),
    )));
    let shipper =
        NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
    runtime
        .install_node_durability(
            fixture.target.application(),
            Arc::new(NodeDurability::new(
                gate,
                shipper,
                Arc::new(TestNodeAuthority::default()),
                transport,
                lease,
            )),
        )
        .unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    store.fail_puts();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle.execute(
            identity(124),
            Digest::from_bytes([125; 32]),
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"fleet-proof".to_vec()))
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.commit_sequence(), 1);
    tokio::time::timeout(std::time::Duration::from_secs(2), store.wait_until_failed())
        .await
        .unwrap();

    let control = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(control.value().root.as_ref().unwrap().commit_sequence, 0);
    let value = handle
        .query(64, 64, |connection| {
            let value = connection
                .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
            Ok(value.to_be_bytes().to_vec())
        })
        .await
        .unwrap();
    assert_eq!(i64::from_be_bytes(value.try_into().unwrap()), 1);
    assert_eq!(runtime.stats().active_cells(), 1);
    let pending = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.value().state, ControlState::Serving);
    assert_eq!(pending.value().owner.as_ref().unwrap().session, session);
    assert_eq!(pending.value().root.as_ref().unwrap().commit_sequence, 0);
    assert!(runtime.stats().unpublished_node_log_bytes() > 0);
    store.allow_puts();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let control = CellAuthority::new(fixture.layout.clone())
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap();
            if control.value().root.as_ref().unwrap().commit_sequence == 1
                && runtime.stats().unpublished_node_log_bytes() == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
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

#[tokio::test]
async fn node_byte_admission_rejects_before_sql_execution() {
    let fixture = fixture();
    let handle = activate(&fixture, 1024 * 1024).await;
    assert!(matches!(
        handle
            .execute(
                identity(12),
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

#[tokio::test]
async fn post_commit_publication_failure_returns_resolvable_unknown_outcome() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    delete_control_root(&fixture).await;
    let request = identity(14);
    let digest = Digest::from_bytes([15; 32]);
    assert!(matches!(
        handle
            .execute(request, digest, 20, 1_024, 1_024, |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"not-yet-published".to_vec()))
            })
            .await,
        Err(crab_cell_runtime::Error::OutcomeUnknown {
            request_id,
            operation_digest,
            ..
        }) if request_id == request.request_id && operation_digest == digest
    ));
    assert!(matches!(
        handle
            .execute(request, digest, 21, 1_024, 1_024, |_| {
                Ok(HandlerOutcome::Success(Vec::new()))
            })
            .await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    assert_eq!(
        handle.resolve(request, digest, 21, 1_024).await.unwrap(),
        Resolution::Unknown
    );
}
