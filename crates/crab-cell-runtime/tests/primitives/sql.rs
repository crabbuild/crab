use std::sync::Arc;

use crab_cell_runtime::cell::actor::CellRuntime;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::catalog::{CatalogEntry, CellCatalog};
use crab_cell_runtime::cell::schema::install_runtime_schema;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::control::Owner;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::IncarnationId;
use crab_cell_runtime::identity::{
    ApplicationId, CellId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_cell_runtime::primitives::blob::install_blob_schema;
use crab_cell_runtime::primitives::cron::install_cron_schema;
use crab_cell_runtime::primitives::sql::{
    SqlBatch, SqlResultSet, SqlStatement, SqlValue, register_sql, sql_batch, sql_query_batch,
};
use crab_cell_runtime::primitives::sql::{SqlCell, SqlModule};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, Command, CommandContext, CommandResult, ModuleDescriptor,
    NamespaceDescriptor, RegistryBuilder,
};
use crab_cell_runtime::registry::{MigrationDescriptor, OperationDescriptor};
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Limits, rusqlite::Connection};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

use crate::support::fixtures::mutation_identity;

const SQL_MODULE: &str = "sql-test";
const SQL_NAMESPACE: NamespaceId = NamespaceId::from_bytes([7; 16]);
const SQL_MIGRATION: &str =
    "CREATE TABLE app_items(id INTEGER PRIMARY KEY, name TEXT NOT NULL, payload BLOB);";

struct TestSql;

impl SqlModule for TestSql {
    const MODULE: &'static str = SQL_MODULE;
    const BATCH_COMMAND_ID: u32 = 1;
    const BATCH_QUERY_ID: u32 = 1;
}

impl CellModule for TestSql {
    const NAME: &'static str = SQL_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<ModuleDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: SQL_MODULE,
            source_digest: Digest::from_bytes([8; 32]),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: SQL_MIGRATION,
                digest: Digest::from_bytes(*blake3::hash(SQL_MIGRATION.as_bytes()).as_bytes()),
            }])),
            commands: &[
                OperationDescriptor {
                    id: 2,
                    codec_version: 1,
                    schema_min: 1,
                    schema_max: 1,
                    input_limit: 8,
                    output_limit: 8,
                },
                OperationDescriptor {
                    id: 1,
                    codec_version: 1,
                    schema_min: 1,
                    schema_max: 1,
                    input_limit: 1024 * 1024,
                    output_limit: 1024 * 1024,
                },
            ],
            queries: &[OperationDescriptor {
                id: 1,
                codec_version: 1,
                schema_min: 1,
                schema_max: 1,
                input_limit: 1024 * 1024,
                output_limit: 1024 * 1024,
            }],
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &[NamespaceDescriptor {
                id: SQL_NAMESPACE,
                name: SQL_MODULE,
                role: CatalogRole::Sql,
                shards: 1,
                effect_targets: &[],
                dead_letter: None,
            }],
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_sql::<Self>(registry)?;
        registry.bind_command::<WriteBlob>()
    }
}

const BLOB_BYTES: usize = (1 << 20) + 17;
const BLOB_CHUNK: usize = 256 * 1024;

struct WriteBlob;

impl Command for WriteBlob {
    const MODULE: &'static str = SQL_MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = u8;
    type Output = ();

    fn execute(
        context: &mut CommandContext<'_, '_>,
        mode: u8,
    ) -> crab_cell_runtime::Result<CommandResult<()>> {
        if mode == 0 {
            context.sql(&SqlBatch { statements: vec![statement(
                "INSERT INTO app_items(id, name, payload) VALUES (8, 'incremental', zeroblob(?1))",
                vec![SqlValue::Integer(BLOB_BYTES as i64)],
            )] })?;
            for offset in (0..BLOB_BYTES).step_by(BLOB_CHUNK) {
                let chunk = vec![(offset / BLOB_CHUNK) as u8; BLOB_CHUNK.min(BLOB_BYTES - offset)];
                context.write_sql_blob("app_items", "payload", 8, offset, &chunk)?;
            }
        } else if mode <= 2 {
            // A release of staged storage and partial BLOB application must
            // both roll back on rejection or a handler error.
            context.sql(&SqlBatch {
                statements: vec![statement("DELETE FROM app_items WHERE id = 7", vec![])],
            })?;
            context.write_sql_blob("app_items", "payload", 8, 0, &[99; 32])?;
            if mode == 1 {
                return Ok(CommandResult::Rejected(()));
            }
            return Err(crab_cell_runtime::Error::Command(
                "injected after incremental write",
            ));
        } else {
            for table in [
                "sys_meta",
                "SyS_requests",
                "kv_entries",
                "queue_messages",
                "workflow_runs",
                "blob_objects",
                "cron_schedules",
                "sqlite_schema",
            ] {
                assert!(
                    matches!(
                        context.write_sql_blob(table, "payload", 1, 0, &[1]),
                        Err(crab_cell_runtime::Error::Command(
                            "SQL blob targets a protected table"
                        ))
                    ),
                    "{table}"
                );
            }
            assert!(
                context
                    .write_sql_blob("app_items", "payload", 8, 0, &vec![1; 1 << 20])
                    .is_err()
            );
            assert!(
                context
                    .write_sql_blob("app_items", "payload", 8, BLOB_BYTES, &[1])
                    .is_err()
            );
            assert!(
                context
                    .write_sql_blob("app_items", "payload", 8, usize::MAX, &[1])
                    .is_err()
            );
            assert!(
                context
                    .write_sql_blob("app_items", "payload", 999, 0, &[1])
                    .is_err()
            );
            assert!(
                context
                    .write_sql_blob("app_items", "id", 8, 0, &[1])
                    .is_err()
            );
        }
        Ok(CommandResult::Success(()))
    }
}

async fn assert_blob(sql: &SqlCell<TestSql>) {
    for offset in (0..BLOB_BYTES).step_by(BLOB_CHUNK) {
        let result = sql.query(None, SqlBatch { statements: vec![statement(
            "SELECT length(payload), substr(payload, ?1, ?2) FROM app_items WHERE id = 8",
            vec![SqlValue::Integer(offset as i64 + 1), SqlValue::Integer(BLOB_CHUNK as i64)],
        )] }).await.unwrap();
        assert_eq!(
            result.output[0].rows,
            vec![vec![
                SqlValue::Integer(BLOB_BYTES as i64),
                SqlValue::Blob(vec![
                    (offset / BLOB_CHUNK) as u8;
                    BLOB_CHUNK.min(BLOB_BYTES - offset)
                ]),
            ]]
        );
    }
}

fn sql_registry() -> Arc<crab_cell_runtime::Registry> {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "sql-api-test".into(),
        cargo_lock_digest: Digest::from_bytes([9; 32]),
    });
    builder.register(TestSql).unwrap();
    Arc::new(builder.finish().unwrap())
}

fn install_sql_schema(
    transaction: &crab_ltx::rusqlite::Transaction<'_>,
) -> crab_cell_runtime::Result<()> {
    transaction.execute_batch(SQL_MIGRATION)?;
    Ok(())
}

fn connection() -> Connection {
    let mut connection = Connection::open_in_memory().unwrap();
    install_runtime_schema(
        &mut connection,
        CellId::from_bytes([1; 32]),
        IncarnationId::from_bytes([2; 16]),
        1,
    )
    .unwrap();
    connection
        .execute_batch(
            "CREATE TABLE app_items(id INTEGER PRIMARY KEY, name TEXT NOT NULL, payload BLOB);\n\
             CREATE VIEW app_runtime_metadata AS SELECT commit_sequence FROM sys_meta;\n\
             CREATE TRIGGER app_items_guard AFTER INSERT ON app_items BEGIN\n\
               UPDATE sys_meta SET logical_time_ms = logical_time_ms + 1 WHERE singleton = 1;\n\
             END;",
        )
        .unwrap();
    connection
}

fn statement(sql: &str, parameters: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.to_owned(),
        parameters,
    }
}

#[test]
fn typed_batch_mutates_and_materializes_in_order() {
    let mut connection = connection();
    connection
        .execute_batch("DROP TRIGGER app_items_guard")
        .unwrap();
    let transaction = connection.transaction().unwrap();
    let results = sql_batch(
        &transaction,
        &SqlBatch {
            statements: vec![
                statement(
                    "INSERT INTO app_items(id, name, payload) VALUES (?1, ?2, ?3)",
                    vec![
                        SqlValue::Integer(7),
                        SqlValue::Text("crab".into()),
                        SqlValue::Blob(vec![1, 2, 3]),
                    ],
                ),
                statement(
                    "SELECT id, name, payload, NULL, 1.5 FROM app_items WHERE id = ?1",
                    vec![SqlValue::Integer(7)],
                ),
            ],
        },
    )
    .unwrap();
    assert_eq!(
        results,
        vec![
            SqlResultSet {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 1,
            },
            SqlResultSet {
                columns: vec![
                    "id".into(),
                    "name".into(),
                    "payload".into(),
                    "NULL".into(),
                    "1.5".into()
                ],
                rows: vec![vec![
                    SqlValue::Integer(7),
                    SqlValue::Text("crab".into()),
                    SqlValue::Blob(vec![1, 2, 3]),
                    SqlValue::Null,
                    SqlValue::Real(1.5),
                ]],
                rows_affected: 0,
            },
        ]
    );
    transaction.commit().unwrap();
}

#[test]
fn authorizer_blocks_runtime_tables_and_indirect_trigger_or_view_access() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_blob_schema(&transaction).unwrap();
    install_cron_schema(&transaction).unwrap();
    for sql in [
        "SELECT commit_sequence FROM sys_meta",
        "SELECT object_key FROM blob_objects",
        "SELECT schedule_id FROM cron_schedules",
        "DELETE FROM blob_objects",
        "DELETE FROM cron_schedules",
        "SELECT commit_sequence FROM app_runtime_metadata",
        "INSERT INTO app_items(id, name) VALUES (1, 'blocked by trigger')",
        "PRAGMA user_version",
        "ATTACH DATABASE ':memory:' AS other",
        "SAVEPOINT nested",
        "SELECT load_extension('missing')",
        "CREATE TABLE app_other(value INTEGER)",
    ] {
        assert!(
            sql_batch(
                &transaction,
                &SqlBatch {
                    statements: vec![statement(sql, Vec::new())],
                },
            )
            .is_err(),
            "statement unexpectedly authorized: {sql}"
        );
    }
    transaction.rollback().unwrap();

    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM sys_meta", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn read_boundary_rejects_mutation_and_mutation_returning() {
    let mut connection = connection();
    connection
        .execute_batch("DROP TRIGGER app_items_guard")
        .unwrap();
    let update = SqlBatch {
        statements: vec![statement(
            "UPDATE app_items SET name = 'changed'",
            Vec::new(),
        )],
    };
    assert!(sql_query_batch(&connection, &update).is_err());

    let returning = SqlBatch {
        statements: vec![statement(
            "INSERT INTO app_items(id, name) VALUES (3, 'three') RETURNING id",
            Vec::new(),
        )],
    };
    let transaction = connection.transaction().unwrap();
    assert!(sql_batch(&transaction, &returning).is_err());
    transaction.rollback().unwrap();
}

#[test]
fn batch_rejects_unbounded_or_ambiguous_inputs_and_outputs() {
    let connection = connection();
    let too_many = SqlBatch {
        statements: (0..129)
            .map(|_| statement("SELECT 1", Vec::new()))
            .collect(),
    };
    assert!(sql_query_batch(&connection, &too_many).is_err());

    let multiple = SqlBatch {
        statements: vec![statement("SELECT ';'; SELECT 2", Vec::new())],
    };
    assert!(sql_query_batch(&connection, &multiple).is_err());

    let wrong_parameters = SqlBatch {
        statements: vec![statement("SELECT ?1", Vec::new())],
    };
    assert!(sql_query_batch(&connection, &wrong_parameters).is_err());

    let too_many_rows = SqlBatch {
        statements: vec![statement(
            "WITH RECURSIVE values_(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM values_ WHERE value <= 1000) SELECT value FROM values_",
            Vec::new(),
        )],
    };
    assert!(sql_query_batch(&connection, &too_many_rows).is_err());

    let oversized = SqlBatch {
        statements: vec![statement(
            "SELECT ?1",
            vec![SqlValue::Blob(vec![0; (1 << 20) + 1])],
        )],
    };
    assert!(sql_query_batch(&connection, &oversized).is_err());

    let oversized_result = SqlBatch {
        statements: vec![statement("SELECT zeroblob(1048576)", Vec::new())],
    };
    assert!(sql_query_batch(&connection, &oversized_result).is_err());
}

#[test]
fn quoted_semicolons_and_comments_do_not_create_a_second_statement() {
    let connection = connection();
    let results = sql_query_batch(
        &connection,
        &SqlBatch {
            statements: vec![statement("SELECT ';' AS value /* ; */ -- ;\n", Vec::new())],
        },
    )
    .unwrap();
    assert_eq!(results[0].rows, vec![vec![SqlValue::Text(";".into())]]);
}

#[tokio::test]
async fn typed_sql_cell_publishes_enforces_read_only_queries_and_survives_restore() {
    let registry = sql_registry();
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        SQL_NAMESPACE,
        b"repository-7",
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
                CatalogRole::Sql,
                registry.module_code(SQL_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout);
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
            install_sql_schema,
        )
        .await
        .unwrap();
    let sql = SqlCell::<TestSql>::new(
        CellClient::local(registry.clone(), handle.clone()),
        target.clone(),
    )
    .unwrap();
    let committed = sql
        .batch(
            mutation_identity(7),
            SqlBatch {
                statements: vec![
                    statement(
                        "INSERT INTO app_items(id, name, payload) VALUES (?1, ?2, ?3)",
                        vec![
                            SqlValue::Integer(7),
                            SqlValue::Text("crab".into()),
                            SqlValue::Blob(vec![1, 2, 3]),
                        ],
                    ),
                    statement(
                        "SELECT name FROM app_items WHERE id = ?1",
                        vec![SqlValue::Integer(7)],
                    ),
                ],
            },
        )
        .await
        .unwrap();
    assert_eq!(committed.receipt.commit_sequence, 1);
    assert_eq!(
        committed.output[1].rows,
        vec![vec![SqlValue::Text("crab".into())]]
    );
    let mutation_query = sql
        .query(
            Some(committed.receipt),
            SqlBatch {
                statements: vec![statement(
                    "UPDATE app_items SET name = 'wrong' WHERE id = 7",
                    Vec::new(),
                )],
            },
        )
        .await;
    assert!(matches!(
        mutation_query,
        Err(InvocationError::NotStarted(_))
    ));
    let client = CellClient::local(registry.clone(), handle.clone());
    client
        .command::<WriteBlob>(&target, mutation_identity(8), 0)
        .await
        .unwrap();
    assert!(matches!(
        client
            .command::<WriteBlob>(&target, mutation_identity(9), 1)
            .await,
        Err(InvocationError::Rejected(_))
    ));
    assert!(
        client
            .command::<WriteBlob>(&target, mutation_identity(10), 2)
            .await
            .is_err()
    );
    let latest = client
        .command::<WriteBlob>(&target, mutation_identity(11), 3)
        .await
        .unwrap();
    assert_blob(&sql).await;
    handle.drain().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
    let second_session = SessionId::from_bytes([9; 16]);
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
    let restored_sql =
        SqlCell::<TestSql>::new(CellClient::local(registry, restored.clone()), target).unwrap();
    let observed = restored_sql
        .query(
            Some(committed.receipt),
            SqlBatch {
                statements: vec![statement(
                    "SELECT id, name, payload FROM app_items WHERE id = ?1",
                    vec![SqlValue::Integer(7)],
                )],
            },
        )
        .await
        .unwrap();
    assert_eq!(observed.receipt, latest.receipt);
    assert_eq!(
        observed.output[0].rows,
        vec![vec![
            SqlValue::Integer(7),
            SqlValue::Text("crab".into()),
            SqlValue::Blob(vec![1, 2, 3]),
        ]]
    );
    assert_blob(&restored_sql).await;
    restored.drain().await.unwrap();
}
