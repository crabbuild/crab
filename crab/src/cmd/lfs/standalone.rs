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
use crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE;
use crab_git::lfs_pointer::hex_encode;
use crab_git::pointer_detect::{PointerKind, classify};

/// Run `crab lfs clean`: read file content from stdin, produce an LFS
/// pointer on stdout.
///
/// The original content is staged in the local LFS cache. Remote publication
/// is owned by the pre-push path, matching Git LFS's clean-filter contract.
pub fn run_lfs_clean(path: Option<&str>) -> Result<()> {
    let file_name = path.unwrap_or("<unknown file>");
    let extensions = configured_extensions_sorted()?;
    let lfs_dir = resolve_lfs_storage_dir()?;
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
    staged.install(&lfs_dir)?;
    let oid_hex = hex_encode(&oid);

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
    let mut input = io::stdin().lock();
    if skip || git_lfs_skip_smudge() {
        io::copy(&mut input, &mut io::stdout().lock()).map_err(CrabError::Io)?;
        return Ok(());
    }
    let skip_download_errors =
        resolve_smudge_lfs_config().is_ok_and(|config| config.skip_download_errors);

    let mut prefix = Vec::with_capacity(MAX_LFS_POINTER_SIZE + 1);
    input
        .by_ref()
        .take((MAX_LFS_POINTER_SIZE + 1) as u64)
        .read_to_end(&mut prefix)
        .map_err(CrabError::Io)?;

    if prefix.len() > MAX_LFS_POINTER_SIZE {
        write_smudge_passthrough(&prefix, &mut input)?;
        return Ok(());
    }

    match classify(&prefix) {
        PointerKind::Lfs(pointer) => {
            if !smudge_path_allowed(path)? {
                write_smudge_passthrough(&prefix, &mut input)?;
                return Ok(());
            }

            let lfs_dir = resolve_lfs_storage_dir()?;

            // A corrupt cache entry is a miss, never materializable content.
            if crate::lfs::cache::is_valid(&lfs_dir, &pointer.oid, pointer.size)? {
                let local_path = crate::lfs::cache::object_path(&lfs_dir, &pointer.oid);
                write_smudged_file(&pointer, &local_path, path_for_ext(path))?;
                return Ok(());
            }

            let content_path = match resolve_from_remote(&pointer, &lfs_dir) {
                Ok(path) => path,
                Err(error) if skip_download_errors => {
                    tracing::warn!(
                        oid = %hex_encode(&pointer.oid),
                        error = %error,
                        "smudge: LFS download failed; preserving pointer because skipdownloaderrors is enabled"
                    );
                    write_smudge_passthrough(&prefix, &mut input)?;
                    return Ok(());
                }
                Err(CrabError::LfsObjectCorrupt { .. }) => {
                    return Err(CrabError::LfsObjectCorrupt {
                        oid: hex_encode(&pointer.oid),
                    });
                }
                Err(error) => return Err(error),
            };
            write_smudged_file(&pointer, &content_path, path_for_ext(path))?;
        }
        PointerKind::Crab(_) | PointerKind::NotAPointer => {
            write_smudge_passthrough(&prefix, &mut input)?;
        }
    }

    Ok(())
}

fn write_smudged_file(
    pointer: &crab_git::lfs_pointer::LfsPointer,
    path: &Path,
    file_name: &str,
) -> Result<()> {
    if pointer.extensions.is_empty() {
        let mut output = io::stdout().lock();
        return crate::lfs::cache::stream_verified(path, &pointer.oid, pointer.size, |bytes| {
            output.write_all(bytes).map_err(CrabError::Io)
        });
    }

    let content = std::fs::read(path).map_err(CrabError::Io)?;
    crate::lfs::cache::verify_bytes(&pointer.oid, pointer.size, &content)?;
    let content = crate::lfs::extension::smudge_content(pointer, content, file_name)?;
    write_stdout(&content)
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

/// Download and atomically install an LFS object without retaining its body.
fn resolve_from_remote(
    pointer: &crab_git::lfs_pointer::LfsPointer,
    lfs_dir: &Path,
) -> Result<PathBuf> {
    let ctx = super::store_setup::resolve_lfs_remote_for_operation_sync("smudge")?;
    let temp = crate::lfs::cache::new_temp_path(lfs_dir)?;
    let temp_path = temp.to_path_buf();
    super::block_on_runtime(async {
        ctx.store
            .download_to_file(&pointer.oid, pointer.size, &temp_path)
            .await
            .map_err(CrabError::from)
    })?;
    crate::lfs::cache::install_verified_temp_path(lfs_dir, &pointer.oid, pointer.size, temp)
}

fn write_stdout(data: &[u8]) -> Result<()> {
    io::stdout()
        .write_all(data)
        .map_err(|e| CrabError::Configuration {
            key: "stdout".to_owned(),
            origin: format!("failed to write to stdout: {e}"),
        })
}

fn write_smudge_passthrough<R: Read>(prefix: &[u8], input: &mut R) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(prefix).map_err(CrabError::Io)?;
    io::copy(input, &mut stdout).map_err(CrabError::Io)?;
    Ok(())
}

fn resolve_lfs_storage_dir() -> Result<PathBuf> {
    let repo_root = discover_repo_root()?;
    LfsConfig::resolve_storage_dir(&repo_root)
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
