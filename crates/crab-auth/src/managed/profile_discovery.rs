use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use reqwest::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use reqwest::{Certificate, StatusCode, Url};

use super::profile::{CachedDiscovery, ServiceProfileStore, validate_etag};
use super::{DiscoveryDocument, ServiceProfile};
use crate::error::{AuthError, Result};

const SUPPORTED_API_VERSIONS: &[u16] = &[1];
const MAX_DISCOVERY_BODY_BYTES: usize = 1024 * 1024;
const MAX_STALE_SECONDS: u64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryCacheStatus {
    Fresh,
    Revalidated,
    Refreshed,
    Stale,
}

/// Validated discovery plus the highest mutually supported API version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDiscovery {
    pub document: DiscoveryDocument,
    pub api_version: u16,
    pub cache_status: DiscoveryCacheStatus,
}

/// Resolves and persists discovery for profiles without handling user tokens.
pub struct ManagedDiscoveryClient {
    transport: Arc<dyn DiscoveryTransport>,
}

impl Default for ManagedDiscoveryClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagedDiscoveryClient {
    #[must_use]
    pub fn new() -> Self {
        Self {
            transport: Arc::new(ReqwestDiscoveryTransport),
        }
    }

    /// Uses fresh cached metadata or refreshes it with an ETag-bound HTTPS request.
    pub async fn resolve(
        &self,
        store: &ServiceProfileStore,
        authority: &str,
    ) -> Result<ResolvedDiscovery> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        self.resolve_at(store, authority, now).await
    }

    /// Discovers and stores a new profile only after the response validates.
    pub async fn bootstrap(
        &self,
        store: &ServiceProfileStore,
        profile: ServiceProfile,
    ) -> Result<ResolvedDiscovery> {
        if profile.discovery.is_some() {
            return Err(AuthError::InvalidManagedContract(
                "service profile bootstrap requires an empty discovery cache".to_owned(),
            ));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        self.resolve_profile_at(store, profile, now).await
    }

    async fn resolve_at(
        &self,
        store: &ServiceProfileStore,
        authority: &str,
        now_unix_seconds: u64,
    ) -> Result<ResolvedDiscovery> {
        let profile = store.require(authority)?;
        self.resolve_profile_at(store, profile, now_unix_seconds)
            .await
    }

    async fn resolve_profile_at(
        &self,
        store: &ServiceProfileStore,
        mut profile: ServiceProfile,
        now_unix_seconds: u64,
    ) -> Result<ResolvedDiscovery> {
        let authority = profile.authority.clone();
        profile.validate()?;
        if let Some(cached) = profile.discovery.as_ref()
            && cached.is_fresh_at(now_unix_seconds)
        {
            return resolved(cached.document.clone(), DiscoveryCacheStatus::Fresh);
        }

        let request = DiscoveryRequest {
            endpoint: Url::parse(&profile.discovery_url()).map_err(|_| {
                AuthError::InvalidManagedContract("profile discovery URL is invalid".to_owned())
            })?,
            if_none_match: profile
                .discovery
                .as_ref()
                .and_then(|cached| cached.etag.clone()),
            enterprise_ca: profile
                .trust
                .enterprise_ca
                .as_ref()
                .map(|ca| ca.pem_file.clone()),
            system_roots: profile.trust.system_roots,
        };
        let response = match self.transport.fetch(request).await {
            Ok(response) => response,
            Err(error) => {
                if self.can_use_stale(&profile, now_unix_seconds).await
                    && let Some(cached) = profile.discovery
                {
                    return resolved(cached.document, DiscoveryCacheStatus::Stale);
                }
                return Err(error);
            }
        };

        if response.status == StatusCode::NOT_MODIFIED {
            let Some(cached) = profile.discovery.as_mut() else {
                return Err(AuthError::ManagedDiscoveryRejected {
                    endpoint: profile.discovery_url(),
                    status: response.status.as_u16(),
                });
            };
            validate_cache_control(response.cache_max_age_seconds, &cached.document)?;
            cached.retrieved_at_unix_seconds = now_unix_seconds;
            if response.etag.is_some() {
                cached.etag = response.etag;
            }
            validate_etag(cached.etag.as_deref())?;
            let document = cached.document.clone();
            store.store(&profile)?;
            return resolved(document, DiscoveryCacheStatus::Revalidated);
        }

        if response.status.is_server_error() {
            if self.can_use_stale(&profile, now_unix_seconds).await
                && let Some(cached) = profile.discovery
            {
                return resolved(cached.document, DiscoveryCacheStatus::Stale);
            }
            return Err(AuthError::ManagedDiscoveryUnavailable {
                authority: profile.authority,
            });
        }
        if response.status != StatusCode::OK {
            return Err(AuthError::ManagedDiscoveryRejected {
                endpoint: profile.discovery_url(),
                status: response.status.as_u16(),
            });
        }

        let document: DiscoveryDocument = serde_json::from_slice(&response.body)
            .map_err(|source| AuthError::ParseManagedDiscovery { source })?;
        document.validate(&authority, SUPPORTED_API_VERSIONS)?;
        validate_cache_control(response.cache_max_age_seconds, &document)?;
        validate_etag(response.etag.as_deref())?;
        profile.discovery = Some(CachedDiscovery {
            document: document.clone(),
            retrieved_at_unix_seconds: now_unix_seconds,
            etag: response.etag,
        });
        store.store(&profile)?;
        resolved(document, DiscoveryCacheStatus::Refreshed)
    }

    async fn can_use_stale(&self, profile: &ServiceProfile, now_unix_seconds: u64) -> bool {
        let Some(cached) = profile.discovery.as_ref() else {
            return false;
        };
        let stale_limit =
            u64::from(cached.document.cache.max_age_seconds).saturating_add(MAX_STALE_SECONDS);
        if now_unix_seconds.saturating_sub(cached.retrieved_at_unix_seconds) > stale_limit {
            return false;
        }
        let Ok(api_base) = Url::parse(&cached.document.api_base) else {
            return false;
        };
        self.transport
            .probe_origin(
                api_base,
                profile
                    .trust
                    .enterprise_ca
                    .as_ref()
                    .map(|ca| ca.pem_file.clone()),
                profile.trust.system_roots,
            )
            .await
    }

    #[cfg(test)]
    fn with_transport(transport: Arc<dyn DiscoveryTransport>) -> Self {
        Self { transport }
    }
}

/// Builds the redirect-free HTTPS client used for a profile's OIDC and API calls.
pub fn managed_http_client(profile: &ServiceProfile) -> Result<reqwest::Client> {
    profile.validate()?;
    let endpoint = Url::parse(&profile.discovery_origin).map_err(|_| {
        AuthError::InvalidManagedContract("profile discovery origin is invalid".to_owned())
    })?;
    client(
        profile
            .trust
            .enterprise_ca
            .as_ref()
            .map(|ca| ca.pem_file.as_path()),
        profile.trust.system_roots,
        &endpoint,
    )
}

fn resolved(
    document: DiscoveryDocument,
    cache_status: DiscoveryCacheStatus,
) -> Result<ResolvedDiscovery> {
    let api_version = document
        .api_versions
        .iter()
        .filter(|version| SUPPORTED_API_VERSIONS.contains(version))
        .max()
        .copied()
        .ok_or_else(|| AuthError::UnsupportedManagedApiVersion {
            supported: SUPPORTED_API_VERSIONS.to_vec(),
            advertised: document.api_versions.clone(),
        })?;
    Ok(ResolvedDiscovery {
        document,
        api_version,
        cache_status,
    })
}

fn validate_cache_control(
    response_max_age_seconds: Option<u32>,
    document: &DiscoveryDocument,
) -> Result<()> {
    if response_max_age_seconds != Some(document.cache.max_age_seconds) {
        return Err(AuthError::InvalidManagedContract(
            "discovery Cache-Control max-age does not match the response document".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct DiscoveryRequest {
    endpoint: Url,
    if_none_match: Option<String>,
    enterprise_ca: Option<PathBuf>,
    system_roots: bool,
}

#[derive(Debug)]
struct DiscoveryResponse {
    status: StatusCode,
    etag: Option<String>,
    cache_max_age_seconds: Option<u32>,
    body: Vec<u8>,
}

#[async_trait]
trait DiscoveryTransport: Send + Sync {
    async fn fetch(&self, request: DiscoveryRequest) -> Result<DiscoveryResponse>;
    async fn probe_origin(
        &self,
        endpoint: Url,
        enterprise_ca: Option<PathBuf>,
        system_roots: bool,
    ) -> bool;
}

struct ReqwestDiscoveryTransport;

#[async_trait]
impl DiscoveryTransport for ReqwestDiscoveryTransport {
    async fn fetch(&self, request: DiscoveryRequest) -> Result<DiscoveryResponse> {
        let client = client(
            request.enterprise_ca.as_deref(),
            request.system_roots,
            &request.endpoint,
        )?;
        let mut builder = client.get(request.endpoint.clone());
        if let Some(etag) = request.if_none_match {
            builder = builder.header(IF_NONE_MATCH, etag);
        }
        let mut response =
            builder
                .send()
                .await
                .map_err(|source| AuthError::ManagedDiscoveryRequest {
                    endpoint: request.endpoint.to_string(),
                    source,
                })?;
        if response.url() != &request.endpoint {
            return Err(AuthError::InvalidManagedContract(
                "discovery response origin changed during the request".to_owned(),
            ));
        }
        let status = response.status();
        let etag = optional_header(response.headers(), ETAG.as_str())?;
        let cache_control = optional_header(response.headers(), CACHE_CONTROL.as_str())?;
        let cache_max_age_seconds = cache_control.as_deref().and_then(cache_max_age);
        let mut body = Vec::new();
        while let Some(chunk) =
            response
                .chunk()
                .await
                .map_err(|source| AuthError::ManagedDiscoveryRequest {
                    endpoint: request.endpoint.to_string(),
                    source,
                })?
        {
            if body.len().saturating_add(chunk.len()) > MAX_DISCOVERY_BODY_BYTES {
                return Err(AuthError::InvalidManagedContract(
                    "discovery response exceeds the one MiB limit".to_owned(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(DiscoveryResponse {
            status,
            etag,
            cache_max_age_seconds,
            body,
        })
    }

    async fn probe_origin(
        &self,
        endpoint: Url,
        enterprise_ca: Option<PathBuf>,
        system_roots: bool,
    ) -> bool {
        let Ok(client) = client(enterprise_ca.as_deref(), system_roots, &endpoint) else {
            return false;
        };
        client
            .get(endpoint.clone())
            .send()
            .await
            .is_ok_and(|response| {
                response.url().scheme() == endpoint.scheme()
                    && response.url().host_str() == endpoint.host_str()
                    && response.url().port_or_known_default() == endpoint.port_or_known_default()
            })
    }
}

fn client(
    enterprise_ca: Option<&std::path::Path>,
    system_roots: bool,
    endpoint: &Url,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .tls_built_in_root_certs(system_roots);
    if let Some(path) = enterprise_ca {
        let pem = std::fs::read(path)?;
        let certificate =
            Certificate::from_pem(&pem).map_err(|source| AuthError::ManagedDiscoveryRequest {
                endpoint: endpoint.to_string(),
                source,
            })?;
        builder = builder.add_root_certificate(certificate);
    }
    builder
        .build()
        .map_err(|source| AuthError::ManagedDiscoveryRequest {
            endpoint: endpoint.to_string(),
            source,
        })
}

fn optional_header(headers: &reqwest::header::HeaderMap, name: &str) -> Result<Option<String>> {
    headers
        .get(name)
        .map(|value| {
            value.to_str().map(str::to_owned).map_err(|_| {
                AuthError::InvalidManagedContract(format!(
                    "discovery {name} header is not valid text"
                ))
            })
        })
        .transpose()
}

fn cache_max_age(value: &str) -> Option<u32> {
    value.split(',').map(str::trim).find_map(|directive| {
        directive
            .strip_prefix("max-age=")
            .and_then(|seconds| seconds.parse().ok())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::managed::{
        AuthFlow, CapabilityName, DiscoveryCache, OidcClientDiscovery, ServiceTrust,
    };

    struct MockTransport {
        responses: Mutex<VecDeque<Result<DiscoveryResponse>>>,
        requests: Mutex<Vec<DiscoveryRequest>>,
        reachable: bool,
    }

    #[async_trait]
    impl DiscoveryTransport for MockTransport {
        async fn fetch(&self, request: DiscoveryRequest) -> Result<DiscoveryResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses.lock().unwrap().pop_front().unwrap()
        }

        async fn probe_origin(
            &self,
            _endpoint: Url,
            _enterprise_ca: Option<PathBuf>,
            _system_roots: bool,
        ) -> bool {
            self.reachable
        }
    }

    fn document(authority: &str) -> DiscoveryDocument {
        DiscoveryDocument {
            schema_version: 1,
            authority: authority.to_owned(),
            api_base: format!("https://api.{authority}/v1"),
            api_versions: vec![1],
            oidc: OidcClientDiscovery {
                issuer: format!("https://identity.{authority}"),
                client_id: "crab-cli".to_owned(),
                scopes: vec!["openid".to_owned()],
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

    fn stored_profile(directory: &tempfile::TempDir, retrieved_at: u64) -> ServiceProfileStore {
        let store = ServiceProfileStore::new(directory.path().to_path_buf());
        let mut profile = ServiceProfile::new("crab.build", ServiceTrust::default()).unwrap();
        profile.discovery = Some(CachedDiscovery {
            document: document("crab.build"),
            retrieved_at_unix_seconds: retrieved_at,
            etag: Some("\"old\"".to_owned()),
        });
        store.store(&profile).unwrap();
        store
    }

    fn response(status: StatusCode, document: Option<&DiscoveryDocument>) -> DiscoveryResponse {
        DiscoveryResponse {
            status,
            etag: Some("\"new\"".to_owned()),
            cache_max_age_seconds: Some(60),
            body: document
                .map(|document| serde_json::to_vec(document).unwrap())
                .unwrap_or_default(),
        }
    }

    #[tokio::test]
    async fn fresh_profile_uses_cache_without_network_request() {
        let directory = tempfile::tempdir().unwrap();
        let store = stored_profile(&directory, 100);
        let transport = Arc::new(MockTransport {
            responses: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            reachable: false,
        });
        let client = ManagedDiscoveryClient::with_transport(transport.clone());

        let resolved = client.resolve_at(&store, "crab.build", 150).await.unwrap();

        assert_eq!(resolved.cache_status, DiscoveryCacheStatus::Fresh);
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn expired_profile_revalidates_with_etag_and_persists_retrieval_time() {
        let directory = tempfile::tempdir().unwrap();
        let store = stored_profile(&directory, 100);
        let transport = Arc::new(MockTransport {
            responses: Mutex::new(VecDeque::from([Ok(response(
                StatusCode::NOT_MODIFIED,
                None,
            ))])),
            requests: Mutex::new(Vec::new()),
            reachable: false,
        });
        let client = ManagedDiscoveryClient::with_transport(transport.clone());

        let resolved = client.resolve_at(&store, "crab.build", 161).await.unwrap();

        assert_eq!(resolved.cache_status, DiscoveryCacheStatus::Revalidated);
        let requests = transport.requests.lock().unwrap();
        assert_eq!(
            requests[0].endpoint.as_str(),
            "https://crab.build/.well-known/crab"
        );
        assert_eq!(requests[0].if_none_match.as_deref(), Some("\"old\""));
        drop(requests);
        let cached = store
            .load("crab.build")
            .unwrap()
            .unwrap()
            .discovery
            .unwrap();
        assert_eq!(cached.retrieved_at_unix_seconds, 161);
        assert_eq!(cached.etag.as_deref(), Some("\"new\""));
    }

    #[tokio::test]
    async fn changed_authority_fails_closed_and_retains_cached_profile() {
        let directory = tempfile::tempdir().unwrap();
        let store = stored_profile(&directory, 100);
        let changed = document("other.example");
        let transport = Arc::new(MockTransport {
            responses: Mutex::new(VecDeque::from([Ok(response(
                StatusCode::OK,
                Some(&changed),
            ))])),
            requests: Mutex::new(Vec::new()),
            reachable: true,
        });
        let client = ManagedDiscoveryClient::with_transport(transport);

        assert!(client.resolve_at(&store, "crab.build", 161).await.is_err());
        let cached = store
            .load("crab.build")
            .unwrap()
            .unwrap()
            .discovery
            .unwrap();
        assert_eq!(cached.document.authority, "crab.build");
        assert_eq!(cached.retrieved_at_unix_seconds, 100);
    }

    #[tokio::test]
    async fn bounded_stale_requires_reachable_pinned_api_origin() {
        let directory = tempfile::tempdir().unwrap();
        let store = stored_profile(&directory, 100);
        let transport = Arc::new(MockTransport {
            responses: Mutex::new(VecDeque::from([Err(
                AuthError::ManagedDiscoveryUnavailable {
                    authority: "crab.build".to_owned(),
                },
            )])),
            requests: Mutex::new(Vec::new()),
            reachable: true,
        });
        let client = ManagedDiscoveryClient::with_transport(transport);

        let resolved = client.resolve_at(&store, "crab.build", 200).await.unwrap();

        assert_eq!(resolved.cache_status, DiscoveryCacheStatus::Stale);
    }

    #[tokio::test]
    async fn stale_profile_beyond_bound_fails_even_when_origin_is_reachable() {
        let directory = tempfile::tempdir().unwrap();
        let store = stored_profile(&directory, 100);
        let transport = Arc::new(MockTransport {
            responses: Mutex::new(VecDeque::from([Err(
                AuthError::ManagedDiscoveryUnavailable {
                    authority: "crab.build".to_owned(),
                },
            )])),
            requests: Mutex::new(Vec::new()),
            reachable: true,
        });
        let client = ManagedDiscoveryClient::with_transport(transport);

        assert!(
            client
                .resolve_at(&store, "crab.build", 100 + 60 + MAX_STALE_SECONDS + 1)
                .await
                .is_err()
        );
    }

    #[test]
    fn cache_control_parses_only_numeric_max_age() {
        assert_eq!(cache_max_age("public, max-age=3600"), Some(3600));
        assert_eq!(cache_max_age("public, max-age=forever"), None);
    }
}
