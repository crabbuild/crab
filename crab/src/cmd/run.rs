//! `crab run` — stage execution with content-addressed caching.
//!
//! Three invocation modes routed from a single [`RunArgs`]:
//!
//! 1. **Inline single-stage** (phase 1): `crab run --name foo --deps …
//!    --outs … -- cmd args`. Parses CLI flags into an ephemeral
//!    [`Stage`], resolves path deps via whole-file blake3, and walks
//!    the executor's state machine.
//!
//! 2. **Single-stage from `crab.yaml`** (phase 3): `crab run
//!    <stage>` in a repo whose root contains a workflow yaml. Parses
//!    yaml, picks the named stage, resolves its deps with the working
//!    tree, lockfile, and in-memory run state, then executes via the
//!    same executor the no-args DAG mode uses.
//!
//! 3. **No-args DAG** (phase 3): `crab run` with no positional and
//!    no inline flags. Parses `crab.yaml`, builds the DAG,
//!    topo-sorts, scans prior journals for non-terminal stages,
//!    executes every stale stage in topological order, and writes
//!    `crab.lock` atomically at the end.
//!
//! Output policy:
//! - Text mode prints a short human-readable summary.
//! - `--json` emits a single `Envelope<WorkflowStageResult>` (or
//!   `Envelope<WorkflowPlan>` under `--dry-run`) via `core::output`.
//! - `--jsonl` emits a stream of `workflow.stage.*` events and a
//!   terminal `workflow.stage_result` event through `JsonlStream`.
//!   DAG-run structured output lands in task 3.14.
//!
//! Event wiring (phase 1, single-stage mode): events are emitted at
//! the `cmd/run.rs` boundary between executor calls rather than from
//! inside the executor. The executor still records the same
//! transitions to the journal, so consumers get the full trajectory
//! there; the JSONL stream carries the `started`, `cache_checked`,
//! `produced`, `hashed`, `committed`, and `failed` events. Queue
//! callers may also subscribe to the spawned child boundary so kill
//! requests do not race setup before a process exists.
//!
//! Partial-DAG success semantics (`--keep-going`, `--ignore-errors`)
//! follow design §"DAG partial success":
//! - default: first stage failure aborts the rest of the DAG.
//! - `--keep-going`: downstream stages of the failure are marked
//!   `NotStarted`; unrelated independent branches continue.
//! - `--ignore-errors`: all remaining stages attempted even when
//!   their producers failed (missing producer outs still surface as
//!   `StageDepMissing` at resolve time).
//!
//! Visualizing the DAG:
//! - `crab workflow dag` renders the inferred producer →
//!   consumer graph as ASCII or Mermaid (`--format mermaid`) so you
//!   can inspect the schedule `crab run` would follow without
//!   executing anything.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info, info_span, instrument, warn};
use uuid::Uuid;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::metrics::Metrics;
use crate::core::output::{
    JsonlStream, OutputMode, WORKFLOW_RUN_SCHEMA, WORKFLOW_SCHEMA_VERSION,
    WORKFLOW_STAGE_NOT_STARTED_SCHEMA, WORKFLOW_STAGE_RESULT_SCHEMA, WORKFLOW_STAGE_RETRY_SCHEMA,
    WorkflowRunSummary, WorkflowStageCacheChecked, WorkflowStageCommitted, WorkflowStageEvent,
    WorkflowStageFailed, WorkflowStageHashed, WorkflowStageNotStarted, WorkflowStageOut,
    WorkflowStageProduced, WorkflowStageResult, WorkflowStageRetry, WorkflowStageStarted,
    emit_json,
};
use crate::workflow::cache::{
    CurrentFile, OverwriteDecision, OverwriteFlags, RemoteArtifactStores, StageCacheEntry,
    cached_artifacts, overwrite_policy, read_local, read_local_xorb,
};
use crate::workflow::executor::{
    ExecutorConfig, StageOutResolver, resolve_dep_hashes_with_wdir_allow_missing_remote_aliases,
    run_local,
};
use crate::workflow::gitignore::ensure_workflow_ignored;
use crate::workflow::hasher::{ResolvedStage, compute as compute_stage_hash};
use crate::workflow::journal::{Journal, RunOutcome};
use crate::workflow::lockfile_split::{self, LockfileMode, StageProvenance};
use crate::workflow::materialize::write_atomic;
use crate::workflow::params::resolve_stage_param_values_with_wdir;
use crate::workflow::resume::{
    self, CliFlags, DagResumeReport, FsState, ResumeAction, StageAction, walk_dag,
};
use crate::workflow::scheduler_lock::SchedulerLock;
use crate::workflow::stage::{
    Cmd, Dep, DepUrlHashExt, EnvSpec, Out, OutKind, Resources, RetryPolicy, Stage, StageName,
    is_url_dep,
};
use crab_types::workflow::StageHash;
use crab_workflow::{
    FailureKind, Graph, LockedDep, Lockfile, RetryDecision, RunState, StageState, Workflow, retry,
    yaml,
};

pub use crate::core::output::WorkflowStageOut as StageResultOut;
/// Re-export of the canonical single-envelope schema for
/// `crab run` (single-stage mode). Kept as a `pub use` so
/// existing call sites — tests, integration harnesses — that
/// referenced `cmd::run::StageResult` keep compiling.
pub use crate::core::output::WorkflowStageResult as StageResult;

/// Schema and version strings for structured output envelopes.
/// Workflow plan is single-stage-specific and stays local to this
/// module; stage-level schemas live in `core::output::event_payloads`.
const WORKFLOW_PLAN_SCHEMA: &str = "workflow.plan";
const WORKFLOW_DAG_PLAN_SCHEMA: &str = "workflow.dag_plan";
/// Umbrella schema for the `JsonlStream` that drives the workflow
/// stage event stream. Individual events carry their own
/// `workflow.stage.*` schema; this label identifies the stream as a
/// whole for consumers keying on `schema`.
const WORKFLOW_STAGE_EVENT_STREAM_SCHEMA: &str = "workflow.stage.event";

#[derive(Clone)]
pub(crate) struct RunInvocationOptions {
    pub mirror_child_output: bool,
    pub external_kill_path: Option<PathBuf>,
    pub child_started: Option<Arc<dyn Fn(u32) + Send + Sync>>,
}

impl Default for RunInvocationOptions {
    fn default() -> Self {
        Self {
            mirror_child_output: true,
            external_kill_path: None,
            child_started: None,
        }
    }
}

/// CLI arguments for `crab run`.
///
/// Matches the flag set called out in design §"Single stage". Fields
/// are grouped so `clap` can render a readable `--help` output.
#[derive(Debug, Clone, Parser)]
pub struct RunArgs {
    /// Stage name. Required for single-stage mode. Validated against
    /// the R17 grammar (`^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$`).
    #[arg(long)]
    pub name: Option<String>,

    /// Dep paths. Repeatable.
    #[arg(long = "deps", value_name = "PATH")]
    pub deps: Vec<PathBuf>,

    /// Out paths. Repeatable.
    #[arg(long = "outs", value_name = "PATH")]
    pub outs: Vec<PathBuf>,

    /// Env allowlist — only the named variables participate in the
    /// stage hash. Repeatable. Conflicts with `--empty-env`.
    #[arg(long = "env", value_name = "VAR", conflicts_with = "empty_env")]
    pub env: Vec<String>,

    /// Run the stage with an empty environment (plus the minimum
    /// PATH / HOME / TMPDIR injected by `workflow::env`).
    #[arg(long, default_value_t = false)]
    pub empty_env: bool,

    /// Per-stage timeout (e.g. `30s`, `5m`, `1h`). Empty means no
    /// timeout — the stage runs until the child exits.
    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<String>,

    /// Opt into the hermetic sandbox.
    #[arg(long, default_value_t = false)]
    pub hermetic: bool,

    /// Mark the stage non-deterministic. Participates in the stage
    /// hash so cache entries from deterministic runs don't satisfy a
    /// stage the user has declared non-reproducible.
    #[arg(long, default_value_t = false)]
    pub nondeterministic: bool,

    /// Ignore a cache hit and force re-execution.
    #[arg(long, default_value_t = false)]
    pub force: bool,

    /// Print the plan and exit without executing.
    #[arg(long, visible_alias = "dry", default_value_t = false)]
    pub dry_run: bool,

    /// Ask before executing each stage that would otherwise run.
    #[arg(long, short = 'i', default_value_t = false)]
    pub interactive: bool,

    /// Materialize cached outs only. Exits 3 on a cache miss (R6).
    #[arg(long, conflicts_with = "no_run_cache", default_value_t = false)]
    pub cache_only: bool,

    /// Execute stage commands even when a matching run-cache entry
    /// exists. Fresh outputs are still written to the cache.
    #[arg(
        long = "no-run-cache",
        conflicts_with = "cache_only",
        default_value_t = false
    )]
    pub no_run_cache: bool,

    /// Execute stages and update lockfiles without writing new
    /// run-cache entries or output xorbs.
    #[arg(long = "no-commit", default_value_t = false)]
    pub no_commit: bool,

    /// Refuse a cache hit when it would overwrite an existing,
    /// differing file at a declared out path (R12).
    #[arg(long, default_value_t = false)]
    pub no_overwrite: bool,

    /// Trust outs on the filesystem when resuming a `Running` crash,
    /// even if the journal did not record their hashes.
    #[arg(long, default_value_t = false)]
    pub resume_trust_outputs: bool,

    /// Abandon a stuck journal by marking it `Aborted`, then exit.
    #[arg(long, value_name = "RUN_ID")]
    pub abandon: Option<String>,

    /// Print an input-hash breakdown on a cache miss. Also printed
    /// implicitly under `--dry-run`.
    #[arg(long, default_value_t = false)]
    pub explain_miss: bool,

    /// Seconds to wait for the workflow scheduler lock before
    /// giving up. Defaults to `[workflow] lock_timeout_secs`
    /// (600 per R24). `--no-wait` overrides to zero.
    #[arg(long, value_name = "SECS")]
    pub lock_timeout: Option<u64>,

    /// Fail fast if another `crab run` already holds the
    /// scheduler lock. Equivalent to `--lock-timeout 0`; wins when
    /// both are set.
    #[arg(long, default_value_t = false)]
    pub no_wait: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, conflicts_with = "jsonl", default_value_t = false)]
    pub json: bool,

    /// Streaming JSONL output.
    #[arg(long, conflicts_with = "json", default_value_t = false)]
    pub jsonl: bool,

    /// Discover and merge every `crab.yaml` under the repo root,
    /// prefixing nested stage names with their containing directory
    /// joined by dots (R17). Overrides `[workflow] discover` when
    /// set; otherwise the config setting wins.
    #[arg(long, short = 'R', default_value_t = false)]
    pub recursive: bool,

    /// DVC-compatible target mode: run only the named target stage(s)
    /// without adding upstream dependencies.
    #[arg(
        long = "single-item",
        short = 's',
        conflicts_with_all = ["downstream", "pipeline", "all_pipelines"],
        default_value_t = false
    )]
    pub single_item: bool,

    /// DVC-compatible target mode: run the named target stage(s) and
    /// their downstream consumers.
    #[arg(
        long,
        conflicts_with_all = ["single_item", "pipeline", "all_pipelines"],
        default_value_t = false
    )]
    pub downstream: bool,

    /// DVC-compatible execution mode: after a stage executes, force
    /// its descendants to execute instead of restoring run-cache hits.
    #[arg(long = "force-downstream", default_value_t = false)]
    pub force_downstream: bool,

    /// DVC-compatible target mode: run every stage in the pipeline
    /// component containing the target stage(s).
    #[arg(
        long,
        short = 'p',
        conflicts_with_all = ["single_item", "downstream", "all_pipelines"],
        default_value_t = false
    )]
    pub pipeline: bool,

    /// DVC-compatible target mode: discover and run all pipelines
    /// under the repository root. Positional targets are ignored.
    #[arg(
        long = "all-pipelines",
        short = 'P',
        conflicts_with_all = ["single_item", "downstream", "pipeline", "workflow", "stages", "glob"],
        default_value_t = false
    )]
    pub all_pipelines: bool,

    /// DAG partial-success mode: when a stage fails, downstream
    /// stages are marked NotStarted but unrelated branches continue.
    /// Only meaningful in multi-stage mode; ignored for inline
    /// single-stage runs. See design §"DAG partial success".
    #[arg(long, default_value_t = false)]
    pub keep_going: bool,

    /// Aggressive partial-success: attempt every remaining stage
    /// even when its producers failed. Implies `--keep-going`.
    /// Missing producer outs still surface as `StageDepMissing`
    /// at resolve time.
    #[arg(long, default_value_t = false)]
    pub ignore_errors: bool,

    /// Maximum number of stages to execute concurrently. Overrides
    /// `[workflow] parallelism` from config. Defaults to
    /// `min(num_cpus, 8)`.
    #[arg(long, value_name = "N")]
    pub parallelism: Option<u32>,

    /// Push newly-produced stage cache entries to the configured
    /// remote after each stage commits. Without this flag, no
    /// remote writes occur (backward compatible).
    #[arg(long, default_value_t = false)]
    pub cache_push: bool,

    /// Skip stages whose only "change" is that dep files are missing
    /// from the workspace but their hashes in the lockfile haven't
    /// changed. Combined with `--pull`, only pull data needed for
    /// stages that actually need to re-execute.
    #[arg(long, default_value_t = false)]
    pub allow_missing: bool,

    /// Automatically download missing dep files from the remote
    /// before executing stages that need them. When combined with
    /// `--allow-missing`, only data for changed stages is downloaded.
    #[arg(long, default_value_t = false)]
    pub pull: bool,

    /// Validate `crab.yaml` without executing any stage. Parses
    /// the workflow, runs all semantic checks (name grammar,
    /// self-loops, duplicate outs, value ranges), and reports all
    /// errors as a structured JSON array. Exits 0 on valid, 2 on
    /// any error.
    #[arg(long, default_value_t = false)]
    pub validate: bool,

    /// Watch mode: execute the DAG once, then watch all declared dep
    /// paths for modifications. On change, recompute staleness and
    /// re-execute affected stages. Exit on SIGINT/SIGTERM.
    #[cfg(feature = "watch")]
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Execute only stages belonging to the named workflow (plus
    /// their upstream deps from other workflows). Requires the
    /// `workflows:` key in `crab.yaml`.
    #[arg(long, value_name = "NAME")]
    pub workflow: Option<String>,

    /// Execute only stages matching the glob pattern (plus their
    /// upstream deps via transitive closure). Supports `*` and `?`
    /// wildcards.
    #[arg(long, value_name = "GLOB")]
    pub stages: Option<String>,

    /// Treat positional targets as glob patterns over stage names,
    /// matching DVC's `repro --glob` behavior.
    #[arg(long, default_value_t = false)]
    pub glob: bool,

    /// Command argv (inline single-stage mode) or positional stage
    /// targets from `crab.yaml`. Clap captures everything after `--`
    /// into this vec; we disambiguate modes at runtime in [`run_in`].
    ///
    /// - Inline mode (requires `--name`): treat the whole vec as
    ///   argv for the child process.
    /// - Yaml mode (no `--name`, `crab.yaml` present): targets
    ///   select stage sets using DVC-compatible recursion flags.
    /// - DAG mode: the vec is empty and `crab.yaml` is parsed as
    ///   a whole.
    pub cmd: Vec<String>,
}

impl RunArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, self.jsonl)
    }
}

/// Structured-output payload for `--dry-run`. Captures the resolved
/// stage hash and dep fingerprints so consumers can reason about what
/// would happen without executing anything. Richer fields (cost
/// estimate, cache-source breakdown) land in task 1.18.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct WorkflowPlan {
    pub stage_name: String,
    pub stage_hash: String,
    pub cache_hit: bool,
    pub deps: Vec<PlanDep>,
    pub outs: Vec<PlanOut>,
    pub cmd: PlanCmd,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PlanDep {
    pub path: PathBuf,
    pub file_hash: String,
    pub size: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PlanOut {
    pub path: PathBuf,
    pub kind: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PlanCmd {
    Argv { argv: Vec<String> },
    Shell { shell: String },
    ShellList { commands: Vec<String> },
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct WorkflowDagPlan {
    pub stages: Vec<DagPlanStage>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DagPlanStage {
    pub stage_name: String,
    pub cmd: PlanCmd,
}

/// Entry point dispatched from `main.rs`.
pub async fn exec(args: RunArgs) -> Result<()> {
    let mode = args.output_mode();

    // `--abandon` takes priority: mark the target journal `Aborted`
    // and exit, never parse the stage.
    if let Some(run_id_str) = args.abandon.as_deref() {
        return run_abandon(run_id_str, mode);
    }

    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_in(&args, &cwd, mode).await
}

/// Testable entry point that accepts a working directory explicitly.
pub async fn run_in(args: &RunArgs, repo_root: &Path, mode: OutputMode) -> Result<()> {
    run_in_with_options(args, repo_root, mode, RunInvocationOptions::default()).await
}

pub(crate) async fn run_in_with_options(
    args: &RunArgs,
    repo_root: &Path,
    mode: OutputMode,
    options: RunInvocationOptions,
) -> Result<()> {
    // Config gate: workflow layer opt-in via `[workflow] enabled = true`.
    // Falling back to defaults is the right move in a fresh repo — the
    // feature flag default is already `false`, so the user's intent
    // stays correct either way.
    let config = Config::resolve_for_repo(repo_root).unwrap_or_default();
    if !config.workflow.enabled {
        return Err(CrabError::WorkflowDisabled);
    }

    // Route by mode. Priority:
    //   1. Inline single-stage: `--name` set OR `--deps`/`--outs`
    //      populated. Preserves phase-1 behavior even when a
    //      `crab.yaml` exists so scripts relying on the inline
    //      form keep working.
    //   2. `crab.yaml` discovered (root or recursively, per the
    //      effective discover mode): positional arg → single stage
    //      from yaml; no positional → full DAG.
    //   3. Nothing to do: fall through to the phase-1 error asking
    //      for `--name`.
    let inline_signals = args.name.is_some() || !args.deps.is_empty() || !args.outs.is_empty();

    if inline_signals {
        return run_inline_single_stage(args, repo_root, mode, &config, options).await;
    }

    // Workflow discovery: R2. `--recursive` wins over the config
    // setting; otherwise `[workflow] discover` dictates the mode.
    // Root mode with nested yaml files returns
    // `WorkflowDiscoveryAmbiguous` — that propagates unchanged so
    // the user sees both candidate files.
    let discover_mode = resolve_discover_mode(args, &config);
    let discovered = crate::workflow::discover::discover(repo_root, discover_mode)?;

    if !discovered.is_empty() {
        if args.validate {
            run_validate(repo_root, &discovered);
            return Ok(());
        }
        return run_with_yaml(args, repo_root, &discovered, mode, &config, options).await;
    }

    // No yaml and no inline flags — surface the phase-1 error so
    // the user gets a clear pointer at what to add.
    Err(CrabError::Configuration {
        key: "no `crab.yaml` found and no inline flags; \
              pass --name/--deps/--outs/-- or add a crab.yaml"
            .into(),
        origin: "cli".into(),
    })
}

/// Phase-1 inline single-stage path. Kept byte-for-byte equivalent
/// to the original `run_in` so existing callers are unaffected.
async fn run_inline_single_stage(
    args: &RunArgs,
    repo_root: &Path,
    mode: OutputMode,
    config: &Config,
    options: RunInvocationOptions,
) -> Result<()> {
    // Validate CLI shape before touching the filesystem.
    let stage_name_str = args
        .name
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: "--name is required for single-stage `crab run`".into(),
            origin: "cli".into(),
        })?;
    let stage_name = StageName::parse(stage_name_str)?;

    if args.cmd.is_empty() {
        return Err(CrabError::Configuration {
            key: "no command provided — pass the argv after `--`".into(),
            origin: "cli".into(),
        });
    }
    let cmd = Cmd::Argv(args.cmd.clone());

    // Ensure `.gitignore` shields the workflow scratch tree on first use.
    ensure_workflow_ignored(repo_root)?;

    // Build the stage.
    let env_spec = build_env_spec(args);
    let timeout = parse_timeout(args.timeout.as_deref())?;
    let outs = build_outs(&args.outs);
    let deps: Vec<Dep> = args.deps.iter().map(cli_dep).collect();

    let stage = Stage {
        deps: deps.clone(),
        outs: outs.clone(),
        env: env_spec.clone(),
        retry: None,
        timeout,
        persist: false,
        nondeterministic: args.nondeterministic,
        hermetic: args.hermetic,
        ..Stage::new(stage_name.clone(), cmd.clone())
    };

    // Validate declared outs early — before we touch the journal.
    for out in &stage.outs {
        out.validate(&stage.name)?;
    }

    // Resolve deps to content hashes. Missing paths become
    // `StageDepMissing`; non-file entries (symlinks, FIFOs, etc.)
    // become `StageDepMalformed`.
    // When --pull is set, attempt to download missing deps first.
    if args.pull {
        try_pull_missing_deps(&stage, &stage_name, repo_root);
    }
    let remote_aliases = workflow_remote_aliases(config);
    let dep_hashes =
        resolve_dep_hashes_with_aliases(&stage.name, &deps, repo_root, &remote_aliases)?;

    let resolved = ResolvedStage {
        stage: stage.clone(),
        dep_hashes: dep_hashes.clone(),
        params: std::collections::BTreeMap::new(),
        env: env_spec.clone(),
        cmd: cmd.clone(),
        outs: outs.clone(),
    };
    let stage_hash = compute_stage_hash(&resolved);

    // Pre-execution planning / cache probe.
    let workflow_root = repo_root.join(".crab").join("workflow");
    let cache_root = repo_root.join(".crab").join("cache");
    crate::workflow::cache::probe_cache_writable(&cache_root);
    let cache_lookup_enabled = stage_cache_lookup_enabled(&stage, args);
    let execution_cache_lookup_enabled = stage_execution_cache_lookup_enabled(&stage, args);
    let cached = if cache_lookup_enabled {
        read_local(&cache_root, &stage_hash).ok().flatten()
    } else {
        None
    };
    let cache_hit = cached.is_some() && !args.force;

    if args.explain_miss && !cache_hit {
        emit_miss_explanation(&resolved, &stage_hash, mode, repo_root);
    }

    if args.dry_run {
        emit_plan(&resolved, &stage_hash, cache_hit, mode);
        return Ok(());
    }

    if args.interactive && !cache_hit && !confirm_interactive_stage(&stage_name)? {
        info!(stage = %stage_name, "run: stage skipped by interactive prompt");
        return Ok(());
    }

    // Build the remote store for cache pull (and push if --cache-push).
    // Failures are non-fatal — we fall through to local-only mode.
    let remote = try_build_workflow_remote(repo_root, config, args.cache_push).await;
    let remote_store = remote.as_ref().map(|remote| remote.store.clone());
    let remote_prefix = remote.as_ref().map(|remote| remote.prefix.clone());
    let remote_primary_fallback_store = remote
        .as_ref()
        .and_then(|remote| remote.primary_fallback.as_ref())
        .map(|fallback| fallback.store.clone());
    let remote_primary_fallback_prefix = remote
        .as_ref()
        .and_then(|remote| remote.primary_fallback.as_ref())
        .map(|fallback| fallback.prefix.clone());
    let remote_artifact_stores = remote
        .as_ref()
        .and_then(|remote| remote.artifact_stores.clone());

    if args.cache_only {
        let cache_only_ctx = CacheOnlyContext {
            args,
            mode,
            cache_lookup_enabled,
            artifact_stores: remote_artifact_stores.as_ref(),
            remote: CacheOnlyRemote {
                selected: remote_store.as_ref().zip(remote_prefix.as_deref()).map(
                    |(store, prefix)| WorkflowRemoteCandidate {
                        store,
                        prefix,
                        source: "selected",
                    },
                ),
                primary_fallback: remote_primary_fallback_store
                    .as_ref()
                    .zip(remote_primary_fallback_prefix.as_deref())
                    .map(|(store, prefix)| WorkflowRemoteCandidate {
                        store,
                        prefix,
                        source: "primary-fallback",
                    }),
            },
            cache_root: &cache_root,
            working_dir: Some(repo_root),
        };
        return cache_only_path(
            &stage_name,
            &stage_hash,
            &outs,
            cached.as_ref(),
            cache_only_ctx,
        )
        .await;
    }

    // Normal execution path. Sweep orphan sidecars + resume prior
    // non-terminal journals before opening ours.
    let run_id = Uuid::now_v7();
    sweep_orphans(&workflow_root, repo_root, &outs)?;

    // Serialize against other `crab run` invocations on this repo
    // (design §"Concurrency model"). We hold the lock for the full
    // remainder of the run; drop on return releases it.
    let lock_timeout = compute_lock_timeout(args, config);
    let _scheduler_lock = SchedulerLock::acquire(&workflow_root, lock_timeout)?;

    // Process-local metrics Arc. The counters bumped here are
    // observability-only; they're not persisted across `crab run`
    // invocations (see R21 — counters live on the in-process
    // `Metrics` struct). Handing the executor a clone means miss
    // path / hit path / failure / retry attempts all land on the
    // same Arc as journal-resume bumps below.
    let metrics = Arc::new(Metrics::new());
    scan_prior_journals(&workflow_root, run_id, args, metrics.as_ref())?;

    let journal_path = workflow_root
        .join("runs")
        .join(run_id.to_string())
        .join("journal.db");
    let journal = Journal::open(&journal_path)?;
    journal.insert_run_start(run_id, env!("CARGO_PKG_VERSION"), &host_fingerprint())?;
    journal.insert_stage_start(run_id, stage_name.as_str())?;
    journal.transition(run_id, stage_name.as_str(), 1, StageState::Resolved, "{}")?;

    // Executor config pulls defaults from the workflow config so
    // operator overrides apply without threading them through again.
    let executor_cfg = ExecutorConfig {
        workflow_root: workflow_root.clone(),
        cache_root: cache_root.clone(),
        graceful_shutdown: std::time::Duration::from_secs(
            config.workflow.graceful_shutdown_timeout_secs,
        ),
        stderr_tail_bytes: crate::workflow::signals::DEFAULT_STDERR_TAIL_BYTES,
        mirror_child_output: options.mirror_child_output,
        external_kill_path: options.external_kill_path.clone(),
        child_started: options.child_started.clone(),
        max_outs_per_stage: config.workflow.max_outs_per_stage,
        default_max_out_bytes: Some(config.workflow.max_out_bytes),
        host_fingerprint: host_fingerprint(),
        working_dir: Some(repo_root.to_path_buf()),
        metrics: Some(metrics.clone()),
        cache_push: args.cache_push,
        no_run_cache: args.no_run_cache || args.force,
        no_commit: args.no_commit,
        remote_store: remote_store.clone(),
        remote_prefix: remote_prefix.clone(),
        remote_primary_fallback_store,
        remote_primary_fallback_prefix,
        remote_artifact_stores: remote_artifact_stores.clone(),
        remote_aliases,
        min_cache_headroom: config.workflow.min_cache_headroom_bytes(),
    };

    // JSONL stream for `--jsonl` mode. Held behind an Option so we can
    // emit a start event up front and a terminal result event at the
    // end without juggling the stream reference across branches.
    let mut jsonl = if mode == OutputMode::Jsonl {
        Some(JsonlStream::new(
            WORKFLOW_STAGE_EVENT_STREAM_SCHEMA,
            WORKFLOW_SCHEMA_VERSION,
            std::io::stdout(),
        ))
    } else {
        None
    };
    emit_jsonl_started(jsonl.as_mut(), stage_name.as_str(), &stage_hash);

    // Cache-checked event — consumers learn the probe outcome before
    // any running/produced event fires. `hit_source` matches the
    // executor's journal payload vocabulary: "local", "remote", or
    // "none" (phase 1 only consults local).
    emit_jsonl_cache_checked(
        jsonl.as_mut(),
        stage_name.as_str(),
        &stage_hash,
        cache_hit,
        if cache_hit { "local" } else { "none" },
    );

    let started = Instant::now();

    // Retry loop for inline single-stage mode. Inline stages
    // typically have `retry: None` (no CLI flag for retry policy
    // yet), but the loop is wired so yaml-backed inline stages
    // with a retry policy work correctly.
    let policy = stage.retry.clone().unwrap_or_else(RetryPolicy::no_retry);
    let mut attempt: u32 = 1;
    let exec_result = loop {
        let result = run_local(&resolved, &executor_cfg, &journal, run_id, attempt)
            .await
            .map_err(CrabError::from);
        match result {
            Ok(entry) => break Ok(entry),
            Err(e) => {
                let kind = classify_failure_kind(&e);
                match retry::should_retry(&policy, &kind, attempt) {
                    RetryDecision::Retry { backoff } => {
                        let (reason, _, _, _) = classify_failure(&e);
                        debug!(
                            stage = %stage_name,
                            attempt,
                            backoff_ms = backoff.as_millis() as u64,
                            "retry: scheduling next attempt"
                        );
                        emit_jsonl_retry(
                            jsonl.as_mut(),
                            stage_name.as_str(),
                            &stage_hash,
                            attempt,
                            reason,
                            backoff,
                        );
                        clean_partial_outputs(&stage, repo_root);
                        attempt += 1;
                        journal.insert_stage_retry(run_id, stage_name.as_str(), attempt)?;
                        journal.transition(
                            run_id,
                            stage_name.as_str(),
                            attempt,
                            StageState::Resolved,
                            "{}",
                        )?;
                        tokio::time::sleep(backoff).await;
                    }
                    RetryDecision::Exhausted => {
                        break Err(e);
                    }
                }
            }
        }
    };

    let result = match exec_result {
        Ok(entry) => {
            // Whether this was a hit depends on whether we already had
            // a local entry before the executor ran (and --force wasn't
            // set). The executor does its own lookup internally but
            // returns the entry in both cases; we use our pre-probe to
            // answer cleanly.
            //
            // Remote cache hits: if the pre-probe showed no local entry
            // but the local cache now has one (written by pull_remote),
            // the executor served from the remote cache.
            let used_cache = cache_hit;
            let from_remote = !cache_hit
                && execution_cache_lookup_enabled
                && remote_store.is_some()
                && read_local(&cache_root, &stage_hash)
                    .ok()
                    .flatten()
                    .is_some();

            // Cache hit path: materialize outs via the atomic sidecar
            // write, respecting overwrite policy from task 1.16.
            if used_cache {
                materialize_hit(&stage_name, run_id, &entry, &cache_root, args)?;

                // P7: on_cache_hit hook execution for inline stages.
                if stage.side_effects {
                    if let Some(hook_cmd) = &stage.on_cache_hit {
                        let status = crate::workflow::executor::execute_hook(
                            hook_cmd,
                            &stage.env,
                            executor_cfg.working_dir.as_deref(),
                        )
                        .await?;
                        if !status.success() {
                            let exit_code = status.code().unwrap_or(-1);
                            return Err(CrabError::StageSideEffectHookFailed {
                                stage: stage_name.as_str().to_owned(),
                                exit_code,
                            });
                        }
                    } else {
                        warn!(
                            stage = %stage_name,
                            "stage has side_effects: true but no on_cache_hit hook; \
                             side effects were skipped on this cache hit"
                        );
                    }
                }
            } else if !from_remote {
                // Miss-path events — emit after the executor has
                // produced/hashed but before we mark `Committed` so
                // consumers see the whole transition sequence.
                emit_jsonl_produced(jsonl.as_mut(), stage_name.as_str(), &stage_hash);
                emit_jsonl_hashed(jsonl.as_mut(), stage_name.as_str(), &stage_hash, &entry);
            }

            journal.mark_run_outcome(run_id, RunOutcome::Success)?;

            let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            let mut stage_result = build_stage_result(
                stage_name.as_str(),
                &entry,
                used_cache || from_remote,
                duration_ms,
            );
            if from_remote {
                stage_result.source = Some("Remote".to_owned());
            }
            if used_cache && stage.side_effects && stage.on_cache_hit.is_none() {
                stage_result.side_effects_skipped = true;
            }
            emit_jsonl_committed(jsonl.as_mut(), &stage_result);

            // Persist the lockfile entry for this inline stage so
            // `crab status --workflow` reports it as up-to-date.
            // The lockfile may already contain entries from prior
            // runs of other stages; we upsert this one and save.
            let lockfile_path = repo_root.join("crab.lock");
            let mut lockfile = Lockfile::load(&lockfile_path)?;
            upsert_lockfile(&mut lockfile, &stage, &entry, repo_root, BTreeMap::new())?;
            lockfile.save(&lockfile_path)?;

            stage_result
        }
        Err(err) => {
            // Best-effort outcome flag; whether the journal's
            // transition actually committed depends on the executor's
            // own error-path recording.
            let _ = journal.mark_run_outcome(run_id, RunOutcome::Failure);
            emit_jsonl_failed(jsonl.as_mut(), stage_name.as_str(), &stage_hash, &err);
            return Err(err);
        }
    };

    emit_result(jsonl.as_mut(), &result, mode);
    Ok(())
}

/// Bundle of everything the lockfile I/O layer needs to know to do
/// split-or-single persistence for a workflow run.
///
/// Built once in `run_with_yaml` and threaded unchanged through the
/// DAG and single-stage paths. Cheap to clone — the provenance map
/// is already tiny (one entry per stage) and the workflow file list
/// is at most a handful of paths.
struct LockfileContext {
    mode: LockfileMode,
    workflow_files: Vec<PathBuf>,
    provenance: StageProvenance,
}

impl LockfileContext {
    /// Load the merged in-memory lockfile view for this context.
    fn load(&self, repo_root: &Path) -> Result<Lockfile> {
        lockfile_split::load_lockfiles(repo_root, &self.workflow_files, self.mode)
            .map_err(Into::into)
    }

    /// Persist the merged lockfile back to disk, partitioning into
    /// per-file lockfiles when [`LockfileMode::Split`] is active.
    fn save(&self, repo_root: &Path, lockfile: &Lockfile) -> Result<()> {
        lockfile_split::save_lockfiles(
            repo_root,
            &self.workflow_files,
            &self.provenance,
            lockfile,
            self.mode,
        )
        .map_err(Into::into)
    }
}

/// Dispatcher for `crab.yaml`-backed modes. Parses every
/// discovered yaml once and routes to either single-stage-from-
/// yaml (when the user passed a positional stage name) or full-DAG
/// mode. When recursive discovery returns multiple yamls they are
/// merged into a single [`Workflow`] with nested stage names
/// prefixed per R17.
async fn run_with_yaml(
    args: &RunArgs,
    repo_root: &Path,
    yaml_paths: &[PathBuf],
    mode: OutputMode,
    config: &Config,
    options: RunInvocationOptions,
) -> Result<()> {
    // Disallow inline-flag combinations that don't make sense in
    // yaml mode — users who mix `--deps`/`--outs` with a yaml-
    // driven run are almost certainly confused about which mode
    // they want.
    if !args.env.is_empty() || args.empty_env || args.timeout.is_some() || args.hermetic {
        return Err(CrabError::Configuration {
            key: "env/timeout/hermetic overrides are only valid in inline \
                  single-stage mode; declare them in crab.yaml"
                .into(),
            origin: "cli".into(),
        });
    }

    // Single-yaml fast path: read + parse directly so the error
    // surface (line numbers, path) matches the pre-recursive
    // behavior. Multi-yaml recursive mode goes through the merger.
    // In both cases we capture per-stage provenance so the
    // split-lockfile layer knows which lockfile each stage belongs
    // to. For single-yaml repos provenance is trivial (every stage
    // came from the one file) and the lookup stays free.
    let (workflow, provenance) = if yaml_paths.len() == 1 {
        let path = &yaml_paths[0];
        let yaml_text = std::fs::read_to_string(path).map_err(CrabError::Io)?;
        let wf = yaml::parse_at(path, &yaml_text)?;
        let mut prov: StageProvenance = std::collections::BTreeMap::new();
        for name in wf.stages.keys() {
            prov.insert(name.clone(), path.clone());
        }
        (wf, prov)
    } else {
        crate::workflow::discover::parse_all_with_provenance(repo_root, yaml_paths)?
    };
    let graph = Graph::build(&workflow.stages)?;

    let lockfile_mode = match config.workflow.lockfile {
        crate::core::config::WorkflowLockfile::Single => LockfileMode::Single,
        crate::core::config::WorkflowLockfile::Split => LockfileMode::Split,
    };
    let lock_ctx = LockfileContext {
        mode: lockfile_mode,
        workflow_files: yaml_paths.to_vec(),
        provenance,
    };

    if args.dry_run {
        emit_dag_plan(args, repo_root, &workflow, &graph, mode)?;
        return Ok(());
    }

    ensure_workflow_ignored(repo_root)?;

    #[cfg(feature = "watch")]
    if args.watch {
        let target = watch_target(args)?;
        return run_watch(
            args, repo_root, mode, config, &workflow, &graph, &lock_ctx, target, options,
        )
        .await;
    }

    run_dag(
        args, repo_root, mode, config, &workflow, &graph, &lock_ctx, options,
    )
    .await
}

/// `crab run --validate` path. Parses `crab.yaml`, runs all
/// semantic checks, and reports all errors as a structured JSON
/// array. Exits 0 on valid, 2 on any error.
fn run_validate(repo_root: &Path, yaml_paths: &[PathBuf]) {
    let mut errors: Vec<serde_json::Value> = Vec::new();

    // Layer 1+2: YAML syntax + schema validation (deny_unknown_fields).
    let workflow = if yaml_paths.len() == 1 {
        let path = &yaml_paths[0];
        let yaml_text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                errors.push(serde_json::json!({
                    "kind": "IoError",
                    "path": path.display().to_string(),
                    "message": e.to_string(),
                }));
                emit_validate_json(&errors);
                std::process::exit(2);
            }
        };
        match yaml::parse_at(path, &yaml_text) {
            Ok(w) => w,
            Err(e) => {
                let error = CrabError::from(e);
                errors.push(yaml_error_to_json(&error));
                // Even on parse failure, try to report what we can.
                // But we can't do semantic checks without a parsed workflow.
                emit_validate_json(&errors);
                std::process::exit(2);
            }
        }
    } else {
        match crate::workflow::discover::parse_all(repo_root, yaml_paths) {
            Ok(w) => w,
            Err(e) => {
                errors.push(yaml_error_to_json(&CrabError::from(e)));
                emit_validate_json(&errors);
                std::process::exit(2);
            }
        }
    };

    // Layer 3: Semantic validation — collects ALL errors.
    let semantic_errors = yaml::validate_semantics(&workflow);
    for err in semantic_errors {
        let error = CrabError::from(err);
        errors.push(yaml_error_to_json(&error));
    }

    // Layer 3b: Graph-level checks (duplicate outs, cycles).
    if let Err(e) = Graph::build(&workflow.stages) {
        let error = CrabError::from(e);
        errors.push(yaml_error_to_json(&error));
    }

    if errors.is_empty() {
        // Build stage listing with expanded stage details.
        let total = workflow.stages.len();
        let expanded_count = workflow
            .stages
            .keys()
            .filter(|name| name.is_expanded())
            .count();

        let stage_names: Vec<&str> = workflow.stages.keys().map(StageName::as_str).collect();

        let mut result = serde_json::json!({
            "valid": true,
            "stages": stage_names,
            "total_stages": total,
        });

        if expanded_count > 0 {
            let expanded_names: Vec<&str> = workflow
                .stages
                .keys()
                .filter(|n| n.is_expanded())
                .map(StageName::as_str)
                .collect();
            result["expanded_stages"] = serde_json::json!(expanded_names);
            result["expanded_count"] = serde_json::json!(expanded_count);
        }

        emit_validate_json(&result);
    } else {
        emit_validate_json(&errors);
        std::process::exit(2);
    }
}

fn emit_validate_json<T: Serialize>(data: T) {
    emit_json("workflow.validate", "1.0", data);
}

/// Convert a [`CrabError`] into a structured JSON value for
/// `--validate` output.
fn yaml_error_to_json(err: &CrabError) -> serde_json::Value {
    match err {
        CrabError::WorkflowParse { path, source } => {
            let mut obj = serde_json::json!({
                "kind": "WorkflowYamlUnknownKey",
                "message": source.to_string(),
            });
            if !path.as_os_str().is_empty() {
                obj["path"] = serde_json::Value::String(path.display().to_string());
            }
            if let Some(loc) = source.location() {
                obj["line"] = serde_json::Value::Number(loc.line().into());
                obj["column"] = serde_json::Value::Number(loc.column().into());
            }
            obj
        }
        CrabError::WorkflowCycle { stages } => {
            serde_json::json!({
                "kind": "WorkflowCycle",
                "stages": stages,
                "message": err.to_string(),
            })
        }
        CrabError::WorkflowStageNameInvalid { name, reason } => {
            serde_json::json!({
                "kind": "WorkflowStageNameInvalid",
                "name": name,
                "reason": reason,
                "message": err.to_string(),
            })
        }
        CrabError::WorkflowDuplicateOutput {
            first,
            second,
            path,
        } => {
            serde_json::json!({
                "kind": "WorkflowDuplicateOutput",
                "first": first,
                "second": second,
                "path": path.display().to_string(),
                "message": err.to_string(),
            })
        }
        CrabError::Configuration { key, origin } => {
            serde_json::json!({
                "kind": "WorkflowValidationError",
                "field": key,
                "value": origin,
                "message": err.to_string(),
            })
        }
        CrabError::WorkflowValidationError {
            field,
            value,
            expected,
        } => {
            serde_json::json!({
                "kind": "WorkflowValidationError",
                "field": field,
                "value": value,
                "expected": expected,
                "message": err.to_string(),
            })
        }
        CrabError::WorkflowSelfLoop { stage, path } => {
            serde_json::json!({
                "kind": "WorkflowCycle",
                "stage": stage,
                "path": path.display().to_string(),
                "message": err.to_string(),
            })
        }
        CrabError::WorkflowTemplateUndefined { key, field, stage } => {
            serde_json::json!({
                "kind": "WorkflowTemplateUndefined",
                "key": key,
                "field": field,
                "stage": stage,
                "message": err.to_string(),
            })
        }
        _ => {
            serde_json::json!({
                "kind": "WorkflowValidationError",
                "message": err.to_string(),
            })
        }
    }
}

/// Single-stage from yaml: `crab run <stage>`. Runs just the
/// named stage, consulting the lockfile for producer outs rather
/// than re-running the whole upstream chain. This is the
/// "re-run this one thing with what I've got" path; phase 3's
/// full DAG pathway handles "run deps first".
async fn run_yaml_single_stage(
    args: &RunArgs,
    repo_root: &Path,
    mode: OutputMode,
    config: &Config,
    workflow: &Workflow,
    lock_ctx: &LockfileContext,
    stage_name_str: &str,
    options: RunInvocationOptions,
) -> Result<()> {
    let stage_name = StageName::parse(stage_name_str)?;
    let stage = workflow
        .stages
        .get(&stage_name)
        .ok_or_else(|| CrabError::Configuration {
            key: format!("stage '{stage_name_str}' not found in crab.yaml"),
            origin: "cli".into(),
        })?;

    let workflow_root = repo_root.join(".crab").join("workflow");
    let cache_root = repo_root.join(".crab").join("cache");
    crate::workflow::cache::probe_cache_writable(&cache_root);
    let lockfile = lock_ctx.load(repo_root)?;

    // Lock and journal setup mirrors inline single-stage.
    let lock_timeout = compute_lock_timeout(args, config);
    let _scheduler_lock = SchedulerLock::acquire(&workflow_root, lock_timeout)?;

    let run_id = Uuid::now_v7();
    sweep_orphans(&workflow_root, repo_root, &stage.outs)?;

    let metrics = Arc::new(Metrics::new());
    scan_prior_journals(&workflow_root, run_id, args, metrics.as_ref())?;

    let journal_path = workflow_root
        .join("runs")
        .join(run_id.to_string())
        .join("journal.db");
    let journal = Journal::open(&journal_path)?;
    journal.insert_run_start(run_id, env!("CARGO_PKG_VERSION"), &host_fingerprint())?;

    let run_state = RunState::new();
    let executor_cfg = build_executor_cfg(
        &workflow_root,
        &cache_root,
        config,
        Some(repo_root.to_path_buf()),
        args.no_run_cache || args.force,
        args.no_commit,
        options,
    );

    // JSONL stream for `--jsonl` mode in single-stage-from-yaml.
    let mut jsonl = if mode == OutputMode::Jsonl {
        Some(JsonlStream::new(
            WORKFLOW_STAGE_EVENT_STREAM_SCHEMA,
            WORKFLOW_SCHEMA_VERSION,
            std::io::stdout(),
        ))
    } else {
        None
    };

    if args.interactive {
        let cache_hit = preview_stage_cache_hit(
            stage,
            &stage_name,
            repo_root,
            &workflow.params,
            &run_state,
            Some(&lockfile),
            &executor_cfg,
            args,
        )
        .unwrap_or(false);
        if !cache_hit && !confirm_interactive_stage(&stage_name)? {
            journal.mark_run_outcome(run_id, RunOutcome::Success)?;
            emit_not_started(jsonl.as_mut(), &stage_name, "interactive_skip", mode);
            return Ok(());
        }
    }

    let outcome = execute_one_stage_from_yaml_with_jsonl(
        stage,
        &stage_name,
        repo_root,
        &workflow.params,
        &run_state,
        Some(&lockfile),
        &executor_cfg,
        &journal,
        run_id,
        args,
        jsonl.as_mut(),
    )
    .await;

    match outcome {
        Ok((entry, used_cache, duration_ms, params)) => {
            journal.mark_run_outcome(run_id, RunOutcome::Success)?;
            let mut result =
                build_stage_result(stage_name.as_str(), &entry, used_cache, duration_ms);
            if used_cache && stage.side_effects && stage.on_cache_hit.is_none() {
                result.side_effects_skipped = true;
            }
            // Also persist the lockfile entry for this stage — a
            // single-stage yaml run is still a run, and downstream
            // consumers rely on the lockfile to pick up the new
            // hashes.
            let mut lockfile = lockfile;
            upsert_lockfile(&mut lockfile, stage, &entry, repo_root, params)?;
            let yaml_stages: BTreeSet<StageName> = workflow.stages.keys().cloned().collect();
            prune_and_save_lockfile_via_ctx(&mut lockfile, &yaml_stages, lock_ctx, repo_root)?;
            emit_result(jsonl.as_mut(), &result, mode);
            Ok(())
        }
        Err(err) => {
            let _ = journal.mark_run_outcome(run_id, RunOutcome::Failure);
            Err(err)
        }
    }
}

/// No-args DAG mode: walk every stage in topological order,
/// executing stale stages and skipping cache hits. Honors
/// `--keep-going` / `--ignore-errors` per design §"DAG partial
/// success".
///
/// Structured-output wiring: when `--jsonl` is active the walker
/// opens a single [`JsonlStream`] and emits `workflow.stage.*`
/// events per stage (started, cache_checked, produced, hashed,
/// committed or failed) alongside `workflow.stage.not_started`
/// events for stages the scheduler skips due to partial-failure
/// policy. The final `result` line carries a `workflow.run` summary
/// with the same payload `--json` emits (see [`WorkflowRunSummary`]).
#[instrument(name = "workflow.run", skip_all, fields(stages = workflow.stages.len()))]
async fn run_dag(
    args: &RunArgs,
    repo_root: &Path,
    mode: OutputMode,
    config: &Config,
    workflow: &Workflow,
    graph: &Graph,
    lock_ctx: &LockfileContext,
    options: RunInvocationOptions,
) -> Result<()> {
    let workflow_root = repo_root.join(".crab").join("workflow");
    let cache_root = repo_root.join(".crab").join("cache");
    crate::workflow::cache::probe_cache_writable(&cache_root);
    let mut lockfile = lock_ctx.load(repo_root)?;

    // Acquire scheduler lock once for the whole DAG — design's
    // concurrency model is "one `crab run` per repo".
    let lock_timeout = compute_lock_timeout(args, config);
    let _scheduler_lock = SchedulerLock::acquire(&workflow_root, lock_timeout)?;

    let run_id = Uuid::now_v7();
    // Sweep orphan sidecars across every declared out path so a
    // prior crashed run doesn't leave artifacts that poison this
    // one. Each stage's sweep is scoped to its out's parent dir.
    for stage in workflow.stages.values() {
        sweep_orphans(&workflow_root, repo_root, &stage.outs)?;
    }

    let metrics = Arc::new(Metrics::new());
    scan_prior_journals(&workflow_root, run_id, args, metrics.as_ref())?;

    let journal_path = workflow_root
        .join("runs")
        .join(run_id.to_string())
        .join("journal.db");
    let journal = Journal::open(&journal_path)?;
    journal.insert_run_start(run_id, env!("CARGO_PKG_VERSION"), &host_fingerprint())?;

    // Resume plan: for each stage, decide Execute / Skip / Resume
    // based on the journal + filesystem state.
    let cli_flags = CliFlags {
        resume_trust_outputs: args.resume_trust_outputs,
        force: args.force,
    };
    let fs_state_getter = |_: &StageName| FsState::default();
    let plan: DagResumeReport = walk_dag(graph, &journal, run_id, cli_flags, &fs_state_getter)?;

    // Apply --workflow / --stages filters to restrict which stages
    // are executed. Filtered-out stages are treated as pre-committed
    // (skipped) so the scheduler never dispatches them.
    let stage_filter = filter_stages(args, workflow, graph)?;

    let executor_cfg = build_executor_cfg(
        &workflow_root,
        &cache_root,
        config,
        Some(repo_root.to_path_buf()),
        args.no_run_cache || args.force,
        args.no_commit,
        options,
    );

    // Open the JSONL stream up-front so every stage event lands on
    // the same writer. The stream's umbrella schema is the stage-
    // event schema used in single-stage mode; individual events
    // overwrite it per-line so consumers can dispatch on `schema`.
    let jsonl = if mode == OutputMode::Jsonl {
        Some(JsonlStream::new(
            WORKFLOW_STAGE_EVENT_STREAM_SCHEMA,
            WORKFLOW_SCHEMA_VERSION,
            std::io::stdout(),
        ))
    } else {
        None
    };

    let started_at = Instant::now();
    let mut run_state = RunState::new();
    let mut failed: BTreeSet<StageName> = BTreeSet::new();
    let mut not_started: BTreeSet<StageName> = BTreeSet::new();
    let mut succeeded: BTreeSet<StageName> = BTreeSet::new();
    let mut stage_results: Vec<WorkflowStageResult> = Vec::new();
    let mut force_downstream_stages: BTreeSet<StageName> = BTreeSet::new();

    // `--ignore-errors` implies `--keep-going`: if the user wants
    // to try every remaining stage, they're already opting into
    // non-aborting behavior on failure.
    let keep_going = args.keep_going || args.ignore_errors;

    // Resolve effective parallelism from CLI, config, and system.
    let mut parallelism = crate::workflow::scheduler::resolve_parallelism(
        args.parallelism,
        config.workflow.parallelism,
    );
    if args.interactive {
        parallelism = 1;
    }

    // Determine which stages are pre-committed (cache hits from
    // the resume plan) so the scheduler seeds them as "done".
    let mut skip_stages: BTreeSet<StageName> = BTreeSet::new();
    for stage_name in &plan.order {
        let action = plan.action_for(stage_name).unwrap_or(StageAction::Execute);
        if let StageAction::Skip { cached: true } = action {
            debug!(stage = %stage_name, "dag: skipping cache-committed stage");
            succeeded.insert(stage_name.clone());
            skip_stages.insert(stage_name.clone());
        }
        // If a stage filter is active and this stage is NOT in the
        // allowed set, skip it as well.
        if let Some(ref allowed) = stage_filter
            && !allowed.contains(stage_name)
        {
            debug!(stage = %stage_name, "dag: skipping filtered-out stage");
            skip_stages.insert(stage_name.clone());
        }
    }

    // Build the parallel scheduler.
    let mut scheduler = crate::workflow::scheduler::DagScheduler::new(
        graph,
        crate::workflow::scheduler::SchedulerConfig {
            parallelism,
            keep_going,
            ignore_errors: args.ignore_errors,
        },
        &skip_stages,
    );

    // Mark pre-committed stages as succeeded in the scheduler so
    // their consumers become eligible.
    for stage_name in &skip_stages {
        scheduler.handle_success(stage_name, graph, &Resources::default());
    }

    // Build a resource map for the scheduler to check availability.
    let stage_resources: BTreeMap<StageName, Resources> = workflow
        .stages
        .iter()
        .map(|(name, stage)| (name.clone(), stage.resources.clone()))
        .collect();

    // Channel for collecting results from spawned stage tasks.
    let (result_tx, mut result_rx) =
        mpsc::channel::<ParallelStageResult>(parallelism as usize * 2 + 1);

    // Shared JSONL stream for concurrent event emission.
    let jsonl_shared: Option<Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>> =
        jsonl.map(|j| Arc::new(tokio::sync::Mutex::new(j)));

    // The scheduler loop: dispatch ready stages, collect results.
    let semaphore = scheduler.semaphore().clone();
    let mut first_error: Option<CrabError> = None;

    loop {
        // Dispatch as many ready stages as we can (bounded by semaphore).
        while let Some(stage_name) = scheduler.next_ready(&stage_resources) {
            // Check if this stage is blocked (marked not_started by
            // a prior failure's cascade).
            if scheduler.is_blocked(&stage_name) {
                emit_not_started_shared(
                    jsonl_shared.as_ref(),
                    &stage_name,
                    "upstream_failed",
                    mode,
                )
                .await;
                scheduler.not_started.insert(stage_name.clone());
                not_started.insert(stage_name);
                continue;
            }

            let Some(stage) = workflow.stages.get(&stage_name) else {
                warn!(stage = %stage_name, "dag: plan references unknown stage");
                continue;
            };

            // Frozen stages are skipped entirely — no hash, no cache
            // check, no execution, no lockfile update. `--force` does
            // NOT override frozen. Downstream stages proceed using
            // whatever outputs exist on disk.
            if stage.frozen {
                info!(stage = %stage_name, "dag: stage frozen, skipping");
                emit_not_started_shared(jsonl_shared.as_ref(), &stage_name, "frozen", mode).await;
                succeeded.insert(stage_name.clone());
                scheduler.handle_success(
                    &stage_name,
                    graph,
                    &stage_resources
                        .get(&stage_name)
                        .cloned()
                        .unwrap_or_default(),
                );
                continue;
            }

            // Condition evaluation: when a stage has a `condition:`
            // field and it evaluates to false, treat it as frozen
            // for this run.
            if let Some(ref condition) = stage.condition
                && !condition.evaluate(repo_root)
            {
                info!(stage = %stage_name, "dag: condition false, skipping");
                emit_not_started_shared(
                    jsonl_shared.as_ref(),
                    &stage_name,
                    "condition_false",
                    mode,
                )
                .await;
                succeeded.insert(stage_name.clone());
                scheduler.handle_success(
                    &stage_name,
                    graph,
                    &stage_resources
                        .get(&stage_name)
                        .cloned()
                        .unwrap_or_default(),
                );
                continue;
            }

            // Precompute the stage hash once for prompt decisions,
            // JSONL planning events, and the spawned task metadata.
            let resolver = StageOutResolver::new(&run_state, Some(&lockfile), repo_root);
            let dep_hashes_preview = resolve_dep_hashes_with_wdir_allow_missing_remote_aliases(
                &stage_name,
                &stage.deps,
                repo_root,
                &resolver,
                stage.wdir.as_deref(),
                args.allow_missing,
                Some(&lockfile),
                Some(&stage_name),
                &executor_cfg.remote_aliases,
            )
            .ok();
            let params_preview = resolve_stage_param_values_with_wdir(
                repo_root,
                &workflow.params,
                &stage.params,
                stage_name.as_str(),
                stage.wdir.as_deref(),
            )
            .ok();
            let stage_hash_preview = dep_hashes_preview.as_ref().map(|deps| {
                let resolved = ResolvedStage {
                    stage: stage.clone(),
                    dep_hashes: deps.clone(),
                    params: params_preview.clone().unwrap_or_default(),
                    env: stage.env.clone(),
                    cmd: stage.cmd.clone(),
                    outs: stage.outs.clone(),
                };
                compute_stage_hash(&resolved)
            });
            let cache_hit_preview = stage_hash_preview
                .as_ref()
                .and_then(|hash| {
                    if stage_execution_cache_lookup_enabled(stage, args)
                        && !force_downstream_stages.contains(&stage_name)
                    {
                        read_local(&cache_root, hash).ok().flatten()
                    } else {
                        None
                    }
                })
                .is_some()
                && !args.force;

            if args.interactive && !cache_hit_preview && !confirm_interactive_stage(&stage_name)? {
                info!(stage = %stage_name, "dag: stage skipped by interactive prompt");
                emit_not_started_shared(
                    jsonl_shared.as_ref(),
                    &stage_name,
                    "interactive_skip",
                    mode,
                )
                .await;
                not_started.insert(stage_name.clone());
                succeeded.insert(stage_name.clone());
                scheduler.handle_success(
                    &stage_name,
                    graph,
                    &stage_resources
                        .get(&stage_name)
                        .cloned()
                        .unwrap_or_default(),
                );
                continue;
            }

            // Try to acquire a semaphore permit. If none available,
            // push the stage back and break to wait for results.
            let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                scheduler.ready_queue.push(std::cmp::Reverse(stage_name));
                break;
            };

            scheduler.mark_dispatched(&stage_name);
            metrics.inc_workflow_stages_total();
            let force_stage_downstream =
                args.force_downstream && force_downstream_stages.contains(&stage_name);

            // Acquire resources for this stage.
            let stage_res = stage_resources
                .get(&stage_name)
                .cloned()
                .unwrap_or_default();
            scheduler.acquire_resources(&stage_res);

            // Spawn the stage execution as a tokio task.
            let tx = result_tx.clone();
            let stage_clone = stage.clone();
            let stage_name_clone = stage_name.clone();
            let repo_root_owned = repo_root.to_path_buf();
            let lockfile_clone = lockfile.clone();
            let mut executor_cfg_clone = executor_cfg.clone();
            if force_stage_downstream {
                executor_cfg_clone.no_run_cache = true;
            }
            let journal_path_clone = journal_path.clone();
            let args_force = args.force;
            let args_allow_missing = args.allow_missing;
            let args_pull = args.pull;
            let cache_root_clone = cache_root.clone();
            let param_files_clone = workflow.params.clone();
            let jsonl_shared_clone = jsonl_shared.clone();

            // Emit started + cache_checked events before spawning.
            if let Some(hash) = stage_hash_preview.as_ref() {
                emit_started_shared(
                    jsonl_shared.as_ref(),
                    stage_name.as_str(),
                    hash,
                    &started_at,
                )
                .await;
                emit_cache_checked_shared(
                    jsonl_shared.as_ref(),
                    stage_name.as_str(),
                    hash,
                    cache_hit_preview,
                    if cache_hit_preview { "local" } else { "none" },
                    &started_at,
                )
                .await;
            }

            let hash_preview_for_task = stage_hash_preview;
            let stage_name_for_span = stage_name.as_str().to_owned();
            let stage_hash_for_span = stage_hash_preview
                .as_ref()
                .map(StageHash::as_hex)
                .unwrap_or_default();

            tokio::spawn(async move {
                let stage_span = info_span!(
                    "workflow.stage",
                    stage = %stage_name_for_span,
                    stage_hash = %stage_hash_for_span,
                    source = tracing::field::Empty,
                    duration_ms = tracing::field::Empty,
                );
                let outcome = execute_stage_parallel(
                    &stage_clone,
                    &stage_name_clone,
                    &repo_root_owned,
                    &lockfile_clone,
                    &executor_cfg_clone,
                    &journal_path_clone,
                    run_id,
                    args_force,
                    &cache_root_clone,
                    &param_files_clone,
                    args_allow_missing,
                    args_pull,
                    jsonl_shared_clone,
                    started_at,
                )
                .instrument(stage_span.clone())
                .await;

                // Record source and duration on the span.
                match &outcome {
                    Ok((_, used_cache, duration_ms, _)) => {
                        stage_span
                            .record("source", if *used_cache { "Cache" } else { "Execution" });
                        stage_span.record("duration_ms", *duration_ms);
                    }
                    Err(_) => {
                        stage_span.record("source", "Failed");
                    }
                }

                let result = ParallelStageResult {
                    stage_name: stage_name_clone,
                    stage_hash_preview: hash_preview_for_task,
                    outcome,
                };

                // Release the semaphore permit before sending.
                drop(permit);
                let _ = tx.send(result).await;
            });
        }

        // If nothing is in flight and nothing is ready, we're done.
        if !scheduler.has_in_flight() && scheduler.is_done() {
            break;
        }

        // Wait for the next result from a spawned task.
        let Some(result) = result_rx.recv().await else {
            // Channel closed unexpectedly — all senders dropped.
            break;
        };

        let stage_name = result.stage_name;
        let stage_hash_preview = result.stage_hash_preview;

        match result.outcome {
            Ok((entry, used_cache, duration_ms, params)) => {
                let stage = workflow.stages.get(&stage_name);

                // Update lockfile in memory.
                if let Some(s) = stage {
                    let _ = upsert_lockfile(&mut lockfile, s, &entry, repo_root, params);
                }

                let mut stage_result =
                    build_stage_result(stage_name.as_str(), &entry, used_cache, duration_ms);
                mark_side_effects_skipped(stage, &mut stage_result);

                // Emit produced + hashed + committed events.
                if !used_cache {
                    emit_produced_shared(
                        jsonl_shared.as_ref(),
                        stage_name.as_str(),
                        &entry.stage_hash,
                        &started_at,
                    )
                    .await;
                    emit_hashed_shared(
                        jsonl_shared.as_ref(),
                        stage_name.as_str(),
                        &entry.stage_hash,
                        &entry,
                        &started_at,
                    )
                    .await;
                }
                emit_committed_shared(jsonl_shared.as_ref(), &stage_result, &started_at).await;

                stage_results.push(stage_result);
                succeeded.insert(stage_name.clone());
                run_state.insert(stage_name.clone(), entry);
                if args.force_downstream && !used_cache {
                    force_downstream_stages.extend(transitive_consumers(&stage_name, graph));
                }
                scheduler.handle_success(
                    &stage_name,
                    graph,
                    &stage_resources
                        .get(&stage_name)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            Err(err) => {
                warn!(
                    stage = %stage_name,
                    error = %err,
                    "dag: stage failed"
                );
                let hash = stage_hash_preview.unwrap_or_else(StageHash::zero);
                emit_failed_shared(
                    jsonl_shared.as_ref(),
                    stage_name.as_str(),
                    &hash,
                    &err,
                    &started_at,
                )
                .await;

                failed.insert(stage_name.clone());
                scheduler.handle_failure(
                    &stage_name,
                    graph,
                    &stage_resources
                        .get(&stage_name)
                        .cloned()
                        .unwrap_or_default(),
                );

                // Emit not_started for newly-blocked stages.
                for blocked in &scheduler.not_started {
                    if !not_started.contains(blocked) {
                        emit_not_started_shared(
                            jsonl_shared.as_ref(),
                            blocked,
                            "upstream_failed",
                            mode,
                        )
                        .await;
                    }
                }
                not_started = scheduler.not_started.clone();

                if first_error.is_none() {
                    first_error = Some(err);
                }

                if !keep_going && !scheduler.has_in_flight() {
                    break;
                }
            }
        }
    }

    // Wait for any remaining in-flight stages to complete (drain).
    drop(result_tx);
    while let Some(result) = result_rx.recv().await {
        let stage_name = result.stage_name;
        match result.outcome {
            Ok((entry, used_cache, duration_ms, params)) => {
                let stage = workflow.stages.get(&stage_name);
                if let Some(s) = stage {
                    let _ = upsert_lockfile(&mut lockfile, s, &entry, repo_root, params);
                }
                let mut stage_result =
                    build_stage_result(stage_name.as_str(), &entry, used_cache, duration_ms);
                mark_side_effects_skipped(stage, &mut stage_result);
                if !used_cache {
                    emit_produced_shared(
                        jsonl_shared.as_ref(),
                        stage_name.as_str(),
                        &entry.stage_hash,
                        &started_at,
                    )
                    .await;
                    emit_hashed_shared(
                        jsonl_shared.as_ref(),
                        stage_name.as_str(),
                        &entry.stage_hash,
                        &entry,
                        &started_at,
                    )
                    .await;
                }
                emit_committed_shared(jsonl_shared.as_ref(), &stage_result, &started_at).await;
                stage_results.push(stage_result);
                succeeded.insert(stage_name.clone());
                run_state.insert(stage_name, entry);
            }
            Err(err) => {
                warn!(stage = %stage_name, error = %err, "dag: in-flight stage failed during drain");
                let hash = result.stage_hash_preview.unwrap_or_else(StageHash::zero);
                emit_failed_shared(
                    jsonl_shared.as_ref(),
                    stage_name.as_str(),
                    &hash,
                    &err,
                    &started_at,
                )
                .await;
                failed.insert(stage_name);
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }

    // Final outcome: success if nothing failed, else failure.
    let outcome = if failed.is_empty() {
        RunOutcome::Success
    } else {
        RunOutcome::Failure
    };
    journal.mark_run_outcome(run_id, outcome)?;
    metrics.inc_workflow_runs_total();

    // Orphan pruning per R5: stages in the lockfile that are no
    // longer in `crab.yaml` are removed with a warn. We save the
    // lockfile once at the end of the DAG, atomically.
    let yaml_stages: BTreeSet<StageName> = workflow.stages.keys().cloned().collect();
    {
        let _lockfile_span = info_span!("workflow.lockfile.write").entered();
        prune_and_save_lockfile_via_ctx(&mut lockfile, &yaml_stages, lock_ctx, repo_root)?;
    }

    let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

    // Recover the JSONL stream from the Arc<Mutex> for the summary.
    let mut jsonl_recovered: Option<JsonlStream<std::io::Stdout>> = None;
    if let Some(shared) = jsonl_shared
        && let Ok(inner) = Arc::try_unwrap(shared)
    {
        jsonl_recovered = Some(inner.into_inner());
    }

    emit_dag_summary(
        jsonl_recovered.as_mut(),
        &stage_results,
        &succeeded,
        &failed,
        &not_started,
        duration_ms,
        mode,
    );

    if !failed.is_empty() {
        let first = failed
            .iter()
            .next()
            .map_or_else(|| "<unknown>".to_owned(), |n| n.as_str().to_owned());
        return Err(first_error.unwrap_or_else(|| CrabError::Configuration {
            key: format!(
                "DAG run had {} failed stage(s); first: {first}",
                failed.len()
            ),
            origin: "workflow".into(),
        }));
    }

    Ok(())
}

// --- Parallel scheduler helpers ---

/// Result sent from a spawned stage task back to the scheduler loop.
struct ParallelStageResult {
    stage_name: StageName,
    stage_hash_preview: Option<StageHash>,
    outcome: Result<(StageCacheEntry, bool, u64, BTreeMap<String, String>)>,
}

/// Execute a single stage in a spawned task context. This is the
/// parallel-safe version that doesn't take a mutable JSONL stream
/// reference (events are emitted via the shared Arc<Mutex> from the
/// caller).
///
/// Each task opens its own journal connection to the same SQLite file.
/// WAL mode with `busy_timeout = 5000` handles concurrent writers.
#[expect(
    clippy::too_many_arguments,
    reason = "parallel stage execution needs all context passed in"
)]
async fn execute_stage_parallel(
    stage: &Stage,
    stage_name: &StageName,
    repo_root: &Path,
    lockfile: &Lockfile,
    executor_cfg: &ExecutorConfig,
    journal_path: &Path,
    run_id: Uuid,
    force: bool,
    cache_root: &Path,
    param_files: &[PathBuf],
    allow_missing: bool,
    pull: bool,
    jsonl: Option<Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>>,
    run_started_at: Instant,
) -> Result<(StageCacheEntry, bool, u64, BTreeMap<String, String>)> {
    // Each parallel task opens its own journal connection. SQLite WAL
    // mode with busy_timeout handles concurrent writers.
    let journal = Journal::open(journal_path)?;
    // Validate declared outs before any journal work.
    for out in &stage.outs {
        out.validate(stage_name)?;
    }

    // When --pull is set, attempt to download missing dep files from
    // the remote before resolving hashes.
    if pull {
        try_pull_missing_deps(stage, stage_name, repo_root);
    }

    // Build a fresh RunState for dep resolution. In parallel mode,
    // each task resolves deps against the lockfile and working tree
    // (not the in-memory run state which is only updated after
    // results come back to the scheduler).
    let run_state = RunState::new();
    let resolver = StageOutResolver::new(&run_state, Some(lockfile), repo_root);
    let dep_hashes = resolve_dep_hashes_with_wdir_allow_missing_remote_aliases(
        stage_name,
        &stage.deps,
        repo_root,
        &resolver,
        stage.wdir.as_deref(),
        allow_missing,
        Some(lockfile),
        Some(stage_name),
        &executor_cfg.remote_aliases,
    )?;
    let params = resolve_stage_param_values_with_wdir(
        repo_root,
        param_files,
        &stage.params,
        stage_name.as_str(),
        stage.wdir.as_deref(),
    )?;

    let resolved = ResolvedStage {
        stage: stage.clone(),
        dep_hashes,
        params: params.clone(),
        env: stage.env.clone(),
        cmd: stage.cmd.clone(),
        outs: stage.outs.clone(),
    };
    let stage_hash = compute_stage_hash(&resolved);

    let cache_lookup_enabled = stage.run_cache_lookup_enabled() && !executor_cfg.no_run_cache;
    let cached = if cache_lookup_enabled {
        read_local(cache_root, &stage_hash).ok().flatten()
    } else {
        None
    };
    let cache_hit = cached.is_some() && !force;

    journal.insert_stage_start(run_id, stage_name.as_str())?;
    journal.transition(run_id, stage_name.as_str(), 1, StageState::Resolved, "{}")?;

    let started = Instant::now();

    // Retry loop.
    let policy = stage.retry.clone().unwrap_or_else(RetryPolicy::no_retry);
    let mut attempt: u32 = 1;
    let exec_result = loop {
        let result = run_local(&resolved, executor_cfg, &journal, run_id, attempt)
            .await
            .map_err(CrabError::from);
        match result {
            Ok(entry) => break Ok(entry),
            Err(e) => {
                let kind = classify_failure_kind(&e);
                match retry::should_retry(&policy, &kind, attempt) {
                    RetryDecision::Retry { backoff } => {
                        let (reason, _, _, _) = classify_failure(&e);
                        debug!(
                            stage = %stage_name,
                            attempt,
                            backoff_ms = backoff.as_millis() as u64,
                            "retry: scheduling next attempt"
                        );
                        emit_retry_shared(
                            jsonl.as_ref(),
                            stage_name.as_str(),
                            &stage_hash,
                            attempt,
                            reason,
                            backoff,
                            &run_started_at,
                        )
                        .await;
                        clean_partial_outputs(stage, repo_root);
                        attempt += 1;
                        journal.insert_stage_retry(run_id, stage_name.as_str(), attempt)?;
                        journal.transition(
                            run_id,
                            stage_name.as_str(),
                            attempt,
                            StageState::Resolved,
                            "{}",
                        )?;
                        tokio::time::sleep(backoff).await;
                    }
                    RetryDecision::Exhausted => {
                        break Err(e);
                    }
                }
            }
        }
    };

    match exec_result {
        Ok(entry) => {
            if cache_hit {
                let flags = OverwriteFlags {
                    force,
                    no_overwrite: false,
                };
                materialize_hit_with_flags(stage_name, run_id, &entry, cache_root, flags)?;

                // P7: on_cache_hit hook.
                if stage.side_effects
                    && let Some(hook_cmd) = &stage.on_cache_hit
                {
                    let status = crate::workflow::executor::execute_hook(
                        hook_cmd,
                        &stage.env,
                        executor_cfg.working_dir.as_deref(),
                    )
                    .await?;
                    if !status.success() {
                        let exit_code = status.code().unwrap_or(-1);
                        return Err(CrabError::StageSideEffectHookFailed {
                            stage: stage_name.as_str().to_owned(),
                            exit_code,
                        });
                    }
                }
            }
            let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            Ok((entry, cache_hit, duration_ms, params))
        }
        Err(err) => {
            if attempt > 1 && policy.max_attempts > 1 {
                Err(CrabError::StageRetryExhausted {
                    stage: stage_name.as_str().to_owned(),
                    attempts: attempt,
                })
            } else {
                Err(err)
            }
        }
    }
}

// --- Shared JSONL emission helpers (thread-safe via Arc<Mutex>) ---

async fn emit_started_shared(
    jsonl: Option<&Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>>,
    stage: &str,
    stage_hash: &StageHash,
    started_at: &Instant,
) {
    let Some(shared) = jsonl else { return };
    let mut stream = shared.lock().await;
    let payload = WorkflowStageStarted {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        attempt: 1,
        elapsed_ms: Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Started(&payload));
}

async fn emit_cache_checked_shared(
    jsonl: Option<&Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>>,
    stage: &str,
    stage_hash: &StageHash,
    hit: bool,
    hit_source: &str,
    started_at: &Instant,
) {
    let Some(shared) = jsonl else { return };
    let mut stream = shared.lock().await;
    let payload = WorkflowStageCacheChecked {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        hit,
        hit_source: hit_source.to_owned(),
        elapsed_ms: Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::CacheChecked(&payload));
}

async fn emit_retry_shared(
    jsonl: Option<&Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>>,
    stage: &str,
    stage_hash: &StageHash,
    attempt: u32,
    reason: &str,
    backoff: std::time::Duration,
    started_at: &Instant,
) {
    let Some(shared) = jsonl else { return };
    let mut stream = shared.lock().await;
    let payload = WorkflowStageRetry {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        attempt,
        reason: reason.to_owned(),
        backoff_ms: backoff.as_millis().min(u128::from(u64::MAX)) as u64,
        exhausted: false,
        elapsed_ms: Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
    };
    stream.emit_schema_event(WORKFLOW_STAGE_RETRY_SCHEMA, "event", &payload);
}

async fn emit_produced_shared(
    jsonl: Option<&Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>>,
    stage: &str,
    stage_hash: &StageHash,
    started_at: &Instant,
) {
    let Some(shared) = jsonl else { return };
    let mut stream = shared.lock().await;
    let payload = WorkflowStageProduced {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        exit_code: 0,
        elapsed_ms: Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Produced(&payload));
}

async fn emit_hashed_shared(
    jsonl: Option<&Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>>,
    stage: &str,
    stage_hash: &StageHash,
    entry: &StageCacheEntry,
    started_at: &Instant,
) {
    let Some(shared) = jsonl else { return };
    let mut stream = shared.lock().await;
    let payload = WorkflowStageHashed {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        outs: entry
            .outs
            .iter()
            .map(|o| WorkflowStageOut {
                path: o.path.clone(),
                file_hash: o.file_hash.clone(),
                size: o.size,
            })
            .collect(),
        elapsed_ms: Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Hashed(&payload));
}

async fn emit_committed_shared(
    jsonl: Option<&Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>>,
    result: &WorkflowStageResult,
    started_at: &Instant,
) {
    let Some(shared) = jsonl else { return };
    let mut stream = shared.lock().await;
    let payload = WorkflowStageCommitted {
        stage: result.stage_name.clone(),
        stage_hash: result.stage_hash.clone(),
        duration_ms: result.duration_ms,
        attempts: result.attempts,
        cache_hit: result.cache_hit,
        elapsed_ms: Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Committed(&payload));
}

async fn emit_failed_shared(
    jsonl: Option<&Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>>,
    stage: &str,
    stage_hash: &StageHash,
    err: &CrabError,
    started_at: &Instant,
) {
    let Some(shared) = jsonl else { return };
    let mut stream = shared.lock().await;
    let (reason, exit_code, signal, timed_out) = classify_failure(err);
    let payload = WorkflowStageFailed {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        reason: reason.to_owned(),
        exit_code,
        signal,
        timed_out,
        stderr_tail: None,
        elapsed_ms: Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Failed(&payload));
    stream.emit_error_info(crate::core::output::ErrorInfo::from(err));
}

async fn emit_not_started_shared(
    jsonl: Option<&Arc<tokio::sync::Mutex<JsonlStream<std::io::Stdout>>>>,
    stage: &StageName,
    reason: &str,
    mode: OutputMode,
) {
    match mode {
        OutputMode::Jsonl => {
            let Some(shared) = jsonl else { return };
            let mut stream = shared.lock().await;
            let payload = WorkflowStageNotStarted {
                stage: stage.as_str().to_owned(),
                reason: reason.to_owned(),
            };
            stream.emit_schema_event(WORKFLOW_STAGE_NOT_STARTED_SCHEMA, "event", &payload);
        }
        OutputMode::Json => {
            let payload = WorkflowStageNotStarted {
                stage: stage.as_str().to_owned(),
                reason: reason.to_owned(),
            };
            emit_json(
                WORKFLOW_STAGE_NOT_STARTED_SCHEMA,
                WORKFLOW_SCHEMA_VERSION,
                payload,
            );
        }
        OutputMode::Text => {
            warn!(stage = %stage, reason = %reason, "dag: stage not started");
        }
    }
}

/// Common executor config factory so single-stage-from-yaml and
/// DAG mode stay in sync without a shared hand-built literal.
///
/// `working_dir` anchors child processes to a specific directory.
/// `crab run` always passes `None` (the child inherits the CLI's
/// cwd, which is the repo root). `crab exp run` passes the
/// experiment tmpdir so stage commands resolve relative paths
/// against the throwaway worktree (R23).
fn build_executor_cfg(
    workflow_root: &Path,
    cache_root: &Path,
    config: &Config,
    working_dir: Option<PathBuf>,
    no_run_cache: bool,
    no_commit: bool,
    options: RunInvocationOptions,
) -> ExecutorConfig {
    ExecutorConfig {
        workflow_root: workflow_root.to_path_buf(),
        cache_root: cache_root.to_path_buf(),
        graceful_shutdown: std::time::Duration::from_secs(
            config.workflow.graceful_shutdown_timeout_secs,
        ),
        stderr_tail_bytes: crate::workflow::signals::DEFAULT_STDERR_TAIL_BYTES,
        mirror_child_output: options.mirror_child_output,
        external_kill_path: options.external_kill_path,
        child_started: options.child_started,
        max_outs_per_stage: config.workflow.max_outs_per_stage,
        default_max_out_bytes: Some(config.workflow.max_out_bytes),
        host_fingerprint: host_fingerprint(),
        working_dir,
        metrics: None,
        cache_push: false,
        no_run_cache,
        no_commit,
        remote_store: None,
        remote_prefix: None,
        remote_primary_fallback_store: None,
        remote_primary_fallback_prefix: None,
        remote_artifact_stores: None,
        remote_aliases: workflow_remote_aliases(config),
        min_cache_headroom: config.workflow.min_cache_headroom_bytes(),
    }
}

fn workflow_remote_aliases(config: &Config) -> std::collections::BTreeMap<String, String> {
    config
        .workflow
        .remotes
        .iter()
        .map(|(name, remote)| (name.clone(), remote.url.clone()))
        .collect()
}

struct WorkflowRemote {
    store: Arc<crate::workflow::WorkflowStore>,
    prefix: String,
    primary_fallback: Option<WorkflowPrimaryFallback>,
    artifact_stores: Option<RemoteArtifactStores>,
}

struct WorkflowPrimaryFallback {
    store: Arc<crate::workflow::WorkflowStore>,
    prefix: String,
}

#[derive(Clone, Copy)]
struct WorkflowRemoteCandidate<'a> {
    store: &'a Arc<crate::workflow::WorkflowStore>,
    prefix: &'a str,
    source: &'static str,
}

#[derive(Clone, Copy, Default)]
struct CacheOnlyRemote<'a> {
    selected: Option<WorkflowRemoteCandidate<'a>>,
    primary_fallback: Option<WorkflowRemoteCandidate<'a>>,
}

impl<'a> CacheOnlyRemote<'a> {
    fn candidates(self) -> [Option<WorkflowRemoteCandidate<'a>>; 2] {
        [self.selected, self.primary_fallback]
    }
}

struct CacheOnlyContext<'a> {
    args: &'a RunArgs,
    mode: OutputMode,
    cache_lookup_enabled: bool,
    artifact_stores: Option<&'a RemoteArtifactStores>,
    remote: CacheOnlyRemote<'a>,
    cache_root: &'a Path,
    working_dir: Option<&'a Path>,
}

/// Build a remote store for workflow cache operations.
///
/// Returns `None` when no crab remote is configured or credentials
/// are unavailable — the caller falls through to local-only mode.
async fn try_build_workflow_remote(
    repo_root: &Path,
    config: &Config,
    cache_push: bool,
) -> Option<WorkflowRemote> {
    let Ok(url_str) = crate::cmd::workflow::read_crab_remote_url(repo_root) else {
        return None;
    };
    let Ok(crab_url) = crate::git::url::CrabUrl::parse(&url_str) else {
        return None;
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let resolver = crate::replication::StoreResolver::new(config, &crab_url, &cancel);
    let artifact_stores =
        crate::cmd::workflow::build_workflow_artifact_stores(config, &cancel).await;
    let artifact_stores = (!artifact_stores.is_empty()).then_some(artifact_stores);

    if cache_push {
        let selection = resolver.write_store("workflow-cache-push").await.ok()?;
        return Some(WorkflowRemote {
            store: Arc::new(crate::workflow::WorkflowStore::from_storage(
                selection.store.into_storage(),
            )),
            prefix: selection.router.repo_prefix().to_owned(),
            primary_fallback: None,
            artifact_stores,
        });
    }

    let selection = resolver.read_store("workflow-cache-pull").await.ok()?;
    let primary_fallback = if matches!(
        &selection.source,
        crate::replication::ReadSource::Replica { .. }
    ) {
        let primary = resolver
            .write_store("workflow-cache-pull-primary-fallback")
            .await
            .ok()?;
        Some(WorkflowPrimaryFallback {
            store: Arc::new(crate::workflow::WorkflowStore::from_storage(
                primary.store.into_storage(),
            )),
            prefix: primary.router.repo_prefix().to_owned(),
        })
    } else {
        None
    };

    Some(WorkflowRemote {
        store: Arc::new(crate::workflow::WorkflowStore::from_storage(
            selection.store.into_storage(),
        )),
        prefix: selection.router.repo_prefix().to_owned(),
        primary_fallback,
        artifact_stores,
    })
}

/// Execute one stage defined in `crab.yaml`, resolving its deps
/// via the multi-stage resolver (run-state → lockfile → working
/// tree) and walking the same executor path inline single-stage
/// uses.
///
/// Returns `(cache_entry, used_cache, duration_ms, params)` on success.
#[allow(dead_code)]
#[expect(
    clippy::too_many_arguments,
    reason = "shared helper between single-stage and DAG modes; \
              grouping would force a parameter struct that only \
              lives inside this file"
)]
async fn execute_one_stage_from_yaml(
    stage: &Stage,
    stage_name: &StageName,
    repo_root: &Path,
    param_files: &[PathBuf],
    run_state: &RunState,
    lockfile: Option<&Lockfile>,
    executor_cfg: &ExecutorConfig,
    journal: &Journal,
    run_id: Uuid,
    args: &RunArgs,
) -> Result<(StageCacheEntry, bool, u64, BTreeMap<String, String>)> {
    execute_one_stage_from_yaml_with_jsonl(
        stage,
        stage_name,
        repo_root,
        param_files,
        run_state,
        lockfile,
        executor_cfg,
        journal,
        run_id,
        args,
        None,
    )
    .await
}

/// Inner implementation that optionally emits retry events on a JSONL
/// stream. Separated so callers without a stream don't need to thread
/// `None` through explicitly.
#[expect(
    clippy::too_many_arguments,
    reason = "shared helper between single-stage and DAG modes; \
              grouping would force a parameter struct that only \
              lives inside this file"
)]
async fn execute_one_stage_from_yaml_with_jsonl(
    stage: &Stage,
    stage_name: &StageName,
    repo_root: &Path,
    param_files: &[PathBuf],
    run_state: &RunState,
    lockfile: Option<&Lockfile>,
    executor_cfg: &ExecutorConfig,
    journal: &Journal,
    run_id: Uuid,
    args: &RunArgs,
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
) -> Result<(StageCacheEntry, bool, u64, BTreeMap<String, String>)> {
    // Validate declared outs before any journal work.
    for out in &stage.outs {
        out.validate(stage_name)?;
    }

    // When --pull is set, attempt to download missing dep files from
    // the remote before resolving hashes. This runs before dep
    // resolution so that successfully pulled files are picked up by
    // the normal hash computation.
    if args.pull {
        try_pull_missing_deps(stage, stage_name, repo_root);
    }

    let resolver = StageOutResolver::new(run_state, lockfile, repo_root);
    let dep_hashes = resolve_dep_hashes_with_wdir_allow_missing_remote_aliases(
        stage_name,
        &stage.deps,
        repo_root,
        &resolver,
        stage.wdir.as_deref(),
        args.allow_missing,
        lockfile,
        Some(stage_name),
        &executor_cfg.remote_aliases,
    )?;
    let params = resolve_stage_param_values_with_wdir(
        repo_root,
        param_files,
        &stage.params,
        stage_name.as_str(),
        stage.wdir.as_deref(),
    )?;

    let resolved = ResolvedStage {
        stage: stage.clone(),
        dep_hashes,
        params: params.clone(),
        env: stage.env.clone(),
        cmd: stage.cmd.clone(),
        outs: stage.outs.clone(),
    };
    let stage_hash = compute_stage_hash(&resolved);

    let cached = if stage.run_cache_lookup_enabled() && !executor_cfg.no_run_cache {
        read_local(&executor_cfg.cache_root, &stage_hash)
            .ok()
            .flatten()
    } else {
        None
    };
    let cache_hit = cached.is_some() && !args.force;

    journal.insert_stage_start(run_id, stage_name.as_str())?;
    journal.transition(run_id, stage_name.as_str(), 1, StageState::Resolved, "{}")?;

    let started = Instant::now();

    // Retry loop: wraps run_local with the stage's retry policy.
    let policy = stage.retry.clone().unwrap_or_else(RetryPolicy::no_retry);
    let mut attempt: u32 = 1;
    let mut jsonl = jsonl;
    let exec_result = loop {
        let result = run_local(&resolved, executor_cfg, journal, run_id, attempt)
            .await
            .map_err(CrabError::from);
        match result {
            Ok(entry) => break Ok(entry),
            Err(e) => {
                let kind = classify_failure_kind(&e);
                match retry::should_retry(&policy, &kind, attempt) {
                    RetryDecision::Retry { backoff } => {
                        let (reason, _, _, _) = classify_failure(&e);
                        debug!(
                            stage = %stage_name,
                            attempt,
                            backoff_ms = backoff.as_millis() as u64,
                            "retry: scheduling next attempt"
                        );
                        // Emit retry event on the JSONL stream.
                        emit_jsonl_retry(
                            jsonl.as_deref_mut(),
                            stage_name.as_str(),
                            &stage_hash,
                            attempt,
                            reason,
                            backoff,
                        );
                        // Clean partial outputs from the failed attempt.
                        clean_partial_outputs(stage, repo_root);
                        // Record the retry in the journal as a new attempt row.
                        attempt += 1;
                        journal.insert_stage_retry(run_id, stage_name.as_str(), attempt)?;
                        journal.transition(
                            run_id,
                            stage_name.as_str(),
                            attempt,
                            StageState::Resolved,
                            "{}",
                        )?;
                        tokio::time::sleep(backoff).await;
                    }
                    RetryDecision::Exhausted => {
                        break Err(e);
                    }
                }
            }
        }
    };

    match exec_result {
        Ok(entry) => {
            if cache_hit {
                materialize_hit(stage_name, run_id, &entry, &executor_cfg.cache_root, args)?;

                // P7: on_cache_hit hook execution. Fires only on
                // cache hits, never on the miss path or during
                // retry attempts.
                if stage.side_effects {
                    if let Some(hook_cmd) = &stage.on_cache_hit {
                        let status = crate::workflow::executor::execute_hook(
                            hook_cmd,
                            &stage.env,
                            executor_cfg.working_dir.as_deref(),
                        )
                        .await?;
                        if !status.success() {
                            let exit_code = status.code().unwrap_or(-1);
                            return Err(CrabError::StageSideEffectHookFailed {
                                stage: stage_name.as_str().to_owned(),
                                exit_code,
                            });
                        }
                    } else {
                        warn!(
                            stage = %stage_name,
                            "stage has side_effects: true but no on_cache_hit hook; \
                             side effects were skipped on this cache hit"
                        );
                    }
                }
            }
            let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            Ok((entry, cache_hit, duration_ms, params))
        }
        Err(err) => {
            // If retries were exhausted, wrap the error to surface
            // the attempt count and exhaustion flag.
            if attempt > 1 && policy.max_attempts > 1 {
                Err(CrabError::StageRetryExhausted {
                    stage: stage_name.as_str().to_owned(),
                    attempts: attempt,
                })
            } else {
                Err(err)
            }
        }
    }
}

/// Translate a stage's cache entry into lockfile records and
/// upsert. Deps recorded here come from the resolver — we hash the
/// live working-tree + run-state view, same data the stage_hash
/// was computed over.
fn upsert_lockfile(
    lockfile: &mut Lockfile,
    stage: &Stage,
    entry: &StageCacheEntry,
    repo_root: &Path,
    params: BTreeMap<String, String>,
) -> Result<()> {
    let mut deps: Vec<LockedDep> = Vec::new();
    for dep in &stage.deps {
        if let Dep::Path(p) = dep {
            // When the stage has a wdir, resolve relative dep paths
            // against repo_root/wdir/ and store them as wdir/dep in
            // the lockfile (repo-relative).
            let (abs, lockfile_path) = if p.is_absolute() {
                (p.clone(), p.clone())
            } else if let Some(ref wdir) = stage.wdir {
                (repo_root.join(wdir).join(p), wdir.join(p))
            } else {
                (repo_root.join(p), p.clone())
            };
            // Best-effort: if the file vanished between exec and
            // lockfile-write we drop it. A fully correct answer
            // would carry the resolver's computed hashes through —
            // that lives in task 3.14 once structured DAG output
            // ships. For this wiring step we re-read rather than
            // thread the hash-map back out.
            let Ok(meta) = std::fs::metadata(&abs) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&abs) else {
                continue;
            };
            let h = blake3::hash(&bytes);
            let mut hash_bytes = [0u8; 32];
            hash_bytes.copy_from_slice(h.as_bytes());
            deps.push(LockedDep {
                path: lockfile_path,
                hash: hash_bytes,
                size: meta.len(),
            });
        }
    }

    // Resolve env values for the lockfile so explain-miss can diff
    // against them later.
    let env: BTreeMap<String, String> = match &stage.env {
        EnvSpec::Allowlist(vars) => vars
            .iter()
            .filter_map(|v| std::env::var(v).ok().map(|val| (v.clone(), val)))
            .collect(),
        EnvSpec::Inherit | EnvSpec::Empty => BTreeMap::new(),
    };

    lockfile.upsert(entry, deps, params, env)?;
    Ok(())
}

/// Split-aware variant: prune stages not in `yaml_stages`, then
/// persist via the [`LockfileContext`]. Used by both the DAG and
/// single-stage yaml paths so `[workflow] lockfile = "split"` users
/// get the same atomic write + prune semantics as single-file users.
fn prune_and_save_lockfile_via_ctx(
    lockfile: &mut Lockfile,
    yaml_stages: &BTreeSet<StageName>,
    lock_ctx: &LockfileContext,
    repo_root: &Path,
) -> Result<()> {
    let pruned = lockfile.prune_stages_not_in(yaml_stages);
    if !pruned.is_empty() {
        let names: Vec<String> = pruned.iter().map(|n| n.as_str().to_owned()).collect();
        warn!(
            pruned = ?names,
            "workflow lockfile: pruned stages no longer present in crab.yaml"
        );
    }
    lock_ctx.save(repo_root, lockfile)
}

/// Structured not-started event for partial-DAG reporting. Text
/// mode falls through to a `warn!` so interactive users see it.
/// In `--jsonl` mode the event rides the open stream so it stays in
/// sequence with per-stage events; `--json` mode falls back to a
/// single-line envelope since there's no stream to target.
#[allow(dead_code)]
fn emit_not_started(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    stage: &StageName,
    reason: &str,
    mode: OutputMode,
) {
    match mode {
        OutputMode::Jsonl => {
            let Some(stream) = jsonl else { return };
            let payload = WorkflowStageNotStarted {
                stage: stage.as_str().to_owned(),
                reason: reason.to_owned(),
            };
            // `workflow.stage.not_started` isn't part of the
            // `WorkflowStageEvent` enum (it has no stage_hash) so
            // we route through the general schema-override path.
            stream.emit_schema_event(WORKFLOW_STAGE_NOT_STARTED_SCHEMA, "event", &payload);
        }
        OutputMode::Json => {
            let payload = WorkflowStageNotStarted {
                stage: stage.as_str().to_owned(),
                reason: reason.to_owned(),
            };
            emit_json(
                WORKFLOW_STAGE_NOT_STARTED_SCHEMA,
                WORKFLOW_SCHEMA_VERSION,
                payload,
            );
        }
        OutputMode::Text => {
            warn!(stage = %stage, reason = %reason, "dag: stage not started");
        }
    }
}

/// Terminal DAG summary. Emits the `workflow.run` envelope in
/// `--json` mode (one envelope per process) and as the final
/// `result` event on the JSONL stream in `--jsonl` mode. Text mode
/// falls back to an `info!` line with the three bin counts.
fn emit_dag_summary(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    results: &[WorkflowStageResult],
    succeeded: &BTreeSet<StageName>,
    failed: &BTreeSet<StageName>,
    not_started: &BTreeSet<StageName>,
    duration_ms: u64,
    mode: OutputMode,
) {
    match mode {
        OutputMode::Json => {
            let summary = build_run_summary(results, succeeded, failed, not_started, duration_ms);
            emit_json(WORKFLOW_RUN_SCHEMA, WORKFLOW_SCHEMA_VERSION, summary);
        }
        OutputMode::Jsonl => {
            // On the JSONL stream the terminal `result` line
            // carries the same `workflow.run` payload `--json`
            // uses. The stream's umbrella schema stays at
            // `workflow.stage.event`; the run-summary schema
            // rides in on the line's `schema` field.
            let Some(stream) = jsonl else { return };
            let summary = build_run_summary(results, succeeded, failed, not_started, duration_ms);
            stream.emit_schema_event(WORKFLOW_RUN_SCHEMA, "result", &summary);
        }
        OutputMode::Text => {
            info!(
                succeeded = succeeded.len(),
                failed = failed.len(),
                not_started = not_started.len(),
                duration_ms = duration_ms,
                "workflow dag run complete"
            );
        }
    }
}

/// Build the terminal [`WorkflowRunSummary`] payload from the
/// scheduler's per-stage bins. Stage-name lists are drawn from the
/// [`BTreeSet`]s so the JSON output is deterministic across runs.
fn build_run_summary(
    results: &[WorkflowStageResult],
    succeeded: &BTreeSet<StageName>,
    failed: &BTreeSet<StageName>,
    not_started: &BTreeSet<StageName>,
    duration_ms: u64,
) -> WorkflowRunSummary {
    WorkflowRunSummary {
        succeeded: succeeded.iter().map(|n| n.as_str().to_owned()).collect(),
        failed: failed.iter().map(|n| n.as_str().to_owned()).collect(),
        not_started: not_started.iter().map(|n| n.as_str().to_owned()).collect(),
        stages: results
            .iter()
            .map(|r| WorkflowStageResult {
                stage_name: r.stage_name.clone(),
                stage_hash: r.stage_hash.clone(),
                cache_hit: r.cache_hit,
                duration_ms: r.duration_ms,
                outs: r
                    .outs
                    .iter()
                    .map(|o| WorkflowStageOut {
                        path: o.path.clone(),
                        file_hash: o.file_hash.clone(),
                        size: o.size,
                    })
                    .collect(),
                attempts: r.attempts,
                side_effects_skipped: r.side_effects_skipped,
                source: r.source.clone(),
            })
            .collect(),
        duration_ms,
    }
}

/// `--cache-only` branch: no execution, no journal. Either materialize
/// cached outs or exit 3 via `StageCacheMiss`.
///
/// When a local cache miss occurs but a remote store is configured,
/// attempts a remote pull before giving up. Remote hits are
/// materialized and reported with `source: "Remote"`.
async fn cache_only_path(
    stage_name: &StageName,
    stage_hash: &StageHash,
    outs: &[Out],
    cached: Option<&StageCacheEntry>,
    ctx: CacheOnlyContext<'_>,
) -> Result<()> {
    if !ctx.cache_lookup_enabled {
        return Err(CrabError::StageCacheMiss {
            stage: stage_name.as_str().to_owned(),
            reason: "stage is always changed and cannot be replayed from run cache".to_owned(),
        });
    }

    let (entry, from_remote) = if let Some(e) = cached {
        (e.clone(), false)
    } else {
        // Local miss — try remote before giving up.
        let mut tried_remote = false;
        let mut last_error = None;

        for candidate in ctx.remote.candidates().into_iter().flatten() {
            tried_remote = true;
            match crate::workflow::cache::pull_remote_with_artifact_stores(
                candidate.store,
                candidate.prefix,
                ctx.artifact_stores,
                stage_hash,
                ctx.cache_root,
                ctx.working_dir,
            )
            .await
            {
                Ok(Some(remote_entry)) => {
                    info!(
                        stage = %stage_name,
                        stage_hash = %stage_hash,
                        remote_source = candidate.source,
                        "cache-only: remote hit"
                    );
                    return cache_only_emit_hit(
                        stage_name,
                        outs,
                        ctx.args,
                        ctx.mode,
                        ctx.cache_root,
                        remote_entry,
                        true,
                    );
                }
                Ok(None) => {
                    debug!(
                        stage = %stage_name,
                        stage_hash = %stage_hash,
                        remote_source = candidate.source,
                        "cache-only: remote miss"
                    );
                }
                Err(e) => {
                    debug!(
                        stage = %stage_name,
                        remote_source = candidate.source,
                        error = %e,
                        "cache-only: remote pull failed"
                    );
                    last_error = Some(e.to_string());
                }
            }
        }

        if tried_remote {
            return Err(CrabError::StageCacheMiss {
                stage: stage_name.as_str().to_owned(),
                reason: last_error.map_or_else(
                    || format!("no local or remote cache entry for {stage_hash}"),
                    |error| format!("no local cache entry for {stage_hash}; remote error: {error}"),
                ),
            });
        }

        return Err(CrabError::StageCacheMiss {
            stage: stage_name.as_str().to_owned(),
            reason: format!("no local cache entry for {stage_hash}"),
        });
    };

    cache_only_emit_hit(
        stage_name,
        outs,
        ctx.args,
        ctx.mode,
        ctx.cache_root,
        entry,
        from_remote,
    )
}

fn cache_only_emit_hit(
    stage_name: &StageName,
    outs: &[Out],
    args: &RunArgs,
    mode: OutputMode,
    cache_root: &Path,
    entry: StageCacheEntry,
    from_remote: bool,
) -> Result<()> {
    // Even without a journal, we still run the materialization through
    // the same sidecar path — it's what makes the hit atomic.
    let run_id = Uuid::now_v7();
    materialize_hit(stage_name, run_id, &entry, cache_root, args)?;
    // Touch the declared outs list so unused warnings don't fire when
    // the executor doesn't enumerate outs (they're already in `entry`).
    let _ = outs;

    let duration_ms = 0;
    let mut result = build_stage_result(stage_name.as_str(), &entry, true, duration_ms);
    if from_remote {
        result.source = Some("Remote".to_owned());
    }
    emit_result(None, &result, mode);
    Ok(())
}

/// Materialize every out from a cache entry, guarded by the overwrite
/// policy. Skips no-op writes (existing file already matches) and
/// respects `--no-overwrite` / `--force`.
fn materialize_hit(
    stage_name: &StageName,
    run_id: Uuid,
    entry: &StageCacheEntry,
    cache_root: &Path,
    args: &RunArgs,
) -> Result<()> {
    let flags = OverwriteFlags {
        force: args.force,
        no_overwrite: args.no_overwrite,
    };
    materialize_hit_with_flags(stage_name, run_id, entry, cache_root, flags)
}

fn materialize_hit_with_flags(
    stage_name: &StageName,
    run_id: Uuid,
    entry: &StageCacheEntry,
    cache_root: &Path,
    flags: OverwriteFlags,
) -> Result<()> {
    for out in cached_artifacts(entry) {
        match out.kind {
            OutKind::Directory => {
                // Directory out: materialize from the tree manifest.
                if let Some(ref manifest) = out.tree_manifest {
                    crate::workflow::materialize::materialize_directory(
                        &out.path, manifest, cache_root, run_id,
                    )?;
                } else {
                    // Legacy cache entry without tree manifest — the
                    // directory should still be on disk from the
                    // original run. Nothing to do.
                    debug!(
                        stage = %stage_name,
                        path = %out.path.display(),
                        "cache hit: directory out has no tree manifest, skipping materialization"
                    );
                }
            }
            OutKind::File | OutKind::Stdout => {
                let current = inspect_existing(&out.path);
                let decision =
                    overwrite_policy(stage_name.as_str(), &out.path, out, current.as_ref(), flags)?;

                if decision == OverwriteDecision::NoOp {
                    debug!(
                        stage = %stage_name,
                        path = %out.path.display(),
                        "cache hit: on-disk file already matches, skipping write"
                    );
                    continue;
                }

                let bytes = cached_file_bytes(stage_name, cache_root, out)?;
                write_atomic(&out.path, &bytes, run_id, out.mode)?;
            }
        }
    }
    Ok(())
}

fn cached_file_bytes(
    stage_name: &StageName,
    cache_root: &Path,
    out: &crate::workflow::cache::CachedOut,
) -> Result<Vec<u8>> {
    if let Some(bytes) = read_local_xorb(cache_root, &out.file_hash)? {
        return Ok(bytes);
    }

    std::fs::read(&out.path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CrabError::StageCacheMiss {
                stage: stage_name.as_str().to_owned(),
                reason: format!(
                    "cache entry references {} but neither the output nor local cache bytes are present",
                    out.path.display()
                ),
            }
        } else {
            CrabError::Io(e)
        }
    })
}

/// Inspect a file that sits at a declared out path. Used by the
/// overwrite policy to decide whether the cache write is a no-op or a
/// clobber. Git-dirty detection is deferred to task 3.x; for now we
/// compute the on-disk hash only.
fn inspect_existing(path: &Path) -> Option<CurrentFile> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let hash = format!("b3:{}", blake3::hash(&bytes).to_hex());
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    };
    #[cfg(not(unix))]
    let mode = 0o644_u32;
    Some(CurrentFile {
        file_hash: hash,
        mode,
        git_dirty: false,
    })
}

/// Resolve declared deps to `(repo_relative_path, blake3)`. Mirrors
/// the whole-file hashing pattern used by the clean filter in
/// `git/clean.rs`: a fresh `blake3::Hasher`, fed via `std::io::copy`.
///
/// When `wdir` is `Some`, relative dep paths are resolved against
/// `repo_root/wdir/` and stored with the `wdir/` prefix in the key.
#[cfg(test)]
fn resolve_dep_hashes(
    stage: &StageName,
    deps: &[Dep],
    repo_root: &Path,
) -> Result<std::collections::BTreeMap<String, [u8; 32]>> {
    resolve_dep_hashes_with_aliases(stage, deps, repo_root, &std::collections::BTreeMap::new())
}

fn resolve_dep_hashes_with_aliases(
    stage: &StageName,
    deps: &[Dep],
    repo_root: &Path,
    remote_aliases: &std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, [u8; 32]>> {
    resolve_dep_hashes_local(stage, deps, repo_root, None, remote_aliases)
}

fn resolve_dep_hashes_local(
    stage: &StageName,
    deps: &[Dep],
    repo_root: &Path,
    wdir: Option<&Path>,
    remote_aliases: &std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, [u8; 32]>> {
    let mut out = std::collections::BTreeMap::new();
    for dep in deps {
        match dep {
            Dep::Path(p) => {
                let abs = if p.is_absolute() {
                    p.clone()
                } else if let Some(w) = wdir {
                    repo_root.join(w).join(p)
                } else {
                    repo_root.join(p)
                };
                let meta = std::fs::metadata(&abs).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        CrabError::StageDepMissing {
                            stage: stage.as_str().to_owned(),
                            path: p.clone(),
                        }
                    } else {
                        CrabError::Io(e)
                    }
                })?;
                if meta.is_dir() {
                    // Directory dep: hash via the tree-manifest
                    // hasher (R16). Non-regular entries inside the
                    // tree surface as `StageDepMalformed` with the
                    // declaring stage attached.
                    let tree =
                        crate::workflow::hasher::hash_directory(&abs, true).map_err(|e| {
                            let e = CrabError::from(e);
                            match &e {
                                CrabError::Io(io_err)
                                    if matches!(
                                        io_err.kind(),
                                        std::io::ErrorKind::InvalidInput
                                    ) =>
                                {
                                    let msg = io_err.to_string();
                                    CrabError::StageDepMalformed {
                                        stage: stage.as_str().to_owned(),
                                        path: p.clone(),
                                        reason: if msg.contains("symlink") {
                                            "directory contains a symlink entry"
                                        } else {
                                            "directory contains a non-regular entry"
                                        },
                                    }
                                }
                                _ => e,
                            }
                        })?;
                    let key = local_repo_relative_dep_key(p, wdir);
                    out.insert(key, tree.hash);
                    continue;
                }
                if !meta.is_file() {
                    return Err(CrabError::StageDepMalformed {
                        stage: stage.as_str().to_owned(),
                        path: p.clone(),
                        reason: "phase 1 only supports regular-file deps",
                    });
                }

                let mut hasher = blake3::Hasher::new();
                let mut file = std::fs::File::open(&abs).map_err(CrabError::Io)?;
                std::io::copy(&mut file, &mut hasher).map_err(CrabError::Io)?;
                let digest: [u8; 32] = *hasher.finalize().as_bytes();

                let key = local_repo_relative_dep_key(p, wdir);
                out.insert(key, digest);
            }
            Dep::Url { .. } => {
                let Some((key, digest)) = dep.url_hash_with_remote_aliases(remote_aliases)? else {
                    continue;
                };
                out.insert(key, digest);
            }
            // Remote / cross-repo / OCI / stage-output dep resolvers
            // that cannot be reduced to an explicit digest surface a
            // clear error rather than being silently mis-hashed.
            Dep::CrabRef { .. }
            | Dep::GitRef { .. }
            | Dep::OciImage { .. }
            | Dep::StageOut { .. } => {
                return Err(CrabError::StageRemoteExecutionUnsupported);
            }
        }
    }
    Ok(out)
}

/// Build the repo-relative key for a dep path in the single-stage
/// runner. When `wdir` is set and the path is relative, prepend
/// `wdir/` so the lockfile stores an unambiguous repo-relative path.
fn local_repo_relative_dep_key(p: &Path, wdir: Option<&Path>) -> String {
    if p.is_absolute() {
        return p.to_string_lossy().into_owned();
    }
    match wdir {
        Some(w) => {
            let joined = w.join(p);
            joined.to_string_lossy().into_owned()
        }
        None => p.to_string_lossy().into_owned(),
    }
}

/// Effective scheduler-lock timeout.
///
/// Priority:
/// 1. `--no-wait` forces `Duration::ZERO` (fail fast on contention).
/// 2. `--lock-timeout N` overrides the config default.
/// 3. Falls through to `[workflow] lock_timeout_secs` (600 per R24).
fn compute_lock_timeout(args: &RunArgs, config: &Config) -> std::time::Duration {
    if args.no_wait {
        return std::time::Duration::ZERO;
    }
    let secs = args
        .lock_timeout
        .unwrap_or(config.workflow.lock_timeout_secs);
    std::time::Duration::from_secs(secs)
}

fn confirm_interactive_stage(stage_name: &StageName) -> Result<bool> {
    let mut stderr = io::stderr();
    write!(stderr, "Run stage '{}'? [y/N] ", stage_name.as_str()).map_err(CrabError::Io)?;
    stderr.flush().map_err(CrabError::Io)?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer).map_err(CrabError::Io)?;
    Ok(interactive_answer_is_yes(&answer))
}

fn interactive_answer_is_yes(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

#[expect(
    clippy::too_many_arguments,
    reason = "cache preview needs the same context as stage execution"
)]
fn preview_stage_cache_hit(
    stage: &Stage,
    stage_name: &StageName,
    repo_root: &Path,
    param_files: &[PathBuf],
    run_state: &RunState,
    lockfile: Option<&Lockfile>,
    executor_cfg: &ExecutorConfig,
    args: &RunArgs,
) -> Result<bool> {
    let resolver = StageOutResolver::new(run_state, lockfile, repo_root);
    let dep_hashes = resolve_dep_hashes_with_wdir_allow_missing_remote_aliases(
        stage_name,
        &stage.deps,
        repo_root,
        &resolver,
        stage.wdir.as_deref(),
        args.allow_missing,
        lockfile,
        Some(stage_name),
        &executor_cfg.remote_aliases,
    )?;
    let params = resolve_stage_param_values_with_wdir(
        repo_root,
        param_files,
        &stage.params,
        stage_name.as_str(),
        stage.wdir.as_deref(),
    )?;
    let resolved = ResolvedStage {
        stage: stage.clone(),
        dep_hashes,
        params,
        env: stage.env.clone(),
        cmd: stage.cmd.clone(),
        outs: stage.outs.clone(),
    };
    let stage_hash = compute_stage_hash(&resolved);
    Ok(stage.run_cache_lookup_enabled()
        && !executor_cfg.no_run_cache
        && !args.force
        && read_local(&executor_cfg.cache_root, &stage_hash)
            .ok()
            .flatten()
            .is_some())
}

/// Effective workflow discovery mode.
///
/// `--recursive` always wins; otherwise the config `[workflow]
/// discover` setting dictates the mode (default `Root`). Mapping
/// the config enum to the workflow enum here keeps `run_in` free
/// of the cross-module import.
fn resolve_discover_mode(
    args: &RunArgs,
    config: &Config,
) -> crate::workflow::discover::DiscoverMode {
    use crate::core::config::WorkflowDiscover;
    use crate::workflow::discover::DiscoverMode;
    if args.recursive || args.all_pipelines {
        return DiscoverMode::Recursive;
    }
    match config.workflow.discover {
        WorkflowDiscover::Root => DiscoverMode::Root,
        WorkflowDiscover::Recursive => DiscoverMode::Recursive,
    }
}

#[cfg(feature = "watch")]
fn watch_target(args: &RunArgs) -> Result<Option<&str>> {
    if args.all_pipelines
        || args.glob
        || args.downstream
        || args.pipeline
        || args.workflow.is_some()
        || args.stages.is_some()
    {
        return Err(CrabError::Configuration {
            key: "`crab run --watch` accepts at most one exact positional stage target".to_owned(),
            origin: "cli".into(),
        });
    }
    match args.cmd.len() {
        0 => Ok(None),
        1 => Ok(Some(args.cmd[0].as_str())),
        _ => Err(CrabError::Configuration {
            key: format!(
                "`crab run --watch` accepts at most one positional stage target, got {}",
                args.cmd.len()
            ),
            origin: "cli".into(),
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetClosure {
    Upstream,
    Exact,
    Downstream,
    Pipeline,
}

impl TargetClosure {
    fn from_args(args: &RunArgs) -> Self {
        if args.single_item {
            Self::Exact
        } else if args.downstream {
            Self::Downstream
        } else if args.pipeline {
            Self::Pipeline
        } else {
            Self::Upstream
        }
    }
}

/// Filter a workflow's stages based on target-selection flags.
///
/// When `--workflow <name>` is set, only stages belonging to that named
/// workflow are included (plus their upstream deps from other workflows
/// via transitive closure through the graph).
///
/// When `--stages <glob>` is set, only stages whose names match the glob
/// pattern are included (plus their upstream deps via transitive closure).
///
/// Positional yaml targets match DVC's default behavior: exact stage names
/// include their upstream closure unless `--single-item`, `--downstream`, or
/// `--pipeline` changes the closure direction. `--glob` treats those
/// positional targets as glob patterns over stage names.
///
/// Filters combine by intersection before graph closure. Returns `None`
/// when no filtering is active, which means execute every discovered stage.
pub fn filter_stages(
    args: &RunArgs,
    workflow: &Workflow,
    graph: &Graph,
) -> Result<Option<BTreeSet<StageName>>> {
    if args.all_pipelines {
        return Ok(None);
    }

    let workflow_filter = args.workflow.as_deref();
    let stages_glob = args.stages.as_deref();
    let target_filters = target_stage_filters(args, workflow)?;

    if workflow_filter.is_none() && stages_glob.is_none() && target_filters.is_empty() {
        return Ok(None);
    }

    let mut filters: Vec<BTreeSet<StageName>> = Vec::new();

    if let Some(wf_name) = workflow_filter {
        let mut workflow_matched = BTreeSet::new();
        for (stage_name, membership) in &workflow.workflow_membership {
            if membership == wf_name {
                workflow_matched.insert(stage_name.clone());
            }
        }
        if workflow_matched.is_empty() && !workflow.workflow_membership.is_empty() {
            return Err(CrabError::Configuration {
                key: format!("--workflow '{wf_name}' matched no stages"),
                origin: "cli".into(),
            });
        }
        if workflow.workflow_membership.is_empty() {
            return Err(CrabError::Configuration {
                key: format!("--workflow '{wf_name}' requires a `workflows:` key in crab.yaml"),
                origin: "cli".into(),
            });
        }
        filters.push(workflow_matched);
    }

    if let Some(pattern) = stages_glob {
        filters.push(match_stage_glob(workflow, pattern, "--stages")?);
    }
    filters.extend(target_filters);

    let mut iter = filters.into_iter();
    let Some(mut seed_stages) = iter.next() else {
        return Ok(None);
    };
    for filter in iter {
        seed_stages = seed_stages.intersection(&filter).cloned().collect();
        if seed_stages.is_empty() {
            return Err(CrabError::Configuration {
                key: "stage target filters have no stages in common".to_owned(),
                origin: "cli".into(),
            });
        }
    }

    Ok(Some(expand_stage_selection(
        seed_stages,
        graph,
        TargetClosure::from_args(args),
    )))
}

fn target_stage_filters(args: &RunArgs, workflow: &Workflow) -> Result<Vec<BTreeSet<StageName>>> {
    if args.cmd.is_empty() {
        return Ok(Vec::new());
    }

    let mut targets = BTreeSet::new();
    for raw in &args.cmd {
        if args.glob {
            let pattern = stage_target_glob_pattern(raw)?;
            targets.extend(match_stage_glob(workflow, &pattern, "--glob target")?);
            continue;
        }
        let name = stage_target_name(raw)?;
        if !workflow.stages.contains_key(&name) {
            return Err(CrabError::Configuration {
                key: format!("stage target '{raw}' not found in crab.yaml"),
                origin: "cli".into(),
            });
        }
        targets.insert(name);
    }

    Ok(vec![targets])
}

pub(crate) fn stage_target_name(raw: &str) -> Result<StageName> {
    if let Some((prefix, leaf)) = path_qualified_stage_target(raw)? {
        let leaf = StageName::parse(leaf)?;
        return Ok(StageName::from_joined(&prefix, &leaf)?);
    }
    Ok(StageName::parse_effective(raw)?)
}

fn stage_target_glob_pattern(raw: &str) -> Result<String> {
    if let Some((prefix, leaf)) = path_qualified_stage_target(raw)? {
        if prefix.is_empty() {
            return Ok(leaf.to_owned());
        }
        return Ok(format!("{prefix}.{leaf}"));
    }
    Ok(raw.to_owned())
}

fn path_qualified_stage_target(raw: &str) -> Result<Option<(String, &str)>> {
    let Some((path, leaf)) = raw.rsplit_once(':') else {
        return Ok(None);
    };
    if path.is_empty() || leaf.is_empty() {
        return Ok(None);
    }
    let Some(prefix) = workflow_path_stage_prefix(path)? else {
        return Ok(None);
    };
    Ok(Some((prefix, leaf)))
}

fn workflow_path_stage_prefix(path: &str) -> Result<Option<String>> {
    let path = Path::new(path);
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };

    let mut components =
        repo_relative_path_components(path.parent().unwrap_or_else(|| Path::new("")))?;
    match file_name {
        "crab.yaml" | "dvc.yaml" => {}
        name => {
            let Some(stem) = name.strip_suffix(".workflow.yaml") else {
                return Ok(None);
            };
            if stem.is_empty() {
                return Ok(None);
            }
            components.push(stem.to_owned());
        }
    }

    for component in &components {
        StageName::parse(component)?;
    }
    Ok(Some(components.join(".")))
}

fn repo_relative_path_components(path: &Path) -> Result<Vec<String>> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err(CrabError::Configuration {
                        key: "path-qualified stage target".to_owned(),
                        origin: "workflow path must be valid UTF-8".to_owned(),
                    });
                };
                components.push(part.to_owned());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(CrabError::Configuration {
                    key: "path-qualified stage target".to_owned(),
                    origin: "workflow path must be repo-relative".to_owned(),
                });
            }
        }
    }
    Ok(components)
}

fn match_stage_glob(
    workflow: &Workflow,
    pattern: &str,
    origin: &str,
) -> Result<BTreeSet<StageName>> {
    let glob = globset::GlobBuilder::new(pattern)
        .case_insensitive(false)
        .literal_separator(false)
        .build()
        .map_err(|e| CrabError::Configuration {
            key: format!("{origin} glob pattern '{pattern}'"),
            origin: e.to_string(),
        })?
        .compile_matcher();

    let mut matched = BTreeSet::new();
    for stage_name in workflow.stages.keys() {
        if glob.is_match(stage_name.as_str()) {
            matched.insert(stage_name.clone());
        }
    }

    if matched.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("{origin} '{pattern}' matched no stages"),
            origin: "cli".into(),
        });
    }

    Ok(matched)
}

fn expand_stage_selection(
    seed_stages: BTreeSet<StageName>,
    graph: &Graph,
    closure: TargetClosure,
) -> BTreeSet<StageName> {
    match closure {
        TargetClosure::Exact => seed_stages,
        TargetClosure::Upstream => {
            expand_with_neighbors(seed_stages, |stage| graph.producers_of(stage))
        }
        TargetClosure::Downstream => {
            expand_with_neighbors(seed_stages, |stage| graph.consumers_of(stage))
        }
        TargetClosure::Pipeline => expand_with_neighbors(seed_stages, |stage| {
            let mut neighbors = graph.producers_of(stage);
            neighbors.extend(graph.consumers_of(stage));
            neighbors
        }),
    }
}

fn expand_with_neighbors<F>(
    seed_stages: BTreeSet<StageName>,
    mut neighbors: F,
) -> BTreeSet<StageName>
where
    F: FnMut(&StageName) -> Vec<StageName>,
{
    let mut result = seed_stages.clone();
    let mut queue: Vec<StageName> = seed_stages.into_iter().collect();
    while let Some(stage) = queue.pop() {
        for neighbor in neighbors(&stage) {
            if result.insert(neighbor.clone()) {
                queue.push(neighbor);
            }
        }
    }
    result
}

fn stage_cache_lookup_enabled(stage: &Stage, args: &RunArgs) -> bool {
    stage.run_cache_lookup_enabled() && !args.no_run_cache
}

fn stage_execution_cache_lookup_enabled(stage: &Stage, args: &RunArgs) -> bool {
    stage_cache_lookup_enabled(stage, args) && !args.force
}

fn transitive_consumers(root: &StageName, graph: &Graph) -> BTreeSet<StageName> {
    let mut visited = BTreeSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root.clone());

    while let Some(stage) = queue.pop_front() {
        for consumer in graph.consumers_of(&stage) {
            if visited.insert(consumer.clone()) {
                queue.push_back(consumer);
            }
        }
    }

    visited
}

fn build_env_spec(args: &RunArgs) -> EnvSpec {
    if args.empty_env {
        EnvSpec::Empty
    } else if !args.env.is_empty() {
        EnvSpec::Allowlist(args.env.clone())
    } else {
        EnvSpec::Inherit
    }
}

fn cli_dep(path: &PathBuf) -> Dep {
    let value = path.to_string_lossy();
    if is_url_dep(value.as_ref()) {
        Dep::Url {
            url: value.into_owned(),
            digest: None,
        }
    } else {
        Dep::Path(path.clone())
    }
}

fn build_outs(paths: &[PathBuf]) -> Vec<Out> {
    paths
        .iter()
        .map(|p| {
            let path_str = p.to_string_lossy();
            if path_str.ends_with('/') {
                // Trailing slash signals a directory out (P4).
                // Strip the trailing slash for the canonical path.
                let trimmed = path_str.trim_end_matches('/');
                Out::new(PathBuf::from(trimmed), OutKind::Directory)
            } else {
                Out::new(p.clone(), OutKind::File)
            }
        })
        .collect()
}

fn parse_timeout(raw: Option<&str>) -> Result<Option<std::time::Duration>> {
    let Some(raw) = raw else { return Ok(None) };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let (num_str, multiplier) = if let Some(n) = raw.strip_suffix('h') {
        (n, 3600_u64)
    } else if let Some(n) = raw.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = raw.strip_suffix('s') {
        (n, 1)
    } else {
        (raw, 1)
    };
    let value: u64 = num_str.parse().map_err(|_| CrabError::Configuration {
        key: format!("invalid --timeout value: {raw}"),
        origin: "cli".into(),
    })?;
    Ok(Some(std::time::Duration::from_secs(value * multiplier)))
}

fn host_fingerprint() -> String {
    format!(
        "{}-{}-crab-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
    )
}

fn build_stage_result(
    stage_name: &str,
    entry: &StageCacheEntry,
    cache_hit: bool,
    duration_ms: u64,
) -> WorkflowStageResult {
    let source = if cache_hit {
        Some("Local".to_owned())
    } else {
        Some("Execution".to_owned())
    };
    WorkflowStageResult {
        stage_name: stage_name.to_owned(),
        stage_hash: entry.stage_hash.as_hex(),
        cache_hit,
        duration_ms,
        outs: entry
            .outs
            .iter()
            .map(|o| WorkflowStageOut {
                path: o.path.clone(),
                file_hash: o.file_hash.clone(),
                size: o.size,
            })
            .collect(),
        attempts: entry.attempts,
        side_effects_skipped: false,
        source,
    }
}

fn mark_side_effects_skipped(stage: Option<&Stage>, result: &mut WorkflowStageResult) {
    let Some(stage) = stage else { return };
    if result.cache_hit && stage.side_effects && stage.on_cache_hit.is_none() {
        result.side_effects_skipped = true;
        warn!(
            stage = %result.stage_name,
            "stage has side_effects: true but no on_cache_hit hook; \
             side effects were skipped on this cache hit"
        );
    }
}

fn emit_plan(resolved: &ResolvedStage, stage_hash: &StageHash, cache_hit: bool, mode: OutputMode) {
    let plan = WorkflowPlan {
        stage_name: resolved.stage.name.as_str().to_owned(),
        stage_hash: stage_hash.as_hex(),
        cache_hit,
        deps: resolved
            .dep_hashes
            .iter()
            .map(|(path, hash)| PlanDep {
                path: PathBuf::from(path),
                file_hash: format!("b3:{}", hex_lower(hash)),
                size: std::fs::metadata(path).map_or(0, |m| m.len()),
            })
            .collect(),
        outs: resolved
            .outs
            .iter()
            .map(|o| PlanOut {
                path: o.path.clone(),
                kind: o.kind.as_str().to_owned(),
            })
            .collect(),
        cmd: match &resolved.cmd {
            Cmd::Argv(v) => PlanCmd::Argv { argv: v.clone() },
            Cmd::Shell(s) => PlanCmd::Shell { shell: s.clone() },
            Cmd::ShellList(commands) => PlanCmd::ShellList {
                commands: commands.clone(),
            },
        },
    };

    match mode {
        OutputMode::Json => emit_json(WORKFLOW_PLAN_SCHEMA, "1.0", plan),
        OutputMode::Jsonl => {
            // Under --jsonl --dry-run we still emit a single envelope:
            // the plan is terminal, no streaming events make sense.
            emit_json(WORKFLOW_PLAN_SCHEMA, "1.0", plan);
        }
        OutputMode::Text => {
            info!(
                stage = %plan.stage_name,
                stage_hash = %plan.stage_hash,
                cache_hit = plan.cache_hit,
                deps = plan.deps.len(),
                outs = plan.outs.len(),
                "workflow dry-run plan"
            );
        }
    }
}

fn emit_dag_plan(
    args: &RunArgs,
    repo_root: &Path,
    workflow: &Workflow,
    graph: &Graph,
    mode: OutputMode,
) -> Result<()> {
    let selected = filter_stages(args, workflow, graph)?;
    let mut stages = Vec::new();

    for stage_name in graph.toposort() {
        if let Some(ref selected) = selected
            && !selected.contains(&stage_name)
        {
            continue;
        }
        let Some(stage) = workflow.stages.get(&stage_name) else {
            continue;
        };
        if stage.frozen {
            continue;
        }
        if let Some(ref condition) = stage.condition
            && !condition.evaluate(repo_root)
        {
            continue;
        }
        stages.push(DagPlanStage {
            stage_name: stage_name.as_str().to_owned(),
            cmd: plan_cmd_from_cmd(&stage.cmd),
        });
    }

    let plan = WorkflowDagPlan { stages };

    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(WORKFLOW_DAG_PLAN_SCHEMA, "1.0", plan);
        }
        OutputMode::Text => {
            for stage in &plan.stages {
                info!(
                    stage = %stage.stage_name,
                    cmd = %plan_cmd_display(&stage.cmd),
                    "workflow dry-run stage"
                );
            }
            info!(stages = plan.stages.len(), "workflow dry-run plan");
        }
    }

    Ok(())
}

fn plan_cmd_from_cmd(cmd: &Cmd) -> PlanCmd {
    match cmd {
        Cmd::Argv(argv) => PlanCmd::Argv { argv: argv.clone() },
        Cmd::Shell(shell) => PlanCmd::Shell {
            shell: shell.clone(),
        },
        Cmd::ShellList(commands) => PlanCmd::ShellList {
            commands: commands.clone(),
        },
    }
}

fn plan_cmd_display(cmd: &PlanCmd) -> String {
    match cmd {
        PlanCmd::Argv { argv } => argv.join(" "),
        PlanCmd::Shell { shell } => shell.clone(),
        PlanCmd::ShellList { commands } => commands.join(" && "),
    }
}

fn emit_result(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    result: &WorkflowStageResult,
    mode: OutputMode,
) {
    match mode {
        OutputMode::Json => emit_json(
            WORKFLOW_STAGE_RESULT_SCHEMA,
            WORKFLOW_SCHEMA_VERSION,
            result,
        ),
        OutputMode::Jsonl => {
            if let Some(stream) = jsonl {
                // Terminal `result` event carries the canonical
                // `workflow.stage_result` payload. Even though the
                // stream's umbrella schema is
                // `workflow.stage.event`, the final payload shape
                // matches `--json` output byte-for-byte.
                stream.emit_result(result);
            }
        }
        OutputMode::Text => {
            info!(
                stage = %result.stage_name,
                stage_hash = %result.stage_hash,
                cache_hit = result.cache_hit,
                duration_ms = result.duration_ms,
                outs = result.outs.len(),
                "workflow stage complete"
            );
        }
    }
}

fn emit_jsonl_started(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    stage: &str,
    stage_hash: &StageHash,
) {
    let Some(stream) = jsonl else { return };
    let payload = WorkflowStageStarted {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        attempt: 1,
        elapsed_ms: None,
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Started(&payload));
}

fn emit_jsonl_cache_checked(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    stage: &str,
    stage_hash: &StageHash,
    hit: bool,
    hit_source: &str,
) {
    let Some(stream) = jsonl else { return };
    let payload = WorkflowStageCacheChecked {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        hit,
        hit_source: hit_source.to_owned(),
        elapsed_ms: None,
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::CacheChecked(&payload));
}

fn emit_jsonl_produced(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    stage: &str,
    stage_hash: &StageHash,
) {
    let Some(stream) = jsonl else { return };
    let payload = WorkflowStageProduced {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        exit_code: 0,
        elapsed_ms: None,
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Produced(&payload));
}

fn emit_jsonl_hashed(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    stage: &str,
    stage_hash: &StageHash,
    entry: &StageCacheEntry,
) {
    let Some(stream) = jsonl else { return };
    let payload = WorkflowStageHashed {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        outs: entry
            .outs
            .iter()
            .map(|o| WorkflowStageOut {
                path: o.path.clone(),
                file_hash: o.file_hash.clone(),
                size: o.size,
            })
            .collect(),
        elapsed_ms: None,
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Hashed(&payload));
}

fn emit_jsonl_committed(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    result: &WorkflowStageResult,
) {
    let Some(stream) = jsonl else { return };
    let payload = WorkflowStageCommitted {
        stage: result.stage_name.clone(),
        stage_hash: result.stage_hash.clone(),
        duration_ms: result.duration_ms,
        attempts: result.attempts,
        cache_hit: result.cache_hit,
        elapsed_ms: None,
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Committed(&payload));
}

fn emit_jsonl_failed(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    stage: &str,
    stage_hash: &StageHash,
    err: &CrabError,
) {
    let Some(stream) = jsonl else { return };
    let (reason, exit_code, signal, timed_out) = classify_failure(err);
    let payload = WorkflowStageFailed {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        reason: reason.to_owned(),
        exit_code,
        signal,
        timed_out,
        // Phase 1 doesn't surface stderr here — the executor owns
        // the tail and it's captured in the journal payload. Wiring
        // it across the error enum is task 1.19.
        stderr_tail: None,
        elapsed_ms: None,
    };
    stream.emit_workflow_stage_event(&WorkflowStageEvent::Failed(&payload));
    // Also flush a structured error-info terminal event so consumers
    // who only consume the final line see the full error envelope.
    stream.emit_error_info(crate::core::output::ErrorInfo::from(err));
}

fn emit_jsonl_retry(
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
    stage: &str,
    stage_hash: &StageHash,
    attempt: u32,
    reason: &str,
    backoff: std::time::Duration,
) {
    let Some(stream) = jsonl else { return };
    let payload = WorkflowStageRetry {
        stage: stage.to_owned(),
        stage_hash: stage_hash.as_hex(),
        attempt,
        reason: reason.to_owned(),
        backoff_ms: backoff.as_millis().min(u128::from(u64::MAX)) as u64,
        exhausted: false,
        elapsed_ms: None,
    };
    stream.emit_schema_event(WORKFLOW_STAGE_RETRY_SCHEMA, "event", &payload);
}

/// Map a `CrabError` onto the `workflow.stage.failed` payload's
/// reason tag + signal/exit-code fields. The vocabulary mirrors the
/// executor's journal `FailedPayload` so consumers of either stream
/// see the same reason strings.
fn classify_failure(err: &CrabError) -> (&'static str, Option<i32>, Option<i32>, bool) {
    match err {
        CrabError::StageExecFailed { exit_code, .. } => {
            ("exit_nonzero", Some(*exit_code), None, false)
        }
        CrabError::StageExecSignaled { signal, .. } => ("signal", None, Some(*signal), false),
        CrabError::StageExecTimeout { .. } => ("timeout", None, None, true),
        CrabError::StageDiskFull { .. } => ("disk_full", None, None, false),
        CrabError::StageOutMalformed { .. } => ("out_malformed", None, None, false),
        CrabError::StageOutTooLarge { .. } => ("out_too_large", None, None, false),
        CrabError::StageOutCountExceeded { .. } => ("out_count_exceeded", None, None, false),
        _ => ("other", None, None, false),
    }
}

/// Map a `CrabError` onto the retry module's [`FailureKind`] so
/// the retry policy can decide whether to retry.
fn classify_failure_kind(err: &CrabError) -> FailureKind {
    match err {
        CrabError::StageExecFailed { exit_code, .. } => FailureKind::ExitCode(*exit_code),
        CrabError::StageExecSignaled { signal, .. } => FailureKind::Signal(*signal),
        CrabError::StageExecTimeout { .. } => FailureKind::Timeout,
        // Non-retryable failures: use exit code 0 which won't match
        // any policy's on_exit_codes list.
        _ => FailureKind::ExitCode(0),
    }
}

/// Remove partial output files from a failed attempt so the next
/// retry starts from a clean state.
fn clean_partial_outputs(stage: &Stage, repo_root: &Path) {
    for out in &stage.outs {
        let path = if out.path.is_absolute() {
            out.path.clone()
        } else {
            repo_root.join(&out.path)
        };
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Attempt to download missing dep files from the remote before
/// executing a stage. When `--pull` is set and a `Dep::Path` file
/// is absent from the workspace, this function tries to hydrate it
/// from the configured remote.
///
/// Returns the list of dep paths that were successfully pulled.
/// Deps that fail to download are left missing — the normal dep
/// resolution will surface `StageDepMissing` for them.
fn try_pull_missing_deps(stage: &Stage, stage_name: &StageName, repo_root: &Path) -> Vec<PathBuf> {
    let pulled = Vec::new();
    for dep in &stage.deps {
        if let Dep::Path(p) = dep {
            let abs = if p.is_absolute() {
                p.clone()
            } else if let Some(wdir) = &stage.wdir {
                repo_root.join(wdir).join(p)
            } else {
                repo_root.join(p)
            };
            if !abs.exists() {
                // The dep file is missing — attempt to pull from remote.
                warn!(
                    stage = %stage_name,
                    dep = %p.display(),
                    "pull: dep file missing; remote hydration not yet connected"
                );
                // Stub: actual remote download will be wired once the
                // storage layer integration for dep-level hydration is
                // complete. For now, the missing dep falls through to
                // normal resolution which will either use the lockfile
                // hash (--allow-missing) or error with StageDepMissing.
            }
        }
    }
    pulled
}

fn emit_miss_explanation(
    resolved: &ResolvedStage,
    hash: &StageHash,
    mode: OutputMode,
    repo_root: &Path,
) {
    let lockfile_path = repo_root.join("crab.lock");
    let lockfile = Lockfile::load(&lockfile_path).unwrap_or_default();
    let stage_name = &resolved.stage.name;

    // Build the current cmd in CachedCmd form for comparison.
    let current_cmd = match &resolved.cmd {
        Cmd::Argv(v) => crate::workflow::cache::CachedCmd::Argv { argv: v.clone() },
        Cmd::Shell(s) => crate::workflow::cache::CachedCmd::Shell { shell: s.clone() },
        Cmd::ShellList(commands) => crate::workflow::cache::CachedCmd::ShellList {
            commands: commands.clone(),
        },
    };

    // Resolve current env values for comparison.
    let current_env: BTreeMap<String, String> = match &resolved.env {
        EnvSpec::Allowlist(vars) => vars
            .iter()
            .filter_map(|v| std::env::var(v).ok().map(|val| (v.clone(), val)))
            .collect(),
        EnvSpec::Inherit | EnvSpec::Empty => BTreeMap::new(),
    };

    // Check if the lockfile has an entry for this stage.
    let diffs = lockfile.diff_against_resolved(
        stage_name,
        &resolved.dep_hashes,
        &resolved.params,
        &current_env,
        &current_cmd,
    );

    match diffs {
        Some(diff_entries) => {
            // Lockfile entry exists — emit the field-by-field diff.
            let locked_stage = lockfile.get(stage_name);
            let lockfile_hash = locked_stage
                .map(|s| s.stage_hash.as_hex())
                .unwrap_or_default();

            match mode {
                OutputMode::Json | OutputMode::Jsonl => {
                    let payload = serde_json::json!({
                        "schema": "workflow.explain_miss",
                        "data": {
                            "stage": stage_name.as_str(),
                            "stage_hash_current": format!("b3:{}", hash.as_hex()),
                            "stage_hash_lockfile": format!("b3:{}", lockfile_hash),
                            "diffs": diff_entries,
                        }
                    });
                    emit_json("workflow.explain_miss", "1.0", payload);
                }
                OutputMode::Text => {
                    info!(
                        stage = %stage_name,
                        stage_hash_current = %hash,
                        stage_hash_lockfile = %lockfile_hash,
                        diffs = diff_entries.len(),
                        "cache miss — field-by-field diff against lockfile"
                    );
                    for d in &diff_entries {
                        info!(
                            category = %d.category,
                            key = %d.key,
                            old = ?d.old,
                            new = ?d.new,
                            "changed"
                        );
                    }
                }
            }
        }
        None => {
            // No lockfile entry — "never run" case.
            match mode {
                OutputMode::Json | OutputMode::Jsonl => {
                    let payload = serde_json::json!({
                        "schema": "workflow.explain_miss",
                        "data": {
                            "stage": stage_name.as_str(),
                            "stage_hash_current": format!("b3:{}", hash.as_hex()),
                            "stage_hash_lockfile": null,
                            "reason": "never run",
                            "diffs": [],
                        }
                    });
                    emit_json("workflow.explain_miss", "1.0", payload);
                }
                OutputMode::Text => {
                    info!(
                        stage = %stage_name,
                        stage_hash = %hash,
                        "cache miss — stage has never been run (no lockfile entry)"
                    );
                }
            }
        }
    }
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Handle `--abandon <run_id>`: open the target journal and mark its
/// run outcome `Aborted`, then exit.
fn run_abandon(run_id_str: &str, mode: OutputMode) -> Result<()> {
    let run_id = Uuid::parse_str(run_id_str).map_err(|_| CrabError::Configuration {
        key: format!("--abandon requires a UUID, got {run_id_str:?}"),
        origin: "cli".into(),
    })?;
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    abandon_in(&cwd, run_id, mode)
}

/// Testable `--abandon` worker.
pub fn abandon_in(repo_root: &Path, run_id: Uuid, mode: OutputMode) -> Result<()> {
    let journal_path = repo_root
        .join(".crab")
        .join("workflow")
        .join("runs")
        .join(run_id.to_string())
        .join("journal.db");
    let journal = Journal::open(&journal_path)?;
    journal.mark_run_outcome(run_id, RunOutcome::Aborted)?;

    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            let payload = serde_json::json!({
                "run_id": run_id.to_string(),
                "outcome": "aborted",
            });
            emit_json("workflow.abandon", "1.0", payload);
        }
        OutputMode::Text => {
            info!(run_id = %run_id, "workflow journal marked aborted");
        }
    }
    Ok(())
}

/// Scan `workflow/runs/*` for non-terminal journals and log what the
/// resume path would do. Full multi-stage resume lands in task 3.7;
/// for phase 1 we surface the information so operators aren't flying
/// blind, but we don't automatically take action on other journals.
///
/// Per R21, each prior non-terminal journal bumps
/// `workflow_journal_resumes` exactly once — counting per stage row
/// would double-count every multi-stage run.
fn scan_prior_journals(
    workflow_root: &Path,
    current_run: Uuid,
    args: &RunArgs,
    metrics: &Metrics,
) -> Result<()> {
    let runs_dir = workflow_root.join("runs");
    let entries = match std::fs::read_dir(&runs_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CrabError::Io(e)),
    };

    let cli = CliFlags {
        resume_trust_outputs: args.resume_trust_outputs,
        force: args.force,
    };

    for entry in entries {
        let entry = entry.map_err(CrabError::Io)?;
        let name = entry.file_name();
        let run_id_str = name.to_string_lossy();
        let Ok(run_id) = Uuid::parse_str(&run_id_str) else {
            continue;
        };
        if run_id == current_run {
            continue;
        }
        let journal_path = entry.path().join("journal.db");
        let journal = match Journal::open(&journal_path) {
            Ok(j) => j,
            Err(e) => {
                warn!(run_id = %run_id, error = %e, "could not open prior journal; skipping");
                continue;
            }
        };
        if journal.run_outcome(run_id)?.is_some() {
            continue;
        }
        let rows = journal.stages_not_committed(run_id)?;
        if rows.is_empty() {
            continue;
        }
        // Count the run once — not once per stage row — before
        // iterating stage rows for logging.
        metrics.inc_workflow_journal_resumes();
        for row in rows {
            let action = resume::decide(row.state, FsState::default(), cli);
            info!(
                run_id = %run_id,
                stage = %row.stage_name,
                state = %row.state,
                action = ?action,
                "workflow: prior non-terminal journal discovered"
            );
            if matches!(action, ResumeAction::Discard) {
                debug!(run_id = %run_id, stage = %row.stage_name, "resume: discard");
            }
        }
    }
    Ok(())
}

fn sweep_orphans(workflow_root: &Path, repo_root: &Path, outs: &[Out]) -> Result<()> {
    // Phase 1 scope: sweep the parent directory of each declared out
    // plus the workflow scratch root. Active run_ids are empty — we
    // haven't opened ours yet, and the full active-run tracking lives
    // in task 3.7's DAG scheduler.
    let active: Vec<Uuid> = Vec::new();
    let _ = resume::sweep_orphan_sidecars(workflow_root, &active)?;
    for out in outs {
        // `Path::parent` on a bare filename returns `Some("")`. Treat
        // that — and the absent-parent case — as "sweep relative to
        // the repo root" so sidecars at top-level out paths don't
        // escape the sweep.
        let parent_rel = out
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| repo_root.to_path_buf(), Path::to_path_buf);
        let parent = if parent_rel.is_absolute() {
            parent_rel
        } else {
            repo_root.join(parent_rel)
        };
        if parent.exists() {
            let _ = resume::sweep_orphan_sidecars(&parent, &active)?;
        }
    }
    Ok(())
}

// --- Watch mode ---

/// Watch mode: execute the DAG once, then watch dep paths for changes
/// and re-execute affected stages on each change. Exits on SIGINT.
#[cfg(feature = "watch")]
#[expect(
    clippy::too_many_arguments,
    reason = "watch mode reuses the parsed workflow dispatch context across reruns"
)]
async fn run_watch(
    args: &RunArgs,
    repo_root: &Path,
    mode: OutputMode,
    config: &Config,
    workflow: &Workflow,
    graph: &Graph,
    lock_ctx: &LockfileContext,
    target_stage: Option<&str>,
    options: RunInvocationOptions,
) -> Result<()> {
    use crate::core::output::{WORKFLOW_WATCH_TRIGGERED_SCHEMA, WorkflowWatchTriggered};
    use crate::workflow::watcher::{DepWatcher, collect_dep_paths, collect_transitive_dep_paths};

    // Initial DAG run (or single-stage run).
    if let Some(name) = target_stage {
        run_yaml_single_stage(
            args,
            repo_root,
            mode,
            config,
            workflow,
            lock_ctx,
            name,
            options.clone(),
        )
        .await?;
    } else {
        // Ignore errors from the initial run — watch mode continues
        // watching even if the first run had failures.
        let _ = run_dag(
            args,
            repo_root,
            mode,
            config,
            workflow,
            graph,
            lock_ctx,
            options.clone(),
        )
        .await;
    }

    // Determine which dep paths to watch.
    let dep_paths = if let Some(name) = target_stage {
        let stage_name = StageName::parse(name)?;
        collect_transitive_dep_paths(&stage_name, &workflow.stages, graph)
    } else {
        collect_dep_paths(&workflow.stages)
    };

    if dep_paths.is_empty() {
        info!("watch: no dep paths to watch; exiting");
        return Ok(());
    }

    debug!(paths = dep_paths.len(), "watch: starting file watcher");

    let mut watcher = DepWatcher::start(&dep_paths, repo_root)?;

    // JSONL stream for watch events.
    let mut jsonl = if mode == OutputMode::Jsonl {
        Some(JsonlStream::new(
            "workflow.watch",
            WORKFLOW_SCHEMA_VERSION,
            std::io::stdout(),
        ))
    } else {
        None
    };

    // Watch loop: wait for changes, then re-execute.
    loop {
        tokio::select! {
            batch = watcher.next_batch() => {
                let Some(changed) = batch else {
                    // Watcher channel closed.
                    break;
                };

                let changed_paths: Vec<String> = changed
                    .iter()
                    .map(|p| p.strip_prefix(repo_root).unwrap_or(p).display().to_string())
                    .collect();

                info!(
                    changed = changed_paths.len(),
                    "watch: dep change detected, re-executing"
                );

                // Emit watch.triggered event.
                if let Some(ref mut stream) = jsonl {
                    let payload = WorkflowWatchTriggered {
                        changed_paths: changed_paths.clone(),
                        coalesced_events: changed.len(),
                    };
                    stream.emit_schema_event(
                        WORKFLOW_WATCH_TRIGGERED_SCHEMA,
                        "event",
                        &payload,
                    );
                }

                // Re-execute affected stages.
                if let Some(name) = target_stage {
                    let _ = run_yaml_single_stage(
                        args,
                        repo_root,
                        mode,
                        config,
                        workflow,
                        lock_ctx,
                        name,
                        options.clone(),
                    )
                    .await;
                } else {
                    let _ = run_dag(
                        args,
                        repo_root,
                        mode,
                        config,
                        workflow,
                        graph,
                        lock_ctx,
                        options.clone(),
                    )
                    .await;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("watch: received SIGINT, exiting");
                break;
            }
        }
    }

    Ok(())
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
    use crab_workflow::Defaults;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serialize env-mutating tests across threads so `CRAB_WORKFLOW_ENABLED`
    /// can't race. The config subsystem reads it as part of
    /// `Config::resolve_local()`.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    struct EnabledGuard;

    impl EnabledGuard {
        fn new() -> (std::sync::MutexGuard<'static, ()>, Self) {
            let lock = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: serialized via ENV_GUARD.
            unsafe { std::env::set_var("CRAB_WORKFLOW_ENABLED", "1") };
            (lock, Self)
        }
    }

    impl Drop for EnabledGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via ENV_GUARD.
            unsafe { std::env::remove_var("CRAB_WORKFLOW_ENABLED") };
        }
    }

    #[test]
    fn run_resolver_accepts_pinned_url_digest() {
        let tmp = TempDir::new().unwrap();
        let stage = StageName::parse("fetch").unwrap();
        let deps = vec![Dep::Url {
            url: "https://example.com/data.bin".to_owned(),
            digest: Some(format!("b3:{}", "56".repeat(32))),
        }];

        let hashes = resolve_dep_hashes(&stage, &deps, tmp.path()).unwrap();
        assert_eq!(
            hashes.get("https://example.com/data.bin"),
            Some(&[0x56; 32])
        );
    }

    #[test]
    fn run_resolver_accepts_unpinned_http_url_dep() {
        let tmp = TempDir::new().unwrap();
        let stage = StageName::parse("fetch").unwrap();
        let url = crate::workflow::stage::test_support::serve_http_body_once(b"run-url-body");
        let deps = vec![Dep::Url {
            url: url.clone(),
            digest: None,
        }];

        let hashes = resolve_dep_hashes(&stage, &deps, tmp.path()).unwrap();
        assert_eq!(
            hashes.get(&url),
            Some(blake3::hash(b"run-url-body").as_bytes())
        );
    }

    #[test]
    fn run_resolver_expands_remote_alias_url_dep() {
        let tmp = TempDir::new().unwrap();
        let stage = StageName::parse("fetch").unwrap();
        let base_url =
            crate::workflow::stage::test_support::serve_http_body_once(b"run-alias-body");
        let base_url = base_url.trim_end_matches("data.bin").to_owned();
        let deps = vec![Dep::Url {
            url: "remote://datasets/raw.csv".to_owned(),
            digest: None,
        }];
        let aliases = BTreeMap::from([("datasets".to_owned(), base_url)]);

        let hashes = resolve_dep_hashes_with_aliases(&stage, &deps, tmp.path(), &aliases).unwrap();

        assert_eq!(
            hashes.get("remote://datasets/raw.csv"),
            Some(blake3::hash(b"run-alias-body").as_bytes())
        );
    }

    #[test]
    fn run_resolver_rejects_unpinned_unsupported_url_dep() {
        let tmp = TempDir::new().unwrap();
        let stage = StageName::parse("fetch").unwrap();
        let deps = vec![Dep::Url {
            url: "ssh://example.com/data.bin".to_owned(),
            digest: None,
        }];

        let err = resolve_dep_hashes(&stage, &deps, tmp.path())
            .expect_err("unsupported URL deps need provider fetch support");
        assert!(matches!(err, CrabError::StageRemoteExecutionUnsupported));
    }

    /// Base args using repo-relative paths so `Out::validate` accepts
    /// the out paths. `cp` is invoked with absolute paths via the tmp
    /// dir so the child actually finds the dep.
    fn base_args(tmp: &Path) -> RunArgs {
        RunArgs {
            name: Some("copy".into()),
            deps: vec![PathBuf::from("a.txt")],
            outs: vec![PathBuf::from("b.txt")],
            env: vec![],
            empty_env: false,
            timeout: None,
            hermetic: false,
            nondeterministic: false,
            force: false,
            dry_run: false,
            interactive: false,
            cache_only: false,
            no_run_cache: false,
            no_commit: false,
            no_overwrite: false,
            resume_trust_outputs: false,
            abandon: None,
            explain_miss: false,
            lock_timeout: None,
            no_wait: false,
            json: false,
            jsonl: false,
            recursive: false,
            single_item: false,
            downstream: false,
            force_downstream: false,
            pipeline: false,
            all_pipelines: false,
            keep_going: false,
            ignore_errors: false,
            parallelism: None,
            cache_push: false,
            allow_missing: false,
            pull: false,
            validate: false,
            #[cfg(feature = "watch")]
            watch: false,
            workflow: None,
            stages: None,
            glob: false,
            cmd: vec![
                "/bin/cp".into(),
                tmp.join("a.txt").to_string_lossy().into(),
                tmp.join("b.txt").to_string_lossy().into(),
            ],
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_stage_name_is_rejected() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hi").unwrap();

        let mut args = base_args(tmp.path());
        args.name = Some("has spaces".into());
        let err = run_in(&args, tmp.path(), OutputMode::Text)
            .await
            .expect_err("space in name must fail");
        assert!(
            matches!(err, CrabError::WorkflowStageNameInvalid { .. }),
            "wrong variant: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hermetic_flag_executes_or_reports_unsupported_backend() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hi").unwrap();

        let mut args = base_args(tmp.path());
        args.hermetic = true;
        let result = run_in_with_options(
            &args,
            tmp.path(),
            OutputMode::Text,
            RunInvocationOptions {
                mirror_child_output: false,
                ..RunInvocationOptions::default()
            },
        )
        .await;
        if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file() {
            result.expect("--hermetic should execute through the sandbox");
            assert_eq!(fs::read(tmp.path().join("b.txt")).unwrap(), b"hi");
        } else {
            match result.expect_err("--hermetic should require a supported backend") {
                CrabError::Configuration { key, origin } => {
                    assert!(key.contains("copy"));
                    assert!(origin.contains("sandbox-exec"));
                }
                other => panic!("wrong variant: {other}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hermetic_flag_reports_undeclared_read_path() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hi").unwrap();
        fs::write(tmp.path().join("secret.txt"), b"nope").unwrap();

        let mut args = base_args(tmp.path());
        args.hermetic = true;
        args.cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "cat secret.txt > b.txt".into(),
        ];
        let result = run_in_with_options(
            &args,
            tmp.path(),
            OutputMode::Text,
            RunInvocationOptions {
                mirror_child_output: false,
                ..RunInvocationOptions::default()
            },
        )
        .await;
        if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file() {
            match result.expect_err("undeclared read must fail hermetic execution") {
                CrabError::WorkflowHermeticViolation { stage, path } => {
                    assert_eq!(stage, "copy");
                    assert_eq!(
                        path,
                        fs::canonicalize(tmp.path()).unwrap().join("secret.txt")
                    );
                }
                other => panic!("wrong variant: {other}"),
            }
        } else {
            assert!(matches!(result, Err(CrabError::Configuration { .. })));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hermetic_flag_reports_undeclared_write_path() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hi").unwrap();

        let mut args = base_args(tmp.path());
        args.hermetic = true;
        args.cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "cat a.txt > secret-out.txt".into(),
        ];
        let result = run_in_with_options(
            &args,
            tmp.path(),
            OutputMode::Text,
            RunInvocationOptions {
                mirror_child_output: false,
                ..RunInvocationOptions::default()
            },
        )
        .await;
        if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file() {
            match result.expect_err("undeclared write must fail hermetic execution") {
                CrabError::WorkflowHermeticViolation { stage, path } => {
                    assert_eq!(stage, "copy");
                    assert_eq!(
                        path,
                        fs::canonicalize(tmp.path()).unwrap().join("secret-out.txt")
                    );
                }
                other => panic!("wrong variant: {other}"),
            }
        } else {
            assert!(matches!(result, Err(CrabError::Configuration { .. })));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_dep_reports_dep_missing() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        // Dep path deliberately missing.
        let args = base_args(tmp.path());
        let err = run_in(&args, tmp.path(), OutputMode::Text)
            .await
            .expect_err("missing dep must fail");
        assert!(
            matches!(err, CrabError::StageDepMissing { .. }),
            "wrong variant: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workflow_disabled_returns_config_error() {
        let _lock = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized via ENV_GUARD.
        unsafe { std::env::remove_var("CRAB_WORKFLOW_ENABLED") };
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hi").unwrap();
        let args = base_args(tmp.path());
        let err = run_in(&args, tmp.path(), OutputMode::Text)
            .await
            .expect_err("disabled workflow must fail");
        assert!(
            matches!(err, CrabError::WorkflowDisabled),
            "wrong variant: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn happy_path_runs_and_second_invocation_hits_cache() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        // cwd has to match repo_root so `cp` writes and the executor
        // verifies at the same relative path.
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        fs::write(tmp.path().join("a.txt"), b"payload").unwrap();
        let args = base_args(tmp.path());

        let result = run_in(&args, tmp.path(), OutputMode::Text).await;

        // Restore cwd before panicking — otherwise later tests race.
        std::env::set_current_dir(&prev_cwd).unwrap();
        result.expect("first run should succeed");

        assert_eq!(
            fs::read(tmp.path().join("b.txt")).unwrap(),
            b"payload".to_vec()
        );

        // Second run: deps unchanged → cache hit. Leave the out file in
        // place; the materialize path should detect the hash match and
        // no-op the write without touching disk.
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = run_in(&args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();
        result.expect("second run should hit cache");

        assert_eq!(
            fs::read(tmp.path().join("b.txt")).unwrap(),
            b"payload".to_vec()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn force_reexecutes_even_on_cache_hit() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        fs::write(tmp.path().join("a.txt"), b"x").unwrap();

        let args = base_args(tmp.path());
        let result = run_in(&args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();
        result.expect("first run succeeds");

        // The cache entry exists. With --force the executor re-runs cp,
        // so the out file is recreated from the live dep. Replacing cp
        // with a guaranteed failure proves the executor ran.
        let mut force_args = base_args(tmp.path());
        force_args.force = true;
        force_args.cmd = vec!["/bin/sh".into(), "-c".into(), "exit 77".into()];

        std::env::set_current_dir(tmp.path()).unwrap();
        let err_result = run_in(&force_args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();
        let err = err_result.expect_err("--force must re-execute");
        match err {
            CrabError::StageExecFailed { exit_code, .. } => assert_eq!(exit_code, 77),
            other => panic!("wrong variant: {other}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_run_cache_reexecutes_existing_cache_entry() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let mut args = base_args(tmp.path());
        args.name = Some("count".into());
        args.deps = vec![];
        args.outs = vec![PathBuf::from("out.txt")];
        args.cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "n=$(cat counter 2>/dev/null || echo 0); \
             n=$((n + 1)); \
             printf '%s\\n' \"$n\" > counter; \
             printf '%s\\n' \"$n\" > out.txt"
                .into(),
        ];

        let first = run_in(&args, tmp.path(), OutputMode::Text).await;
        if let Err(err) = first {
            std::env::set_current_dir(&prev_cwd).unwrap();
            panic!("first run succeeds: {err}");
        }
        std::fs::remove_file(tmp.path().join("out.txt")).unwrap();

        args.no_run_cache = true;
        let second = run_in(&args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();

        second.expect("--no-run-cache re-executes cached stage");
        assert_eq!(
            fs::read_to_string(tmp.path().join("out.txt")).unwrap(),
            "2\n"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("counter")).unwrap(),
            "2\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_commit_executes_without_writing_run_cache_entry() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let mut args = base_args(tmp.path());
        args.name = Some("count".into());
        args.deps = vec![];
        args.outs = vec![PathBuf::from("out.txt")];
        args.no_commit = true;
        args.cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "n=$(cat counter 2>/dev/null || echo 0); \
             n=$((n + 1)); \
             printf '%s\\n' \"$n\" > counter; \
             printf '%s\\n' \"$n\" > out.txt"
                .into(),
        ];

        let first = run_in(&args, tmp.path(), OutputMode::Text).await;
        if let Err(err) = first {
            std::env::set_current_dir(&prev_cwd).unwrap();
            panic!("first run succeeds: {err}");
        }
        assert!(
            tmp.path().join("crab.lock").exists(),
            "--no-commit still updates the lockfile"
        );
        std::fs::remove_file(tmp.path().join("out.txt")).unwrap();

        args.no_commit = false;
        let second = run_in(&args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();

        second.expect("second run executes because no cache entry was written");
        assert_eq!(
            fs::read_to_string(tmp.path().join("out.txt")).unwrap(),
            "2\n"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("counter")).unwrap(),
            "2\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn force_reexecutes_existing_cache_entry_with_same_hash() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let mut args = base_args(tmp.path());
        args.name = Some("count".into());
        args.deps = vec![];
        args.outs = vec![PathBuf::from("out.txt")];
        args.cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "n=$(cat counter 2>/dev/null || echo 0); \
             n=$((n + 1)); \
             printf '%s\\n' \"$n\" > counter; \
             printf '%s\\n' \"$n\" > out.txt"
                .into(),
        ];

        let first = run_in(&args, tmp.path(), OutputMode::Text).await;
        if let Err(err) = first {
            std::env::set_current_dir(&prev_cwd).unwrap();
            panic!("first run succeeds: {err}");
        }

        args.force = true;
        let second = run_in(&args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();

        second.expect("--force re-executes cached stage");
        assert_eq!(
            fs::read_to_string(tmp.path().join("counter")).unwrap(),
            "2\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dry_run_emits_stage_hash_without_executing() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), b"x").unwrap();

        let mut args = base_args(tmp.path());
        args.dry_run = true;
        // cp would fail since b.txt is missing on disk and the dest dir
        // exists. We assert the command never ran by checking that the
        // out file is absent afterwards.
        run_in(&args, tmp.path(), OutputMode::Text)
            .await
            .expect("dry run is pure");
        assert!(
            !tmp.path().join("b.txt").exists(),
            "dry run must not execute the command"
        );
    }

    #[test]
    fn abandon_marks_journal_aborted() {
        let tmp = TempDir::new().unwrap();
        let run_id = Uuid::now_v7();
        // Seed a journal with a run row we can abandon.
        let journal_path = tmp
            .path()
            .join(".crab")
            .join("workflow")
            .join("runs")
            .join(run_id.to_string())
            .join("journal.db");
        let journal = Journal::open(&journal_path).unwrap();
        journal
            .insert_run_start(run_id, env!("CARGO_PKG_VERSION"), "test")
            .unwrap();
        drop(journal);

        abandon_in(tmp.path(), run_id, OutputMode::Text).unwrap();

        let reopened = Journal::open(&journal_path).unwrap();
        assert_eq!(
            reopened.run_outcome(run_id).unwrap(),
            Some(RunOutcome::Aborted)
        );
    }

    #[test]
    fn build_env_spec_branches() {
        let mut args = RunArgs {
            name: None,
            deps: vec![],
            outs: vec![],
            env: vec![],
            empty_env: false,
            timeout: None,
            hermetic: false,
            nondeterministic: false,
            force: false,
            dry_run: false,
            interactive: false,
            cache_only: false,
            no_run_cache: false,
            no_commit: false,
            no_overwrite: false,
            resume_trust_outputs: false,
            abandon: None,
            explain_miss: false,
            lock_timeout: None,
            no_wait: false,
            json: false,
            jsonl: false,
            recursive: false,
            single_item: false,
            downstream: false,
            force_downstream: false,
            pipeline: false,
            all_pipelines: false,
            keep_going: false,
            ignore_errors: false,
            parallelism: None,
            cache_push: false,
            allow_missing: false,
            pull: false,
            validate: false,
            #[cfg(feature = "watch")]
            watch: false,
            workflow: None,
            stages: None,
            glob: false,
            cmd: vec![],
        };
        assert!(matches!(build_env_spec(&args), EnvSpec::Inherit));
        args.empty_env = true;
        assert!(matches!(build_env_spec(&args), EnvSpec::Empty));
        args.empty_env = false;
        args.env = vec!["PATH".into()];
        match build_env_spec(&args) {
            EnvSpec::Allowlist(v) => assert_eq!(v, vec!["PATH".to_owned()]),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn cli_dep_parses_dvc_url_deps() {
        assert!(matches!(
            cli_dep(&PathBuf::from("s3://bucket/data.csv")),
            Dep::Url { ref url, digest: None } if url == "s3://bucket/data.csv"
        ));
        assert!(matches!(
            cli_dep(&PathBuf::from("data/input.csv")),
            Dep::Path(path) if path == PathBuf::from("data/input.csv")
        ));
    }

    #[test]
    fn parse_timeout_covers_suffixes() {
        assert_eq!(parse_timeout(None).unwrap(), None);
        assert_eq!(
            parse_timeout(Some("30s")).unwrap(),
            Some(std::time::Duration::from_secs(30))
        );
        assert_eq!(
            parse_timeout(Some("5m")).unwrap(),
            Some(std::time::Duration::from_secs(300))
        );
        assert_eq!(
            parse_timeout(Some("1h")).unwrap(),
            Some(std::time::Duration::from_secs(3600))
        );
        assert_eq!(
            parse_timeout(Some("42")).unwrap(),
            Some(std::time::Duration::from_secs(42))
        );
        assert!(parse_timeout(Some("abc")).is_err());
    }

    // --- Yaml-backed run_in modes ---

    /// RunArgs with no inline flags — used by the yaml dispatcher
    /// tests. The caller adds the positional stage name (or leaves
    /// `cmd` empty for DAG mode) before running.
    fn yaml_base_args() -> RunArgs {
        RunArgs {
            name: None,
            deps: vec![],
            outs: vec![],
            env: vec![],
            empty_env: false,
            timeout: None,
            hermetic: false,
            nondeterministic: false,
            force: false,
            dry_run: false,
            interactive: false,
            cache_only: false,
            no_run_cache: false,
            no_commit: false,
            no_overwrite: false,
            resume_trust_outputs: false,
            abandon: None,
            explain_miss: false,
            lock_timeout: None,
            no_wait: false,
            json: false,
            jsonl: false,
            recursive: false,
            single_item: false,
            downstream: false,
            force_downstream: false,
            pipeline: false,
            all_pipelines: false,
            keep_going: false,
            ignore_errors: false,
            parallelism: None,
            cache_push: false,
            allow_missing: false,
            pull: false,
            validate: false,
            #[cfg(feature = "watch")]
            watch: false,
            workflow: None,
            stages: None,
            glob: false,
            cmd: vec![],
        }
    }

    fn selected_stage_names(args: &RunArgs, yaml_text: &str) -> BTreeSet<String> {
        let workflow = yaml::parse(yaml_text).expect("workflow parses");
        let graph = Graph::build(&workflow.stages).expect("graph builds");
        filter_stages(args, &workflow, &graph)
            .expect("filter resolves")
            .expect("filter active")
            .into_iter()
            .map(|name| name.as_str().to_owned())
            .collect()
    }

    fn selected_stage_names_from_workflow(args: &RunArgs, workflow: &Workflow) -> BTreeSet<String> {
        let graph = Graph::build(&workflow.stages).expect("graph builds");
        filter_stages(args, workflow, &graph)
            .expect("filter resolves")
            .expect("filter active")
            .into_iter()
            .map(|name| name.as_str().to_owned())
            .collect()
    }

    fn workflow_with_effective_stages(names: &[&str]) -> Workflow {
        let stages = names
            .iter()
            .map(|name| {
                let stage_name = StageName::parse_effective(name).expect("valid effective name");
                (
                    stage_name.clone(),
                    Stage::new(stage_name, Cmd::Shell("true".to_owned())),
                )
            })
            .collect();
        Workflow {
            params: Vec::new(),
            metrics: Vec::new(),
            plots: Vec::new(),
            plot_configs: Vec::new(),
            defaults: Defaults::default(),
            stages,
            workflow_membership: BTreeMap::new(),
        }
    }

    fn target_selection_yaml() -> &'static str {
        r#"stages:
  prepare:
    cmd: "true"
    outs:
      - prepared.txt
  train_a:
    cmd: "true"
    deps:
      - prepared.txt
    outs:
      - train-a.txt
  train_b:
    cmd: "true"
    deps:
      - prepared.txt
    outs:
      - train-b.txt
  evaluate:
    cmd: "true"
    deps:
      - train-a.txt
    outs:
      - metrics.json
  archive:
    cmd: "true"
    deps:
      - metrics.json
    outs:
      - archive.txt
  standalone:
    cmd: "true"
    outs:
      - standalone.txt
"#
    }

    #[test]
    fn run_args_accept_dvc_target_flags() {
        let args = RunArgs::try_parse_from(["run", "--single-item", "--glob", "train_*"])
            .expect("single item glob parses");
        assert!(args.single_item);
        assert!(args.glob);
        assert_eq!(args.cmd, vec!["train_*".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "--single-item", "train", "--json"])
            .expect("output flags parse after targets");
        assert!(args.single_item);
        assert!(args.json);
        assert_eq!(args.cmd, vec!["train".to_owned()]);

        let args = RunArgs::try_parse_from([
            "run",
            "--name",
            "copy",
            "--deps",
            "a.txt",
            "--outs",
            "b.txt",
            "--",
            "/bin/sh",
            "-c",
            "cp a.txt b.txt",
        ])
        .expect("inline commands still parse after --");
        assert_eq!(
            args.cmd,
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "cp a.txt b.txt".to_owned()
            ]
        );

        let args = RunArgs::try_parse_from(["run", "--pipeline", "evaluate"])
            .expect("pipeline target parses");
        assert!(args.pipeline);
        assert_eq!(args.cmd, vec!["evaluate".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "--all-pipelines", "ignored"])
            .expect("all-pipelines target parses");
        assert!(args.all_pipelines);
        assert_eq!(args.cmd, vec!["ignored".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "-R", "pipelines.train"])
            .expect("recursive short flag parses");
        assert!(args.recursive);
        assert_eq!(args.cmd, vec!["pipelines.train".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "--dry"]).expect("dry alias parses");
        assert!(args.dry_run);

        let args = RunArgs::try_parse_from(["run", "-i", "train"]).expect("interactive parses");
        assert!(args.interactive);
        assert_eq!(args.cmd, vec!["train".to_owned()]);

        let args =
            RunArgs::try_parse_from(["run", "--no-commit", "train"]).expect("no-commit parses");
        assert!(args.no_commit);
        assert_eq!(args.cmd, vec!["train".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "--force-downstream", "train"])
            .expect("force-downstream parses");
        assert!(args.force_downstream);
        assert_eq!(args.cmd, vec!["train".to_owned()]);

        let err = RunArgs::try_parse_from(["run", "--single-item", "--downstream", "train"])
            .expect_err("exclusive target modes conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        let err = RunArgs::try_parse_from(["run", "--cache-only", "--no-run-cache"])
            .expect_err("cache-only conflicts with no-run-cache");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn interactive_answer_accepts_only_yes() {
        assert!(interactive_answer_is_yes("y\n"));
        assert!(interactive_answer_is_yes("yes"));
        assert!(interactive_answer_is_yes("YES"));
        assert!(!interactive_answer_is_yes(""));
        assert!(!interactive_answer_is_yes("n"));
        assert!(!interactive_answer_is_yes("anything else"));
    }

    #[test]
    fn target_stage_defaults_to_upstream_closure() {
        let mut args = yaml_base_args();
        args.cmd = vec!["evaluate".to_owned()];

        let selected = selected_stage_names(&args, target_selection_yaml());

        assert_eq!(
            selected,
            BTreeSet::from([
                "prepare".to_owned(),
                "train_a".to_owned(),
                "evaluate".to_owned()
            ])
        );
    }

    #[test]
    fn single_item_target_selects_only_named_stage() {
        let mut args = yaml_base_args();
        args.single_item = true;
        args.cmd = vec!["evaluate".to_owned()];

        let selected = selected_stage_names(&args, target_selection_yaml());

        assert_eq!(selected, BTreeSet::from(["evaluate".to_owned()]));
    }

    #[test]
    fn downstream_target_selects_target_and_consumers() {
        let mut args = yaml_base_args();
        args.downstream = true;
        args.cmd = vec!["train_a".to_owned()];

        let selected = selected_stage_names(&args, target_selection_yaml());

        assert_eq!(
            selected,
            BTreeSet::from([
                "train_a".to_owned(),
                "evaluate".to_owned(),
                "archive".to_owned()
            ])
        );
    }

    #[test]
    fn pipeline_target_selects_connected_component() {
        let mut args = yaml_base_args();
        args.pipeline = true;
        args.cmd = vec!["evaluate".to_owned()];

        let selected = selected_stage_names(&args, target_selection_yaml());

        assert_eq!(
            selected,
            BTreeSet::from([
                "prepare".to_owned(),
                "train_a".to_owned(),
                "train_b".to_owned(),
                "evaluate".to_owned(),
                "archive".to_owned()
            ])
        );
    }

    #[test]
    fn glob_targets_match_stage_names_before_closure() {
        let mut args = yaml_base_args();
        args.glob = true;
        args.cmd = vec!["train_*".to_owned()];

        let selected = selected_stage_names(&args, target_selection_yaml());

        assert_eq!(
            selected,
            BTreeSet::from([
                "prepare".to_owned(),
                "train_a".to_owned(),
                "train_b".to_owned()
            ])
        );
    }

    #[test]
    fn all_pipelines_ignores_positional_targets() {
        let mut args = yaml_base_args();
        args.all_pipelines = true;
        args.cmd = vec!["evaluate".to_owned()];
        let workflow = yaml::parse(target_selection_yaml()).expect("workflow parses");
        let graph = Graph::build(&workflow.stages).expect("graph builds");

        let selected = filter_stages(&args, &workflow, &graph).expect("filter resolves");

        assert!(selected.is_none());
    }

    #[test]
    fn dotted_target_selects_effective_stage_name() {
        let workflow = workflow_with_effective_stages(&["models.train"]);
        let mut args = yaml_base_args();
        args.cmd = vec!["models.train".to_owned()];

        let selected = selected_stage_names_from_workflow(&args, &workflow);

        assert_eq!(selected, BTreeSet::from(["models.train".to_owned()]));
    }

    #[test]
    fn dvc_path_target_selects_prefixed_stage_name() {
        let workflow = workflow_with_effective_stages(&["models.train"]);
        let mut args = yaml_base_args();
        args.cmd = vec!["models/dvc.yaml:train".to_owned()];

        let selected = selected_stage_names_from_workflow(&args, &workflow);

        assert_eq!(selected, BTreeSet::from(["models.train".to_owned()]));
    }

    #[test]
    fn dvc_path_glob_targets_stages_from_that_workflow_file() {
        let workflow =
            workflow_with_effective_stages(&["models.train_a", "models.train_b", "eval.train_a"]);
        let mut args = yaml_base_args();
        args.glob = true;
        args.cmd = vec!["models/dvc.yaml:train_*".to_owned()];

        let selected = selected_stage_names_from_workflow(&args, &workflow);

        assert_eq!(
            selected,
            BTreeSet::from(["models.train_a".to_owned(), "models.train_b".to_owned()])
        );
    }

    /// Yaml mode without a yaml file and without inline flags
    /// surfaces a configuration error rather than panicking or
    /// falling through to the phase-1 path.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_yaml_and_no_inline_flags_reports_config_error() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let args = yaml_base_args();
        let err = run_in(&args, tmp.path(), OutputMode::Text)
            .await
            .expect_err("no yaml and no inline flags must error");
        assert!(
            matches!(err, CrabError::Configuration { .. }),
            "wrong variant: {err}"
        );
    }

    /// Single-stage-from-yaml: `crab run <stage>` with a valid
    /// `crab.yaml`. Runs the one named stage, which cascades
    /// through the usual executor path and materializes its out.
    #[tokio::test(flavor = "multi_thread")]
    async fn yaml_single_stage_runs_named_stage() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        // Seed the dep and write a minimal yaml that cp's it.
        fs::write(tmp.path().join("a.txt"), b"yaml-payload").unwrap();
        let yaml = format!(
            "stages:\n  copy:\n    cmd:\n      argv: [\"/bin/cp\", \"{src}\", \"{dst}\"]\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n",
            src = tmp.path().join("a.txt").to_string_lossy(),
            dst = tmp.path().join("b.txt").to_string_lossy(),
        );
        fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

        let mut args = yaml_base_args();
        args.cmd = vec!["copy".into()];

        let result = run_in(&args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();
        result.expect("yaml single-stage run succeeds");

        assert_eq!(
            fs::read(tmp.path().join("b.txt")).unwrap(),
            b"yaml-payload".to_vec(),
            "named stage produced its out"
        );

        // Lockfile must have been written with the one stage.
        let lock_path = tmp.path().join("crab.lock");
        assert!(lock_path.exists(), "yaml mode writes crab.lock");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn yaml_hermetic_stage_executes_or_reports_unsupported_backend() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        fs::write(tmp.path().join("a.txt"), b"yaml-hermetic").unwrap();
        let yaml = format!(
            "stages:\n  copy:\n    hermetic: true\n    cmd:\n      argv: [\"/bin/cp\", \"{src}\", \"{dst}\"]\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n",
            src = tmp.path().join("a.txt").to_string_lossy(),
            dst = tmp.path().join("b.txt").to_string_lossy(),
        );
        fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

        let mut args = yaml_base_args();
        args.cmd = vec!["copy".into()];
        let result = run_in_with_options(
            &args,
            tmp.path(),
            OutputMode::Text,
            RunInvocationOptions {
                mirror_child_output: false,
                ..RunInvocationOptions::default()
            },
        )
        .await;
        std::env::set_current_dir(&prev_cwd).unwrap();

        if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file() {
            result.expect("yaml hermetic stage should execute through the sandbox");
            assert_eq!(
                fs::read(tmp.path().join("b.txt")).unwrap(),
                b"yaml-hermetic"
            );
        } else {
            assert!(matches!(result, Err(CrabError::Configuration { .. })));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn yaml_dag_dry_run_does_not_execute_or_write_lockfile() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        fs::write(
            tmp.path().join("crab.yaml"),
            r#"stages:
  write:
    cmd: "printf 'nope\n' > out.txt"
    outs:
      - out.txt
"#,
        )
        .unwrap();

        let mut args = yaml_base_args();
        args.dry_run = true;
        let result = run_in(&args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();

        result.expect("dry-run plan succeeds");
        assert!(
            !tmp.path().join("out.txt").exists(),
            "dry run must not execute stage commands"
        );
        assert!(
            !tmp.path().join("crab.lock").exists(),
            "dry run must not write the lockfile"
        );
    }

    /// Multi-stage DAG with `--keep-going`: stage a fails, its
    /// downstream stage b is marked not_started, but unrelated
    /// stage c still runs. Exit is non-zero overall; c's out is
    /// materialized.
    #[tokio::test(flavor = "multi_thread")]
    async fn dag_keep_going_runs_unrelated_branch_on_failure() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        fs::write(tmp.path().join("c-input.txt"), b"unrelated").unwrap();

        // Stage 'a' (fails) → 'b' (depends on a.out, blocked).
        // Stage 'c' is independent and should succeed under keep-going.
        let yaml = format!(
            r#"stages:
  a:
    cmd:
      argv: ["/bin/sh", "-c", "exit 13"]
    outs:
      - a.out
  b:
    cmd:
      argv: ["/bin/cp", "a.out", "b.out"]
    deps:
      - a.out
    outs:
      - b.out
  c:
    cmd:
      argv: ["/bin/cp", "{src}", "{dst}"]
    deps:
      - c-input.txt
    outs:
      - c.out
"#,
            src = tmp.path().join("c-input.txt").to_string_lossy(),
            dst = tmp.path().join("c.out").to_string_lossy(),
        );
        fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

        let mut args = yaml_base_args();
        args.keep_going = true;

        let result = run_in(&args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();

        // Overall run reports failure (stage a died), but c still
        // completed — its out is on disk.
        assert!(
            result.is_err(),
            "dag run with failed stage must surface failure"
        );
        assert!(
            tmp.path().join("c.out").exists(),
            "unrelated branch must complete under --keep-going: {:?}",
            fs::read_dir(tmp.path())
                .map(|it| it
                    .filter_map(|e| e.ok().map(|e| e.file_name()))
                    .collect::<Vec<_>>())
                .unwrap_or_default()
        );
        assert!(
            !tmp.path().join("b.out").exists(),
            "downstream of failed stage must not run"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn force_downstream_reexecutes_descendant_cache_hit_after_upstream_change() {
        let (_lock, _guard) = EnabledGuard::new();
        let tmp = TempDir::new().unwrap();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        fs::write(tmp.path().join("trigger.txt"), b"v1").unwrap();
        fs::write(
            tmp.path().join("crab.yaml"),
            r#"stages:
  a:
    cmd: |
      n=$(cat counter-a 2>/dev/null || echo 0)
      n=$((n + 1))
      printf '%s\n' "$n" > counter-a
      printf 'stable\n' > a.txt
    deps:
      - trigger.txt
    outs:
      - a.txt
  b:
    cmd: |
      n=$(cat counter-b 2>/dev/null || echo 0)
      n=$((n + 1))
      printf '%s\n' "$n" > counter-b
      cp a.txt b.txt
    deps:
      - a.txt
    outs:
      - b.txt
"#,
        )
        .unwrap();

        let args = yaml_base_args();
        let first = run_in(&args, tmp.path(), OutputMode::Text).await;
        if let Err(err) = first {
            std::env::set_current_dir(&prev_cwd).unwrap();
            panic!("first dag run succeeds: {err}");
        }
        assert_eq!(
            fs::read_to_string(tmp.path().join("counter-b")).unwrap(),
            "1\n"
        );

        fs::write(tmp.path().join("trigger.txt"), b"v2").unwrap();
        let mut force_downstream_args = yaml_base_args();
        force_downstream_args.force_downstream = true;
        let second = run_in(&force_downstream_args, tmp.path(), OutputMode::Text).await;
        std::env::set_current_dir(&prev_cwd).unwrap();

        second.expect("--force-downstream run succeeds");
        assert_eq!(
            fs::read_to_string(tmp.path().join("counter-a")).unwrap(),
            "2\n"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("counter-b")).unwrap(),
            "2\n"
        );
    }
}
