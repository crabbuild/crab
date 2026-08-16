//! Crab Auth enterprise service client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use crab_coordination::active_active::ActiveActiveReplicationConfig;
use crab_types::storage::StorageScope;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::client_config::CrabAuthClientConfig;
use crate::credential_provider::CredentialProvider;
use crate::credential_response::{CrabAuthCredentialResponse, parse_credential_response};
use crate::credentials::{CloudCredentials, CredentialResolution};
use crate::error::{AuthError, Result};
use crate::protected_push::{
    PushFinalizeResponse, PushPrepareResponse, PushRefUpdate, validate_push_finalize_response,
    validate_push_prepare_response,
};
use crate::token_cache::TokenCache;

/// Refresh window: credentials within 5 minutes of expiry trigger a refresh.
const REFRESH_WINDOW: Duration = Duration::from_secs(300);

/// Default retry count for HTTP 5xx errors.
const MAX_RETRIES: u32 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Canonical token-cache key for Crab Auth.
const CRAB_AUTH_CACHE_KEY: &str = "crab-auth";

/// Request body sent to the Crab Auth endpoint.
#[derive(Serialize)]
struct CrabAuthRequest {
    id_token: String,
    repo_url: String,
    operation: String,
    client_version: String,
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
}

impl CrabAuthProvider {
    /// Creates a Crab Auth provider from validated auth-domain config.
    pub fn new(config: CrabAuthClientConfig) -> Result<Self> {
        let cache_path = shellexpand_tilde(&config.token_cache_path);
        let token_cache = Arc::new(TokenCache::new(PathBuf::from(cache_path))?);

        Ok(Self {
            endpoint: config.endpoint,
            issuer_url: config.issuer_url,
            client_id: config.client_id,
            client_version: config.client_version,
            token_cache,
            cached: RwLock::new(None),
            http: reqwest::Client::new(),
        })
    }

    /// Checks if cached credentials are still valid.
    fn is_cached_valid(cached: Option<&CachedCrabAuthCreds>) -> bool {
        match cached {
            Some(c) => {
                let now = SystemTime::now();
                match c.expires_at.duration_since(now) {
                    Ok(remaining) => remaining > REFRESH_WINDOW,
                    Err(_) => false,
                }
            }
            None => false,
        }
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
    ) -> Result<CrabAuthCredentialResponse> {
        let body = CrabAuthRequest {
            id_token: id_token.to_owned(),
            repo_url: repo_url.to_owned(),
            operation: operation.to_owned(),
            client_version: self.client_version.clone(),
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

            let result = self
                .http
                .post(&self.endpoint)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {id_token}"))
                .json(&body)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await;

            let resp = match result {
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
            let resp_body = resp
                .text()
                .await
                .map_err(|source| AuthError::CrabAuthRequest {
                    operation: "read credentials response",
                    endpoint: self.endpoint.clone(),
                    source,
                })?;

            if status.is_success() {
                return parse_credential_response(&resp_body);
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

        {
            let guard = self.cached.read().await;
            if Self::is_cached_valid(guard.as_ref())
                && let Some(c) = guard.as_ref()
                && Self::cached_matches(c, bucket, prefix, operation)
            {
                return Ok(CredentialResolution {
                    credentials: c.creds.clone(),
                    storage_scope: c.storage_scope.clone(),
                });
            }
        }

        let cached_tokens = self
            .token_cache
            .load(CRAB_AUTH_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;

        let id_token = cached_tokens.id_token.clone();
        let repo_url = format!("crab://{bucket}/{prefix}");

        let auth_resp = match self.call_endpoint(&id_token, &repo_url, operation).await {
            Ok(resp) => resp,
            Err(error) if should_refresh_after_auth_error(&error) => {
                debug!("crab-auth returned auth error, attempting ID token refresh");
                let refreshed = self.refresh_id_token().await?;
                self.call_endpoint(&refreshed, &repo_url, operation).await?
            }
            Err(error) => return Err(error),
        };

        let expires_at = parse_iso8601(&auth_resp.expires_at).unwrap_or_else(|| {
            warn!(
                expires_at = %auth_resp.expires_at,
                "failed to parse crab-auth expires_at, defaulting to 1 hour from now"
            );
            SystemTime::now() + Duration::from_secs(3600)
        });

        let creds = auth_resp.cloud_credentials(expires_at)?;
        let storage_scope = auth_resp.storage_scope;

        let mut guard = self.cached.write().await;
        *guard = Some(CachedCrabAuthCreds {
            bucket: bucket.to_owned(),
            source_prefix: prefix.to_owned(),
            operation: normalize_operation(operation),
            creds: creds.clone(),
            storage_scope: storage_scope.clone(),
            expires_at,
        });
        Ok(CredentialResolution {
            credentials: creds,
            storage_scope,
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

fn normalize_operation(operation: &str) -> String {
    operation.trim().to_ascii_lowercase()
}

fn parse_iso8601(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let date_parts: Vec<&str> = date.split('-').collect();
    let time_parts: Vec<&str> = time.split(':').collect();

    if date_parts.len() != 3 || time_parts.len() != 3 {
        return None;
    }

    let year: i64 = date_parts[0].parse().ok()?;
    let month: i64 = date_parts[1].parse().ok()?;
    let day: i64 = date_parts[2].parse().ok()?;
    let hour: i64 = time_parts[0].parse().ok()?;
    let min: i64 = time_parts[1].parse().ok()?;
    let sec: i64 = time_parts[2].parse().ok()?;

    let days = days_from_civil(year, month, day);
    let unix_secs = days * 86400 + hour * 3600 + min * 60 + sec;

    if unix_secs < 0 {
        return None;
    }

    Some(std::time::UNIX_EPOCH + Duration::from_secs(unix_secs as u64))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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
    }

    #[test]
    fn parse_iso8601_invalid() {
        assert!(parse_iso8601("not-a-date").is_none());
        assert!(parse_iso8601("2026-04-24").is_none());
        assert!(parse_iso8601("").is_none());
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
        });
        assert!(CrabAuthProvider::is_cached_valid(cached.as_ref()));
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
