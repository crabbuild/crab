//! Node-lease fencing for a live Cell runtime.

use super::*;

#[tokio::test]
async fn fleet_runtime_stays_fenced_until_one_live_node_lease_is_installed() {
    let session = SessionId::from_bytes([41; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        1_024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();

    assert!(matches!(
        runtime.try_reserve_node_bytes(1),
        Err(crab_cell_runtime::Error::Fenced)
    ));
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    assert!(runtime.install_node_lease(lease.clone()).is_err());
    drop(runtime.try_reserve_node_bytes(1).unwrap());

    lease.fence();
    assert!(matches!(
        runtime.try_reserve_node_bytes(1),
        Err(crab_cell_runtime::Error::Fenced)
    ));
    runtime.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn node_lease_expiry_hides_an_inflight_committed_command() {
    let fixture = fixture_for(b"node-lease-output-gate");
    let session = SessionId::from_bytes([42; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();

    let command = tokio::spawn(async move {
        handle
            .execute(
                mutation_identity_window(43, 10, 10_000),
                Digest::from_bytes([44; 32]),
                20,
                1,
                16,
                move |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    entered_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    Ok(HandlerOutcome::Success(b"hidden".to_vec()))
                },
            )
            .await
    });
    tokio::task::spawn_blocking(move || {
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
    })
    .await
    .unwrap();
    lease.fence();
    resume_tx.send(()).unwrap();

    match command.await.unwrap() {
        Err(crab_cell_runtime::Error::OutcomeUnknown { source, .. }) => {
            assert!(matches!(*source, crab_cell_runtime::Error::Fenced));
        }
        other => panic!("unexpected command result: {other:?}"),
    }
    let control = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(control.value().root.as_ref().unwrap().commit_sequence, 0);
    assert!(matches!(
        runtime.shutdown().await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
}
#[tokio::test]
async fn activation_rejects_control_owned_by_another_node_session() {
    let fixture = fixture();
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session: SessionId::from_bytes([4; 16]),
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        SessionId::from_bytes([9; 16]),
    )
    .unwrap();
    assert!(matches!(
        runtime
            .bootstrap(
                proof,
                fixture.replica,
                authority,
                observed,
                fixture.database,
                |_| Ok(()),
            )
            .await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
}
