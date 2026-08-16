//! Shared managed-service client and server wire contracts.

mod admin;
mod api;
mod discovery;
mod openapi;
mod operations;
mod profile;
#[cfg(feature = "oidc-client")]
mod profile_discovery;
mod push;
mod transfer;

pub use admin::{
    AddOrganizationMemberRequest, CreateOrganizationRequest, CreateServiceAccountRequest,
    IssuedServiceToken, Organization, OrganizationMemberPage, OrganizationMembership,
    OrganizationPage, OrganizationRole, OrganizationState, RepositoryRequestedState,
    RotateServiceTokenRequest, ServiceAccount, ServiceAccountKind, ServiceAccountList,
    UpdateOrganizationMemberRequest, UpdateOrganizationRequest, UpdateRepositoryRequest,
};
pub use api::{
    ApiError, ApiErrorEnvelope, CreateRepositoryRequest, EntityTag, IdempotencyKey,
    LogicalRepository, PageCursor, RepositoryPage, RepositoryState,
};
#[cfg(feature = "oidc-client")]
mod client;
#[cfg(feature = "oidc-client")]
pub use client::{BearerToken, ManagedApiClient, ManagedApiError, ManagedApiResult, ManagedEntity};
pub use discovery::{
    AuthFlow, CapabilityName, DiscoveryCache, DiscoveryDocument, OidcClientDiscovery,
    ServiceCapabilities, TransferMode,
};
pub use openapi::{ManagedApiDoc, managed_openapi_breaking_changes, managed_openapi_json};
pub use operations::{
    AuditDecision, AuditEvent, AuditEventPage, AuditExportFormat, AuditExportOperation,
    AuditExportRequest, JobActionRequest, JobPage, JobState, JobSummary, PromoteReplicaRequest,
    PromoteReplicaResponse, UsageCategory, UsageReport, UsageTotal,
};
pub use profile::{
    CachedDiscovery, EnterpriseCaReference, ServiceProfile, ServiceProfileLocatorError,
    ServiceProfileStore, ServiceTrust, managed_token_cache_key, service_profile_directory,
};
#[cfg(feature = "oidc-client")]
pub use profile_discovery::{
    DiscoveryCacheStatus, ManagedDiscoveryClient, ResolvedDiscovery, managed_http_client,
};
pub use push::{
    PushAbortResponse, PushAdmissionPlan, PushFinalizeRequest, PushPrepareRequest,
    PushPrepareResponse, PushReplicationRequest,
};
pub use transfer::{
    DirectObjectCredentials, GatewayAccess, GatewayObject, GatewayObjectPage,
    GrantValidationContext, ProviderCredentials, SecretString, StagingScope, TransferGrant,
    TransferGrantRequest, TransferOperation, TransferPermission, TransferScope, TransferTransport,
};

/// Initial schema version for managed-service wire contracts.
pub const MANAGED_SCHEMA_VERSION: u16 = 1;
