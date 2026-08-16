use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::MANAGED_SCHEMA_VERSION;
use super::api::invalid;
use super::discovery::{DiscoveryDocument, validate_authority, validate_https_url};
use crate::error::{AuthError, Result};
use serde::{Deserialize, Serialize};

/// Reference to a PEM bundle installed by an enterprise administrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnterpriseCaReference {
    pub pem_file: PathBuf,
}

/// TLS trust configuration for one exact managed-service authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceTrust {
    #[serde(default = "enabled")]
    pub system_roots: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_ca: Option<EnterpriseCaReference>,
}

impl Default for ServiceTrust {
    fn default() -> Self {
        Self {
            system_roots: true,
            enterprise_ca: None,
        }
    }
}

/// Validated discovery metadata persisted without credentials or tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachedDiscovery {
    pub document: DiscoveryDocument,
    pub retrieved_at_unix_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl CachedDiscovery {
    #[must_use]
    pub fn is_fresh_at(&self, now_unix_seconds: u64) -> bool {
        now_unix_seconds.saturating_sub(self.retrieved_at_unix_seconds)
            <= u64::from(self.document.cache.max_age_seconds)
    }
}

/// Non-secret client configuration bound to one exact service authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceProfile {
    pub schema_version: u16,
    pub authority: String,
    pub discovery_origin: String,
    #[serde(default)]
    pub trust: ServiceTrust,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<CachedDiscovery>,
}

impl ServiceProfile {
    /// Creates a profile whose discovery origin is pinned to its exact authority.
    pub fn new(authority: impl Into<String>, trust: ServiceTrust) -> Result<Self> {
        let authority = authority.into();
        let profile = Self {
            schema_version: MANAGED_SCHEMA_VERSION,
            discovery_origin: format!("https://{authority}"),
            authority,
            trust,
            discovery: None,
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Creates a profile from a canonical HTTPS authority root.
    pub fn from_origin(origin: &str, trust: ServiceTrust) -> Result<Self> {
        let parsed = validate_https_url(origin, "service origin")?;
        let authority = parsed.host_str().ok_or_else(|| {
            AuthError::InvalidManagedContract("service origin has no authority".to_owned())
        })?;
        if parsed.port().is_some()
            || parsed.path() != "/"
            || origin.trim_end_matches('/') != format!("https://{authority}")
        {
            return invalid("service origin must be the canonical HTTPS authority root");
        }
        Self::new(authority, trust)
    }

    /// Validates the profile binding and any cached discovery metadata.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != MANAGED_SCHEMA_VERSION {
            return invalid("unsupported managed service profile schema version");
        }
        validate_authority(&self.authority)?;
        let origin = validate_https_url(&self.discovery_origin, "discovery_origin")?;
        if origin.host_str() != Some(self.authority.as_str())
            || origin.port().is_some()
            || origin.path() != "/"
        {
            return invalid("discovery origin must be the HTTPS root of the configured authority");
        }
        if let Some(ca) = &self.trust.enterprise_ca
            && (!ca.pem_file.is_absolute() || ca.pem_file.as_os_str().is_empty())
        {
            return invalid("enterprise CA reference must be an absolute PEM file path");
        }
        if !self.trust.system_roots && self.trust.enterprise_ca.is_none() {
            return invalid("service profile must configure at least one TLS trust source");
        }
        if let Some(cached) = &self.discovery {
            cached.document.validate(&self.authority, &[1])?;
            validate_etag(cached.etag.as_deref())?;
        }
        Ok(())
    }

    #[must_use]
    pub fn discovery_url(&self) -> String {
        format!(
            "{}/.well-known/crab",
            self.discovery_origin.trim_end_matches('/')
        )
    }
}

/// Atomic on-disk store with one JSON profile per exact authority.
pub struct ServiceProfileStore {
    root: PathBuf,
}

/// Errors raised while classifying a repository against installed profiles.
#[derive(Debug, thiserror::Error)]
pub enum ServiceProfileLocatorError {
    /// The repository URL is malformed.
    #[error(transparent)]
    Url(#[from] crab_git::UrlError),
    /// Installed profile state could not be read or validated.
    #[error("managed service profile for {authority} is invalid: {source}")]
    Profile {
        authority: String,
        #[source]
        source: AuthError,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveServiceProfile {
    schema_version: u16,
    authority: String,
}

impl ServiceProfileStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Writes a validated profile atomically without storing token material.
    pub fn store(&self, profile: &ServiceProfile) -> Result<()> {
        profile.validate()?;
        let bytes = serde_json::to_vec_pretty(profile)
            .map_err(|source| AuthError::SerializeServiceProfile { source })?;
        self.write_atomic(&self.profile_path(&profile.authority), &bytes)
    }

    /// Loads and revalidates the profile for exactly `authority`.
    pub fn load(&self, authority: &str) -> Result<Option<ServiceProfile>> {
        validate_authority(authority)?;
        let path = self.profile_path(authority);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let profile: ServiceProfile = serde_json::from_slice(&bytes)
            .map_err(|source| AuthError::ParseServiceProfile { source })?;
        profile.validate()?;
        if profile.authority != authority {
            return invalid("stored service profile authority does not match its lookup key");
        }
        Ok(Some(profile))
    }

    /// Lists every installed profile in authority order.
    ///
    /// Invalid profile documents fail the whole read so callers never offer a
    /// selector whose authority binding has not been validated.
    pub fn list(&self) -> Result<Vec<ServiceProfile>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut authorities = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            let Some(authority) = file_name.strip_suffix(".json") else {
                continue;
            };
            if authority == "active" {
                continue;
            }
            validate_authority(authority)?;
            authorities.push(authority.to_owned());
        }
        authorities.sort_unstable();

        let mut profiles = Vec::with_capacity(authorities.len());
        for authority in authorities {
            if let Some(profile) = self.load(&authority)? {
                profiles.push(profile);
            }
        }
        Ok(profiles)
    }

    /// Classifies a Crab URL using the exact installed-profile set.
    ///
    /// This performs no network discovery. A malformed installed profile is an
    /// error and never causes its authority to fall back to direct storage.
    pub fn classify_repository_url(
        &self,
        url: &str,
    ) -> std::result::Result<crab_git::RepositoryLocator, ServiceProfileLocatorError> {
        let parsed = crab_git::CrabUrl::parse(url)?;
        let has_profile = self
            .load(&parsed.bucket)
            .map_err(|source| ServiceProfileLocatorError::Profile {
                authority: parsed.bucket.clone(),
                source,
            })?
            .is_some();
        Ok(crab_git::RepositoryLocator::parse(url, |_| has_profile)?)
    }

    /// Selects an installed profile without copying credentials into profile storage.
    pub fn set_active(&self, authority: &str) -> Result<()> {
        self.require(authority)?;
        let active = ActiveServiceProfile {
            schema_version: MANAGED_SCHEMA_VERSION,
            authority: authority.to_owned(),
        };
        let bytes = serde_json::to_vec_pretty(&active)
            .map_err(|source| AuthError::SerializeServiceProfile { source })?;
        self.write_atomic(&self.root.join("active.json"), &bytes)
    }

    /// Loads the active profile and rejects stale or mismatched selectors.
    pub fn active(&self) -> Result<Option<ServiceProfile>> {
        let bytes = match fs::read(self.root.join("active.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let active: ActiveServiceProfile = serde_json::from_slice(&bytes)
            .map_err(|source| AuthError::ParseServiceProfile { source })?;
        if active.schema_version != MANAGED_SCHEMA_VERSION {
            return invalid("unsupported active service profile schema version");
        }
        self.require(&active.authority).map(Some)
    }

    pub(crate) fn require(&self, authority: &str) -> Result<ServiceProfile> {
        self.load(authority)?
            .ok_or_else(|| AuthError::ManagedProfileNotFound {
                authority: authority.to_owned(),
            })
    }

    fn profile_path(&self, authority: &str) -> PathBuf {
        self.root.join(format!("{authority}.json"))
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        temporary.write_all(bytes)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .map_err(|error| AuthError::Io(error.error))?;
        sync_directory(&self.root)
    }
}

/// Returns the service-profile directory adjacent to the protected token cache.
#[must_use]
pub fn service_profile_directory(token_cache_directory: &Path) -> PathBuf {
    token_cache_directory
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("service-profiles")
}

/// Returns the isolated encrypted-token cache key for one managed authority.
pub fn managed_token_cache_key(authority: &str) -> Result<String> {
    validate_authority(authority)?;
    Ok(format!("managed-{authority}"))
}

pub(super) fn validate_etag(etag: Option<&str>) -> Result<()> {
    if let Some(etag) = etag
        && (etag.is_empty() || etag.len() > 512 || etag.chars().any(char::is_control))
    {
        return invalid("discovery ETag is invalid");
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

const fn enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::{AuthFlow, CapabilityName, DiscoveryCache, OidcClientDiscovery};

    fn discovery(authority: &str) -> DiscoveryDocument {
        DiscoveryDocument {
            schema_version: 1,
            authority: authority.to_owned(),
            api_base: format!("https://api.{authority}/v1"),
            api_versions: vec![1],
            oidc: OidcClientDiscovery {
                issuer: format!("https://identity.{authority}"),
                client_id: "crab-cli".to_owned(),
                scopes: vec!["openid".to_owned(), "offline_access".to_owned()],
            },
            auth_flows: vec![AuthFlow::new("authorization_code_pkce").unwrap()],
            service_version: "1.0.0".to_owned(),
            minimum_cli_version: "0.1.0".to_owned(),
            capabilities: vec![CapabilityName::new("gateway-v1").unwrap()],
            cache: DiscoveryCache {
                max_age_seconds: 60,
            },
        }
    }

    #[test]
    fn store_round_trip_is_keyed_by_exact_authority_and_contains_no_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let store = ServiceProfileStore::new(directory.path().to_path_buf());
        let mut profile =
            ServiceProfile::new("code.corp.example", ServiceTrust::default()).unwrap();
        profile.discovery = Some(CachedDiscovery {
            document: discovery("code.corp.example"),
            retrieved_at_unix_seconds: 123,
            etag: Some("\"discovery-v1\"".to_owned()),
        });

        store.store(&profile).unwrap();

        assert_eq!(store.load("code.corp.example").unwrap(), Some(profile));
        assert!(store.load("other.corp.example").unwrap().is_none());
        let body = fs::read_to_string(directory.path().join("code.corp.example.json")).unwrap();
        assert!(!body.contains("token"));
        assert!(!body.contains("credential"));
    }

    #[test]
    fn profile_rejects_authority_alias_origin_and_relative_ca() {
        let mut profile =
            ServiceProfile::new("code.corp.example", ServiceTrust::default()).unwrap();
        profile.discovery_origin = "https://alias.corp.example".to_owned();
        assert!(profile.validate().is_err());

        profile.discovery_origin = "https://code.corp.example".to_owned();
        profile.trust.enterprise_ca = Some(EnterpriseCaReference {
            pem_file: PathBuf::from("corp-ca.pem"),
        });
        assert!(profile.validate().is_err());

        profile.trust.enterprise_ca = None;
        profile.trust.system_roots = false;
        assert!(profile.validate().is_err());
    }

    #[test]
    fn profile_origin_requires_canonical_https_root() {
        assert_eq!(
            ServiceProfile::from_origin("https://crab.build", ServiceTrust::default())
                .unwrap()
                .authority,
            "crab.build"
        );
        assert!(ServiceProfile::from_origin("http://crab.build", ServiceTrust::default()).is_err());
        assert!(
            ServiceProfile::from_origin("https://Crab.Build", ServiceTrust::default()).is_err()
        );
        assert!(
            ServiceProfile::from_origin("https://crab.build/api", ServiceTrust::default()).is_err()
        );
    }

    #[test]
    fn load_rejects_profile_stored_under_another_authority() {
        let directory = tempfile::tempdir().unwrap();
        let profile = ServiceProfile::new("code.corp.example", ServiceTrust::default()).unwrap();
        fs::write(
            directory.path().join("other.corp.example.json"),
            serde_json::to_vec(&profile).unwrap(),
        )
        .unwrap();
        let store = ServiceProfileStore::new(directory.path().to_path_buf());

        assert!(store.load("other.corp.example").is_err());
    }

    #[test]
    fn cached_discovery_freshness_saturates_clock_skew() {
        let cached = CachedDiscovery {
            document: discovery("crab.build"),
            retrieved_at_unix_seconds: 100,
            etag: None,
        };

        assert!(cached.is_fresh_at(99));
        assert!(cached.is_fresh_at(160));
        assert!(!cached.is_fresh_at(161));
    }

    #[test]
    fn active_profile_selection_round_trips_without_token_material() {
        let directory = tempfile::tempdir().unwrap();
        let store = ServiceProfileStore::new(directory.path().to_path_buf());
        let profile = ServiceProfile::new("crab.build", ServiceTrust::default()).unwrap();
        store.store(&profile).unwrap();

        store.set_active("crab.build").unwrap();

        assert_eq!(store.active().unwrap(), Some(profile));
        let selector = fs::read_to_string(directory.path().join("active.json")).unwrap();
        assert!(!selector.contains("token"));
    }

    #[test]
    fn installed_profiles_are_sorted_and_exclude_the_active_selector() {
        let directory = tempfile::tempdir().unwrap();
        let store = ServiceProfileStore::new(directory.path().to_path_buf());
        let hosted = ServiceProfile::new("crab.build", ServiceTrust::default()).unwrap();
        let enterprise = ServiceProfile::new("code.corp.example", ServiceTrust::default()).unwrap();
        store.store(&hosted).unwrap();
        store.store(&enterprise).unwrap();
        store.set_active("crab.build").unwrap();

        let authorities = store
            .list()
            .unwrap()
            .into_iter()
            .map(|profile| profile.authority)
            .collect::<Vec<_>>();

        assert_eq!(authorities, vec!["code.corp.example", "crab.build"]);
    }

    #[test]
    fn managed_token_keys_and_profile_directory_are_isolated() {
        assert_eq!(
            managed_token_cache_key("code.corp.example").unwrap(),
            "managed-code.corp.example"
        );
        assert_eq!(
            service_profile_directory(Path::new("/config/crab/tokens")),
            PathBuf::from("/config/crab/service-profiles")
        );
    }

    #[test]
    fn repository_classification_uses_reserved_and_exact_profile_authorities() {
        let directory = tempfile::tempdir().unwrap();
        let store = ServiceProfileStore::new(directory.path().to_path_buf());
        let profile = ServiceProfile::new("code.corp.example", ServiceTrust::default()).unwrap();
        store.store(&profile).unwrap();

        let hosted = store
            .classify_repository_url("crab://crab.build/acme/models")
            .unwrap();
        let enterprise = store
            .classify_repository_url("crab://code.corp.example/acme/models")
            .unwrap();
        let direct = store
            .classify_repository_url("crab://customer-bucket/acme/models")
            .unwrap();

        assert!(matches!(hosted, crab_git::RepositoryLocator::Managed(_)));
        assert!(matches!(
            enterprise,
            crab_git::RepositoryLocator::Managed(_)
        ));
        assert!(matches!(direct, crab_git::RepositoryLocator::Direct(_)));
    }
}
