//! `batch_git_status` RPC handler.
//!
//! Runs `git status --porcelain -b` for multiple repos in parallel,
//! parses branch/dirty/ahead/behind from the output, and supports
//! delta-mode polling via an in-memory cache of last-known output.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tracing::debug;

use crate::Result;

/// Maximum number of concurrent `git status` subprocesses.
const MAX_CONCURRENT: usize = 10;

/// In-memory cache of last-known `git status --porcelain -b` output
/// per repo path. Ephemeral — lost on agent restart.
#[derive(Clone, Default)]
pub struct GitStatusCache {
    inner: Arc<RwLock<HashMap<PathBuf, CacheEntry>>>,
}

struct CacheEntry {
    output: String,
    timestamp: u64,
}

impl GitStatusCache {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Deserialize)]
pub struct BatchGitStatusParams {
    pub repo_paths: Vec<String>,
    pub delta_since: Option<u64>,
}

/// Parsed result for a single repo.
#[derive(Debug)]
struct RepoStatusResult {
    relative_path: String,
    branch: Option<String>,
    dirty_count: usize,
    ahead: Option<u32>,
    behind: Option<u32>,
    error: Option<String>,
}

/// Execute `batch_git_status` — runs `git status --porcelain -b` for
/// each repo path in parallel (max 10 concurrent), parses the output,
/// and optionally filters to only repos whose output changed.
pub async fn batch_git_status(
    cache: &GitStatusCache,
    params: BatchGitStatusParams,
) -> Result<Value> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let delta_since = params.delta_since;
    let cache_clone = cache.clone();

    // Run git status for all repos concurrently (bounded).
    let results: Vec<Option<RepoStatusResult>> = stream::iter(params.repo_paths)
        .map(|repo_path| {
            let cache_ref = cache_clone.clone();
            async move {
                let path = PathBuf::from(&repo_path);
                let (output, error) = run_git_status(&path).await;

                // Check delta: if delta_since is set and output hasn't
                // changed from the cached value, skip this repo.
                if let Some(_delta_ts) = delta_since {
                    let cache_guard = cache_ref.inner.read().await;
                    if let Some(entry) = cache_guard.get(&path) {
                        if entry.timestamp >= _delta_ts {
                            if let Some(ref current_output) = output {
                                if *current_output == entry.output {
                                    return None; // unchanged, skip
                                }
                            }
                        }
                    }
                }

                // Update cache with current output.
                if let Some(ref out_str) = output {
                    let mut cache_guard = cache_ref.inner.write().await;
                    cache_guard.insert(
                        path.clone(),
                        CacheEntry {
                            output: out_str.clone(),
                            timestamp: now,
                        },
                    );
                }

                // Derive relative_path: use the last path component as
                // a reasonable default. The caller provides absolute paths.
                let relative_path = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_else(|| repo_path.as_str())
                    .to_owned();

                if let Some(err_msg) = error {
                    return Some(RepoStatusResult {
                        relative_path,
                        branch: None,
                        dirty_count: 0,
                        ahead: None,
                        behind: None,
                        error: Some(err_msg),
                    });
                }

                let parsed = parse_git_status(output.as_deref().unwrap_or(""));
                Some(RepoStatusResult {
                    relative_path,
                    branch: parsed.branch,
                    dirty_count: parsed.dirty_count,
                    ahead: parsed.ahead,
                    behind: parsed.behind,
                    error: None,
                })
            }
        })
        .buffer_unordered(MAX_CONCURRENT)
        .collect()
        .await;

    // Filter out None (unchanged repos in delta mode).
    let statuses: Vec<Value> = results
        .into_iter()
        .flatten()
        .map(|r| {
            json!({
                "relative_path": r.relative_path,
                "branch": r.branch,
                "dirty_count": r.dirty_count,
                "ahead": r.ahead,
                "behind": r.behind,
                "error": r.error,
            })
        })
        .collect();

    Ok(json!({
        "statuses": statuses,
        "poll_timestamp": now,
    }))
}

/// Run `git status --porcelain -b` in the given directory.
/// Returns (Some(stdout), None) on success, or (None, Some(error)) on failure.
async fn run_git_status(repo_path: &Path) -> (Option<String>, Option<String>) {
    let mut command = match crate::shellout::git_command() {
        Ok(command) => command,
        Err(error) => return (None, Some(error.to_string())),
    };
    let result = command
        .args(["status", "--porcelain", "-b"])
        .current_dir(repo_path)
        .output()
        .await;

    match result {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                (Some(stdout), None)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                debug!(path = %repo_path.display(), %stderr, "git status failed");
                (None, Some(stderr.trim().to_owned()))
            }
        }
        Err(e) => (None, Some(format!("failed to spawn git: {e}"))),
    }
}

/// Parsed git status output.
struct ParsedGitStatus {
    branch: Option<String>,
    dirty_count: usize,
    ahead: Option<u32>,
    behind: Option<u32>,
}

/// Parse the output of `git status --porcelain -b`.
///
/// First line format: `## branch...tracking [ahead N, behind M]`
/// or `## branch` (no tracking info).
/// Remaining lines: one per dirty/staged file.
fn parse_git_status(output: &str) -> ParsedGitStatus {
    let lines: Vec<&str> = output.lines().filter(|l| !l.is_empty()).collect();

    if lines.is_empty() {
        return ParsedGitStatus {
            branch: None,
            dirty_count: 0,
            ahead: None,
            behind: None,
        };
    }

    let header = lines[0];
    let mut branch: Option<String> = None;
    let mut ahead: Option<u32> = None;
    let mut behind: Option<u32> = None;

    if let Some(header_content) = header.strip_prefix("## ") {
        if let Some(rest) = header_content.strip_prefix("No commits yet on ") {
            branch = Some(rest.to_owned());
        } else if header_content == "HEAD (no branch)" {
            branch = None;
        } else {
            // Split on "..." to separate local branch from tracking
            if let Some(dots_idx) = header_content.find("...") {
                branch = Some(header_content[..dots_idx].to_owned());

                // Look for [ahead N, behind M] or [ahead N] or [behind M]
                if let Some(bracket_start) = header_content.rfind('[') {
                    let bracket_section = &header_content[bracket_start..];
                    if let Some(bracket_end) = bracket_section.find(']') {
                        let inside = &bracket_section[1..bracket_end];
                        for part in inside.split(',') {
                            let part = part.trim();
                            if let Some(n) = part.strip_prefix("ahead ") {
                                ahead = n.trim().parse().ok();
                            } else if let Some(n) = part.strip_prefix("behind ") {
                                behind = n.trim().parse().ok();
                            }
                        }
                    }
                }
            } else {
                // No tracking info, just branch name
                branch = Some(header_content.trim().to_owned());
            }
        }
    }

    // Count dirty files (all lines after the header)
    let dirty_count = lines.len() - 1;

    ParsedGitStatus {
        branch,
        dirty_count,
        ahead,
        behind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_branch_no_tracking() {
        let output = "## main\n";
        let parsed = parse_git_status(output);
        assert_eq!(parsed.branch.as_deref(), Some("main"));
        assert_eq!(parsed.dirty_count, 0);
        assert_eq!(parsed.ahead, None);
        assert_eq!(parsed.behind, None);
    }

    #[test]
    fn parse_branch_with_tracking_ahead_behind() {
        let output = "## main...origin/main [ahead 2, behind 1]\n M src/lib.rs\n?? new_file.txt\n";
        let parsed = parse_git_status(output);
        assert_eq!(parsed.branch.as_deref(), Some("main"));
        assert_eq!(parsed.dirty_count, 2);
        assert_eq!(parsed.ahead, Some(2));
        assert_eq!(parsed.behind, Some(1));
    }

    #[test]
    fn parse_branch_ahead_only() {
        let output = "## feat/auth...origin/feat/auth [ahead 3]\n";
        let parsed = parse_git_status(output);
        assert_eq!(parsed.branch.as_deref(), Some("feat/auth"));
        assert_eq!(parsed.dirty_count, 0);
        assert_eq!(parsed.ahead, Some(3));
        assert_eq!(parsed.behind, None);
    }

    #[test]
    fn parse_branch_behind_only() {
        let output = "## develop...origin/develop [behind 5]\n";
        let parsed = parse_git_status(output);
        assert_eq!(parsed.branch.as_deref(), Some("develop"));
        assert_eq!(parsed.dirty_count, 0);
        assert_eq!(parsed.ahead, None);
        assert_eq!(parsed.behind, Some(5));
    }

    #[test]
    fn parse_detached_head() {
        let output = "## HEAD (no branch)\n M file.rs\n";
        let parsed = parse_git_status(output);
        assert_eq!(parsed.branch, None);
        assert_eq!(parsed.dirty_count, 1);
    }

    #[test]
    fn parse_no_commits_yet() {
        let output = "## No commits yet on main\n?? README.md\n";
        let parsed = parse_git_status(output);
        assert_eq!(parsed.branch.as_deref(), Some("main"));
        assert_eq!(parsed.dirty_count, 1);
    }

    #[test]
    fn parse_empty_output() {
        let parsed = parse_git_status("");
        assert_eq!(parsed.branch, None);
        assert_eq!(parsed.dirty_count, 0);
        assert_eq!(parsed.ahead, None);
        assert_eq!(parsed.behind, None);
    }

    #[test]
    fn parse_clean_repo_with_tracking() {
        let output = "## main...origin/main\n";
        let parsed = parse_git_status(output);
        assert_eq!(parsed.branch.as_deref(), Some("main"));
        assert_eq!(parsed.dirty_count, 0);
        assert_eq!(parsed.ahead, None);
        assert_eq!(parsed.behind, None);
    }
}
