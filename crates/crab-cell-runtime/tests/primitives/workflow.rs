use std::sync::Arc;

use crab_cell_runtime::{
    ActivityCompletion, ActivityCompletionOutcome, ActivityLeaseOutcome, ActivitySupport,
    ActivityTokenSource, ApplicationId, CatalogEntry, CatalogRole, CellAuthority, CellCatalog,
    CellRuntime, CellTarget, Digest, EffectCommandIntent, HandlerOutcome, IncarnationId,
    MutationIdentity, NamespaceId, Owner, RequestId, SessionId, SqlWorkerPool, TenantId,
    WorkflowAction, WorkflowContext, WorkflowDecision, WorkflowDefinition, WorkflowOutcome,
    WorkflowSignal, WorkflowStart, WorkflowStatus, install_runtime_schema, install_workflow_schema,
    peer_wire, workflow_cancel, workflow_claim_activities, workflow_cleanup_terminal,
    workflow_complete_activity, workflow_extend_activity, workflow_fire_timer, workflow_signal,
    workflow_start, workflow_validate_activity_claim,
};
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Limits};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};
use prost::Message;

const WORKFLOW_NAMESPACE: NamespaceId = NamespaceId::from_bytes([3; 16]);
const EFFECT_NAMESPACE: NamespaceId = NamespaceId::from_bytes([8; 16]);
static EFFECT_TARGETS: [NamespaceId; 1] = [EFFECT_NAMESPACE];

#[derive(Clone, Copy)]
struct Definition {
    digest: Digest,
}

impl WorkflowDefinition for Definition {
    fn digest(&self) -> Digest {
        self.digest
    }

    fn effect_targets(&self) -> &'static [NamespaceId] {
        &EFFECT_TARGETS
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
            b"finish-with-effect" => {
                effect_decision(&context, context.source().tenant(), EFFECT_NAMESPACE)
            }
            b"finish-with-undeclared-effect" => effect_decision(
                &context,
                context.source().tenant(),
                NamespaceId::from_bytes([9; 16]),
            ),
            b"finish-with-cross-tenant-effect" => {
                effect_decision(&context, TenantId::from_bytes([99; 16]), EFFECT_NAMESPACE)
            }
            event => Ok(WorkflowDecision {
                status: WorkflowStatus::Running,
                state: event.to_vec(),
                result: None,
                actions: Vec::new(),
            }),
        }
    }
}

fn effect_decision(
    context: &WorkflowContext,
    tenant: TenantId,
    namespace: NamespaceId,
) -> crab_cell_runtime::Result<WorkflowDecision> {
    Ok(WorkflowDecision {
        status: WorkflowStatus::Completed,
        state: b"done".to_vec(),
        result: Some(b"effect-scheduled".to_vec()),
        actions: vec![WorkflowAction::Effect {
            intent: EffectCommandIntent {
                target: CellTarget::new(
                    tenant,
                    context.source().application(),
                    namespace,
                    b"destination",
                )?,
                command_id: 7,
                codec_version: 1,
                input: b"canonical-destination-command".to_vec(),
                expires_at_ms: 20_000,
            },
        }],
    })
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
        source_target().cell_id(),
        IncarnationId::from_bytes([2; 16]),
        1,
    )
    .unwrap();
    let transaction = connection.transaction().unwrap();
    install_workflow_schema(&transaction).unwrap();
    transaction.commit().unwrap();
    connection
}

fn source_target() -> CellTarget {
    CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        WORKFLOW_NAMESPACE,
        b"workflow",
    )
    .unwrap()
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
fn workflow_event_counter_tracks_history_and_reports_drift() {
    let mut connection = connection();
    let source = source_target();
    let definition = Definition {
        digest: Digest::from_bytes([4; 32]),
    };
    let transaction = connection.transaction().unwrap();
    workflow_start(&transaction, &source, 10, &start(5), &definition).unwrap();
    crab_cell_runtime::verify_workflow_event_count(&transaction).unwrap();
    transaction
        .execute(
            "UPDATE workflow_control SET event_count = event_count + 1 WHERE singleton = 1",
            [],
        )
        .unwrap();
    assert!(crab_cell_runtime::verify_workflow_event_count(&transaction).is_err());
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
        crab_cell_runtime::effect_id(source.cell_id(), IncarnationId::from_bytes([2; 16]), 1, 0,)
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
    let workflow_target = target.clone();
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
                &workflow_target,
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
    let workflow_target = target.clone();
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
                    &workflow_target,
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
