//! Forge provider detection from remote URL + manual override.
//!
//! On `open_local`, the agent reads `git config --get remote.origin.url`
//! and maps the hostname to a [`Provider`]. Self-hosted GitLab and Gitea
//! instances are probed via their version endpoints. A manual override
//! in workspace settings (`forge_provider` field in
//! `.crab/desktop/settings.json`) takes precedence over auto-detection.

use std::path::Path;

use reqwest::Client;
use serde::Deserialize;
use tracing::{debug, warn};

use super::Provider;

/// Result of provider detection. `None` means the forge could not be
/// identified (features disabled for this repo).
pub type DetectedProvider = Option<Provider>;

/// Full detection result including provider, owner, and repo name.
#[derive(Debug, Clone)]
pub struct DetectedForge {
    pub provider: Provider,
    pub owner: String,
    pub name: String,
}

/// Detect the forge provider for a repository at `work_dir`.
///
/// Resolution order:
/// 1. Manual override from workspace settings (`forge_provider` field).
/// 2. Auto-detection from the remote origin URL hostname.
/// 3. Probe self-hosted GitLab (`/api/v4/version`) and Gitea
///    (`/api/v1/version`) for unknown hosts.
///
/// Returns `None` when the provider cannot be determined.
pub async fn detect_provider(work_dir: &Path) -> DetectedProvider {
    // 1. Check manual override first.
    if let Some(provider) = read_manual_override(work_dir) {
        debug!(provider = ?provider, "forge provider from manual override");
        return Some(provider);
    }

    // 2. Read the remote origin URL.
    let remote_url = match read_remote_origin_url(work_dir).await {
        Some(url) => url,
        None => {
            debug!("no remote.origin.url found; forge features disabled");
            return None;
        }
    };

    // 3. Detect from URL patterns.
    detect_from_url(&remote_url).await
}

/// Detect the forge provider, owner, and repo name for a repository.
///
/// Like [`detect_provider`] but also extracts the owner/name from the
/// remote URL. Returns `None` when the provider cannot be determined.
/// Persists the result to `.crab/desktop/settings.json` so that
/// subsequent RPC calls (e.g. `forge_list_prs`) can read it back.
pub async fn detect_and_persist(work_dir: &Path) -> Option<DetectedForge> {
    let remote_url = read_remote_origin_url(work_dir).await?;

    // Check manual override first.
    let provider = if let Some(p) = read_manual_override(work_dir) {
        p
    } else {
        detect_from_url(&remote_url).await?
    };

    // Extract owner/name from the URL.
    let (owner, name) = extract_owner_name(&remote_url)?;

    let result = DetectedForge {
        provider,
        owner: owner.clone(),
        name: name.clone(),
    };

    // Persist to settings so forge RPCs can read it back.
    persist_forge_settings(work_dir, &result);

    Some(result)
}

/// Detect the provider from a remote URL string.
///
/// Parses the URL (supporting both HTTPS and SSH formats) and matches
/// the hostname against known forge domains. For unknown hosts, probes
/// the GitLab and Gitea version endpoints.
pub async fn detect_from_url(remote_url: &str) -> DetectedProvider {
    let host = extract_host(remote_url)?;

    // Match well-known hosts.
    if let Some(provider) = match_known_host(&host) {
        debug!(host = %host, provider = ?provider, "matched known forge host");
        return Some(provider);
    }

    // Unknown host — probe for self-hosted instances.
    debug!(host = %host, "probing unknown host for self-hosted forge");
    probe_self_hosted(&host).await
}

/// Extract the hostname from a remote URL.
///
/// Handles common formats:
/// - `https://github.com/owner/repo.git`
/// - `git@github.com:owner/repo.git`
/// - `ssh://git@github.com/owner/repo.git`
/// - `github.com:owner/repo` (SCP-like without user)
fn extract_host(url: &str) -> Option<String> {
    // Windows drive and UNC prefixes name local Git sources. Treating a drive
    // as an SSH host makes repository open wait for unrelated network probes.
    if matches!(
        Path::new(url).components().next(),
        Some(std::path::Component::Prefix(_))
    ) {
        return None;
    }
    if url.contains("://") {
        let parsed = url::Url::parse(url).ok()?;
        if !matches!(parsed.scheme(), "https" | "http" | "ssh" | "git") {
            return None;
        }
        return parsed.host_str().map(str::to_lowercase);
    }

    // Handle SCP-like syntax: [user@]host:path
    if let Some((before_colon, path)) = url.split_once(':') {
        // Make sure it's not a scheme (no slashes before the colon).
        if !before_colon.contains('/') && !path.starts_with(':') {
            // Strip optional user@ prefix.
            let host_part = if let Some(at_pos) = before_colon.rfind('@') {
                &before_colon[at_pos + 1..]
            } else {
                before_colon
            };
            if !host_part.is_empty() {
                return Some(host_part.to_lowercase());
            }
        }
    }

    None
}

/// Match a hostname against well-known forge domains.
fn match_known_host(host: &str) -> Option<Provider> {
    // GitHub: github.com or *.github.com (e.g. ghe.company.com is NOT
    // matched here — that's GitHub Enterprise with a custom domain and
    // would need manual override or a probe).
    if host == "github.com" || host.ends_with(".github.com") {
        return Some(Provider::GitHub);
    }

    // GitLab: gitlab.com
    if host == "gitlab.com" {
        return Some(Provider::GitLab);
    }

    // Bitbucket: bitbucket.org
    if host == "bitbucket.org" {
        return Some(Provider::Bitbucket);
    }

    None
}

/// Extract the owner and repository name from a remote URL.
///
/// Handles common formats:
/// - `https://github.com/owner/repo.git` → ("owner", "repo")
/// - `git@github.com:owner/repo.git` → ("owner", "repo")
/// - `ssh://git@github.com/owner/repo.git` → ("owner", "repo")
/// - `github.com:owner/repo` → ("owner", "repo")
///
/// Strips `.git` suffix from the repo name if present.
fn extract_owner_name(url: &str) -> Option<(String, String)> {
    // Extract the path portion after the host.
    let path = if let Ok(parsed) = url::Url::parse(url) {
        // Standard URL: path starts with /
        parsed.path().trim_start_matches('/').to_owned()
    } else if let Some(colon_pos) = url.find(':') {
        // SCP-like: everything after the colon
        let before_colon = &url[..colon_pos];
        if !before_colon.contains('/') {
            url[colon_pos + 1..].to_owned()
        } else {
            return None;
        }
    } else {
        return None;
    };

    // Split path into segments, filtering empty ones.
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if segments.len() < 2 {
        return None;
    }

    let owner = segments[0].to_owned();
    // For GitLab subgroups, the last segment is the repo name and
    // everything before it is the "owner" (group path). For simplicity,
    // we take the first segment as owner and the second as name — this
    // covers GitHub, Bitbucket, and simple GitLab layouts.
    let mut name = segments[1].to_owned();

    // Strip .git suffix.
    if name.ends_with(".git") {
        name.truncate(name.len() - 4);
    }

    if owner.is_empty() || name.is_empty() {
        return None;
    }

    Some((owner, name))
}

/// Persist detected forge info to `.crab/desktop/settings.json`.
///
/// Merges with existing settings (preserving token, theme, etc.).
/// Best-effort: failures are logged but don't block repo open.
fn persist_forge_settings(work_dir: &Path, forge: &DetectedForge) {
    let settings_dir = work_dir.join(".crab").join("desktop");
    let settings_path = settings_dir.join("settings.json");

    // Read existing settings (if any) to merge.
    let mut settings: serde_json::Map<String, serde_json::Value> =
        match std::fs::read_to_string(&settings_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => serde_json::Map::new(),
        };

    // Write provider, owner, name — but don't overwrite an existing token.
    settings.insert(
        "forge_provider".to_owned(),
        serde_json::to_value(&forge.provider).unwrap_or_default(),
    );
    settings.insert(
        "forge_owner".to_owned(),
        serde_json::Value::String(forge.owner.clone()),
    );
    settings.insert(
        "forge_name".to_owned(),
        serde_json::Value::String(forge.name.clone()),
    );

    // Ensure directory exists.
    if let Err(e) = std::fs::create_dir_all(&settings_dir) {
        warn!(error = %e, "failed to create .crab/desktop/ directory for forge settings");
        return;
    }

    match serde_json::to_string_pretty(&settings) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&settings_path, json) {
                warn!(error = %e, "failed to write forge settings to disk");
            } else {
                debug!(
                    provider = ?forge.provider,
                    owner = %forge.owner,
                    name = %forge.name,
                    "persisted forge settings"
                );
            }
        }
        Err(e) => {
            warn!(error = %e, "failed to serialize forge settings");
        }
    }
}

/// Probe a self-hosted instance for GitLab or Gitea version endpoints.
///
/// Tries GitLab first (`/api/v4/version`), then Gitea (`/api/v1/version`).
/// Uses a short timeout to avoid blocking the open flow.
async fn probe_self_hosted(host: &str) -> DetectedProvider {
    let client = match Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "failed to build HTTP client for forge probe");
            return None;
        }
    };

    // Probe GitLab: GET https://<host>/api/v4/version
    // A successful response (even 401 with the right structure) indicates GitLab.
    let gitlab_url = format!("https://{host}/api/v4/version");
    match client.get(&gitlab_url).send().await {
        Ok(resp) => {
            // GitLab returns 200 with {"version": "...", "revision": "..."}
            // when unauthenticated access is allowed, or 401 when it's not.
            // Either way, the endpoint existing is a strong signal.
            let status = resp.status();
            if status.is_success() {
                // Verify it looks like a GitLab response.
                if let Ok(body) = resp.json::<GitLabVersionResponse>().await {
                    if !body.version.is_empty() {
                        debug!(host = %host, version = %body.version, "detected GitLab");
                        return Some(Provider::GitLab);
                    }
                }
            } else if status.as_u16() == 401 {
                // 401 on /api/v4/version is still a strong GitLab signal —
                // the endpoint exists but requires auth.
                debug!(host = %host, "detected GitLab (401 on version endpoint)");
                return Some(Provider::GitLab);
            }
        }
        Err(e) => {
            debug!(host = %host, error = %e, "GitLab probe failed");
        }
    }

    // Probe Gitea: GET https://<host>/api/v1/version
    let gitea_url = format!("https://{host}/api/v1/version");
    match client.get(&gitea_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(body) = resp.json::<GiteaVersionResponse>().await {
                if !body.version.is_empty() {
                    debug!(host = %host, version = %body.version, "detected Gitea");
                    return Some(Provider::Gitea);
                }
            }
        }
        Ok(_) => {
            debug!(host = %host, "Gitea probe returned non-success");
        }
        Err(e) => {
            debug!(host = %host, error = %e, "Gitea probe failed");
        }
    }

    debug!(host = %host, "could not detect forge provider");
    None
}

/// Read the manual `forge_provider` override from workspace settings.
///
/// Settings file: `<work_dir>/.crab/desktop/settings.json`.
/// Expected field: `"forge_provider": "github" | "gitlab" | "bitbucket" | "gitea"`
fn read_manual_override(work_dir: &Path) -> Option<Provider> {
    let settings_path = work_dir.join(".crab").join("desktop").join("settings.json");

    let content = std::fs::read_to_string(&settings_path).ok()?;
    let settings: WorkspaceSettings = serde_json::from_str(&content).ok()?;

    settings.forge_provider
}

/// Read `remote.origin.url` from the git config of the repository.
async fn read_remote_origin_url(work_dir: &Path) -> Option<String> {
    let output = crate::shellout::git_command()
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(work_dir)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if url.is_empty() { None } else { Some(url) }
}

// --- Internal types ---

/// Subset of workspace settings relevant to forge detection.
#[derive(Debug, Deserialize)]
struct WorkspaceSettings {
    #[serde(default)]
    forge_provider: Option<Provider>,
}

/// GitLab version endpoint response.
#[derive(Debug, Deserialize)]
struct GitLabVersionResponse {
    #[serde(default)]
    version: String,
}

/// Gitea version endpoint response.
#[derive(Debug, Deserialize)]
struct GiteaVersionResponse {
    #[serde(default)]
    version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_host_https() {
        assert_eq!(
            extract_host("https://github.com/owner/repo.git"),
            Some("github.com".to_owned())
        );
    }

    #[test]
    fn extract_host_ssh_scp() {
        assert_eq!(
            extract_host("git@github.com:owner/repo.git"),
            Some("github.com".to_owned())
        );
    }

    #[test]
    fn extract_host_ssh_url() {
        assert_eq!(
            extract_host("ssh://git@gitlab.com/owner/repo.git"),
            Some("gitlab.com".to_owned())
        );
    }

    #[test]
    fn extract_host_no_user_scp() {
        assert_eq!(
            extract_host("bitbucket.org:owner/repo.git"),
            Some("bitbucket.org".to_owned())
        );
    }

    #[test]
    fn extract_host_case_insensitive() {
        assert_eq!(
            extract_host("https://GitHub.COM/owner/repo"),
            Some("github.com".to_owned())
        );
    }

    #[test]
    fn extract_host_empty_returns_none() {
        assert_eq!(extract_host(""), None);
    }

    #[test]
    fn local_and_object_store_remotes_have_no_forge_host() {
        for remote in [
            "/tmp/seed",
            "../seed",
            "./seed:backup",
            "file:///tmp/seed",
            "file://server/share/seed",
            "crab://bucket/repository",
            "s3://bucket/repository",
            "gs://bucket/repository",
            "az://account/container/repository",
            "helper::repository",
        ] {
            assert_eq!(extract_host(remote), None, "{remote}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_local_remotes_have_no_forge_host() {
        for remote in [
            "C:/fixtures/seed",
            r"C:\fixtures\seed",
            "D:seed",
            r"\\server\share\seed",
            r"\\?\C:\fixtures\seed",
            r"\\?\UNC\server\share\seed",
        ] {
            assert_eq!(extract_host(remote), None, "{remote}");
        }
    }

    #[test]
    fn network_remotes_preserve_forge_hosts() {
        for remote in [
            "https://git.example.com/team/repository",
            "ssh://git@git.example.com:2222/team/repository",
            "git://git.example.com/team/repository",
            "git@git.example.com:team/repository",
            "git.example.com:team/repository",
        ] {
            assert_eq!(extract_host(remote).as_deref(), Some("git.example.com"));
        }
    }

    #[test]
    fn match_known_github() {
        assert_eq!(match_known_host("github.com"), Some(Provider::GitHub));
    }

    #[test]
    fn match_known_github_subdomain() {
        assert_eq!(
            match_known_host("enterprise.github.com"),
            Some(Provider::GitHub)
        );
    }

    #[test]
    fn match_known_gitlab() {
        assert_eq!(match_known_host("gitlab.com"), Some(Provider::GitLab));
    }

    #[test]
    fn match_known_bitbucket() {
        assert_eq!(match_known_host("bitbucket.org"), Some(Provider::Bitbucket));
    }

    #[test]
    fn match_known_unknown_host() {
        assert_eq!(match_known_host("my-company.example.com"), None);
    }

    #[test]
    fn detect_from_url_github_https() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(detect_from_url("https://github.com/owner/repo.git"));
        assert_eq!(result, Some(Provider::GitHub));
    }

    #[test]
    fn detect_from_url_github_ssh() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(detect_from_url("git@github.com:owner/repo.git"));
        assert_eq!(result, Some(Provider::GitHub));
    }

    #[test]
    fn detect_from_url_gitlab_https() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(detect_from_url("https://gitlab.com/group/project.git"));
        assert_eq!(result, Some(Provider::GitLab));
    }

    #[test]
    fn detect_from_url_bitbucket_ssh() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(detect_from_url("git@bitbucket.org:team/repo.git"));
        assert_eq!(result, Some(Provider::Bitbucket));
    }

    #[test]
    fn detect_from_url_empty() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(detect_from_url(""));
        assert_eq!(result, None);
    }

    #[test]
    fn manual_override_reads_settings() {
        let dir = tempfile::tempdir().unwrap();
        let settings_dir = dir.path().join(".crab").join("desktop");
        std::fs::create_dir_all(&settings_dir).unwrap();
        std::fs::write(
            settings_dir.join("settings.json"),
            r#"{"forge_provider": "gitea"}"#,
        )
        .unwrap();

        let result = read_manual_override(dir.path());
        assert_eq!(result, Some(Provider::Gitea));
    }

    #[test]
    fn manual_override_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_manual_override(dir.path());
        assert_eq!(result, None);
    }

    #[test]
    fn manual_override_no_field() {
        let dir = tempfile::tempdir().unwrap();
        let settings_dir = dir.path().join(".crab").join("desktop");
        std::fs::create_dir_all(&settings_dir).unwrap();
        std::fs::write(settings_dir.join("settings.json"), r#"{"theme": "dark"}"#).unwrap();

        let result = read_manual_override(dir.path());
        assert_eq!(result, None);
    }

    // --- extract_owner_name tests ---

    #[test]
    fn extract_owner_name_https() {
        assert_eq!(
            extract_owner_name("https://github.com/chrislu/seaweedfs.git"),
            Some(("chrislu".to_owned(), "seaweedfs".to_owned()))
        );
    }

    #[test]
    fn extract_owner_name_ssh_scp() {
        assert_eq!(
            extract_owner_name("git@github.com:octocat/hello-world.git"),
            Some(("octocat".to_owned(), "hello-world".to_owned()))
        );
    }

    #[test]
    fn extract_owner_name_ssh_url() {
        assert_eq!(
            extract_owner_name("ssh://git@gitlab.com/group/project.git"),
            Some(("group".to_owned(), "project".to_owned()))
        );
    }

    #[test]
    fn extract_owner_name_no_git_suffix() {
        assert_eq!(
            extract_owner_name("https://github.com/owner/repo"),
            Some(("owner".to_owned(), "repo".to_owned()))
        );
    }

    #[test]
    fn extract_owner_name_empty() {
        assert_eq!(extract_owner_name(""), None);
    }

    #[test]
    fn extract_owner_name_no_path() {
        assert_eq!(extract_owner_name("https://github.com/"), None);
    }

    // --- persist_forge_settings tests ---

    #[test]
    fn persist_creates_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let forge = DetectedForge {
            provider: Provider::GitHub,
            owner: "octocat".to_owned(),
            name: "hello-world".to_owned(),
        };

        persist_forge_settings(dir.path(), &forge);

        let settings_path = dir
            .path()
            .join(".crab")
            .join("desktop")
            .join("settings.json");
        let content = std::fs::read_to_string(&settings_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["forge_provider"], "github");
        assert_eq!(parsed["forge_owner"], "octocat");
        assert_eq!(parsed["forge_name"], "hello-world");
    }

    #[test]
    fn persist_merges_with_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let settings_dir = dir.path().join(".crab").join("desktop");
        std::fs::create_dir_all(&settings_dir).unwrap();
        std::fs::write(
            settings_dir.join("settings.json"),
            r#"{"theme": "dark", "forge_token": "ghp_existing"}"#,
        )
        .unwrap();

        let forge = DetectedForge {
            provider: Provider::GitHub,
            owner: "octocat".to_owned(),
            name: "hello-world".to_owned(),
        };

        persist_forge_settings(dir.path(), &forge);

        let content = std::fs::read_to_string(settings_dir.join("settings.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        // New fields written.
        assert_eq!(parsed["forge_provider"], "github");
        assert_eq!(parsed["forge_owner"], "octocat");
        assert_eq!(parsed["forge_name"], "hello-world");
        // Existing fields preserved.
        assert_eq!(parsed["theme"], "dark");
        assert_eq!(parsed["forge_token"], "ghp_existing");
    }
}
