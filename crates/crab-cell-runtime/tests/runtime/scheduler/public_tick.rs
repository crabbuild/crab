//! Public tick cron firing and queue dead-letter routing.

use super::*;

#[test]
fn public_tick_fires_due_cron_schedules_through_its_targets() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_cron_schema(&transaction).unwrap();
    let target = CronTarget::new("cron-fixture", NamespaceId::from_bytes([9; 16]), 1, 1, 4096);
    transaction
        .execute(
            "INSERT INTO cron_schedules(schedule_id, target_index, target_partition, payload, \
             interval_ms, next_due_ms, occurrence, enabled, generation, updated_at_ms) \
             VALUES (?1, 0, X'00', ?2, 1000, 1000, 0, 1, 1, 0)",
            crab_ltx::rusqlite::params![
                crab_ltx::rusqlite::types::Value::Blob(vec![4; 16]),
                crab_ltx::rusqlite::types::Value::Blob(b"payload".to_vec()),
            ],
        )
        .unwrap();

    let outcome =
        scheduler_tick(&transaction, &source_target(), 2_000, &[], None, &[target]).unwrap();

    assert_eq!(
        outcome.processed, 2,
        "both due occurrences reach the depth-first class through the public entry"
    );
    let schedule = transaction
        .query_row(
            "SELECT next_due_ms, occurrence FROM cron_schedules",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap();
    assert_eq!(schedule, (3_000, 2));
    let effects: i64 = transaction
        .query_row("SELECT count(*) FROM sys_effects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(effects, 2, "each occurrence publishes its own effect");
}
#[test]
fn public_tick_dead_letters_exhausted_queue_work_through_its_target() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    install_queue_schema(&transaction).unwrap();
    let dead_letter_namespace = NamespaceId::from_bytes([10; 16]);
    let dead_letter = QueueDeadLetterTarget::new("dead-letter", dead_letter_namespace, 1, 7, 1);
    transaction
        .execute(
            "INSERT INTO queue_messages(message_id, payload, state, attempt, due_at_ms, \
             expires_at_ms, token, lease_until_ms, result_code, dead_letter_effect_id) \
             VALUES (?1, X'', 0, 20, 1000, 10000, NULL, NULL, NULL, NULL)",
            [crab_ltx::rusqlite::types::Value::Blob(vec![5; 16])],
        )
        .unwrap();

    scheduler_tick(
        &transaction,
        &source_target(),
        2_000,
        &[],
        Some(dead_letter),
        &[],
    )
    .unwrap();

    let (state, dead_lettered): (i64, bool) = transaction
        .query_row(
            "SELECT state, dead_letter_effect_id IS NOT NULL FROM queue_messages",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (state, dead_lettered),
        (3, true),
        "an exhausted ready message becomes dead and names its dead-letter effect"
    );
    let effects: i64 = transaction
        .query_row("SELECT count(*) FROM sys_effects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(effects, 1, "the dead-letter target receives one effect");
}
