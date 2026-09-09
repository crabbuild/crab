use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crab_storage::StorageProviderKind;
use serde::Deserialize;

use crate::{Error, Result};

/// Listener, credentials, and logical repository catalog.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    /// Base domain used to accept both path-style and virtual-hosted requests.
    pub endpoint_domain: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    pub credentials: Vec<CredentialConfig>,
    pub repositories: Vec<RepositoryConfig>,
}

/// One Crab-issued S3 access key mapped to a logical principal.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    pub access_key: String,
    pub secret_key_file: PathBuf,
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
            if repository.bucket.is_empty()
                || repository.prefix.is_empty()
                || !placements.insert((repository.provider, &repository.bucket, &repository.prefix))
            {
                return Err(Error::Config(
                    "repository bucket/prefix placements must be present and unique",
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
    use std::os::unix::fs::PermissionsExt as _;

    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::Config(
            "credential secret files must not be accessible by group or other users",
        ));
    }
    Ok(())
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

fn default_provider() -> StorageProviderKind {
    StorageProviderKind::S3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_bucket_names_follow_the_frozen_profile() {
        for value in ["abc", "a-b.c9", &format!("a{}z", "b".repeat(61))] {
            assert!(valid_bucket_name(value));
        }
        for value in ["ab", "UPPER", "-abc", "abc-", "a..b", "a_b"] {
            assert!(!valid_bucket_name(value));
        }
    }

    #[cfg(unix)]
    #[test]
    fn credential_secret_rejects_group_or_other_access() {
        use std::os::unix::fs::PermissionsExt as _;

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(validate_secret_permissions(&std::fs::metadata(file.path()).unwrap()).is_err());
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        validate_secret_permissions(&std::fs::metadata(file.path()).unwrap()).unwrap();
    }
}
