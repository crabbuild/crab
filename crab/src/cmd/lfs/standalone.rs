//! `crab lfs clean` and `crab lfs smudge` — standalone filter commands.
//!
//! These provide traditional single-file clean/smudge filter behavior as
//! an alternative to the long-running filter-process protocol. Content is
//! read from stdin and written to stdout.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::core::error::{CrabError, Result};
use crate::lfs::config::LfsConfig;
use crate::lfs::extension::{clean_content_with_extensions, configured_extensions_sorted};
use crab_git::lfs_pointer::hex_encode;
use crab_git::pointer_detect::{PointerKind, classify};

/// Run `crab lfs clean`: read file content from stdin, produce an LFS
/// pointer on stdout.
///
/// The original content is staged in the local LFS cache and uploaded
/// to the remote store if a remote is configured.
pub fn run_lfs_clean(path: Option<&str>) -> Result<()> {
    let mut content = Vec::new();
    io::stdin()
        .read_to_end(&mut content)
        .map_err(|e| CrabError::Configuration {
            key: "stdin".to_owned(),
            origin: format!("failed to read stdin: {e}"),
        })?;

    let file_name = path.unwrap_or("<unknown file>");
    let extensions = configured_extensions_sorted()?;
    if extensions.is_empty() && is_lfs_pointer(&content) {
        io::stdout()
            .write_all(&content)
            .map_err(|e| CrabError::Configuration {
                key: "stdout".to_owned(),
                origin: format!("failed to write to stdout: {e}"),
            })?;
        return Ok(());
    }

    let cleaned = clean_content_with_extensions(&content, file_name, &extensions)?;
    let pointer = crab_git::lfs_pointer::LfsPointer {
        oid: cleaned.oid,
        size: cleaned.size,
        extensions: cleaned.pointer_extensions,
    };

    let serialized = pointer.serialize();

    io::stdout()
        .write_all(&serialized)
        .map_err(|e| CrabError::Configuration {
            key: "stdout".to_owned(),
            origin: format!("failed to write to stdout: {e}"),
        })?;

    // Stage content in local LFS cache.
    let oid_hex = hex_encode(&cleaned.oid);
    if let Ok(git_dir) = discover_git_dir() {
        let local_path = git_dir
            .join("lfs")
            .join("objects")
            .join(&oid_hex[..2])
            .join(&oid_hex[2..4])
            .join(&oid_hex);

        if !local_path.is_file() {
            if let Some(parent) = local_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&local_path, &cleaned.content);
        }
    }

    // Upload to remote store if configured.
    if let Ok(ctx) = super::store_setup::resolve_lfs_remote_sync() {
        let content_bytes = bytes::Bytes::from(cleaned.content);
        let oid = cleaned.oid;
        if let Err(e) = super::block_on_runtime(async move {
            ctx.store
                .put(&oid, content_bytes)
                .await
                .map_err(CrabError::from)
        }) {
            tracing::debug!(
                oid = %oid_hex,
                error = %e,
                "clean: failed to upload to remote (will be pushed later)",
            );
        }
    }

    tracing::debug!(oid = %oid_hex, size = cleaned.size, "clean: produced LFS pointer");
    Ok(())
}

fn is_lfs_pointer(content: &[u8]) -> bool {
    matches!(classify(content), PointerKind::Lfs(_))
}

/// Run `crab lfs smudge`: read an LFS pointer from stdin, produce the
/// original file content on stdout.
///
/// With `--skip`, passes the pointer through unchanged (lazy mode).
pub fn run_lfs_smudge(path: Option<&str>, skip: bool) -> Result<()> {
    let mut input = Vec::new();
    io::stdin()
        .read_to_end(&mut input)
        .map_err(|e| CrabError::Configuration {
            key: "stdin".to_owned(),
            origin: format!("failed to read stdin: {e}"),
        })?;

    if skip || git_lfs_skip_smudge() {
        io::stdout()
            .write_all(&input)
            .map_err(|e| CrabError::Configuration {
                key: "stdout".to_owned(),
                origin: format!("failed to write to stdout: {e}"),
            })?;
        return Ok(());
    }

    match classify(&input) {
        PointerKind::Lfs(pointer) => {
            if !smudge_path_allowed(path)? {
                write_stdout(&input)?;
                return Ok(());
            }

            let oid_hex = hex_encode(&pointer.oid);

            // Try local cache first.
            if let Some(content) = try_local_cache(&oid_hex)? {
                let content =
                    crate::lfs::extension::smudge_content(&pointer, content, path_for_ext(path))?;
                write_stdout(&content)?;
                return Ok(());
            }

            // Try remote download.
            match resolve_from_remote(&pointer.oid) {
                Ok(content) => {
                    // Cache locally.
                    cache_locally(&oid_hex, &content);
                    let content = crate::lfs::extension::smudge_content(
                        &pointer,
                        content,
                        path_for_ext(path),
                    )?;
                    write_stdout(&content)?;
                }
                Err(e) => {
                    tracing::debug!(
                        oid = %oid_hex,
                        error = %e,
                        "smudge: failed to download, passing pointer through",
                    );
                    // Pass pointer through so the file is at least readable.
                    write_stdout(&input)?;
                }
            }
        }
        PointerKind::Crab(_) | PointerKind::NotAPointer => {
            write_stdout(&input)?;
        }
    }

    Ok(())
}

fn path_for_ext(path: Option<&str>) -> &str {
    path.unwrap_or("<unknown file>")
}

fn smudge_path_allowed(path: Option<&str>) -> Result<bool> {
    let Ok(config) = resolve_smudge_lfs_config() else {
        return Ok(true);
    };
    crate::lfs::fetch_filter::path_allowed_by_fetch_filters(
        path.unwrap_or("<unknown file>"),
        config.fetch_include.as_deref(),
        config.fetch_exclude.as_deref(),
    )
}

fn resolve_smudge_lfs_config() -> Result<LfsConfig> {
    let repo_root = discover_repo_root()?;
    LfsConfig::resolve(&repo_root)
}

fn git_lfs_skip_smudge() -> bool {
    std::env::var("GIT_LFS_SKIP_SMUDGE")
        .ok()
        .is_some_and(|value| !value.is_empty() && value != "0" && value != "false")
}

/// Try to read from local `.git/lfs/objects` cache.
fn try_local_cache(oid_hex: &str) -> Result<Option<Vec<u8>>> {
    let Ok(git_dir) = discover_git_dir() else {
        return Ok(None);
    };

    let local_path = git_dir
        .join("lfs")
        .join("objects")
        .join(&oid_hex[..2])
        .join(&oid_hex[2..4])
        .join(oid_hex);

    if local_path.is_file() {
        let content = std::fs::read(&local_path).map_err(CrabError::Io)?;
        return Ok(Some(content));
    }
    Ok(None)
}

/// Download from remote LFS store.
fn resolve_from_remote(oid: &[u8; 32]) -> Result<Vec<u8>> {
    let ctx = super::store_setup::resolve_lfs_remote_for_operation_sync("smudge")?;
    super::block_on_runtime(async {
        let content = ctx.store.get(oid).await?;
        Ok(content.to_vec())
    })
}

/// Cache content in local `.git/lfs/objects`.
fn cache_locally(oid_hex: &str, content: &[u8]) {
    if let Ok(git_dir) = discover_git_dir() {
        let local_path = git_dir
            .join("lfs")
            .join("objects")
            .join(&oid_hex[..2])
            .join(&oid_hex[2..4])
            .join(oid_hex);

        if let Some(parent) = local_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&local_path, content);
    }
}

fn write_stdout(data: &[u8]) -> Result<()> {
    io::stdout()
        .write_all(data)
        .map_err(|e| CrabError::Configuration {
            key: "stdout".to_owned(),
            origin: format!("failed to write to stdout: {e}"),
        })
}

fn discover_git_dir() -> Result<PathBuf> {
    crate::git::discover::discover_common_git_dir()
}

fn discover_repo_root() -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to discover repository root: {e}")))?;
    if !output.status.success() {
        let cwd = std::env::current_dir().map_err(CrabError::Io)?;
        return Ok(cwd);
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if root.is_empty() {
        let cwd = std::env::current_dir().map_err(CrabError::Io)?;
        return Ok(cwd);
    }
    Ok(Path::new(&root).to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_git::lfs_pointer::LfsPointer;

    #[test]
    fn detects_lfs_pointer_input() {
        let pointer = LfsPointer {
            oid: [1; 32],
            size: 42,
            extensions: Vec::new(),
        }
        .serialize();

        assert!(is_lfs_pointer(&pointer));
    }

    #[test]
    fn regular_content_is_not_lfs_pointer_input() {
        assert!(!is_lfs_pointer(b"regular file content"));
    }
}
