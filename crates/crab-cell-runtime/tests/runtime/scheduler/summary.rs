//! Scheduler summaries for deadline classes and primitive deadlines.

use super::*;

#[test]
fn runtime_only_summary_clamps_overdue_work() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    assert_eq!(scheduler_next_due_ms(&transaction, 10).unwrap(), None);
    transaction
        .execute(
            "INSERT INTO sys_requests VALUES (?1, ?2, 1, X'', 1, 20, 30)",
            ([3_u8; 16].as_slice(), [4_u8; 32].as_slice()),
        )
        .unwrap();
    assert_eq!(scheduler_next_due_ms(&transaction, 10).unwrap(), Some(30));
    assert_eq!(scheduler_next_due_ms(&transaction, 40).unwrap(), Some(40));
}
#[test]
fn summary_covers_all_installed_primitive_deadline_classes() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_kv_schema(&transaction).unwrap();
    install_queue_schema(&transaction).unwrap();
    install_workflow_schema(&transaction).unwrap();
    transaction
        .execute(
            "INSERT INTO kv_entries VALUES (X'01', X'02', zeroblob(28), X'03', 90)",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO queue_messages VALUES (zeroblob(16), X'04', 1, 1, 50, 100, zeroblob(16), 80, NULL, NULL)",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO sys_effects VALUES (zeroblob(32), zeroblob(32), X'05', 0, 0, 70, 110, NULL, NULL, 1, NULL)",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO workflow_runs VALUES (X'06', zeroblob(16), zeroblob(32), 0, X'', 0, NULL, NULL)",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO workflow_activities VALUES (zeroblob(16), zeroblob(16), 'job', X'', 0, 0, 60, 120, NULL, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO workflow_timers VALUES (zeroblob(16), X'01010101010101010101010101010101', 40, 0)",
            [],
        )
        .unwrap();
    assert_eq!(scheduler_next_due_ms(&transaction, 10).unwrap(), Some(40));
    transaction
        .execute("UPDATE workflow_timers SET state = 1", [])
        .unwrap();
    assert_eq!(scheduler_next_due_ms(&transaction, 10).unwrap(), Some(60));
}
#[test]
fn summary_ignores_ready_queue_available_time() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_queue_schema(&transaction).unwrap();
    transaction
        .execute(
            "INSERT INTO queue_messages VALUES (X'01010101010101010101010101010101', X'04', 0, 0, 10, 100, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();

    assert_eq!(scheduler_next_due_ms(&transaction, 10).unwrap(), Some(100));
}
#[test]
fn summary_wakes_immediately_for_exhausted_ready_queue_work() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_queue_schema(&transaction).unwrap();
    transaction
        .execute(
            "INSERT INTO queue_messages VALUES (X'02020202020202020202020202020202', X'04', 0, 20, 100, 1000, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();

    assert_eq!(scheduler_next_due_ms(&transaction, 10).unwrap(), Some(10));
}
#[test]
fn exhausted_ready_deadline_uses_the_attempt_index() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_queue_schema(&transaction).unwrap();
    let details = transaction
        .prepare(
            "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM queue_messages INDEXED BY queue_attempts WHERE state = 0 AND attempt >= ?1)",
        )
        .unwrap()
        .query_map([i64::from(20_u32)], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();

    assert!(
        details
            .iter()
            .any(|detail| detail.contains("SEARCH") && detail.contains("queue_attempts"))
    );
}
#[test]
fn summary_uses_queue_lease_deadline_not_ready_available_time() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_queue_schema(&transaction).unwrap();
    transaction
        .execute(
            "INSERT INTO queue_messages VALUES (X'03030303030303030303030303030303', X'04', 1, 1, 10, 1000, zeroblob(16), 80, NULL, NULL)",
            [],
        )
        .unwrap();

    assert_eq!(scheduler_next_due_ms(&transaction, 10).unwrap(), Some(80));
}
#[test]
fn summary_uses_cron_deadline_when_the_schedule_schema_is_installed() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_cron_schema(&transaction).unwrap();
    transaction
        .execute(
            "INSERT INTO cron_schedules(schedule_id, target_index, target_partition, payload, interval_ms, next_due_ms, occurrence, enabled, generation, updated_at_ms) VALUES (X'02020202020202020202020202020202', 0, X'00', X'', 30000, 80, 0, 1, 1, 10)",
            [],
        )
        .unwrap();

    assert_eq!(scheduler_next_due_ms(&transaction, 10).unwrap(), Some(80));
}
