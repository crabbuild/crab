//! Authentication and repository authorization for the Git LFS gateway.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{Extensions, HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use scrypt::{
    Scrypt,
    password_hash::{PasswordHash, PasswordVerifier},
};
use serde::Deserialize;
use tracing::debug;

use crate::error::{LfsServerError, Result};

const POLICY_ACTIONS: &[&str] = &["read", "write", "admin"];
const MTLS_CN_HEADER: &str = "x-client-cn";
const DUMMY_BASIC_PASSWORD_HASH: &str =
    "$scrypt$ln=16,r=8,p=1$aM15713r3Xsvxbi31lqr1Q$nFNh2CVHVjNldFVKDHDlm4CbdRSCdEBsjjJxD+iCs5E";
const MAX_SCRYPT_MEMORY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SCRYPT_PARALLELISM: u32 = 4;
pub(crate) const MAX_AUTH_CONCURRENCY: usize = 4;

/// Authentication method selected by the gateway configuration.
#[derive(Clone)]
pub enum AuthConfig {
    /// Do not require a credential. The principal is anonymous.
    None,
    /// HTTP Basic authentication backed by bounded scrypt PHC password hashes.
    Basic { users: HashMap<String, String> },
    /// Bearer authentication backed by BLAKE3 token hashes.
    Bearer { users: HashMap<String, [u8; 32]> },
    /// mTLS identity from the native TLS acceptor or an explicitly trusted proxy header.
    Mtls,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("AuthConfig");
        match self {
            Self::None => debug.field("mechanism", &"none"),
            Self::Basic { users } => debug
                .field("mechanism", &"basic")
                .field("user_count", &users.len()),
            Self::Bearer { users } => debug
                .field("mechanism", &"bearer")
                .field("principal_count", &users.len()),
            Self::Mtls => debug.field("mechanism", &"mtls"),
        }
        .finish()
    }
}

/// Identity established by the authentication middleware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// Principal used for policy evaluation.
    pub principal: String,
}

/// Identity inserted by the native mTLS acceptor after certificate validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsClientIdentity {
    /// Stable certificate-derived principal.
    pub principal: String,
}

/// Repository authorization policy loaded from YAML.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthPolicy {
    /// Rules evaluated with OR semantics.
    pub rules: Vec<PolicyRule>,
}

/// One principal's repository/action grant.
#[derive(Debug, Clone, Deserialize)]
pub struct PolicyRule {
    /// Authentication principal.
    pub principal: String,
    /// Exact repository names or a single trailing star prefix wildcard.
    pub repos: Vec<String>,
    /// Allowed operations: read, write, and admin.
    pub actions: Vec<String>,
}

impl AuthPolicy {
    /// Loads and validates a YAML policy file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(|source| {
            LfsServerError::Config(format!(
                "failed to read policy file {}: {source}",
                path.display()
            ))
        })?;
        let policy: Self = serde_yaml::from_str(&contents).map_err(|source| {
            LfsServerError::Config(format!(
                "invalid policy YAML in {}: {source}",
                path.display()
            ))
        })?;
        policy.validate(path)?;
        Ok(policy)
    }

    /// Returns whether a principal may perform an operation on a repository.
    #[must_use]
    pub fn is_authorized(&self, principal: &str, repository: &str, action: &str) -> bool {
        self.rules.iter().any(|rule| {
            rule.principal == principal
                && rule.actions.iter().any(|candidate| candidate == action)
                && rule
                    .repos
                    .iter()
                    .any(|pattern| repo_matches(pattern, repository))
        })
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if self.rules.is_empty() {
            return Err(policy_error(path, "rules must contain at least one rule"));
        }
        for (index, rule) in self.rules.iter().enumerate() {
            let field = format!("rules[{index}]");
            validate_text(path, &format!("{field}.principal"), &rule.principal)?;
            if rule.repos.is_empty() {
                return Err(policy_error(
                    path,
                    &format!("{field}.repos must contain at least one pattern"),
                ));
            }
            for (repo_index, pattern) in rule.repos.iter().enumerate() {
                validate_repo_pattern(path, &format!("{field}.repos[{repo_index}]"), pattern)?;
            }
            if rule.actions.is_empty() {
                return Err(policy_error(
                    path,
                    &format!("{field}.actions must contain at least one action"),
                ));
            }
            for (action_index, action) in rule.actions.iter().enumerate() {
                let action_field = format!("{field}.actions[{action_index}]");
                let action = validate_text(path, &action_field, action)?;
                if !POLICY_ACTIONS.contains(&action) {
                    return Err(policy_error(
                        path,
                        &format!(
                            "{action_field} has unknown action {action:?}; expected read, write, or admin"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Authenticates an LFS request and stores the identity in request extensions.
pub async fn auth_middleware(
    State(state): State<Arc<crate::http::AppState>>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    if matches!(request.uri().path(), "/healthz" | "/readyz" | "/metrics") {
        return next.run(request).await;
    }
    if let Some(retry_after) = state.request_limiter.retry_after() {
        state.metrics.rate_limited();
        return request_rate_limited(retry_after);
    }
    let signed_action_candidate = state.config.action_secret.is_some()
        && crate::http::is_signed_action_candidate(request.method(), request.uri());
    match authenticate_identity(
        &state.config.auth,
        &headers,
        request.extensions(),
        state.native_mtls(),
        state.config.trust_proxy_mtls,
        &state.auth_permits,
    )
    .await
    {
        Ok(identity) => {
            debug!(principal = %identity.principal, "authenticated LFS request");
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        Err(_reason) if signed_action_candidate => {
            debug!("using signed LFS action capability");
            request.extensions_mut().insert(ClientIdentity {
                principal: "signed-action".to_owned(),
            });
            next.run(request).await
        }
        Err(reason) => unauthorized(&state.config.auth, reason),
    }
}

async fn authenticate_identity(
    config: &AuthConfig,
    headers: &HeaderMap,
    extensions: &Extensions,
    native_mtls: bool,
    trusted_proxy: bool,
    auth_permits: &Arc<tokio::sync::Semaphore>,
) -> std::result::Result<ClientIdentity, String> {
    let AuthConfig::Basic { users } = config else {
        return extract_identity(config, headers, extensions, native_mtls, trusted_proxy);
    };
    let (principal, password) = basic_credentials(headers)?;
    let expected = users
        .get(&principal)
        .map(String::as_str)
        .unwrap_or(DUMMY_BASIC_PASSWORD_HASH)
        .to_owned();
    let permit = auth_permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "authentication service is shutting down".to_owned())?;
    let verified = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        verify_password(&expected, password.as_bytes())
    })
    .await
    .map_err(|_| "password verification task failed".to_owned())?;
    if !verified || !users.contains_key(&principal) {
        return Err("invalid Basic credentials".to_owned());
    }
    Ok(ClientIdentity { principal })
}

/// Extracts an identity from the configured HTTP/TLS credential.
pub(crate) fn extract_identity(
    config: &AuthConfig,
    headers: &HeaderMap,
    extensions: &Extensions,
    native_mtls: bool,
    trusted_proxy: bool,
) -> std::result::Result<ClientIdentity, String> {
    match config {
        AuthConfig::None => Ok(ClientIdentity {
            principal: "anonymous".to_owned(),
        }),
        AuthConfig::Basic { users } => {
            let (principal, password) = basic_credentials(headers)?;
            let expected = users
                .get(&principal)
                .map(String::as_str)
                .unwrap_or(DUMMY_BASIC_PASSWORD_HASH);
            if !verify_password(expected, password.as_bytes()) || !users.contains_key(&principal) {
                return Err("invalid Basic credentials".to_owned());
            }
            Ok(ClientIdentity { principal })
        }
        AuthConfig::Bearer { users } => {
            let token = authorization_value(headers, "Bearer")?;
            let token_hash = blake3::hash(token.as_bytes());
            users
                .iter()
                .find(|(_, expected)| constant_time_eq(token_hash.as_bytes(), *expected))
                .map(|(principal, _)| ClientIdentity {
                    principal: principal.clone(),
                })
                .ok_or_else(|| "invalid bearer token".to_owned())
        }
        AuthConfig::Mtls => {
            if native_mtls {
                let identity = extensions
                    .get::<TlsClientIdentity>()
                    .ok_or_else(|| "missing verified client certificate identity".to_owned())?;
                return Ok(ClientIdentity {
                    principal: identity.principal.clone(),
                });
            }
            if !trusted_proxy {
                return Err(
                    "mTLS requires a verified client certificate or an explicitly trusted proxy"
                        .to_owned(),
                );
            }
            let principal = headers
                .get(MTLS_CN_HEADER)
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("missing or empty {MTLS_CN_HEADER} header"))?;
            Ok(ClientIdentity {
                principal: principal.to_owned(),
            })
        }
    }
}

pub(crate) fn validate_password_hash(value: &str, field: &str) -> Result<String> {
    let parsed = PasswordHash::new(value).map_err(|_| {
        LfsServerError::Config(format!("{field} must be a valid scrypt PHC password hash"))
    })?;
    if !value.starts_with("$scrypt$") || !scrypt_params_are_safe(&parsed) {
        return Err(LfsServerError::Config(format!(
            "{field} must be a valid scrypt PHC password hash with at most 128 MiB of verification memory and parallelism at most {MAX_SCRYPT_PARALLELISM}"
        )));
    }
    Ok(value.to_owned())
}

fn verify_password(encoded: &str, password: &[u8]) -> bool {
    let Ok(parsed) = PasswordHash::new(encoded) else {
        return false;
    };
    scrypt_params_are_safe(&parsed) && Scrypt.verify_password(password, &parsed).is_ok()
}

fn scrypt_params_are_safe(hash: &PasswordHash<'_>) -> bool {
    let Ok(params) = scrypt::Params::try_from(hash) else {
        return false;
    };
    let memory = 128u64
        .checked_mul(u64::from(params.r()))
        .and_then(|value| value.checked_shl(u32::from(params.log_n())))
        .unwrap_or(u64::MAX);
    memory <= MAX_SCRYPT_MEMORY_BYTES && params.p() <= MAX_SCRYPT_PARALLELISM
}

fn basic_credentials(headers: &HeaderMap) -> std::result::Result<(String, String), String> {
    let encoded = authorization_value(headers, "Basic")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "invalid Basic credentials".to_owned())?;
    let decoded = String::from_utf8(decoded).map_err(|_| "invalid Basic credentials".to_owned())?;
    let (principal, password) = decoded
        .split_once(':')
        .ok_or_else(|| "invalid Basic credentials".to_owned())?;
    if principal.is_empty() || password.is_empty() {
        return Err("invalid Basic credentials".to_owned());
    }
    Ok((principal.to_owned(), password.to_owned()))
}

fn authorization_value<'a>(
    headers: &'a HeaderMap,
    scheme: &str,
) -> std::result::Result<&'a str, String> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "missing Authorization header".to_owned())?;
    let (actual_scheme, credentials) = value
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("Authorization header must use {scheme}"))?;
    if !actual_scheme.eq_ignore_ascii_case(scheme) || credentials.trim().is_empty() {
        return Err(format!("Authorization header must use {scheme}"));
    }
    Ok(credentials.trim())
}

fn unauthorized(config: &AuthConfig, reason: String) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({ "message": reason })),
    )
        .into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/vnd.git-lfs+json"),
    );
    let challenge = match config {
        AuthConfig::Basic { .. } => Some("Basic realm=\"Git LFS\""),
        AuthConfig::Bearer { .. } => Some("Bearer"),
        AuthConfig::None | AuthConfig::Mtls => None,
    };
    if let Some(challenge) = challenge {
        response.headers_mut().insert(
            header::HeaderName::from_static("lfs-authenticate"),
            header::HeaderValue::from_static(challenge),
        );
    }
    response
}

fn request_rate_limited(retry_after: std::time::Duration) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(serde_json::json!({
            "message": "request rate limit exceeded"
        })),
    )
        .into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/vnd.git-lfs+json"),
    );
    let seconds = retry_after.as_secs().max(1).to_string();
    if let Ok(value) = header::HeaderValue::from_str(&seconds) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn validate_text<'a>(path: &Path, field: &str, value: &'a str) -> Result<&'a str> {
    if value.is_empty() || value.trim() != value {
        return Err(policy_error(
            path,
            &format!("{field} must be non-empty and have no surrounding whitespace"),
        ));
    }
    Ok(value)
}

fn validate_repo_pattern(path: &Path, field: &str, pattern: &str) -> Result<()> {
    let pattern = validate_text(path, field, pattern)?;
    if pattern != "*" && pattern.matches('*').count() > 1 {
        return Err(policy_error(
            path,
            &format!("{field} may contain at most one wildcard"),
        ));
    }
    if pattern != "*" && pattern.contains('*') && !pattern.ends_with('*') {
        return Err(policy_error(
            path,
            &format!("{field} wildcard must be trailing"),
        ));
    }
    let prefix = pattern
        .strip_suffix('*')
        .unwrap_or(pattern)
        .trim_end_matches('/');
    if prefix.is_empty() && pattern != "*" {
        return Err(policy_error(
            path,
            &format!("{field} has an invalid repository pattern"),
        ));
    }
    if prefix.starts_with('/')
        || prefix.contains("//")
        || prefix
            .split('/')
            .any(|segment| segment == "." || segment == "..")
    {
        return Err(policy_error(
            path,
            &format!("{field} has an invalid repository pattern"),
        ));
    }
    Ok(())
}

fn policy_error(path: &Path, detail: &str) -> LfsServerError {
    LfsServerError::Config(format!(
        "invalid policy YAML in {}: {detail}",
        path.display()
    ))
}

fn repo_matches(pattern: &str, repository: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    pattern
        .strip_suffix('*')
        .map_or(pattern == repository, |prefix| {
            repository.starts_with(prefix)
        })
}

fn constant_time_eq(actual: &[u8], expected: &[u8]) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (actual, expected) in actual.iter().zip(expected) {
        difference |= actual ^ expected;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn basic_credentials_map_to_configured_principal() {
        let mut users = HashMap::new();
        users.insert("alice".to_owned(), DUMMY_BASIC_PASSWORD_HASH.to_owned());
        let mut headers = HeaderMap::new();
        let encoded = base64::engine::general_purpose::STANDARD.encode("alice:password".as_bytes());
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).expect("valid header"),
        );
        let identity = extract_identity(
            &AuthConfig::Basic { users },
            &headers,
            &Extensions::new(),
            false,
            false,
        )
        .expect("credentials should authenticate");
        assert_eq!(identity.principal, "alice");
    }

    #[test]
    fn basic_credentials_reject_wrong_password_and_unknown_user() {
        let mut users = HashMap::new();
        users.insert("alice".to_owned(), DUMMY_BASIC_PASSWORD_HASH.to_owned());
        for credentials in ["alice:wrong", "unknown:password"] {
            let mut headers = HeaderMap::new();
            let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {encoded}")).expect("valid header"),
            );
            let error = extract_identity(
                &AuthConfig::Basic {
                    users: users.clone(),
                },
                &headers,
                &Extensions::new(),
                false,
                false,
            )
            .unwrap_err();
            assert_eq!(error, "invalid Basic credentials");
        }
    }

    #[test]
    fn password_hash_validation_rejects_expensive_parameters() {
        let error = validate_password_hash(
            "$scrypt$ln=21,r=8,p=1$aM15713r3Xsvxbi31lqr1Q$nFNh2CVHVjNldFVKDHDlm4CbdRSCdEBsjjJxD+iCs5E",
            "auth.users.alice",
        )
        .unwrap_err();
        assert!(error.to_string().contains("at most 128 MiB"));
    }

    #[test]
    fn proxy_mtls_identity_requires_explicit_trust() {
        let mut headers = HeaderMap::new();
        headers.insert(MTLS_CN_HEADER, HeaderValue::from_static("alice"));
        let error = extract_identity(
            &AuthConfig::Mtls,
            &headers,
            &Extensions::new(),
            false,
            false,
        )
        .unwrap_err();
        assert!(error.contains("explicitly trusted proxy"));

        let identity =
            extract_identity(&AuthConfig::Mtls, &headers, &Extensions::new(), false, true)
                .expect("trusted proxy identity should authenticate");
        assert_eq!(identity.principal, "alice");
    }

    #[test]
    fn policy_matches_exact_and_prefix_repositories() {
        let policy: AuthPolicy = serde_yaml::from_str(
            "rules:\n  - principal: alice\n    repos: [team/model, team/*]\n    actions: [read]\n",
        )
        .expect("valid policy");
        assert!(policy.is_authorized("alice", "team/model", "read"));
        assert!(policy.is_authorized("alice", "team/other", "read"));
        assert!(!policy.is_authorized("alice", "other/model", "read"));
    }
}
