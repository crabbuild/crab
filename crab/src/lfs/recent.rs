//! Git LFS recent-ref and recent-commit selection helpers.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{ChildStdin, Command};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::process::{self, MAX_CAPTURE_BYTES};

/// Returns recent local and optionally remote ref tips.
///
/// `extra_days` is added to `lfs.fetchrecentrefsdays`, matching prune's
/// offset-based retention window.
pub(crate) fn recent_ref_oids(extra_days: u64, cancel: &CancellationToken) -> Result<Vec<String>> {
    check_cancelled(cancel)?;
    let root = std::env::current_dir().map_err(CrabError::Io)?;
    recent_ref_oids_in(&root, extra_days, cancel)
}

pub(crate) fn git_config_u64(key: &str, default: u64, cancel: &CancellationToken) -> Result<u64> {
    check_cancelled(cancel)?;
    let root = std::env::current_dir().map_err(CrabError::Io)?;
    git_config_u64_in(&root, key, default, cancel)
}

pub(crate) fn recent_ref_oids_in(
    repo_root: &Path,
    extra_days: u64,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let fetch_days = git_config_u64_in(repo_root, "lfs.fetchrecentrefsdays", 7, cancel)?;
    if fetch_days == 0 {
        return Ok(Vec::new());
    }
    let include_remotes = git_config_bool_in(repo_root, "lfs.fetchrecentremoterefs", true, cancel)?;
    let cutoff = cutoff_unix(fetch_days.saturating_add(extra_days));

    let mut args = vec![
        "for-each-ref",
        "--format=%(objectname) %(committerdate:unix)",
        "refs/heads",
    ];
    if include_remotes {
        args.push("refs/remotes");
    }

    let output = git_output(repo_root, &args, cancel)?;

    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(parse_recent_ref_oids(
        &String::from_utf8_lossy(&output.stdout),
        cutoff,
    ))
}

/// Returns commits within each selected tip's recent-history window.
pub(crate) fn recent_commit_oids(
    revisions: &[String],
    extra_days: u64,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    check_cancelled(cancel)?;
    let root = std::env::current_dir().map_err(CrabError::Io)?;
    recent_commit_oids_in(&root, revisions, extra_days, cancel)
}

pub(crate) fn recent_commit_oids_in(
    repo_root: &Path,
    revisions: &[String],
    extra_days: u64,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    check_cancelled(cancel)?;
    if revisions.is_empty() {
        return Ok(Vec::new());
    }

    let days = git_config_u64_in(repo_root, "lfs.fetchrecentcommitsdays", 0, cancel)?;
    if days == 0 {
        return Ok(Vec::new());
    }
    let window = days.saturating_add(extra_days).saturating_mul(24 * 60 * 60);
    let mut roots = HashSet::new();
    let mut remaining = MAX_CAPTURE_BYTES;
    for revision in revisions {
        check_cancelled(cancel)?;
        let commit = format!("{revision}^{{commit}}");
        let resolved = git_output(
            repo_root,
            &["rev-parse", "--verify", "--end-of-options", &commit],
            cancel,
        )?;
        let resolved = crate::lfs::discovery::successful_git("rev-parse", resolved)?;
        let resolved = String::from_utf8(resolved)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let oid = resolved.trim();
        if !crate::lfs::discovery::is_git_object_id(oid) {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "invalid resolved commit ID").into(),
            );
        }
        if roots.contains(oid) {
            continue;
        }
        crate::lfs::discovery::spend_scan_budget(
            &mut remaining,
            2 * (oid.len() + std::mem::size_of::<String>() * 2),
        )?;
        roots.insert(oid.to_owned());
    }
    let mut command = Command::new("git");
    command
        .args([
            "rev-list",
            "--timestamp",
            "--parents",
            "--topo-order",
            "--stdin",
        ])
        .current_dir(repo_root);
    let write_roots = |mut stdin: ChildStdin| -> Result<()> {
        for root in &roots {
            check_cancelled(cancel)?;
            writeln!(stdin, "{root}")?;
        }
        Ok(())
    };
    let output = process::run(command, cancel, Some(write_roots), |stdout| {
        select_recent_commits(stdout, &roots, window, &mut remaining, cancel)
    })?;
    crate::lfs::discovery::successful_git("rev-list", output)
}

fn select_recent_commits(
    stdout: impl Read,
    roots: &HashSet<String>,
    window: u64,
    remaining: &mut u64,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let mut selected = Vec::new();
    let mut cutoffs = HashMap::<String, u64>::new();
    let mut seen_roots = HashSet::new();
    let mut reader = BufReader::new(stdout);
    let mut record = Vec::new();
    loop {
        check_cancelled(cancel)?;
        record.clear();
        let read = (&mut reader)
            .take(MAX_CAPTURE_BYTES + 1)
            .read_until(b'\n', &mut record)?;
        if read == 0 {
            break;
        }
        if read as u64 > MAX_CAPTURE_BYTES || record.pop() != Some(b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized or truncated recent commit",
            )
            .into());
        }
        let line = std::str::from_utf8(&record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let (timestamp, oid, parents) = parse_timestamped_commit(line)?;
        let mut cutoff = cutoffs.remove(oid);
        if cutoff.is_some() {
            // Only the pending frontier is retained. Refund its entry when
            // consumed so long history is bounded by live state, not rows.
            *remaining += (oid.len() + std::mem::size_of::<String>() * 2) as u64;
        }
        if let Some(root) = roots.get(oid) {
            if !seen_roots.insert(root) {
                return Err(
                    io::Error::new(io::ErrorKind::InvalidData, "duplicate recent root").into(),
                );
            }
            let own = timestamp.saturating_sub(window);
            cutoff = Some(cutoff.map_or(own, |inherited| inherited.min(own)));
        }
        let cutoff = cutoff.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected commit in recent history",
            )
        })?;
        if timestamp >= cutoff {
            crate::lfs::discovery::spend_scan_budget(
                remaining,
                oid.len() + std::mem::size_of::<String>() * 2,
            )?;
            selected.push(oid.to_owned());
        }
        // Children precede parents. Propagate the widest applicable window
        // once through shared ancestry, even across skewed timestamps, so
        // independent tips cannot lose each other's retained commits.
        for parent in parents.split_whitespace() {
            if let Some(existing) = cutoffs.get_mut(parent) {
                *existing = (*existing).min(cutoff);
            } else {
                crate::lfs::discovery::spend_scan_budget(
                    remaining,
                    parent.len() + std::mem::size_of::<String>() * 2,
                )?;
                cutoffs.insert(parent.to_owned(), cutoff);
            }
        }
    }
    if seen_roots.len() != roots.len() || !cutoffs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "incomplete recent history").into());
    }
    Ok(selected)
}

pub(crate) fn git_config_u64_in(
    repo_root: &Path,
    key: &str,
    default: u64,
    cancel: &CancellationToken,
) -> Result<u64> {
    let output = git_output(
        repo_root,
        &["config", "--type", "int", "--get", key],
        cancel,
    )?;

    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(default);
        }
        return Err(CrabError::Internal(format!(
            "git config failed while reading {key}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    value.parse::<u64>().map_err(|e| CrabError::Configuration {
        key: key.to_owned(),
        origin: format!("expected unsigned integer, got {value}: {e}"),
    })
}

pub(crate) fn git_config_bool_in(
    repo_root: &Path,
    key: &str,
    default: bool,
    cancel: &CancellationToken,
) -> Result<bool> {
    let output = git_output(
        repo_root,
        &["config", "--type", "bool", "--get", key],
        cancel,
    )?;

    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(default);
        }
        return Err(CrabError::Internal(format!(
            "git config failed while reading {key}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    match value.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(CrabError::Configuration {
            key: key.to_owned(),
            origin: format!("expected boolean, got {value}"),
        }),
    }
}

fn git_output(
    repo_root: &Path,
    args: &[&str],
    cancel: &CancellationToken,
) -> Result<process::Output<Vec<u8>>> {
    let mut command = Command::new("git");
    command.args(args).current_dir(repo_root);
    // Selection holds up both fetch and prune. Join the owned Git process
    // and both bounded pipes before cancellation returns to either caller.
    process::run(
        command,
        cancel,
        None::<fn(ChildStdin) -> Result<()>>,
        |stdout| Ok(process::capture_output(stdout, MAX_CAPTURE_BYTES)?),
    )
}

pub(crate) fn rev_list_can_be_empty(stderr: &str) -> bool {
    stderr.contains("bad default revision")
        || stderr.contains("does not have any commits")
        || stderr.contains("ambiguous argument")
        || stderr.contains("unknown revision")
}

fn cutoff_unix(days: u64) -> u64 {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs();
    now_secs.saturating_sub(days.saturating_mul(24 * 60 * 60))
}

fn parse_recent_ref_oids(output: &str, cutoff_unix: u64) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let (oid, timestamp) = line.split_once(' ')?;
            let timestamp = timestamp.trim().parse::<u64>().ok()?;
            if timestamp < cutoff_unix {
                return None;
            }
            Some(oid.trim().to_owned())
        })
        .filter(|oid| !oid.is_empty())
        .collect()
}

fn parse_timestamped_commit(line: &str) -> Result<(u64, &str, &str)> {
    let (timestamp, object_ids) = line.split_once(' ').ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "malformed timestamped commit")
    })?;
    let timestamp = timestamp
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let (oid, parents) = object_ids.split_once(' ').unwrap_or((object_ids, ""));
    if object_ids
        .split(' ')
        .any(|oid| !crate::lfs::discovery::is_git_object_id(oid))
    {
        return Err(
            io::Error::new(io::ErrorKind::InvalidData, "invalid timestamped commit ID").into(),
        );
    }
    Ok((timestamp, oid, parents))
}

#[cfg(test)]
mod tests;
