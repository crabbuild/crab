//! `crab workflow status` — per-stage state report.
//!
//! Extends the hydration-oriented `crab status` with a workflow-
//! layer view: for every stage declared in `crab.yaml` report
//! whether it is
//!
//! - **up-to-date**: current stage hash matches the lockfile entry.
//! - **stale**: differs from the lockfile, with a reason pointing at
//!   the first differing input (deps, params, env, cmd).
//! - **never-run**: no lockfile entry exists for the stage.
//! - **in-flight**: a non-terminal row for the stage exists in one
//!   of the journals under `.crab/workflow/runs/`.
//!
//! `--why <stage>` goes deeper, emitting a field-by-field input-hash
//! breakdown for a single stage. That's the same information
//! `crab run --explain-miss` surfaces, produced here without
//! executing anything.
//!
//! Structured output routes through `core/output::emit_json` with
//! schema `workflow.status`. Text mode prints a short human-readable
//! table.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::workflow::cache::{CachedCmd, read_local, remote_stage_ref_path};
use crate::workflow::discover::{self, DiscoverMode};
use crate::workflow::hasher::{ResolvedStage, compute as compute_stage_hash};
use crate::workflow::journal::Journal;
use crate::workflow::params::resolve_stage_param_values_with_wdir;
use crate::workflow::stage::{Cmd, Dep, DepUrlHashExt, OutKind, Stage, StageName};
use crab_types::workflow::StageHash;
use crab_workflow::{Graph, LockedStage, Lockfile, Workflow};

/// Structured-output schema label for `crab workflow status`.
pub const WORKFLOW_STATUS_SCHEMA: &str = "workflow.status";

/// Clap args for `crab workflow status`.
#[derive(Debug, Clone, Parser, Default)]
pub struct StatusArgs {
    /// Drill into one stage and emit a field-by-field input-hash
    /// diff. Same payload `crab run --explain-miss` produces; this
    /// form is read-only and safe on any repo with a `crab.yaml`.
    #[arg(long, value_name = "STAGE")]
    pub why: Option<String>,

    /// Merge every `crab.yaml` under the repo root (R2). Defaults
    /// to the `[workflow] discover` config setting.
    #[arg(long, default_value_t = false)]
    pub recursive: bool,

    /// Path to the lockfile. Defaults to `crab.lock` at the repo
    /// root. Handy for tests that stage a lockfile outside the repo.
    #[arg(long, value_name = "PATH")]
    pub lockfile: Option<PathBuf>,

    /// Include upstream stages for any selected targets.
    #[arg(long, short = 'd', default_value_t = false, conflicts_with = "why")]
    pub with_deps: bool,

    /// Compare local stage-cache entries with the configured Crab remote.
    #[arg(long, short = 'c', default_value_t = false, conflicts_with = "why")]
    pub cloud: bool,

    /// Crab git remote name to compare against. Implies `--cloud`.
    #[arg(long, short = 'r', value_name = "NAME", conflicts_with = "why")]
    pub remote: Option<String>,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Stage names or declared output paths to inspect.
    #[arg(value_name = "TARGET", conflicts_with = "why")]
    pub targets: Vec<String>,
}

impl StatusArgs {
    /// Output mode derived from `--json`. Status is a single
    /// terminal envelope, never a streaming event sequence.
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

// ─── Payloads ─────────────────────────────────────────────────────────

/// Top-level JSON payload for `workflow.status`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StatusPayload {
    pub stages: Vec<StageStatusEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteStatusSummary>,
}

/// Summary of local stage-cache state compared with a remote cache.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RemoteStatusSummary {
    pub remote: String,
    pub checked: u32,
    pub in_sync: u32,
    pub new: u32,
    pub deleted: u32,
    pub missing: u32,
    pub uncached: u32,
}

/// One row of the structured status report.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct StageStatusEntry {
    pub stage: String,
    /// One of `up_to_date` / `stale` / `never_run` / `in_flight`.
    pub state: String,
    /// Populated when `state = stale`; one of `dep` / `param` /
    /// `env` / `cmd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Populated when `reason = dep`/`param`/`env`: the specific
    /// key that invalidated the cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_key: Option<String>,
    /// Currently-computed stage hash. Always populated unless the
    /// stage cannot be resolved (e.g. a dep path is missing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_hash: Option<String>,
    /// Hash recorded in the lockfile for this stage. `None` when
    /// the stage has never been run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lockfile_stage_hash: Option<String>,
    /// In-flight stages carry the owning run_id so the operator can
    /// call `crab workflow journal show <run_id>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Human-readable description from the `desc:` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    /// DVC-style remote/cache comparison state: `in_sync`,
    /// `new`, `deleted`, `missing`, or `uncached`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_state: Option<String>,
    /// Whether the selected stage hash exists in the local stage cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_cache: Option<bool>,
    /// Whether the selected stage hash has a remote stage-cache ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_cache: Option<bool>,
    /// Internal cacheability bit used to keep remote status honest.
    #[serde(skip)]
    #[schemars(skip)]
    remote_cacheable: bool,
}

/// Payload for `--why <stage>`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct WhyPayload {
    pub stage: String,
    pub stage_hash: String,
    /// `None` when no lockfile entry exists for the stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lockfile_stage_hash: Option<String>,
    /// `true` when the recomputed hash matches the lockfile.
    pub up_to_date: bool,
    /// Current inputs.
    pub current: Inputs,
    /// Lockfile inputs. `None` when never-run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lockfile: Option<Inputs>,
    /// List of diffs keyed by category.
    pub diffs: Vec<FieldDiff>,
}

/// Snapshot of a stage's hash inputs used by `--why`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Inputs {
    pub cmd: CmdView,
    /// Ordered by key for stable output.
    pub deps: BTreeMap<String, String>,
    pub params: BTreeMap<String, String>,
    pub env: EnvView,
}

/// Canonical command view for `--why` output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CmdView {
    Argv { argv: Vec<String> },
    Shell { shell: String },
    ShellList { commands: Vec<String> },
}

/// Canonical env view for `--why` output. Mirrors `EnvSpec` but
/// flattens the allowlist so it round-trips through JSON cleanly.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum EnvView {
    Inherit,
    Allowlist { vars: Vec<String> },
    Empty,
}

/// One field-level difference emitted in the `--why` payload.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FieldDiff {
    /// One of `dep`, `param`, `env`, `cmd`.
    pub category: String,
    pub key: String,
    /// Hex hash (`dep`) or stringified value (`param`, `env`, `cmd`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lockfile: Option<String>,
}

// ─── Entry points ─────────────────────────────────────────────────────

/// CLI entry point. Dispatched from `main.rs`.
pub fn exec(args: StatusArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    crate::cmd::lfs::block_on_runtime(run_async(&args, &cwd, args.output_mode()))
}

/// Async entry point used by the top-level async dispatcher.
pub async fn exec_async(args: StatusArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_async(&args, &cwd, args.output_mode()).await
}

/// Testable variant that accepts an explicit `repo_root`.
pub fn run(args: &StatusArgs, repo_root: &Path, mode: OutputMode) -> Result<()> {
    run_local(args, repo_root, mode)
}

/// Testable async variant that also supports remote cache status.
pub async fn run_async(args: &StatusArgs, repo_root: &Path, mode: OutputMode) -> Result<()> {
    if !args.cloud && args.remote.is_none() {
        return run_local(args, repo_root, mode);
    }

    let config = Config::resolve_for_repo(repo_root)?;
    let Some(mut entries) = status_entries(args, repo_root, &config, mode)? else {
        return Ok(());
    };
    let remote = attach_remote_status(&mut entries, args, repo_root, &config).await?;
    emit_status(&entries, mode, Some(remote));
    Ok(())
}

fn run_local(args: &StatusArgs, repo_root: &Path, mode: OutputMode) -> Result<()> {
    let config = Config::resolve_for_repo(repo_root)?;
    let Some(entries) = status_entries(args, repo_root, &config, mode)? else {
        return Ok(());
    };
    emit_status(&entries, mode, None);
    Ok(())
}

fn status_entries(
    args: &StatusArgs,
    repo_root: &Path,
    config: &Config,
    mode: OutputMode,
) -> Result<Option<Vec<StageStatusEntry>>> {
    // Config gate: an explicit workflow opt-out must apply to read-only
    // status too; otherwise status could claim a lockfile this binary did
    // not produce.
    if !config.workflow.enabled {
        return Err(CrabError::WorkflowDisabled);
    }

    // Discover and merge the workflow yaml(s). Errors propagate —
    // the user's intent is to see status for a declared workflow;
    // an unparseable yaml is a hard fail, same as `crab run`.
    let discover_mode = effective_discover_mode(args, &config);
    let discovered = discover::discover(repo_root, discover_mode)?;
    if discovered.is_empty() {
        return Err(CrabError::Configuration {
            key: "no crab.yaml found; workflow status requires a declared workflow".into(),
            origin: "cli".into(),
        });
    }
    // Parse with provenance so the split-lockfile loader knows
    // which per-file lockfile each stage's recorded hash lives in.
    let (workflow, _provenance) = discover::parse_all_with_provenance(repo_root, &discovered)?;

    // `--lockfile` override wins for ad-hoc inspection (tests and
    // scratch debugging). Otherwise the mode flows from config: in
    // Single we read the monolithic file, in Split we merge every
    // per-workflow lockfile into one in-memory view.
    let lockfile = if let Some(explicit) = args.lockfile.clone() {
        Lockfile::load(&explicit)?
    } else {
        let mode = match config.workflow.lockfile {
            crate::core::config::WorkflowLockfile::Single => {
                crate::workflow::lockfile_split::LockfileMode::Single
            }
            crate::core::config::WorkflowLockfile::Split => {
                crate::workflow::lockfile_split::LockfileMode::Split
            }
        };
        crate::workflow::lockfile_split::load_lockfiles(repo_root, &discovered, mode)?
    };

    let workflow_root = repo_root.join(".crab").join("workflow");
    let in_flight = scan_in_flight(&workflow_root);

    if let Some(stage_str) = &args.why {
        let stage_name = StageName::parse_effective(stage_str)?;
        let stage = workflow
            .stages
            .get(&stage_name)
            .ok_or_else(|| CrabError::Configuration {
                key: format!("--why: stage '{stage_str}' not declared in crab.yaml"),
                origin: "cli".into(),
            })?;
        let remote_aliases = workflow_remote_aliases(&config);
        let payload = build_why_payload(
            stage,
            &workflow.params,
            &lockfile,
            repo_root,
            &remote_aliases,
        )?;
        emit_why(&payload, mode);
        return Ok(None);
    }

    let remote_aliases = workflow_remote_aliases(&config);
    Ok(Some(
        match select_status_stages(&args.targets, args.with_deps, &workflow)? {
            Some(selected) => build_selected_status_entries(
                &workflow,
                &lockfile,
                &in_flight,
                repo_root,
                &selected,
                &remote_aliases,
            ),
            None => {
                build_status_entries(&workflow, &lockfile, &in_flight, repo_root, &remote_aliases)
            }
        },
    ))
}

fn workflow_remote_aliases(config: &Config) -> BTreeMap<String, String> {
    config
        .workflow
        .remotes
        .iter()
        .map(|(name, remote)| (name.clone(), remote.url.clone()))
        .collect()
}

// ─── Status entry construction ────────────────────────────────────────

fn effective_discover_mode(args: &StatusArgs, config: &Config) -> DiscoverMode {
    use crate::core::config::WorkflowDiscover;
    if args.recursive {
        return DiscoverMode::Recursive;
    }
    match config.workflow.discover {
        WorkflowDiscover::Root => DiscoverMode::Root,
        WorkflowDiscover::Recursive => DiscoverMode::Recursive,
    }
}

/// Scan `.crab/workflow/runs/` and return a map from stage name to
/// run_id for every non-terminal stage row across all open journals.
/// A stage is "in-flight" in the user-visible sense when some prior
/// run has started it but hasn't yet reached `Committed` / `Failed`
/// / `Aborted`. Best-effort: a journal we can't open is silently
/// skipped — the user isn't blocked on status for journals that
/// would also be unreadable by `crab run`.
fn scan_in_flight(workflow_root: &Path) -> BTreeMap<String, String> {
    let runs_dir = workflow_root.join("runs");
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(&runs_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let run_id_name = entry.file_name().to_string_lossy().into_owned();
        if uuid::Uuid::parse_str(&run_id_name).is_err() {
            continue;
        }
        let journal_path = entry.path().join("journal.db");
        let Ok(journal) = Journal::open(&journal_path) else {
            continue;
        };
        let Ok(run_id) = uuid::Uuid::parse_str(&run_id_name) else {
            continue;
        };
        // A terminated run can't hold in-flight stages even if its
        // rows are still non-`Committed` — `Failed`/`Aborted` are
        // terminal themselves, and `stages_not_committed` excludes
        // them.
        if journal.run_outcome(run_id).ok().flatten().is_some() {
            continue;
        }
        if let Ok(rows) = journal.stages_not_committed(run_id) {
            for row in rows {
                out.entry(row.stage_name)
                    .or_insert_with(|| run_id.to_string());
            }
        }
    }
    out
}

/// Build one [`StageStatusEntry`] per declared stage. The output is
/// stable-ordered by stage name because `Workflow::stages` is a
/// `BTreeMap<StageName, _>`.
fn build_status_entries(
    workflow: &Workflow,
    lockfile: &Lockfile,
    in_flight: &BTreeMap<String, String>,
    repo_root: &Path,
    remote_aliases: &BTreeMap<String, String>,
) -> Vec<StageStatusEntry> {
    let selected = workflow.stages.keys().cloned().collect();
    build_selected_status_entries(
        workflow,
        lockfile,
        in_flight,
        repo_root,
        &selected,
        remote_aliases,
    )
}

fn build_selected_status_entries(
    workflow: &Workflow,
    lockfile: &Lockfile,
    in_flight: &BTreeMap<String, String>,
    repo_root: &Path,
    selected: &BTreeSet<StageName>,
    remote_aliases: &BTreeMap<String, String>,
) -> Vec<StageStatusEntry> {
    workflow
        .stages
        .iter()
        .filter(|(name, _)| selected.contains(*name))
        .map(|(name, stage)| {
            build_entry(
                name,
                stage,
                &workflow.params,
                lockfile,
                in_flight,
                repo_root,
                remote_aliases,
            )
        })
        .collect()
}

fn select_status_stages(
    targets: &[String],
    with_deps: bool,
    workflow: &Workflow,
) -> Result<Option<BTreeSet<StageName>>> {
    if targets.is_empty() {
        return Ok(None);
    }

    let mut selected = BTreeSet::new();
    for target in targets {
        selected.extend(stages_for_status_target(target, workflow)?);
    }

    if with_deps {
        let graph = Graph::build(&workflow.stages)?;
        selected = upstream_closure(selected, &graph);
    }

    Ok(Some(selected))
}

fn stages_for_status_target(target: &str, workflow: &Workflow) -> Result<BTreeSet<StageName>> {
    if let Ok(name) = crate::cmd::run::stage_target_name(target)
        && workflow.stages.contains_key(&name)
    {
        return Ok(BTreeSet::from([name]));
    }

    let target_path = status_target_path(target)?;
    if workflow_file_target(&target_path) {
        return Ok(workflow.stages.keys().cloned().collect());
    }

    let mut matched = BTreeSet::new();
    for (name, stage) in &workflow.stages {
        if stage_declares_output_target(stage, &target_path) {
            matched.insert(name.clone());
        }
    }
    if matched.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("status target '{target}' matched no workflow stage"),
            origin: "cli".into(),
        });
    }
    Ok(matched)
}

fn status_target_path(target: &str) -> Result<PathBuf> {
    let path = Path::new(target);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(CrabError::Configuration {
            key: format!("status target '{target}'"),
            origin: "target path must be repo-relative".into(),
        });
    }
    normalize_status_path(path).ok_or_else(|| CrabError::Configuration {
        key: format!("status target '{target}'"),
        origin: "target path must not contain '..' components".into(),
    })
}

fn workflow_file_target(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "crab.yaml" | "dvc.yaml"))
}

fn stage_declares_output_target(stage: &Stage, target: &Path) -> bool {
    stage.outs.iter().any(|out| {
        let Some(path) = stage_repo_relative_path(&out.path, stage.wdir.as_deref()) else {
            return false;
        };
        match out.kind {
            OutKind::Directory => target == path || target.starts_with(&path),
            OutKind::File | OutKind::Stdout => target == path,
        }
    })
}

fn stage_repo_relative_path(path: &Path, wdir: Option<&Path>) -> Option<PathBuf> {
    if path.is_absolute() {
        return normalize_status_path(path);
    }
    match wdir {
        Some(w) => normalize_status_path(&w.join(path)),
        None => normalize_status_path(path),
    }
}

fn normalize_status_path(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => out.push(part),
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

fn upstream_closure(seed: BTreeSet<StageName>, graph: &Graph) -> BTreeSet<StageName> {
    let mut selected = seed.clone();
    let mut queue: Vec<StageName> = seed.into_iter().collect();
    while let Some(stage) = queue.pop() {
        for producer in graph.producers_of(&stage) {
            if selected.insert(producer.clone()) {
                queue.push(producer);
            }
        }
    }
    selected
}

fn build_entry(
    name: &StageName,
    stage: &Stage,
    param_files: &[PathBuf],
    lockfile: &Lockfile,
    in_flight: &BTreeMap<String, String>,
    repo_root: &Path,
    remote_aliases: &BTreeMap<String, String>,
) -> StageStatusEntry {
    let name_str = name.as_str().to_owned();
    let desc = stage.desc.clone();

    // In-flight wins over everything else — the user needs to know
    // another runner is mid-execution before worrying about cache
    // state (which is stale by definition while a run is writing to
    // it).
    if let Some(run_id) = in_flight.get(&name_str) {
        return StageStatusEntry {
            stage: name_str,
            state: "in_flight".to_owned(),
            reason: None,
            changed_key: None,
            stage_hash: None,
            lockfile_stage_hash: None,
            run_id: Some(run_id.clone()),
            desc,
            remote_state: None,
            local_cache: None,
            remote_cache: None,
            remote_cacheable: stage.remote_cache_push_enabled(),
        };
    }

    // Frozen stages are always reported as frozen regardless of
    // lockfile or dep state.
    if stage.frozen {
        return StageStatusEntry {
            stage: name_str,
            state: "frozen".to_owned(),
            reason: Some("skipped".to_owned()),
            changed_key: None,
            stage_hash: None,
            lockfile_stage_hash: lockfile.get(name).map(|l| l.stage_hash.as_hex()),
            run_id: None,
            desc,
            remote_state: None,
            local_cache: None,
            remote_cache: None,
            remote_cacheable: stage.remote_cache_push_enabled(),
        };
    }

    let locked = lockfile.get(name);
    if stage.always_changed() {
        return StageStatusEntry {
            stage: name_str,
            state: "stale".to_owned(),
            reason: Some("always_changed".to_owned()),
            changed_key: None,
            stage_hash: resolve_current_hash(stage, param_files, repo_root, remote_aliases)
                .ok()
                .map(|hash| hash.as_hex()),
            lockfile_stage_hash: locked.map(|l| l.stage_hash.as_hex()),
            run_id: None,
            desc,
            remote_state: None,
            local_cache: None,
            remote_cache: None,
            remote_cacheable: stage.remote_cache_push_enabled(),
        };
    }

    let Ok((current_hash, current_deps)) =
        resolve_current_inputs(stage, param_files, repo_root, remote_aliases)
    else {
        return StageStatusEntry {
            stage: name_str,
            state: "stale".to_owned(),
            reason: Some("dep".to_owned()),
            changed_key: None,
            stage_hash: None,
            lockfile_stage_hash: locked.map(|l| l.stage_hash.as_hex()),
            run_id: None,
            desc,
            remote_state: None,
            local_cache: None,
            remote_cache: None,
            remote_cacheable: stage.remote_cache_push_enabled(),
        };
    };
    let Some(locked) = locked else {
        return StageStatusEntry {
            stage: name_str,
            state: "never_run".to_owned(),
            reason: None,
            changed_key: None,
            stage_hash: Some(current_hash.as_hex()),
            lockfile_stage_hash: None,
            run_id: None,
            desc,
            remote_state: None,
            local_cache: None,
            remote_cache: None,
            remote_cacheable: stage.remote_cache_push_enabled(),
        };
    };

    if current_hash == locked.stage_hash {
        return StageStatusEntry {
            stage: name_str,
            state: "up_to_date".to_owned(),
            reason: None,
            changed_key: None,
            stage_hash: Some(current_hash.as_hex()),
            lockfile_stage_hash: Some(locked.stage_hash.as_hex()),
            run_id: None,
            desc,
            remote_state: None,
            local_cache: None,
            remote_cache: None,
            remote_cacheable: stage.remote_cache_push_enabled(),
        };
    }

    // Stale: find the first differing input so the text-mode caller
    // can show an actionable reason without dumping every diff.
    let (reason, changed_key) = classify_stale(
        stage,
        param_files,
        locked,
        repo_root,
        remote_aliases,
        Some(&current_deps),
    );
    StageStatusEntry {
        stage: name_str,
        state: "stale".to_owned(),
        reason: Some(reason),
        changed_key,
        stage_hash: Some(current_hash.as_hex()),
        lockfile_stage_hash: Some(locked.stage_hash.as_hex()),
        run_id: None,
        desc,
        remote_state: None,
        local_cache: None,
        remote_cache: None,
        remote_cacheable: stage.remote_cache_push_enabled(),
    }
}

async fn attach_remote_status(
    entries: &mut [StageStatusEntry],
    args: &StatusArgs,
    repo_root: &Path,
    config: &Config,
) -> Result<RemoteStatusSummary> {
    let cancel = CancellationToken::new();
    let (store, prefix) = crate::cmd::workflow::build_remote_store_for(
        repo_root,
        config,
        args.remote.as_deref(),
        &cancel,
    )
    .await?;
    let remote_url =
        crate::cmd::workflow::read_crab_remote_url_for(repo_root, args.remote.as_deref())?;
    let cache_root = repo_root.join(".crab").join("cache");
    let mut summary = RemoteStatusSummary {
        remote: remote_url,
        checked: 0,
        in_sync: 0,
        new: 0,
        deleted: 0,
        missing: 0,
        uncached: 0,
    };

    for entry in entries {
        summary.checked += 1;
        if !entry.remote_cacheable {
            entry.local_cache = Some(false);
            entry.remote_cache = Some(false);
            entry.remote_state = Some("uncached".to_owned());
            summary.uncached += 1;
            continue;
        }

        let Some(hash) = entry
            .stage_hash
            .as_deref()
            .or(entry.lockfile_stage_hash.as_deref())
            .map(stage_hash_from_hex)
            .transpose()?
        else {
            entry.local_cache = Some(false);
            entry.remote_cache = Some(false);
            entry.remote_state = Some("missing".to_owned());
            summary.missing += 1;
            continue;
        };

        let local = read_local(&cache_root, &hash)?.is_some();
        let remote = remote_stage_ref_exists(&store, &prefix, &hash).await?;
        let state = match (local, remote) {
            (true, true) => {
                summary.in_sync += 1;
                "in_sync"
            }
            (true, false) => {
                summary.new += 1;
                "new"
            }
            (false, true) => {
                summary.deleted += 1;
                "deleted"
            }
            (false, false) => {
                summary.missing += 1;
                "missing"
            }
        };
        entry.local_cache = Some(local);
        entry.remote_cache = Some(remote);
        entry.remote_state = Some(state.to_owned());
    }

    Ok(summary)
}

async fn remote_stage_ref_exists(
    store: &crate::workflow::WorkflowStore,
    prefix: &str,
    stage_hash: &StageHash,
) -> Result<bool> {
    let path = remote_stage_ref_path(prefix, stage_hash);
    match store.head(&path).await {
        Ok(_) => Ok(true),
        Err(crate::workflow::WorkflowError::NotFound { .. }) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn stage_hash_from_hex(hex: &str) -> Result<StageHash> {
    if hex.len() != 64 {
        return Err(CrabError::Configuration {
            key: format!("stage hash '{hex}'"),
            origin: "expected 64 lowercase hex characters".into(),
        });
    }

    let mut out = [0u8; 32];
    for (idx, slot) in out.iter_mut().enumerate() {
        let start = idx * 2;
        *slot = u8::from_str_radix(&hex[start..start + 2], 16).map_err(|_| {
            CrabError::Configuration {
                key: format!("stage hash '{hex}'"),
                origin: "expected lowercase hex characters".into(),
            }
        })?;
    }
    Ok(StageHash(out))
}

/// Compute the current stage hash for `stage`, resolving declared
/// deps to whole-file Blake3 hashes or URL dep digests. Remote /
/// stage-out / non-HTTP URL deps that this read-only probe can't
/// resolve bubble up as `StageRemoteExecutionUnsupported`; the caller
/// converts that to a stale-dep entry.
fn resolve_current_hash(
    stage: &Stage,
    param_files: &[PathBuf],
    repo_root: &Path,
    remote_aliases: &BTreeMap<String, String>,
) -> Result<StageHash> {
    resolve_current_inputs(stage, param_files, repo_root, remote_aliases).map(|(hash, _)| hash)
}

fn resolve_current_inputs(
    stage: &Stage,
    param_files: &[PathBuf],
    repo_root: &Path,
    remote_aliases: &BTreeMap<String, String>,
) -> Result<(StageHash, BTreeMap<String, [u8; 32]>)> {
    let dep_hashes = resolve_path_dep_hashes(stage, repo_root, remote_aliases)?;
    let params = resolve_stage_param_values_with_wdir(
        repo_root,
        param_files,
        &stage.params,
        stage.name.as_str(),
        stage.wdir.as_deref(),
    )?;
    let resolved = ResolvedStage {
        stage: stage.clone(),
        dep_hashes: dep_hashes.clone(),
        params,
        env: stage.env.clone(),
        cmd: stage.cmd.clone(),
        outs: stage.outs.clone(),
    };
    Ok((compute_stage_hash(&resolved), dep_hashes))
}

/// Single-stage read-only dep resolver for the status command.
/// Mirrors the miss-path resolver in `cmd/run.rs` but does not
/// require a run journal — status is a pure read.
///
/// Directory deps are treated as path deps and hashed via the
/// tree-manifest hasher. URL deps use their declared digest or hash
/// live HTTP(S) bytes. Other non-path deps are not resolvable without
/// the executor's composite resolver; they surface as
/// `StageRemoteExecutionUnsupported` and the caller classifies the
/// stage as stale-dep.
fn resolve_path_dep_hashes(
    stage: &Stage,
    repo_root: &Path,
    remote_aliases: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, [u8; 32]>> {
    let wdir = stage.wdir.as_deref();
    let mut out = BTreeMap::new();
    for dep in &stage.deps {
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
                            stage: stage.name.as_str().to_owned(),
                            path: p.clone(),
                        }
                    } else {
                        CrabError::Io(e)
                    }
                })?;
                if meta.is_dir() {
                    let tree = crate::workflow::hasher::hash_directory(&abs, true)?;
                    let key = status_repo_relative_dep_key(p, wdir);
                    out.insert(key, tree.hash);
                    continue;
                }
                if !meta.is_file() {
                    return Err(CrabError::StageDepMalformed {
                        stage: stage.name.as_str().to_owned(),
                        path: p.clone(),
                        reason: "non-regular dep entry",
                    });
                }
                let mut hasher = blake3::Hasher::new();
                let mut file = std::fs::File::open(&abs).map_err(CrabError::Io)?;
                std::io::copy(&mut file, &mut hasher).map_err(CrabError::Io)?;
                let digest: [u8; 32] = *hasher.finalize().as_bytes();
                let key = status_repo_relative_dep_key(p, wdir);
                out.insert(key, digest);
            }
            Dep::Url { .. } => {
                let Some((key, digest)) = dep.url_hash_with_remote_aliases_and_index(
                    remote_aliases,
                    Some(&repo_root.join(".crab/workflow/external-hashes.json")),
                )?
                else {
                    continue;
                };
                out.insert(key, digest);
            }
            // Non-path deps without an explicit digest need the full
            // composite resolver and a valid run journal / lockfile
            // context. Out of scope for read-only status — the
            // caller classifies the stage as stale on this error.
            Dep::StageOut { .. }
            | Dep::CrabRef { .. }
            | Dep::GitRef { .. }
            | Dep::OciImage { .. } => {
                return Err(CrabError::StageRemoteExecutionUnsupported);
            }
        }
    }
    Ok(out)
}

/// Build the repo-relative key for a dep path in the status command.
/// When `wdir` is set and the path is relative, prepend `wdir/` so
/// the key matches what the lockfile stores.
fn status_repo_relative_dep_key(p: &std::path::Path, wdir: Option<&std::path::Path>) -> String {
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

/// Return `(reason, changed_key)` for a stale stage.
///
/// Checks in a deterministic order: deps, then params, then env,
/// then cmd. First mismatch wins — that matches the cache-miss
/// reason the user sees in `--explain-miss`.
fn classify_stale(
    stage: &Stage,
    param_files: &[PathBuf],
    locked: &LockedStage,
    repo_root: &Path,
    remote_aliases: &BTreeMap<String, String>,
    current_deps: Option<&BTreeMap<String, [u8; 32]>>,
) -> (String, Option<String>) {
    // Deps: recompute hashes for every path dep and compare against
    // the lockfile. A dep that's recorded but now missing, or a new
    // dep added to yaml, surfaces here.
    let resolved_deps = current_deps
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| resolve_path_dep_hashes(stage, repo_root, remote_aliases));
    match resolved_deps {
        Ok(current_deps) => {
            let locked_deps: BTreeMap<String, [u8; 32]> = locked
                .deps
                .iter()
                .map(|d| (d.path.to_string_lossy().into_owned(), d.hash))
                .collect();
            if let Some(key) = first_map_diff(&current_deps, &locked_deps) {
                return ("dep".to_owned(), Some(key));
            }
        }
        Err(_) => {
            // Dep resolution failed — that's a dep change (probably
            // a missing file). Classify as stale-dep without a
            // specific key since we can't enumerate.
            return ("dep".to_owned(), None);
        }
    }

    match resolve_stage_param_values_with_wdir(
        repo_root,
        param_files,
        &stage.params,
        stage.name.as_str(),
        stage.wdir.as_deref(),
    ) {
        Ok(current_params) => {
            if let Some(key) = first_map_diff(&current_params, &locked.params) {
                return ("param".to_owned(), Some(key));
            }
        }
        Err(_) => return ("param".to_owned(), None),
    }

    // Env: the lockfile records the resolved key=value pairs under
    // the stage's `EnvSpec::Allowlist`. For the declared stage we
    // can compare the allowlist set directly; Inherit / Empty flip
    // is a category change and surfaces as `env` with no key.
    match diff_env(stage, locked) {
        EnvDiff::Equal => {}
        EnvDiff::KeyChanged(key) => return ("env".to_owned(), Some(key)),
        EnvDiff::PolicyChanged => return ("env".to_owned(), None),
    }

    // Cmd: last resort. If deps, params, and env all match, the
    // cmd must be what's different — otherwise we wouldn't have
    // gotten a stage-hash mismatch.
    ("cmd".to_owned(), None)
}

/// Return the first key that differs between `a` and `b`. The key
/// space is the union of both maps; a key present in one and absent
/// in the other counts as a difference.
fn first_map_diff<V: Eq>(a: &BTreeMap<String, V>, b: &BTreeMap<String, V>) -> Option<String> {
    // Walk the union in sorted order. `BTreeMap` iteration is
    // already sorted so a merge-style walk terminates on the first
    // mismatch without allocating the full union.
    let mut ai = a.iter().peekable();
    let mut bi = b.iter().peekable();
    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return None,
            (Some((ak, _)), None) => return Some((*ak).clone()),
            (None, Some((bk, _))) => return Some((*bk).clone()),
            (Some((ak, av)), Some((bk, bv))) => match ak.cmp(bk) {
                std::cmp::Ordering::Less => return Some((*ak).clone()),
                std::cmp::Ordering::Greater => return Some((*bk).clone()),
                std::cmp::Ordering::Equal => {
                    if av != bv {
                        return Some((*ak).clone());
                    }
                    ai.next();
                    bi.next();
                }
            },
        }
    }
}

/// Result of comparing env declarations between the current stage
/// and its lockfile entry.
enum EnvDiff {
    /// Declarations are cache-compatible (same policy, same keys).
    Equal,
    /// A specific allowlist var was added or removed.
    KeyChanged(String),
    /// Policy itself (Inherit / Allowlist / Empty) changed.
    PolicyChanged,
}

/// Diff env declarations. Returns [`EnvDiff`] rather than a nested
/// `Option` so the three outcomes stay distinguishable at the call
/// site.
fn diff_env(stage: &Stage, locked: &LockedStage) -> EnvDiff {
    use crate::workflow::stage::EnvSpec;
    match &stage.env {
        EnvSpec::Inherit => {
            // Inherit doesn't contribute values to the lockfile; the
            // lockfile's env map is informational. Treat as matching
            // iff the lockfile env is empty — otherwise the stage
            // previously ran under a different policy.
            if locked.env.is_empty() {
                EnvDiff::Equal
            } else {
                EnvDiff::PolicyChanged
            }
        }
        EnvSpec::Empty => {
            if locked.env.is_empty() {
                EnvDiff::Equal
            } else {
                EnvDiff::PolicyChanged
            }
        }
        EnvSpec::Allowlist(vars) => {
            // Compare the set of allowlisted vars against the
            // lockfile's recorded env keys. A var newly added or
            // removed in yaml is the reason for the stale-env state.
            let current: BTreeMap<String, String> =
                vars.iter().map(|v| (v.clone(), String::new())).collect();
            let locked_keys: BTreeMap<String, String> = locked
                .env
                .keys()
                .map(|k| (k.clone(), String::new()))
                .collect();
            match first_map_diff(&current, &locked_keys) {
                Some(key) => EnvDiff::KeyChanged(key),
                None => EnvDiff::Equal,
            }
        }
    }
}

// ─── `--why` payload ──────────────────────────────────────────────────

fn build_why_payload(
    stage: &Stage,
    param_files: &[PathBuf],
    lockfile: &Lockfile,
    repo_root: &Path,
    remote_aliases: &BTreeMap<String, String>,
) -> Result<WhyPayload> {
    // Failing to resolve deps for `--why` is a useful error to
    // surface: the user explicitly asked for detail on this stage.
    let dep_hashes = resolve_path_dep_hashes(stage, repo_root, remote_aliases)?;
    let params = resolve_stage_param_values_with_wdir(
        repo_root,
        param_files,
        &stage.params,
        stage.name.as_str(),
        stage.wdir.as_deref(),
    )?;
    let current_hash = {
        let resolved = ResolvedStage {
            stage: stage.clone(),
            dep_hashes: dep_hashes.clone(),
            params: params.clone(),
            env: stage.env.clone(),
            cmd: stage.cmd.clone(),
            outs: stage.outs.clone(),
        };
        compute_stage_hash(&resolved)
    };

    let current_inputs = Inputs {
        cmd: cmd_view(&stage.cmd),
        deps: dep_hashes
            .iter()
            .map(|(k, v)| (k.clone(), hex_lower(v)))
            .collect(),
        params,
        env: env_view(&stage.env),
    };

    let locked = lockfile.get(&stage.name);
    let lockfile_inputs = locked.map(|l| Inputs {
        cmd: cmd_view_cached(&l.cmd),
        deps: l
            .deps
            .iter()
            .map(|d| (d.path.to_string_lossy().into_owned(), hex_lower(&d.hash)))
            .collect(),
        params: l.params.clone(),
        env: EnvView::Allowlist {
            vars: l.env.keys().cloned().collect(),
        },
    });

    let diffs = build_diffs(
        &current_inputs,
        lockfile_inputs.as_ref(),
        &stage.cmd,
        locked,
    );

    Ok(WhyPayload {
        stage: stage.name.as_str().to_owned(),
        stage_hash: current_hash.as_hex(),
        lockfile_stage_hash: locked.map(|l| l.stage_hash.as_hex()),
        up_to_date: locked.is_some_and(|l| l.stage_hash == current_hash),
        current: current_inputs,
        lockfile: lockfile_inputs,
        diffs,
    })
}

fn build_diffs(
    current: &Inputs,
    lockfile: Option<&Inputs>,
    current_cmd: &Cmd,
    locked: Option<&LockedStage>,
) -> Vec<FieldDiff> {
    let mut out = Vec::new();
    let Some(lf) = lockfile else {
        return out;
    };

    // Deps — one entry per key that differs.
    collect_map_diffs(&current.deps, &lf.deps, "dep", &mut out);

    // Params.
    collect_map_diffs(&current.params, &lf.params, "param", &mut out);

    // Env (allowlist keys only).
    let current_env_keys: BTreeMap<String, String> = match &current.env {
        EnvView::Allowlist { vars } => vars.iter().map(|v| (v.clone(), String::new())).collect(),
        EnvView::Inherit | EnvView::Empty => BTreeMap::new(),
    };
    let lock_env_keys: BTreeMap<String, String> = match &lf.env {
        EnvView::Allowlist { vars } => vars.iter().map(|v| (v.clone(), String::new())).collect(),
        EnvView::Inherit | EnvView::Empty => BTreeMap::new(),
    };
    collect_map_diffs(&current_env_keys, &lock_env_keys, "env", &mut out);

    // Cmd — a single diff entry when the serialized form changed.
    if let Some(lst) = locked {
        let current_str = cmd_string(current_cmd);
        let lock_str = cached_cmd_string(&lst.cmd);
        if current_str != lock_str {
            out.push(FieldDiff {
                category: "cmd".to_owned(),
                key: "cmd".to_owned(),
                current: Some(current_str),
                lockfile: Some(lock_str),
            });
        }
    }

    out
}

fn collect_map_diffs(
    current: &BTreeMap<String, String>,
    lockfile: &BTreeMap<String, String>,
    category: &'static str,
    out: &mut Vec<FieldDiff>,
) {
    // Union walk: every key appearing in either side either matches
    // or produces one diff row.
    let all_keys: std::collections::BTreeSet<&String> =
        current.keys().chain(lockfile.keys()).collect();
    for key in all_keys {
        let cur = current.get(key);
        let lck = lockfile.get(key);
        if cur != lck {
            out.push(FieldDiff {
                category: category.to_owned(),
                key: key.clone(),
                current: cur.cloned(),
                lockfile: lck.cloned(),
            });
        }
    }
}

fn cmd_view(cmd: &Cmd) -> CmdView {
    match cmd {
        Cmd::Argv(args) => CmdView::Argv { argv: args.clone() },
        Cmd::Shell(s) => CmdView::Shell { shell: s.clone() },
        Cmd::ShellList(commands) => CmdView::ShellList {
            commands: commands.clone(),
        },
    }
}

fn cmd_view_cached(cmd: &CachedCmd) -> CmdView {
    match cmd {
        CachedCmd::Argv { argv } => CmdView::Argv { argv: argv.clone() },
        CachedCmd::Shell { shell } => CmdView::Shell {
            shell: shell.clone(),
        },
        CachedCmd::ShellList { commands } => CmdView::ShellList {
            commands: commands.clone(),
        },
    }
}

fn env_view(env: &crate::workflow::stage::EnvSpec) -> EnvView {
    use crate::workflow::stage::EnvSpec;
    match env {
        EnvSpec::Inherit => EnvView::Inherit,
        EnvSpec::Empty => EnvView::Empty,
        EnvSpec::Allowlist(vars) => EnvView::Allowlist { vars: vars.clone() },
    }
}

/// Human-readable rendering of a [`Cmd`] for diff output.
fn cmd_string(cmd: &Cmd) -> String {
    match cmd {
        Cmd::Argv(args) => format!("argv: {args:?}"),
        Cmd::Shell(s) => format!("shell: {s}"),
        Cmd::ShellList(commands) => format!("shells: {commands:?}"),
    }
}

fn cached_cmd_string(cmd: &CachedCmd) -> String {
    match cmd {
        CachedCmd::Argv { argv } => format!("argv: {argv:?}"),
        CachedCmd::Shell { shell } => format!("shell: {shell}"),
        CachedCmd::ShellList { commands } => format!("shells: {commands:?}"),
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

// ─── Output rendering ─────────────────────────────────────────────────

fn emit_status(
    entries: &[StageStatusEntry],
    mode: OutputMode,
    remote: Option<RemoteStatusSummary>,
) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(
                WORKFLOW_STATUS_SCHEMA,
                "1.0",
                StatusPayload {
                    stages: entries.to_vec(),
                    remote,
                },
            );
        }
        OutputMode::Text => {
            if entries.is_empty() {
                println!("Crab workflow status: no stages declared in crab.yaml");
                return;
            }
            println!("Crab workflow status:");
            for e in entries {
                let tag = match e.state.as_str() {
                    "up_to_date" => "up-to-date",
                    "never_run" => "never-run",
                    "in_flight" => "in-flight",
                    "stale" => "stale",
                    other => other,
                };
                let detail = match (
                    e.reason.as_deref(),
                    e.changed_key.as_deref(),
                    e.run_id.as_deref(),
                ) {
                    (Some(reason), Some(key), _) => format!(" (reason: {reason}, key: {key})"),
                    (Some(reason), None, _) => format!(" (reason: {reason})"),
                    (None, _, Some(run_id)) => format!(" (run_id: {run_id})"),
                    _ => String::new(),
                };
                let desc_suffix = match e.desc.as_deref() {
                    Some(d) => format!(" — {d}"),
                    None => String::new(),
                };
                let remote_suffix = match e.remote_state.as_deref() {
                    Some(state) => format!(" [remote: {state}]"),
                    None => String::new(),
                };
                println!(
                    "  {:<30} {}{}{}{}",
                    e.stage, tag, detail, remote_suffix, desc_suffix
                );
            }
            if let Some(remote) = remote {
                println!(
                    "Remote cache: {} checked, {} in sync, {} new, {} deleted, {} missing, {} uncached ({})",
                    remote.checked,
                    remote.in_sync,
                    remote.new,
                    remote.deleted,
                    remote.missing,
                    remote.uncached,
                    remote.remote
                );
            }
        }
    }
}

fn emit_why(payload: &WhyPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(WORKFLOW_STATUS_SCHEMA, "1.0", payload);
        }
        OutputMode::Text => {
            info!(
                stage = %payload.stage,
                stage_hash = %payload.stage_hash,
                up_to_date = payload.up_to_date,
                diffs = payload.diffs.len(),
                "workflow: stage input-hash breakdown"
            );
            for d in &payload.diffs {
                info!(
                    category = %d.category,
                    key = %d.key,
                    current = d.current.as_deref().unwrap_or("<absent>"),
                    lockfile = d.lockfile.as_deref().unwrap_or("<absent>"),
                    "diff"
                );
            }
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use crate::workflow::cache::{CachedCmd, CachedOut, StageCacheEntry};
    use crate::workflow::stage::{Cmd, EnvSpec, ParamRef, Stage, StageName};
    use crab_workflow::LockedDep;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    fn write_yaml(root: &Path, body: &str) {
        fs::write(root.join("crab.yaml"), body).unwrap();
    }

    /// Compute a stage hash and populate a cache entry matching it
    /// so [`Lockfile::upsert`] produces a lockfile row the status
    /// command recognizes as up-to-date.
    fn seed_lockfile(lockfile: &mut Lockfile, stage: &Stage, dep_hash: [u8; 32], dep_path: &str) {
        let resolved = ResolvedStage {
            stage: stage.clone(),
            dep_hashes: {
                let mut m = BTreeMap::new();
                m.insert(dep_path.to_owned(), dep_hash);
                m
            },
            params: BTreeMap::new(),
            env: stage.env.clone(),
            cmd: stage.cmd.clone(),
            outs: stage.outs.clone(),
        };
        let stage_hash = compute_stage_hash(&resolved);

        let entry = StageCacheEntry {
            schema_version: 1,
            stage_hash,
            stage_name: stage.name.as_str().to_owned(),
            cmd: match &stage.cmd {
                Cmd::Argv(a) => CachedCmd::Argv { argv: a.clone() },
                Cmd::Shell(s) => CachedCmd::Shell { shell: s.clone() },
                Cmd::ShellList(commands) => CachedCmd::ShellList {
                    commands: commands.clone(),
                },
            },
            outs: stage
                .outs
                .iter()
                .map(|o| CachedOut {
                    path: o.path.clone(),
                    kind: o.kind,
                    push: o.push,
                    remote: o.remote.clone(),
                    file_hash: format!("b3:{}", hex_lower(&[0u8; 32])),
                    size: 0,
                    mode: 0o644,
                    tree_manifest: None,
                })
                .collect(),
            metrics: Vec::new(),
            plots: Vec::new(),
            executed_at: "2024-01-01T00:00:00.000Z".into(),
            duration_ms: 1,
            exec_id: None,
            attempts: 1,
            host_fingerprint: "test-host".into(),
        };

        let locked_deps = vec![LockedDep {
            path: PathBuf::from(dep_path),
            hash: dep_hash,
            size: 0,
        }];
        lockfile
            .upsert(&entry, locked_deps, BTreeMap::new(), BTreeMap::new())
            .unwrap();
    }

    fn hash_file(path: &Path) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        let mut file = std::fs::File::open(path).unwrap();
        std::io::copy(&mut file, &mut hasher).unwrap();
        *hasher.finalize().as_bytes()
    }

    #[test]
    fn args_accept_dvc_remote_status_spellings() {
        let args =
            StatusArgs::try_parse_from(["status", "--cloud", "--remote", "origin", "--json"])
                .unwrap();

        assert!(args.cloud);
        assert_eq!(args.remote.as_deref(), Some("origin"));
        assert!(args.json);
    }

    #[test]
    fn stage_hash_from_hex_requires_full_hex_digest() {
        let valid = "01".repeat(32);
        assert_eq!(stage_hash_from_hex(&valid).unwrap().as_hex(), valid);
        assert!(stage_hash_from_hex("abcd").is_err());
        assert!(stage_hash_from_hex(&format!("{}zz", "01".repeat(31))).is_err());
    }

    #[test]
    fn matching_lockfile_reports_up_to_date() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_yaml(
            root,
            "stages:\n  build:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env: empty\n",
        );
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("build").unwrap())
            .unwrap()
            .clone();

        let dep_hash = hash_file(&root.join("a.txt"));
        let mut lockfile = Lockfile::new();
        seed_lockfile(&mut lockfile, &stage, dep_hash, "a.txt");

        let entries = build_status_entries(
            &workflow,
            &lockfile,
            &BTreeMap::new(),
            root,
            &BTreeMap::new(),
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, "up_to_date");
        assert!(entries[0].reason.is_none());
    }

    #[test]
    fn pinned_url_dep_reports_up_to_date_when_digest_matches_lockfile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let digest = format!("b3:{}", "34".repeat(32));
        write_yaml(
            root,
            &format!(
                "stages:\n  fetch:\n    cmd: \"python fetch.py\"\n    deps:\n      - url:\n          url: \"https://example.com/data.bin\"\n          digest: \"{digest}\"\n    outs:\n      - data.bin\n    env: empty\n",
            ),
        );

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("fetch").unwrap())
            .unwrap()
            .clone();

        let mut lockfile = Lockfile::new();
        seed_lockfile(
            &mut lockfile,
            &stage,
            [0x34; 32],
            "https://example.com/data.bin",
        );

        let entries = build_status_entries(
            &workflow,
            &lockfile,
            &BTreeMap::new(),
            root,
            &BTreeMap::new(),
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, "up_to_date");
        assert!(entries[0].reason.is_none());
    }

    #[test]
    fn http_url_dep_reports_up_to_date_when_body_matches_lockfile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let url = crate::workflow::stage::test_support::serve_http_body_n(b"status-url-body", 2);
        write_yaml(
            root,
            &format!(
                "stages:\n  fetch:\n    cmd: \"python fetch.py\"\n    deps:\n      - \"{url}\"\n    outs:\n      - data.bin\n    env: empty\n",
            ),
        );

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("fetch").unwrap())
            .unwrap()
            .clone();

        let mut lockfile = Lockfile::new();
        seed_lockfile(
            &mut lockfile,
            &stage,
            *blake3::hash(b"status-url-body").as_bytes(),
            &url,
        );

        let entries = build_status_entries(
            &workflow,
            &lockfile,
            &BTreeMap::new(),
            root,
            &BTreeMap::new(),
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, "up_to_date");
        assert!(entries[0].reason.is_none());
    }

    #[test]
    fn remote_alias_url_dep_reports_up_to_date_when_body_matches_lockfile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let base_url =
            crate::workflow::stage::test_support::serve_http_body_n(b"status-alias-body", 2);
        let base_url = base_url.trim_end_matches("data.bin").to_owned();
        write_yaml(
            root,
            "stages:\n  fetch:\n    cmd: \"python fetch.py\"\n    deps:\n      - \"remote://datasets/raw.csv\"\n    outs:\n      - data.bin\n    env: empty\n",
        );

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("fetch").unwrap())
            .unwrap()
            .clone();

        let mut lockfile = Lockfile::new();
        seed_lockfile(
            &mut lockfile,
            &stage,
            *blake3::hash(b"status-alias-body").as_bytes(),
            "remote://datasets/raw.csv",
        );
        let aliases = BTreeMap::from([("datasets".to_owned(), base_url)]);

        let entries = build_status_entries(&workflow, &lockfile, &BTreeMap::new(), root, &aliases);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, "up_to_date");
        assert!(entries[0].reason.is_none());
    }

    #[test]
    fn http_url_dep_reports_stale_when_body_differs_from_lockfile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let url = crate::workflow::stage::test_support::serve_http_body_n(b"new-url-body", 2);
        write_yaml(
            root,
            &format!(
                "stages:\n  fetch:\n    cmd: \"python fetch.py\"\n    deps:\n      - url:\n          url: \"{url}\"\n    outs:\n      - data.bin\n    env: empty\n",
            ),
        );

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("fetch").unwrap())
            .unwrap()
            .clone();

        let mut lockfile = Lockfile::new();
        seed_lockfile(
            &mut lockfile,
            &stage,
            *blake3::hash(b"old-url-body").as_bytes(),
            &url,
        );

        let entries = build_status_entries(
            &workflow,
            &lockfile,
            &BTreeMap::new(),
            root,
            &BTreeMap::new(),
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, "stale");
        assert_eq!(entries[0].reason.as_deref(), Some("dep"));
        assert_eq!(entries[0].changed_key.as_deref(), Some(url.as_str()));
    }

    #[test]
    fn always_changed_stage_reports_stale_even_when_hash_matches() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_yaml(
            root,
            "stages:\n  poll:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env: empty\n    always_changed: true\n",
        );
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("poll").unwrap())
            .unwrap()
            .clone();

        let dep_hash = hash_file(&root.join("a.txt"));
        let mut lockfile = Lockfile::new();
        seed_lockfile(&mut lockfile, &stage, dep_hash, "a.txt");

        let entries = build_status_entries(
            &workflow,
            &lockfile,
            &BTreeMap::new(),
            root,
            &BTreeMap::new(),
        );
        assert_eq!(entries[0].state, "stale");
        assert_eq!(entries[0].reason.as_deref(), Some("always_changed"));
        assert!(entries[0].stage_hash.is_some());
        assert!(entries[0].lockfile_stage_hash.is_some());
    }

    #[test]
    fn changed_dep_reports_stale_dep() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_yaml(
            root,
            "stages:\n  build:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env: empty\n",
        );
        fs::write(root.join("a.txt"), b"original").unwrap();

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("build").unwrap())
            .unwrap()
            .clone();

        let orig_hash = hash_file(&root.join("a.txt"));
        let mut lockfile = Lockfile::new();
        seed_lockfile(&mut lockfile, &stage, orig_hash, "a.txt");

        // Mutate the dep after seeding — should now report stale-dep.
        fs::write(root.join("a.txt"), b"modified").unwrap();

        let entries = build_status_entries(
            &workflow,
            &lockfile,
            &BTreeMap::new(),
            root,
            &BTreeMap::new(),
        );
        assert_eq!(entries[0].state, "stale");
        assert_eq!(entries[0].reason.as_deref(), Some("dep"));
        assert_eq!(entries[0].changed_key.as_deref(), Some("a.txt"));
    }

    #[test]
    fn missing_lockfile_reports_never_run() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_yaml(
            root,
            "stages:\n  build:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env: empty\n",
        );
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let lockfile = Lockfile::new();

        let entries = build_status_entries(
            &workflow,
            &lockfile,
            &BTreeMap::new(),
            root,
            &BTreeMap::new(),
        );
        assert_eq!(entries[0].state, "never_run");
        assert!(entries[0].lockfile_stage_hash.is_none());
        assert!(entries[0].stage_hash.is_some());
    }

    #[test]
    fn in_flight_stage_reports_in_flight() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_yaml(
            root,
            "stages:\n  build:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env: empty\n",
        );
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let mut in_flight = BTreeMap::new();
        in_flight.insert(
            "build".to_owned(),
            "00000000-0000-0000-0000-000000000001".to_owned(),
        );

        let lockfile = Lockfile::new();
        let entries =
            build_status_entries(&workflow, &lockfile, &in_flight, root, &BTreeMap::new());
        assert_eq!(entries[0].state, "in_flight");
        assert_eq!(
            entries[0].run_id.as_deref(),
            Some("00000000-0000-0000-0000-000000000001")
        );
    }

    #[test]
    fn stale_env_reports_env_category_on_policy_change() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_yaml(
            root,
            "stages:\n  build:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env:\n      - PATH\n",
        );
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("build").unwrap())
            .unwrap()
            .clone();

        // Seed the lockfile using the same stage but with `env = empty`
        // so the recomputed hash differs by env policy alone.
        let mut alt = stage.clone();
        alt.env = EnvSpec::Empty;
        let dep_hash = hash_file(&root.join("a.txt"));
        let mut lockfile = Lockfile::new();
        seed_lockfile(&mut lockfile, &alt, dep_hash, "a.txt");

        let entries = build_status_entries(
            &workflow,
            &lockfile,
            &BTreeMap::new(),
            root,
            &BTreeMap::new(),
        );
        assert_eq!(entries[0].state, "stale");
        assert_eq!(entries[0].reason.as_deref(), Some("env"));
    }

    #[test]
    fn stale_cmd_when_only_cmd_differs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_yaml(
            root,
            "stages:\n  build:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env: empty\n",
        );
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("build").unwrap())
            .unwrap()
            .clone();

        // Seed the lockfile with a different cmd string, same deps.
        let mut alt = stage.clone();
        alt.cmd = Cmd::Shell("echo different".into());
        let dep_hash = hash_file(&root.join("a.txt"));
        let mut lockfile = Lockfile::new();
        seed_lockfile(&mut lockfile, &alt, dep_hash, "a.txt");

        let entries = build_status_entries(
            &workflow,
            &lockfile,
            &BTreeMap::new(),
            root,
            &BTreeMap::new(),
        );
        assert_eq!(entries[0].state, "stale");
        assert_eq!(entries[0].reason.as_deref(), Some("cmd"));
    }

    #[test]
    fn why_payload_lists_diffs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_yaml(
            root,
            "stages:\n  build:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env: empty\n",
        );
        fs::write(root.join("a.txt"), b"new").unwrap();

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("build").unwrap())
            .unwrap()
            .clone();

        let old_hash = [7u8; 32];
        let mut lockfile = Lockfile::new();
        seed_lockfile(&mut lockfile, &stage, old_hash, "a.txt");

        let payload = build_why_payload(&stage, &[], &lockfile, root, &BTreeMap::new()).unwrap();
        assert!(!payload.up_to_date);
        assert!(
            payload
                .diffs
                .iter()
                .any(|d| d.category == "dep" && d.key == "a.txt")
        );
        assert!(payload.lockfile_stage_hash.is_some());
        assert_ne!(
            payload.stage_hash,
            payload.lockfile_stage_hash.as_deref().unwrap()
        );
    }

    #[test]
    fn why_payload_marks_up_to_date_when_matching() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_yaml(
            root,
            "stages:\n  build:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env: empty\n",
        );
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let workflow = discover::parse_all(root, &[root.join("crab.yaml")]).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("build").unwrap())
            .unwrap()
            .clone();

        let dep_hash = hash_file(&root.join("a.txt"));
        let mut lockfile = Lockfile::new();
        seed_lockfile(&mut lockfile, &stage, dep_hash, "a.txt");

        let payload = build_why_payload(&stage, &[], &lockfile, root, &BTreeMap::new()).unwrap();
        assert!(payload.up_to_date);
        assert!(payload.diffs.is_empty());
    }

    #[test]
    fn first_map_diff_detects_added_key() {
        let mut a: BTreeMap<String, [u8; 32]> = BTreeMap::new();
        a.insert("x".into(), [0; 32]);
        let b: BTreeMap<String, [u8; 32]> = BTreeMap::new();
        assert_eq!(first_map_diff(&a, &b).as_deref(), Some("x"));
    }

    #[test]
    fn first_map_diff_detects_removed_key() {
        let a: BTreeMap<String, [u8; 32]> = BTreeMap::new();
        let mut b: BTreeMap<String, [u8; 32]> = BTreeMap::new();
        b.insert("y".into(), [1; 32]);
        assert_eq!(first_map_diff(&a, &b).as_deref(), Some("y"));
    }

    #[test]
    fn first_map_diff_detects_value_change() {
        let mut a: BTreeMap<String, [u8; 32]> = BTreeMap::new();
        a.insert("k".into(), [1; 32]);
        let mut b: BTreeMap<String, [u8; 32]> = BTreeMap::new();
        b.insert("k".into(), [2; 32]);
        assert_eq!(first_map_diff(&a, &b).as_deref(), Some("k"));
    }

    #[test]
    fn first_map_diff_returns_none_when_equal() {
        let mut a: BTreeMap<String, [u8; 32]> = BTreeMap::new();
        a.insert("k".into(), [1; 32]);
        let b = a.clone();
        assert!(first_map_diff(&a, &b).is_none());
    }

    #[test]
    fn param_ref_change_reports_param_category() {
        // Build two stages that differ only in params. The yaml
        // declares a param ref; the lockfile was seeded from a
        // version without it. With deps, cmd, env, and flags all
        // matching, classify_stale should settle on category=param.
        let mut stage_with = Stage::new(
            StageName::parse("build").unwrap(),
            Cmd::Shell("true".into()),
        );
        stage_with.env = EnvSpec::Empty;
        stage_with.params = vec![ParamRef::parse("model.lr").unwrap()];

        // Seed the lockfile by hand so the recorded deps are empty —
        // otherwise `seed_lockfile`'s phantom dep fires the dep
        // branch before the classifier ever reaches params.
        let mut stage_without = stage_with.clone();
        stage_without.params = vec![];
        let resolved = ResolvedStage {
            stage: stage_without.clone(),
            dep_hashes: BTreeMap::new(),
            params: BTreeMap::new(),
            env: stage_without.env.clone(),
            cmd: stage_without.cmd.clone(),
            outs: stage_without.outs.clone(),
        };
        let stage_hash = compute_stage_hash(&resolved);
        let entry = StageCacheEntry {
            schema_version: 1,
            stage_hash,
            stage_name: stage_without.name.as_str().to_owned(),
            cmd: CachedCmd::Shell {
                shell: "true".into(),
            },
            outs: Vec::new(),
            metrics: Vec::new(),
            plots: Vec::new(),
            executed_at: "2024-01-01T00:00:00.000Z".into(),
            duration_ms: 1,
            exec_id: None,
            attempts: 1,
            host_fingerprint: "test-host".into(),
        };
        let mut lockfile = Lockfile::new();
        lockfile
            .upsert(&entry, Vec::new(), BTreeMap::new(), BTreeMap::new())
            .unwrap();

        let locked = lockfile.get(&stage_with.name).unwrap();
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("params.yaml"), b"model:\n  lr: 0.01\n").unwrap();
        let (reason, key) =
            classify_stale(&stage_with, &[], locked, tmp.path(), &BTreeMap::new(), None);
        assert_eq!(reason, "param");
        assert_eq!(key.as_deref(), Some("model.lr"));
    }
}
