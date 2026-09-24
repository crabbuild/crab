use std::sync::Arc;

use crab_cell_runtime::cell::actor::CellRuntime;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::catalog::{CatalogEntry, CellCatalog};
use crab_cell_runtime::cell::schema::install_runtime_schema;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::control::Owner;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::IncarnationId;
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_cell_runtime::primitives::maintenance::MaintenanceModule;
use crab_cell_runtime::primitives::queue::{
    QueueClaimRequest, QueueLeaseAction, QueueLeaseOutcome, QueueSendOutcome, QueueSendRequest,
    QueueState, QueueTokenSource, install_queue_schema, queue_apply_lease, queue_claim,
    queue_cleanup_expired, queue_send, queue_validate_claim, register_queue,
};
use crab_cell_runtime::primitives::queue::{QueueModule, QueueNamespace};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, ModuleDescriptor, NamespaceDescriptor, RegistryBuilder,
};
use crab_cell_runtime::registry::{MigrationDescriptor, OperationDescriptor};
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Limits};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

use crate::support::fixtures::{mutation_identity, now_ms};

const QUEUE_MODULE: &str = "queue-test";
const QUEUE_NAMESPACE: NamespaceId = NamespaceId::from_bytes([6; 16]);
const QUEUE_MIGRATION: &str = include_str!("../../src/migrations/queue.sql");
const QUEUE_COMMANDS: &[OperationDescriptor] = &[
    operation(1, 270 * 1024, 32),
    operation(2, 8, 530 * 1024),
    operation(3, 64, 16),
    operation(4, 8, 5),
    operation(5, 8, 16),
];
const QUEUE_QUERIES: &[OperationDescriptor] = &[operation(1, 530 * 1024, 1), operation(2, 1, 64)];

struct TestQueue;

impl QueueModule for TestQueue {
    const NAMESPACE: NamespaceId = QUEUE_NAMESPACE;
    const SEND_COMMAND_ID: u32 = 1;
    const CLAIM_COMMAND_ID: u32 = 2;
    const LEASE_COMMAND_ID: u32 = 3;
    const VALIDATE_QUERY_ID: u32 = 1;
    const CONTROL_COMMAND_ID: u32 = 5;
    const INFO_QUERY_ID: u32 = 2;
}

impl MaintenanceModule for TestQueue {
    const MODULE: &'static str = QUEUE_MODULE;
    const TICK_COMMAND_ID: u32 = 4;
}

impl CellModule for TestQueue {
    const NAME: &'static str = QUEUE_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<ModuleDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: QUEUE_MODULE,
            source_digest: Digest::from_bytes([4; 32]),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: QUEUE_MIGRATION,
                digest: Digest::from_bytes(*blake3::hash(QUEUE_MIGRATION.as_bytes()).as_bytes()),
            }])),
            commands: QUEUE_COMMANDS,
            queries: QUEUE_QUERIES,
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &[NamespaceDescriptor {
                id: QUEUE_NAMESPACE,
                name: QUEUE_MODULE,
                role: CatalogRole::Queue,
                shards: 1,
                effect_targets: &[],
                dead_letter: None,
            }],
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_queue::<Self>(registry)
    }
}

const DEAD_LETTER_MODULE: &str = "dead-letter-test";
const DEAD_LETTER_NAMESPACE: NamespaceId = NamespaceId::from_bytes([7; 16]);
const SOURCE_MODULE: &str = "source-queue-test";
const SOURCE_NAMESPACE: NamespaceId = NamespaceId::from_bytes([8; 16]);

struct DeadLetterQueue;

impl QueueModule for DeadLetterQueue {
    const NAMESPACE: NamespaceId = DEAD_LETTER_NAMESPACE;
    const SEND_COMMAND_ID: u32 = 1;
    const CLAIM_COMMAND_ID: u32 = 2;
    const LEASE_COMMAND_ID: u32 = 3;
    const VALIDATE_QUERY_ID: u32 = 1;
    const CONTROL_COMMAND_ID: u32 = 5;
    const INFO_QUERY_ID: u32 = 2;
}

impl MaintenanceModule for DeadLetterQueue {
    const MODULE: &'static str = DEAD_LETTER_MODULE;
    const TICK_COMMAND_ID: u32 = 4;
}

impl CellModule for DeadLetterQueue {
    const NAME: &'static str = DEAD_LETTER_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        queue_descriptor(DEAD_LETTER_MODULE, DEAD_LETTER_NAMESPACE, &[], None, 10)
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_queue::<Self>(registry)
    }
}

struct SourceQueue;

impl QueueModule for SourceQueue {
    const NAMESPACE: NamespaceId = SOURCE_NAMESPACE;
    const SEND_COMMAND_ID: u32 = 1;
    const CLAIM_COMMAND_ID: u32 = 2;
    const LEASE_COMMAND_ID: u32 = 3;
    const VALIDATE_QUERY_ID: u32 = 1;
    const CONTROL_COMMAND_ID: u32 = 5;
    const INFO_QUERY_ID: u32 = 2;
}

impl MaintenanceModule for SourceQueue {
    const MODULE: &'static str = SOURCE_MODULE;
    const TICK_COMMAND_ID: u32 = 4;
    const QUEUE_DEAD_LETTER: Option<crab_cell_runtime::primitives::queue::QueueDeadLetterTarget> =
        Some(
            crab_cell_runtime::primitives::queue::QueueDeadLetterTarget::new(
                DEAD_LETTER_MODULE,
                DEAD_LETTER_NAMESPACE,
                1,
                1,
                1,
            ),
        );
}

impl CellModule for SourceQueue {
    const NAME: &'static str = SOURCE_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        queue_descriptor(
            SOURCE_MODULE,
            SOURCE_NAMESPACE,
            &[DEAD_LETTER_NAMESPACE],
            Some(DEAD_LETTER_NAMESPACE),
            11,
        )
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_queue::<Self>(registry)
    }
}

struct BadSourceQueue;

impl QueueModule for BadSourceQueue {
    const NAMESPACE: NamespaceId = NamespaceId::from_bytes([9; 16]);
    const SEND_COMMAND_ID: u32 = 1;
    const CLAIM_COMMAND_ID: u32 = 2;
    const LEASE_COMMAND_ID: u32 = 3;
    const VALIDATE_QUERY_ID: u32 = 1;
    const CONTROL_COMMAND_ID: u32 = 5;
    const INFO_QUERY_ID: u32 = 2;
}

impl MaintenanceModule for BadSourceQueue {
    const MODULE: &'static str = "bad-source-queue-test";
    const TICK_COMMAND_ID: u32 = 4;
    const QUEUE_DEAD_LETTER: Option<crab_cell_runtime::primitives::queue::QueueDeadLetterTarget> =
        Some(
            crab_cell_runtime::primitives::queue::QueueDeadLetterTarget::new(
                DEAD_LETTER_MODULE,
                DEAD_LETTER_NAMESPACE,
                2,
                1,
                1,
            ),
        );
}

impl CellModule for BadSourceQueue {
    const NAME: &'static str = "bad-source-queue-test";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        queue_descriptor(
            Self::NAME,
            Self::NAMESPACE,
            &[DEAD_LETTER_NAMESPACE],
            Some(DEAD_LETTER_NAMESPACE),
            12,
        )
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_queue::<Self>(registry)
    }
}

fn queue_descriptor(
    name: &'static str,
    namespace: NamespaceId,
    effect_targets: &'static [NamespaceId],
    dead_letter: Option<NamespaceId>,
    digest: u8,
) -> &'static ModuleDescriptor {
    Box::leak(Box::new(ModuleDescriptor {
        name,
        source_digest: Digest::from_bytes([digest; 32]),
        retained_codes: &[],
        schema_min: 1,
        schema_max: 1,
        migrations: Box::leak(Box::new([MigrationDescriptor {
            version: 1,
            sql: QUEUE_MIGRATION,
            digest: Digest::from_bytes(*blake3::hash(QUEUE_MIGRATION.as_bytes()).as_bytes()),
        }])),
        commands: QUEUE_COMMANDS,
        queries: QUEUE_QUERIES,
        workflow_definitions: &[],
        activity_types: &[],
        namespaces: Box::leak(Box::new([NamespaceDescriptor {
            id: namespace,
            name,
            role: CatalogRole::Queue,
            shards: 1,
            effect_targets,
            dead_letter,
        }])),
    }))
}

const fn operation(id: u32, input_limit: u32, output_limit: u32) -> OperationDescriptor {
    OperationDescriptor {
        id,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit,
        output_limit,
    }
}

fn queue_registry() -> Arc<crab_cell_runtime::Registry> {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "queue-api-test".into(),
        cargo_lock_digest: Digest::from_bytes([5; 32]),
    });
    builder.register(TestQueue).unwrap();
    Arc::new(builder.finish().unwrap())
}

#[test]
fn queue_registry_validates_dead_letter_module_ids_and_shards() {
    let mut valid = RegistryBuilder::new(BuildDescriptor {
        source_revision: "queue-dead-letter-test".into(),
        cargo_lock_digest: Digest::from_bytes([13; 32]),
    });
    valid.register(SourceQueue).unwrap();
    valid.register(DeadLetterQueue).unwrap();
    valid.finish().unwrap();

    let mut invalid = RegistryBuilder::new(BuildDescriptor {
        source_revision: "queue-dead-letter-drift-test".into(),
        cargo_lock_digest: Digest::from_bytes([14; 32]),
    });
    invalid.register(BadSourceQueue).unwrap();
    invalid.register(DeadLetterQueue).unwrap();
    assert!(invalid.finish().is_err());
}

struct Tokens(u8);

impl QueueTokenSource for Tokens {
    fn next_token(&mut self) -> crab_cell_runtime::Result<[u8; 16]> {
        self.0 = self
            .0
            .checked_add(1)
            .ok_or(crab_cell_runtime::Error::Command(
                "test queue token overflow",
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
    install_queue_schema(&transaction).unwrap();
    transaction.commit().unwrap();
    connection
}

fn send_request(producer: u8, payload: &[u8], available_at_ms: i64) -> QueueSendRequest {
    QueueSendRequest {
        producer_id: [producer; 16],
        payload: payload.to_vec(),
        available_at_ms,
    }
}

mod lease;
mod namespace;
mod send;
