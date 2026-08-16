//! Auth client contracts and token handling for Crab.

#[cfg(feature = "aws-oidc-client")]
pub mod aws_oidc;
#[cfg(feature = "azure-entra-client")]
pub mod azure_entra;
pub mod client_config;
#[cfg(feature = "crab-auth-client")]
pub mod crab_auth_client;
pub mod credential_provider;
pub mod credential_response;
pub mod credentials;
pub mod error;
#[cfg(feature = "gcp-workload-identity-client")]
pub mod gcp_federation;
pub mod managed;
#[cfg(feature = "oidc-client")]
pub mod oidc;
pub mod protected_push;
pub mod provider;
pub mod static_credentials;
pub mod token_cache;

#[cfg(feature = "aws-oidc-client")]
pub use aws_oidc::AwsOidcProvider;
#[cfg(feature = "azure-entra-client")]
pub use azure_entra::AzureEntraProvider;
pub use client_config::{
    AwsOidcConfig, AzureEntraConfig, CrabAuthClientConfig, CredentialProviderConfig,
    GcpWorkloadIdentityConfig, StaticAuthConfig,
};
#[cfg(feature = "crab-auth-client")]
pub use crab_auth_client::{CrabAuthProvider, ProtectedPushPrepare, create_crab_auth_provider};
pub use credential_provider::{CredentialProvider, create_credential_provider};
pub use credential_response::{
    CrabAuthCredentialResponse, credentials_from_response, parse_credential_response,
};
pub use credentials::{AzureReadScope, AzureToken, CloudCredentials, CredentialResolution};
#[cfg(feature = "gcp-workload-identity-client")]
pub use gcp_federation::GcpFederationProvider;
pub use managed::*;
#[cfg(feature = "oidc-client")]
pub use oidc::{
    OidcDiscovery, OidcTokens, discover, discover_with_client, refresh_tokens,
    refresh_tokens_with_client, revoke_token, revoke_token_with_client,
};
pub use protected_push::{
    PushFinalizeResponse, PushPrepareResponse, PushRefUpdate, normalize_optional_oid,
    validate_push_finalize_response, validate_push_prepare_response, validate_push_ref_update,
    validate_push_ref_updates,
};
pub use provider::{AuthProviderKind, parse_scope_list};
pub use static_credentials::{StaticCredentialResolver, StaticProvider};
