use std::{future::Future, pin::Pin, sync::Arc, time::UNIX_EPOCH};

use crab_cell_runtime::{
    ApplicationId, BoundedDecoder, BoundedEncoder, BuildDescriptor, CatalogEntry, CatalogRole,
    CellAuthority, CellClient, CellDescription, CellModule, CellTarget, CodecError, Command,
    CommandContext, CommandResult, Digest, EffectClaim, EffectClaimRequest, EffectIntent,
    EffectLeaseOutcome, EffectModule, EffectPeerClient, EffectRunOutcome, EffectSource,
    EffectSupervisor, IncarnationId, InvocationError, MigrationDescriptor, ModuleDescriptor,
    MutationIdentity, NamespaceDescriptor, NamespaceId, OperationDescriptor, Owner, PeerAuthorizer,
    PeerCellResolver, PeerDispatcher, PeerPrincipal, PeerRoundTrip, PeerSigner, PeerVerifier,
    Query, QueryContext, Receipt, Registry, RegistryBuilder, RequestId, Resolution, SessionId,
    SqlBatch, SqlStatement, SqlValue, SqlWorkerPool, TenantId, VerifiedPeerRequest, WireValue,
    command_operation_digest, effect_id, effect_insert, effect_operation_digest, peer_wire as wire,
    register_effect_delivery,
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
    OperationDescriptor {
        id: 4,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 8,
        output_limit: 1 << 20,
    },
    OperationDescriptor {
        id: 5,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 1 << 20,
        output_limit: 16,
    },
];
const QUERIES: &[OperationDescriptor] = &[
    OperationDescriptor {
        id: 1,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 1,
        output_limit: 8,
    },
    OperationDescriptor {
        id: 2,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 1 << 20,
        output_limit: 1,
    },
];

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
        register_effect_delivery::<Self>(registry)?;
        Ok(())
    }
}

impl EffectModule for RepositoryModule {
    const MODULE: &'static str = MODULE;
    const CLAIM_COMMAND_ID: u32 = 4;
    const LEASE_COMMAND_ID: u32 = 5;
    const VALIDATE_QUERY_ID: u32 = 2;
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

struct LocalResolver {
    target: CellTarget,
    handle: crab_cell_runtime::CellHandle,
}

impl PeerCellResolver for LocalResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<
        Box<
            dyn Future<Output = crab_cell_runtime::Result<crab_cell_runtime::CellHandle>>
                + Send
                + 'static,
        >,
    > {
        let matches = target == self.target;
        let handle = self.handle.clone();
        Box::pin(async move {
            if matches {
                Ok(handle)
            } else {
                Err(crab_cell_runtime::Error::CellNotActive)
            }
        })
    }
}

struct RepositoryAuthorizer;

impl PeerAuthorizer for RepositoryAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> crab_cell_runtime::Result<()> {
        if request.permits("repository.issue.create") {
            Ok(())
        } else {
            Err(crab_cell_runtime::Error::PeerAuthorization(
                "missing repository action",
            ))
        }
    }
}

struct LoopbackRoundTrip {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
}

impl PeerRoundTrip for LoopbackRoundTrip {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>> {
        let verifier = Arc::clone(&self.verifier);
        let dispatcher = Arc::clone(&self.dispatcher);
        Box::pin(async move {
            let now_ms = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| crab_cell_runtime::Error::Peer("test clock"))?
                    .as_millis(),
            )
            .map_err(|_| crab_cell_runtime::Error::Peer("test clock overflow"))?;
            let verified = verifier.verify(&request, now_ms)?;
            if verified.target() != &target {
                return Err(crab_cell_runtime::Error::Peer("round trip target changed"));
            }
            dispatcher.dispatch_bytes(&verified, now_ms).await
        })
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
async fn local_and_peer_command_share_digest_dedup_and_query_state() {
    let fixture = fixture().await;
    let client = CellClient::local(Arc::clone(&fixture.registry), fixture.handle.clone());
    let identity = mutation(14);
    let committed = client
        .command::<CreateComment>(&fixture.target, identity, b"same".to_vec())
        .await
        .unwrap();

    let signer = PeerSigner::new(
        SessionId::from_bytes([12; 16]),
        fixture.registry.release_digest(),
        ed25519_dalek::SigningKey::from_bytes(&[13; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        SessionId::from_bytes([12; 16]),
        fixture.registry.release_digest(),
        signer.verifying_key(),
    ));
    let dispatcher = Arc::new(PeerDispatcher::new(
        Arc::clone(&fixture.registry),
        Arc::new(LocalResolver {
            target: fixture.target.clone(),
            handle: fixture.handle.clone(),
        }),
        Arc::new(RepositoryAuthorizer),
    ));
    let peer = CellClient::peer(
        Arc::clone(&fixture.registry),
        Arc::new(signer),
        PeerPrincipal {
            issuer: "https://identity.example".into(),
            subject: "alice".into(),
            actions: vec!["repository.issue.create".into()],
        },
        Arc::new(LoopbackRoundTrip {
            verifier,
            dispatcher,
        }),
    );
    assert_eq!(
        peer.command::<CreateComment>(&fixture.target, identity, b"same".to_vec())
            .await
            .unwrap(),
        committed
    );
    let observed = peer
        .query::<CountComments>(&fixture.target, Some(committed.receipt), ())
        .await
        .unwrap();
    assert_eq!(observed.output, 1);
    assert_eq!(observed.receipt.commit_sequence, 1);

    fixture.handle.drain().await.unwrap();
}

#[tokio::test]
async fn authenticated_effect_delivery_publishes_once_and_resolves_from_inbox() {
    let fixture = fixture().await;
    let signer = PeerSigner::new(
        SessionId::from_bytes([12; 16]),
        fixture.registry.release_digest(),
        ed25519_dalek::SigningKey::from_bytes(&[13; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        SessionId::from_bytes([12; 16]),
        fixture.registry.release_digest(),
        signer.verifying_key(),
    ));
    let dispatcher = Arc::new(PeerDispatcher::new(
        Arc::clone(&fixture.registry),
        Arc::new(LocalResolver {
            target: fixture.target.clone(),
            handle: fixture.handle.clone(),
        }),
        Arc::new(RepositoryAuthorizer),
    ));
    let round_trip = Arc::new(LoopbackRoundTrip {
        verifier,
        dispatcher,
    });
    let principal = PeerPrincipal {
        issuer: "crab-runtime:test".into(),
        subject: "source-session".into(),
        actions: vec!["repository.issue.create".into()],
    };
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let source_cell = crab_cell_runtime::CellId::from_bytes([21; 32]);
    let source_incarnation = IncarnationId::from_bytes([22; 16]);
    let source_sequence = 9;
    let ordinal = 3;
    let effect_id = effect_id(source_cell, source_incarnation, source_sequence, ordinal);
    let identity = wire::EffectIdentity {
        effect_id: effect_id.to_vec(),
        source_cell: source_cell.as_bytes().to_vec(),
        source_incarnation: source_incarnation.as_bytes().to_vec(),
        source_sequence,
        ordinal,
        expires_at_ms: now_ms + 60_000,
    };
    let mut encoder = BoundedEncoder::new(64).unwrap();
    b"effect".to_vec().encode(&mut encoder).unwrap();
    let request = wire::EffectRequest {
        target: Some(wire::Target {
            tenant_id: fixture.target.tenant().as_bytes().to_vec(),
            application_id: fixture.target.application().as_bytes().to_vec(),
            namespace_id: fixture.target.namespace().as_bytes().to_vec(),
            partition: fixture.target.partition().to_vec(),
        }),
        destination_incarnation: fixture.handle.incarnation().as_bytes().to_vec(),
        identity: Some(identity.clone()),
        operation: Some(wire::effect_request::Operation::CellCommand(
            wire::CellCommand {
                command_id: CreateComment::ID,
                codec_version: CreateComment::CODEC_VERSION,
                input: encoder.finish(),
            },
        )),
    };
    let operation_digest = effect_operation_digest(
        fixture.target.cell_id(),
        effect_id,
        &prost::Message::encode_to_vec(&request),
    );
    let claim = EffectClaim {
        effect_id,
        destination: fixture.target.cell_id(),
        operation: prost::Message::encode_to_vec(&request),
        operation_digest,
        attempt: 1,
        token: [23; 16],
        lease_until_ms: now_ms + 30_000,
        expires_at_ms: identity.expires_at_ms,
        created_sequence: source_sequence,
    };
    let client = EffectPeerClient::new(Arc::new(signer), principal, round_trip);
    let delivered = client.deliver(&claim, now_ms).await.unwrap();
    assert_eq!(delivered.commit_sequence(), 1);
    assert_eq!(client.deliver(&claim, now_ms + 1).await.unwrap(), delivered);
    assert_eq!(
        client.resolve(&claim, now_ms + 2).await.unwrap(),
        Resolution::Committed(delivered)
    );

    assert_eq!(
        CellClient::local(Arc::clone(&fixture.registry), fixture.handle.clone())
            .query::<CountComments>(&fixture.target, None, ())
            .await
            .unwrap()
            .output,
        1
    );
    fixture.handle.drain().await.unwrap();
}

#[tokio::test]
async fn typed_effect_source_publishes_claim_validation_ack_and_lost_lease() {
    let fixture = fixture().await;
    let source_cell = fixture.target.cell_id();
    let source_incarnation = fixture.handle.incarnation();
    let source_sequence = 1;
    let ordinal = 0;
    let identity = mutation(30);
    let effect_id = effect_id(source_cell, source_incarnation, source_sequence, ordinal);
    let expires_at_ms = identity.issued_at_ms + 60_000;
    let mut encoder = BoundedEncoder::new(64).unwrap();
    b"effect-source".to_vec().encode(&mut encoder).unwrap();
    let request = wire::EffectRequest {
        target: Some(wire::Target {
            tenant_id: fixture.target.tenant().as_bytes().to_vec(),
            application_id: fixture.target.application().as_bytes().to_vec(),
            namespace_id: fixture.target.namespace().as_bytes().to_vec(),
            partition: fixture.target.partition().to_vec(),
        }),
        destination_incarnation: source_incarnation.as_bytes().to_vec(),
        identity: Some(wire::EffectIdentity {
            effect_id: effect_id.to_vec(),
            source_cell: source_cell.as_bytes().to_vec(),
            source_incarnation: source_incarnation.as_bytes().to_vec(),
            source_sequence,
            ordinal,
            expires_at_ms,
        }),
        operation: Some(wire::effect_request::Operation::CellCommand(
            wire::CellCommand {
                command_id: CreateComment::ID,
                codec_version: CreateComment::CODEC_VERSION,
                input: encoder.finish(),
            },
        )),
    };
    let operation = prost::Message::encode_to_vec(&request);
    let operation_bytes = operation.len();
    fixture
        .handle
        .execute(
            identity,
            Digest::from_bytes([31; 32]),
            identity.issued_at_ms,
            operation_bytes,
            1,
            move |transaction| {
                effect_insert(
                    transaction,
                    source_cell,
                    source_incarnation,
                    source_sequence,
                    ordinal,
                    identity.issued_at_ms,
                    &EffectIntent {
                        destination: source_cell,
                        operation,
                        expires_at_ms,
                    },
                )?;
                Ok(crab_cell_runtime::HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();

    let source = EffectSource::<RepositoryModule>::new(
        CellClient::local(Arc::clone(&fixture.registry), fixture.handle.clone()),
        fixture.target.clone(),
    );
    let claimed = source
        .claim(
            mutation(32),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed.receipt.commit_sequence, 2);
    assert_eq!(claimed.output.len(), 1);
    assert_eq!(claimed.output[0].effect_id, effect_id);

    let claim = claimed.output[0].clone();
    let validated = source
        .validate(vec![claim.clone()], claimed.receipt)
        .await
        .unwrap();
    assert!(validated.output);
    assert_eq!(validated.receipt.commit_sequence, 2);

    let acknowledged = source
        .ack(mutation(33), claim.clone(), b"destination result".to_vec())
        .await
        .unwrap();
    assert_eq!(acknowledged.output, EffectLeaseOutcome::Delivered);
    assert_eq!(acknowledged.receipt.commit_sequence, 3);

    let lost = source.retry(mutation(34), claim).await.unwrap_err();
    assert!(matches!(
        lost,
        InvocationError::Rejected(ref outcome)
            if outcome.output == EffectLeaseOutcome::LeaseLost
                && outcome.receipt.commit_sequence == 4
    ));

    fixture.handle.drain().await.unwrap();
}

#[tokio::test]
async fn effect_supervisor_delivers_to_inbox_and_acknowledges_source() {
    let fixture = fixture().await;
    let source_cell = fixture.target.cell_id();
    let source_incarnation = fixture.handle.incarnation();
    let source_sequence = 1;
    let identity = mutation(40);
    let effect_id = effect_id(source_cell, source_incarnation, source_sequence, 0);
    let expires_at_ms = identity.issued_at_ms + 60_000;
    let mut encoder = BoundedEncoder::new(64).unwrap();
    b"supervised".to_vec().encode(&mut encoder).unwrap();
    let request = wire::EffectRequest {
        target: Some(wire::Target {
            tenant_id: fixture.target.tenant().as_bytes().to_vec(),
            application_id: fixture.target.application().as_bytes().to_vec(),
            namespace_id: fixture.target.namespace().as_bytes().to_vec(),
            partition: fixture.target.partition().to_vec(),
        }),
        destination_incarnation: source_incarnation.as_bytes().to_vec(),
        identity: Some(wire::EffectIdentity {
            effect_id: effect_id.to_vec(),
            source_cell: source_cell.as_bytes().to_vec(),
            source_incarnation: source_incarnation.as_bytes().to_vec(),
            source_sequence,
            ordinal: 0,
            expires_at_ms,
        }),
        operation: Some(wire::effect_request::Operation::CellCommand(
            wire::CellCommand {
                command_id: CreateComment::ID,
                codec_version: CreateComment::CODEC_VERSION,
                input: encoder.finish(),
            },
        )),
    };
    let operation = prost::Message::encode_to_vec(&request);
    let operation_bytes = operation.len();
    fixture
        .handle
        .execute(
            identity,
            Digest::from_bytes([41; 32]),
            identity.issued_at_ms,
            operation_bytes,
            1,
            move |transaction| {
                effect_insert(
                    transaction,
                    source_cell,
                    source_incarnation,
                    source_sequence,
                    0,
                    identity.issued_at_ms,
                    &EffectIntent {
                        destination: source_cell,
                        operation,
                        expires_at_ms,
                    },
                )?;
                Ok(crab_cell_runtime::HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();

    let signer = PeerSigner::new(
        SessionId::from_bytes([42; 16]),
        fixture.registry.release_digest(),
        ed25519_dalek::SigningKey::from_bytes(&[43; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        SessionId::from_bytes([42; 16]),
        fixture.registry.release_digest(),
        signer.verifying_key(),
    ));
    let dispatcher = Arc::new(PeerDispatcher::new(
        Arc::clone(&fixture.registry),
        Arc::new(LocalResolver {
            target: fixture.target.clone(),
            handle: fixture.handle.clone(),
        }),
        Arc::new(RepositoryAuthorizer),
    ));
    let peer = EffectPeerClient::new(
        Arc::new(signer),
        PeerPrincipal {
            issuer: "crab-runtime:test".into(),
            subject: "source-session".into(),
            actions: vec!["repository.issue.create".into()],
        },
        Arc::new(LoopbackRoundTrip {
            verifier,
            dispatcher,
        }),
    );
    let source = EffectSource::<RepositoryModule>::new(
        CellClient::local(Arc::clone(&fixture.registry), fixture.handle.clone()),
        fixture.target.clone(),
    );
    let outcome = EffectSupervisor::new(source, peer, 5_000)
        .unwrap()
        .run_once()
        .await
        .unwrap();
    assert!(
        matches!(
            &outcome,
            EffectRunOutcome::Delivered {
                destination: crab_cell_runtime::StoredOutcome::Success {
                    result,
                    commit_sequence: 3,
                },
                receipt: Receipt {
                    commit_sequence: 4,
                    ..
                },
            } if {
                let mut decoder = BoundedDecoder::new(result, 64).unwrap();
                let decoded = Vec::<u8>::decode(&mut decoder).unwrap();
                decoder.finish().unwrap();
                decoded == b"supervised"
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        CellClient::local(Arc::clone(&fixture.registry), fixture.handle.clone())
            .query::<CountComments>(&fixture.target, None, ())
            .await
            .unwrap()
            .output,
        1
    );

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
