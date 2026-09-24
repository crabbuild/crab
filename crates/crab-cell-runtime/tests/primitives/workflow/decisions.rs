//! Start, signal, terminal, effect, and cancellation decisions.

use super::*;

#[test]
fn workflow_event_counter_tracks_history_and_reports_drift() {
    let mut connection = connection();
    let source = source_target();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    workflow_start(&transaction, &source, 10, &start(5), &definition).unwrap();
    crab_cell_runtime::primitives::workflow::verify_workflow_event_count(&transaction).unwrap();
    transaction
        .execute(
            "UPDATE workflow_control SET event_count = event_count + 1 WHERE singleton = 1",
            [],
        )
        .unwrap();
    assert!(
        crab_cell_runtime::primitives::workflow::verify_workflow_event_count(&transaction).is_err()
    );
}
#[test]
fn start_allocates_stable_actions_and_signal_identity_is_conflict_safe() {
    let mut connection = connection();
    let source = source_target();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, status, sequence) =
        applied(workflow_start(&transaction, &source, 10, &start(5), &definition).unwrap());
    assert_eq!((status, sequence), (WorkflowStatus::Running, 1));
    let state: Vec<u8> = transaction
        .query_row(
            "SELECT state FROM workflow_runs WHERE run_id = ?1",
            [run_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    let timer_id: Vec<u8> = transaction
        .query_row(
            "SELECT timer_id FROM workflow_timers WHERE run_id = ?1",
            [run_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    let activity_id: Vec<u8> = transaction
        .query_row(
            "SELECT activity_id FROM workflow_activities WHERE run_id = ?1",
            [run_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, [timer_id, activity_id].concat());
    transaction.commit().unwrap();

    let signal = WorkflowSignal {
        workflow_id: b"build-42".to_vec(),
        run_id,
        signal_id: [6; 16],
        event: b"continue".to_vec(),
    };
    let transaction = connection.transaction().unwrap();
    let wrong = Definition {
        digest: Digest::from_bytes([9; 32]),
    };
    assert!(workflow_signal(&transaction, &source, 11, &signal, &wrong).is_err());
    assert_eq!(
        applied(workflow_signal(&transaction, &source, 11, &signal, &definition).unwrap()),
        (run_id, WorkflowStatus::Running, 2)
    );
    assert_eq!(
        workflow_signal(&transaction, &source, 12, &signal, &definition).unwrap(),
        WorkflowOutcome::Duplicate {
            run_id,
            status: WorkflowStatus::Running,
            event_sequence: 2,
        }
    );
    let mut changed = signal.clone();
    changed.event = b"changed".to_vec();
    assert_eq!(
        workflow_signal(&transaction, &source, 12, &changed, &definition).unwrap(),
        WorkflowOutcome::IdentityConflict
    );
    transaction.commit().unwrap();
}
#[test]
fn invalid_decision_is_rejected_before_any_workflow_rows_are_written() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    assert!(
        workflow_start(
            &transaction,
            &source_target(),
            10,
            &start(5),
            &InvalidDefinition,
        )
        .is_err()
    );
    let rows: (i64, i64, i64) = transaction
        .query_row(
            "SELECT (SELECT count(*) FROM workflow_runs), (SELECT count(*) FROM workflow_events), (SELECT count(*) FROM workflow_timers)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(rows, (0, 0, 0));
    transaction.commit().unwrap();
}
#[test]
fn terminal_transition_inserts_effect_with_cell_command_identity() {
    let mut connection = connection();
    let source = source_target();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, _, _) =
        applied(workflow_start(&transaction, &source, 10, &start(5), &definition).unwrap());
    let outcome = workflow_signal(
        &transaction,
        &source,
        11,
        &WorkflowSignal {
            workflow_id: b"build-42".to_vec(),
            run_id,
            signal_id: [7; 16],
            event: b"finish-with-effect".to_vec(),
        },
        &definition,
    )
    .unwrap();
    assert_eq!(applied(outcome), (run_id, WorkflowStatus::Completed, 2));
    let stored: (Vec<u8>, Vec<u8>, i64) = transaction
        .query_row(
            "SELECT effect_id, operation, expires_at_ms FROM sys_effects",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        stored.0,
        crab_cell_runtime::primitives::effects::effect_id(
            source.cell_id(),
            IncarnationId::from_bytes([2; 16]),
            1,
            0,
        )
        .to_vec()
    );
    assert_eq!(stored.2, 20_000);
    let request = peer_wire::EffectRequest::decode(stored.1.as_slice()).unwrap();
    assert!(request.destination_incarnation.is_empty());
    let command = match request.operation {
        Some(peer_wire::effect_request::Operation::CellCommand(command)) => command,
        None => panic!("workflow effect must contain a typed Cell command"),
    };
    assert_eq!((command.command_id, command.codec_version), (7, 1));
    assert_eq!(command.input, b"canonical-destination-command");
    transaction.commit().unwrap();
}
#[test]
fn effect_transition_rejects_undeclared_or_cross_tenant_targets_before_writes() {
    for event in [
        b"finish-with-undeclared-effect".as_slice(),
        b"finish-with-cross-tenant-effect".as_slice(),
    ] {
        let mut connection = connection();
        let source = source_target();
        let definition = Definition {
            digest: Digest::from_bytes([4; 32]),
        };
        let transaction = connection.transaction().unwrap();
        let (run_id, _, _) =
            applied(workflow_start(&transaction, &source, 10, &start(5), &definition).unwrap());
        let result = workflow_signal(
            &transaction,
            &source,
            11,
            &WorkflowSignal {
                workflow_id: b"build-42".to_vec(),
                run_id,
                signal_id: [7; 16],
                event: event.to_vec(),
            },
            &definition,
        );
        assert!(result.is_err());
        let unchanged: (i64, i64, i64) = transaction
            .query_row(
                "SELECT status, (SELECT count(*) FROM workflow_events), (SELECT count(*) FROM sys_effects) FROM workflow_runs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(unchanged, (0, 1, 0));
        transaction.commit().unwrap();
    }
}
#[test]
fn cancellation_is_idempotent_and_clears_all_outstanding_work() {
    let mut connection = connection();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, _, _) = applied(
        workflow_start(&transaction, &source_target(), 10, &start(5), &definition).unwrap(),
    );
    let cancellation = WorkflowSignal {
        workflow_id: b"build-42".to_vec(),
        run_id,
        signal_id: [7; 16],
        event: b"cancelled by user".to_vec(),
    };
    assert_eq!(
        applied(workflow_cancel(&transaction, 11, &cancellation).unwrap()),
        (run_id, WorkflowStatus::Cancelled, 2)
    );
    assert_eq!(
        workflow_cancel(&transaction, 12, &cancellation).unwrap(),
        WorkflowOutcome::Duplicate {
            run_id,
            status: WorkflowStatus::Cancelled,
            event_sequence: 2,
        }
    );
    let states: (i64, i64) = transaction
        .query_row(
            "SELECT (SELECT state FROM workflow_activities), (SELECT state FROM workflow_timers)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(states, (4, 2));
    transaction.commit().unwrap();
}
