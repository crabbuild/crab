use std::{sync::Arc, time::UNIX_EPOCH};

use crab_cell_runtime::{
    ApplicationId, BoundedDecoder, BoundedEncoder, BuildDescriptor, CatalogEntry, CatalogRole,
    CellAuthority, CellClient, CellDescription, CellModule, CellTarget, CodecError, Command,
    CommandContext, CommandResult, Digest, IncarnationId, InvocationError, MigrationDescriptor,
    ModuleDescriptor, MutationIdentity, NamespaceDescriptor, NamespaceId, OperationDescriptor,
    Owner, Query, QueryContext, Receipt, Registry, RegistryBuilder, RequestId, SessionId, SqlBatch,
    SqlStatement, SqlValue, SqlWorkerPool, TenantId, WireValue, command_operation_digest,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

const MODULE: &str = "repository";
const NAMESPACE: NamespaceId = NamespaceId::from_bytes([6; 16]);
const MIGRATION: &str =
    "CREATE TABLE comments(body BLOB NOT NULL); CREATE TABLE audit(value INTEGER NOT NULL)";
const COMMANDS: &[OperationDescriptor] = &[
    OperationDescriptor {
        id: 1,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 64,
        output_limit: 64,
    },
    OperationDescriptor {
        id: 2,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 64,
        output_limit: 64,
    },
    OperationDescriptor {
        id: 3,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 64,
        output_limit: 1,
    },
];
const QUERIES: &[OperationDescriptor] = &[OperationDescriptor {
    id: 1,
    codec_version: 1,
    schema_min: 1,
    schema_max: 1,
    input_limit: 1,
    output_limit: 8,
}];

struct RepositoryModule;

impl CellModule for RepositoryModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        descriptor()
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_command::<CreateComment>()?;
        registry.bind_command::<RejectComment>()?;
        registry.bind_command::<InvalidResultComment>()?;
        registry.bind_query::<CountComments>()?;
        Ok(())
    }
}

struct CreateComment;

impl Command for CreateComment {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "INSERT INTO comments(body) VALUES (?)".into(),
                parameters: vec![SqlValue::Blob(input.clone())],
            }],
        })?;
        Ok(CommandResult::Success(input))
    }
}

struct RejectComment;

impl Command for RejectComment {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "INSERT INTO comments(body) VALUES (?)".into(),
                parameters: vec![SqlValue::Blob(input)],
            }],
        })?;
        Ok(CommandResult::Rejected(b"moderated".to_vec()))
    }
}

struct InvalidOutput;

impl WireValue for InvalidOutput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u8(2)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        decoder.read_bool()?;
        Ok(Self)
    }
}

struct InvalidResultComment;

impl Command for InvalidResultComment {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Vec<u8>;
    type Output = InvalidOutput;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "INSERT INTO comments(body) VALUES (?)".into(),
                parameters: vec![SqlValue::Blob(input)],
            }],
        })?;
        Ok(CommandResult::Success(InvalidOutput))
    }
}

struct CountComments;

impl Query for CountComments {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = ();
    type Output = u64;

    fn execute(
        context: &mut QueryContext<'_>,
        _input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        let results = context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "SELECT COUNT(*) FROM comments".into(),
                parameters: Vec::new(),
            }],
        })?;
        match results[0].rows.first().and_then(|row| row.first()) {
            Some(SqlValue::Integer(count)) => u64::try_from(*count)
                .map_err(|_| crab_cell_runtime::Error::Command("negative comment count")),
            _ => Err(crab_cell_runtime::Error::Command(
                "comment count query returned no integer",
            )),
        }
    }
}

struct WrongModuleCommand;

impl Command for WrongModuleCommand {
    const MODULE: &'static str = "other";
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = ();
    type Output = ();

    fn execute(
        _context: &mut CommandContext<'_, '_>,
        _input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        Ok(CommandResult::Success(()))
    }
}

fn descriptor() -> &'static ModuleDescriptor {
    static DESCRIPTOR: std::sync::OnceLock<ModuleDescriptor> = std::sync::OnceLock::new();
    DESCRIPTOR.get_or_init(|| ModuleDescriptor {
        name: MODULE,
        source_digest: Digest::from_bytes([3; 32]),
        schema_min: 1,
        schema_max: 1,
        migrations: Box::leak(Box::new([MigrationDescriptor {
            version: 1,
            sql: MIGRATION,
            digest: Digest::from_bytes(*blake3::hash(MIGRATION.as_bytes()).as_bytes()),
        }])),
        commands: COMMANDS,
        queries: QUERIES,
        workflow_definitions: &[],
        activity_types: &[],
        namespaces: &[NamespaceDescriptor {
            id: NAMESPACE,
            name: MODULE,
            role: CatalogRole::Repository,
            shards: 1,
            effect_targets: &[],
            dead_letter: None,
        }],
    })
}

fn registry() -> Arc<Registry> {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "client-test".into(),
        cargo_lock_digest: Digest::from_bytes([9; 32]),
    });
    builder.register(RepositoryModule).unwrap();
    Arc::new(builder.finish().unwrap())
}

struct Fixture {
    _directory: tempfile::TempDir,
    target: CellTarget,
    handle: crab_cell_runtime::CellHandle,
    registry: Arc<Registry>,
}

async fn fixture() -> Fixture {
    let registry = registry();
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NAMESPACE,
        b"repository-42",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([4; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("client"), [2; 16]);
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog = crab_cell_runtime::CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Repository,
                registry.module_code(MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let session = SessionId::from_bytes([5; 16]);
    let authority = CellAuthority::new(layout);
    let observed = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: "https://local.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let handle = crab_cell_runtime::CellRuntime::new(
        SqlWorkerPool::new(1, 4).unwrap(),
        4 * 1024 * 1024,
        session,
    )
    .unwrap()
    .bootstrap(
        proof,
        replica,
        authority,
        observed,
        directory.path().join("repository.sqlite"),
        |transaction| {
            transaction.execute_batch(MIGRATION)?;
            Ok(())
        },
    )
    .await
    .unwrap();
    Fixture {
        _directory: directory,
        target,
        handle,
        registry,
    }
}

fn mutation(byte: u8) -> MutationIdentity {
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

#[tokio::test]
async fn typed_client_publishes_replays_rejections_and_receipted_reads() {
    let fixture = fixture().await;
    let client = CellClient::local(fixture.registry, fixture.handle.clone());
    let identity = mutation(7);

    let committed = client
        .command::<CreateComment>(&fixture.target, identity, b"first".to_vec())
        .await
        .unwrap();
    assert_eq!(committed.output, b"first");
    assert_eq!(committed.receipt.commit_sequence, 1);

    let replay = client
        .command::<CreateComment>(&fixture.target, identity, b"first".to_vec())
        .await
        .unwrap();
    assert_eq!(replay, committed);

    let rejected = client
        .command::<RejectComment>(&fixture.target, mutation(8), b"hidden".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(
        rejected,
        InvocationError::Rejected(ref outcome)
            if outcome.output == b"moderated" && outcome.receipt.commit_sequence == 2
    ));

    let observed = client
        .query::<CountComments>(&fixture.target, Some(committed.receipt), ())
        .await
        .unwrap();
    assert_eq!(observed.output, 1);
    assert_eq!(observed.receipt.commit_sequence, 2);

    fixture.handle.drain().await.unwrap();
}

#[tokio::test]
async fn typed_client_rejects_conflicting_identity_receipt_and_module_before_execution() {
    let fixture = fixture().await;
    let client = CellClient::local(fixture.registry, fixture.handle.clone());
    let identity = mutation(9);
    let committed = client
        .command::<CreateComment>(&fixture.target, identity, b"first".to_vec())
        .await
        .unwrap();

    assert!(matches!(
        client
            .command::<CreateComment>(&fixture.target, identity, b"different".to_vec())
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::RequestConflict
        ))
    ));
    assert!(matches!(
        client
            .query::<CountComments>(
                &fixture.target,
                Some(Receipt {
                    incarnation: IncarnationId::from_bytes([99; 16]),
                    ..committed.receipt
                }),
                (),
            )
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::Command("minimum receipt does not match Cell")
        ))
    ));
    assert!(matches!(
        client
            .command::<WrongModuleCommand>(&fixture.target, mutation(10), ())
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::Registry("operation module does not own namespace")
        ))
    ));
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    assert!(matches!(
        client
            .command::<CreateComment>(
                &fixture.target,
                MutationIdentity {
                    request_id: RequestId::from_bytes([11; 16]),
                    issued_at_ms: now_ms - 2,
                    expires_at_ms: now_ms - 1,
                },
                b"expired".to_vec(),
            )
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::Command("invalid mutation identity lifetime")
        ))
    ));

    fixture.handle.drain().await.unwrap();
}

#[tokio::test]
async fn invalid_typed_result_preserves_the_published_receipt() {
    let fixture = fixture().await;
    let client = CellClient::local(fixture.registry, fixture.handle.clone());

    let result = client
        .command::<InvalidResultComment>(&fixture.target, mutation(12), b"stored".to_vec())
        .await;
    assert!(matches!(
        result,
        Err(InvocationError::InvalidPublishedResult {
            receipt: Receipt {
                commit_sequence: 1,
                ..
            },
            ..
        })
    ));
    assert_eq!(
        client
            .query::<CountComments>(&fixture.target, None, ())
            .await
            .unwrap()
            .output,
        1
    );

    fixture.handle.drain().await.unwrap();
}

#[test]
fn command_digest_is_canonical_and_binds_incarnation_identity_and_input() {
    let description = CellDescription {
        cell: crab_cell_runtime::CellId::from_bytes([1; 32]),
        incarnation: IncarnationId::from_bytes([2; 16]),
        code: Digest::from_bytes([3; 32]),
        schema: 1,
    };
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([4; 16]),
        issued_at_ms: 5,
        expires_at_ms: 6,
    };
    let mut encoder = BoundedEncoder::new(64).unwrap();
    b"input".to_vec().encode(&mut encoder).unwrap();
    let input = encoder.finish();
    let digest = command_operation_digest::<CreateComment>(description, identity, &input).unwrap();
    assert_eq!(
        *digest.as_bytes(),
        [
            220, 198, 211, 0, 239, 12, 190, 71, 247, 29, 242, 5, 254, 27, 25, 124, 91, 249, 225,
            28, 47, 142, 161, 104, 78, 125, 152, 205, 181, 67, 244, 134,
        ]
    );
    assert_ne!(
        digest,
        command_operation_digest::<CreateComment>(
            CellDescription {
                incarnation: IncarnationId::from_bytes([9; 16]),
                ..description
            },
            identity,
            &input,
        )
        .unwrap()
    );
    assert_ne!(
        digest,
        command_operation_digest::<CreateComment>(description, identity, b"other").unwrap()
    );
}
