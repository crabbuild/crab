use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crab_storage::StorageProviderKind;
use object_store::path::Path as ObjectStorePath;
use serde::Deserialize;

use crate::{Error, Result};

const MIN_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 1024 * 1024 * 1024 * 1024 * 1024;

/// Listener, credentials, and logical repository catalog.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    /// Private listener for orchestration liveness and readiness probes.
    pub management_listen: SocketAddr,
    /// Base domain used to accept both path-style and virtual-hosted requests.
    pub endpoint_domain: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    /// Per-process active request budget. Capacity is reserved by operation class.
    #[serde(default = "default_max_in_flight_requests")]
    pub max_in_flight_requests: usize,
    /// Required process-local read cache placement and retention budget.
    pub cache: LocalCacheConfig,
    pub credentials: Vec<CredentialConfig>,
    pub repositories: Vec<RepositoryConfig>,
}

/// Process-local cache policy shared by every repository served by this process.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCacheConfig {
    pub directory: PathBuf,
    pub max_bytes: u64,
}

/// One Crab-issued S3 access key mapped to a logical principal.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    pub access_key: String,
    pub secret_key_file: PathBuf,
    /// Session token and expiry must be configured together for temporary credentials.
    pub session_token_file: Option<PathBuf>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<time::OffsetDateTime>,
    pub principal: String,
}

/// One logical S3 bucket backed by a Crab repository placement.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub name: String,
    #[serde(default = "default_provider")]
    pub provider: StorageProviderKind,
    pub bucket: String,
    pub prefix: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    #[serde(default)]
    pub members: Vec<RepositoryMember>,
    #[serde(default)]
    pub protected_branches: Vec<String>,
    /// Distributed capacity for durable, non-terminal multipart sessions.
    #[serde(default = "default_max_active_multipart_uploads")]
    pub max_active_multipart_uploads: usize,
    /// Maximum registered temporary bytes retained by one multipart session.
    #[serde(default = "default_multipart_staging_bytes_per_upload")]
    pub multipart_staging_bytes_per_upload: u64,
    /// Inactivity-independent lifetime of an open multipart session.
    #[serde(default = "default_multipart_upload_ttl_seconds")]
    pub multipart_upload_ttl_seconds: u64,
}

/// One principal's repository permission.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryMember {
    pub principal: String,
    pub access: RepositoryAccess,
}

/// Repository permission ordered from reads through administration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryAccess {
    Read,
    Write,
    Admin,
}

impl Config {
    /// Read and validate configuration without exposing secret values.
    pub fn read(path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.listen == self.management_listen {
            return Err(Error::Config(
                "listen and management_listen must use different addresses",
            ));
        }
        if self.repositories.is_empty() || self.credentials.is_empty() {
            return Err(Error::Config(
                "configure at least one repository and one credential",
            ));
        }
        if self.region.is_empty()
            || self.region.len() > 63
            || !self
                .region
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(Error::Config("region must be a non-empty AWS region name"));
        }
        if !(8..=4096).contains(&self.max_in_flight_requests) {
            return Err(Error::Config(
                "max_in_flight_requests must be between 8 and 4096",
            ));
        }
        if !self.cache.directory.is_absolute()
            || !(MIN_CACHE_BYTES..=MAX_CACHE_BYTES).contains(&self.cache.max_bytes)
        {
            return Err(Error::Config(
                "cache.directory must be absolute and cache.max_bytes must be between 64 MiB and 1 PiB",
            ));
        }
        if let Some(domain) = self.endpoint_domain.as_deref() {
            s3s::host::SingleDomain::new(domain)?;
        }
        let mut access_keys = HashSet::new();
        let mut configured_principals = HashSet::new();
        for credential in &self.credentials {
            if credential.access_key.len() < 3
                || credential.access_key.len() > 128
                || credential.access_key.chars().any(char::is_whitespace)
                || credential.principal.trim() != credential.principal
                || credential.principal.is_empty()
                || !access_keys.insert(&credential.access_key)
            {
                return Err(Error::Config(
                    "credentials require unique access keys and non-empty principals",
                ));
            }
            let metadata = std::fs::metadata(&credential.secret_key_file)?;
            if !metadata.is_file() {
                return Err(Error::Config("credential secret path must be a file"));
            }
            validate_secret_permissions(&metadata)?;
            match (&credential.session_token_file, credential.expires_at) {
                (Some(path), Some(_)) => {
                    let metadata = std::fs::metadata(path)?;
                    if !metadata.is_file() {
                        return Err(Error::Config(
                            "credential session token path must be a file",
                        ));
                    }
                    validate_secret_permissions(&metadata)?;
                }
                (None, None) => {}
                _ => {
                    return Err(Error::Config(
                        "credential session_token_file and expires_at must be configured together",
                    ));
                }
            }
            configured_principals.insert(credential.principal.as_str());
        }
        let mut names = HashSet::new();
        let mut placements = HashSet::new();
        for repository in &self.repositories {
            if repository.provider == StorageProviderKind::Local {
                return Err(Error::Config(
                    "repository providers must be s3, gcs, or azure",
                ));
            }
            if !valid_bucket_name(&repository.name)
                || !names.insert(repository.name.to_ascii_lowercase())
            {
                return Err(Error::Config(
                    "repository names must be unique valid lowercase S3 bucket names",
                ));
            }
            let canonical_prefix = ObjectStorePath::from(repository.prefix.as_str()).to_string();
            if repository.bucket.is_empty()
                || canonical_prefix.is_empty()
                || !placements.insert((
                    repository.provider,
                    repository.bucket.clone(),
                    canonical_prefix,
                ))
            {
                return Err(Error::Config(
                    "repository bucket/prefix placements must be present and unique",
                ));
            }
            if !(1..=10_000).contains(&repository.max_active_multipart_uploads) {
                return Err(Error::Config(
                    "max_active_multipart_uploads must be between 1 and 10000",
                ));
            }
            if !(5 * 1024 * 1024..=50_000_000_000_000)
                .contains(&repository.multipart_staging_bytes_per_upload)
            {
                return Err(Error::Config(
                    "multipart_staging_bytes_per_upload must be between 5 MiB and 50 TB",
                ));
            }
            if !(1..=365 * 24 * 60 * 60).contains(&repository.multipart_upload_ttl_seconds) {
                return Err(Error::Config(
                    "multipart_upload_ttl_seconds must be between 1 second and 365 days",
                ));
            }
            let branch = format!("refs/heads/{}", repository.default_branch);
            if repository.default_branch.starts_with("refs/")
                || crab_git::validate_push_refname(&branch).is_err()
            {
                return Err(Error::Config("default_branch must be a valid short branch"));
            }
            let mut principals = HashSet::new();
            if repository.members.iter().any(|member| {
                member.principal.trim() != member.principal
                    || member.principal.is_empty()
                    || !configured_principals.contains(member.principal.as_str())
                    || !principals.insert(&member.principal)
            }) {
                return Err(Error::Config(
                    "repository members require unique configured principals",
                ));
            }
            let mut protected = HashSet::new();
            if repository.protected_branches.iter().any(|name| {
                name.starts_with("refs/")
                    || crab_git::validate_push_refname(&format!("refs/heads/{name}")).is_err()
                    || !protected.insert(name)
            }) {
                return Err(Error::Config(
                    "protected branches must be unique valid short branch names",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn validate_secret_permissions(metadata: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    // Kubernetes projects read-only Secrets as root-owned files whose group is
    // the pod fsGroup. Limit that exception to this process's effective group.
    // SAFETY: getegid only reads the calling process's effective identity.
    let process_gid = unsafe { libc::getegid() };
    if !secret_permissions_are_private(metadata.permissions().mode(), metadata.gid(), process_gid) {
        return Err(Error::Config(
            "credential secret files must be owner-only or read-only by the process group",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn secret_permissions_are_private(mode: u32, file_gid: u32, process_gid: u32) -> bool {
    let group_permissions = mode & 0o070;
    mode & 0o007 == 0
        && (group_permissions == 0 || (group_permissions == 0o040 && file_gid == process_gid))
}

#[cfg(not(unix))]
fn validate_secret_permissions(_metadata: &std::fs::Metadata) -> Result<()> {
    Ok(())
}

fn valid_bucket_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && !value.contains("..")
}

fn default_branch() -> String {
    "main".to_owned()
}

fn default_region() -> String {
    "us-east-1".to_owned()
}

fn default_max_in_flight_requests() -> usize {
    32
}

fn default_provider() -> StorageProviderKind {
    StorageProviderKind::S3
}

fn default_max_active_multipart_uploads() -> usize {
    1_024
}

fn default_multipart_staging_bytes_per_upload() -> u64 {
    50_000_000_000_000
}

fn default_multipart_upload_ttl_seconds() -> u64 {
    7 * 24 * 60 * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config(secret_key_file: PathBuf) -> Config {
        Config {
            listen: "127.0.0.1:8080".parse().unwrap(),
            management_listen: "127.0.0.1:8081".parse().unwrap(),
            endpoint_domain: None,
            region: default_region(),
            max_in_flight_requests: default_max_in_flight_requests(),
            cache: LocalCacheConfig {
                directory: std::env::temp_dir().join("crab-s3-gateway-test-cache"),
                max_bytes: 128 * 1024 * 1024,
            },
            credentials: vec![CredentialConfig {
                access_key: "test-access-key".to_owned(),
                secret_key_file,
                session_token_file: None,
                expires_at: None,
                principal: "test-principal".to_owned(),
            }],
            repositories: vec![RepositoryConfig {
                name: "test-repository".to_owned(),
                provider: StorageProviderKind::S3,
                bucket: "test-storage".to_owned(),
                prefix: "repositories/test".to_owned(),
                default_branch: default_branch(),
                members: vec![RepositoryMember {
                    principal: "test-principal".to_owned(),
                    access: RepositoryAccess::Write,
                }],
                protected_branches: Vec::new(),
                max_active_multipart_uploads: default_max_active_multipart_uploads(),
                multipart_staging_bytes_per_upload: default_multipart_staging_bytes_per_upload(),
                multipart_upload_ttl_seconds: default_multipart_upload_ttl_seconds(),
            }],
        }
    }

    #[test]
    fn logical_bucket_names_follow_the_frozen_profile() {
        for value in ["abc", "a-b.c9", &format!("a{}z", "b".repeat(61))] {
            assert!(valid_bucket_name(value));
        }
        for value in ["ab", "UPPER", "-abc", "abc-", "a..b", "a_b"] {
            assert!(!valid_bucket_name(value));
        }
    }

    #[test]
    fn request_budget_rejects_unsafe_extremes() {
        let secret = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(secret.path(), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let mut config = valid_config(secret.path().to_owned());
        for value in [8, 32, 4096] {
            config.max_in_flight_requests = value;
            config.validate().unwrap();
        }
        for value in [0, 7, 4097, usize::MAX] {
            config.max_in_flight_requests = value;
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn management_listener_must_be_separate() {
        let secret = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(secret.path(), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let mut config = valid_config(secret.path().to_owned());
        config.management_listen = config.listen;
        assert!(config.validate().is_err());
    }

    #[test]
    fn physical_placements_reject_equivalent_object_store_prefixes() {
        let secret = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(secret.path(), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let mut config = valid_config(secret.path().to_owned());
        config.repositories[0].prefix = "repositories//test".to_owned();
        let mut equivalent = config.repositories[0].clone();
        equivalent.name = "other-repository".to_owned();
        equivalent.prefix = "repositories/test".to_owned();
        config.repositories.push(equivalent);

        assert!(config.validate().is_err());
    }

    #[test]
    fn local_cache_requires_explicit_bounded_absolute_placement() {
        let secret = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(secret.path(), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let mut config = valid_config(secret.path().to_owned());
        config.cache.directory = PathBuf::from("relative-cache");
        let relative = config.validate();
        config.cache.directory = std::env::temp_dir().join("crab-s3-gateway-test-cache");
        config.cache.max_bytes = MIN_CACHE_BYTES - 1;
        let undersized = config.validate();
        config.cache.max_bytes = MAX_CACHE_BYTES + 1;
        let oversized = config.validate();

        assert!(relative.is_err() && undersized.is_err() && oversized.is_err());
    }

    #[test]
    fn multipart_capacity_rejects_unbounded_or_invalid_policy() {
        let secret = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(secret.path(), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let mut config = valid_config(secret.path().to_owned());
        config.repositories[0].max_active_multipart_uploads = 0;
        let no_sessions = config.validate();
        config.repositories[0].max_active_multipart_uploads = 1;
        config.repositories[0].multipart_staging_bytes_per_upload = 5 * 1024 * 1024 - 1;
        let undersized_staging = config.validate();
        config.repositories[0].multipart_staging_bytes_per_upload = 5 * 1024 * 1024;
        config.repositories[0].multipart_upload_ttl_seconds = 0;
        let no_expiry = config.validate();

        assert!(no_sessions.is_err() && undersized_staging.is_err() && no_expiry.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn credential_secret_accepts_owner_only_access() {
        assert!(secret_permissions_are_private(0o600, 10, 20));
    }

    #[cfg(unix)]
    #[test]
    fn credential_secret_accepts_read_access_by_process_group() {
        assert!(secret_permissions_are_private(0o440, 20, 20));
    }

    #[cfg(unix)]
    #[test]
    fn credential_secret_rejects_broader_access() {
        let insecure = [
            secret_permissions_are_private(0o440, 10, 20),
            secret_permissions_are_private(0o460, 20, 20),
            secret_permissions_are_private(0o444, 20, 20),
        ];
        assert!(insecure.into_iter().all(|accepted| !accepted));
    }

    #[test]
    fn temporary_credentials_require_token_and_expiry_together() {
        let secret = tempfile::NamedTempFile::new().unwrap();
        let token = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        for path in [secret.path(), token.path()] {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut config = valid_config(secret.path().to_owned());
        config.credentials[0].session_token_file = Some(token.path().to_owned());
        assert!(config.validate().is_err());
        config.credentials[0].expires_at = Some(time::OffsetDateTime::now_utc());
        config.validate().unwrap();
        config.credentials[0].session_token_file = None;
        assert!(config.validate().is_err());
    }
}
