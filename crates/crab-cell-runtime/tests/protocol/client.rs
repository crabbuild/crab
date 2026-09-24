use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

use crab_cell_runtime::cell::actor::CellRuntime;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::catalog::{CatalogEntry, CatalogProof};
use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::cell::executor::Resolution;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::client::{CellDescription, Receipt, command_operation_digest};
use crab_cell_runtime::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};
use crab_cell_runtime::control::Owner;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::fleet::telemetry::{
    CellTelemetry, PrimitiveOperationKind, PrimitiveOperationOutcome,
};
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_cell_runtime::identity::{IncarnationId, RequestId};
use crab_cell_runtime::peer::wire;
use crab_cell_runtime::peer::{
    EffectPeerClient, PeerAuthorizer, PeerCellResolver, PeerDispatcher, PeerPrincipal,
    PeerRoundTrip, PeerSigner, PeerVerifier, VerifiedPeerRequest,
};
use crab_cell_runtime::primitives::effects::{
    EffectClaim, EffectClaimRequest, EffectCommandIntent, EffectLeaseOutcome, EffectRunOutcome,
    effect_id, effect_operation_digest, register_effect_delivery,
};
use crab_cell_runtime::primitives::effects::{EffectModule, EffectSource};
use crab_cell_runtime::primitives::sql::{SqlBatch, SqlStatement, SqlValue};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, Command, ModuleDescriptor, NamespaceDescriptor, Query, Registry,
    RegistryBuilder,
};
use crab_cell_runtime::registry::{
    CommandContext, CommandResult, MigrationDescriptor, OperationDescriptor, QueryContext,
};
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Limits};
use crab_storage::Store;
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
    OperationDescriptor {
        id: 6,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 64,
        output_limit: 64,
    },
    OperationDescriptor {
        id: 7,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 64,
        output_limit: 64,
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
    OperationDescriptor {
        id: 3,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 36,
        output_limit: 1 << 20,
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
        registry.bind_command::<EmitEffectComment>()?;
        registry.bind_command::<EmitUndeclaredEffect>()?;
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
    const STATUS_QUERY_ID: u32 = 3;
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

struct EmitEffectComment;

impl Command for EmitEffectComment {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 6;
    const CODEC_VERSION: u32 = 1;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        let expires_at_ms = context
            .now_ms()
            .checked_add(60_000)
            .ok_or(crab_cell_runtime::Error::Command("effect expiry overflow"))?;
        let effect_id = context.emit_effect(&EffectCommandIntent {
            target: context.target().clone(),
            command_id: CreateComment::ID,
            codec_version: CreateComment::CODEC_VERSION,
            input,
            expires_at_ms,
        })?;
        Ok(CommandResult::Success(effect_id.to_vec()))
    }
}

struct EmitUndeclaredEffect;

impl Command for EmitUndeclaredEffect {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        let target = CellTarget::new(
            context.target().tenant(),
            context.target().application(),
            NamespaceId::from_bytes([99; 16]),
            context.target().partition(),
        )?;
        context.emit_effect(&EffectCommandIntent {
            target,
            command_id: CreateComment::ID,
            codec_version: CreateComment::CODEC_VERSION,
            input,
            expires_at_ms: context
                .now_ms()
                .checked_add(60_000)
                .ok_or(crab_cell_runtime::Error::Command("effect expiry overflow"))?,
        })?;
        Ok(CommandResult::Success(Vec::new()))
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
        retained_codes: &[],
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
            effect_targets: &[NAMESPACE],
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
    layout: CellStorageLayout,
    replica: CellReplica,
    authority: CellAuthority,
    proof: CatalogProof,
    runtime: Option<CellRuntime>,
    session: SessionId,
    incarnation: IncarnationId,
    target: CellTarget,
    handle: Option<crab_cell_runtime::cell::actor::CellHandle>,
    registry: Arc<Registry>,
}

impl Fixture {
    fn handle(&self) -> &crab_cell_runtime::cell::actor::CellHandle {
        self.handle.as_ref().expect("fixture handle is present")
    }

    fn take_handle(&mut self) -> crab_cell_runtime::cell::actor::CellHandle {
        self.handle.take().expect("fixture handle is present")
    }
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
    let catalog =
        crab_cell_runtime::cell::catalog::CellCatalog::new(layout.clone(), target.tenant());
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
    let authority = CellAuthority::new(layout.clone());
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
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 4).unwrap(), 4 * 1024 * 1024, session).unwrap();
    let handle = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
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
        layout,
        replica,
        authority,
        proof,
        runtime: Some(runtime),
        session,
        incarnation,
        target,
        handle: Some(handle),
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
    handle: crab_cell_runtime::cell::actor::CellHandle,
}

impl PeerCellResolver for LocalResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = crab_cell_runtime::Result<crab_cell_runtime::cell::actor::CellHandle>,
                > + Send
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

mod effects;
mod telemetry;
mod typed;
