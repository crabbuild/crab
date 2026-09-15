use crab_cell_runtime::{
    CellId, IncarnationId, SessionId, WorkflowAction, WorkflowContext, WorkflowDecision,
    WorkflowDefinition, WorkflowStatus, effect_id, install_kv_schema, install_queue_schema,
    install_runtime_schema, install_workflow_schema, preferred_scanner, scheduler_next_due_ms,
    scheduler_tick,
};

struct ExpiryDefinition;

impl WorkflowDefinition for ExpiryDefinition {
    fn digest(&self) -> crab_cell_runtime::Digest {
        crab_cell_runtime::Digest::from_bytes([9; 32])
    }

    fn transition(
        &self,
        _state: &[u8],
        event: &[u8],
        _context: WorkflowContext,
    ) -> crab_cell_runtime::Result<WorkflowDecision> {
        assert!(event.starts_with(b"activity\0\x01"));
        Ok(WorkflowDecision {
            status: WorkflowStatus::Failed,
            state: b"activity-expired".to_vec(),
            result: Some(event.to_vec()),
            actions: Vec::new(),
        })
    }
}

static EXPIRY_DEFINITION: ExpiryDefinition = ExpiryDefinition;

struct EffectDefinition;

impl WorkflowDefinition for EffectDefinition {
    fn digest(&self) -> crab_cell_runtime::Digest {
        crab_cell_runtime::Digest::from_bytes([10; 32])
    }

    fn transition(
        &self,
        _state: &[u8],
        event: &[u8],
        _context: WorkflowContext,
    ) -> crab_cell_runtime::Result<WorkflowDecision> {
        Ok(WorkflowDecision {
            status: WorkflowStatus::Failed,
            state: b"activity-expired".to_vec(),
            result: Some(event.to_vec()),
            actions: vec![WorkflowAction::Effect {
                destination: CellId::from_bytes([8; 32]),
                operation: event.to_vec(),
                expires_at_ms: 20_000,
            }],
        })
    }
}

static EFFECT_DEFINITION: EffectDefinition = EffectDefinition;

fn connection() -> crab_ltx::rusqlite::Connection {
    let mut connection = crab_ltx::rusqlite::Connection::open_in_memory().unwrap();
    install_runtime_schema(
        &mut connection,
        CellId::from_bytes([1; 32]),
        IncarnationId::from_bytes([2; 16]),
        1,
    )
    .unwrap();
    connection
}

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
        scheduler_tick(&transaction, 10, &[]).unwrap().processed,
        128
    );
    let remaining: usize = transaction
        .query_row("SELECT count(*) FROM sys_requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 12);
    assert_eq!(scheduler_tick(&transaction, 10, &[]).unwrap().processed, 12);
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
            "INSERT INTO queue_messages VALUES (zeroblob(16), X'02', 0, 0, 1, 5, NULL, NULL, NULL)",
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

    let outcome = scheduler_tick(&transaction, 10, &[&EXPIRY_DEFINITION]).unwrap();
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
        scheduler_tick(&transaction, 10, &[&EFFECT_DEFINITION])
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
            CellId::from_bytes([1; 32]),
            IncarnationId::from_bytes([2; 16]),
            1,
            0,
        )
        .to_vec(),
        effect_id(
            CellId::from_bytes([1; 32]),
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
fn rendezvous_scanner_choice_is_order_independent_and_uses_both_nodes() {
    let first = SessionId::from_bytes([1; 16]);
    let second = SessionId::from_bytes([2; 16]);
    let mut winners = std::collections::HashSet::new();
    for shard in 0_u8..=u8::MAX {
        let forward = preferred_scanner(shard, &[first, second]).unwrap().unwrap();
        let reverse = preferred_scanner(shard, &[second, first]).unwrap().unwrap();
        assert_eq!(forward, reverse);
        winners.insert(*forward.as_bytes());
    }
    assert_eq!(winners.len(), 2);
    assert!(preferred_scanner(0, &[first, first]).is_err());
}

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
            "INSERT INTO queue_messages VALUES (zeroblob(16), X'04', 1, 1, 50, 100, zeroblob(16), 80, NULL)",
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
