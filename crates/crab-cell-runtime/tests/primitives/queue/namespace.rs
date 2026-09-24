//! Typed queue namespace recovery after owner loss.

use super::*;

#[tokio::test]
async fn typed_queue_namespace_recovers_after_owner_loss() {
    let registry = queue_registry();
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        QUEUE_NAMESPACE,
        &0_u32.to_be_bytes(),
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([2; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("runtime"), [3; 16]);
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Queue,
                registry.module_code(QUEUE_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let first_session = SessionId::from_bytes([4; 16]);
    let observed = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://first.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            observed,
            directory.path().join("first.sqlite"),
            install_queue_schema,
        )
        .await
        .unwrap();
    let queue = QueueNamespace::<TestQueue>::new(
        CellClient::local(registry.clone(), handle.clone()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    let available_at_ms = now_ms() + 100;
    let sent = queue
        .send(
            mutation_identity(7),
            QueueSendRequest {
                producer_id: [7; 16],
                payload: b"job".to_vec(),
                available_at_ms,
            },
        )
        .await
        .unwrap();
    assert!(matches!(sent.output, QueueSendOutcome::Sent { .. }));
    let conflict = queue
        .send(
            mutation_identity(8),
            QueueSendRequest {
                producer_id: [7; 16],
                payload: b"different".to_vec(),
                available_at_ms,
            },
        )
        .await;
    assert!(matches!(
        conflict,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == QueueSendOutcome::ProducerConflict
    ));
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let claimed = queue
        .claim(
            mutation_identity(9),
            0,
            QueueClaimRequest {
                limit: 1,
                lease_ms: 10_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed.output.len(), 1);
    assert_eq!(claimed.output[0].payload, b"job");
    assert_eq!(
        authority
            .load(cell)
            .await
            .unwrap()
            .unwrap()
            .value()
            .next_due_ms,
        Some(claimed.output[0].lease_until_ms)
    );
    assert!(
        queue
            .validate_claim(0, claimed.output.clone(), Some(claimed.receipt))
            .await
            .unwrap()
            .output
    );
    drop(queue);
    drop(handle);
    drop(runtime);
    let stale = authority.load(cell).await.unwrap().unwrap();
    let second_session = SessionId::from_bytes([11; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            proof,
            replica,
            authority.clone(),
            stale,
            crate::support::fencing::fence_session(&layout, first_session, second_session)
                .await
                .direct_takeover()
                .unwrap(),
            crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                layout.clone(),
                Limits::default(),
            ),
            directory.path().join("second.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://second.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_queue = QueueNamespace::<TestQueue>::new(
        CellClient::local(registry, restored.clone()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    assert!(
        restored_queue
            .validate_claim(0, claimed.output.clone(), Some(claimed.receipt))
            .await
            .unwrap()
            .output
    );
    let acked = restored_queue
        .ack(
            mutation_identity(10),
            0,
            claimed.output[0].message_id,
            claimed.output[0].token,
        )
        .await
        .unwrap();
    assert_eq!(
        acked.output,
        QueueLeaseOutcome::Applied {
            state: QueueState::Acked,
            lease_until_ms: None,
        }
    );
    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}
