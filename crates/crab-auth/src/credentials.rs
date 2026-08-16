//! Credential contracts resolved by auth providers.

use std::time::SystemTime;

use crab_types::storage::{StorageProviderKind, StorageScope};

/// Resolved cloud-native credentials ready for object-store construction.
///
/// Each variant carries the provider-specific fields needed by the storage
/// layer to configure an object-store builder. Auth owns credential resolution;
/// storage owns how these credentials become a concrete store.
#[derive(Debug, Clone)]
pub enum CloudCredentials {
    /// S3-compatible credentials.
    ///
    /// AWS STS credentials include a session token. S3-compatible static
    /// credentials omit it because some self-hosted stores reject
    /// `x-amz-security-token`.
    Aws {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
        expires_at: SystemTime,
        region: String,
    },
    /// GCP OAuth2 access token.
    Gcp {
        access_token: String,
        expires_at: SystemTime,
    },
    /// Azure bearer or SAS token.
    Azure {
        account: String,
        token: AzureToken,
        expires_at: SystemTime,
    },
    /// Azure credentials split across read scopes and one write prefix.
    AzureScoped {
        account: String,
        read_scopes: Vec<AzureReadScope>,
        write_token: AzureToken,
        write_prefix: String,
        expires_at: SystemTime,
    },
    /// Sentinel asking storage construction to use the cloud SDK default chain.
    StaticEnv { provider: StorageProviderKind },
}

/// A credential resolution result, optionally scoped to auth-issued prefixes.
#[derive(Debug, Clone)]
pub struct CredentialResolution {
    pub credentials: CloudCredentials,
    pub storage_scope: Option<StorageScope>,
}

impl CredentialResolution {
    #[must_use]
    pub fn new(credentials: CloudCredentials) -> Self {
        Self {
            credentials,
            storage_scope: None,
        }
    }

    #[must_use]
    pub fn with_storage_scope(credentials: CloudCredentials, storage_scope: StorageScope) -> Self {
        Self {
            credentials,
            storage_scope: Some(storage_scope),
        }
    }
}

/// Azure token type: either an OAuth2 bearer token or a SAS token.
#[derive(Debug, Clone)]
pub enum AzureToken {
    Bearer(String),
    Sas(String),
}

/// A path-limited Azure read credential.
#[derive(Debug, Clone)]
pub struct AzureReadScope {
    pub prefix: String,
    pub token: AzureToken,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_resolution_has_no_storage_scope() {
        let resolution = CredentialResolution::new(CloudCredentials::StaticEnv {
            provider: StorageProviderKind::S3,
        });

        assert!(resolution.storage_scope.is_none());
    }

    #[test]
    fn scoped_resolution_preserves_scope() {
        let scope = StorageScope {
            repo_prefix: "views/scope/repo".into(),
            global_prefix: "views/scope/global".into(),
            source_repo: "team/repo".into(),
            scope_hash: "scope".into(),
        };

        let resolution = CredentialResolution::with_storage_scope(
            CloudCredentials::Gcp {
                access_token: "token".into(),
                expires_at: SystemTime::UNIX_EPOCH,
            },
            scope.clone(),
        );

        assert_eq!(resolution.storage_scope, Some(scope));
    }
}
