use super::*;
use crate::identity::IncarnationId;
use crate::identity::{ApplicationId, TenantId};
use crab_ltx::rusqlite::Connection;

struct RunningDefinition;

impl WorkflowDefinition for RunningDefinition {
    fn digest(&self) -> Digest {
        Digest::from_bytes([8; 32])
    }

    fn transition(&self, _: &[u8], event: &[u8], _: WorkflowContext) -> Result<WorkflowDecision> {
        Ok(WorkflowDecision {
            status: WorkflowStatus::Running,
            state: event.to_vec(),
            result: None,
            actions: Vec::new(),
        })
    }
}

#[test]
fn embedded_workflow_migration_matches_normative_contract() {
    assert_eq!(
        WORKFLOW_SCHEMA,
        include_str!("../../../docs/contracts/workflow.sql")
    );
}

#[test]
fn status_codec_rejects_unknown_values() {
    assert_eq!(WorkflowStatus::decode(0).unwrap(), WorkflowStatus::Running);
    assert_eq!(WorkflowStatus::decode(4).unwrap(), WorkflowStatus::Paused);
    assert!(WorkflowStatus::decode(5).is_err());
}

#[test]
fn pause_resume_and_terminal_restart_preserve_run_identity_rules() {
    let mut connection = Connection::open_in_memory().unwrap();
    let transaction = connection.transaction().unwrap();
    let source = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NamespaceId::from_bytes([3; 16]),
        b"workflow",
    )
    .unwrap();
    crate::cell::schema::install_runtime_schema_in(
        &transaction,
        source.cell_id(),
        IncarnationId::from_bytes([4; 16]),
        1,
    )
    .unwrap();
    install_workflow_schema(&transaction).unwrap();
    let start = WorkflowStart {
        workflow_id: b"build-42".to_vec(),
        request_id: RequestId::from_bytes([5; 16]),
        event: b"start".to_vec(),
    };
    let WorkflowOutcome::Applied { run_id, .. } =
        workflow_start(&transaction, &source, 10, &start, &RunningDefinition).unwrap()
    else {
        panic!("workflow did not start");
    };
    transaction
        .execute(
            "INSERT INTO workflow_activities(run_id, activity_id, activity_type, input, state, attempt, due_at_ms, expires_at_ms, token, lease_until_ms, completion_token, completion_digest, result) VALUES (?1, ?2, 'test', X'', 1, 1, 10, 100, ?3, 50, NULL, NULL, NULL)",
            (
                run_id.as_slice(),
                [10_u8; 16].as_slice(),
                [11_u8; 16].as_slice(),
            ),
        )
        .unwrap();
    assert_eq!(
        workflow_control(
            &transaction,
            &source,
            11,
            &WorkflowControl {
                workflow_id: start.workflow_id.clone(),
                run_id,
                action: WorkflowControlAction::Pause,
            },
            &RunningDefinition,
        )
        .unwrap(),
        WorkflowOutcome::Busy
    );
    transaction
        .execute(
            "DELETE FROM workflow_activities WHERE run_id = ?1",
            [run_id.as_slice()],
        )
        .unwrap();
    let paused = workflow_control(
        &transaction,
        &source,
        11,
        &WorkflowControl {
            workflow_id: start.workflow_id.clone(),
            run_id,
            action: WorkflowControlAction::Pause,
        },
        &RunningDefinition,
    )
    .unwrap();
    assert!(matches!(
        paused,
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Paused,
            ..
        }
    ));
    assert_eq!(
        workflow_signal(
            &transaction,
            &source,
            12,
            &WorkflowSignal {
                workflow_id: start.workflow_id.clone(),
                run_id,
                signal_id: [6; 16],
                event: b"blocked".to_vec(),
            },
            &RunningDefinition,
        )
        .unwrap(),
        WorkflowOutcome::NotRunning
    );
    workflow_control(
        &transaction,
        &source,
        13,
        &WorkflowControl {
            workflow_id: start.workflow_id.clone(),
            run_id,
            action: WorkflowControlAction::Resume,
        },
        &RunningDefinition,
    )
    .unwrap();
    workflow_cancel(
        &transaction,
        14,
        &WorkflowSignal {
            workflow_id: start.workflow_id.clone(),
            run_id,
            signal_id: [7; 16],
            event: b"cancel".to_vec(),
        },
    )
    .unwrap();
    let restarted = workflow_control(
        &transaction,
        &source,
        15,
        &WorkflowControl {
            workflow_id: start.workflow_id,
            run_id,
            action: WorkflowControlAction::Restart {
                request_id: RequestId::from_bytes([9; 16]),
                event: b"restart".to_vec(),
            },
        },
        &RunningDefinition,
    )
    .unwrap();
    let WorkflowOutcome::Applied {
        run_id: restarted_run,
        ..
    } = restarted
    else {
        panic!("workflow did not restart");
    };
    assert_ne!(run_id, restarted_run);
}

struct SizedStateDefinition(usize);

impl WorkflowDefinition for SizedStateDefinition {
    fn digest(&self) -> Digest {
        Digest::from_bytes([9; 32])
    }

    fn transition(&self, _: &[u8], _: &[u8], _: WorkflowContext) -> Result<WorkflowDecision> {
        Ok(WorkflowDecision {
            status: WorkflowStatus::Running,
            state: vec![b's'; self.0],
            result: None,
            actions: Vec::new(),
        })
    }
}

struct ActivityDefinition {
    input_bytes: usize,
    lifetime_ms: i64,
}

impl WorkflowDefinition for ActivityDefinition {
    fn digest(&self) -> Digest {
        Digest::from_bytes([12; 32])
    }

    fn transition(&self, _: &[u8], _: &[u8], _: WorkflowContext) -> Result<WorkflowDecision> {
        Ok(WorkflowDecision {
            status: WorkflowStatus::Running,
            state: b"running".to_vec(),
            result: None,
            actions: vec![WorkflowAction::Activity {
                activity_type: "probe".to_string(),
                input: vec![b'i'; self.input_bytes],
                due_at_ms: 10,
                expires_at_ms: 10 + self.lifetime_ms,
            }],
        })
    }
}

struct Tokens(u8);

impl ActivityTokenSource for Tokens {
    fn next_token(&mut self) -> Result<[u8; 16]> {
        self.0 = self
            .0
            .checked_add(1)
            .ok_or(Error::Command("test activity token overflow"))?;
        Ok([self.0; 16])
    }
}

fn workflow_connection() -> (Connection, CellTarget) {
    let connection = Connection::open_in_memory().unwrap();
    let source = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NamespaceId::from_bytes([3; 16]),
        b"workflow",
    )
    .unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    crate::cell::schema::install_runtime_schema_in(
        &transaction,
        source.cell_id(),
        IncarnationId::from_bytes([4; 16]),
        1,
    )
    .unwrap();
    install_workflow_schema(&transaction).unwrap();
    transaction.commit().unwrap();
    (connection, source)
}

fn start_request(event: &[u8]) -> WorkflowStart {
    WorkflowStart {
        workflow_id: b"build-42".to_vec(),
        request_id: RequestId::from_bytes([7; 16]),
        event: event.to_vec(),
    }
}

#[test]
fn workflow_state_at_the_one_mib_boundary_is_accepted() {
    let (mut connection, source) = workflow_connection();
    let transaction = connection.transaction().unwrap();
    let outcome = workflow_start(
        &transaction,
        &source,
        10,
        &start_request(b"start"),
        &SizedStateDefinition(MAX_WORKFLOW_BYTES),
    )
    .unwrap();
    assert!(matches!(outcome, WorkflowOutcome::Applied { .. }));
}

#[test]
fn workflow_state_over_one_mib_is_rejected() {
    let (mut connection, source) = workflow_connection();
    let transaction = connection.transaction().unwrap();
    let outcome = workflow_start(
        &transaction,
        &source,
        10,
        &start_request(b"start"),
        &SizedStateDefinition(MAX_WORKFLOW_BYTES + 1),
    );
    assert!(matches!(
        outcome,
        Err(Error::Command("workflow decision exceeds limits"))
    ));
}

#[test]
fn activity_input_over_256_kib_is_rejected() {
    let (mut connection, source) = workflow_connection();
    let transaction = connection.transaction().unwrap();
    let outcome = workflow_start(
        &transaction,
        &source,
        10,
        &start_request(b"start"),
        &ActivityDefinition {
            input_bytes: MAX_ACTIVITY_PAYLOAD_BYTES + 1,
            lifetime_ms: 1_000,
        },
    );
    assert!(matches!(
        outcome,
        Err(Error::Command("invalid workflow activity action"))
    ));
}

#[test]
fn activity_lifetime_past_seven_days_is_rejected() {
    let (mut connection, source) = workflow_connection();
    let transaction = connection.transaction().unwrap();
    let outcome = workflow_start(
        &transaction,
        &source,
        10,
        &start_request(b"start"),
        &ActivityDefinition {
            input_bytes: 0,
            lifetime_ms: MAX_ACTIVITY_LIFETIME_MS + 1,
        },
    );
    assert!(matches!(
        outcome,
        Err(Error::Command("invalid workflow activity action"))
    ));
}

#[test]
fn activity_attempts_stop_at_twenty() {
    let (mut connection, source) = workflow_connection();
    let transaction = connection.transaction().unwrap();
    let definition = RunningDefinition;
    let WorkflowOutcome::Applied { run_id, .. } = workflow_start(
        &transaction,
        &source,
        10,
        &start_request(b"start"),
        &definition,
    )
    .unwrap() else {
        panic!("workflow did not start");
    };
    transaction
        .execute(
            "INSERT INTO workflow_activities(run_id, activity_id, activity_type, input, state, attempt, due_at_ms, expires_at_ms, token, lease_until_ms, completion_token, completion_digest, result) VALUES (?1, ?2, 'probe', X'', 0, ?3, 10, 100, NULL, NULL, NULL, NULL, NULL)",
            (
                run_id.as_slice(),
                [13_u8; 16].as_slice(),
                i64::from(crate::primitives::workflow::activity::MAX_ATTEMPTS),
            ),
        )
        .unwrap();
    let supported = [ActivitySupport {
        activity_type: "probe".to_string(),
        definition_digest: definition.digest(),
    }];
    let mut tokens = Tokens(0);
    assert!(
        workflow_claim_activities(&transaction, 10, 1, 5_000, &supported, &mut tokens)
            .unwrap()
            .is_empty()
    );
}
