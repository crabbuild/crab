//! Activity leases, timer firing, and retention cleanup.

use super::*;

#[test]
fn due_timer_fires_once_and_terminal_transition_cancels_sibling_activity() {
    let mut connection = connection();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, _, _) = applied(
        workflow_start(&transaction, &source_target(), 10, &start(5), &definition).unwrap(),
    );
    let timer_id: [u8; 16] = transaction
        .query_row("SELECT timer_id FROM workflow_timers", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        workflow_fire_timer(
            &transaction,
            &source_target(),
            19,
            run_id,
            timer_id,
            &definition
        )
        .unwrap(),
        WorkflowOutcome::NotDue
    );
    assert_eq!(
        applied(
            workflow_fire_timer(
                &transaction,
                &source_target(),
                20,
                run_id,
                timer_id,
                &definition
            )
            .unwrap(),
        ),
        (run_id, WorkflowStatus::Completed, 2)
    );
    assert_eq!(
        workflow_fire_timer(
            &transaction,
            &source_target(),
            21,
            run_id,
            timer_id,
            &definition
        )
        .unwrap(),
        WorkflowOutcome::Duplicate {
            run_id,
            status: WorkflowStatus::Completed,
            event_sequence: 2,
        }
    );
    let activity_state: i64 = transaction
        .query_row("SELECT state FROM workflow_activities", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(activity_state, 4);
    transaction.commit().unwrap();
}
#[test]
fn activity_claim_retry_extension_and_completion_bind_exact_attempt() {
    let mut connection = connection();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let support = ActivitySupport {
        activity_type: "email".into(),
        definition_digest: definition.digest(),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, _, _) = applied(
        workflow_start(&transaction, &source_target(), 10, &start(5), &definition).unwrap(),
    );
    let mut tokens = Tokens(0);
    assert!(
        workflow_claim_activities(
            &transaction,
            10,
            1,
            5_000,
            &[ActivitySupport {
                activity_type: "email".into(),
                definition_digest: Digest::from_bytes([99; 32]),
            }],
            &mut tokens,
        )
        .unwrap()
        .is_empty()
    );
    let first = workflow_claim_activities(
        &transaction,
        10,
        1,
        5_000,
        std::slice::from_ref(&support),
        &mut tokens,
    )
    .unwrap()
    .remove(0);
    assert_eq!(first.attempt, 1);
    assert!(
        workflow_validate_activity_claim(&transaction, 11, std::slice::from_ref(&first)).unwrap()
    );
    assert!(
        !workflow_validate_activity_claim(&transaction, 4_011, std::slice::from_ref(&first))
            .unwrap()
    );
    assert_eq!(
        workflow_extend_activity(&transaction, 100, &first, 10_000).unwrap(),
        ActivityLeaseOutcome::Extended {
            lease_until_ms: 10_100,
        }
    );

    let failed = ActivityCompletion {
        run_id,
        activity_id: first.activity_id,
        attempt: first.attempt,
        lease_token: first.token,
        completion_token: [7; 16],
        result: b"transient".to_vec(),
        failed: true,
        retryable: true,
    };
    assert_eq!(
        workflow_complete_activity(&transaction, &source_target(), 101, &failed, &definition)
            .unwrap(),
        ActivityCompletionOutcome::Retrying { due_at_ms: 301 }
    );
    assert_eq!(
        workflow_complete_activity(&transaction, &source_target(), 102, &failed, &definition)
            .unwrap(),
        ActivityCompletionOutcome::Duplicate {
            result: b"transient".to_vec(),
        }
    );
    let mut conflict = failed.clone();
    conflict.result = b"different".to_vec();
    assert_eq!(
        workflow_complete_activity(&transaction, &source_target(), 102, &conflict, &definition)
            .unwrap(),
        ActivityCompletionOutcome::IdentityConflict
    );

    let second = workflow_claim_activities(&transaction, 301, 1, 5_000, &[support], &mut tokens)
        .unwrap()
        .remove(0);
    assert_eq!(second.attempt, 2);
    assert_eq!(
        workflow_complete_activity(&transaction, &source_target(), 302, &failed, &definition)
            .unwrap(),
        ActivityCompletionOutcome::LeaseLost
    );
    let completed = ActivityCompletion {
        run_id,
        activity_id: second.activity_id,
        attempt: second.attempt,
        lease_token: second.token,
        completion_token: [8; 16],
        result: b"sent".to_vec(),
        failed: false,
        retryable: false,
    };
    assert_eq!(
        workflow_complete_activity(&transaction, &source_target(), 302, &completed, &definition)
            .unwrap(),
        ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
            run_id,
            status: WorkflowStatus::Running,
            event_sequence: 2,
        })
    );
    transaction.commit().unwrap();
}
#[test]
fn terminal_cleanup_removes_children_only_after_retention() {
    const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
    let mut connection = connection();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, _, _) = applied(
        workflow_start(&transaction, &source_target(), 10, &start(5), &definition).unwrap(),
    );
    let timer_id: [u8; 16] = transaction
        .query_row("SELECT timer_id FROM workflow_timers", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap()
        .try_into()
        .unwrap();
    applied(
        workflow_fire_timer(
            &transaction,
            &source_target(),
            20,
            run_id,
            timer_id,
            &definition,
        )
        .unwrap(),
    );
    assert_eq!(
        workflow_cleanup_terminal(&transaction, RETENTION_MS + 19).unwrap(),
        0
    );
    assert_eq!(
        workflow_cleanup_terminal(&transaction, RETENTION_MS + 20).unwrap(),
        1
    );
    let rows: (i64, i64, i64, i64) = transaction
        .query_row(
            "SELECT (SELECT count(*) FROM workflow_runs), (SELECT count(*) FROM workflow_events), (SELECT count(*) FROM workflow_activities), (SELECT count(*) FROM workflow_timers)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(rows, (0, 0, 0, 0));
    transaction.commit().unwrap();
}
