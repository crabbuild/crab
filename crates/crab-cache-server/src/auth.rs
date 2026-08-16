//! Authentication middleware and authorization policy for the cache service.
//!
//! Extracts client identity from incoming requests based on the configured
//! [`AuthConfig`] mechanism (mTLS, bearer token, or pre-shared key) and
//! stores it as a request extension so handlers can access it via
//! `Extension<ClientIdentity>`.
//!
//! The optional [`AuthPolicy`] maps principals to repo prefixes and allowed
//! actions, enforced per-request by handlers.

use axum::extract::{Request, State};
use axum::http::{Extensions, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tracing::debug;

use super::config::AuthConfig;
use super::state::AppState;

const POLICY_ACTIONS: &[&str] = &["read", "write", "dedup", "admin"];

/// Authenticated client identity extracted by the auth middleware.
///
/// Handlers access this via `Extension<ClientIdentity>` on authenticated
/// routes. Health and metrics endpoints skip auth entirely (they're
/// registered before the auth layer in the router).
#[derive(Debug, Clone)]
pub struct ClientIdentity {
    /// The authenticated principal: CN from a client cert, subject from a
    /// bearer token, or `"psk-client"` for pre-shared key auth.
    pub principal: String,
}

/// Identity extracted from a verified native mTLS client certificate.
#[derive(Debug, Clone)]
pub struct TlsClientIdentity {
    /// Auth principal derived from the verified leaf certificate.
    pub principal: String,
}

/// Header set by the mTLS terminator (reverse proxy or axum-server TLS
/// layer) containing the client certificate's Common Name.
const MTLS_CN_HEADER: &str = "x-client-cn";

/// Header carrying the pre-shared key for PSK authentication.
const PSK_HEADER: &str = "x-cache-psk";

/// Axum middleware that authenticates requests and inserts a
/// [`ClientIdentity`] into request extensions.
///
/// Applied to the router via `axum::middleware::from_fn_with_state`.
/// Routes registered *before* this layer (health, metrics) are not
/// subject to authentication.
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let native_mtls = state
        .config
        .tls
        .as_ref()
        .is_some_and(|tls| tls.client_ca_path.is_some());
    match extract_identity(
        &state.config.auth,
        &headers,
        request.extensions(),
        native_mtls,
    ) {
        Ok(identity) => {
            debug!(principal = %identity.principal, "authenticated client");
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        Err(reason) => {
            debug!(reason = %reason, "authentication failed");
            (StatusCode::UNAUTHORIZED, reason).into_response()
        }
    }
}

/// Extract a [`ClientIdentity`] from request metadata based on the configured
/// auth mechanism. Returns `Err(reason)` on failure.
fn extract_identity(
    config: &AuthConfig,
    headers: &HeaderMap,
    extensions: &Extensions,
    native_mtls: bool,
) -> Result<ClientIdentity, String> {
    match config {
        AuthConfig::Mtls => extract_mtls(headers, extensions, native_mtls),
        AuthConfig::Bearer { .. } => extract_bearer(headers),
        AuthConfig::Psk { key_hash } => extract_psk(headers, key_hash),
    }
}

/// mTLS: use the verified TLS extension for native mTLS or a trusted proxy
/// header for deployments that terminate mTLS before the cache service.
fn extract_mtls(
    headers: &HeaderMap,
    extensions: &Extensions,
    native_mtls: bool,
) -> Result<ClientIdentity, String> {
    if native_mtls {
        let identity = extensions
            .get::<TlsClientIdentity>()
            .ok_or("missing verified client certificate identity")?;
        return Ok(ClientIdentity {
            principal: identity.principal.clone(),
        });
    }

    let cn = headers
        .get(MTLS_CN_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing or empty {MTLS_CN_HEADER} header"))?;

    Ok(ClientIdentity {
        principal: cn.to_string(),
    })
}

/// Bearer: extract the token from the `Authorization: Bearer <token>` header.
///
/// For now, the token value itself is used as the principal. Full JWT
/// validation (signature check against JWKS, expiry, claims extraction)
/// is a separate concern — the middleware just extracts the credential.
fn extract_bearer(headers: &HeaderMap) -> Result<ClientIdentity, String> {
    let auth_value = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or("missing Authorization header")?;

    let token = auth_value
        .strip_prefix("Bearer ")
        .filter(|t| !t.is_empty())
        .ok_or("Authorization header must be 'Bearer <token>'")?;

    Ok(ClientIdentity {
        principal: token.to_string(),
    })
}

/// PSK: extract the key from the `X-Cache-PSK` header, blake3-hash it,
/// and compare against the configured `key_hash` using constant-time
/// comparison.
fn extract_psk(headers: &HeaderMap, expected_hash: &[u8; 32]) -> Result<ClientIdentity, String> {
    let psk = headers
        .get(PSK_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing or empty {PSK_HEADER} header"))?;

    let actual_hash = blake3::hash(psk.as_bytes());

    // Constant-time comparison to prevent timing attacks.
    if constant_time_eq(actual_hash.as_bytes(), expected_hash) {
        Ok(ClientIdentity {
            principal: "psk-client".to_string(),
        })
    } else {
        Err("invalid pre-shared key".to_string())
    }
}

/// Constant-time byte comparison to prevent timing side-channels.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Authorization policy
// ---------------------------------------------------------------------------

/// YAML-based authorization policy mapping principals to repo prefixes and
/// allowed actions.
///
/// When loaded from a policy file, each request is checked against the rules
/// to determine whether the authenticated principal may perform the requested
/// action on the given repo path.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AuthPolicy {
    pub rules: Vec<PolicyRule>,
}

/// Redacted authorization policy diagnostics for startup checks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuthPolicyDiagnostics {
    pub rule_count: usize,
    pub repo_pattern_count: usize,
    pub actions: Vec<String>,
}

/// A single authorization rule granting a principal access to a set of repos
/// for a set of actions.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PolicyRule {
    /// Principal identifier, e.g. `"CN=dev-team"` or `"psk-client"`.
    pub principal: String,
    /// Repo path patterns with glob-style `*` suffix, e.g. `["org/team-a/*", "org/shared/*"]`.
    pub repos: Vec<String>,
    /// Allowed actions: `"read"`, `"write"`, `"dedup"`, `"admin"`.
    pub actions: Vec<String>,
}

impl AuthPolicy {
    /// Parse a YAML policy file from disk.
    pub fn from_file(path: &std::path::Path) -> Result<Self, super::error::CacheServiceError> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            super::error::CacheServiceError::ConfigError(format!(
                "failed to read policy file {}: {e}",
                path.display()
            ))
        })?;
        let policy: AuthPolicy = serde_yaml::from_str(&contents).map_err(|e| {
            super::error::CacheServiceError::ConfigError(format!(
                "invalid policy YAML in {}: {e}",
                path.display()
            ))
        })?;
        policy.validate(path)?;
        Ok(policy)
    }

    fn validate(&self, path: &std::path::Path) -> Result<(), super::error::CacheServiceError> {
        if self.rules.is_empty() {
            return Err(policy_config_error(
                path,
                "rules must contain at least one rule",
            ));
        }

        for (index, rule) in self.rules.iter().enumerate() {
            let rule_path = format!("rules[{index}]");
            validate_non_empty_trimmed(path, &format!("{rule_path}.principal"), &rule.principal)?;

            if rule.repos.is_empty() {
                return Err(policy_config_error(
                    path,
                    &format!("{rule_path}.repos must contain at least one pattern"),
                ));
            }
            for (repo_index, pattern) in rule.repos.iter().enumerate() {
                validate_repo_pattern(path, &format!("{rule_path}.repos[{repo_index}]"), pattern)?;
            }

            if rule.actions.is_empty() {
                return Err(policy_config_error(
                    path,
                    &format!("{rule_path}.actions must contain at least one action"),
                ));
            }
            for (action_index, action) in rule.actions.iter().enumerate() {
                let field = format!("{rule_path}.actions[{action_index}]");
                let action = validate_non_empty_trimmed(path, &field, action)?;
                if !POLICY_ACTIONS.contains(&action) {
                    return Err(policy_config_error(
                        path,
                        &format!(
                            "{field} has unknown action {action:?}; expected one of {}",
                            POLICY_ACTIONS.join(", ")
                        ),
                    ));
                }
            }
        }

        Ok(())
    }

    /// Check whether `principal` is authorized to perform `action` on `repo_path`.
    ///
    /// Uses glob-style matching: a repo pattern ending in `*` matches any
    /// repo path sharing that prefix. A bare `"*"` matches everything.
    pub fn is_authorized(&self, principal: &str, repo_path: &str, action: &str) -> bool {
        self.rules.iter().any(|rule| {
            rule.principal == principal
                && rule.actions.iter().any(|a| a == action)
                && rule
                    .repos
                    .iter()
                    .any(|pattern| glob_match(pattern, repo_path))
        })
    }

    /// Check whether `principal` has the given `action` permission regardless
    /// of repo (used for actions like "dedup" and "admin" that aren't
    /// repo-scoped).
    pub fn has_action(&self, principal: &str, action: &str) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.principal == principal && rule.actions.iter().any(|a| a == action))
    }

    /// Return redacted policy diagnostics for preflight output.
    pub fn diagnostics(&self) -> AuthPolicyDiagnostics {
        let actions = POLICY_ACTIONS
            .iter()
            .filter(|&&action| {
                self.rules.iter().any(|rule| {
                    rule.actions
                        .iter()
                        .any(|candidate| candidate.as_str() == action)
                })
            })
            .map(|action| (*action).to_string())
            .collect();

        AuthPolicyDiagnostics {
            rule_count: self.rules.len(),
            repo_pattern_count: self.rules.iter().map(|rule| rule.repos.len()).sum(),
            actions,
        }
    }
}

fn validate_non_empty_trimmed<'a>(
    path: &std::path::Path,
    field: &str,
    value: &'a str,
) -> Result<&'a str, super::error::CacheServiceError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(policy_config_error(
            path,
            &format!("{field} must not be empty"),
        ));
    }
    if trimmed != value {
        return Err(policy_config_error(
            path,
            &format!("{field} must not have leading or trailing whitespace"),
        ));
    }
    Ok(trimmed)
}

fn validate_repo_pattern(
    path: &std::path::Path,
    field: &str,
    pattern: &str,
) -> Result<(), super::error::CacheServiceError> {
    let pattern = validate_non_empty_trimmed(path, field, pattern)?;
    if pattern == "*" {
        return Ok(());
    }
    if pattern.matches('*').count() > 1 || (pattern.contains('*') && !pattern.ends_with('*')) {
        return Err(policy_config_error(
            path,
            &format!("{field} may only use a single trailing '*' wildcard"),
        ));
    }

    let mut prefix = pattern.strip_suffix('*').unwrap_or(pattern);
    if let Some(stripped) = prefix.strip_suffix('/') {
        prefix = stripped;
    }
    if prefix.is_empty() || prefix.starts_with('/') || prefix.contains("//") {
        return Err(policy_config_error(
            path,
            &format!("{field} has invalid repo pattern {pattern:?}"),
        ));
    }
    if prefix
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(policy_config_error(
            path,
            &format!("{field} must not contain '.' or '..' path segments"),
        ));
    }
    Ok(())
}

fn policy_config_error(path: &std::path::Path, detail: &str) -> super::error::CacheServiceError {
    super::error::CacheServiceError::ConfigError(format!(
        "invalid policy YAML in {}: {detail}",
        path.display()
    ))
}

/// Simple glob matching: patterns ending in `*` match any path sharing the
/// prefix. A bare `"*"` matches everything. Patterns without `*` require an
/// exact match.
fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        value.starts_with(prefix)
    } else {
        pattern == value
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn make_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            // Test helper inside `mod tests` — unwrap is acceptable.
            // See finding CR7-F5.
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes())
                    .expect("test header name must be valid"),
                HeaderValue::from_str(v).expect("test header value must be valid"),
            );
        }
        map
    }

    fn extract_for_test(
        config: &AuthConfig,
        headers: &HeaderMap,
    ) -> Result<ClientIdentity, String> {
        let extensions = Extensions::new();
        extract_identity(config, headers, &extensions, false)
    }

    // --- mTLS ---

    #[test]
    fn mtls_extracts_cn_from_header() {
        let headers = make_headers(&[("x-client-cn", "CN=dev-team")]);
        let id = extract_for_test(&AuthConfig::Mtls, &headers).unwrap();
        assert_eq!(id.principal, "CN=dev-team");
    }

    #[test]
    fn mtls_rejects_missing_header() {
        let headers = HeaderMap::new();
        let err = extract_for_test(&AuthConfig::Mtls, &headers).unwrap_err();
        assert!(err.contains("x-client-cn"));
    }

    #[test]
    fn mtls_rejects_empty_header() {
        let headers = make_headers(&[("x-client-cn", "")]);
        let err = extract_for_test(&AuthConfig::Mtls, &headers).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn mtls_extracts_native_tls_identity() {
        let headers = HeaderMap::new();
        let mut extensions = Extensions::new();
        extensions.insert(TlsClientIdentity {
            principal: "mtls-sha256:abc123".to_string(),
        });

        let id = extract_identity(&AuthConfig::Mtls, &headers, &extensions, true).unwrap();

        assert_eq!(id.principal, "mtls-sha256:abc123");
    }

    #[test]
    fn mtls_rejects_missing_native_tls_identity() {
        let headers = make_headers(&[("x-client-cn", "CN=proxy-client")]);
        let extensions = Extensions::new();

        let err = extract_identity(&AuthConfig::Mtls, &headers, &extensions, true).unwrap_err();

        assert!(err.contains("verified client certificate"));
    }

    // --- Bearer ---

    #[test]
    fn bearer_extracts_token() {
        let headers = make_headers(&[("authorization", "Bearer my-jwt-token")]);
        let id = extract_for_test(&AuthConfig::Bearer { jwks_url: None }, &headers).unwrap();
        assert_eq!(id.principal, "my-jwt-token");
    }

    #[test]
    fn bearer_rejects_missing_header() {
        let headers = HeaderMap::new();
        let err = extract_for_test(&AuthConfig::Bearer { jwks_url: None }, &headers).unwrap_err();
        assert!(err.contains("Authorization"));
    }

    #[test]
    fn bearer_rejects_wrong_scheme() {
        let headers = make_headers(&[("authorization", "Basic dXNlcjpwYXNz")]);
        let err = extract_for_test(&AuthConfig::Bearer { jwks_url: None }, &headers).unwrap_err();
        assert!(err.contains("Bearer"));
    }

    #[test]
    fn bearer_rejects_empty_token() {
        let headers = make_headers(&[("authorization", "Bearer ")]);
        let err = extract_for_test(&AuthConfig::Bearer { jwks_url: None }, &headers).unwrap_err();
        assert!(err.contains("Bearer"));
    }

    // --- PSK ---

    #[test]
    fn psk_accepts_correct_key() {
        let key = "my-secret-key";
        let key_hash: [u8; 32] = *blake3::hash(key.as_bytes()).as_bytes();
        let headers = make_headers(&[("x-cache-psk", key)]);
        let id = extract_for_test(&AuthConfig::Psk { key_hash }, &headers).unwrap();
        assert_eq!(id.principal, "psk-client");
    }

    #[test]
    fn psk_rejects_wrong_key() {
        let key_hash: [u8; 32] = *blake3::hash(b"correct-key").as_bytes();
        let headers = make_headers(&[("x-cache-psk", "wrong-key")]);
        let err = extract_for_test(&AuthConfig::Psk { key_hash }, &headers).unwrap_err();
        assert!(err.contains("invalid"));
    }

    #[test]
    fn psk_rejects_missing_header() {
        let key_hash: [u8; 32] = *blake3::hash(b"key").as_bytes();
        let headers = HeaderMap::new();
        let err = extract_for_test(&AuthConfig::Psk { key_hash }, &headers).unwrap_err();
        assert!(err.contains("x-cache-psk"));
    }

    #[test]
    fn psk_rejects_empty_header() {
        let key_hash: [u8; 32] = *blake3::hash(b"key").as_bytes();
        let headers = make_headers(&[("x-cache-psk", "")]);
        let err = extract_for_test(&AuthConfig::Psk { key_hash }, &headers).unwrap_err();
        assert!(err.contains("empty"));
    }

    // --- constant_time_eq ---

    #[test]
    fn constant_time_eq_same_bytes() {
        assert!(constant_time_eq(&[1, 2, 3], &[1, 2, 3]));
    }

    #[test]
    fn constant_time_eq_different_bytes() {
        assert!(!constant_time_eq(&[1, 2, 3], &[1, 2, 4]));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(&[1, 2], &[1, 2, 3]));
    }

    // --- glob_match ---

    #[test]
    fn glob_match_wildcard_matches_all() {
        assert!(glob_match("*", "org/repo-a"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_match_prefix_wildcard() {
        assert!(glob_match("org/*", "org/repo-a"));
        assert!(glob_match("org/*", "org/repo-b/sub"));
        assert!(!glob_match("org/*", "other/repo"));
    }

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("org/repo-a", "org/repo-a"));
        assert!(!glob_match("org/repo-a", "org/repo-b"));
    }

    // --- AuthPolicy ---

    fn sample_policy() -> AuthPolicy {
        AuthPolicy {
            rules: vec![
                PolicyRule {
                    principal: "CN=dev-team".to_string(),
                    repos: vec!["org/team-a/*".to_string(), "org/shared/*".to_string()],
                    actions: vec!["read".to_string(), "write".to_string(), "dedup".to_string()],
                },
                PolicyRule {
                    principal: "CN=ci-runner".to_string(),
                    repos: vec!["org/*".to_string()],
                    actions: vec!["read".to_string(), "dedup".to_string()],
                },
                PolicyRule {
                    principal: "CN=admin".to_string(),
                    repos: vec!["*".to_string()],
                    actions: vec![
                        "read".to_string(),
                        "write".to_string(),
                        "dedup".to_string(),
                        "admin".to_string(),
                    ],
                },
            ],
        }
    }

    #[test]
    fn policy_authorizes_matching_principal_repo_action() {
        let policy = sample_policy();
        assert!(policy.is_authorized("CN=dev-team", "org/team-a/repo", "read"));
        assert!(policy.is_authorized("CN=dev-team", "org/shared/data", "write"));
    }

    #[test]
    fn policy_rejects_unauthorized_repo() {
        let policy = sample_policy();
        assert!(!policy.is_authorized("CN=dev-team", "org/other-team/repo", "read"));
    }

    #[test]
    fn policy_rejects_unauthorized_action() {
        let policy = sample_policy();
        assert!(!policy.is_authorized("CN=ci-runner", "org/repo", "write"));
    }

    #[test]
    fn policy_admin_has_full_access() {
        let policy = sample_policy();
        assert!(policy.is_authorized("CN=admin", "any/repo", "admin"));
        assert!(policy.is_authorized("CN=admin", "org/team-a/repo", "read"));
    }

    #[test]
    fn policy_has_action_checks_without_repo() {
        let policy = sample_policy();
        assert!(policy.has_action("CN=dev-team", "dedup"));
        assert!(!policy.has_action("CN=dev-team", "admin"));
        assert!(policy.has_action("CN=admin", "admin"));
    }

    #[test]
    fn policy_unknown_principal_denied() {
        let policy = sample_policy();
        assert!(!policy.is_authorized("CN=unknown", "org/repo", "read"));
        assert!(!policy.has_action("CN=unknown", "read"));
    }

    #[test]
    fn policy_diagnostics_count_rules_patterns_and_actions_without_principals() {
        let policy = sample_policy();

        let diagnostics = policy.diagnostics();

        assert_eq!(diagnostics.rule_count, 3);
        assert_eq!(diagnostics.repo_pattern_count, 4);
        assert_eq!(
            diagnostics.actions,
            vec![
                "read".to_string(),
                "write".to_string(),
                "dedup".to_string(),
                "admin".to_string(),
            ]
        );
        let json = serde_json::to_string(&diagnostics).unwrap();
        assert!(!json.contains("CN=dev-team"));
        assert!(!json.contains("CN=admin"));
    }

    #[test]
    fn policy_parses_from_yaml() {
        let yaml = r#"
rules:
  - principal: "CN=dev-team"
    repos: ["org/team-a/*", "org/shared/*"]
    actions: ["read", "write", "dedup"]
  - principal: "CN=admin"
    repos: ["*"]
    actions: ["read", "write", "dedup", "admin"]
"#;
        let policy: AuthPolicy = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(policy.rules.len(), 2);
        assert!(policy.is_authorized("CN=dev-team", "org/team-a/repo", "read"));
        assert!(policy.is_authorized("CN=admin", "any/repo", "admin"));
        assert!(!policy.is_authorized("CN=dev-team", "other/repo", "read"));
    }

    fn load_policy_from_yaml(
        yaml: &str,
    ) -> std::result::Result<AuthPolicy, crate::error::CacheServiceError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.yaml");
        std::fs::write(&path, yaml).unwrap();
        AuthPolicy::from_file(&path)
    }

    #[test]
    fn policy_from_file_accepts_valid_yaml() {
        let policy = load_policy_from_yaml(
            r#"
rules:
  - principal: "mtls-sha256:abc123"
    repos: [".crab", "org/team/*"]
    actions: ["read", "write", "dedup", "admin"]
"#,
        )
        .unwrap();

        assert!(policy.is_authorized("mtls-sha256:abc123", ".crab", "read"));
    }

    #[test]
    fn policy_from_file_rejects_empty_rules() {
        let err = load_policy_from_yaml("rules: []\n").unwrap_err();

        assert!(err.to_string().contains("at least one rule"));
    }

    #[test]
    fn policy_from_file_rejects_empty_principal() {
        let err = load_policy_from_yaml(
            r#"
rules:
  - principal: ""
    repos: [".crab"]
    actions: ["read"]
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("principal"));
    }

    #[test]
    fn policy_from_file_rejects_empty_repos() {
        let err = load_policy_from_yaml(
            r#"
rules:
  - principal: "psk-client"
    repos: []
    actions: ["read"]
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("repos"));
    }

    #[test]
    fn policy_from_file_rejects_invalid_repo_pattern() {
        let err = load_policy_from_yaml(
            r#"
rules:
  - principal: "psk-client"
    repos: ["org/../repo"]
    actions: ["read"]
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn policy_from_file_rejects_mid_pattern_wildcard() {
        let err = load_policy_from_yaml(
            r#"
rules:
  - principal: "psk-client"
    repos: ["org/*/repo"]
    actions: ["read"]
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("trailing '*'"));
    }

    #[test]
    fn policy_from_file_rejects_empty_actions() {
        let err = load_policy_from_yaml(
            r#"
rules:
  - principal: "psk-client"
    repos: [".crab"]
    actions: []
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("actions"));
    }

    #[test]
    fn policy_from_file_rejects_unknown_action() {
        let err = load_policy_from_yaml(
            r#"
rules:
  - principal: "psk-client"
    repos: [".crab"]
    actions: ["read", "admn"]
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unknown action"));
    }
}
