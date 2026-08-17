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

use object_store::path::Path as ObjectPath;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
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
use crate::storage::StoreLayout;
use crate::storage::head_class::head_with_class;
use crate::storage::store::Store;
use crate::tier::audit_shim::{self, AuditOp};

const OPTIMIZE_XORBS_AUTH_OPERATION: &str = "optimize-xorbs";
const OPTIMIZE_XORBS_OPERATION: &str = "optimize xorbs";
const OPTIMIZE_XORBS_PLAN_SCHEMA: &str = "optimize.xorbs.plan";
const OPTIMIZE_XORBS_EVENT_SCHEMA: &str = "optimize.xorbs.event";

/// Read the remote URL from `.crab/remote` and build a Store.
///
/// Resolve the configured remote and build the store used by planning and
/// execution. Remote operations fail closed because an empty source snapshot
/// would make a restripe plan look successful without touching the bucket.
async fn try_build_store(
    cfg: &Config,
    cancel: &CancellationToken,
) -> Result<(Store, crate::git::url::CrabUrl)> {
    let remote_path = crate::git::discover::resolve_crab_dir()
        .map_or_else(|| PathBuf::from(".crab/remote"), |d| d.join("remote"));
    let url = std::fs::read_to_string(&remote_path).map_err(|error| CrabError::Configuration {
        key: "remote".to_string(),
        origin: format!(
            "failed to read {}: {error}; run `crab init <url>` first",
            remote_path.display()
        ),
    })?;
    let parsed = crate::git::url::CrabUrl::parse(url.trim())?;
    let store =
        crate::auth::build_store(cfg, &parsed, OPTIMIZE_XORBS_AUTH_OPERATION, cancel).await?;
    Ok((store, parsed))
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
pub async fn run_restripe(
    args: &RestripeArgs,
    cfg: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    let output_mode = OutputMode::from_flags(args.json, args.jsonl);

    let journal_path = resolve_journal_path()?;

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

    // Handle --dry-run.
    if args.dry_run {
        let (store, _) = try_build_store(cfg, cancel).await?;
        let sources = enumerate_sources(&store, cancel).await?;
        let (profile_name, profile) = resolve_profile(args, cfg, Some(&sources))?;
        info!(profile = %profile_name, "resolved xorb optimization profile");
        run_dry_run(
            &profile_name,
            &profile,
            &sources,
            args.include_cold,
            output_mode,
        );
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
        return run_apply(args, &journal_path, output_mode, cfg, cancel).await;
    }

    // Resolve profile for the read-only summary. A remote source snapshot is
    // only needed for dry-run/apply, so the default summary stays offline.
    let (profile_name, profile) = resolve_profile(args, cfg, None)?;
    info!(profile = %profile_name, "resolved xorb optimization profile");

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

fn resolve_journal_path() -> Result<PathBuf> {
    let crab_dir =
        crate::git::discover::resolve_crab_dir().ok_or_else(|| CrabError::Configuration {
            key: ".crab".to_string(),
            origin: "run this command inside a Crab Git repository".to_string(),
        })?;
    Ok(crab_dir.join("restripe/journal.db"))
}

/// Resolve the profile from CLI args or auto-inference.
fn resolve_profile(
    args: &RestripeArgs,
    cfg: &Config,
    sources: Option<&[SourceXorbMeta]>,
) -> Result<(String, Profile)> {
    if let Some(ref name) = args.profile {
        let profile = Profile::from_name(name, &cfg.restripe)?;
        profile.validate()?;
        Ok((name.clone(), profile))
    } else {
        // The source snapshot is the available remote statistic at this
        // boundary. It is still a real size distribution, unlike the former
        // empty scan that always selected `code`.
        info!("no --profile specified, auto-inferring from repository statistics");
        let sizes = sources
            .unwrap_or_default()
            .iter()
            .map(|source| source.size_bytes)
            .collect();
        let stats = RepoStats::scan(sizes);
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

/// Snapshot source xorbs and their storage classes before a plan or run.
async fn enumerate_sources(
    store: &Store,
    cancel: &CancellationToken,
) -> Result<Vec<SourceXorbMeta>> {
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }

    let prefix = ObjectPath::from(".crab/xorbs/");
    let objects = store.list_prefix(&prefix).await?;
    let mut sources = Vec::with_capacity(objects.len());

    for object in objects {
        if cancel.is_cancelled() {
            return Err(CrabError::Cancelled);
        }

        let key = object.location.to_string();
        let hash = key
            .strip_prefix(".crab/xorbs/")
            .ok_or_else(|| CrabError::Configuration {
                key: format!("remote xorb key {key}"),
                origin: "xorb listing returned an object outside .crab/xorbs/".to_string(),
            })?;
        if hash.is_empty() || hash.contains('/') {
            return Err(CrabError::Configuration {
                key: format!("remote xorb key {key}"),
                origin: "expected one content hash directly below .crab/xorbs/".to_string(),
            });
        }

        let head = head_with_class(store, &object.location).await?;
        sources.push(SourceXorbMeta {
            hash: hash.to_string(),
            size_bytes: object.size,
            storage_class: head.class.to_string(),
            is_archive: head.class.is_archive_class(),
        });
    }

    sources.sort_unstable_by(|left, right| left.hash.cmp(&right.hash));
    Ok(sources)
}

/// Execute a dry-run estimation.
fn run_dry_run(
    profile_name: &str,
    profile: &Profile,
    sources: &[SourceXorbMeta],
    include_cold: bool,
    output_mode: OutputMode,
) {
    let cal = CalibrationConfig::default();
    let estimate = planner::estimate(profile_name, profile, sources, &cal, include_cold);

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
    journal_path: &Path,
    output_mode: OutputMode,
    cfg: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    let start = Instant::now();

    // Check for concurrent GC.
    let crab_dir = journal_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| CrabError::Configuration {
            key: "restripe journal".to_string(),
            origin: "journal path has no .crab parent".to_string(),
        })?;
    executor::check_gc_not_running(crab_dir)?;

    // Open journal (acquires exclusive lock).
    let journal = RestripeJournal::open(journal_path)?;

    let recorded_run = if args.resume {
        Some(
            journal
                .active_run()?
                .ok_or_else(|| CrabError::Configuration {
                    key: "--resume".to_string(),
                    origin: "no active xorb optimization run to resume".to_string(),
                })?,
        )
    } else {
        None
    };

    let (store, parsed) = try_build_store(cfg, cancel).await?;
    let router = StoreLayout::new(store.clone(), parsed.repo_path.clone());
    let sources = if recorded_run.is_none() {
        enumerate_sources(&store, cancel).await?
    } else {
        Vec::new()
    };

    let (run_id, profile_name, profile) = if let Some(run) = recorded_run {
        let recorded_profile = Profile::from_json(&run.profile)?;
        let profile_name = if let Some(name) = args.profile.as_deref() {
            let requested = Profile::from_name(name, &cfg.restripe)?;
            if requested != recorded_profile {
                return Err(CrabError::Configuration {
                    key: "--profile".to_string(),
                    origin: "resume profile does not match the profile recorded in the journal"
                        .to_string(),
                });
            }
            name.to_string()
        } else {
            profile_label(&recorded_profile)
        };
        info!(run_id = %run.run_id, profile = %profile_name, "resuming xorb optimization run");
        (run.run_id, profile_name, recorded_profile)
    } else {
        let (profile_name, profile) = resolve_profile(args, cfg, Some(&sources))?;
        let run_id = uuid::Uuid::now_v7().to_string();
        journal.start_run(&run_id, &profile.to_json())?;
        for source in &sources {
            journal.insert_source(&run_id, &source.hash)?;
        }
        info!(run_id = %run_id, profile = %profile_name, sources = sources.len(), "started new xorb optimization run");
        (run_id, profile_name, profile)
    };

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
        let mut options = crate::tier::runtime::restore_options_from_config(cfg)?;
        if let Some(tier) = &args.restore_tier {
            options.tier = crate::tier::runtime::parse_restore_tier(tier)?;
        }
        let backend = crate::tier::runtime::build_restore_backend(cfg, &parsed).await?;
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
    };

    let outcome = executor::execute(
        &journal,
        &run_id,
        &profile,
        &exec_cfg,
        cancel,
        Some(&store),
        restore_orchestrator.as_deref(),
    )
    .await?;

    // Reconcile.
    let reconcile_outcome =
        reconcile::finalize(&journal, &run_id, Some(&store), Some(&router), cancel).await?;

    let counts = journal.count_by_status(&run_id)?;
    if counts.pending > 0 || counts.staged > 0 {
        return Err(CrabError::Configuration {
            key: "restripe run pending sources".to_string(),
            origin: format!(
                "{} source(s) remain incomplete; rerun with --resume",
                counts.pending + counts.staged
            ),
        });
    }

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
        profile: profile_name.clone(),
        counts: RestripeCounts {
            done: counts.done,
            corrupt: counts.corrupt,
            skipped: counts.skipped,
            pending: counts.pending + counts.staged,
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

fn profile_label(profile: &Profile) -> String {
    if *profile == Profile::ml() {
        "ml".to_string()
    } else if *profile == Profile::dataset() {
        "dataset".to_string()
    } else if *profile == Profile::code() {
        "code".to_string()
    } else {
        "recorded".to_string()
    }
}
