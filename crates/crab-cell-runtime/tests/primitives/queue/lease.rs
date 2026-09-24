//! Claim tokens, lease transitions, retry limits, and reclaim bounds.

use super::*;

#[test]
fn claim_tokens_require_published_live_lease_for_ack_retry_and_extend() {
    let mut connection = connection();
    let namespace = NamespaceId::from_bytes([3; 16]);
    let transaction = connection.transaction().unwrap();
    for producer in 1..=2 {
        queue_send(
            &transaction,
            namespace,
            10,
            &send_request(producer, &[producer], 10),
        )
        .unwrap();
    }
    transaction.commit().unwrap();

    let transaction = connection.transaction().unwrap();
    let mut tokens = Tokens(0);
    let claimed = queue_claim(&transaction, 20, 2, 5_000, &mut tokens).unwrap();
    assert_eq!(claimed.len(), 2);
    assert_eq!(claimed[0].attempt, 1);
    transaction.commit().unwrap();
    assert!(queue_validate_claim(&connection, 21, &claimed).unwrap());
    assert!(!queue_validate_claim(&connection, 4_021, &claimed).unwrap());

    let first = &claimed[0];
    let transaction = connection.transaction().unwrap();
    assert_eq!(
        queue_apply_lease(
            &transaction,
            100,
            first.message_id,
            [99; 16],
            QueueLeaseAction::Ack,
        )
        .unwrap(),
        QueueLeaseOutcome::LeaseLost
    );
    assert_eq!(
        queue_apply_lease(
            &transaction,
            100,
            first.message_id,
            first.token,
            QueueLeaseAction::Extend {
                extension_ms: 10_000,
            },
        )
        .unwrap(),
        QueueLeaseOutcome::Applied {
            state: QueueState::Leased,
            lease_until_ms: Some(10_100),
        }
    );
    transaction.commit().unwrap();

    let transaction = connection.transaction().unwrap();
    assert_eq!(
        queue_apply_lease(
            &transaction,
            101,
            first.message_id,
            first.token,
            QueueLeaseAction::Ack,
        )
        .unwrap(),
        QueueLeaseOutcome::Applied {
            state: QueueState::Acked,
            lease_until_ms: None,
        }
    );
    transaction.commit().unwrap();
}
#[test]
fn retry_and_expired_reclaim_preserve_attempt_limits_and_cleanup_bounds() {
    let mut connection = connection();
    let namespace = NamespaceId::from_bytes([3; 16]);
    let transaction = connection.transaction().unwrap();
    queue_send(&transaction, namespace, 0, &send_request(1, b"payload", 0)).unwrap();
    transaction.commit().unwrap();

    let mut tokens = Tokens(0);
    let transaction = connection.transaction().unwrap();
    let first = queue_claim(&transaction, 0, 1, 5_000, &mut tokens)
        .unwrap()
        .remove(0);
    assert_eq!(
        queue_apply_lease(
            &transaction,
            1,
            first.message_id,
            first.token,
            QueueLeaseAction::Retry { delay_ms: 100 },
        )
        .unwrap(),
        QueueLeaseOutcome::Applied {
            state: QueueState::Ready,
            lease_until_ms: None,
        }
    );
    transaction.commit().unwrap();

    let transaction = connection.transaction().unwrap();
    assert!(
        queue_claim(&transaction, 100, 1, 5_000, &mut tokens)
            .unwrap()
            .is_empty()
    );
    let second = queue_claim(&transaction, 101, 1, 5_000, &mut tokens)
        .unwrap()
        .remove(0);
    assert_eq!(second.attempt, 2);
    transaction
        .execute(
            "UPDATE queue_messages SET attempt = 20, lease_until_ms = 102 WHERE message_id = ?1",
            [second.message_id.as_slice()],
        )
        .unwrap();
    transaction.commit().unwrap();

    let transaction = connection.transaction().unwrap();
    assert!(
        queue_claim(&transaction, 102, 1, 5_000, &mut tokens)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        transaction
            .query_row(
                "SELECT state FROM queue_messages WHERE message_id = ?1",
                [second.message_id.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        3
    );
    transaction
        .execute(
            "UPDATE queue_messages SET expires_at_ms = 102 WHERE message_id = ?1",
            [second.message_id.as_slice()],
        )
        .unwrap();
    assert_eq!(queue_cleanup_expired(&transaction, 102).unwrap(), 1);
    assert_eq!(
        transaction
            .query_row("SELECT count(*) FROM queue_dedup", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    transaction.commit().unwrap();
}
