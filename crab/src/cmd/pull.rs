//! `crab pull` — git pull + conditional auto-hydration.
//!
//! Wraps `git pull` via `std::process::Command`, detects conflicts and
//! unreachable remotes from the exit code and stderr, then conditionally
//! hydrates newly-fetched pointer blobs that match the hydration filter.

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::OutputMode;
use crate::core::pattern::{PatternFilter, build_filter};
use crate::core::style::CliStyle;
use crate::engine::pointer::is_working_tree_pointer;

/// Arguments for the `crab pull` command.
pub struct PullArgs {
    /// Remote name (default: "origin").
    pub remote: String,
    /// Branch to pull (default: current branch).
    pub branch: Option<String>,
    /// Skip automatic hydration after pulling.
    pub no_hydrate: bool,
    /// Output mode.
    pub mode: OutputMode,
}

/// Result of executing `git pull`.
enum GitPullResult {
    /// Pull succeeded; `changed_files` lists paths that were updated.
    Success { changed_files: Vec<String> },
    /// Pull encountered merge conflicts.
    Conflict { files: Vec<String> },
    /// Remote was unreachable.
    Unreachable { remote: String, reason: String },
}

/// Run the `crab pull` command: git pull + conditional hydration.
pub async fn run_pull(args: &PullArgs, cancel: &CancellationToken) -> Result<()> {
    let style = CliStyle::resolve(args.mode);
    let start = Instant::now();
    let repo_root = std::env::current_dir().map_err(CrabError::Io)?;

    // Phase 1: git pull
    if !args.mode.is_machine() {
        eprintln!(
            "Pulling from {}/{}\u{2026}",
            args.remote,
            args.branch.as_deref().unwrap_or("(current branch)")
        );
    }

    let pull_result = execute_git_pull(
        &args.remote,
        args.branch.as_deref(),
        !args.mode.is_machine(),
    )?;

    match pull_result {
        GitPullResult::Success { changed_files } => {
            let pull_elapsed = start.elapsed();

            if changed_files.is_empty() {
                if !args.mode.is_machine() {
                    eprintln!(
                        "{}",
                        style.ok(&format!(
                            "Already up to date ({:.1}s)",
                            pull_elapsed.as_secs_f64()
                        ))
                    );
                }
                return Ok(());
            }

            // Show what was pulled.
            if !args.mode.is_machine() {
                eprintln!(
                    "  Fetched {} file(s) in {:.1}s",
                    changed_files.len(),
                    pull_elapsed.as_secs_f64()
                );
            }

            if args.no_hydrate {
                if !args.mode.is_machine() {
                    eprintln!(
                        "{}",
                        style.ok(&format!(
                            "Pull complete ({} file(s) updated, hydration skipped, {:.1}s)",
                            changed_files.len(),
                            pull_elapsed.as_secs_f64()
                        ))
                    );
                }
                return Ok(());
            }

            let config = Config::resolve_for_repo(&repo_root)?;
            let hydration_plan = pull_hydration_plan(&config)?;
            let filter = match hydration_plan {
                PullHydrationPlan::Skip(reason) => {
                    if !args.mode.is_machine() {
                        eprintln!(
                            "{}",
                            style.ok(&format!(
                                "Pull complete ({} file(s) updated, hydration skipped: {}, {:.1}s)",
                                changed_files.len(),
                                reason,
                                pull_elapsed.as_secs_f64()
                            ))
                        );
                    }
                    return Ok(());
                }
                PullHydrationPlan::All => None,
                PullHydrationPlan::Filtered(ref filter) => Some(filter),
            };

            // Phase 2: hydrate newly-fetched pointers matching filter
            let pointers = find_hydration_candidates(&changed_files, filter);

            if pointers.is_empty() {
                if !args.mode.is_machine() {
                    eprintln!(
                        "{}",
                        style.ok(&format!(
                            "Pull complete ({} file(s) updated, no pointers to hydrate, {:.1}s)",
                            changed_files.len(),
                            pull_elapsed.as_secs_f64()
                        ))
                    );
                }
                return Ok(());
            }

            if !args.mode.is_machine() {
                let total_pointer_size: u64 = pointers
                    .iter()
                    .filter_map(|p| std::fs::metadata(p).ok())
                    .map(|m| m.len())
                    .sum();
                eprintln!(
                    "  Hydrating {} file(s) ({} pointer bytes)\u{2026}",
                    pointers.len(),
                    format_size_compact(total_pointer_size)
                );
            }

            hydrate_pointers(&pointers, args.mode, &config, cancel).await?;

            let total_elapsed = start.elapsed();
            if !args.mode.is_machine() {
                eprintln!(
                    "{}",
                    style.ok(&format!(
                        "Pull complete ({} file(s) updated, {} hydrated, {:.1}s)",
                        changed_files.len(),
                        pointers.len(),
                        total_elapsed.as_secs_f64()
                    ))
                );
            }

            Ok(())
        }
        GitPullResult::Conflict { files } => Err(CrabError::PullConflict {
            count: files.len(),
            files,
        }),
        GitPullResult::Unreachable { remote, reason } => {
            Err(CrabError::PullRemoteUnreachable { remote, reason })
        }
    }
}

enum PullHydrationPlan {
    Skip(&'static str),
    All,
    Filtered(PatternFilter),
}

fn pull_hydration_plan(config: &Config) -> Result<PullHydrationPlan> {
    if !config.checkout.lazy {
        return Ok(PullHydrationPlan::All);
    }

    if !config.hydrate.auto {
        return Ok(PullHydrationPlan::Skip("lazy checkout"));
    }

    if config.hydrate.include.is_empty() {
        return Ok(PullHydrationPlan::Skip("no auto-hydrate patterns"));
    }

    let filter = build_filter(&config.hydrate.include, &config.hydrate.exclude)?;
    Ok(PullHydrationPlan::Filtered(filter))
}

/// Execute `git pull` and classify the result.
///
/// Streams git's stderr directly to the terminal so the user sees
/// progress from the remote helper in real time. Only stdout is
/// captured for parsing.
fn execute_git_pull(
    remote: &str,
    branch: Option<&str>,
    show_progress: bool,
) -> Result<GitPullResult> {
    // Record HEAD before pull so we can diff against it afterward.
    let head_before = get_head_sha();

    let mut cmd = Command::new("git");
    cmd.arg("pull");
    cmd.arg(remote);
    if let Some(b) = branch {
        cmd.arg(b);
    }

    debug!(remote = remote, branch = ?branch, "executing git pull");

    // Let stderr pass through to the terminal so the user sees
    // progress from git-remote-crab in real time. Capture stdout
    // for parsing the result.
    if show_progress {
        cmd.stderr(std::process::Stdio::inherit());
    } else {
        cmd.stderr(std::process::Stdio::piped());
    }
    cmd.stdout(std::process::Stdio::piped());

    let output = cmd.output().map_err(|e| {
        CrabError::Io(std::io::Error::other(format!(
            "failed to execute git pull: {e}"
        )))
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // stderr is empty when inherited (passed through to terminal).
    let stderr = if show_progress {
        String::new()
    } else {
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    if output.status.success() {
        // Use HEAD diff to reliably detect changed files.
        let changed_files = diff_since(&head_before);
        info!(files_changed = changed_files.len(), "git pull succeeded");
        return Ok(GitPullResult::Success { changed_files });
    }

    // Detect unreachable remote from stderr patterns.
    if is_remote_unreachable(&stderr) {
        return Ok(GitPullResult::Unreachable {
            remote: remote.to_owned(),
            reason: extract_transport_error(&stderr),
        });
    }

    // Detect merge conflicts from exit code + stderr.
    if is_merge_conflict(&stderr, output.status.code()) {
        let conflict_files = parse_conflict_files(&stderr, &stdout);
        return Ok(GitPullResult::Conflict {
            files: conflict_files,
        });
    }

    // Unknown failure — wrap as I/O error with stderr context.
    let exit_desc = output
        .status
        .code()
        .map_or_else(|| "signal".to_owned(), |c| c.to_string());
    Err(CrabError::Io(std::io::Error::other(format!(
        "git pull failed (exit {exit_desc}): {}",
        stderr.trim()
    ))))
}

/// Get the current HEAD SHA (short form). Returns empty string on failure.
fn get_head_sha() -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Diff files changed between a previous HEAD and current HEAD.
/// Falls back to counting working tree files if the diff is empty but
/// git clearly did work (new branch fetched, files checked out).
fn diff_since(head_before: &str) -> Vec<String> {
    let head_after = get_head_sha_now();

    // If HEAD didn't exist before (fresh repo) or changed, diff the range.
    if !head_before.is_empty() && !head_after.is_empty() && head_before != head_after {
        let output = Command::new("git")
            .args([
                "diff",
                "--name-only",
                "-z",
                &format!("{head_before}..{head_after}"),
            ])
            .output();

        if let Ok(out) = output
            && out.status.success()
        {
            let files = parse_git_paths(&out.stdout);
            if !files.is_empty() {
                return files;
            }
        }
    }

    // Fallback: try ORIG_HEAD..HEAD (git sets ORIG_HEAD on pull).
    if let Ok(out) = Command::new("git")
        .args(["diff", "--name-only", "-z", "ORIG_HEAD..HEAD"])
        .output()
        && out.status.success()
    {
        let files = parse_git_paths(&out.stdout);
        if !files.is_empty() {
            return files;
        }
    }

    // Final fallback: if HEAD changed from empty (new repo) or HEAD moved,
    // list all tracked files at HEAD (this is the "first pull" case where
    // the entire tree is new).
    if head_before.is_empty()
        && !head_after.is_empty()
        && let Ok(out) = Command::new("git")
            .args(["ls-tree", "-z", "--name-only", "-r", "HEAD"])
            .output()
        && out.status.success()
    {
        let files = parse_git_paths(&out.stdout);
        return files;
    }

    Vec::new()
}

fn parse_git_paths(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|raw| !raw.is_empty())
        .map(|raw| String::from_utf8_lossy(raw).into_owned())
        .collect()
}

/// Get current HEAD SHA (called after pull).
fn get_head_sha_now() -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Check if stderr indicates the remote is unreachable.
fn is_remote_unreachable(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("fatal: unable to access")
        || lower.contains("could not resolve host")
        || lower.contains("connection refused")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
        || lower.contains("connection timed out")
        || lower.contains("ssh: connect to host")
        || lower.contains("fatal: could not read from remote repository")
}

/// Extract the transport error message from stderr.
fn extract_transport_error(stderr: &str) -> String {
    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("fatal:") || trimmed.starts_with("ssh:") {
            return trimmed.to_owned();
        }
    }
    stderr.trim().to_owned()
}

/// Check if the output indicates a merge conflict.
fn is_merge_conflict(stderr: &str, exit_code: Option<i32>) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("conflict")
        || lower.contains("merge conflict")
        || lower.contains("automatic merge failed")
        || lower.contains("fix conflicts")
        || (exit_code == Some(1) && lower.contains("merge"))
}

/// Parse conflict file paths from git output.
fn parse_conflict_files(stderr: &str, stdout: &str) -> Vec<String> {
    let mut files = Vec::new();
    let combined = format!("{stdout}\n{stderr}");

    for line in combined.lines() {
        let trimmed = line.trim();
        // "CONFLICT (content): Merge conflict in <path>"
        if let Some(rest) = trimmed.strip_prefix("CONFLICT")
            && let Some(idx) = rest.find("Merge conflict in ")
        {
            let path = rest[idx + "Merge conflict in ".len()..].trim();
            if !path.is_empty() {
                files.push(path.to_owned());
            }
        }
    }

    if files.is_empty() {
        files.push("(see git status for details)".to_owned());
    }

    files
}

/// Find pointer files among the changed files that are candidates for hydration.
fn find_hydration_candidates(
    changed_files: &[String],
    filter: Option<&PatternFilter>,
) -> Vec<PathBuf> {
    let mut pointers = Vec::new();

    for file in changed_files {
        if let Some(filter) = filter
            && !filter.matches(file)
        {
            continue;
        }

        let path = PathBuf::from(file);
        if !path.exists() {
            continue;
        }

        match is_working_tree_pointer(&path) {
            Ok(true) => {
                pointers.push(path);
            }
            Ok(false) => {}
            Err(e) => {
                debug!(path = %path.display(), err = %e, "skipping file during hydration check");
            }
        }
    }

    pointers
}

/// Hydrate the given pointer files using the existing hydrate infrastructure.
///
/// Delegates to `crab hydrate` with the specific file paths.
async fn hydrate_pointers(
    pointers: &[PathBuf],
    mode: OutputMode,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    if pointers.is_empty() {
        return Ok(());
    }

    let patterns: Vec<String> = pointers
        .iter()
        .filter_map(|p| p.to_str().map(str::to_owned))
        .collect();

    let hydrate_args = crate::cmd::hydrate::HydrateArgs {
        patterns,
        include: Vec::new(),
        exclude: Vec::new(),
        all: false,
        mode,
        manifest: None,
        manifest_ref: None,
        profile: None,
        ignore_sparse: false,
        recover_from: None,
    };

    crate::cmd::hydrate::run_hydrate(&hydrate_args, config, cancel).await
}

/// Format a byte count compactly for progress messages.
fn format_size_compact(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    #[expect(
        clippy::cast_precision_loss,
        reason = "file sizes fit in f64 without meaningful precision loss"
    )]
    let b = bytes as f64;
    let mut idx = 0;
    let mut scaled = b;
    while scaled >= 1024.0 && idx < UNITS.len() - 1 {
        scaled /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes} B")
    } else {
        format!("{scaled:.1} {unit}", unit = UNITS[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_paths_handles_nul_delimited_names() {
        let paths = parse_git_paths(b"normal.txt\0dir/with space.bin\0");
        assert_eq!(paths, vec!["normal.txt", "dir/with space.bin"]);
    }

    #[test]
    fn lazy_pull_without_auto_hydrate_skips() {
        let mut config = Config::default();
        config.checkout.lazy = true;

        assert!(matches!(
            pull_hydration_plan(&config).unwrap(),
            PullHydrationPlan::Skip("lazy checkout")
        ));
    }

    #[test]
    fn eager_pull_keeps_fallback_hydration() {
        let config = Config::default();

        assert!(matches!(
            pull_hydration_plan(&config).unwrap(),
            PullHydrationPlan::All
        ));
    }

    #[test]
    fn lazy_pull_with_auto_patterns_filters() {
        let mut config = Config::default();
        config.checkout.lazy = true;
        config.hydrate.auto = true;
        config.hydrate.include = vec!["models/**".to_owned()];

        assert!(matches!(
            pull_hydration_plan(&config).unwrap(),
            PullHydrationPlan::Filtered(_)
        ));
    }
}
