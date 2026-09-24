use std::marker::PhantomData;

use crate::cell::catalog::CatalogRole;
use crate::client::{CellClient, Committed, InvocationError, Observed, Receipt};
use crate::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};
use crate::identity::RequestId;
use crate::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, TenantId, partition_for_shard, shard_for_scope,
};
use crate::registry::{Command, Query, RegistryBuilder};
use crate::registry::{CommandContext, CommandResult, QueryContext};

mod codec;

use super::{
    MAX_WORKFLOW_BYTES, WorkflowControl, WorkflowControlAction, WorkflowDefinition,
    WorkflowOutcome, WorkflowRun, WorkflowSignal, WorkflowStart, WorkflowStatus, workflow_cancel,
    workflow_control, workflow_signal, workflow_start, workflow_state,
};

/// Compile-time namespace, definition and operation IDs for one Workflow module.
pub trait WorkflowModule: Send + Sync + 'static {
    const MODULE: &'static str;
    const NAMESPACE: NamespaceId;
    const CURRENT_DEFINITION: &'static dyn WorkflowDefinition;
    const DEFINITIONS: &'static [&'static dyn WorkflowDefinition];
    const CODEC_VERSION: u32 = 1;
    const START_COMMAND_ID: u32;
    const SIGNAL_COMMAND_ID: u32;
    const CANCEL_COMMAND_ID: u32;
    const CONTROL_COMMAND_ID: u32;
    const GET_QUERY_ID: u32;
}

/// Registers every executable definition and the module's typed Workflow bindings.
pub fn register_workflow<M: WorkflowModule>(registry: &mut RegistryBuilder) -> crate::Result<()> {
    for definition in definitions::<M>()? {
        registry.bind_workflow_definition(M::MODULE, *definition)?;
    }
    registry.bind_command::<WorkflowStartCommand<M>>()?;
    registry.bind_command::<WorkflowSignalCommand<M>>()?;
    registry.bind_command::<WorkflowCancelCommand<M>>()?;
    registry.bind_command::<WorkflowControlCommand<M>>()?;
    registry.bind_query::<WorkflowGetQuery<M>>()
}

/// Typed Workflow operator control bound to immutable module operation IDs.
pub struct WorkflowControlCommand<M>(PhantomData<fn() -> M>);

impl<M: WorkflowModule> Command for WorkflowControlCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::CONTROL_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = WorkflowControl;
    type Output = WorkflowOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let source = context.target().clone();
        classify(workflow_control(
            context.primitive_transaction(),
            &source,
            context.now_ms(),
            &input,
            M::CURRENT_DEFINITION,
        )?)
    }
}

/// Typed Workflow start bound to immutable module operation IDs.
pub struct WorkflowStartCommand<M>(PhantomData<fn() -> M>);

impl<M: WorkflowModule> Command for WorkflowStartCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::START_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = WorkflowStart;
    type Output = WorkflowOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let source = context.target().clone();
        classify(workflow_start(
            context.primitive_transaction(),
            &source,
            context.now_ms(),
            &input,
            M::CURRENT_DEFINITION,
        )?)
    }
}

/// Typed Workflow signal bound to immutable module operation IDs.
pub struct WorkflowSignalCommand<M>(PhantomData<fn() -> M>);

impl<M: WorkflowModule> Command for WorkflowSignalCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::SIGNAL_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = WorkflowSignal;
    type Output = WorkflowOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let source = context.target().clone();
        let Some(definition) =
            definition_for_workflow::<M>(context.primitive_transaction(), &input.workflow_id)?
        else {
            return classify(WorkflowOutcome::RunMismatch);
        };
        classify(workflow_signal(
            context.primitive_transaction(),
            &source,
            context.now_ms(),
            &input,
            definition,
        )?)
    }
}

pub(super) fn definitions<M: WorkflowModule>()
-> crate::Result<&'static [&'static dyn WorkflowDefinition]> {
    if M::DEFINITIONS.is_empty()
        || !M::DEFINITIONS
            .iter()
            .any(|definition| definition.digest() == M::CURRENT_DEFINITION.digest())
    {
        return Err(crate::Error::Registry(
            "current workflow definition is absent from inventory",
        ));
    }
    Ok(M::DEFINITIONS)
}

pub(super) fn definition<M: WorkflowModule>(
    digest: Digest,
) -> crate::Result<&'static dyn WorkflowDefinition> {
    definitions::<M>()?
        .iter()
        .copied()
        .find(|definition| definition.digest() == digest)
        .ok_or(crate::Error::Command(
            "workflow definition digest is unavailable",
        ))
}

fn definition_for_workflow<M: WorkflowModule>(
    transaction: &crab_ltx::rusqlite::Transaction<'_>,
    workflow_id: &[u8],
) -> crate::Result<Option<&'static dyn WorkflowDefinition>> {
    super::workflow_definition_digest(transaction, workflow_id)?
        .map(definition::<M>)
        .transpose()
}

/// Typed Workflow cancellation bound to immutable module operation IDs.
pub struct WorkflowCancelCommand<M>(PhantomData<fn() -> M>);

impl<M: WorkflowModule> Command for WorkflowCancelCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::CANCEL_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = WorkflowSignal;
    type Output = WorkflowOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        classify(workflow_cancel(
            context.primitive_transaction(),
            context.now_ms(),
            &input,
        )?)
    }
}

/// Bounded current-state query for one workflow identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowGetRequest {
    pub workflow_id: Vec<u8>,
}

/// Typed Workflow state query bound to immutable module operation IDs.
pub struct WorkflowGetQuery<M>(PhantomData<fn() -> M>);

impl<M: WorkflowModule> Query for WorkflowGetQuery<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::GET_QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = WorkflowGetRequest;
    type Output = Option<WorkflowRun>;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        workflow_state(context.primitive_connection(), &input.workflow_id)
    }
}

/// Authorized Workflow capability with deterministic workflow-ID sharding.
pub struct WorkflowNamespace<M> {
    client: CellClient,
    tenant: TenantId,
    application: ApplicationId,
    shards: u32,
    module: PhantomData<fn() -> M>,
}

impl<M> Clone for WorkflowNamespace<M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            tenant: self.tenant,
            application: self.application,
            shards: self.shards,
            module: PhantomData,
        }
    }
}

impl<M: WorkflowModule> WorkflowNamespace<M> {
    /// Creates a Workflow capability from its compiled namespace topology.
    pub fn new(
        client: CellClient,
        tenant: TenantId,
        application: ApplicationId,
    ) -> crate::Result<Self> {
        let shards = client.require_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Workflow)?;
        Ok(Self {
            client,
            tenant,
            application,
            shards,
            module: PhantomData,
        })
    }

    /// Starts one workflow with the runtime mutation identity as its run identity.
    pub async fn start(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        workflow_id: Vec<u8>,
        event: Vec<u8>,
    ) -> std::result::Result<Committed<WorkflowOutcome>, InvocationError<WorkflowOutcome>> {
        let target = self
            .target(&workflow_id)
            .map_err(InvocationError::NotStarted)?;
        let input = WorkflowStart {
            workflow_id,
            request_id: identity.request_id,
            event,
        };
        self.client
            .command::<WorkflowStartCommand<M>>(&target, identity, input)
            .await
    }

    /// Delivers one idempotent external signal to its workflow-ID shard.
    pub async fn signal(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        signal: WorkflowSignal,
    ) -> std::result::Result<Committed<WorkflowOutcome>, InvocationError<WorkflowOutcome>> {
        let target = self
            .target(&signal.workflow_id)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<WorkflowSignalCommand<M>>(&target, identity, signal)
            .await
    }

    /// Cancels one running workflow and its outstanding local work.
    pub async fn cancel(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        signal: WorkflowSignal,
    ) -> std::result::Result<Committed<WorkflowOutcome>, InvocationError<WorkflowOutcome>> {
        let target = self
            .target(&signal.workflow_id)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<WorkflowCancelCommand<M>>(&target, identity, signal)
            .await
    }

    /// Pauses a quiescent run so timers and new activity claims stop.
    pub async fn pause(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        workflow_id: Vec<u8>,
        run_id: [u8; 16],
    ) -> std::result::Result<Committed<WorkflowOutcome>, InvocationError<WorkflowOutcome>> {
        self.control(identity, workflow_id, run_id, WorkflowControlAction::Pause)
            .await
    }

    /// Resumes a paused run without changing its deterministic history.
    pub async fn resume(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        workflow_id: Vec<u8>,
        run_id: [u8; 16],
    ) -> std::result::Result<Committed<WorkflowOutcome>, InvocationError<WorkflowOutcome>> {
        self.control(identity, workflow_id, run_id, WorkflowControlAction::Resume)
            .await
    }

    /// Replaces a terminal run with a new run of the current definition.
    pub async fn restart(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        workflow_id: Vec<u8>,
        run_id: [u8; 16],
        event: Vec<u8>,
    ) -> std::result::Result<Committed<WorkflowOutcome>, InvocationError<WorkflowOutcome>> {
        let request_id = identity.request_id;
        self.control(
            identity,
            workflow_id,
            run_id,
            WorkflowControlAction::Restart { request_id, event },
        )
        .await
    }

    async fn control(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        workflow_id: Vec<u8>,
        run_id: [u8; 16],
        action: WorkflowControlAction,
    ) -> std::result::Result<Committed<WorkflowOutcome>, InvocationError<WorkflowOutcome>> {
        let target = self
            .target(&workflow_id)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<WorkflowControlCommand<M>>(
                &target,
                identity,
                WorkflowControl {
                    workflow_id,
                    run_id,
                    action,
                },
            )
            .await
    }

    /// Reads one workflow at an optional minimum publication receipt.
    pub async fn state(
        &self,
        workflow_id: Vec<u8>,
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<Option<WorkflowRun>>, InvocationError<Option<WorkflowRun>>>
    {
        let target = self
            .target(&workflow_id)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .query::<WorkflowGetQuery<M>>(&target, minimum, WorkflowGetRequest { workflow_id })
            .await
    }

    fn target(&self, workflow_id: &[u8]) -> crate::Result<CellTarget> {
        let shard = shard_for_scope(M::NAMESPACE, workflow_id, self.shards)?;
        CellTarget::new(
            self.tenant,
            self.application,
            M::NAMESPACE,
            &partition_for_shard(shard),
        )
    }
}

fn classify(outcome: WorkflowOutcome) -> crate::Result<CommandResult<WorkflowOutcome>> {
    Ok(match outcome {
        WorkflowOutcome::Applied { .. } | WorkflowOutcome::Duplicate { .. } => {
            CommandResult::Success(outcome)
        }
        WorkflowOutcome::AlreadyExists
        | WorkflowOutcome::IdentityConflict
        | WorkflowOutcome::RunMismatch
        | WorkflowOutcome::NotRunning
        | WorkflowOutcome::NotDue
        | WorkflowOutcome::Busy => CommandResult::Rejected(outcome),
    })
}
