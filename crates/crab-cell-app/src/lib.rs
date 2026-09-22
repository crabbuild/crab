//! Stable author-facing composition for statically linked Cell applications.
//!
//! This crate compiles the existing runtime registry together with a bounded,
//! deterministic topology descriptor. Node lifecycle, storage providers,
//! authority and HTTP policy remain outside this boundary.

use std::{marker::PhantomData, sync::Arc};

use crab_cell_runtime::{
    ApplicationId, BlobArtifactStore, BlobModule, BlobNamespace, BuildDescriptor, CatalogRole,
    CellClient, CellModule, CellTarget, Command, Committed, CronModule, CronNamespace, Digest,
    EffectModule, EffectSource, Error, InvocationError, KvModule, KvNamespace, NamespaceId,
    Observed, PendingMutation, PreparedCommand, Query, QueueModule, QueueNamespace, Registry,
    RegistryBuilder, Resolution, Result, SqlCell, SqlModule, TenantId, WorkflowActivities,
    WorkflowActivityModule, WorkflowModule, WorkflowNamespace, partition_for_shard,
};

const DESCRIPTOR_MAGIC: &[u8] = b"crab.application.v1\0";
const MAX_APPLICATION_NAME_BYTES: usize = 128;
const MAX_CELL_TYPES: usize = 128;
const MAX_DESCRIPTOR_BYTES: usize = 256 * 1024;
const MAX_PARTITION_VERSION: u32 = 1;

/// One application-owned Cell topology declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellType {
    module: &'static str,
    name: &'static str,
    namespace: NamespaceId,
    role: CatalogRole,
    shards: u32,
    partition_version: u32,
    schema_min: u32,
    schema_max: u32,
    database_limit_bytes: u64,
    capture_limit_bytes: u64,
}

impl CellType {
    /// Creates a bounded topology declaration with conservative version one defaults.
    pub fn new(
        module: &'static str,
        name: &'static str,
        namespace: NamespaceId,
        role: CatalogRole,
        shards: u32,
    ) -> Result<Self> {
        let cell_type = Self {
            module,
            name,
            namespace,
            role,
            shards,
            partition_version: 1,
            schema_min: 1,
            schema_max: 1,
            database_limit_bytes: 64 * 1024 * 1024,
            capture_limit_bytes: 16 * 1024 * 1024,
        };
        cell_type.validate()?;
        Ok(cell_type)
    }

    /// Replaces the declared application limits without changing stable identity.
    pub fn with_limits(
        mut self,
        database_limit_bytes: u64,
        capture_limit_bytes: u64,
    ) -> Result<Self> {
        self.database_limit_bytes = database_limit_bytes;
        self.capture_limit_bytes = capture_limit_bytes;
        self.validate()?;
        Ok(self)
    }

    /// Replaces the schema range while retaining the stable Cell identity.
    pub fn with_schema_range(mut self, schema_min: u32, schema_max: u32) -> Result<Self> {
        self.schema_min = schema_min;
        self.schema_max = schema_max;
        self.validate()?;
        Ok(self)
    }

    /// Returns the stable module owner.
    #[must_use]
    pub const fn module(&self) -> &'static str {
        self.module
    }

    /// Returns the diagnostic Cell type name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the stable namespace identity.
    #[must_use]
    pub const fn namespace(&self) -> NamespaceId {
        self.namespace
    }

    /// Returns the catalog role pinned for this type.
    #[must_use]
    pub const fn role(&self) -> CatalogRole {
        self.role
    }

    /// Returns the fixed shard count.
    #[must_use]
    pub const fn shards(&self) -> u32 {
        self.shards
    }

    /// Maps one bounded application scope to its stable shard number.
    pub fn shard_for_scope(&self, scope: &[u8]) -> Result<u32> {
        crab_cell_runtime::shard_for_scope(self.namespace, scope, self.shards)
    }

    /// Returns the canonical partition bytes for one application scope.
    pub fn partition_for_scope(&self, scope: &[u8]) -> Result<[u8; 4]> {
        Ok(crab_cell_runtime::partition_for_shard(
            self.shard_for_scope(scope)?,
        ))
    }

    fn validate(&self) -> Result<()> {
        if !valid_name(self.module)
            || !valid_name(self.name)
            || self.namespace.as_bytes().iter().all(|byte| *byte == 0)
            || !(1..=4096).contains(&self.shards)
            || !self.shards.is_power_of_two()
            || self.partition_version == 0
            || self.partition_version > MAX_PARTITION_VERSION
            || self.schema_min == 0
            || self.schema_min > self.schema_max
            || self.database_limit_bytes == 0
            || self.capture_limit_bytes == 0
        {
            return Err(Error::Registry("invalid Cell type declaration"));
        }
        Ok(())
    }
}

/// Startup-only compiler for one static application.
pub struct ApplicationBuilder {
    name: &'static str,
    registry: RegistryBuilder,
    cell_types: Vec<CellType>,
}

impl ApplicationBuilder {
    /// Creates an application compiler from the same build evidence as the runtime registry.
    pub fn new(name: &'static str, build: BuildDescriptor) -> Result<Self> {
        if !valid_name(name) {
            return Err(Error::Registry("invalid application name"));
        }
        Ok(Self {
            name,
            registry: RegistryBuilder::new(build),
            cell_types: Vec::new(),
        })
    }

    /// Registers one statically linked runtime module.
    pub fn register<M: CellModule>(&mut self, module: M) -> Result<()> {
        self.registry.register(module)
    }

    /// Adds one topology declaration and rejects duplicate stable identities.
    pub fn cell_type(&mut self, cell_type: CellType) -> Result<()> {
        cell_type.validate()?;
        if self.cell_types.iter().any(|existing| {
            existing.namespace == cell_type.namespace || existing.name == cell_type.name
        }) {
            return Err(Error::Registry("duplicate Cell type identity"));
        }
        self.cell_types.push(cell_type);
        Ok(())
    }

    /// Freezes the registry and emits deterministic application descriptor bytes.
    pub fn finish(mut self) -> Result<CompiledApplication> {
        if self.cell_types.is_empty() || self.cell_types.len() > MAX_CELL_TYPES {
            return Err(Error::Registry("Cell type count must be in 1..=128"));
        }
        self.cell_types
            .sort_by_key(|cell_type| *cell_type.namespace.as_bytes());
        let registry = Arc::new(self.registry.finish()?);
        if registry.namespace_count() != self.cell_types.len() {
            return Err(Error::Registry(
                "application topology does not declare every registry namespace",
            ));
        }
        for cell_type in &self.cell_types {
            let Some((module, namespace)) = registry.namespace_contract(cell_type.namespace) else {
                return Err(Error::Registry("Cell type namespace is not registered"));
            };
            if module != cell_type.module
                || namespace.role != cell_type.role
                || namespace.shards != cell_type.shards
            {
                return Err(Error::Registry("Cell type differs from compiled namespace"));
            }
            if registry.module_schema_range(cell_type.module)
                != Some((cell_type.schema_min, cell_type.schema_max))
            {
                return Err(Error::Registry(
                    "Cell type schema range differs from compiled module",
                ));
            }
        }
        let bytes = encode_descriptor(self.name, &registry, &self.cell_types)?;
        let digest = Digest::from_bytes(*blake3::hash(&bytes).as_bytes());
        Ok(CompiledApplication {
            registry,
            descriptor: bytes,
            digest,
            name: self.name,
            cell_types: self.cell_types,
        })
    }
}

/// Immutable application artifact consumed by node composition.
pub struct CompiledApplication {
    registry: Arc<Registry>,
    descriptor: Vec<u8>,
    digest: Digest,
    name: &'static str,
    cell_types: Vec<CellType>,
}

impl CompiledApplication {
    /// Returns the runtime registry used by every application invocation.
    #[must_use]
    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    /// Returns canonical topology and release descriptor bytes.
    #[must_use]
    pub fn descriptor_bytes(&self) -> &[u8] {
        &self.descriptor
    }

    /// Returns the digest of [`Self::descriptor_bytes`].
    #[must_use]
    pub const fn descriptor_digest(&self) -> Digest {
        self.digest
    }

    /// Returns the stable diagnostic application name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the sorted topology inventory.
    #[must_use]
    pub fn cell_types(&self) -> &[CellType] {
        &self.cell_types
    }

    fn validate_module(&self, namespace: NamespaceId, module: &'static str) -> Result<()> {
        let Some(cell_type) = self
            .cell_types
            .iter()
            .find(|cell_type| cell_type.namespace == namespace)
        else {
            return Err(Error::Registry("namespace is not declared by application"));
        };
        let Some((registered_module, _)) = self.registry.namespace_contract(namespace) else {
            return Err(Error::Registry("namespace contract is missing"));
        };
        if registered_module != module {
            return Err(Error::Registry("namespace module differs from capability"));
        }
        if cell_type.module != module {
            return Err(Error::Registry("Cell type module differs from capability"));
        }
        Ok(())
    }
}

/// Trait implemented by a statically linked application root.
pub trait CellApplication: Send + Sync + 'static {
    const NAME: &'static str;

    fn register(builder: &mut ApplicationBuilder) -> Result<()>;

    /// Compiles this application using deterministic build evidence.
    fn compile(build: BuildDescriptor) -> Result<CompiledApplication> {
        let mut builder = ApplicationBuilder::new(Self::NAME, build)?;
        Self::register(&mut builder)?;
        builder.finish()
    }
}

/// Tenant/application-bound typed capability over an already-started client.
pub struct ApplicationHandle<A> {
    client: CellClient,
    tenant: TenantId,
    application: ApplicationId,
    compiled: Arc<CompiledApplication>,
    marker: PhantomData<fn() -> A>,
}

impl<A> Clone for ApplicationHandle<A> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            tenant: self.tenant,
            application: self.application,
            compiled: Arc::clone(&self.compiled),
            marker: PhantomData,
        }
    }
}

impl<A> ApplicationHandle<A> {
    /// Binds a compiled application to one tenant and application identity.
    #[must_use]
    pub fn new(
        client: CellClient,
        compiled: Arc<CompiledApplication>,
        tenant: TenantId,
        application: ApplicationId,
    ) -> Self {
        Self {
            client,
            tenant,
            application,
            compiled,
            marker: PhantomData,
        }
    }

    /// Returns a handle whose Blob capability uses the configured object store.
    #[must_use]
    pub fn with_blob_artifact_store(&self, store: BlobArtifactStore) -> Self {
        let mut handle = self.clone();
        handle.client = handle.client.with_blob_artifact_store(store);
        handle
    }

    /// Returns the immutable compiled artifact.
    #[must_use]
    pub fn compiled(&self) -> &CompiledApplication {
        &self.compiled
    }

    /// Executes one statically typed command after enforcing application scope.
    pub async fn command<C: Command>(
        &self,
        target: &CellTarget,
        identity: crab_cell_runtime::MutationIdentity,
        input: C::Input,
    ) -> std::result::Result<Committed<C::Output>, InvocationError<C::Output>> {
        if let Err(error) = self.validate_target_module(target, C::MODULE) {
            return Err(InvocationError::NotStarted(error));
        }
        self.client.command::<C>(target, identity, input).await
    }

    /// Prepares a scoped typed command whose evidence can be resolved after cancellation.
    ///
    /// Rejects a target outside this application before preparing the mutation.
    pub async fn prepare_command<C: Command>(
        &self,
        target: &CellTarget,
        identity: crab_cell_runtime::MutationIdentity,
        input: C::Input,
    ) -> std::result::Result<PreparedCommand<C>, InvocationError<C::Output>> {
        self.validate_target_module(target, C::MODULE)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .prepare_command::<C>(target, identity, input)
            .await
    }

    /// Executes one statically typed query after enforcing application scope.
    pub async fn query<Q: Query>(
        &self,
        target: &CellTarget,
        minimum: Option<crab_cell_runtime::Receipt>,
        input: Q::Input,
    ) -> std::result::Result<Observed<Q::Output>, InvocationError<Q::Output>> {
        if let Err(error) = self.validate_target_module(target, Q::MODULE) {
            return Err(InvocationError::NotStarted(error));
        }
        self.client.query::<Q>(target, minimum, input).await
    }

    /// Resolves a pending command against the current owner after checking its application scope.
    ///
    /// The caller must keep the pending mutation from its original typed invocation;
    /// resolution can remain unknown until the owner recovers or the identity expires.
    pub async fn resolve(
        &self,
        pending: &PendingMutation,
    ) -> std::result::Result<Resolution, InvocationError<Vec<u8>>> {
        self.validate_target(pending.target())
            .map_err(InvocationError::NotStarted)?;
        self.client.resolve(pending).await
    }

    /// Returns the typed KV capability for a compiled KV module.
    pub fn kv<M: KvModule>(&self, namespace: NamespaceId) -> Result<KvNamespace<M>> {
        self.validate_namespace(namespace, M::MODULE, CatalogRole::Kv)?;
        KvNamespace::new(
            self.client.clone(),
            self.tenant,
            self.application,
            namespace,
        )
    }

    /// Returns the typed SQL capability for one explicitly selected Cell.
    pub fn sql<M: SqlModule>(&self, target: CellTarget) -> Result<SqlCell<M>> {
        self.validate_target(&target)?;
        self.validate_namespace(target.namespace(), M::MODULE, CatalogRole::Sql)?;
        SqlCell::new(self.client.clone(), target)
    }

    /// Returns the typed Blob capability for a compiled Blob module.
    pub fn blob<M: BlobModule>(&self) -> Result<BlobNamespace<M>> {
        self.validate_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Blob)?;
        BlobNamespace::new(self.client.clone(), self.tenant, self.application)
    }

    /// Returns the typed Queue capability for a compiled Queue module.
    pub fn queue<M: QueueModule>(&self) -> Result<QueueNamespace<M>> {
        self.validate_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Queue)?;
        QueueNamespace::new(self.client.clone(), self.tenant, self.application)
    }

    /// Returns the typed Cron capability for a compiled Cron module.
    pub fn cron<M: CronModule>(&self) -> Result<CronNamespace<M>> {
        self.validate_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Cron)?;
        CronNamespace::new(self.client.clone(), self.tenant, self.application)
    }

    /// Returns the typed Workflow capability for a compiled Workflow module.
    pub fn workflow<M: WorkflowModule>(&self) -> Result<WorkflowNamespace<M>> {
        self.validate_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Workflow)?;
        WorkflowNamespace::new(self.client.clone(), self.tenant, self.application)
    }

    /// Returns the native activity capability for one compiled Workflow module.
    pub fn activities<M: WorkflowActivityModule>(&self) -> Result<WorkflowActivities<M>> {
        self.validate_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Workflow)?;
        if !self.compiled.registry.has_activity_runner(M::NAMESPACE) {
            return Err(Error::Registry("activity runner is not registered"));
        }
        WorkflowActivities::new(self.client.clone(), self.tenant, self.application)
    }

    /// Returns the source effect capability for one explicitly selected Cell.
    ///
    /// Effects are emitted by commands through [`CommandContext::emit_effect`];
    /// this capability is for claiming and acknowledging the resulting source
    /// ledger. The destination still owns external idempotency.
    pub fn effects<M: EffectModule>(&self, target: CellTarget) -> Result<EffectSource<M>> {
        self.validate_target(&target)?;
        self.compiled
            .validate_module(target.namespace(), M::MODULE)?;
        if !self.compiled.registry.has_effect_runner(target.namespace()) {
            return Err(Error::Registry("effect runner is not registered"));
        }
        self.client.effect_source::<M>(target)
    }

    fn validate_target(&self, target: &CellTarget) -> Result<()> {
        if target.tenant() != self.tenant || target.application() != self.application {
            return Err(Error::Identity("Cell target is outside application scope"));
        }
        let Some(cell_type) = self
            .compiled
            .cell_types
            .iter()
            .find(|cell_type| cell_type.namespace == target.namespace())
        else {
            return Err(Error::Registry("namespace is not declared by application"));
        };
        let shard = target
            .partition()
            .try_into()
            .map(u32::from_be_bytes)
            .map_err(|_| Error::Identity("Cell target partition is not canonical"))?;
        if shard >= cell_type.shards || partition_for_shard(shard).as_slice() != target.partition()
        {
            return Err(Error::Identity(
                "Cell target partition is outside the declared shard range",
            ));
        }
        Ok(())
    }

    fn validate_target_module(&self, target: &CellTarget, module: &'static str) -> Result<()> {
        self.validate_target(target)?;
        self.compiled.validate_module(target.namespace(), module)
    }

    fn validate_namespace(
        &self,
        namespace: NamespaceId,
        module: &'static str,
        role: CatalogRole,
    ) -> Result<()> {
        let Some(cell_type) = self
            .compiled
            .cell_types
            .iter()
            .find(|cell_type| cell_type.namespace == namespace)
        else {
            return Err(Error::Registry("namespace is not declared by application"));
        };
        if cell_type.role != role {
            return Err(Error::Registry("namespace role differs from capability"));
        }
        self.compiled.validate_module(namespace, module)?;
        let Some((_, contract)) = self.compiled.registry.namespace_contract(namespace) else {
            return Err(Error::Registry("namespace contract is missing"));
        };
        if contract.role != role {
            return Err(Error::Registry("namespace module differs from capability"));
        }
        Ok(())
    }
}

fn encode_descriptor(name: &str, registry: &Registry, cell_types: &[CellType]) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(128);
    bytes.extend_from_slice(DESCRIPTOR_MAGIC);
    push_text(&mut bytes, name)?;
    bytes.extend_from_slice(registry.release_digest().as_bytes());
    bytes.extend_from_slice(&(cell_types.len() as u16).to_be_bytes());
    for cell_type in cell_types {
        push_text(&mut bytes, cell_type.module)?;
        push_text(&mut bytes, cell_type.name)?;
        bytes.extend_from_slice(cell_type.namespace.as_bytes());
        bytes.push(role_code(cell_type.role));
        bytes.extend_from_slice(&cell_type.shards.to_be_bytes());
        bytes.extend_from_slice(&cell_type.partition_version.to_be_bytes());
        bytes.extend_from_slice(&cell_type.schema_min.to_be_bytes());
        bytes.extend_from_slice(&cell_type.schema_max.to_be_bytes());
        bytes.extend_from_slice(&cell_type.database_limit_bytes.to_be_bytes());
        bytes.extend_from_slice(&cell_type.capture_limit_bytes.to_be_bytes());
    }
    if bytes.len() > MAX_DESCRIPTOR_BYTES {
        return Err(Error::Registry("application descriptor exceeds 256 KiB"));
    }
    Ok(bytes)
}

fn push_text(bytes: &mut Vec<u8>, value: &str) -> Result<()> {
    if value.len() > MAX_APPLICATION_NAME_BYTES {
        return Err(Error::Registry(
            "application descriptor text exceeds 128 bytes",
        ));
    }
    let length = u16::try_from(value.len())
        .map_err(|_| Error::Registry("application descriptor text length overflow"))?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_APPLICATION_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn role_code(role: CatalogRole) -> u8 {
    match role {
        CatalogRole::Repository => 0,
        CatalogRole::Sql => 1,
        CatalogRole::Kv => 2,
        CatalogRole::Queue => 3,
        CatalogRole::Workflow => 4,
        CatalogRole::Blob => 5,
        CatalogRole::Cron => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_cell_runtime::{CellModule, Digest, ModuleDescriptor, NamespaceDescriptor};

    struct SqlModule;

    impl CellModule for SqlModule {
        const NAME: &'static str = "app-sql";

        fn descriptor(&self) -> &'static ModuleDescriptor {
            static DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
                name: "app-sql",
                source_digest: Digest::from_bytes([1; 32]),
                retained_codes: &[],
                schema_min: 1,
                schema_max: 1,
                migrations: &[crab_cell_runtime::MigrationDescriptor {
                    version: 1,
                    sql: "-- app-sql migration v1",
                    digest: Digest::from_bytes([
                        0x43, 0x1d, 0x99, 0x74, 0x5c, 0x8f, 0x39, 0x00, 0x9d, 0x2e, 0xe2, 0x00,
                        0x7c, 0x75, 0x42, 0xe3, 0xa5, 0xf0, 0x0c, 0x92, 0x21, 0x8b, 0xf7, 0x3b,
                        0x8f, 0x6f, 0x6b, 0xc3, 0x38, 0xe4, 0xf5, 0xca,
                    ]),
                }],
                commands: &[],
                queries: &[],
                workflow_definitions: &[],
                activity_types: &[],
                namespaces: &[
                    NamespaceDescriptor {
                        id: NamespaceId::from_bytes([2; 16]),
                        name: "app-sql",
                        role: CatalogRole::Sql,
                        shards: 1,
                        effect_targets: &[],
                        dead_letter: None,
                    },
                    NamespaceDescriptor {
                        id: NamespaceId::from_bytes([3; 16]),
                        name: "app-sql-2",
                        role: CatalogRole::Sql,
                        shards: 1,
                        effect_targets: &[],
                        dead_letter: None,
                    },
                ],
            };
            &DESCRIPTOR
        }

        fn register(self, _registry: &mut RegistryBuilder) -> Result<()> {
            Ok(())
        }
    }

    fn build(reverse: bool) -> CompiledApplication {
        let mut builder = ApplicationBuilder::new(
            "app",
            BuildDescriptor {
                source_revision: "source".into(),
                cargo_lock_digest: Digest::from_bytes([9; 32]),
            },
        )
        .unwrap();
        builder.register(SqlModule).unwrap();
        let first = CellType::new(
            "app-sql",
            "orders",
            NamespaceId::from_bytes([2; 16]),
            CatalogRole::Sql,
            1,
        )
        .unwrap();
        let second = CellType::new(
            "app-sql",
            "inventory",
            NamespaceId::from_bytes([3; 16]),
            CatalogRole::Sql,
            1,
        )
        .unwrap();
        if reverse {
            builder.cell_type(second).unwrap();
            builder.cell_type(first).unwrap();
        } else {
            builder.cell_type(first).unwrap();
            builder.cell_type(second).unwrap();
        }
        builder.finish().unwrap()
    }

    #[test]
    fn descriptor_is_stable_and_scope_is_checked() {
        let first = build(false);
        let second = build(true);
        assert_eq!(first.descriptor_bytes(), second.descriptor_bytes());
        assert_eq!(first.descriptor_digest(), second.descriptor_digest());
        assert_eq!(
            first.registry().release_digest(),
            first.registry().release_digest()
        );
    }

    #[test]
    fn compiled_application_rejects_cross_module_invocation_targets() {
        let application = build(false);
        let sql_namespace = NamespaceId::from_bytes([2; 16]);

        assert!(
            application
                .validate_module(sql_namespace, "app-sql")
                .is_ok()
        );
        assert!(
            application
                .validate_module(sql_namespace, "unrelated-module")
                .is_err()
        );
        assert!(
            application
                .validate_module(NamespaceId::from_bytes([99; 16]), "app-sql")
                .is_err()
        );
    }

    #[test]
    fn duplicate_cell_namespace_and_mismatched_module_fail_closed() {
        let mut builder = ApplicationBuilder::new(
            "app",
            BuildDescriptor {
                source_revision: "source".into(),
                cargo_lock_digest: Digest::from_bytes([9; 32]),
            },
        )
        .unwrap();
        let cell_type = CellType::new(
            "app-sql",
            "orders",
            NamespaceId::from_bytes([2; 16]),
            CatalogRole::Sql,
            1,
        )
        .unwrap();
        builder.cell_type(cell_type).unwrap();
        assert!(builder.cell_type(cell_type).is_err());
        builder.register(SqlModule).unwrap();
        let mut wrong = ApplicationBuilder::new(
            "app",
            BuildDescriptor {
                source_revision: "source".into(),
                cargo_lock_digest: Digest::from_bytes([9; 32]),
            },
        )
        .unwrap();
        wrong
            .cell_type(
                CellType::new(
                    "other",
                    "orders",
                    NamespaceId::from_bytes([2; 16]),
                    CatalogRole::Sql,
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        wrong.register(SqlModule).unwrap();
        assert!(wrong.finish().is_err());
    }

    #[test]
    fn every_registered_namespace_requires_a_cell_type() {
        let mut builder = ApplicationBuilder::new(
            "app",
            BuildDescriptor {
                source_revision: "source".into(),
                cargo_lock_digest: Digest::from_bytes([9; 32]),
            },
        )
        .unwrap();
        builder.register(SqlModule).unwrap();
        builder
            .cell_type(
                CellType::new(
                    "app-sql",
                    "orders",
                    NamespaceId::from_bytes([2; 16]),
                    CatalogRole::Sql,
                    1,
                )
                .unwrap(),
            )
            .unwrap();

        assert!(builder.finish().is_err());
    }

    #[test]
    fn cell_type_schema_range_must_match_compiled_module() {
        let mut builder = ApplicationBuilder::new(
            "app",
            BuildDescriptor {
                source_revision: "source".into(),
                cargo_lock_digest: Digest::from_bytes([9; 32]),
            },
        )
        .unwrap();
        builder.register(SqlModule).unwrap();
        builder
            .cell_type(
                CellType::new(
                    "app-sql",
                    "orders",
                    NamespaceId::from_bytes([2; 16]),
                    CatalogRole::Sql,
                    1,
                )
                .unwrap()
                .with_schema_range(1, 2)
                .unwrap(),
            )
            .unwrap();

        assert!(builder.finish().is_err());
    }

    #[test]
    fn duplicate_cell_type_name_fails_closed() {
        let mut builder = ApplicationBuilder::new(
            "app",
            BuildDescriptor {
                source_revision: "source".into(),
                cargo_lock_digest: Digest::from_bytes([9; 32]),
            },
        )
        .unwrap();
        let first = CellType::new(
            "app-sql",
            "orders",
            NamespaceId::from_bytes([2; 16]),
            CatalogRole::Sql,
            1,
        )
        .unwrap();
        let second = CellType::new(
            "app-sql",
            "orders",
            NamespaceId::from_bytes([3; 16]),
            CatalogRole::Sql,
            1,
        )
        .unwrap();
        builder.cell_type(first).unwrap();
        assert!(builder.cell_type(second).is_err());
    }

    #[test]
    fn cell_type_limits_and_partition_bounds_fail_closed() {
        for shards in [0, 3, 8_192] {
            assert!(
                CellType::new(
                    "app-sql",
                    "orders",
                    NamespaceId::from_bytes([2; 16]),
                    CatalogRole::Sql,
                    shards,
                )
                .is_err()
            );
        }
        assert!(
            CellType::new(
                "app-sql",
                "orders",
                NamespaceId::from_bytes([0; 16]),
                CatalogRole::Sql,
                1,
            )
            .is_err()
        );

        let cell_type = CellType::new(
            "app-sql",
            "orders",
            NamespaceId::from_bytes([2; 16]),
            CatalogRole::Sql,
            1,
        )
        .unwrap();
        assert!(cell_type.with_limits(0, 1).is_err());
        assert!(cell_type.with_limits(1, 0).is_err());
        assert!(cell_type.with_schema_range(0, 1).is_err());
        assert!(cell_type.with_schema_range(2, 1).is_err());
    }
}
