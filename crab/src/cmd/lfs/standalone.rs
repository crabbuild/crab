//! `crab lfs clean` and `crab lfs smudge` — standalone filter commands.
//!
//! These provide traditional single-file clean/smudge filter behavior as
//! an alternative to the long-running filter-process protocol. Content is
//! read from stdin and written to stdout.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::core::error::{CrabError, Result};
use crate::lfs::config::LfsConfig;
use crate::lfs::extension::{clean_staged_with_extensions, configured_extensions_sorted};
use crab_git::lfs_pointer::hex_encode;
use crab_git::pointer_detect::{PointerKind, classify};

/// Run `crab lfs clean`: read file content from stdin, produce an LFS
/// pointer on stdout.
///
/// The original content is staged in the local LFS cache and uploaded
/// to the remote store if a remote is configured.
pub fn run_lfs_clean(path: Option<&str>) -> Result<()> {
    let file_name = path.unwrap_or("<unknown file>");
    let extensions = configured_extensions_sorted()?;
    let lfs_dir = discover_git_dir()?.join("lfs");
    let mut writer = crate::lfs::cache::ObjectWriter::new(&lfs_dir)?;
    io::copy(&mut io::stdin().lock(), &mut writer).map_err(CrabError::Io)?;
    let staged = writer.finish()?;
    if extensions.is_empty() && staged.size() <= 1024 {
        let content = std::fs::read(staged.path()).map_err(CrabError::Io)?;
        if is_lfs_pointer(&content) {
            write_stdout(&content)?;
            return Ok(());
        }
    }
    let cleaned = clean_staged_with_extensions(staged, &lfs_dir, file_name, &extensions)?;
    let staged = cleaned.staged;
    let pointer_extensions = cleaned.pointer_extensions;

    let oid = *staged.oid();
    let size = staged.size();
    let local_path = staged.install(&lfs_dir)?;
    let oid_hex = hex_encode(&oid);

    // Upload to remote store if configured.
    if let Ok(ctx) = super::store_setup::resolve_lfs_remote_sync() {
        let upload_path = local_path.clone();
        if let Err(e) = super::block_on_runtime(async move {
            ctx.store
                .put_stream(&oid, &upload_path)
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

    let pointer = crab_git::lfs_pointer::LfsPointer {
        oid,
        size,
        extensions: pointer_extensions,
    };
    write_stdout(&pointer.serialize())?;
    tracing::debug!(oid = %oid_hex, size, "clean: produced LFS pointer");
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

            let lfs_dir = discover_git_dir()?.join("lfs");

            // A corrupt cache entry is a miss, never materializable content.
            match crate::lfs::cache::read_pointer(&lfs_dir, &pointer) {
                Ok(Some(content)) => {
                    let content = crate::lfs::extension::smudge_content(
                        &pointer,
                        content,
                        path_for_ext(path),
                    )?;
                    write_stdout(&content)?;
                    return Ok(());
                }
                Ok(None) | Err(CrabError::LfsObjectCorrupt { .. }) => {}
                Err(error) => return Err(error),
            }

            let content = resolve_from_remote(&pointer.oid)?;
            crate::lfs::cache::verify_pointer(&pointer, &content)?;
            crate::lfs::cache::install_bytes(&lfs_dir, &pointer.oid, pointer.size, &content)?;
            let content =
                crate::lfs::extension::smudge_content(&pointer, content, path_for_ext(path))?;
            write_stdout(&content)?;
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

/// Download from remote LFS store.
fn resolve_from_remote(oid: &[u8; 32]) -> Result<Vec<u8>> {
    let ctx = super::store_setup::resolve_lfs_remote_for_operation_sync("smudge")?;
    super::block_on_runtime(async {
        let content = ctx.store.verify(oid).await?;
        Ok(content.to_vec())
    })
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
