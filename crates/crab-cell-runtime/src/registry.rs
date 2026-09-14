use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crab_ltx::rusqlite::{Connection, Transaction};

mod descriptor;

use descriptor::encode_release;

use crate::{
    CatalogRole, CellId, Digest, Error, HandlerOutcome, NamespaceId, Result, SqlBatch,
    SqlResultSet, WireValue,
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
    cell: CellId,
    sequence: u64,
    now_ms: i64,
    input_limit: u32,
    output_limit: u32,
}

impl CommandContext<'_, '_> {
    #[must_use]
    pub const fn cell_id(&self) -> CellId {
        self.cell
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn now_ms(&self) -> i64 {
        self.now_ms
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
    pub cell: CellId,
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
}

impl RegistryBuilder {
    #[must_use]
    pub fn new(build: BuildDescriptor) -> Self {
        Self {
            build,
            modules: Vec::new(),
            commands: BTreeMap::new(),
            queries: BTreeMap::new(),
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
        validate_namespaces(&namespace_owners)?;

        let command_descriptors = operation_descriptors(&self.modules, |module| module.commands);
        let query_descriptors = operation_descriptors(&self.modules, |module| module.queries);

        let (release_bytes, module_codes) = encode_release(&self.build, &self.modules)?;
        if release_bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(Error::Registry("release descriptor exceeds 256 KiB"));
        }
        let release_digest = Digest::from_bytes(*blake3::hash(&release_bytes).as_bytes());
        Ok(Registry {
            release_bytes,
            release_digest,
            module_codes,
            commands: self.commands,
            command_descriptors,
            queries: self.queries,
            query_descriptors,
            namespace_modules: namespace_owners,
        })
    }
}

/// Immutable compiled registry shared by runtime and release inspection.
pub struct Registry {
    release_bytes: Vec<u8>,
    release_digest: Digest,
    module_codes: BTreeMap<String, Digest>,
    commands: BTreeMap<BindingKey, CommandHandler>,
    command_descriptors: BTreeMap<BindingKey, OperationDescriptor>,
    queries: BTreeMap<BindingKey, QueryHandler>,
    query_descriptors: BTreeMap<BindingKey, OperationDescriptor>,
    namespace_modules: HashMap<NamespaceId, (&'static str, NamespaceDescriptor)>,
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
            cell: invocation.cell,
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BindingKey {
    module: String,
    id: u32,
    codec_version: u32,
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
