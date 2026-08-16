//! CLI entry point for `crab optimize xorbs` — xorb-level optimization
//! with named profiles, dry-run estimation, and crash-safe execution.
//!
//! This is **not** `cmd::repack` (git-pack consolidation). The existing
//! `crab repack` command is untouched.
//!
//! Flags:
//! - `--profile <name>` — named profile (`ml`, `dataset`, `code`, or custom).
//! - `--dry-run` — estimate without writing.
//! - `--apply` — execute xorb optimization.
//! - `--abort` — flag the journal; next run refuses to resume.
//! - `--resume` — resume a previously interrupted run.
//! - `--drop-journal` — delete the journal (requires `--yes-really`).
//! - `--include-cold` — include archive-class source xorbs (default: true).
//! - `--restore-tier` — restore tier for archive sources.
//! - `--output-class` — storage class for destination xorbs.
//! - `--json` / `--jsonl` — structured output.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::info;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::restripe::executor::{self, ExecutorConfig};
use crate::restripe::inference::{self, RepoStats};
use crate::restripe::journal::RestripeJournal;
use crate::restripe::planner::{self, CalibrationConfig, SourceXorbMeta};
use crate::restripe::profile::Profile;
use crate::restripe::reconcile;
use crate::tier::audit_shim::{self, AuditOp};

const OPTIMIZE_XORBS_AUTH_OPERATION: &str = "optimize-xorbs";
const OPTIMIZE_XORBS_OPERATION: &str = "optimize xorbs";
const OPTIMIZE_XORBS_PLAN_SCHEMA: &str = "optimize.xorbs.plan";
const OPTIMIZE_XORBS_EVENT_SCHEMA: &str = "optimize.xorbs.event";

/// Read the remote URL from `.crab/remote` and build a Store.
///
/// Returns `None` if the remote file is missing or the URL is invalid,
/// allowing the executor to operate in journal-only mode for testing.
async fn try_build_store(
    cfg: &Config,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<(crate::storage::store::Store, crate::git::url::CrabUrl)> {
    let remote_path = crate::git::discover::resolve_crab_dir()
        .map_or_else(|| PathBuf::from(".crab/remote"), |d| d.join("remote"));
    let url = std::fs::read_to_string(&remote_path).ok()?;
    let url = url.trim();
    let parsed = crate::git::url::CrabUrl::parse(url).ok()?;
    let store = crate::auth::build_store(cfg, &parsed, OPTIMIZE_XORBS_AUTH_OPERATION, cancel)
        .await
        .ok()?;
    Some((store, parsed))
}

// ---------------------------------------------------------------------------
// CLI arguments (used by main.rs)
// ---------------------------------------------------------------------------

/// Arguments for `crab optimize xorbs`.
#[derive(Debug, clap::Parser)]
pub struct RestripeArgs {
    /// Named xorb optimization profile: `ml`, `dataset`, `code`, or a custom name.
    /// When omitted, the profile is auto-inferred from repository statistics.
    #[arg(long)]
    pub profile: Option<String>,

    /// Estimate the xorb optimization without writing any data.
    #[arg(long)]
    pub dry_run: bool,

    /// Execute the xorb optimization operation.
    #[arg(long)]
    pub apply: bool,

    /// Flag the current run as aborted. The next invocation will refuse
    /// to resume and prompt for `--resume` or `--drop-journal`.
    #[arg(long)]
    pub abort: bool,

    /// Resume a previously interrupted xorb optimization run.
    #[arg(long)]
    pub resume: bool,

    /// Delete the xorb optimization journal. Requires `--yes-really`.
    #[arg(long)]
    pub drop_journal: bool,

    /// Confirm destructive operations (`--drop-journal`).
    #[arg(long)]
    pub yes_really: bool,

    /// Include archive-class source xorbs in the optimization.
    #[arg(long, default_value = "true")]
    pub include_cold: bool,

    /// Restore tier for archive sources: `expedited`, `standard`, `bulk`.
    #[arg(long)]
    pub restore_tier: Option<String>,

    /// Storage class for destination xorbs (e.g. `STANDARD`, `STANDARD_IA`).
    #[arg(long)]
    pub output_class: Option<String>,

    /// Structured JSON output (single envelope with terminal result).
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,

    /// Streaming JSONL output (one event per line).
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

// ---------------------------------------------------------------------------
// Structured output payloads
// ---------------------------------------------------------------------------

/// Final summary payload for `--json` / `--jsonl` output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RestripeSummary {
    /// Run identifier.
    pub run_id: String,
    /// Profile used.
    pub profile: String,
    /// Counts by status.
    pub counts: RestripeCounts,
    /// Total bytes read.
    pub bytes_read: u64,
    /// Total bytes written.
    pub bytes_written: u64,
    /// Wall-clock duration in milliseconds.
    pub elapsed_ms: u64,
    /// List of corrupt source xorb hashes.
    pub corrupt_list: Vec<String>,
}

/// Per-status counts in the summary.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RestripeCounts {
    pub done: u64,
    pub corrupt: u64,
    pub skipped: u64,
    pub pending: u64,
}

/// Control event emitted by xorb optimization management operations.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OptimizeXorbsControlEvent {
    pub event: OptimizeXorbsControlEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Control event kind for xorb optimization management operations.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OptimizeXorbsControlEventKind {
    JournalDropped,
    Aborted,
}

/// Payload variants emitted under the `optimize.xorbs.event` schema.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum OptimizeXorbsEventPayload {
    Plan(planner::RestripeEstimate),
    Summary(RestripeSummary),
    Control(OptimizeXorbsControlEvent),
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the `crab optimize xorbs` command.
pub async fn run_restripe(args: &RestripeArgs, cfg: &Config) -> Result<()> {
    let output_mode = OutputMode::from_flags(args.json, args.jsonl);

    // Journal path: `.crab/restripe/journal.db` relative to the repo root.
    let journal_path = PathBuf::from(".crab/restripe/journal.db");

    // Handle --drop-journal.
    if args.drop_journal {
        if !args.yes_really {
            return Err(CrabError::Configuration {
                key: "--drop-journal".to_string(),
                origin: "--drop-journal requires --yes-really for safety".to_string(),
            });
        }
        if RestripeJournal::exists(&journal_path) {
            RestripeJournal::drop_journal(&journal_path)?;
            info!("xorb optimization journal dropped");
            if output_mode == OutputMode::Json {
                emit_json(
                    OPTIMIZE_XORBS_EVENT_SCHEMA,
                    "1.0",
                    OptimizeXorbsControlEvent {
                        event: OptimizeXorbsControlEventKind::JournalDropped,
                        run_id: None,
                    },
                );
            }
        } else {
            info!("no xorb optimization journal found");
        }
        return Ok(());
    }

    // Resolve profile.
    let (profile_name, profile) = resolve_profile(args, cfg)?;
    info!(profile = %profile_name, "resolved xorb optimization profile");

    // Handle --dry-run.
    if args.dry_run {
        run_dry_run(&profile_name, &profile, output_mode, cfg);
        return Ok(());
    }

    // Handle --abort.
    if args.abort {
        return run_abort(&journal_path, output_mode);
    }

    // Handle --apply or --resume.
    if args.apply || args.resume {
        crate::replication::ensure_active_active_maintenance_admitted(
            cfg,
            OPTIMIZE_XORBS_OPERATION,
        )?;
        return run_apply(
            args,
            &profile_name,
            &profile,
            &journal_path,
            output_mode,
            cfg,
        )
        .await;
    }

    // Default: show profile info and suggest --dry-run or --apply.
    println!("Xorb optimization profile: {profile_name}");
    println!(
        "  target_xorb_bytes: {} MiB",
        profile.target_xorb_bytes / (1024 * 1024)
    );
    println!("  max_xorbs_per_file: {}", profile.max_xorbs_per_file);
    println!("  group_by: {}", profile.group_by);
    println!("  compression: {}", profile.compression);
    println!();
    println!("Use --dry-run to estimate, or --apply to execute.");

    Ok(())
}

/// Resolve the profile from CLI args or auto-inference.
fn resolve_profile(args: &RestripeArgs, cfg: &Config) -> Result<(String, Profile)> {
    if let Some(ref name) = args.profile {
        let profile = Profile::from_name(name, &cfg.restripe)?;
        profile.validate()?;
        Ok((name.clone(), profile))
    } else {
        // Auto-infer from repo stats.
        // In the real implementation this would scan the file-index.
        // For now, default to `code` as a safe fallback.
        info!("no --profile specified, auto-inferring from repository statistics");
        let stats = RepoStats::scan(vec![]);
        let profile = inference::infer(&stats);
        let name = if profile == Profile::ml() {
            "ml"
        } else if profile == Profile::dataset() {
            "dataset"
        } else {
            "code"
        };
        Ok((name.to_string(), profile))
    }
}

/// Execute a dry-run estimation.
fn run_dry_run(profile_name: &str, profile: &Profile, output_mode: OutputMode, _cfg: &Config) {
    let cal = CalibrationConfig::default();

    // In the real implementation, we'd enumerate source xorbs via HEAD.
    // For now, return an empty estimate.
    let sources: Vec<SourceXorbMeta> = Vec::new();
    let estimate = planner::estimate(profile_name, profile, &sources, &cal, true);

    match output_mode {
        OutputMode::Json => {
            emit_json(OPTIMIZE_XORBS_PLAN_SCHEMA, "1.0", &estimate);
        }
        OutputMode::Jsonl => {
            // JSONL mode: emit the estimate as a single event.
            let stdout = std::io::stdout();
            let mut stream = JsonlStream::new(OPTIMIZE_XORBS_EVENT_SCHEMA, "1.0", stdout.lock());
            stream.emit_result(&estimate);
        }
        OutputMode::Text => {
            println!("Xorb optimization dry-run estimate:");
            println!("  Profile: {}", estimate.profile);
            println!("  Source xorbs: {}", estimate.source_count);
            println!("  Source bytes: {} bytes", estimate.source_bytes);
            println!("  Estimated dest xorbs: {}", estimate.estimated_dest_count);
            println!(
                "  Estimated dest bytes: {} bytes",
                estimate.estimated_dest_bytes
            );
            println!("  Estimated wall-clock: {}s", estimate.estimated_wall_secs);
            println!("  Estimated cost: ${}", estimate.estimated_cost_usd);
            if estimate.archive_source_count > 0 {
                println!(
                    "  Archive sources: {} ({} bytes)",
                    estimate.archive_source_count, estimate.archive_source_bytes
                );
            }
        }
    }
}

/// Abort the current xorb optimization run.
fn run_abort(journal_path: &Path, output_mode: OutputMode) -> Result<()> {
    if !RestripeJournal::exists(journal_path) {
        info!("no xorb optimization journal found — nothing to abort");
        return Ok(());
    }

    let journal = RestripeJournal::open(journal_path)?;
    if let Some(run) = journal.active_run()? {
        journal.abort_run(&run.run_id)?;
        info!(run_id = %run.run_id, "xorb optimization run aborted");

        if output_mode == OutputMode::Json {
            emit_json(
                OPTIMIZE_XORBS_EVENT_SCHEMA,
                "1.0",
                OptimizeXorbsControlEvent {
                    event: OptimizeXorbsControlEventKind::Aborted,
                    run_id: Some(run.run_id),
                },
            );
        } else if output_mode == OutputMode::Text {
            println!("Xorb optimization run {} aborted.", run.run_id);
            println!("Run `crab gc` to reclaim staged orphan xorbs.");
        }
    } else {
        info!("no active xorb optimization run to abort");
    }

    Ok(())
}

/// Execute or resume a xorb optimization run.
async fn run_apply(
    args: &RestripeArgs,
    profile_name: &str,
    profile: &Profile,
    journal_path: &Path,
    output_mode: OutputMode,
    cfg: &Config,
) -> Result<()> {
    let start = Instant::now();

    // Check for concurrent GC.
    let crab_dir = PathBuf::from(".crab");
    executor::check_gc_not_running(&crab_dir)?;

    // Open journal (acquires exclusive lock).
    let journal = RestripeJournal::open(journal_path)?;

    // Check for existing run.
    let run_id = if args.resume {
        match journal.active_run()? {
            Some(run) => {
                info!(run_id = %run.run_id, "resuming xorb optimization run");
                run.run_id
            }
            None => {
                return Err(CrabError::Configuration {
                    key: "--resume".to_string(),
                    origin: "no active xorb optimization run to resume".to_string(),
                });
            }
        }
    } else {
        // Start a new run.
        let run_id = uuid::Uuid::now_v7().to_string();
        let profile_json = profile.to_json();
        journal.start_run(&run_id, &profile_json)?;
        info!(run_id = %run_id, profile = %profile_name, "started new xorb optimization run");
        run_id
    };

    // Set up cancellation for graceful SIGINT/SIGTERM.
    let cancel = tokio_util::sync::CancellationToken::new();

    // Build the object store for xorb I/O. Falls back to journal-only
    // mode when the remote is not configured (e.g. in tests).
    let store_and_url = try_build_store(cfg, &cancel).await;
    let store = store_and_url.as_ref().map(|(store, _)| store.clone());
    if store.is_none() {
        info!("no remote configured; running in journal-only mode");
    }

    // Execute the pipeline.
    let exec_cfg = ExecutorConfig {
        include_cold: args.include_cold,
        restore_tier: args
            .restore_tier
            .clone()
            .unwrap_or_else(|| cfg.tier.restore_tier.clone()),
        output_class: args
            .output_class
            .clone()
            .unwrap_or_else(|| cfg.tier.restripe_output_class.clone()),
        ..ExecutorConfig::default()
    };

    // Audit: record restripe start.
    audit_shim::record(
        AuditOp::RestripeStart,
        &serde_json::json!({
            "run_id": run_id,
            "profile": profile_name,
            "include_cold": exec_cfg.include_cold,
            "output_class": exec_cfg.output_class,
        }),
    );

    let restore_orchestrator = if args.include_cold && cfg.tier.enabled {
        if let Some((_, parsed)) = &store_and_url {
            let mut options = crate::tier::runtime::restore_options_from_config(cfg)?;
            if let Some(tier) = &args.restore_tier {
                options.tier = crate::tier::runtime::parse_restore_tier(tier)?;
            }
            let backend = crate::tier::runtime::build_restore_backend(cfg, parsed).await?;
            Some(Arc::new(
                crate::tier::restore::RestoreOrchestrator::with_options(
                    backend,
                    cfg.tier.restore_max_concurrency,
                    Duration::from_secs(cfg.tier.restore_timeout_secs),
                    options,
                ),
            ))
        } else {
            None
        }
    } else {
        None
    };

    let outcome = executor::execute(
        &journal,
        &run_id,
        profile,
        &exec_cfg,
        &cancel,
        store.as_ref(),
        restore_orchestrator.as_deref(),
    )
    .await?;

    // Reconcile.
    let reconcile_outcome = reconcile::finalize(&journal, &run_id, store.as_ref()).await?;

    // Complete the run.
    journal.complete_run(&run_id)?;

    // Audit: record restripe finalize.
    audit_shim::record(
        AuditOp::RestripeFinalize,
        &serde_json::json!({
            "run_id": run_id,
            "profile": profile_name,
            "sources_done": outcome.sources_done,
            "sources_corrupt": outcome.sources_corrupt,
            "entries_updated": reconcile_outcome.entries_updated,
            "cas_attempts": reconcile_outcome.cas_attempts,
        }),
    );

    let elapsed = start.elapsed();

    // Emit summary.
    let summary = RestripeSummary {
        run_id: run_id.clone(),
        profile: profile_name.to_string(),
        counts: RestripeCounts {
            done: outcome.sources_done,
            corrupt: outcome.sources_corrupt,
            skipped: outcome.sources_skipped,
            pending: 0,
        },
        bytes_read: outcome.bytes_read,
        bytes_written: outcome.bytes_written,
        elapsed_ms: elapsed.as_millis() as u64,
        corrupt_list: outcome.corrupt_list.clone(),
    };

    match output_mode {
        OutputMode::Json => {
            emit_json(OPTIMIZE_XORBS_EVENT_SCHEMA, "1.0", &summary);
        }
        OutputMode::Jsonl => {
            let stdout = std::io::stdout();
            let mut stream = JsonlStream::new(OPTIMIZE_XORBS_EVENT_SCHEMA, "1.0", stdout.lock());
            stream.emit_result(&summary);
        }
        OutputMode::Text => {
            println!("Xorb optimization complete:");
            println!("  Run ID: {}", summary.run_id);
            println!("  Profile: {}", summary.profile);
            println!("  Done: {}", summary.counts.done);
            println!("  Corrupt: {}", summary.counts.corrupt);
            println!("  Skipped: {}", summary.counts.skipped);
            println!("  Bytes read: {}", summary.bytes_read);
            println!("  Bytes written: {}", summary.bytes_written);
            println!("  Duration: {}ms", summary.elapsed_ms);
            if !summary.corrupt_list.is_empty() {
                println!("  Corrupt sources:");
                for xorb in &summary.corrupt_list {
                    println!("    - {xorb}");
                }
                println!("  Recommendation: run `crab fsck` to investigate.");
            }
        }
    }

    Ok(())
}
