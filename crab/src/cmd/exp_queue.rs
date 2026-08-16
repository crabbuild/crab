//! `crab exp queue` / `crab exp start` / `crab exp status` / `crab exp stop`
//!
//! Experiment queue management commands. These subcommands allow users to:
//! - Queue experiments with parameter overrides (including range expressions)
//! - Start processing the queue with parallel workers
//! - Check queue status
//! - Remove non-active queue entries
//! - Stop workers and interrupt running tasks on request

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crab_workflow::{ExpQueue, ExpQueueEntry, ExpStatus, ExperimentId};

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::workflow::exp_range::{cartesian_product, expand_param_value};
use crate::workflow::exp_worktree::override_allows_missing_value;

/// Schema labels for structured output.
pub const EXP_QUEUE_SCHEMA: &str = "workflow.exp.queue";
pub const EXP_START_SCHEMA: &str = "workflow.exp.start";
pub const EXP_STATUS_SCHEMA: &str = "workflow.exp.status";
pub const EXP_QUEUE_REMOVE_SCHEMA: &str = "workflow.exp.queue.remove";
pub const EXP_QUEUE_LOGS_SCHEMA: &str = "workflow.exp.queue.logs";
pub const EXP_QUEUE_KILL_SCHEMA: &str = "workflow.exp.queue.kill";
pub const EXP_STOP_SCHEMA: &str = "workflow.exp.stop";

const EXP_SCHEMA_VERSION: &str = "1.0";
const QUEUE_LOG_POLL_INTERVAL: Duration = Duration::from_millis(250);

// ─── Clap args ────────────────────────────────────────────────────────

/// Args for `crab exp queue`.
#[derive(Debug, Clone, Parser)]
pub struct QueueArgs {
    /// Parameter overrides as `key=value`. Repeatable.
    /// Values may be range expressions: `key=range(start,stop,step)`
    /// or comma-separated lists: `key=val1,val2,val3`.
    /// Multiple `--set-param` flags produce the Cartesian product.
    #[arg(
        long = "set-param",
        short = 'S',
        alias = "set",
        value_name = "KEY=VALUE"
    )]
    pub set_param: Vec<String>,

    /// Repo-relative ignored or untracked paths to copy into each
    /// experiment worktree before running.
    #[arg(long = "copy-paths", short = 'C', value_name = "PATH")]
    pub copy_paths: Vec<PathBuf>,

    /// Human-readable message stored with each queued experiment.
    #[arg(long, short = 'm', value_name = "MESSAGE")]
    pub message: Option<String>,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl QueueArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp start`.
#[derive(Debug, Clone, Parser)]
pub struct StartArgs {
    /// Maximum number of parallel experiment workers.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub jobs: u32,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl StartArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp status`.
#[derive(Debug, Clone, Parser)]
pub struct StatusArgs {
    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl StatusArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab queue remove`.
#[derive(Debug, Clone, Parser)]
pub struct QueueRemoveArgs {
    /// Queued task ids or unambiguous prefixes to remove.
    #[arg(value_name = "TASK")]
    pub ids: Vec<String>,

    /// Remove every non-running queue entry.
    #[arg(long, default_value_t = false)]
    pub all: bool,

    /// Remove queued-but-not-started entries.
    #[arg(long, default_value_t = false)]
    pub queued: bool,

    /// Remove successfully completed entries.
    #[arg(long, default_value_t = false)]
    pub success: bool,

    /// Remove failed entries.
    #[arg(long, default_value_t = false)]
    pub failed: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl QueueRemoveArgs {
    pub(crate) fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab queue logs`.
#[derive(Debug, Clone, Parser)]
pub struct QueueLogsArgs {
    /// Queued task id or unambiguous prefix.
    #[arg(value_name = "TASK")]
    pub id: String,

    /// Text encoding for task output. Crab queue logs are UTF-8.
    #[arg(long, short = 'e', value_name = "ENCODING")]
    pub encoding: Option<String>,

    /// Follow a running task until it completes.
    #[arg(long, short = 'f', default_value_t = false)]
    pub follow: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl QueueLogsArgs {
    pub(crate) fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab queue kill`.
#[derive(Debug, Clone, Parser)]
pub struct QueueKillArgs {
    /// Running queued task ids or unambiguous prefixes to interrupt.
    #[arg(value_name = "TASK")]
    pub ids: Vec<String>,

    /// Immediately kill selected tasks. With no task ids, kills every running task.
    #[arg(long, short = 'f', default_value_t = false)]
    pub force: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl QueueKillArgs {
    pub(crate) fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp stop`.
#[derive(Debug, Clone, Parser)]
pub struct StopArgs {
    /// Kill currently running tasks instead of waiting for them to finish.
    #[arg(long, default_value_t = false)]
    pub kill: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl StopArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

// ─── Payloads ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct QueueTargetFlags {
    pub recursive: bool,
    pub single_item: bool,
    pub downstream: bool,
    pub force_downstream: bool,
    pub pipeline: bool,
    pub all_pipelines: bool,
    pub glob: bool,
}

/// Payload emitted by `exp queue`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpQueuePayload {
    /// Number of experiments queued in this invocation.
    pub queued_count: usize,
    /// IDs of the queued experiments.
    pub experiment_ids: Vec<String>,
    /// The base commit all experiments snapshot from.
    pub base_commit: String,
}

/// Payload emitted by `exp start`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpStartPayload {
    /// Number of experiments processed.
    pub processed: usize,
    /// Number that completed successfully.
    pub succeeded: usize,
    /// Number that failed.
    pub failed: usize,
    /// IDs of experiments that succeeded.
    pub succeeded_ids: Vec<String>,
    /// IDs of experiments that failed.
    pub failed_ids: Vec<String>,
}

/// Payload emitted by `exp status`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpStatusPayload {
    pub pending: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    pub total: usize,
}

/// Payload emitted by `queue remove`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpQueueRemovePayload {
    /// Queue entry ids removed from disk.
    pub removed: Vec<String>,
    /// Running queue entries that matched a broad selector but were left intact.
    pub skipped_running: Vec<String>,
}

/// Payload emitted by `queue logs`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpQueueLogsPayload {
    /// Resolved queue task id.
    pub id: String,
    /// Current or persisted console output for the task.
    pub contents: String,
    /// Number of UTF-8 bytes in `contents`.
    pub bytes: usize,
    /// Whether `--follow` was requested.
    pub followed: bool,
}

/// Payload emitted by `queue kill`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpQueueKillPayload {
    /// Queue task ids that received a kill request.
    pub killed: Vec<String>,
    /// Whether tasks were force-killed instead of gracefully interrupted.
    pub force: bool,
}

pub(crate) struct ExpQueueCleanResult {
    pub(crate) active_run_ids: Vec<ExperimentId>,
    pub(crate) removed_active_markers: usize,
    pub(crate) removed_kill_requests: usize,
    pub(crate) removed_logs: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct ActiveQueueRun {
    id: String,
    tmpdir: PathBuf,
    started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_started_at: Option<String>,
}

pub(crate) struct ActiveQueueRunGuard {
    path: PathBuf,
    kill_path: PathBuf,
}

impl Drop for ActiveQueueRunGuard {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %self.path.display(), error = %e, "failed to clear active queue run marker");
            }
        }
        match std::fs::remove_file(&self.kill_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %self.kill_path.display(), error = %e, "failed to clear queue kill request");
            }
        }
    }
}

/// Payload emitted by `exp stop`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpStopPayload {
    /// Whether a stop signal was successfully written.
    pub signaled: bool,
    /// Running queue tasks that also received a kill request.
    pub killed: Vec<String>,
}

// ─── Entry points ─────────────────────────────────────────────────────

/// `crab exp queue`.
pub fn exec_queue(args: QueueArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_queue(&args, &cwd)
}

/// `crab exp start`.
pub async fn exec_start(args: StartArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_start(&args, &cwd).await
}

/// `crab exp status`.
pub fn exec_status(args: StatusArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_status(&args, &cwd)
}

/// `crab queue remove`.
pub fn exec_queue_remove(args: QueueRemoveArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_queue_remove(&args, &cwd).map(|_| ())
}

/// `crab queue logs`.
pub fn exec_queue_logs(args: QueueLogsArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_queue_logs(&args, &cwd).map(|_| ())
}

/// `crab queue kill`.
pub fn exec_queue_kill(args: QueueKillArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_queue_kill(&args, &cwd).map(|_| ())
}

/// `crab exp stop`.
pub fn exec_stop(args: StopArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_stop(&args, &cwd)
}

// ─── Implementations ──────────────────────────────────────────────────

/// Testable `exp queue` implementation.
pub fn run_exp_queue(args: &QueueArgs, repo_root: &Path) -> Result<()> {
    let mode = args.output_mode();
    let payload = queue_experiments(
        repo_root,
        &args.set_param,
        None,
        args.message.clone(),
        Vec::new(),
        QueueTargetFlags::default(),
        args.copy_paths.clone(),
        false,
        "exp queue",
    )?;
    emit_queue(&payload, mode);
    Ok(())
}

pub(crate) fn queue_from_exp_run(
    repo_root: &Path,
    set_param: &[String],
    name: Option<String>,
    message: Option<String>,
    targets: Vec<String>,
    target_flags: QueueTargetFlags,
    copy_paths: Vec<PathBuf>,
    mode: OutputMode,
) -> Result<()> {
    let payload = queue_experiments(
        repo_root,
        set_param,
        name,
        message,
        targets,
        target_flags,
        copy_paths,
        true,
        "exp run --queue",
    )?;
    emit_queue(&payload, mode);
    Ok(())
}

fn queue_experiments(
    repo_root: &Path,
    set_param: &[String],
    name: Option<String>,
    message: Option<String>,
    targets: Vec<String>,
    target_flags: QueueTargetFlags,
    copy_paths: Vec<PathBuf>,
    allow_empty: bool,
    command: &str,
) -> Result<ExpQueuePayload> {
    if set_param.is_empty() && !allow_empty {
        return Err(CrabError::Configuration {
            key: "at least one --set-param is required".to_owned(),
            origin: command.to_owned(),
        });
    }

    // Parse each --set-param into (key, expanded_values).
    let mut param_expansions: Vec<(String, Vec<String>)> = Vec::new();
    for entry in set_param {
        let (key, values) = match entry.split_once('=') {
            Some((key, value_expr)) => (key, expand_param_value(value_expr)?),
            None if override_allows_missing_value(entry) => (entry.as_str(), vec![String::new()]),
            None => {
                return Err(CrabError::Configuration {
                    key: format!("--set-param entry missing '=': {entry}"),
                    origin: command.to_owned(),
                });
            }
        };
        if key.is_empty() {
            return Err(CrabError::Configuration {
                key: format!("--set-param entry has empty key: {entry}"),
                origin: command.to_owned(),
            });
        }
        param_expansions.push((key.to_owned(), values));
    }

    // Compute Cartesian product of all parameter values.
    let combinations = if param_expansions.is_empty() {
        vec![std::collections::BTreeMap::new()]
    } else {
        cartesian_product(&param_expansions)
    };

    // Snapshot current HEAD.
    let base_commit = resolve_head(repo_root)?;

    // Queue one experiment per combination.
    let queue_dir = queue_dir(repo_root);
    let queue = ExpQueue::new(queue_dir);
    let queued_at = crab_types::time::now_rfc3339_millis();

    let mut experiment_ids = Vec::with_capacity(combinations.len());
    for combo in &combinations {
        let id = ExpQueue::generate_id();
        let entry = ExpQueueEntry {
            id: id.clone(),
            queued_at: queued_at.clone(),
            base_commit: base_commit.clone(),
            name: queued_name(name.as_deref(), combinations.len(), experiment_ids.len()),
            message: message.clone(),
            param_overrides: combo.clone(),
            targets: targets.clone(),
            recursive: target_flags.recursive,
            single_item: target_flags.single_item,
            downstream: target_flags.downstream,
            force_downstream: target_flags.force_downstream,
            pipeline: target_flags.pipeline,
            all_pipelines: target_flags.all_pipelines,
            glob: target_flags.glob,
            copy_paths: copy_paths.clone(),
            status: ExpStatus::Pending,
        };
        queue.enqueue(&entry)?;
        experiment_ids.push(id);
    }

    info!(
        queued = experiment_ids.len(),
        base_commit = %base_commit,
        "experiments queued"
    );

    Ok(ExpQueuePayload {
        queued_count: experiment_ids.len(),
        experiment_ids,
        base_commit,
    })
}

fn queued_name(name: Option<&str>, total: usize, index: usize) -> Option<String> {
    name.map(|name| {
        if total <= 1 {
            name.to_owned()
        } else {
            format!("{}-{}", name, index + 1)
        }
    })
}

/// Testable `exp start` implementation.
pub async fn run_exp_start(args: &StartArgs, repo_root: &Path) -> Result<()> {
    let mode = args.output_mode();
    let jobs = args.jobs.max(1);

    // Warn if jobs exceeds available CPU cores.
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    if jobs > cpus {
        warn!(
            jobs = jobs,
            cpus = cpus,
            "--jobs exceeds available CPU cores; performance may degrade"
        );
    }

    let queue_dir = queue_dir(repo_root);
    let queue = ExpQueue::new(queue_dir.clone());
    let stop_path = queue_stop_path(repo_root);
    clear_stale_stop_signal(repo_root, &queue, &stop_path)?;

    // Read pending entries.
    let pending = queue.list_pending()?;
    if pending.is_empty() {
        info!("no pending experiments in queue");
        let payload = ExpStartPayload {
            processed: 0,
            succeeded: 0,
            failed: 0,
            succeeded_ids: Vec::new(),
            failed_ids: Vec::new(),
        };
        emit_start(&payload, mode);
        return Ok(());
    }

    info!(
        pending = pending.len(),
        jobs = jobs,
        "starting experiment workers"
    );

    // Set up stop signal (checked between experiments).
    let stop_flag = Arc::new(AtomicBool::new(false));

    let mut succeeded_ids = Vec::new();
    let mut failed_ids = Vec::new();

    // Process experiments with bounded concurrency using a semaphore.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(jobs as usize));
    let mut handles = Vec::new();

    for entry in pending {
        // Check stop signal before spawning.
        if stop_flag.load(Ordering::Relaxed) || stop_path.exists() {
            info!("stop signal detected, not starting new experiments");
            break;
        }

        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| CrabError::Internal(format!("semaphore acquire failed: {e}")))?;

        let repo = repo_root.to_path_buf();
        let q_dir = queue_dir.clone();
        let stop_p = stop_path.clone();
        let stop_f = stop_flag.clone();

        let handle = tokio::spawn(async move {
            let result = run_single_experiment(&repo, &q_dir, &entry, &stop_p, &stop_f).await;
            drop(permit);
            (entry.id.clone(), result)
        });
        handles.push(handle);
    }

    // Collect results.
    for handle in handles {
        match handle.await {
            Ok((id, Ok(()))) => succeeded_ids.push(id),
            Ok((id, Err(e))) => {
                warn!(exp_id = %id, error = %e, "experiment failed");
                failed_ids.push(id);
            }
            Err(e) => {
                warn!(error = %e, "experiment task panicked");
                failed_ids.push("<panicked>".to_owned());
            }
        }
    }

    // Clean up stop file if present.
    if stop_path.exists() {
        let _ = std::fs::remove_file(&stop_path);
    }

    let payload = ExpStartPayload {
        processed: succeeded_ids.len() + failed_ids.len(),
        succeeded: succeeded_ids.len(),
        failed: failed_ids.len(),
        succeeded_ids,
        failed_ids,
    };
    emit_start(&payload, mode);
    Ok(())
}

/// Run a single queued experiment through the same metadata-producing
/// path as `crab exp run`.
async fn run_single_experiment(
    repo_root: &Path,
    queue_dir: &Path,
    entry: &ExpQueueEntry,
    _stop_path: &Path,
    _stop_flag: &AtomicBool,
) -> Result<()> {
    let queue = ExpQueue::new(queue_dir.to_path_buf());

    // A kill request after this point is user intent for this run.
    // Clear stale files first so active marker setup cannot erase a
    // fresh `queue kill` that races with startup.
    let kill_path = queue_kill_path(repo_root, &entry.id);
    match std::fs::remove_file(&kill_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CrabError::Io(e)),
    }

    queue.update_status(&entry.id, ExpStatus::Running)?;

    let result = run_queued_experiment(repo_root, entry).await;

    // Update queue status based on result.
    match &result {
        Ok(()) => {
            queue.update_status(&entry.id, ExpStatus::Done)?;
            info!(exp_id = %entry.id, "experiment completed successfully");
        }
        Err(e) => {
            warn!(exp_id = %entry.id, error = %e, "experiment failed");
            queue.update_status(&entry.id, ExpStatus::Failed)?;
        }
    }

    result
}

async fn run_queued_experiment(repo_root: &Path, entry: &ExpQueueEntry) -> Result<()> {
    let exp_id: ExperimentId = entry.id.parse()?;
    crate::cmd::exp::run_exp_run_with_id(
        repo_root,
        exp_id,
        entry.param_overrides.clone(),
        crate::cmd::exp::ExpRunExecutionOptions::from_queue_entry(entry),
        Some(entry.base_commit.clone()),
        Some(entry.base_commit.as_str()),
        entry.name.clone(),
        vec![
            "crab".to_owned(),
            "exp".to_owned(),
            "start".to_owned(),
            entry.id.clone(),
        ],
    )
    .await
    .map(|_| ())
}

/// Testable `exp status` implementation.
pub fn run_exp_status(args: &StatusArgs, repo_root: &Path) -> Result<()> {
    let mode = args.output_mode();
    let queue_dir = queue_dir(repo_root);
    let queue = ExpQueue::new(queue_dir);

    let all = queue.list_all()?;

    let mut pending = 0usize;
    let mut running = 0usize;
    let mut done = 0usize;
    let mut failed = 0usize;
    for entry in &all {
        match entry.status {
            ExpStatus::Pending => pending += 1,
            ExpStatus::Running => {
                if active_queue_child_started(repo_root, &entry.id)? {
                    running += 1;
                } else {
                    pending += 1;
                }
            }
            ExpStatus::Done => done += 1,
            ExpStatus::Failed => failed += 1,
        }
    }

    let payload = ExpStatusPayload {
        pending,
        running,
        done,
        failed,
        total: all.len(),
    };
    emit_status(&payload, mode);
    Ok(())
}

/// Testable `queue remove` implementation. Removes queued or completed
/// task entries only; running entries are never deleted by this command.
pub fn run_exp_queue_remove(
    args: &QueueRemoveArgs,
    repo_root: &Path,
) -> Result<ExpQueueRemovePayload> {
    if args.ids.is_empty() && !args.all && !args.queued && !args.success && !args.failed {
        return Err(CrabError::Configuration {
            key: "queue remove".to_owned(),
            origin: "at least one task id or selector is required".to_owned(),
        });
    }

    let mode = args.output_mode();
    let queue_dir = queue_dir(repo_root);
    let queue = ExpQueue::new(queue_dir);
    let entries = queue.list_all()?;
    let mut selected = BTreeSet::new();
    let mut skipped_running = BTreeSet::new();

    for id in &args.ids {
        let entry = resolve_queue_entry(&entries, id)?;
        if entry.status == ExpStatus::Running {
            return Err(CrabError::Configuration {
                key: id.clone(),
                origin: "queue remove cannot remove running tasks; use queue kill or stop processing first"
                    .to_owned(),
            });
        }
        selected.insert(entry.id.clone());
    }

    for entry in &entries {
        let matches = args.all
            || (args.queued && entry.status == ExpStatus::Pending)
            || (args.success && entry.status == ExpStatus::Done)
            || (args.failed && entry.status == ExpStatus::Failed);
        if !matches {
            continue;
        }
        if entry.status == ExpStatus::Running {
            skipped_running.insert(entry.id.clone());
        } else {
            selected.insert(entry.id.clone());
        }
    }

    let removed = selected.into_iter().collect::<Vec<_>>();
    for id in &removed {
        queue.remove(id)?;
        remove_queue_log(repo_root, id)?;
    }

    let payload = ExpQueueRemovePayload {
        removed,
        skipped_running: skipped_running.into_iter().collect(),
    };
    emit_queue_remove(&payload, mode);
    Ok(payload)
}

/// Testable `queue logs` implementation.
pub fn run_exp_queue_logs(args: &QueueLogsArgs, repo_root: &Path) -> Result<ExpQueueLogsPayload> {
    validate_log_encoding(args.encoding.as_deref())?;

    let mode = args.output_mode();
    let queue = ExpQueue::new(queue_dir(repo_root));
    let entries = queue.list_all()?;
    let entry = resolve_queue_entry(&entries, &args.id)?;
    let id = entry.id.clone();

    let contents = if args.follow && entry.status == ExpStatus::Running && mode == OutputMode::Text
    {
        follow_queue_logs_text(repo_root, &id)?
    } else if args.follow && entry.status == ExpStatus::Running {
        wait_for_queue_log_completion(repo_root, &id)?
    } else {
        read_queue_log_contents(repo_root, &id, entry.status)?
    };

    let payload = ExpQueueLogsPayload {
        id,
        bytes: contents.len(),
        contents,
        followed: args.follow,
    };
    emit_queue_logs(&payload, mode);
    Ok(payload)
}

/// Testable `queue kill` implementation.
pub fn run_exp_queue_kill(args: &QueueKillArgs, repo_root: &Path) -> Result<ExpQueueKillPayload> {
    if args.ids.is_empty() && !args.force {
        return Err(CrabError::Configuration {
            key: "queue kill".to_owned(),
            origin: "pass one or more running task ids, or --force to kill all running tasks"
                .to_owned(),
        });
    }

    let mode = args.output_mode();
    let payload = kill_queue_tasks(repo_root, &args.ids, args.force)?;
    emit_queue_kill(&payload, mode);
    Ok(payload)
}

/// Testable `exp stop` implementation.
pub fn run_exp_stop(args: &StopArgs, repo_root: &Path) -> Result<()> {
    let mode = args.output_mode();

    // Write a stop signal file that running workers check.
    let stop_path = queue_stop_path(repo_root);
    if let Some(parent) = stop_path.parent() {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    std::fs::write(&stop_path, b"stop").map_err(CrabError::Io)?;

    let killed = if args.kill {
        kill_queue_tasks(repo_root, &[], true)?.killed
    } else {
        Vec::new()
    };

    info!(
        killed = killed.len(),
        "stop signal written; running experiments will finish and no new ones will start"
    );

    let payload = ExpStopPayload {
        signaled: true,
        killed,
    };
    emit_stop(&payload, mode);
    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────

pub(crate) fn queue_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".crab/exp-queue")
}

fn queue_stop_path(repo_root: &Path) -> PathBuf {
    queue_dir(repo_root).join(".stop")
}

fn queue_log_path(repo_root: &Path, id: &str) -> PathBuf {
    queue_dir(repo_root).join("logs").join(format!("{id}.log"))
}

fn clear_stale_stop_signal(repo_root: &Path, queue: &ExpQueue, stop_path: &Path) -> Result<()> {
    if !stop_path.exists() {
        return Ok(());
    }

    for entry in queue.list_all()? {
        if entry.status == ExpStatus::Running && active_queue_child_started(repo_root, &entry.id)? {
            return Ok(());
        }
    }

    match std::fs::remove_file(stop_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CrabError::Io(e)),
    }
}

pub(crate) fn queue_kill_path(repo_root: &Path, id: &str) -> PathBuf {
    queue_dir(repo_root).join("kill").join(format!("{id}.json"))
}

fn active_run_path(repo_root: &Path, id: &str) -> PathBuf {
    queue_dir(repo_root)
        .join("running")
        .join(format!("{id}.json"))
}

pub(crate) fn clean_exp_queue_housekeeping(repo_root: &Path) -> Result<ExpQueueCleanResult> {
    let queue = ExpQueue::new(queue_dir(repo_root));
    let entries = queue.list_all()?;
    let statuses: BTreeMap<String, ExpStatus> = entries
        .iter()
        .map(|entry| (entry.id.clone(), entry.status))
        .collect();
    let mut active_run_ids = BTreeSet::new();

    for entry in &entries {
        if entry.status == ExpStatus::Running
            && let Ok(id) = entry.id.parse::<ExperimentId>()
        {
            active_run_ids.insert(id);
        }
    }

    let removed_active_markers =
        clean_stale_active_run_markers(repo_root, &statuses, &mut active_run_ids)?;
    let removed_kill_requests = clean_stale_kill_requests(repo_root, &statuses)?;
    let removed_logs = clean_orphan_queue_logs(repo_root, &statuses)?;

    Ok(ExpQueueCleanResult {
        active_run_ids: active_run_ids.into_iter().collect(),
        removed_active_markers,
        removed_kill_requests,
        removed_logs,
    })
}

pub(crate) fn mark_queue_run_active(
    repo_root: &Path,
    id: &str,
    tmpdir: &Path,
    started_at: &str,
) -> Result<ActiveQueueRunGuard> {
    let path = active_run_path(repo_root, id);
    let kill_path = queue_kill_path(repo_root, id);
    let parent = path.parent().ok_or_else(|| {
        CrabError::Internal(format!(
            "active queue run path has no parent: {}",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let marker = ActiveQueueRun {
        id: id.to_owned(),
        tmpdir: tmpdir.to_path_buf(),
        started_at: started_at.to_owned(),
        child_pid: None,
        child_started_at: None,
    };
    let json = serde_json::to_vec_pretty(&marker).map_err(|e| {
        CrabError::Internal(format!("failed to serialize active queue run {id}: {e}"))
    })?;
    atomic_write(&path, &json)?;
    Ok(ActiveQueueRunGuard { path, kill_path })
}

pub(crate) fn mark_queue_child_started(repo_root: &Path, id: &str, pid: u32) -> Result<()> {
    let path = active_run_path(repo_root, id);
    let bytes = std::fs::read(&path).map_err(CrabError::Io)?;
    let mut marker = read_active_queue_run_bytes(id, &bytes)?;
    if marker.id != id {
        return Err(CrabError::Internal(format!(
            "active queue marker id mismatch for {id}: {}",
            marker.id
        )));
    }
    marker.child_pid = Some(pid);
    marker.child_started_at = Some(crab_types::time::now_rfc3339_millis());
    let json = serde_json::to_vec_pretty(&marker).map_err(|e| {
        CrabError::Internal(format!("failed to serialize active queue run {id}: {e}"))
    })?;
    atomic_write(&path, &json)
}

pub(crate) fn persist_queue_logs(
    repo_root: &Path,
    id: &str,
    tmpdir: &Path,
    status: &str,
) -> Result<()> {
    let contents = collect_queue_log_contents(tmpdir)?;
    let path = queue_log_path(repo_root, id);
    let parent = path.parent().ok_or_else(|| {
        CrabError::Internal(format!("queue log path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    atomic_write(&path, contents.as_bytes())?;
    info!(
        exp_id = %id,
        status = status,
        bytes = contents.len(),
        "persisted queue task logs"
    );
    Ok(())
}

pub(crate) fn remove_queue_log(repo_root: &Path, id: &str) -> Result<()> {
    let path = queue_log_path(repo_root, id);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CrabError::Io(e)),
    }
}

fn kill_queue_tasks(repo_root: &Path, ids: &[String], force: bool) -> Result<ExpQueueKillPayload> {
    let queue = ExpQueue::new(queue_dir(repo_root));
    let entries = queue.list_all()?;
    let mut selected = BTreeSet::new();

    if ids.is_empty() {
        for entry in &entries {
            if entry.status == ExpStatus::Running {
                selected.insert(entry.id.clone());
            }
        }
    } else {
        for id in ids {
            let entry = resolve_queue_entry(&entries, id)?;
            if entry.status != ExpStatus::Running {
                return Err(CrabError::Configuration {
                    key: id.clone(),
                    origin: "queue kill can only interrupt running tasks".to_owned(),
                });
            }
            selected.insert(entry.id.clone());
        }
    }

    let killed = selected.into_iter().collect::<Vec<_>>();
    for id in &killed {
        write_queue_kill_request(repo_root, id, force)?;
    }

    Ok(ExpQueueKillPayload { killed, force })
}

fn write_queue_kill_request(repo_root: &Path, id: &str, force: bool) -> Result<()> {
    let path = queue_kill_path(repo_root, id);
    let request = if force { "force\n" } else { "graceful\n" };
    atomic_write(&path, request.as_bytes())
}

fn validate_log_encoding(encoding: Option<&str>) -> Result<()> {
    let Some(encoding) = encoding else {
        return Ok(());
    };
    let normalized = encoding.replace(['_', '-'], "").to_ascii_lowercase();
    if normalized == "utf8" {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: encoding.to_owned(),
        origin: "queue logs supports UTF-8 task logs".to_owned(),
    })
}

fn read_queue_log_contents(repo_root: &Path, id: &str, status: ExpStatus) -> Result<String> {
    if status == ExpStatus::Pending {
        return Err(CrabError::Configuration {
            key: id.to_owned(),
            origin: "queue task has not started, so no logs are available".to_owned(),
        });
    }

    if status == ExpStatus::Running
        && let Some(active) = read_active_queue_run(repo_root, id)?
        && active.tmpdir.exists()
    {
        return collect_queue_log_contents(&active.tmpdir);
    }

    read_persisted_queue_log(repo_root, id)
}

fn wait_for_queue_log_completion(repo_root: &Path, id: &str) -> Result<String> {
    loop {
        let queue = ExpQueue::new(queue_dir(repo_root));
        let entries = queue.list_all()?;
        let entry = resolve_queue_entry(&entries, id)?;
        if entry.status != ExpStatus::Running {
            return read_queue_log_contents(repo_root, id, entry.status);
        }
        std::thread::sleep(QUEUE_LOG_POLL_INTERVAL);
    }
}

fn follow_queue_logs_text(repo_root: &Path, id: &str) -> Result<String> {
    let mut offsets = BTreeMap::<PathBuf, usize>::new();
    let mut rendered = String::new();

    loop {
        if let Some(active) = read_active_queue_run(repo_root, id)?
            && active.tmpdir.exists()
        {
            emit_log_deltas(&active.tmpdir, &mut offsets, &mut rendered)?;
        }

        let queue = ExpQueue::new(queue_dir(repo_root));
        let entries = queue.list_all()?;
        let entry = resolve_queue_entry(&entries, id)?;
        if entry.status != ExpStatus::Running {
            let final_contents = read_queue_log_contents(repo_root, id, entry.status)?;
            print_log_suffix(&rendered, &final_contents);
            if final_contents.starts_with(&rendered) {
                return Ok(final_contents);
            }
            return Ok(rendered);
        }

        std::thread::sleep(QUEUE_LOG_POLL_INTERVAL);
    }
}

fn emit_log_deltas(
    tmpdir: &Path,
    offsets: &mut BTreeMap<PathBuf, usize>,
    rendered: &mut String,
) -> Result<()> {
    for path in queue_stage_log_paths(tmpdir)? {
        let bytes = std::fs::read(&path).map_err(CrabError::Io)?;
        let start = offsets.get(&path).copied().unwrap_or(0).min(bytes.len());
        offsets.insert(path, bytes.len());
        if start >= bytes.len() {
            continue;
        }
        let delta = String::from_utf8_lossy(&bytes[start..]).into_owned();
        print!("{delta}");
        std::io::stdout().flush().map_err(CrabError::Io)?;
        rendered.push_str(&delta);
    }
    Ok(())
}

fn print_log_suffix(rendered: &str, final_contents: &str) {
    if final_contents.starts_with(rendered) && final_contents.len() > rendered.len() {
        print!("{}", &final_contents[rendered.len()..]);
        let _ = std::io::stdout().flush();
    }
}

fn read_active_queue_run(repo_root: &Path, id: &str) -> Result<Option<ActiveQueueRun>> {
    let path = active_run_path(repo_root, id);
    match std::fs::read(&path) {
        Ok(bytes) => read_active_queue_run_bytes(id, &bytes).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CrabError::Io(e)),
    }
}

fn active_queue_child_started(repo_root: &Path, id: &str) -> Result<bool> {
    Ok(read_active_queue_run(repo_root, id)?
        .is_some_and(|active| active.child_pid.is_some() && active.tmpdir.exists()))
}

fn clean_stale_active_run_markers(
    repo_root: &Path,
    statuses: &BTreeMap<String, ExpStatus>,
    active_run_ids: &mut BTreeSet<ExperimentId>,
) -> Result<usize> {
    let dir = queue_dir(repo_root).join("running");
    let iter = match std::fs::read_dir(&dir) {
        Ok(iter) => iter,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(CrabError::Io(e)),
    };

    let mut removed = 0usize;
    for entry in iter {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        if !entry.file_type().map_err(CrabError::Io)?.is_file()
            || path.extension().and_then(|ext| ext.to_str()) != Some("json")
        {
            continue;
        }

        let Some(file_id) = queue_id_from_path(&path) else {
            continue;
        };
        let bytes = std::fs::read(&path).map_err(CrabError::Io)?;
        let active = match read_active_queue_run_bytes(&file_id, &bytes) {
            Ok(active) => active,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "removing malformed active queue marker");
                removed += remove_queue_housekeeping_file(&path)? as usize;
                continue;
            }
        };

        let is_current =
            file_id == active.id && statuses.get(&active.id) == Some(&ExpStatus::Running);
        if is_current && active.tmpdir.exists() {
            if let Ok(id) = active.id.parse::<ExperimentId>() {
                active_run_ids.insert(id);
            }
            continue;
        }

        removed += remove_queue_housekeeping_file(&path)? as usize;
    }

    Ok(removed)
}

fn clean_stale_kill_requests(
    repo_root: &Path,
    statuses: &BTreeMap<String, ExpStatus>,
) -> Result<usize> {
    let dir = queue_dir(repo_root).join("kill");
    clean_stale_queue_files(&dir, "json", |id| {
        statuses.get(id) == Some(&ExpStatus::Running)
    })
}

fn clean_orphan_queue_logs(
    repo_root: &Path,
    statuses: &BTreeMap<String, ExpStatus>,
) -> Result<usize> {
    let dir = queue_dir(repo_root).join("logs");
    clean_stale_queue_files(&dir, "log", |id| statuses.contains_key(id))
}

fn clean_stale_queue_files(
    dir: &Path,
    extension: &str,
    should_keep: impl Fn(&str) -> bool,
) -> Result<usize> {
    let iter = match std::fs::read_dir(dir) {
        Ok(iter) => iter,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(CrabError::Io(e)),
    };

    let mut removed = 0usize;
    for entry in iter {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        if !entry.file_type().map_err(CrabError::Io)?.is_file()
            || path.extension().and_then(|ext| ext.to_str()) != Some(extension)
        {
            continue;
        }

        let Some(id) = queue_id_from_path(&path) else {
            continue;
        };
        if should_keep(&id) {
            continue;
        }

        removed += remove_queue_housekeeping_file(&path)? as usize;
    }

    Ok(removed)
}

fn queue_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(std::borrow::ToOwned::to_owned)
}

fn remove_queue_housekeeping_file(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(CrabError::Io(e)),
    }
}

fn read_active_queue_run_bytes(id: &str, bytes: &[u8]) -> Result<ActiveQueueRun> {
    serde_json::from_slice(bytes)
        .map_err(|e| CrabError::Internal(format!("malformed active queue run {id}: {e}")))
}

fn read_persisted_queue_log(repo_root: &Path, id: &str) -> Result<String> {
    let path = queue_log_path(repo_root, id);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CrabError::NotFound {
            path: path.display().to_string(),
        }),
        Err(e) => Err(CrabError::Io(e)),
    }
}

fn collect_queue_log_contents(tmpdir: &Path) -> Result<String> {
    let mut contents = String::new();
    for path in queue_stage_log_paths(tmpdir)? {
        let bytes = std::fs::read(path).map_err(CrabError::Io)?;
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(&String::from_utf8_lossy(&bytes));
    }
    Ok(contents)
}

fn queue_stage_log_paths(tmpdir: &Path) -> Result<Vec<PathBuf>> {
    let runs_dir = tmpdir.join(".crab/workflow/runs");
    if !runs_dir.exists() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for run_dir in std::fs::read_dir(runs_dir).map_err(CrabError::Io)? {
        let run_dir = run_dir.map_err(CrabError::Io)?;
        let run_path = run_dir.path();
        if !run_path.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&run_path).map_err(CrabError::Io)? {
            let entry = entry.map_err(CrabError::Io)?;
            let path = entry.path();
            let is_stage_log = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("stage-"))
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("log"));
            if path.is_file() && is_stage_log {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        CrabError::Internal(format!("queue path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let tmp = tempfile::NamedTempFile::new_in(parent).map_err(CrabError::Io)?;
    std::fs::write(tmp.path(), bytes).map_err(CrabError::Io)?;
    tmp.persist(path).map_err(|e| {
        CrabError::Internal(format!(
            "failed to persist queue file to {}: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

/// Resolve HEAD to a commit hash.
fn resolve_head(repo_root: &Path) -> Result<String> {
    let output = StdCommand::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git rev-parse: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "git rev-parse HEAD failed: {}",
            stderr.trim()
        )));
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(sha)
}

fn resolve_queue_entry<'a>(
    entries: &'a [ExpQueueEntry],
    selector: &str,
) -> Result<&'a ExpQueueEntry> {
    let mut matches = entries
        .iter()
        .filter(|entry| entry.id == selector || entry.id.starts_with(selector));
    let Some(first) = matches.next() else {
        return Err(CrabError::Configuration {
            key: selector.to_owned(),
            origin: "queue task not found".to_owned(),
        });
    };
    if matches.next().is_some() {
        return Err(CrabError::Configuration {
            key: selector.to_owned(),
            origin: "queue task prefix is ambiguous".to_owned(),
        });
    }
    Ok(first)
}

// ─── Output rendering ─────────────────────────────────────────────────

fn emit_queue(payload: &ExpQueuePayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_QUEUE_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!(
                "Queued {} experiment(s) from commit {}",
                payload.queued_count,
                &payload.base_commit[..12.min(payload.base_commit.len())]
            );
            for id in &payload.experiment_ids {
                println!("  {id}");
            }
        }
    }
}

fn emit_start(payload: &ExpStartPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_START_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!(
                "Processed {} experiment(s): {} succeeded, {} failed",
                payload.processed, payload.succeeded, payload.failed
            );
            if !payload.succeeded_ids.is_empty() {
                println!("Succeeded:");
                for id in &payload.succeeded_ids {
                    println!("  {id}");
                }
            }
            if !payload.failed_ids.is_empty() {
                println!("Failed:");
                for id in &payload.failed_ids {
                    println!("  {id}");
                }
            }
        }
    }
}

fn emit_status(payload: &ExpStatusPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_STATUS_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!("Experiment queue ({} total):", payload.total);
            println!("  Pending:  {}", payload.pending);
            println!("  Running:  {}", payload.running);
            println!("  Done:     {}", payload.done);
            println!("  Failed:   {}", payload.failed);
        }
    }
}

fn emit_queue_remove(payload: &ExpQueueRemovePayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_QUEUE_REMOVE_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!("Removed {} queue task(s).", payload.removed.len());
            for id in &payload.removed {
                println!("  {id}");
            }
            if !payload.skipped_running.is_empty() {
                println!("Skipped running task(s):");
                for id in &payload.skipped_running {
                    println!("  {id}");
                }
            }
        }
    }
}

fn emit_queue_logs(payload: &ExpQueueLogsPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_QUEUE_LOGS_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            print!("{}", payload.contents);
            let _ = std::io::stdout().flush();
        }
    }
}

fn emit_queue_kill(payload: &ExpQueueKillPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_QUEUE_KILL_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            let verb = if payload.force { "Killed" } else { "Signaled" };
            println!("{} {} running queue task(s).", verb, payload.killed.len());
            for id in &payload.killed {
                println!("  {id}");
            }
        }
    }
}

fn emit_stop(payload: &ExpStopPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_STOP_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            if payload.signaled {
                if payload.killed.is_empty() {
                    println!("Stop signal sent. Running experiments will finish gracefully.");
                } else {
                    println!(
                        "Stop signal sent. Killed {} running task(s).",
                        payload.killed.len()
                    );
                    for id in &payload.killed {
                        println!("  {id}");
                    }
                }
            } else {
                println!("Failed to send stop signal.");
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn queue_entry(id: &str, status: ExpStatus) -> ExpQueueEntry {
        ExpQueueEntry {
            id: id.to_owned(),
            queued_at: "2026-05-05T12:00:00Z".to_owned(),
            base_commit: "abc123".to_owned(),
            name: None,
            message: None,
            param_overrides: std::collections::BTreeMap::new(),
            targets: Vec::new(),
            recursive: false,
            single_item: false,
            downstream: false,
            force_downstream: false,
            pipeline: false,
            all_pipelines: false,
            glob: false,
            copy_paths: Vec::new(),
            status,
        }
    }

    fn init_repo(root: &Path) {
        run_git(root, &["init", "--initial-branch=main"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("README.md"), "test\n").unwrap();
        run_git(root, &["add", "README.md"]);
        run_git(root, &["commit", "-m", "initial"]);
    }

    fn init_workflow_repo(root: &Path) {
        run_git(root, &["init", "--initial-branch=main"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);

        std::fs::create_dir_all(root.join(".crab")).unwrap();
        std::fs::write(
            root.join(".crab/config.toml"),
            "[workflow]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(root.join("params.yaml"), "model:\n  lr: 0.001\n").unwrap();
        std::fs::write(
            root.join("crab.yaml"),
            concat!(
                "params:\n",
                "  - params.yaml\n",
                "stages:\n",
                "  copy:\n",
                "    cmd: \"cp params.yaml out.txt\"\n",
                "    deps:\n",
                "      - params.yaml\n",
                "    outs:\n",
                "      - out.txt\n",
                "    params:\n",
                "      - model.lr\n",
            ),
        )
        .unwrap();
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-m", "initial workflow"]);
    }

    fn init_copy_paths_workflow_repo(root: &Path) {
        run_git(root, &["init", "--initial-branch=main"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);

        std::fs::create_dir_all(root.join(".crab")).unwrap();
        std::fs::write(
            root.join(".crab/config.toml"),
            "[workflow]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crab.yaml"),
            concat!(
                "stages:\n",
                "  use_secret:\n",
                "    cmd: \"cp secret.txt out.txt\"\n",
                "    deps:\n",
                "      - secret.txt\n",
                "    outs:\n",
                "      - out.txt\n",
            ),
        )
        .unwrap();
        run_git(root, &["add", ".crab/config.toml", "crab.yaml"]);
        run_git(root, &["commit", "-m", "initial workflow"]);
        std::fs::write(root.join("secret.txt"), "queued\n").unwrap();
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn queue_accepts_remove_override_without_equals() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let args = QueueArgs {
            set_param: vec!["~model.dropout".to_owned()],
            copy_paths: Vec::new(),
            message: None,
            json: true,
        };

        run_exp_queue(&args, root).unwrap();

        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        let entries = queue.list_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]
                .param_overrides
                .get("~model.dropout")
                .map(String::as_str),
            Some("")
        );
    }

    #[test]
    fn queue_args_accept_set_short_and_alias() {
        let args = QueueArgs::try_parse_from([
            "queue",
            "-S",
            "model.lr=0.01",
            "--set",
            "model.depth=4",
            "-C",
            "secrets.env",
            "-m",
            "queued message",
        ])
        .unwrap();

        assert_eq!(args.set_param, vec!["model.lr=0.01", "model.depth=4"]);
        assert_eq!(args.copy_paths, vec![PathBuf::from("secrets.env")]);
        assert_eq!(args.message.as_deref(), Some("queued message"));
    }

    #[test]
    fn queue_from_exp_run_allows_empty_overrides_and_persists_name() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let payload = queue_experiments(
            root,
            &[],
            Some("manual-snapshot".to_owned()),
            Some("manual message".to_owned()),
            vec!["train".to_owned()],
            QueueTargetFlags {
                recursive: true,
                single_item: true,
                force_downstream: true,
                ..QueueTargetFlags::default()
            },
            vec![PathBuf::from("secrets.env")],
            true,
            "exp run --queue",
        )
        .unwrap();
        assert_eq!(payload.queued_count, 1);

        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        let entries = queue.list_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name.as_deref(), Some("manual-snapshot"));
        assert_eq!(entries[0].message.as_deref(), Some("manual message"));
        assert!(entries[0].param_overrides.is_empty());
        assert_eq!(entries[0].targets, vec!["train".to_owned()]);
        assert!(entries[0].recursive);
        assert!(entries[0].single_item);
        assert!(entries[0].force_downstream);
        assert_eq!(entries[0].copy_paths, vec![PathBuf::from("secrets.env")]);
    }

    #[test]
    fn queue_from_exp_run_suffixes_names_for_sweeps() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let payload = queue_experiments(
            root,
            &["model.lr=0.001,0.002".to_owned()],
            Some("sweep".to_owned()),
            None,
            Vec::new(),
            QueueTargetFlags::default(),
            Vec::new(),
            true,
            "exp run --queue",
        )
        .unwrap();
        assert_eq!(payload.queued_count, 2);

        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        let mut names = queue
            .list_all()
            .unwrap()
            .into_iter()
            .map(|entry| entry.name.unwrap())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["sweep-1", "sweep-2"]);
    }

    #[test]
    fn queue_expands_hydra_choice_and_stop_exclusive_range() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let payload = queue_experiments(
            root,
            &[
                "model.arch=choice(resnet,efficientnet)".to_owned(),
                "model.lr=range(1,4)".to_owned(),
            ],
            None,
            None,
            Vec::new(),
            QueueTargetFlags::default(),
            Vec::new(),
            true,
            "exp run --queue",
        )
        .unwrap();
        assert_eq!(payload.queued_count, 6);

        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        let entries = queue.list_all().unwrap();
        assert_eq!(entries.len(), 6);
        assert_eq!(entries[0].param_overrides["model.arch"], "resnet");
        assert_eq!(entries[0].param_overrides["model.lr"], "1");
        assert_eq!(entries[5].param_overrides["model.arch"], "efficientnet");
        assert_eq!(entries[5].param_overrides["model.lr"], "3");
    }

    #[test]
    fn queue_remove_all_keeps_running_tasks() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        queue
            .enqueue(&queue_entry("running-task", ExpStatus::Running))
            .unwrap();
        queue
            .enqueue(&queue_entry("done-task", ExpStatus::Done))
            .unwrap();
        queue
            .enqueue(&queue_entry("failed-task", ExpStatus::Failed))
            .unwrap();
        queue
            .enqueue(&queue_entry("pending-task", ExpStatus::Pending))
            .unwrap();
        std::fs::create_dir_all(root.join(".crab/exp-queue/logs")).unwrap();
        std::fs::write(queue_log_path(root, "running-task"), "running\n").unwrap();
        std::fs::write(queue_log_path(root, "done-task"), "done\n").unwrap();
        std::fs::write(queue_log_path(root, "failed-task"), "failed\n").unwrap();
        std::fs::write(queue_log_path(root, "pending-task"), "pending\n").unwrap();

        let payload = run_exp_queue_remove(
            &QueueRemoveArgs {
                ids: Vec::new(),
                all: true,
                queued: false,
                success: false,
                failed: false,
                json: false,
            },
            root,
        )
        .unwrap();

        assert_eq!(
            payload.removed,
            vec!["done-task", "failed-task", "pending-task"]
        );
        assert_eq!(payload.skipped_running, vec!["running-task"]);

        let remaining = queue.list_all().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "running-task");
        assert!(queue_log_path(root, "running-task").exists());
        assert!(!queue_log_path(root, "done-task").exists());
        assert!(!queue_log_path(root, "failed-task").exists());
        assert!(!queue_log_path(root, "pending-task").exists());
    }

    #[test]
    fn queue_remove_rejects_running_task_by_id() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        queue
            .enqueue(&queue_entry("running-task", ExpStatus::Running))
            .unwrap();

        let err = run_exp_queue_remove(
            &QueueRemoveArgs {
                ids: vec!["running-task".to_owned()],
                all: false,
                queued: false,
                success: false,
                failed: false,
                json: false,
            },
            root,
        )
        .unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn queue_logs_read_persisted_stage_output() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let worktree = TempDir::new().unwrap();
        let run_dir = worktree.path().join(".crab/workflow/runs/run-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("stage-copy.log"), "copy output\n").unwrap();
        std::fs::write(run_dir.join("stage-train.log"), "train output\n").unwrap();

        persist_queue_logs(root, "done-task", worktree.path(), "success").unwrap();
        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        queue
            .enqueue(&queue_entry("done-task", ExpStatus::Done))
            .unwrap();

        let payload = run_exp_queue_logs(
            &QueueLogsArgs {
                id: "done".to_owned(),
                encoding: Some("utf-8".to_owned()),
                follow: false,
                json: true,
            },
            root,
        )
        .unwrap();

        assert_eq!(payload.id, "done-task");
        assert!(payload.contents.contains("copy output"));
        assert!(payload.contents.contains("train output"));
    }

    #[test]
    fn queue_kill_force_writes_requests_for_all_running_tasks() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        queue
            .enqueue(&queue_entry("running-a", ExpStatus::Running))
            .unwrap();
        queue
            .enqueue(&queue_entry("running-b", ExpStatus::Running))
            .unwrap();
        queue
            .enqueue(&queue_entry("pending-task", ExpStatus::Pending))
            .unwrap();

        let payload = run_exp_queue_kill(
            &QueueKillArgs {
                ids: Vec::new(),
                force: true,
                json: false,
            },
            root,
        )
        .unwrap();

        assert_eq!(payload.killed, vec!["running-a", "running-b"]);
        assert_eq!(
            std::fs::read_to_string(queue_kill_path(root, "running-a")).unwrap(),
            "force\n"
        );
        assert!(!queue_kill_path(root, "pending-task").exists());
    }

    #[test]
    fn queue_kill_target_writes_graceful_request() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        queue
            .enqueue(&queue_entry("running-task", ExpStatus::Running))
            .unwrap();

        let payload = run_exp_queue_kill(
            &QueueKillArgs {
                ids: vec!["running-task".to_owned()],
                force: false,
                json: false,
            },
            root,
        )
        .unwrap();

        assert_eq!(payload.killed, vec!["running-task"]);
        assert_eq!(
            std::fs::read_to_string(queue_kill_path(root, "running-task")).unwrap(),
            "graceful\n"
        );
    }

    #[test]
    fn queue_stop_kill_writes_stop_and_kill_requests() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        queue
            .enqueue(&queue_entry("running-task", ExpStatus::Running))
            .unwrap();

        run_exp_stop(
            &StopArgs {
                kill: true,
                json: false,
            },
            root,
        )
        .unwrap();

        assert!(root.join(".crab/exp-queue/.stop").exists());
        assert_eq!(
            std::fs::read_to_string(queue_kill_path(root, "running-task")).unwrap(),
            "force\n"
        );
    }

    #[test]
    fn start_clears_stop_signal_without_active_worker() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        queue
            .enqueue(&queue_entry("pending-task", ExpStatus::Pending))
            .unwrap();
        std::fs::write(queue_stop_path(root), b"stop").unwrap();

        clear_stale_stop_signal(root, &queue, &queue_stop_path(root)).unwrap();

        assert!(!queue_stop_path(root).exists());
    }

    #[test]
    fn start_preserves_stop_signal_for_active_worker() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let worktree = TempDir::new().unwrap();
        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        queue
            .enqueue(&queue_entry("running-task", ExpStatus::Running))
            .unwrap();
        std::fs::write(queue_stop_path(root), b"stop").unwrap();
        let _guard = mark_queue_run_active(
            root,
            "running-task",
            worktree.path(),
            "2026-05-05T12:00:00Z",
        )
        .unwrap();
        mark_queue_child_started(root, "running-task", 42).unwrap();

        clear_stale_stop_signal(root, &queue, &queue_stop_path(root)).unwrap();

        assert!(queue_stop_path(root).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_writes_metadata_for_queued_experiment_id_at_queued_commit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_workflow_repo(root);

        let base_commit = resolve_head(root).unwrap();
        run_exp_queue(
            &QueueArgs {
                set_param: vec!["model.lr=0.002".to_owned()],
                copy_paths: Vec::new(),
                message: None,
                json: true,
            },
            root,
        )
        .unwrap();
        std::fs::write(root.join("params.yaml"), "model:\n  lr: 0.9\n").unwrap();
        run_git(root, &["add", "params.yaml"]);
        run_git(root, &["commit", "-m", "move head after queue"]);

        run_exp_start(
            &StartArgs {
                jobs: 1,
                json: true,
            },
            root,
        )
        .await
        .unwrap();

        let queue = ExpQueue::new(root.join(".crab/exp-queue"));
        let entries = queue.list_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, ExpStatus::Done);

        let meta_path = root
            .join(".crab/workflow/exp")
            .join(format!("{}.meta.json", entries[0].id));
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        assert_eq!(meta["exp_id"], entries[0].id);
        assert_eq!(meta["base_commit"], base_commit);
        assert_eq!(meta["queue_commit"], base_commit);
        assert_eq!(meta["param_overrides"]["model.lr"], "0.002");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_replays_copy_paths_for_queued_experiment() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_copy_paths_workflow_repo(root);

        queue_from_exp_run(
            root,
            &[],
            None,
            None,
            Vec::new(),
            QueueTargetFlags::default(),
            vec![PathBuf::from("secret.txt")],
            OutputMode::Text,
        )
        .unwrap();

        run_exp_start(
            &StartArgs {
                jobs: 1,
                json: false,
            },
            root,
        )
        .await
        .unwrap();

        let entries = ExpQueue::new(root.join(".crab/exp-queue"))
            .list_all()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, ExpStatus::Done);
    }
}
