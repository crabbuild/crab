//! `crab lfs push` and `crab lfs pre-push` — upload LFS objects.
//!
//! Wires the CLI push/pre-push subcommands to
//! [`crate::lfs::batch::BatchResolver`] and
//! [`crate::lfs::lock::LockManager`].

use std::path::Path;
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::process::MAX_CAPTURE_BYTES;
use crate::lfs::batch::BatchResolver;
use crate::lfs::discovery::{
    HistoryOperation, collect_pointers_from_history_in,
    collect_pointers_from_range_in_with_base_refs, pointer_memory, spend_scan_budget,
};
use crate::lfs::lock::LockManager;
use crab_git::lfs_pointer::{LfsPointer, hex_encode};
use crab_git::pre_push::{PrePushUpdate, read_pre_push};

use super::store_setup::{git_user_identity, resolve_lfs_remote_context, validate_git_push_url};

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
/// to upload history reachable from local branches/tags, `--object-id` to upload a
/// single object by OID, and `--dry-run` to preview what would be pushed.
pub fn run_lfs_push(options: LfsPushOptions, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let mut resolved = resolve_push_args(&options)?;
    if options.stdin {
        let values = super::input::read_stdin_lines(cancel)?;
        if options.object_id.is_some() {
            validate_object_ids(&values)?;
            resolved.object_ids = values;
        } else {
            resolved.refs = values;
        }
    }

    if options.object_id.is_some() {
        // An empty object-ID stream is an empty request, never an implicit
        // HEAD upload. Keep its mode even when the selected set is empty.
        if resolved.object_ids.is_empty() {
            eprintln!("push: no LFS objects to push");
            return Ok(());
        }
        if options.dry_run {
            eprintln!("push: would upload {} object(s)", resolved.object_ids.len());
            for oid_hex in &resolved.object_ids {
                eprintln!("  {}", &oid_hex[..oid_hex.len().min(10)]);
            }
            return Ok(());
        }

        let ctx = super::block_on_runtime(resolve_lfs_remote_context(
            "push",
            resolved.remote.as_deref(),
            Path::new("."),
            cancel,
        ))?;
        let pointers = object_id_pointers(&ctx.local_lfs_dir, &resolved.object_ids)?;
        return super::block_on_runtime(async {
            let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config, cancel);
            resolver.upload_missing(&pointers).await?;
            eprintln!("push: uploaded {} object(s)", pointers.len());
            Ok(())
        });
    }

    if options.stdin && resolved.refs.is_empty() && !options.all {
        eprintln!("push: no LFS objects to push");
        return Ok(());
    }
    let ctx = super::block_on_runtime(resolve_lfs_remote_context(
        "push",
        resolved.remote.as_deref(),
        Path::new("."),
        cancel,
    ))?;
    // A direct URL or project-default URL has no named remote-tracking set.
    // Do not guess origin or exclude another remote's published history.
    let remote = resolved
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|remote| !remote.is_empty() && !remote.contains("://"));
    let operation = if options.all {
        HistoryOperation::PushAll
    } else {
        HistoryOperation::Push { remote }
    };
    let pointers = collect_push_pointers(Path::new("."), operation, &resolved.refs, cancel)?;

    if pointers.is_empty() {
        eprintln!("push: no LFS objects to push");
        return Ok(());
    }

    super::block_on_runtime(async {
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config, cancel);
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
            } else {
                return Err(CrabError::Configuration {
                    key: "lfs push".to_owned(),
                    origin: "multiple remote operands for --object-id".to_owned(),
                });
            }
        }
        object_ids.extend(options.args.iter().cloned());
        validate_object_ids(&object_ids)?;

        if options.stdin && !object_ids.is_empty() {
            return Err(CrabError::Configuration {
                key: "lfs push".to_owned(),
                origin: "--stdin reads object IDs instead of command-line object IDs".to_owned(),
            });
        }

        if object_ids.is_empty() && !options.stdin {
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

    Ok(ResolvedPushArgs {
        remote: options.remote.clone(),
        refs: options.args.clone(),
        object_ids: Vec::new(),
    })
}

fn validate_object_ids(object_ids: &[String]) -> Result<()> {
    if let Some(index) = object_ids.iter().position(|value| !is_oid_hex(value)) {
        return Err(CrabError::Configuration {
            key: "lfs push".to_owned(),
            origin: format!(
                "invalid object ID at position {}: expected 64 hexadecimal characters",
                index + 1
            ),
        });
    }
    Ok(())
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
        validate_git_push_url(remote, url, cancel)?;
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

    let ctx = super::block_on_runtime(resolve_lfs_remote_context(
        "push",
        remote,
        Path::new("."),
        cancel,
    ))?;

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
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config, cancel);
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

fn collect_push_pointers(
    repo_dir: &Path,
    operation: HistoryOperation<'_>,
    refs: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<LfsPointer>> {
    check_cancelled(cancel)?;
    let mut pointers = Vec::new();
    let mut seen = std::collections::HashMap::new();
    let mut remaining = MAX_CAPTURE_BYTES;
    let mut retain = |_: String, pointer: LfsPointer| {
        if let Some(size) = seen.get(&pointer.oid) {
            if *size != pointer.size {
                return Err(CrabError::LfsObjectCorrupt {
                    oid: hex_encode(&pointer.oid),
                });
            }
        } else {
            spend_scan_budget(
                &mut remaining,
                pointer_memory("", &pointer) + std::mem::size_of::<([u8; 32], u64)>(),
            )?;
            seen.insert(pointer.oid, pointer.size);
            pointers.push(pointer);
        }
        Ok(())
    };
    for (path, pointer) in collect_pointers_from_history_in(repo_dir, refs, operation, cancel)? {
        check_cancelled(cancel)?;
        retain(path, pointer)?;
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
