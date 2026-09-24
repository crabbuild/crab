//! Follower and fleet proofs, object publication, and binding epochs.

use super::*;

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
