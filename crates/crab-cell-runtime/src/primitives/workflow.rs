//! Workflow primitive: runs, timers, activities, signals, and controls.
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::identity::RequestId;
use crate::identity::{CellTarget, Digest, NamespaceId};
use crate::primitives::effects::EffectBatch;
use crate::primitives::effects::EffectCommandIntent;
use crate::primitives::effects::validate_effect_command_intent;
use crate::{Error, Result};

mod activity;
mod activity_api;
mod activity_codec;
mod api;
mod maintenance;

use maintenance::next_effect_batch;
pub use maintenance::workflow_fire_timer;
pub(crate) use maintenance::{workflow_fail_one_expired_activity, workflow_fire_one_due_timer};

pub use activity::{
    ActivityClaim, ActivityCompletion, ActivityCompletionOutcome, ActivityLeaseOutcome,
    ActivitySupport, ActivityTokenSource, MAX_ACTIVITY_PAYLOAD_BYTES, SystemActivityTokens,
    workflow_claim_activities, workflow_cleanup_terminal, workflow_complete_activity,
    workflow_extend_activity, workflow_validate_activity_claim,
};
pub(crate) use activity::{workflow_cleanup_terminal_bounded, workflow_reclaim_expired_bounded};
pub use activity_api::{
    ActivityCancellation, ActivityContext, ActivityExecution, ActivityHandler, ActivityRunOutcome,
    ActivitySupervisor, ActivitySupervisorError, BlockingActivityHandler, WorkflowActivities,
    WorkflowActivityClaimCommand, WorkflowActivityClaimRequest, WorkflowActivityCompleteCommand,
    WorkflowActivityExtendCommand, WorkflowActivityExtendRequest, WorkflowActivityModule,
    WorkflowActivityValidateQuery, WorkflowActivityValidateRequest, register_activity,
    register_blocking_activity, register_workflow_activities,
};
pub use api::{
    WorkflowCancelCommand, WorkflowControlCommand, WorkflowGetQuery, WorkflowGetRequest,
    WorkflowModule, WorkflowNamespace, WorkflowSignalCommand, WorkflowStartCommand,
    register_workflow,
};

const WORKFLOW_SCHEMA: &str = include_str!("../migrations/workflow.sql");
pub(crate) const WORKFLOW_TABLE: &str = "workflow_activities";
const MAX_WORKFLOW_BYTES: usize = 1 << 20;
const MAX_ACTIONS: usize = 128;
const MAX_EVENTS_PER_CELL: u64 = 100_000;
const MAX_ACTIVITY_LIFETIME_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

/// Durable workflow run state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Paused,
}

impl WorkflowStatus {
    fn encode(self) -> i64 {
        match self {
            Self::Running => 0,
            Self::Completed => 1,
            Self::Failed => 2,
            Self::Cancelled => 3,
            Self::Paused => 4,
        }
    }

    fn decode(value: i64) -> Result<Self> {
        match value {
            0 => Ok(Self::Running),
            1 => Ok(Self::Completed),
            2 => Ok(Self::Failed),
            3 => Ok(Self::Cancelled),
            4 => Ok(Self::Paused),
            _ => Err(Error::Command("invalid stored workflow status")),
        }
    }
}

/// One deterministic scheduling intention returned by a workflow definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowAction {
    Activity {
        activity_type: String,
        input: Vec<u8>,
        due_at_ms: i64,
        expires_at_ms: i64,
    },
    Timer {
        due_at_ms: i64,
    },
    Effect {
        intent: EffectCommandIntent,
    },
}

/// Complete result of one pure workflow transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowDecision {
    pub status: WorkflowStatus,
    pub state: Vec<u8>,
    pub result: Option<Vec<u8>>,
    pub actions: Vec<WorkflowAction>,
}

/// Stable inputs available to a workflow definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowContext {
    source: CellTarget,
    run_id: [u8; 16],
    event_sequence: u64,
    now_ms: i64,
}

impl WorkflowContext {
    /// Returns the source Cell so effects can derive same-tenant application targets.
    #[must_use]
    pub const fn source(&self) -> &CellTarget {
        &self.source
    }

    #[must_use]
    pub const fn run_id(&self) -> [u8; 16] {
        self.run_id
    }

    #[must_use]
    pub const fn event_sequence(&self) -> u64 {
        self.event_sequence
    }

    #[must_use]
    pub const fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// Derives the ID that the action at `ordinal` will receive if committed.
    #[must_use]
    pub fn action_id(&self, ordinal: u32) -> [u8; 16] {
        action_id(self.run_id, self.event_sequence, ordinal)
    }
}

/// A statically registered, deterministic workflow state machine.
pub trait WorkflowDefinition: Send + Sync + 'static {
    fn digest(&self) -> Digest;

    /// Declares every namespace this definition may target with an effect.
    fn effect_targets(&self) -> &'static [NamespaceId] {
        &[]
    }

    fn transition(
        &self,
        state: &[u8],
        event: &[u8],
        context: WorkflowContext,
    ) -> Result<WorkflowDecision>;
}

/// Inputs that create one new workflow run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowStart {
    pub workflow_id: Vec<u8>,
    pub request_id: RequestId,
    pub event: Vec<u8>,
}

/// Idempotent external signal for one exact run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowSignal {
    pub workflow_id: Vec<u8>,
    pub run_id: [u8; 16],
    pub signal_id: [u8; 16],
    pub event: Vec<u8>,
}

/// Business outcome of a workflow operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowOutcome {
    Applied {
        run_id: [u8; 16],
        status: WorkflowStatus,
        event_sequence: u64,
    },
    Duplicate {
        run_id: [u8; 16],
        status: WorkflowStatus,
        event_sequence: u64,
    },
    AlreadyExists,
    IdentityConflict,
    RunMismatch,
    NotRunning,
    NotDue,
    Busy,
}

/// Administrative transition for one exact workflow run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowControl {
    pub workflow_id: Vec<u8>,
    pub run_id: [u8; 16],
    pub action: WorkflowControlAction,
}

/// Durable workflow operator action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowControlAction {
    Pause,
    Resume,
    Restart {
        request_id: RequestId,
        event: Vec<u8>,
    },
}

/// Materialized current state of one workflow run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowRun {
    pub workflow_id: Vec<u8>,
    pub run_id: [u8; 16],
    pub definition_digest: Digest,
    pub status: WorkflowStatus,
    pub state: Vec<u8>,
    pub event_sequence: u64,
    pub result: Option<Vec<u8>>,
}

/// Installs the exact version-one Workflow schema during bootstrap or migration.
pub fn install_workflow_schema(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(WORKFLOW_SCHEMA)?;
    Ok(())
}

/// Starts one workflow and applies its first deterministic transition atomically.
pub fn workflow_start(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    now_ms: i64,
    request: &WorkflowStart,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    let mut effects = next_effect_batch(transaction, source, now_ms)?;
    workflow_start_with_effects(
        transaction,
        &mut effects,
        source,
        now_ms,
        request,
        definition,
    )
}

fn workflow_start_with_effects(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    source: &CellTarget,
    now_ms: i64,
    request: &WorkflowStart,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    validate_now(now_ms)?;
    validate_identifier(&request.workflow_id)?;
    validate_event(&request.event)?;
    if transaction
        .query_row(
            "SELECT 1 FROM workflow_runs WHERE workflow_id = ?1",
            [request.workflow_id.as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        return Ok(WorkflowOutcome::AlreadyExists);
    }

    let run_id = run_id(source.namespace(), request.request_id);
    let sequence = next_sequence(transaction, 0)?;
    let decision = definition.transition(
        &[],
        &request.event,
        WorkflowContext {
            source: source.clone(),
            run_id,
            event_sequence: sequence,
            now_ms,
        },
    )?;
    validate_decision(&decision, source, definition.effect_targets(), now_ms)?;
    transaction.execute(
        "INSERT INTO workflow_runs(workflow_id, run_id, definition_digest, status, state, event_sequence, result, completed_at_ms) VALUES (?1, ?2, ?3, 0, X'', 0, NULL, NULL)",
        (
            request.workflow_id.as_slice(),
            run_id.as_slice(),
            definition.digest().as_bytes().as_slice(),
        ),
    )?;
    insert_event(
        transaction,
        run_id,
        sequence,
        event_id(run_id, request.request_id.as_bytes()),
        &request.event,
    )?;
    apply_decision(transaction, effects, run_id, sequence, now_ms, decision)
}

/// Applies one idempotent signal to a running workflow.
pub fn workflow_signal(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    now_ms: i64,
    signal: &WorkflowSignal,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    let mut effects = next_effect_batch(transaction, source, now_ms)?;
    workflow_signal_with_effects(
        transaction,
        &mut effects,
        source,
        now_ms,
        signal,
        definition,
    )
}

fn workflow_signal_with_effects(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    source: &CellTarget,
    now_ms: i64,
    signal: &WorkflowSignal,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    validate_now(now_ms)?;
    validate_identifier(&signal.workflow_id)?;
    validate_event(&signal.event)?;
    let run = load_run(transaction, &signal.workflow_id)?;
    let Some(run) = run else {
        return Ok(WorkflowOutcome::RunMismatch);
    };
    if run.run_id != signal.run_id {
        return Ok(WorkflowOutcome::RunMismatch);
    }
    verify_definition(&run, definition)?;
    let id = event_id(run.run_id, &signal.signal_id);
    if let Some(digest) = stored_event_digest(transaction, run.run_id, id)? {
        return if digest == event_digest(&signal.event) {
            Ok(WorkflowOutcome::Duplicate {
                run_id: run.run_id,
                status: run.status,
                event_sequence: run.event_sequence,
            })
        } else {
            Ok(WorkflowOutcome::IdentityConflict)
        };
    }
    if run.status != WorkflowStatus::Running {
        return Ok(WorkflowOutcome::NotRunning);
    }
    let (sequence, decision) =
        prepare_transition(transaction, source, &run, definition, &signal.event, now_ms)?;
    commit_transition(
        transaction,
        effects,
        run.run_id,
        sequence,
        id,
        &signal.event,
        now_ms,
        decision,
    )
}

/// Cancels a running or paused workflow and its outstanding local work.
pub fn workflow_cancel(
    transaction: &Transaction<'_>,
    now_ms: i64,
    signal: &WorkflowSignal,
) -> Result<WorkflowOutcome> {
    validate_now(now_ms)?;
    validate_identifier(&signal.workflow_id)?;
    validate_event(&signal.event)?;
    let Some(run) = load_run(transaction, &signal.workflow_id)? else {
        return Ok(WorkflowOutcome::RunMismatch);
    };
    if run.run_id != signal.run_id {
        return Ok(WorkflowOutcome::RunMismatch);
    }
    let id = event_id(run.run_id, &signal.signal_id);
    if let Some(digest) = stored_event_digest(transaction, run.run_id, id)? {
        return if digest == event_digest(&signal.event) {
            Ok(WorkflowOutcome::Duplicate {
                run_id: run.run_id,
                status: run.status,
                event_sequence: run.event_sequence,
            })
        } else {
            Ok(WorkflowOutcome::IdentityConflict)
        };
    }
    if !matches!(run.status, WorkflowStatus::Running | WorkflowStatus::Paused) {
        return Ok(WorkflowOutcome::NotRunning);
    }
    let sequence = next_sequence(transaction, run.event_sequence)?;
    insert_event(transaction, run.run_id, sequence, id, &signal.event)?;
    if transaction.execute(
        "UPDATE workflow_runs SET status = 3, event_sequence = ?1, completed_at_ms = ?2 WHERE run_id = ?3 AND status IN (0, 4)",
        (sequence as i64, now_ms, run.run_id.as_slice()),
    )? != 1
    {
        return Err(Error::Command("workflow run changed during cancellation"));
    }
    cancel_outstanding(transaction, run.run_id)?;
    Ok(WorkflowOutcome::Applied {
        run_id: run.run_id,
        status: WorkflowStatus::Cancelled,
        event_sequence: sequence,
    })
}

/// Pauses, resumes, or restarts one exact workflow run.
pub fn workflow_control(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    now_ms: i64,
    request: &WorkflowControl,
    definition: &dyn WorkflowDefinition,
) -> Result<WorkflowOutcome> {
    validate_now(now_ms)?;
    validate_identifier(&request.workflow_id)?;
    let Some(run) = load_run(transaction, &request.workflow_id)? else {
        return Ok(WorkflowOutcome::RunMismatch);
    };
    if run.run_id != request.run_id {
        return Ok(WorkflowOutcome::RunMismatch);
    }
    match &request.action {
        WorkflowControlAction::Pause => {
            if run.status != WorkflowStatus::Running {
                return Ok(WorkflowOutcome::NotRunning);
            }
            let leased = transaction.query_row(
                "SELECT count(*) FROM workflow_activities WHERE run_id = ?1 AND state = 1",
                [run.run_id.as_slice()],
                |row| row.get::<_, i64>(0),
            )?;
            if leased != 0 {
                return Ok(WorkflowOutcome::Busy);
            }
            if transaction.execute(
                "UPDATE workflow_runs SET status = 4 WHERE run_id = ?1 AND status = 0",
                [run.run_id.as_slice()],
            )? != 1
            {
                return Err(Error::Command("workflow changed during pause"));
            }
            Ok(WorkflowOutcome::Applied {
                run_id: run.run_id,
                status: WorkflowStatus::Paused,
                event_sequence: run.event_sequence,
            })
        }
        WorkflowControlAction::Resume => {
            if run.status != WorkflowStatus::Paused {
                return Ok(WorkflowOutcome::NotRunning);
            }
            if transaction.execute(
                "UPDATE workflow_runs SET status = 0 WHERE run_id = ?1 AND status = 4",
                [run.run_id.as_slice()],
            )? != 1
            {
                return Err(Error::Command("workflow changed during resume"));
            }
            Ok(WorkflowOutcome::Applied {
                run_id: run.run_id,
                status: WorkflowStatus::Running,
                event_sequence: run.event_sequence,
            })
        }
        WorkflowControlAction::Restart { request_id, event } => {
            if matches!(run.status, WorkflowStatus::Running | WorkflowStatus::Paused) {
                return Ok(WorkflowOutcome::Busy);
            }
            if run_id(source.namespace(), *request_id) == run.run_id {
                return Ok(WorkflowOutcome::IdentityConflict);
            }
            validate_event(event)?;
            delete_workflow_run(transaction, run.run_id)?;
            workflow_start(
                transaction,
                source,
                now_ms,
                &WorkflowStart {
                    workflow_id: request.workflow_id.clone(),
                    request_id: *request_id,
                    event: event.clone(),
                },
                definition,
            )
        }
    }
}

fn delete_workflow_run(transaction: &Transaction<'_>, run_id: [u8; 16]) -> Result<()> {
    transaction.execute(
        "DELETE FROM workflow_activities WHERE run_id = ?1",
        [run_id.as_slice()],
    )?;
    transaction.execute(
        "DELETE FROM workflow_timers WHERE run_id = ?1",
        [run_id.as_slice()],
    )?;
    transaction.execute(
        "DELETE FROM workflow_events WHERE run_id = ?1",
        [run_id.as_slice()],
    )?;
    if transaction.execute(
        "DELETE FROM workflow_runs WHERE run_id = ?1 AND status BETWEEN 1 AND 3",
        [run_id.as_slice()],
    )? != 1
    {
        return Err(Error::Command("workflow changed during restart"));
    }
    Ok(())
}

fn exact_array<const N: usize>(value: Vec<u8>, message: &'static str) -> Result<[u8; N]> {
    value.try_into().map_err(|_| Error::Command(message))
}

/// Reads one bounded current workflow state without exposing primitive tables.
pub fn workflow_state(connection: &Connection, workflow_id: &[u8]) -> Result<Option<WorkflowRun>> {
    validate_identifier(workflow_id)?;
    let row = connection
        .query_row(
            "SELECT run_id, definition_digest, status, state, event_sequence, result FROM workflow_runs WHERE workflow_id = ?1",
            [workflow_id],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((run_id, definition_digest, status, state, event_sequence, result)) = row else {
        return Ok(None);
    };
    let run_id = run_id
        .try_into()
        .map_err(|_| Error::Command("invalid stored workflow run ID"))?;
    let definition_digest = Digest::from_bytes(
        definition_digest
            .try_into()
            .map_err(|_| Error::Command("invalid stored workflow definition digest"))?,
    );
    let status = WorkflowStatus::decode(status)?;
    let event_sequence = u64::try_from(event_sequence)
        .map_err(|_| Error::Command("invalid stored workflow event sequence"))?;
    let result_bytes = result.as_ref().map_or(0, Vec::len);
    if state.len() > MAX_WORKFLOW_BYTES
        || result_bytes > MAX_WORKFLOW_BYTES
        || state.len().saturating_add(result_bytes) > MAX_WORKFLOW_BYTES
    {
        return Err(Error::Command("workflow state result exceeds 1 MiB"));
    }
    Ok(Some(WorkflowRun {
        workflow_id: workflow_id.to_vec(),
        run_id,
        definition_digest,
        status,
        state,
        event_sequence,
        result,
    }))
}

#[derive(Clone)]
pub(super) struct StoredRun {
    pub(super) run_id: [u8; 16],
    pub(super) definition_digest: [u8; 32],
    pub(super) status: WorkflowStatus,
    pub(super) state: Vec<u8>,
    pub(super) event_sequence: u64,
}

fn load_run(transaction: &Transaction<'_>, workflow_id: &[u8]) -> Result<Option<StoredRun>> {
    transaction
        .query_row(
            "SELECT run_id, definition_digest, status, state, event_sequence FROM workflow_runs WHERE workflow_id = ?1",
            [workflow_id],
            decode_run,
        )
        .optional()
        .map_err(Into::into)
}

pub(super) fn workflow_definition_digest(
    transaction: &Transaction<'_>,
    workflow_id: &[u8],
) -> Result<Option<Digest>> {
    Ok(load_run(transaction, workflow_id)?.map(|run| Digest::from_bytes(run.definition_digest)))
}

pub(super) fn workflow_definition_digest_by_run(
    transaction: &Transaction<'_>,
    run_id: [u8; 16],
) -> Result<Option<Digest>> {
    Ok(load_run_by_id(transaction, run_id)?.map(|run| Digest::from_bytes(run.definition_digest)))
}

pub(super) fn load_run_by_id(
    transaction: &Transaction<'_>,
    run_id: [u8; 16],
) -> Result<Option<StoredRun>> {
    transaction
        .query_row(
            "SELECT run_id, definition_digest, status, state, event_sequence FROM workflow_runs WHERE run_id = ?1",
            [run_id.as_slice()],
            decode_run,
        )
        .optional()
        .map_err(Into::into)
}

fn decode_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRun> {
    decode_run_offset(row, 0)
}

fn decode_run_offset(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<StoredRun> {
    let run_id = row.get::<_, Vec<u8>>(offset)?;
    let definition_digest = row.get::<_, Vec<u8>>(offset + 1)?;
    let status = row.get::<_, i64>(offset + 2)?;
    let state = row.get::<_, Vec<u8>>(offset + 3)?;
    let event_sequence = row.get::<_, i64>(offset + 4)?;
    let run_id = run_id
        .try_into()
        .map_err(|_| invalid_data("workflow run ID"))?;
    let definition_digest = definition_digest
        .try_into()
        .map_err(|_| invalid_data("workflow definition digest"))?;
    let status = WorkflowStatus::decode(status).map_err(|_| invalid_data("workflow status"))?;
    if state.len() > MAX_WORKFLOW_BYTES || event_sequence < 0 {
        return Err(invalid_data("workflow run"));
    }
    Ok(StoredRun {
        run_id,
        definition_digest,
        status,
        state,
        event_sequence: event_sequence as u64,
    })
}

fn invalid_data(field: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Blob,
        format!("invalid stored {field}").into(),
    )
}

pub(super) fn verify_definition(
    run: &StoredRun,
    definition: &dyn WorkflowDefinition,
) -> Result<()> {
    if run.definition_digest != *definition.digest().as_bytes() {
        return Err(Error::Command("workflow definition digest is unavailable"));
    }
    Ok(())
}

pub(super) fn prepare_transition(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    run: &StoredRun,
    definition: &dyn WorkflowDefinition,
    event: &[u8],
    now_ms: i64,
) -> Result<(u64, WorkflowDecision)> {
    verify_definition(run, definition)?;
    let sequence = next_sequence(transaction, run.event_sequence)?;
    let context = WorkflowContext {
        source: source.clone(),
        run_id: run.run_id,
        event_sequence: sequence,
        now_ms,
    };
    let decision = definition.transition(&run.state, event, context)?;
    validate_decision(&decision, source, definition.effect_targets(), now_ms)?;
    Ok((sequence, decision))
}

pub(super) fn commit_transition(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    run_id: [u8; 16],
    sequence: u64,
    id: [u8; 32],
    event: &[u8],
    now_ms: i64,
    decision: WorkflowDecision,
) -> Result<WorkflowOutcome> {
    insert_event(transaction, run_id, sequence, id, event)?;
    apply_decision(transaction, effects, run_id, sequence, now_ms, decision)
}

pub(super) fn validate_decision(
    decision: &WorkflowDecision,
    source: &CellTarget,
    effect_targets: &[NamespaceId],
    now_ms: i64,
) -> Result<()> {
    if decision.status == WorkflowStatus::Paused {
        return Err(Error::Command(
            "workflow definitions cannot return operator-only paused status",
        ));
    }
    if decision.state.len() > MAX_WORKFLOW_BYTES
        || decision
            .result
            .as_ref()
            .is_some_and(|value| value.len() > MAX_WORKFLOW_BYTES)
        || decision.actions.len() > MAX_ACTIONS
    {
        return Err(Error::Command("workflow decision exceeds limits"));
    }
    if decision.status == WorkflowStatus::Running && decision.result.is_some() {
        return Err(Error::Command("running workflow cannot have a result"));
    }
    if decision.status != WorkflowStatus::Running
        && decision
            .actions
            .iter()
            .any(|action| !matches!(action, WorkflowAction::Effect { .. }))
    {
        return Err(Error::Command(
            "terminal workflow cannot schedule local work",
        ));
    }
    let mut bytes = 0_usize;
    for action in &decision.actions {
        match action {
            WorkflowAction::Activity {
                activity_type,
                input,
                due_at_ms,
                expires_at_ms,
            } => {
                if activity_type.is_empty()
                    || activity_type.len() > 256
                    || input.len() > MAX_ACTIVITY_PAYLOAD_BYTES
                    || *due_at_ms < now_ms
                    || *expires_at_ms <= now_ms
                    || *expires_at_ms < *due_at_ms
                    || *expires_at_ms > now_ms.saturating_add(MAX_ACTIVITY_LIFETIME_MS)
                {
                    return Err(Error::Command("invalid workflow activity action"));
                }
                bytes = bytes
                    .checked_add(activity_type.len())
                    .and_then(|value| value.checked_add(input.len()))
                    .and_then(|value| value.checked_add(32))
                    .ok_or(Error::Command("workflow action byte count overflow"))?;
            }
            WorkflowAction::Timer { due_at_ms } => {
                if *due_at_ms < now_ms {
                    return Err(Error::Command("workflow timer is in the past"));
                }
                bytes = bytes
                    .checked_add(24)
                    .ok_or(Error::Command("workflow action byte count overflow"))?;
            }
            WorkflowAction::Effect { intent } => {
                if intent.target.tenant() != source.tenant()
                    || intent.target.application() != source.application()
                    || !effect_targets.contains(&intent.target.namespace())
                {
                    return Err(Error::Command("workflow effect target is not declared"));
                }
                validate_effect_command_intent(now_ms, intent)?;
                bytes = bytes
                    .checked_add(intent.input.len())
                    .and_then(|value| value.checked_add(intent.target.partition().len()))
                    .and_then(|value| value.checked_add(64))
                    .ok_or(Error::Command("workflow action byte count overflow"))?;
            }
        }
    }
    if bytes > MAX_WORKFLOW_BYTES {
        return Err(Error::Command("workflow actions exceed 1 MiB"));
    }
    Ok(())
}

fn apply_decision(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    run_id: [u8; 16],
    sequence: u64,
    now_ms: i64,
    decision: WorkflowDecision,
) -> Result<WorkflowOutcome> {
    let existing: i64 = transaction.query_row(
        "SELECT (SELECT count(*) FROM workflow_activities WHERE run_id = ?1 AND state IN (0, 1)) + (SELECT count(*) FROM workflow_timers WHERE run_id = ?1 AND state = 0)",
        [run_id.as_slice()],
        |row| row.get(0),
    )?;
    let local_actions = decision
        .actions
        .iter()
        .filter(|action| !matches!(action, WorkflowAction::Effect { .. }))
        .count();
    let prospective = usize::try_from(existing)
        .ok()
        .and_then(|value| value.checked_add(local_actions))
        .ok_or(Error::Command("invalid outstanding workflow task count"))?;
    if prospective > MAX_ACTIONS {
        return Err(Error::Command(
            "workflow has more than 128 outstanding tasks",
        ));
    }

    for (ordinal, action) in decision.actions.iter().enumerate() {
        let ordinal = u32::try_from(ordinal)
            .map_err(|_| Error::Command("workflow action ordinal overflow"))?;
        let id = action_id(run_id, sequence, ordinal);
        match action {
            WorkflowAction::Activity {
                activity_type,
                input,
                due_at_ms,
                expires_at_ms,
            } => {
                transaction.execute(
                    "INSERT INTO workflow_activities(run_id, activity_id, activity_type, input, state, attempt, due_at_ms, expires_at_ms, token, lease_until_ms, completion_token, completion_digest, result) VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, ?6, NULL, NULL, NULL, NULL, NULL)",
                    (
                        run_id.as_slice(),
                        id.as_slice(),
                        activity_type,
                        input.as_slice(),
                        due_at_ms,
                        expires_at_ms,
                    ),
                )?;
            }
            WorkflowAction::Timer { due_at_ms } => {
                transaction.execute(
                    "INSERT INTO workflow_timers(run_id, timer_id, due_at_ms, state) VALUES (?1, ?2, ?3, 0)",
                    (run_id.as_slice(), id.as_slice(), due_at_ms),
                )?;
            }
            WorkflowAction::Effect { intent } => {
                effects.insert_command(transaction, intent)?;
            }
        }
    }

    let completed_at_ms = (decision.status != WorkflowStatus::Running).then_some(now_ms);
    if transaction.execute(
        "UPDATE workflow_runs SET status = ?1, state = ?2, event_sequence = ?3, result = ?4, completed_at_ms = ?5 WHERE run_id = ?6 AND status = 0",
        (
            decision.status.encode(),
            decision.state.as_slice(),
            sequence as i64,
            decision.result.as_deref(),
            completed_at_ms,
            run_id.as_slice(),
        ),
    )? != 1
    {
        return Err(Error::Command("workflow run changed during decision"));
    }
    if decision.status != WorkflowStatus::Running {
        cancel_outstanding(transaction, run_id)?;
    }
    Ok(WorkflowOutcome::Applied {
        run_id,
        status: decision.status,
        event_sequence: sequence,
    })
}

fn cancel_outstanding(transaction: &Transaction<'_>, run_id: [u8; 16]) -> Result<()> {
    transaction.execute(
        "UPDATE workflow_activities SET state = 4, token = NULL, lease_until_ms = NULL WHERE run_id = ?1 AND state IN (0, 1)",
        [run_id.as_slice()],
    )?;
    transaction.execute(
        "UPDATE workflow_timers SET state = 2 WHERE run_id = ?1 AND state = 0",
        [run_id.as_slice()],
    )?;
    Ok(())
}

fn next_sequence(transaction: &Transaction<'_>, current: u64) -> Result<u64> {
    let total: i64 = transaction.query_row(
        "SELECT event_count FROM workflow_control WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if total < 0 || total as u64 >= MAX_EVENTS_PER_CELL {
        return Err(Error::Capacity(
            "workflow event history reached 100,000 rows",
        ));
    }
    current
        .checked_add(1)
        .filter(|sequence| *sequence <= i64::MAX as u64)
        .ok_or(Error::Command("workflow event sequence overflow"))
}

/// Verifies Workflow's transactionally maintained event counter without repairing it.
pub fn verify_workflow_event_count(connection: &Connection) -> Result<()> {
    let stored: i64 = connection.query_row(
        "SELECT event_count FROM workflow_control WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let computed: i64 =
        connection.query_row("SELECT count(*) FROM workflow_events", [], |row| row.get(0))?;
    if stored != computed {
        return Err(Error::Command(
            "workflow event counter does not match events",
        ));
    }
    Ok(())
}

fn insert_event(
    transaction: &Transaction<'_>,
    run_id: [u8; 16],
    sequence: u64,
    id: [u8; 32],
    event: &[u8],
) -> Result<()> {
    transaction.execute(
        "INSERT INTO workflow_events(run_id, sequence, event_id, event_digest, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            run_id.as_slice(),
            sequence as i64,
            id.as_slice(),
            event_digest(event).as_slice(),
            event,
        ),
    )?;
    Ok(())
}

fn stored_event_digest(
    transaction: &Transaction<'_>,
    run_id: [u8; 16],
    id: [u8; 32],
) -> Result<Option<[u8; 32]>> {
    let digest = transaction
        .query_row(
            "SELECT event_digest FROM workflow_events WHERE run_id = ?1 AND event_id = ?2",
            (run_id.as_slice(), id.as_slice()),
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    digest
        .map(|value| {
            value
                .try_into()
                .map_err(|_| Error::Command("invalid stored workflow event digest"))
        })
        .transpose()
}

fn run_id(namespace: NamespaceId, request_id: RequestId) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.workflow-run.v1\0");
    hasher.update(namespace.as_bytes());
    hasher.update(request_id.as_bytes());
    let digest = hasher.finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    id
}

fn action_id(run_id: [u8; 16], sequence: u64, ordinal: u32) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.action.v1\0");
    hasher.update(&run_id);
    hasher.update(&sequence.to_be_bytes());
    hasher.update(&ordinal.to_be_bytes());
    let digest = hasher.finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    id
}

fn event_id(run_id: [u8; 16], source_id: &[u8; 16]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&run_id);
    hasher.update(source_id);
    *hasher.finalize().as_bytes()
}

fn timer_event_id(run_id: [u8; 16], timer_id: [u8; 16]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&run_id);
    hasher.update(&timer_id);
    hasher.update(b"fired");
    *hasher.finalize().as_bytes()
}

fn event_digest(event: &[u8]) -> [u8; 32] {
    *blake3::hash(event).as_bytes()
}

fn validate_identifier(identifier: &[u8]) -> Result<()> {
    if identifier.is_empty() || identifier.len() > 1024 {
        return Err(Error::Command("workflow ID must be in 1..=1024 bytes"));
    }
    Ok(())
}

fn validate_event(event: &[u8]) -> Result<()> {
    if event.len() > MAX_WORKFLOW_BYTES {
        return Err(Error::Command("workflow event exceeds 1 MiB"));
    }
    Ok(())
}

fn validate_now(now_ms: i64) -> Result<()> {
    if now_ms < 0 {
        return Err(Error::Command("workflow time must be non-negative"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
