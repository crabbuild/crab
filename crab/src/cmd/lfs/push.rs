//! `crab lfs push` and `crab lfs pre-push` — upload LFS objects.
//!
//! Wires the CLI push/pre-push subcommands to
//! [`crate::lfs::batch::BatchResolver`] and
//! [`crate::lfs::lock::LockManager`].

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use crate::core::error::{CrabError, Result};
use crate::lfs::batch::BatchResolver;
use crate::lfs::lock::LockManager;
use crab_git::lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE, hex_encode};
use crab_git::pointer_detect::{PointerKind, classify};

use super::store_setup::{git_user_identity, resolve_lfs_remote_for_operation_with_remote_sync};

#[derive(Debug, Clone, Default)]
pub struct LfsPushOptions {
    pub remote: Option<String>,
    pub args: Vec<String>,
    pub all: bool,
    pub object_id: Option<Option<String>>,
    pub stdin: bool,
    pub dry_run: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ResolvedPushArgs {
    remote: Option<String>,
    refs: Vec<String>,
    object_ids: Vec<String>,
}

/// Run `crab lfs push`.
///
/// Uploads missing LFS objects to the remote store. Supports `--all`
/// to upload every locally-known object, `--object-id` to upload a
/// single object by OID, and `--dry-run` to preview what would be pushed.
pub fn run_lfs_push(options: LfsPushOptions) -> Result<()> {
    let resolved = resolve_push_args(&options)?;

    if !resolved.object_ids.is_empty() {
        let pointers: Vec<LfsPointer> = resolved
            .object_ids
            .iter()
            .map(|oid_hex| {
                parse_oid_hex(oid_hex).map(|oid| LfsPointer {
                    oid,
                    size: 0,
                    extensions: Vec::new(),
                })
            })
            .collect::<Result<_>>()?;

        if options.dry_run {
            eprintln!("push: would upload {} object(s)", pointers.len());
            for oid_hex in &resolved.object_ids {
                eprintln!("  {}", &oid_hex[..oid_hex.len().min(10)]);
            }
            return Ok(());
        }

        let ctx =
            resolve_lfs_remote_for_operation_with_remote_sync("push", resolved.remote.as_deref())?;
        return super::block_on_runtime(async {
            let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config);
            resolver.upload_missing(&pointers).await?;
            eprintln!("push: uploaded {} object(s)", pointers.len());
            Ok(())
        });
    }

    let ctx =
        resolve_lfs_remote_for_operation_with_remote_sync("push", resolved.remote.as_deref())?;
    let pointers = collect_push_pointers(options.all, &resolved.refs)?;

    if pointers.is_empty() {
        eprintln!("push: no LFS objects to push");
        return Ok(());
    }

    super::block_on_runtime(async {
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config);
        let missing = resolver.find_missing_for_push(&pointers).await?;

        if missing.is_empty() {
            eprintln!("push: all objects up to date");
            return Ok(());
        }

        if options.dry_run {
            eprintln!("push: would upload {} object(s)", missing.len());
            for ptr in &missing {
                eprintln!("  {}", &hex_encode(&ptr.oid)[..10]);
            }
            return Ok(());
        }

        eprintln!("push: uploading {} object(s)", missing.len());
        let progress = super::progress::TransferProgress::new("Uploading", missing.len() as u64);
        resolver.upload_missing(&missing).await?;
        progress.finish();
        super::logs::log_transfer_event("push", missing.len() as u64, progress.elapsed_secs());
        eprintln!("push: done");
        Ok(())
    })
}

fn resolve_push_args(options: &LfsPushOptions) -> Result<ResolvedPushArgs> {
    let stdin_values = if options.stdin {
        read_stdin_lines()?
    } else {
        Vec::new()
    };

    if options.object_id.is_some() {
        if options.all {
            return Err(CrabError::Configuration {
                key: "lfs push".to_owned(),
                origin: "--all cannot be combined with --object-id".to_owned(),
            });
        }

        let mut remote = None;
        let mut object_ids = Vec::new();
        if let Some(value) = options.object_id.as_ref().and_then(|value| value.as_ref()) {
            if is_oid_hex(value) {
                object_ids.push(value.clone());
            } else {
                remote = Some(value.clone());
            }
        }
        if let Some(remote_or_oid) = &options.remote {
            if is_oid_hex(remote_or_oid) {
                object_ids.push(remote_or_oid.clone());
            } else if remote.is_none() {
                remote = Some(remote_or_oid.clone());
            }
        }
        object_ids.extend(
            options
                .args
                .iter()
                .filter(|value| is_oid_hex(value))
                .cloned(),
        );

        if !stdin_values.is_empty() {
            if !object_ids.is_empty() {
                return Err(CrabError::Configuration {
                    key: "lfs push".to_owned(),
                    origin: "--stdin reads object IDs instead of command-line object IDs"
                        .to_owned(),
                });
            }
            object_ids.extend(stdin_values);
        }

        if object_ids.is_empty() {
            return Err(CrabError::Configuration {
                key: "lfs push".to_owned(),
                origin: "--object-id requires at least one object ID or --stdin".to_owned(),
            });
        }

        return Ok(ResolvedPushArgs {
            remote,
            refs: Vec::new(),
            object_ids,
        });
    }

    if options.stdin && !options.args.is_empty() {
        return Err(CrabError::Configuration {
            key: "lfs push".to_owned(),
            origin: "--stdin reads refs instead of command-line refs".to_owned(),
        });
    }

    let refs = if options.stdin {
        stdin_values
    } else {
        options.args.clone()
    };

    Ok(ResolvedPushArgs {
        remote: options.remote.clone(),
        refs,
        object_ids: Vec::new(),
    })
}

fn read_stdin_lines() -> Result<Vec<String>> {
    let stdin = std::io::stdin();
    stdin
        .lock()
        .lines()
        .map(|line| {
            line.map(|line| line.trim().to_owned())
                .map_err(CrabError::Io)
        })
        .filter(|line| !matches!(line, Ok(value) if value.is_empty()))
        .collect()
}

fn is_oid_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Run `crab lfs pre-push`.
///
/// Invoked by the pre-push hook. Reads ref updates from stdin, collects
/// LFS pointers from pushed commits, checks lock conflicts, uploads
/// missing objects, and exits non-zero on failure.
pub fn run_lfs_pre_push() -> Result<()> {
    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("push", None)?;

    // Read ref updates from stdin (git pre-push hook format):
    // <local-ref> <local-sha> <remote-ref> <remote-sha>
    let stdin = std::io::stdin();
    let mut local_shas = Vec::new();
    let mut remote_shas = Vec::new();

    for line in stdin.lock().lines() {
        let line = line.map_err(CrabError::Io)?;
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let local_sha = parts[1];
        let remote_sha = parts[3];

        // Skip delete ref updates (local SHA all zeros).
        if local_sha.chars().all(|c| c == '0') {
            continue;
        }

        local_shas.push(local_sha.to_owned());
        if !remote_sha.chars().all(|c| c == '0') {
            remote_shas.push(remote_sha.to_owned());
        }
    }

    if local_shas.is_empty() {
        return Ok(());
    }

    // Collect LFS pointers from the commits being pushed.
    let pointers = collect_pointers_from_range(&local_shas, &remote_shas)?;

    if pointers.is_empty() {
        return Ok(());
    }

    super::block_on_runtime(async {
        // Check lock conflicts.
        let owner = git_user_identity().unwrap_or_default();
        let lock_store = crate::storage::Store::from_storage(ctx.store.store().clone());
        let lock_mgr = LockManager::lfs(lock_store, &ctx.prefix);
        let paths: Vec<String> = pointers.iter().map(|(p, _)| p.clone()).collect();
        let conflicts = lock_mgr.check_conflicts(&paths, &owner).await?;
        if !conflicts.is_empty() {
            for c in &conflicts {
                eprintln!(
                    "pre-push: lock conflict on {} (locked by {})",
                    c.path, c.owner
                );
            }
            return Err(CrabError::LfsLockConflict {
                path: conflicts[0].path.clone(),
                owner: conflicts[0].owner.clone(),
            });
        }

        // Upload missing objects.
        let ptrs: Vec<LfsPointer> = pointers.into_iter().map(|(_, p)| p).collect();
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config);
        let missing = resolver.find_missing_for_push(&ptrs).await?;

        if !missing.is_empty() {
            eprintln!("pre-push: uploading {} LFS object(s)", missing.len());
            resolver.upload_missing(&missing).await?;
        }

        Ok(())
    })
}

/// Collect LFS pointers from HEAD (or all refs for `--all`).
fn collect_push_pointers(all: bool, refs: &[String]) -> Result<Vec<LfsPointer>> {
    let tree_lines = if all {
        if refs.is_empty() {
            ls_tree_all_refs()?
        } else {
            ls_tree_refs(refs)?
        }
    } else if refs.is_empty() {
        ls_tree_head()?
    } else {
        ls_tree_refs(refs)?
    };

    batch_read_pointers(&tree_lines)
}

/// Collect `(path, LfsPointer)` pairs from a commit range being pushed.
fn collect_pointers_from_range(
    local_shas: &[String],
    remote_shas: &[String],
) -> Result<Vec<(String, LfsPointer)>> {
    collect_pointers_from_range_in(Path::new("."), local_shas, remote_shas)
}

pub(crate) fn collect_lfs_object_ids_from_range_in(
    repo_dir: &Path,
    local_shas: &[String],
    remote_shas: &[String],
) -> Result<Vec<String>> {
    let mut object_ids: Vec<String> =
        collect_pointers_from_range_in(repo_dir, local_shas, remote_shas)?
            .into_iter()
            .map(|(_, pointer)| hex_encode(&pointer.oid))
            .collect();
    object_ids.sort();
    object_ids.dedup();
    Ok(object_ids)
}

pub(crate) fn collect_pointers_from_range_in(
    repo_dir: &Path,
    local_shas: &[String],
    remote_shas: &[String],
) -> Result<Vec<(String, LfsPointer)>> {
    // Build rev-list args: local_sha ^remote_sha ...
    let mut args = vec!["rev-list".to_owned(), "--objects".to_owned()];
    for sha in local_shas {
        args.push(sha.clone());
    }
    for sha in remote_shas {
        args.push(format!("^{sha}"));
    }

    let output = git_command_in(repo_dir)
        .args(&args)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git rev-list: {e}")))?;

    if !output.status.success() {
        // Fall back to HEAD scan if rev-list fails.
        let ptrs = batch_read_pointers_in(repo_dir, &ls_tree_head_in(repo_dir)?)?;
        return Ok(ptrs.into_iter().map(|p| (String::new(), p)).collect());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut blob_paths: Vec<(String, String)> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Lines with paths: "<hash> <path>"
        if let Some((hash, path)) = line.split_once(' ') {
            blob_paths.push((hash.to_owned(), path.to_owned()));
        }
    }

    if blob_paths.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-read blobs to find LFS pointers.
    let oids_input: String = blob_paths
        .iter()
        .map(|(hash, _)| hash.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let stdout = cat_file_batch_stdout(repo_dir, oids_input)?;
    let mut entries = Vec::new();
    let mut pos = 0;
    let mut line_idx = 0;

    while pos < stdout.len() && line_idx < blob_paths.len() {
        let header_end = match stdout[pos..].iter().position(|&b| b == b'\n') {
            Some(p) => pos + p,
            None => break,
        };

        let header = String::from_utf8_lossy(&stdout[pos..header_end]);
        let parts: Vec<&str> = header.split_whitespace().collect();

        if parts.len() < 3 || parts[1] == "missing" {
            pos = header_end + 1;
            line_idx += 1;
            continue;
        }

        let Ok(obj_size) = parts[2].parse::<usize>() else {
            pos = header_end + 1;
            line_idx += 1;
            continue;
        };

        let content_start = header_end + 1;
        let content_end = content_start + obj_size;

        if content_end > stdout.len() {
            break;
        }

        let content = &stdout[content_start..content_end];
        let (_, path) = &blob_paths[line_idx];

        if parts[1] == "blob"
            && content.len() <= MAX_LFS_POINTER_SIZE
            && let PointerKind::Lfs(pointer) = classify(content)
            && pointer.size > 0
        {
            entries.push((path.clone(), pointer));
        }

        pos = content_end + 1;
        line_idx += 1;
    }

    Ok(entries)
}

/// Batch-read blobs and extract LFS pointers (without paths).
fn batch_read_pointers(tree_lines: &[(String, String)]) -> Result<Vec<LfsPointer>> {
    batch_read_pointers_in(Path::new("."), tree_lines)
}

fn batch_read_pointers_in(
    repo_dir: &Path,
    tree_lines: &[(String, String)],
) -> Result<Vec<LfsPointer>> {
    if tree_lines.is_empty() {
        return Ok(Vec::new());
    }

    let oids_input: String = tree_lines
        .iter()
        .map(|(hash, _)| hash.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let stdout = cat_file_batch_stdout(repo_dir, oids_input)?;
    let mut pointers = Vec::new();
    let mut seen = HashSet::new();
    let mut pos = 0;
    let mut line_idx = 0;

    while pos < stdout.len() && line_idx < tree_lines.len() {
        let header_end = match stdout[pos..].iter().position(|&b| b == b'\n') {
            Some(p) => pos + p,
            None => break,
        };

        let header = String::from_utf8_lossy(&stdout[pos..header_end]);
        let parts: Vec<&str> = header.split_whitespace().collect();

        if parts.len() < 3 || parts[1] == "missing" {
            pos = header_end + 1;
            line_idx += 1;
            continue;
        }

        let Ok(obj_size) = parts[2].parse::<usize>() else {
            pos = header_end + 1;
            line_idx += 1;
            continue;
        };

        let content_start = header_end + 1;
        let content_end = content_start + obj_size;

        if content_end > stdout.len() {
            break;
        }

        let content = &stdout[content_start..content_end];

        if parts[1] == "blob"
            && content.len() <= MAX_LFS_POINTER_SIZE
            && let PointerKind::Lfs(pointer) = classify(content)
            && pointer.size > 0
            && seen.insert(pointer.oid)
        {
            pointers.push(pointer);
        }

        pos = content_end + 1;
        line_idx += 1;
    }

    Ok(pointers)
}

fn cat_file_batch_stdout(repo_dir: &Path, oids_input: String) -> Result<Vec<u8>> {
    let mut child = git_command_in(repo_dir)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git cat-file: {e}")))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CrabError::Internal("git cat-file stdin unavailable".to_owned()))?;
    let stdin_writer = thread::spawn(move || stdin.write_all(oids_input.as_bytes()));

    let output = child
        .wait_with_output()
        .map_err(|e| CrabError::Internal(format!("git cat-file failed: {e}")))?;

    let write_result = stdin_writer
        .join()
        .map_err(|_| CrabError::Internal("git cat-file stdin writer panicked".to_owned()))?;
    write_result
        .map_err(|e| CrabError::Internal(format!("failed to write git cat-file input: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "git cat-file exited with status {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    Ok(output.stdout)
}

fn ls_tree_head() -> Result<Vec<(String, String)>> {
    ls_tree_ref_in(Path::new("."), "HEAD")
}

fn ls_tree_head_in(repo_dir: &Path) -> Result<Vec<(String, String)>> {
    ls_tree_ref_in(repo_dir, "HEAD")
}

fn ls_tree_ref(ref_name: &str) -> Result<Vec<(String, String)>> {
    ls_tree_ref_in(Path::new("."), ref_name)
}

fn ls_tree_ref_in(repo_dir: &Path, ref_name: &str) -> Result<Vec<(String, String)>> {
    let output = git_command_in(repo_dir)
        .args(["ls-tree", "-r", ref_name])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-tree: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name") {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!(
            "git ls-tree failed for {ref_name}: {stderr}"
        )));
    }

    Ok(parse_ls_tree_output(&output.stdout))
}

fn git_command_in(repo_dir: &Path) -> Command {
    let mut command = Command::new("git");
    // Git sets these variables when invoking a remote helper. The scan owns
    // an explicit repository directory; inherited relative values would be
    // resolved again after changing directory and point at `.git/.git`.
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .current_dir(repo_dir);
    command
}

fn ls_tree_refs(refs: &[String]) -> Result<Vec<(String, String)>> {
    let mut all_entries = Vec::new();
    for ref_name in refs {
        all_entries.extend(ls_tree_ref(ref_name)?);
    }
    Ok(all_entries)
}

fn ls_tree_all_refs() -> Result<Vec<(String, String)>> {
    let refs_output = Command::new("git")
        .args(["rev-parse", "--all"])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git rev-parse: {e}")))?;

    if !refs_output.status.success() {
        return Ok(Vec::new());
    }

    let refs_text = String::from_utf8_lossy(&refs_output.stdout);
    let mut all_entries = Vec::new();

    for ref_sha in refs_text.lines() {
        let ref_sha = ref_sha.trim();
        if ref_sha.is_empty() {
            continue;
        }
        let output = Command::new("git")
            .args(["ls-tree", "-r", ref_sha])
            .output()
            .map_err(|e| {
                CrabError::Internal(format!("failed to run git ls-tree for {ref_sha}: {e}"))
            })?;
        if output.status.success() {
            let entries = parse_ls_tree_output(&output.stdout);
            all_entries.extend(entries);
        }
    }

    Ok(all_entries)
}

fn parse_ls_tree_output(output: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(output);
    let mut results = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((meta, filename)) = line.split_once('\t') else {
            continue;
        };
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 3 || parts[1] != "blob" {
            continue;
        }
        results.push((parts[2].to_owned(), filename.to_owned()));
    }
    results
}

/// Parse a hex OID string into 32 bytes.
fn parse_oid_hex(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 {
        return Err(CrabError::Configuration {
            key: format!(
                "invalid OID length: expected 64 hex chars, got {}",
                hex.len()
            ),
            origin: hex.to_owned(),
        });
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(hex.as_bytes()[i * 2]).map_err(|()| CrabError::Configuration {
            key: "invalid hex char in OID".into(),
            origin: hex.to_owned(),
        })?;
        let lo = hex_nibble(hex.as_bytes()[i * 2 + 1]).map_err(|()| CrabError::Configuration {
            key: "invalid hex char in OID".into(),
            origin: hex.to_owned(),
        })?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> std::result::Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    #[test]
    fn resolve_push_args_accepts_legacy_single_object_id() {
        let options = LfsPushOptions {
            object_id: Some(Some(oid(1))),
            ..LfsPushOptions::default()
        };

        let resolved = resolve_push_args(&options).unwrap();
        assert_eq!(
            resolved,
            ResolvedPushArgs {
                remote: None,
                refs: Vec::new(),
                object_ids: vec![oid(1)],
            }
        );
    }

    #[test]
    fn resolve_push_args_accepts_multiple_object_ids() {
        let options = LfsPushOptions {
            remote: Some(oid(2)),
            args: vec![oid(3)],
            object_id: Some(Some(oid(1))),
            ..LfsPushOptions::default()
        };

        let resolved = resolve_push_args(&options).unwrap();
        assert_eq!(resolved.remote, None);
        assert_eq!(resolved.object_ids, vec![oid(1), oid(2), oid(3)]);
    }

    #[test]
    fn resolve_push_args_treats_non_oid_object_id_value_as_remote() {
        let options = LfsPushOptions {
            remote: Some(oid(4)),
            object_id: Some(Some("origin".to_owned())),
            ..LfsPushOptions::default()
        };

        let resolved = resolve_push_args(&options).unwrap();
        assert_eq!(resolved.remote.as_deref(), Some("origin"));
        assert_eq!(resolved.object_ids, vec![oid(4)]);
    }

    #[test]
    fn resolve_push_args_keeps_ref_operands() {
        let options = LfsPushOptions {
            remote: Some("origin".to_owned()),
            args: vec!["main".to_owned(), "release".to_owned()],
            ..LfsPushOptions::default()
        };

        let resolved = resolve_push_args(&options).unwrap();
        assert_eq!(resolved.remote.as_deref(), Some("origin"));
        assert_eq!(resolved.refs, vec!["main", "release"]);
    }

    #[test]
    fn resolve_push_args_keeps_object_id_remote_operand() {
        let options = LfsPushOptions {
            remote: Some("origin".to_owned()),
            args: vec![oid(2)],
            object_id: Some(Some(oid(1))),
            ..LfsPushOptions::default()
        };

        let resolved = resolve_push_args(&options).unwrap();

        assert_eq!(resolved.remote.as_deref(), Some("origin"));
        assert_eq!(resolved.object_ids, vec![oid(1), oid(2)]);
    }

    #[test]
    fn cat_file_batch_stdout_handles_large_object_lists() {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .current_dir(dir.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());

        let blob_dir = dir.path().join("blobs");
        std::fs::create_dir(&blob_dir).unwrap();
        let mut paths = String::new();
        for index in 0..2048 {
            let file_name = format!("blob-{index:04}.txt");
            let path = blob_dir.join(&file_name);
            std::fs::write(path, format!("blob-{index:04}-{}\n", "x".repeat(256))).unwrap();
            paths.push_str("blobs/");
            paths.push_str(&file_name);
            paths.push('\n');
        }

        let mut child = Command::new("git")
            .current_dir(dir.path())
            .args(["hash-object", "-w", "--stdin-paths"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(paths.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());

        let object_ids = String::from_utf8(output.stdout).unwrap();
        let mut cat_file_input = object_ids.trim_end().to_owned();
        cat_file_input.push('\n');
        assert!(cat_file_input.len() > 64 * 1024);

        let stdout = cat_file_batch_stdout(dir.path(), cat_file_input).unwrap();
        let text = String::from_utf8_lossy(&stdout);
        assert_eq!(text.matches(" blob ").count(), 2048);
    }

    #[test]
    fn repository_commands_clear_remote_helper_git_context() {
        let command = git_command_in(Path::new("repo/.git"));
        let overrides: std::collections::HashMap<_, _> = command.get_envs().collect();

        assert_eq!(overrides.get(std::ffi::OsStr::new("GIT_DIR")), Some(&None));
        assert_eq!(
            overrides.get(std::ffi::OsStr::new("GIT_WORK_TREE")),
            Some(&None)
        );
        assert_eq!(
            overrides.get(std::ffi::OsStr::new("GIT_COMMON_DIR")),
            Some(&None)
        );
    }
}
