use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::schemas::{
    BuildDescriptor, MAX_DESCRIPTOR_BYTES, MigrationDescriptor, ModuleDescriptor,
    NamespaceDescriptor, OperationDescriptor, RetainedCodeDescriptor,
};
use crate::Result;
use crate::cell::catalog::CatalogRole;
use crate::identity::Digest;
use crate::identity::encode_hex;

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
        raw_modules.push(RawModule::new(base, code, module.retained_codes));
    }
    raw_namespaces.sort_by(|left, right| left.id.cmp(&right.id));
    let release = RawRelease {
        build: RawBuild {
            cargo_lock_digest: encode_hex(build.cargo_lock_digest.as_bytes()),
            source_revision: build.source_revision.clone(),
        },
        modules: raw_modules,
        namespaces: raw_namespaces,
        peer_versions: [1],
        runtime: "crab-http-server".to_owned(),
        version: 1,
    };
    Ok((serde_json::to_vec(&release)?, codes))
}

pub(super) fn verify_rolling_compatibility(previous: &[u8], candidate: &[u8]) -> Result<()> {
    if let Some(reason) = rolling_incompatibility(previous, candidate)? {
        return Err(crate::Error::Registry(reason));
    }
    Ok(())
}

pub(super) fn requires_persisted_work_inventory(previous: &[u8], candidate: &[u8]) -> Result<bool> {
    Ok(rolling_incompatibility(previous, candidate)?.is_some())
}

fn rolling_incompatibility(previous: &[u8], candidate: &[u8]) -> Result<Option<&'static str>> {
    let previous = decode_compatibility_release(previous)?;
    let candidate = decode_compatibility_release(candidate)?;

    for old in &previous.modules {
        let Some(new) = candidate
            .modules
            .iter()
            .find(|module| module.name == old.name)
        else {
            return Ok(Some("rolling release removes a compiled module"));
        };
        if new.schema_min > old.schema_min || new.schema_max < old.schema_max {
            return Ok(Some("rolling release narrows a module schema range"));
        }
        if new.code != old.code
            && !new.retained_codes.iter().any(|retained| {
                retained.code == old.code
                    && retained.schema_min <= old.schema_min
                    && retained.schema_max >= old.schema_max
            })
        {
            return Ok(Some(
                "rolling release does not retain predecessor module code",
            ));
        }
        if let Some(reason) = operation_incompatibility(
            &old.commands,
            &new.commands,
            "rolling release removes a command codec",
            "rolling release narrows a command contract",
        ) {
            return Ok(Some(reason));
        }
        if let Some(reason) = operation_incompatibility(
            &old.queries,
            &new.queries,
            "rolling release removes a query codec",
            "rolling release narrows a query contract",
        ) {
            return Ok(Some(reason));
        }
        if !old
            .migrations
            .iter()
            .all(|migration| new.migrations.contains(migration))
        {
            return Ok(Some("rolling release removes a migration digest"));
        }
        if !old
            .workflows
            .iter()
            .all(|workflow| new.workflows.contains(workflow))
        {
            return Ok(Some("rolling release removes a workflow definition"));
        }
        if !old
            .activities
            .iter()
            .all(|activity| new.activities.contains(activity))
        {
            return Ok(Some("rolling release removes an activity type"));
        }
    }
    if !previous
        .namespaces
        .iter()
        .all(|namespace| candidate.namespaces.contains(namespace))
    {
        return Ok(Some(
            "rolling release changes or removes a namespace contract",
        ));
    }
    Ok(None)
}

fn operation_incompatibility(
    previous: &[RawOperation],
    candidate: &[RawOperation],
    missing: &'static str,
    narrowed: &'static str,
) -> Option<&'static str> {
    for old in previous {
        let Some(new) = candidate
            .iter()
            .find(|operation| operation.id == old.id && operation.codec == old.codec)
        else {
            return Some(missing);
        };
        if new.schema_min > old.schema_min
            || new.schema_max < old.schema_max
            || new.input_limit < old.input_limit
            || new.output_limit < old.output_limit
        {
            return Some(narrowed);
        }
    }
    None
}

fn decode_compatibility_release(bytes: &[u8]) -> Result<RawRelease> {
    if bytes.is_empty() || bytes.len() > MAX_DESCRIPTOR_BYTES {
        return Err(crate::Error::Registry("release descriptor size is invalid"));
    }
    let release: RawRelease = serde_json::from_slice(bytes)?;
    if release.version != 1 || release.runtime != "crab-http-server" || release.peer_versions != [1]
    {
        return Err(crate::Error::Registry(
            "release descriptor identity is invalid",
        ));
    }
    Ok(release)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawRelease {
    build: RawBuild,
    modules: Vec<RawModule>,
    namespaces: Vec<RawNamespace>,
    peer_versions: [u32; 1],
    runtime: String,
    version: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawBuild {
    cargo_lock_digest: String,
    source_revision: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawModule {
    activities: Vec<String>,
    code: String,
    commands: Vec<RawOperation>,
    migrations: Vec<RawMigration>,
    name: String,
    queries: Vec<RawOperation>,
    retained_codes: Vec<RawRetainedCode>,
    schema_max: u32,
    schema_min: u32,
    source_digest: String,
    workflows: Vec<String>,
}

impl RawModule {
    fn new(
        base: RawModuleBase<'_>,
        code: Digest,
        retained_codes: &[RetainedCodeDescriptor],
    ) -> Self {
        let mut retained_codes = retained_codes
            .iter()
            .map(RawRetainedCode::from)
            .collect::<Vec<_>>();
        retained_codes.sort();
        Self {
            activities: base.activities.into_iter().map(str::to_owned).collect(),
            code: encode_hex(code.as_bytes()),
            commands: base.commands,
            migrations: base.migrations,
            name: base.name.to_owned(),
            queries: base.queries,
            retained_codes,
            schema_max: base.schema_max,
            schema_min: base.schema_min,
            source_digest: base.source_digest,
            workflows: base.workflows,
        }
    }
}

#[derive(Deserialize, Ord, PartialOrd, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RawRetainedCode {
    code: String,
    schema_max: u32,
    schema_min: u32,
}

impl From<&RetainedCodeDescriptor> for RawRetainedCode {
    fn from(retained: &RetainedCodeDescriptor) -> Self {
        Self {
            code: encode_hex(retained.code.as_bytes()),
            schema_max: retained.schema_max,
            schema_min: retained.schema_min,
        }
    }
}

#[derive(Clone, Serialize)]
struct RawModuleBase<'a> {
    activities: Vec<&'a str>,
    commands: Vec<RawOperation>,
    migrations: Vec<RawMigration>,
    name: &'a str,
    namespaces: Vec<RawNamespace>,
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

#[derive(Clone, Deserialize, Ord, PartialOrd, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Clone, Deserialize, Ord, PartialOrd, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct RawNamespace {
    dead_letter: Option<String>,
    effect_targets: Vec<String>,
    id: String,
    module: String,
    name: String,
    role: String,
    shards: u32,
}

impl RawNamespace {
    fn new(module: &str, namespace: &NamespaceDescriptor) -> Self {
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
            module: module.to_owned(),
            name: namespace.name.to_owned(),
            role: role_name(namespace.role).to_owned(),
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
        CatalogRole::Blob => "blob",
        CatalogRole::Cron => "cron",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{requires_persisted_work_inventory, verify_rolling_compatibility};

    fn release(code: &str, retained_codes: Value) -> Value {
        json!({
            "build": {
                "cargo_lock_digest": "11".repeat(32),
                "source_revision": "test"
            },
            "modules": [{
                "activities": ["send"],
                "code": code,
                "commands": [{
                    "codec": 1,
                    "id": 1,
                    "input_limit": 16,
                    "output_limit": 16,
                    "schema_max": 1,
                    "schema_min": 1
                }],
                "migrations": [{"digest": "22".repeat(32), "version": 1}],
                "name": "workflow",
                "queries": [{
                    "codec": 1,
                    "id": 2,
                    "input_limit": 8,
                    "output_limit": 16,
                    "schema_max": 1,
                    "schema_min": 1
                }],
                "retained_codes": retained_codes,
                "schema_max": 1,
                "schema_min": 1,
                "source_digest": "33".repeat(32),
                "workflows": ["44".repeat(32)]
            }],
            "namespaces": [{
                "dead_letter": null,
                "effect_targets": [],
                "id": "55".repeat(16),
                "module": "workflow",
                "name": "workflow",
                "role": "workflow",
                "shards": 1
            }],
            "peer_versions": [1],
            "runtime": "crab-http-server",
            "version": 1
        })
    }

    fn bytes(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    #[test]
    fn rolling_release_retains_executable_and_persisted_work_contracts() {
        let old_code = "66".repeat(32);
        let previous = release(&old_code, json!([]));
        let candidate = release(
            &"77".repeat(32),
            json!([{"code": old_code, "schema_max": 1, "schema_min": 1}]),
        );

        verify_rolling_compatibility(&bytes(&previous), &bytes(&candidate)).unwrap();
        assert!(!requires_persisted_work_inventory(&bytes(&previous), &bytes(&candidate)).unwrap());
    }

    #[test]
    fn rolling_release_rejects_removed_runtime_contracts() {
        let old_code = "66".repeat(32);
        let previous = release(&old_code, json!([]));
        let base = release(
            &"77".repeat(32),
            json!([{"code": old_code, "schema_max": 1, "schema_min": 1}]),
        );

        let mut cases = Vec::new();
        let mut missing_code = base.clone();
        missing_code["modules"][0]["retained_codes"] = json!([]);
        cases.push(missing_code);
        let mut missing_command = base.clone();
        missing_command["modules"][0]["commands"] = json!([]);
        cases.push(missing_command);
        let mut missing_workflow = base.clone();
        missing_workflow["modules"][0]["workflows"] = json!([]);
        cases.push(missing_workflow);
        let mut changed_namespace = base;
        changed_namespace["namespaces"][0]["shards"] = json!(2);
        cases.push(changed_namespace);

        for candidate in cases {
            assert!(verify_rolling_compatibility(&bytes(&previous), &bytes(&candidate)).is_err());
            assert!(
                requires_persisted_work_inventory(&bytes(&previous), &bytes(&candidate)).unwrap()
            );
        }
    }
}
