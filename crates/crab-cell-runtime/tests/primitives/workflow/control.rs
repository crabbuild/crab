//! Operator pause and restart control paths.

use super::*;

#[test]
fn operator_pause_waits_for_a_live_lease_and_freezes_scheduled_work() {
    let mut connection = connection();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let support = ActivitySupport {
        activity_type: "email".into(),
        definition_digest: definition.digest(),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, status, _) = applied(
        workflow_start(&transaction, &source_target(), 10, &start(5), &definition).unwrap(),
    );
    assert_eq!(status, WorkflowStatus::Running);
    let timer_id: [u8; 16] = transaction
        .query_row("SELECT timer_id FROM workflow_timers", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap()
        .try_into()
        .unwrap();
    let control = |now_ms: i64, action: WorkflowControlAction| {
        workflow_control(
            &transaction,
            &source_target(),
            now_ms,
            &WorkflowControl {
                workflow_id: b"build-42".to_vec(),
                run_id,
                action,
            },
            &definition,
        )
        .unwrap()
    };

    // A live activity lease blocks the operator pause.
    let mut tokens = Tokens(0);
    let claimed = workflow_claim_activities(
        &transaction,
        10,
        1,
        5_000,
        std::slice::from_ref(&support),
        &mut tokens,
    )
    .unwrap();
    assert_eq!(
        claimed.len(),
        1,
        "the start decision schedules one claimable activity"
    );
    assert_eq!(
        control(20, WorkflowControlAction::Pause),
        WorkflowOutcome::Busy
    );

    let completion = ActivityCompletion {
        run_id,
        activity_id: claimed[0].activity_id,
        attempt: claimed[0].attempt,
        lease_token: claimed[0].token,
        completion_token: [8; 16],
        result: b"sent".to_vec(),
        failed: false,
        retryable: false,
    };
    assert!(matches!(
        workflow_complete_activity(&transaction, &source_target(), 30, &completion, &definition)
            .unwrap(),
        ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied { .. })
    ));
    // The completion is an event of its own, so the pause must preserve the
    // sequence the run has now, not the one it started with.
    let sequence_before_pause: u64 = transaction
        .query_row("SELECT event_sequence FROM workflow_runs", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        control(40, WorkflowControlAction::Pause),
        WorkflowOutcome::Applied {
            run_id,
            status: WorkflowStatus::Paused,
            event_sequence: sequence_before_pause,
        },
        "pause keeps the history boundary"
    );

    // Durable work stays queued while paused, but neither class may run it.
    assert_eq!(
        workflow_fire_timer(
            &transaction,
            &source_target(),
            41,
            run_id,
            timer_id,
            &definition
        )
        .unwrap(),
        WorkflowOutcome::NotRunning
    );
    assert!(
        workflow_claim_activities(
            &transaction,
            41,
            1,
            5_000,
            std::slice::from_ref(&support),
            &mut tokens,
        )
        .unwrap()
        .is_empty()
    );
    let state_while_paused: (i64, i64) = transaction
        .query_row(
            "SELECT (SELECT state FROM workflow_activities), (SELECT state FROM workflow_timers)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        state_while_paused,
        (2, 0),
        "the completed activity stays completed and the due timer stays unstarted while paused"
    );

    assert_eq!(
        control(42, WorkflowControlAction::Resume),
        WorkflowOutcome::Applied {
            run_id,
            status: WorkflowStatus::Running,
            event_sequence: sequence_before_pause,
        },
        "resume returns the same run without synthesizing an event"
    );
    assert_eq!(
        applied(
            workflow_fire_timer(
                &transaction,
                &source_target(),
                43,
                run_id,
                timer_id,
                &definition
            )
            .unwrap()
        ),
        (run_id, WorkflowStatus::Completed, sequence_before_pause + 1),
        "the timer that came due while paused fires after resume"
    );
    transaction.commit().unwrap();
}
#[test]
fn operator_restart_clears_terminal_history_and_uses_the_current_definition() {
    let mut connection = connection();
    let original = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let current = Definition {
        digest: Digest::from_bytes([7; 32]),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, _, _) =
        applied(workflow_start(&transaction, &source_target(), 10, &start(5), &original).unwrap());
    let restart = |now_ms: i64, run_id: [u8; 16], definition: &Definition| {
        workflow_control(
            &transaction,
            &source_target(),
            now_ms,
            &WorkflowControl {
                workflow_id: b"build-42".to_vec(),
                run_id,
                action: WorkflowControlAction::Restart {
                    request_id: RequestId::from_bytes([9; 16]),
                    event: b"start".to_vec(),
                },
            },
            definition,
        )
        .unwrap()
    };

    // A live or paused run is not restartable.
    assert_eq!(restart(10, run_id, &current), WorkflowOutcome::Busy);
    assert!(matches!(
        workflow_control(
            &transaction,
            &source_target(),
            12,
            &WorkflowControl {
                workflow_id: b"build-42".to_vec(),
                run_id,
                action: WorkflowControlAction::Pause,
            },
            &original,
        )
        .unwrap(),
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Paused,
            ..
        }
    ));
    assert_eq!(restart(10, run_id, &current), WorkflowOutcome::Busy);

    assert!(matches!(
        workflow_cancel(
            &transaction,
            14,
            &WorkflowSignal {
                workflow_id: b"build-42".to_vec(),
                run_id,
                signal_id: [8; 16],
                event: b"cancel".to_vec(),
            },
        )
        .unwrap(),
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Cancelled,
            ..
        }
    ));

    let (restarted, status, sequence) = applied(restart(10, run_id, &current));
    assert_ne!(restarted, run_id, "restart starts a new run identity");
    assert_eq!(
        (status, sequence),
        (WorkflowStatus::Running, 1),
        "the restarted run starts from its first event"
    );
    let rows: (i64, i64, i64, i64) = transaction
        .query_row(
            "SELECT (SELECT count(*) FROM workflow_runs), \
             (SELECT count(*) FROM workflow_events), \
             (SELECT count(*) FROM workflow_activities), \
             (SELECT count(*) FROM workflow_timers)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        rows,
        (1, 1, 1, 1),
        "terminal history is deleted and the start decision schedules fresh work"
    );
    let digest: Vec<u8> = transaction
        .query_row("SELECT definition_digest FROM workflow_runs", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        digest,
        current.digest().as_bytes().to_vec(),
        "the restarted run pins the definition the operator passed"
    );
    // The same request may not own two runs: cancel the restarted run first so
    // the identity rule, not the busy rule, decides the answer.
    let cancelled = workflow_cancel(
        &transaction,
        10,
        &WorkflowSignal {
            workflow_id: b"build-42".to_vec(),
            run_id: restarted,
            // The restart's request id may not double as a signal id: the two
            // identity spaces derive from the same run identity.
            signal_id: [10; 16],
            event: b"cancel".to_vec(),
        },
    )
    .unwrap();
    assert!(
        matches!(cancelled, WorkflowOutcome::Applied { .. }),
        "cancelling the restarted run returned {cancelled:?}"
    );
    assert_eq!(
        restart(10, restarted, &current),
        WorkflowOutcome::IdentityConflict,
        "the same request cannot restart the run it already started"
    );
    transaction.commit().unwrap();
}
