//! Shared store setup for LFS commands that need remote access.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::core::error::{CrabError, Result};
use crate::lfs::config::LfsConfig;
use crab_lfs::LfsObjectStore;

/// Resolved LFS remote context for commands that need cloud access.
pub struct LfsRemoteContext {
    /// The remote LFS object store.
    pub store: Arc<LfsObjectStore>,
    /// Path to the local `.git/lfs` directory.
    pub local_lfs_dir: PathBuf,
    /// Resolved LFS configuration.
    pub config: LfsConfig,
    /// The object store prefix (repo path within the bucket).
    pub prefix: String,
}

/// Resolve the remote store from `.crab/config.toml`.
///
/// Reads the remote URL, parses it, routes read-class operations through
/// replica-aware selection, and wraps the selected Store in an LfsObjectStore.
/// Returns an error with a clear message if the remote is not configured.
pub async fn resolve_lfs_remote() -> Result<LfsRemoteContext> {
    resolve_lfs_remote_for_operation("lfs").await
}

pub async fn resolve_lfs_remote_for_operation(operation: &str) -> Result<LfsRemoteContext> {
    resolve_lfs_remote_for_operation_with_remote(operation, None).await
}

/// Resolve the LFS store for a specific operation and optional Git remote name.
pub async fn resolve_lfs_remote_for_operation_with_remote(
    operation: &str,
    remote: Option<&str>,
) -> Result<LfsRemoteContext> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    resolve_lfs_remote_for_operation_with_remote_from(operation, remote, &cwd).await
}

/// Resolve the LFS store using the worktree containing `repo_root`.
pub async fn resolve_lfs_remote_for_operation_with_remote_from(
    operation: &str,
    remote: Option<&str>,
    repo_root: &Path,
) -> Result<LfsRemoteContext> {
    let worktree = crate::git::worktree::WorktreeContext::resolve_from_path(repo_root)?;
    let repo_root = &worktree.current_worktree_root;
    let url = match remote {
        Some(name) => read_git_remote_url_from(name, repo_root)?,
        None => read_repo_remote_url_from(repo_root)?,
    };
    let config = crate::core::config::Config::resolve_for_repo(repo_root)?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let cache_dir = crab_auth::token_cache::expand_token_cache_path(&config.auth.token_cache_path);
    let managed_resolver = crab_auth_store::ManagedRepositoryResolver::new(cache_dir);
    let locator = managed_resolver.classify(&url)?;
    let (lfs_store, prefix) = match locator {
        crab_git::RepositoryLocator::Managed(repository) => {
            if !is_lfs_read_operation(operation) {
                return Err(CrabError::LfsUnsupported {
                    command: operation.to_owned(),
                    reason: "managed LFS writes require the protected-push protocol".to_owned(),
                });
            }
            let managed = managed_resolver
                .resolve(&repository, crab_auth::TransferOperation::Hydrate, &cancel)
                .await?;
            let prefix = managed.repository_prefix;
            (
                Arc::new(LfsObjectStore::new(managed.store, &prefix)),
                prefix,
            )
        }
        crab_git::RepositoryLocator::Direct(repository) => {
            let parsed = crate::git::url::CrabUrl {
                bucket: repository.bucket,
                repo_path: repository.repo_prefix,
            };
            let resolver = crate::replication::StoreResolver::new(&config, &parsed, &cancel);
            if is_lfs_read_operation(operation) {
                let selection = resolver.read_store(operation).await?;
                let prefix = selection.router.repo_prefix().to_owned();
                let store = if matches!(
                    &selection.source,
                    crate::replication::ReadSource::Replica { .. }
                ) {
                    let primary = resolver.write_store("lfs-read-primary-fallback").await?;
                    let primary_prefix = primary.router.repo_prefix().to_owned();
                    LfsObjectStore::new_with_primary_fallback(
                        selection.store.into_storage(),
                        &prefix,
                        primary.store.into_storage(),
                        &primary_prefix,
                    )
                } else {
                    LfsObjectStore::new(selection.store.into_storage(), &prefix)
                };
                (Arc::new(store), prefix)
            } else {
                let selection = resolver.write_store(operation).await?;
                let prefix = selection.router.repo_prefix().to_owned();
                (
                    Arc::new(LfsObjectStore::new(selection.store.into_storage(), &prefix)),
                    prefix,
                )
            }
        }
    };

    let lfs_config = LfsConfig::resolve(repo_root)?;

    let local_lfs_dir = lfs_config.storage_dir(&worktree.common_git_dir);

    Ok(LfsRemoteContext {
        store: lfs_store,
        local_lfs_dir,
        config: lfs_config,
        prefix,
    })
}

pub(crate) async fn resolve_crab_read_layout(
    url: &str,
    operation: &str,
    config: &crate::core::config::Config,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<crate::storage::StoreLayout> {
    let cache_dir = crab_auth::token_cache::expand_token_cache_path(&config.auth.token_cache_path);
    let managed_resolver = crab_auth_store::ManagedRepositoryResolver::new(cache_dir);
    match managed_resolver.classify(url)? {
        crab_git::RepositoryLocator::Managed(repository) => {
            let managed = managed_resolver
                .resolve(&repository, crab_auth::TransferOperation::Hydrate, cancel)
                .await?;
            Ok(crate::storage::StoreLayout::new(
                crate::storage::Store::from_storage(managed.store),
                managed.repository_prefix,
            ))
        }
        crab_git::RepositoryLocator::Direct(repository) => {
            let parsed = crate::git::url::CrabUrl {
                bucket: repository.bucket,
                repo_path: repository.repo_prefix,
            };
            crate::replication::select_read_store(config, parsed, operation, cancel)
                .await
                .map(|selection| selection.router)
        }
    }
}

/// Synchronous wrapper for [`resolve_lfs_remote`] for callers that are
/// not in an async context. Uses the current tokio runtime handle if
/// available, otherwise creates a temporary runtime.
pub fn resolve_lfs_remote_sync() -> Result<LfsRemoteContext> {
    resolve_lfs_remote_for_operation_sync("lfs")
}

pub fn resolve_lfs_remote_for_operation_sync(operation: &str) -> Result<LfsRemoteContext> {
    resolve_lfs_remote_for_operation_with_remote_sync(operation, None)
}

/// Synchronous wrapper for resolving an operation-scoped LFS store with an optional Git remote.
pub fn resolve_lfs_remote_for_operation_with_remote_sync(
    operation: &str,
    remote: Option<&str>,
) -> Result<LfsRemoteContext> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    resolve_lfs_remote_for_operation_with_remote_sync_from(operation, remote, &cwd)
}

/// Synchronous wrapper for resolving an operation-scoped LFS store from an explicit worktree.
pub fn resolve_lfs_remote_for_operation_with_remote_sync_from(
    operation: &str,
    remote: Option<&str>,
    repo_root: &Path,
) -> Result<LfsRemoteContext> {
    let operation = operation.to_owned();
    let remote = remote.map(ToOwned::to_owned);
    let repo_root = repo_root.to_owned();
    super::block_on_runtime(async move {
        resolve_lfs_remote_for_operation_with_remote_from(&operation, remote.as_deref(), &repo_root)
            .await
    })
}

fn is_lfs_read_operation(operation: &str) -> bool {
    matches!(
        operation,
        "pull" | "fetch" | "smudge" | "download" | "checkout" | "migrate-export" | "prune"
    )
}

/// Read the remote URL from `.crab/config.toml`.
pub(crate) fn read_repo_remote_url() -> Result<String> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    let repo_root =
        crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)?.current_worktree_root;
    read_repo_remote_url_from(&repo_root)
}

pub(super) fn read_repo_remote_url_from(repo_root: &Path) -> Result<String> {
    let worktree = crate::git::worktree::WorktreeContext::resolve_from_path(repo_root)?;
    let config_path = worktree.shared_crab_dir.join("config.toml");
    if config_path.is_file() {
        let content =
            std::fs::read_to_string(&config_path).map_err(|e| CrabError::Configuration {
                key: format!("failed to read .crab/config.toml: {e}"),
                origin: config_path.display().to_string(),
            })?;
        let table: toml::Table = content.parse().map_err(|e| CrabError::Configuration {
            key: format!("failed to parse .crab/config.toml: {e}"),
            origin: config_path.display().to_string(),
        })?;
        if let Some(url) = table
            .get("remote")
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
        {
            return Ok(url.to_owned());
        }
    }

    let remote_path = worktree.shared_crab_dir.join("remote");
    if remote_path.is_file() {
        let url = std::fs::read_to_string(&remote_path).map_err(|e| CrabError::Configuration {
            key: format!("failed to read .crab/remote: {e}"),
            origin: remote_path.display().to_string(),
        })?;
        let trimmed = url.trim().to_owned();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }

    let output = git_command(repo_root)
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .output()
        .ok();
    if let Some(o) = output
        && o.status.success()
    {
        let url = String::from_utf8_lossy(&o.stdout).trim().to_owned();
        if url.starts_with("crab://") {
            return Ok(url);
        }
    }

    Err(CrabError::Configuration {
        key: "no crab remote configured".into(),
        origin: "run `crab init <bucket-url>` to set up a remote".into(),
    })
}

fn read_git_remote_url_from(name: &str, repo_root: &Path) -> Result<String> {
    let output = git_command(repo_root)
        .args(["remote", "get-url", name])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Configuration {
            key: format!("failed to read git remote \"{name}\": {e}"),
            origin: "git remote get-url".into(),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CrabError::Configuration {
            key: format!("git remote \"{name}\" is not configured"),
            origin: stderr,
        });
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if url.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("git remote \"{name}\" has an empty URL"),
            origin: "git remote get-url".into(),
        });
    }
    Ok(url)
}

fn git_command(repo_root: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_QUARANTINE_PATH")
        .env_remove("GIT_NAMESPACE");
    command
}

/// Get the current user identity from git config for lock operations.
pub fn git_user_identity() -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["config", "user.email"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());

    if let Some(email) = output.filter(|email| !email.is_empty()) {
        return Ok(email);
    }

    let output = std::process::Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());

    output
        .filter(|n| !n.is_empty())
        .ok_or_else(|| CrabError::Configuration {
            key: "git user identity not configured".into(),
            origin: "set git config user.email or user.name".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_git_remote_url_from_reads_named_remote() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["remote", "add", "archive", "crab://bucket/repo"])
            .current_dir(dir.path())
            .status()
            .unwrap();

        let url = read_git_remote_url_from("archive", dir.path()).unwrap();

        assert_eq!(url, "crab://bucket/repo");
    }

    #[test]
    fn read_git_remote_url_from_rejects_missing_remote() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();

        let result = read_git_remote_url_from("missing", dir.path());

        assert!(result.is_err());
    }

    #[test]
    fn lfs_pull_operations_are_replica_read_eligible() {
        for operation in [
            "pull",
            "fetch",
            "smudge",
            "download",
            "checkout",
            "prune",
            "migrate-export",
        ] {
            assert!(
                is_lfs_read_operation(operation),
                "{operation} should use replica-aware LFS reads"
            );
        }
    }

    #[test]
    fn lfs_write_and_lock_operations_stay_primary() {
        for operation in [
            "lfs", "clean", "push", "pre-push", "lock", "unlock", "locks",
        ] {
            assert!(
                !is_lfs_read_operation(operation),
                "{operation} must use the primary LFS write path"
            );
        }
    }
}
