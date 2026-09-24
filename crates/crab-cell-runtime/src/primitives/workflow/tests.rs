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
