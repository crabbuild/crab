//! Cross-platform credential retrieval from the OS keychain.
//!
//! Mirrors the storage logic in `electron/keychain.ts` so the agent
//! can read credentials that the renderer stored via
//! `window.crab.keychain.set()`. This avoids a round-trip to the
//! main process and works even when the agent is running standalone.
//!
//! Platform backends:
//! - macOS: Keychain Services via `security find-generic-password`
//! - Linux: libsecret via `secret-tool lookup`
//! - Windows: PowerShell `Get-StoredCredential` (DPAPI)
//!
//! The default service name is `"crab-desktop"` — matching the
//! `DEFAULT_SERVICE` constant in `electron/keychain.ts`.

use std::process::Stdio;

use tokio::process::Command;
use tracing::{debug, warn};

/// The service name used by the simplified keychain API in the renderer.
const DEFAULT_SERVICE: &str = "crab-desktop";

/// Retrieve a credential from the OS keychain.
///
/// Uses the same service/account convention as `electron/keychain.ts`:
/// - Service: `"crab-desktop"`
/// - Account: the key (e.g. `"crab-storage.my-bucket.aws.access_key_id"`)
///
/// Returns `None` if the credential doesn't exist or retrieval fails.
pub async fn get_credential(key: &str) -> Option<String> {
    get_password(DEFAULT_SERVICE, key).await
}

/// Retrieve a credential using the full service + account pair.
pub async fn get_password(service: &str, account: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        macos_get(service, account).await
    }
    #[cfg(target_os = "linux")]
    {
        linux_get(service, account).await
    }
    #[cfg(target_os = "windows")]
    {
        windows_get(service, account).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (service, account);
        warn!("credential store not supported on this platform");
        None
    }
}

/// Store a credential in the OS keychain.
///
/// Used by `storage_config_set` to persist credentials from the agent
/// side (when the renderer passes them via RPC).
pub async fn set_credential(key: &str, value: &str) -> bool {
    set_password(DEFAULT_SERVICE, key, value).await
}

/// Store a credential using the full service + account pair.
pub async fn set_password(service: &str, account: &str, password: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_set(service, account, password).await
    }
    #[cfg(target_os = "linux")]
    {
        linux_set(service, account, password).await
    }
    #[cfg(target_os = "windows")]
    {
        windows_set(service, account, password).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (service, account, password);
        warn!("credential store not supported on this platform");
        false
    }
}

// ─── macOS: Keychain Services via `security` CLI ────────────────────

#[cfg(target_os = "macos")]
async fn macos_get(service: &str, account: &str) -> Option<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        debug!(service, account, "keychain entry not found (macOS)");
        return None;
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(target_os = "macos")]
async fn macos_set(service: &str, account: &str, password: &str) -> bool {
    // Delete existing entry first (security add-generic-password fails if it exists).
    let _ = Command::new("security")
        .args(["delete-generic-password", "-s", service, "-a", account])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;

    let output = Command::new("security")
        .args([
            "add-generic-password",
            "-s",
            service,
            "-a",
            account,
            "-w",
            password,
            "-U", // update if exists
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await;

    match output {
        Ok(o) => o.status.success(),
        Err(e) => {
            warn!(error = %e, "failed to store credential in macOS keychain");
            false
        }
    }
}

// ─── Linux: libsecret via `secret-tool` ─────────────────────────────

#[cfg(target_os = "linux")]
async fn linux_get(service: &str, account: &str) -> Option<String> {
    let output = Command::new("secret-tool")
        .args(["lookup", "service", service, "account", account])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        debug!(service, account, "keychain entry not found (Linux)");
        return None;
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(target_os = "linux")]
async fn linux_set(service: &str, account: &str, password: &str) -> bool {
    use tokio::io::AsyncWriteExt;

    // secret-tool reads the secret from stdin.
    let mut child = match Command::new("secret-tool")
        .args([
            "store",
            "--label",
            &format!("{service} - {account}"),
            "service",
            service,
            "account",
            account,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "failed to spawn secret-tool");
            return false;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(password.as_bytes()).await;
        drop(stdin);
    }

    match child.wait().await {
        Ok(status) => status.success(),
        Err(e) => {
            warn!(error = %e, "secret-tool store failed");
            false
        }
    }
}

// ─── Windows: DPAPI / Credential Manager ────────────────────────────

#[cfg(target_os = "windows")]
async fn windows_get(service: &str, account: &str) -> Option<String> {
    let target = format!("{service}/{account}");

    // Use PowerShell to retrieve the credential via DPAPI.
    let script = format!(
        "$cred = Get-StoredCredential -Target '{}' -ErrorAction SilentlyContinue; \
         if ($cred) {{ $cred.GetNetworkCredential().Password }} else {{ '' }}",
        target.replace('\'', "''")
    );

    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        // Fallback: try cmdkey /list to check existence, then use
        // a different retrieval method.
        debug!(service, account, "credential not found via PowerShell");
        return None;
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(target_os = "windows")]
async fn windows_set(service: &str, account: &str, password: &str) -> bool {
    let target = format!("{service}/{account}");

    // Use cmdkey to store the credential.
    let output = Command::new("cmdkey")
        .args([
            &format!("/generic:{}", target),
            &format!("/user:{}", account),
            &format!("/pass:{}", password),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await;

    match output {
        Ok(o) => o.status.success(),
        Err(e) => {
            warn!(error = %e, "failed to store credential via cmdkey");
            false
        }
    }
}

// ─── Storage credential resolution ──────────────────────────────────

/// Key prefix used by the renderer when storing storage credentials.
/// Matches the convention in `CloneDialog.tsx`:
///   `crab-storage.<bucket>.<provider>.<field>`
const STORAGE_KEY_PREFIX: &str = "crab-storage";

/// Resolved storage credentials ready to be injected as environment
/// variables into a `crab` subprocess.
#[derive(Debug, Default)]
pub struct StorageCredentials {
    pub env_vars: Vec<(String, String)>,
}

/// Resolve storage credentials for a given work directory.
///
/// Reads `.crab/config.toml` to determine the provider and bucket,
/// then looks up the corresponding credentials in the OS keychain.
/// Returns environment variables that should be injected into the
/// `crab` subprocess.
///
/// Credential lookup order:
/// 1. OS keychain (stored by the desktop UI)
/// 2. Existing environment variables (already set by the user)
///
/// If credentials are already present in the environment, they take
/// precedence (the user may have set them in `.env` or their shell).
pub async fn resolve_storage_credentials(work_dir: &std::path::Path) -> StorageCredentials {
    let config_path = work_dir.join(".crab").join("config.toml");
    let mut creds = StorageCredentials::default();

    // Read config.toml to determine provider and bucket.
    let config_content = match tokio::fs::read_to_string(&config_path).await {
        Ok(c) => c,
        Err(_) => return creds, // No config — nothing to resolve.
    };

    // Parse the remote URL to extract provider and bucket.
    let remote_url = config_content
        .lines()
        .find(|l| l.starts_with("url"))
        .and_then(|l| l.split('=').nth(1))
        .map(|v| v.trim().trim_matches('"').to_owned());

    let Some(url) = remote_url else {
        return creds;
    };

    let (provider, bucket) = if url.starts_with("crab://") {
        ("aws", url.strip_prefix("crab://").unwrap_or(""))
    } else if url.starts_with("gs://") {
        ("gcp", url.strip_prefix("gs://").unwrap_or(""))
    } else if url.starts_with("az://") {
        ("azure", url.strip_prefix("az://").unwrap_or(""))
    } else {
        return creds; // Not an object-store remote.
    };

    // Extract just the bucket name (strip any prefix path).
    let bucket = bucket.split('/').next().unwrap_or(bucket);
    let key_prefix = format!("{STORAGE_KEY_PREFIX}.{bucket}");

    match provider {
        "aws" => {
            resolve_aws_credentials(&key_prefix, &mut creds).await;
        }
        "gcp" => {
            resolve_gcp_credentials(&key_prefix, &mut creds).await;
        }
        "azure" => {
            resolve_azure_credentials(&key_prefix, &mut creds).await;
        }
        _ => {}
    }

    creds
}

async fn resolve_aws_credentials(key_prefix: &str, creds: &mut StorageCredentials) {
    // Skip if already set in environment.
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() {
        debug!("AWS credentials already in environment, skipping keychain lookup");
        return;
    }

    let access_key = get_credential(&format!("{key_prefix}.aws.access_key_id")).await;
    let secret_key = get_credential(&format!("{key_prefix}.aws.secret_access_key")).await;
    let session_token = get_credential(&format!("{key_prefix}.aws.session_token")).await;
    let region = get_credential(&format!("{key_prefix}.aws.region")).await;

    if let Some(ak) = access_key {
        creds.env_vars.push(("AWS_ACCESS_KEY_ID".into(), ak));
    }
    if let Some(sk) = secret_key {
        creds.env_vars.push(("AWS_SECRET_ACCESS_KEY".into(), sk));
    }
    if let Some(st) = session_token {
        creds.env_vars.push(("AWS_SESSION_TOKEN".into(), st));
    }
    if let Some(r) = region {
        creds
            .env_vars
            .push(("AWS_DEFAULT_REGION".into(), r.clone()));
        creds.env_vars.push(("AWS_REGION".into(), r));
    }
}

async fn resolve_gcp_credentials(key_prefix: &str, creds: &mut StorageCredentials) {
    // Skip if already set in environment.
    if std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_ok() {
        debug!("GCP credentials already in environment, skipping keychain lookup");
        return;
    }

    let service_account_key =
        get_credential(&format!("{key_prefix}.gcp.service_account_key")).await;

    if let Some(key_json) = service_account_key {
        // Write the service account JSON to a temp file and set
        // GOOGLE_APPLICATION_CREDENTIALS to point at it. The file is
        // cleaned up by the OS temp directory lifecycle.
        let tmp_dir = std::env::temp_dir().join("crab-gcp-creds");
        let _ = tokio::fs::create_dir_all(&tmp_dir).await;
        let cred_file = tmp_dir.join(format!(
            "sa-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        if tokio::fs::write(&cred_file, &key_json).await.is_ok() {
            creds.env_vars.push((
                "GOOGLE_APPLICATION_CREDENTIALS".into(),
                cred_file.display().to_string(),
            ));
        }
    }

    let project_id = get_credential(&format!("{key_prefix}.gcp.project_id")).await;
    if let Some(pid) = project_id {
        creds.env_vars.push(("GCLOUD_PROJECT".into(), pid.clone()));
        creds.env_vars.push(("GCP_PROJECT".into(), pid));
    }
}

async fn resolve_azure_credentials(key_prefix: &str, creds: &mut StorageCredentials) {
    // Skip if already set in environment.
    if std::env::var("AZURE_STORAGE_CONNECTION_STRING").is_ok()
        || std::env::var("AZURE_STORAGE_ACCESS_KEY").is_ok()
    {
        debug!("Azure credentials already in environment, skipping keychain lookup");
        return;
    }

    let connection_string = get_credential(&format!("{key_prefix}.azure.connection_string")).await;
    let storage_account = get_credential(&format!("{key_prefix}.azure.storage_account")).await;
    let storage_key = get_credential(&format!("{key_prefix}.azure.storage_key")).await;

    if let Some(cs) = connection_string {
        creds
            .env_vars
            .push(("AZURE_STORAGE_CONNECTION_STRING".into(), cs));
    } else {
        // Fall back to account + key pair.
        if let Some(acct) = storage_account {
            creds.env_vars.push(("AZURE_STORAGE_ACCOUNT".into(), acct));
        }
        if let Some(key) = storage_key {
            creds
                .env_vars
                .push(("AZURE_STORAGE_ACCESS_KEY".into(), key));
        }
    }
}
