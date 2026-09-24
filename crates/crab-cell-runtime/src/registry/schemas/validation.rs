//! Build, module, namespace, and primitive descriptor validation.

use super::*;

pub(in crate::registry) fn validate_build(build: &BuildDescriptor) -> Result<()> {
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

pub(in crate::registry) fn validate_module(
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

pub(in crate::registry) fn validate_operations(
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

pub(in crate::registry) fn validate_namespaces(
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

pub(in crate::registry) fn validate_workflow_effect_targets(
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

pub(in crate::registry) fn validate_queue_bindings(
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

pub(in crate::registry) fn validate_cron_bindings(
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

pub(in crate::registry) fn validate_primitive_bindings(
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

pub(in crate::registry) fn validate_maintenance_bindings(
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

pub(in crate::registry) fn validate_invocation(
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
