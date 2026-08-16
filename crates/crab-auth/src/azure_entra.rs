//! Azure credential provider using Entra ID.
//!
//! Uses a cached OIDC ID token either directly as a bearer token for Azure
//! Blob Storage, or exchanges it via a Crab Auth endpoint for a SAS token or
//! scoped bearer token. No `azure-identity` SDK dependency is needed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::client_config::AzureEntraConfig;
use crate::credential_provider::CredentialProvider;
use crate::credentials::{AzureToken, CloudCredentials, CredentialResolution};
use crate::error::{AuthError, Result};
use crate::token_cache::TokenCache;

/// Refresh window: credentials within 5 minutes of expiry trigger a refresh.
const REFRESH_WINDOW: Duration = Duration::from_secs(300);

/// Default token lifetime when no explicit expiry is available.
const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);

/// Canonical token-cache key for Azure Entra.
const AZURE_ENTRA_CACHE_KEY: &str = "azure-entra";

/// Azure credential provider using Entra ID tokens.
pub struct AzureEntraProvider {
    tenant_id: String,
    token_cache: Arc<TokenCache>,
    auth_endpoint: Option<String>,
    storage_account: Option<String>,
    issuer_url: String,
    client_id: String,
    cached: RwLock<Option<CachedAzureCreds>>,
}

impl std::fmt::Debug for AzureEntraProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureEntraProvider")
            .field("tenant_id", &self.tenant_id)
            .field("auth_endpoint", &self.auth_endpoint)
            .field("storage_account", &self.storage_account)
            .finish_non_exhaustive()
    }
}

struct CachedAzureCreds {
    creds: CloudCredentials,
    expires_at: SystemTime,
}

#[derive(Deserialize)]
struct AzureAuthResponse {
    #[serde(default)]
    #[allow(dead_code)]
    token_type: Option<String>,
    #[serde(default)]
    sas_token: Option<String>,
    #[serde(default)]
    bearer_token: Option<String>,
    #[serde(default)]
    storage_account: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
}

impl AzureEntraProvider {
    /// Creates an Azure Entra provider from validated auth-domain config.
    pub fn new(config: AzureEntraConfig) -> Result<Self> {
        let cache_path = shellexpand_tilde(&config.token_cache_path);
        let token_cache = Arc::new(TokenCache::new(PathBuf::from(cache_path))?);

        Ok(Self {
            tenant_id: config.tenant_id,
            token_cache,
            auth_endpoint: config.auth_endpoint,
            storage_account: config.storage_account,
            issuer_url: config.issuer_url,
            client_id: config.client_id,
            cached: RwLock::new(None),
        })
    }

    fn is_cached_valid(cached: Option<&CachedAzureCreds>) -> bool {
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

    async fn exchange_via_endpoint(
        &self,
        endpoint: &str,
        id_token: &str,
    ) -> Result<(AzureToken, SystemTime, String)> {
        let body = serde_json::json!({
            "id_token": id_token,
            "provider": "azure",
        })
        .to_string();

        let client = reqwest::Client::new();
        let resp = client
            .post(endpoint)
            .header("Authorization", format!("Bearer {id_token}"))
            .header("Content-Type", "application/json")
            .body(body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|source| AuthError::AzureRequest {
                operation: "Crab Auth exchange",
                endpoint: endpoint.to_owned(),
                source,
            })?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|source| AuthError::AzureRequest {
                operation: "read Crab Auth exchange response",
                endpoint: endpoint.to_owned(),
                source,
            })?;

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AuthError::CredentialsExpired(format!(
                "Azure Crab Auth endpoint returned 401: {body}"
            )));
        }

        if !status.is_success() {
            return Err(classify_azure_error(&body, "Crab Auth"));
        }

        parse_crab_auth_response(&body)
    }

    fn use_id_token_as_bearer(id_token: &str) -> (AzureToken, SystemTime) {
        let expires_at = SystemTime::now() + DEFAULT_TOKEN_LIFETIME;
        (AzureToken::Bearer(id_token.to_owned()), expires_at)
    }

    async fn refresh_id_token(&self) -> Result<String> {
        let cached_tokens = self
            .token_cache
            .load(AZURE_ENTRA_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;

        let refresh_token = cached_tokens.refresh_token.as_deref().ok_or_else(|| {
            AuthError::CredentialsExpired("no refresh token available; run `crab login`".into())
        })?;

        let discovery = crate::oidc::discover(&self.issuer_url).await?;
        let new_tokens =
            crate::oidc::refresh_tokens(&discovery.token_endpoint, &self.client_id, refresh_token)
                .await?;

        self.token_cache.store(
            AZURE_ENTRA_CACHE_KEY,
            &new_tokens.id_token,
            new_tokens.refresh_token.as_deref(),
        )?;

        Ok(new_tokens.id_token)
    }
}

#[async_trait]
impl CredentialProvider for AzureEntraProvider {
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
            .load(AZURE_ENTRA_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;

        let id_token = cached_tokens.id_token.clone();

        let result = if let Some(ref endpoint) = self.auth_endpoint {
            match self.exchange_via_endpoint(endpoint, &id_token).await {
                Ok(pair) => Ok(pair),
                Err(AuthError::CredentialsExpired(_)) => {
                    debug!("Azure Crab Auth endpoint returned 401, attempting ID token refresh");
                    let refreshed = self.refresh_id_token().await?;
                    self.exchange_via_endpoint(endpoint, &refreshed).await
                }
                Err(error) => Err(error),
            }
        } else {
            let account = self
                .storage_account
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or(AuthError::AzureConfig {
                    key: "auth.azure.storage_account",
                    reason: "azure-entra direct bearer credentials require a storage_account",
                })?;
            let (token, expires_at) = Self::use_id_token_as_bearer(&id_token);
            Ok((token, expires_at, account.to_owned()))
        };

        let (token, expires_at, account) = result?;

        let creds = CloudCredentials::Azure {
            account,
            token,
            expires_at,
        };

        let mut guard = self.cached.write().await;
        *guard = Some(CachedAzureCreds {
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

fn parse_crab_auth_response(body: &str) -> Result<(AzureToken, SystemTime, String)> {
    let resp: AzureAuthResponse =
        serde_json::from_str(body).map_err(|source| AuthError::ParseAzureResponse {
            operation: "Crab Auth exchange",
            endpoint: "Crab Auth endpoint".into(),
            source,
        })?;

    let token = if let Some(sas) = resp.sas_token.filter(|s| !s.is_empty()) {
        AzureToken::Sas(sas)
    } else if let Some(bearer) = resp.bearer_token.filter(|s| !s.is_empty()) {
        AzureToken::Bearer(bearer)
    } else {
        return Err(AuthError::InvalidCredentialResponse(
            "Azure Crab Auth response contains neither sas_token nor bearer_token".into(),
        ));
    };

    let expires_at = resp
        .expires_at
        .as_deref()
        .and_then(parse_iso8601)
        .unwrap_or_else(|| {
            warn!(
                "Azure Crab Auth response missing or unparseable expires_at, defaulting to 1 hour"
            );
            SystemTime::now() + DEFAULT_TOKEN_LIFETIME
        });

    let account = resp
        .storage_account
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AuthError::InvalidCredentialResponse(
                "Azure Crab Auth response missing storage_account".into(),
            )
        })?;

    Ok((token, expires_at, account))
}

fn classify_azure_error(body: &str, context: &str) -> AuthError {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("error_description"))
                .and_then(|m| m.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned());

    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned));

    match code.as_deref() {
        Some("invalid_grant" | "interaction_required") => {
            AuthError::CredentialsExpired(format!("Azure {context}: {message}"))
        }
        Some("unauthorized_client" | "access_denied") => {
            AuthError::AzureRejected(format!("Azure {context} forbidden: {message}"))
        }
        _ => AuthError::AzureRejected(format!("Azure {context} failed: {message}")),
    }
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

    fn config() -> AzureEntraConfig {
        AzureEntraConfig {
            tenant_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            auth_endpoint: None,
            storage_account: None,
            issuer_url: "https://login.microsoftonline.com/tenant/v2.0".into(),
            client_id: "test-client".into(),
            token_cache_path: tempfile::NamedTempFile::new()
                .unwrap()
                .path()
                .to_string_lossy()
                .into_owned(),
        }
    }

    #[test]
    fn parse_crab_auth_response_sas_token() {
        let json = r#"{
            "token_type": "sas",
            "storage_account": "acct",
            "sas_token": "sv=2024-11-04&ss=b&srt=sco&sp=rl&se=2026-04-24T18:00:00Z",
            "expires_at": "2026-04-24T18:00:00Z"
        }"#;
        let (token, expires_at, account) = parse_crab_auth_response(json).unwrap();

        assert_eq!(account, "acct");
        assert!(matches!(token, AzureToken::Sas(ref s) if s.starts_with("sv=")));
        assert_eq!(
            expires_at
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1777053600
        );
    }

    #[test]
    fn parse_crab_auth_response_bearer_token() {
        let json = r#"{
            "token_type": "bearer",
            "storage_account": "acct",
            "bearer_token": "eyJhbGciOiJSUzI1NiIs.test.token",
            "expires_at": "2026-04-24T18:00:00Z"
        }"#;
        let (token, _, account) = parse_crab_auth_response(json).unwrap();

        assert_eq!(account, "acct");
        assert!(matches!(token, AzureToken::Bearer(ref s) if s.contains("test.token")));
    }

    #[test]
    fn parse_crab_auth_response_sas_takes_precedence() {
        let json = r#"{
            "storage_account": "acct",
            "sas_token": "sv=2024&ss=b",
            "bearer_token": "eyJ.test",
            "expires_at": "2026-04-24T18:00:00Z"
        }"#;
        let (token, _, _) = parse_crab_auth_response(json).unwrap();

        assert!(matches!(token, AzureToken::Sas(_)));
    }

    #[test]
    fn parse_crab_auth_response_missing_both_tokens() {
        let json = r#"{"expires_at": "2026-04-24T18:00:00Z"}"#;
        let err = parse_crab_auth_response(json).unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
    }

    #[test]
    fn parse_crab_auth_response_empty_tokens() {
        let json = r#"{"storage_account": "acct", "sas_token": "", "bearer_token": ""}"#;
        let err = parse_crab_auth_response(json).unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
    }

    #[test]
    fn parse_crab_auth_response_malformed_json() {
        let err = parse_crab_auth_response("not json").unwrap_err();

        assert!(matches!(err, AuthError::ParseAzureResponse { .. }));
    }

    #[test]
    fn parse_crab_auth_response_missing_expires_defaults_to_1h() {
        let json = r#"{"storage_account": "acct", "sas_token": "sv=2024&ss=b"}"#;
        let (token, expires_at, account) = parse_crab_auth_response(json).unwrap();

        assert_eq!(account, "acct");
        assert!(matches!(token, AzureToken::Sas(_)));
        let remaining = expires_at
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        assert!(remaining > Duration::from_secs(3500));
        assert!(remaining < Duration::from_secs(3700));
    }

    #[test]
    fn parse_crab_auth_response_requires_storage_account() {
        let json = r#"{"sas_token": "sv=2024&ss=b"}"#;
        let err = parse_crab_auth_response(json).unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
    }

    #[test]
    fn classify_azure_error_invalid_grant() {
        let body = r#"{"error":"invalid_grant","message":"Token expired"}"#;
        let err = classify_azure_error(body, "Crab Auth");

        assert!(matches!(err, AuthError::CredentialsExpired(_)));
    }

    #[test]
    fn classify_azure_error_access_denied() {
        let body = r#"{"error":"access_denied","message":"Not authorized"}"#;
        let err = classify_azure_error(body, "Crab Auth");

        assert!(matches!(err, AuthError::AzureRejected(message) if message.contains("forbidden")));
    }

    #[test]
    fn classify_azure_error_unauthorized_client() {
        let body = r#"{"error":"unauthorized_client","error_description":"Client not allowed"}"#;
        let err = classify_azure_error(body, "Crab Auth");

        assert!(matches!(err, AuthError::AzureRejected(message) if message.contains("forbidden")));
    }

    #[test]
    fn classify_azure_error_unknown() {
        let body = r#"{"error":"server_error","message":"Internal error"}"#;
        let err = classify_azure_error(body, "Crab Auth");

        assert!(matches!(err, AuthError::AzureRejected(_)));
    }

    #[test]
    fn classify_azure_error_malformed_body() {
        let err = classify_azure_error("not json", "Crab Auth");

        assert!(matches!(err, AuthError::AzureRejected(message) if message.contains("not json")));
    }

    #[test]
    fn parse_iso8601_valid() {
        let ts = parse_iso8601("2026-04-24T18:00:00Z").unwrap();
        let epoch_secs = ts.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

        assert_eq!(epoch_secs, 1777053600);
    }

    #[test]
    fn parse_iso8601_with_whitespace() {
        let ts = parse_iso8601("  2026-04-24T18:00:00Z  ").unwrap();
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
        assert!(!AzureEntraProvider::is_cached_valid(None));
    }

    #[test]
    fn is_cached_valid_returns_false_for_expired() {
        let cached = Some(CachedAzureCreds {
            creds: CloudCredentials::Azure {
                account: "acct".into(),
                token: AzureToken::Bearer("test".into()),
                expires_at: SystemTime::now() - Duration::from_secs(60),
            },
            expires_at: SystemTime::now() - Duration::from_secs(60),
        });

        assert!(!AzureEntraProvider::is_cached_valid(cached.as_ref()));
    }

    #[test]
    fn is_cached_valid_returns_false_within_refresh_window() {
        let cached = Some(CachedAzureCreds {
            creds: CloudCredentials::Azure {
                account: "acct".into(),
                token: AzureToken::Bearer("test".into()),
                expires_at: SystemTime::now() + Duration::from_secs(240),
            },
            expires_at: SystemTime::now() + Duration::from_secs(240),
        });

        assert!(!AzureEntraProvider::is_cached_valid(cached.as_ref()));
    }

    #[test]
    fn is_cached_valid_returns_true_outside_refresh_window() {
        let cached = Some(CachedAzureCreds {
            creds: CloudCredentials::Azure {
                account: "acct".into(),
                token: AzureToken::Bearer("test".into()),
                expires_at: SystemTime::now() + Duration::from_secs(1800),
            },
            expires_at: SystemTime::now() + Duration::from_secs(1800),
        });

        assert!(AzureEntraProvider::is_cached_valid(cached.as_ref()));
    }

    #[test]
    fn new_captures_no_auth_endpoint() {
        let provider = AzureEntraProvider::new(config()).unwrap();

        assert!(provider.auth_endpoint.is_none());
    }

    #[test]
    fn new_captures_auth_endpoint() {
        let mut cfg = config();
        cfg.auth_endpoint = Some("https://crab-auth.corp.example.com/v1/azure".into());

        let provider = AzureEntraProvider::new(cfg).unwrap();

        assert_eq!(
            provider.auth_endpoint.as_deref(),
            Some("https://crab-auth.corp.example.com/v1/azure")
        );
    }

    #[test]
    fn use_id_token_as_bearer_returns_bearer_variant() {
        let (token, expires_at) = AzureEntraProvider::use_id_token_as_bearer("eyJ.test.jwt");

        assert!(matches!(token, AzureToken::Bearer(ref s) if s == "eyJ.test.jwt"));
        let remaining = expires_at
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        assert!(remaining > Duration::from_secs(3500));
    }
}
