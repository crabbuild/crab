//! Shared credential-provider Interface.

use async_trait::async_trait;

use crate::client_config::CredentialProviderConfig;
use crate::credentials::CredentialResolution;
use crate::error::{AuthError, Result};
use crate::static_credentials::StaticProvider;

/// Resolves cloud credentials for a repository operation.
///
/// Implementations may use static environment credentials, local token caches,
/// OIDC exchanges, or a Crab Auth endpoint. The Interface stays storage-free:
/// callers receive cloud credential contracts and decide how to build stores.
#[async_trait]
pub trait CredentialProvider: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Resolve credentials for accessing a repository prefix in a bucket.
    async fn resolve(
        &self,
        bucket: &str,
        prefix: &str,
        operation: &str,
    ) -> std::result::Result<CredentialResolution, Self::Error>;

    /// Returns whether cached credentials are expired or near expiry.
    fn needs_refresh(&self) -> bool;

    /// Force-refresh credentials using provider-owned cached identity.
    async fn refresh(&self) -> std::result::Result<CredentialResolution, Self::Error>;

    /// Force-refresh credentials for a repository operation.
    async fn refresh_for(
        &self,
        bucket: &str,
        prefix: &str,
        operation: &str,
    ) -> std::result::Result<CredentialResolution, Self::Error> {
        let _ = (bucket, prefix, operation);
        self.refresh().await
    }

    /// Returns the authenticated identity when one is cheaply available.
    fn identity(&self) -> Option<&str>;
}

/// Creates the credential provider described by `config`.
pub fn create_credential_provider(
    config: CredentialProviderConfig,
) -> Result<Box<dyn CredentialProvider<Error = AuthError>>> {
    match config {
        CredentialProviderConfig::AwsOidc(config) => create_aws_oidc_provider(config),
        CredentialProviderConfig::GcpWorkloadIdentity(config) => {
            create_gcp_workload_identity_provider(config)
        }
        CredentialProviderConfig::AzureEntra(config) => create_azure_entra_provider(config),
        CredentialProviderConfig::CrabAuth(config) => create_crab_auth_provider(config),
        CredentialProviderConfig::Static(config) | CredentialProviderConfig::None(config) => {
            Ok(Box::new(StaticProvider::new(config)))
        }
    }
}

#[cfg(feature = "aws-oidc-client")]
fn create_aws_oidc_provider(
    config: crate::client_config::AwsOidcConfig,
) -> Result<Box<dyn CredentialProvider<Error = AuthError>>> {
    Ok(Box::new(crate::aws_oidc::AwsOidcProvider::new(config)?))
}

#[cfg(not(feature = "aws-oidc-client"))]
fn create_aws_oidc_provider(
    _config: crate::client_config::AwsOidcConfig,
) -> Result<Box<dyn CredentialProvider<Error = AuthError>>> {
    Err(AuthError::ProviderFeatureDisabled {
        provider: crate::provider::AuthProviderKind::AwsOidc,
        feature: "aws-oidc-client",
    })
}

#[cfg(feature = "gcp-workload-identity-client")]
fn create_gcp_workload_identity_provider(
    config: crate::client_config::GcpWorkloadIdentityConfig,
) -> Result<Box<dyn CredentialProvider<Error = AuthError>>> {
    Ok(Box::new(crate::gcp_federation::GcpFederationProvider::new(
        config,
    )?))
}

#[cfg(not(feature = "gcp-workload-identity-client"))]
fn create_gcp_workload_identity_provider(
    _config: crate::client_config::GcpWorkloadIdentityConfig,
) -> Result<Box<dyn CredentialProvider<Error = AuthError>>> {
    Err(AuthError::ProviderFeatureDisabled {
        provider: crate::provider::AuthProviderKind::GcpWorkloadIdentity,
        feature: "gcp-workload-identity-client",
    })
}

#[cfg(feature = "azure-entra-client")]
fn create_azure_entra_provider(
    config: crate::client_config::AzureEntraConfig,
) -> Result<Box<dyn CredentialProvider<Error = AuthError>>> {
    Ok(Box::new(crate::azure_entra::AzureEntraProvider::new(
        config,
    )?))
}

#[cfg(not(feature = "azure-entra-client"))]
fn create_azure_entra_provider(
    _config: crate::client_config::AzureEntraConfig,
) -> Result<Box<dyn CredentialProvider<Error = AuthError>>> {
    Err(AuthError::ProviderFeatureDisabled {
        provider: crate::provider::AuthProviderKind::AzureEntra,
        feature: "azure-entra-client",
    })
}

#[cfg(feature = "crab-auth-client")]
fn create_crab_auth_provider(
    config: crate::client_config::CrabAuthClientConfig,
) -> Result<Box<dyn CredentialProvider<Error = AuthError>>> {
    Ok(Box::new(
        crate::crab_auth_client::create_crab_auth_provider(config)?,
    ))
}

#[cfg(not(feature = "crab-auth-client"))]
fn create_crab_auth_provider(
    _config: crate::client_config::CrabAuthClientConfig,
) -> Result<Box<dyn CredentialProvider<Error = AuthError>>> {
    Err(AuthError::ProviderFeatureDisabled {
        provider: crate::provider::AuthProviderKind::CrabAuth,
        feature: "crab-auth-client",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_config::{CredentialProviderConfig, StaticAuthConfig};
    use crab_types::storage::StorageProviderKind;

    #[test]
    fn static_provider_can_be_created_without_client_features() {
        let provider =
            create_credential_provider(CredentialProviderConfig::Static(StaticAuthConfig {
                storage_provider: StorageProviderKind::S3,
            }))
            .unwrap();

        assert!(!provider.needs_refresh());
        assert!(provider.identity().is_none());
    }

    #[test]
    fn none_provider_uses_static_env_contract() {
        let provider =
            create_credential_provider(CredentialProviderConfig::None(StaticAuthConfig {
                storage_provider: StorageProviderKind::Gcs,
            }))
            .unwrap();

        assert!(!provider.needs_refresh());
        assert!(provider.identity().is_none());
    }
}
