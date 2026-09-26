//! Database admission includes runtime receipts and persists across owner changes.

use super::*;
use crab_cell_runtime::cell::actor::CellHandle;
use crab_cell_runtime::primitives::capacity::SCHEMA;

const RESULT_BYTES: usize = 100 * 1024;

async fn hold_capacity(handle: &CellHandle) {
    handle.execute(
        mutation_identity_window(80, 10, 10_000), Digest::from_bytes([80; 32]),
        20, 1024, 1024, |transaction| {
            transaction.execute_batch(SCHEMA)?;
            transaction.execute_batch(
                "INSERT INTO capacity_reservations VALUES(X'01', 96); UPDATE capacity_total SET pages = 96;",
            )?;
            Ok(HandlerOutcome::Success(Vec::new()))
        },
    ).await.unwrap();
}

fn release_sql(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> crab_cell_runtime::Result<()> {
    transaction
        .execute_batch("DELETE FROM capacity_reservations; UPDATE capacity_total SET pages = 0;")?;
    Ok(())
}

async fn assert_held(handle: &CellHandle) {
    let observed = handle.query(64, 64, |connection| {
        let values = connection.query_row(
            "SELECT value, (SELECT pages FROM capacity_total), (SELECT SUM(pages) FROM capacity_reservations) FROM counter", [],
            |row| Ok([row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?]),
        )?;
        Ok(values.into_iter().flat_map(i64::to_be_bytes).collect())
    }).await.unwrap();
    assert_eq!(
        observed,
        [0_i64, 96, 96]
            .into_iter()
            .flat_map(i64::to_be_bytes)
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reservation_survives_receipt_refusal_failed_release_and_owner_restore() {
    let fixture = fixture_with_limits(
        b"reserved-command",
        Limits {
            max_database_bytes: 512 * 1024,
            ..Limits::default()
        },
    );
    let (runtime, handle, _pool) = activate_runtime(&fixture, 16 * 1024 * 1024).await;
    hold_capacity(&handle).await;
    let failed_identity = mutation_identity_window(81, 10, 10_000);
    let failed_digest = Digest::from_bytes([81; 32]);
    let failed = handle
        .execute(
            failed_identity,
            failed_digest,
            21,
            1024,
            RESULT_BYTES,
            |transaction| {
                transaction.execute("UPDATE counter SET value = 1", [])?;
                Ok(HandlerOutcome::Success(vec![1; RESULT_BYTES]))
            },
        )
        .await;
    assert!(matches!(failed, Err(crab_cell_runtime::Error::Capacity(_))));
    assert_held(&handle).await;
    assert_eq!(
        handle
            .resolve(failed_identity, failed_digest, 22, RESULT_BYTES)
            .await
            .unwrap(),
        Resolution::Absent
    );

    for (request, reject) in [(82, false), (83, true)] {
        let result = handle
            .execute(
                mutation_identity_window(request, 10, 10_000),
                Digest::from_bytes([request; 32]),
                23,
                1024,
                1024,
                move |transaction| {
                    release_sql(transaction)?;
                    transaction.execute("UPDATE counter SET value = 99", [])?;
                    if reject {
                        return Ok(HandlerOutcome::Rejected(Vec::new()));
                    }
                    Err(crab_cell_runtime::Error::Command("injected after release"))
                },
            )
            .await;
        if reject {
            assert!(matches!(result, Ok(StoredOutcome::Rejected { .. })));
        } else {
            assert!(matches!(
                result,
                Err(crab_cell_runtime::Error::Command("injected after release"))
            ));
        }
        assert_held(&handle).await;
    }
    runtime.shutdown().await.unwrap();
    let session = SessionId::from_bytes([5; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(2, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
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
    let observed = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            observed,
            fixture._directory.path().join("capacity-restored.sqlite"),
            Owner {
                session,
                endpoint: "https://capacity-restored.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_held(&restored).await;
    assert!(matches!(
        restored
            .execute(
                failed_identity,
                failed_digest,
                24,
                1024,
                RESULT_BYTES,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = 1", [])?;
                    Ok(HandlerOutcome::Success(vec![1; RESULT_BYTES]))
                },
            )
            .await,
        Err(crab_cell_runtime::Error::Capacity(_))
    ));
    let committed = restored
        .execute(
            mutation_identity_window(84, 10, 10_000),
            Digest::from_bytes([84; 32]),
            25,
            1024,
            RESULT_BYTES,
            |transaction| {
                release_sql(transaction)?;
                transaction.execute("UPDATE counter SET value = 1", [])?;
                Ok(HandlerOutcome::Success(vec![1; RESULT_BYTES]))
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        committed,
        StoredOutcome::Success {
            commit_sequence: 3,
            ..
        }
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn reservation_protects_effect_inbox_and_allows_retry_after_release() {
    let fixture = fixture_with_limits(
        b"reserved-effect",
        Limits {
            max_database_bytes: 512 * 1024,
            ..Limits::default()
        },
    );
    let (runtime, handle, _pool) = activate_runtime(&fixture, 16 * 1024 * 1024).await;
    hold_capacity(&handle).await;
    let delivery = InboxDelivery {
        effect_id: [85; 32],
        operation_digest: Digest::from_bytes([85; 32]),
        expires_at_ms: 10_000,
    };
    let apply = |transaction: &crab_ltx::rusqlite::Transaction<'_>| {
        transaction.execute("UPDATE counter SET value = value + 1", [])?;
        Ok(HandlerOutcome::Success(vec![1; RESULT_BYTES]))
    };
    assert!(matches!(
        handle
            .deliver_effect(delivery, 21, 1024, RESULT_BYTES, apply)
            .await,
        Err(crab_cell_runtime::Error::Capacity(_))
    ));
    assert_held(&handle).await;
    assert_eq!(
        handle
            .resolve_effect(delivery, 22, RESULT_BYTES)
            .await
            .unwrap(),
        Resolution::Absent
    );
    handle
        .execute(
            mutation_identity_window(86, 10, 10_000),
            Digest::from_bytes([86; 32]),
            23,
            1024,
            1024,
            |transaction| {
                release_sql(transaction)?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        handle
            .deliver_effect(delivery, 24, 1024, RESULT_BYTES, apply)
            .await
            .unwrap(),
        StoredOutcome::Success {
            commit_sequence: 3,
            ..
        }
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_refuses_overcommitted_database_reservations_before_publication() {
    let fixture = fixture_with_limits(
        b"reserved-bootstrap",
        Limits {
            max_database_bytes: 512 * 1024,
            ..Limits::default()
        },
    );
    let session = SessionId::from_bytes([4; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024, session).unwrap();
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
                session,
                endpoint: "https://capacity-bootstrap.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let result = runtime.bootstrap(proof, fixture.replica.clone(), authority.clone(), observed,
        fixture.database.clone(), |transaction| {
            transaction.execute_batch(SCHEMA)?;
            transaction.execute_batch("INSERT INTO capacity_reservations VALUES(X'01', 128); UPDATE capacity_total SET pages = 128;")?;
            Ok(())
        }).await;
    assert!(matches!(result, Err(crab_cell_runtime::Error::Capacity(_))));
    assert!(
        authority
            .load(fixture.target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .root
            .is_none()
    );
    runtime.shutdown().await.unwrap();
}

struct CapacityMigration;

impl crab_cell_runtime::registry::CellModule for CapacityMigration {
    const NAME: &'static str = "capacity-migration";

    fn descriptor(&self) -> &'static crab_cell_runtime::registry::ModuleDescriptor {
        use crab_cell_runtime::registry::{
            MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor, RetainedCodeDescriptor,
        };
        static DESCRIPTOR: std::sync::OnceLock<ModuleDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: Self::NAME,
            source_digest: Digest::from_bytes([91; 32]),
            retained_codes: Box::leak(Box::new([RetainedCodeDescriptor { code: Digest::from_bytes([5; 32]), schema_min: 1, schema_max: 1 }])),
            schema_min: 1, schema_max: 2,
            migrations: Box::leak(Box::new([SCHEMA,
                "CREATE TABLE migration_payload(value BLOB); INSERT INTO migration_payload VALUES(zeroblob(102400)); UPDATE counter SET value = 1;"
            ].into_iter().enumerate().map(|(index, sql)| MigrationDescriptor {
                version: index as u32 + 1, sql, digest: Digest::from_bytes(*blake3::hash(sql.as_bytes()).as_bytes()),
            }).collect::<Vec<_>>())),
            commands: &[], queries: &[], workflow_definitions: &[], activity_types: &[],
            namespaces: Box::leak(Box::new([NamespaceDescriptor {
                id: NamespaceId::from_bytes([6; 16]), name: "capacity-migration", role: CatalogRole::Repository,
                shards: 1, effect_targets: &[], dead_letter: None,
            }])),
        })
    }

    fn register(
        self,
        _: &mut crab_cell_runtime::registry::RegistryBuilder,
    ) -> crab_cell_runtime::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn migration_cannot_spend_prepared_work_reservations() {
    use crab_cell_runtime::registry::{BuildDescriptor, RegistryBuilder};
    let fixture = fixture_with_limits(
        b"reserved-migration",
        Limits {
            max_database_bytes: 512 * 1024,
            ..Limits::default()
        },
    );
    let (runtime, handle, _pool) = activate_runtime(&fixture, 16 * 1024 * 1024).await;
    hold_capacity(&handle).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let before = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let mut registry = RegistryBuilder::new(BuildDescriptor {
        source_revision: "capacity-test".into(),
        cargo_lock_digest: Digest::from_bytes([92; 32]),
    });
    registry.register(CapacityMigration).unwrap();
    let registry = registry.finish().unwrap();
    let plan = registry
        .next_migration(fixture.target.namespace(), Digest::from_bytes([5; 32]), 1)
        .unwrap()
        .unwrap();
    assert!(matches!(
        handle.migrate(plan, 30).await,
        Err(crab_cell_runtime::Error::Capacity(_))
    ));
    let after = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.value().root, before.value().root);
    // Migrations drain the old capability even on refusal. Inspect the original
    // SQL file after shutdown to prove both schema and handler writes rolled back.
    runtime.shutdown().await.unwrap();
    let connection = crab_ltx::rusqlite::Connection::open_with_flags(
        &fixture.database,
        crab_ltx::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let state = connection.query_row("SELECT value, (SELECT pages FROM capacity_total), (SELECT schema_version FROM sys_meta), (SELECT COUNT(*) FROM sqlite_schema WHERE name = 'migration_payload') FROM counter", [], |row| Ok((row.get::<_,i64>(0)?, row.get::<_,i64>(1)?, row.get::<_,i64>(2)?, row.get::<_,i64>(3)?))).unwrap();
    assert_eq!(state, (0, 96, 1, 0));
}
