use std::sync::Arc;

use crab_cell_runtime::{
    ActivityCompletion, ActivityCompletionOutcome, ActivityLeaseOutcome, ActivitySupport,
    ActivityTokenSource, ApplicationId, CatalogEntry, CatalogRole, CellAuthority, CellCatalog,
    CellRuntime, CellTarget, Digest, HandlerOutcome, IncarnationId, MutationIdentity, NamespaceId,
    Owner, RequestId, SessionId, SqlWorkerPool, TenantId, WorkflowAction, WorkflowContext,
    WorkflowDecision, WorkflowDefinition, WorkflowOutcome, WorkflowSignal, WorkflowStart,
    WorkflowStatus, install_runtime_schema, install_workflow_schema, workflow_cancel,
    workflow_claim_activities, workflow_cleanup_terminal, workflow_complete_activity,
    workflow_extend_activity, workflow_fire_timer, workflow_signal, workflow_start,
    workflow_validate_activity_claim,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

#[derive(Clone, Copy)]
struct Definition {
    digest: Digest,
}

impl WorkflowDefinition for Definition {
    fn digest(&self) -> Digest {
        self.digest
    }

    fn transition(
        &self,
        _state: &[u8],
        event: &[u8],
        context: WorkflowContext,
    ) -> crab_cell_runtime::Result<WorkflowDecision> {
        match event {
            b"start" => {
                let timer = context.action_id(0);
                let activity = context.action_id(1);
                let mut state = timer.to_vec();
                state.extend_from_slice(&activity);
                Ok(WorkflowDecision {
                    status: WorkflowStatus::Running,
                    state,
                    result: None,
                    actions: vec![
                        WorkflowAction::Timer { due_at_ms: 20 },
                        WorkflowAction::Activity {
                            activity_type: "email".into(),
                            input: b"payload".to_vec(),
                            due_at_ms: 10,
                            expires_at_ms: 20_000,
                        },
                    ],
                })
            }
            value if value.starts_with(b"timer\0") => Ok(WorkflowDecision {
                status: WorkflowStatus::Completed,
                state: b"done".to_vec(),
                result: Some(b"timer-fired".to_vec()),
                actions: Vec::new(),
            }),
            b"finish" => Ok(WorkflowDecision {
                status: WorkflowStatus::Completed,
                state: b"done".to_vec(),
                result: Some(b"signalled".to_vec()),
                actions: Vec::new(),
            }),
            event => Ok(WorkflowDecision {
                status: WorkflowStatus::Running,
                state: event.to_vec(),
                result: None,
                actions: Vec::new(),
            }),
        }
    }
}

struct InvalidDefinition;

impl WorkflowDefinition for InvalidDefinition {
    fn digest(&self) -> Digest {
        Digest::from_bytes([12; 32])
    }

    fn transition(
        &self,
        _state: &[u8],
        _event: &[u8],
        _context: WorkflowContext,
    ) -> crab_cell_runtime::Result<WorkflowDecision> {
        Ok(WorkflowDecision {
            status: WorkflowStatus::Completed,
            state: Vec::new(),
            result: None,
            actions: vec![WorkflowAction::Timer { due_at_ms: 20 }],
        })
    }
}

struct Tokens(u8);

impl ActivityTokenSource for Tokens {
    fn next_token(&mut self) -> crab_cell_runtime::Result<[u8; 16]> {
        self.0 = self
            .0
            .checked_add(1)
            .ok_or(crab_cell_runtime::Error::Command(
                "test activity token overflow",
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
    install_workflow_schema(&transaction).unwrap();
    transaction.commit().unwrap();
    connection
}

fn start(request_id: u8) -> WorkflowStart {
    WorkflowStart {
        workflow_id: b"build-42".to_vec(),
        request_id: RequestId::from_bytes([request_id; 16]),
        event: b"start".to_vec(),
    }
}

fn applied(outcome: WorkflowOutcome) -> ([u8; 16], WorkflowStatus, u64) {
    match outcome {
        WorkflowOutcome::Applied {
            run_id,
            status,
            event_sequence,
        } => (run_id, status, event_sequence),
        other => panic!("expected applied workflow outcome, got {other:?}"),
    }
}

#[test]
fn start_allocates_stable_actions_and_signal_identity_is_conflict_safe() {
    let mut connection = connection();
    let namespace = NamespaceId::from_bytes([3; 16]);
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, status, sequence) =
        applied(workflow_start(&transaction, namespace, 10, &start(5), &definition).unwrap());
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
    assert!(workflow_signal(&transaction, 11, &signal, &wrong).is_err());
    assert_eq!(
        applied(workflow_signal(&transaction, 11, &signal, &definition).unwrap()),
        (run_id, WorkflowStatus::Running, 2)
    );
    assert_eq!(
        workflow_signal(&transaction, 12, &signal, &definition).unwrap(),
        WorkflowOutcome::Duplicate {
            run_id,
            status: WorkflowStatus::Running,
            event_sequence: 2,
        }
    );
    let mut changed = signal.clone();
    changed.event = b"changed".to_vec();
    assert_eq!(
        workflow_signal(&transaction, 12, &changed, &definition).unwrap(),
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
            NamespaceId::from_bytes([3; 16]),
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
fn cancellation_is_idempotent_and_clears_all_outstanding_work() {
    let mut connection = connection();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, _, _) = applied(
        workflow_start(
            &transaction,
            NamespaceId::from_bytes([3; 16]),
            10,
            &start(5),
            &definition,
        )
        .unwrap(),
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

#[test]
fn due_timer_fires_once_and_terminal_transition_cancels_sibling_activity() {
    let mut connection = connection();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    let (run_id, _, _) = applied(
        workflow_start(
            &transaction,
            NamespaceId::from_bytes([3; 16]),
            10,
            &start(5),
            &definition,
        )
        .unwrap(),
    );
    let timer_id: [u8; 16] = transaction
        .query_row("SELECT timer_id FROM workflow_timers", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        workflow_fire_timer(&transaction, 19, run_id, timer_id, &definition).unwrap(),
        WorkflowOutcome::NotDue
    );
    assert_eq!(
        applied(workflow_fire_timer(&transaction, 20, run_id, timer_id, &definition).unwrap()),
        (run_id, WorkflowStatus::Completed, 2)
    );
    assert_eq!(
        workflow_fire_timer(&transaction, 21, run_id, timer_id, &definition).unwrap(),
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
        workflow_start(
            &transaction,
            NamespaceId::from_bytes([3; 16]),
            10,
            &start(5),
            &definition,
        )
        .unwrap(),
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
        workflow_complete_activity(&transaction, 101, &failed, &definition).unwrap(),
        ActivityCompletionOutcome::Retrying { due_at_ms: 301 }
    );
    assert_eq!(
        workflow_complete_activity(&transaction, 102, &failed, &definition).unwrap(),
        ActivityCompletionOutcome::Duplicate {
            result: b"transient".to_vec(),
        }
    );
    let mut conflict = failed.clone();
    conflict.result = b"different".to_vec();
    assert_eq!(
        workflow_complete_activity(&transaction, 102, &conflict, &definition).unwrap(),
        ActivityCompletionOutcome::IdentityConflict
    );

    let second = workflow_claim_activities(&transaction, 301, 1, 5_000, &[support], &mut tokens)
        .unwrap()
        .remove(0);
    assert_eq!(second.attempt, 2);
    assert_eq!(
        workflow_complete_activity(&transaction, 302, &failed, &definition).unwrap(),
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
        workflow_complete_activity(&transaction, 302, &completed, &definition).unwrap(),
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
        workflow_start(
            &transaction,
            NamespaceId::from_bytes([3; 16]),
            10,
            &start(5),
            &definition,
        )
        .unwrap(),
    );
    let timer_id: [u8; 16] = transaction
        .query_row("SELECT timer_id FROM workflow_timers", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap()
        .try_into()
        .unwrap();
    applied(workflow_fire_timer(&transaction, 20, run_id, timer_id, &definition).unwrap());
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

#[tokio::test]
async fn published_workflow_restores_from_exact_root_on_a_new_owner() {
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
            CatalogEntry::new(
                &target,
                CatalogRole::Workflow,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
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
            install_workflow_schema,
        )
        .await
        .unwrap();
    let definition = Definition {
        digest: Digest::from_bytes([5; 32]),
    };
    let request = start(7);
    handle
        .execute(
            MutationIdentity {
                request_id: RequestId::from_bytes([8; 16]),
                issued_at_ms: 10,
                expires_at_ms: 10_000,
            },
            Digest::from_bytes([9; 32]),
            10,
            128,
            128,
            move |transaction| match workflow_start(
                transaction,
                namespace,
                10,
                &request,
                &definition,
            )? {
                WorkflowOutcome::Applied { run_id, .. } => {
                    Ok(HandlerOutcome::Success(run_id.to_vec()))
                }
                _ => Ok(HandlerOutcome::Rejected(b"workflow exists".to_vec())),
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
    assert_eq!(idle.value().next_due_ms, Some(10));
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
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let count = connection.query_row(
                    "SELECT count(*) FROM workflow_runs WHERE status = 0 AND event_sequence = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )?;
                Ok(vec![count as u8])
            })
            .await
            .unwrap(),
        vec![1]
    );
    let claimed = Arc::new(std::sync::Mutex::new(None));
    let observed = claimed.clone();
    restored
        .execute(
            MutationIdentity {
                request_id: RequestId::from_bytes([12; 16]),
                issued_at_ms: 11,
                expires_at_ms: 10_000,
            },
            Digest::from_bytes([13; 32]),
            11,
            128,
            512,
            move |transaction| {
                let mut tokens = Tokens(20);
                let claim = workflow_claim_activities(
                    transaction,
                    11,
                    1,
                    5_000,
                    &[ActivitySupport {
                        activity_type: "email".into(),
                        definition_digest: Digest::from_bytes([5; 32]),
                    }],
                    &mut tokens,
                )?
                .into_iter()
                .next()
                .ok_or(crab_cell_runtime::Error::Command(
                    "missing restored workflow activity",
                ))?;
                *observed.lock().unwrap() = Some(claim);
                Ok(HandlerOutcome::Success(b"claimed".to_vec()))
            },
        )
        .await
        .unwrap();
    let claim = claimed.lock().unwrap().clone().unwrap();
    assert_eq!(
        restored
            .query(64, 64, {
                let claim = claim.clone();
                move |connection| {
                    Ok(vec![u8::from(workflow_validate_activity_claim(
                        connection,
                        12,
                        &[claim],
                    )?)])
                }
            })
            .await
            .unwrap(),
        vec![1]
    );
    restored
        .execute(
            MutationIdentity {
                request_id: RequestId::from_bytes([14; 16]),
                issued_at_ms: 13,
                expires_at_ms: 10_000,
            },
            Digest::from_bytes([15; 32]),
            13,
            128,
            128,
            move |transaction| {
                let completion = ActivityCompletion {
                    run_id: claim.run_id,
                    activity_id: claim.activity_id,
                    attempt: claim.attempt,
                    lease_token: claim.token,
                    completion_token: [16; 16],
                    result: b"sent".to_vec(),
                    failed: false,
                    retryable: false,
                };
                match workflow_complete_activity(
                    transaction,
                    13,
                    &completion,
                    &Definition {
                        digest: Digest::from_bytes([5; 32]),
                    },
                )? {
                    ActivityCompletionOutcome::Applied(_) => {
                        Ok(HandlerOutcome::Success(b"completed".to_vec()))
                    }
                    _ => Ok(HandlerOutcome::Rejected(b"completion rejected".to_vec())),
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let state =
                    connection.query_row("SELECT state FROM workflow_activities", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                let events =
                    connection.query_row("SELECT count(*) FROM workflow_events", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                Ok(vec![state as u8, events as u8])
            })
            .await
            .unwrap(),
        vec![2, 2]
    );
    restored.drain().await.unwrap();
}
