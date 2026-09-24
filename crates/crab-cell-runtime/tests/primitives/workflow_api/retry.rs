//! Published-report retry after a lost response.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn published_report_survives_a_lost_response_and_completes_on_retry() {
    let registry = registry();
    let target = CellTarget::new(
        TenantId::from_bytes([51; 16]),
        ApplicationId::from_bytes([52; 16]),
        WORKFLOW_NAMESPACE,
        &0_u32.to_be_bytes(),
    )
    .unwrap();
    let incarnation = IncarnationId::from_bytes([53; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("report-publication"),
        [52; 16],
    );
    let replica = CellReplica::new(
        layout.clone(),
        *target.cell_id().as_bytes(),
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
    let authority = CellAuthority::new(layout);
    let session = SessionId::from_bytes([54; 16]);
    let control = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: "https://report-worker.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let report_path = directory.path().join("build-report.txt");
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            replica,
            authority,
            control,
            directory.path().join("workflow.sqlite"),
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
    let mut event = b"publish-report\0".to_vec();
    event.extend_from_slice(report_path.to_str().unwrap().as_bytes());
    workflows
        .start(mutation_identity(55), b"build-report".to_vec(), event)
        .await
        .unwrap();
    let pool = BlockingActivityPool::new(1).unwrap();
    let first = registry
        .run_activity_once(client.clone(), &target, 5_000, pool.try_reserve().unwrap())
        .await
        .unwrap();
    assert!(matches!(first, ActivityRunOutcome::Retrying { .. }));
    let published = std::fs::read(&report_path).unwrap();
    assert!(published.starts_with(b"build report\nrequest="));
    assert_eq!(
        workflows
            .state(b"build-report".to_vec(), None)
            .await
            .unwrap()
            .output
            .unwrap()
            .status,
        WorkflowStatus::Running
    );

    tokio::time::sleep(Duration::from_millis(250)).await;
    let second = registry
        .run_activity_once(client, &target, 5_000, pool.try_reserve().unwrap())
        .await
        .unwrap();
    let ActivityRunOutcome::Completed { receipt, .. } = second else {
        panic!("report activity did not complete after retry: {second:?}");
    };
    let state = workflows
        .state(b"build-report".to_vec(), Some(receipt))
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(state.status, WorkflowStatus::Completed);
    let result = state.result.unwrap();
    assert_eq!(result.len(), 62);
    assert_eq!(&result[..9], b"activity\0");
    assert_eq!(result[9], 0);
    assert_eq!(
        published,
        format!("build report\nrequest={:02x?}\n", &result[30..]).into_bytes()
    );
    assert_eq!(std::fs::read(report_path).unwrap(), published);
    pool.shutdown().await.unwrap();
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}
