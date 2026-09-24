//! Exact-root restore of a published workflow on a new owner.

use super::*;

#[tokio::test]
async fn published_workflow_restores_from_exact_root_on_a_new_owner() {
    let namespace = NamespaceId::from_bytes([6; 16]);
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        namespace,
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
                Digest::from_bytes([5; 32]),
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
            install_workflow_schema,
        )
        .await
        .unwrap();
    let definition = Definition {
        digest: Digest::from_bytes([5; 32]),
    };
    let request = start(7);
    let workflow_target = target.clone();
    handle
        .execute(
            MutationIdentity {
                request_id: RequestId::from_bytes([8; 16]),
                issued_at_ms: 10,
                expires_at_ms: 10_000,
            },
            Digest::from_bytes([9; 32]),
            10,
            128,
            128,
            move |transaction| match workflow_start(
                transaction,
                &workflow_target,
                10,
                &request,
                &definition,
            )? {
                WorkflowOutcome::Applied { run_id, .. } => {
                    Ok(HandlerOutcome::Success(run_id.to_vec()))
                }
                _ => Ok(HandlerOutcome::Rejected(b"workflow exists".to_vec())),
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
    assert_eq!(idle.value().next_due_ms, Some(10));
    let second_session = SessionId::from_bytes([11; 16]);
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
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let count = connection.query_row(
                    "SELECT count(*) FROM workflow_runs WHERE status = 0 AND event_sequence = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )?;
                Ok(vec![count as u8])
            })
            .await
            .unwrap(),
        vec![1]
    );
    let claimed = Arc::new(std::sync::Mutex::new(None));
    let observed = claimed.clone();
    restored
        .execute(
            MutationIdentity {
                request_id: RequestId::from_bytes([12; 16]),
                issued_at_ms: 11,
                expires_at_ms: 10_000,
            },
            Digest::from_bytes([13; 32]),
            11,
            128,
            512,
            move |transaction| {
                let mut tokens = Tokens(20);
                let claim = workflow_claim_activities(
                    transaction,
                    11,
                    1,
                    5_000,
                    &[ActivitySupport {
                        activity_type: "email".into(),
                        definition_digest: Digest::from_bytes([5; 32]),
                    }],
                    &mut tokens,
                )?
                .into_iter()
                .next()
                .ok_or(crab_cell_runtime::Error::Command(
                    "missing restored workflow activity",
                ))?;
                *observed.lock().unwrap() = Some(claim);
                Ok(HandlerOutcome::Success(b"claimed".to_vec()))
            },
        )
        .await
        .unwrap();
    let claim = claimed.lock().unwrap().clone().unwrap();
    assert_eq!(
        restored
            .query(64, 64, {
                let claim = claim.clone();
                move |connection| {
                    Ok(vec![u8::from(workflow_validate_activity_claim(
                        connection,
                        12,
                        &[claim],
                    )?)])
                }
            })
            .await
            .unwrap(),
        vec![1]
    );
    let workflow_target = target.clone();
    restored
        .execute(
            MutationIdentity {
                request_id: RequestId::from_bytes([14; 16]),
                issued_at_ms: 13,
                expires_at_ms: 10_000,
            },
            Digest::from_bytes([15; 32]),
            13,
            128,
            128,
            move |transaction| {
                let completion = ActivityCompletion {
                    run_id: claim.run_id,
                    activity_id: claim.activity_id,
                    attempt: claim.attempt,
                    lease_token: claim.token,
                    completion_token: [16; 16],
                    result: b"sent".to_vec(),
                    failed: false,
                    retryable: false,
                };
                match workflow_complete_activity(
                    transaction,
                    &workflow_target,
                    13,
                    &completion,
                    &Definition {
                        digest: Digest::from_bytes([5; 32]),
                    },
                )? {
                    ActivityCompletionOutcome::Applied(_) => {
                        Ok(HandlerOutcome::Success(b"completed".to_vec()))
                    }
                    _ => Ok(HandlerOutcome::Rejected(b"completion rejected".to_vec())),
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let state =
                    connection.query_row("SELECT state FROM workflow_activities", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                let events =
                    connection.query_row("SELECT count(*) FROM workflow_events", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                Ok(vec![state as u8, events as u8])
            })
            .await
            .unwrap(),
        vec![2, 2]
    );
    restored.drain().await.unwrap();
}
