//! `crab workflow journal` — inspect and prune workflow run journals.
//!
//! Runs under `crab run` leave a SQLite journal at
//! `.crab/workflow/runs/<run_id>/journal.db`. This command group
//! exposes three operator-facing views of those journals:
//!
//! - **`show <run_id>`**: replay every stage's state trajectory for
//!   one run. Useful after a crash to understand where things
//!   stopped, or when diagnosing a stuck resume.
//! - **`ls`**: list every journal under `runs/` with its start time
//!   and outcome. Terminal runs are tagged `success` / `failure` /
//!   `aborted`; non-terminal ones are tagged `in_flight`.
//! - **`gc [--keep N=50]`**: delete the oldest terminal journals,
//!   keeping the most recent `N`. In-flight journals are never
//!   touched — the resume path needs them intact. `--dry-run` lists
//!   what would be removed without touching the filesystem.
//!
//! All three support `--json` via `core/output::emit_json`. Schemas:
//! `workflow.journal.show`, `workflow.journal.ls`,
//! `workflow.journal.gc`.
//!
//! The command is pure-read (apart from `gc`). `gc` acquires the
//! workflow scheduler lock before scanning and deleting so a run
//! cannot transition a journal after the retention decision.

use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use serde::Serialize;
use tracing::{info, warn};
use uuid::Uuid;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crate::workflow::journal::{Journal, RunOutcome, RunRow, StageRunRow};
use tokio_util::sync::CancellationToken;

/// Structured-output schema labels for the three subcommands.
pub const WORKFLOW_JOURNAL_SHOW_SCHEMA: &str = "workflow.journal.show";
pub const WORKFLOW_JOURNAL_LS_SCHEMA: &str = "workflow.journal.ls";
pub const WORKFLOW_JOURNAL_GC_SCHEMA: &str = "workflow.journal.gc";

/// Default retention for `workflow journal gc` — keep the 50 most
/// recent terminal journals. Tracks the operator-visible default
/// called out in the design's risk register.
pub const DEFAULT_GC_KEEP: usize = 50;
const MAX_WORKFLOW_JOURNALS: usize = 1_000_000;
const MAX_WORKFLOW_STAGE_ROWS: usize = 1_000_000;
const MAX_WORKFLOW_GC_FAILURES: usize = 1_024;

// ─── Clap surface ─────────────────────────────────────────────────────

/// `crab workflow journal <sub>` dispatch.
#[derive(Debug, Clone, Subcommand)]
pub enum JournalCmd {
    /// Print one journal's full stage trajectory.
    Show(ShowArgs),
    /// List every journal under `.crab/workflow/runs/`.
    Ls(LsArgs),
    /// Remove the oldest terminal journals beyond the keep count.
    Gc(GcArgs),
}

/// Args for `crab workflow journal show <run_id>`.
#[derive(Debug, Clone, Parser)]
pub struct ShowArgs {
    /// UUID of the run to inspect.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl ShowArgs {
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab workflow journal ls`.
#[derive(Debug, Clone, Parser, Default)]
pub struct LsArgs {
    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl LsArgs {
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab workflow journal gc`.
#[derive(Debug, Clone, Parser)]
pub struct GcArgs {
    /// Number of most-recent terminal journals to keep. Default 50.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_GC_KEEP)]
    pub keep: usize,

    /// Report what would be removed without touching the
    /// filesystem. Compose with `--json` for scripted previews.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl Default for GcArgs {
    fn default() -> Self {
        Self {
            keep: DEFAULT_GC_KEEP,
            dry_run: false,
            json: false,
        }
    }
}

impl GcArgs {
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

// ─── Payloads ─────────────────────────────────────────────────────────

/// Top-level payload for `workflow.journal.show`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ShowPayload {
    pub run_id: String,
    /// Unix epoch seconds. `None` when the run row is missing
    /// (journal never had `insert_run_start` called — should not
    /// happen for a well-formed run, but we surface it cleanly).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// One of `success` / `failure` / `aborted` / `in_flight`.
    pub outcome: String,
    /// Every stage row for the run, ordered by `(stage_name,
    /// attempt)`. One row per attempt per stage.
    pub stages: Vec<StageRowView>,
}

/// Payload for one row emitted by `show`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StageRowView {
    pub stage: String,
    pub attempt: u32,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub timed_out: bool,
    /// Unix epoch seconds the row was last updated.
    pub updated_at: i64,
    /// Last 8 KiB of stderr on failure. Absent on success paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

/// Top-level payload for `workflow.journal.ls`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LsPayload {
    /// One entry per discovered journal. Sorted newest-first by
    /// `started_at` so the most relevant entries show at the top
    /// of both the JSON array and the text table.
    pub journals: Vec<JournalSummary>,
}

/// One row of the `ls` output.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JournalSummary {
    pub run_id: String,
    /// Unix epoch seconds. `None` if the journal has no `runs` row
    /// (corruption; surfaced rather than silently dropped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// One of `success` / `failure` / `aborted` / `in_flight`.
    pub outcome: String,
}

/// Top-level payload for `workflow.journal.gc`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GcPayload {
    pub keep: usize,
    pub dry_run: bool,
    /// Run IDs that would be (or were) removed, in the order the
    /// scanner found them after sorting by `started_at`.
    pub removed: Vec<String>,
    /// Run IDs retained under the keep policy.
    pub kept: Vec<String>,
    /// Non-terminal journals skipped outright — never GC candidates.
    pub in_flight: Vec<String>,
}

// ─── Entry points ─────────────────────────────────────────────────────

/// `crab workflow journal show`.
pub fn exec_show(args: ShowArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    let worktree = crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)?;
    run_show(&args, &worktree.current_worktree_root, args.output_mode())
}

/// `crab workflow journal ls`.
pub fn exec_ls(args: LsArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    let worktree = crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)?;
    run_ls(&args, &worktree.current_worktree_root, args.output_mode())
}

/// `crab workflow journal gc`.
pub fn exec_gc(args: GcArgs) -> Result<()> {
    exec_gc_with_cancel(args, &CancellationToken::new())
}

/// Run workflow journal GC while honoring the caller's cancellation token.
pub fn exec_gc_with_cancel(args: GcArgs, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    let worktree = crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)?;
    run_gc_with_cancel(
        &args,
        &worktree.current_worktree_root,
        args.output_mode(),
        cancel,
    )
    .map(|_| ())
}

/// Testable variant of `show`.
pub fn run_show(args: &ShowArgs, repo_root: &Path, mode: OutputMode) -> Result<()> {
    let run_id = Uuid::parse_str(&args.run_id).map_err(|_| CrabError::Configuration {
        key: format!("invalid run_id: {}", args.run_id),
        origin: "cli".into(),
    })?;
    let journal_path = journal_path_for(repo_root, run_id);
    if !journal_path.exists() {
        return Err(CrabError::NotFound {
            path: journal_path.display().to_string(),
        });
    }
    let journal = Journal::open(&journal_path)?;
    let run_row = journal.run_row(run_id)?;
    let rows = journal.all_stage_rows_with_limit(run_id, MAX_WORKFLOW_STAGE_ROWS)?;
    let payload = build_show_payload(run_id, run_row.as_ref(), &rows);
    emit_show(&payload, mode);
    Ok(())
}

/// Testable variant of `ls`.
pub fn run_ls(_args: &LsArgs, repo_root: &Path, mode: OutputMode) -> Result<()> {
    let runs_dir = runs_dir(repo_root);
    let summaries = collect_summaries(&runs_dir)?;
    let payload = LsPayload {
        journals: summaries,
    };
    emit_ls(&payload, mode);
    Ok(())
}

/// Testable variant of `gc`. Returns the payload so tests can
/// inspect it without parsing stdout.
pub fn run_gc(args: &GcArgs, repo_root: &Path, mode: OutputMode) -> Result<GcPayload> {
    run_gc_with_cancel(args, repo_root, mode, &CancellationToken::new())
}

/// Testable variant of `gc` with cancellation support.
pub fn run_gc_with_cancel(
    args: &GcArgs,
    repo_root: &Path,
    mode: OutputMode,
    cancel: &CancellationToken,
) -> Result<GcPayload> {
    check_cancelled(cancel)?;
    let runs_dir = runs_dir(repo_root);
    let workflow_root = runs_dir
        .parent()
        .ok_or_else(|| CrabError::Internal("workflow runs directory has no parent".to_owned()))?;
    let _scheduler_lock = crate::workflow::scheduler_lock::SchedulerLock::try_acquire(
        workflow_root,
    )?
    .ok_or(CrabError::ConcurrentMaintenance {
        other: "workflow run",
    })?;
    let summaries = collect_summaries_with_cancel(&runs_dir, cancel)?;
    let payload = decide_gc(summaries, args.keep);
    let mut removed = Vec::new();
    let mut failures = Vec::new();
    let mut failures_omitted = 0usize;
    if !args.dry_run {
        for run_id in &payload.removed {
            check_cancelled(cancel)?;
            let dir = runs_dir.join(run_id);
            match fs::remove_dir_all(&dir) {
                Ok(()) => {
                    info!(run_id = %run_id, "workflow journal gc: removed");
                    removed.push(run_id.clone());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    removed.push(run_id.clone());
                }
                Err(error) if failures.len() < MAX_WORKFLOW_GC_FAILURES => {
                    failures.push(format!("{run_id}: {error}"));
                }
                Err(_) => failures_omitted = failures_omitted.saturating_add(1),
            }
        }
    } else {
        check_cancelled(cancel)?;
        removed.clone_from(&payload.removed);
    }
    check_cancelled(cancel)?;
    let out = GcPayload {
        keep: args.keep,
        dry_run: args.dry_run,
        removed,
        kept: payload.kept.clone(),
        in_flight: payload.in_flight.clone(),
    };
    emit_gc(&out, mode);
    if !failures.is_empty() || failures_omitted > 0 {
        return Err(CrabError::Internal(format!(
            "workflow journal GC failed to remove {} journal(s): {}{}",
            failures.len().saturating_add(failures_omitted),
            failures.join("; "),
            if failures_omitted > 0 {
                format!(" ({} additional failures omitted)", failures_omitted)
            } else {
                String::new()
            }
        )));
    }
    Ok(out)
}

// ─── Scanning helpers ─────────────────────────────────────────────────

fn runs_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".crab").join("workflow").join("runs")
}

fn journal_path_for(repo_root: &Path, run_id: Uuid) -> PathBuf {
    runs_dir(repo_root)
        .join(run_id.to_string())
        .join("journal.db")
}

/// Walk `runs/` and return one summary per valid journal, sorted
/// newest-first by `started_at`. Directories that don't parse as a
/// UUID or whose journal can't be opened are silently skipped —
/// the operator doesn't need `ls` to crash on a corrupt subdirectory.
///
/// Returns an empty vector when `runs/` is absent (fresh repo, or a
/// repo that has never run a workflow).
fn collect_summaries(runs_dir: &Path) -> Result<Vec<JournalSummary>> {
    collect_summaries_with_cancel(runs_dir, &CancellationToken::new())
}

fn collect_summaries_with_cancel(
    runs_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<JournalSummary>> {
    check_cancelled(cancel)?;
    let mut out: Vec<JournalSummary> = Vec::new();
    let entries = match fs::read_dir(runs_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(CrabError::Io(e)),
    };
    for entry in entries {
        check_cancelled(cancel)?;
        let entry = entry.map_err(CrabError::Io)?;
        if !entry.file_type().map_err(CrabError::Io)?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let run_id_str = name.to_string_lossy().into_owned();
        let Ok(run_id) = Uuid::parse_str(&run_id_str) else {
            continue;
        };
        let journal_path = entry.path().join("journal.db");
        if !journal_path.exists() {
            continue;
        }
        if out.len() >= MAX_WORKFLOW_JOURNALS {
            return Err(CrabError::Configuration {
                key: "workflow journal count".to_owned(),
                origin: format!(
                    "workflow journal directory contains more than {MAX_WORKFLOW_JOURNALS} readable journals"
                ),
            });
        }
        let journal = match Journal::open(&journal_path) {
            Ok(j) => j,
            Err(e) => {
                warn!(run_id = %run_id, error = %e, "workflow journal: skipping unreadable journal");
                continue;
            }
        };
        let run_row = journal.run_row(run_id).ok().flatten();
        let outcome = run_row
            .as_ref()
            .and_then(|r| r.outcome)
            .map_or("in_flight", RunOutcome::as_str)
            .to_owned();
        out.push(JournalSummary {
            run_id: run_id.to_string(),
            started_at: run_row.as_ref().map(|r| r.started_at),
            outcome,
        });
    }
    // Newest first. Journals without a `started_at` sort last so
    // well-formed rows stay on top. When `started_at` ties (the
    // journal only stores unix seconds, so two back-to-back runs
    // collide), fall back to comparing the `run_id` string: UUIDv7
    // is timestamp-prefixed, so its lexicographic order is also
    // chronological, and we want newest-first here too.
    out.sort_by(|a, b| {
        let a_key = a.started_at.unwrap_or(i64::MIN);
        let b_key = b.started_at.unwrap_or(i64::MIN);
        b_key.cmp(&a_key).then_with(|| b.run_id.cmp(&a.run_id))
    });
    Ok(out)
}

/// Decide which journals to keep, remove, or skip-as-in-flight.
/// Pure function — separated from filesystem effects so unit tests
/// can exercise the retention logic without creating SQLite files.
fn decide_gc(summaries: Vec<JournalSummary>, keep: usize) -> GcDecision {
    let mut in_flight: Vec<String> = Vec::new();
    let mut terminal: Vec<JournalSummary> = Vec::new();
    for s in summaries {
        if is_terminal_outcome(&s.outcome) {
            terminal.push(s);
        } else {
            in_flight.push(s.run_id);
        }
    }
    // `terminal` is already newest-first from `collect_summaries`.
    let (kept, removed): (Vec<JournalSummary>, Vec<JournalSummary>) = if terminal.len() <= keep {
        (terminal, Vec::new())
    } else {
        let (k, r) = terminal.split_at(keep);
        (k.to_vec(), r.to_vec())
    };
    GcDecision {
        kept: kept.into_iter().map(|s| s.run_id).collect(),
        removed: removed.into_iter().map(|s| s.run_id).collect(),
        in_flight,
    }
}

/// An outcome string is terminal when it's one of the three values
/// [`RunOutcome::as_str`] produces.
fn is_terminal_outcome(s: &str) -> bool {
    matches!(s, "success" | "failure" | "aborted")
}

#[derive(Debug, Clone)]
struct GcDecision {
    kept: Vec<String>,
    removed: Vec<String>,
    in_flight: Vec<String>,
}

// ─── Show payload builder ─────────────────────────────────────────────

fn build_show_payload(run_id: Uuid, run_row: Option<&RunRow>, rows: &[StageRunRow]) -> ShowPayload {
    let outcome = run_row
        .and_then(|r| r.outcome)
        .map_or("in_flight", RunOutcome::as_str)
        .to_owned();
    let stages = rows.iter().map(stage_view).collect();
    ShowPayload {
        run_id: run_id.to_string(),
        started_at: run_row.map(|r| r.started_at),
        ended_at: run_row.and_then(|r| r.ended_at),
        outcome,
        stages,
    }
}

fn stage_view(row: &StageRunRow) -> StageRowView {
    StageRowView {
        stage: row.stage_name.clone(),
        attempt: row.attempt,
        state: row.state.to_string(),
        exit_code: row.exit_code,
        signal: row.signal,
        timed_out: row.timed_out,
        updated_at: row.updated_at,
        stderr_tail: row.stderr_tail.clone(),
    }
}

// ─── Output rendering ─────────────────────────────────────────────────

fn emit_show(payload: &ShowPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(WORKFLOW_JOURNAL_SHOW_SCHEMA, "1.0", payload);
        }
        OutputMode::Text => {
            println!("run_id: {}", payload.run_id);
            println!("outcome: {}", payload.outcome);
            if let Some(started) = payload.started_at {
                println!("started_at: {started}");
            }
            if let Some(ended) = payload.ended_at {
                println!("ended_at: {ended}");
            }
            if payload.stages.is_empty() {
                println!("stages: (none)");
                return;
            }
            println!("stages:");
            for row in &payload.stages {
                use std::fmt::Write as _;
                let mut line = format!(
                    "  {:<30} attempt={} state={}",
                    row.stage, row.attempt, row.state
                );
                if let Some(code) = row.exit_code {
                    let _ = write!(line, " exit={code}");
                }
                if let Some(sig) = row.signal {
                    let _ = write!(line, " signal={sig}");
                }
                if row.timed_out {
                    line.push_str(" timed_out=true");
                }
                println!("{line}");
            }
        }
    }
}

fn emit_ls(payload: &LsPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(WORKFLOW_JOURNAL_LS_SCHEMA, "1.0", payload);
        }
        OutputMode::Text => {
            if payload.journals.is_empty() {
                println!("No workflow journals found.");
                return;
            }
            // Tabular header row: column titles are pure literals by
            // design, even though clippy would prefer them inlined.
            #[allow(
                clippy::print_literal,
                reason = "table header row uses literal strings"
            )]
            {
                println!("{:<36}  {:<12}  {}", "RUN_ID", "OUTCOME", "STARTED_AT");
            }
            for j in &payload.journals {
                let started = j
                    .started_at
                    .map_or_else(|| "-".to_owned(), |s| s.to_string());
                println!("{:<36}  {:<12}  {}", j.run_id, j.outcome, started);
            }
        }
    }
}

fn emit_gc(payload: &GcPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(WORKFLOW_JOURNAL_GC_SCHEMA, "1.0", payload);
        }
        OutputMode::Text => {
            let label = if payload.dry_run {
                "would remove"
            } else {
                "removed"
            };
            info!(
                keep = payload.keep,
                dry_run = payload.dry_run,
                removed = payload.removed.len(),
                kept = payload.kept.len(),
                in_flight = payload.in_flight.len(),
                "workflow journal gc",
            );
            if payload.removed.is_empty() {
                println!("No terminal journals beyond --keep={}.", payload.keep);
            } else {
                println!(
                    "{} {} terminal journal(s); kept {}, in-flight {} skipped:",
                    label,
                    payload.removed.len(),
                    payload.kept.len(),
                    payload.in_flight.len(),
                );
                for run_id in &payload.removed {
                    println!("  {run_id}");
                }
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use crab_workflow::StageState;
    use tempfile::TempDir;

    /// Build a journal at `.crab/workflow/runs/<run_id>/journal.db`
    /// under `root` and return the opened handle so the test can
    /// transition states without re-resolving the path.
    fn seed_journal(root: &Path, run_id: Uuid) -> Journal {
        let path = root
            .join(".crab")
            .join("workflow")
            .join("runs")
            .join(run_id.to_string())
            .join("journal.db");
        let j = Journal::open(&path).unwrap();
        j.insert_run_start(run_id, "test", "host").unwrap();
        j
    }

    #[test]
    fn ls_returns_empty_when_no_journals_exist() {
        let tmp = TempDir::new().unwrap();
        let args = LsArgs::default();
        // Exercise through the testable entry point so the
        // NotFound-on-runs-dir branch is covered.
        run_ls(&args, tmp.path(), OutputMode::Json).unwrap();

        // And verify the internal scanner directly.
        let summaries = collect_summaries(&runs_dir(tmp.path())).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn ls_lists_terminal_and_in_flight_runs_sorted_newest_first() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let run_old = Uuid::now_v7();
        {
            let j = seed_journal(root, run_old);
            j.mark_run_outcome(run_old, RunOutcome::Success).unwrap();
        }
        // Bump the clock so the next UUIDv7 sorts strictly after.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let run_new = Uuid::now_v7();
        {
            let _j = seed_journal(root, run_new);
            // Leave in-flight.
        }

        let summaries = collect_summaries(&runs_dir(root)).unwrap();
        assert_eq!(summaries.len(), 2);
        // Newest journal is the in-flight one we seeded last.
        assert_eq!(summaries[0].run_id, run_new.to_string());
        assert_eq!(summaries[0].outcome, "in_flight");
        assert_eq!(summaries[1].run_id, run_old.to_string());
        assert_eq!(summaries[1].outcome, "success");
    }

    #[test]
    fn show_emits_one_row_per_stage_attempt() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let run_id = Uuid::now_v7();

        let j = seed_journal(root, run_id);
        j.insert_stage_start(run_id, "build").unwrap();
        j.transition(run_id, "build", 1, StageState::Resolved, "{}")
            .unwrap();
        j.transition(run_id, "build", 1, StageState::CacheChecked, "{}")
            .unwrap();

        let rows = j.all_stage_rows(run_id).unwrap();
        let run_row = j.run_row(run_id).unwrap();
        let payload = build_show_payload(run_id, run_row.as_ref(), &rows);
        assert_eq!(payload.run_id, run_id.to_string());
        assert_eq!(payload.outcome, "in_flight");
        assert_eq!(payload.stages.len(), 1);
        let row = &payload.stages[0];
        assert_eq!(row.stage, "build");
        assert_eq!(row.attempt, 1);
        // Latest recorded state wins (the journal stores one row
        // per attempt and transitions update in place).
        assert_eq!(row.state, StageState::CacheChecked.to_string());
    }

    #[test]
    fn show_run_missing_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let args = ShowArgs {
            run_id: Uuid::now_v7().to_string(),
            json: false,
        };
        let err =
            run_show(&args, tmp.path(), OutputMode::Text).expect_err("missing journal must fail");
        assert!(matches!(err, CrabError::NotFound { .. }));
    }

    #[test]
    fn show_rejects_malformed_run_id() {
        let tmp = TempDir::new().unwrap();
        let args = ShowArgs {
            run_id: "not-a-uuid".into(),
            json: false,
        };
        let err =
            run_show(&args, tmp.path(), OutputMode::Text).expect_err("non-UUID run_id must fail");
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn gc_with_keep_1_removes_all_but_newest_terminal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Three terminal journals, spaced out so their UUIDv7
        // timestamps sort distinctly.
        let mut terminal_ids = Vec::new();
        for _ in 0..3 {
            let run_id = Uuid::now_v7();
            let j = seed_journal(root, run_id);
            j.mark_run_outcome(run_id, RunOutcome::Success).unwrap();
            terminal_ids.push(run_id);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let args = GcArgs {
            keep: 1,
            dry_run: false,
            json: false,
        };
        let payload = run_gc(&args, root, OutputMode::Json).unwrap();

        // Kept the newest terminal journal; removed the other two.
        assert_eq!(payload.kept.len(), 1);
        assert_eq!(payload.kept[0], terminal_ids[2].to_string());
        assert_eq!(payload.removed.len(), 2);
        assert!(payload.removed.contains(&terminal_ids[0].to_string()));
        assert!(payload.removed.contains(&terminal_ids[1].to_string()));

        // Filesystem matches the decision.
        let runs = runs_dir(root);
        assert!(runs.join(terminal_ids[2].to_string()).exists());
        assert!(!runs.join(terminal_ids[0].to_string()).exists());
        assert!(!runs.join(terminal_ids[1].to_string()).exists());
    }

    #[test]
    fn gc_never_removes_in_flight_journals() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // One in-flight journal; no terminal journals.
        let in_flight = Uuid::now_v7();
        let _j = seed_journal(root, in_flight);

        let args = GcArgs {
            keep: 0,
            dry_run: false,
            json: false,
        };
        let payload = run_gc(&args, root, OutputMode::Json).unwrap();
        assert!(payload.removed.is_empty());
        assert_eq!(payload.in_flight, vec![in_flight.to_string()]);
        // Journal directory still exists.
        assert!(runs_dir(root).join(in_flight.to_string()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn gc_ignores_symlinked_run_directories() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let run_id = Uuid::now_v7();
        let outside_journal = outside.path().join("journal.db");
        let journal = Journal::open(&outside_journal).unwrap();
        journal.insert_run_start(run_id, "test", "host").unwrap();
        journal
            .mark_run_outcome(run_id, RunOutcome::Success)
            .unwrap();
        let runs = runs_dir(tmp.path());
        fs::create_dir_all(&runs).unwrap();
        std::os::unix::fs::symlink(outside.path(), runs.join(run_id.to_string())).unwrap();

        let payload = run_gc(
            &GcArgs {
                keep: 0,
                dry_run: false,
                json: false,
            },
            tmp.path(),
            OutputMode::Json,
        )
        .unwrap();

        assert!(payload.removed.is_empty());
        assert!(outside_journal.exists());
    }

    #[test]
    fn gc_dry_run_preserves_filesystem() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let run_id = Uuid::now_v7();
        let j = seed_journal(root, run_id);
        j.mark_run_outcome(run_id, RunOutcome::Failure).unwrap();
        drop(j);

        let args = GcArgs {
            keep: 0,
            dry_run: true,
            json: false,
        };
        let payload = run_gc(&args, root, OutputMode::Json).unwrap();
        assert_eq!(payload.removed, vec![run_id.to_string()]);
        // Directory is still on disk because of --dry-run.
        assert!(runs_dir(root).join(run_id.to_string()).exists());
    }

    #[test]
    fn gc_honors_cancellation_before_scanning_or_deleting() {
        let tmp = TempDir::new().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = run_gc_with_cancel(
            &GcArgs {
                keep: 0,
                dry_run: false,
                json: false,
            },
            tmp.path(),
            OutputMode::Json,
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(error, CrabError::Cancelled));
    }

    #[test]
    fn decide_gc_partitions_terminal_and_in_flight_cleanly() {
        // Pure unit test — no sqlite required. Exercises the
        // partition/retain logic end-to-end at minimal cost.
        let summaries = vec![
            JournalSummary {
                run_id: "r-new-term".into(),
                started_at: Some(300),
                outcome: "success".into(),
            },
            JournalSummary {
                run_id: "r-mid-inflight".into(),
                started_at: Some(200),
                outcome: "in_flight".into(),
            },
            JournalSummary {
                run_id: "r-old-term".into(),
                started_at: Some(100),
                outcome: "aborted".into(),
            },
        ];
        let decision = decide_gc(summaries, 1);
        assert_eq!(decision.kept, vec!["r-new-term"]);
        assert_eq!(decision.removed, vec!["r-old-term"]);
        assert_eq!(decision.in_flight, vec!["r-mid-inflight"]);
    }

    #[test]
    fn decide_gc_keeps_all_when_terminal_count_is_at_or_below_keep() {
        let summaries = vec![
            JournalSummary {
                run_id: "r1".into(),
                started_at: Some(200),
                outcome: "success".into(),
            },
            JournalSummary {
                run_id: "r2".into(),
                started_at: Some(100),
                outcome: "failure".into(),
            },
        ];
        let decision = decide_gc(summaries, 5);
        assert_eq!(decision.kept, vec!["r1", "r2"]);
        assert!(decision.removed.is_empty());
    }
}
