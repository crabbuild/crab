//! `crab workflow push-cache` — backfill remote cache from local entries.
//!
//! Scans all local stage cache entries under `.crab/cache/stages/`
//! and pushes any that lack a corresponding remote ref. This is the
//! batch counterpart to `crab run --cache-push` which pushes
//! incrementally after each stage commits.

use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::git::url::CrabUrl;
use crate::workflow::WorkflowStore;
use crate::workflow::cache::{self, RemoteArtifactStores};

/// Structured-output schema label for `crab workflow push-cache`.
pub const WORKFLOW_PUSH_CACHE_SCHEMA: &str = "workflow.push_cache";

/// Clap args for `crab workflow push-cache`.
#[derive(Debug, Clone, Parser)]
pub struct PushCacheArgs {
    /// Push all local stage cache entries that are missing from the
    /// remote. Without this flag, only entries from the current run
    /// would be pushed (but that's handled by `--cache-push` on
    /// `crab run`).
    #[arg(long, default_value_t = false)]
    pub all: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl PushCacheArgs {
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Structured output payload for `push-cache`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PushCacheResult {
    pub pushed: u32,
    pub skipped: u32,
    pub errors: u32,
}

/// Execute `crab workflow push-cache`.
pub async fn exec_push_cache(args: PushCacheArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    exec_push_cache_in(&args, &cwd).await
}

/// Testable entry point.
pub async fn exec_push_cache_in(args: &PushCacheArgs, repo_root: &Path) -> Result<()> {
    let config = Config::resolve_local().unwrap_or_default();
    if !config.workflow.enabled {
        return Err(CrabError::WorkflowDisabled);
    }

    if !args.all {
        return Err(CrabError::Configuration {
            key: "`crab workflow push-cache` requires --all".into(),
            origin: "cli".into(),
        });
    }

    let cache_root = repo_root.join(".crab").join("cache");
    if !cache_root.exists() {
        info!("no local cache entries found; nothing to push");
        let result = PushCacheResult {
            pushed: 0,
            skipped: 0,
            errors: 0,
        };
        let mode = args.output_mode();
        if mode == OutputMode::Json {
            emit_json(WORKFLOW_PUSH_CACHE_SCHEMA, "1", &result);
        }
        return Ok(());
    }

    // Build the remote store from the repo's crab remote config.
    let (store, prefix) = build_remote_store(repo_root, &config).await?;
    let cancel = CancellationToken::new();
    let artifact_stores = build_workflow_artifact_stores(&config, &cancel).await;
    let artifact_stores = (!artifact_stores.is_empty()).then_some(artifact_stores);

    let push_result = cache::push_all_local_with_artifact_stores(
        &store,
        &prefix,
        artifact_stores.as_ref(),
        &cache_root,
    )
    .await?;

    info!(
        pushed = push_result.pushed,
        skipped = push_result.skipped,
        errors = push_result.errors,
        "push-cache --all complete"
    );

    let mode = args.output_mode();
    if mode == OutputMode::Json {
        let result = PushCacheResult {
            pushed: push_result.pushed,
            skipped: push_result.skipped,
            errors: push_result.errors,
        };
        emit_json(WORKFLOW_PUSH_CACHE_SCHEMA, "1", &result);
    } else {
        println!(
            "Pushed {} entries, skipped {} (already remote), {} errors",
            push_result.pushed, push_result.skipped, push_result.errors
        );
    }

    Ok(())
}

/// Build a remote Store + prefix from the repo's crab configuration.
///
/// Reuses the same credential chain and store construction as
/// `crab push` — no separate credential configuration needed.
async fn build_remote_store(repo_root: &Path, config: &Config) -> Result<(WorkflowStore, String)> {
    build_remote_store_for(repo_root, config, None).await
}

pub(crate) async fn build_remote_store_for(
    repo_root: &Path,
    config: &Config,
    remote_name: Option<&str>,
) -> Result<(WorkflowStore, String)> {
    // Read the crab remote URL from git's remote configuration.
    let url_str = read_crab_remote_url_for(repo_root, remote_name)?;
    let crab_url = CrabUrl::parse(&url_str)?;
    let prefix = crab_url.repo_path.clone();

    let cancel = CancellationToken::new();
    let store = crate::auth::build_store(config, &crab_url, "workflow-push-cache", &cancel).await?;

    Ok((WorkflowStore::from_storage(store.into_storage()), prefix))
}

pub(crate) async fn build_workflow_artifact_stores(
    config: &Config,
    cancel: &CancellationToken,
) -> RemoteArtifactStores {
    let mut stores = RemoteArtifactStores::default();
    for (name, remote) in &config.workflow.remotes {
        let parsed = match CrabUrl::parse(remote.url.trim()) {
            Ok(parsed) => parsed,
            Err(e) => {
                debug!(
                    remote = %name,
                    error = %e,
                    "workflow remote is not a Crab artifact remote; keeping it available for remote:// aliases"
                );
                let _ = stores.insert_failure(name.clone(), e.to_string());
                continue;
            }
        };

        let operation = format!("workflow-artifact-remote-{name}");
        match crate::auth::build_store(config, &parsed, &operation, cancel).await {
            Ok(store) => {
                let store = WorkflowStore::from_storage(store.into_storage());
                if let Err(e) =
                    stores.insert(name.clone(), Arc::new(store), parsed.repo_path.clone())
                {
                    warn!(
                        remote = %name,
                        error = %e,
                        "workflow artifact remote name is invalid"
                    );
                    let _ = stores.insert_failure(name.clone(), e.to_string());
                }
            }
            Err(e) => {
                warn!(
                    remote = %name,
                    error = %e,
                    "workflow artifact remote could not be opened"
                );
                let _ = stores.insert_failure(name.clone(), e.to_string());
            }
        }
    }
    stores
}

/// Read the crab:// remote URL from git config.
pub fn read_crab_remote_url(repo_root: &Path) -> Result<String> {
    read_crab_remote_url_for(repo_root, None)
}

/// Read a named crab:// remote URL from git config.
pub fn read_crab_remote_url_for(repo_root: &Path, remote_name: Option<&str>) -> Result<String> {
    let remote_name = remote_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("origin");

    // Try reading from the selected git remote first.
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", remote_name])
        .current_dir(repo_root)
        .output()
        .map_err(CrabError::Io)?;

    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if url.starts_with("crab://") {
            return Ok(url);
        }
    }

    // Fall back to .crab/config if present.
    let config_path = repo_root.join(".crab").join("config");
    if config_path.exists() {
        let text = std::fs::read_to_string(&config_path).map_err(CrabError::Io)?;
        for line in text.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("url") {
                let val = val
                    .trim_start_matches(|c: char| c == ' ' || c == '=')
                    .trim();
                if val.starts_with("crab://") {
                    return Ok(val.to_owned());
                }
            }
        }
    }

    Err(CrabError::Configuration {
        key: format!(
            "no crab:// remote URL found; configure with `crab init` or set git remote {remote_name}"
        ),
        origin: "workflow remote cache".into(),
    })
}
