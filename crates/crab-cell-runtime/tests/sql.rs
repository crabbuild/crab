use crab_cell_runtime::{
    CellId, IncarnationId, SqlBatch, SqlResultSet, SqlStatement, SqlValue, install_runtime_schema,
    sql_batch, sql_query_batch,
};
use crab_ltx::rusqlite::Connection;

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
    for sql in [
        "SELECT commit_sequence FROM sys_meta",
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
