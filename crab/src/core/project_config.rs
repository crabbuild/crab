//! `crab.toml` project configuration.
//!
//! [`ProjectConfig`] is the user-facing, repo-committed configuration file
//! that travels with the repository. It declares the remote URL, tracking
//! patterns, hydration behavior, mirror settings, and optional auth hints.
//!
//! This is distinct from `.crab/local.toml`, which stores durable
//! machine-specific choices and is never committed. Runtime state such as
//! staging, caches, journals, and locks lives elsewhere under `.crab/`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::config::{StorageProvider, WorkflowOverlay};
use crate::core::error::{CrabError, Result};
use crab_types::replication::ReplicationConfig;

// ---------------------------------------------------------------------------
// Section structs
// ---------------------------------------------------------------------------

/// Remote storage location (required).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    pub url: String,
}

/// File patterns to track with the crab filter driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackConfig {
    pub patterns: Vec<String>,
}

/// Hydration mode: lazy (pointer until accessed) or eager (materialize immediately).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HydrateMode {
    Lazy,
    Eager,
}

/// Hydration behavior on clone/checkout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HydrateConfig {
    pub default: HydrateMode,
    pub auto_patterns: Option<Vec<String>>,
}

/// Mirror mode: keep a git remote (e.g. GitHub) in sync with a crab remote.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorConfig {
    pub origin_remote: String,
    pub crab_remote: String,
}

/// Project-level auth hints. Named `ProjectAuthConfig` to avoid collision
/// with the existing `AuthConfig` in `core/config.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAuthConfig {
    pub storage_provider: Option<StorageProvider>,
}

/// Named paths that Crab should hydrate together.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefetchProfileConfig {
    pub paths: Vec<String>,
}

/// Shared prefetch profiles committed with the repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefetchConfig {
    pub profiles: BTreeMap<String, PrefetchProfileConfig>,
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// Top-level `crab.toml` project configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default = "default_config_version")]
    pub version: u32,
    pub remote: RemoteConfig,
    pub track: Option<TrackConfig>,
    pub hydrate: Option<HydrateConfig>,
    pub mirror: Option<MirrorConfig>,
    pub replication: Option<ReplicationConfig>,
    pub auth: Option<ProjectAuthConfig>,
    pub prefetch: Option<PrefetchConfig>,
    pub workflow: Option<WorkflowOverlay>,
}

// ---------------------------------------------------------------------------
// File name constant
// ---------------------------------------------------------------------------

/// The well-known config file name searched by [`ProjectConfig::load_for_repo`].
pub const CONFIG_FILE_NAME: &str = "crab.toml";

const CONFIG_VERSION: u32 = 1;

/// Header comment prepended when writing the config file.
const HEADER_COMMENT: &str = "# Crab project configuration\n\n";

const fn default_config_version() -> u32 {
    CONFIG_VERSION
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl ProjectConfig {
    /// Load and deserialize a `crab.toml` from the given path.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!("failed to read crab.toml: {e}"),
        })?;

        Self::parse(&content, &path.display().to_string())
    }

    /// Parse and validate a `crab.toml` document from an in-memory source.
    pub fn parse(content: &str, origin: &str) -> Result<Self> {
        let config: Self = toml::from_str(content).map_err(|e| CrabError::Configuration {
            key: CONFIG_FILE_NAME.to_owned(),
            origin: format!("failed to parse {origin}: {e}"),
        })?;
        if config.version != CONFIG_VERSION {
            return Err(CrabError::IncompatibleFormat {
                required: format!("crab.toml version {CONFIG_VERSION}"),
                found: config.version.to_string(),
            });
        }
        Ok(config)
    }

    /// Load the project configuration discovered from `start`.
    ///
    /// The retired `.crab.toml` filename is ignored.
    pub fn load_for_repo(start: &Path) -> Result<Option<Self>> {
        let mut current: &Path = start;
        loop {
            let candidate = current.join(CONFIG_FILE_NAME);
            if candidate.is_file() {
                return Self::load(&candidate).map(Some);
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => return Ok(None),
            }
        }
    }

    /// Resolve the canonical repository URL from the committed project file.
    pub fn remote_url(start: &Path) -> Result<String> {
        let config = Self::load_for_repo(start)?.ok_or_else(|| CrabError::Configuration {
            key: "remote.url".to_owned(),
            origin: format!("{CONFIG_FILE_NAME} was not found; run `crab configure <REMOTE>`"),
        })?;
        if config.remote.url.trim().is_empty() {
            return Err(CrabError::Configuration {
                key: "remote.url".to_owned(),
                origin: format!("{CONFIG_FILE_NAME} must declare a non-empty [remote].url"),
            });
        }
        Ok(config.remote.url)
    }

    /// Serialize `config` to TOML and write it to `path` with a header comment.
    pub fn write(path: &Path, config: &ProjectConfig) -> Result<()> {
        let body = toml::to_string_pretty(config).map_err(|e| CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!("failed to serialize crab.toml: {e}"),
        })?;

        let content = format!("{HEADER_COMMENT}{body}");

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CrabError::Configuration {
                key: path.display().to_string(),
                origin: format!("failed to create parent directory: {e}"),
            })?;
        }

        std::fs::write(path, content).map_err(|e| CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!("failed to write crab.toml: {e}"),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crab_types::replication::{ReplicaConfig, ReplicationProviderKind, ReplicationRpo};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn round_trip_full_config() {
        let config = ProjectConfig {
            version: CONFIG_VERSION,
            remote: RemoteConfig {
                url: "crab://my-bucket/my-repo".to_string(),
            },
            track: Some(TrackConfig {
                patterns: vec!["*.bin".to_string(), "*.safetensors".to_string()],
            }),
            hydrate: Some(HydrateConfig {
                default: HydrateMode::Lazy,
                auto_patterns: Some(vec!["*.py".to_string(), "*.rs".to_string()]),
            }),
            mirror: Some(MirrorConfig {
                origin_remote: "origin".to_string(),
                crab_remote: "crab".to_string(),
            }),
            replication: Some(ReplicationConfig {
                primary: Some("crab://my-bucket/my-repo".to_string()),
                replicas: vec![ReplicaConfig {
                    name: "west".to_string(),
                    provider: ReplicationProviderKind::S3,
                    url: "s3://my-bucket-west/my-repo".to_string(),
                    region: "us-west-2".to_string(),
                    backfill: false,
                    read: true,
                    rpo: ReplicationRpo::Fast,
                }],
                ..ReplicationConfig::default()
            }),
            auth: Some(ProjectAuthConfig {
                storage_provider: Some(crate::core::config::StorageProvider::S3),
            }),
            prefetch: None,
            workflow: None,
        };

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("crab.toml");

        ProjectConfig::write(&path, &config).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("# Crab project configuration"));

        let loaded = ProjectConfig::load(&path).unwrap();
        assert_eq!(loaded.remote.url, "crab://my-bucket/my-repo");
        assert_eq!(
            loaded.track.as_ref().unwrap().patterns,
            vec!["*.bin", "*.safetensors"]
        );
        assert!(matches!(
            loaded.hydrate.as_ref().unwrap().default,
            HydrateMode::Lazy
        ));
        assert_eq!(loaded.mirror.as_ref().unwrap().origin_remote, "origin");
        assert_eq!(
            loaded.replication.as_ref().unwrap().replicas[0].provider,
            ReplicationProviderKind::S3
        );
        assert_eq!(
            loaded.auth.as_ref().unwrap().storage_provider,
            Some(crate::core::config::StorageProvider::S3)
        );
    }

    #[test]
    fn partial_config_only_remote() {
        let toml_content = r#"
[remote]
url = "crab://bucket/repo"
"#;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("crab.toml");
        fs::write(&path, toml_content).unwrap();

        let config = ProjectConfig::load(&path).unwrap();
        assert_eq!(config.remote.url, "crab://bucket/repo");
        assert!(config.track.is_none());
        assert!(config.hydrate.is_none());
        assert!(config.mirror.is_none());
        assert!(config.auth.is_none());
    }

    #[test]
    fn discover_walks_up_directories() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Write config at root
        let config = ProjectConfig {
            version: CONFIG_VERSION,
            remote: RemoteConfig {
                url: "crab://found/it".to_string(),
            },
            track: None,
            hydrate: None,
            mirror: None,
            replication: None,
            auth: None,
            prefetch: None,
            workflow: None,
        };
        ProjectConfig::write(&root.join("crab.toml"), &config).unwrap();

        // Create nested directory
        let nested = root.join("a").join("b").join("c");
        fs::create_dir_all(&nested).unwrap();

        // Discover from nested should find root config
        let found = ProjectConfig::load_for_repo(&nested).unwrap().unwrap();
        assert_eq!(found.remote.url, "crab://found/it");
    }

    #[test]
    fn discover_returns_none_when_missing() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("no").join("config").join("here");
        fs::create_dir_all(&nested).unwrap();

        assert!(ProjectConfig::load_for_repo(&nested).unwrap().is_none());
    }

    #[test]
    fn retired_hidden_config_is_ignored() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".crab.toml"),
            "[remote]\nurl = \"crab://retired/repo\"\n",
        )
        .unwrap();

        assert!(ProjectConfig::load_for_repo(dir.path()).unwrap().is_none());
    }

    #[test]
    fn project_config_rejects_local_aws_profile() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            "[remote]\nurl = \"crab://bucket/repo\"\n\n[auth]\nprofile = \"local-only\"\n",
        )
        .unwrap();

        assert!(ProjectConfig::load(&path).is_err());
    }

    #[test]
    fn hydrate_mode_eager_round_trip() {
        let toml_content = r#"
[remote]
url = "crab://bucket/repo"

[hydrate]
default = "eager"
"#;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("crab.toml");
        fs::write(&path, toml_content).unwrap();

        let config = ProjectConfig::load(&path).unwrap();
        assert!(matches!(
            config.hydrate.as_ref().unwrap().default,
            HydrateMode::Eager
        ));
    }
}
