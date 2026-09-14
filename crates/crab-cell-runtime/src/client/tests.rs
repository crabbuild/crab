use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    CellClient, CellDescription, CellTransport, EncodedCommand, EncodedObservation, EncodedQuery,
    EncodedResolve, InvocationError,
};
use crate::{
    ApplicationId, BuildDescriptor, CatalogRole, CellModule, CellTarget, Command, CommandContext,
    CommandResult, Digest, Error, IncarnationId, MigrationDescriptor, ModuleDescriptor,
    MutationIdentity, NamespaceDescriptor, NamespaceId, OperationDescriptor, RegistryBuilder,
    RequestId, Resolution, StoredOutcome, TenantId,
};

const MODULE: &str = "pending-test";
const NAMESPACE: NamespaceId = NamespaceId::from_bytes([3; 16]);
const MIGRATION: &str = "CREATE TABLE pending_test(value BLOB NOT NULL)";

struct PendingCommand;

impl Command for PendingCommand {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        _context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        Ok(CommandResult::Success(input))
    }
}

struct PendingModule;

impl CellModule for PendingModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: MODULE,
            source_digest: Digest::from_bytes([4; 32]),
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: MIGRATION,
                digest: Digest::from_bytes(*blake3::hash(MIGRATION.as_bytes()).as_bytes()),
            }])),
            commands: &[OperationDescriptor {
                id: 1,
                codec_version: 1,
                schema_min: 1,
                schema_max: 1,
                input_limit: 64,
                output_limit: 64,
            }],
            queries: &[],
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

    fn register(self, registry: &mut RegistryBuilder) -> crate::Result<()> {
        registry.bind_command::<PendingCommand>()
    }
}

struct UnknownTransport {
    description: CellDescription,
    command_digest: Arc<Mutex<Option<Digest>>>,
    resolved_digest: Arc<Mutex<Option<Digest>>>,
}

impl CellTransport for UnknownTransport {
    fn describe(
        &self,
        _target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = crate::Result<CellDescription>> + Send + 'static>> {
        let description = self.description;
        Box::pin(async move { Ok(description) })
    }

    fn command(
        &self,
        command: EncodedCommand,
    ) -> Pin<Box<dyn Future<Output = crate::Result<StoredOutcome>> + Send + 'static>> {
        let observed = self.command_digest.clone();
        Box::pin(async move {
            *observed.lock().unwrap() = Some(command.operation_digest);
            Err(Error::OutcomeUnknown {
                request_id: command.identity.request_id,
                operation_digest: command.operation_digest,
                source: Box::new(Error::RuntimeClosed),
            })
        })
    }

    fn query(
        &self,
        _query: EncodedQuery,
    ) -> Pin<Box<dyn Future<Output = crate::Result<EncodedObservation>> + Send + 'static>> {
        Box::pin(async { Err(Error::Command("unexpected query")) })
    }

    fn resolve(
        &self,
        resolve: EncodedResolve,
    ) -> Pin<Box<dyn Future<Output = crate::Result<Resolution>> + Send + 'static>> {
        let observed = self.resolved_digest.clone();
        Box::pin(async move {
            *observed.lock().unwrap() = Some(resolve.operation_digest);
            Ok(Resolution::Unknown)
        })
    }
}

#[tokio::test]
async fn unknown_outcome_keeps_identity_and_digest_for_resolve() {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "pending-test".into(),
        cargo_lock_digest: Digest::from_bytes([5; 32]),
    });
    builder.register(PendingModule).unwrap();
    let registry = Arc::new(builder.finish().unwrap());
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NAMESPACE,
        b"pending",
    )
    .unwrap();
    let description = CellDescription {
        cell: target.cell_id(),
        incarnation: IncarnationId::from_bytes([6; 16]),
        code: registry.module_code(MODULE).unwrap(),
        schema: 1,
    };
    let command_digest = Arc::new(Mutex::new(None));
    let resolved_digest = Arc::new(Mutex::new(None));
    let client = CellClient::new(
        registry,
        Arc::new(UnknownTransport {
            description,
            command_digest: command_digest.clone(),
            resolved_digest: resolved_digest.clone(),
        }),
    );
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([7; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    };

    let pending = match client
        .command::<PendingCommand>(&target, identity, b"input".to_vec())
        .await
    {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("unexpected command outcome: {outcome:?}"),
    };
    assert_eq!(pending.identity(), identity);
    assert_eq!(
        Some(pending.operation_digest()),
        *command_digest.lock().unwrap()
    );
    assert_eq!(client.resolve(&pending).await.unwrap(), Resolution::Unknown);
    assert_eq!(
        Some(pending.operation_digest()),
        *resolved_digest.lock().unwrap()
    );
}
