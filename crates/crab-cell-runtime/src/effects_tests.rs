use super::EffectBatch;
use crate::{
    ApplicationId, CellId, CellTarget, Digest, EffectCommandIntent, EffectLeaseOutcome,
    EffectTokenSource, HandlerOutcome, InboxApplyOutcome, InboxDelivery, IncarnationId,
    NamespaceId, TenantId, effect_ack_delivered, effect_claim, effect_cleanup_terminal,
    effect_validate_claim, inbox_apply, inbox_cleanup_expired, install_runtime_schema, peer_wire,
};
use prost::Message;

struct Tokens(u8);

impl EffectTokenSource for Tokens {
    fn next_token(&mut self) -> crate::Result<[u8; 16]> {
        self.0 = self
            .0
            .checked_add(1)
            .ok_or(crate::Error::Command("test effect token overflow"))?;
        Ok([self.0; 16])
    }
}

fn connection(cell: u8, incarnation: u8) -> crab_ltx::rusqlite::Connection {
    let mut connection = crab_ltx::rusqlite::Connection::open_in_memory().unwrap();
    install_runtime_schema(
        &mut connection,
        CellId::from_bytes([cell; 32]),
        IncarnationId::from_bytes([incarnation; 16]),
        1,
    )
    .unwrap();
    connection
        .execute("CREATE TABLE applied(value BLOB NOT NULL)", [])
        .unwrap();
    connection
}

fn source_target() -> CellTarget {
    CellTarget::new(
        TenantId::from_bytes([9; 16]),
        ApplicationId::from_bytes([10; 16]),
        NamespaceId::from_bytes([11; 16]),
        b"source-partition",
    )
    .unwrap()
}

fn source_connection() -> crab_ltx::rusqlite::Connection {
    let mut connection = crab_ltx::rusqlite::Connection::open_in_memory().unwrap();
    install_runtime_schema(
        &mut connection,
        source_target().cell_id(),
        IncarnationId::from_bytes([2; 16]),
        1,
    )
    .unwrap();
    connection
}

fn command_intent(target: CellTarget, input: &[u8], expires_at_ms: i64) -> EffectCommandIntent {
    EffectCommandIntent {
        target,
        command_id: 1,
        codec_version: 1,
        input: input.to_vec(),
        expires_at_ms,
    }
}

#[test]
fn command_effect_is_canonical_and_does_not_pin_destination_incarnation() {
    let mut source = source_connection();
    let source_target = source_target();
    let target = CellTarget::new(
        source_target.tenant(),
        source_target.application(),
        NamespaceId::from_bytes([5; 16]),
        b"target-partition",
    )
    .unwrap();
    let transaction = source.transaction().unwrap();
    let mut batch = EffectBatch::new(&transaction, &source_target, 1, 10).unwrap();
    let effect_id = batch
        .insert_command(
            &transaction,
            &EffectCommandIntent {
                target: target.clone(),
                command_id: 7,
                codec_version: 1,
                input: b"typed-input".to_vec(),
                expires_at_ms: 10_000,
            },
        )
        .unwrap();
    let (destination, operation): (Vec<u8>, Vec<u8>) = transaction
        .query_row(
            "SELECT destination, operation FROM sys_effects WHERE effect_id = ?1",
            [effect_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let request = peer_wire::EffectRequest::decode(operation.as_slice()).unwrap();
    assert_eq!(destination.as_slice(), target.cell_id().as_bytes());
    assert!(request.destination_incarnation.is_empty());
    assert_eq!(request.encode_to_vec(), operation);
    let identity = request.identity.unwrap();
    assert_eq!(identity.effect_id, effect_id);
    assert_eq!(identity.source_cell, source_target.cell_id().as_bytes());
    assert_eq!(identity.source_incarnation, [2; 16]);
    assert_eq!(identity.source_sequence, 1);
    assert_eq!(identity.ordinal, 0);
    transaction.commit().unwrap();
}

#[test]
fn command_effect_rejects_a_source_target_for_another_cell() {
    let mut source = source_connection();
    let other = CellTarget::new(
        source_target().tenant(),
        source_target().application(),
        source_target().namespace(),
        b"another-source",
    )
    .unwrap();
    let transaction = source.transaction().unwrap();
    assert!(EffectBatch::new(&transaction, &other, 1, 10).is_err());
}

#[test]
fn command_effect_rejects_a_foreign_application_before_writes() {
    let mut source = source_connection();
    let source_target = source_target();
    let target = CellTarget::new(
        TenantId::from_bytes([3; 16]),
        ApplicationId::from_bytes([4; 16]),
        NamespaceId::from_bytes([5; 16]),
        b"foreign-application",
    )
    .unwrap();
    let transaction = source.transaction().unwrap();
    let result = EffectBatch::new(&transaction, &source_target, 1, 10)
        .unwrap()
        .insert_command(&transaction, &command_intent(target, b"foreign", 10_000));
    assert!(matches!(
        result,
        Err(crate::Error::Identity(
            "effect target is outside the source application scope"
        ))
    ));
    let count: i64 = transaction
        .query_row("SELECT COUNT(*) FROM sys_effects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    transaction.rollback().unwrap();
}

#[test]
fn target_commit_and_lost_response_retry_execute_destination_once() {
    const EXPIRES_AT_MS: i64 = 10_000;
    const INBOX_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
    let source_target = source_target();
    let destination = CellTarget::new(
        source_target.tenant(),
        source_target.application(),
        NamespaceId::from_bytes([5; 16]),
        b"destination",
    )
    .unwrap();
    let mut source = source_connection();
    let mut target = connection(3, 4);

    let source_transaction = source.transaction().unwrap();
    let operation = command_intent(destination.clone(), b"apply-value", EXPIRES_AT_MS);
    let effect_id = EffectBatch::new(&source_transaction, &source_target, 1, 0)
        .unwrap()
        .insert_command(&source_transaction, &operation)
        .unwrap();
    assert_eq!(
        EffectBatch::new(&source_transaction, &source_target, 1, 0)
            .unwrap()
            .insert_command(&source_transaction, &operation)
            .unwrap(),
        effect_id
    );
    assert!(
        EffectBatch::new(&source_transaction, &source_target, 1, 0)
            .unwrap()
            .insert_command(
                &source_transaction,
                &command_intent(destination, b"changed", EXPIRES_AT_MS),
            )
            .is_err()
    );
    source_transaction.commit().unwrap();

    let source_transaction = source.transaction().unwrap();
    let mut tokens = Tokens(0);
    let first = effect_claim(&source_transaction, 10, 1, 5_000, &mut tokens)
        .unwrap()
        .remove(0);
    source_transaction.commit().unwrap();
    assert!(effect_validate_claim(&source, 11, std::slice::from_ref(&first)).unwrap());

    let delivery = InboxDelivery {
        effect_id: first.effect_id,
        operation_digest: first.operation_digest,
        expires_at_ms: first.expires_at_ms,
    };
    let target_transaction = target.transaction().unwrap();
    assert_eq!(
        inbox_apply(&target_transaction, 12, delivery, 128, |transaction| {
            transaction.execute(
                "INSERT INTO applied(value) VALUES (?1)",
                [b"value".as_slice()],
            )?;
            Ok(HandlerOutcome::Success(b"ok".to_vec()))
        })
        .unwrap(),
        InboxApplyOutcome::Success {
            result: b"ok".to_vec(),
            commit_sequence: 1,
            duplicate: false,
        }
    );
    target_transaction.commit().unwrap();

    let source_transaction = source.transaction().unwrap();
    assert!(
        effect_claim(&source_transaction, 5_010, 1, 5_000, &mut tokens)
            .unwrap()
            .is_empty()
    );
    source_transaction.commit().unwrap();
    let source_transaction = source.transaction().unwrap();
    let second = effect_claim(&source_transaction, 5_210, 1, 5_000, &mut tokens)
        .unwrap()
        .remove(0);
    assert_eq!(second.effect_id, first.effect_id);
    assert_eq!(second.operation, first.operation);
    assert_eq!(second.operation_digest, first.operation_digest);
    assert_eq!(second.attempt, 2);
    source_transaction.commit().unwrap();

    let target_transaction = target.transaction().unwrap();
    assert_eq!(
        inbox_apply(&target_transaction, 5_211, delivery, 128, |_| {
            panic!("duplicate inbox delivery must not invoke its handler")
        })
        .unwrap(),
        InboxApplyOutcome::Success {
            result: b"ok".to_vec(),
            commit_sequence: 1,
            duplicate: true,
        }
    );
    assert_eq!(
        inbox_apply(
            &target_transaction,
            5_211,
            InboxDelivery {
                operation_digest: Digest::from_bytes([99; 32]),
                ..delivery
            },
            128,
            |_| panic!("conflicting inbox delivery must not invoke its handler"),
        )
        .unwrap(),
        InboxApplyOutcome::Conflict
    );
    target_transaction.commit().unwrap();
    assert_eq!(
        target
            .query_row("SELECT count(*) FROM applied", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );

    let source_transaction = source.transaction().unwrap();
    assert_eq!(
        effect_ack_delivered(&source_transaction, 5_212, &second, b"ok").unwrap(),
        EffectLeaseOutcome::Delivered
    );
    assert_eq!(
        effect_ack_delivered(&source_transaction, 5_212, &second, b"ok").unwrap(),
        EffectLeaseOutcome::LeaseLost
    );
    assert_eq!(
        effect_cleanup_terminal(&source_transaction, EXPIRES_AT_MS).unwrap(),
        1
    );
    source_transaction.commit().unwrap();

    let target_transaction = target.transaction().unwrap();
    assert_eq!(
        inbox_cleanup_expired(&target_transaction, EXPIRES_AT_MS).unwrap(),
        0
    );
    assert_eq!(
        inbox_cleanup_expired(&target_transaction, EXPIRES_AT_MS + INBOX_RETENTION_MS,).unwrap(),
        1
    );
    target_transaction.commit().unwrap();
}

#[test]
fn rejected_effect_rolls_back_target_writes_but_publishes_inbox_result() {
    let mut target = connection(3, 4);
    let delivery = InboxDelivery {
        effect_id: [5; 32],
        operation_digest: Digest::from_bytes([6; 32]),
        expires_at_ms: 10_000,
    };
    let transaction = target.transaction().unwrap();
    assert_eq!(
        inbox_apply(&transaction, 10, delivery, 128, |transaction| {
            transaction.execute("INSERT INTO applied(value) VALUES (X'01')", [])?;
            Ok(HandlerOutcome::Rejected(b"denied".to_vec()))
        })
        .unwrap(),
        InboxApplyOutcome::Rejected {
            result: b"denied".to_vec(),
            commit_sequence: 1,
            duplicate: false,
        }
    );
    assert_eq!(
        transaction
            .query_row("SELECT count(*) FROM applied", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        inbox_apply(&transaction, 11, delivery, 128, |_| {
            panic!("stored rejection must not invoke its handler")
        })
        .unwrap(),
        InboxApplyOutcome::Rejected {
            result: b"denied".to_vec(),
            commit_sequence: 1,
            duplicate: true,
        }
    );
    transaction.commit().unwrap();
}

#[test]
fn one_command_cannot_exceed_effect_count_or_byte_limits() {
    let source_target = source_target();
    let destination = CellTarget::new(
        source_target.tenant(),
        source_target.application(),
        NamespaceId::from_bytes([5; 16]),
        b"destination",
    )
    .unwrap();
    let mut source = source_connection();
    let transaction = source.transaction().unwrap();
    let mut effects = EffectBatch::new(&transaction, &source_target, 1, 0).unwrap();
    assert!(
        effects
            .insert_command(
                &transaction,
                &command_intent(destination.clone(), &vec![0; 1 << 20], 10_000),
            )
            .is_err()
    );
    for ordinal in 0..128 {
        effects
            .insert_command(
                &transaction,
                &command_intent(destination.clone(), &[ordinal as u8], 10_000),
            )
            .unwrap();
    }
    assert!(
        effects
            .insert_command(
                &transaction,
                &command_intent(destination, b"overflow", 10_000),
            )
            .is_err()
    );
    transaction.commit().unwrap();
    assert_eq!(
        source
            .query_row("SELECT count(*) FROM sys_effects", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        128
    );
}

#[test]
fn byte_limit_rejection_does_not_consume_the_next_effect_ordinal() {
    let source_target = source_target();
    let destination = CellTarget::new(
        source_target.tenant(),
        source_target.application(),
        NamespaceId::from_bytes([5; 16]),
        b"destination",
    )
    .unwrap();
    let mut source = source_connection();
    let transaction = source.transaction().unwrap();
    let mut effects = EffectBatch::new(&transaction, &source_target, 1, 0).unwrap();
    let large = vec![0; 9_000];
    let mut successful = 0_u32;
    while effects
        .insert_command(
            &transaction,
            &command_intent(destination.clone(), &large, 10_000),
        )
        .is_ok()
    {
        successful += 1;
    }
    let effect_id = effects
        .insert_command(
            &transaction,
            &command_intent(destination, b"after-limit", 10_000),
        )
        .unwrap();
    let operation: Vec<u8> = transaction
        .query_row(
            "SELECT operation FROM sys_effects WHERE effect_id = ?1",
            [effect_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    let request = peer_wire::EffectRequest::decode(operation.as_slice()).unwrap();
    assert_eq!(request.identity.unwrap().ordinal, successful);
    transaction.commit().unwrap();
}

#[test]
fn manual_retry_preserves_identity_and_never_reopens_terminal_effect() {
    let source_target = source_target();
    let destination = CellTarget::new(
        source_target.tenant(),
        source_target.application(),
        NamespaceId::from_bytes([5; 16]),
        b"destination",
    )
    .unwrap();
    let mut source = source_connection();
    let transaction = source.transaction().unwrap();
    EffectBatch::new(&transaction, &source_target, 1, 0)
        .unwrap()
        .insert_command(&transaction, &command_intent(destination, b"work", 5_100))
        .unwrap();
    let mut tokens = Tokens(0);
    let claim = effect_claim(&transaction, 0, 1, 5_000, &mut tokens)
        .unwrap()
        .remove(0);
    assert_eq!(
        crate::effect_retry(&transaction, 4_999, &claim).unwrap(),
        EffectLeaseOutcome::Failed
    );
    assert!(
        effect_claim(&transaction, 4_999, 1, 5_000, &mut tokens)
            .unwrap()
            .is_empty()
    );
    transaction.commit().unwrap();
}
