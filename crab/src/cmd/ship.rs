//! `crab ship <patterns> -m <message>` — one-shot add + commit + push.
//!
//! Combines `crab add`, `git commit`, and `crab push` into a single command
//! for users who want a simple "stage, commit, and upload" workflow without
//! thinking in separate git primitives.
//!
//! Uses the native `crab push` pipeline for concurrent uploads rather than
//! shelling out to `git push`.

use std::process::Command;
use std::time::Instant;

use schemars::JsonSchema;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::cmd::add::{AddArgs, AddSummary, run_add_without_terminal_output};
use crate::cmd::push::{PushArgs, PushSummaryPayload, run_push_without_terminal_output};
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::core::style::CliStyle;

// ---------------------------------------------------------------------------
// Per-phase timing
// ---------------------------------------------------------------------------

/// Schema name for the terminal ship JSON envelope.
const SHIP_SCHEMA: &str = "ship";

/// Schema version for the terminal ship payload.
const SHIP_VERSION: &str = "1.0";

/// Per-phase timing results for the ship command.
///
/// Captures wall-clock elapsed time for each phase (staging, commit, push).
/// The push field is `None` when `--no-push` is set.
pub struct ShipTimings {
    pub staging_ms: u64,
    pub commit_ms: u64,
    pub push_ms: Option<u64>,
}

impl ShipTimings {
    /// Format for text display: "Staged in 2.1s, Committed in 0.1s, Pushed in 4.3s"
    pub fn format_text(&self) -> String {
        let mut parts = vec![
            format!("Staged in {:.1}s", self.staging_ms as f64 / 1000.0),
            format!("Committed in {:.1}s", self.commit_ms as f64 / 1000.0),
        ];
        if let Some(push_ms) = self.push_ms {
            parts.push(format!("Pushed in {:.1}s", push_ms as f64 / 1000.0));
        }
        parts.join(", ")
    }

    /// Total elapsed time across all phases.
    pub fn total_ms(&self) -> u64 {
        self.staging_ms + self.commit_ms + self.push_ms.unwrap_or(0)
    }
}

/// Structured JSON payload for ship per-phase timing.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ShipTimingPayload {
    pub staging_ms: u64,
    pub commit_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_ms: Option<u64>,
    pub total_ms: u64,
}

impl From<&ShipTimings> for ShipTimingPayload {
    fn from(t: &ShipTimings) -> Self {
        Self {
            staging_ms: t.staging_ms,
            commit_ms: t.commit_ms,
            push_ms: t.push_ms,
            total_ms: t.total_ms(),
        }
    }
}

/// Structured terminal result for the complete ship operation.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ShipPayload {
    pub add: AddSummary,
    pub dry_run: bool,
    pub committed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push: Option<PushSummaryPayload>,
    pub timings: ShipTimingPayload,
}

/// Arguments for the `crab ship` command.
pub struct ShipArgs {
    /// Glob patterns to ship (e.g. `*.safetensors`, `.`).
    pub patterns: Vec<String>,
    /// Commit message.
    pub message: String,
    /// Maximum number of concurrent file-processing tasks.
    pub jobs: usize,
    /// Push to this remote (default: origin).
    pub remote: String,
    /// Push to this branch (default: current branch).
    pub branch: Option<String>,
    /// Integrate the current branch and retry after non-fast-forward or lock contention.
    pub rebase_on_non_fast_forward: bool,
    /// Maximum integration retry attempts.
    pub rebase_retry_limit: u32,
    /// Skip the push step (just add + commit).
    pub no_push: bool,
    /// Show what would be shipped without making changes.
    pub dry_run: bool,
    /// Output mode.
    pub mode: OutputMode,
}

/// Run the `crab ship` command: add + commit + push in one shot.
pub async fn run_ship(args: &ShipArgs, cancel: &CancellationToken) -> Result<()> {
    let start = Instant::now();
    let style = CliStyle::resolve(args.mode);

    // Dry-run mode: show what would happen without making changes.
    if args.dry_run {
        return run_ship_dry_run(args, cancel).await;
    }

    // --- Phase 1: Staging (crab add) ---
    let staging_start = Instant::now();

    let add_args = AddArgs {
        patterns: args.patterns.clone(),
        jobs: args.jobs,
        dry_run: false,
        skip_git_add: false,
        mode: args.mode,
    };

    if !args.mode.is_machine() {
        eprintln!("Staging files...");
    }
    let add_summary = run_add_without_terminal_output(&add_args, cancel).await?;

    let staging_ms = staging_start.elapsed().as_millis() as u64;

    // --- Phase 2: Commit ---
    let commit_start = Instant::now();

    if !args.mode.is_machine() {
        eprintln!("Committing...");
    }

    let git_dir = crate::git::discover::discover_git_dir()?;
    let repo_root = git_dir
        .parent()
        .ok_or_else(|| CrabError::Internal("git dir has no parent".into()))?;

    let mut metadata_paths = Vec::with_capacity(2);
    if repo_root.join(".gitattributes").exists() {
        metadata_paths.push(".gitattributes");
    }
    if repo_root.join(".crab.toml").exists() {
        metadata_paths.push(".crab.toml");
    }
    crate::git::index::stage_paths(repo_root, &metadata_paths)?;

    let commit_output = Command::new("git")
        .args(["commit", "-m", &args.message])
        .current_dir(repo_root)
        .output()?;

    let mut committed = true;
    if !commit_output.status.success() {
        let stdout = String::from_utf8_lossy(&commit_output.stdout);
        let stderr = String::from_utf8_lossy(&commit_output.stderr);
        // "nothing to commit" is not a commit failure for ship. In the
        // common recovery path, HEAD may already contain the pointer commit
        // while the Crab remote still needs the large-file payload.
        if is_nothing_to_commit(&stdout, &stderr) {
            committed = false;
            if !args.mode.is_machine() {
                if args.no_push {
                    eprintln!("Nothing to commit (files may already be up to date).");
                } else {
                    eprintln!("Nothing to commit; pushing existing HEAD...");
                }
            }
        } else {
            return Err(CrabError::Internal(format!(
                "git commit failed: {}",
                git_command_diagnostics(&stdout, &stderr)
            )));
        }
    }

    let commit_ms = commit_start.elapsed().as_millis() as u64;
    let commit_oid = resolve_head_oid(repo_root)?;

    // --- Phase 3: Push (native concurrent push pipeline) ---
    let (push_summary, push_ms) = if args.no_push {
        (None, None)
    } else {
        let push_start = Instant::now();

        if !args.mode.is_machine() {
            eprintln!("Pushing...");
        }

        let refspec = match &args.branch {
            Some(b) => vec![b.clone()],
            None => Vec::new(), // crab push defaults to current branch
        };

        let push_args = PushArgs {
            remote: Some(args.remote.clone()),
            refspecs: refspec,
            upload_concurrency: None,
            lock_wait_secs: None,
            manifest_cas_retries: None,
            rebase_on_non_fast_forward: args.rebase_on_non_fast_forward,
            rebase_retry_limit: args.rebase_retry_limit,
            dry_run: false,
            force: false,
            follow_tags: false,
            verbose: false,
            no_incremental: false,
            no_color: false,
            json: args.mode == OutputMode::Json,
            jsonl: args.mode == OutputMode::Jsonl,
        };

        let summary = run_push_without_terminal_output(&push_args, cancel).await?;

        (Some(summary), Some(push_start.elapsed().as_millis() as u64))
    };

    // --- Timing summary ---
    let timings = ShipTimings {
        staging_ms,
        commit_ms,
        push_ms,
    };

    let elapsed = start.elapsed();

    match args.mode {
        OutputMode::Text => {
            let timing_line = timings.format_text();
            let commit_word = if committed {
                "committed"
            } else {
                "reused existing commit"
            };
            if args.no_push {
                eprintln!(
                    "{}",
                    style.ok(&format!(
                        "Shipped ({commit_word} locally) in {:.1}s — {}",
                        elapsed.as_secs_f64(),
                        timing_line
                    ))
                );
            } else {
                eprintln!(
                    "{}",
                    style.ok(&format!(
                        "Shipped to {} ({commit_word}) in {:.1}s — {}",
                        args.remote,
                        elapsed.as_secs_f64(),
                        timing_line
                    ))
                );
            }
        }
        OutputMode::Json | OutputMode::Jsonl => {
            let payload = ShipPayload {
                add: add_summary,
                dry_run: false,
                committed,
                commit_oid: Some(commit_oid),
                push: push_summary,
                timings: ShipTimingPayload::from(&timings),
            };
            emit_json(SHIP_SCHEMA, SHIP_VERSION, payload);
        }
    }

    Ok(())
}

/// Git reports an empty commit candidate on stdout in the common path,
/// but some versions/hooks put related diagnostics on stderr. Treat both
/// streams as a single human message for robust no-op detection.
fn is_nothing_to_commit(stdout: &str, stderr: &str) -> bool {
    let combined = format!("{stdout}\n{stderr}");
    combined.contains("nothing to commit") || combined.contains("no changes added")
}

fn git_command_diagnostics(stdout: &str, stderr: &str) -> String {
    let mut parts = Vec::new();
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        parts.push(stderr);
    }
    if !stdout.is_empty() {
        parts.push(stdout);
    }
    if parts.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        parts.join("\n")
    }
}

fn resolve_head_oid(repo_root: &std::path::Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "failed to resolve shipped commit: {}",
            git_command_diagnostics(
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            )
        )));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        Err(CrabError::Internal(
            "git rev-parse HEAD returned an empty object id".to_owned(),
        ))
    } else {
        Ok(value)
    }
}

/// Dry-run mode: show what would be staged, committed, and pushed.
async fn run_ship_dry_run(args: &ShipArgs, cancel: &CancellationToken) -> Result<()> {
    // Run add in dry-run mode to show what would be staged.
    let add_args = AddArgs {
        patterns: args.patterns.clone(),
        jobs: args.jobs,
        dry_run: true,
        skip_git_add: false,
        mode: args.mode,
    };

    if !args.mode.is_machine() {
        eprintln!("Dry run — showing what would be shipped:\n");
        eprintln!("=== Files to stage ===");
    }
    let add_summary = run_add_without_terminal_output(&add_args, cancel).await?;

    if args.mode.is_machine() {
        emit_json(
            SHIP_SCHEMA,
            SHIP_VERSION,
            ShipPayload {
                add: add_summary,
                dry_run: true,
                committed: false,
                commit_oid: None,
                push: None,
                timings: ShipTimingPayload {
                    staging_ms: 0,
                    commit_ms: 0,
                    push_ms: None,
                    total_ms: 0,
                },
            },
        );
        return Ok(());
    }

    // Show current git status for context.
    eprintln!("\n=== Git status ===");
    let status_output = Command::new("git").args(["status", "--porcelain"]).output();
    if let Ok(output) = status_output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.is_empty() {
            eprintln!("  (no changes)");
        } else {
            for line in stdout.lines() {
                eprintln!("  {line}");
            }
        }
    }

    // Show push target.
    eprintln!("\n=== Push target ===");
    let branch_display = args.branch.as_deref().unwrap_or("(current branch)");
    eprintln!("  Remote: {}", args.remote);
    eprintln!("  Branch: {branch_display}");

    if args.no_push {
        eprintln!("\n  (push skipped: --no-push)");
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn ship_timings_format_text_all_phases() {
        let timings = ShipTimings {
            staging_ms: 2100,
            commit_ms: 150,
            push_ms: Some(4300),
        };
        let text = timings.format_text();
        assert_eq!(text, "Staged in 2.1s, Committed in 0.1s, Pushed in 4.3s");
    }

    #[test]
    fn nothing_to_commit_detection_checks_stdout_and_stderr() {
        assert!(is_nothing_to_commit(
            "On branch main\nnothing to commit, working tree clean\n",
            ""
        ));
        assert!(is_nothing_to_commit("", "no changes added to commit\n"));
        assert!(!is_nothing_to_commit(
            "",
            "fatal: unable to auto-detect email address\n"
        ));
    }

    #[test]
    fn ship_timings_format_text_no_push() {
        let timings = ShipTimings {
            staging_ms: 1000,
            commit_ms: 500,
            push_ms: None,
        };
        let text = timings.format_text();
        assert_eq!(text, "Staged in 1.0s, Committed in 0.5s");
        assert!(!text.contains("Pushed"));
    }

    #[test]
    fn ship_timings_format_text_zero_values() {
        let timings = ShipTimings {
            staging_ms: 0,
            commit_ms: 0,
            push_ms: Some(0),
        };
        let text = timings.format_text();
        assert_eq!(text, "Staged in 0.0s, Committed in 0.0s, Pushed in 0.0s");
    }

    #[test]
    fn ship_timings_format_text_sub_second() {
        let timings = ShipTimings {
            staging_ms: 50,
            commit_ms: 10,
            push_ms: Some(200),
        };
        let text = timings.format_text();
        // 50ms = 0.1s (rounded), 10ms = 0.0s, 200ms = 0.2s
        assert!(text.contains("Staged in 0.1s"));
        assert!(text.contains("Committed in 0.0s"));
        assert!(text.contains("Pushed in 0.2s"));
    }

    #[test]
    fn ship_timings_total_ms_with_push() {
        let timings = ShipTimings {
            staging_ms: 1000,
            commit_ms: 200,
            push_ms: Some(3000),
        };
        assert_eq!(timings.total_ms(), 4200);
    }

    #[test]
    fn ship_timings_total_ms_without_push() {
        let timings = ShipTimings {
            staging_ms: 1000,
            commit_ms: 200,
            push_ms: None,
        };
        assert_eq!(timings.total_ms(), 1200);
    }

    #[test]
    fn ship_timing_payload_from_timings() {
        let timings = ShipTimings {
            staging_ms: 2100,
            commit_ms: 150,
            push_ms: Some(4300),
        };
        let payload = ShipTimingPayload::from(&timings);
        assert_eq!(payload.staging_ms, 2100);
        assert_eq!(payload.commit_ms, 150);
        assert_eq!(payload.push_ms, Some(4300));
        assert_eq!(payload.total_ms, 6550);
    }

    #[test]
    fn ship_timing_payload_no_push() {
        let timings = ShipTimings {
            staging_ms: 1000,
            commit_ms: 500,
            push_ms: None,
        };
        let payload = ShipTimingPayload::from(&timings);
        assert_eq!(payload.push_ms, None);
        assert_eq!(payload.total_ms, 1500);
    }

    #[test]
    fn ship_timing_payload_serializes_correctly() {
        let payload = ShipTimingPayload {
            staging_ms: 2100,
            commit_ms: 150,
            push_ms: Some(4300),
            total_ms: 6550,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["staging_ms"], 2100);
        assert_eq!(json["commit_ms"], 150);
        assert_eq!(json["push_ms"], 4300);
        assert_eq!(json["total_ms"], 6550);
    }

    #[test]
    fn ship_timing_payload_omits_push_ms_when_none() {
        let payload = ShipTimingPayload {
            staging_ms: 1000,
            commit_ms: 500,
            push_ms: None,
            total_ms: 1500,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["staging_ms"], 1000);
        assert_eq!(json["commit_ms"], 500);
        assert!(json.get("push_ms").is_none());
        assert_eq!(json["total_ms"], 1500);
    }

    #[test]
    fn ship_timing_format_one_decimal_place() {
        // Verify the format always produces one decimal place
        let timings = ShipTimings {
            staging_ms: 1234,
            commit_ms: 5678,
            push_ms: Some(9012),
        };
        let text = timings.format_text();
        // 1234ms = 1.2s, 5678ms = 5.7s, 9012ms = 9.0s
        assert!(text.contains("1.2s"));
        assert!(text.contains("5.7s"));
        assert!(text.contains("9.0s"));
    }
}
