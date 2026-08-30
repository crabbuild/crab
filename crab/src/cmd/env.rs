//! `crab env` — print diagnostic environment information.
//!
//! Displays the crab version, git version, remote URL, storage backend,
//! git driver configuration, and relevant environment variables. Useful
//! for bug reports and troubleshooting.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::core::error::Result;
use crate::core::output::{OutputMode, emit_json};

/// Payload emitted by `crab env --json`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EnvPayload {
    pub crab_version: String,
    pub git_sha: String,
    pub build_timestamp: String,
    pub git_version: Option<String>,
    pub remote_url: Option<String>,
    pub platform: String,
}

/// Run `crab env` in the current working directory.
pub fn run_env(mode: OutputMode) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_env_in(&cwd, mode)
}

/// Print diagnostic environment information rooted at `root`.
pub fn run_env_in(root: &Path, mode: OutputMode) -> Result<()> {
    if mode == OutputMode::Json {
        let payload = collect_env_payload(root);
        emit_json("env", "1.0", payload);
        return Ok(());
    }

    print_crab_version();
    print_git_version();
    println!();
    print_remote_info(root);
    println!();
    print_filter_config(root);
    println!();
    print_storage_env();
    Ok(())
}

/// Collect all environment info into a structured payload.
fn collect_env_payload(root: &Path) -> EnvPayload {
    let git_version = Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    let remote_url = crate::core::project_config::ProjectConfig::load_for_repo(root)
        .ok()
        .flatten()
        .map(|config| config.remote.url)
        .filter(|url| !url.trim().is_empty());

    EnvPayload {
        crab_version: env!("CRAB_BUILD_VERSION").to_owned(),
        git_sha: env!("CRAB_BUILD_GIT_SHA").to_owned(),
        build_timestamp: env!("CRAB_BUILD_TIMESTAMP").to_owned(),
        git_version,
        remote_url,
        platform: current_platform(),
    }
}

/// Return a human-readable platform string (e.g. `"aarch64-apple-darwin"`).
fn current_platform() -> String {
    format!(
        "{}-{}-{}",
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
        std::env::consts::OS,
    )
}

fn print_crab_version() {
    println!(
        "crab version {} ({})",
        env!("CRAB_BUILD_VERSION"),
        env!("CRAB_BUILD_GIT_SHA"),
    );
}

fn print_git_version() {
    let version = Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "git not found".into());
    print!("{}", version.trim_end());
    println!();
}

fn print_remote_info(root: &Path) {
    match crate::core::project_config::ProjectConfig::load_for_repo(root) {
        Ok(Some(config)) => {
            let url = config.remote.url;
            println!("Remote={url}");
            match crate::git::url::CrabUrl::parse(&url) {
                Ok(parsed) => {
                    println!("  Bucket={}", parsed.bucket);
                    println!("  RepoPath={}", parsed.repo_path);
                }
                Err(e) => println!("  (parse error: {e})"),
            }
        }
        Ok(None) => println!("Remote=<not configured>"),
        Err(error) => println!("Remote=<invalid: {error}>"),
    }
}

fn print_filter_config(root: &Path) {
    let keys = [
        "filter.crab.process",
        "filter.crab.clean",
        "filter.crab.smudge",
        "filter.crab.required",
        "diff.crab.command",
    ];

    for key in &keys {
        let value = Command::new("git")
            .args(["config", "--local", key])
            .current_dir(root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();

        if value.is_empty() {
            println!("git config {key} = <not set>");
        } else {
            println!("git config {key} = {value:?}");
        }
    }
}

fn print_storage_env() {
    let env_vars = [
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_PROFILE",
        "AWS_ENDPOINT_URL",
        "CRAB_LOG",
        "CRAB_CACHE_DIR",
    ];

    println!("Environment:");
    for var in &env_vars {
        if let Ok(val) = std::env::var(var) {
            println!("  {var}={val}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_runs_without_crab_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Should not error even without .crab/ directory.
        let result = run_env_in(dir.path(), OutputMode::Text);
        assert!(result.is_ok());
    }

    #[test]
    fn collect_env_payload_populates_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        let payload = collect_env_payload(dir.path());
        assert!(!payload.crab_version.is_empty());
        assert!(!payload.git_sha.is_empty());
        assert!(!payload.build_timestamp.is_empty());
        assert!(!payload.platform.is_empty());
        // No crab.toml means the remote URL is unavailable.
        assert!(payload.remote_url.is_none());
    }

    #[test]
    fn collect_env_payload_reads_remote_url() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("crab.toml"),
            "[remote]\nurl = \"crab://bucket/repo\"\n",
        )
        .unwrap();

        let payload = collect_env_payload(dir.path());
        assert_eq!(payload.remote_url.as_deref(), Some("crab://bucket/repo"));
    }

    #[test]
    fn current_platform_is_non_empty() {
        let p = current_platform();
        assert!(!p.is_empty());
        // Should contain at least two dashes separating arch-family-os.
        assert!(p.matches('-').count() >= 2);
    }
}
