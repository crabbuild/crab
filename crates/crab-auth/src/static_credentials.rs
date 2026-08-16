//! Static credential resolution for cloud SDK default chains.

use async_trait::async_trait;
use crab_types::storage::StorageProviderKind;

use crate::client_config::StaticAuthConfig;
use crate::credential_provider::CredentialProvider;
use crate::credentials::{CloudCredentials, CredentialResolution};
use crate::error::{AuthError, Result};

/// Resolves static environment-chain credentials for a concrete storage provider.
#[derive(Debug, Clone, Copy)]
pub struct StaticCredentialResolver {
    storage_provider: StorageProviderKind,
}

impl StaticCredentialResolver {
    /// Creates a static credential resolver for an already selected provider.
    #[must_use]
    pub fn new(storage_provider: StorageProviderKind) -> Self {
        Self { storage_provider }
    }

    /// Returns the concrete provider used for static-env store construction.
    #[must_use]
    pub fn resolved_provider(self) -> StorageProviderKind {
        self.storage_provider
    }

    /// Returns the static-env credential sentinel consumed by storage construction.
    #[must_use]
    pub fn resolve(self) -> CredentialResolution {
        CredentialResolution::new(CloudCredentials::StaticEnv {
            provider: self.storage_provider,
        })
    }

    /// Static environment-chain credentials are refreshed by the provider SDK.
    #[must_use]
    pub fn needs_refresh(self) -> bool {
        false
    }

    /// Static environment-chain credentials have no crab-managed identity.
    #[must_use]
    pub fn identity(self) -> Option<&'static str> {
        None
    }
}

/// Credential provider for cloud SDK default-chain credentials.
#[derive(Debug, Clone)]
pub struct StaticProvider {
    resolver: StaticCredentialResolver,
}

impl StaticProvider {
    /// Creates a provider for an already selected static credential provider.
    #[must_use]
    pub fn new(config: StaticAuthConfig) -> Self {
        Self {
            resolver: StaticCredentialResolver::new(config.storage_provider),
        }
    }

    /// Returns the concrete storage provider selected for static credentials.
    #[must_use]
    pub fn resolved_provider(&self) -> StorageProviderKind {
        self.resolver.resolved_provider()
    }
}

#[async_trait]
impl CredentialProvider for StaticProvider {
    type Error = AuthError;

    async fn resolve(
        &self,
        _bucket: &str,
        _prefix: &str,
        _operation: &str,
    ) -> Result<CredentialResolution> {
        Ok(self.resolver.resolve())
    }

    fn needs_refresh(&self) -> bool {
        self.resolver.needs_refresh()
    }

    async fn refresh(&self) -> Result<CredentialResolution> {
        self.resolve("", "", "").await
    }

    fn identity(&self) -> Option<&str> {
        self.resolver.identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_static_env_credentials_for_selected_provider() {
        for provider in [
            StorageProviderKind::S3,
            StorageProviderKind::Gcs,
            StorageProviderKind::Azure,
        ] {
            let resolver = StaticCredentialResolver::new(provider);
            let resolution = resolver.resolve();

            assert!(matches!(
                resolution.credentials,
                CloudCredentials::StaticEnv { provider: actual } if actual == provider
            ));
        }
    }

    #[test]
    fn exposes_static_provider_lifecycle_contract() {
        let resolver = StaticCredentialResolver::new(StorageProviderKind::S3);

        assert_eq!(resolver.resolved_provider(), StorageProviderKind::S3);
        assert!(!resolver.needs_refresh());
        assert!(resolver.identity().is_none());
    }

    #[tokio::test]
    async fn static_provider_s3_returns_static_env() {
        let provider = StaticProvider::new(StaticAuthConfig {
            storage_provider: StorageProviderKind::S3,
        });
        let creds = provider.resolve("bucket", "prefix", "push").await.unwrap();

        match creds.credentials {
            CloudCredentials::StaticEnv { provider } => {
                assert_eq!(provider, StorageProviderKind::S3);
            }
            _ => panic!("expected StaticEnv variant"),
        }
    }

    #[tokio::test]
    async fn static_provider_gcs_returns_static_env() {
        let provider = StaticProvider::new(StaticAuthConfig {
            storage_provider: StorageProviderKind::Gcs,
        });
        let creds = provider.resolve("bucket", "prefix", "fetch").await.unwrap();

        match creds.credentials {
            CloudCredentials::StaticEnv { provider } => {
                assert_eq!(provider, StorageProviderKind::Gcs);
            }
            _ => panic!("expected StaticEnv variant"),
        }
    }

    #[tokio::test]
    async fn static_provider_azure_returns_static_env() {
        let provider = StaticProvider::new(StaticAuthConfig {
            storage_provider: StorageProviderKind::Azure,
        });
        let creds = provider.resolve("bucket", "prefix", "clone").await.unwrap();

        match creds.credentials {
            CloudCredentials::StaticEnv { provider } => {
                assert_eq!(provider, StorageProviderKind::Azure);
            }
            _ => panic!("expected StaticEnv variant"),
        }
    }

    #[test]
    fn static_provider_never_needs_crab_managed_refresh() {
        let provider = StaticProvider::new(StaticAuthConfig {
            storage_provider: StorageProviderKind::S3,
        });

        assert!(!provider.needs_refresh());
    }

    #[test]
    fn static_provider_has_no_crab_managed_identity() {
        let provider = StaticProvider::new(StaticAuthConfig {
            storage_provider: StorageProviderKind::S3,
        });

        assert!(provider.identity().is_none());
    }

    #[tokio::test]
    async fn static_provider_refresh_returns_same_contract_as_resolve() {
        let provider = StaticProvider::new(StaticAuthConfig {
            storage_provider: StorageProviderKind::Gcs,
        });
        let creds = provider.refresh().await.unwrap();

        match creds.credentials {
            CloudCredentials::StaticEnv { provider } => {
                assert_eq!(provider, StorageProviderKind::Gcs);
            }
            _ => panic!("expected StaticEnv variant"),
        }
    }
}
