use std::sync::Arc;

use crab_cell_runtime::cell::actor::CellRuntime;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::catalog::{CatalogEntry, CellCatalog};
use crab_cell_runtime::cell::executor::{HandlerOutcome, MutationIdentity};
use crab_cell_runtime::cell::schema::install_runtime_schema;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::control::Owner;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_cell_runtime::identity::{IncarnationId, RequestId};
use crab_cell_runtime::peer::wire as peer_wire;
use crab_cell_runtime::primitives::effects::EffectCommandIntent;
use crab_cell_runtime::primitives::workflow::{
    ActivityCompletion, ActivityCompletionOutcome, ActivityLeaseOutcome, ActivitySupport,
    ActivityTokenSource, WorkflowAction, WorkflowContext, WorkflowControl, WorkflowControlAction,
    WorkflowDecision, WorkflowDefinition, WorkflowOutcome, WorkflowSignal, WorkflowStart,
    WorkflowStatus, install_workflow_schema, workflow_cancel, workflow_claim_activities,
    workflow_cleanup_terminal, workflow_complete_activity, workflow_control,
    workflow_extend_activity, workflow_fire_timer, workflow_signal, workflow_start,
    workflow_validate_activity_claim,
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

mod activity;
mod control;
mod decisions;
mod recovery;
