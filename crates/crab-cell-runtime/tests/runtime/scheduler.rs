use crab_cell_runtime::cell::schema::install_runtime_schema;
use crab_cell_runtime::fleet::scheduler::{
    SchedulerFleet, preferred_scanner, scheduler_next_due_ms, scheduler_tick,
};
use crab_cell_runtime::identity::IncarnationId;
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_cell_runtime::node::{NodeAdvertisement, NodeCapacity};
use crab_cell_runtime::primitives::blob::install_blob_schema;
use crab_cell_runtime::primitives::cron::{CronTarget, install_cron_schema};
use crab_cell_runtime::primitives::effects::{EffectCommandIntent, effect_id};
use crab_cell_runtime::primitives::kv::install_kv_schema;
use crab_cell_runtime::primitives::queue::{QueueDeadLetterTarget, install_queue_schema};
use crab_cell_runtime::primitives::workflow::{
    WorkflowAction, WorkflowContext, WorkflowDecision, WorkflowDefinition, WorkflowStatus,
    install_workflow_schema,
};
use ed25519_dalek::SigningKey;

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

const EFFECT_NAMESPACE: NamespaceId = NamespaceId::from_bytes([8; 16]);
static EFFECT_TARGETS: [NamespaceId; 1] = [EFFECT_NAMESPACE];

impl WorkflowDefinition for EffectDefinition {
    fn digest(&self) -> crab_cell_runtime::Digest {
        crab_cell_runtime::Digest::from_bytes([10; 32])
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
        Ok(WorkflowDecision {
            status: WorkflowStatus::Failed,
            state: b"activity-expired".to_vec(),
            result: Some(event.to_vec()),
            actions: vec![WorkflowAction::Effect {
                intent: EffectCommandIntent {
                    target: CellTarget::new(
                        context.source().tenant(),
                        context.source().application(),
                        EFFECT_NAMESPACE,
                        b"destination",
                    )?,
                    command_id: 7,
                    codec_version: 1,
                    input: event.to_vec(),
                    expires_at_ms: 20_000,
                },
            }],
        })
    }
}

static EFFECT_DEFINITION: EffectDefinition = EffectDefinition;

fn connection() -> crab_ltx::rusqlite::Connection {
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

fn source_target() -> CellTarget {
    CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NamespaceId::from_bytes([3; 16]),
        b"scheduler",
    )
    .unwrap()
}

mod public_tick;
mod scanner;
mod summary;
mod tick;
