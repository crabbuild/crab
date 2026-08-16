//! Auth client configuration contracts.

use crate::provider::AuthProviderKind;

/// Static environment credential configuration.
#[derive(Debug, Clone)]
pub struct StaticAuthConfig {
    pub storage_provider: crab_types::storage::StorageProviderKind,
}

/// AWS OIDC credential-provider configuration.
#[derive(Debug, Clone)]
pub struct AwsOidcConfig {
    pub role_arn: String,
    pub region: String,
    pub session_duration_secs: u64,
    pub issuer_url: String,
    pub client_id: String,
    pub token_cache_path: String,
}

/// GCP Workload Identity Federation credential-provider configuration.
#[derive(Debug, Clone)]
pub struct GcpWorkloadIdentityConfig {
    pub workload_identity_pool: String,
    pub service_account: String,
    pub issuer_url: String,
    pub client_id: String,
    pub token_cache_path: String,
}

/// Azure Entra credential-provider configuration.
#[derive(Debug, Clone)]
pub struct AzureEntraConfig {
    pub tenant_id: String,
    pub auth_endpoint: Option<String>,
    pub storage_account: Option<String>,
    pub issuer_url: String,
    pub client_id: String,
    pub token_cache_path: String,
}

/// Crab Auth credential-provider configuration.
#[derive(Debug, Clone)]
pub struct CrabAuthClientConfig {
    pub endpoint: String,
    pub issuer_url: String,
    pub client_id: String,
    pub token_cache_path: String,
    pub client_version: String,
}

/// Validated credential-provider configuration owned by the auth domain.
#[derive(Debug, Clone)]
pub enum CredentialProviderConfig {
    /// AWS STS via OIDC token exchange.
    AwsOidc(AwsOidcConfig),
    /// GCP Workload Identity Federation.
    GcpWorkloadIdentity(GcpWorkloadIdentityConfig),
    /// Azure Entra ID.
    AzureEntra(AzureEntraConfig),
    /// Enterprise Crab Auth endpoint.
    CrabAuth(CrabAuthClientConfig),
    /// Static credentials from the cloud SDK environment chain.
    Static(StaticAuthConfig),
    /// No Crab-managed auth; storage construction still uses static env credentials.
    None(StaticAuthConfig),
}

impl CredentialProviderConfig {
    /// Returns the configured provider kind.
    #[must_use]
    pub fn kind(&self) -> AuthProviderKind {
        match self {
            Self::AwsOidc(_) => AuthProviderKind::AwsOidc,
            Self::GcpWorkloadIdentity(_) => AuthProviderKind::GcpWorkloadIdentity,
            Self::AzureEntra(_) => AuthProviderKind::AzureEntra,
            Self::CrabAuth(_) => AuthProviderKind::CrabAuth,
            Self::Static(_) => AuthProviderKind::Static,
            Self::None(_) => AuthProviderKind::None,
        }
    }
}
