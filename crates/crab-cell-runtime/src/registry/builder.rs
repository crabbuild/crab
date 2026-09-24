//! Registry construction and the compiled module registry.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rusqlite::{Connection, Transaction};

use super::*;
use crate::registry::handlers::{
    ActivityFunction, ActivityFuture, ActivityRunner, CommandHandler, EffectRunner,
    MaintenanceRunner, QueryHandler,
};
use crate::registry::schemas::{
    MAX_DESCRIPTOR_BYTES, MAX_MODULES, operation_descriptors, typed_activity,
    typed_activity_runner, typed_blocking_activity, typed_command, typed_effect, typed_maintenance,
    typed_query, valid_name, validate_build, validate_cron_bindings, validate_invocation,
    validate_maintenance_bindings, validate_module, validate_namespaces,
    validate_primitive_bindings, validate_queue_bindings, validate_workflow_effect_targets,
};

/// Mutable startup-only collector for descriptors and compiled bindings.
pub struct RegistryBuilder {
    pub(super) build: BuildDescriptor,
    pub(super) modules: Vec<&'static ModuleDescriptor>,
    pub(super) commands: BTreeMap<BindingKey, CommandHandler>,
    pub(super) queries: BTreeMap<BindingKey, QueryHandler>,
    pub(super) workflow_definitions: HashMap<(String, [u8; 32]), Vec<NamespaceId>>,
    pub(super) activities: BTreeMap<ActivityKey, ActivityFunction>,
    pub(super) activity_claims: BTreeSet<ActivityKey>,
    pub(super) activity_runners: HashMap<NamespaceId, ActivityRunner>,
    pub(super) queue_bindings: Vec<QueueBinding>,
    pub(super) blob_bindings: Vec<PrimitiveBinding>,
    pub(super) cron_bindings: Vec<CronBinding>,
    pub(super) maintenance_bindings:
        BTreeMap<&'static str, Option<crate::primitives::queue::QueueDeadLetterTarget>>,
    pub(super) maintenance_runners: BTreeMap<&'static str, MaintenanceRunner>,
    pub(super) effect_runners: BTreeMap<&'static str, EffectRunner>,
    pub(super) maintenance_operations: BTreeMap<&'static str, (u32, u32)>,
    pub(super) effect_operations: BTreeMap<&'static str, (u32, u32, u32, u32, u32)>,
    pub(super) activity_operations: HashMap<NamespaceId, (u32, u32, u32, u32, u32)>,
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
            activity_runners: HashMap::new(),
            queue_bindings: Vec::new(),
            blob_bindings: Vec::new(),
            cron_bindings: Vec::new(),
            maintenance_bindings: BTreeMap::new(),
            maintenance_runners: BTreeMap::new(),
            effect_runners: BTreeMap::new(),
            maintenance_operations: BTreeMap::new(),
            effect_operations: BTreeMap::new(),
            activity_operations: HashMap::new(),
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
        dead_letter: Option<crate::primitives::queue::QueueDeadLetterTarget>,
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

    pub(crate) fn bind_cron_module(
        &mut self,
        module: &'static str,
        namespace: NamespaceId,
        targets: &'static [crate::primitives::cron::CronTarget],
    ) -> Result<()> {
        if self
            .cron_bindings
            .iter()
            .any(|binding| binding.namespace == namespace)
        {
            return Err(Error::Registry("duplicate Cron module binding"));
        }
        self.cron_bindings.push(CronBinding {
            module,
            namespace,
            targets,
        });
        Ok(())
    }

    pub(crate) fn bind_blob_module(
        &mut self,
        module: &'static str,
        namespace: NamespaceId,
    ) -> Result<()> {
        if self
            .blob_bindings
            .iter()
            .any(|binding| binding.namespace == namespace)
        {
            return Err(Error::Registry("duplicate Blob module binding"));
        }
        self.blob_bindings
            .push(PrimitiveBinding { module, namespace });
        Ok(())
    }

    pub(crate) fn bind_maintenance_module(
        &mut self,
        module: &'static str,
        queue_dead_letter: Option<crate::primitives::queue::QueueDeadLetterTarget>,
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

    pub(crate) fn bind_maintenance_runner<M: MaintenanceModule>(&mut self) -> Result<()> {
        if self
            .maintenance_runners
            .insert(M::MODULE, typed_maintenance::<M>)
            .is_some()
        {
            return Err(Error::Registry("duplicate maintenance runner binding"));
        }
        self.maintenance_operations
            .insert(M::MODULE, (M::TICK_COMMAND_ID, M::CODEC_VERSION));
        Ok(())
    }

    pub(crate) fn bind_effect_runner<M: EffectModule>(&mut self) -> Result<()> {
        if self
            .effect_runners
            .insert(M::MODULE, typed_effect::<M>)
            .is_some()
        {
            return Err(Error::Registry("duplicate effect runner binding"));
        }
        self.effect_operations.insert(
            M::MODULE,
            (
                M::CLAIM_COMMAND_ID,
                M::LEASE_COMMAND_ID,
                M::VALIDATE_QUERY_ID,
                M::STATUS_QUERY_ID,
                M::CODEC_VERSION,
            ),
        );
        Ok(())
    }

    pub(crate) fn bind_activity_runner<M: WorkflowActivityModule>(&mut self) -> Result<()> {
        if self
            .activity_runners
            .insert(M::NAMESPACE, typed_activity_runner::<M>)
            .is_some()
        {
            return Err(Error::Registry("duplicate activity runner binding"));
        }
        self.activity_operations.insert(
            M::NAMESPACE,
            (
                M::ACTIVITY_CLAIM_COMMAND_ID,
                M::ACTIVITY_COMPLETE_COMMAND_ID,
                M::ACTIVITY_EXTEND_COMMAND_ID,
                M::ACTIVITY_VALIDATE_QUERY_ID,
                M::CODEC_VERSION,
            ),
        );
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
        if self
            .activities
            .insert(key, ActivityFunction::Async(typed_activity::<A>))
            .is_some()
        {
            return Err(Error::Registry("duplicate activity binding"));
        }
        Ok(())
    }

    /// Binds one descriptor activity to a node-owned blocking callback.
    pub fn bind_blocking_activity<A: BlockingActivityHandler>(
        &mut self,
        module: &'static str,
        definition: Digest,
    ) -> std::result::Result<(), RegistryError> {
        let key = ActivityKey::new(module, definition, A::TYPE)?;
        if self
            .activities
            .insert(
                key,
                ActivityFunction::Blocking(typed_blocking_activity::<A>),
            )
            .is_some()
        {
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
        validate_primitive_bindings(
            &self.blob_bindings,
            CatalogRole::Blob,
            "Blob descriptors and compiled bindings differ",
            &namespace_owners,
        )?;
        validate_cron_bindings(&self.cron_bindings, &namespace_owners, &self.modules)?;
        validate_maintenance_bindings(&self.maintenance_bindings, &self.queue_bindings)?;
        if self
            .maintenance_bindings
            .keys()
            .copied()
            .collect::<BTreeSet<_>>()
            != self.maintenance_runners.keys().copied().collect()
        {
            return Err(Error::Registry(
                "maintenance metadata and runner bindings differ",
            ));
        }
        let expected_activity_runners = self
            .modules
            .iter()
            .filter(|module| !module.activity_types.is_empty())
            .flat_map(|module| {
                module
                    .namespaces
                    .iter()
                    .filter(|namespace| namespace.role == CatalogRole::Workflow)
                    .map(|namespace| namespace.id)
            })
            .collect::<HashSet<_>>();
        if expected_activity_runners != self.activity_runners.keys().copied().collect() {
            return Err(Error::Registry(
                "activity metadata and runner bindings differ",
            ));
        }

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
        let module_migrations = self
            .modules
            .iter()
            .map(|module| (module.name, module.migrations))
            .collect();

        let (release_bytes, module_codes) = encode_release(&self.build, &self.modules)?;
        let module_retained_codes = self
            .modules
            .iter()
            .map(|module| {
                let current = module_codes
                    .get(module.name)
                    .copied()
                    .ok_or(Error::Registry("module code is unavailable"))?;
                if module
                    .retained_codes
                    .iter()
                    .any(|retained| retained.code == current)
                {
                    return Err(Error::Registry(
                        "current module code is retained as predecessor",
                    ));
                }
                Ok((module.name, module.retained_codes))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut supported_codes = module_codes.values().copied().collect::<HashSet<_>>();
        supported_codes.extend(
            module_retained_codes
                .values()
                .flat_map(|retained| retained.iter().map(|descriptor| descriptor.code)),
        );
        if supported_codes.len() > MAX_MODULES {
            return Err(Error::Registry(
                "current and retained module code inventory exceeds 128",
            ));
        }
        if release_bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(Error::Registry("release descriptor exceeds 256 KiB"));
        }
        let release_digest = Digest::from_bytes(*blake3::hash(&release_bytes).as_bytes());
        let blocking_modules = self
            .activities
            .iter()
            .filter_map(|(key, handler)| {
                matches!(handler, ActivityFunction::Blocking(_)).then_some(key.module.as_str())
            })
            .collect::<HashSet<_>>();
        let blocking_activity_namespaces = namespace_owners
            .iter()
            .filter_map(|(namespace, (module, descriptor))| {
                (descriptor.role == CatalogRole::Workflow && blocking_modules.contains(*module))
                    .then_some(*namespace)
            })
            .collect();
        let module_names = self.modules.iter().map(|module| module.name).collect();
        Ok(Registry {
            release_bytes,
            release_digest,
            module_codes,
            module_schemas,
            module_migrations,
            module_retained_codes,
            module_names,
            commands: self.commands,
            command_descriptors,
            queries: self.queries,
            query_descriptors,
            namespace_modules: namespace_owners,
            activities: self.activities,
            blocking_activity_namespaces,
            activity_runners: self.activity_runners,
            maintenance_runners: self.maintenance_runners,
            effect_runners: self.effect_runners,
            maintenance_operations: self.maintenance_operations,
            effect_operations: self.effect_operations,
            activity_operations: self.activity_operations,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct QueueBinding {
    pub(super) module: &'static str,
    pub(super) namespace: NamespaceId,
    pub(super) send_command_id: u32,
    pub(super) codec_version: u32,
    pub(super) dead_letter: Option<crate::primitives::queue::QueueDeadLetterTarget>,
}

#[derive(Clone, Copy)]
pub(super) struct CronBinding {
    pub(super) module: &'static str,
    pub(super) namespace: NamespaceId,
    pub(super) targets: &'static [crate::primitives::cron::CronTarget],
}

#[derive(Clone, Copy)]
pub(super) struct PrimitiveBinding {
    pub(super) module: &'static str,
    pub(super) namespace: NamespaceId,
}

/// Immutable compiled registry shared by runtime and release inspection.
#[derive(Clone)]
pub struct Registry {
    pub(super) release_bytes: Vec<u8>,
    pub(super) release_digest: Digest,
    pub(super) module_codes: BTreeMap<String, Digest>,
    pub(super) module_names: Vec<&'static str>,
    pub(super) module_schemas: BTreeMap<String, (u32, u32)>,
    pub(super) module_migrations: BTreeMap<&'static str, &'static [MigrationDescriptor]>,
    pub(super) module_retained_codes: BTreeMap<&'static str, &'static [RetainedCodeDescriptor]>,
    pub(super) commands: BTreeMap<BindingKey, CommandHandler>,
    pub(super) command_descriptors: BTreeMap<BindingKey, OperationDescriptor>,
    pub(super) queries: BTreeMap<BindingKey, QueryHandler>,
    pub(super) query_descriptors: BTreeMap<BindingKey, OperationDescriptor>,
    pub(super) namespace_modules: HashMap<NamespaceId, (&'static str, NamespaceDescriptor)>,
    pub(super) activities: BTreeMap<ActivityKey, ActivityFunction>,
    pub(super) blocking_activity_namespaces: HashSet<NamespaceId>,
    pub(super) activity_runners: HashMap<NamespaceId, ActivityRunner>,
    pub(super) maintenance_runners: BTreeMap<&'static str, MaintenanceRunner>,
    pub(super) effect_runners: BTreeMap<&'static str, EffectRunner>,
    pub(super) maintenance_operations: BTreeMap<&'static str, (u32, u32)>,
    pub(super) effect_operations: BTreeMap<&'static str, (u32, u32, u32, u32, u32)>,
    pub(super) activity_operations: HashMap<NamespaceId, (u32, u32, u32, u32, u32)>,
}

mod run;

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

    /// Returns the schema range compiled for one registered module.
    #[must_use]
    pub fn module_schema_range(&self, module: &str) -> Option<(u32, u32)> {
        self.module_schemas.get(module).copied()
    }

    /// Returns the compiled module names in sorted order.
    #[must_use]
    pub fn module_names(&self) -> &[&'static str] {
        &self.module_names
    }

    /// Returns the sorted module-code inventory advertised by eligible nodes.
    #[must_use]
    pub fn module_digests(&self) -> Vec<Digest> {
        let mut digests = self.module_codes.values().copied().collect::<Vec<_>>();
        digests.extend(
            self.module_retained_codes
                .values()
                .flat_map(|retained| retained.iter().map(|descriptor| descriptor.code)),
        );
        digests.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        digests.dedup();
        digests
    }

    /// Verifies that this registry can replace one previously selected release online.
    ///
    /// Every executable and persisted-work contract from the predecessor must remain
    /// available. Removing one requires an offline maintenance activation.
    pub fn verify_rolling_from(&self, previous: &[u8]) -> Result<()> {
        verify_rolling_compatibility(previous, &self.release_bytes)
    }

    /// Reports whether an offline rollout removes or narrows a predecessor contract.
    ///
    /// A true result requires complete persisted-work admission before the
    /// predecessor implementation or codec can be removed.
    pub fn requires_persisted_work_inventory_from(&self, previous: &[u8]) -> Result<bool> {
        requires_persisted_work_inventory(previous, &self.release_bytes)
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
        if descriptor.role != role || !(*schema_min..=*schema_max).contains(&schema) {
            return false;
        }
        self.supports_module_code(module, code, schema)
    }

    /// Reports whether one Cell already uses the module's target code and schema.
    #[must_use]
    pub fn is_current_cell(
        &self,
        namespace: NamespaceId,
        role: CatalogRole,
        code: Digest,
        schema: u32,
    ) -> bool {
        self.current_cell_version(namespace, role) == Some((code, schema))
    }

    /// Returns the target code and schema for one registered namespace role.
    #[must_use]
    pub fn current_cell_version(
        &self,
        namespace: NamespaceId,
        role: CatalogRole,
    ) -> Option<(Digest, u32)> {
        let (module, descriptor) = self.namespace_modules.get(&namespace)?;
        if descriptor.role != role {
            return None;
        }
        Some((
            *self.module_codes.get(*module)?,
            self.module_schemas.get(*module)?.1,
        ))
    }

    /// Selects the next compiled migration for one exact Cell code/schema pair.
    pub fn next_migration(
        &self,
        namespace: NamespaceId,
        code: Digest,
        schema: u32,
    ) -> Result<Option<MigrationPlan>> {
        let (module, _) = self
            .namespace_modules
            .get(&namespace)
            .ok_or(Error::Registry("namespace is unavailable"))?;
        let current_code = self
            .module_codes
            .get(*module)
            .copied()
            .ok_or(Error::Registry("module code is unavailable"))?;
        let (schema_min, schema_max) = self
            .module_schemas
            .get(*module)
            .copied()
            .ok_or(Error::Registry("module schema range is unavailable"))?;
        if !(schema_min..=schema_max).contains(&schema)
            || (code != current_code
                && !self
                    .module_retained_codes
                    .get(module)
                    .is_some_and(|retained| {
                        retained.iter().any(|descriptor| {
                            descriptor.code == code
                                && (descriptor.schema_min..=descriptor.schema_max).contains(&schema)
                        })
                    }))
        {
            return Err(Error::Registry(
                "Cell code/schema is not executable by this registry",
            ));
        }
        let (to_schema, migration) = match schema.checked_add(1).filter(|next| *next <= schema_max)
        {
            Some(to_schema) => {
                let migration = self
                    .module_migrations
                    .get(module)
                    .and_then(|migrations| {
                        migrations
                            .iter()
                            .find(|migration| migration.version == to_schema)
                    })
                    .copied()
                    .ok_or(Error::Registry("next migration is unavailable"))?;
                (to_schema, Some(migration))
            }
            None if code != current_code => (schema, None),
            None => return Ok(None),
        };
        Ok(Some(MigrationPlan {
            module,
            from_code: code,
            to_code: current_code,
            from_schema: schema,
            to_schema,
            migration,
        }))
    }

    /// Maps one registered internal command to its exact fleet authorization.
    #[must_use]
    pub fn internal_command_action(
        &self,
        namespace: NamespaceId,
        command_id: u32,
        codec_version: u32,
    ) -> Option<&'static str> {
        let module = self.namespace_modules.get(&namespace)?.0;
        if self
            .maintenance_operations
            .get(module)
            .is_some_and(|&(tick, codec)| tick == command_id && codec == codec_version)
        {
            return Some("cell.scheduler.tick");
        }
        if self
            .effect_operations
            .get(module)
            .is_some_and(|&(claim, lease, _, _, codec)| {
                codec == codec_version && matches!(command_id, id if id == claim || id == lease)
            })
        {
            return Some("cell.effect.source");
        }
        if self.activity_operations.get(&namespace).is_some_and(
            |&(claim, complete, extend, _, codec)| {
                codec == codec_version
                    && matches!(command_id, id if id == claim || id == complete || id == extend)
            },
        ) {
            return Some("cell.activity.source");
        }
        None
    }

    /// Maps one registered internal query to its exact fleet authorization.
    #[must_use]
    pub fn internal_query_action(
        &self,
        namespace: NamespaceId,
        query_id: u32,
        codec_version: u32,
    ) -> Option<&'static str> {
        let module = self.namespace_modules.get(&namespace)?.0;
        if self
            .effect_operations
            .get(module)
            .is_some_and(|&(_, _, validate, status, codec)| {
                (validate == query_id || status == query_id) && codec == codec_version
            })
        {
            return Some("cell.effect.source");
        }
        if self
            .activity_operations
            .get(&namespace)
            .is_some_and(|&(_, _, _, validate, codec)| {
                validate == query_id && codec == codec_version
            })
        {
            return Some("cell.activity.source");
        }
        None
    }

    /// Returns the compiled owner and descriptor for one namespace.
    pub fn namespace_contract(
        &self,
        namespace: NamespaceId,
    ) -> Option<(&'static str, NamespaceDescriptor)> {
        self.namespace_modules.get(&namespace).copied()
    }

    /// Returns the number of namespaces compiled into this release.
    #[must_use]
    pub fn namespace_count(&self) -> usize {
        self.namespace_modules.len()
    }

    pub(crate) fn command_contract<C: Command>(
        &self,
        namespace: NamespaceId,
    ) -> Result<OperationDescriptor> {
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
    ) -> Result<OperationDescriptor> {
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
    ) -> Result<(&'static str, OperationDescriptor)> {
        self.routed_operation_contract(namespace, id, codec_version, &self.command_descriptors)
    }

    pub(crate) fn routed_query_contract(
        &self,
        namespace: NamespaceId,
        id: u32,
        codec_version: u32,
    ) -> Result<(&'static str, OperationDescriptor)> {
        self.routed_operation_contract(namespace, id, codec_version, &self.query_descriptors)
    }

    pub(super) fn routed_operation_contract(
        &self,
        namespace: NamespaceId,
        id: u32,
        codec_version: u32,
        descriptors: &BTreeMap<BindingKey, OperationDescriptor>,
    ) -> Result<(&'static str, OperationDescriptor)> {
        let module = self
            .namespace_modules
            .get(&namespace)
            .map(|(module, _)| *module)
            .ok_or(Error::Registry("operation namespace is unavailable"))?;
        let operation =
            self.operation_contract(namespace, module, id, codec_version, descriptors)?;
        Ok((module, operation))
    }

    pub(super) fn operation_contract(
        &self,
        namespace: NamespaceId,
        module: &'static str,
        id: u32,
        codec_version: u32,
        descriptors: &BTreeMap<BindingKey, OperationDescriptor>,
    ) -> Result<OperationDescriptor> {
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
        Ok(operation)
    }

    pub(crate) fn supports_module_code(&self, module: &str, code: Digest, schema: u32) -> bool {
        if self
            .module_schemas
            .get(module)
            .is_none_or(|(schema_min, schema_max)| !(*schema_min..=*schema_max).contains(&schema))
        {
            return false;
        }
        self.module_codes.get(module) == Some(&code)
            || self
                .module_retained_codes
                .get(module)
                .is_some_and(|retained| {
                    retained.iter().any(|descriptor| {
                        descriptor.code == code
                            && (descriptor.schema_min..=descriptor.schema_max).contains(&schema)
                    })
                })
    }

    /// Executes one already bounded command through its exact compiled binding.
    pub fn execute_command(
        &self,
        transaction: &Transaction<'_>,
        invocation: CommandInvocation<'_>,
    ) -> Result<HandlerOutcome> {
        let issued_at_ms = invocation.now_ms;
        self.execute_command_with_issue_time(transaction, invocation, issued_at_ms)
    }

    pub(crate) fn execute_command_with_issue_time(
        &self,
        transaction: &Transaction<'_>,
        invocation: CommandInvocation<'_>,
        issued_at_ms: i64,
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
        if self
            .namespace_modules
            .get(&invocation.target.namespace())
            .map(|(module, _)| *module)
            != Some(invocation.module)
        {
            return Err(Error::Registry("operation module does not own namespace"));
        }
        validate_invocation(operation, invocation.schema, invocation.input.len())?;
        let handler = self
            .commands
            .get(&key)
            .ok_or(Error::Registry("command binding is unavailable"))?;
        let mut context = CommandContext {
            transaction,
            target: invocation.target.clone(),
            effect_targets: self
                .namespace_modules
                .get(&invocation.target.namespace())
                .ok_or(Error::Registry("command target namespace is unavailable"))
                .map(|(_, descriptor)| descriptor.effect_targets)?,
            sequence: invocation.sequence,
            now_ms: invocation.now_ms,
            issued_at_ms,
            input_limit: operation.input_limit,
            output_limit: operation.output_limit,
            effects: None,
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct BindingKey {
    pub(super) module: String,
    pub(super) id: u32,
    pub(super) codec_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ActivityKey {
    pub(super) module: String,
    pub(super) definition: [u8; 32],
    pub(super) activity: String,
}

impl ActivityKey {
    pub(super) fn new(module: &str, definition: Digest, activity: &str) -> Result<Self> {
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
    pub(super) fn new(module: &str, id: u32, codec_version: u32) -> Result<Self> {
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
