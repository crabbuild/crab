//! `crab lfs push` and `crab lfs pre-push` — upload LFS objects.
//!
//! Wires the CLI push/pre-push subcommands to
//! [`crate::lfs::batch::BatchResolver`] and
//! [`crate::lfs::lock::LockManager`].

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
#[cfg(test)]
use std::thread;

use crate::core::error::{CrabError, Result};
use crate::lfs::batch::BatchResolver;
use crate::lfs::lock::LockManager;
use crab_git::lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE, hex_encode};
use crab_git::pointer_detect::{PointerKind, classify};

use super::store_setup::{
    git_user_identity, resolve_lfs_remote_for_operation_with_remote_sync, validate_git_push_url,
};

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
pub fn run_lfs_pre_push(remote: Option<&str>, url: Option<&str>) -> Result<()> {
    if remote.is_none() && url.is_some() {
        return Err(CrabError::Configuration {
            key: "lfs pre-push".to_owned(),
            origin: "Git supplied a remote URL without a remote name".to_owned(),
        });
    }
    if let (Some(remote), Some(url)) = (remote, url) {
        validate_git_push_url(remote, url)?;
    }
    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("push", remote)?;

    // Read ref updates from stdin (git pre-push hook format):
    // <local-ref> <local-sha> <remote-ref> <remote-sha>
    let stdin = std::io::stdin();
    let mut input = String::new();
    stdin
        .lock()
        .read_to_string(&mut input)
        .map_err(CrabError::Io)?;
    let (local_shas, remote_shas) = parse_pre_push_input(&input)?;

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

fn parse_pre_push_input(input: &str) -> Result<(Vec<String>, Vec<String>)> {
    let mut local_shas = Vec::new();
    let mut remote_shas = Vec::new();

    for (line_number, line) in input.lines().enumerate() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        if parts.len() != 4 {
            return Err(CrabError::Configuration {
                key: "lfs pre-push".to_owned(),
                origin: format!("invalid ref update on input line {}", line_number + 1),
            });
        }

        let local_sha = parts[1];
        let remote_sha = parts[3];
        if !is_git_object_id(local_sha) || !is_git_object_id(remote_sha) {
            return Err(CrabError::Configuration {
                key: "lfs pre-push".to_owned(),
                origin: format!("invalid object ID on input line {}", line_number + 1),
            });
        }

        // Skip delete ref updates (local SHA all zeros).
        if local_sha.bytes().all(|c| c == b'0') {
            continue;
        }

        local_shas.push(local_sha.to_owned());
        if !remote_sha.bytes().all(|c| c == b'0') {
            remote_shas.push(remote_sha.to_owned());
        }
    }

    Ok((local_shas, remote_shas))
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Collect LFS pointers from HEAD (or all refs for `--all`).
fn collect_push_pointers(all: bool, refs: &[String]) -> Result<Vec<LfsPointer>> {
    let ref_names = if all {
        if refs.is_empty() {
            all_ref_names()?
        } else {
            refs.to_vec()
        }
    } else if refs.is_empty() {
        vec!["HEAD".to_owned()]
    } else {
        refs.to_vec()
    };

    let mut pointers = Vec::new();
    let mut seen = HashSet::new();
    for ref_name in ref_names {
        visit_lfs_pointers_in_tree(Path::new("."), &ref_name, |_, pointer| {
            if seen.insert(pointer.oid) {
                pointers.push(pointer);
            }
            Ok(())
        })?;
    }
    Ok(pointers)
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
    // Git's blob:limit filter omits blobs at least the given size. LFS
    // pointers are bounded, so the scan never streams ordinary large blobs
    // through cat-file merely to prove that they are not pointers.
    let mut args = vec![
        "rev-list".to_owned(),
        "--objects".to_owned(),
        "-z".to_owned(),
        format!("--filter=blob:limit={}", MAX_LFS_POINTER_SIZE + 1),
    ];
    for sha in local_shas {
        args.push(sha.clone());
    }
    for sha in remote_shas {
        args.push(format!("^{sha}"));
    }
    let mut entries = Vec::new();
    visit_lfs_blobs_in_git_command(repo_dir, &args, parse_rev_list_record, |path, pointer| {
        entries.push((path, pointer));
        Ok(())
    })?;
    Ok(entries)
}

fn visit_lfs_pointers_in_tree(
    repo_dir: &Path,
    ref_name: &str,
    mut visitor: impl FnMut(String, LfsPointer) -> Result<()>,
) -> Result<()> {
    let args = vec![
        "ls-tree".to_owned(),
        "-r".to_owned(),
        "-z".to_owned(),
        ref_name.to_owned(),
    ];
    let mut seen = HashSet::new();
    visit_lfs_blobs_in_git_command(repo_dir, &args, parse_ls_tree_record, |path, pointer| {
        if seen.insert(pointer.oid) {
            visitor(path, pointer)?;
        }
        Ok(())
    })
}

fn visit_lfs_blobs_in_git_command(
    repo_dir: &Path,
    args: &[String],
    parse_record: fn(&[u8], &mut Option<String>) -> Result<Option<(String, String)>>,
    mut visitor: impl FnMut(String, LfsPointer) -> Result<()>,
) -> Result<()> {
    let mut producer = git_command_in(repo_dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| CrabError::Internal(format!("failed to spawn git discovery: {error}")))?;
    let mut cat = git_command_in(repo_dir)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| CrabError::Internal(format!("failed to spawn git cat-file: {error}")))?;

    let producer_stdout = producer
        .stdout
        .take()
        .ok_or_else(|| CrabError::Internal("git discovery stdout unavailable".to_owned()))?;
    let mut producer_stdout = BufReader::new(producer_stdout);
    let mut cat_stdin = BufWriter::new(
        cat.stdin
            .take()
            .ok_or_else(|| CrabError::Internal("git cat-file stdin unavailable".to_owned()))?,
    );
    let cat_stdout = cat
        .stdout
        .take()
        .ok_or_else(|| CrabError::Internal("git cat-file stdout unavailable".to_owned()))?;
    let mut cat_stdout = BufReader::new(cat_stdout);

    let scan_result = (|| -> Result<()> {
        let mut record = Vec::new();
        let mut parser_state = None;
        loop {
            record.clear();
            let read = producer_stdout
                .read_until(0, &mut record)
                .map_err(|error| {
                    CrabError::Internal(format!("failed to read git discovery: {error}"))
                })?;
            if read == 0 {
                break;
            }
            let terminated = record.last() == Some(&0);
            if terminated {
                record.pop();
            }
            let Some((oid, path)) = parse_record(&record, &mut parser_state)? else {
                continue;
            };

            cat_stdin
                .write_all(oid.as_bytes())
                .and_then(|_| cat_stdin.write_all(b"\n"))
                .and_then(|_| cat_stdin.flush())
                .map_err(|error| {
                    CrabError::Internal(format!("failed to query git cat-file: {error}"))
                })?;

            let mut header = Vec::new();
            cat_stdout.read_until(b'\n', &mut header).map_err(|error| {
                CrabError::Internal(format!("failed to read git cat-file: {error}"))
            })?;
            if header.last() != Some(&b'\n') {
                return Err(CrabError::Internal(
                    "git cat-file returned a truncated object header".to_owned(),
                ));
            }
            header.pop();
            let parts: Vec<&str> = std::str::from_utf8(&header)
                .map_err(|error| {
                    CrabError::Internal(format!("git cat-file returned invalid UTF-8: {error}"))
                })?
                .split_whitespace()
                .collect();
            if parts.len() < 2 || parts[1] == "missing" {
                return Err(CrabError::Internal(format!(
                    "git cat-file could not read object {oid}"
                )));
            }
            if parts.len() != 3 || parts[0] != oid {
                return Err(CrabError::Internal(
                    "git cat-file returned a malformed object header".to_owned(),
                ));
            }
            let object_size = parts[2].parse::<u64>().map_err(|_| {
                CrabError::Internal("git cat-file returned an invalid object size".to_owned())
            })?;

            let content = if object_size <= MAX_LFS_POINTER_SIZE as u64 {
                let size = usize::try_from(object_size).map_err(|_| {
                    CrabError::Internal(
                        "git cat-file object size does not fit in memory".to_owned(),
                    )
                })?;
                let mut content = vec![0; size];
                cat_stdout.read_exact(&mut content).map_err(|error| {
                    CrabError::Internal(format!(
                        "git cat-file returned truncated object content: {error}"
                    ))
                })?;
                Some(content)
            } else {
                let mut limited = (&mut cat_stdout).take(object_size);
                io::copy(&mut limited, &mut io::sink()).map_err(|error| {
                    CrabError::Internal(format!("failed to discard git object content: {error}"))
                })?;
                None
            };
            let mut separator = [0u8; 1];
            cat_stdout.read_exact(&mut separator).map_err(|error| {
                CrabError::Internal(format!(
                    "git cat-file returned an unterminated object record: {error}"
                ))
            })?;
            if separator[0] != b'\n' {
                return Err(CrabError::Internal(
                    "git cat-file returned an unterminated object record".to_owned(),
                ));
            }

            if parts[1] == "blob"
                && let Some(content) = content
                && let PointerKind::Lfs(pointer) = classify(&content)
                && pointer.size > 0
            {
                visitor(path, pointer)?;
            }
            if !terminated {
                break;
            }
        }
        Ok(())
    })();

    let scan_failed = scan_result.is_err();
    drop(cat_stdin);
    if scan_failed {
        // A callback or framing error can leave either producer blocked on a
        // full pipe. Kill both children before waiting so malformed input
        // cannot turn a bounded scan into a process deadlock.
        let _ = producer.kill();
        let _ = cat.kill();
    }
    drop(producer_stdout);
    drop(cat_stdout);
    let cat_output = cat
        .wait_with_output()
        .map_err(|error| CrabError::Internal(format!("git cat-file failed: {error}")))?;
    let producer_output = producer
        .wait_with_output()
        .map_err(|error| CrabError::Internal(format!("git discovery failed: {error}")))?;

    if let Err(error) = scan_result {
        return Err(error);
    }
    if !producer_output.status.success() {
        let command = args.first().map(String::as_str).unwrap_or("git discovery");
        return Err(CrabError::Internal(format!(
            "git {command} failed: {}",
            String::from_utf8_lossy(&producer_output.stderr).trim()
        )));
    }
    if !cat_output.status.success() {
        return Err(CrabError::Internal(format!(
            "git cat-file exited with status {}: {}",
            cat_output.status,
            String::from_utf8_lossy(&cat_output.stderr).trim()
        )));
    }
    Ok(())
}

fn parse_rev_list_record(
    record: &[u8],
    pending_oid: &mut Option<String>,
) -> Result<Option<(String, String)>> {
    if let Some(path) = record.strip_prefix(b"path=") {
        let oid = pending_oid.take().ok_or_else(|| {
            CrabError::Internal("git rev-list returned a path without an object ID".to_owned())
        })?;
        if path.is_empty() {
            return Err(CrabError::Internal(
                "git rev-list returned an empty object path".to_owned(),
            ));
        }
        return Ok(Some((oid, String::from_utf8_lossy(path).into_owned())));
    }

    let oid = std::str::from_utf8(record).map_err(|_| {
        CrabError::Internal("git rev-list returned a malformed object record".to_owned())
    })?;
    if !is_git_object_id(oid) {
        return Err(CrabError::Internal(
            "git rev-list returned a malformed object record".to_owned(),
        ));
    }
    // `rev-list --objects -z` emits an object ID record followed by a
    // `path=...` record only for objects that have a name. Commits and trees
    // therefore leave a pending ID that is deliberately discarded by the
    // next ID or at EOF.
    *pending_oid = Some(oid.to_owned());
    Ok(None)
}

fn parse_ls_tree_record(
    record: &[u8],
    _pending_oid: &mut Option<String>,
) -> Result<Option<(String, String)>> {
    let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
        return Err(CrabError::Internal(
            "git ls-tree returned a malformed object record".to_owned(),
        ));
    };
    let (meta, path) = record.split_at(separator);
    let path = &path[1..];
    let parts: Vec<&str> = std::str::from_utf8(meta)
        .map_err(|_| CrabError::Internal("git ls-tree returned invalid UTF-8".to_owned()))?
        .split_whitespace()
        .collect();
    if parts.len() != 3 {
        return Err(CrabError::Internal(
            "git ls-tree returned a malformed object record".to_owned(),
        ));
    }
    if parts[1] != "blob" {
        return Ok(None);
    }
    if !is_git_object_id(parts[2]) || path.is_empty() {
        return Err(CrabError::Internal(
            "git ls-tree returned a malformed object record".to_owned(),
        ));
    }
    Ok(Some((
        parts[2].to_owned(),
        String::from_utf8_lossy(path).into_owned(),
    )))
}

fn all_ref_names() -> Result<Vec<String>> {
    let output = git_command_in(Path::new("."))
        .args(["rev-parse", "--all"])
        .output()
        .map_err(|error| CrabError::Internal(format!("failed to run git rev-parse: {error}")))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            (!line.is_empty()).then(|| line.to_owned())
        })
        .collect())
}

/// Retained for the focused pipe/backpressure regression test.
#[cfg(test)]
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
    use sha2::Digest;

    fn oid(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    fn git_oid(byte: u8) -> String {
        format!("{byte:02x}").repeat(20)
    }

    #[test]
    fn pre_push_parser_preserves_updates_and_skips_deletes() {
        let input = format!(
            "refs/heads/main {} refs/heads/main {}\nrefs/heads/old {} refs/heads/old {}\n",
            git_oid(1),
            git_oid(2),
            "0".repeat(40),
            git_oid(3),
        );

        let (local, remote) = parse_pre_push_input(&input).unwrap();

        assert_eq!(local, vec![git_oid(1)]);
        assert_eq!(remote, vec![git_oid(2)]);
    }

    #[test]
    fn pre_push_parser_rejects_malformed_updates() {
        let error = parse_pre_push_input("refs/heads/main only-three-fields\n").unwrap_err();

        assert!(matches!(error, CrabError::Configuration { .. }));
    }

    #[test]
    fn range_scan_does_not_fall_back_when_rev_list_fails() {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .current_dir(dir.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());

        let error =
            collect_pointers_from_range_in(dir.path(), &["not-a-git-object".to_owned()], &[])
                .unwrap_err();

        assert!(format!("{error}").contains("git rev-list failed"));
    }

    #[test]
    fn range_scan_streams_zipped_rev_list_and_cat_file_records() {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                Command::new("git")
                    .current_dir(dir.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let content = b"streamed lfs object";
        let pointer = LfsPointer {
            oid: sha2::Sha256::digest(content).into(),
            size: content.len() as u64,
            extensions: Vec::new(),
        };
        std::fs::write(dir.path().join("asset.bin"), pointer.serialize()).unwrap();
        std::fs::write(
            dir.path().join("large.bin"),
            vec![b'x'; MAX_LFS_POINTER_SIZE + 1024],
        )
        .unwrap();
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["commit", "--quiet", "-m", "fixture"])
                .status()
                .unwrap()
                .success()
        );
        let head = String::from_utf8(
            Command::new("git")
                .current_dir(dir.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();

        let entries = collect_pointers_from_range_in(dir.path(), &[head], &[]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "asset.bin");
        assert_eq!(entries[0].1, pointer);
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
