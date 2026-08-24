//! `crab lfs convert` — bidirectional LFS<->Crab pointer conversion.
//!
//! Conversion is intentionally routed through the canonical storage paths:
//! LFS-to-Crab resolves and verifies LFS bytes, switches tracking to
//! `filter=crab`, then delegates to `crab add` so chunk staging and git index
//! pointer writes stay identical to the normal Crab flow. Crab-to-LFS verifies
//! hydrated Crab bytes before writing the local LFS cache, uploading the LFS
//! object, switching tracking to `filter=lfs`, and staging an LFS pointer.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result};
use crate::core::output::OutputMode;
use crate::core::pattern::build_filter;
use crab_git::lfs_pointer::LfsPointer;
use crab_git::pointer_detect::{PointerKind, classify};
use crab_types::pointer::Pointer;

use super::store_setup::resolve_lfs_remote_for_operation_sync;

const CONVERT_MANIFEST: &str = "crab-lfs-convert-state.json";

/// Direction of conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertDirection {
    LfsToXet,
    XetToLfs,
}

#[derive(Debug)]
struct ConvertCandidate {
    path: String,
    index_blob: Vec<u8>,
    source: SourcePointer,
}

#[derive(Debug)]
enum SourcePointer {
    Lfs(LfsPointer),
    Crab(Pointer),
}

#[derive(Debug, Serialize, Deserialize)]
struct ConvertManifest {
    gitattributes: Option<String>,
    files: Vec<ConvertManifestFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConvertManifestFile {
    path: String,
    index_blob_b64: String,
}

/// Run a conversion.
pub fn run_convert(
    direction: ConvertDirection,
    pattern: &str,
    dry_run: bool,
    repo_root: &Path,
) -> Result<()> {
    let candidates = collect_candidates(direction, pattern, repo_root)?;
    if candidates.is_empty() {
        eprintln!("convert: no matching source pointers for {pattern:?}");
        return Ok(());
    }

    let mut total_bytes = 0u64;
    for candidate in &candidates {
        total_bytes = total_bytes.saturating_add(match &candidate.source {
            SourcePointer::Lfs(pointer) => pointer.size,
            SourcePointer::Crab(pointer) => pointer.size,
        });
    }

    if dry_run {
        eprintln!(
            "convert: would convert {} file(s), {}",
            candidates.len(),
            format_size(total_bytes),
        );
        for candidate in &candidates {
            eprintln!("  {}", candidate.path);
        }
        return Ok(());
    }

    write_manifest(repo_root, &candidates)?;

    match direction {
        ConvertDirection::LfsToXet => convert_lfs_to_crab(pattern, repo_root, &candidates)?,
        ConvertDirection::XetToLfs => convert_crab_to_lfs(pattern, repo_root, &candidates)?,
    }

    eprintln!(
        "convert: converted {} file(s), {}",
        candidates.len(),
        format_size(total_bytes),
    );
    Ok(())
}

/// Restore the index and `.gitattributes` from the last conversion manifest.
pub fn run_rollback(repo_root: &Path) -> Result<()> {
    let manifest_path = manifest_path(repo_root)?;
    if !manifest_path.is_file() {
        return Err(CrabError::Configuration {
            key: "crab lfs convert --rollback".into(),
            origin: "no conversion manifest found; nothing to roll back".into(),
        });
    }

    let raw = std::fs::read_to_string(&manifest_path).map_err(CrabError::Io)?;
    let manifest: ConvertManifest =
        serde_json::from_str(&raw).map_err(|e| CrabError::Configuration {
            key: "conversion manifest".into(),
            origin: format!("failed to parse {}: {e}", manifest_path.display()),
        })?;

    restore_gitattributes(repo_root, manifest.gitattributes.as_deref())?;

    for file in &manifest.files {
        let blob =
            BASE64_STANDARD
                .decode(&file.index_blob_b64)
                .map_err(|e| CrabError::Configuration {
                    key: file.path.clone(),
                    origin: format!("invalid base64 index blob in conversion manifest: {e}"),
                })?;
        write_blob_to_index(repo_root, &file.path, &blob)?;
    }

    std::fs::remove_file(&manifest_path).map_err(CrabError::Io)?;
    eprintln!("convert: rolled back {} file(s)", manifest.files.len());
    Ok(())
}

fn convert_lfs_to_crab(
    pattern: &str,
    repo_root: &Path,
    candidates: &[ConvertCandidate],
) -> Result<()> {
    for candidate in candidates {
        let SourcePointer::Lfs(pointer) = &candidate.source else {
            continue;
        };
        let content = resolve_lfs_content(repo_root, &candidate.path, pointer)?;
        write_worktree_file(repo_root, &candidate.path, &content)?;
    }

    crate::lfs::track::untrack(pattern, repo_root)?;
    crate::cmd::track::run_track_in(pattern, repo_root)?;

    let add_args = crate::cmd::add::AddArgs {
        patterns: vec![pattern.to_owned()],
        jobs: 8,
        dry_run: false,
        skip_git_add: false,
        mode: OutputMode::Text,
    };
    super::block_on_runtime(async {
        crate::cmd::add::run_add(&add_args, &CancellationToken::new()).await
    })
}

fn convert_crab_to_lfs(
    pattern: &str,
    repo_root: &Path,
    candidates: &[ConvertCandidate],
) -> Result<()> {
    let ctx = resolve_lfs_remote_for_operation_sync("push")?;

    for candidate in candidates {
        let SourcePointer::Crab(pointer) = &candidate.source else {
            continue;
        };
        let content = resolve_crab_content(repo_root, &candidate.path, pointer)?;
        let oid: [u8; 32] = Sha256::digest(&content).into();
        let local_path =
            crate::lfs::cache::install_bytes(&ctx.local_lfs_dir, &oid, pointer.size, &content)?;
        drop(content);
        super::block_on_runtime(async {
            ctx.store
                .put_stream(&oid, &local_path)
                .await
                .map_err(CrabError::from)
        })?;

        let lfs_pointer = LfsPointer {
            oid,
            size: pointer.size,
            extensions: Vec::new(),
        };
        write_blob_to_index(repo_root, &candidate.path, &lfs_pointer.serialize())?;
    }

    crate::cmd::track::run_untrack_in(pattern, repo_root)?;
    crate::lfs::track::track_with_opts(pattern, repo_root, true, false)?;
    Ok(())
}

fn collect_candidates(
    direction: ConvertDirection,
    pattern: &str,
    repo_root: &Path,
) -> Result<Vec<ConvertCandidate>> {
    let filter = build_filter(&[pattern.to_owned()], &[])?;
    let mut candidates = Vec::new();

    for path in git_ls_files(repo_root)? {
        if !filter.matches(&path) {
            continue;
        }
        let Some(blob) = read_index_blob(repo_root, &path)? else {
            continue;
        };
        let source = match direction {
            ConvertDirection::LfsToXet => match classify(&blob) {
                PointerKind::Lfs(pointer) if pointer.size > 0 => SourcePointer::Lfs(pointer),
                _ => continue,
            },
            ConvertDirection::XetToLfs => match Pointer::parse(&blob) {
                Ok(pointer) if pointer.size > 0 => SourcePointer::Crab(pointer),
                _ => continue,
            },
        };

        candidates.push(ConvertCandidate {
            path,
            index_blob: blob,
            source,
        });
    }

    Ok(candidates)
}

fn resolve_lfs_content(repo_root: &Path, rel_path: &str, pointer: &LfsPointer) -> Result<Vec<u8>> {
    let full_path = repo_root.join(rel_path);
    if full_path.is_file() {
        let content = std::fs::read(&full_path).map_err(CrabError::Io)?;
        if classify(&content).is_lfs_pointer() {
            // Fall through to cache/remote resolution.
        } else {
            verify_lfs_bytes(rel_path, pointer, &content)?;
            return Ok(content);
        }
    }

    let git_dir = discover_git_dir(repo_root)?;
    let lfs_dir = git_dir.join("lfs");
    match crate::lfs::cache::read_pointer(&lfs_dir, pointer) {
        Ok(Some(content)) => return Ok(content),
        Ok(None) | Err(CrabError::LfsObjectCorrupt { .. }) => {}
        Err(error) => return Err(error),
    }

    let ctx = resolve_lfs_remote_for_operation_sync("download")?;
    let content = super::block_on_runtime(async {
        ctx.store
            .verify(&pointer.oid)
            .await
            .map_err(CrabError::from)
    })?
    .to_vec();
    verify_lfs_bytes(rel_path, pointer, &content)?;
    crate::lfs::cache::install_bytes(&lfs_dir, &pointer.oid, pointer.size, &content)?;
    Ok(content)
}

fn resolve_crab_content(repo_root: &Path, rel_path: &str, pointer: &Pointer) -> Result<Vec<u8>> {
    let full_path = repo_root.join(rel_path);
    if full_path.is_file() {
        let content = std::fs::read(&full_path).map_err(CrabError::Io)?;
        if Pointer::parse(&content).is_ok() {
            // Hydrate below.
        } else {
            verify_crab_bytes(rel_path, pointer, &content)?;
            return Ok(content);
        }
    } else if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }

    std::fs::write(&full_path, pointer.serialize()).map_err(CrabError::Io)?;
    run_crab_hydrate(repo_root, rel_path)?;

    let content = std::fs::read(&full_path).map_err(CrabError::Io)?;
    verify_crab_bytes(rel_path, pointer, &content)?;
    Ok(content)
}

fn verify_lfs_bytes(rel_path: &str, pointer: &LfsPointer, content: &[u8]) -> Result<()> {
    let actual: [u8; 32] = Sha256::digest(content).into();
    if actual != pointer.oid || content.len() as u64 != pointer.size {
        return Err(CrabError::Configuration {
            key: rel_path.to_owned(),
            origin: "working tree bytes do not match the indexed LFS pointer; commit, stash, or checkout before converting".into(),
        });
    }
    Ok(())
}

fn verify_crab_bytes(rel_path: &str, pointer: &Pointer, content: &[u8]) -> Result<()> {
    let actual: [u8; 32] = *blake3::hash(content).as_bytes();
    if actual != pointer.file_hash || content.len() as u64 != pointer.size {
        return Err(CrabError::Configuration {
            key: rel_path.to_owned(),
            origin: "working tree bytes do not match the indexed Crab pointer; hydrate or checkout before converting".into(),
        });
    }
    Ok(())
}

fn run_crab_hydrate(repo_root: &Path, rel_path: &str) -> Result<()> {
    let bin = crate::cmd::init::crab_binary_path();
    let output = Command::new(bin)
        .args(["hydrate", rel_path, "--json"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run crab hydrate: {e}")))?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "crab hydrate failed for {rel_path}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn write_worktree_file(repo_root: &Path, rel_path: &str, content: &[u8]) -> Result<()> {
    let full_path = repo_root.join(rel_path);
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    std::fs::write(full_path, content).map_err(CrabError::Io)
}

fn write_manifest(repo_root: &Path, candidates: &[ConvertCandidate]) -> Result<()> {
    let manifest_path = manifest_path(repo_root)?;
    if manifest_path.exists() {
        return Err(CrabError::Configuration {
            key: "crab lfs convert".into(),
            origin: "previous conversion manifest exists; run `crab lfs convert --rollback` before another conversion".into(),
        });
    }

    let gitattributes_path = repo_root.join(".gitattributes");
    let gitattributes = match std::fs::read_to_string(&gitattributes_path) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(CrabError::Io(e)),
    };

    let files = candidates
        .iter()
        .map(|candidate| ConvertManifestFile {
            path: candidate.path.clone(),
            index_blob_b64: BASE64_STANDARD.encode(&candidate.index_blob),
        })
        .collect();

    let manifest = ConvertManifest {
        gitattributes,
        files,
    };
    let json = serde_json::to_vec_pretty(&manifest).map_err(|e| {
        CrabError::Internal(format!("failed to serialize conversion manifest: {e}"))
    })?;
    std::fs::write(manifest_path, json).map_err(CrabError::Io)
}

fn restore_gitattributes(repo_root: &Path, content: Option<&str>) -> Result<()> {
    let path = repo_root.join(".gitattributes");
    match content {
        Some(content) => std::fs::write(path, content).map_err(CrabError::Io),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CrabError::Io(e)),
        },
    }
}

fn manifest_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(discover_git_dir(repo_root)?.join(CONVERT_MANIFEST))
}

fn git_ls_files(repo_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-files"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-files: {e}")))?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

fn read_index_blob(repo_root: &Path, rel_path: &str) -> Result<Option<Vec<u8>>> {
    let spec = format!(":{rel_path}");
    let output = Command::new("git")
        .args(["show", &spec])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git show: {e}")))?;
    if output.status.success() {
        Ok(Some(output.stdout))
    } else {
        Ok(None)
    }
}

fn write_blob_to_index(repo_root: &Path, rel_path: &str, blob: &[u8]) -> Result<()> {
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(repo_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git hash-object: {e}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| CrabError::Internal("hash-object stdin missing".into()))?
        .write_all(blob)
        .map_err(|e| CrabError::Internal(format!("failed to write blob to hash-object: {e}")))?;
    let output = child
        .wait_with_output()
        .map_err(|e| CrabError::Internal(format!("failed to wait on git hash-object: {e}")))?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let update = Command::new("git")
        .args([
            "update-index",
            "--add",
            "--replace",
            "--cacheinfo",
            &format!("100644,{sha},{rel_path}"),
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git update-index: {e}")))?;
    if !update.status.success() {
        return Err(CrabError::Internal(format!(
            "git update-index failed for {rel_path}: {}",
            String::from_utf8_lossy(&update.stderr)
        )));
    }
    Ok(())
}

fn discover_git_dir(repo_root: &Path) -> Result<PathBuf> {
    crate::git::discover::discover_common_git_dir_from(repo_root)
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

trait PointerKindExt {
    fn is_lfs_pointer(&self) -> bool;
}

impl PointerKindExt for PointerKind {
    fn is_lfs_pointer(&self) -> bool {
        matches!(self, PointerKind::Lfs(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_index_blobs() {
        let manifest = ConvertManifest {
            gitattributes: Some("*.bin filter=lfs\n".to_owned()),
            files: vec![ConvertManifestFile {
                path: "model.bin".to_owned(),
                index_blob_b64: BASE64_STANDARD.encode(b"pointer"),
            }],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: ConvertManifest = serde_json::from_str(&json).unwrap();
        let blob = BASE64_STANDARD
            .decode(&parsed.files[0].index_blob_b64)
            .unwrap();
        assert_eq!(blob, b"pointer");
    }
}
