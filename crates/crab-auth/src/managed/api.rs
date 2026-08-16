use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{MANAGED_SCHEMA_VERSION, TransferMode};
use crate::error::{AuthError, Result};

/// Stable managed API error envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ApiErrorEnvelope {
    pub error: ApiError,
}

/// Stable managed API error body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub request_id: String,
    pub retryable: bool,
    #[serde(default)]
    pub details: BTreeMap<String, serde_json::Value>,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} (request {})", self.code, self.request_id)
    }
}

/// Managed repository lifecycle state visible to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryState {
    Provisioning,
    Active,
    Archived,
    SoftDeleted,
    Failed,
}

/// Logical repository representation returned by repository resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LogicalRepository {
    pub schema_version: u16,
    pub repository_id: Uuid,
    pub organization_id: Uuid,
    pub canonical_url: String,
    pub state: RepositoryState,
    pub revision: u64,
    pub transfer_modes: Vec<TransferMode>,
    pub protected_push: bool,
}

/// Versioned repository-create body shared by clients and the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRepositoryRequest {
    pub schema_version: u16,
    pub slug: String,
}

/// Cursor page returned by the managed repository collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RepositoryPage {
    pub schema_version: u16,
    pub repositories: Vec<LogicalRepository>,
    pub next_cursor: Option<PageCursor>,
}

impl RepositoryPage {
    /// Validates page version and every logical repository.
    pub fn validate(&self) -> Result<()> {
        require_v1(self.schema_version, "repository page")?;
        for repository in &self.repositories {
            repository.validate()?;
        }
        Ok(())
    }
}

impl LogicalRepository {
    /// Validates the version and public canonical URL of a repository response.
    pub fn validate(&self) -> Result<()> {
        require_v1(self.schema_version, "logical repository")?;
        if self.repository_id.is_nil() || self.organization_id.is_nil() {
            return invalid("logical repository IDs must not be nil");
        }
        if self.revision == 0 {
            return invalid("logical repository revision must be positive");
        }
        if self.transfer_modes.is_empty() {
            return invalid("logical repository has no transfer mode");
        }
        if !self.canonical_url.starts_with("crab://") {
            return invalid("logical repository canonical_url is not a crab URL");
        }
        Ok(())
    }
}

/// Opaque signed pagination cursor.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct PageCursor(String);

impl PageCursor {
    /// Creates a cursor from its opaque wire value.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 4096
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return invalid("pagination cursor is not a bounded base64url token");
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PageCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PageCursor")
            .field(&"<opaque>")
            .finish()
    }
}

impl<'de> Deserialize<'de> for PageCursor {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Strong entity tag used by managed mutation preconditions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct EntityTag(String);

impl EntityTag {
    /// Creates a strong quoted ETag.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let inner = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'));
        if value.len() > 130
            || value.starts_with("W/")
            || !matches!(inner, Some(inner) if !inner.is_empty()
                && inner.bytes().all(|byte| byte.is_ascii_graphic() && byte != b'"'))
        {
            return invalid("ETag must be a bounded strong quoted value");
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for EntityTag {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Idempotency key supplied for retryable managed mutations.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Creates a bounded visible-ASCII idempotency key.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return invalid("idempotency key must be 1-128 visible ASCII bytes");
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for IdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("IdempotencyKey")
            .field(&"<opaque>")
            .finish()
    }
}

impl<'de> Deserialize<'de> for IdempotencyKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

pub(super) fn require_v1(schema_version: u16, contract: &str) -> Result<()> {
    if schema_version != MANAGED_SCHEMA_VERSION {
        return invalid(format!(
            "unsupported {contract} schema version {schema_version}; expected {MANAGED_SCHEMA_VERSION}"
        ));
    }
    Ok(())
}

pub(super) fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(AuthError::InvalidManagedContract(message.into()))
}
