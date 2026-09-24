//! Authenticated peer migration plans and digest conflicts.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_peer_migration_derives_plan_and_reconciles_retry() {
    let registry = Arc::new(compiled_registry());
    let target = CellTarget::new(
        TenantId::from_bytes([80; 16]),
        ApplicationId::from_bytes([81; 16]),
        NAMESPACE,
        b"peer-code-only",
    )
    .unwrap();
    let incarnation = IncarnationId::from_bytes([82; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("peer-code-only-migration"),
        [81; 16],
    );
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(CatalogEntry::new(&target, CatalogRole::Sql, PREDECESSOR_CODE, 2).unwrap())
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let owner_session = SessionId::from_bytes([83; 16]);
    let initial = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: owner_session,
                endpoint: "https://peer-migration.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 2).unwrap(),
        16 * 1024 * 1024,
        owner_session,
    )
    .unwrap();
    let files = tempfile::TempDir::new().unwrap();
    let replica = CellReplica::new(
        layout,
        *target.cell_id().as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof.clone(),
            replica,
            authority.clone(),
            initial,
            files.path().join("peer-code-only.sqlite"),
            |transaction| {
                transaction.execute_batch(MIGRATION_ONE)?;
                transaction.execute_batch(MIGRATION_TWO)?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let expected = crab_cell_runtime::client::CellDescription {
        cell: target.cell_id(),
        incarnation,
        code: handle.code(),
        schema: handle.schema(),
    };
    let plan = registry
        .next_migration(target.namespace(), expected.code, expected.schema)
        .unwrap()
        .unwrap();
    let signing_session = SessionId::from_bytes([84; 16]);
    let signer = PeerSigner::new(
        signing_session,
        registry.release_digest(),
        ed25519_dalek::SigningKey::from_bytes(&[85; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        signing_session,
        registry.release_digest(),
        signer.verifying_key(),
    ));
    let dispatcher = Arc::new(PeerDispatcher::new(
        Arc::clone(&registry),
        Arc::new(RuntimeResolver {
            target: target.clone(),
            proof: proof.clone(),
            authority: authority.clone(),
            runtime: runtime.clone(),
        }),
        Arc::new(MigrationAuthorizer),
    ));
    let client = MigrationPeerClient::new(
        Arc::new(signer),
        PeerPrincipal {
            issuer: "crab-runtime:test".into(),
            subject: "release-operator".into(),
            actions: vec!["cell.release.migrate".into()],
        },
        Arc::new(LoopbackRoundTrip {
            verifier,
            dispatcher,
        }),
    );

    let migrated = client
        .migrate(target.clone(), expected, plan, 10)
        .await
        .unwrap();
    assert_eq!(migrated.code, registry.module_code(MODULE).unwrap());
    assert_eq!(migrated.schema, 2);
    assert_eq!(
        client.migrate(target, expected, plan, 10).await.unwrap(),
        migrated
    );

    let control = authority.load(proof.entry().cell()).await.unwrap().unwrap();
    runtime
        .local_handle(proof, &control)
        .await
        .unwrap()
        .unwrap()
        .drain()
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn migration_digest_conflict_blocks_activation() {
    let registry = compiled_registry();
    let target = CellTarget::new(
        TenantId::from_bytes([71; 16]),
        ApplicationId::from_bytes([72; 16]),
        NAMESPACE,
        b"migration-conflict",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([73; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("migration-conflict"),
        [72; 16],
    );
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(CatalogEntry::new(&target, CatalogRole::Sql, PREDECESSOR_CODE, 1).unwrap())
        .await
        .unwrap();
    let authority = CellAuthority::new(layout);
    let session = SessionId::from_bytes([74; 16]);
    let initial = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: "https://migration-conflict.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let files = tempfile::TempDir::new().unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            replica,
            authority.clone(),
            initial,
            files.path().join("conflict.sqlite"),
            |transaction| {
                transaction.execute_batch(MIGRATION_ONE)?;
                transaction.execute(
                    "INSERT INTO sys_migrations(version, digest, applied_sequence) VALUES (2, ?1, 0)",
                    [[99_u8; 32].as_slice()],
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let before = authority.load(cell).await.unwrap().unwrap();
    let plan = registry
        .next_migration(NAMESPACE, handle.code(), handle.schema())
        .unwrap()
        .unwrap();
    assert!(matches!(
        handle.migrate(plan, 10).await,
        Err(crab_cell_runtime::Error::Registry(
            "migration digest conflicts with SQLite history"
        ))
    ));
    let after = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let current = authority.load(cell).await.unwrap().unwrap();
            if current.value().state == ControlState::Idle {
                break current;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(after.value().incarnation, before.value().incarnation);
    assert_eq!(after.value().code, before.value().code);
    assert_eq!(after.value().schema, before.value().schema);
    assert_eq!(after.value().root, before.value().root);
    assert!(after.value().owner.is_none());
    assert!(matches!(
        handle.query(1, 1, |_| Ok(Vec::new())).await,
        Err(crab_cell_runtime::Error::CellDraining) | Err(crab_cell_runtime::Error::Fenced)
    ));
    runtime.shutdown().await.unwrap();
}
