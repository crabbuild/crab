//! OIDC authentication flows for `crab login`.
//!
//! `crab-auth` owns provider-neutral discovery, refresh, and revocation.
//! This CLI Adapter owns:
//! - Authorization code flow with PKCE (S256)
//! - Device code flow (for headless/SSH sessions)

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncBufReadExt;
use tokio::net::TcpListener;
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result};

pub use crab_auth::oidc::{OidcDiscovery, OidcTokens, discover, refresh_tokens, revoke_token};

/// 5-minute timeout for interactive authentication flows.
const AUTH_TIMEOUT: Duration = Duration::from_mins(5);

/// Minimum PKCE code_verifier length (RFC 7636).
const PKCE_VERIFIER_LEN: usize = 64;

/// Response from the device authorization endpoint.
#[derive(Debug, Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

/// Token endpoint error response.
#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Run the authorization code flow with PKCE (S256).
///
/// 1. Generate `code_verifier` + `code_challenge` (S256)
/// 2. Start local HTTP listener on a random port
/// 3. Open browser to `authorization_endpoint` with `redirect_uri=http://localhost:{port}/callback`
/// 4. Wait for callback with authorization code (timeout: 5 minutes)
/// 5. Exchange code + code_verifier for tokens at `token_endpoint`
pub async fn authorization_code_flow(
    discovery: &OidcDiscovery,
    client_id: &str,
    scopes: &str,
) -> Result<OidcTokens> {
    let client = reqwest::Client::new();
    authorization_code_flow_with_client(discovery, client_id, scopes, &client).await
}

/// Run authorization-code PKCE with caller-configured TLS and redirect policy.
pub async fn authorization_code_flow_with_client(
    discovery: &OidcDiscovery,
    client_id: &str,
    scopes: &str,
    client: &reqwest::Client,
) -> Result<OidcTokens> {
    let (code_verifier, code_challenge) = generate_pkce_pair();
    let state = generate_state();

    // Bind to a random port on localhost.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| CrabError::AuthFailed {
            path: format!("failed to bind local listener: {e}"),
        })?;
    let local_addr = listener.local_addr().map_err(|e| CrabError::AuthFailed {
        path: format!("failed to get local address: {e}"),
    })?;
    let redirect_uri = format!("http://127.0.0.1:{}/callback", local_addr.port());

    // Build the authorization URL.
    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        discovery.authorization_endpoint,
        urlencoded(client_id),
        urlencoded(&redirect_uri),
        urlencoded(scopes),
        urlencoded(&state),
        urlencoded(&code_challenge),
    );

    // Launch browser or display URL.
    open_browser(&auth_url);

    // Wait for the callback with a 5-minute timeout.
    let code = tokio::time::timeout(AUTH_TIMEOUT, wait_for_callback(listener, &state))
        .await
        .map_err(|_| CrabError::AuthFailed {
            path: "authorization code flow timed out after 5 minutes".into(),
        })??;

    // Exchange the authorization code for tokens.
    exchange_code(
        client,
        &discovery.token_endpoint,
        client_id,
        &code,
        &redirect_uri,
        &code_verifier,
    )
    .await
}

/// Run the device code flow.
///
/// 1. POST to `device_authorization_endpoint` → device_code, user_code, verification_uri
/// 2. Display verification_uri and user_code on stderr
/// 3. Poll token_endpoint every `interval` until user completes auth (timeout: 5 minutes)
pub async fn device_code_flow(
    discovery: &OidcDiscovery,
    client_id: &str,
    scopes: &str,
) -> Result<OidcTokens> {
    let client = reqwest::Client::new();
    device_code_flow_with_client(discovery, client_id, scopes, &client).await
}

/// Run device authorization with caller-configured TLS and redirect policy.
pub async fn device_code_flow_with_client(
    discovery: &OidcDiscovery,
    client_id: &str,
    scopes: &str,
    client: &reqwest::Client,
) -> Result<OidcTokens> {
    let device_endpoint = discovery
        .device_authorization_endpoint
        .as_deref()
        .ok_or_else(|| CrabError::AuthFailed {
            path: "IdP does not support device code flow (no device_authorization_endpoint)".into(),
        })?;

    let resp = client
        .post(device_endpoint)
        .form(&[("client_id", client_id), ("scope", scopes)])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| CrabError::AuthFailed {
            path: format!("device authorization request failed: {e}"),
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.map_err(|e| CrabError::AuthFailed {
            path: format!("failed to read device authorization error response: {e}"),
        })?;
        return Err(CrabError::AuthFailed {
            path: format!("device authorization returned HTTP {status}: {body}"),
        });
    }

    let device_auth: DeviceAuthResponse = resp.json().await.map_err(|e| CrabError::AuthFailed {
        path: format!("failed to parse device authorization response: {e}"),
    })?;

    // Display the user code and verification URI on stderr.
    eprintln!();
    eprintln!("To authenticate, open this URL in a browser:");
    eprintln!("  {}", device_auth.verification_uri);
    eprintln!();
    eprintln!("Enter code: {}", device_auth.user_code);
    eprintln!();

    let poller = HttpDevicePoller { client };
    tokio::time::timeout(
        AUTH_TIMEOUT,
        poll_device_tokens(&poller, discovery, client_id, &device_auth),
    )
    .await
    .map_err(|_| CrabError::AuthFailed {
        path: "device code flow timed out after 5 minutes".into(),
    })?
}

enum TokenPollResponse {
    Tokens(OidcTokens),
    OAuthError(TokenErrorResponse),
    Unexpected(String),
}

#[async_trait]
trait DevicePoller: Send + Sync {
    async fn wait(&self, duration: Duration);
    async fn poll(
        &self,
        token_endpoint: &str,
        client_id: &str,
        device_code: &str,
    ) -> Result<TokenPollResponse>;
}

struct HttpDevicePoller<'a> {
    client: &'a reqwest::Client,
}

#[async_trait]
impl DevicePoller for HttpDevicePoller<'_> {
    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn poll(
        &self,
        token_endpoint: &str,
        client_id: &str,
        device_code: &str,
    ) -> Result<TokenPollResponse> {
        let response = self
            .client
            .post(token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", client_id),
            ])
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| CrabError::AuthFailed {
                path: format!("device code token poll failed: {error}"),
            })?;
        if response.status().is_success() {
            return response
                .json::<OidcTokens>()
                .await
                .map(TokenPollResponse::Tokens)
                .map_err(|error| CrabError::AuthFailed {
                    path: format!("failed to parse token response: {error}"),
                });
        }
        let body = response
            .text()
            .await
            .map_err(|error| CrabError::AuthFailed {
                path: format!("failed to read device code token response: {error}"),
            })?;
        Ok(serde_json::from_str::<TokenErrorResponse>(&body)
            .map(TokenPollResponse::OAuthError)
            .unwrap_or(TokenPollResponse::Unexpected(body)))
    }
}

async fn poll_device_tokens(
    poller: &dyn DevicePoller,
    discovery: &OidcDiscovery,
    client_id: &str,
    device_auth: &DeviceAuthResponse,
) -> Result<OidcTokens> {
    if device_auth.interval == 0 || device_auth.expires_in == 0 {
        return Err(CrabError::AuthFailed {
            path: "device authorization returned invalid polling bounds".into(),
        });
    }
    let expires_after = Duration::from_secs(device_auth.expires_in).min(AUTH_TIMEOUT);
    let mut elapsed = Duration::ZERO;
    let mut poll_interval = Duration::from_secs(device_auth.interval);
    loop {
        if elapsed.saturating_add(poll_interval) > expires_after {
            return Err(CrabError::AuthFailed {
                path: "device authorization code expired".into(),
            });
        }
        poller.wait(poll_interval).await;
        elapsed = elapsed.saturating_add(poll_interval);
        match poller
            .poll(
                &discovery.token_endpoint,
                client_id,
                &device_auth.device_code,
            )
            .await?
        {
            TokenPollResponse::Tokens(tokens) => return Ok(tokens),
            TokenPollResponse::OAuthError(error) if error.error == "authorization_pending" => {
                debug!("device code flow: authorization pending, polling again");
            }
            TokenPollResponse::OAuthError(error) if error.error == "slow_down" => {
                debug!("device code flow: slow_down, increasing interval");
                poll_interval = poll_interval.saturating_add(Duration::from_secs(5));
            }
            TokenPollResponse::OAuthError(error) => {
                let description = error.error_description.unwrap_or(error.error);
                return Err(CrabError::AuthFailed {
                    path: format!("device code flow rejected: {description}"),
                });
            }
            TokenPollResponse::Unexpected(body) => {
                return Err(CrabError::AuthFailed {
                    path: format!("unexpected token endpoint response: {body}"),
                });
            }
        }
    }
}

/// Generate a PKCE code_verifier and its S256 code_challenge.
///
/// The verifier is a 64-character random string from the unreserved
/// character set (RFC 7636 §4.1). The challenge is the base64url-encoded
/// SHA-256 hash of the verifier.
fn generate_pkce_pair() -> (String, String) {
    let mut rng = rand::rng();
    let verifier: String = (0..PKCE_VERIFIER_LEN)
        .map(|_| {
            const CHARSET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    let challenge = URL_SAFE_NO_PAD.encode(hash);

    (verifier, challenge)
}

/// Generate a random state parameter for CSRF protection.
fn generate_state() -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// Minimal percent-encoding for URL query parameters.
fn urlencoded(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Attempt to open a URL in the user's default browser.
///
/// On macOS uses `open`, on Linux uses `xdg-open`. If neither works,
/// prints the URL to stderr for manual opening.
fn open_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("xdg-open").arg(url).spawn()
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported platform for browser launch",
        ))
    };

    match result {
        Ok(_) => debug!("opened browser for authentication"),
        Err(e) => {
            warn!(error = %e, "could not open browser automatically");
            eprintln!();
            eprintln!("Open this URL in your browser to authenticate:");
            eprintln!("  {url}");
            eprintln!();
        }
    }
}

/// Wait for the OAuth2 callback on the local HTTP listener.
///
/// Parses the `code` and `state` query parameters from the GET request,
/// validates the state, and returns the authorization code. Serves a
/// simple HTML success page to the browser.
async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    let (stream, _) = listener.accept().await.map_err(|e| CrabError::AuthFailed {
        path: format!("failed to accept callback connection: {e}"),
    })?;

    let mut reader = tokio::io::BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .map_err(|e| CrabError::AuthFailed {
            path: format!("failed to read callback request: {e}"),
        })?;

    // Parse the request line: "GET /callback?code=xxx&state=yyy HTTP/1.1"
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| CrabError::AuthFailed {
            path: "malformed callback request".into(),
        })?;

    let query = path.split_once('?').map_or("", |(_, q)| q);

    let params: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

    // Check for error response from IdP.
    if let Some(error) = params.get("error") {
        let desc = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        send_error_page(&mut reader).await;
        return Err(CrabError::AuthFailed {
            path: format!("IdP returned error: {desc}"),
        });
    }

    // Validate state to prevent CSRF.
    let state = params.get("state").ok_or_else(|| CrabError::AuthFailed {
        path: "callback missing state parameter".into(),
    })?;
    if state != expected_state {
        send_error_page(&mut reader).await;
        return Err(CrabError::AuthFailed {
            path: "callback state mismatch (possible CSRF)".into(),
        });
    }

    let code = params
        .get("code")
        .ok_or_else(|| CrabError::AuthFailed {
            path: "callback missing authorization code".into(),
        })?
        .clone();

    // Send a success page to the browser.
    send_success_page(&mut reader).await;

    Ok(code)
}

/// Send a simple HTML success page to the browser callback.
async fn send_success_page<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W) {
    let body = concat!(
        "<html><body style=\"font-family:sans-serif;text-align:center;padding:2em\">",
        "<h2>Authentication successful</h2>",
        "<p>You can close this tab and return to the terminal.</p>",
        "</body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = tokio::io::AsyncWriteExt::write_all(writer, response.as_bytes()).await;
    let _ = tokio::io::AsyncWriteExt::flush(writer).await;
}

/// Send a simple HTML error page to the browser callback.
async fn send_error_page<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W) {
    let body = concat!(
        "<html><body style=\"font-family:sans-serif;text-align:center;padding:2em\">",
        "<h2>Authentication failed</h2>",
        "<p>Please check the terminal for details.</p>",
        "</body></html>"
    );
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = tokio::io::AsyncWriteExt::write_all(writer, response.as_bytes()).await;
    let _ = tokio::io::AsyncWriteExt::flush(writer).await;
}

/// Exchange an authorization code for tokens at the token endpoint.
async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<OidcTokens> {
    debug!("exchanging authorization code for tokens");

    let resp = client
        .post(token_endpoint)
        .form(&authorization_code_parameters(
            client_id,
            code,
            redirect_uri,
            code_verifier,
        ))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| CrabError::AuthFailed {
            path: format!("token exchange request failed: {e}"),
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.map_err(|e| CrabError::AuthFailed {
            path: format!("failed to read token exchange error response: {e}"),
        })?;
        return Err(CrabError::AuthFailed {
            path: format!("token exchange returned HTTP {status}: {body}"),
        });
    }

    resp.json::<OidcTokens>()
        .await
        .map_err(|e| CrabError::AuthFailed {
            path: format!("failed to parse token exchange response: {e}"),
        })
}

fn authorization_code_parameters<'a>(
    client_id: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    code_verifier: &'a str,
) -> [(&'static str, &'a str); 5] {
    [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", code_verifier),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    struct MockPoller {
        waits: Mutex<Vec<Duration>>,
        responses: Mutex<VecDeque<TokenPollResponse>>,
    }

    #[async_trait]
    impl DevicePoller for MockPoller {
        async fn wait(&self, duration: Duration) {
            self.waits.lock().unwrap().push(duration);
        }

        async fn poll(
            &self,
            _token_endpoint: &str,
            _client_id: &str,
            _device_code: &str,
        ) -> Result<TokenPollResponse> {
            Ok(self.responses.lock().unwrap().pop_front().unwrap())
        }
    }

    fn oidc_discovery() -> OidcDiscovery {
        OidcDiscovery {
            issuer: Some("https://identity.crab.build".to_owned()),
            authorization_endpoint: "https://identity.crab.build/authorize".to_owned(),
            token_endpoint: "https://identity.crab.build/token".to_owned(),
            device_authorization_endpoint: Some("https://identity.crab.build/device".to_owned()),
            revocation_endpoint: None,
            userinfo_endpoint: None,
        }
    }

    fn device_auth(interval: u64, expires_in: u64) -> DeviceAuthResponse {
        DeviceAuthResponse {
            device_code: "device-code".to_owned(),
            user_code: "ABCD-EFGH".to_owned(),
            verification_uri: "https://identity.crab.build/device".to_owned(),
            interval,
            expires_in,
        }
    }

    fn tokens() -> OidcTokens {
        OidcTokens {
            id_token: "header.payload.signature".to_owned(),
            access_token: "access".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            expires_in: 3600,
            token_type: "Bearer".to_owned(),
        }
    }

    #[test]
    fn pkce_pair_has_correct_lengths() {
        let (verifier, challenge) = generate_pkce_pair();
        assert_eq!(verifier.len(), PKCE_VERIFIER_LEN);
        // SHA-256 = 32 bytes → base64url without padding = 43 chars.
        assert_eq!(challenge.len(), 43);
    }

    #[test]
    fn pkce_verifier_uses_valid_charset() {
        let (verifier, _) = generate_pkce_pair();
        for c in verifier.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' || c == '~',
                "invalid character in verifier: {c}"
            );
        }
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let (verifier, challenge) = generate_pkce_pair();
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(challenge, expected);
    }

    #[test]
    fn authorization_code_exchange_binds_exact_pkce_verifier() {
        let parameters = authorization_code_parameters(
            "crab-cli",
            "authorization-code",
            "http://127.0.0.1/callback",
            "exact-generated-verifier",
        );

        assert_eq!(
            parameters.last(),
            Some(&("code_verifier", "exact-generated-verifier"))
        );
    }

    #[test]
    fn pkce_pairs_are_unique() {
        let (v1, _) = generate_pkce_pair();
        let (v2, _) = generate_pkce_pair();
        assert_ne!(v1, v2);
    }

    #[test]
    fn state_is_nonempty_and_unique() {
        let s1 = generate_state();
        let s2 = generate_state();
        assert!(!s1.is_empty());
        assert_ne!(s1, s2);
    }

    #[test]
    fn urlencoded_encodes_special_chars() {
        assert_eq!(urlencoded("hello world"), "hello+world");
        assert_eq!(urlencoded("a=b&c=d"), "a%3Db%26c%3Dd");
    }

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
        let result = serde_json::from_str::<OidcDiscovery>(json);
        assert!(result.is_err());
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
        // Real IdP discovery documents contain many fields we don't use
        // (jwks_uri, response_types_supported, etc.). Serde should
        // silently ignore them since we don't use deny_unknown_fields.
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
    fn token_error_response_parses_with_description() {
        let json = r#"{
            "error": "invalid_grant",
            "error_description": "The refresh token has expired"
        }"#;
        let err: TokenErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(err.error, "invalid_grant");
        assert_eq!(
            err.error_description.as_deref(),
            Some("The refresh token has expired")
        );
    }

    #[test]
    fn token_error_response_parses_without_description() {
        let json = r#"{ "error": "authorization_pending" }"#;
        let err: TokenErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(err.error, "authorization_pending");
        assert!(err.error_description.is_none());
    }

    #[test]
    fn token_error_response_parses_slow_down() {
        let json = r#"{ "error": "slow_down", "error_description": "polling too fast" }"#;
        let err: TokenErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(err.error, "slow_down");
    }

    #[test]
    fn device_auth_response_uses_default_interval() {
        let json = r#"{
            "device_code": "dc_abc",
            "user_code": "ABCD-1234",
            "verification_uri": "https://idp.example.com/device",
            "expires_in": 600
        }"#;
        let resp: DeviceAuthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.device_code, "dc_abc");
        assert_eq!(resp.user_code, "ABCD-1234");
        assert_eq!(resp.interval, 5); // default_interval()
    }

    #[test]
    fn device_auth_response_respects_custom_interval() {
        let json = r#"{
            "device_code": "dc_abc",
            "user_code": "ABCD-1234",
            "verification_uri": "https://idp.example.com/device",
            "interval": 10,
            "expires_in": 600
        }"#;
        let resp: DeviceAuthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.interval, 10);
    }

    #[test]
    fn state_has_expected_base64url_length() {
        // 32 random bytes → base64url without padding = 43 chars.
        let state = generate_state();
        assert_eq!(state.len(), 43);
    }

    #[tokio::test]
    async fn callback_rejects_mismatched_pkce_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let callback = tokio::spawn(async move { wait_for_callback(listener, "expected").await });
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            b"GET /callback?code=code&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .unwrap();

        let error = callback.await.unwrap().unwrap_err();

        assert!(error.to_string().contains("state mismatch"));
    }

    #[tokio::test]
    async fn device_polling_honors_interval_and_slow_down() {
        let poller = MockPoller {
            waits: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([
                TokenPollResponse::OAuthError(TokenErrorResponse {
                    error: "authorization_pending".to_owned(),
                    error_description: None,
                }),
                TokenPollResponse::OAuthError(TokenErrorResponse {
                    error: "slow_down".to_owned(),
                    error_description: None,
                }),
                TokenPollResponse::Tokens(tokens()),
            ])),
        };

        let result =
            poll_device_tokens(&poller, &oidc_discovery(), "crab-cli", &device_auth(3, 30))
                .await
                .unwrap();

        assert_eq!(result.access_token, "access");
        assert_eq!(
            *poller.waits.lock().unwrap(),
            [
                Duration::from_secs(3),
                Duration::from_secs(3),
                Duration::from_secs(8)
            ]
        );
    }

    #[tokio::test]
    async fn device_polling_stops_at_issuer_expiry() {
        let poller = MockPoller {
            waits: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([
                TokenPollResponse::OAuthError(TokenErrorResponse {
                    error: "authorization_pending".to_owned(),
                    error_description: None,
                }),
                TokenPollResponse::Tokens(tokens()),
            ])),
        };

        let error = poll_device_tokens(&poller, &oidc_discovery(), "crab-cli", &device_auth(3, 5))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("expired"));
        assert_eq!(*poller.waits.lock().unwrap(), [Duration::from_secs(3)]);
    }

    #[tokio::test]
    async fn device_polling_surfaces_access_denial_without_retry() {
        let poller = MockPoller {
            waits: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([TokenPollResponse::OAuthError(
                TokenErrorResponse {
                    error: "access_denied".to_owned(),
                    error_description: Some("user denied access".to_owned()),
                },
            )])),
        };

        let error = poll_device_tokens(&poller, &oidc_discovery(), "crab-cli", &device_auth(2, 30))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("user denied access"));
        assert_eq!(poller.waits.lock().unwrap().len(), 1);
    }

    #[test]
    fn urlencoded_handles_empty_string() {
        assert_eq!(urlencoded(""), "");
    }

    #[test]
    fn urlencoded_preserves_most_unreserved_chars() {
        // form_urlencoded encodes per application/x-www-form-urlencoded,
        // which preserves alphanumerics, '-', '_', '.', '*' but encodes '~'.
        assert_eq!(urlencoded("abc-._123"), "abc-._123");
    }
}
