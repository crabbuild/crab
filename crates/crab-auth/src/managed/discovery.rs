use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};
use url::Url;
use utoipa::ToSchema;

use super::api::{invalid, require_v1};
use crate::error::Result;

/// Named managed-service capability. Unknown valid names are intentionally retained.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct CapabilityName(String);

impl CapabilityName {
    /// Creates a lowercase capability token such as `direct-s3-v1`.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if !is_token(&value, b'-') {
            return invalid("capability must be a lowercase hyphenated token");
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CapabilityName {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// OAuth flow name advertised by discovery.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct AuthFlow(String);

impl AuthFlow {
    /// Creates a lowercase OAuth flow token.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if !is_token(&value, b'_') {
            return invalid("authentication flow must be a lowercase underscored token");
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for AuthFlow {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Transfer transports negotiated through discovery and capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferMode {
    DirectS3,
    DirectGcs,
    DirectAzure,
    Gateway,
}

/// Public OIDC client metadata returned by service discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OidcClientDiscovery {
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

/// Discovery cache bounds supplied by the service authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiscoveryCache {
    pub max_age_seconds: u32,
}

/// Versioned `/.well-known/crab` discovery document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiscoveryDocument {
    pub schema_version: u16,
    pub authority: String,
    pub api_base: String,
    pub api_versions: Vec<u16>,
    pub oidc: OidcClientDiscovery,
    pub auth_flows: Vec<AuthFlow>,
    pub service_version: String,
    pub minimum_cli_version: String,
    pub capabilities: Vec<CapabilityName>,
    pub cache: DiscoveryCache,
}

impl DiscoveryDocument {
    /// Validates authority binding, HTTPS origins, version overlap, and cache bounds.
    pub fn validate(&self, expected_authority: &str, supported_api_versions: &[u16]) -> Result<()> {
        require_v1(self.schema_version, "discovery")?;
        validate_authority(expected_authority)?;
        if self.authority != expected_authority {
            return invalid("discovery authority does not match the configured authority");
        }
        validate_https_url(&self.api_base, "api_base")?;
        if Url::parse(&self.api_base)
            .map(|url| url.path().trim_end_matches('/') != "/v1")
            .unwrap_or(true)
        {
            return invalid("discovery api_base must identify the /v1 API root");
        }
        validate_https_url(&self.oidc.issuer, "oidc.issuer")?;
        if self.api_versions.is_empty()
            || !self
                .api_versions
                .iter()
                .any(|version| supported_api_versions.contains(version))
        {
            return Err(crate::error::AuthError::UnsupportedManagedApiVersion {
                supported: supported_api_versions.to_vec(),
                advertised: self.api_versions.clone(),
            });
        }
        require_unique_positive(&self.api_versions, "api_versions")?;
        if self.oidc.client_id.is_empty()
            || self.oidc.client_id.len() > 255
            || self.oidc.client_id.chars().any(char::is_control)
        {
            return invalid("discovery OIDC client_id is invalid");
        }
        if self.oidc.scopes.is_empty()
            || !self.oidc.scopes.iter().any(|scope| scope == "openid")
            || has_empty_or_duplicate(&self.oidc.scopes)
        {
            return invalid("discovery OIDC scopes must be unique and include openid");
        }
        if self.auth_flows.is_empty() || has_duplicate(self.auth_flows.iter().map(AuthFlow::as_str))
        {
            return invalid("discovery auth_flows must be non-empty and unique");
        }
        if has_duplicate(self.capabilities.iter().map(CapabilityName::as_str)) {
            return invalid("discovery capabilities must be unique");
        }
        if self.service_version.is_empty() || self.minimum_cli_version.is_empty() {
            return invalid("discovery service and minimum CLI versions must be non-empty");
        }
        if !(1..=86_400).contains(&self.cache.max_age_seconds) {
            return invalid("discovery cache lifetime must be between 1 and 86400 seconds");
        }
        Ok(())
    }
}

/// Authenticated capability and protocol-bound response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ServiceCapabilities {
    pub schema_version: u16,
    pub api_versions: Vec<u16>,
    pub transfer_grant_versions: Vec<u16>,
    pub transfer_modes: Vec<TransferMode>,
    pub capabilities: Vec<CapabilityName>,
    pub max_page_size: u32,
    pub max_grant_lifetime_seconds: u32,
}

impl ServiceCapabilities {
    /// Validates advertised bounds before the client negotiates an operation.
    pub fn validate(&self) -> Result<()> {
        require_v1(self.schema_version, "capabilities")?;
        require_unique_positive(&self.api_versions, "api_versions")?;
        require_unique_positive(&self.transfer_grant_versions, "transfer_grant_versions")?;
        if !self.transfer_grant_versions.contains(&1) {
            return invalid("capabilities do not advertise transfer grant version 1");
        }
        if self.transfer_modes.is_empty() {
            return invalid("capabilities have no transfer mode");
        }
        if self.max_page_size == 0 || self.max_grant_lifetime_seconds == 0 {
            return invalid("capability limits must be positive");
        }
        Ok(())
    }
}

pub(super) fn validate_https_url(value: &str, field: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| {
        crate::error::AuthError::InvalidManagedContract(format!("{field} is not a valid URL"))
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return invalid(format!(
            "{field} must be an HTTPS URL without credentials, query, or fragment"
        ));
    }
    Ok(url)
}

pub(super) fn validate_authority(value: &str) -> Result<()> {
    let url = validate_https_url(&format!("https://{value}"), "authority")?;
    if url.port().is_some()
        || url.host_str() != Some(value)
        || value.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        return invalid("authority must be a lowercase DNS name without a port");
    }
    Ok(())
}

fn require_unique_positive(values: &[u16], field: &str) -> Result<()> {
    if values.is_empty() || values.contains(&0) {
        return invalid(format!("{field} must contain positive versions"));
    }
    let unique = values.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != values.len() {
        return invalid(format!("{field} must not contain duplicates"));
    }
    Ok(())
}

fn has_empty_or_duplicate(values: &[String]) -> bool {
    values.iter().any(|value| value.is_empty()) || has_duplicate(values.iter().map(String::as_str))
}

fn has_duplicate<'a>(values: impl Iterator<Item = &'a str>) -> bool {
    let mut unique = BTreeSet::new();
    values.into_iter().any(|value| !unique.insert(value))
}

fn is_token(value: &str, separator: u8) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with(char::from(separator))
        && !value.ends_with(char::from(separator))
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == separator)
}
