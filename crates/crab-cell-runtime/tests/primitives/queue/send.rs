//! Producer send idempotency and delayed scheduling.

use super::*;

#[test]
fn producer_send_is_idempotent_and_conflicts_on_changed_payload_or_schedule() {
    let mut connection = connection();
    let namespace = NamespaceId::from_bytes([3; 16]);
    let request = send_request(1, b"payload", 20);
    let transaction = connection.transaction().unwrap();
    let first = queue_send(&transaction, namespace, 10, &request).unwrap();
    let second = queue_send(&transaction, namespace, 10, &request).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        queue_send(
            &transaction,
            namespace,
            10,
            &send_request(1, b"changed", 20),
        )
        .unwrap(),
        QueueSendOutcome::ProducerConflict
    );
    assert_eq!(
        queue_send(
            &transaction,
            namespace,
            10,
            &send_request(1, b"payload", 21),
        )
        .unwrap(),
        QueueSendOutcome::ProducerConflict
    );
    transaction.commit().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM queue_messages", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn delayed_send_with_past_schedule_becomes_immediately_ready() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    let outcome = queue_send(
        &transaction,
        QUEUE_NAMESPACE,
        20,
        &send_request(2, b"delayed-effect", 10),
    )
    .unwrap();
    let QueueSendOutcome::Sent { message_id } = outcome else {
        panic!("first queue send must insert")
    };
    let due_at_ms: i64 = transaction
        .query_row(
            "SELECT due_at_ms FROM queue_messages WHERE message_id = ?1",
            [message_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(due_at_ms, 20);
    transaction.commit().unwrap();
}
