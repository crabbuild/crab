use std::marker::PhantomData;

use crab_ltx::rusqlite::Connection;

use crate::{
    BoundedDecoder, BoundedEncoder, CatalogRole, CodecError, Command, CommandContext,
    CommandResult, CronTarget, Error, QueueDeadLetterTarget, RegistryBuilder, SchedulerTickOutcome,
    WireValue, WorkflowDefinition, scheduler::scheduler_tick_at,
};

const REQUESTS: u8 = 1 << 0;
const INBOX: u8 = 1 << 1;
const EFFECTS: u8 = 1 << 2;
const QUEUE_MESSAGES: u8 = 1 << 3;
const QUEUE_DEDUP: u8 = 1 << 4;
const WORKFLOWS: u8 = 1 << 5;
const BLOBS: u8 = 1 << 6;
const CRON_SCHEDULES: u8 = 1 << 7;

/// Conservative inventory of rows that can retain executable release contracts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PersistedWorkInventory {
    bits: u8,
    unknown: bool,
}

impl PersistedWorkInventory {
    pub(crate) const fn unknown() -> Self {
        Self {
            bits: 0,
            unknown: true,
        }
    }

    /// Reports whether contract removal can proceed without transforming durable work.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.unknown && self.bits == 0
    }

    pub(crate) const fn is_unknown(self) -> bool {
        self.unknown
    }

    /// Names the first durable work class blocking contract removal.
    #[must_use]
    pub const fn first_blocker(self) -> Option<&'static str> {
        if self.unknown {
            Some("maintenance release is blocked by unknown persisted work")
        } else if self.bits & REQUESTS != 0 {
            Some("maintenance release is blocked by retained request outcomes")
        } else if self.bits & INBOX != 0 {
            Some("maintenance release is blocked by retained effect inbox outcomes")
        } else if self.bits & EFFECTS != 0 {
            Some("maintenance release is blocked by retained source effects")
        } else if self.bits & QUEUE_MESSAGES != 0 {
            Some("maintenance release is blocked by retained Queue messages")
        } else if self.bits & QUEUE_DEDUP != 0 {
            Some("maintenance release is blocked by retained Queue producer identities")
        } else if self.bits & WORKFLOWS != 0 {
            Some("maintenance release is blocked by retained Workflow runs")
        } else if self.bits & BLOBS != 0 {
            Some("maintenance release is blocked by retained Blob objects")
        } else if self.bits & CRON_SCHEDULES != 0 {
            Some("maintenance release is blocked by retained Cron schedules")
        } else {
            None
        }
    }

    pub(crate) fn encode(self) -> Vec<u8> {
        if self.unknown {
            Vec::new()
        } else {
            vec![self.bits]
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> crate::Result<Self> {
        match bytes {
            [bits] => Ok(Self {
                bits: *bits,
                unknown: false,
            }),
            _ => Err(Error::Command("invalid persisted-work inventory")),
        }
    }
}

pub(crate) fn inspect_persisted_work(
    connection: &Connection,
    role: CatalogRole,
) -> crate::Result<PersistedWorkInventory> {
    let mut bits = 0;
    bits |= exists(connection, "SELECT EXISTS(SELECT 1 FROM sys_requests)")? * REQUESTS;
    bits |= exists(connection, "SELECT EXISTS(SELECT 1 FROM sys_inbox)")? * INBOX;
    bits |= exists(connection, "SELECT EXISTS(SELECT 1 FROM sys_effects)")? * EFFECTS;
    if role == CatalogRole::Queue {
        bits |= exists(connection, "SELECT EXISTS(SELECT 1 FROM queue_messages)")? * QUEUE_MESSAGES;
        bits |= exists(connection, "SELECT EXISTS(SELECT 1 FROM queue_dedup)")? * QUEUE_DEDUP;
    }
    if role == CatalogRole::Workflow {
        bits |= exists(connection, "SELECT EXISTS(SELECT 1 FROM workflow_runs)")? * WORKFLOWS;
    }
    if role == CatalogRole::Blob {
        bits |= exists(connection, "SELECT EXISTS(SELECT 1 FROM blob_objects)")? * BLOBS;
    }
    if role == CatalogRole::Cron {
        bits |= exists(connection, "SELECT EXISTS(SELECT 1 FROM cron_schedules)")? * CRON_SCHEDULES;
    }
    Ok(PersistedWorkInventory {
        bits,
        unknown: false,
    })
}

fn exists(connection: &Connection, sql: &str) -> crate::Result<u8> {
    let exists = connection.query_row(sql, [], |row| row.get::<_, i64>(0))?;
    match exists {
        0 => Ok(0),
        1 => Ok(1),
        _ => Err(Error::Command("invalid persisted-work existence result")),
    }
}

/// Compile-time binding for the internal maintenance command of one module.
pub trait MaintenanceModule: Send + Sync + 'static {
    const MODULE: &'static str;
    const CODEC_VERSION: u32 = 1;
    const TICK_COMMAND_ID: u32;
    const WORKFLOW_DEFINITIONS: &'static [&'static dyn WorkflowDefinition] = &[];
    const QUEUE_DEAD_LETTER: Option<QueueDeadLetterTarget> = None;
    const CRON_TARGETS: &'static [CronTarget] = &[];
}

/// Registers one module's internal scheduler Tick command.
pub fn register_maintenance<M: MaintenanceModule>(
    registry: &mut RegistryBuilder,
) -> crate::Result<()> {
    registry.bind_maintenance_module(M::MODULE, M::QUEUE_DEAD_LETTER)?;
    registry.bind_maintenance_runner::<M>()?;
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
            context.target(),
            context.sequence(),
            context.now_ms(),
            M::WORKFLOW_DEFINITIONS,
            M::QUEUE_DEAD_LETTER,
            M::CRON_TARGETS,
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
    use crab_ltx::rusqlite::Connection;

    use super::*;
    use crate::{
        CellId, IncarnationId, install_queue_schema, install_runtime_schema,
        install_workflow_schema,
    };

    #[test]
    fn maintenance_codecs_reject_unbounded_results() {
        let mut encoder = BoundedEncoder::new(16).unwrap();
        assert!(
            MaintenanceTickOutcome::Applied { processed: 129 }
                .encode(&mut encoder)
                .is_err()
        );
    }

    #[test]
    fn retained_runtime_outcome_blocks_contract_removal() {
        let mut connection = Connection::open_in_memory().unwrap();
        install_runtime_schema(
            &mut connection,
            CellId::from_bytes([1; 32]),
            IncarnationId::from_bytes([2; 16]),
            1,
        )
        .unwrap();
        assert!(
            inspect_persisted_work(&connection, CatalogRole::Repository)
                .unwrap()
                .is_empty()
        );
        connection
            .execute(
                "INSERT INTO sys_requests VALUES (?1, ?2, 1, X'', 1, 1, 1)",
                ([3_u8; 16].as_slice(), [4_u8; 32].as_slice()),
            )
            .unwrap();

        assert_eq!(
            inspect_persisted_work(&connection, CatalogRole::Repository)
                .unwrap()
                .first_blocker(),
            Some("maintenance release is blocked by retained request outcomes")
        );
    }

    #[test]
    fn unknown_inventory_stays_fail_closed_without_changing_wire_width() {
        let unknown = PersistedWorkInventory::unknown();
        assert!(unknown.is_unknown());
        assert!(!unknown.is_empty());
        assert_eq!(
            unknown.first_blocker(),
            Some("maintenance release is blocked by unknown persisted work")
        );
        assert!(PersistedWorkInventory::decode(&unknown.encode()).is_err());

        let empty = PersistedWorkInventory::decode(&[0]).unwrap();
        assert!(empty.is_empty());
        assert!(!empty.is_unknown());
    }

    #[test]
    fn queue_and_workflow_roles_inventory_primitive_rows() {
        let mut queue = Connection::open_in_memory().unwrap();
        install_runtime_schema(
            &mut queue,
            CellId::from_bytes([5; 32]),
            IncarnationId::from_bytes([6; 16]),
            1,
        )
        .unwrap();
        let transaction = queue.transaction().unwrap();
        install_queue_schema(&transaction).unwrap();
        transaction.commit().unwrap();
        queue
            .execute(
                "INSERT INTO queue_dedup VALUES (?1, ?2, ?3, 1)",
                (
                    [7_u8; 16].as_slice(),
                    [8_u8; 32].as_slice(),
                    [9_u8; 16].as_slice(),
                ),
            )
            .unwrap();
        assert_eq!(
            inspect_persisted_work(&queue, CatalogRole::Queue)
                .unwrap()
                .first_blocker(),
            Some("maintenance release is blocked by retained Queue producer identities")
        );

        let mut workflow = Connection::open_in_memory().unwrap();
        install_runtime_schema(
            &mut workflow,
            CellId::from_bytes([10; 32]),
            IncarnationId::from_bytes([11; 16]),
            1,
        )
        .unwrap();
        let transaction = workflow.transaction().unwrap();
        install_workflow_schema(&transaction).unwrap();
        transaction.commit().unwrap();
        workflow
            .execute(
                "INSERT INTO workflow_runs VALUES (X'01', ?1, ?2, 0, X'', 0, NULL, NULL)",
                ([12_u8; 16].as_slice(), [13_u8; 32].as_slice()),
            )
            .unwrap();
        assert_eq!(
            inspect_persisted_work(&workflow, CatalogRole::Workflow)
                .unwrap()
                .first_blocker(),
            Some("maintenance release is blocked by retained Workflow runs")
        );
    }
}
