//! Runtime wiring for tier lifecycle and restore providers.
//!
//! This module is intentionally small: it connects resolved config and
//! repository remote metadata to the provider-neutral tier interfaces.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crab_types::storage::StorageProviderKind;

use crate::core::config::{AuthProvider, Config};
use crate::core::error::{CrabError, Result};
use crate::git::url::CrabUrl;

use super::plan::BucketProbe;
use super::provider::{LifecycleProvider, Provider, RestoreBackend, RestoreTier};
use super::restore::RestoreOptions;

/// Read and parse the current repository's `crab://` remote URL.
pub fn current_crab_url() -> Result<CrabUrl> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    let repo_root = crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)
        .map(|worktree| worktree.current_worktree_root)
        .unwrap_or(cwd);
    let remote = read_crab_remote_url(&repo_root)?;
    CrabUrl::parse(&remote)
}

/// Resolve the tier provider from auth/storage config.
pub fn resolve_provider(config: &Config) -> Provider {
    match config.auth.provider {
        AuthProvider::AwsOidc => Provider::S3,
        AuthProvider::GcpWorkloadIdentity => Provider::Gcs,
        AuthProvider::AzureEntra => Provider::Azure,
        AuthProvider::CrabAuth | AuthProvider::Static | AuthProvider::None => {
            let provider = config
                .auth
                .storage_provider
                .storage_provider_kind()
                .unwrap_or_else(crab_storage::resolve_static_env_provider);
            tier_provider_from_storage_kind(provider)
        }
    }
}

fn tier_provider_from_storage_kind(provider: StorageProviderKind) -> Provider {
    match provider {
        StorageProviderKind::S3 => Provider::S3,
        StorageProviderKind::Gcs => Provider::Gcs,
        StorageProviderKind::Azure => Provider::Azure,
        StorageProviderKind::Local => Provider::S3,
    }
}

/// Build the lifecycle provider for the configured backend.
pub async fn build_lifecycle_provider(
    config: &Config,
    url: &CrabUrl,
) -> Result<Box<dyn LifecycleProvider>> {
    match resolve_provider(config) {
        Provider::S3 => build_s3_lifecycle_provider(config, url),
        Provider::Gcs => build_gcs_lifecycle_provider(url).await,
        Provider::Azure => build_azure_lifecycle_provider(config, url),
    }
}

/// Build the restore backend for the configured backend.
pub async fn build_restore_backend(
    config: &Config,
    url: &CrabUrl,
) -> Result<Arc<dyn RestoreBackend>> {
    match resolve_provider(config) {
        Provider::S3 => build_s3_restore_backend(config, url),
        Provider::Gcs => build_gcs_restore_backend(url).await,
        Provider::Azure => build_azure_restore_backend(config, url),
    }
}

/// Probe bucket state required by lifecycle planning.
pub async fn probe_bucket(
    config: &Config,
    url: &CrabUrl,
    provider: &dyn LifecycleProvider,
) -> Result<BucketProbe> {
    match provider.kind() {
        Provider::S3 => probe_s3_bucket(config, url).await,
        Provider::Gcs | Provider::Azure => Ok(BucketProbe {
            provider: provider.kind(),
            versioning_enabled: false,
            object_lock_enabled: false,
            existing_rule_ids: provider
                .get()
                .await?
                .map_or_else(Vec::new, |doc| doc.rule_ids),
        }),
    }
}

/// Build restore options from `[tier]` config.
pub fn restore_options_from_config(config: &Config) -> Result<RestoreOptions> {
    Ok(RestoreOptions {
        tier: parse_restore_tier(&config.tier.restore_tier)?,
        duration: Duration::from_secs(u64::from(config.tier.restore_duration_days) * 86_400),
    })
}

/// Parse a configured restore tier.
pub fn parse_restore_tier(raw: &str) -> Result<RestoreTier> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "expedited" => Ok(RestoreTier::Expedited),
        "standard" => Ok(RestoreTier::Standard),
        "bulk" => Ok(RestoreTier::Bulk),
        "high" => Ok(RestoreTier::High),
        other => Err(CrabError::Configuration {
            key: format!(
                "invalid tier.restore_tier '{other}' (expected expedited, standard, bulk, or high)"
            ),
            origin: "tier.restore_tier".into(),
        }),
    }
}

fn read_crab_remote_url(repo_root: &Path) -> Result<String> {
    if let Some(url) = git_origin_crab_url(repo_root) {
        return Ok(url);
    }

    let crab_dir = repo_root.join(".crab");
    let config_path = crab_dir.join("config.toml");
    if config_path.is_file() {
        let content =
            std::fs::read_to_string(&config_path).map_err(|e| CrabError::Configuration {
                key: format!("failed to read {}: {e}", config_path.display()),
                origin: config_path.display().to_string(),
            })?;
        let table: toml::Table = content.parse().map_err(|e| CrabError::Configuration {
            key: format!("failed to parse {}: {e}", config_path.display()),
            origin: config_path.display().to_string(),
        })?;
        if let Some(url) = table
            .get("remote")
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
        {
            return Ok(url.to_owned());
        }
    }

    let remote_path = crab_dir.join("remote");
    if remote_path.is_file() {
        let remote =
            std::fs::read_to_string(&remote_path).map_err(|e| CrabError::Configuration {
                key: format!("failed to read {}: {e}", remote_path.display()),
                origin: remote_path.display().to_string(),
            })?;
        let trimmed = remote.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }

    Err(CrabError::Configuration {
        key: "no crab remote configured".into(),
        origin: repo_root.display().to_string(),
    })
}

fn git_origin_crab_url(repo_root: &Path) -> Option<String> {
    let mut command = std::process::Command::new("git");
    command
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_QUARANTINE_PATH")
        .env_remove("GIT_NAMESPACE");
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    url.starts_with("crab://").then_some(url)
}

fn aws_region(config: &Config) -> String {
    config
        .auth
        .aws
        .region
        .clone()
        .or_else(|| std::env::var("AWS_REGION").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .unwrap_or_else(|| "us-east-1".into())
}

#[cfg(feature = "tier-s3")]
fn build_s3_lifecycle_provider(
    config: &Config,
    url: &CrabUrl,
) -> Result<Box<dyn LifecycleProvider>> {
    Ok(Box::new(super::provider::s3::S3LifecycleProvider::new(
        url.bucket.clone(),
        aws_region(config),
    )))
}

#[cfg(not(feature = "tier-s3"))]
fn build_s3_lifecycle_provider(
    _config: &Config,
    _url: &CrabUrl,
) -> Result<Box<dyn LifecycleProvider>> {
    Err(CrabError::TierProviderUnsupported {
        provider: "s3 (crate built without tier-s3 feature)".into(),
    })
}

#[cfg(feature = "tier-s3")]
fn build_s3_restore_backend(config: &Config, url: &CrabUrl) -> Result<Arc<dyn RestoreBackend>> {
    Ok(Arc::new(super::provider::s3::S3LifecycleProvider::new(
        url.bucket.clone(),
        aws_region(config),
    )))
}

#[cfg(not(feature = "tier-s3"))]
fn build_s3_restore_backend(_config: &Config, _url: &CrabUrl) -> Result<Arc<dyn RestoreBackend>> {
    Err(CrabError::TierProviderUnsupported {
        provider: "s3 restore (crate built without tier-s3 feature)".into(),
    })
}

#[cfg(feature = "tier-gcs")]
async fn build_gcs_lifecycle_provider(url: &CrabUrl) -> Result<Box<dyn LifecycleProvider>> {
    Ok(Box::new(
        super::provider::gcs::GcsLifecycleProvider::new(url.bucket.clone()).await?,
    ))
}

#[cfg(not(feature = "tier-gcs"))]
async fn build_gcs_lifecycle_provider(_url: &CrabUrl) -> Result<Box<dyn LifecycleProvider>> {
    Err(CrabError::TierProviderUnsupported {
        provider: "gcs (crate built without tier-gcs feature)".into(),
    })
}

#[cfg(feature = "tier-gcs")]
async fn build_gcs_restore_backend(url: &CrabUrl) -> Result<Arc<dyn RestoreBackend>> {
    Ok(Arc::new(
        super::provider::gcs::GcsLifecycleProvider::new(url.bucket.clone()).await?,
    ))
}

#[cfg(not(feature = "tier-gcs"))]
async fn build_gcs_restore_backend(_url: &CrabUrl) -> Result<Arc<dyn RestoreBackend>> {
    Err(CrabError::TierProviderUnsupported {
        provider: "gcs restore (crate built without tier-gcs feature)".into(),
    })
}

#[cfg(feature = "tier-azure")]
fn build_azure_lifecycle_provider(
    config: &Config,
    url: &CrabUrl,
) -> Result<Box<dyn LifecycleProvider>> {
    let account = azure_storage_account(config)?;
    Ok(Box::new(
        super::provider::azure::AzureLifecycleProvider::from_env(account, url.bucket.clone())?,
    ))
}

#[cfg(not(feature = "tier-azure"))]
fn build_azure_lifecycle_provider(
    _config: &Config,
    _url: &CrabUrl,
) -> Result<Box<dyn LifecycleProvider>> {
    Err(CrabError::TierProviderUnsupported {
        provider: "azure (crate built without tier-azure feature)".into(),
    })
}

#[cfg(feature = "tier-azure")]
fn build_azure_restore_backend(config: &Config, url: &CrabUrl) -> Result<Arc<dyn RestoreBackend>> {
    let account = azure_storage_account(config)?;
    Ok(Arc::new(
        super::provider::azure::AzureLifecycleProvider::from_env(account, url.bucket.clone())?,
    ))
}

#[cfg(not(feature = "tier-azure"))]
fn build_azure_restore_backend(
    _config: &Config,
    _url: &CrabUrl,
) -> Result<Arc<dyn RestoreBackend>> {
    Err(CrabError::TierProviderUnsupported {
        provider: "azure restore (crate built without tier-azure feature)".into(),
    })
}

#[cfg(feature = "tier-azure")]
fn azure_storage_account(config: &Config) -> Result<String> {
    config
        .auth
        .azure
        .storage_account
        .clone()
        .or_else(|| std::env::var("AZURE_STORAGE_ACCOUNT").ok())
        .ok_or_else(|| CrabError::Configuration {
            key: "missing Azure storage account for tier provider".into(),
            origin: "auth.azure.storage_account or AZURE_STORAGE_ACCOUNT".into(),
        })
}

#[cfg(feature = "tier-s3")]
async fn probe_s3_bucket(config: &Config, url: &CrabUrl) -> Result<BucketProbe> {
    use aws_sdk_s3::types::BucketVersioningStatus;

    let s3 = super::provider::s3::S3LifecycleProvider::new(url.bucket.clone(), aws_region(config));
    let versioning = s3
        .client()
        .get_bucket_versioning()
        .bucket(s3.bucket())
        .send()
        .await
        .map_err(|e| CrabError::Internal(format!("S3 get bucket versioning failed: {e}")))?;
    let versioning_enabled = matches!(versioning.status(), Some(BucketVersioningStatus::Enabled));

    let object_lock_enabled = match s3
        .client()
        .get_object_lock_configuration()
        .bucket(s3.bucket())
        .send()
        .await
    {
        Ok(_) => true,
        Err(err) => {
            let service_err = err.into_service_error();
            if service_err.meta().code().is_some_and(|code| {
                matches!(
                    code,
                    "ObjectLockConfigurationNotFoundError" | "NoSuchObjectLockConfiguration"
                )
            }) {
                false
            } else {
                return Err(CrabError::Internal(format!(
                    "S3 get object-lock configuration failed: {service_err}"
                )));
            }
        }
    };

    Ok(BucketProbe {
        provider: Provider::S3,
        versioning_enabled,
        object_lock_enabled,
        existing_rule_ids: s3.get().await?.map_or_else(Vec::new, |doc| doc.rule_ids),
    })
}

#[cfg(not(feature = "tier-s3"))]
async fn probe_s3_bucket(_config: &Config, _url: &CrabUrl) -> Result<BucketProbe> {
    Err(CrabError::TierProviderUnsupported {
        provider: "s3 bucket probe (crate built without tier-s3 feature)".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_restore_tier_accepts_known_values() {
        assert_eq!(
            parse_restore_tier("standard").unwrap(),
            RestoreTier::Standard
        );
        assert_eq!(
            parse_restore_tier("EXPEDITED").unwrap(),
            RestoreTier::Expedited
        );
        assert_eq!(parse_restore_tier("bulk").unwrap(), RestoreTier::Bulk);
        assert_eq!(parse_restore_tier("high").unwrap(), RestoreTier::High);
    }

    #[test]
    fn parse_restore_tier_rejects_unknown_values() {
        assert!(parse_restore_tier("overnight").is_err());
    }
}
