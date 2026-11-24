//! Crab Auth enterprise service client.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use crab_coordination::active_active::ActiveActiveReplicationConfig;
use crab_types::storage::StorageScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::cdn::{CdnViewerGrant, valid_approval_digest};
use crate::client_config::CrabAuthClientConfig;
use crate::credential_provider::CredentialProvider;
use crate::credential_response::{CrabAuthCredentialResponse, parse_credential_response};
use crate::credentials::{CloudCredentials, CredentialResolution};
use crate::error::{AuthError, Result};
use crate::protected_push::{
    PushFinalizeResponse, PushPrepareResponse, PushRefUpdate, validate_push_finalize_response,
    validate_push_prepare_response,
};
use crate::token_cache::{CachedTokens, TokenCache};

mod view_retention;
use view_retention::{RetainedView, RetainedViewProof};
pub use view_retention::{ViewReadOperation, ViewRetention, ViewRetentionLease};

/// Refresh window: credentials within 5 minutes of expiry trigger a refresh.
const REFRESH_WINDOW: Duration = Duration::from_secs(300);
const MAX_NEGOTIATED_CREDENTIAL_BYTES: usize = 1024 * 1024;

/// Default retry count for HTTP 5xx errors.
const MAX_RETRIES: u32 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Canonical token-cache key for Crab Auth.
const CRAB_AUTH_CACHE_KEY: &str = "crab-auth";

/// Request body sent to the Crab Auth endpoint.
#[derive(Serialize)]
struct CrabAuthRequest<'a> {
    id_token: String,
    repo_url: String,
    operation: String,
    client_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retained_view: Option<RetainedViewProof<'a>>,
}

#[derive(Serialize)]
struct PushPrepareRequest {
    id_token: String,
    repo_url: String,
    ref_updates: Vec<PushRefUpdate>,
    client_version: String,
}

#[derive(Debug)]
pub struct ProtectedPushPrepare {
    pub credentials: CloudCredentials,
    pub expires_at: SystemTime,
    pub push_id: String,
    pub upload_prefix: String,
}

#[derive(Serialize)]
struct PushFinalizeRequest {
    id_token: String,
    repo_url: String,
    ref_updates: Vec<PushRefUpdate>,
    push_id: String,
    client_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_active: Option<PushFinalizeActiveActive>,
}

#[derive(Debug, Clone, Serialize)]
struct PushFinalizeActiveActive {
    replication: ActiveActiveReplicationConfig,
    writer: String,
}

/// Client for an enterprise-hosted Crab Auth credential service.
pub struct CrabAuthProvider {
    endpoint: String,
    issuer_url: String,
    client_id: String,
    client_version: String,
    token_cache: Arc<TokenCache>,
    cached: RwLock<Option<CachedCrabAuthCreds>>,
    http: reqwest::Client,
    cdn: Vec<CdnViewerDeployment>,
    allow_viewer_grants: bool,
    cdn_principal: OnceLock<[u8; 32]>,
    retained_view: OnceLock<Arc<RetainedView>>,
}

/// Trusted viewer deployment selected for one source repository.
#[derive(Clone, Debug)]
pub struct CdnViewerDeployment {
    pub bucket: String,
    pub source_repo: String,
    pub origin: String,
    pub base_url: String,
    approval_sha256: Option<String>,
}

const CDN_APPROVAL_HEADER: &str = "x-crab-cdn-approval-sha256";
const MAX_CDN_APPROVALS: usize = 16;

impl CdnViewerDeployment {
    /// Return the native storage provider named by this validated deployment.
    #[must_use]
    pub fn storage_provider(&self) -> Option<crab_types::storage::StorageProviderKind> {
        use crate::cdn::CdnProvider;
        use crab_types::storage::StorageProviderKind;
        Some(match native_cdn_identity(&self.origin)?.0 {
            CdnProvider::Cloudfront => StorageProviderKind::S3,
            CdnProvider::CloudCdn => StorageProviderKind::Gcs,
            CdnProvider::FrontDoor => StorageProviderKind::Azure,
        })
    }

    /// Validate a native deployment and derive its bucket or container identity.
    pub fn new(origin: &str, source_repo: &str, base_url: &str) -> Result<Self> {
        use crate::cdn::{canonical_https_base, canonical_prefix};

        let base_url = base_url.trim_end_matches('/');
        let Some((_, bucket)) = native_cdn_identity(origin)
            .filter(|_| canonical_prefix(source_repo) && canonical_https_base(base_url))
        else {
            return Err(AuthError::InvalidCredentialResponse(
                "invalid native origin, repository prefix or CDN HTTPS base".into(),
            ));
        };
        Ok(Self {
            bucket: bucket.to_owned(),
            source_repo: source_repo.to_owned(),
            origin: origin.to_owned(),
            base_url: base_url.to_owned(),
            approval_sha256: None,
        })
    }

    /// Pin the reviewed private deployment receipt. Omit it to use native authorization.
    pub fn with_approval(mut self, sha256: Option<&str>) -> Result<Self> {
        if sha256.is_some_and(|value| !valid_approval_digest(value)) {
            return Err(AuthError::InvalidCrabAuthRequest(
                "expected a lowercase SHA-256 digest for private CDN approval".into(),
            ));
        }
        self.approval_sha256 = sha256.map(str::to_owned);
        Ok(self)
    }

    /// Return the trusted digest carried in the viewer negotiation.
    #[must_use]
    pub fn approval_sha256(&self) -> Option<&str> {
        self.approval_sha256.as_deref()
    }

    /// Validate that public bytes are served from a distinct native export of the same cloud.
    pub fn validate_public_export(&self, export_origin: &str) -> Result<()> {
        if native_cdn_identity(&self.origin).is_none()
            || native_cdn_identity(export_origin).is_none()
            || !crab_types::storage::public_cdn_export_is_isolated(&self.origin, export_origin)
        {
            return Err(AuthError::InvalidCredentialResponse(
                "public CDN requires a separate export bucket or Azure storage account in the source cloud".into(),
            ));
        }
        Ok(())
    }
}

fn native_cdn_identity(origin: &str) -> Option<(crate::cdn::CdnProvider, &str)> {
    use crate::cdn::{CdnProvider, canonical_origin};

    let identity = if let Some(bucket) = origin.strip_prefix("s3://") {
        (CdnProvider::Cloudfront, bucket)
    } else if let Some(bucket) = origin.strip_prefix("gs://") {
        (CdnProvider::CloudCdn, bucket)
    } else {
        let (_, container) = origin.split_once(".blob.core.windows.net/")?;
        (CdnProvider::FrontDoor, container)
    };
    canonical_origin(identity.0, origin).then_some(identity)
}

impl std::fmt::Debug for CrabAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrabAuthProvider")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Creates a Crab Auth provider for credential and protected-push operations.
pub fn create_crab_auth_provider(config: CrabAuthClientConfig) -> Result<CrabAuthProvider> {
    CrabAuthProvider::new(config)
}

/// In-memory cached credentials with expiry.
struct CachedCrabAuthCreds {
    bucket: String,
    source_prefix: String,
    operation: String,
    creds: CloudCredentials,
    storage_scope: Option<StorageScope>,
    expires_at: SystemTime,
    cdn: Option<CdnViewerGrant>,
    captured_at: SystemTime,
}

impl CrabAuthProvider {
    /// Creates a Crab Auth provider from validated auth-domain config.
    pub fn new(config: CrabAuthClientConfig) -> Result<Self> {
        let cache_path = shellexpand_tilde(&config.token_cache_path);
        let token_cache = Arc::new(TokenCache::new(PathBuf::from(cache_path))?);
        // The request body also carries the identity token. Redirects must
        // never forward it to another credential endpoint.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|source| AuthError::CrabAuthRequest {
                operation: "construct credential transport",
                endpoint: config.endpoint.clone(),
                source,
            })?;

        Ok(Self {
            endpoint: config.endpoint,
            issuer_url: config.issuer_url,
            client_id: config.client_id,
            client_version: config.client_version,
            token_cache,
            cached: RwLock::new(None),
            http,
            cdn: Vec::new(),
            allow_viewer_grants: true,
            cdn_principal: OnceLock::new(),
            retained_view: OnceLock::new(),
        })
    }

    /// Opt in to short-lived viewer grants for the selected repository.
    pub fn with_cdn(self, deployment: CdnViewerDeployment) -> Result<Self> {
        self.with_cdn_deployments(vec![deployment])
    }

    /// Approve native deployments for repositories whose source locators share a bucket name.
    pub fn with_cdn_deployments(mut self, deployments: Vec<CdnViewerDeployment>) -> Result<Self> {
        crate::cdn_checkout::validate_issuer_urls(&self.endpoint, &self.issuer_url)?;
        // An earlier origin-only authorization has no pinned CDN principal.
        *self.cached.get_mut() = None;
        self.cdn = deployments;
        Ok(self)
    }

    /// Disable viewer grant negotiation while retaining the bound issuer and principal.
    #[must_use]
    pub fn with_viewer_grants(mut self, enabled: bool) -> Self {
        if enabled != self.allow_viewer_grants {
            *self.cached.get_mut() = None;
            self.allow_viewer_grants = enabled;
        }
        self
    }

    /// Binds a private checkout process to the principal selected by its SDK parent.
    pub fn with_cdn_checkout(
        self,
        deployment: CdnViewerDeployment,
        principal: [u8; 32],
    ) -> Result<Self> {
        let provider = self.with_cdn(deployment)?;
        let pinned = provider.cdn_principal.get_or_init(|| principal);
        if *pinned != principal {
            return Err(bound_principal_changed());
        }
        Ok(provider)
    }

    /// Returns the principal fingerprint established by a private CDN read.
    #[must_use]
    pub fn cdn_principal(&self) -> Option<[u8; 32]> {
        self.cdn_principal.get().copied()
    }

    /// Checks if cached credentials are still valid.
    fn is_cached_valid(cached: Option<&CachedCrabAuthCreds>) -> bool {
        let now = SystemTime::now();
        cached.is_some_and(|cached| {
            cached
                .expires_at
                .duration_since(now)
                .is_ok_and(|ttl| ttl > REFRESH_WINDOW)
                && cached.cdn.as_ref().is_none_or(|grant| {
                    grant
                        .refresh_at(cached.captured_at)
                        .is_some_and(|refresh_at| now < refresh_at)
                })
        })
    }

    fn cached_matches(
        cached: &CachedCrabAuthCreds,
        bucket: &str,
        prefix: &str,
        operation: &str,
    ) -> bool {
        cached.storage_scope.is_none()
            && cached.bucket == bucket
            && cached.source_prefix == prefix
            && cached.operation == normalize_operation(operation)
    }

    async fn call_endpoint(
        &self,
        id_token: &str,
        repo_url: &str,
        operation: &str,
        bound_read: bool,
        approval_pins: Option<&str>,
        retained_view: Option<RetainedViewProof<'_>>,
    ) -> Result<(CrabAuthCredentialResponse, Option<String>)> {
        let body = CrabAuthRequest {
            id_token: id_token.to_owned(),
            repo_url: repo_url.to_owned(),
            operation: operation.to_owned(),
            client_version: self.client_version.clone(),
            retained_view,
        };

        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_secs(1 << (attempt - 1));
                debug!(
                    attempt,
                    delay_secs = delay.as_secs(),
                    "retrying crab-auth request"
                );
                tokio::time::sleep(delay).await;
            }

            let request = self
                .http
                .post(&self.endpoint)
                .header("Content-Type", "application/json")
                .bearer_auth(id_token)
                .json(&body)
                .timeout(REQUEST_TIMEOUT);
            let request = if let Some(pins) = approval_pins {
                request
                    .header("X-Crab-CDN-Grant-Version", "1")
                    .header(CDN_APPROVAL_HEADER, pins)
            } else {
                request
            };
            let result = request.send().await;

            let mut resp = match result {
                Ok(r) => r,
                Err(source) => {
                    last_err = Some(AuthError::CrabAuthRequest {
                        operation: "credentials",
                        endpoint: self.endpoint.clone(),
                        source,
                    });
                    continue;
                }
            };

            let status = resp.status();
            let read_error = |source| AuthError::CrabAuthRequest {
                operation: "read credentials response",
                endpoint: self.endpoint.clone(),
                source,
            };
            if bound_read && status.is_success() {
                let approvals = resp.headers().get_all(CDN_APPROVAL_HEADER);
                let mut approvals = approvals.iter();
                let approval = approvals
                    .next()
                    .map(|value| {
                        value
                            .to_str()
                            .ok()
                            .filter(|value| valid_approval_digest(value))
                            .map(str::to_owned)
                            .ok_or_else(invalid_negotiated_response)
                    })
                    .transpose()?;
                if approvals.next().is_some() {
                    return Err(invalid_negotiated_response());
                }
                let mut body = Vec::new();
                while let Some(chunk) = resp.chunk().await.map_err(read_error)? {
                    if body.len().saturating_add(chunk.len()) > MAX_NEGOTIATED_CREDENTIAL_BYTES {
                        return Err(AuthError::InvalidCredentialResponse(
                            "negotiated credential response exceeds its byte limit".into(),
                        ));
                    }
                    body.extend_from_slice(&chunk);
                }
                // Parse errors can echo invalid values from the issuer's secret-bearing body.
                let response = std::str::from_utf8(&body)
                    .ok()
                    .and_then(|body| parse_credential_response(body).ok())
                    .ok_or_else(invalid_negotiated_response)?;
                if response.cdn.is_some() != approval.is_some() {
                    return Err(invalid_negotiated_response());
                }
                return Ok((response, approval));
            }
            let resp_body = if bound_read {
                "credential response omitted".to_owned()
            } else {
                resp.text().await.map_err(read_error)?
            };
            if status.is_success() {
                return parse_credential_response(&resp_body).map(|response| (response, None));
            }

            let error = AuthError::CrabAuthRejected {
                operation: "credentials",
                endpoint: self.endpoint.clone(),
                status: status.as_u16(),
                body: resp_body,
            };

            if status.is_server_error() {
                last_err = Some(error);
                continue;
            }

            return Err(error);
        }

        Err(last_err.unwrap_or_else(|| AuthError::CrabAuthFailed {
            operation: "credentials",
            endpoint: self.endpoint.clone(),
            reason: "request failed after retries".into(),
        }))
    }

    async fn call_json_endpoint<T, R>(
        &self,
        path: &'static str,
        id_token: &str,
        body: &T,
    ) -> Result<R>
    where
        T: Serialize + Sync,
        R: for<'de> Deserialize<'de>,
    {
        let url = self.endpoint_for(path);
        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_secs(1 << (attempt - 1));
                debug!(
                    attempt,
                    delay_secs = delay.as_secs(),
                    "retrying crab-auth request"
                );
                tokio::time::sleep(delay).await;
            }

            let result = self
                .http
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {id_token}"))
                .json(body)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await;

            let resp = match result {
                Ok(r) => r,
                Err(source) => {
                    last_err = Some(AuthError::CrabAuthRequest {
                        operation: path,
                        endpoint: url.clone(),
                        source,
                    });
                    continue;
                }
            };

            let status = resp.status();
            let resp_body = resp
                .text()
                .await
                .map_err(|source| AuthError::CrabAuthRequest {
                    operation: "read JSON response",
                    endpoint: url.clone(),
                    source,
                })?;

            if status.is_success() {
                return serde_json::from_str(&resp_body).map_err(|source| {
                    AuthError::ParseCrabAuthResponse {
                        operation: path,
                        endpoint: url.clone(),
                        source,
                    }
                });
            }

            let error = AuthError::CrabAuthRejected {
                operation: path,
                endpoint: url.clone(),
                status: status.as_u16(),
                body: resp_body,
            };

            if status.is_server_error() {
                last_err = Some(error);
                continue;
            }

            return Err(error);
        }

        Err(last_err.unwrap_or_else(|| AuthError::CrabAuthFailed {
            operation: path,
            endpoint: url,
            reason: "request failed after retries".into(),
        }))
    }

    fn endpoint_for(&self, path: &str) -> String {
        let base = self
            .endpoint
            .trim_end_matches('/')
            .strip_suffix("/v1/credentials")
            .unwrap_or_else(|| self.endpoint.trim_end_matches('/'));
        format!("{base}{path}")
    }

    fn cached_id_token(&self) -> Result<String> {
        let cached_tokens = self
            .token_cache
            .load(CRAB_AUTH_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;
        Ok(cached_tokens.id_token)
    }

    pub async fn prepare_push(
        &self,
        bucket: &str,
        prefix: &str,
        ref_updates: Vec<PushRefUpdate>,
    ) -> Result<ProtectedPushPrepare> {
        let repo_url = format!("crab://{bucket}/{prefix}");
        let id_token = self.cached_id_token()?;
        let body = PushPrepareRequest {
            id_token: id_token.clone(),
            repo_url: repo_url.clone(),
            ref_updates,
            client_version: self.client_version.clone(),
        };
        let response: PushPrepareResponse = match self
            .call_json_endpoint("/v1/push/prepare", &id_token, &body)
            .await
        {
            Ok(resp) => resp,
            Err(error) if should_refresh_after_auth_error(&error) => {
                let refreshed = self.refresh_id_token().await?;
                let retry_body = PushPrepareRequest {
                    id_token: refreshed,
                    ..body
                };
                self.call_json_endpoint("/v1/push/prepare", &retry_body.id_token, &retry_body)
                    .await?
            }
            Err(error) => return Err(error),
        };

        validate_push_prepare_response(prefix, &response)?;

        let expires_at = parse_iso8601(&response.expires_at).unwrap_or_else(|| {
            warn!(
                expires_at = %response.expires_at,
                "failed to parse crab-auth prepare expires_at, defaulting to 1 hour from now"
            );
            SystemTime::now() + Duration::from_secs(3600)
        });
        let credentials = response.cloud_credentials(expires_at)?;
        Ok(ProtectedPushPrepare {
            credentials,
            expires_at,
            push_id: response.push_id,
            upload_prefix: response.upload_prefix,
        })
    }

    pub async fn finalize_push(
        &self,
        bucket: &str,
        prefix: &str,
        ref_updates: Vec<PushRefUpdate>,
        push_id: &str,
        active_active_replication: Option<ActiveActiveReplicationConfig>,
        active_active_writer: Option<String>,
    ) -> Result<PushFinalizeResponse> {
        let repo_url = format!("crab://{bucket}/{prefix}");
        let id_token = self.cached_id_token()?;
        let active_active = match (active_active_replication, active_active_writer) {
            (Some(replication), Some(writer)) => Some(PushFinalizeActiveActive {
                replication,
                writer,
            }),
            (None, None) => None,
            _ => {
                return Err(AuthError::InvalidCrabAuthRequest(
                    "active-active finalize requires both replication config and writer name"
                        .into(),
                ));
            }
        };
        let body = PushFinalizeRequest {
            id_token: id_token.clone(),
            repo_url,
            ref_updates,
            push_id: push_id.to_owned(),
            client_version: self.client_version.clone(),
            active_active,
        };
        let response = match self
            .call_json_endpoint("/v1/push/finalize", &id_token, &body)
            .await
        {
            Ok(resp) => Ok(resp),
            Err(error) if should_refresh_after_auth_error(&error) => {
                let refreshed = self.refresh_id_token().await?;
                let retry_body = PushFinalizeRequest {
                    id_token: refreshed,
                    ..body
                };
                self.call_json_endpoint("/v1/push/finalize", &retry_body.id_token, &retry_body)
                    .await
            }
            Err(error) => Err(error),
        }?;
        validate_push_finalize_response(&response)?;
        Ok(response)
    }

    async fn refresh_id_token(&self) -> Result<String> {
        let cached_tokens = self
            .token_cache
            .load(CRAB_AUTH_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;

        let refresh_token = cached_tokens.refresh_token.as_deref().ok_or_else(|| {
            AuthError::CredentialsExpired("no refresh token available; run `crab login`".into())
        })?;

        let discovery = crate::oidc::discover(&self.issuer_url).await?;
        let new_tokens =
            crate::oidc::refresh_tokens(&discovery.token_endpoint, &self.client_id, refresh_token)
                .await?;

        self.token_cache.store(
            CRAB_AUTH_CACHE_KEY,
            &new_tokens.id_token,
            new_tokens.refresh_token.as_deref(),
        )?;

        Ok(new_tokens.id_token)
    }

    fn load_bound_login(&self) -> Result<CachedTokens> {
        let tokens = self
            .token_cache
            .load(CRAB_AUTH_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;
        let subject = TokenCache::oidc_identity(&tokens.id_token, &self.issuer_url)?.subject;
        // The issuer is a validated URL, so the zero separator cannot occur
        // in it. The digest lets a checkout child bind without receiving a token.
        let mut digest = Sha256::new();
        digest.update(b"Crab Auth CDN principal\0");
        digest.update(self.issuer_url.as_bytes());
        digest.update([0]);
        digest.update(subject.as_bytes());
        let principal = digest.finalize().into();
        if *self.cdn_principal.get_or_init(|| principal) != principal {
            return Err(bound_principal_changed());
        }
        Ok(tokens)
    }

    async fn refresh_bound_id_token(&self, rejected_token: &str) -> Result<String> {
        let current = self.load_bound_login()?;
        if current.id_token != rejected_token {
            return Ok(current.id_token);
        }
        let refresh_token = current.refresh_token.as_deref().ok_or_else(|| {
            AuthError::CredentialsExpired("no refresh token available; run `crab login`".into())
        })?;
        let discovery = crate::oidc::discover_with_client(&self.issuer_url, &self.http)
            .await
            .map_err(redact_bound_oidc_error)?;
        discovery.validate_for_issuer(&self.issuer_url)?;
        let replacement = crate::oidc::refresh_tokens_with_client(
            &discovery.token_endpoint,
            &self.client_id,
            refresh_token,
            &self.http,
        )
        .await
        .map_err(redact_bound_oidc_error)?;
        self.token_cache.store_refreshed_id_token(
            CRAB_AUTH_CACHE_KEY,
            &self.issuer_url,
            &current,
            &replacement,
        )?;
        Ok(replacement.id_token)
    }
}

#[async_trait]
impl CredentialProvider for CrabAuthProvider {
    type Error = AuthError;

    async fn resolve(
        &self,
        bucket: &str,
        prefix: &str,
        operation: &str,
    ) -> Result<CredentialResolution> {
        if operation.trim().eq_ignore_ascii_case("push") {
            return Err(AuthError::InvalidCrabAuthRequest(
                "push requires /v1/push/prepare and /v1/push/finalize".into(),
            ));
        }

        let mut deployments = self
            .cdn
            .iter()
            .filter(|deployment| deployment.bucket == bucket && deployment.source_repo == prefix);
        let bound_read = deployments.clone().next().is_some();
        let approval_pins = if self.allow_viewer_grants {
            let pins = deployments
                .clone()
                .filter_map(CdnViewerDeployment::approval_sha256)
                .collect::<std::collections::BTreeSet<_>>();
            if pins.len() > MAX_CDN_APPROVALS {
                return Err(AuthError::InvalidCrabAuthRequest(
                    "too many private CDN approvals for the selected repository".into(),
                ));
            }
            (!pins.is_empty()).then(|| pins.into_iter().collect::<Vec<_>>().join(","))
        } else {
            None
        };
        let retained_view = self.retained_view.get();
        let principal_bound = bound_read || operation == "mount" || retained_view.is_some();
        let login = principal_bound
            .then(|| self.load_bound_login())
            .transpose()?;

        if retained_view.is_none() {
            let guard = self.cached.read().await;
            if Self::is_cached_valid(guard.as_ref())
                && let Some(c) = guard.as_ref()
                && Self::cached_matches(c, bucket, prefix, operation)
            {
                return Ok(CredentialResolution {
                    credentials: c.creds.clone(),
                    storage_scope: c.storage_scope.clone(),
                    cdn: c.cdn.clone(),
                });
            }
        }

        let id_token = match login {
            Some(login) => login.id_token,
            None => self.cached_id_token()?,
        };
        let repo_url = format!("crab://{bucket}/{prefix}");
        let lease = retained_view
            .map(|view| view.lease_for(&repo_url, operation))
            .transpose()?;
        let proof = retained_view
            .zip(lease.as_ref())
            .map(|(view, lease)| view.proof(lease));
        let (auth_resp, approval) = match self
            .call_endpoint(
                &id_token,
                &repo_url,
                operation,
                principal_bound,
                approval_pins.as_deref(),
                proof,
            )
            .await
        {
            Ok(resp) => resp,
            Err(error) if should_refresh_after_auth_error(&error) => {
                debug!("crab-auth returned auth error, attempting ID token refresh");
                let refreshed = if principal_bound {
                    self.refresh_bound_id_token(&id_token).await?
                } else {
                    self.refresh_id_token().await?
                };
                self.call_endpoint(
                    &refreshed,
                    &repo_url,
                    operation,
                    principal_bound,
                    approval_pins.as_deref(),
                    proof,
                )
                .await?
            }
            Err(error) => return Err(error),
        };

        if proof.is_some_and(|proof| !proof.matches(auth_resp.storage_scope.as_ref())) {
            return Err(invalid_negotiated_response());
        }

        let expiry = parse_iso8601(&auth_resp.expires_at);
        if principal_bound && expiry.is_none() {
            return Err(invalid_negotiated_response());
        }
        if principal_bound && auth_resp.permissions.as_slice() != ["read"] {
            return Err(AuthError::InvalidCredentialResponse(
                "bound Crab Auth reads require a read-only authorization".into(),
            ));
        }
        let captured_at = SystemTime::now();
        if let Some(grant) = &auth_resp.cdn {
            if approval_pins.is_none() {
                return Err(invalid_negotiated_response());
            }
            let parent = expiry.ok_or_else(|| {
                AuthError::InvalidCredentialResponse(
                    "CDN viewer grant requires a valid parent expiry".into(),
                )
            })?;
            let deployment = deployments
                .find(|item| {
                    item.origin == grant.origin
                        && item.base_url == grant.base_url
                        && item.approval_sha256() == approval.as_deref()
                })
                .ok_or_else(|| {
                    AuthError::InvalidCredentialResponse(
                        "Crab Auth returned a CDN viewer grant for an unapproved deployment".into(),
                    )
                })?;
            grant.validate_binding(
                &deployment.origin,
                &deployment.base_url,
                prefix,
                auth_resp.storage_scope.as_ref(),
                parent,
                captured_at,
            )?;
        }
        let expires_at = expiry.unwrap_or_else(|| {
            warn!(
                expires_at = %auth_resp.expires_at,
                "failed to parse crab-auth expires_at, defaulting to 1 hour from now"
            );
            SystemTime::now() + Duration::from_secs(3600)
        });

        let creds = auth_resp.cloud_credentials(expires_at).map_err(|error| {
            if principal_bound {
                invalid_negotiated_response()
            } else {
                error
            }
        })?;
        let storage_scope = auth_resp.storage_scope;
        let cdn = auth_resp.cdn;

        let mut guard = self.cached.write().await;
        *guard = Some(CachedCrabAuthCreds {
            bucket: bucket.to_owned(),
            source_prefix: prefix.to_owned(),
            operation: normalize_operation(operation),
            creds: creds.clone(),
            storage_scope: storage_scope.clone(),
            expires_at,
            cdn: cdn.clone(),
            captured_at,
        });
        Ok(CredentialResolution {
            credentials: creds,
            storage_scope,
            cdn,
        })
    }

    fn needs_refresh(&self) -> bool {
        match self.cached.try_read() {
            Ok(guard) => !Self::is_cached_valid(guard.as_ref()),
            Err(_) => true,
        }
    }

    async fn refresh(&self) -> Result<CredentialResolution> {
        self.refresh_for("", "", "").await
    }

    async fn refresh_for(
        &self,
        bucket: &str,
        prefix: &str,
        operation: &str,
    ) -> Result<CredentialResolution> {
        let mut guard = self.cached.write().await;
        *guard = None;
        drop(guard);
        self.resolve(bucket, prefix, operation).await
    }

    fn identity(&self) -> Option<&str> {
        None
    }
}

fn should_refresh_after_auth_error(error: &AuthError) -> bool {
    match error {
        AuthError::CredentialsExpired(_) => true,
        AuthError::CrabAuthRejected { status, body, .. } => {
            *status == 401 || body.to_ascii_lowercase().contains("expired")
        }
        _ => false,
    }
}

fn invalid_negotiated_response() -> AuthError {
    AuthError::InvalidCredentialResponse("invalid negotiated credential response".into())
}

fn bound_principal_changed() -> AuthError {
    AuthError::CredentialsExpired("Crab Auth login changed; reopen the read operation".into())
}

fn redact_bound_oidc_error(error: AuthError) -> AuthError {
    let body = || "OIDC response omitted".to_owned();
    match error {
        AuthError::OidcRefreshExpired {
            endpoint, status, ..
        } => AuthError::OidcRefreshExpired {
            endpoint,
            status,
            body: body(),
        },
        AuthError::OidcRejected {
            operation,
            endpoint,
            status,
            ..
        } => AuthError::OidcRejected {
            operation,
            endpoint,
            status,
            body: body(),
        },
        AuthError::ParseOidcResponse { .. } => AuthError::InvalidCredentialResponse(
            "invalid OIDC response for a bound Crab Auth read".into(),
        ),
        other => other,
    }
}

fn normalize_operation(operation: &str) -> String {
    operation.trim().to_ascii_lowercase()
}

fn parse_iso8601(s: &str) -> Option<SystemTime> {
    let instant =
        time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339)
            .ok()?;
    let seconds = u64::try_from(instant.unix_timestamp()).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, instant.nanosecond()))
}

fn shellexpand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            return format!("{home}/{rest}");
        }
    }
    path.to_owned()
}

#[cfg(test)]
mod cdn_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CrabAuthClientConfig {
        CrabAuthClientConfig {
            endpoint: "https://auth.example.com/v1/credentials".into(),
            issuer_url: "https://idp.example.com".into(),
            client_id: "test-client".into(),
            token_cache_path: tempfile::NamedTempFile::new()
                .unwrap()
                .path()
                .to_string_lossy()
                .into_owned(),
            client_version: "test-version".into(),
        }
    }

    #[test]
    fn parse_iso8601_valid() {
        let ts = parse_iso8601("2026-04-24T18:00:00Z").unwrap();
        let epoch_secs = ts.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(epoch_secs, 1777053600);
        assert_eq!(
            parse_iso8601("2026-04-24T11:00:00.125-07:00"),
            ts.checked_add(Duration::from_millis(125)),
        );
    }

    #[test]
    fn parse_iso8601_invalid() {
        assert!(parse_iso8601("not-a-date").is_none());
        assert!(parse_iso8601("2026-04-24").is_none());
        assert!(parse_iso8601("").is_none());
        assert!(parse_iso8601("2026-04-99T18:00:00Z").is_none());
    }

    #[test]
    fn is_cached_valid_returns_false_for_none() {
        assert!(!CrabAuthProvider::is_cached_valid(None));
    }

    #[test]
    fn is_cached_valid_returns_false_within_refresh_window() {
        let cached = Some(CachedCrabAuthCreds {
            bucket: "bucket".into(),
            source_prefix: "repo".into(),
            operation: "fetch".into(),
            creds: CloudCredentials::Gcp {
                access_token: "ya29.test".into(),
                expires_at: SystemTime::now() + Duration::from_secs(240),
            },
            expires_at: SystemTime::now() + Duration::from_secs(240),
            storage_scope: None,
            cdn: None,
            captured_at: SystemTime::now(),
        });
        assert!(!CrabAuthProvider::is_cached_valid(cached.as_ref()));
    }

    #[test]
    fn is_cached_valid_returns_true_outside_refresh_window() {
        let cached = Some(CachedCrabAuthCreds {
            bucket: "bucket".into(),
            source_prefix: "repo".into(),
            operation: "fetch".into(),
            creds: CloudCredentials::Gcp {
                access_token: "ya29.test".into(),
                expires_at: SystemTime::now() + Duration::from_secs(1800),
            },
            expires_at: SystemTime::now() + Duration::from_secs(1800),
            storage_scope: None,
            cdn: None,
            captured_at: SystemTime::now(),
        });
        assert!(CrabAuthProvider::is_cached_valid(cached.as_ref()));
    }

    #[test]
    fn credentials_cache_accepts_a_fresh_short_lived_viewer() {
        let now = SystemTime::now();
        let expires_at = now + Duration::from_secs(1800);
        let viewer_expiry = (now + Duration::from_secs(14))
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cdn = serde_json::from_value(serde_json::json!({
            "provider": "cloud_cdn",
            "origin": "gs://bucket",
            "base_url": "https://downloads.example.com",
            "repo_prefix": "repo",
            "global_prefix": null,
            "expires_at": viewer_expiry,
            "grants": [{"prefix": "repo/v2/history/", "credential": "fixture"}],
        }))
        .unwrap();
        let mut cached = CachedCrabAuthCreds {
            bucket: "bucket".into(),
            source_prefix: "repo".into(),
            operation: "fetch".into(),
            creds: CloudCredentials::Gcp {
                access_token: "fixture".into(),
                expires_at,
            },
            expires_at,
            storage_scope: None,
            cdn: Some(cdn),
            captured_at: now,
        };
        assert!(CrabAuthProvider::is_cached_valid(Some(&cached)));
        cached.captured_at = now - Duration::from_secs(16);
        assert!(!CrabAuthProvider::is_cached_valid(Some(&cached)));
    }

    #[test]
    fn cached_credentials_match_only_same_repo_operation_and_unscoped_view() {
        let cached = CachedCrabAuthCreds {
            bucket: "bucket".into(),
            source_prefix: "repo".into(),
            operation: "fetch".into(),
            creds: CloudCredentials::Gcp {
                access_token: "ya29.test".into(),
                expires_at: SystemTime::now() + Duration::from_secs(1800),
            },
            expires_at: SystemTime::now() + Duration::from_secs(1800),
            storage_scope: None,
            cdn: None,
            captured_at: SystemTime::now(),
        };

        assert!(CrabAuthProvider::cached_matches(
            &cached, "bucket", "repo", " Fetch "
        ));
        assert!(!CrabAuthProvider::cached_matches(
            &cached,
            "other-bucket",
            "repo",
            "fetch"
        ));
        assert!(!CrabAuthProvider::cached_matches(
            &cached, "bucket", "other", "fetch"
        ));
        assert!(!CrabAuthProvider::cached_matches(
            &cached, "bucket", "repo", "clone"
        ));
    }

    #[test]
    fn cached_credentials_do_not_reuse_path_scoped_view_credentials() {
        let scope_hash = "b".repeat(64);
        let cached = CachedCrabAuthCreds {
            bucket: "bucket".into(),
            source_prefix: "repo".into(),
            operation: "fetch".into(),
            creds: CloudCredentials::Gcp {
                access_token: "ya29.test".into(),
                expires_at: SystemTime::now() + Duration::from_secs(1800),
            },
            expires_at: SystemTime::now() + Duration::from_secs(1800),
            storage_scope: Some(StorageScope {
                repo_prefix: format!("repo/acl-views/v1/{scope_hash}/7-deadbeef"),
                global_prefix: format!("repo/acl-views/v1/{scope_hash}/7-deadbeef/.crab"),
                source_repo: "repo".to_owned(),
                scope_hash,
            }),
            cdn: None,
            captured_at: SystemTime::now(),
        };

        assert!(!CrabAuthProvider::cached_matches(
            &cached, "bucket", "repo", "fetch"
        ));
    }

    #[test]
    fn create_crab_auth_provider_preserves_endpoint_and_caller_version() {
        let provider = create_crab_auth_provider(config()).unwrap();

        assert_eq!(provider.endpoint, "https://auth.example.com/v1/credentials");
        assert_eq!(provider.client_version, "test-version");
    }

    #[tokio::test]
    async fn negotiated_credential_errors_bound_and_redact_issuer_responses() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let invalid = br#"{"provider": ["fixture-secret"]}"#.to_vec();
        let denied = b"fixture-secret expired".to_vec();
        let oversized = vec![b' '; MAX_NEGOTIATED_CREDENTIAL_BYTES + 1];
        for (viewer_grants, status, body, expected, refresh) in [
            (
                true,
                "200 OK",
                invalid,
                "invalid negotiated credential response",
                false,
            ),
            (
                true,
                "401 Unauthorized",
                denied.clone(),
                "credential response omitted",
                true,
            ),
            (
                false,
                "403 Forbidden",
                denied,
                "credential response omitted",
                false,
            ),
            (true, "200 OK", oversized, "exceeds its byte limit", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/v1/credentials", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
                }
                assert_eq!(
                    request
                        .to_ascii_lowercase()
                        .contains("x-crab-cdn-grant-version: 1\r\n"),
                    viewer_grants
                );
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            });
            let mut provider = create_crab_auth_provider(config())
                .unwrap()
                .with_viewer_grants(viewer_grants);
            provider.endpoint = endpoint;
            provider.http = reqwest::Client::builder().no_proxy().build().unwrap();
            let error = provider
                .call_endpoint(
                    "fixture-token",
                    "crab://bucket/repo",
                    "fetch",
                    true,
                    viewer_grants.then_some(
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    ),
                    None,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected));
            assert!(!format!("{error:?}").contains("fixture-secret"));
            assert_eq!(should_refresh_after_auth_error(&error), refresh);
            tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn resolve_rejects_push_without_calling_legacy_credentials_endpoint() {
        let provider = create_crab_auth_provider(config()).unwrap();

        let err = provider
            .resolve("bucket", "repo", " Push ")
            .await
            .expect_err("push must not use /v1/credentials");

        assert!(matches!(err, AuthError::InvalidCrabAuthRequest(_)));
        assert!(
            err.to_string().contains("/v1/push/prepare")
                && err.to_string().contains("/v1/push/finalize"),
            "error must point clients to the protected push flow: {err}"
        );
    }

    #[test]
    fn crab_auth_policy_denial_does_not_trigger_token_refresh() {
        let error = AuthError::CrabAuthRejected {
            operation: "/v1/push/finalize",
            endpoint: "https://auth.example.com/v1/push/finalize".into(),
            status: 403,
            body: r#"{"detail":{"error":"forbidden","message":"explicitly denied"}}"#.into(),
        };

        assert!(!should_refresh_after_auth_error(&error));
    }

    #[test]
    fn crab_auth_unauthorized_still_triggers_token_refresh() {
        let error = AuthError::CrabAuthRejected {
            operation: "/v1/push/finalize",
            endpoint: "https://auth.example.com/v1/push/finalize".into(),
            status: 401,
            body: r#"{"detail":{"error":"invalid_token"}}"#.into(),
        };

        assert!(should_refresh_after_auth_error(&error));
    }
}
