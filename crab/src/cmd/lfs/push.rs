//! `crab lfs push` and `crab lfs pre-push` — upload LFS objects.
//!
//! Wires the CLI push/pre-push subcommands to
//! [`crate::lfs::batch::BatchResolver`] and
//! [`crate::lfs::lock::LockManager`].

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{ChildStdin, Command};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::process::{self, GIT_ENV_REMOVALS, MAX_CAPTURE_BYTES};
use crate::lfs::batch::BatchResolver;
use crate::lfs::lock::LockManager;
use crab_git::lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE, hex_encode};
use crab_git::pointer_detect::{PointerKind, classify};
use crab_git::pre_push::{PrePushUpdate, read_pre_push};

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
pub fn run_lfs_push(options: LfsPushOptions, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let resolved = resolve_push_args(&options)?;

    if !resolved.object_ids.is_empty() {
        if options.dry_run {
            eprintln!("push: would upload {} object(s)", resolved.object_ids.len());
            for oid_hex in &resolved.object_ids {
                eprintln!("  {}", &oid_hex[..oid_hex.len().min(10)]);
            }
            return Ok(());
        }

        let ctx =
            resolve_lfs_remote_for_operation_with_remote_sync("push", resolved.remote.as_deref())?;
        let pointers = object_id_pointers(&ctx.local_lfs_dir, &resolved.object_ids)?;
        return super::block_on_runtime(async {
            let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config);
            resolver.upload_missing(&pointers).await?;
            eprintln!("push: uploaded {} object(s)", pointers.len());
            Ok(())
        });
    }

    let ctx =
        resolve_lfs_remote_for_operation_with_remote_sync("push", resolved.remote.as_deref())?;
    let pointers = collect_push_pointers(options.all, &resolved.refs, cancel)?;

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

fn object_id_pointers(lfs_dir: &Path, object_ids: &[String]) -> Result<Vec<LfsPointer>> {
    object_ids
        .iter()
        .map(|oid_hex| {
            let oid = parse_oid_hex(oid_hex)?;
            let path = crate::lfs::cache::object_path(lfs_dir, &oid);
            let metadata = std::fs::metadata(&path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    CrabError::LfsObjectMissing {
                        oid: oid_hex.clone(),
                    }
                } else {
                    CrabError::Io(error)
                }
            })?;
            if !metadata.is_file() {
                return Err(CrabError::LfsObjectMissing {
                    oid: oid_hex.clone(),
                });
            }
            Ok(LfsPointer {
                oid,
                size: metadata.len(),
                extensions: Vec::new(),
            })
        })
        .collect()
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
pub fn run_lfs_pre_push(
    remote: Option<&str>,
    url: Option<&str>,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    if remote.is_none() && url.is_some() {
        return Err(CrabError::Configuration {
            key: "lfs pre-push".to_owned(),
            origin: "Git supplied a remote URL without a remote name".to_owned(),
        });
    }
    if let (Some(remote), Some(url)) = (remote, url) {
        validate_git_push_url(remote, url)?;
    }
    // Validate the complete batch before resolving cloud access. The cap is
    // local hook admission policy, not a limit on Git repository size.
    let updates = read_pre_push(std::io::stdin().lock(), 16 * 1024 * 1024)?;
    run_lfs_pre_push_batch(remote, &updates, cancel)
}

pub(crate) fn run_lfs_pre_push_batch(
    remote: Option<&str>,
    updates: &[PrePushUpdate],
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    let (local_shas, remote_shas) = pre_push_revisions(updates);

    if local_shas.is_empty() {
        return Ok(());
    }

    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("push", remote)?;

    // Ref updates contain only the refs Git is changing. Excluding the
    // compacted manifest's complete ref-tip set prevents a multi-branch push
    // from rescanning pointers that are already reachable from another remote
    // branch or tag.
    let base_manifest_refs = load_remote_manifest_ref_tips(&ctx)?;

    // Collect LFS pointers from the commits being pushed.
    let pointers = collect_pointers_from_range_with_base_refs(
        &local_shas,
        &remote_shas,
        &base_manifest_refs,
        cancel,
    )?;

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

fn pre_push_revisions(updates: &[PrePushUpdate]) -> (Vec<String>, Vec<String>) {
    let mut local_shas = Vec::new();
    let mut remote_shas = Vec::new();
    for update in updates {
        let Some(local_oid) = &update.local_oid else {
            continue;
        };
        local_shas.push(local_oid.clone());
        if let Some(remote_oid) = &update.remote_oid {
            remote_shas.push(remote_oid.clone());
        }
    }
    (local_shas, remote_shas)
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Collect LFS pointers from HEAD (or all refs for `--all`).
fn collect_push_pointers(
    all: bool,
    refs: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<LfsPointer>> {
    let ref_names = if all {
        if refs.is_empty() {
            all_ref_names(cancel)?
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
    let mut remaining = MAX_CAPTURE_BYTES;
    for ref_name in ref_names {
        visit_lfs_pointers_in_tree(Path::new("."), &ref_name, cancel, |_, pointer| {
            if seen.insert(pointer.oid) {
                spend_scan_budget(
                    &mut remaining,
                    pointer_memory("", &pointer) + std::mem::size_of::<[u8; 32]>(),
                )?;
                pointers.push(pointer);
            }
            Ok(())
        })?;
    }
    Ok(pointers)
}

/// Collect `(path, LfsPointer)` pairs from a commit range being pushed.
fn collect_pointers_from_range_with_base_refs(
    local_shas: &[String],
    remote_shas: &[String],
    base_manifest_refs: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    collect_pointers_from_range_in_with_base_refs(
        Path::new("."),
        local_shas,
        remote_shas,
        base_manifest_refs,
        cancel,
    )
}

fn load_remote_manifest_ref_tips(
    ctx: &super::store_setup::LfsRemoteContext,
) -> Result<Vec<String>> {
    let store = crate::storage::Store::from_storage(ctx.store.store().clone());
    let router = crate::storage::StoreLayout::new(store.clone(), ctx.prefix.clone());
    super::block_on_runtime(async move {
        match crate::metadata::manifest::read_manifest(&store, &router).await {
            Ok((manifest, _)) => Ok(manifest.refs.into_values().collect()),
            Err(CrabError::NotFound { .. }) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    })
}

pub(crate) fn collect_lfs_object_ids_from_range_in(
    repo_dir: &Path,
    local_shas: &[String],
    remote_shas: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let mut object_ids: Vec<String> =
        collect_pointers_from_range_in(repo_dir, local_shas, remote_shas, cancel)?
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
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    collect_pointers_from_range_in_with_base_refs(repo_dir, local_shas, remote_shas, &[], cancel)
}

fn collect_pointers_from_range_in_with_base_refs(
    repo_dir: &Path,
    local_shas: &[String],
    remote_shas: &[String],
    base_manifest_refs: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    check_cancelled(cancel)?;
    // A size filter can hide a corrupt pointer behind a forged large header.
    // Verify the introduced range's named objects; large bodies stay streamed.
    let mut args = vec!["rev-list".to_owned(), "--objects".to_owned()];
    for sha in local_shas {
        args.push(sha.clone());
    }
    let mut exclusions = HashSet::with_capacity(remote_shas.len() + base_manifest_refs.len());
    exclusions.extend(remote_shas.iter().cloned());
    exclusions.extend(base_manifest_refs.iter().cloned());
    let mut exclusions = exclusions.into_iter().collect::<Vec<_>>();
    exclusions.sort_unstable();
    for sha in exclusions {
        args.push(format!("^{sha}"));
    }
    let mut entries = Vec::new();
    visit_lfs_blobs_in_git_command(
        repo_dir,
        &args,
        b'\n',
        parse_rev_list_record,
        cancel,
        |path, pointer| {
            entries.push((path, pointer));
            Ok(())
        },
    )?;
    Ok(entries)
}

fn visit_lfs_pointers_in_tree(
    repo_dir: &Path,
    ref_name: &str,
    cancel: &CancellationToken,
    mut visitor: impl FnMut(String, LfsPointer) -> Result<()>,
) -> Result<()> {
    let args = vec![
        "ls-tree".to_owned(),
        "-r".to_owned(),
        "-z".to_owned(),
        ref_name.to_owned(),
    ];
    let mut seen = HashSet::new();
    visit_lfs_blobs_in_git_command(
        repo_dir,
        &args,
        b'\0',
        parse_ls_tree_record,
        cancel,
        |path, pointer| {
            if seen.insert(pointer.oid) {
                visitor(path, pointer)?;
            }
            Ok(())
        },
    )
}

type DiscoveryParser = fn(&[u8], &mut Option<String>) -> Result<Option<(String, String)>>;

fn visit_lfs_blobs_in_git_command(
    repo_dir: &Path,
    args: &[String],
    record_terminator: u8,
    parse_record: DiscoveryParser,
    cancel: &CancellationToken,
    mut visitor: impl FnMut(String, LfsPointer) -> Result<()>,
) -> Result<()> {
    let mut producer = git_command_in(repo_dir);
    producer.args(args);
    let discovery = process::run(
        producer,
        cancel,
        None::<fn(ChildStdin) -> Result<()>>,
        |stdout| {
            let mut pointers = Vec::new();
            let mut remaining = MAX_CAPTURE_BYTES;
            read_discovery(
                stdout,
                record_terminator,
                parse_record,
                cancel,
                MAX_CAPTURE_BYTES,
                |records| {
                    pointers.extend(read_lfs_batch(repo_dir, records, cancel, &mut remaining)?);
                    Ok(())
                },
            )?;
            Ok(pointers)
        },
    )?;
    let operation = args.first().map_or("discovery", String::as_str);
    // Each bounded batch is joined inside the discovery worker. Keep every
    // candidate private until discovery also exits successfully: a late
    // framing, checksum or producer failure invalidates the entire scan.
    for (path, pointer) in successful_git(operation, discovery)? {
        check_cancelled(cancel)?;
        visitor(path, pointer)?;
    }
    Ok(())
}

fn read_lfs_batch(
    repo_dir: &Path,
    records: &[(String, String)],
    cancel: &CancellationToken,
    remaining: &mut u64,
) -> Result<Vec<(String, LfsPointer)>> {
    let mut batch = git_command_in(repo_dir);
    batch.args(["cat-file", "--batch"]);
    let write_stdin = |mut stdin: ChildStdin| {
        for (oid, _) in records {
            check_cancelled(cancel)?;
            writeln!(stdin, "{oid}")?;
        }
        Ok(())
    };
    let read_stdout = |stdout| {
        let mut pointers = Vec::new();
        crab_git::batch::visit_small_blobs(
            stdout,
            records.iter().map(|(oid, _)| oid.as_str()),
            MAX_LFS_POINTER_SIZE,
            &|| cancel.is_cancelled(),
            |index, body| {
                if let PointerKind::Lfs(pointer) = classify(body)
                    && pointer.size > 0
                {
                    let path = &records[index].1;
                    spend_scan_budget(remaining, pointer_memory(path, &pointer))?;
                    pointers.push((path.clone(), pointer));
                }
                Ok(())
            },
        )?;
        Ok(pointers)
    };
    let output = process::run(batch, cancel, Some(write_stdin), read_stdout)?;
    successful_git("cat-file", output)
}

fn read_discovery(
    stdout: impl Read,
    terminator: u8,
    parse_record: DiscoveryParser,
    cancel: &CancellationToken,
    batch_bytes: u64,
    mut visit_batch: impl FnMut(&[(String, String)]) -> Result<()>,
) -> Result<()> {
    const MAX_RECORD_BYTES: u64 = 1024 * 1024;
    let mut reader = BufReader::new(stdout);
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut state = None;
    let mut remaining = batch_bytes;
    loop {
        check_cancelled(cancel)?;
        record.clear();
        let read = (&mut reader)
            .take(MAX_RECORD_BYTES + 1)
            .read_until(terminator, &mut record)?;
        if read == 0 {
            break;
        }
        if read as u64 > MAX_RECORD_BYTES || record.last() != Some(&terminator) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Git LFS discovery record is oversized or truncated",
            )
            .into());
        }
        record.pop();
        if let Some((oid, path)) = parse_record(&record, &mut state)? {
            let bytes = std::mem::size_of::<(String, String)>() + oid.len() + path.len();
            if bytes as u64 > remaining && !records.is_empty() {
                visit_batch(&records)?;
                records.clear();
                remaining = batch_bytes;
            }
            spend_scan_budget(&mut remaining, bytes)?;
            records.push((oid, path));
        }
    }
    if !records.is_empty() {
        visit_batch(&records)?;
    }
    Ok(())
}

fn pointer_memory(path: &str, pointer: &LfsPointer) -> usize {
    std::mem::size_of::<(String, LfsPointer)>()
        + path.len()
        + std::mem::size_of_val(pointer.extensions.as_slice())
        + pointer
            .extensions
            .iter()
            .map(|extension| extension.name.len() + extension.oid_type.len())
            .sum::<usize>()
}

fn spend_scan_budget(remaining: &mut u64, bytes: usize) -> io::Result<()> {
    *remaining = remaining.checked_sub(bytes as u64).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Git LFS scan exceeds inventory limit",
        )
    })?;
    Ok(())
}

fn successful_git<T>(operation: &str, output: process::Output<T>) -> Result<T> {
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git {operation} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
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

    let separator = record.iter().position(|byte| byte.is_ascii_whitespace());
    let (oid_bytes, path) = match separator {
        Some(index) => (&record[..index], &record[index + 1..]),
        None => (record, &record[0..0]),
    };
    let oid = std::str::from_utf8(oid_bytes).map_err(|_| {
        CrabError::Internal("git rev-list returned a malformed object record".to_owned())
    })?;
    if !is_git_object_id(oid) {
        return Err(CrabError::Internal(
            "git rev-list returned a malformed object record".to_owned(),
        ));
    }

    if !path.is_empty() {
        *pending_oid = None;
        return Ok(Some((
            oid.to_owned(),
            String::from_utf8_lossy(path).into_owned(),
        )));
    }

    // Commits and trees have no object name. Keep the most recent unnamed
    // object for Git versions that emit a separate `path=...` record; normal
    // line-delimited output replaces it on each row.
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

fn all_ref_names(cancel: &CancellationToken) -> Result<Vec<String>> {
    let mut command = git_command_in(Path::new("."));
    command.args(["rev-parse", "--all"]);
    let output = process::run(
        command,
        cancel,
        None::<fn(ChildStdin) -> Result<()>>,
        |stdout| Ok(process::capture_output(stdout, MAX_CAPTURE_BYTES)?),
    )?;
    let stdout = successful_git("rev-parse", output)?;
    Ok(String::from_utf8_lossy(&stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            (!line.is_empty()).then(|| line.to_owned())
        })
        .collect())
}

fn git_command_in(repo_dir: &Path) -> Command {
    let mut command = Command::new("git");
    for key in GIT_ENV_REMOVALS {
        command.env_remove(key);
    }
    command
        .arg("--no-replace-objects")
        .env("GIT_NO_LAZY_FETCH", "1")
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
mod tests;
