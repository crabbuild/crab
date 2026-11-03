//! OAuth device flow initiation for forge providers.
//!
//! The agent's role is limited: it POSTs to the provider's device
//! authorization endpoint to obtain the device code, user code, and
//! verification URI, then emits an `auth/start` notification so the
//! Electron main process can open the browser and begin polling.
//!
//! The main process handles the actual token polling loop and keychain
//! storage, then notifies the agent via `auth/token_updated` when done.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::types::Provider;
use crate::{AgentError, Result, SharedStdout, notify};

/// Parameters for the `forge_auth_start` RPC verb.
#[derive(Debug, Deserialize)]
pub struct ForgeAuthStartParams {
    /// Which forge provider to authenticate with.
    pub provider: Provider,
    /// OAuth client ID. For GitHub this is the registered OAuth App's
    /// client ID. Passed from the renderer/settings.
    pub client_id: String,
    /// Optional scopes to request (space-separated for GitHub,
    /// comma-separated for GitLab).
    #[serde(default)]
    pub scopes: Option<String>,
}

/// Response from the device authorization endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    /// The device verification code (sent to the token endpoint).
    pub device_code: String,
    /// The code the user enters in the browser.
    pub user_code: String,
    /// The URL the user visits to authorize.
    pub verification_uri: String,
    /// How often to poll (seconds). Defaults to 5 if not provided.
    pub interval: u64,
    /// When the device code expires (seconds).
    pub expires_in: u64,
}

/// Initiate the OAuth device flow for a given provider.
///
/// POSTs to the provider's device authorization endpoint and returns
/// the device code response. Also emits an `auth/start` notification
/// so the main process can open the browser and start polling.
pub async fn forge_auth_start(params: ForgeAuthStartParams, out: SharedStdout) -> Result<Value> {
    let device_resp = request_device_code(&params).await?;

    // Emit the auth/start notification so the main process opens the
    // browser and begins the poll-for-token loop.
    let notification_payload = json!({
        "provider": params.provider,
        "client_id": params.client_id,
        "device_code": device_resp.device_code,
        "user_code": device_resp.user_code,
        "verification_uri": device_resp.verification_uri,
        "interval": device_resp.interval,
        "expires_in": device_resp.expires_in,
    });

    notify(&out, "auth/start", &notification_payload).await?;

    debug!(
        provider = ?params.provider,
        user_code = %device_resp.user_code,
        "device flow initiated"
    );

    // Return the device code info to the renderer so it can display
    // the user code to the user.
    Ok(json!({
        "device_code": device_resp.device_code,
        "user_code": device_resp.user_code,
        "verification_uri": device_resp.verification_uri,
        "interval": device_resp.interval,
        "expires_in": device_resp.expires_in,
    }))
}

/// POST to the provider's device authorization endpoint.
async fn request_device_code(params: &ForgeAuthStartParams) -> Result<DeviceCodeResponse> {
    let client = Client::new();

    match params.provider {
        Provider::GitHub => request_github_device_code(&client, params).await,
        Provider::GitLab => request_gitlab_device_code(&client, params).await,
        Provider::Bitbucket => request_bitbucket_device_code(&client, params).await,
        Provider::Gitea => {
            // Gitea doesn't support device flow — the renderer should
            // use PAT fallback for this provider.
            Err(AgentError::Other(
                "Gitea does not support OAuth device flow; use a personal access token instead"
                    .to_owned(),
            ))
        }
    }
}

/// GitHub device authorization: POST to
/// `https://github.com/login/device/code`
async fn request_github_device_code(
    client: &Client,
    params: &ForgeAuthStartParams,
) -> Result<DeviceCodeResponse> {
    let mut form = vec![("client_id", params.client_id.as_str())];
    let scopes = params.scopes.clone().unwrap_or_default();
    if !scopes.is_empty() {
        form.push(("scope", scopes.as_str()));
    }

    let resp = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| AgentError::Other(format!("GitHub device code request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!(%status, %body, "GitHub device code endpoint returned error");
        return Err(AgentError::Other(format!(
            "GitHub device code request failed with status {status}: {body}"
        )));
    }

    let body: GitHubDeviceCodeResponse = resp.json().await.map_err(|e| {
        AgentError::Other(format!("failed to parse GitHub device code response: {e}"))
    })?;

    Ok(DeviceCodeResponse {
        device_code: body.device_code,
        user_code: body.user_code,
        verification_uri: body.verification_uri,
        interval: body.interval.unwrap_or(5),
        expires_in: body.expires_in,
    })
}

/// GitLab device authorization: POST to
/// `https://gitlab.com/oauth/authorize_device`
async fn request_gitlab_device_code(
    client: &Client,
    params: &ForgeAuthStartParams,
) -> Result<DeviceCodeResponse> {
    let mut form = vec![("client_id", params.client_id.as_str())];
    let scopes = params.scopes.clone().unwrap_or_default();
    if !scopes.is_empty() {
        form.push(("scope", scopes.as_str()));
    }

    let resp = client
        .post("https://gitlab.com/oauth/authorize_device")
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| AgentError::Other(format!("GitLab device code request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!(%status, %body, "GitLab device code endpoint returned error");
        return Err(AgentError::Other(format!(
            "GitLab device code request failed with status {status}: {body}"
        )));
    }

    let body: GitLabDeviceCodeResponse = resp.json().await.map_err(|e| {
        AgentError::Other(format!("failed to parse GitLab device code response: {e}"))
    })?;

    Ok(DeviceCodeResponse {
        device_code: body.device_code,
        user_code: body.user_code,
        verification_uri: body.verification_uri,
        interval: body.interval.unwrap_or(5),
        expires_in: body.expires_in,
    })
}

/// Bitbucket does not support RFC 8628 device authorization grants.
/// Authentication must use App Passwords or API Tokens (token-only flow).
async fn request_bitbucket_device_code(
    _client: &Client,
    _params: &ForgeAuthStartParams,
) -> Result<DeviceCodeResponse> {
    Err(AgentError::Other(
        "Bitbucket Cloud does not support the OAuth device flow. \
         Use an App Password or API Token instead."
            .to_string(),
    ))
}

// --- Provider-specific response shapes ---

#[derive(Debug, Deserialize)]
struct GitHubDeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct GitLabDeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: u64,
}
