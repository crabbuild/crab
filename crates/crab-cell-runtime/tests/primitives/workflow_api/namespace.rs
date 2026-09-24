//! Typed workflow namespace publication, rejection, reads, and restore.

use super::*;

#[tokio::test]
async fn typed_workflow_namespace_publishes_rejects_reads_and_survives_restore() {
    let registry = registry();
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        WORKFLOW_NAMESPACE,
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
                CatalogRole::Workflow,
                registry.module_code(WORKFLOW_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let first_session = SessionId::from_bytes([4; 16]);
    let control = authority
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
            control,
            directory.path().join("first.sqlite"),
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
    let started = workflows
        .start(identity(7), b"build-42".to_vec(), b"start".to_vec())
        .await
        .unwrap();
    let WorkflowOutcome::Applied { run_id, .. } = started.output else {
        panic!("workflow start was not applied");
    };
    let signal = WorkflowSignal {
        workflow_id: b"build-42".to_vec(),
        run_id,
        signal_id: [8; 16],
        event: b"continue".to_vec(),
    };
    let signalled = workflows.signal(identity(9), signal.clone()).await.unwrap();
    assert!(matches!(
        signalled.output,
        WorkflowOutcome::Applied {
            event_sequence: 2,
            ..
        }
    ));
    assert!(matches!(
        workflows
            .signal(identity(10), signal.clone())
            .await
            .unwrap()
            .output,
        WorkflowOutcome::Duplicate {
            event_sequence: 2,
            ..
        }
    ));
    let mut conflicting = signal;
    conflicting.event = b"different".to_vec();
    let conflict = workflows.signal(identity(11), conflicting).await;
    assert!(matches!(
        conflict,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == WorkflowOutcome::IdentityConflict
    ));
    let state = workflows
        .state(b"build-42".to_vec(), Some(signalled.receipt))
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(state.state, b"continue");
    assert_eq!(state.event_sequence, 2);
    let timer = workflows
        .start(identity(16), b"timer-build".to_vec(), b"timer".to_vec())
        .await
        .unwrap();
    let mut due = DueCellScan::new(&catalog, authority.clone(), target.cell_id().as_bytes()[0])
        .await
        .unwrap();
    let due = due.next_batch(i64::MAX).await.unwrap().unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(
        due[0]
            .control()
            .value()
            .root
            .as_ref()
            .unwrap()
            .commit_sequence,
        timer.receipt.commit_sequence
    );
    let routed = runtime
        .local_handle(due[0].catalog().clone(), due[0].control())
        .await
        .unwrap()
        .unwrap();
    let scheduler_client = CellClient::local(registry.clone(), routed);
    let tick = registry
        .run_maintenance_once(
            scheduler_client.clone(),
            target.clone(),
            identity(17),
            MaintenanceTickRequest {
                expected_commit_sequence: timer.receipt.commit_sequence,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        tick.output,
        MaintenanceTickOutcome::Applied { processed: 1 }
    );
    assert_eq!(
        workflows
            .state(b"timer-build".to_vec(), Some(tick.receipt))
            .await
            .unwrap()
            .output
            .unwrap()
            .state,
        b"timer-complete"
    );
    let stale = registry
        .run_maintenance_once(
            scheduler_client,
            target.clone(),
            identity(18),
            MaintenanceTickRequest {
                expected_commit_sequence: timer.receipt.commit_sequence,
            },
        )
        .await
        .unwrap();
    assert_eq!(stale.output, MaintenanceTickOutcome::Stale);
    handle.drain().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
    let second_session = SessionId::from_bytes([12; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            replica,
            authority,
            idle,
            directory.path().join("second.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://second.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_workflows = WorkflowNamespace::<TestWorkflow>::new(
        CellClient::local(registry, restored.clone()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    assert_eq!(
        restored_workflows
            .state(b"build-42".to_vec(), Some(signalled.receipt))
            .await
            .unwrap()
            .output
            .unwrap()
            .state,
        b"continue"
    );
    let cancelled = restored_workflows
        .cancel(
            identity(13),
            WorkflowSignal {
                workflow_id: b"build-42".to_vec(),
                run_id,
                signal_id: [14; 16],
                event: b"cancel".to_vec(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        cancelled.output,
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Cancelled,
            event_sequence: 3,
            ..
        }
    ));
    restored.drain().await.unwrap();
}
