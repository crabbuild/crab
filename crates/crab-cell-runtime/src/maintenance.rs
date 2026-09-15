use std::marker::PhantomData;

use crate::{
    BoundedDecoder, BoundedEncoder, CodecError, Command, CommandContext, CommandResult, Error,
    RegistryBuilder, SchedulerTickOutcome, WireValue, WorkflowDefinition,
    scheduler::scheduler_tick_at,
};

/// Compile-time binding for the internal maintenance command of one module.
pub trait MaintenanceModule: Send + Sync + 'static {
    const MODULE: &'static str;
    const CODEC_VERSION: u32 = 1;
    const TICK_COMMAND_ID: u32;
    const WORKFLOW_DEFINITIONS: &'static [&'static dyn WorkflowDefinition] = &[];
}

/// Registers one module's internal scheduler Tick command.
pub fn register_maintenance<M: MaintenanceModule>(
    registry: &mut RegistryBuilder,
) -> crate::Result<()> {
    registry.bind_command::<MaintenanceTickCommand<M>>()
}

/// Published root position from which a due-Cell scan scheduled this Tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceTickRequest {
    pub expected_commit_sequence: u64,
}

/// Durable result of one scheduled Tick attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceTickOutcome {
    Applied { processed: u32 },
    Stale,
}

/// Typed system command that advances bounded maintenance through the actor.
pub struct MaintenanceTickCommand<M>(PhantomData<fn() -> M>);

impl<M: MaintenanceModule> Command for MaintenanceTickCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::TICK_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = MaintenanceTickRequest;
    type Output = MaintenanceTickOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let expected_next = input
            .expected_commit_sequence
            .checked_add(1)
            .ok_or(Error::Command("maintenance expected sequence overflow"))?;
        if expected_next != context.sequence() {
            return Ok(CommandResult::Success(MaintenanceTickOutcome::Stale));
        }
        let SchedulerTickOutcome { processed } = scheduler_tick_at(
            context.primitive_transaction(),
            context.sequence(),
            context.now_ms(),
            M::WORKFLOW_DEFINITIONS,
        )?;
        Ok(CommandResult::Success(MaintenanceTickOutcome::Applied {
            processed,
        }))
    }
}

impl WireValue for MaintenanceTickRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.expected_commit_sequence)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            expected_commit_sequence: decoder.read_u64()?,
        })
    }
}

impl WireValue for MaintenanceTickOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Applied { processed } if *processed <= 128 => {
                encoder.write_u8(0)?;
                encoder.write_u32(*processed)
            }
            Self::Applied { .. } => {
                Err(CodecError::Invalid("maintenance result exceeds 128 items"))
            }
            Self::Stale => encoder.write_u8(1),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => {
                let processed = decoder.read_u32()?;
                if processed > 128 {
                    return Err(CodecError::Invalid("maintenance result exceeds 128 items"));
                }
                Ok(Self::Applied { processed })
            }
            1 => Ok(Self::Stale),
            _ => Err(CodecError::Invalid("invalid maintenance result tag")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_codecs_reject_unbounded_results() {
        let mut encoder = BoundedEncoder::new(16).unwrap();
        assert!(
            MaintenanceTickOutcome::Applied { processed: 129 }
                .encode(&mut encoder)
                .is_err()
        );
    }
}
