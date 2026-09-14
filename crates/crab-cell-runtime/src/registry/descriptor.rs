use std::collections::BTreeMap;

use serde::Serialize;

use super::{
    BuildDescriptor, MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor,
    OperationDescriptor,
};
use crate::{CatalogRole, Digest, Result, identity::encode_hex};

pub(super) fn encode_release(
    build: &BuildDescriptor,
    modules: &[&ModuleDescriptor],
) -> Result<(Vec<u8>, BTreeMap<String, Digest>)> {
    let mut raw_modules = Vec::with_capacity(modules.len());
    let mut raw_namespaces = Vec::new();
    let mut codes = BTreeMap::new();
    for module in modules {
        let base = RawModuleBase::from(*module);
        let code = Digest::from_bytes(*blake3::hash(&serde_json::to_vec(&base)?).as_bytes());
        codes.insert(module.name.to_owned(), code);
        raw_namespaces.extend(base.namespaces.iter().cloned());
        raw_modules.push(RawModule::new(base, code));
    }
    raw_namespaces.sort_by(|left, right| left.id.cmp(&right.id));
    let release = RawRelease {
        build: RawBuild {
            cargo_lock_digest: encode_hex(build.cargo_lock_digest.as_bytes()),
            source_revision: &build.source_revision,
        },
        modules: raw_modules,
        namespaces: raw_namespaces,
        peer_versions: [1],
        runtime: "crab-http-server",
        version: 1,
    };
    Ok((serde_json::to_vec(&release)?, codes))
}

#[derive(Serialize)]
struct RawRelease<'a> {
    build: RawBuild<'a>,
    modules: Vec<RawModule<'a>>,
    namespaces: Vec<RawNamespace<'a>>,
    peer_versions: [u32; 1],
    runtime: &'static str,
    version: u32,
}

#[derive(Serialize)]
struct RawBuild<'a> {
    cargo_lock_digest: String,
    source_revision: &'a str,
}

#[derive(Serialize)]
struct RawModule<'a> {
    activities: Vec<&'a str>,
    code: String,
    commands: Vec<RawOperation>,
    migrations: Vec<RawMigration>,
    name: &'a str,
    queries: Vec<RawOperation>,
    schema_max: u32,
    schema_min: u32,
    source_digest: String,
    workflows: Vec<String>,
}

impl<'a> RawModule<'a> {
    fn new(base: RawModuleBase<'a>, code: Digest) -> Self {
        Self {
            activities: base.activities,
            code: encode_hex(code.as_bytes()),
            commands: base.commands,
            migrations: base.migrations,
            name: base.name,
            queries: base.queries,
            schema_max: base.schema_max,
            schema_min: base.schema_min,
            source_digest: base.source_digest,
            workflows: base.workflows,
        }
    }
}

#[derive(Clone, Serialize)]
struct RawModuleBase<'a> {
    activities: Vec<&'a str>,
    commands: Vec<RawOperation>,
    migrations: Vec<RawMigration>,
    name: &'a str,
    namespaces: Vec<RawNamespace<'a>>,
    queries: Vec<RawOperation>,
    schema_max: u32,
    schema_min: u32,
    source_digest: String,
    workflows: Vec<String>,
}

impl<'a> From<&'a ModuleDescriptor> for RawModuleBase<'a> {
    fn from(module: &'a ModuleDescriptor) -> Self {
        let mut activities = module.activity_types.to_vec();
        activities.sort_unstable();
        let mut commands = module
            .commands
            .iter()
            .map(RawOperation::from)
            .collect::<Vec<_>>();
        commands.sort();
        let mut migrations = module
            .migrations
            .iter()
            .map(RawMigration::from)
            .collect::<Vec<_>>();
        migrations.sort();
        let mut queries = module
            .queries
            .iter()
            .map(RawOperation::from)
            .collect::<Vec<_>>();
        queries.sort();
        let mut workflows = module
            .workflow_definitions
            .iter()
            .map(|digest| encode_hex(digest.as_bytes()))
            .collect::<Vec<_>>();
        workflows.sort();
        let mut namespaces = module
            .namespaces
            .iter()
            .map(|namespace| RawNamespace::new(module.name, namespace))
            .collect::<Vec<_>>();
        namespaces.sort_by(|left, right| left.id.cmp(&right.id));
        Self {
            activities,
            commands,
            migrations,
            name: module.name,
            namespaces,
            queries,
            schema_max: module.schema_max,
            schema_min: module.schema_min,
            source_digest: encode_hex(module.source_digest.as_bytes()),
            workflows,
        }
    }
}

#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Serialize)]
struct RawOperation {
    codec: u32,
    id: u32,
    input_limit: u32,
    output_limit: u32,
    schema_max: u32,
    schema_min: u32,
}

impl From<&OperationDescriptor> for RawOperation {
    fn from(operation: &OperationDescriptor) -> Self {
        Self {
            codec: operation.codec_version,
            id: operation.id,
            input_limit: operation.input_limit,
            output_limit: operation.output_limit,
            schema_max: operation.schema_max,
            schema_min: operation.schema_min,
        }
    }
}

#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Serialize)]
struct RawMigration {
    digest: String,
    version: u32,
}

impl From<&MigrationDescriptor> for RawMigration {
    fn from(migration: &MigrationDescriptor) -> Self {
        Self {
            digest: encode_hex(migration.digest.as_bytes()),
            version: migration.version,
        }
    }
}

#[derive(Clone, Serialize)]
struct RawNamespace<'a> {
    dead_letter: Option<String>,
    effect_targets: Vec<String>,
    id: String,
    module: &'a str,
    name: &'a str,
    role: &'static str,
    shards: u32,
}

impl<'a> RawNamespace<'a> {
    fn new(module: &'a str, namespace: &'a NamespaceDescriptor) -> Self {
        let mut effect_targets = namespace
            .effect_targets
            .iter()
            .map(|target| encode_hex(target.as_bytes()))
            .collect::<Vec<_>>();
        effect_targets.sort();
        Self {
            dead_letter: namespace
                .dead_letter
                .map(|target| encode_hex(target.as_bytes())),
            effect_targets,
            id: encode_hex(namespace.id.as_bytes()),
            module,
            name: namespace.name,
            role: role_name(namespace.role),
            shards: namespace.shards,
        }
    }
}

fn role_name(role: CatalogRole) -> &'static str {
    match role {
        CatalogRole::Repository => "repository",
        CatalogRole::Sql => "sql",
        CatalogRole::Kv => "kv",
        CatalogRole::Queue => "queue",
        CatalogRole::Workflow => "workflow",
    }
}
