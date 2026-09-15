use std::marker::PhantomData;

use crate::{
    ApplicationId, BoundedDecoder, BoundedEncoder, CatalogRole, CellClient, CellTarget, CodecError,
    Command, CommandContext, CommandResult, Committed, Digest, InvocationError, NamespaceId,
    Observed, Query, QueryContext, Receipt, RegistryBuilder, RequestId, TenantId, WireValue,
    partition_for_shard, shard_for_scope,
};

use super::{
    MAX_WORKFLOW_BYTES, WorkflowDefinition, WorkflowOutcome, WorkflowRun, WorkflowSignal,
    WorkflowStart, WorkflowStatus, workflow_cancel, workflow_signal, workflow_start,
    workflow_state,
};

const APPLIED_TAG: u8 = 0;
const DUPLICATE_TAG: u8 = 1;
const ALREADY_EXISTS_TAG: u8 = 2;
const IDENTITY_CONFLICT_TAG: u8 = 3;
const RUN_MISMATCH_TAG: u8 = 4;
const NOT_RUNNING_TAG: u8 = 5;
const NOT_DUE_TAG: u8 = 6;

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
    registry.bind_query::<WorkflowGetQuery<M>>()
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
        classify(workflow_start(
            context.primitive_transaction(),
            M::NAMESPACE,
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
        let Some(definition) =
            definition_for_workflow::<M>(context.primitive_transaction(), &input.workflow_id)?
        else {
            return classify(WorkflowOutcome::RunMismatch);
        };
        classify(workflow_signal(
            context.primitive_transaction(),
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
        identity: crate::MutationIdentity,
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
        identity: crate::MutationIdentity,
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
        identity: crate::MutationIdentity,
        signal: WorkflowSignal,
    ) -> std::result::Result<Committed<WorkflowOutcome>, InvocationError<WorkflowOutcome>> {
        let target = self
            .target(&signal.workflow_id)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<WorkflowCancelCommand<M>>(&target, identity, signal)
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
        | WorkflowOutcome::NotDue => CommandResult::Rejected(outcome),
    })
}

impl WireValue for WorkflowStart {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.workflow_id)?;
        encoder.write_bytes(self.request_id.as_bytes())?;
        encoder.write_bytes(&self.event)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            workflow_id: decoder.read_bytes()?.to_vec(),
            request_id: RequestId::from_bytes(read_fixed(decoder, "workflow request ID length")?),
            event: decoder.read_bytes()?.to_vec(),
        })
    }
}

impl WireValue for WorkflowSignal {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.workflow_id)?;
        encoder.write_bytes(&self.run_id)?;
        encoder.write_bytes(&self.signal_id)?;
        encoder.write_bytes(&self.event)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            workflow_id: decoder.read_bytes()?.to_vec(),
            run_id: read_fixed(decoder, "workflow run ID length")?,
            signal_id: read_fixed(decoder, "workflow signal ID length")?,
            event: decoder.read_bytes()?.to_vec(),
        })
    }
}

impl WireValue for WorkflowOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Applied {
                run_id,
                status,
                event_sequence,
            } => encode_outcome(APPLIED_TAG, *run_id, *status, *event_sequence, encoder),
            Self::Duplicate {
                run_id,
                status,
                event_sequence,
            } => encode_outcome(DUPLICATE_TAG, *run_id, *status, *event_sequence, encoder),
            Self::AlreadyExists => encoder.write_u8(ALREADY_EXISTS_TAG),
            Self::IdentityConflict => encoder.write_u8(IDENTITY_CONFLICT_TAG),
            Self::RunMismatch => encoder.write_u8(RUN_MISMATCH_TAG),
            Self::NotRunning => encoder.write_u8(NOT_RUNNING_TAG),
            Self::NotDue => encoder.write_u8(NOT_DUE_TAG),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            APPLIED_TAG => {
                decode_outcome(decoder, |run_id, status, event_sequence| Self::Applied {
                    run_id,
                    status,
                    event_sequence,
                })
            }
            DUPLICATE_TAG => {
                decode_outcome(decoder, |run_id, status, event_sequence| Self::Duplicate {
                    run_id,
                    status,
                    event_sequence,
                })
            }
            ALREADY_EXISTS_TAG => Ok(Self::AlreadyExists),
            IDENTITY_CONFLICT_TAG => Ok(Self::IdentityConflict),
            RUN_MISMATCH_TAG => Ok(Self::RunMismatch),
            NOT_RUNNING_TAG => Ok(Self::NotRunning),
            NOT_DUE_TAG => Ok(Self::NotDue),
            _ => Err(CodecError::Invalid("invalid workflow outcome tag")),
        }
    }
}

impl WireValue for WorkflowGetRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.workflow_id)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            workflow_id: decoder.read_bytes()?.to_vec(),
        })
    }
}

impl WireValue for WorkflowRun {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        validate_run(self)?;
        encoder.write_bytes(&self.workflow_id)?;
        encoder.write_bytes(&self.run_id)?;
        encoder.write_bytes(self.definition_digest.as_bytes())?;
        encode_status(self.status, encoder)?;
        encoder.write_bytes(&self.state)?;
        encoder.write_u64(self.event_sequence)?;
        self.result.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let run = Self {
            workflow_id: decoder.read_bytes()?.to_vec(),
            run_id: read_fixed(decoder, "workflow run ID length")?,
            definition_digest: Digest::from_bytes(read_fixed(
                decoder,
                "workflow definition digest length",
            )?),
            status: decode_status(decoder)?,
            state: decoder.read_bytes()?.to_vec(),
            event_sequence: decoder.read_u64()?,
            result: Option::<Vec<u8>>::decode(decoder)?,
        };
        validate_run(&run)?;
        Ok(run)
    }
}

fn encode_outcome(
    tag: u8,
    run_id: [u8; 16],
    status: WorkflowStatus,
    event_sequence: u64,
    encoder: &mut BoundedEncoder,
) -> Result<(), CodecError> {
    if event_sequence == 0 {
        return Err(CodecError::Invalid("invalid workflow event sequence"));
    }
    encoder.write_u8(tag)?;
    encoder.write_bytes(&run_id)?;
    encode_status(status, encoder)?;
    encoder.write_u64(event_sequence)
}

fn decode_outcome(
    decoder: &mut BoundedDecoder<'_>,
    outcome: impl FnOnce([u8; 16], WorkflowStatus, u64) -> WorkflowOutcome,
) -> Result<WorkflowOutcome, CodecError> {
    let run_id = read_fixed(decoder, "workflow run ID length")?;
    let status = decode_status(decoder)?;
    let event_sequence = decoder.read_u64()?;
    if event_sequence == 0 {
        return Err(CodecError::Invalid("invalid workflow event sequence"));
    }
    Ok(outcome(run_id, status, event_sequence))
}

fn encode_status(status: WorkflowStatus, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
    encoder.write_u8(match status {
        WorkflowStatus::Running => 0,
        WorkflowStatus::Completed => 1,
        WorkflowStatus::Failed => 2,
        WorkflowStatus::Cancelled => 3,
    })
}

fn decode_status(decoder: &mut BoundedDecoder<'_>) -> Result<WorkflowStatus, CodecError> {
    match decoder.read_u8()? {
        0 => Ok(WorkflowStatus::Running),
        1 => Ok(WorkflowStatus::Completed),
        2 => Ok(WorkflowStatus::Failed),
        3 => Ok(WorkflowStatus::Cancelled),
        _ => Err(CodecError::Invalid("invalid workflow status tag")),
    }
}

fn validate_run(run: &WorkflowRun) -> Result<(), CodecError> {
    let result_bytes = run.result.as_ref().map_or(0, Vec::len);
    if run.workflow_id.is_empty()
        || run.workflow_id.len() > 1024
        || run
            .definition_digest
            .as_bytes()
            .iter()
            .all(|byte| *byte == 0)
        || run.event_sequence == 0
        || run.state.len() > MAX_WORKFLOW_BYTES
        || result_bytes > MAX_WORKFLOW_BYTES
        || run.state.len().saturating_add(result_bytes) > MAX_WORKFLOW_BYTES
        || (run.status == WorkflowStatus::Running && run.result.is_some())
    {
        return Err(CodecError::Invalid("invalid workflow run"));
    }
    Ok(())
}

fn read_fixed<const N: usize>(
    decoder: &mut BoundedDecoder<'_>,
    message: &'static str,
) -> Result<[u8; N], CodecError> {
    decoder
        .read_bytes()?
        .try_into()
        .map_err(|_| CodecError::Invalid(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_ltx::rusqlite::Connection;

    struct TestDefinition {
        digest: Digest,
        prefix: &'static [u8],
    }

    impl WorkflowDefinition for TestDefinition {
        fn digest(&self) -> Digest {
            self.digest
        }

        fn transition(
            &self,
            _state: &[u8],
            event: &[u8],
            _context: super::super::WorkflowContext,
        ) -> crate::Result<super::super::WorkflowDecision> {
            let mut state = self.prefix.to_vec();
            state.extend_from_slice(event);
            Ok(super::super::WorkflowDecision {
                status: WorkflowStatus::Running,
                state,
                result: None,
                actions: Vec::new(),
            })
        }
    }

    static OLD_DEFINITION: TestDefinition = TestDefinition {
        digest: Digest::from_bytes([11; 32]),
        prefix: b"old:",
    };
    static NEW_DEFINITION: TestDefinition = TestDefinition {
        digest: Digest::from_bytes([12; 32]),
        prefix: b"new:",
    };
    static TEST_DEFINITIONS: [&dyn WorkflowDefinition; 2] = [&OLD_DEFINITION, &NEW_DEFINITION];

    struct RolloverWorkflow;

    impl WorkflowModule for RolloverWorkflow {
        const MODULE: &'static str = "rollover";
        const NAMESPACE: NamespaceId = NamespaceId::from_bytes([13; 16]);
        const CURRENT_DEFINITION: &'static dyn WorkflowDefinition = &NEW_DEFINITION;
        const DEFINITIONS: &'static [&'static dyn WorkflowDefinition] = &TEST_DEFINITIONS;
        const START_COMMAND_ID: u32 = 1;
        const SIGNAL_COMMAND_ID: u32 = 2;
        const CANCEL_COMMAND_ID: u32 = 3;
        const GET_QUERY_ID: u32 = 1;
    }

    fn roundtrip<T: WireValue + PartialEq + std::fmt::Debug>(value: T) {
        let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
        value.encode(&mut encoder).unwrap();
        let bytes = encoder.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 1024 * 1024).unwrap();
        assert_eq!(T::decode(&mut decoder).unwrap(), value);
        decoder.finish().unwrap();
    }

    #[test]
    fn workflow_codecs_roundtrip_commands_outcomes_and_state() {
        roundtrip(WorkflowStart {
            workflow_id: b"build-42".to_vec(),
            request_id: RequestId::from_bytes([1; 16]),
            event: b"start".to_vec(),
        });
        roundtrip(WorkflowSignal {
            workflow_id: b"build-42".to_vec(),
            run_id: [2; 16],
            signal_id: [3; 16],
            event: b"finish".to_vec(),
        });
        roundtrip(WorkflowOutcome::Applied {
            run_id: [2; 16],
            status: WorkflowStatus::Running,
            event_sequence: 1,
        });
        for outcome in [
            WorkflowOutcome::AlreadyExists,
            WorkflowOutcome::IdentityConflict,
            WorkflowOutcome::RunMismatch,
            WorkflowOutcome::NotRunning,
            WorkflowOutcome::NotDue,
        ] {
            roundtrip(outcome);
        }
        roundtrip(WorkflowGetRequest {
            workflow_id: b"build-42".to_vec(),
        });
        roundtrip(Some(WorkflowRun {
            workflow_id: b"build-42".to_vec(),
            run_id: [2; 16],
            definition_digest: Digest::from_bytes([4; 32]),
            status: WorkflowStatus::Completed,
            state: b"done".to_vec(),
            event_sequence: 2,
            result: Some(b"ok".to_vec()),
        }));
    }

    #[test]
    fn workflow_decoder_rejects_invalid_ids_sequences_and_state_shape() {
        let mut invalid = BoundedEncoder::new(32).unwrap();
        invalid.write_u8(APPLIED_TAG).unwrap();
        invalid.write_bytes(&[1; 15]).unwrap();
        let bytes = invalid.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 32).unwrap();
        assert!(matches!(
            WorkflowOutcome::decode(&mut decoder),
            Err(CodecError::Invalid("workflow run ID length"))
        ));

        let run = WorkflowRun {
            workflow_id: b"build-42".to_vec(),
            run_id: [2; 16],
            definition_digest: Digest::from_bytes([4; 32]),
            status: WorkflowStatus::Running,
            state: Vec::new(),
            event_sequence: 1,
            result: Some(b"not allowed".to_vec()),
        };
        let mut encoder = BoundedEncoder::new(128).unwrap();
        assert!(matches!(
            run.encode(&mut encoder),
            Err(CodecError::Invalid("invalid workflow run"))
        ));
    }

    #[test]
    fn persisted_definition_digest_dispatches_to_retained_old_code() {
        let mut connection = Connection::open_in_memory().unwrap();
        let transaction = connection.transaction().unwrap();
        crate::schema::install_runtime_schema_in(
            &transaction,
            crate::CellId::from_bytes([1; 32]),
            crate::IncarnationId::from_bytes([2; 16]),
            1,
        )
        .unwrap();
        super::super::install_workflow_schema(&transaction).unwrap();
        let request = WorkflowStart {
            workflow_id: b"old-run".to_vec(),
            request_id: RequestId::from_bytes([14; 16]),
            event: b"start".to_vec(),
        };
        let started = super::super::workflow_start(
            &transaction,
            RolloverWorkflow::NAMESPACE,
            10,
            &request,
            &OLD_DEFINITION,
        )
        .unwrap();
        let WorkflowOutcome::Applied { run_id, .. } = started else {
            panic!("old workflow did not start");
        };

        let retained =
            definition_for_workflow::<RolloverWorkflow>(&transaction, &request.workflow_id)
                .unwrap()
                .unwrap();
        assert_eq!(retained.digest(), OLD_DEFINITION.digest());
        super::super::workflow_signal(
            &transaction,
            11,
            &WorkflowSignal {
                workflow_id: request.workflow_id.clone(),
                run_id,
                signal_id: [15; 16],
                event: b"continue".to_vec(),
            },
            retained,
        )
        .unwrap();
        let run = super::super::workflow_state(&transaction, &request.workflow_id)
            .unwrap()
            .unwrap();
        assert_eq!(run.state, b"old:continue");

        let current =
            definition::<RolloverWorkflow>(RolloverWorkflow::CURRENT_DEFINITION.digest()).unwrap();
        assert_eq!(current.digest(), NEW_DEFINITION.digest());
    }
}
