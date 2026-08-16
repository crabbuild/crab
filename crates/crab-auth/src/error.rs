use crate::provider::AuthProviderKind;

pub type Result<T> = std::result::Result<T, AuthError>;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("auth I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to serialize cached tokens: {source}")]
    SerializeTokens {
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to parse cached tokens: {source}")]
    ParseCachedTokens {
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to serialize managed service profile: {source}")]
    SerializeServiceProfile {
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to parse managed service profile: {source}")]
    ParseServiceProfile {
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to parse managed service discovery response: {source}")]
    ParseManagedDiscovery {
        #[source]
        source: serde_json::Error,
    },

    #[error("managed service profile for authority {authority} was not found")]
    ManagedProfileNotFound { authority: String },

    #[error("JWT payload base64 decode failed: {source}")]
    JwtPayloadBase64 {
        #[source]
        source: base64::DecodeError,
    },

    #[error("JWT payload is not valid JSON: {source}")]
    JwtPayloadJson {
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid JWT: {0}")]
    InvalidJwt(String),

    #[error("invalid protected-push ref update: {0}")]
    InvalidProtectedPushRefUpdate(String),

    #[error("invalid protected-push ref updates: {0}")]
    InvalidProtectedPushRefUpdates(String),

    #[error("invalid protected-push prepare response: {0}")]
    InvalidProtectedPushPrepareResponse(String),

    #[error("invalid protected-push finalize response: {0}")]
    InvalidProtectedPushFinalizeResponse(String),

    #[error("invalid credential response: {0}")]
    InvalidCredentialResponse(String),

    #[error("invalid managed service contract: {0}")]
    InvalidManagedContract(String),

    #[error(
        "managed service API version is incompatible: client supports {supported:?}, service advertises {advertised:?}"
    )]
    UnsupportedManagedApiVersion {
        supported: Vec<u16>,
        advertised: Vec<u16>,
    },

    #[cfg(feature = "oidc-client")]
    #[error("managed service discovery request failed at {endpoint}: {source}")]
    ManagedDiscoveryRequest {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[cfg(feature = "oidc-client")]
    #[error("managed service discovery returned HTTP {status} at {endpoint}")]
    ManagedDiscoveryRejected { endpoint: String, status: u16 },

    #[cfg(feature = "oidc-client")]
    #[error("managed service discovery is unavailable for authority {authority}")]
    ManagedDiscoveryUnavailable { authority: String },

    #[error("auth provider {provider} requires crab-auth feature {feature}")]
    ProviderFeatureDisabled {
        provider: AuthProviderKind,
        feature: &'static str,
    },

    #[error("failed to parse credential response: {source}")]
    ParseCredentialResponse {
        #[source]
        source: serde_json::Error,
    },

    #[error("no credentials available")]
    NoCredentials,

    #[error("credentials expired: {0}")]
    CredentialsExpired(String),

    #[cfg(feature = "oidc-client")]
    #[error("OIDC {operation} request failed at {endpoint}: {source}")]
    OidcRequest {
        operation: &'static str,
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[cfg(feature = "oidc-client")]
    #[error("OIDC {operation} returned HTTP {status} at {endpoint}: {body}")]
    OidcRejected {
        operation: &'static str,
        endpoint: String,
        status: u16,
        body: String,
    },

    #[cfg(feature = "oidc-client")]
    #[error("failed to parse OIDC {operation} response from {endpoint}: {source}")]
    ParseOidcResponse {
        operation: &'static str,
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[cfg(feature = "oidc-client")]
    #[error("OIDC token refresh returned HTTP {status} at {endpoint}: {body}")]
    OidcRefreshExpired {
        endpoint: String,
        status: u16,
        body: String,
    },

    #[cfg(feature = "crab-auth-client")]
    #[error("crab-auth {operation} request failed at {endpoint}: {source}")]
    CrabAuthRequest {
        operation: &'static str,
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[cfg(feature = "crab-auth-client")]
    #[error("crab-auth {operation} returned HTTP {status} at {endpoint}: {body}")]
    CrabAuthRejected {
        operation: &'static str,
        endpoint: String,
        status: u16,
        body: String,
    },

    #[cfg(feature = "crab-auth-client")]
    #[error("failed to parse crab-auth {operation} response from {endpoint}: {source}")]
    ParseCrabAuthResponse {
        operation: &'static str,
        endpoint: String,
        #[source]
        source: serde_json::Error,
    },

    #[cfg(feature = "crab-auth-client")]
    #[error("crab-auth {operation} failed at {endpoint}: {reason}")]
    CrabAuthFailed {
        operation: &'static str,
        endpoint: String,
        reason: String,
    },

    #[cfg(feature = "crab-auth-client")]
    #[error("invalid crab-auth request: {0}")]
    InvalidCrabAuthRequest(String),

    #[cfg(feature = "aws-oidc-client")]
    #[error("AWS STS request failed at {endpoint}: {source}")]
    AwsStsRequest {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[cfg(feature = "aws-oidc-client")]
    #[error("AWS STS rejected web identity token: {0}")]
    AwsStsRejected(String),

    #[cfg(feature = "azure-entra-client")]
    #[error("invalid Azure credential config for {key}: {reason}")]
    AzureConfig {
        key: &'static str,
        reason: &'static str,
    },

    #[cfg(feature = "azure-entra-client")]
    #[error("Azure {operation} request failed at {endpoint}: {source}")]
    AzureRequest {
        operation: &'static str,
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[cfg(feature = "azure-entra-client")]
    #[error("failed to parse Azure {operation} response from {endpoint}: {source}")]
    ParseAzureResponse {
        operation: &'static str,
        endpoint: String,
        #[source]
        source: serde_json::Error,
    },

    #[cfg(feature = "azure-entra-client")]
    #[error("Azure rejected Entra credentials: {0}")]
    AzureRejected(String),

    #[cfg(feature = "gcp-workload-identity-client")]
    #[error("GCP {operation} request failed at {endpoint}: {source}")]
    GcpRequest {
        operation: &'static str,
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[cfg(feature = "gcp-workload-identity-client")]
    #[error("failed to parse GCP {operation} response from {endpoint}: {source}")]
    ParseGcpResponse {
        operation: &'static str,
        endpoint: String,
        #[source]
        source: serde_json::Error,
    },

    #[cfg(feature = "gcp-workload-identity-client")]
    #[error("GCP rejected workload identity credentials: {0}")]
    GcpRejected(String),

    #[error("{operation} failed: {reason}")]
    Crypto {
        operation: &'static str,
        reason: String,
    },

    #[error("token key store error: {0}")]
    KeyStore(String),
}
