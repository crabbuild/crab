use std::{sync::Arc, time::UNIX_EPOCH};

use crab_cell_runtime::{
    ApplicationId, BuildDescriptor, CatalogEntry, CatalogRole, CellAuthority, CellCatalog,
    CellClient, CellModule, CellRuntime, CellTarget, Digest, IncarnationId, InvocationError,
    MaintenanceModule, MigrationDescriptor, ModuleDescriptor, MutationIdentity,
    NamespaceDescriptor, NamespaceId, OperationDescriptor, Owner, QueueClaimRequest,
    QueueLeaseAction, QueueLeaseOutcome, QueueModule, QueueNamespace, QueueSendOutcome,
    QueueSendRequest, QueueState, QueueTokenSource, RegistryBuilder, RequestId, SessionId,
    SqlWorkerPool, TenantId, install_queue_schema, install_runtime_schema, queue_apply_lease,
    queue_claim, queue_cleanup_expired, queue_send, queue_validate_claim, register_queue,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

const QUEUE_MODULE: &str = "queue-test";
const QUEUE_NAMESPACE: NamespaceId = NamespaceId::from_bytes([6; 16]);
const QUEUE_MIGRATION: &str = include_str!("../src/migrations/queue.sql");
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
    const QUEUE_DEAD_LETTER: Option<crab_cell_runtime::QueueDeadLetterTarget> =
        Some(crab_cell_runtime::QueueDeadLetterTarget::new(
            DEAD_LETTER_MODULE,
            DEAD_LETTER_NAMESPACE,
            1,
            1,
            1,
        ));
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
    const QUEUE_DEAD_LETTER: Option<crab_cell_runtime::QueueDeadLetterTarget> =
        Some(crab_cell_runtime::QueueDeadLetterTarget::new(
            DEAD_LETTER_MODULE,
            DEAD_LETTER_NAMESPACE,
            2,
            1,
            1,
        ));
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

fn current_identity(byte: u8) -> MutationIdentity {
    let now_ms = unix_time_ms();
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

fn unix_time_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
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

#[test]
fn producer_send_is_idempotent_and_conflicts_on_changed_payload_or_schedule() {
    let mut connection = connection();
    let namespace = NamespaceId::from_bytes([3; 16]);
    let request = send_request(1, b"payload", 20);
    let transaction = connection.transaction().unwrap();
    let first = queue_send(&transaction, namespace, 10, &request).unwrap();
    let second = queue_send(&transaction, namespace, 10, &request).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        queue_send(
            &transaction,
            namespace,
            10,
            &send_request(1, b"changed", 20),
        )
        .unwrap(),
        QueueSendOutcome::ProducerConflict
    );
    assert_eq!(
        queue_send(
            &transaction,
            namespace,
            10,
            &send_request(1, b"payload", 21),
        )
        .unwrap(),
        QueueSendOutcome::ProducerConflict
    );
    transaction.commit().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM queue_messages", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn delayed_send_with_past_schedule_becomes_immediately_ready() {
    let mut connection = connection();
    let transaction = connection.transaction().unwrap();
    let outcome = queue_send(
        &transaction,
        QUEUE_NAMESPACE,
        20,
        &send_request(2, b"delayed-effect", 10),
    )
    .unwrap();
    let QueueSendOutcome::Sent { message_id } = outcome else {
        panic!("first queue send must insert")
    };
    let due_at_ms: i64 = transaction
        .query_row(
            "SELECT due_at_ms FROM queue_messages WHERE message_id = ?1",
            [message_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(due_at_ms, 20);
    transaction.commit().unwrap();
}

#[test]
fn claim_tokens_require_published_live_lease_for_ack_retry_and_extend() {
    let mut connection = connection();
    let namespace = NamespaceId::from_bytes([3; 16]);
    let transaction = connection.transaction().unwrap();
    for producer in 1..=2 {
        queue_send(
            &transaction,
            namespace,
            10,
            &send_request(producer, &[producer], 10),
        )
        .unwrap();
    }
    transaction.commit().unwrap();

    let transaction = connection.transaction().unwrap();
    let mut tokens = Tokens(0);
    let claimed = queue_claim(&transaction, 20, 2, 5_000, &mut tokens).unwrap();
    assert_eq!(claimed.len(), 2);
    assert_eq!(claimed[0].attempt, 1);
    transaction.commit().unwrap();
    assert!(queue_validate_claim(&connection, 21, &claimed).unwrap());
    assert!(!queue_validate_claim(&connection, 4_021, &claimed).unwrap());

    let first = &claimed[0];
    let transaction = connection.transaction().unwrap();
    assert_eq!(
        queue_apply_lease(
            &transaction,
            100,
            first.message_id,
            [99; 16],
            QueueLeaseAction::Ack,
        )
        .unwrap(),
        QueueLeaseOutcome::LeaseLost
    );
    assert_eq!(
        queue_apply_lease(
            &transaction,
            100,
            first.message_id,
            first.token,
            QueueLeaseAction::Extend {
                extension_ms: 10_000,
            },
        )
        .unwrap(),
        QueueLeaseOutcome::Applied {
            state: QueueState::Leased,
            lease_until_ms: Some(10_100),
        }
    );
    transaction.commit().unwrap();

    let transaction = connection.transaction().unwrap();
    assert_eq!(
        queue_apply_lease(
            &transaction,
            101,
            first.message_id,
            first.token,
            QueueLeaseAction::Ack,
        )
        .unwrap(),
        QueueLeaseOutcome::Applied {
            state: QueueState::Acked,
            lease_until_ms: None,
        }
    );
    transaction.commit().unwrap();
}

#[test]
fn retry_and_expired_reclaim_preserve_attempt_limits_and_cleanup_bounds() {
    let mut connection = connection();
    let namespace = NamespaceId::from_bytes([3; 16]);
    let transaction = connection.transaction().unwrap();
    queue_send(&transaction, namespace, 0, &send_request(1, b"payload", 0)).unwrap();
    transaction.commit().unwrap();

    let mut tokens = Tokens(0);
    let transaction = connection.transaction().unwrap();
    let first = queue_claim(&transaction, 0, 1, 5_000, &mut tokens)
        .unwrap()
        .remove(0);
    assert_eq!(
        queue_apply_lease(
            &transaction,
            1,
            first.message_id,
            first.token,
            QueueLeaseAction::Retry { delay_ms: 100 },
        )
        .unwrap(),
        QueueLeaseOutcome::Applied {
            state: QueueState::Ready,
            lease_until_ms: None,
        }
    );
    transaction.commit().unwrap();

    let transaction = connection.transaction().unwrap();
    assert!(
        queue_claim(&transaction, 100, 1, 5_000, &mut tokens)
            .unwrap()
            .is_empty()
    );
    let second = queue_claim(&transaction, 101, 1, 5_000, &mut tokens)
        .unwrap()
        .remove(0);
    assert_eq!(second.attempt, 2);
    transaction
        .execute(
            "UPDATE queue_messages SET attempt = 20, lease_until_ms = 102 WHERE message_id = ?1",
            [second.message_id.as_slice()],
        )
        .unwrap();
    transaction.commit().unwrap();

    let transaction = connection.transaction().unwrap();
    assert!(
        queue_claim(&transaction, 102, 1, 5_000, &mut tokens)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        transaction
            .query_row(
                "SELECT state FROM queue_messages WHERE message_id = ?1",
                [second.message_id.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        3
    );
    transaction
        .execute(
            "UPDATE queue_messages SET expires_at_ms = 102 WHERE message_id = ?1",
            [second.message_id.as_slice()],
        )
        .unwrap();
    assert_eq!(queue_cleanup_expired(&transaction, 102).unwrap(), 1);
    assert_eq!(
        transaction
            .query_row("SELECT count(*) FROM queue_dedup", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    transaction.commit().unwrap();
}

#[tokio::test]
async fn typed_queue_namespace_publishes_validates_and_survives_restore() {
    let registry = queue_registry();
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        QUEUE_NAMESPACE,
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
                CatalogRole::Queue,
                registry.module_code(QUEUE_MODULE).unwrap(),
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
            install_queue_schema,
        )
        .await
        .unwrap();
    let queue = QueueNamespace::<TestQueue>::new(
        CellClient::local(registry.clone(), handle.clone()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    let available_at_ms = unix_time_ms() + 100;
    let sent = queue
        .send(
            current_identity(7),
            QueueSendRequest {
                producer_id: [7; 16],
                payload: b"job".to_vec(),
                available_at_ms,
            },
        )
        .await
        .unwrap();
    assert!(matches!(sent.output, QueueSendOutcome::Sent { .. }));
    let conflict = queue
        .send(
            current_identity(8),
            QueueSendRequest {
                producer_id: [7; 16],
                payload: b"different".to_vec(),
                available_at_ms,
            },
        )
        .await;
    assert!(matches!(
        conflict,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == QueueSendOutcome::ProducerConflict
    ));
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let claimed = queue
        .claim(
            current_identity(9),
            0,
            QueueClaimRequest {
                limit: 1,
                lease_ms: 10_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed.output.len(), 1);
    assert_eq!(claimed.output[0].payload, b"job");
    assert_eq!(
        authority
            .load(cell)
            .await
            .unwrap()
            .unwrap()
            .value()
            .next_due_ms,
        Some(claimed.output[0].lease_until_ms)
    );
    assert!(
        queue
            .validate_claim(0, claimed.output.clone(), Some(claimed.receipt))
            .await
            .unwrap()
            .output
    );
    handle.drain().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
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
    let restored_queue = QueueNamespace::<TestQueue>::new(
        CellClient::local(registry, restored.clone()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    assert!(
        restored_queue
            .validate_claim(0, claimed.output.clone(), Some(claimed.receipt))
            .await
            .unwrap()
            .output
    );
    let acked = restored_queue
        .ack(
            current_identity(10),
            0,
            claimed.output[0].message_id,
            claimed.output[0].token,
        )
        .await
        .unwrap();
    assert_eq!(
        acked.output,
        QueueLeaseOutcome::Applied {
            state: QueueState::Acked,
            lease_until_ms: None,
        }
    );
    restored.drain().await.unwrap();
}
