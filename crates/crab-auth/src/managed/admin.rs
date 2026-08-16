use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{PageCursor, SecretString};

/// Managed organization lifecycle state visible to administrators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrganizationState {
    Active,
    Suspended,
    Deleting,
    Deleted,
}

/// Organization returned by the managed administration API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Organization {
    pub schema_version: u16,
    pub id: Uuid,
    pub slug: String,
    pub state: OrganizationState,
    pub revision: u64,
}

/// Versioned organization-create body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateOrganizationRequest {
    pub schema_version: u16,
    pub slug: String,
}

/// Versioned organization-rename body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateOrganizationRequest {
    pub schema_version: u16,
    pub slug: String,
}

/// Cursor page returned by the managed organization collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrganizationPage {
    pub schema_version: u16,
    pub organizations: Vec<Organization>,
    pub next_cursor: Option<PageCursor>,
}

/// Organization membership role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OrganizationRole {
    Owner,
    Admin,
    Writer,
    Reader,
    Billing,
}

/// Organization membership returned by the managed administration API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrganizationMembership {
    pub schema_version: u16,
    pub organization_id: Uuid,
    pub principal_id: Uuid,
    pub role: OrganizationRole,
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Versioned membership-create body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddOrganizationMemberRequest {
    pub schema_version: u16,
    pub principal_id: Uuid,
    pub role: OrganizationRole,
}

/// Versioned membership-role update body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateOrganizationMemberRequest {
    pub schema_version: u16,
    pub role: OrganizationRole,
}

/// Cursor page returned by the managed membership collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrganizationMemberPage {
    pub schema_version: u16,
    pub members: Vec<OrganizationMembership>,
    pub next_cursor: Option<PageCursor>,
}

/// Repository state accepted by an administration update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryRequestedState {
    Archived,
}

/// Versioned repository rename or state update body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateRepositoryRequest {
    pub schema_version: u16,
    pub slug: Option<String>,
    pub state: Option<RepositoryRequestedState>,
}

/// Supported service-account authentication mechanisms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAccountKind {
    OidcWorkload,
    OpaqueToken,
}

/// Service account returned by the managed administration API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ServiceAccount {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub kind: String,
    pub role: String,
    pub issuer: Option<String>,
    pub subject: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub revoked_at: Option<OffsetDateTime>,
    pub revision: u64,
}

/// Service-account creation body for workload and opaque credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateServiceAccountRequest {
    pub kind: ServiceAccountKind,
    pub name: String,
    pub role: String,
    pub issuer: Option<String>,
    pub subject: Option<String>,
    pub expires_in_seconds: Option<u64>,
}

/// Opaque service-account credential rotation body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RotateServiceTokenRequest {
    pub expires_in_seconds: u64,
    pub overlap_seconds: u64,
}

/// Service accounts visible in an organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ServiceAccountList {
    pub schema_version: u16,
    pub accounts: Vec<ServiceAccount>,
}

/// One-time opaque credential returned only by create and rotate operations.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssuedServiceToken {
    pub schema_version: u16,
    pub account: ServiceAccount,
    pub credential_id: Uuid,
    pub token: SecretString,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub expires_at: OffsetDateTime,
}

impl std::fmt::Debug for IssuedServiceToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedServiceToken")
            .field("schema_version", &self.schema_version)
            .field("account", &self.account)
            .field("credential_id", &self.credential_id)
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
