//! GCP credential provider using OIDC to Workload Identity Federation.
//!
//! Exchanges a cached OIDC ID token for a federated access token via
//! `sts.googleapis.com`, then impersonates a service account via
//! `iamcredentials.googleapis.com` to obtain a short-lived OAuth2 token
//! for GCS operations. No `google-cloud-sdk` dependency is needed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::client_config::GcpWorkloadIdentityConfig;
use crate::credential_provider::CredentialProvider;
use crate::credentials::{CloudCredentials, CredentialResolution};
use crate::error::{AuthError, Result};
use crate::token_cache::TokenCache;

/// Refresh window: credentials within 5 minutes of expiry trigger a refresh.
const REFRESH_WINDOW: Duration = Duration::from_secs(300);

/// Canonical token-cache key for GCP Workload Identity Federation.
const GCP_WORKLOAD_IDENTITY_CACHE_KEY: &str = "gcp-workload-identity";

/// GCP credential provider using OIDC to Workload Identity Federation.
pub struct GcpFederationProvider {
    workload_identity_pool: String,
    service_account: String,
    audience: String,
    issuer_url: String,
    client_id: String,
    token_cache: Arc<TokenCache>,
    cached: RwLock<Option<CachedGcpCreds>>,
}

impl std::fmt::Debug for GcpFederationProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcpFederationProvider")
            .field("workload_identity_pool", &self.workload_identity_pool)
            .field("service_account", &self.service_account)
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

struct CachedGcpCreds {
    creds: CloudCredentials,
    expires_at: SystemTime,
}

#[derive(Deserialize)]
struct StsTokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: String,
    /// Lifetime in seconds.
    #[allow(dead_code)]
    expires_in: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImpersonateResponse {
    access_token: String,
    expire_time: String,
}

impl GcpFederationProvider {
    /// Creates a GCP WIF provider from validated auth-domain config.
    pub fn new(config: GcpWorkloadIdentityConfig) -> Result<Self> {
        let audience = derive_audience(&config.workload_identity_pool);
        let cache_path = shellexpand_tilde(&config.token_cache_path);
        let token_cache = Arc::new(TokenCache::new(PathBuf::from(cache_path))?);

        Ok(Self {
            workload_identity_pool: config.workload_identity_pool,
            service_account: config.service_account,
            audience,
            issuer_url: config.issuer_url,
            client_id: config.client_id,
            token_cache,
            cached: RwLock::new(None),
        })
    }

    fn is_cached_valid(cached: Option<&CachedGcpCreds>) -> bool {
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

    async fn exchange_token(&self, id_token: &str) -> Result<StsTokenResponse> {
        let endpoint = "https://sts.googleapis.com/v1/token";
        let client = reqwest::Client::new();
        let resp = client
            .post(endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "grant_type={}&subject_token={}&subject_token_type={}&audience={}&requested_token_type={}",
                urlencoded("urn:ietf:params:oauth:grant-type:token-exchange"),
                urlencoded(id_token),
                urlencoded("urn:ietf:params:oauth:token-type:jwt"),
                urlencoded(&self.audience),
                urlencoded("urn:ietf:params:oauth:token-type:access_token"),
            ))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|source| AuthError::GcpRequest {
                operation: "STS token exchange",
                endpoint: endpoint.to_owned(),
                source,
            })?;

        let status = resp.status();
        let body = resp.text().await.map_err(|source| AuthError::GcpRequest {
            operation: "read STS token exchange response",
            endpoint: endpoint.to_owned(),
            source,
        })?;

        if !status.is_success() {
            return Err(classify_gcp_error(&body, "STS token exchange"));
        }

        serde_json::from_str(&body).map_err(|source| AuthError::ParseGcpResponse {
            operation: "STS token exchange",
            endpoint: endpoint.to_owned(),
            source,
        })
    }

    async fn impersonate_service_account(
        &self,
        federated_token: &str,
    ) -> Result<ImpersonateResponse> {
        let endpoint = format!(
            "https://iamcredentials.googleapis.com/v1/serviceAccounts/{}:generateAccessToken",
            urlencoded(&self.service_account),
        );

        let client = reqwest::Client::new();
        let resp = client
            .post(&endpoint)
            .header("Authorization", format!("Bearer {federated_token}"))
            .header("Content-Type", "application/json")
            .body(r#"{"scope":["https://www.googleapis.com/auth/cloud-platform"]}"#)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|source| AuthError::GcpRequest {
                operation: "service account impersonation",
                endpoint: endpoint.clone(),
                source,
            })?;

        let status = resp.status();
        let body = resp.text().await.map_err(|source| AuthError::GcpRequest {
            operation: "read service account impersonation response",
            endpoint: endpoint.clone(),
            source,
        })?;

        if !status.is_success() {
            return Err(classify_gcp_error(&body, "service account impersonation"));
        }

        serde_json::from_str(&body).map_err(|source| AuthError::ParseGcpResponse {
            operation: "service account impersonation",
            endpoint,
            source,
        })
    }

    async fn refresh_id_token(&self) -> Result<String> {
        let cached_tokens = self
            .token_cache
            .load(GCP_WORKLOAD_IDENTITY_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;

        let refresh_token = cached_tokens.refresh_token.as_deref().ok_or_else(|| {
            AuthError::CredentialsExpired("no refresh token available; run `crab login`".into())
        })?;

        let discovery = crate::oidc::discover(&self.issuer_url).await?;
        let new_tokens =
            crate::oidc::refresh_tokens(&discovery.token_endpoint, &self.client_id, refresh_token)
                .await?;

        self.token_cache.store(
            GCP_WORKLOAD_IDENTITY_CACHE_KEY,
            &new_tokens.id_token,
            new_tokens.refresh_token.as_deref(),
        )?;

        Ok(new_tokens.id_token)
    }
}

#[async_trait]
impl CredentialProvider for GcpFederationProvider {
    type Error = AuthError;

    async fn resolve(
        &self,
        _bucket: &str,
        _prefix: &str,
        _operation: &str,
    ) -> Result<CredentialResolution> {
        {
            let guard = self.cached.read().await;
            if let Some(c) = guard.as_ref()
                && Self::is_cached_valid(Some(c))
            {
                return Ok(CredentialResolution::new(c.creds.clone()));
            }
        }

        let cached_tokens = self
            .token_cache
            .load(GCP_WORKLOAD_IDENTITY_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;

        let id_token = cached_tokens.id_token.clone();

        let sts_resp = match self.exchange_token(&id_token).await {
            Ok(resp) => resp,
            Err(AuthError::CredentialsExpired(_)) => {
                debug!("GCP STS returned expired token error, attempting ID token refresh");
                let refreshed = self.refresh_id_token().await?;
                self.exchange_token(&refreshed).await?
            }
            Err(error) => return Err(error),
        };

        let impersonate_resp = self
            .impersonate_service_account(&sts_resp.access_token)
            .await?;

        let expires_at = parse_rfc3339(&impersonate_resp.expire_time).unwrap_or_else(|| {
            warn!(
                expire_time = %impersonate_resp.expire_time,
                "failed to parse GCP expireTime, defaulting to 1 hour from now"
            );
            SystemTime::now() + Duration::from_secs(3600)
        });

        let creds = CloudCredentials::Gcp {
            access_token: impersonate_resp.access_token,
            expires_at,
        };

        let mut guard = self.cached.write().await;
        *guard = Some(CachedGcpCreds {
            creds: creds.clone(),
            expires_at,
        });
        Ok(CredentialResolution::new(creds))
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

fn derive_audience(workload_identity_pool: &str) -> String {
    if workload_identity_pool.starts_with("//iam.googleapis.com/") {
        workload_identity_pool.to_owned()
    } else {
        format!("//iam.googleapis.com/{workload_identity_pool}")
    }
}

fn classify_gcp_error(body: &str, context: &'static str) -> AuthError {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned());

    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("code"))
                .and_then(serde_json::Value::as_u64)
        });

    match code {
        Some(401) => AuthError::CredentialsExpired(format!("GCP {context}: {message}")),
        Some(403) => AuthError::GcpRejected(format!("GCP {context} forbidden: {message}")),
        _ => AuthError::GcpRejected(format!("GCP {context} failed: {message}")),
    }
}

fn parse_rfc3339(s: &str) -> Option<SystemTime> {
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

fn urlencoded(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
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

    fn config(workload_identity_pool: &str) -> GcpWorkloadIdentityConfig {
        GcpWorkloadIdentityConfig {
            workload_identity_pool: workload_identity_pool.into(),
            service_account: "sa@project.iam.gserviceaccount.com".into(),
            issuer_url: "https://idp.example.com".into(),
            client_id: "test-client".into(),
            token_cache_path: tempfile::NamedTempFile::new()
                .unwrap()
                .path()
                .to_string_lossy()
                .into_owned(),
        }
    }

    #[test]
    fn audience_from_pool_resource_name() {
        let pool =
            "projects/123456/locations/global/workloadIdentityPools/my-pool/providers/my-provider";
        let audience = derive_audience(pool);

        assert_eq!(
            audience,
            "//iam.googleapis.com/projects/123456/locations/global/workloadIdentityPools/my-pool/providers/my-provider"
        );
    }

    #[test]
    fn audience_already_prefixed() {
        let pool = "//iam.googleapis.com/projects/123456/locations/global/workloadIdentityPools/my-pool/providers/my-provider";
        let audience = derive_audience(pool);

        assert_eq!(audience, pool);
    }

    #[test]
    fn new_derives_audience_from_config() {
        let provider = GcpFederationProvider::new(config(
            "projects/123/locations/global/workloadIdentityPools/pool/providers/prov",
        ))
        .unwrap();

        assert_eq!(
            provider.audience,
            "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/pool/providers/prov"
        );
    }

    #[test]
    fn parse_sts_token_response() {
        let json = r#"{
            "access_token": "ya29.federated_token_abc",
            "token_type": "Bearer",
            "expires_in": 3600
        }"#;
        let resp: StsTokenResponse = serde_json::from_str(json).unwrap();

        assert_eq!(resp.access_token, "ya29.federated_token_abc");
        assert_eq!(resp.expires_in, 3600);
    }

    #[test]
    fn parse_sts_token_response_missing_field() {
        let json = r#"{ "token_type": "Bearer" }"#;
        let result = serde_json::from_str::<StsTokenResponse>(json);

        assert!(result.is_err());
    }

    #[test]
    fn parse_impersonate_response() {
        let json = r#"{
            "accessToken": "ya29.impersonated_token_xyz",
            "expireTime": "2026-04-24T18:00:00Z"
        }"#;
        let resp: ImpersonateResponse = serde_json::from_str(json).unwrap();

        assert_eq!(resp.access_token, "ya29.impersonated_token_xyz");
        assert_eq!(resp.expire_time, "2026-04-24T18:00:00Z");
    }

    #[test]
    fn parse_impersonate_response_missing_field() {
        let json = r#"{ "accessToken": "ya29.abc" }"#;
        let result = serde_json::from_str::<ImpersonateResponse>(json);

        assert!(result.is_err());
    }

    #[test]
    fn classify_gcp_error_401() {
        let body = r#"{"error":{"code":401,"message":"Token expired"}}"#;
        let err = classify_gcp_error(body, "STS token exchange");

        assert!(matches!(err, AuthError::CredentialsExpired(_)));
    }

    #[test]
    fn classify_gcp_error_403() {
        let body = r#"{"error":{"code":403,"message":"Permission denied"}}"#;
        let err = classify_gcp_error(body, "service account impersonation");

        assert!(matches!(err, AuthError::GcpRejected(message) if message.contains("forbidden")));
    }

    #[test]
    fn classify_gcp_error_unknown_code() {
        let body = r#"{"error":{"code":500,"message":"Internal error"}}"#;
        let err = classify_gcp_error(body, "STS token exchange");

        assert!(matches!(err, AuthError::GcpRejected(_)));
    }

    #[test]
    fn classify_gcp_error_malformed_body() {
        let err = classify_gcp_error("not json", "STS token exchange");

        assert!(matches!(err, AuthError::GcpRejected(message) if message.contains("not json")));
    }

    #[test]
    fn parse_rfc3339_valid() {
        let ts = parse_rfc3339("2026-04-24T18:00:00Z").unwrap();
        let epoch_secs = ts.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

        assert_eq!(epoch_secs, 1777053600);
    }

    #[test]
    fn parse_rfc3339_with_whitespace() {
        let ts = parse_rfc3339("  2026-04-24T18:00:00Z  ").unwrap();
        let epoch_secs = ts.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

        assert_eq!(epoch_secs, 1777053600);
    }

    #[test]
    fn parse_rfc3339_invalid() {
        assert!(parse_rfc3339("not-a-date").is_none());
        assert!(parse_rfc3339("2026-04-24").is_none());
        assert!(parse_rfc3339("").is_none());
    }

    #[test]
    fn is_cached_valid_returns_false_for_none() {
        assert!(!GcpFederationProvider::is_cached_valid(None));
    }

    #[test]
    fn is_cached_valid_returns_false_for_expired() {
        let cached = Some(CachedGcpCreds {
            creds: CloudCredentials::Gcp {
                access_token: "ya29.test".into(),
                expires_at: SystemTime::now() - Duration::from_secs(60),
            },
            expires_at: SystemTime::now() - Duration::from_secs(60),
        });

        assert!(!GcpFederationProvider::is_cached_valid(cached.as_ref()));
    }

    #[test]
    fn is_cached_valid_returns_false_within_refresh_window() {
        let cached = Some(CachedGcpCreds {
            creds: CloudCredentials::Gcp {
                access_token: "ya29.test".into(),
                expires_at: SystemTime::now() + Duration::from_secs(240),
            },
            expires_at: SystemTime::now() + Duration::from_secs(240),
        });

        assert!(!GcpFederationProvider::is_cached_valid(cached.as_ref()));
    }

    #[test]
    fn is_cached_valid_returns_true_outside_refresh_window() {
        let cached = Some(CachedGcpCreds {
            creds: CloudCredentials::Gcp {
                access_token: "ya29.test".into(),
                expires_at: SystemTime::now() + Duration::from_secs(1800),
            },
            expires_at: SystemTime::now() + Duration::from_secs(1800),
        });

        assert!(GcpFederationProvider::is_cached_valid(cached.as_ref()));
    }
}
