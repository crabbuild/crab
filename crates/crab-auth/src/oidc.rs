//! Provider-neutral OIDC discovery and token endpoint client helpers.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::{AuthError, Result};

/// OIDC discovery document fields used by Crab auth clients.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcDiscovery {
    /// Exact issuer identifier returned by discovery.
    pub issuer: Option<String>,
    /// Authorization endpoint for the authorization code flow.
    pub authorization_endpoint: String,
    /// Token endpoint for exchanging codes and refresh tokens.
    pub token_endpoint: String,
    /// Device authorization endpoint, when the IdP supports it.
    pub device_authorization_endpoint: Option<String>,
    /// Token revocation endpoint (RFC 7009).
    pub revocation_endpoint: Option<String>,
    /// UserInfo endpoint for fetching user profile claims.
    pub userinfo_endpoint: Option<String>,
}

impl OidcDiscovery {
    /// Validates exact issuer binding and HTTPS endpoints for managed login.
    pub fn validate_for_issuer(&self, expected_issuer: &str) -> Result<()> {
        if self.issuer.as_deref() != Some(expected_issuer) {
            return Err(AuthError::InvalidManagedContract(
                "OIDC discovery issuer does not match the managed service issuer".to_owned(),
            ));
        }
        for (name, endpoint) in [
            (
                "authorization_endpoint",
                Some(self.authorization_endpoint.as_str()),
            ),
            ("token_endpoint", Some(self.token_endpoint.as_str())),
            (
                "device_authorization_endpoint",
                self.device_authorization_endpoint.as_deref(),
            ),
            ("revocation_endpoint", self.revocation_endpoint.as_deref()),
            ("userinfo_endpoint", self.userinfo_endpoint.as_deref()),
        ] {
            let Some(endpoint) = endpoint else {
                continue;
            };
            let parsed = url::Url::parse(endpoint).map_err(|_| {
                AuthError::InvalidManagedContract(format!("OIDC {name} is not a valid URL"))
            })?;
            if parsed.scheme() != "https"
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                return Err(AuthError::InvalidManagedContract(format!(
                    "OIDC {name} must be an HTTPS URL without credentials or fragment"
                )));
            }
        }
        Ok(())
    }
}

/// Tokens returned by an OIDC token endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcTokens {
    /// OIDC ID token (JWT).
    pub id_token: String,
    /// OAuth2 access token.
    pub access_token: String,
    /// Refresh token, when issued by the IdP.
    pub refresh_token: Option<String>,
    /// Token lifetime in seconds.
    pub expires_in: u64,
    /// Token type, usually `Bearer`.
    pub token_type: String,
}

/// Fetches the OIDC discovery document for an issuer URL.
pub async fn discover(issuer_url: &str) -> Result<OidcDiscovery> {
    discover_with_client(issuer_url, &reqwest::Client::new()).await
}

/// Fetches OIDC discovery with a caller-configured TLS and redirect policy.
pub async fn discover_with_client(
    issuer_url: &str,
    client: &reqwest::Client,
) -> Result<OidcDiscovery> {
    let endpoint = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );
    debug!(url = %endpoint, "fetching OIDC discovery document");

    let resp = client
        .get(&endpoint)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|source| AuthError::OidcRequest {
            operation: "discovery",
            endpoint: endpoint.clone(),
            source,
        })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|source| AuthError::OidcRequest {
            operation: "read discovery error response",
            endpoint: endpoint.clone(),
            source,
        })?;
        return Err(AuthError::OidcRejected {
            operation: "discovery",
            endpoint,
            status,
            body,
        });
    }

    resp.json::<OidcDiscovery>()
        .await
        .map_err(|source| AuthError::ParseOidcResponse {
            operation: "discovery",
            endpoint,
            source,
        })
}

/// Refreshes tokens using an OIDC `refresh_token` grant.
pub async fn refresh_tokens(
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<OidcTokens> {
    refresh_tokens_with_client(
        token_endpoint,
        client_id,
        refresh_token,
        &reqwest::Client::new(),
    )
    .await
}

/// Refreshes tokens with caller-configured TLS and redirect policy.
pub async fn refresh_tokens_with_client(
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
    client: &reqwest::Client,
) -> Result<OidcTokens> {
    debug!("refreshing tokens via refresh_token grant");

    let resp = client
        .post(token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|source| AuthError::OidcRequest {
            operation: "token refresh",
            endpoint: token_endpoint.to_owned(),
            source,
        })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|source| AuthError::OidcRequest {
            operation: "read token refresh error response",
            endpoint: token_endpoint.to_owned(),
            source,
        })?;
        return Err(AuthError::OidcRefreshExpired {
            endpoint: token_endpoint.to_owned(),
            status,
            body,
        });
    }

    resp.json::<OidcTokens>()
        .await
        .map_err(|source| AuthError::ParseOidcResponse {
            operation: "token refresh",
            endpoint: token_endpoint.to_owned(),
            source,
        })
}

/// Revokes a token at the IdP revocation endpoint on a best-effort basis.
pub async fn revoke_token(revocation_endpoint: &str, client_id: &str, token: &str) -> Result<()> {
    revoke_token_with_client(
        revocation_endpoint,
        client_id,
        token,
        &reqwest::Client::new(),
    )
    .await
}

/// Revokes a token with a caller-configured TLS and redirect policy.
pub async fn revoke_token_with_client(
    revocation_endpoint: &str,
    client_id: &str,
    token: &str,
    client: &reqwest::Client,
) -> Result<()> {
    debug!(endpoint = %revocation_endpoint, "revoking token");

    let resp = client
        .post(revocation_endpoint)
        .form(&[("token", token), ("client_id", client_id)])
        .timeout(Duration::from_secs(30))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            debug!("token revoked successfully");
            Ok(())
        }
        Ok(r) => {
            warn!(
                status = %r.status(),
                "token revocation returned non-success status; continuing"
            );
            Ok(())
        }
        Err(error) => {
            warn!(%error, "token revocation request failed; continuing");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_document_parses_minimal() {
        let json = r#"{
            "authorization_endpoint": "https://idp.example.com/authorize",
            "token_endpoint": "https://idp.example.com/token"
        }"#;
        let disc: OidcDiscovery = serde_json::from_str(json).unwrap();

        assert_eq!(
            disc.authorization_endpoint,
            "https://idp.example.com/authorize"
        );
        assert_eq!(disc.token_endpoint, "https://idp.example.com/token");
        assert!(disc.device_authorization_endpoint.is_none());
        assert!(disc.revocation_endpoint.is_none());
        assert!(disc.userinfo_endpoint.is_none());
        assert!(disc.issuer.is_none());
    }

    #[test]
    fn discovery_document_parses_full() {
        let json = r#"{
            "authorization_endpoint": "https://idp.example.com/authorize",
            "token_endpoint": "https://idp.example.com/token",
            "device_authorization_endpoint": "https://idp.example.com/device",
            "revocation_endpoint": "https://idp.example.com/revoke",
            "userinfo_endpoint": "https://idp.example.com/userinfo"
        }"#;
        let disc: OidcDiscovery = serde_json::from_str(json).unwrap();

        assert_eq!(
            disc.device_authorization_endpoint.as_deref(),
            Some("https://idp.example.com/device")
        );
        assert_eq!(
            disc.revocation_endpoint.as_deref(),
            Some("https://idp.example.com/revoke")
        );
    }

    #[test]
    fn discovery_document_rejects_missing_required() {
        let json = r#"{ "authorization_endpoint": "https://idp.example.com/authorize" }"#;

        assert!(serde_json::from_str::<OidcDiscovery>(json).is_err());
    }

    #[test]
    fn oidc_tokens_round_trip() {
        let tokens = OidcTokens {
            id_token: "eyJ...".into(),
            access_token: "at_123".into(),
            refresh_token: Some("rt_456".into()),
            expires_in: 3600,
            token_type: "Bearer".into(),
        };

        let json = serde_json::to_string(&tokens).unwrap();
        let parsed: OidcTokens = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id_token, "eyJ...");
        assert_eq!(parsed.expires_in, 3600);
        assert_eq!(parsed.refresh_token.as_deref(), Some("rt_456"));
    }

    #[test]
    fn oidc_tokens_parses_without_refresh() {
        let json = r#"{
            "id_token": "eyJ...",
            "access_token": "at_123",
            "expires_in": 3600,
            "token_type": "Bearer"
        }"#;
        let tokens: OidcTokens = serde_json::from_str(json).unwrap();

        assert!(tokens.refresh_token.is_none());
    }

    #[test]
    fn discovery_document_ignores_unknown_fields() {
        let json = r#"{
            "authorization_endpoint": "https://idp.example.com/authorize",
            "token_endpoint": "https://idp.example.com/token",
            "jwks_uri": "https://idp.example.com/.well-known/jwks.json",
            "response_types_supported": ["code", "id_token"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "issuer": "https://idp.example.com",
            "scopes_supported": ["openid", "email", "profile"]
        }"#;
        let disc: OidcDiscovery = serde_json::from_str(json).unwrap();

        assert_eq!(
            disc.authorization_endpoint,
            "https://idp.example.com/authorize"
        );
        assert_eq!(disc.token_endpoint, "https://idp.example.com/token");
        assert!(disc.device_authorization_endpoint.is_none());
    }

    #[test]
    fn managed_discovery_requires_exact_issuer_and_https_endpoints() {
        let mut discovery = OidcDiscovery {
            issuer: Some("https://identity.crab.build".to_owned()),
            authorization_endpoint: "https://identity.crab.build/authorize".to_owned(),
            token_endpoint: "https://identity.crab.build/token".to_owned(),
            device_authorization_endpoint: None,
            revocation_endpoint: None,
            userinfo_endpoint: None,
        };
        discovery
            .validate_for_issuer("https://identity.crab.build")
            .unwrap();

        assert!(
            discovery
                .validate_for_issuer("https://other.example")
                .is_err()
        );
        discovery.token_endpoint = "http://identity.crab.build/token".to_owned();
        assert!(
            discovery
                .validate_for_issuer("https://identity.crab.build")
                .is_err()
        );
    }
}
