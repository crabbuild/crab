//! Workflow wire codecs for start, signal, control, outcome, and run payloads.

use super::*;

const APPLIED_TAG: u8 = 0;
const DUPLICATE_TAG: u8 = 1;
const ALREADY_EXISTS_TAG: u8 = 2;
const IDENTITY_CONFLICT_TAG: u8 = 3;
const RUN_MISMATCH_TAG: u8 = 4;
const NOT_RUNNING_TAG: u8 = 5;
const NOT_DUE_TAG: u8 = 6;
const BUSY_TAG: u8 = 7;
const CONTROL_PAUSE_TAG: u8 = 0;
const CONTROL_RESUME_TAG: u8 = 1;
const CONTROL_RESTART_TAG: u8 = 2;

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
            Self::Busy => encoder.write_u8(BUSY_TAG),
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
            BUSY_TAG => Ok(Self::Busy),
            _ => Err(CodecError::Invalid("invalid workflow outcome tag")),
        }
    }
}

impl WireValue for WorkflowControl {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.workflow_id)?;
        encoder.write_bytes(&self.run_id)?;
        match &self.action {
            WorkflowControlAction::Pause => encoder.write_u8(CONTROL_PAUSE_TAG),
            WorkflowControlAction::Resume => encoder.write_u8(CONTROL_RESUME_TAG),
            WorkflowControlAction::Restart { request_id, event } => {
                encoder.write_u8(CONTROL_RESTART_TAG)?;
                encoder.write_bytes(request_id.as_bytes())?;
                encoder.write_bytes(event)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let workflow_id = decoder.read_bytes()?.to_vec();
        let run_id = read_fixed(decoder, "workflow run ID length")?;
        let action = match decoder.read_u8()? {
            CONTROL_PAUSE_TAG => WorkflowControlAction::Pause,
            CONTROL_RESUME_TAG => WorkflowControlAction::Resume,
            CONTROL_RESTART_TAG => WorkflowControlAction::Restart {
                request_id: RequestId::from_bytes(read_fixed(
                    decoder,
                    "workflow restart request ID length",
                )?),
                event: decoder.read_bytes()?.to_vec(),
            },
            _ => return Err(CodecError::Invalid("invalid workflow control action tag")),
        };
        Ok(Self {
            workflow_id,
            run_id,
            action,
        })
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
        WorkflowStatus::Paused => 4,
    })
}

fn decode_status(decoder: &mut BoundedDecoder<'_>) -> Result<WorkflowStatus, CodecError> {
    match decoder.read_u8()? {
        0 => Ok(WorkflowStatus::Running),
        1 => Ok(WorkflowStatus::Completed),
        2 => Ok(WorkflowStatus::Failed),
        3 => Ok(WorkflowStatus::Cancelled),
        4 => Ok(WorkflowStatus::Paused),
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
        || (matches!(run.status, WorkflowStatus::Running | WorkflowStatus::Paused)
            && run.result.is_some())
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
            _context: crate::primitives::workflow::WorkflowContext,
        ) -> crate::Result<crate::primitives::workflow::WorkflowDecision> {
            let mut state = self.prefix.to_vec();
            state.extend_from_slice(event);
            Ok(crate::primitives::workflow::WorkflowDecision {
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
        const CONTROL_COMMAND_ID: u32 = 4;
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
            WorkflowOutcome::Busy,
        ] {
            roundtrip(outcome);
        }
        roundtrip(WorkflowGetRequest {
            workflow_id: b"build-42".to_vec(),
        });
        roundtrip(WorkflowControl {
            workflow_id: b"build-42".to_vec(),
            run_id: [2; 16],
            action: WorkflowControlAction::Restart {
                request_id: RequestId::from_bytes([8; 16]),
                event: b"again".to_vec(),
            },
        });
        roundtrip(WorkflowOutcome::Applied {
            run_id: [2; 16],
            status: WorkflowStatus::Paused,
            event_sequence: 1,
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
        let source = CellTarget::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
            RolloverWorkflow::NAMESPACE,
            b"rollover",
        )
        .unwrap();
        crate::cell::schema::install_runtime_schema_in(
            &transaction,
            source.cell_id(),
            crate::identity::IncarnationId::from_bytes([2; 16]),
            1,
        )
        .unwrap();
        crate::primitives::workflow::install_workflow_schema(&transaction).unwrap();
        let request = WorkflowStart {
            workflow_id: b"old-run".to_vec(),
            request_id: RequestId::from_bytes([14; 16]),
            event: b"start".to_vec(),
        };
        let started =
            super::super::workflow_start(&transaction, &source, 10, &request, &OLD_DEFINITION)
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
            &source,
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
