//! Forge-token retrieval from the OS keychain. Object-store credentials are
//! owned by the encrypted desktop connection store.

use std::process::Stdio;
use tokio::process::Command;
use tracing::debug;

/// Retrieve a forge token using the main process service/account convention.
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
        tracing::warn!("credential store not supported on this platform");
        None
    }
}

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
