//! Shared replication configuration contracts.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::storage::StorageProviderKind;

/// Cloud provider backing a replication target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReplicationProviderKind {
    S3,
    Gcs,
    Azure,
}

impl ReplicationProviderKind {
    /// Parse a user-facing replication provider alias.
    pub fn parse(value: &str) -> Result<Self, ReplicationParseError> {
        match value.to_ascii_lowercase().as_str() {
            "s3" | "aws" => Ok(Self::S3),
            "gcs" | "gs" | "google" => Ok(Self::Gcs),
            "azure" | "az" => Ok(Self::Azure),
            other => Err(ReplicationParseError::new(
                format!("unsupported replication provider {other:?}"),
                "replication.provider",
            )),
        }
    }

    /// Stable lowercase provider label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Gcs => "gcs",
            Self::Azure => "azure",
        }
    }

    /// Return the storage provider backing this replication provider.
    #[must_use]
    pub fn storage_provider_kind(self) -> StorageProviderKind {
        match self {
            Self::S3 => StorageProviderKind::S3,
            Self::Gcs => StorageProviderKind::Gcs,
            Self::Azure => StorageProviderKind::Azure,
        }
    }

    /// Return the replication provider represented by a cloud storage provider.
    #[must_use]
    pub fn from_storage_provider_kind(provider: StorageProviderKind) -> Option<Self> {
        match provider {
            StorageProviderKind::S3 => Some(Self::S3),
            StorageProviderKind::Gcs => Some(Self::Gcs),
            StorageProviderKind::Azure => Some(Self::Azure),
            StorageProviderKind::Local => None,
        }
    }
}

impl fmt::Display for ReplicationProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Requested replication recovery profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReplicationRpo {
    Standard,
    Fast,
}

impl ReplicationRpo {
    /// Parse a user-facing recovery profile.
    pub fn parse(value: &str) -> Result<Self, ReplicationParseError> {
        match value.to_ascii_lowercase().as_str() {
            "standard" => Ok(Self::Standard),
            "fast" => Ok(Self::Fast),
            other => Err(ReplicationParseError::new(
                format!("unsupported replication rpo {other:?}"),
                "replication.rpo",
            )),
        }
    }

    /// Stable lowercase RPO label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Fast => "fast",
        }
    }
}

impl fmt::Display for ReplicationRpo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Top-level replication settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReplicationConfig {
    /// Primary write remote. If absent, `[remote].url` remains authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    /// Replication write model.
    #[serde(default, skip_serializing_if = "is_read_replica_mode")]
    pub mode: ReplicationMode,
    /// Linearizable coordinator for active-active writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<ReplicationCoordinatorConfig>,
    /// Regional write ingress endpoints for active-active mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writers: Vec<WriterConfig>,
    /// Read replicas available for primary fallback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replicas: Vec<ReplicaConfig>,
}

impl ReplicationConfig {
    /// Returns whether any configured replica is enabled for reads.
    #[must_use]
    pub fn has_read_replicas(&self) -> bool {
        self.replicas.iter().any(|replica| replica.read)
    }

    /// Returns whether the repository uses active-active write mode.
    #[must_use]
    pub fn is_active_active(&self) -> bool {
        self.mode == ReplicationMode::ActiveActive
    }
}

/// Replication write model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReplicationMode {
    #[default]
    ReadReplica,
    ActiveActive,
}

impl ReplicationMode {
    /// Stable kebab-case mode label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadReplica => "read-replica",
            Self::ActiveActive => "active-active",
        }
    }

    /// Returns whether the mode keeps writes on the primary.
    #[must_use]
    pub fn is_read_replica(self) -> bool {
        self == Self::ReadReplica
    }
}

impl fmt::Display for ReplicationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive field values by reference"
)]
fn is_read_replica_mode(mode: &ReplicationMode) -> bool {
    *mode == ReplicationMode::ReadReplica
}

/// Active-active coordinator backend kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReplicationCoordinatorKind {
    #[default]
    Managed,
}

/// Active-active coordinator consistency contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReplicationCoordinatorConsistency {
    #[default]
    Linearizable,
}

/// Linearizable coordinator configuration for active-active writes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReplicationCoordinatorConfig {
    #[serde(default)]
    pub kind: ReplicationCoordinatorKind,
    pub url: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failover_regions: Vec<String>,
    #[serde(default)]
    pub consistency: ReplicationCoordinatorConsistency,
}

/// Regional active-active writer endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WriterConfig {
    pub name: String,
    pub url: String,
    pub region: String,
    #[serde(default = "default_writer_enabled")]
    pub enabled: bool,
}

fn default_writer_enabled() -> bool {
    true
}

/// One configured read replica.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReplicaConfig {
    pub name: String,
    pub provider: ReplicationProviderKind,
    pub url: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub backfill: bool,
    #[serde(default = "default_read_enabled")]
    pub read: bool,
    #[serde(default = "default_rpo")]
    pub rpo: ReplicationRpo,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive field values by reference"
)]
fn is_false(value: &bool) -> bool {
    !*value
}

fn default_read_enabled() -> bool {
    false
}

fn default_rpo() -> ReplicationRpo {
    ReplicationRpo::Standard
}

/// Parse failure for replication config enum aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationParseError {
    key: String,
    origin: String,
}

impl ReplicationParseError {
    fn new(key: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            origin: origin.into(),
        }
    }

    /// Config key or field associated with the parse failure.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Human-readable source or reason for the parse failure.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

impl fmt::Display for ReplicationParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.origin, self.key)
    }
}

impl std::error::Error for ReplicationParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_aliases_parse() {
        assert_eq!(
            ReplicationProviderKind::parse("aws").unwrap(),
            ReplicationProviderKind::S3
        );
        assert_eq!(
            ReplicationProviderKind::parse("gs").unwrap(),
            ReplicationProviderKind::Gcs
        );
        assert_eq!(
            ReplicationProviderKind::parse("az").unwrap(),
            ReplicationProviderKind::Azure
        );
    }

    #[test]
    fn provider_parse_error_keeps_config_origin() {
        let err = ReplicationProviderKind::parse("oracle").unwrap_err();
        assert_eq!(err.key(), "unsupported replication provider \"oracle\"");
        assert_eq!(err.origin(), "replication.provider");
    }

    #[test]
    fn provider_maps_to_storage_provider() {
        assert_eq!(
            ReplicationProviderKind::S3.storage_provider_kind(),
            StorageProviderKind::S3
        );
        assert_eq!(
            ReplicationProviderKind::Gcs.storage_provider_kind(),
            StorageProviderKind::Gcs
        );
        assert_eq!(
            ReplicationProviderKind::Azure.storage_provider_kind(),
            StorageProviderKind::Azure
        );
    }

    #[test]
    fn storage_provider_maps_to_replication_provider() {
        assert_eq!(
            ReplicationProviderKind::from_storage_provider_kind(StorageProviderKind::S3),
            Some(ReplicationProviderKind::S3)
        );
        assert_eq!(
            ReplicationProviderKind::from_storage_provider_kind(StorageProviderKind::Gcs),
            Some(ReplicationProviderKind::Gcs)
        );
        assert_eq!(
            ReplicationProviderKind::from_storage_provider_kind(StorageProviderKind::Azure),
            Some(ReplicationProviderKind::Azure)
        );
        assert_eq!(
            ReplicationProviderKind::from_storage_provider_kind(StorageProviderKind::Local),
            None
        );
    }

    #[test]
    fn rpo_aliases_parse() {
        assert_eq!(
            ReplicationRpo::parse("standard").unwrap(),
            ReplicationRpo::Standard
        );
        assert_eq!(ReplicationRpo::parse("fast").unwrap(), ReplicationRpo::Fast);
    }

    #[test]
    fn config_helpers_report_enabled_modes() {
        let mut config = ReplicationConfig::default();
        assert!(!config.has_read_replicas());
        assert!(!config.is_active_active());

        config.mode = ReplicationMode::ActiveActive;
        config.replicas.push(ReplicaConfig {
            name: "west".into(),
            provider: ReplicationProviderKind::S3,
            url: "s3://bucket/repo".into(),
            region: "us-west-2".into(),
            backfill: false,
            read: true,
            rpo: ReplicationRpo::Standard,
        });

        assert!(config.has_read_replicas());
        assert!(config.is_active_active());
    }
}
