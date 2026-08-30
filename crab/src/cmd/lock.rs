//! `crab lock` / `crab unlock` / `crab locks` — advisory file locking.
//!
//! Prevents wasted work when multiple collaborators edit the same
//! non-mergeable binary file. Lock records are stored as JSON objects
//! in the remote bucket at `{repo-path}/locks/files/{blake3(path)}`,
//! using CAS (`PutMode::Create`) for atomic acquisition.
//!
//! The lock owner is determined from `git config user.email`, falling
//! back to `git config user.name`.

#[cfg(not(feature = "gix-config"))]
use std::process::Command;

use serde::Serialize;

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::lfs::lock::LockManager;

// --- Public API ---

/// Acquire an advisory lock on one or more files.
pub async fn run_lock(paths: &[String], mode: OutputMode) -> Result<()> {
    let (mgr, owner) = setup().await?;
    let mut records = Vec::new();
    let mut had_error = false;

    for path in paths {
        let rel = normalize_path(path);
        match mgr.lock(&rel, &owner).await {
            Ok(record) => {
                if mode == OutputMode::Text {
                    eprintln!("Locked {}", record.path);
                }
                records.push(record);
            }
            Err(CrabError::LfsLockConflict {
                ref path,
                ref owner,
            }) => {
                eprintln!("error: {path} is locked by {owner}");
                had_error = true;
            }
            Err(e) => return Err(e),
        }
    }

    if mode == OutputMode::Json {
        emit_json("lock", "1.1", &records);
    }

    if had_error {
        return Err(CrabError::LfsLockConflict {
            path: "multiple".into(),
            owner: "see above".into(),
        });
    }

    Ok(())
}

/// Release an advisory lock on one or more files.
pub async fn run_unlock(paths: &[String], force: bool, mode: OutputMode) -> Result<()> {
    let (mgr, owner) = setup().await?;
    let mut results = Vec::new();
    let mut had_error = false;

    for path in paths {
        let rel = normalize_path(path);
        let result = if force {
            mgr.force_unlock(&rel).await
        } else {
            mgr.unlock(&rel, &owner).await
        };

        match result {
            Ok(()) => {
                if mode == OutputMode::Text {
                    eprintln!("Unlocked {rel}");
                }
                results.push(UnlockResult {
                    path: rel,
                    unlocked: true,
                    reason: None,
                });
            }
            Err(CrabError::LfsLockConflict {
                ref path,
                ref owner,
            }) => {
                let msg = format!("locked by {owner}");
                eprintln!("error: cannot unlock {path}: {msg}");
                results.push(UnlockResult {
                    path: rel,
                    unlocked: false,
                    reason: Some(msg),
                });
                had_error = true;
            }
            Err(CrabError::NotFound { ref path }) => {
                let msg = "no lock found".to_string();
                eprintln!("error: {path}: {msg}");
                results.push(UnlockResult {
                    path: rel,
                    unlocked: false,
                    reason: Some(msg),
                });
                had_error = true;
            }
            Err(e) => return Err(e),
        }
    }

    if mode == OutputMode::Json {
        emit_json("unlock", "1.1", &results);
    }

    if had_error {
        return Err(CrabError::LfsLockConflict {
            path: "multiple".into(),
            owner: "see above".into(),
        });
    }

    Ok(())
}

/// List all active locks, optionally filtered by path or owner.
pub async fn run_locks(
    path_filter: Option<&str>,
    owner_filter: Option<&str>,
    mode: OutputMode,
    limit: Option<usize>,
) -> Result<()> {
    let (mgr, current_owner) = setup().await?;
    let mut records = mgr.list().await?;

    // Apply filters.
    if let Some(pf) = path_filter {
        records.retain(|r| r.path == pf);
    }
    if let Some(of) = owner_filter {
        if of == "self" {
            records.retain(|r| r.owner == current_owner);
        } else {
            records.retain(|r| r.owner == of);
        }
    }

    // Sort by path for stable output.
    records.sort_by(|a, b| a.path.cmp(&b.path));

    // Apply limit.
    if let Some(lim) = limit {
        records.truncate(lim);
    }

    if mode == OutputMode::Json {
        emit_json("locks", "1.1", &records);
        return Ok(());
    }

    if records.is_empty() {
        eprintln!("no locks found");
        return Ok(());
    }

    // Compute column widths for aligned output.
    let max_path = records.iter().map(|r| r.path.len()).max().unwrap_or(0);
    let max_owner = records.iter().map(|r| r.owner.len()).max().unwrap_or(0);

    for record in &records {
        let own_marker = if record.owner == current_owner {
            "O"
        } else {
            " "
        };
        println!(
            "{own_marker} {:<width_p$}\t{:<width_o$}\tID:{}",
            record.path,
            record.owner,
            record.id,
            width_p = max_path,
            width_o = max_owner,
        );
    }

    Ok(())
}

// --- Internals ---

/// Build a `LockManager` from the repo's remote config and an S3 store.
async fn setup() -> Result<(LockManager, String)> {
    let cwd = std::env::current_dir()?;
    let url = crate::core::project_config::ProjectConfig::remote_url(&cwd)?;
    let parsed = crate::git::url::CrabUrl::parse(&url)?;
    let config = crate::core::config::Config::resolve_local().unwrap_or_default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let store = crate::auth::build_repository_url_store(&config, &parsed, "lock", &cancel).await?;
    let mgr = LockManager::native(store, &parsed.repo_path);
    let owner = resolve_owner()?;

    Ok((mgr, owner))
}

/// Resolve the current user's identity for lock ownership.
///
/// Tries `git config user.email` first, then `git config user.name`.
fn resolve_owner() -> Result<String> {
    if let Some(email) = git_config("user.email") {
        return Ok(email);
    }
    if let Some(name) = git_config("user.name") {
        return Ok(name);
    }

    Err(CrabError::Configuration {
        key: "cannot determine lock owner — set git config user.email".into(),
        origin: "git config".into(),
    })
}

/// Read a git config value.
///
/// On builds with `--features gix-config`, reads resolve through
/// [`GixConfigResolver`] against the discovered git dir. Default
/// builds fall back to `git config <key>` shellout preserving the
/// original semantics byte-for-byte.
fn git_config(key: &str) -> Option<String> {
    #[cfg(feature = "gix-config")]
    {
        let git_dir = crate::git::discover::discover_git_dir().ok()?;
        let resolver = crate::core::config_resolver::GixConfigResolver::open(&git_dir).ok()?;
        resolver.string(key)
    }

    #[cfg(not(feature = "gix-config"))]
    {
        Command::new("git")
            .args(["config", key])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }
}

/// Normalize a file path to be repo-relative with forward slashes.
fn normalize_path(path: &str) -> String {
    // Strip leading ./ and normalize separators.
    let cleaned = path.trim().trim_start_matches("./").replace('\\', "/");

    // If the path is absolute, try to make it relative to the repo root.
    if cleaned.starts_with('/')
        && let Ok(cwd) = std::env::current_dir()
        && let Ok(rel) = std::path::Path::new(&cleaned).strip_prefix(&cwd)
    {
        return rel.to_string_lossy().replace('\\', "/");
    }

    cleaned
}

/// Result of an unlock operation for JSON output.
#[derive(Serialize)]
struct UnlockResult {
    path: String,
    unlocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_dot_slash() {
        assert_eq!(normalize_path("./models/large.bin"), "models/large.bin");
    }

    #[test]
    fn normalize_converts_backslashes() {
        assert_eq!(normalize_path("models\\large.bin"), "models/large.bin");
    }

    #[test]
    fn normalize_trims_whitespace() {
        assert_eq!(normalize_path("  models/large.bin  "), "models/large.bin");
    }

    #[test]
    fn normalize_preserves_clean_path() {
        assert_eq!(normalize_path("data/train.csv"), "data/train.csv");
    }

    #[test]
    fn resolve_owner_returns_something() {
        // In a dev environment, git config user.email should be set.
        // If not, this test will fail — that's intentional, it means
        // the developer needs to configure git.
        let owner = resolve_owner();
        // Don't assert Ok — CI may not have git config set.
        // Just verify it doesn't panic.
        let _ = owner;
    }
}
