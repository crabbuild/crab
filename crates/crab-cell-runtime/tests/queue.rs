use std::sync::Arc;

use crab_cell_runtime::{
    ApplicationId, CatalogEntry, CatalogRole, CellAuthority, CellCatalog, CellRuntime, CellTarget,
    Digest, HandlerOutcome, IncarnationId, MutationIdentity, NamespaceId, Owner, QueueLeaseAction,
    QueueLeaseOutcome, QueueMessage, QueueSendOutcome, QueueSendRequest, QueueState,
    QueueTokenSource, RequestId, SessionId, SqlWorkerPool, StoredOutcome, TenantId,
    install_queue_schema, install_runtime_schema, queue_apply_lease, queue_claim,
    queue_cleanup_expired, queue_send, queue_validate_claim,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

struct Tokens(u8);

impl QueueTokenSource for Tokens {
    fn next_token(&mut self) -> crab_cell_runtime::Result<[u8; 16]> {
        self.0 = self
            .0
            .checked_add(1)
            .ok_or(crab_cell_runtime::Error::Command(
                "test queue token overflow",
            ))?;
        Ok([self.0; 16])
    }
}

fn connection() -> crab_ltx::rusqlite::Connection {
    let mut connection = crab_ltx::rusqlite::Connection::open_in_memory().unwrap();
    install_runtime_schema(
        &mut connection,
        crab_cell_runtime::CellId::from_bytes([1; 32]),
        IncarnationId::from_bytes([2; 16]),
        1,
    )
    .unwrap();
    let transaction = connection.transaction().unwrap();
    install_queue_schema(&transaction).unwrap();
    transaction.commit().unwrap();
    connection
}

fn send_request(producer: u8, payload: &[u8], available_at_ms: i64) -> QueueSendRequest {
    QueueSendRequest {
        producer_id: [producer; 16],
        payload: payload.to_vec(),
        available_at_ms,
    }
}

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

fn encode_claim(message: &QueueMessage) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(44 + message.payload.len());
    encoded.extend_from_slice(&message.message_id);
    encoded.extend_from_slice(&message.token);
    encoded.extend_from_slice(&message.attempt.to_be_bytes());
    encoded.extend_from_slice(&message.lease_until_ms.to_be_bytes());
    encoded.extend_from_slice(&message.payload);
    encoded
}

fn decode_claim(encoded: &[u8]) -> QueueMessage {
    QueueMessage {
        message_id: encoded[..16].try_into().unwrap(),
        token: encoded[16..32].try_into().unwrap(),
        attempt: u32::from_be_bytes(encoded[32..36].try_into().unwrap()),
        lease_until_ms: i64::from_be_bytes(encoded[36..44].try_into().unwrap()),
        payload: encoded[44..].to_vec(),
    }
}

#[tokio::test]
async fn claimed_task_is_emitted_only_after_publication_and_survives_restore() {
    let namespace = NamespaceId::from_bytes([6; 16]);
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        namespace,
        &0_u32.to_be_bytes(),
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
            CatalogEntry::new(&target, CatalogRole::Queue, Digest::from_bytes([5; 32]), 1).unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
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
            install_queue_schema,
        )
        .await
        .unwrap();
    let request = send_request(7, b"job", 20);
    handle
        .execute(
            MutationIdentity {
                request_id: RequestId::from_bytes([7; 16]),
                issued_at_ms: 10,
                expires_at_ms: 10_000,
            },
            Digest::from_bytes([8; 32]),
            20,
            128,
            128,
            move |transaction| {
                let outcome = queue_send(transaction, namespace, 20, &request)?;
                match outcome {
                    QueueSendOutcome::Sent { message_id } => {
                        Ok(HandlerOutcome::Success(message_id.to_vec()))
                    }
                    QueueSendOutcome::ProducerConflict => {
                        Ok(HandlerOutcome::Rejected(b"producer conflict".to_vec()))
                    }
                }
            },
        )
        .await
        .unwrap();
    let claimed = handle
        .execute(
            MutationIdentity {
                request_id: RequestId::from_bytes([9; 16]),
                issued_at_ms: 21,
                expires_at_ms: 10_000,
            },
            Digest::from_bytes([10; 32]),
            21,
            64,
            512,
            |transaction| {
                let mut tokens = Tokens(10);
                let claim = queue_claim(transaction, 21, 1, 5_000, &mut tokens)?
                    .into_iter()
                    .next()
                    .ok_or(crab_cell_runtime::Error::Command("missing queued task"))?;
                Ok(HandlerOutcome::Success(encode_claim(&claim)))
            },
        )
        .await
        .unwrap();
    let claimed = match claimed {
        StoredOutcome::Success { result, .. } => decode_claim(&result),
        StoredOutcome::Rejected { .. } => panic!("queue claim was rejected"),
    };
    assert_eq!(
        authority
            .load(cell)
            .await
            .unwrap()
            .unwrap()
            .value()
            .next_due_ms,
        Some(5_021)
    );
    assert!(
        handle
            .query(64, 64, {
                let claimed = claimed.clone();
                move |connection| {
                    Ok(vec![u8::from(queue_validate_claim(
                        connection,
                        22,
                        &[claimed],
                    )?)])
                }
            })
            .await
            .unwrap()[0]
            != 0
    );
    handle.drain().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
    let second_session = SessionId::from_bytes([11; 16]);
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
    assert!(
        restored
            .query(64, 64, move |connection| {
                Ok(vec![u8::from(queue_validate_claim(
                    connection,
                    23,
                    &[claimed],
                )?)])
            })
            .await
            .unwrap()[0]
            != 0
    );
    restored.drain().await.unwrap();
}
