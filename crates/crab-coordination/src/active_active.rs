//! Pure active-active replication planning.

use std::collections::{BTreeSet, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{CoordinationError, Result};
use crate::write_coordinator::{
    CommitRequest, CoordinatedRefUpdate, CoordinatorRepairSnapshot, ManagedCoordinatorProvider,
};

/// Replication write model relevant to active-active coordination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ActiveActiveMode {
    #[default]
    ReadReplica,
    ActiveActive,
}

impl ActiveActiveMode {
    #[must_use]
    pub fn is_active_active(self) -> bool {
        self == Self::ActiveActive
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive field values by reference"
)]
fn is_read_replica_mode(mode: &ActiveActiveMode) -> bool {
    *mode == ActiveActiveMode::ReadReplica
}

/// Active-active coordinator kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActiveActiveCoordinatorKind {
    #[default]
    Managed,
}

/// Consistency contract required for active-active writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActiveActiveCoordinatorConsistency {
    #[default]
    Linearizable,
}

/// Linearizable coordinator configuration for active-active writes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActiveCoordinatorConfig {
    #[serde(default)]
    pub kind: ActiveActiveCoordinatorKind,
    pub url: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failover_regions: Vec<String>,
    #[serde(default)]
    pub consistency: ActiveActiveCoordinatorConsistency,
}

/// Regional active-active writer endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActiveWriterConfig {
    pub name: String,
    pub url: String,
    pub region: String,
    #[serde(default = "default_writer_enabled")]
    pub enabled: bool,
}

fn default_writer_enabled() -> bool {
    true
}

/// Active-active write-planning configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActiveReplicationConfig {
    #[serde(default, skip_serializing_if = "is_read_replica_mode")]
    pub mode: ActiveActiveMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<ActiveActiveCoordinatorConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writers: Vec<ActiveActiveWriterConfig>,
}

impl ActiveActiveReplicationConfig {
    #[must_use]
    pub fn is_active_active(&self) -> bool {
        self.mode.is_active_active()
    }
}

/// Managed coordinator resource parsed from an active-active coordinator URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveActiveCoordinatorResource {
    pub provider: ManagedCoordinatorProvider,
    pub name: String,
}

/// Active-active push request prepared for a managed coordinator.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActivePushPlan {
    pub writer: ActiveActiveWriterConfig,
    pub coordinator_url: String,
    pub request: CommitRequest,
}

/// One regional manifest repair action derived from coordinator truth.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActiveRepairAction {
    pub operation_id: String,
    pub manifest_generation: u64,
    pub region: String,
    pub writer: ActiveActiveWriterConfig,
    pub source_region: String,
    pub refs: Vec<CoordinatedRefUpdate>,
    pub uploaded_objects: Vec<String>,
}

/// Repair plan for committed active-active transactions not materialized everywhere.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActiveRepairPlan {
    pub coordinator_epoch: u64,
    pub actions: Vec<ActiveActiveRepairAction>,
}

/// Validate active-active configuration shape.
///
/// This only validates Crab's coordination contract. A concrete coordinator
/// adapter must still prove the backend is linearizable before writes run.
pub fn validate_active_active_config(replication: &ActiveActiveReplicationConfig) -> Result<()> {
    if !replication.is_active_active() {
        return Ok(());
    }

    let coordinator =
        replication
            .coordinator
            .as_ref()
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator".into(),
                origin: "active-active mode requires a managed coordinator".into(),
            })?;

    if coordinator.url.trim().is_empty() {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.url".into(),
            origin: "active-active coordinator URL must not be empty".into(),
        });
    }
    active_active_coordinator_resource(&coordinator.url)?;
    if coordinator.region.trim().is_empty() {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.region".into(),
            origin: "active-active coordinator region must not be empty".into(),
        });
    }

    if !matches!(
        coordinator.consistency,
        ActiveActiveCoordinatorConsistency::Linearizable
    ) {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.consistency".into(),
            origin: "active-active writes require linearizable coordination".into(),
        });
    }

    let mut seen = HashSet::new();
    let mut enabled_regions = HashSet::new();
    let mut enabled = 0usize;
    for writer in &replication.writers {
        if writer.name.trim().is_empty() {
            return Err(CoordinationError::Configuration {
                key: "replication.writers.name".into(),
                origin: "writer name must not be empty".into(),
            });
        }
        if !seen.insert(writer.name.as_str()) {
            return Err(CoordinationError::Configuration {
                key: "replication.writers.name".into(),
                origin: format!("duplicate writer name {}", writer.name),
            });
        }
        if writer.url.trim().is_empty() || writer.region.trim().is_empty() {
            return Err(CoordinationError::Configuration {
                key: format!("replication.writers.{}", writer.name),
                origin: "writer url and region must not be empty".into(),
            });
        }
        if writer.enabled {
            enabled += 1;
            if !enabled_regions.insert(writer.region.as_str()) {
                return Err(CoordinationError::Configuration {
                    key: "replication.writers.region".into(),
                    origin: format!(
                        "multiple enabled active-active writers are configured for region {}",
                        writer.region
                    ),
                });
            }
        }
    }

    if enabled == 0 {
        return Err(CoordinationError::Configuration {
            key: "replication.writers".into(),
            origin: "active-active mode requires at least one enabled writer".into(),
        });
    }

    Ok(())
}

/// Parse the managed coordinator backend and resource name from a coordinator URL.
///
/// # Errors
///
/// Returns [`CoordinationError::Configuration`] when the URL is missing a
/// supported scheme or resource name.
pub fn active_active_coordinator_resource(url: &str) -> Result<ActiveActiveCoordinatorResource> {
    let (scheme, rest) =
        url.trim()
            .split_once("://")
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator.url".into(),
                origin:
                    "active-active coordinator URL must use dynamodb://, spanner://, or cosmosdb://"
                        .into(),
            })?;
    let provider = match scheme {
        "dynamodb" => ManagedCoordinatorProvider::DynamoDb,
        "spanner" => ManagedCoordinatorProvider::Spanner,
        "cosmosdb" => ManagedCoordinatorProvider::CosmosDb,
        _ => {
            return Err(CoordinationError::Configuration {
                key: "replication.coordinator.url".into(),
                origin: format!(
                    "unsupported active-active coordinator backend {scheme}; expected dynamodb, spanner, or cosmosdb"
                ),
            });
        }
    };
    let name = rest.trim_matches('/').trim();
    if name.is_empty() {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.url".into(),
            origin: "active-active coordinator URL must include a coordinator resource name".into(),
        });
    }
    Ok(ActiveActiveCoordinatorResource {
        provider,
        name: name.to_owned(),
    })
}

/// Build the coordinator commit request for an active-active push.
pub fn plan_active_active_push(
    replication: &ActiveActiveReplicationConfig,
    preferred_writer: Option<&str>,
    manifest_generation: u64,
    refs: Vec<CoordinatedRefUpdate>,
    uploaded_objects: Vec<String>,
) -> Result<ActiveActivePushPlan> {
    validate_active_active_config(replication)?;

    if refs.is_empty() {
        return Err(CoordinationError::Configuration {
            key: "replication.active_active.refs".into(),
            origin: "active-active push requires at least one ref update".into(),
        });
    }

    let coordinator =
        replication
            .coordinator
            .as_ref()
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator".into(),
                origin: "active-active mode requires a managed coordinator".into(),
            })?;
    let writer = select_active_active_writer(replication, preferred_writer)?;
    let target_regions = active_active_target_regions(replication);
    let operation_id = active_active_operation_id(
        &writer,
        &coordinator.url,
        manifest_generation,
        &refs,
        &uploaded_objects,
        &target_regions,
    );

    Ok(ActiveActivePushPlan {
        coordinator_url: coordinator.url.clone(),
        request: CommitRequest {
            operation_id,
            writer: writer.name.clone(),
            region: writer.region.clone(),
            manifest_generation,
            refs,
            uploaded_objects,
            target_regions,
        },
        writer,
    })
}

/// Plan regional manifest repairs from a coordinator snapshot.
pub fn plan_active_active_repair(
    replication: &ActiveActiveReplicationConfig,
    snapshot: &CoordinatorRepairSnapshot,
) -> Result<ActiveActiveRepairPlan> {
    validate_active_active_config(replication)?;

    let mut actions = Vec::with_capacity(snapshot.materialization_gaps.len());
    for gap in &snapshot.materialization_gaps {
        let writer = active_active_writer_for_region(replication, &gap.region)?;
        actions.push(ActiveActiveRepairAction {
            operation_id: gap.operation_id.clone(),
            manifest_generation: gap.manifest_generation,
            region: gap.region.clone(),
            writer,
            source_region: gap.source_region.clone(),
            refs: gap.refs.clone(),
            uploaded_objects: gap.uploaded_objects.clone(),
        });
    }
    actions.sort_by(|left, right| {
        left.operation_id
            .cmp(&right.operation_id)
            .then_with(|| left.region.cmp(&right.region))
            .then_with(|| left.writer.name.cmp(&right.writer.name))
    });
    Ok(ActiveActiveRepairPlan {
        coordinator_epoch: snapshot.coordinator_epoch,
        actions,
    })
}

fn select_active_active_writer(
    replication: &ActiveActiveReplicationConfig,
    preferred_writer: Option<&str>,
) -> Result<ActiveActiveWriterConfig> {
    let preferred_writer = preferred_writer.and_then(|name| {
        let name = name.trim();
        (!name.is_empty()).then_some(name)
    });

    if let Some(name) = preferred_writer {
        let writer = replication
            .writers
            .iter()
            .find(|writer| writer.name == name)
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.writers".into(),
                origin: format!("active-active writer {name} is not configured"),
            })?;
        if !writer.enabled {
            return Err(CoordinationError::Configuration {
                key: format!("replication.writers.{name}"),
                origin: "active-active writer is disabled".into(),
            });
        }
        return Ok(writer.clone());
    }

    replication
        .writers
        .iter()
        .find(|writer| writer.enabled)
        .cloned()
        .ok_or_else(|| CoordinationError::Configuration {
            key: "replication.writers".into(),
            origin: "active-active mode requires at least one enabled writer".into(),
        })
}

/// Select the enabled active-active writer for a materialization region.
pub fn active_active_writer_for_region(
    replication: &ActiveActiveReplicationConfig,
    region: &str,
) -> Result<ActiveActiveWriterConfig> {
    let writer = replication
        .writers
        .iter()
        .find(|writer| writer.region == region)
        .ok_or_else(|| CoordinationError::Configuration {
            key: "replication.writers.region".into(),
            origin: format!("no active-active writer is configured for region {region}"),
        })?;
    if writer.enabled {
        return Ok(writer.clone());
    }
    Err(CoordinationError::Configuration {
        key: format!("replication.writers.{}", writer.name),
        origin: format!(
            "active-active writer {} for region {region} is disabled",
            writer.name
        ),
    })
}

/// Select the active-active writer whose URL matches the push remote.
pub fn active_active_writer_name_for_remote(
    replication: &ActiveActiveReplicationConfig,
    remote_url: Option<&str>,
) -> Result<String> {
    let remote_url = remote_url
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| CoordinationError::Configuration {
            key: "replication.writers".into(),
            origin: "active-active push requires a remote URL that matches an enabled writer"
                .into(),
        })?;

    let matches = replication
        .writers
        .iter()
        .filter(|writer| writer.url.trim() == remote_url)
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(CoordinationError::Configuration {
            key: "replication.writers.url".into(),
            origin: format!("multiple active-active writers use remote URL {remote_url}"),
        });
    }
    if let Some(writer) = matches.first() {
        if writer.enabled {
            return Ok(writer.name.clone());
        }
        return Err(CoordinationError::Configuration {
            key: format!("replication.writers.{}", writer.name),
            origin: format!("active-active writer {} is disabled", writer.name),
        });
    }

    Err(CoordinationError::Configuration {
        key: "replication.writers.url".into(),
        origin: format!(
            "active-active push remote URL {remote_url} does not match any configured enabled writer"
        ),
    })
}

fn active_active_operation_id(
    writer: &ActiveActiveWriterConfig,
    coordinator_url: &str,
    manifest_generation: u64,
    refs: &[CoordinatedRefUpdate],
    uploaded_objects: &[String],
    target_regions: &[String],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, "format", "crab-active-active-push-v1");
    hash_field(&mut hasher, "writer.name", &writer.name);
    hash_field(&mut hasher, "writer.region", &writer.region);
    hash_field(&mut hasher, "writer.url", &writer.url);
    hash_field(&mut hasher, "coordinator.url", coordinator_url);
    hasher.update(&manifest_generation.to_le_bytes());

    let mut refs = refs.to_vec();
    refs.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.expected.cmp(&right.expected))
            .then_with(|| left.new.cmp(&right.new))
            .then_with(|| left.force.cmp(&right.force))
    });
    for update in refs {
        hash_field(&mut hasher, "ref.name", &update.name);
        hash_optional_field(&mut hasher, "ref.expected", update.expected.as_deref());
        hash_optional_field(&mut hasher, "ref.new", update.new.as_deref());
        hasher.update(&[u8::from(update.force)]);
    }
    let mut uploaded_objects = uploaded_objects.to_vec();
    uploaded_objects.sort();
    for key in uploaded_objects {
        hash_field(&mut hasher, "uploaded_object", &key);
    }
    let mut target_regions = target_regions.to_vec();
    target_regions.sort();
    for region in target_regions {
        hash_field(&mut hasher, "target_region", &region);
    }

    format!("crab-op-{}", hasher.finalize().to_hex())
}

fn active_active_target_regions(replication: &ActiveActiveReplicationConfig) -> Vec<String> {
    replication
        .writers
        .iter()
        .filter(|writer| writer.enabled)
        .filter_map(|writer| {
            let region = writer.region.trim();
            (!region.is_empty()).then(|| region.to_owned())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn hash_optional_field(hasher: &mut blake3::Hasher, tag: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hash_field(hasher, tag, value);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn hash_field(hasher: &mut blake3::Hasher, tag: &str, value: &str) {
    hasher.update(&(tag.len() as u64).to_le_bytes());
    hasher.update(tag.as_bytes());
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}
