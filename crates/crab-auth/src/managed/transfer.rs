use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use super::api::{invalid, require_v1};
use super::discovery::validate_https_url;
use super::{MANAGED_SCHEMA_VERSION, ManagedCdnBinding};
use crate::cdn::{CdnProvider, CdnViewerGrant};
use crate::error::Result;

pub(crate) const CDN_TRANSFER_GRANT_VERSION: u16 = 2;

/// Opaque secret transported only inside a short-lived grant.
#[derive(Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct SecretString(String);

impl SecretString {
    /// Creates a non-empty bounded wire secret.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > 16_384 || value.chars().any(char::is_control) {
            return invalid("grant secret must be non-empty, bounded, and contain no controls");
        }
        Ok(Self(value))
    }

    /// Exposes the secret only at the credential or gateway construction boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Operation authorized by a transfer grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferOperation {
    Clone,
    Fetch,
    Hydrate,
    PushUpload,
}

impl TransferOperation {
    fn is_read(self) -> bool {
        matches!(self, Self::Clone | Self::Fetch | Self::Hydrate)
    }
}

/// Smallest object capability expressible by a managed transfer grant.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TransferPermission {
    ReadObject,
    ReadMetadata,
    ListPrefix,
    CreateImmutableObject,
}

/// Exact staging sub-scope for one protected push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StagingScope {
    pub push_id: Uuid,
    pub prefix: String,
}

/// Physical repository and optional staging scope authorized by a grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferScope {
    pub repository_prefix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staging: Option<StagingScope>,
}

/// Provider-specific temporary credentials returned in direct mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderCredentials {
    Aws {
        access_key_id: SecretString,
        secret_access_key: SecretString,
        session_token: SecretString,
    },
    Gcp {
        access_token: SecretString,
    },
    Azure {
        account: String,
        token: SecretString,
    },
}

/// Direct object-store route and temporary credential payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DirectObjectCredentials {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub container: String,
    pub object_prefix: String,
    pub credentials: ProviderCredentials,
}

/// Authenticated object-gateway route for providers without safe downscoping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GatewayAccess {
    pub service_url: String,
    pub token: SecretString,
}

/// Versioned bounded object listing returned by the managed gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GatewayObjectPage {
    pub schema_version: u16,
    pub objects: Vec<GatewayObject>,
}

/// Relative repository object metadata returned by the managed gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GatewayObject {
    pub key: String,
    pub size: u64,
}

impl GatewayObjectPage {
    /// Decodes and validates one bounded gateway list response.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 4 * 1024 * 1024 {
            return invalid("managed gateway list response exceeds the client limit");
        }
        let page: Self = serde_json::from_slice(bytes).map_err(|_| {
            crate::error::AuthError::InvalidManagedContract(
                "managed gateway list response is invalid JSON".to_owned(),
            )
        })?;
        require_v1(page.schema_version, "gateway object page")?;
        if page.objects.len() > 10_000 {
            return invalid("managed gateway list response contains too many objects");
        }
        for object in &page.objects {
            validate_prefix(&object.key, "gateway object key")?;
        }
        Ok(page)
    }
}

/// Transport selected for a managed transfer grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransferTransport {
    Direct { direct: DirectObjectCredentials },
    Gateway { gateway: GatewayAccess },
}

/// Versioned, repository-bound, short-lived transfer authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferGrant {
    pub schema_version: u16,
    pub grant_id: Uuid,
    pub repository_id: Uuid,
    pub operation: TransferOperation,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub expires_at: OffsetDateTime,
    pub permissions: Vec<TransferPermission>,
    pub storage_scope: TransferScope,
    pub transport: TransferTransport,
    /// Present only on a v2 native direct read grant.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_cdn"
    )]
    #[schema(value_type = CdnViewerGrant, required = false)]
    pub cdn: Option<CdnViewerGrant>,
    /// Echo of the requested deployment approval, present only on v2 reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_approval_sha256: Option<String>,
}

fn deserialize_cdn<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<CdnViewerGrant>, D::Error> {
    CdnViewerGrant::deserialize(deserializer)
        .map(Some)
        .map_err(|_| serde::de::Error::custom("invalid CDN viewer grant"))
}

/// Operation-bound request for a managed transfer grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferGrantRequest {
    /// Version 1, or version 2 for reads when advertised by capabilities.
    pub schema_version: u16,
    pub repository_id: Uuid,
    pub operation: TransferOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_approval_sha256: Option<String>,
}

/// Client-side facts required to validate a grant before store construction.
#[derive(Debug, Clone, Copy)]
pub struct GrantValidationContext<'a> {
    /// Exact version selected for this transfer request.
    pub schema_version: u16,
    pub repository_id: Uuid,
    pub operation: TransferOperation,
    pub repository_prefix: &'a str,
    pub cdn_binding: Option<&'a ManagedCdnBinding>,
    pub push_id: Option<Uuid>,
    pub allowed_permissions: &'a [TransferPermission],
    pub now: OffsetDateTime,
    pub not_after: OffsetDateTime,
}

impl TransferGrant {
    /// Validates version, repository, operation, expiry, scope, and transport before use.
    pub fn validate(&self, context: GrantValidationContext<'_>) -> Result<()> {
        if self.schema_version != context.schema_version {
            return invalid("transfer grant version does not match the requested version");
        }
        match (self.schema_version, context.cdn_binding) {
            (MANAGED_SCHEMA_VERSION, None)
                if self.cdn.is_none() && self.cdn_approval_sha256.is_none() => {}
            (CDN_TRANSFER_GRANT_VERSION, Some(binding)) if self.operation.is_read() => {
                binding.validate_grant(self)?;
            }
            _ => {
                return invalid(
                    "transfer grant version or CDN authority is invalid for this operation",
                );
            }
        }
        if self.grant_id.is_nil() || self.repository_id.is_nil() {
            return invalid("transfer grant IDs must not be nil");
        }
        if self.repository_id != context.repository_id {
            return invalid("transfer grant repository does not match the requested repository");
        }
        if self.operation != context.operation {
            return invalid("transfer grant operation does not match the requested operation");
        }
        if context.not_after <= context.now
            || self.expires_at <= context.now
            || self.expires_at > context.not_after
        {
            return invalid("transfer grant expiry is outside the authorized lifetime");
        }
        validate_prefix(context.repository_prefix, "expected repository prefix")?;
        validate_permissions(
            self.operation,
            &self.permissions,
            context.allowed_permissions,
        )?;
        self.storage_scope.validate(context)?;
        self.transport
            .validate(&self.storage_scope, self.operation)?;
        if let Some(cdn) = &self.cdn {
            let TransferTransport::Direct { direct } = &self.transport else {
                return invalid("gateway transfer cannot authorize a CDN route");
            };
            direct.validate_cdn(cdn, context.repository_prefix, self.expires_at, context.now)?;
        }
        Ok(())
    }
}

impl TransferScope {
    fn validate(&self, context: GrantValidationContext<'_>) -> Result<()> {
        validate_prefix(&self.repository_prefix, "repository prefix")?;
        if self.repository_prefix != context.repository_prefix {
            return invalid("transfer grant physical repository scope is wider or different");
        }
        if context.operation.is_read() {
            if self.staging.is_some() || context.push_id.is_some() {
                return invalid("read transfer grant must not contain a staging scope");
            }
            return Ok(());
        }

        let expected_push_id = context.push_id.ok_or_else(|| {
            crate::error::AuthError::InvalidManagedContract(
                "push transfer validation requires a push ID".to_owned(),
            )
        })?;
        let staging = self.staging.as_ref().ok_or_else(|| {
            crate::error::AuthError::InvalidManagedContract(
                "push transfer grant is missing its staging scope".to_owned(),
            )
        })?;
        if staging.push_id != expected_push_id {
            return invalid("transfer grant staging scope belongs to another push");
        }
        let expected = format!(
            "{}/staging/{}",
            self.repository_prefix,
            expected_push_id.simple()
        );
        if staging.prefix != expected {
            return invalid("transfer grant staging prefix is not the exact push scope");
        }
        validate_prefix(&staging.prefix, "staging prefix")
    }
}

impl TransferTransport {
    fn validate(&self, scope: &TransferScope, operation: TransferOperation) -> Result<()> {
        match self {
            Self::Direct { direct } => direct.validate(scope, operation),
            Self::Gateway { gateway } => gateway.validate(),
        }
    }
}

impl DirectObjectCredentials {
    fn validate_cdn(
        &self,
        cdn: &CdnViewerGrant,
        repository_prefix: &str,
        parent_expiry: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<()> {
        if self.endpoint.is_some() {
            return invalid("CDN transfer requires a native endpoint without overrides");
        }
        let (provider, origin) = self.cdn_origin();
        cdn.validate_authority(provider)?;
        if cdn.origin != origin {
            return invalid("CDN viewer grant does not match the managed origin");
        }
        cdn.validate_scope(repository_prefix, None)?;
        let expiry = i64::try_from(cdn.expires_at).ok();
        if !expiry.is_some_and(|expiry| {
            expiry > now.unix_timestamp() && expiry <= parent_expiry.unix_timestamp()
        }) {
            return invalid("CDN viewer grant expires outside its managed read authorization");
        }
        Ok(())
    }

    pub(super) fn cdn_origin(&self) -> (CdnProvider, String) {
        let container = &self.container;
        match &self.credentials {
            ProviderCredentials::Aws { .. } => {
                (CdnProvider::Cloudfront, format!("s3://{container}"))
            }
            ProviderCredentials::Gcp { .. } => (CdnProvider::CloudCdn, format!("gs://{container}")),
            ProviderCredentials::Azure { account, .. } => (
                CdnProvider::FrontDoor,
                format!("https://{account}.blob.core.windows.net/{container}"),
            ),
        }
    }

    fn validate(&self, scope: &TransferScope, operation: TransferOperation) -> Result<()> {
        if let Some(endpoint) = &self.endpoint {
            validate_https_url(endpoint, "direct endpoint")?;
        }
        if self.container.is_empty()
            || self.container.len() > 255
            || self.container.contains('/')
            || self.container.chars().any(char::is_control)
        {
            return invalid("direct grant container is invalid");
        }
        validate_prefix(&self.object_prefix, "direct object prefix")?;
        let expected_prefix = if operation.is_read() {
            scope.repository_prefix.as_str()
        } else {
            scope
                .staging
                .as_ref()
                .map(|staging| staging.prefix.as_str())
                .ok_or_else(|| {
                    crate::error::AuthError::InvalidManagedContract(
                        "direct push transport is missing its staging scope".to_owned(),
                    )
                })?
        };
        if self.object_prefix != expected_prefix {
            return invalid("direct object prefix is wider or different from the grant scope");
        }
        match &self.credentials {
            ProviderCredentials::Aws { .. } if self.region.as_deref().is_none_or(str::is_empty) => {
                invalid("AWS direct grant requires a region")
            }
            ProviderCredentials::Azure { account, .. } if account.is_empty() => {
                invalid("Azure direct grant requires a storage account")
            }
            _ => Ok(()),
        }
    }
}

impl GatewayAccess {
    fn validate(&self) -> Result<()> {
        let url = validate_https_url(&self.service_url, "gateway service_url")?;
        if url.path() == "/" || url.path().is_empty() {
            return invalid("gateway service_url must include its object API path");
        }
        Ok(())
    }
}

fn validate_permissions(
    operation: TransferOperation,
    permissions: &[TransferPermission],
    allowed: &[TransferPermission],
) -> Result<()> {
    if permissions.is_empty() {
        return invalid("transfer grant must contain at least one permission");
    }
    let unique = permissions.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != permissions.len() {
        return invalid("transfer grant permissions must be unique");
    }
    if permissions
        .iter()
        .any(|permission| !allowed.contains(permission))
    {
        return invalid("transfer grant contains a permission wider than requested");
    }
    let required = if operation.is_read() {
        TransferPermission::ReadObject
    } else {
        TransferPermission::CreateImmutableObject
    };
    if !permissions.contains(&required) {
        return invalid("transfer grant is missing its operation's required permission");
    }
    if operation.is_read() && permissions.contains(&TransferPermission::CreateImmutableObject) {
        return invalid("read transfer grant must not create objects");
    }
    Ok(())
}

fn validate_prefix(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 2048
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return invalid(format!("{field} is not a normalized object prefix"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::GatewayObjectPage;

    #[test]
    fn gateway_object_page_rejects_unsafe_keys_and_unknown_versions() {
        let unsafe_key =
            br#"{"schema_version":1,"objects":[{"key":"../other/manifest","size":1}]}"#;
        let unknown_version = br#"{"schema_version":2,"objects":[]}"#;

        assert!(GatewayObjectPage::from_slice(unsafe_key).is_err());
        assert!(GatewayObjectPage::from_slice(unknown_version).is_err());
    }
}
