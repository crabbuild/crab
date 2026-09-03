//! `crab lfs fetch` and `crab lfs pull` — download LFS objects.
//!
//! Wires the CLI fetch/pull subcommands to [`crate::lfs::batch::BatchResolver`]
//! via the shared [`super::store_setup`] module.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::emit_json;
use crate::lfs::batch::{BatchResolver, PatternFilter};
use crab_git::lfs_pointer::{LfsPointer, hex_encode};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::store_setup::resolve_lfs_remote_for_operation_with_remote_sync;

#[derive(Debug, Clone, Default)]
pub struct LfsFetchOptions {
    pub remote: Option<String>,
    pub refs: Vec<String>,
    pub include: Option<String>,
    pub exclude: Option<String>,
    pub recent: bool,
    pub all: bool,
    pub stdin: bool,
    pub prune: bool,
    pub refetch: bool,
    pub dry_run: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LfsPullOptions {
    pub remote: Option<String>,
    pub include: Option<String>,
    pub exclude: Option<String>,
}

/// Run `crab lfs fetch`.
///
/// Downloads missing LFS objects from the remote store into the local
/// `.git/lfs/objects` cache. Supports include/exclude pattern filtering,
/// `--recent`, `--all`, and `--dry-run`.
pub fn run_lfs_fetch(options: LfsFetchOptions, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    validate_fetch_options(&options)?;
    let (remote, refs) = remote_and_refs_from_options(&options)?;
    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("pull", remote.as_deref())?;
    let entries = collect_lfs_pointers(Path::new("."), options.all, options.recent, &refs, cancel)?;
    check_cancelled(cancel)?;

    if entries.is_empty() {
        if options.json {
            write_fetch_json(&[], &ctx.prefix, &ctx.local_lfs_dir)?;
        } else {
            eprintln!("fetch: no LFS objects to fetch");
        }
        if options.prune {
            run_fetch_prune(options.dry_run, cancel)?;
        }
        return Ok(());
    }

    let inc_filter = options
        .include
        .as_deref()
        .map(PatternFilter::new)
        .transpose()?;
    let exc_filter = options
        .exclude
        .as_deref()
        .map(PatternFilter::new)
        .transpose()?;

    let transfers = plan_fetch_transfers(
        &entries,
        inc_filter.as_ref(),
        exc_filter.as_ref(),
        &ctx.local_lfs_dir,
        options.refetch,
    )?;
    check_cancelled(cancel)?;

    if transfers.is_empty() {
        if options.json {
            write_fetch_json(&[], &ctx.prefix, &ctx.local_lfs_dir)?;
        } else {
            eprintln!("fetch: all objects up to date");
        }
        if options.prune {
            run_fetch_prune(options.dry_run, cancel)?;
        }
        return Ok(());
    }

    if options.dry_run {
        if options.json {
            write_fetch_json(&transfers, &ctx.prefix, &ctx.local_lfs_dir)?;
        }
        if !options.json {
            eprintln!("fetch: would download {} object(s)", transfers.len());
            for transfer in &transfers {
                eprintln!("  {}", &hex_encode(&transfer.pointer.oid)[..10]);
            }
        }
        if options.prune {
            run_fetch_prune(true, cancel)?;
        }
        return Ok(());
    }

    let pointers: Vec<LfsPointer> = transfers
        .iter()
        .map(|transfer| transfer.pointer.clone())
        .collect();
    let downloaded_count = pointers.len();
    let remote_prefix_for_json = ctx.prefix.clone();
    let local_lfs_dir_for_json = ctx.local_lfs_dir.clone();

    super::block_on_runtime(async {
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config, cancel);

        if !options.json {
            eprintln!("fetch: downloading {downloaded_count} object(s)");
        }
        let progress =
            super::progress::TransferProgress::new("Downloading", downloaded_count as u64);
        resolver
            .download_objects(&pointers, options.refetch)
            .await?;
        progress.finish();
        super::logs::log_transfer_event("fetch", downloaded_count as u64, progress.elapsed_secs());
        if !options.json {
            eprintln!("fetch: done");
        }
        Ok(())
    })?;

    if options.json {
        write_fetch_json(&transfers, &remote_prefix_for_json, &local_lfs_dir_for_json)?;
    }

    if options.prune {
        run_fetch_prune(false, cancel)?;
    }

    Ok(())
}

fn validate_fetch_options(options: &LfsFetchOptions) -> Result<()> {
    if options.stdin && !options.refs.is_empty() {
        return Err(CrabError::Configuration {
            key: "lfs fetch".to_owned(),
            origin: "--stdin reads refs instead of command-line refs".to_owned(),
        });
    }
    if options.all && options.recent {
        return Err(CrabError::Configuration {
            key: "lfs fetch".to_owned(),
            origin: "--all cannot be combined with --recent".to_owned(),
        });
    }
    if options.all && (options.include.is_some() || options.exclude.is_some()) {
        return Err(CrabError::Configuration {
            key: "lfs fetch".to_owned(),
            origin: "--all cannot be combined with --include or --exclude".to_owned(),
        });
    }
    if options.json && options.prune {
        return Err(CrabError::Configuration {
            key: "lfs fetch".to_owned(),
            origin: "Cannot combine --json with --prune".to_owned(),
        });
    }
    Ok(())
}

fn remote_and_refs_from_options(
    options: &LfsFetchOptions,
) -> Result<(Option<String>, Vec<String>)> {
    let refs = refs_from_options(options)?;
    if options.stdin {
        return Ok((options.remote.clone(), refs));
    }

    let Some(candidate) = options.remote.as_ref() else {
        return Ok((None, refs));
    };

    if git_remote_exists(candidate) || !refs.is_empty() {
        return Ok((Some(candidate.clone()), refs));
    }

    Ok((None, vec![candidate.clone()]))
}

fn git_remote_exists(name: &str) -> bool {
    Command::new("git")
        .args(["remote", "get-url", name])
        .output()
        .ok()
        .is_some_and(|output| output.status.success())
}

fn refs_from_options(options: &LfsFetchOptions) -> Result<Vec<String>> {
    if !options.stdin {
        return Ok(options.refs.clone());
    }

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

#[derive(Debug, Clone)]
struct FetchTransfer {
    path: String,
    pointer: LfsPointer,
}

fn plan_fetch_transfers(
    entries: &[(String, LfsPointer)],
    include: Option<&PatternFilter>,
    exclude: Option<&PatternFilter>,
    local_lfs_dir: &Path,
    refetch: bool,
) -> Result<Vec<FetchTransfer>> {
    let mut transfers = Vec::new();
    let mut seen_sizes = HashMap::new();

    for (path, pointer) in entries {
        if let Some(inc) = include
            && !inc.matches(path)
        {
            continue;
        }
        if let Some(exc) = exclude
            && exc.matches(path)
        {
            continue;
        }
        if let Some(existing_size) = seen_sizes.get(&pointer.oid) {
            if *existing_size != pointer.size {
                return Err(CrabError::LfsObjectCorrupt {
                    oid: hex_encode(&pointer.oid),
                });
            }
            continue;
        }
        seen_sizes.insert(pointer.oid, pointer.size);
        let local_valid = crate::lfs::cache::is_valid(local_lfs_dir, &pointer.oid, pointer.size)?;
        if !refetch && local_valid {
            continue;
        }
        transfers.push(FetchTransfer {
            path: path.clone(),
            pointer: pointer.clone(),
        });
    }

    Ok(transfers)
}

#[derive(Serialize)]
struct FetchJsonPayload {
    transfers: Vec<FetchJsonTransfer>,
}

#[derive(Serialize)]
struct FetchJsonTransfer {
    name: String,
    oid: String,
    size: u64,
    actions: FetchJsonActions,
    path: String,
}

#[derive(Serialize)]
struct FetchJsonActions {
    download: FetchJsonDownload,
}

#[derive(Serialize)]
struct FetchJsonDownload {
    href: String,
    expires_at: &'static str,
}

fn write_fetch_json(
    transfers: &[FetchTransfer],
    remote_prefix: &str,
    local_lfs_dir: &Path,
) -> Result<()> {
    let transfers = transfers
        .iter()
        .map(|transfer| {
            let oid = hex_encode(&transfer.pointer.oid);
            FetchJsonTransfer {
                name: transfer.path.clone(),
                oid: oid.clone(),
                size: transfer.pointer.size,
                actions: FetchJsonActions {
                    download: FetchJsonDownload {
                        href: fetch_json_href(remote_prefix, &oid),
                        expires_at: "0001-01-01T00:00:00Z",
                    },
                },
                path: local_object_path(local_lfs_dir, &transfer.pointer.oid)
                    .display()
                    .to_string(),
            }
        })
        .collect();
    let payload = FetchJsonPayload { transfers };
    emit_json("lfs.fetch", "1.1", &payload);
    Ok(())
}

fn fetch_json_href(remote_prefix: &str, oid_hex: &str) -> String {
    let object_path = format!(
        "lfs/objects/{}/{}/{}",
        &oid_hex[..2],
        &oid_hex[2..4],
        oid_hex
    );
    let prefix = remote_prefix.trim_matches('/');
    if prefix.is_empty() {
        return format!("crab-lfs://{object_path}");
    }
    format!("crab-lfs://{prefix}/{object_path}")
}

fn local_object_path(local_lfs_dir: &Path, oid: &[u8; 32]) -> PathBuf {
    let oid_hex = hex_encode(oid);
    local_lfs_dir
        .join("objects")
        .join(&oid_hex[..2])
        .join(&oid_hex[2..4])
        .join(oid_hex)
}

fn run_fetch_prune(dry_run: bool, cancel: &CancellationToken) -> Result<()> {
    super::prune::run_lfs_prune_with_cancel(
        super::prune::LfsPruneOptions {
            verify_remote: false,
            no_verify_remote: false,
            verify_unreachable: false,
            no_verify_unreachable: false,
            when_unverified: Some("continue".to_owned()),
            recent: false,
            dry_run,
            force: true,
            verbose: dry_run,
        },
        cancel,
    )
}

/// Run `crab lfs pull`.
///
/// Fetches missing LFS objects then replaces LFS pointers in the working
/// tree with the actual file content.
pub fn run_lfs_pull(options: LfsPullOptions, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("pull", options.remote.as_deref())?;
    let entries = collect_lfs_pointers(Path::new("."), false, false, &[], cancel)?;
    check_cancelled(cancel)?;

    if entries.is_empty() {
        eprintln!("pull: no LFS objects to pull");
        return Ok(());
    }

    let inc_filter = options
        .include
        .as_deref()
        .map(PatternFilter::new)
        .transpose()?;
    let exc_filter = options
        .exclude
        .as_deref()
        .map(PatternFilter::new)
        .transpose()?;

    super::block_on_runtime(async {
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config, cancel);

        let missing =
            resolver.find_missing_for_fetch(&entries, inc_filter.as_ref(), exc_filter.as_ref())?;

        if !missing.is_empty() {
            eprintln!("pull: downloading {} object(s)", missing.len());
            let progress =
                super::progress::TransferProgress::new("Downloading", missing.len() as u64);
            resolver.download_missing(&missing).await?;
            progress.finish();
            super::logs::log_transfer_event("pull", missing.len() as u64, progress.elapsed_secs());
        }

        Ok::<(), CrabError>(())
    })?;

    let checkout_paths =
        checkout_paths_for_pull(&entries, inc_filter.as_ref(), exc_filter.as_ref());
    check_cancelled(cancel)?;

    if checkout_paths.is_empty() {
        eprintln!("pull: updated 0 file(s) in working tree");
        return Ok(());
    }

    super::checkout::run_lfs_checkout(super::checkout::LfsCheckoutOptions {
        paths: checkout_paths,
        ..super::checkout::LfsCheckoutOptions::default()
    })?;
    check_cancelled(cancel)?;
    Ok(())
}

fn checkout_paths_for_pull(
    entries: &[(String, LfsPointer)],
    include: Option<&PatternFilter>,
    exclude: Option<&PatternFilter>,
) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(path, _)| {
            if let Some(inc) = include
                && !inc.matches(path)
            {
                return None;
            }
            if let Some(exc) = exclude
                && exc.matches(path)
            {
                return None;
            }
            Some(path.clone())
        })
        .collect()
}

fn collect_lfs_pointers(
    repo_dir: &Path,
    all: bool,
    recent: bool,
    refs: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    check_cancelled(cancel)?;
    let mut selected = if all && refs.is_empty() {
        crate::lfs::discovery::all_ref_names(repo_dir, cancel)?
    } else if refs.is_empty() {
        let repository = gix::discover(repo_dir).map_err(io::Error::other)?;
        let head = repository.head().map_err(io::Error::other)?;
        if head.is_unborn() {
            Vec::new()
        } else {
            vec!["HEAD".to_owned()]
        }
    } else {
        refs.to_vec()
    };
    let recent_commits = if recent {
        let recent_refs = crate::lfs::recent::recent_ref_oids_in(repo_dir, 0, cancel)?;
        check_cancelled(cancel)?;
        let mut roots = selected.clone();
        roots.extend(recent_refs.iter().cloned());
        selected.extend(recent_refs);
        crate::lfs::recent::recent_commit_oids_in(repo_dir, &roots, 0, cancel)?
    } else {
        Vec::new()
    };
    check_cancelled(cancel)?;
    let mut seen = HashSet::new();
    selected.retain(|revision| seen.insert(revision.clone()));
    crate::lfs::discovery::collect_pointers_for_fetch_in(
        repo_dir,
        &selected,
        &recent_commits,
        cancel,
    )
}

#[cfg(test)]
mod tests;
