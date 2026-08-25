//! Git LFS recent-ref and recent-commit selection helpers.

use std::path::Path;
use std::process::Command;

use crate::core::error::{CrabError, Result};

/// Returns recent local and optionally remote ref tips.
///
/// `extra_days` is added to `lfs.fetchrecentrefsdays`, matching prune's
/// offset-based retention window.
pub(crate) fn recent_ref_oids(extra_days: u64) -> Result<Vec<String>> {
    let root = std::env::current_dir().map_err(CrabError::Io)?;
    recent_ref_oids_in(&root, extra_days)
}

pub(crate) fn git_config_u64(key: &str, default: u64) -> Result<u64> {
    let root = std::env::current_dir().map_err(CrabError::Io)?;
    git_config_u64_in(&root, key, default)
}

pub(crate) fn recent_ref_oids_in(repo_root: &Path, extra_days: u64) -> Result<Vec<String>> {
    let fetch_days = git_config_u64_in(repo_root, "lfs.fetchrecentrefsdays", 7)?;
    if fetch_days == 0 {
        return Ok(Vec::new());
    }
    let include_remotes = git_config_bool_in(repo_root, "lfs.fetchrecentremoterefs", true)?;
    let cutoff = cutoff_unix(fetch_days.saturating_add(extra_days));

    let mut args = vec![
        "for-each-ref",
        "--format=%(objectname) %(committerdate:unix)",
        "refs/heads",
    ];
    if include_remotes {
        args.push("refs/remotes");
    }

    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to list recent refs: {e}")))?;

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

/// Returns commits inside `lfs.fetchrecentcommitsdays` reachable from revisions.
pub(crate) fn recent_commit_oids(revisions: &[String]) -> Result<Vec<String>> {
    let root = std::env::current_dir().map_err(CrabError::Io)?;
    recent_commit_oids_in(&root, revisions)
}

pub(crate) fn recent_commit_oids_in(repo_root: &Path, revisions: &[String]) -> Result<Vec<String>> {
    if revisions.is_empty() {
        return Ok(Vec::new());
    }

    let days = git_config_u64_in(repo_root, "lfs.fetchrecentcommitsdays", 0)?;
    if days == 0 {
        return Ok(Vec::new());
    }
    let cutoff = cutoff_unix(days);

    let mut args = vec!["rev-list".to_owned(), "--timestamp".to_owned()];
    args.extend(revisions.iter().cloned());

    let output = Command::new("git")
        .args(&args)
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to list recent commits: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if rev_list_can_be_empty(&stderr) {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {stderr}"
        )));
    }

    Ok(parse_timestamped_commit_oids(
        &String::from_utf8_lossy(&output.stdout),
        cutoff,
    ))
}

pub(crate) fn git_config_u64_in(repo_root: &Path, key: &str, default: u64) -> Result<u64> {
    let output = Command::new("git")
        .args(["config", "--type", "int", "--get", key])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to read {key}: {e}")))?;

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

fn git_config_bool_in(repo_root: &Path, key: &str, default: bool) -> Result<bool> {
    let output = Command::new("git")
        .args(["config", "--type", "bool", "--get", key])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to read {key}: {e}")))?;

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

fn parse_timestamped_commit_oids(output: &str, cutoff_unix: u64) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let (timestamp, oid) = line.split_once(' ')?;
            let timestamp = timestamp.trim().parse::<u64>().ok()?;
            if timestamp < cutoff_unix {
                return None;
            }
            Some(oid.trim().to_owned())
        })
        .filter(|oid| !oid.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recent_ref_oids_filters_by_cutoff() {
        let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 100
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 90
cccccccccccccccccccccccccccccccccccccccc bad
";

        let refs = parse_recent_ref_oids(output, 100);

        assert_eq!(refs, vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]);
    }

    #[test]
    fn parse_timestamped_commit_oids_filters_by_cutoff() {
        let output = "\
100 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
90 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
bad cccccccccccccccccccccccccccccccccccccccc
";

        let commits = parse_timestamped_commit_oids(output, 100);

        assert_eq!(commits, vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]);
    }

    #[test]
    fn rev_list_empty_errors_are_non_fatal() {
        assert!(rev_list_can_be_empty("fatal: bad default revision 'HEAD'"));
        assert!(rev_list_can_be_empty(
            "fatal: your current branch does not have any commits"
        ));
        assert!(rev_list_can_be_empty(
            "fatal: ambiguous argument 'refs/stash'"
        ));
        assert!(!rev_list_can_be_empty("fatal: object database is corrupt"));
    }
}
