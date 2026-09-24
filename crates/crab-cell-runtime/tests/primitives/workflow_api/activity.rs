//! Native activity heartbeats and node-loss recovery.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn native_activity_heartbeats_and_recovers_after_node_loss() {
    HEARTBEAT_OBSERVED.store(false, Ordering::Release);
    FAILOVER_ACTIVITY_ENTERED.store(false, Ordering::Release);
    FAILOVER_ACTIVITY_BLOCKED.store(true, Ordering::Release);
    FAILOVER_ACTIVITY_ATTEMPTS.store(0, Ordering::Release);
    let registry = registry();
    assert!(registry.has_blocking_activities());
    assert!(registry.requires_blocking_activity(WORKFLOW_NAMESPACE));
    assert_eq!(
        registry.internal_command_action(WORKFLOW_NAMESPACE, 7, 1),
        Some("cell.scheduler.tick")
    );
    assert_eq!(
        registry.internal_command_action(WORKFLOW_NAMESPACE, 4, 1),
        Some("cell.activity.source")
    );
    assert_eq!(
        registry.internal_query_action(WORKFLOW_NAMESPACE, 2, 1),
        Some("cell.activity.source")
    );
    assert_eq!(
        registry.internal_command_action(WORKFLOW_NAMESPACE, 1, 1),
        None
    );
    let target = CellTarget::new(
        TenantId::from_bytes([21; 16]),
        ApplicationId::from_bytes([22; 16]),
        WORKFLOW_NAMESPACE,
        &0_u32.to_be_bytes(),
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([23; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("activity-runtime"), [22; 16]);
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
                CatalogRole::Workflow,
                registry.module_code(WORKFLOW_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let first_session = SessionId::from_bytes([25; 16]);
    let control = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://activity-first.internal:8081".into(),
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
    let first_database = directory.path().join("activity-first.sqlite");
    let handle = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            control,
            first_database.clone(),
            install_workflow_schema,
        )
        .await
        .unwrap();
    let client = CellClient::local(registry.clone(), handle.clone());
    let workflows = WorkflowNamespace::<TestWorkflow>::new(
        client.clone(),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    workflows
        .start(
            mutation_identity(26),
            b"activity-build".to_vec(),
            b"activity".to_vec(),
        )
        .await
        .unwrap();
    assert!(registry.has_activity_runner(target.namespace()));
    let blocking_pool = BlockingActivityPool::new(1).unwrap();
    assert!(matches!(
        registry
            .run_activity_once(client.clone(), &target, 5_000, None)
            .await,
        Err(
            crab_cell_runtime::primitives::workflow::ActivitySupervisorError::Runtime(
                Error::Capacity("blocking activity slot was not reserved")
            )
        )
    ));
    let completed = registry
        .run_activity_once(client, &target, 5_000, blocking_pool.try_reserve().unwrap())
        .await
        .unwrap();
    let ActivityRunOutcome::Completed { workflow, receipt } = completed else {
        panic!("native activity was not completed");
    };
    assert!(matches!(
        workflow,
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Completed,
            event_sequence: 2,
            ..
        }
    ));
    assert!(HEARTBEAT_OBSERVED.load(Ordering::Acquire));
    let state = workflows
        .state(b"activity-build".to_vec(), Some(receipt))
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(state.state, b"activity-complete");
    assert!(state.result.unwrap().ends_with(b"payload-complete"));
    workflows
        .start(
            mutation_identity(28),
            b"retry-build".to_vec(),
            b"activity-retry".to_vec(),
        )
        .await
        .unwrap();
    assert!(matches!(
        registry
            .run_activity_once(
                CellClient::local(registry.clone(), handle.clone()),
                &target,
                5_000,
                blocking_pool.try_reserve().unwrap(),
            )
            .await
            .unwrap(),
        ActivityRunOutcome::Retrying { .. }
    ));
    workflows
        .start(
            mutation_identity(29),
            b"blocking-build".to_vec(),
            b"activity-blocking".to_vec(),
        )
        .await
        .unwrap();
    let blocking = blocking_pool.try_reserve().unwrap().unwrap();
    assert!(matches!(
        registry
            .run_activity_once(
                CellClient::local(registry.clone(), handle.clone()),
                &target,
                5_000,
                Some(blocking),
            )
            .await
            .unwrap(),
        ActivityRunOutcome::Completed { .. }
    ));
    assert!(
        workflows
            .state(b"blocking-build".to_vec(), None)
            .await
            .unwrap()
            .output
            .unwrap()
            .result
            .unwrap()
            .ends_with(b"payload-blocking")
    );
    workflows
        .start(
            mutation_identity(30),
            b"failover-build".to_vec(),
            b"activity-failover".to_vec(),
        )
        .await
        .unwrap();
    let failover_registry = registry.clone();
    let failover_client = CellClient::local(registry.clone(), handle.clone());
    let failover_target = target.clone();
    let failover_pool = blocking_pool.clone();
    let first_attempt = tokio::spawn(async move {
        // The retry activity may become due before the newly started failover activity.
        // Consume that retry first so this attempt deterministically owns the failover lease.
        loop {
            let reservation = loop {
                if let Some(reservation) = failover_pool.try_reserve().unwrap() {
                    break reservation;
                }
                tokio::task::yield_now().await;
            };
            let outcome = match failover_registry
                .run_activity_once(
                    failover_client.clone(),
                    &failover_target,
                    5_000,
                    Some(reservation),
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    return Err::<
                        ActivityRunOutcome,
                        crab_cell_runtime::primitives::workflow::ActivitySupervisorError,
                    >(error);
                }
            };
            if matches!(
                outcome,
                ActivityRunOutcome::Retrying { .. } | ActivityRunOutcome::Idle { .. }
            ) {
                continue;
            }
            return Ok(outcome);
        }
    });
    // The activity claim crosses the SQL worker and the node-owned callback
    // boundary. Allow one lease interval for a busy multi-crate test runner;
    // the short sleep keeps this readiness wait from monopolizing the runtime.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !FAILOVER_ACTIVITY_ENTERED.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    first_attempt.abort();
    assert!(first_attempt.await.unwrap_err().is_cancelled());
    assert_eq!(FAILOVER_ACTIVITY_ATTEMPTS.load(Ordering::Acquire), 1);
    blocking_pool.shutdown().await.unwrap();
    drop(workflows);
    drop(handle);
    drop(runtime);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match std::fs::remove_file(&first_database) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("dropped node did not release its local SQLite file");

    let stale_owner = authority.load(cell).await.unwrap().unwrap();
    assert_eq!(
        stale_owner.value().owner.as_ref().unwrap().session,
        first_session
    );
    assert!(stale_owner.value().root.is_some());
    let fenced = fence_session(&layout, first_session, SessionId::from_bytes([27; 16])).await;
    let takeover = fenced.direct_takeover().unwrap();
    let second_session = SessionId::from_bytes([27; 16]);
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
            stale_owner,
            takeover,
            crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                layout.clone(),
                Limits::default(),
            ),
            directory.path().join("activity-second.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://activity-second.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_workflows = WorkflowNamespace::<TestWorkflow>::new(
        CellClient::local(registry.clone(), restored.clone()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    assert_eq!(
        restored_workflows
            .state(b"activity-build".to_vec(), Some(receipt))
            .await
            .unwrap()
            .output
            .unwrap()
            .state,
        b"activity-complete"
    );
    // Node-session fencing makes Cell takeover immediate, but activity leases
    // remain valid until their published deadline.
    tokio::time::sleep(Duration::from_millis(5_100)).await;
    let current = authority.load(cell).await.unwrap().unwrap();
    let current_sequence = current.value().root.as_ref().unwrap().commit_sequence;
    let restored_client = CellClient::local(registry.clone(), restored.clone());
    let reclaimed = registry
        .run_maintenance_once(
            restored_client.clone(),
            target.clone(),
            mutation_identity(31),
            MaintenanceTickRequest {
                expected_commit_sequence: current_sequence,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        reclaimed.output,
        MaintenanceTickOutcome::Applied { processed: 1 }
    );
    FAILOVER_ACTIVITY_BLOCKED.store(false, Ordering::Release);
    let restored_blocking_pool = BlockingActivityPool::new(1).unwrap();
    let older_retry = registry
        .run_activity_once(
            restored_client.clone(),
            &target,
            5_000,
            restored_blocking_pool.try_reserve().unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(older_retry, ActivityRunOutcome::Retrying { .. }));
    let failover = registry
        .run_activity_once(
            restored_client,
            &target,
            5_000,
            restored_blocking_pool.try_reserve().unwrap(),
        )
        .await
        .unwrap();
    assert!(
        matches!(failover, ActivityRunOutcome::Completed { .. }),
        "unexpected failover outcome: {failover:?}"
    );
    assert_eq!(FAILOVER_ACTIVITY_ATTEMPTS.load(Ordering::Acquire), 2);
    assert!(
        restored_workflows
            .state(b"failover-build".to_vec(), None)
            .await
            .unwrap()
            .output
            .unwrap()
            .result
            .unwrap()
            .ends_with(b"failover-complete")
    );
    restored_blocking_pool.shutdown().await.unwrap();
    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}
