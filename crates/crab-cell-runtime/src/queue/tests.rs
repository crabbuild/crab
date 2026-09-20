use prost::Message;

use super::*;

fn connection() -> Connection {
    let mut connection = Connection::open_in_memory().unwrap();
    crate::install_runtime_schema(
        &mut connection,
        source_target().cell_id(),
        IncarnationId::from_bytes([2; 16]),
        1,
    )
    .unwrap();
    let transaction = connection.transaction().unwrap();
    install_queue_schema(&transaction).unwrap();
    transaction.commit().unwrap();
    connection
}

fn source_target() -> CellTarget {
    CellTarget::new(
        crate::TenantId::from_bytes([3; 16]),
        crate::ApplicationId::from_bytes([4; 16]),
        NamespaceId::from_bytes([5; 16]),
        &0_u32.to_be_bytes(),
    )
    .unwrap()
}

fn dead_letter_target(shards: u32) -> QueueDeadLetterTarget {
    QueueDeadLetterTarget::new(
        "dead-letter",
        NamespaceId::from_bytes([6; 16]),
        shards,
        7,
        1,
    )
}

fn insert_leased_message(connection: &mut Connection) -> [u8; 16] {
    let transaction = connection.transaction().unwrap();
    let outcome = queue_send(
        &transaction,
        source_target().namespace(),
        0,
        &QueueSendRequest {
            producer_id: [7; 16],
            payload: b"original".to_vec(),
            available_at_ms: 0,
        },
    )
    .unwrap();
    let QueueSendOutcome::Sent { message_id } = outcome else {
        panic!("first queue send must insert")
    };
    transaction
        .execute(
            "UPDATE queue_messages SET state = 1, attempt = 20, token = ?1, lease_until_ms = 100 WHERE message_id = ?2",
            ([8_u8; 16].as_slice(), message_id.as_slice()),
        )
        .unwrap();
    transaction.commit().unwrap();
    message_id
}

#[test]
fn embedded_queue_migration_matches_normative_contract() {
    assert_eq!(QUEUE_SCHEMA, include_str!("../../docs/contracts/queue.sql"));
}

#[test]
fn message_identity_binds_namespace_and_producer() {
    assert_ne!(
        message_id(NamespaceId::from_bytes([1; 16]), [2; 16]),
        message_id(NamespaceId::from_bytes([3; 16]), [2; 16])
    );
}

#[test]
fn dead_transition_atomically_links_one_canonical_queue_effect() {
    let mut connection = connection();
    let message_id = insert_leased_message(&mut connection);
    let transaction = connection.transaction().unwrap();
    let mut effects = EffectBatch::new(&transaction, &source_target(), 1, 10).unwrap();
    let mut dead_letter = QueueDeadLetterWriter::new(dead_letter_target(1), &mut effects);
    assert_eq!(
        queue_apply_lease_with_dead_letter(
            &transaction,
            10,
            message_id,
            [8; 16],
            QueueLeaseAction::Retry { delay_ms: 0 },
            Some(&mut dead_letter),
        )
        .unwrap(),
        QueueLeaseOutcome::Applied {
            state: QueueState::Dead,
            lease_until_ms: None,
        }
    );
    let (linked, operation): (Vec<u8>, Vec<u8>) = transaction
        .query_row(
            "SELECT dead_letter_effect_id, operation FROM queue_messages JOIN sys_effects ON effect_id = dead_letter_effect_id WHERE message_id = ?1",
            [message_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(linked.len(), 32);
    let effect = crate::peer_wire::EffectRequest::decode(operation.as_slice()).unwrap();
    assert!(effect.destination_incarnation.is_empty());
    let command = match effect.operation.unwrap() {
        crate::peer_wire::effect_request::Operation::CellCommand(command) => command,
        _ => panic!("dead letter must use the typed Queue command"),
    };
    assert_eq!(command.command_id, 7);
    let mut decoder =
        crate::BoundedDecoder::new(&command.input, QUEUE_SEND_MAX_INPUT_BYTES).unwrap();
    let request = QueueSendRequest::decode(&mut decoder).unwrap();
    decoder.finish().unwrap();
    assert_eq!(request.payload, b"original");
    assert_eq!(request.available_at_ms, 10);
    transaction.commit().unwrap();
}

#[test]
fn dead_payload_is_retained_until_its_effect_is_terminal() {
    let mut connection = connection();
    let message_id = insert_leased_message(&mut connection);
    let transaction = connection.transaction().unwrap();
    let mut effects = EffectBatch::new(&transaction, &source_target(), 1, 10).unwrap();
    let mut dead_letter = QueueDeadLetterWriter::new(dead_letter_target(1), &mut effects);
    queue_apply_lease_with_dead_letter(
        &transaction,
        10,
        message_id,
        [8; 16],
        QueueLeaseAction::Retry { delay_ms: 0 },
        Some(&mut dead_letter),
    )
    .unwrap();
    transaction
        .execute(
            "UPDATE queue_messages SET expires_at_ms = 10 WHERE message_id = ?1",
            [message_id.as_slice()],
        )
        .unwrap();
    transaction
        .execute("UPDATE queue_dedup SET retain_until_ms = 10", [])
        .unwrap();
    assert_eq!(queue_cleanup_expired(&transaction, 10).unwrap(), 0);
    let retained: (i64, i64) = transaction
        .query_row(
            "SELECT (SELECT count(*) FROM queue_messages), (SELECT count(*) FROM queue_dedup)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(retained, (1, 1));
    transaction
        .execute(
            "UPDATE sys_effects SET state = 2 WHERE effect_id = (SELECT dead_letter_effect_id FROM queue_messages WHERE message_id = ?1)",
            [message_id.as_slice()],
        )
        .unwrap();
    assert_eq!(queue_cleanup_expired(&transaction, 10).unwrap(), 2);
    transaction.commit().unwrap();
}

#[test]
fn queue_state_counters_follow_insert_update_delete_and_report_drift() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    for id in [1_u8, 2] {
        transaction
            .execute(
                "INSERT INTO queue_messages(message_id, payload, state, attempt, due_at_ms, expires_at_ms, token, lease_until_ms, result_code) VALUES (?1, X'01', 0, 0, 0, 100, NULL, NULL, NULL)",
                [vec![id; 16]],
            )
            .unwrap();
    }
    assert_eq!(queue_info(&transaction).unwrap().ready, 2);
    transaction
        .execute(
            "UPDATE queue_messages SET state = 1, token = zeroblob(16), lease_until_ms = 10 WHERE message_id = ?1",
            [vec![1_u8; 16]],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE queue_messages SET state = 2, token = NULL, lease_until_ms = NULL WHERE message_id = ?1",
            [vec![1_u8; 16]],
        )
        .unwrap();
    transaction
        .execute(
            "DELETE FROM queue_messages WHERE message_id = ?1",
            [vec![2_u8; 16]],
        )
        .unwrap();
    verify_queue_counts(&transaction).unwrap();
    assert_eq!(queue_info(&transaction).unwrap().acked, 1);
    transaction
        .execute(
            "UPDATE queue_control SET ready_count = ready_count + 1 WHERE singleton = 1",
            [],
        )
        .unwrap();
    assert!(verify_queue_counts(&transaction).is_err());
}

#[test]
fn repeated_ready_expiry_does_not_duplicate_dead_letter() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    let QueueSendOutcome::Sent { message_id } = queue_send(
        &transaction,
        source_target().namespace(),
        0,
        &QueueSendRequest {
            producer_id: [9; 16],
            payload: b"expire".to_vec(),
            available_at_ms: 0,
        },
    )
    .unwrap() else {
        panic!("first queue send must insert")
    };
    transaction
        .execute(
            "UPDATE queue_messages SET attempt = 20 WHERE message_id = ?1",
            [message_id.as_slice()],
        )
        .unwrap();
    let mut effects = EffectBatch::new(&transaction, &source_target(), 1, 1).unwrap();
    let mut dead_letter = QueueDeadLetterWriter::new(dead_letter_target(1), &mut effects);
    assert_eq!(
        queue_expire_ready_bounded_with_dead_letter(&transaction, 1, 128, Some(&mut dead_letter),)
            .unwrap(),
        1
    );
    assert_eq!(
        queue_expire_ready_bounded_with_dead_letter(&transaction, 1, 128, Some(&mut dead_letter),)
            .unwrap(),
        0
    );
    assert_eq!(
        transaction
            .query_row("SELECT count(*) FROM sys_effects", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    transaction.commit().unwrap();
}

#[test]
fn failed_dead_letter_insert_rolls_back_queue_transition() {
    let mut connection = connection();
    let message_id = insert_leased_message(&mut connection);
    let transaction = connection.transaction().unwrap();
    let mut effects = EffectBatch::new(&transaction, &source_target(), 1, 10).unwrap();
    let mut dead_letter = QueueDeadLetterWriter::new(dead_letter_target(0), &mut effects);
    assert!(
        queue_apply_lease_with_dead_letter(
            &transaction,
            10,
            message_id,
            [8; 16],
            QueueLeaseAction::Retry { delay_ms: 0 },
            Some(&mut dead_letter),
        )
        .is_err()
    );
    drop(transaction);
    let state: i64 = connection
        .query_row(
            "SELECT state FROM queue_messages WHERE message_id = ?1",
            [message_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    let effects: i64 = connection
        .query_row("SELECT count(*) FROM sys_effects", [], |row| row.get(0))
        .unwrap();
    assert_eq!((state, effects), (1, 0));
}

#[test]
fn pause_blocks_claims_but_preserves_messages_for_resume() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    queue_send(
        &transaction,
        source_target().namespace(),
        0,
        &QueueSendRequest {
            producer_id: [10; 16],
            payload: b"work".to_vec(),
            available_at_ms: 0,
        },
    )
    .unwrap();
    assert_eq!(
        queue_control(&transaction, 1, QueueControlAction::Pause).unwrap(),
        QueueControlOutcome::Paused { generation: 1 }
    );
    assert!(
        queue_claim(&transaction, 1, 1, 5_000, &mut SystemQueueTokens)
            .unwrap()
            .is_empty()
    );
    let info = queue_info(&transaction).unwrap();
    assert!(info.paused);
    assert_eq!(info.ready, 1);
    assert_eq!(
        queue_control(&transaction, 2, QueueControlAction::Resume).unwrap(),
        QueueControlOutcome::Resumed { generation: 2 }
    );
    assert_eq!(
        queue_claim(&transaction, 2, 1, 5_000, &mut SystemQueueTokens)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn purge_never_deletes_a_live_lease() {
    let mut connection = connection();
    let message_id = insert_leased_message(&mut connection);
    let transaction = connection.transaction().unwrap();
    assert_eq!(
        queue_control(&transaction, 1, QueueControlAction::Purge { limit: 128 },).unwrap(),
        QueueControlOutcome::Purged { messages: 0 }
    );
    assert_eq!(
        transaction
            .query_row(
                "SELECT count(*) FROM queue_messages WHERE message_id = ?1",
                [message_id.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn redrive_waits_for_a_terminal_dead_letter_effect() {
    let mut connection = connection();
    let message_id = insert_leased_message(&mut connection);
    let transaction = connection.transaction().unwrap();
    let mut effects = EffectBatch::new(&transaction, &source_target(), 1, 10).unwrap();
    let mut dead_letter = QueueDeadLetterWriter::new(dead_letter_target(1), &mut effects);
    queue_apply_lease_with_dead_letter(
        &transaction,
        10,
        message_id,
        [8; 16],
        QueueLeaseAction::Retry { delay_ms: 0 },
        Some(&mut dead_letter),
    )
    .unwrap();
    assert_eq!(
        queue_control(&transaction, 11, QueueControlAction::Redrive { limit: 1 },).unwrap(),
        QueueControlOutcome::Redriven { messages: 0 }
    );
    transaction
        .execute(
            "UPDATE sys_effects SET state = 3 WHERE effect_id = (SELECT dead_letter_effect_id FROM queue_messages WHERE message_id = ?1)",
            [message_id.as_slice()],
        )
        .unwrap();
    assert_eq!(
        queue_control(&transaction, 12, QueueControlAction::Redrive { limit: 1 },).unwrap(),
        QueueControlOutcome::Redriven { messages: 1 }
    );
    assert_eq!(
        transaction
            .query_row(
                "SELECT state, attempt FROM queue_messages WHERE message_id = ?1",
                [message_id.as_slice()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap(),
        (0, 0)
    );
}
