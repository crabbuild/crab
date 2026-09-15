use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    future::Future,
    pin::Pin,
};

use crab_ltx::rusqlite::{Connection, Transaction};

mod descriptor;

use descriptor::encode_release;

use crate::{
    ActivityContext, ActivityExecution, ActivityHandler, ActivitySupport, CatalogRole, CellId,
    CellTarget, Digest, Error, HandlerOutcome, NamespaceId, Result, SqlBatch, SqlResultSet,
    WireValue, WorkflowDefinition,
    codec::{decode_wire, encode_wire},
    sql_batch, sql_query_batch,
};

const MAX_DESCRIPTOR_BYTES: usize = 256 * 1024;
const MAX_MODULES: usize = 128;
const MAX_NAMESPACES: usize = 128;
const MAX_OPERATION_BYTES: u32 = 1024 * 1024;

/// Registry construction error returned before server readiness.
pub type RegistryError = Error;

/// Build evidence included in the canonical release descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildDescriptor {
    pub source_revision: String,
    pub cargo_lock_digest: Digest,
}

/// One exact application migration and its independently verified digest.
#[derive(Clone, Copy, Debug)]
pub struct MigrationDescriptor {
    pub version: u32,
    pub sql: &'static str,
    pub digest: Digest,
}

/// One registered command or query codec and its schema compatibility range.
#[derive(Clone, Copy, Debug)]
pub struct OperationDescriptor {
    pub id: u32,
    pub codec_version: u32,
    pub schema_min: u32,
    pub schema_max: u32,
    pub input_limit: u32,
    pub output_limit: u32,
}

/// One stable Cell namespace compiled into a module.
#[derive(Clone, Copy, Debug)]
pub struct NamespaceDescriptor {
    pub id: NamespaceId,
    pub name: &'static str,
    pub role: CatalogRole,
    pub shards: u32,
    pub effect_targets: &'static [NamespaceId],
    pub dead_letter: Option<NamespaceId>,
}

/// Static module inventory that must match its compiled function bindings.
#[derive(Clone, Copy, Debug)]
pub struct ModuleDescriptor {
    pub name: &'static str,
    pub source_digest: Digest,
    pub schema_min: u32,
    pub schema_max: u32,
    pub migrations: &'static [MigrationDescriptor],
    pub commands: &'static [OperationDescriptor],
    pub queries: &'static [OperationDescriptor],
    pub workflow_definitions: &'static [Digest],
    pub activity_types: &'static [&'static str],
    pub namespaces: &'static [NamespaceDescriptor],
}

/// Synchronous application command context with no raw transaction accessor.
pub struct CommandContext<'borrow, 'connection> {
    transaction: &'borrow Transaction<'connection>,
    target: CellTarget,
    sequence: u64,
    now_ms: i64,
    input_limit: u32,
    output_limit: u32,
}

impl CommandContext<'_, '_> {
    #[must_use]
    pub fn cell_id(&self) -> CellId {
        self.target.cell_id()
    }

    /// Returns the runtime-validated target for deterministic cross-Cell routing.
    #[must_use]
    pub const fn target(&self) -> &CellTarget {
        &self.target
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// Creates the one command-scoped effect identity allocator.
    pub fn effect_batch(&self) -> Result<crate::EffectBatch> {
        crate::EffectBatch::new(self.transaction, &self.target, self.sequence, self.now_ms)
    }

    /// Executes bounded application SQL under the runtime authorizer.
    pub fn sql(&self, batch: &SqlBatch) -> Result<Vec<SqlResultSet>> {
        sql_batch(self.transaction, batch)
    }

    pub(crate) const fn primitive_transaction(&self) -> &Transaction<'_> {
        self.transaction
    }
}

/// Read-only application query context with no raw connection accessor.
pub struct QueryContext<'borrow> {
    connection: &'borrow Connection,
    cell: CellId,
    commit_sequence: u64,
    now_ms: i64,
    input_limit: u32,
    output_limit: u32,
}

impl QueryContext<'_> {
    #[must_use]
    pub const fn cell_id(&self) -> CellId {
        self.cell
    }

    #[must_use]
    pub const fn commit_sequence(&self) -> u64 {
        self.commit_sequence
    }

    #[must_use]
    pub const fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// Executes bounded read-only application SQL under the runtime authorizer.
    pub fn sql(&self, batch: &SqlBatch) -> Result<Vec<SqlResultSet>> {
        sql_query_batch(self.connection, batch)
    }

    pub(crate) const fn primitive_connection(&self) -> &Connection {
        self.connection
    }
}

type CommandHandler = for<'borrow, 'connection> fn(
    &mut CommandContext<'borrow, 'connection>,
    &[u8],
) -> Result<HandlerOutcome>;

type QueryHandler = for<'borrow> fn(&mut QueryContext<'borrow>, &[u8]) -> Result<Vec<u8>>;
type ActivityFuture = Pin<Box<dyn Future<Output = Result<ActivityExecution>> + Send + 'static>>;
type ActivityFunction = fn(ActivityContext, Vec<u8>) -> ActivityFuture;

/// Stored command decision encoded with the command's declared output codec.
pub enum CommandResult<T> {
    Success(T),
    Rejected(T),
}

/// Statically dispatched typed command implemented by compiled Crab code.
pub trait Command: Send + Sync + 'static {
    const MODULE: &'static str;
    const ID: u32;
    const CODEC_VERSION: u32;
    type Input: WireValue;
    type Output: WireValue;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> Result<CommandResult<Self::Output>>;
}

/// Statically dispatched typed query implemented by compiled Crab code.
pub trait Query: Send + Sync + 'static {
    const MODULE: &'static str;
    const ID: u32;
    const CODEC_VERSION: u32;
    type Input: WireValue;
    type Output: WireValue;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> Result<Self::Output>;
}

/// Validated command selection and bounded input supplied by runtime routing.
pub struct CommandInvocation<'a> {
    pub module: &'a str,
    pub operation_id: u32,
    pub codec_version: u32,
    pub schema: u32,
    pub target: CellTarget,
    pub sequence: u64,
    pub now_ms: i64,
    pub input: &'a [u8],
}

/// Validated query selection and bounded input supplied by runtime routing.
pub struct QueryInvocation<'a> {
    pub module: &'a str,
    pub operation_id: u32,
    pub codec_version: u32,
    pub schema: u32,
    pub cell: CellId,
    pub commit_sequence: u64,
    pub now_ms: i64,
    pub input: &'a [u8],
}

/// Source-level module registration contract for statically linked Crab code.
pub trait CellModule: Send + Sync + 'static {
    const NAME: &'static str;

    fn descriptor(&self) -> &'static ModuleDescriptor;
    fn register(self, registry: &mut RegistryBuilder) -> std::result::Result<(), RegistryError>;
}

/// Mutable startup-only collector for descriptors and compiled bindings.
pub struct RegistryBuilder {
    build: BuildDescriptor,
    modules: Vec<&'static ModuleDescriptor>,
    commands: BTreeMap<BindingKey, CommandHandler>,
    queries: BTreeMap<BindingKey, QueryHandler>,
    workflow_definitions: HashMap<(String, [u8; 32]), Vec<NamespaceId>>,
    activities: BTreeMap<ActivityKey, ActivityFunction>,
    activity_claims: BTreeSet<ActivityKey>,
    queue_bindings: Vec<QueueBinding>,
    maintenance_bindings: BTreeMap<&'static str, Option<crate::QueueDeadLetterTarget>>,
}

impl RegistryBuilder {
    #[must_use]
    pub fn new(build: BuildDescriptor) -> Self {
        Self {
            build,
            modules: Vec::new(),
            commands: BTreeMap::new(),
            queries: BTreeMap::new(),
            workflow_definitions: HashMap::new(),
            activities: BTreeMap::new(),
            activity_claims: BTreeSet::new(),
            queue_bindings: Vec::new(),
            maintenance_bindings: BTreeMap::new(),
        }
    }

    /// Registers one static module and all bindings it contributes.
    pub fn register<M: CellModule>(&mut self, module: M) -> std::result::Result<(), RegistryError> {
        let descriptor = module.descriptor();
        if M::NAME != descriptor.name {
            return Err(Error::Registry(
                "module constant and descriptor name differ",
            ));
        }
        module.register(self)?;
        self.modules.push(descriptor);
        Ok(())
    }

    /// Binds one descriptor command key to its monomorphized typed function.
    pub fn bind_command<C: Command>(&mut self) -> std::result::Result<(), RegistryError> {
        let key = BindingKey::new(C::MODULE, C::ID, C::CODEC_VERSION)?;
        if self.commands.insert(key, typed_command::<C>).is_some() {
            return Err(Error::Registry("duplicate command binding"));
        }
        Ok(())
    }

    /// Binds one descriptor query key to its monomorphized typed function.
    pub fn bind_query<Q: Query>(&mut self) -> std::result::Result<(), RegistryError> {
        let key = BindingKey::new(Q::MODULE, Q::ID, Q::CODEC_VERSION)?;
        if self.queries.insert(key, typed_query::<Q>).is_some() {
            return Err(Error::Registry("duplicate query binding"));
        }
        Ok(())
    }

    pub(crate) fn bind_queue_module(
        &mut self,
        module: &'static str,
        namespace: NamespaceId,
        send_command_id: u32,
        codec_version: u32,
        dead_letter: Option<crate::QueueDeadLetterTarget>,
    ) -> Result<()> {
        if self
            .queue_bindings
            .iter()
            .any(|binding| binding.namespace == namespace)
        {
            return Err(Error::Registry("duplicate Queue module binding"));
        }
        self.queue_bindings.push(QueueBinding {
            module,
            namespace,
            send_command_id,
            codec_version,
            dead_letter,
        });
        Ok(())
    }

    pub(crate) fn bind_maintenance_module(
        &mut self,
        module: &'static str,
        queue_dead_letter: Option<crate::QueueDeadLetterTarget>,
    ) -> Result<()> {
        if self
            .maintenance_bindings
            .insert(module, queue_dead_letter)
            .is_some()
        {
            return Err(Error::Registry("duplicate maintenance module binding"));
        }
        Ok(())
    }

    /// Binds one descriptor digest to its statically linked transition function.
    pub fn bind_workflow_definition(
        &mut self,
        module: &'static str,
        definition: &'static dyn WorkflowDefinition,
    ) -> std::result::Result<(), RegistryError> {
        let digest = *definition.digest().as_bytes();
        let targets = definition.effect_targets();
        let unique_targets = targets.iter().copied().collect::<HashSet<_>>();
        if !valid_name(module)
            || digest.iter().all(|byte| *byte == 0)
            || unique_targets.len() != targets.len()
            || self
                .workflow_definitions
                .insert((module.to_owned(), digest), targets.to_vec())
                .is_some()
        {
            return Err(Error::Registry("invalid workflow definition binding"));
        }
        Ok(())
    }

    /// Binds one declared activity type to its statically linked Rust future.
    pub fn bind_activity<A: ActivityHandler>(
        &mut self,
        module: &'static str,
        definition: Digest,
    ) -> std::result::Result<(), RegistryError> {
        let key = ActivityKey::new(module, definition, A::TYPE)?;
        if self.activities.insert(key, typed_activity::<A>).is_some() {
            return Err(Error::Registry("duplicate activity binding"));
        }
        Ok(())
    }

    /// Binds the claim-time activity inventory to the same compiled definitions.
    pub fn bind_activity_inventory(
        &mut self,
        module: &'static str,
        definition: Digest,
        activity_types: &'static [&'static str],
    ) -> std::result::Result<(), RegistryError> {
        if activity_types.is_empty() {
            return Err(Error::Registry("activity claim inventory is empty"));
        }
        for activity in activity_types {
            if !self
                .activity_claims
                .insert(ActivityKey::new(module, definition, activity)?)
            {
                return Err(Error::Registry("duplicate activity claim binding"));
            }
        }
        Ok(())
    }

    /// Freezes registration after validating inventory and canonical bytes.
    pub fn finish(mut self) -> std::result::Result<Registry, RegistryError> {
        validate_build(&self.build)?;
        if self.modules.is_empty() || self.modules.len() > MAX_MODULES {
            return Err(Error::Registry("module count must be in 1..=128"));
        }
        self.modules.sort_by_key(|module| module.name);
        let mut module_names = HashSet::with_capacity(self.modules.len());
        let mut expected_commands = BTreeSet::new();
        let mut expected_queries = BTreeSet::new();
        let mut namespace_owners = HashMap::new();
        for module in &self.modules {
            if !module_names.insert(module.name) {
                return Err(Error::Registry("duplicate module name"));
            }
            validate_module(
                module,
                &mut expected_commands,
                &mut expected_queries,
                &mut namespace_owners,
            )?;
        }
        if expected_commands != self.commands.keys().cloned().collect()
            || expected_queries != self.queries.keys().cloned().collect()
        {
            return Err(Error::Registry("descriptor and function bindings differ"));
        }
        let expected_workflows = self
            .modules
            .iter()
            .flat_map(|module| {
                module
                    .workflow_definitions
                    .iter()
                    .map(|digest| (module.name.to_owned(), *digest.as_bytes()))
            })
            .collect::<HashSet<_>>();
        if expected_workflows
            != self
                .workflow_definitions
                .keys()
                .cloned()
                .collect::<HashSet<_>>()
        {
            return Err(Error::Registry(
                "descriptor and workflow definition bindings differ",
            ));
        }
        let expected_activities = self
            .modules
            .iter()
            .flat_map(|module| {
                module.workflow_definitions.iter().flat_map(|definition| {
                    module.activity_types.iter().map(|activity| ActivityKey {
                        module: module.name.to_owned(),
                        definition: *definition.as_bytes(),
                        activity: (*activity).to_owned(),
                    })
                })
            })
            .collect::<BTreeSet<_>>();
        if expected_activities != self.activities.keys().cloned().collect()
            || expected_activities != self.activity_claims
        {
            return Err(Error::Registry("descriptor and activity bindings differ"));
        }
        validate_namespaces(&namespace_owners)?;
        validate_workflow_effect_targets(
            &self.workflow_definitions,
            &namespace_owners,
            &self.modules,
        )?;
        validate_queue_bindings(&self.queue_bindings, &namespace_owners, &self.modules)?;
        validate_maintenance_bindings(&self.maintenance_bindings, &self.queue_bindings)?;

        let command_descriptors = operation_descriptors(&self.modules, |module| module.commands);
        let query_descriptors = operation_descriptors(&self.modules, |module| module.queries);
        let module_schemas = self
            .modules
            .iter()
            .map(|module| {
                (
                    module.name.to_owned(),
                    (module.schema_min, module.schema_max),
                )
            })
            .collect();

        let (release_bytes, module_codes) = encode_release(&self.build, &self.modules)?;
        if release_bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(Error::Registry("release descriptor exceeds 256 KiB"));
        }
        let release_digest = Digest::from_bytes(*blake3::hash(&release_bytes).as_bytes());
        Ok(Registry {
            release_bytes,
            release_digest,
            module_codes,
            module_schemas,
            commands: self.commands,
            command_descriptors,
            queries: self.queries,
            query_descriptors,
            namespace_modules: namespace_owners,
            activities: self.activities,
        })
    }
}

#[derive(Clone, Copy)]
struct QueueBinding {
    module: &'static str,
    namespace: NamespaceId,
    send_command_id: u32,
    codec_version: u32,
    dead_letter: Option<crate::QueueDeadLetterTarget>,
}

/// Immutable compiled registry shared by runtime and release inspection.
pub struct Registry {
    release_bytes: Vec<u8>,
    release_digest: Digest,
    module_codes: BTreeMap<String, Digest>,
    module_schemas: BTreeMap<String, (u32, u32)>,
    commands: BTreeMap<BindingKey, CommandHandler>,
    command_descriptors: BTreeMap<BindingKey, OperationDescriptor>,
    queries: BTreeMap<BindingKey, QueryHandler>,
    query_descriptors: BTreeMap<BindingKey, OperationDescriptor>,
    namespace_modules: HashMap<NamespaceId, (&'static str, NamespaceDescriptor)>,
    activities: BTreeMap<ActivityKey, ActivityFunction>,
}

impl Registry {
    #[must_use]
    pub fn release_bytes(&self) -> &[u8] {
        &self.release_bytes
    }

    #[must_use]
    pub const fn release_digest(&self) -> Digest {
        self.release_digest
    }

    #[must_use]
    pub fn module_code(&self, module: &str) -> Option<Digest> {
        self.module_codes.get(module).copied()
    }

    /// Returns the sorted module-code inventory advertised by eligible nodes.
    #[must_use]
    pub fn module_digests(&self) -> Vec<Digest> {
        let mut digests = self.module_codes.values().copied().collect::<Vec<_>>();
        digests.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        digests.dedup();
        digests
    }

    /// Reports whether this binary can execute one authoritative Cell pair.
    #[must_use]
    pub fn supports_cell(
        &self,
        namespace: NamespaceId,
        role: CatalogRole,
        code: Digest,
        schema: u32,
    ) -> bool {
        let Some((module, descriptor)) = self.namespace_modules.get(&namespace) else {
            return false;
        };
        let Some((schema_min, schema_max)) = self.module_schemas.get(*module) else {
            return false;
        };
        descriptor.role == role
            && self.module_codes.get(*module) == Some(&code)
            && (*schema_min..=*schema_max).contains(&schema)
    }

    pub(crate) fn activity_support(
        &self,
        module: &'static str,
        definition: Digest,
    ) -> Result<Vec<ActivitySupport>> {
        let supported = self
            .activities
            .keys()
            .filter(|key| key.module == module && key.definition == *definition.as_bytes())
            .map(|key| ActivitySupport {
                activity_type: key.activity.clone(),
                definition_digest: Digest::from_bytes(key.definition),
            })
            .collect::<Vec<_>>();
        if supported.is_empty() {
            return Err(Error::Registry("activity support is unavailable"));
        }
        Ok(supported)
    }

    pub(crate) fn execute_activity(
        &self,
        module: &'static str,
        definition: Digest,
        activity: &str,
        context: ActivityContext,
        input: Vec<u8>,
    ) -> ActivityFuture {
        let key = match ActivityKey::new(module, definition, activity) {
            Ok(key) => key,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let Some(handler) = self.activities.get(&key).copied() else {
            return Box::pin(async { Err(Error::Registry("activity binding is unavailable")) });
        };
        handler(context, input)
    }

    pub(crate) fn namespace_contract(
        &self,
        namespace: NamespaceId,
    ) -> Option<(&'static str, NamespaceDescriptor)> {
        self.namespace_modules.get(&namespace).copied()
    }

    pub(crate) fn command_contract<C: Command>(
        &self,
        namespace: NamespaceId,
    ) -> Result<(OperationDescriptor, Digest)> {
        self.operation_contract(
            namespace,
            C::MODULE,
            C::ID,
            C::CODEC_VERSION,
            &self.command_descriptors,
        )
    }

    pub(crate) fn query_contract<Q: Query>(
        &self,
        namespace: NamespaceId,
    ) -> Result<(OperationDescriptor, Digest)> {
        self.operation_contract(
            namespace,
            Q::MODULE,
            Q::ID,
            Q::CODEC_VERSION,
            &self.query_descriptors,
        )
    }

    pub(crate) fn routed_command_contract(
        &self,
        namespace: NamespaceId,
        id: u32,
        codec_version: u32,
    ) -> Result<(&'static str, OperationDescriptor, Digest)> {
        self.routed_operation_contract(namespace, id, codec_version, &self.command_descriptors)
    }

    pub(crate) fn routed_query_contract(
        &self,
        namespace: NamespaceId,
        id: u32,
        codec_version: u32,
    ) -> Result<(&'static str, OperationDescriptor, Digest)> {
        self.routed_operation_contract(namespace, id, codec_version, &self.query_descriptors)
    }

    fn routed_operation_contract(
        &self,
        namespace: NamespaceId,
        id: u32,
        codec_version: u32,
        descriptors: &BTreeMap<BindingKey, OperationDescriptor>,
    ) -> Result<(&'static str, OperationDescriptor, Digest)> {
        let module = self
            .namespace_modules
            .get(&namespace)
            .map(|(module, _)| *module)
            .ok_or(Error::Registry("operation namespace is unavailable"))?;
        let (operation, code) =
            self.operation_contract(namespace, module, id, codec_version, descriptors)?;
        Ok((module, operation, code))
    }

    fn operation_contract(
        &self,
        namespace: NamespaceId,
        module: &'static str,
        id: u32,
        codec_version: u32,
        descriptors: &BTreeMap<BindingKey, OperationDescriptor>,
    ) -> Result<(OperationDescriptor, Digest)> {
        if self
            .namespace_modules
            .get(&namespace)
            .map(|(owner, _)| *owner)
            != Some(module)
        {
            return Err(Error::Registry("operation module does not own namespace"));
        }
        let key = BindingKey::new(module, id, codec_version)?;
        let operation = descriptors
            .get(&key)
            .copied()
            .ok_or(Error::Registry("operation descriptor is unavailable"))?;
        let code = self
            .module_codes
            .get(module)
            .copied()
            .ok_or(Error::Registry("module code is unavailable"))?;
        Ok((operation, code))
    }

    /// Executes one already bounded command through its exact compiled binding.
    pub fn execute_command(
        &self,
        transaction: &Transaction<'_>,
        invocation: CommandInvocation<'_>,
    ) -> Result<HandlerOutcome> {
        if invocation.sequence == 0 || invocation.now_ms < 0 {
            return Err(Error::Command("invalid registered command context"));
        }
        let key = BindingKey::new(
            invocation.module,
            invocation.operation_id,
            invocation.codec_version,
        )?;
        let operation = self
            .command_descriptors
            .get(&key)
            .ok_or(Error::Registry("command descriptor is unavailable"))?;
        validate_invocation(operation, invocation.schema, invocation.input.len())?;
        let handler = self
            .commands
            .get(&key)
            .ok_or(Error::Registry("command binding is unavailable"))?;
        let mut context = CommandContext {
            transaction,
            target: invocation.target,
            sequence: invocation.sequence,
            now_ms: invocation.now_ms,
            input_limit: operation.input_limit,
            output_limit: operation.output_limit,
        };
        let outcome = handler(&mut context, invocation.input)?;
        let output = match &outcome {
            HandlerOutcome::Success(output) | HandlerOutcome::Rejected(output) => output,
        };
        if output.len() > operation.output_limit as usize {
            return Err(Error::Command("registered operation output exceeds limit"));
        }
        Ok(outcome)
    }

    /// Executes one already bounded query through its exact compiled binding.
    pub fn execute_query(
        &self,
        connection: &Connection,
        invocation: QueryInvocation<'_>,
    ) -> Result<Vec<u8>> {
        if invocation.now_ms < 0 {
            return Err(Error::Command("invalid registered query context"));
        }
        let key = BindingKey::new(
            invocation.module,
            invocation.operation_id,
            invocation.codec_version,
        )?;
        let operation = self
            .query_descriptors
            .get(&key)
            .ok_or(Error::Registry("query descriptor is unavailable"))?;
        validate_invocation(operation, invocation.schema, invocation.input.len())?;
        let handler = self
            .queries
            .get(&key)
            .ok_or(Error::Registry("query binding is unavailable"))?;
        let mut context = QueryContext {
            connection,
            cell: invocation.cell,
            commit_sequence: invocation.commit_sequence,
            now_ms: invocation.now_ms,
            input_limit: operation.input_limit,
            output_limit: operation.output_limit,
        };
        let output = handler(&mut context, invocation.input)?;
        if output.len() > operation.output_limit as usize {
            return Err(Error::Command("registered operation output exceeds limit"));
        }
        Ok(output)
    }
}

fn typed_command<C: Command>(
    context: &mut CommandContext<'_, '_>,
    input: &[u8],
) -> Result<HandlerOutcome> {
    let input = decode_wire::<C::Input>(input, context.input_limit)?;
    let outcome = C::execute(context, input)?;
    Ok(match outcome {
        CommandResult::Success(output) => {
            HandlerOutcome::Success(encode_wire(&output, context.output_limit)?)
        }
        CommandResult::Rejected(output) => {
            HandlerOutcome::Rejected(encode_wire(&output, context.output_limit)?)
        }
    })
}

fn typed_query<Q: Query>(context: &mut QueryContext<'_>, input: &[u8]) -> Result<Vec<u8>> {
    let input = decode_wire::<Q::Input>(input, context.input_limit)?;
    let output = Q::execute(context, input)?;
    Ok(encode_wire(&output, context.output_limit)?)
}

fn typed_activity<A: ActivityHandler>(context: ActivityContext, input: Vec<u8>) -> ActivityFuture {
    Box::pin(async move {
        let outcome = A::execute(context, input).await;
        let payload = match &outcome {
            ActivityExecution::Completed(result) => result,
            ActivityExecution::Failed { details, .. } => details,
        };
        if payload.len() > 256 * 1024 {
            return Err(Error::Command("activity handler result exceeds 256 KiB"));
        }
        Ok(outcome)
    })
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BindingKey {
    module: String,
    id: u32,
    codec_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ActivityKey {
    module: String,
    definition: [u8; 32],
    activity: String,
}

impl ActivityKey {
    fn new(module: &str, definition: Digest, activity: &str) -> Result<Self> {
        if !valid_name(module)
            || !valid_name(activity)
            || definition.as_bytes().iter().all(|byte| *byte == 0)
        {
            return Err(Error::Registry("invalid activity binding key"));
        }
        Ok(Self {
            module: module.to_owned(),
            definition: *definition.as_bytes(),
            activity: activity.to_owned(),
        })
    }
}

impl BindingKey {
    fn new(module: &str, id: u32, codec_version: u32) -> Result<Self> {
        if !valid_name(module) || id == 0 || codec_version == 0 {
            return Err(Error::Registry("invalid function binding key"));
        }
        Ok(Self {
            module: module.to_owned(),
            id,
            codec_version,
        })
    }
}

fn validate_build(build: &BuildDescriptor) -> Result<()> {
    if build.source_revision.is_empty()
        || build.source_revision.len() > 128
        || build
            .cargo_lock_digest
            .as_bytes()
            .iter()
            .all(|byte| *byte == 0)
    {
        return Err(Error::Registry("invalid build descriptor"));
    }
    Ok(())
}

fn validate_module(
    module: &ModuleDescriptor,
    commands: &mut BTreeSet<BindingKey>,
    queries: &mut BTreeSet<BindingKey>,
    namespaces: &mut HashMap<NamespaceId, (&'static str, NamespaceDescriptor)>,
) -> Result<()> {
    if !valid_name(module.name)
        || module.schema_min == 0
        || module.schema_max < module.schema_min
        || module
            .source_digest
            .as_bytes()
            .iter()
            .all(|byte| *byte == 0)
    {
        return Err(Error::Registry("invalid module descriptor"));
    }
    if module.migrations.is_empty() {
        return Err(Error::Registry("module has no migrations"));
    }
    let mut expected_version = module.schema_min;
    for migration in module.migrations {
        if migration.version != expected_version
            || migration.sql.is_empty()
            || migration.digest
                != Digest::from_bytes(*blake3::hash(migration.sql.as_bytes()).as_bytes())
        {
            return Err(Error::Registry("invalid migration inventory"));
        }
        expected_version = expected_version
            .checked_add(1)
            .ok_or(Error::Registry("migration version overflow"))?;
    }
    if expected_version.checked_sub(1) != Some(module.schema_max) {
        return Err(Error::Registry(
            "migration range does not cover module schema",
        ));
    }
    validate_operations(module, module.commands, commands)?;
    validate_operations(module, module.queries, queries)?;
    let mut workflows = HashSet::new();
    for digest in module.workflow_definitions {
        if digest.as_bytes().iter().all(|byte| *byte == 0) || !workflows.insert(*digest) {
            return Err(Error::Registry("invalid workflow definition inventory"));
        }
    }
    let mut activities = HashSet::new();
    for activity in module.activity_types {
        if !valid_name(activity) || !activities.insert(*activity) {
            return Err(Error::Registry("invalid activity inventory"));
        }
    }
    if !activities.is_empty() && workflows.is_empty() {
        return Err(Error::Registry(
            "activity inventory requires a workflow definition",
        ));
    }
    for namespace in module.namespaces {
        if namespaces
            .insert(namespace.id, (module.name, *namespace))
            .is_some()
        {
            return Err(Error::Registry("duplicate namespace ID"));
        }
    }
    Ok(())
}

fn validate_operations(
    module: &ModuleDescriptor,
    operations: &[OperationDescriptor],
    keys: &mut BTreeSet<BindingKey>,
) -> Result<()> {
    for operation in operations {
        if operation.id == 0
            || operation.codec_version == 0
            || operation.schema_min < module.schema_min
            || operation.schema_max > module.schema_max
            || operation.schema_max < operation.schema_min
            || !(1..=MAX_OPERATION_BYTES).contains(&operation.input_limit)
            || !(1..=MAX_OPERATION_BYTES).contains(&operation.output_limit)
            || !keys.insert(BindingKey::new(
                module.name,
                operation.id,
                operation.codec_version,
            )?)
        {
            return Err(Error::Registry("invalid operation inventory"));
        }
    }
    Ok(())
}

fn validate_namespaces(
    namespaces: &HashMap<NamespaceId, (&'static str, NamespaceDescriptor)>,
) -> Result<()> {
    if namespaces.len() > MAX_NAMESPACES {
        return Err(Error::Registry("namespace count exceeds 128"));
    }
    let mut names = HashSet::new();
    for (_, namespace) in namespaces.values() {
        if !valid_name(namespace.name)
            || !names.insert(namespace.name)
            || !(1..=4096).contains(&namespace.shards)
            || !namespace.shards.is_power_of_two()
        {
            return Err(Error::Registry("invalid namespace inventory"));
        }
        let mut targets = HashSet::new();
        for target in namespace.effect_targets {
            if !namespaces.contains_key(target) || !targets.insert(*target) {
                return Err(Error::Registry("invalid effect target inventory"));
            }
        }
        if let Some(dead_letter) = namespace.dead_letter
            && (!targets.contains(&dead_letter)
                || namespaces.get(&dead_letter).map(|(_, value)| value.role)
                    != Some(CatalogRole::Queue))
        {
            return Err(Error::Registry("invalid dead-letter target"));
        }
    }
    for start in namespaces.keys() {
        let mut visited = HashSet::new();
        let mut current = Some(*start);
        while let Some(id) = current {
            if !visited.insert(id) {
                return Err(Error::Registry("dead-letter cycle"));
            }
            current = namespaces.get(&id).and_then(|(_, value)| value.dead_letter);
        }
    }
    Ok(())
}

fn validate_workflow_effect_targets(
    bindings: &HashMap<(String, [u8; 32]), Vec<NamespaceId>>,
    namespaces: &HashMap<NamespaceId, (&'static str, NamespaceDescriptor)>,
    modules: &[&ModuleDescriptor],
) -> Result<()> {
    for module in modules
        .iter()
        .copied()
        .filter(|module| !module.workflow_definitions.is_empty())
    {
        let mut workflow_namespaces = module
            .namespaces
            .iter()
            .filter(|namespace| namespace.role == CatalogRole::Workflow);
        let namespace = workflow_namespaces.next().ok_or(Error::Registry(
            "workflow definitions require one Workflow namespace",
        ))?;
        if workflow_namespaces.next().is_some() {
            return Err(Error::Registry(
                "workflow definitions require one Workflow namespace",
            ));
        }
        let compiled = bindings
            .iter()
            .filter(|((owner, _), _)| owner == module.name)
            .flat_map(|(_, targets)| targets.iter().copied())
            .collect::<HashSet<_>>();
        let declared = namespace
            .effect_targets
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        if compiled != declared
            || compiled
                .iter()
                .any(|target| !namespaces.contains_key(target))
        {
            return Err(Error::Registry(
                "Workflow effect targets and compiled definitions differ",
            ));
        }
    }
    Ok(())
}

fn validate_queue_bindings(
    bindings: &[QueueBinding],
    namespaces: &HashMap<NamespaceId, (&'static str, NamespaceDescriptor)>,
    modules: &[&ModuleDescriptor],
) -> Result<()> {
    let queues = namespaces
        .iter()
        .filter(|(_, (_, namespace))| namespace.role == CatalogRole::Queue)
        .count();
    if bindings.len() != queues {
        return Err(Error::Registry(
            "Queue descriptors and compiled bindings differ",
        ));
    }

    for binding in bindings {
        let Some((owner, namespace)) = namespaces.get(&binding.namespace) else {
            return Err(Error::Registry("Queue binding namespace is unavailable"));
        };
        if namespace.role != CatalogRole::Queue || *owner != binding.module {
            return Err(Error::Registry("Queue binding does not own its namespace"));
        }
        let module = modules
            .iter()
            .find(|module| module.name == binding.module)
            .ok_or(Error::Registry("Queue binding module is unavailable"))?;
        if !module.commands.iter().any(|command| {
            command.id == binding.send_command_id
                && command.codec_version == binding.codec_version
                && command.input_limit >= crate::queue::QUEUE_SEND_MAX_INPUT_BYTES
        }) {
            return Err(Error::Registry(
                "Queue send binding is absent from its descriptor",
            ));
        }
        if namespace.dead_letter != binding.dead_letter.map(|target| target.namespace()) {
            return Err(Error::Registry(
                "Queue dead-letter descriptor and binding differ",
            ));
        }

        let Some(dead_letter) = binding.dead_letter else {
            continue;
        };
        let target = bindings
            .iter()
            .find(|candidate| candidate.namespace == dead_letter.namespace())
            .ok_or(Error::Registry(
                "Queue dead-letter target has no compiled binding",
            ))?;
        if target.module != dead_letter.module()
            || target.send_command_id != dead_letter.send_command_id()
            || target.codec_version != dead_letter.codec_version()
            || namespaces
                .get(&target.namespace)
                .map(|(_, descriptor)| descriptor.shards)
                != Some(dead_letter.shards())
        {
            return Err(Error::Registry(
                "Queue dead-letter target and compiled binding differ",
            ));
        }
    }
    Ok(())
}

fn validate_maintenance_bindings(
    maintenance: &BTreeMap<&'static str, Option<crate::QueueDeadLetterTarget>>,
    queues: &[QueueBinding],
) -> Result<()> {
    for (module, configured) in maintenance {
        let queue = queues.iter().find(|queue| queue.module == *module);
        match (queue, configured) {
            (Some(queue), configured) if queue.dead_letter == *configured => {}
            (None, None) => {}
            _ => {
                return Err(Error::Registry(
                    "maintenance and Queue dead-letter bindings differ",
                ));
            }
        }
    }
    Ok(())
}

fn validate_invocation(
    operation: &OperationDescriptor,
    schema: u32,
    input_bytes: usize,
) -> Result<()> {
    if !(operation.schema_min..=operation.schema_max).contains(&schema) {
        return Err(Error::Command(
            "registered operation does not support schema",
        ));
    }
    if input_bytes > operation.input_limit as usize {
        return Err(Error::Command("registered operation input exceeds limit"));
    }
    Ok(())
}

fn operation_descriptors(
    modules: &[&ModuleDescriptor],
    select: impl Fn(&ModuleDescriptor) -> &[OperationDescriptor],
) -> BTreeMap<BindingKey, OperationDescriptor> {
    modules
        .iter()
        .flat_map(|module| {
            select(module).iter().map(|operation| {
                (
                    BindingKey {
                        module: module.name.to_owned(),
                        id: operation.id,
                        codec_version: operation.codec_version,
                    },
                    *operation,
                )
            })
        })
        .collect()
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
