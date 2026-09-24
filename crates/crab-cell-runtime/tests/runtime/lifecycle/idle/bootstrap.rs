//! Failed and panicking bootstrap capacity release.

use super::*;

#[tokio::test]
async fn failed_bootstrap_keeps_control_unpublished_and_releases_cell_capacity() {
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
    let session = SessionId::from_bytes([4; 16]);
    let authority = CellAuthority::new(fixture.layout.clone());
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
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 2 * 1024 * 1024, session).unwrap();
    assert!(matches!(
        runtime
            .bootstrap(
                proof.clone(),
                fixture.replica.clone(),
                authority.clone(),
                observed,
                fixture.database.clone(),
                |transaction| {
                    transaction.execute("CREATE TABLE should_rollback(value INTEGER)", [])?;
                    Err(crab_cell_runtime::Error::Command("migration rejected"))
                },
            )
            .await,
        Err(crab_cell_runtime::Error::Command("migration rejected"))
    ));
    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value().state, ControlState::Recovering);
    assert!(current.value().root.is_none());

    let handle = runtime
        .bootstrap(
            proof,
            fixture.replica,
            authority,
            current,
            fixture.database.with_file_name("replacement.sqlite"),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();
}
#[tokio::test]
async fn panicking_bootstrap_keeps_worker_alive_and_releases_cell_capacity() {
    let fixture = fixture_for(b"panicking-bootstrap");
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
    let session = SessionId::from_bytes([4; 16]);
    let authority = CellAuthority::new(fixture.layout.clone());
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
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 2 * 1024 * 1024, session).unwrap();

    assert!(matches!(
        runtime
            .bootstrap(
                proof.clone(),
                fixture.replica.clone(),
                authority.clone(),
                observed,
                fixture.database.clone(),
                |_| panic!("bootstrap initializer panic"),
            )
            .await,
        Err(crab_cell_runtime::Error::NativePanic)
    ));
    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value().state, ControlState::Recovering);
    assert!(current.value().root.is_none());

    let handle = runtime
        .bootstrap(
            proof,
            fixture.replica,
            authority,
            current,
            fixture.database.with_file_name("panic-replacement.sqlite"),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();
}
