//! Module, migration, and operation descriptors with their validation rules.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::*;
use crate::registry::builder::{BindingKey, CronBinding, PrimitiveBinding, QueueBinding};
use crate::registry::handlers::{
    ActivityFuture, ActivityRunFuture, EffectFuture, MaintenanceFuture,
};

pub(super) const MAX_DESCRIPTOR_BYTES: usize = 256 * 1024;
pub(super) const MAX_MODULES: usize = 128;
pub(super) const MAX_NAMESPACES: usize = 128;
pub(super) const MAX_MIGRATION_BYTES: usize = 1024 * 1024;
pub(super) const MAX_OPERATION_BYTES: u32 = crate::codec::MAX_WIRE_BYTES as u32;
pub(super) const CODE_ONLY_MIGRATION_BYTES: usize = 2 * 32;

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

/// One predecessor module code intentionally retained by the current binary.
#[derive(Clone, Copy, Debug)]
pub struct RetainedCodeDescriptor {
    pub code: Digest,
    pub schema_min: u32,
    pub schema_max: u32,
}

/// One registry-verified schema or code migration for an active Cell.
#[derive(Clone, Copy, Debug)]
pub struct MigrationPlan {
    pub(super) module: &'static str,
    pub(super) from_code: Digest,
    pub(super) to_code: Digest,
    pub(super) from_schema: u32,
    pub(super) to_schema: u32,
    pub(super) migration: Option<MigrationDescriptor>,
}

impl MigrationPlan {
    #[must_use]
    pub const fn module(&self) -> &'static str {
        self.module
    }

    #[must_use]
    pub const fn from_code(&self) -> Digest {
        self.from_code
    }

    #[must_use]
    pub const fn to_code(&self) -> Digest {
        self.to_code
    }

    #[must_use]
    pub const fn from_schema(&self) -> u32 {
        self.from_schema
    }

    #[must_use]
    pub const fn to_schema(&self) -> u32 {
        self.to_schema
    }

    #[must_use]
    pub const fn digest(&self) -> Option<Digest> {
        match self.migration {
            Some(migration) => Some(migration.digest),
            None => None,
        }
    }

    #[must_use]
    pub const fn sql(&self) -> Option<&'static str> {
        match self.migration {
            Some(migration) => Some(migration.sql),
            None => None,
        }
    }

    #[must_use]
    pub const fn operation_bytes(&self) -> usize {
        match self.migration {
            Some(migration) => migration.sql.len(),
            None => CODE_ONLY_MIGRATION_BYTES,
        }
    }
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
    pub retained_codes: &'static [RetainedCodeDescriptor],
    pub schema_min: u32,
    pub schema_max: u32,
    pub migrations: &'static [MigrationDescriptor],
    pub commands: &'static [OperationDescriptor],
    pub queries: &'static [OperationDescriptor],
    pub workflow_definitions: &'static [Digest],
    pub activity_types: &'static [&'static str],
    pub namespaces: &'static [NamespaceDescriptor],
}

pub(super) fn typed_command<C: Command>(
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

pub(super) fn typed_query<Q: Query>(
    context: &mut QueryContext<'_>,
    input: &[u8],
) -> Result<Vec<u8>> {
    let input = decode_wire::<Q::Input>(input, context.input_limit)?;
    let output = Q::execute(context, input)?;
    Ok(encode_wire(&output, context.output_limit)?)
}

pub(super) fn typed_activity<A: ActivityHandler>(
    context: ActivityContext,
    input: Vec<u8>,
) -> ActivityFuture {
    Box::pin(async move {
        let outcome = A::execute(context, input).await;
        let payload = match &outcome {
            ActivityExecution::Completed(result) => result,
            ActivityExecution::Failed { details, .. } => details,
        };
        if payload.len() > crate::primitives::workflow::MAX_ACTIVITY_PAYLOAD_BYTES {
            return Err(Error::Command("activity handler result exceeds 256 KiB"));
        }
        Ok(outcome)
    })
}

pub(super) fn typed_blocking_activity<A: BlockingActivityHandler>(
    context: ActivityContext,
    input: Vec<u8>,
) -> Result<ActivityExecution> {
    let outcome = A::execute(context, input);
    let payload = match &outcome {
        ActivityExecution::Completed(result) => result,
        ActivityExecution::Failed { details, .. } => details,
    };
    if payload.len() > crate::primitives::workflow::MAX_ACTIVITY_PAYLOAD_BYTES {
        return Err(Error::Command("activity handler result exceeds 256 KiB"));
    }
    Ok(outcome)
}

pub(super) fn typed_maintenance<M: MaintenanceModule>(
    client: CellClient,
    target: CellTarget,
    identity: MutationIdentity,
    request: MaintenanceTickRequest,
) -> MaintenanceFuture {
    Box::pin(async move {
        client
            .command::<MaintenanceTickCommand<M>>(&target, identity, request)
            .await
    })
}

pub(super) fn typed_effect<M: EffectModule>(
    client: CellClient,
    target: CellTarget,
    peer: EffectPeerClient,
    lease_ms: u32,
) -> EffectFuture {
    Box::pin(async move {
        EffectSupervisor::<M>::new(crate::EffectSource::new(client, target), peer, lease_ms)
            .map_err(EffectSupervisorError::Runtime)?
            .run_once()
            .await
    })
}

pub(super) fn typed_activity_runner<M: WorkflowActivityModule>(
    client: CellClient,
    tenant: TenantId,
    application: ApplicationId,
    shard: u32,
    lease_ms: u32,
    blocking: Option<BlockingActivityReservation>,
) -> ActivityRunFuture {
    Box::pin(async move {
        ActivitySupervisor::new(
            WorkflowActivities::<M>::new(client, tenant, application)
                .map_err(ActivitySupervisorError::Runtime)?,
            lease_ms,
        )
        .map_err(ActivitySupervisorError::Runtime)?
        .run_once(shard, blocking)
        .await
    })
}

pub(super) fn validate_build(build: &BuildDescriptor) -> Result<()> {
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

pub(super) fn validate_module(
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
            || migration.sql.len() > MAX_MIGRATION_BYTES
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
    let mut retained_codes = HashSet::new();
    for retained in module.retained_codes {
        if retained.code.as_bytes().iter().all(|byte| *byte == 0)
            || retained.schema_min < module.schema_min
            || retained.schema_max > module.schema_max
            || retained.schema_max < retained.schema_min
            || !retained_codes.insert(retained.code)
        {
            return Err(Error::Registry("invalid retained module code inventory"));
        }
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

pub(super) fn validate_operations(
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

pub(super) fn validate_namespaces(
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

pub(super) fn validate_workflow_effect_targets(
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

pub(super) fn validate_queue_bindings(
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
                && command.input_limit >= crate::primitives::queue::QUEUE_SEND_MAX_INPUT_BYTES
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

pub(super) fn validate_cron_bindings(
    bindings: &[CronBinding],
    namespaces: &HashMap<NamespaceId, (&'static str, NamespaceDescriptor)>,
    modules: &[&ModuleDescriptor],
) -> Result<()> {
    let cron_namespaces = namespaces
        .values()
        .filter(|(_, namespace)| namespace.role == CatalogRole::Cron)
        .count();
    if bindings.len() != cron_namespaces {
        return Err(Error::Registry(
            "Cron descriptors and compiled bindings differ",
        ));
    }
    for binding in bindings {
        let Some((owner, namespace)) = namespaces.get(&binding.namespace) else {
            return Err(Error::Registry("Cron binding namespace is unavailable"));
        };
        if namespace.role != CatalogRole::Cron || *owner != binding.module {
            return Err(Error::Registry("Cron binding does not own its namespace"));
        }
        let compiled_targets = binding
            .targets
            .iter()
            .map(|target| target.namespace())
            .collect::<HashSet<_>>();
        let declared_targets = namespace
            .effect_targets
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        if compiled_targets != declared_targets {
            return Err(Error::Registry("Cron effect targets and descriptor differ"));
        }
        for target in binding.targets {
            let Some((target_owner, _)) = namespaces.get(&target.namespace()) else {
                return Err(Error::Registry("Cron target namespace is unavailable"));
            };
            if *target_owner != target.module() {
                return Err(Error::Registry(
                    "Cron target module differs from descriptor",
                ));
            }
            let target_module = modules
                .iter()
                .find(|module| module.name == target.module())
                .ok_or(Error::Registry("Cron target module is unavailable"))?;
            if !target_module.commands.iter().any(|command| {
                command.id == target.command_id()
                    && command.codec_version == target.codec_version()
                    && command.input_limit == target.input_limit()
            }) {
                return Err(Error::Registry(
                    "Cron target command differs from descriptor",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_primitive_bindings(
    bindings: &[PrimitiveBinding],
    role: CatalogRole,
    mismatch: &'static str,
    namespaces: &HashMap<NamespaceId, (&'static str, NamespaceDescriptor)>,
) -> Result<()> {
    let expected = namespaces
        .values()
        .filter(|(_, namespace)| namespace.role == role)
        .count();
    if bindings.len() != expected {
        return Err(Error::Registry(mismatch));
    }
    for binding in bindings {
        let Some((owner, namespace)) = namespaces.get(&binding.namespace) else {
            return Err(Error::Registry(mismatch));
        };
        if *owner != binding.module || namespace.role != role {
            return Err(Error::Registry(mismatch));
        }
    }
    Ok(())
}

pub(super) fn validate_maintenance_bindings(
    maintenance: &BTreeMap<&'static str, Option<crate::primitives::queue::QueueDeadLetterTarget>>,
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

pub(super) fn validate_invocation(
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

pub(super) fn operation_descriptors(
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

pub(super) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
