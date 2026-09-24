//! Idle receiver failure and stop-acquiring behaviour.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn failed_idle_receiver_does_not_leave_authority_owned() {
    let fixture = fixture_for(b"receiver-activation-failure");
    let session = SessionId::from_bytes([112; 16]);
    let first_runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&first_runtime, &fixture, session).await;
    handle.drain().await.unwrap();
    first_runtime.shutdown().await.unwrap();

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let expected_root = idle.value().root.clone();
    let successor = SessionId::from_bytes([113; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let missing_parent = fixture
        ._directory
        .path()
        .join("receiver-parent-does-not-exist")
        .join("receiver.sqlite");
    assert!(
        runtime
            .acquire_idle_restored(
                proof,
                fixture.replica.clone(),
                authority.clone(),
                idle,
                missing_parent,
                Owner {
                    session: successor,
                    endpoint: "https://receiver-failure.internal:8081".into(),
                },
            )
            .await
            .is_err()
    );

    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value().state, ControlState::Idle);
    assert!(current.value().owner.is_none());
    assert_eq!(current.value().root, expected_root);
    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn stop_acquiring_keeps_existing_cell_serving() {
    let first = fixture_for(b"scale-down-serving");
    let second = fixture_for(b"scale-down-new");
    let session = SessionId::from_bytes([96; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 2).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &first, session).await;
    runtime.stop_acquiring().unwrap();
    assert!(!runtime.is_acquiring());
    assert_eq!(
        handle
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        0_i64.to_be_bytes()
    );
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        second.layout.clone(),
        second.target.tenant(),
    );
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &second.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(second.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let result = runtime
        .bootstrap(
            proof,
            second.replica.clone(),
            authority,
            observed,
            second._directory.path().join("blocked.sqlite"),
            |_| Ok(()),
        )
        .await;
    assert!(matches!(
        result,
        Err(crab_cell_runtime::Error::CellDraining)
    ));
    assert_eq!(runtime.unreleased_cell_count().await.unwrap(), 1);
    handle.drain().await.unwrap();
    assert_eq!(runtime.unreleased_cell_count().await.unwrap(), 0);
    runtime.shutdown().await.unwrap();
}
