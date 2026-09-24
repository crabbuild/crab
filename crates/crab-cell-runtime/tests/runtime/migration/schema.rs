//! Capability and code-only migration publication with exact-root restore.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn migration_replaces_capability_publishes_schema_and_restores_exact_root() {
    let registry = compiled_registry();
    let code = registry.module_code(MODULE).unwrap();
    assert!(registry.supports_cell(NAMESPACE, CatalogRole::Sql, PREDECESSOR_CODE, 1));
    assert!(registry.supports_cell(NAMESPACE, CatalogRole::Sql, PREDECESSOR_CODE, 2));
    assert!(!registry.is_current_cell(NAMESPACE, CatalogRole::Sql, PREDECESSOR_CODE, 2));
    assert!(registry.is_current_cell(NAMESPACE, CatalogRole::Sql, code, 2));
    assert!(registry.module_digests().contains(&PREDECESSOR_CODE));
    assert!(registry.module_digests().contains(&code));
    let target = CellTarget::new(
        TenantId::from_bytes([64; 16]),
        ApplicationId::from_bytes([65; 16]),
        NAMESPACE,
        b"schema-step",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([66; 16]);
    let object_store = Arc::new(PausedPutStore::new());
    let layout = CellStorageLayout::new(
        Store::new(object_store.clone()),
        Path::from("migration-runtime"),
        [65; 16],
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
    let first_session = SessionId::from_bytes([67; 16]);
    let control = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://migration-first.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let files = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 2).unwrap(),
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
            files.path().join("schema-one.sqlite"),
            |transaction| {
                transaction.execute_batch(MIGRATION_ONE)?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let follower_directory = tempfile::TempDir::new().unwrap();
    let follower = NodeId::from_bytes([84; 16]);
    let follower_store = crab_cell_runtime::FollowerStore::open(
        follower_directory.path().to_owned(),
        Limits::default(),
        crab_cell_runtime::ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    let transport: Arc<dyn NodeLogTransport> =
        Arc::new(LocalFollowerTransport::new(follower, follower_store));
    let gate =
        DurabilityGate::new(first_session, NodeId::from_bytes([85; 16]), 1, [follower]).unwrap();
    let shipper =
        NodeLogShipper::new(gate.clone(), Arc::clone(&transport), Limits::default()).unwrap();
    let durability = Arc::new(NodeDurability::new(
        gate,
        shipper,
        Arc::new(MigrationNodeAuthority),
        transport,
        NodeLeaseGuard::new(0, 60_000).unwrap(),
    ));
    runtime
        .install_node_durability(target.application(), durability)
        .unwrap();
    let old_handle = handle.clone();
    let plan = registry
        .next_migration(NAMESPACE, handle.code(), handle.schema())
        .unwrap()
        .unwrap();
    assert_eq!(plan.from_code(), PREDECESSOR_CODE);
    assert_eq!(plan.to_code(), code);
    let migration_digest = plan.digest().unwrap();
    object_store.pause_next();
    let migrated =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.migrate(plan, 10))
            .await
            .unwrap()
            .unwrap();
    object_store.started.wait().await;
    let queued_handle = migrated.handle.clone();
    let mut queued = tokio::spawn(async move {
        queued_handle
            .query(1, 8, |connection| {
                Ok(connection
                    .query_row("SELECT schema_version FROM sys_meta", [], |row| {
                        row.get::<_, u32>(0)
                    })?
                    .to_be_bytes()
                    .to_vec())
            })
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut queued)
            .await
            .is_err()
    );
    object_store.release.wait().await;
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), queued)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        2_u32.to_be_bytes()
    );
    assert_eq!(migrated.outcome.code, code);
    assert_eq!(migrated.outcome.schema, 2);
    assert_eq!(migrated.outcome.commit_sequence, 1);
    assert!(matches!(
        old_handle.query(1, 1, |_| Ok(Vec::new())).await,
        Err(crab_cell_runtime::Error::CellDraining)
    ));
    assert!(
        registry
            .next_migration(NAMESPACE, migrated.handle.code(), migrated.handle.schema())
            .unwrap()
            .is_none()
    );
    let code_only = registry
        .next_migration(NAMESPACE, PREDECESSOR_CODE, 2)
        .unwrap()
        .unwrap();
    assert_eq!(code_only.from_schema(), code_only.to_schema());
    assert_eq!(code_only.from_code(), PREDECESSOR_CODE);
    assert_eq!(code_only.to_code(), code);
    assert!(code_only.sql().is_none());
    let metadata = migrated
        .handle
        .query(64, 128, |connection| {
            let schema = connection.query_row(
                "SELECT schema_version FROM sys_meta WHERE singleton = 1",
                [],
                |row| row.get::<_, u32>(0),
            )?;
            let (digest, sequence) = connection.query_row(
                "SELECT digest, applied_sequence FROM sys_migrations WHERE version = 2",
                [],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u64>(1)?)),
            )?;
            let mut result = schema.to_be_bytes().to_vec();
            result.extend_from_slice(&sequence.to_be_bytes());
            result.extend_from_slice(&digest);
            Ok(result)
        })
        .await
        .unwrap();
    assert_eq!(&metadata[..4], &2_u32.to_be_bytes());
    assert_eq!(&metadata[4..12], &1_u64.to_be_bytes());
    assert_eq!(&metadata[12..], migration_digest.as_bytes());
    let written = migrated
        .handle
        .execute(
            crab_cell_runtime::cell::executor::MutationIdentity {
                request_id: crab_cell_runtime::identity::RequestId::from_bytes([68; 16]),
                issued_at_ms: 10,
                expires_at_ms: 1_000,
            },
            Digest::from_bytes([69; 32]),
            20,
            128,
            64,
            |transaction| {
                transaction.execute(
                    "INSERT INTO records(id, value, label) VALUES (1, x'01', 'migrated')",
                    [],
                )?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    assert_eq!(written.commit_sequence(), 2);
    migrated.handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
    assert_eq!(idle.value().schema, 2);
    assert_eq!(idle.value().code, code);
    assert_eq!(idle.value().root.as_ref().unwrap().commit_sequence, 2);
    let second_session = SessionId::from_bytes([70; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 2).unwrap(),
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
            files.path().join("schema-two.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://migration-second.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(restored.schema(), 2);
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                Ok(
                    connection.query_row("SELECT label FROM records WHERE id = 1", [], |row| {
                        row.get::<_, String>(0).map(String::into_bytes)
                    })?,
                )
            })
            .await
            .unwrap(),
        b"migrated"
    );
    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn code_only_migration_publishes_new_code_without_schema_ledger_entry() {
    let registry = compiled_registry();
    let code = registry.module_code(MODULE).unwrap();
    let target = CellTarget::new(
        TenantId::from_bytes([75; 16]),
        ApplicationId::from_bytes([76; 16]),
        NAMESPACE,
        b"code-only",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([77; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("code-only-migration-runtime"),
        [76; 16],
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
        .provision(CatalogEntry::new(&target, CatalogRole::Sql, PREDECESSOR_CODE, 2).unwrap())
        .await
        .unwrap();
    let authority = CellAuthority::new(layout);
    let session = SessionId::from_bytes([78; 16]);
    let control = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: "https://code-only.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let files = tempfile::TempDir::new().unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 2).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            replica,
            authority.clone(),
            control,
            files.path().join("code-only.sqlite"),
            |transaction| {
                transaction.execute_batch(MIGRATION_ONE)?;
                transaction.execute_batch(MIGRATION_TWO)?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let old_handle = handle.clone();
    let plan = registry
        .next_migration(NAMESPACE, handle.code(), handle.schema())
        .unwrap()
        .unwrap();
    assert_eq!(plan.from_code(), PREDECESSOR_CODE);
    assert_eq!(plan.to_code(), code);
    assert_eq!(plan.from_schema(), 2);
    assert_eq!(plan.to_schema(), 2);
    assert_eq!(plan.digest(), None);
    assert_eq!(plan.sql(), None);

    let migrated = handle.migrate(plan, 10).await.unwrap();
    assert_eq!(migrated.outcome.code, code);
    assert_eq!(migrated.outcome.schema, 2);
    assert_eq!(migrated.outcome.commit_sequence, 1);
    assert!(matches!(
        old_handle.query(1, 1, |_| Ok(Vec::new())).await,
        Err(crab_cell_runtime::Error::CellDraining)
    ));
    let metadata = migrated
        .handle
        .query(64, 64, |connection| {
            let schema = connection.query_row(
                "SELECT schema_version FROM sys_meta WHERE singleton = 1",
                [],
                |row| row.get::<_, u32>(0),
            )?;
            let migrations =
                connection.query_row("SELECT COUNT(*) FROM sys_migrations", [], |row| {
                    row.get::<_, u32>(0)
                })?;
            let mut result = schema.to_be_bytes().to_vec();
            result.extend_from_slice(&migrations.to_be_bytes());
            Ok(result)
        })
        .await
        .unwrap();
    assert_eq!(&metadata[..4], &2_u32.to_be_bytes());
    assert_eq!(&metadata[4..], &0_u32.to_be_bytes());
    let published = authority.load(cell).await.unwrap().unwrap();
    assert_eq!(published.value().code, code);
    assert_eq!(published.value().schema, 2);
    assert_eq!(published.value().root.as_ref().unwrap().commit_sequence, 1);

    migrated.handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}
