//! Tick budgets, cleanup ordering, expiry, and effect ordinals.

use super::*;

#[test]
fn tick_processes_at_most_128_due_items() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    for ordinal in 0_u16..140 {
        let mut request_id = [0; 16];
        request_id[..2].copy_from_slice(&ordinal.to_be_bytes());
        transaction
            .execute(
                "INSERT INTO sys_requests VALUES (?1, ?2, 1, X'', 1, 1, 2)",
                (request_id.as_slice(), [4_u8; 32].as_slice()),
            )
            .unwrap();
    }
    assert_eq!(
        scheduler_tick(&transaction, &source_target(), 10, &[], None, &[])
            .unwrap()
            .processed,
        128
    );
    let remaining: usize = transaction
        .query_row("SELECT count(*) FROM sys_requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 12);
    assert_eq!(
        scheduler_tick(&transaction, &source_target(), 10, &[], None, &[])
            .unwrap()
            .processed,
        12
    );
}
#[test]
fn tick_request_cleanup_cannot_starve_queue_expiry() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_queue_schema(&transaction).unwrap();
    for ordinal in 0_u16..200 {
        let mut request_id = [0; 16];
        request_id[..2].copy_from_slice(&ordinal.to_be_bytes());
        transaction
            .execute(
                "INSERT INTO sys_requests VALUES (?1, ?2, 1, X'', 1, 1, 2)",
                (request_id.as_slice(), [4_u8; 32].as_slice()),
            )
            .unwrap();
    }
    transaction
        .execute(
            "INSERT INTO queue_messages VALUES (zeroblob(16), X'02', 0, 0, 1, 5, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();

    let outcome = scheduler_tick(&transaction, &source_target(), 10, &[], None, &[]).unwrap();

    assert_eq!(outcome.processed, 128);
    assert_eq!(
        transaction
            .query_row("SELECT state FROM queue_messages", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert!(
        transaction
            .query_row("SELECT count(*) FROM sys_requests", [], |row| row
                .get::<_, usize>(0))
            .unwrap()
            > 0
    );
}
#[test]
fn tick_terminalizes_expired_ready_work_and_runs_workflow_failure_transition() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_queue_schema(&transaction).unwrap();
    install_workflow_schema(&transaction).unwrap();
    transaction
        .execute(
            "INSERT INTO sys_effects VALUES (zeroblob(32), zeroblob(32), X'01', 0, 0, 1, 5, NULL, NULL, 1, NULL)",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO queue_messages VALUES (zeroblob(16), X'02', 0, 0, 1, 5, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO workflow_runs VALUES (X'03', zeroblob(16), ?1, 0, X'', 1, NULL, NULL)",
            [EXPIRY_DEFINITION.digest().as_bytes().as_slice()],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO workflow_activities VALUES (zeroblob(16), X'01010101010101010101010101010101', 'job', X'', 0, 0, 1, 5, NULL, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();

    let outcome = scheduler_tick(
        &transaction,
        &source_target(),
        10,
        &[&EXPIRY_DEFINITION],
        None,
        &[],
    )
    .unwrap();
    assert_eq!(outcome.processed, 3);
    let states: (i64, i64, i64) = transaction
        .query_row(
            "SELECT (SELECT state FROM sys_effects), (SELECT state FROM queue_messages), (SELECT status FROM workflow_runs)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(states, (3, 3, 2));
}
#[test]
fn tick_assigns_unique_command_ordinals_to_effects_from_multiple_runs() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_workflow_schema(&transaction).unwrap();
    for byte in [3_u8, 4_u8] {
        transaction
            .execute(
                "INSERT INTO workflow_runs VALUES (?1, ?2, ?3, 0, X'', 1, NULL, NULL)",
                (
                    [byte].as_slice(),
                    [byte; 16].as_slice(),
                    EFFECT_DEFINITION.digest().as_bytes().as_slice(),
                ),
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO workflow_activities VALUES (?1, ?2, 'job', X'', 0, 0, 1, 5, NULL, NULL, NULL, NULL, NULL)",
                ([byte; 16].as_slice(), [byte + 10; 16].as_slice()),
            )
            .unwrap();
    }

    assert_eq!(
        scheduler_tick(
            &transaction,
            &source_target(),
            10,
            &[&EFFECT_DEFINITION],
            None,
            &[]
        )
        .unwrap()
        .processed,
        2
    );
    let mut statement = transaction
        .prepare("SELECT effect_id FROM sys_effects ORDER BY effect_id")
        .unwrap();
    let mut stored = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut expected = vec![
        effect_id(
            source_target().cell_id(),
            IncarnationId::from_bytes([2; 16]),
            1,
            0,
        )
        .to_vec(),
        effect_id(
            source_target().cell_id(),
            IncarnationId::from_bytes([2; 16]),
            1,
            1,
        )
        .to_vec(),
    ];
    expected.sort();
    stored.sort();
    assert_eq!(stored, expected);
}
#[test]
fn tick_cleans_expired_blob_uploads_that_never_completed() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_blob_schema(&transaction).unwrap();
    transaction
        .execute(
            "INSERT INTO blob_uploads VALUES (X'01010101010101010101010101010101', X'02', zeroblob(32), 0, NULL, NULL, X'', 10, 20, 0, NULL, 0, 0)",
            [],
        )
        .unwrap();

    scheduler_tick(&transaction, &source_target(), 25, &[], None, &[]).unwrap();

    let remaining: usize = transaction
        .query_row("SELECT count(*) FROM blob_uploads", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        remaining, 0,
        "an expired upload without an object is garbage"
    );
}
