//! `crab exp` — experiment management.
//!
//! Experiments are content-addressed DAG runs performed against a
//! throwaway worktree that branches off of `HEAD` at run start, with
//! optional `--set key=value` parameter overrides written onto disk
//! before the DAG executes. Each run is keyed by a UUIDv7
//! [`ExperimentId`] so the canonical string form sorts
//! chronologically, and its metadata is persisted locally under
//! `.crab/workflow/exp/<uuid>.meta.json` so `exp show` / `exp ls`
//! / `exp diff` work without round-tripping through remote object
//! storage.
//!
//! This module is pure glue: it reuses
//! [`crate::workflow::exp_worktree::ExperimentWorktree`] for tmpdir
//! creation + override application, [`crate::cmd::run::run_in`] for
//! DAG execution, [`crab_workflow::Lockfile`] to read
//! back stage hashes after a run, and
//! [`crate::workflow::params::parse`] to capture metrics files
//! declared in `crab.yaml`. Every subcommand supports `--json`
//! through the existing [`crate::core::output::emit_json`] envelope.
//!
//! Local experiment subcommands:
//! - `run` — mint an exp id, materialize a tmpdir, execute the DAG,
//!   persist metadata locally. The tmpdir is cleaned up on success
//!   or failure; the orphan sweep at [`crate::workflow::exp_worktree::sweep_orphan_experiment_tmpdirs`]
//!   catches anything that slips past.
//! - `show` — read an experiment's metadata and render it.
//! - `diff` — compare two experiments' parameter overrides, stage
//!   hashes, and metrics.
//! - `ls` — list every local experiment in reverse chronological
//!   order.
//! - `promote` — create a git branch containing an experiment's
//!   captured workspace snapshot.
//! - `apply` — overlay a captured experiment workspace snapshot
//!   onto the user's workspace.
//! - `save` — capture the current workspace as an experiment without
//!   running the DAG.
//! - `remove` — delete selected local metadata blobs, or keep a
//!   selected set and delete the rest.
//! - `clean` — remove experiment tmpdirs and stale queue housekeeping
//!   files left behind by crashed workers.
//! - `gc` — prune local experiment metadata beyond a keep count.
//!
//! Release qualification still requires a GC live-set audit for checkpoint
//! objects and remote clean-clone evidence. Remote stage-cache hydration
//! during `exp pull` remains separate; `workflow push-cache` / `run
//! --pull-cache` are the cache transport.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Instant;

use bytes::Bytes;
use clap::{Parser, ValueEnum};
use object_store::path::Path as ObjectPath;
use regex_lite::Regex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crab_workflow::{
    CHECKPOINT_SCHEMA_VERSION, CheckpointLineage, CheckpointRecord, ExpQueue, ExpQueueEntry,
    ExpStatus, ExperimentId, Lockfile, snapshot_payload,
};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::git::url::CrabUrl;
use crate::storage::store::Store;
use crate::workflow::cache;
use crate::workflow::discover::{self, DiscoverMode};
use crate::workflow::exp_range::is_sweep_expression;
use crate::workflow::exp_worktree::{ExperimentWorktree, override_allows_missing_value};
use crate::workflow::experiment::{
    EXP_META_REF_PREFIX, EXPERIMENT_METADATA_SCHEMA_VERSION, ExperimentMetadata,
    exp_meta_object_path, exp_meta_ref, exp_stage_refs_object_path,
};
use crate::workflow::params;
use crate::workflow::stage::StageName;
use crab_workflow::{Workflow, yaml as yaml_mod};

/// Schema label for `crab exp run` structured output.
pub const EXP_RUN_SCHEMA: &str = "workflow.exp.run";
/// Schema label for `crab exp show` structured output.
pub const EXP_SHOW_SCHEMA: &str = "workflow.exp.show";
/// Schema label for `crab exp diff` structured output.
pub const EXP_DIFF_SCHEMA: &str = "workflow.exp.diff";
/// Schema label for `crab exp ls` structured output.
pub const EXP_LS_SCHEMA: &str = "workflow.exp.ls";
/// Schema label for `crab exp promote` structured output.
pub const EXP_PROMOTE_SCHEMA: &str = "workflow.exp.promote";
/// Schema label for `crab exp apply` structured output.
pub const EXP_APPLY_SCHEMA: &str = "workflow.exp.apply";
/// Schema label for `crab exp reset` structured output.
pub const EXP_RESET_SCHEMA: &str = "workflow.exp.reset";
/// Schema label for `crab exp save` structured output.
pub const EXP_SAVE_SCHEMA: &str = "workflow.exp.save";
/// Schema label for `crab exp rename` structured output.
pub const EXP_RENAME_SCHEMA: &str = "workflow.exp.rename";
/// Schema label for `crab exp push` structured output.
pub const EXP_PUSH_SCHEMA: &str = "workflow.exp.push";
/// Schema label for `crab exp pull` structured output.
pub const EXP_PULL_SCHEMA: &str = "workflow.exp.pull";
/// Schema label for `crab exp remove` structured output.
pub const EXP_REMOVE_SCHEMA: &str = "workflow.exp.remove";
/// Schema label for `crab exp clean` structured output.
pub const EXP_CLEAN_SCHEMA: &str = "workflow.exp.clean";
/// Schema label for `crab exp gc` structured output.
pub const EXP_GC_SCHEMA: &str = "workflow.exp.gc";

/// Envelope schema version bumped alongside any breaking payload
/// change on the `workflow.exp.*` schemas.
const EXP_SCHEMA_VERSION: &str = "1.0";

/// Relative path (from the repo root) to the parent directory that
/// holds per-experiment metadata JSON blobs. Chosen under
/// `.crab/workflow/exp/` so it shares the gitignore coverage the
/// workflow layer establishes on first use.
const EXP_META_PARENT_REL: &str = ".crab/workflow/exp";

/// Suffix for the captured experiment workspace tree. Kept outside
/// the live tmpdir name so orphan tmpdir sweeping cannot delete a
/// completed experiment's apply snapshot.
const EXP_WORKSPACE_SUFFIX: &str = ".workspace";

/// Suffix for the small apply manifest that records files deleted by
/// the experiment relative to its base commit.
const EXP_WORKSPACE_MANIFEST_SUFFIX: &str = ".workspace.json";

const EXP_REMOTE_WORKSPACE_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy)]
struct ExpDiffRenderOptions {
    markdown: bool,
    precision: usize,
    include_unchanged: bool,
    no_path: bool,
}

#[derive(Debug, Clone, Copy)]
struct ExpShowListRenderOptions {
    markdown: bool,
    csv: bool,
    precision: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ExpRunExecutionOptions {
    keep_going: bool,
    force: bool,
    pull: bool,
    allow_missing: bool,
    ignore_errors: bool,
    interactive: bool,
    message: Option<String>,
    mirror_child_output: bool,
    dry_run: bool,
    targets: Vec<String>,
    recursive: bool,
    single_item: bool,
    downstream: bool,
    force_downstream: bool,
    pipeline: bool,
    all_pipelines: bool,
    glob: bool,
    copy_paths: Vec<PathBuf>,
    resume: Option<String>,
}

impl Default for ExpRunExecutionOptions {
    fn default() -> Self {
        Self {
            keep_going: false,
            force: false,
            pull: false,
            allow_missing: false,
            ignore_errors: false,
            interactive: false,
            message: None,
            mirror_child_output: true,
            dry_run: false,
            targets: Vec::new(),
            recursive: false,
            single_item: false,
            downstream: false,
            force_downstream: false,
            pipeline: false,
            all_pipelines: false,
            glob: false,
            copy_paths: Vec::new(),
            resume: None,
        }
    }
}

impl ExpRunExecutionOptions {
    fn from_run_args(args: &RunArgs) -> Self {
        Self {
            keep_going: args.keep_going,
            force: args.force,
            pull: args.pull,
            allow_missing: args.allow_missing,
            ignore_errors: args.ignore_errors,
            interactive: args.interactive,
            message: args.message.clone(),
            mirror_child_output: !args.json,
            dry_run: args.dry_run,
            targets: args.targets.clone(),
            recursive: args.recursive,
            single_item: args.single_item,
            downstream: args.downstream,
            force_downstream: args.force_downstream,
            pipeline: args.pipeline,
            all_pipelines: args.all_pipelines,
            glob: args.glob,
            copy_paths: args.copy_paths.clone(),
            resume: args.resume.clone(),
        }
    }

    pub(crate) fn from_queue_entry(entry: &ExpQueueEntry) -> Self {
        Self {
            targets: entry.targets.clone(),
            recursive: entry.recursive,
            single_item: entry.single_item,
            downstream: entry.downstream,
            force_downstream: entry.force_downstream,
            pipeline: entry.pipeline,
            all_pipelines: entry.all_pipelines,
            glob: entry.glob,
            copy_paths: entry.copy_paths.clone(),
            message: entry.message.clone(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExpShowSortOrder {
    Asc,
    Desc,
}

/// Args for `crab exp run`.
#[derive(Debug, Clone, Parser)]
pub struct RunArgs {
    /// Parameter overrides as `key=value`. Repeatable. Dotted keys
    /// target nested structures inside declared params files.
    #[arg(
        long = "set",
        short = 'S',
        visible_alias = "set-param",
        value_name = "KEY=VALUE"
    )]
    pub set: Vec<String>,

    /// Queue this experiment for later execution without running it now.
    #[arg(long, conflicts_with = "run_all", default_value_t = false)]
    pub queue: bool,

    /// Run all queued experiments. DVC-style shortcut for `crab exp start`.
    #[arg(long = "run-all", conflicts_with = "queue", default_value_t = false)]
    pub run_all: bool,

    /// Number of queued experiments to run in parallel with `--run-all`.
    #[arg(long, short = 'j', value_name = "N", requires = "run_all")]
    pub jobs: Option<u32>,

    /// Forward `--keep-going` to the DAG runner.
    #[arg(long, short = 'k', default_value_t = false)]
    pub keep_going: bool,

    /// Force stages to run even if inputs appear unchanged.
    #[arg(long, short = 'f', default_value_t = false)]
    pub force: bool,

    /// Pull missing dependencies/cache entries before running.
    #[arg(long, default_value_t = false)]
    pub pull: bool,

    /// Skip stages whose only change is missing data.
    #[arg(long, default_value_t = false)]
    pub allow_missing: bool,

    /// Continue through stage failures, including dependent stages.
    #[arg(long, default_value_t = false)]
    pub ignore_errors: bool,

    /// Ask for confirmation before executing each stage that would run.
    #[arg(long, short = 'i', default_value_t = false)]
    pub interactive: bool,

    /// Print the experiment workflow plan without executing or
    /// persisting experiment metadata.
    #[arg(
        long = "dry-run",
        visible_alias = "dry",
        conflicts_with_all = ["queue", "run_all"],
        default_value_t = false
    )]
    pub dry_run: bool,

    /// Discover nested workflow files under the repository root,
    /// matching DVC's recursive pipeline target mode.
    #[arg(long, short = 'R', default_value_t = false)]
    pub recursive: bool,

    /// DVC-compatible target mode: run only target stage(s), without
    /// adding upstream dependencies.
    #[arg(
        long = "single-item",
        short = 's',
        conflicts_with_all = ["downstream", "pipeline", "all_pipelines"],
        default_value_t = false
    )]
    pub single_item: bool,

    /// DVC-compatible target mode: run target stage(s) and downstream
    /// consumers.
    #[arg(
        long,
        conflicts_with_all = ["single_item", "pipeline", "all_pipelines"],
        default_value_t = false
    )]
    pub downstream: bool,

    /// DVC-compatible execution mode: after a stage executes, force
    /// downstream consumers to execute instead of restoring run-cache hits.
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
        conflicts_with_all = ["single_item", "downstream", "pipeline", "glob"],
        default_value_t = false
    )]
    pub all_pipelines: bool,

    /// Treat positional targets as glob patterns over stage names.
    #[arg(long, default_value_t = false)]
    pub glob: bool,

    /// Accepted for DVC-style compatibility; Crab experiment runs
    /// already execute outside the user's workspace.
    #[arg(long, default_value_t = false)]
    pub temp: bool,

    /// Repo-relative ignored or untracked paths to copy into the
    /// experiment worktree before running.
    #[arg(long = "copy-paths", short = 'C', value_name = "PATH")]
    pub copy_paths: Vec<PathBuf>,

    /// Resume a new experiment from the latest acknowledged checkpoint of
    /// this experiment id or unambiguous prefix.
    #[arg(long, value_name = "EXPERIMENT")]
    pub resume: Option<String>,

    /// Max parallel stages. Reserved for future parallel DAG
    /// execution; ignored for now.
    #[arg(long, value_name = "N")]
    pub parallelism: Option<u32>,

    /// Human-readable experiment label. Stored in the metadata blob
    /// so `exp ls` can surface it.
    #[arg(long, short = 'n', value_name = "NAME")]
    pub name: Option<String>,

    /// Human-readable message stored with the experiment.
    #[arg(long, short = 'm', value_name = "MESSAGE")]
    pub message: Option<String>,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Stage targets from `crab.yaml`. Matches `dvc exp run`
    /// target selection with `--single-item`, `--downstream`,
    /// `--pipeline`, `--all-pipelines`, and `--glob`.
    pub targets: Vec<String>,
}

impl RunArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp show [id]`.
#[derive(Debug, Clone, Parser)]
pub struct ShowArgs {
    /// Experiment id in canonical hyphenated UUIDv7 form. Omit to
    /// list recent experiments.
    pub id: Option<String>,

    /// Include all local experiments. Accepted for DVC-style
    /// `exp show --all` / `--all-commits`; Crab's local metadata
    /// list is already unfiltered.
    #[arg(
        long = "all",
        short = 'A',
        visible_alias = "all-commits",
        default_value_t = false
    )]
    pub all: bool,

    /// Accepted DVC selector. Crab's local experiment metadata is
    /// already branch-independent, so this does not widen the scan.
    #[arg(long = "all-branches", short = 'a', default_value_t = false)]
    pub all_branches: bool,

    /// Accepted DVC selector. Crab's local experiment metadata is
    /// already tag-independent, so this does not widen the scan.
    #[arg(long = "all-tags", short = 'T', default_value_t = false)]
    pub all_tags: bool,

    /// Accepted DVC selector for a starting commit. Crab stores
    /// experiments by UUID metadata and currently lists the local
    /// metadata set regardless of commit reachability.
    #[arg(long, value_name = "COMMIT")]
    pub rev: Option<String>,

    /// Show only the N most recent experiments.
    #[arg(
        long,
        short = 'n',
        visible_alias = "num",
        value_name = "N",
        allow_hyphen_values = true
    )]
    pub limit: Option<isize>,

    /// Accepted for DVC CLI compatibility. Crab does not page
    /// `exp show` output, so this is a no-op.
    #[arg(long = "no-pager", default_value_t = false)]
    pub no_pager: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Render the experiment list as a Markdown table.
    #[arg(long, conflicts_with_all = ["json", "csv"], default_value_t = false)]
    pub md: bool,

    /// Render the experiment list as CSV.
    #[arg(long, conflicts_with_all = ["json", "md"], default_value_t = false)]
    pub csv: bool,

    /// Sort the list by id, name, started_at, base_commit, stages, or any
    /// captured param/metric key.
    #[arg(long, value_name = "COLUMN")]
    pub sort_by: Option<String>,

    /// Sort direction for `--sort-by`; defaults to newest first.
    #[arg(long, value_enum)]
    pub sort_order: Option<ExpShowSortOrder>,

    /// Decimal precision for numeric metrics in text/Markdown/CSV output.
    #[arg(long, value_name = "N")]
    pub precision: Option<usize>,

    /// Show only param/metric keys whose values vary across experiments.
    #[arg(long, default_value_t = false)]
    pub only_changed: bool,

    /// Accepted for DVC CLI compatibility. Crab summary rows already
    /// contain captured experiment parameter overrides.
    #[arg(long = "param-deps", default_value_t = false)]
    pub param_deps: bool,

    /// Accepted for DVC CLI compatibility. Crab experiment rows are
    /// identified by UUID and already include the base commit.
    #[arg(long, default_value_t = false)]
    pub sha: bool,

    /// Hide experiments whose persisted metadata status is `failed`.
    #[arg(long = "hide-failed", default_value_t = false)]
    pub hide_failed: bool,

    /// Accepted for DVC CLI compatibility. Queued entries live in
    /// `crab exp queue status`, not in `crab exp show`.
    #[arg(long = "hide-queued", default_value_t = false)]
    pub hide_queued: bool,

    /// Accepted for DVC CLI compatibility. Crab does not synthesize a
    /// workspace row in experiment summaries.
    #[arg(long = "hide-workspace", default_value_t = false)]
    pub hide_workspace: bool,

    /// Accepted for DVC CLI compatibility. Crab summaries are collected
    /// directly from local metadata, so there is no show-cache to refresh.
    #[arg(long, short = 'f', default_value_t = false)]
    pub force: bool,

    /// Remove matching param/metric keys from the list output.
    #[arg(long, value_name = "REGEX")]
    pub drop: Vec<String>,

    /// Keep matching param/metric keys even when `--only-changed`
    /// or `--drop` would remove them.
    #[arg(long, value_name = "REGEX")]
    pub keep: Vec<String>,
}

impl ShowArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }

    fn list_render_options(&self) -> ExpShowListRenderOptions {
        ExpShowListRenderOptions {
            markdown: self.md,
            csv: self.csv,
            precision: self.precision.unwrap_or(5),
        }
    }

    fn has_list_only_flags(&self) -> bool {
        self.all
            || self.all_branches
            || self.all_tags
            || self.rev.is_some()
            || self.limit.is_some()
            || self.md
            || self.csv
            || self.sort_by.is_some()
            || self.sort_order.is_some()
            || self.precision.is_some()
            || self.only_changed
            || self.param_deps
            || self.sha
            || self.hide_failed
            || self.hide_queued
            || self.hide_workspace
            || !self.drop.is_empty()
            || !self.keep.is_empty()
    }
}

/// Args for `crab exp diff <id_a> <id_b>`.
#[derive(Debug, Clone, Parser)]
pub struct DiffArgs {
    /// First experiment id (the "old" side).
    pub id_a: String,
    /// Second experiment id (the "new" side).
    pub id_b: String,

    /// Include unchanged parameters, stage hashes, and metrics.
    #[arg(long = "all", default_value_t = false)]
    pub all: bool,

    /// Accepted for DVC CLI compatibility. Crab experiment metadata
    /// stores parameter overrides, so there is no broader params set
    /// to narrow to stage dependencies.
    #[arg(long = "param-deps", default_value_t = false)]
    pub param_deps: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Render a Markdown report instead of the default text report.
    #[arg(long, conflicts_with = "json", default_value_t = false)]
    pub md: bool,

    /// Hide path/file prefixes in text and Markdown keys.
    #[arg(long = "no-path", default_value_t = false)]
    pub no_path: bool,

    /// Decimal precision for numeric metric values in text/Markdown output.
    #[arg(long, value_name = "N", default_value_t = 5)]
    pub precision: usize,
}

impl DiffArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp ls`.
#[derive(Debug, Clone, Parser, Default)]
pub struct LsArgs {
    /// Show only the N most recent experiments.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl LsArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp promote`.
#[derive(Debug, Clone, Parser)]
pub struct PromoteArgs {
    /// Experiment id to promote.
    pub id: String,

    /// Branch name to create from the experiment snapshot.
    /// Optional for DVC-style `crab exp branch <id> [branch]`; if
    /// omitted, Crab derives one from the experiment name or id.
    #[arg(value_name = "BRANCH")]
    pub branch_name: Option<String>,

    /// Branch name to create from the experiment snapshot.
    #[arg(
        long = "branch",
        short = 'b',
        value_name = "BRANCH",
        conflicts_with = "branch_name"
    )]
    pub branch: Option<String>,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl PromoteArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp apply`.
#[derive(Debug, Clone, Parser)]
pub struct ApplyArgs {
    /// Experiment id or unambiguous prefix to apply to the workspace.
    pub id: String,

    /// Optional checkpoint id or sequence to apply instead of the terminal
    /// workspace snapshot.
    #[arg(long, value_name = "CHECKPOINT")]
    pub checkpoint: Option<String>,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl ApplyArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp reset <id>`.
#[derive(Debug, Clone, Parser)]
pub struct ResetArgs {
    /// Experiment id or unambiguous prefix whose checkpoint lineage is reset.
    pub id: String,

    /// Keep the selected checkpoint as the resume base. Without a selector,
    /// all acknowledged checkpoints are discarded from the active lineage.
    #[arg(long, value_name = "CHECKPOINT")]
    pub checkpoint: Option<String>,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl ResetArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp save`.
#[derive(Debug, Clone, Parser)]
pub struct SaveArgs {
    /// Human-readable experiment label. Accepted for DVC-style
    /// compatibility and preserved in the captured CLI args.
    #[arg(long, short = 'n', value_name = "NAME")]
    pub name: Option<String>,

    /// Human-readable message stored with the experiment.
    #[arg(long, short = 'm', value_name = "MESSAGE")]
    pub message: Option<String>,

    /// Accepted for DVC-style compatibility. Crab experiment saves
    /// always create a fresh UUID, so there is no existing save to
    /// rewrite.
    #[arg(long, short = 'f', default_value_t = false)]
    pub force: bool,

    /// Discover nested workflow files while resolving save targets.
    #[arg(long, short = 'R', default_value_t = false)]
    pub recursive: bool,

    /// Include an untracked path. Crab snapshots the whole workspace
    /// (except `.git` and `.crab`) so this flag is accepted as a
    /// no-op compatibility hint.
    #[arg(long = "include-untracked", short = 'I', value_name = "PATH")]
    pub include_untracked: Vec<PathBuf>,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Workflow targets whose stage hashes and declared metrics
    /// should be recorded in the experiment metadata.
    #[arg(value_name = "TARGET")]
    pub targets: Vec<String>,
}

impl SaveArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp rename`.
#[derive(Debug, Clone, Parser)]
pub struct RenameArgs {
    /// Experiment id or unambiguous prefix to rename.
    pub id: String,

    /// New human-readable experiment label.
    pub name: String,

    /// Allow another local experiment to already carry this label.
    #[arg(long, short = 'f', default_value_t = false)]
    pub force: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl RenameArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp push`.
#[derive(Debug, Clone, Parser)]
pub struct PushArgs {
    /// Experiment ids or unambiguous local prefixes to upload.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,

    /// Upload every local experiment.
    #[arg(
        long = "all",
        short = 'A',
        alias = "all-commits",
        default_value_t = false
    )]
    pub all: bool,

    /// Replace an existing remote copy of the same experiment id.
    #[arg(long, short = 'f', default_value_t = false)]
    pub force: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl PushArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp pull`.
#[derive(Debug, Clone, Parser)]
pub struct PullArgs {
    /// Experiment ids or unambiguous remote prefixes to download.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,

    /// Download every experiment advertised by the configured Crab remote.
    #[arg(
        long = "all",
        short = 'A',
        alias = "all-commits",
        default_value_t = false
    )]
    pub all: bool,

    /// Replace an existing local copy of the same experiment id.
    #[arg(long, short = 'f', default_value_t = false)]
    pub force: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl PullArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp remove`.
#[derive(Debug, Clone, Parser)]
pub struct RemoveArgs {
    /// Experiment ids, exact names, or unambiguous prefixes to
    /// remove. With `--keep`, these are the experiments to preserve
    /// while all others are removed.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,

    /// Remove every local experiment.
    #[arg(long = "all", short = 'A', default_value_t = false)]
    pub all: bool,

    /// Remove pending queued experiments instead of local experiment metadata.
    #[arg(long, default_value_t = false)]
    pub queue: bool,

    /// Remove experiments derived from the specified baseline commit.
    #[arg(long, value_name = "COMMIT")]
    pub rev: Option<String>,

    /// Remove experiments from the last N first-parent commits starting at
    /// `--rev` or HEAD. A negative value selects every first-parent commit.
    #[arg(
        long,
        short = 'n',
        visible_alias = "num",
        value_name = "N",
        allow_hyphen_values = true
    )]
    pub limit: Option<isize>,

    /// Remove experiments from a Crab Git remote name or crab:// URL.
    #[arg(long = "git-remote", short = 'g', value_name = "REMOTE")]
    pub git_remote: Option<String>,

    /// Keep the named experiments and remove every other local experiment.
    #[arg(long, default_value_t = false)]
    pub keep: bool,

    /// Preview what would be removed without touching the filesystem.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl RemoveArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp clean`.
#[derive(Debug, Clone, Parser)]
pub struct CleanArgs {
    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl CleanArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Args for `crab exp gc`.
#[derive(Debug, Clone, Parser)]
pub struct GcArgs {
    /// Number of most-recent experiments to retain. Default 100 —
    /// chosen to match the operator-visible default called out in
    /// the design's risk register ("`exp gc` default retention").
    #[arg(long, value_name = "N", default_value_t = 100)]
    pub keep: usize,

    /// Preview mode — print what would be removed without
    /// touching the filesystem.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl Default for GcArgs {
    fn default() -> Self {
        Self {
            keep: 100,
            dry_run: false,
            json: false,
        }
    }
}

impl GcArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Payload emitted by `exp run`.
///
/// A compact summary of the experiment that just ran — enough for
/// consumers to spot-check success, pick the metadata blob path,
/// and decide whether to invoke `exp show <id>` for the full
/// record.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpRunPayload {
    pub exp_id: String,
    pub base_commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Stage name → stage hash (hex). Populated from the lockfile
    /// written by the DAG run inside the tmpdir.
    pub stages: BTreeMap<String, String>,
    /// Declared metrics files (repo-relative paths) picked up
    /// from `crab.yaml` at run start.
    pub metrics_files: Vec<PathBuf>,
    /// `"success"` or `"failed"`.
    pub status: String,
    pub duration_ms: u64,
    /// RFC3339 timestamp of run start (matches
    /// [`ExperimentMetadata::started_at`]).
    pub started_at: String,
}

/// Payload emitted by `exp show`. Wraps the full metadata blob as
/// a `serde_json::Value` so we don't have to impose a schema on
/// the user-authored metrics maps nested inside.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpShowPayload {
    pub metadata: serde_json::Value,
}

/// Payload emitted by no-id `exp show`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpShowListPayload {
    pub experiments: Vec<ExpSummary>,
}

/// Payload emitted by `exp diff`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpDiffPayload {
    pub id_a: String,
    pub id_b: String,
    /// Parameter overrides added in `id_b`.
    pub params_added: BTreeMap<String, String>,
    /// Parameter overrides removed (present in `id_a`, absent in
    /// `id_b`).
    pub params_removed: BTreeMap<String, String>,
    /// Parameter overrides with different values between the two
    /// experiments: `(old, new)`.
    pub params_changed: BTreeMap<String, (String, String)>,
    /// Parameter overrides present with the same value on both sides.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub params_unchanged: BTreeMap<String, String>,
    /// Stage name → `(hash_a, hash_b)` for stages whose hashes
    /// differ between the two experiments. Stages present in only
    /// one experiment carry `None` on the missing side.
    pub stages_changed: BTreeMap<String, (Option<String>, Option<String>)>,
    /// Stage hashes present with the same value on both sides.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub stages_unchanged: BTreeMap<String, String>,
    /// Metric key → `(value_a, value_b)` for scalar metrics whose
    /// values differ. Added / removed metric keys carry `None` on
    /// the absent side.
    pub metrics_changed: BTreeMap<String, (Option<serde_json::Value>, Option<serde_json::Value>)>,
    /// Metric keys present with the same value on both sides.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub metrics_unchanged: BTreeMap<String, serde_json::Value>,
}

/// Payload emitted by `exp ls`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpLsPayload {
    pub experiments: Vec<ExpSummary>,
}

/// One-line summary used by `exp ls`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExpSummary {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub started_at: String,
    pub base_commit: String,
    pub status: String,
    pub stages: usize,
    /// Parameter overrides applied for the experiment.
    pub params: BTreeMap<String, String>,
    /// Flattened metric key → value pairs captured from declared
    /// metrics files. Nested JSON objects use
    /// `metrics.json:outer.inner` style keys.
    pub metrics: BTreeMap<String, serde_json::Value>,
    /// Sorted metric keys (the declared metrics file paths, not
    /// the metrics' internal scalar keys). Same shape as
    /// [`ExperimentMetadata::metrics`]'s keys.
    pub metrics_keys: Vec<String>,
}

/// Payload emitted by `exp promote`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpPromotePayload {
    pub exp_id: String,
    pub branch: String,
    pub commit: String,
}

/// Payload emitted by `exp apply`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpApplyPayload {
    pub exp_id: String,
    pub applied: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// Payload emitted by `exp reset`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpResetPayload {
    pub exp_id: String,
    pub checkpoint: Option<String>,
    pub reset_stages: Vec<String>,
}

/// Payload emitted by `exp save`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpSavePayload {
    pub exp_id: String,
    pub base_commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub stages: BTreeMap<String, String>,
    pub metrics_files: Vec<PathBuf>,
    pub status: String,
    pub started_at: String,
}

/// Payload emitted by `exp rename`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpRenamePayload {
    pub exp_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_name: Option<String>,
    pub new_name: String,
}

/// Payload emitted by `exp push`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpPushPayload {
    pub pushed: Vec<String>,
    pub skipped: Vec<String>,
}

/// Payload emitted by `exp pull`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpPullPayload {
    pub pulled: Vec<String>,
    pub skipped: Vec<String>,
}

/// Payload emitted by `exp remove`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpRemovePayload {
    pub dry_run: bool,
    pub removed: Vec<String>,
    pub kept: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed_remote: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kept_remote: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed_queue: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kept_queue: Vec<String>,
}

/// Payload emitted by `exp clean`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpCleanPayload {
    pub removed_tmpdirs: usize,
    pub removed_active_markers: usize,
    pub removed_kill_requests: usize,
    pub removed_logs: usize,
}

/// Payload emitted by `exp gc`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExpGcPayload {
    pub keep: usize,
    pub dry_run: bool,
    /// Experiment ids that were (or would be) removed, in the
    /// order the scanner encountered them (chronological,
    /// oldest-first).
    pub removed: Vec<String>,
    /// Experiment ids retained under the keep policy.
    pub kept: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExpWorkspaceManifest {
    deleted: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExpRemoteWorkspaceManifest {
    schema_version: u16,
    deleted: Vec<String>,
    entries: Vec<ExpRemoteWorkspaceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExpRemoteWorkspaceEntry {
    path: String,
    kind: ExpRemoteWorkspaceEntryKind,
    mode: u32,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExpRemoteWorkspaceEntryKind {
    File,
    Dir,
    Symlink,
}

/// `crab exp run`.
///
/// Parses overrides, mints a fresh [`ExperimentId`], materializes a
/// throwaway worktree from the current HEAD, delegates DAG
/// execution to [`crate::cmd::run::run_in`] against the tmpdir, and
/// persists metadata locally under
/// `.crab/workflow/exp/<uuid>.meta.json`.
pub async fn exec_run(args: RunArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_run(&args, &cwd).await
}

/// `crab exp show`.
pub fn exec_show(args: ShowArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_show(&args, &cwd)
}

/// `crab exp diff`.
pub fn exec_diff(args: DiffArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_diff(&args, &cwd)
}

/// `crab exp ls`.
pub fn exec_ls(args: LsArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_ls(&args, &cwd)
}

/// `crab exp promote`.
pub fn exec_promote(args: PromoteArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_promote(&args, &cwd)
}

/// `crab exp remove`.
pub async fn exec_remove(args: RemoveArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_remove(&args, &cwd).await.map(|_| ())
}

/// `crab exp clean`.
pub fn exec_clean(args: CleanArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_clean(&args, &cwd).map(|_| ())
}

/// `crab exp apply`.
pub fn exec_apply(args: ApplyArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_apply(&args, &cwd).map(|_| ())
}

/// `crab exp reset`.
pub fn exec_reset(args: ResetArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_reset(&args, &cwd).map(|_| ())
}

/// `crab exp save`.
pub fn exec_save(args: SaveArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_save(&args, &cwd).map(|_| ())
}

/// `crab exp rename`.
pub fn exec_rename(args: RenameArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_rename(&args, &cwd).map(|_| ())
}

/// `crab exp push`.
pub async fn exec_push(args: PushArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_push(&args, &cwd).await.map(|_| ())
}

/// `crab exp pull`.
pub async fn exec_pull(args: PullArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_pull(&args, &cwd).await.map(|_| ())
}

/// `crab exp gc`.
pub fn exec_gc(args: GcArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_exp_gc(&args, &cwd).map(|_| ())
}

/// Testable `exp run` entry point.
///
/// `repo_root` is the user's actual workspace. The DAG runs against
/// a throwaway tmpdir ([`ExperimentWorktree`]) derived from
/// `HEAD@repo_root`; the user's main worktree is never mutated.
pub async fn run_exp_run(args: &RunArgs, repo_root: &Path) -> Result<()> {
    let mode = args.output_mode();

    if args.run_all {
        let start_args = crate::cmd::exp_queue::StartArgs {
            jobs: args.jobs.unwrap_or(1),
            json: args.json,
        };
        return crate::cmd::exp_queue::run_exp_start(&start_args, repo_root).await;
    }

    let name = args
        .name
        .as_deref()
        .map(|raw| normalize_experiment_name(raw, "exp run"))
        .transpose()?;

    if args.queue {
        return crate::cmd::exp_queue::queue_from_exp_run(
            repo_root,
            &args.set,
            name,
            args.message.clone(),
            args.targets.clone(),
            crate::cmd::exp_queue::QueueTargetFlags {
                recursive: args.recursive,
                single_item: args.single_item,
                downstream: args.downstream,
                force_downstream: args.force_downstream,
                pipeline: args.pipeline,
                all_pipelines: args.all_pipelines,
                glob: args.glob,
            },
            args.copy_paths.clone(),
            mode,
        );
    }

    // Parse `--set k=v` overrides up front so a malformed entry
    // fails loudly before we touch the filesystem.
    reject_sweeps_without_queue(&args.set)?;
    let overrides = parse_overrides(&args.set)?;

    let exp_id = ExperimentId::new_v7();
    let payload = run_exp_run_with_id(
        repo_root,
        exp_id,
        overrides,
        ExpRunExecutionOptions::from_run_args(args),
        None,
        None,
        name,
        std::env::args().collect(),
    )
    .await?;
    emit_run(&payload, mode);
    Ok(())
}

pub(crate) async fn run_exp_run_with_id(
    repo_root: &Path,
    exp_id: ExperimentId,
    overrides: BTreeMap<String, String>,
    options: ExpRunExecutionOptions,
    queue_commit: Option<String>,
    base_commit: Option<&str>,
    name: Option<String>,
    cli_args: Vec<String>,
) -> Result<ExpRunPayload> {
    let started_at = crab_types::time::now_rfc3339_millis();
    let started_instant = Instant::now();
    let is_queued_run = queue_commit.is_some();

    info!(
        exp_id = %exp_id,
        overrides = overrides.len(),
        "exp run: creating tmpdir worktree",
    );

    // Materialize the tmpdir and overlay overrides onto declared
    // params files so they participate in stage hashing.
    let worktree = match base_commit {
        Some(commit) => {
            ExperimentWorktree::create_at_commit(repo_root, exp_id, commit, &overrides)?
        }
        None => ExperimentWorktree::create(repo_root, exp_id, &overrides)?,
    };
    let base_commit = worktree.base_commit.clone();
    let tmpdir_path = worktree.path.clone();
    let checkpoint_control_dir = crate::cmd::workflow_checkpoint::create_control_directory(
        &tmpdir_path,
        &exp_id.to_string(),
    )?;
    let checkpoint_token = crate::cmd::workflow_checkpoint::control_token(&exp_id.to_string());
    copy_paths_into_experiment(repo_root, &tmpdir_path, &options.copy_paths)?;
    if let Some(source) = options.resume.as_deref() {
        let source_id = resolve_experiment_id(repo_root, source)?;
        let records = checkpoint_records(repo_root, &source_id)?;
        let record = records
            .iter()
            .rev()
            .find(|record| record.resumable)
            .ok_or_else(|| CrabError::Configuration {
                key: "exp run --resume".to_owned(),
                origin: format!("experiment {source_id} has no acknowledged checkpoint"),
            })?;
        let source_state = checkpoint_state_dir(repo_root, &source_id);
        let target_state = checkpoint_state_dir(repo_root, &exp_id);
        copy_checkpoint_state(&source_state, &target_state)?;
        apply_checkpoint_record_to(&target_state, &tmpdir_path, record)?;
    }
    let _active_queue_run = if is_queued_run {
        Some(crate::cmd::exp_queue::mark_queue_run_active(
            repo_root,
            &exp_id.to_string(),
            &tmpdir_path,
            &started_at,
        )?)
    } else {
        None
    };
    let queue_child_started = if is_queued_run {
        let repo = repo_root.to_path_buf();
        let id = exp_id.to_string();
        let marked = Arc::new(AtomicBool::new(false));
        Some(Arc::new(move |pid| {
            if marked.swap(true, AtomicOrdering::SeqCst) {
                return;
            }
            if let Err(e) = crate::cmd::exp_queue::mark_queue_child_started(&repo, &id, pid) {
                warn!(exp_id = %id, error = %e, "failed to mark queue task child start");
            }
        }) as Arc<dyn Fn(u32) + Send + Sync>)
    } else {
        None
    };

    // Capture the declared metrics files BEFORE we execute, so the
    // exp metadata faithfully records what the user declared even
    // if the DAG fails before producing them.
    let declared_metrics = read_declared_metrics(&tmpdir_path)?;

    // Dispatch the DAG against the tmpdir via `cmd::run::run_in`.
    // `run_in` already accepts an explicit `repo_root: &Path` so we
    // don't need a CWD swap — the tmpdir IS the effective repo
    // root for the duration of this call.
    let run_args = crate::cmd::run::RunArgs {
        // `cmd::run::run_in` treats inline-flag presence as a
        // single-stage signal; leaving these empty routes to the
        // yaml discovery path, which is what `exp run` wants.
        name: None,
        deps: Vec::new(),
        outs: Vec::new(),
        env: Vec::new(),
        empty_env: false,
        timeout: None,
        hermetic: false,
        nondeterministic: false,
        force: options.force,
        dry_run: options.dry_run,
        interactive: options.interactive,
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
        recursive: options.recursive,
        single_item: options.single_item,
        downstream: options.downstream,
        force_downstream: options.force_downstream,
        pipeline: options.pipeline,
        all_pipelines: options.all_pipelines,
        keep_going: options.keep_going,
        ignore_errors: options.ignore_errors,
        parallelism: None,
        cache_push: false,
        allow_missing: options.allow_missing,
        pull: options.pull,
        validate: false,
        #[cfg(feature = "watch")]
        watch: false,
        workflow: None,
        stages: None,
        glob: options.glob,
        cmd: options.targets.clone(),
    };

    let (checkpoint_stop_tx, checkpoint_stop_rx) = tokio::sync::oneshot::channel();
    let checkpoint_supervisor = tokio::spawn(crate::cmd::workflow_checkpoint::supervise(
        checkpoint_control_dir.clone(),
        tmpdir_path.clone(),
        repo_root.to_path_buf(),
        exp_id.to_string(),
        checkpoint_token.clone(),
        checkpoint_stop_rx,
    ));

    let mut dag_result = crate::cmd::run::run_in_with_options(
        &run_args,
        &tmpdir_path,
        OutputMode::Text,
        crate::cmd::run::RunInvocationOptions {
            mirror_child_output: options.mirror_child_output && !is_queued_run,
            external_kill_path: is_queued_run
                .then(|| crate::cmd::exp_queue::queue_kill_path(repo_root, &exp_id.to_string())),
            child_started: queue_child_started,
            allow_checkpoints: true,
            checkpoint_control_dir: Some(checkpoint_control_dir.clone()),
            checkpoint_run_id: Some(exp_id.to_string()),
            checkpoint_token: Some(checkpoint_token),
        },
    )
    .await;
    if dag_result.is_ok()
        && let Err(error) = crate::cmd::workflow_checkpoint::finalize_checkpoints(
            &tmpdir_path,
            repo_root,
            &exp_id.to_string(),
        )
    {
        dag_result = Err(error);
    }
    let _ = checkpoint_stop_tx.send(());
    let supervisor_result = checkpoint_supervisor
        .await
        .map_err(|error| CrabError::Internal(format!("checkpoint supervisor failed: {error}")))?;
    if let Err(error) = supervisor_result
        && dag_result.is_ok()
    {
        return Err(error);
    }
    let _ = std::fs::remove_dir_all(&checkpoint_control_dir);
    let status = if dag_result.is_ok() {
        "success"
    } else {
        "failed"
    };
    let duration_ms = started_instant
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;

    if options.dry_run {
        if let Err(e) = worktree.cleanup() {
            warn!(
                exp_id = %exp_id,
                error = %e,
                "exp run: dry-run tmpdir cleanup failed; orphan sweep will retry",
            );
        }
        dag_result?;
        return Ok(ExpRunPayload {
            exp_id: exp_id.to_string(),
            base_commit,
            name,
            message: options.message,
            stages: BTreeMap::new(),
            metrics_files: declared_metrics,
            status: "dry-run".to_owned(),
            duration_ms,
            started_at,
        });
    }

    // Read stage hashes from the lockfile the DAG produced inside
    // the tmpdir. On failure the lockfile may not exist; fall back
    // to an empty map rather than propagating a secondary error
    // that would mask the original DAG failure.
    let stages = match Lockfile::load(&tmpdir_path.join("crab.lock")) {
        Ok(lock) => lock
            .stages
            .iter()
            .map(|(name, stage)| (name.as_str().to_owned(), stage.stage_hash.as_hex()))
            .collect::<BTreeMap<_, _>>(),
        Err(e) => {
            warn!(
                exp_id = %exp_id,
                error = %e,
                "exp run: failed to read tmpdir lockfile; metadata.stages will be empty",
            );
            BTreeMap::new()
        }
    };

    // Read declared metrics files from the tmpdir (they may not
    // all exist if the DAG failed partway). Missing files contribute
    // nothing — `exp show` consumers interpret absence as "metric
    // not produced".
    let metrics = collect_metric_values(&tmpdir_path, &declared_metrics);

    if dag_result.is_ok() {
        capture_workspace_snapshot(repo_root, &tmpdir_path, &base_commit, &exp_id)?;
    }

    // Build and persist the metadata blob BEFORE cleaning up the
    // tmpdir, so a cleanup failure doesn't lose the record.
    let metadata = ExperimentMetadata {
        schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
        exp_id,
        base_commit: base_commit.clone(),
        queue_commit,
        name: name.clone(),
        message: options.message.clone(),
        status: status.to_owned(),
        param_overrides: overrides.clone(),
        stages: stages.clone(),
        metrics,
        cli_args,
        host_fingerprint: exp_host_fingerprint(),
        started_at: started_at.clone(),
        ended_at: Some(crab_types::time::now_rfc3339_millis()),
    };

    write_local_metadata(repo_root, &metadata)?;

    if is_queued_run {
        crate::cmd::exp_queue::persist_queue_logs(
            repo_root,
            &exp_id.to_string(),
            &tmpdir_path,
            status,
        )?;
    }

    // Cleanup is best-effort: the orphan sweep will reclaim it on
    // the next `exp run` if this fails.
    if let Err(e) = worktree.cleanup() {
        warn!(
            exp_id = %exp_id,
            error = %e,
            "exp run: tmpdir cleanup failed; orphan sweep will retry",
        );
    }

    // Propagate DAG failure AFTER persisting metadata + cleanup so
    // the caller sees the record of the failed attempt.
    dag_result?;

    Ok(ExpRunPayload {
        exp_id: exp_id.to_string(),
        base_commit,
        name,
        message: options.message,
        stages,
        metrics_files: declared_metrics,
        status: status.to_owned(),
        duration_ms,
        started_at,
    })
}

/// Testable `exp show` entry point.
pub fn run_exp_show(args: &ShowArgs, repo_root: &Path) -> Result<()> {
    let Some(id) = &args.id else {
        let filters = compile_show_column_filters(args)?;
        let mut experiments = collect_show_summaries(
            repo_root,
            normalized_show_limit(args.limit),
            args.sort_by.as_deref(),
            args.sort_order.unwrap_or(ExpShowSortOrder::Desc),
            args.hide_failed,
        )?;
        apply_show_column_filters(&mut experiments, &filters);
        let payload = ExpShowListPayload { experiments };
        emit_show_list(&payload, args.output_mode(), args.list_render_options());
        return Ok(());
    };
    if args.has_list_only_flags() {
        return Err(CrabError::Configuration {
            key: "exp show".to_owned(),
            origin: "list/export/sort options only apply when no experiment id is supplied"
                .to_owned(),
        });
    }

    let exp_id = resolve_experiment_id(repo_root, id)?;
    let metadata = read_local_metadata(repo_root, &exp_id)?;
    let metadata_value = serde_json::to_value(&metadata).map_err(|e| {
        CrabError::Internal(format!("serialize experiment metadata for {exp_id}: {e}"))
    })?;
    let checkpoints = checkpoint_records(repo_root, &exp_id)?;
    let mut metadata_value = metadata_value;
    if let serde_json::Value::Object(object) = &mut metadata_value {
        object.insert(
            "checkpoints".to_owned(),
            serde_json::to_value(&checkpoints).map_err(|error| {
                CrabError::Internal(format!("serialize checkpoints for {exp_id}: {error}"))
            })?,
        );
    }
    let payload = ExpShowPayload {
        metadata: metadata_value,
    };
    emit_show(&payload, &metadata, args.output_mode());
    Ok(())
}

fn normalized_show_limit(limit: Option<isize>) -> Option<usize> {
    limit.and_then(|n| usize::try_from(n).ok())
}

/// Testable `exp diff` entry point.
pub fn run_exp_diff(args: &DiffArgs, repo_root: &Path) -> Result<()> {
    let id_a = resolve_experiment_id(repo_root, &args.id_a)?;
    let id_b = resolve_experiment_id(repo_root, &args.id_b)?;
    let meta_a = read_local_metadata(repo_root, &id_a)?;
    let meta_b = read_local_metadata(repo_root, &id_b)?;
    let payload = build_diff_payload(&meta_a, &meta_b, args.all);
    emit_diff(
        &payload,
        args.output_mode(),
        ExpDiffRenderOptions {
            markdown: args.md,
            precision: args.precision,
            include_unchanged: args.all,
            no_path: args.no_path,
        },
    );
    Ok(())
}

/// Testable `exp ls` entry point.
pub fn run_exp_ls(args: &LsArgs, repo_root: &Path) -> Result<()> {
    let payload = ExpLsPayload {
        experiments: collect_limited_summaries(repo_root, args.limit)?,
    };
    emit_ls(&payload, args.output_mode());
    Ok(())
}

/// Testable `exp promote` entry point.
pub fn run_exp_promote(args: &PromoteArgs, repo_root: &Path) -> Result<()> {
    let exp_id = resolve_experiment_id(repo_root, &args.id)?;
    let metadata = read_local_metadata(repo_root, &exp_id)?;
    let branch = promote_branch_name(args, &metadata);

    let commit = create_experiment_branch(repo_root, &branch, &metadata)?;

    let payload = ExpPromotePayload {
        exp_id: exp_id.to_string(),
        branch,
        commit,
    };
    emit_promote(&payload, args.output_mode());
    Ok(())
}

/// Testable `exp apply` entry point. Returns the payload so tests
/// can assert on the workspace changes without parsing stdout.
pub fn run_exp_apply(args: &ApplyArgs, repo_root: &Path) -> Result<ExpApplyPayload> {
    let exp_id = resolve_experiment_id(repo_root, &args.id)?;
    read_local_metadata(repo_root, &exp_id)?;
    let (applied, deleted) = if let Some(selector) = args.checkpoint.as_deref() {
        let record = select_checkpoint_record(repo_root, &exp_id, selector)?;
        (
            apply_checkpoint_record_to(
                &checkpoint_state_dir(repo_root, &exp_id),
                repo_root,
                &record,
            )?,
            Vec::new(),
        )
    } else {
        apply_experiment_snapshot_to(repo_root, &exp_id, repo_root, "exp apply")?
    };

    let payload = ExpApplyPayload {
        exp_id: exp_id.to_string(),
        applied,
        deleted,
        checkpoint: args.checkpoint.clone(),
    };
    emit_apply(&payload, args.output_mode());
    Ok(payload)
}

/// Testable `exp reset` entry point.
pub fn run_exp_reset(args: &ResetArgs, repo_root: &Path) -> Result<ExpResetPayload> {
    let exp_id = resolve_experiment_id(repo_root, &args.id)?;
    read_local_metadata(repo_root, &exp_id)?;
    let state_root = checkpoint_state_dir(repo_root, &exp_id);
    let state_parent = state_root
        .parent()
        .ok_or_else(|| CrabError::Configuration {
            key: "exp reset".to_owned(),
            origin: format!(
                "checkpoint state path has no parent: {}",
                state_root.display()
            ),
        })?;
    ensure_checkpoint_parent_not_symlink(state_parent)?;
    fs::create_dir_all(state_parent).map_err(CrabError::Io)?;
    let paths = checkpoint_lineage_paths(&state_root)?;
    let selected = args
        .checkpoint
        .as_deref()
        .map(|selector| select_checkpoint_from_paths(&paths, selector))
        .transpose()?;
    let mut reset_stages = Vec::new();
    let mut staged_lineages = Vec::new();
    for path in &paths {
        let lineage = load_checkpoint_lineage(&path)?;
        let stage = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_owned();
        let next = match &selected {
            Some((selected_stage, selected_record)) if selected_stage == &stage => {
                lineage.reset_to(&selected_record.id)?
            }
            Some(_) => lineage.clone(),
            None => CheckpointLineage::default(),
        };
        if next != lineage {
            staged_lineages.push((
                path.file_name()
                    .map(std::ffi::OsString::from)
                    .ok_or_else(|| CrabError::Configuration {
                        key: "exp reset".to_owned(),
                        origin: path.display().to_string(),
                    })?,
                next,
            ));
            reset_stages.push(stage);
        }
    }

    let temporary = state_parent.join(format!(".{exp_id}.reset-{}", uuid::Uuid::now_v7()));
    let result = (|| {
        match fs::symlink_metadata(&state_root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(CrabError::Configuration {
                    key: "exp reset checkpoint state".to_owned(),
                    origin: format!("state root is not a directory: {}", state_root.display()),
                });
            }
            Ok(_) => copy_checkpoint_tree(&state_root, &temporary)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&temporary).map_err(CrabError::Io)?;
            }
            Err(error) => return Err(CrabError::Io(error)),
        }

        for (file_name, lineage) in staged_lineages {
            lineage
                .save_atomic(&temporary.join(file_name))
                .map_err(|error| CrabError::Configuration {
                    key: "exp reset".to_owned(),
                    origin: error.to_string(),
                })?;
        }

        let decision = serde_json::json!({
            "schema_version": CHECKPOINT_SCHEMA_VERSION,
            "experiment": exp_id.to_string(),
            "checkpoint": args.checkpoint,
            "reset_stages": reset_stages,
            "created_at": crab_types::time::now_rfc3339_millis(),
        });
        write_checkpoint_reset_decision(&temporary, &decision)?;
        validate_checkpoint_state(&temporary)?;
        commit_checkpoint_state_reset(&temporary, &state_root)
    })();
    if let Err(error) = result {
        let _ = remove_existing_path(&temporary);
        return Err(error);
    }
    let payload = ExpResetPayload {
        exp_id: exp_id.to_string(),
        checkpoint: args.checkpoint.clone(),
        reset_stages,
    };
    emit_reset(&payload, args.output_mode());
    Ok(payload)
}

fn commit_checkpoint_state_reset(temporary: &Path, state_root: &Path) -> Result<()> {
    let parent = state_root
        .parent()
        .ok_or_else(|| CrabError::Configuration {
            key: "exp reset".to_owned(),
            origin: format!(
                "checkpoint state path has no parent: {}",
                state_root.display()
            ),
        })?;
    ensure_checkpoint_parent_not_symlink(parent)?;
    let backup = parent.join(format!(
        ".{}.reset-backup-{}",
        state_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("checkpoint"),
        uuid::Uuid::now_v7()
    ));
    let had_state = match fs::symlink_metadata(state_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(CrabError::Configuration {
                key: "exp reset checkpoint state".to_owned(),
                origin: format!("state root is not a directory: {}", state_root.display()),
            });
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(CrabError::Io(error)),
    };
    if had_state {
        fs::rename(state_root, &backup).map_err(CrabError::Io)?;
    }
    if let Err(error) = fs::rename(temporary, state_root) {
        if had_state {
            let _ = fs::rename(&backup, state_root);
        }
        return Err(CrabError::Io(error));
    }
    if had_state && let Err(error) = fs::remove_dir_all(&backup) {
        tracing::warn!(path = %backup.display(), error = %error, "checkpoint reset backup cleanup deferred");
    }
    Ok(())
}

fn checkpoint_state_dir(repo_root: &Path, exp_id: &ExperimentId) -> PathBuf {
    repo_root
        .join(".crab/workflow/checkpoints")
        .join(exp_id.to_string())
}

fn copy_checkpoint_state(source: &Path, destination: &Path) -> Result<()> {
    let source_metadata = fs::symlink_metadata(source).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CrabError::Configuration {
                key: "workflow checkpoint state missing".to_owned(),
                origin: source.display().to_string(),
            }
        } else {
            CrabError::Io(error)
        }
    })?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
        return Err(CrabError::Configuration {
            key: "workflow checkpoint state invalid".to_owned(),
            origin: source.display().to_string(),
        });
    }
    if fs::symlink_metadata(destination).is_ok() {
        return Err(CrabError::Configuration {
            key: "workflow checkpoint state collision".to_owned(),
            origin: destination.display().to_string(),
        });
    }
    let parent = destination
        .parent()
        .ok_or_else(|| CrabError::Configuration {
            key: "workflow checkpoint state path invalid".to_owned(),
            origin: destination.display().to_string(),
        })?;
    ensure_checkpoint_parent_not_symlink(parent)?;
    fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let temporary = parent.join(format!(
        ".{}.resume-{}",
        destination
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("checkpoint"),
        uuid::Uuid::now_v7()
    ));
    let result = copy_checkpoint_tree(source, &temporary)
        .and_then(|()| validate_checkpoint_state(&temporary))
        .and_then(|()| fs::rename(&temporary, destination).map_err(CrabError::Io));
    if let Err(error) = result {
        let _ = remove_existing_path(&temporary);
        return Err(error);
    }
    Ok(())
}

fn ensure_checkpoint_parent_not_symlink(parent: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in parent.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        current.push(component.as_os_str());
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            return Err(CrabError::Configuration {
                key: "workflow checkpoint state parent symlink".to_owned(),
                origin: current.display().to_string(),
            });
        }
    }
    Ok(())
}

fn copy_checkpoint_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source).map_err(CrabError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(CrabError::Configuration {
            key: "workflow checkpoint state symlink".to_owned(),
            origin: source.display().to_string(),
        });
    }
    if metadata.is_dir() {
        fs::create_dir_all(destination).map_err(CrabError::Io)?;
        let mut entries = fs::read_dir(source)
            .map_err(CrabError::Io)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(CrabError::Io)?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.contains(".tmp-")
                || name.contains(".backup-")
                || name.contains(".resume-")
                || name.contains(".pull-")
                || name.ends_with(".lock")
            {
                continue;
            }
            copy_checkpoint_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        preserve_mode(source, destination)?;
        return Ok(());
    }
    if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }
        fs::copy(source, destination).map_err(CrabError::Io)?;
        preserve_mode(source, destination)?;
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "workflow checkpoint state entry invalid".to_owned(),
        origin: source.display().to_string(),
    })
}

fn checkpoint_lineage_paths(state_root: &Path) -> Result<Vec<PathBuf>> {
    if !state_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(state_root)
        .map_err(CrabError::Io)?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let is_lineage = entry.file_type().ok().is_some_and(|kind| kind.is_file())
                && path.extension().and_then(|value| value.to_str()) == Some("json")
                && path.file_name().and_then(|value| value.to_str()) != Some("reset.json");
            is_lineage.then_some(path)
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn load_checkpoint_lineage(path: &Path) -> Result<CheckpointLineage> {
    CheckpointLineage::load(path).map_err(|error| CrabError::Configuration {
        key: "workflow checkpoint lineage".to_owned(),
        origin: format!("{}: {error}", path.display()),
    })
}

fn checkpoint_records(repo_root: &Path, exp_id: &ExperimentId) -> Result<Vec<CheckpointRecord>> {
    let paths = checkpoint_lineage_paths(&checkpoint_state_dir(repo_root, exp_id))?;
    let mut records = Vec::new();
    for path in paths {
        records.extend(load_checkpoint_lineage(&path)?.records);
    }
    records.sort_by(|left, right| {
        left.created_at_unix_ms
            .cmp(&right.created_at_unix_ms)
            .then_with(|| left.stage.cmp(&right.stage))
            .then_with(|| left.sequence.cmp(&right.sequence))
    });
    Ok(records)
}

fn select_checkpoint_record(
    repo_root: &Path,
    exp_id: &ExperimentId,
    selector: &str,
) -> Result<CheckpointRecord> {
    let paths = checkpoint_lineage_paths(&checkpoint_state_dir(repo_root, exp_id))?;
    select_checkpoint_from_paths(&paths, selector).map(|(_, record)| record)
}

fn select_checkpoint_from_paths(
    paths: &[PathBuf],
    selector: &str,
) -> Result<(String, CheckpointRecord)> {
    let mut matches = Vec::new();
    for path in paths {
        let lineage = load_checkpoint_lineage(path)?;
        let stage = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_owned();
        for record in lineage.records {
            if record.id == selector
                || selector
                    .parse::<u64>()
                    .is_ok_and(|sequence| record.sequence == sequence)
            {
                matches.push((stage.clone(), record));
            }
        }
    }
    match matches.as_slice() {
        [(stage, record)] => Ok((stage.clone(), record.clone())),
        [] => Err(CrabError::Configuration {
            key: "workflow checkpoint".to_owned(),
            origin: format!("checkpoint selector '{selector}' was not found"),
        }),
        _ => Err(CrabError::Configuration {
            key: "workflow checkpoint".to_owned(),
            origin: format!("checkpoint selector '{selector}' is ambiguous"),
        }),
    }
}

fn apply_checkpoint_record_to(
    state_root: &Path,
    target_root: &Path,
    record: &CheckpointRecord,
) -> Result<Vec<PathBuf>> {
    let mut entries = record
        .outputs
        .iter()
        .map(|(relative, hash)| (relative.as_str(), hash.as_str(), "output"))
        .collect::<Vec<_>>();
    entries.extend(
        record
            .metrics
            .iter()
            .map(|(relative, hash)| (relative.as_str(), hash.as_str(), "metric")),
    );
    entries.sort_by(|left, right| left.0.cmp(right.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(CrabError::Configuration {
            key: "workflow checkpoint".to_owned(),
            origin: format!("checkpoint {} has duplicate output/metric paths", record.id),
        });
    }
    if entries.windows(2).any(|pair| {
        let left = Path::new(pair[0].0);
        let right = Path::new(pair[1].0);
        left.starts_with(right) || right.starts_with(left)
    }) {
        return Err(CrabError::Configuration {
            key: "workflow checkpoint".to_owned(),
            origin: format!(
                "checkpoint {} has overlapping output/metric paths",
                record.id
            ),
        });
    }

    let mut prepared = Vec::with_capacity(entries.len());
    for (index, (relative, hash, kind)) in entries.iter().enumerate() {
        let relative = PathBuf::from(relative);
        if let Err(error) = ensure_safe_workspace_relpath(&relative) {
            remove_prepared_checkpoint_temporaries(&prepared);
            return Err(error);
        }
        let Some(digest) = (*hash).strip_prefix("b3:") else {
            remove_prepared_checkpoint_temporaries(&prepared);
            return Err(CrabError::Configuration {
                key: "workflow checkpoint".to_owned(),
                origin: format!("checkpoint {} has an invalid {kind} hash", record.id),
            });
        };
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            remove_prepared_checkpoint_temporaries(&prepared);
            return Err(CrabError::Configuration {
                key: "workflow checkpoint".to_owned(),
                origin: format!("checkpoint {} has an invalid {kind} hash", record.id),
            });
        }
        let source = state_root.join("objects").join(digest).join("payload");
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) => {
                remove_prepared_checkpoint_temporaries(&prepared);
                return Err(CrabError::Configuration {
                    key: "workflow checkpoint object missing".to_owned(),
                    origin: format!("{}: {error}", source.display()),
                });
            }
        };
        if !metadata.file_type().is_file() && !metadata.file_type().is_dir() {
            remove_prepared_checkpoint_temporaries(&prepared);
            return Err(CrabError::Configuration {
                key: "workflow checkpoint object invalid".to_owned(),
                origin: source.display().to_string(),
            });
        }
        let actual = match checkpoint_payload_hash(&source) {
            Ok(actual) => actual,
            Err(error) => {
                remove_prepared_checkpoint_temporaries(&prepared);
                return Err(error);
            }
        };
        if actual != digest {
            remove_prepared_checkpoint_temporaries(&prepared);
            return Err(CrabError::Configuration {
                key: "workflow checkpoint object corrupt".to_owned(),
                origin: format!("{}: expected {digest}, got {actual}", source.display()),
            });
        }
        let target = target_root.join(&relative);
        if let Some(parent) = target.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            remove_prepared_checkpoint_temporaries(&prepared);
            return Err(CrabError::Io(error));
        }
        let temporary = target.with_file_name(format!(
            ".{}.checkpoint-tmp-{}-{index}-{}",
            target
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("output"),
            std::process::id(),
            crab_types::time::now_rfc3339_millis().replace(':', "")
        ));
        if let Err(error) = remove_existing_path(&temporary) {
            remove_prepared_checkpoint_temporaries(&prepared);
            return Err(error);
        }
        if let Err(error) = snapshot_payload(&source, &temporary) {
            remove_prepared_checkpoint_temporaries(&prepared);
            let _ = remove_existing_path(&temporary);
            return Err(CrabError::Configuration {
                key: "workflow checkpoint apply".to_owned(),
                origin: error.to_string(),
            });
        }
        prepared.push((relative, temporary, target));
    }

    let mut swaps = Vec::with_capacity(prepared.len());
    for (index, (_, temporary, target)) in prepared.iter().enumerate() {
        match CheckpointTargetSwap::apply(temporary, target) {
            Ok(swap) => swaps.push(swap),
            Err(error) => {
                for swap in swaps.into_iter().rev() {
                    drop(swap);
                }
                for (_, temporary, _) in prepared.iter().skip(index) {
                    let _ = remove_existing_path(temporary);
                }
                return Err(error);
            }
        }
    }
    for swap in swaps {
        swap.finish();
    }
    let mut applied = prepared
        .into_iter()
        .map(|(relative, _, _)| relative)
        .collect::<Vec<_>>();
    applied.sort();
    Ok(applied)
}

fn remove_prepared_checkpoint_temporaries(prepared: &[(PathBuf, PathBuf, PathBuf)]) {
    for (_, temporary, _) in prepared {
        let _ = remove_existing_path(temporary);
    }
}

struct CheckpointTargetSwap {
    target: PathBuf,
    backup: Option<PathBuf>,
    committed: bool,
}

impl CheckpointTargetSwap {
    fn apply(temporary: &Path, target: &Path) -> Result<Self> {
        let backup = target.with_file_name(format!(
            ".{}.checkpoint-backup-{}-{}",
            target
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("output"),
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let had_target = match fs::symlink_metadata(target) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(CrabError::Io(error)),
        };
        if had_target {
            fs::rename(target, &backup).map_err(CrabError::Io)?;
        }
        if let Err(error) = fs::rename(temporary, target) {
            if had_target {
                let _ = fs::rename(&backup, target);
            }
            return Err(CrabError::Io(error));
        }
        Ok(Self {
            target: target.to_owned(),
            backup: had_target.then_some(backup),
            committed: false,
        })
    }

    fn finish(mut self) {
        self.committed = true;
        if let Some(backup) = self.backup.take()
            && let Err(error) = remove_existing_path(&backup)
        {
            // The target is already the committed snapshot. A cleanup failure
            // must not turn a multi-path apply into a partial rollback.
            tracing::warn!(path = %backup.display(), error = %error, "checkpoint backup cleanup deferred");
        }
    }
}

impl Drop for CheckpointTargetSwap {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = remove_existing_path(&self.target);
        if let Some(backup) = self.backup.take() {
            let _ = fs::rename(backup, &self.target);
        }
    }
}

fn checkpoint_payload_hash(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path).map_err(CrabError::Io)?;
    let hash = if metadata.is_file() {
        let mut file = fs::File::open(path).map_err(CrabError::Io)?;
        let mut hasher = blake3::Hasher::new();
        std::io::copy(&mut file, &mut hasher).map_err(CrabError::Io)?;
        *hasher.finalize().as_bytes()
    } else if metadata.is_dir() {
        crab_workflow::hasher::hash_directory(path, false)?.hash
    } else {
        return Err(CrabError::Configuration {
            key: "workflow checkpoint object invalid".to_owned(),
            origin: path.display().to_string(),
        });
    };
    Ok(blake3::Hash::from(hash).to_hex().to_string())
}

fn write_checkpoint_reset_decision(state_root: &Path, decision: &serde_json::Value) -> Result<()> {
    ensure_checkpoint_parent_not_symlink(state_root)?;
    fs::create_dir_all(state_root).map_err(CrabError::Io)?;
    let path = state_root.join("reset.json");
    let temporary = state_root.join(format!(
        ".reset.json.tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    let bytes = serde_json::to_vec_pretty(decision)
        .map_err(|error| CrabError::Internal(format!("serialize checkpoint reset: {error}")))?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(CrabError::Io)?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }
    if let Err(error) = replace_checkpoint_reset_file(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn replace_checkpoint_reset_file(temporary: &Path, destination: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(temporary, destination).map_err(CrabError::Io)
    }

    #[cfg(windows)]
    {
        let parent = destination
            .parent()
            .ok_or_else(|| CrabError::Configuration {
                key: "exp reset".to_owned(),
                origin: format!("reset path has no parent: {}", destination.display()),
            })?;
        let backup = parent.join(format!(
            ".reset.json.backup-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let had_destination = match fs::symlink_metadata(destination) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(CrabError::Io(error)),
        };
        if had_destination {
            fs::rename(destination, &backup).map_err(CrabError::Io)?;
        }
        if let Err(error) = fs::rename(temporary, destination) {
            if had_destination {
                let _ = fs::rename(&backup, destination);
            }
            return Err(CrabError::Io(error));
        }
        if had_destination && let Err(error) = fs::remove_file(&backup) {
            tracing::warn!(path = %backup.display(), error = %error, "checkpoint reset backup cleanup deferred");
        }
        Ok(())
    }
}

fn apply_experiment_snapshot_to(
    repo_root: &Path,
    exp_id: &ExperimentId,
    target_root: &Path,
    command: &str,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let snapshot_dir = workspace_dir_path(repo_root, exp_id);
    if !snapshot_dir.is_dir() {
        return Err(CrabError::Configuration {
            key: command.to_owned(),
            origin: format!(
                "experiment {exp_id} has no workspace snapshot; rerun it with a Crab version that records apply snapshots"
            ),
        });
    }

    let manifest = read_workspace_manifest_for_command(repo_root, exp_id, command)?;
    let mut deleted = Vec::new();
    for rel in &manifest.deleted {
        ensure_safe_workspace_relpath(rel)?;
        let target = target_root.join(rel);
        if remove_existing_path(&target)? {
            deleted.push(rel.clone());
        }
    }

    let mut applied = Vec::new();
    apply_workspace_tree(&snapshot_dir, &snapshot_dir, target_root, &mut applied)?;
    applied.sort();
    deleted.sort();
    Ok((applied, deleted))
}

/// Testable `exp save` entry point. Captures the current workspace as
/// an experiment without executing the DAG.
pub fn run_exp_save(args: &SaveArgs, repo_root: &Path) -> Result<ExpSavePayload> {
    for path in &args.include_untracked {
        ensure_safe_workspace_relpath(path)?;
    }
    let name = args
        .name
        .as_deref()
        .map(|raw| normalize_experiment_name(raw, "exp save"))
        .transpose()?;
    if args.force {
        info!("exp save: --force accepted; UUID-based saves are always new experiments");
    }
    let save_selection = resolve_exp_save_selection(args, repo_root)?;

    let started_at = crab_types::time::now_rfc3339_millis();
    let exp_id = ExperimentId::new_v7();
    let base_commit = resolve_current_head(repo_root)?;
    capture_workspace_snapshot(repo_root, repo_root, &base_commit, &exp_id)?;

    let declared_metrics = save_selection.declared_metrics;
    let metrics = collect_metric_values(repo_root, &declared_metrics);
    let stages = read_current_lockfile_stage_hashes(repo_root, save_selection.stages.as_ref());
    let metadata = ExperimentMetadata {
        schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
        exp_id,
        base_commit: base_commit.clone(),
        queue_commit: None,
        name: name.clone(),
        message: args.message.clone(),
        status: "saved".to_owned(),
        param_overrides: BTreeMap::new(),
        stages: stages.clone(),
        metrics,
        cli_args: std::env::args().collect(),
        host_fingerprint: exp_host_fingerprint(),
        started_at: started_at.clone(),
        ended_at: Some(crab_types::time::now_rfc3339_millis()),
    };
    write_local_metadata(repo_root, &metadata)?;

    let payload = ExpSavePayload {
        exp_id: exp_id.to_string(),
        base_commit,
        name,
        message: args.message.clone(),
        stages,
        metrics_files: declared_metrics,
        status: "saved".to_owned(),
        started_at,
    };
    emit_save(&payload, args.output_mode());
    Ok(payload)
}

/// Testable `exp rename` entry point. Renames the local experiment label
/// without changing its immutable UUID or workspace snapshot.
pub fn run_exp_rename(args: &RenameArgs, repo_root: &Path) -> Result<ExpRenamePayload> {
    let new_name = normalize_experiment_name(&args.name, "exp rename")?;
    let exp_id = resolve_experiment_id(repo_root, &args.id)?;
    let mut metadata = read_local_metadata(repo_root, &exp_id)?;
    if !args.force {
        ensure_experiment_name_available(repo_root, &exp_id, &new_name)?;
    }

    let old_name = metadata.name.clone();
    metadata.name = Some(new_name.clone());
    write_local_metadata(repo_root, &metadata)?;

    let payload = ExpRenamePayload {
        exp_id: exp_id.to_string(),
        old_name,
        new_name,
    };
    emit_rename(&payload, args.output_mode());
    Ok(payload)
}

/// Testable `exp push` entry point. Uploads experiment metadata and
/// its captured apply snapshot to the configured Crab remote.
pub async fn run_exp_push(args: &PushArgs, repo_root: &Path) -> Result<ExpPushPayload> {
    let config = Config::resolve_for_repo(repo_root)?;
    if !config.workflow.enabled {
        return Err(CrabError::WorkflowDisabled);
    }
    cache::check_remote_cache_readonly(config.workflow.remote_cache_readonly)?;

    let ids = select_local_experiment_ids(repo_root, &args.ids, args.all, "exp push")?;
    let remote = build_experiment_remote(
        repo_root,
        &config,
        "workflow-exp-push",
        ExperimentRemoteAccess::Write,
    )
    .await?;
    let payload =
        push_experiments_to_remote(&remote.store, &remote.prefix, repo_root, &ids, args.force)
            .await?;
    emit_push(&payload, args.output_mode());
    Ok(payload)
}

/// Testable `exp pull` entry point. Downloads remote experiment
/// metadata and apply snapshots into the local experiment cache.
pub async fn run_exp_pull(args: &PullArgs, repo_root: &Path) -> Result<ExpPullPayload> {
    let config = Config::resolve_for_repo(repo_root)?;
    if !config.workflow.enabled {
        return Err(CrabError::WorkflowDisabled);
    }

    let remote = build_experiment_remote(
        repo_root,
        &config,
        "workflow-exp-pull",
        ExperimentRemoteAccess::Read,
    )
    .await?;
    let ids = resolve_remote_experiment_ids_from_remote(&remote, &args.ids, args.all).await?;
    let payload = pull_experiments_from_remote(&remote, repo_root, &ids, args.force).await?;
    emit_pull(&payload, args.output_mode());
    Ok(payload)
}

/// Testable `exp remove` entry point. Returns the payload so tests
/// can assert on removed/kept without parsing stdout.
pub async fn run_exp_remove(args: &RemoveArgs, repo_root: &Path) -> Result<ExpRemovePayload> {
    if args.git_remote.is_some() {
        return run_exp_remove_remote(args, repo_root).await;
    }

    run_exp_remove_local(args, repo_root)
}

fn run_exp_remove_local(args: &RemoveArgs, repo_root: &Path) -> Result<ExpRemovePayload> {
    if args.queue {
        return run_exp_remove_queue(args, repo_root);
    }

    let summaries = collect_summaries(repo_root)?;
    let all_ids: BTreeSet<String> = summaries.iter().map(|summary| summary.id.clone()).collect();

    if args.all && args.keep {
        return Err(CrabError::Configuration {
            key: "exp remove".to_owned(),
            origin: "--all cannot be combined with --keep".to_owned(),
        });
    }
    if args.all && (!args.ids.is_empty() || args.rev.is_some() || args.limit.is_some()) {
        return Err(CrabError::Configuration {
            key: "exp remove --all".to_owned(),
            origin: "--all cannot be combined with ids, --rev, or --num".to_owned(),
        });
    }
    if args.keep && args.ids.is_empty() && args.rev.is_none() && args.limit.is_none() {
        return Err(CrabError::Configuration {
            key: "exp remove --keep".to_owned(),
            origin: "--keep requires one or more experiment ids, names, --rev, or --num".to_owned(),
        });
    }
    if args.keep && !args.ids.is_empty() && (args.rev.is_some() || args.limit.is_some()) {
        return Err(CrabError::Configuration {
            key: "exp remove --keep".to_owned(),
            origin: "--keep cannot combine explicit ids or names with --rev or --num".to_owned(),
        });
    }
    if !args.all && args.ids.is_empty() && args.rev.is_none() && args.limit.is_none() {
        return Err(CrabError::Configuration {
            key: "exp remove".to_owned(),
            origin: "pass one or more experiment ids, names, --all, --queue, --rev, --num, or --keep <selector>".to_owned(),
        });
    }

    let (selected_ids, selected_queue) = if args.rev.is_some() || args.limit.is_some() {
        (
            select_experiment_ids_by_revs(repo_root, &summaries, args.rev.as_deref(), args.limit)?,
            BTreeSet::new(),
        )
    } else if args.keep {
        (
            resolve_experiment_ids(repo_root, &args.ids)?,
            BTreeSet::new(),
        )
    } else {
        resolve_exp_remove_explicit_ids(repo_root, &args.ids)?
    };
    let selected: BTreeSet<String> = selected_ids
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    let removed: Vec<String> = if args.all {
        all_ids.iter().cloned().collect()
    } else if args.keep {
        all_ids.difference(&selected).cloned().collect()
    } else {
        selected.iter().cloned().collect()
    };
    let removed_set: BTreeSet<String> = removed.iter().cloned().collect();
    let kept: Vec<String> = all_ids.difference(&removed_set).cloned().collect();
    let removed_queue: Vec<String> = selected_queue.iter().cloned().collect();
    let kept_queue = queue_ids_except(repo_root, &selected_queue)?;

    if !args.dry_run {
        for id in &removed {
            let exp_id = parse_experiment_id(id)?;
            remove_experiment_files(repo_root, &exp_id)?;
        }
        remove_queue_ids(repo_root, &removed_queue)?;
    }

    let payload = ExpRemovePayload {
        dry_run: args.dry_run,
        removed,
        kept,
        removed_remote: Vec::new(),
        kept_remote: Vec::new(),
        removed_queue,
        kept_queue,
    };
    emit_remove(&payload, args.output_mode());
    Ok(payload)
}

async fn run_exp_remove_remote(args: &RemoveArgs, repo_root: &Path) -> Result<ExpRemovePayload> {
    if args.queue {
        return Err(CrabError::Configuration {
            key: "exp remove --git-remote".to_owned(),
            origin: "--git-remote cannot be combined with --queue".to_owned(),
        });
    }

    let config = Config::resolve_for_repo(repo_root)?;
    if !config.workflow.enabled {
        return Err(CrabError::WorkflowDisabled);
    }
    cache::check_remote_cache_readonly(config.workflow.remote_cache_readonly)?;

    let remote = args
        .git_remote
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: "exp remove --git-remote".to_owned(),
            origin: "pass a Git remote name or crab:// URL".to_owned(),
        })?;
    let url = resolve_exp_remove_remote_url(repo_root, remote)?;
    let remote = build_experiment_remote_from_url(
        &config,
        &url,
        "workflow-exp-remove",
        ExperimentRemoteAccess::Write,
    )
    .await?;
    let payload = remove_remote_experiments(&remote.store, &remote.prefix, args, repo_root).await?;
    emit_remove(&payload, args.output_mode());
    Ok(payload)
}

fn run_exp_remove_queue(args: &RemoveArgs, repo_root: &Path) -> Result<ExpRemovePayload> {
    if args.keep {
        return Err(CrabError::Configuration {
            key: "exp remove --queue".to_owned(),
            origin: "--queue cannot be combined with --keep".to_owned(),
        });
    }
    if args.all || args.rev.is_some() || args.limit.is_some() {
        return Err(CrabError::Configuration {
            key: "exp remove --queue".to_owned(),
            origin: "--queue cannot be combined with --all, --rev, or --num".to_owned(),
        });
    }

    let selected = resolve_pending_queue_ids(repo_root, &args.ids, true)?;
    let removed_queue: Vec<String> = selected.iter().cloned().collect();
    let kept_queue = queue_ids_except(repo_root, &selected)?;
    if !args.dry_run {
        remove_queue_ids(repo_root, &removed_queue)?;
    }

    let payload = ExpRemovePayload {
        dry_run: args.dry_run,
        removed: Vec::new(),
        kept: collect_summaries(repo_root)?
            .into_iter()
            .map(|summary| summary.id)
            .collect(),
        removed_remote: Vec::new(),
        kept_remote: Vec::new(),
        removed_queue,
        kept_queue,
    };
    emit_remove(&payload, args.output_mode());
    Ok(payload)
}

fn resolve_exp_remove_explicit_ids(
    repo_root: &Path,
    raw_ids: &[String],
) -> Result<(Vec<ExperimentId>, BTreeSet<String>)> {
    let mut local_ids = Vec::new();
    let mut local_seen = BTreeSet::new();
    let mut queue_ids = BTreeSet::new();

    for raw in raw_ids {
        match resolve_experiment_id(repo_root, raw) {
            Ok(id) => {
                if local_seen.insert(id.to_string()) {
                    local_ids.push(id);
                }
            }
            Err(CrabError::ExperimentNotFound { .. }) => {
                let resolved =
                    resolve_pending_queue_ids(repo_root, std::slice::from_ref(raw), false)?;
                queue_ids.extend(resolved);
            }
            Err(e) => return Err(e),
        }
    }

    Ok((local_ids, queue_ids))
}

fn select_experiment_ids_by_revs(
    repo_root: &Path,
    summaries: &[ExpSummary],
    rev: Option<&str>,
    limit: Option<isize>,
) -> Result<Vec<ExperimentId>> {
    let base_commits = select_base_commits(repo_root, rev, limit)?;
    summaries
        .iter()
        .filter(|summary| base_commits.contains(&summary.base_commit))
        .map(|summary| parse_experiment_id(&summary.id))
        .collect()
}

fn select_base_commits(
    repo_root: &Path,
    rev: Option<&str>,
    limit: Option<isize>,
) -> Result<BTreeSet<String>> {
    let baseline = rev.unwrap_or("HEAD");
    if let Some(limit) = limit {
        return list_first_parent_commits(repo_root, baseline, limit);
    }

    Ok(BTreeSet::from([resolve_git_commit(repo_root, baseline)?]))
}

fn resolve_git_commit(repo_root: &Path, rev: &str) -> Result<String> {
    let commitish = format!("{rev}^{{commit}}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", &commitish])
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git rev-parse: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "exp remove --rev".to_owned(),
            origin: format!("cannot resolve commit '{rev}': {}", stderr.trim()),
        });
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(CrabError::Internal(format!(
            "git rev-parse returned unexpected commit SHA '{sha}'"
        )));
    }
    Ok(sha)
}

fn list_first_parent_commits(
    repo_root: &Path,
    rev: &str,
    limit: isize,
) -> Result<BTreeSet<String>> {
    if limit == 0 {
        return Ok(BTreeSet::new());
    }

    let mut command = Command::new("git");
    command.arg("rev-list").arg("--first-parent");
    let max_count;
    if limit > 0 {
        max_count = format!("--max-count={limit}");
        command.arg(max_count.as_str());
    }
    let output = command
        .arg(rev)
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git rev-list: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "exp remove --num".to_owned(),
            origin: format!(
                "cannot list first-parent commits from '{rev}': {}",
                stderr.trim()
            ),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn resolve_pending_queue_ids(
    repo_root: &Path,
    selectors: &[String],
    select_all_pending: bool,
) -> Result<BTreeSet<String>> {
    let queue = ExpQueue::new(crate::cmd::exp_queue::queue_dir(repo_root));
    let entries = queue.list_all()?;
    if select_all_pending && selectors.is_empty() {
        return Ok(entries
            .iter()
            .filter(|entry| entry.status == ExpStatus::Pending)
            .map(|entry| entry.id.clone())
            .collect());
    }

    let mut selected = BTreeSet::new();
    for selector in selectors {
        let entry = resolve_queue_entry_for_exp_remove(&entries, selector)?;
        if entry.status == ExpStatus::Running {
            return Err(CrabError::Configuration {
                key: selector.clone(),
                origin: "queue task is running; use queue kill or queue stop --kill first"
                    .to_owned(),
            });
        }
        if entry.status != ExpStatus::Pending {
            return Err(CrabError::Configuration {
                key: selector.clone(),
                origin:
                    "queue task has already run; use crab queue remove for completed task records"
                        .to_owned(),
            });
        }
        selected.insert(entry.id.clone());
    }
    Ok(selected)
}

fn resolve_queue_entry_for_exp_remove<'a>(
    entries: &'a [ExpQueueEntry],
    selector: &str,
) -> Result<&'a ExpQueueEntry> {
    let mut matches = entries
        .iter()
        .filter(|entry| entry.id == selector || entry.id.starts_with(selector));
    let Some(first) = matches.next() else {
        return Err(CrabError::ExperimentNotFound {
            id: selector.to_owned(),
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

fn queue_ids_except(repo_root: &Path, removed: &BTreeSet<String>) -> Result<Vec<String>> {
    let queue = ExpQueue::new(crate::cmd::exp_queue::queue_dir(repo_root));
    Ok(queue
        .list_all()?
        .into_iter()
        .filter(|entry| !removed.contains(&entry.id))
        .map(|entry| entry.id)
        .collect())
}

fn remove_queue_ids(repo_root: &Path, ids: &[String]) -> Result<()> {
    let queue = ExpQueue::new(crate::cmd::exp_queue::queue_dir(repo_root));
    for id in ids {
        queue.remove(id)?;
        crate::cmd::exp_queue::remove_queue_log(repo_root, id)?;
    }
    Ok(())
}

/// Testable `exp clean` entry point. Removes only transient
/// experiment runtime files, not saved experiment metadata.
pub fn run_exp_clean(args: &CleanArgs, repo_root: &Path) -> Result<ExpCleanPayload> {
    let queue_clean = crate::cmd::exp_queue::clean_exp_queue_housekeeping(repo_root)?;
    let removed_tmpdirs = crate::workflow::exp_worktree::sweep_orphan_experiment_tmpdirs(
        repo_root,
        &queue_clean.active_run_ids,
    )?;
    let payload = ExpCleanPayload {
        removed_tmpdirs,
        removed_active_markers: queue_clean.removed_active_markers,
        removed_kill_requests: queue_clean.removed_kill_requests,
        removed_logs: queue_clean.removed_logs,
    };
    emit_clean(&payload, args.output_mode());
    Ok(payload)
}

/// Testable `exp gc` entry point. Returns the payload so tests can
/// assert on kept/removed without parsing stdout.
pub fn run_exp_gc(args: &GcArgs, repo_root: &Path) -> Result<ExpGcPayload> {
    let summaries = collect_summaries(repo_root)?;
    // `collect_summaries` returns newest-first; split at `keep`.
    let (kept_summaries, removed_summaries): (Vec<_>, Vec<_>) = if summaries.len() <= args.keep {
        (summaries, Vec::new())
    } else {
        let (k, r) = summaries.split_at(args.keep);
        (k.to_vec(), r.to_vec())
    };

    // Actually remove the metadata blobs and any surviving tmpdirs
    // unless --dry-run.
    if !args.dry_run {
        for s in &removed_summaries {
            let id: ExperimentId = match s.id.parse() {
                Ok(i) => i,
                Err(e) => {
                    warn!(exp_id = %s.id, error = %e, "exp gc: unparseable id, skipping");
                    continue;
                }
            };
            match remove_experiment_files(repo_root, &id) {
                Ok(()) => info!(exp_id = %id, "exp gc: removed metadata and workspace snapshot"),
                Err(e) => {
                    warn!(exp_id = %id, error = %e, "exp gc: failed to remove experiment files");
                }
            }
        }

        // Sweep orphan tmpdirs whose exp_id is no longer in the
        // kept set. The sweep is idempotent — it's safe to run
        // even when no tmpdirs are present.
        let active_ids: Vec<ExperimentId> = kept_summaries
            .iter()
            .filter_map(|s| s.id.parse().ok())
            .collect();
        match crate::workflow::exp_worktree::sweep_orphan_experiment_tmpdirs(repo_root, &active_ids)
        {
            Ok(n) if n > 0 => info!(removed_tmpdirs = n, "exp gc: swept orphan tmpdirs"),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "exp gc: orphan tmpdir sweep failed"),
        }
    }

    let payload = ExpGcPayload {
        keep: args.keep,
        dry_run: args.dry_run,
        removed: removed_summaries.iter().map(|s| s.id.clone()).collect(),
        kept: kept_summaries.iter().map(|s| s.id.clone()).collect(),
    };
    emit_gc(&payload, args.output_mode());
    Ok(payload)
}

struct ExperimentRemote {
    store: Store,
    prefix: String,
    primary_fallback: Option<ExperimentPrimaryFallback>,
}

struct ExperimentPrimaryFallback {
    store: Store,
    prefix: String,
}

#[derive(Clone, Copy)]
enum ExperimentRemoteAccess {
    Read,
    Write,
}

async fn build_experiment_remote(
    repo_root: &Path,
    config: &Config,
    operation: &str,
    access: ExperimentRemoteAccess,
) -> Result<ExperimentRemote> {
    let url_str = crate::cmd::workflow::read_crab_remote_url(repo_root)?;
    build_experiment_remote_from_url(config, &url_str, operation, access).await
}

async fn build_experiment_remote_from_url(
    config: &Config,
    url_str: &str,
    operation: &str,
    access: ExperimentRemoteAccess,
) -> Result<ExperimentRemote> {
    let crab_url = CrabUrl::parse(url_str)?;
    let cancel = CancellationToken::new();
    let resolver = crate::replication::StoreResolver::new(config, &crab_url, &cancel);

    match access {
        ExperimentRemoteAccess::Write => {
            let selection = resolver.write_store(operation).await?;
            Ok(ExperimentRemote {
                store: selection.store,
                prefix: selection.router.repo_prefix().to_owned(),
                primary_fallback: None,
            })
        }
        ExperimentRemoteAccess::Read => {
            let selection = resolver.read_store(operation).await?;
            let primary_fallback = if matches!(
                &selection.source,
                crate::replication::ReadSource::Replica { .. }
            ) {
                let primary = resolver
                    .write_store("workflow-exp-pull-primary-fallback")
                    .await?;
                Some(ExperimentPrimaryFallback {
                    store: primary.store,
                    prefix: primary.router.repo_prefix().to_owned(),
                })
            } else {
                None
            };
            Ok(ExperimentRemote {
                store: selection.store,
                prefix: selection.router.repo_prefix().to_owned(),
                primary_fallback,
            })
        }
    }
}

fn resolve_exp_remove_remote_url(repo_root: &Path, remote: &str) -> Result<String> {
    if remote.starts_with("crab://") {
        return Ok(remote.to_owned());
    }

    let output = Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git remote get-url: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "exp remove --git-remote".to_owned(),
            origin: format!("cannot resolve Git remote '{remote}': {}", stderr.trim()),
        });
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if url.starts_with("crab://") {
        Ok(url)
    } else {
        Err(CrabError::Configuration {
            key: "exp remove --git-remote".to_owned(),
            origin: format!(
                "Git remote '{remote}' points to '{url}', but Crab experiment remotes require a crab:// URL"
            ),
        })
    }
}

fn select_local_experiment_ids(
    repo_root: &Path,
    raw_ids: &[String],
    all: bool,
    command: &str,
) -> Result<Vec<ExperimentId>> {
    if all && !raw_ids.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("{command} --all"),
            origin: "--all cannot be combined with explicit experiment ids".to_owned(),
        });
    }
    if all {
        return collect_summaries(repo_root)?
            .into_iter()
            .map(|summary| parse_experiment_id(&summary.id))
            .collect();
    }
    if raw_ids.is_empty() {
        return Err(CrabError::Configuration {
            key: command.to_owned(),
            origin: "pass one or more experiment ids or --all".to_owned(),
        });
    }
    resolve_experiment_ids(repo_root, raw_ids)
}

async fn push_experiments_to_remote(
    store: &Store,
    prefix: &str,
    repo_root: &Path,
    ids: &[ExperimentId],
    force: bool,
) -> Result<ExpPushPayload> {
    let mut pushed = Vec::new();
    let mut skipped = Vec::new();

    for id in ids {
        let ref_path = remote_exp_meta_ref_path(prefix, id);
        if !force && remote_object_exists(store, &ref_path).await? {
            let metadata = read_remote_metadata(store, prefix, id).await?;
            register_workflow_gc_roots(store, prefix, &metadata).await?;
            skipped.push(id.to_string());
            continue;
        }

        let metadata = read_local_metadata(repo_root, id)?;
        let snapshot_dir = workspace_dir_path(repo_root, id);
        if !snapshot_dir.is_dir() {
            return Err(CrabError::Configuration {
                key: "exp push".to_owned(),
                origin: format!(
                    "experiment {id} has no workspace snapshot; rerun it with a Crab version that records apply snapshots"
                ),
            });
        }
        if force {
            delete_remote_experiment(store, prefix, id).await?;
        }

        push_workspace_snapshot(store, prefix, repo_root, id).await?;
        push_checkpoint_state(store, prefix, repo_root, id).await?;

        let meta_bytes = metadata.canonical_json()?;
        let meta_hash = metadata.content_hash()?;
        store
            .put(
                &remote_exp_meta_object_path(prefix, id),
                Bytes::from(meta_bytes),
            )
            .await?;

        let stage_refs = experiment_stage_refs_json(&metadata)?;
        store
            .put(
                &remote_exp_stage_refs_object_path(prefix, id),
                Bytes::from(stage_refs),
            )
            .await?;

        // Establish the conservative workflow GC root after every immutable
        // experiment object is durable, but before publishing the visible
        // metadata ref. Union semantics protect concurrent pushes; a failed
        // ref publication can leave only an extra root, never an unsafe
        // deletion candidate.
        register_workflow_gc_roots(store, prefix, &metadata).await?;

        store.put(&ref_path, Bytes::from(meta_hash)).await?;
        pushed.push(id.to_string());
    }

    Ok(ExpPushPayload { pushed, skipped })
}

async fn register_workflow_gc_roots(
    store: &Store,
    prefix: &str,
    metadata: &ExperimentMetadata,
) -> Result<()> {
    let storage = store.as_storage();
    let registry_router = crab_storage::StoreLayout::new(storage.clone(), prefix.to_owned());
    crab_metadata::ref_registry::union_register_workflow_roots(
        storage,
        &registry_router,
        metadata.stages.values().cloned().collect(),
        vec![metadata.exp_id.to_string()],
    )
    .await
    .map_err(|error| CrabError::Configuration {
        key: "exp push workflow GC roots".to_owned(),
        origin: error.to_string(),
    })?;
    Ok(())
}

async fn pull_experiments_from_remote(
    remote: &ExperimentRemote,
    repo_root: &Path,
    ids: &[ExperimentId],
    force: bool,
) -> Result<ExpPullPayload> {
    let mut pulled = Vec::new();
    let mut skipped = Vec::new();

    for id in ids {
        if meta_file_path(repo_root, id).exists() && !force {
            skipped.push(id.to_string());
            continue;
        }

        match pull_experiment_from_store(&remote.store, &remote.prefix, repo_root, id).await {
            Ok(()) => {}
            Err(e) => {
                if let Some(primary) = &remote.primary_fallback {
                    pull_experiment_from_store(&primary.store, &primary.prefix, repo_root, id)
                        .await?;
                } else {
                    return Err(e);
                }
            }
        }
        pulled.push(id.to_string());
    }

    Ok(ExpPullPayload { pulled, skipped })
}

async fn pull_experiment_from_store(
    store: &Store,
    prefix: &str,
    repo_root: &Path,
    id: &ExperimentId,
) -> Result<()> {
    let metadata = read_remote_metadata(store, prefix, id).await?;
    pull_checkpoint_state(store, prefix, repo_root, id).await?;
    restore_workspace_snapshot(store, prefix, repo_root, id).await?;
    write_local_metadata(repo_root, &metadata)?;
    Ok(())
}

async fn remove_remote_experiments(
    store: &Store,
    prefix: &str,
    args: &RemoveArgs,
    repo_root: &Path,
) -> Result<ExpRemovePayload> {
    if args.all && args.keep {
        return Err(CrabError::Configuration {
            key: "exp remove --git-remote".to_owned(),
            origin: "--all cannot be combined with --keep".to_owned(),
        });
    }
    if args.all && (!args.ids.is_empty() || args.rev.is_some() || args.limit.is_some()) {
        return Err(CrabError::Configuration {
            key: "exp remove --git-remote --all".to_owned(),
            origin: "--all cannot be combined with ids, --rev, or --num".to_owned(),
        });
    }
    if args.keep && args.ids.is_empty() && args.rev.is_none() && args.limit.is_none() {
        return Err(CrabError::Configuration {
            key: "exp remove --git-remote --keep".to_owned(),
            origin: "--keep requires one or more experiment ids, names, --rev, or --num".to_owned(),
        });
    }
    if args.keep && !args.ids.is_empty() && (args.rev.is_some() || args.limit.is_some()) {
        return Err(CrabError::Configuration {
            key: "exp remove --git-remote --keep".to_owned(),
            origin: "--keep cannot combine explicit ids or names with --rev or --num".to_owned(),
        });
    }
    if !args.all && args.ids.is_empty() && args.rev.is_none() && args.limit.is_none() {
        return Err(CrabError::Configuration {
            key: "exp remove --git-remote".to_owned(),
            origin:
                "pass one or more experiment ids, names, --all, --rev, --num, or --keep <selector>"
                    .to_owned(),
        });
    }

    let all_ids = list_remote_experiment_ids(store, prefix).await?;
    let selected_ids = if args.rev.is_some() || args.limit.is_some() {
        select_remote_experiment_ids_by_revs(
            store,
            prefix,
            &all_ids,
            repo_root,
            args.rev.as_deref(),
            args.limit,
        )
        .await?
    } else if args.all {
        all_ids.clone()
    } else {
        resolve_remote_experiment_ids_from_list(store, prefix, &all_ids, &args.ids).await?
    };

    let all_set: BTreeSet<String> = all_ids
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let selected: BTreeSet<String> = selected_ids
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let removed_remote: Vec<String> = if args.all {
        all_ids
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    } else if args.keep {
        all_set.difference(&selected).cloned().collect()
    } else {
        selected.iter().cloned().collect()
    };
    let removed_set: BTreeSet<String> = removed_remote.iter().cloned().collect();
    let kept_remote: Vec<String> = all_set.difference(&removed_set).cloned().collect();

    if !args.dry_run {
        for id in &removed_remote {
            let exp_id = parse_experiment_id(id)?;
            delete_remote_experiment(store, prefix, &exp_id).await?;
        }
    }

    Ok(ExpRemovePayload {
        dry_run: args.dry_run,
        removed: Vec::new(),
        kept: Vec::new(),
        removed_remote,
        kept_remote,
        removed_queue: Vec::new(),
        kept_queue: Vec::new(),
    })
}

async fn select_remote_experiment_ids_by_revs(
    store: &Store,
    prefix: &str,
    remote_ids: &[ExperimentId],
    repo_root: &Path,
    rev: Option<&str>,
    limit: Option<isize>,
) -> Result<Vec<ExperimentId>> {
    let base_commits = select_base_commits(repo_root, rev, limit)?;
    let mut selected = Vec::new();
    for id in remote_ids {
        let metadata = read_remote_metadata(store, prefix, id).await?;
        if base_commits.contains(&metadata.base_commit) {
            selected.push(*id);
        }
    }
    Ok(selected)
}

async fn resolve_remote_experiment_ids_from_remote(
    remote: &ExperimentRemote,
    raw_ids: &[String],
    all: bool,
) -> Result<Vec<ExperimentId>> {
    if let Some(primary) = &remote.primary_fallback {
        // Experiment refs are mutable listing authority, unlike manifest-gated
        // repo objects. Resolve prefixes and --all against primary, then
        // download immutable payloads from the selected replica when possible.
        return resolve_remote_experiment_ids_for_command(
            &primary.store,
            &primary.prefix,
            raw_ids,
            all,
            "exp pull",
        )
        .await;
    }
    resolve_remote_experiment_ids_for_command(
        &remote.store,
        &remote.prefix,
        raw_ids,
        all,
        "exp pull",
    )
    .await
}

#[cfg(test)]
async fn resolve_remote_experiment_ids(
    store: &Store,
    prefix: &str,
    raw_ids: &[String],
    all: bool,
) -> Result<Vec<ExperimentId>> {
    resolve_remote_experiment_ids_for_command(store, prefix, raw_ids, all, "exp pull").await
}

async fn resolve_remote_experiment_ids_for_command(
    store: &Store,
    prefix: &str,
    raw_ids: &[String],
    all: bool,
    command: &str,
) -> Result<Vec<ExperimentId>> {
    if all && !raw_ids.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("{command} --all"),
            origin: "--all cannot be combined with explicit experiment ids".to_owned(),
        });
    }
    if !all && raw_ids.is_empty() {
        return Err(CrabError::Configuration {
            key: command.to_owned(),
            origin: "pass one or more experiment ids, names, or --all".to_owned(),
        });
    }

    let remote_ids = list_remote_experiment_ids(store, prefix).await?;
    if all {
        return Ok(remote_ids);
    }

    resolve_remote_experiment_ids_from_list(store, prefix, &remote_ids, raw_ids).await
}

async fn resolve_remote_experiment_ids_from_list(
    store: &Store,
    prefix: &str,
    remote_ids: &[ExperimentId],
    raw_ids: &[String],
) -> Result<Vec<ExperimentId>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(raw_ids.len());
    for raw in raw_ids {
        let id = resolve_remote_experiment_id(store, prefix, raw, remote_ids).await?;
        if seen.insert(id.to_string()) {
            out.push(id);
        }
    }
    Ok(out)
}

async fn resolve_remote_experiment_id(
    store: &Store,
    prefix: &str,
    raw: &str,
    remote_ids: &[ExperimentId],
) -> Result<ExperimentId> {
    if let Ok(id) = raw.parse::<ExperimentId>() {
        if remote_ids.contains(&id) {
            return Ok(id);
        }
        return Err(CrabError::ExperimentNotFound { id: raw.to_owned() });
    }

    let matches: Vec<String> = remote_ids
        .iter()
        .map(std::string::ToString::to_string)
        .filter(|id| id.starts_with(raw))
        .collect();
    match matches.as_slice() {
        [id] => return parse_experiment_id(id),
        [] => {}
        many => {
            return Err(CrabError::Configuration {
                key: "remote experiment id".to_owned(),
                origin: format!(
                    "prefix '{raw}' matches multiple experiments: {}",
                    many.join(", ")
                ),
            });
        }
    }

    let mut name_matches = Vec::new();
    for id in remote_ids {
        let metadata = read_remote_metadata(store, prefix, id).await?;
        if metadata.name.as_deref() == Some(raw) {
            name_matches.push(id.to_string());
        }
    }
    match name_matches.as_slice() {
        [id] => parse_experiment_id(id),
        [] => Err(CrabError::ExperimentNotFound { id: raw.to_owned() }),
        many => Err(CrabError::Configuration {
            key: "remote experiment name".to_owned(),
            origin: format!(
                "name '{raw}' matches multiple experiments: {}",
                many.join(", ")
            ),
        }),
    }
}

async fn list_remote_experiment_ids(store: &Store, prefix: &str) -> Result<Vec<ExperimentId>> {
    let ref_prefix = remote_key(prefix, EXP_META_REF_PREFIX);
    let metas = store
        .list_prefix(&ObjectPath::from(ref_prefix.clone()))
        .await?;
    let mut ids = Vec::new();
    for meta in metas {
        let key = meta.location.as_ref();
        let Some(raw) = key.strip_prefix(&ref_prefix) else {
            continue;
        };
        if raw.is_empty() || raw.contains('/') {
            continue;
        }
        let id = parse_experiment_id(raw)?;
        ids.push(id);
    }
    ids.sort_by(|left, right| right.cmp(left));
    Ok(ids)
}

async fn remote_object_exists(store: &Store, path: &ObjectPath) -> Result<bool> {
    match store.head(path).await {
        Ok(_) => Ok(true),
        Err(CrabError::NotFound { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

async fn delete_remote_experiment(store: &Store, prefix: &str, id: &ExperimentId) -> Result<()> {
    let ref_path = remote_exp_meta_ref_path(prefix, id);
    match store.delete(&ref_path).await {
        Ok(()) | Err(CrabError::NotFound { .. }) => {}
        Err(e) => return Err(e),
    }
    store
        .delete_prefix(&remote_exp_object_prefix(prefix, id))
        .await?;
    Ok(())
}

async fn read_remote_metadata(
    store: &Store,
    prefix: &str,
    id: &ExperimentId,
) -> Result<ExperimentMetadata> {
    let ref_path = remote_exp_meta_ref_path(prefix, id);
    let (ref_bytes, _) = store.get_with_etag(&ref_path).await?;
    let expected_hash = std::str::from_utf8(&ref_bytes)
        .map_err(|e| CrabError::CorruptObject {
            path: ref_path.as_ref().to_owned(),
            reason: format!("experiment metadata ref is not UTF-8: {e}"),
        })?
        .trim();

    let meta_path = remote_exp_meta_object_path(prefix, id);
    let (meta_bytes, _) = store.get_with_etag(&meta_path).await?;
    let metadata: ExperimentMetadata =
        serde_json::from_slice(&meta_bytes).map_err(|e| CrabError::CorruptObject {
            path: meta_path.as_ref().to_owned(),
            reason: format!("experiment metadata is not valid JSON: {e}"),
        })?;
    if metadata.exp_id != *id {
        return Err(CrabError::CorruptObject {
            path: meta_path.as_ref().to_owned(),
            reason: format!(
                "metadata id {} does not match requested experiment {id}",
                metadata.exp_id
            ),
        });
    }
    let actual_hash = metadata.content_hash()?;
    if expected_hash != actual_hash {
        return Err(CrabError::CorruptObject {
            path: ref_path.as_ref().to_owned(),
            reason: format!(
                "metadata ref points at {expected_hash}, but object hashes to {actual_hash}"
            ),
        });
    }
    Ok(metadata)
}

fn reject_sweeps_without_queue(entries: &[String]) -> Result<()> {
    for entry in entries {
        let Some((_, value)) = entry.split_once('=') else {
            continue;
        };
        if is_sweep_expression(value)? {
            return Err(CrabError::Configuration {
                key: format!("--set-param sweep expression requires --queue: {entry}"),
                origin: "exp run".into(),
            });
        }
    }
    Ok(())
}

/// Parse `--set` / `--set-param` entries into a sorted map.
///
/// Remove entries (`~key` or `file:~key`) may omit `=` and are stored
/// with an empty value because the shared override writer keys off
/// the operation prefix. Repeated keys are not allowed; the second
/// occurrence wins quietly is surprising, so we surface it as a
/// `Configuration` error.
fn parse_overrides(entries: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for entry in entries {
        let (key, value) = match entry.split_once('=') {
            Some((key, value)) => (key, value),
            None if override_allows_missing_value(entry) => (entry.as_str(), ""),
            None => {
                return Err(CrabError::Configuration {
                    key: format!("--set/--set-param entry missing '=': {entry}"),
                    origin: "cli".into(),
                });
            }
        };
        if key.is_empty() {
            return Err(CrabError::Configuration {
                key: format!("--set/--set-param entry has empty key: {entry}"),
                origin: "cli".into(),
            });
        }
        if out.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(CrabError::Configuration {
                key: format!("--set/--set-param key repeated: {key}"),
                origin: "cli".into(),
            });
        }
    }
    Ok(out)
}

fn normalize_experiment_name(raw: &str, command: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(CrabError::Configuration {
            key: command.to_owned(),
            origin: "experiment name must not be empty".to_owned(),
        });
    }
    Ok(name.to_owned())
}

fn ensure_experiment_name_available(
    repo_root: &Path,
    current_id: &ExperimentId,
    name: &str,
) -> Result<()> {
    let current = current_id.to_string();
    if let Some(existing) = collect_summaries(repo_root)?
        .into_iter()
        .find(|summary| summary.id != current && summary.name.as_deref() == Some(name))
    {
        return Err(CrabError::Configuration {
            key: "exp rename".to_owned(),
            origin: format!(
                "experiment {} already has name '{name}'; pass --force to reuse it",
                existing.id
            ),
        });
    }
    Ok(())
}

/// Read the list of declared metrics files from `crab.yaml`.
///
/// Returns an empty vector when the yaml is missing — the DAG
/// runner will surface that as a richer error if the user actually
/// expected a workflow. A parse error here is worth surfacing
/// because the DAG run would fail with the same parse error
/// anyway.
fn read_declared_metrics(tmpdir: &Path) -> Result<Vec<PathBuf>> {
    let yaml_path = tmpdir.join("crab.yaml");
    if !yaml_path.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&yaml_path).map_err(CrabError::Io)?;
    let wf = yaml_mod::parse_at(&yaml_path, &text)?;
    Ok(wf.metrics)
}

#[derive(Debug)]
struct ExpSaveSelection {
    stages: Option<BTreeSet<StageName>>,
    declared_metrics: Vec<PathBuf>,
}

fn resolve_exp_save_selection(args: &SaveArgs, repo_root: &Path) -> Result<ExpSaveSelection> {
    let mode = if save_targets_need_recursive_discovery(args) {
        DiscoverMode::Recursive
    } else {
        DiscoverMode::Root
    };
    let yaml_paths = discover::discover(repo_root, mode)?;
    if yaml_paths.is_empty() {
        if args.targets.is_empty() {
            return Ok(ExpSaveSelection {
                stages: None,
                declared_metrics: Vec::new(),
            });
        }
        return Err(CrabError::Configuration {
            key: "exp save target".to_owned(),
            origin: "no crab.yaml files were discovered".to_owned(),
        });
    }

    let (workflow, provenance) = load_save_workflow(repo_root, &yaml_paths)?;
    let stages = select_exp_save_stages(args, repo_root, &workflow, &provenance)?;
    let declared_metrics =
        read_declared_metrics_for_save(repo_root, &yaml_paths, &provenance, stages.as_ref())?;

    Ok(ExpSaveSelection {
        stages,
        declared_metrics,
    })
}

fn save_targets_need_recursive_discovery(args: &SaveArgs) -> bool {
    args.recursive
        || args
            .targets
            .iter()
            .any(|target| target.contains('/') || target.contains('\\') || target.contains(':'))
}

fn load_save_workflow(
    repo_root: &Path,
    yaml_paths: &[PathBuf],
) -> Result<(Workflow, BTreeMap<StageName, PathBuf>)> {
    if yaml_paths.len() == 1 {
        let path = &yaml_paths[0];
        let text = fs::read_to_string(path).map_err(CrabError::Io)?;
        let workflow = yaml_mod::parse_at(path, &text)?;
        let provenance = workflow
            .stages
            .keys()
            .map(|name| (name.clone(), path.clone()))
            .collect();
        return Ok((workflow, provenance));
    }
    discover::parse_all_with_provenance(repo_root, yaml_paths).map_err(Into::into)
}

fn select_exp_save_stages(
    args: &SaveArgs,
    repo_root: &Path,
    workflow: &Workflow,
    provenance: &BTreeMap<StageName, PathBuf>,
) -> Result<Option<BTreeSet<StageName>>> {
    if args.targets.is_empty() {
        return Ok(None);
    }

    let mut selected = BTreeSet::new();
    for raw in &args.targets {
        let matches = exp_save_target_stages(raw, args.recursive, repo_root, workflow, provenance)?;
        if matches.is_empty() {
            return Err(CrabError::Configuration {
                key: format!("exp save target '{raw}' matched no stages"),
                origin: "cli".to_owned(),
            });
        }
        selected.extend(matches);
    }

    Ok(Some(selected))
}

fn exp_save_target_stages(
    raw: &str,
    recursive: bool,
    repo_root: &Path,
    workflow: &Workflow,
    provenance: &BTreeMap<StageName, PathBuf>,
) -> Result<BTreeSet<StageName>> {
    if let Some((path, leaf)) = raw.rsplit_once(':')
        && !path.is_empty()
        && !leaf.is_empty()
    {
        let files = exp_save_target_workflow_files(path, recursive, repo_root, provenance)?;
        return Ok(provenance
            .iter()
            .filter(|(stage, source)| {
                files.contains(*source) && exp_save_stage_leaf_matches(stage, leaf)
            })
            .map(|(stage, _)| stage.clone())
            .collect());
    }

    if let Ok(stage_name) = StageName::parse_effective(raw)
        && workflow.stages.contains_key(&stage_name)
    {
        return Ok(BTreeSet::from([stage_name]));
    }

    let files = exp_save_target_workflow_files(raw, recursive, repo_root, provenance)?;
    Ok(provenance
        .iter()
        .filter(|(_, source)| files.contains(*source))
        .map(|(stage, _)| stage.clone())
        .collect())
}

fn exp_save_target_workflow_files(
    raw: &str,
    recursive: bool,
    repo_root: &Path,
    provenance: &BTreeMap<StageName, PathBuf>,
) -> Result<BTreeSet<PathBuf>> {
    let rel = normalize_exp_save_target_path(raw)?;
    let target = repo_root.join(&rel);
    let mut files = BTreeSet::new();

    if is_workflow_file_target(&rel) {
        for candidate in exp_save_workflow_file_candidates(repo_root, &rel) {
            if provenance.values().any(|source| source == &candidate) {
                files.insert(candidate);
            }
        }
        return Ok(files);
    }

    if target.is_dir() || recursive {
        for source in provenance.values() {
            if let Ok(source_rel) = source.strip_prefix(repo_root)
                && source_rel.starts_with(&rel)
            {
                files.insert(source.clone());
            }
        }
    }

    Ok(files)
}

fn normalize_exp_save_target_path(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(CrabError::Configuration {
            key: "exp save target".to_owned(),
            origin: format!("target must be repo-relative: {}", path.display()),
        });
    }

    let mut rel = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => rel.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CrabError::Configuration {
                    key: "exp save target".to_owned(),
                    origin: format!("target must stay inside the repo: {}", path.display()),
                });
            }
        }
    }
    if rel.as_os_str().is_empty() {
        return Err(CrabError::Configuration {
            key: "exp save target".to_owned(),
            origin: "target must name a stage, workflow file, or directory".to_owned(),
        });
    }
    Ok(rel)
}

fn is_workflow_file_target(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == "crab.yaml" || name == "dvc.yaml" || name.ends_with(".workflow.yaml")
}

fn exp_save_workflow_file_candidates(repo_root: &Path, rel: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![repo_root.join(rel)];
    if rel.file_name().and_then(|name| name.to_str()) == Some("dvc.yaml") {
        let mut alias = rel.to_path_buf();
        alias.set_file_name("crab.yaml");
        candidates.push(repo_root.join(alias));
    }
    candidates
}

fn exp_save_stage_leaf_matches(stage: &StageName, leaf: &str) -> bool {
    let effective = stage.as_str();
    effective == leaf || effective.rsplit('.').next() == Some(leaf)
}

fn read_declared_metrics_for_save(
    repo_root: &Path,
    yaml_paths: &[PathBuf],
    provenance: &BTreeMap<StageName, PathBuf>,
    selected_stages: Option<&BTreeSet<StageName>>,
) -> Result<Vec<PathBuf>> {
    let selected_files: BTreeSet<PathBuf> = match selected_stages {
        Some(stages) => stages
            .iter()
            .filter_map(|stage| provenance.get(stage).cloned())
            .collect(),
        None => yaml_paths.iter().cloned().collect(),
    };

    let mut metrics = BTreeSet::new();
    for yaml_path in yaml_paths {
        if !selected_files.contains(yaml_path) {
            continue;
        }
        let text = fs::read_to_string(yaml_path).map_err(CrabError::Io)?;
        let workflow = yaml_mod::parse_at(yaml_path, &text)?;
        let rel_dir = yaml_path
            .parent()
            .and_then(|parent| parent.strip_prefix(repo_root).ok())
            .unwrap_or_else(|| Path::new(""));
        for metric in workflow.metrics {
            metrics.insert(if rel_dir.as_os_str().is_empty() {
                metric
            } else {
                rel_dir.join(metric)
            });
        }
    }
    Ok(metrics.into_iter().collect())
}

/// Read each declared metrics file from the tmpdir and return a
/// map suitable for [`ExperimentMetadata::metrics`].
///
/// Metrics are stored as a flat path → raw JSON value map. We use
/// the same flattening parser the rest of the workflow layer uses
/// so numbers / booleans / strings all round-trip through the
/// lockfile / exp show / exp diff path consistently. Missing files
/// silently contribute nothing — the metadata is "what we
/// observed", not "what the yaml declared".
fn collect_metric_values(
    tmpdir: &Path,
    declared: &[PathBuf],
) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    for rel in declared {
        let path = tmpdir.join(rel);
        if !path.is_file() {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "exp run: metric read failed");
                continue;
            }
        };
        let scalars = match params::parse(&bytes, &path) {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "exp run: metric parse failed");
                continue;
            }
        };
        // Key the outer map by the declared relative path so the
        // consumer can tell which file each scalar came from even
        // after the scalars are merged. Inner scalars serialize
        // as their natural JSON type via serde.
        let value = serde_json::to_value(&scalars).unwrap_or(serde_json::Value::Null);
        out.insert(rel.display().to_string(), value);
    }
    out
}

/// Host fingerprint in the same `"<os>-<arch>-crab-<version>"`
/// shape [`crate::workflow::cache::StageCacheEntry`] uses, so the
/// experiment metadata and the stage cache entries agree on host
/// identity.
fn exp_host_fingerprint() -> String {
    format!(
        "{}-{}-crab-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
    )
}

fn resolve_current_head(repo_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git rev-parse: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "git rev-parse HEAD failed in {}: {}",
            repo_root.display(),
            stderr.trim()
        )));
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(CrabError::Internal(format!(
            "git rev-parse returned unexpected HEAD SHA '{sha}'"
        )));
    }
    Ok(sha)
}

fn read_current_lockfile_stage_hashes(
    repo_root: &Path,
    selected_stages: Option<&BTreeSet<StageName>>,
) -> BTreeMap<String, String> {
    match Lockfile::load(&repo_root.join("crab.lock")) {
        Ok(lock) => lock
            .stages
            .iter()
            .filter(|(name, _)| match selected_stages {
                Some(selected) => selected.contains(*name),
                None => true,
            })
            .map(|(name, stage)| (name.as_str().to_owned(), stage.stage_hash.as_hex()))
            .collect(),
        Err(e) => {
            warn!(
                error = %e,
                "exp save: failed to read current lockfile; metadata.stages will be empty",
            );
            BTreeMap::new()
        }
    }
}

/// Path to the local metadata blob for `id`.
fn meta_file_path(repo_root: &Path, id: &ExperimentId) -> PathBuf {
    repo_root
        .join(EXP_META_PARENT_REL)
        .join(format!("{id}.meta.json"))
}

fn workspace_dir_path(repo_root: &Path, id: &ExperimentId) -> PathBuf {
    repo_root
        .join(EXP_META_PARENT_REL)
        .join(format!("{id}{EXP_WORKSPACE_SUFFIX}"))
}

fn workspace_manifest_path(repo_root: &Path, id: &ExperimentId) -> PathBuf {
    repo_root
        .join(EXP_META_PARENT_REL)
        .join(format!("{id}{EXP_WORKSPACE_MANIFEST_SUFFIX}"))
}

fn capture_workspace_snapshot(
    repo_root: &Path,
    tmpdir: &Path,
    base_commit: &str,
    exp_id: &ExperimentId,
) -> Result<()> {
    let snapshot_dir = workspace_dir_path(repo_root, exp_id);
    if snapshot_dir.exists() {
        fs::remove_dir_all(&snapshot_dir).map_err(CrabError::Io)?;
    }
    fs::create_dir_all(&snapshot_dir).map_err(CrabError::Io)?;

    let manifest_path = workspace_manifest_path(repo_root, exp_id);
    let result = (|| {
        copy_workspace_tree(tmpdir, tmpdir, &snapshot_dir)?;

        let deleted = collect_deleted_base_paths(repo_root, tmpdir, base_commit)?;
        let manifest = ExpWorkspaceManifest { deleted };
        let bytes = serde_json::to_vec(&manifest).map_err(|e| {
            CrabError::Internal(format!(
                "experiment workspace manifest serialization failed for {exp_id}: {e}"
            ))
        })?;
        fs::write(&manifest_path, bytes).map_err(CrabError::Io)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&snapshot_dir);
        let _ = fs::remove_file(&manifest_path);
    }
    result
}

fn copy_paths_into_experiment(repo_root: &Path, tmpdir: &Path, paths: &[PathBuf]) -> Result<()> {
    for raw in paths {
        let rel = normalize_copy_path(raw)?;
        let src = repo_root.join(&rel);
        copy_workspace_path(repo_root, &src, tmpdir, &rel)?;
        info!(
            path = %rel.display(),
            "exp run: copied path into experiment worktree"
        );
    }
    Ok(())
}

fn normalize_copy_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(CrabError::Configuration {
            key: "--copy-paths".to_owned(),
            origin: format!("path must be repo-relative: {}", path.display()),
        });
    }

    let mut rel = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => rel.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CrabError::Configuration {
                    key: "--copy-paths".to_owned(),
                    origin: format!("path must stay inside the repo: {}", path.display()),
                });
            }
        }
    }
    if rel.as_os_str().is_empty() {
        return Err(CrabError::Configuration {
            key: "--copy-paths".to_owned(),
            origin: format!("path must name a file or directory: {}", path.display()),
        });
    }
    ensure_safe_workspace_relpath(&rel)?;
    Ok(rel)
}

fn copy_workspace_path(repo_root: &Path, src: &Path, tmpdir: &Path, rel: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(src).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CrabError::Configuration {
                key: "--copy-paths".to_owned(),
                origin: format!("path does not exist: {}", rel.display()),
            }
        } else {
            CrabError::Io(e)
        }
    })?;
    let target = tmpdir.join(rel);
    remove_existing_path(&target)?;

    let file_type = metadata.file_type();
    if file_type.is_dir() {
        fs::create_dir_all(&target).map_err(CrabError::Io)?;
        copy_workspace_tree(repo_root, src, tmpdir)?;
    } else if file_type.is_symlink() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }
        let link_target = fs::read_link(src).map_err(CrabError::Io)?;
        create_symlink(&link_target, &target)?;
    } else if file_type.is_file() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }
        fs::copy(src, &target).map_err(CrabError::Io)?;
        preserve_mode(src, &target)?;
    }

    Ok(())
}

fn copy_workspace_tree(root: &Path, src: &Path, dst_root: &Path) -> Result<()> {
    for entry in fs::read_dir(src).map_err(CrabError::Io)? {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let rel = path.strip_prefix(root).map_err(|e| {
            CrabError::Internal(format!(
                "snapshot path {} is not under {}: {e}",
                path.display(),
                root.display()
            ))
        })?;
        if should_skip_workspace_snapshot_path(rel) {
            continue;
        }
        ensure_safe_workspace_relpath(rel)?;

        let target = dst_root.join(rel);
        let metadata = fs::symlink_metadata(&path).map_err(CrabError::Io)?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            fs::create_dir_all(&target).map_err(CrabError::Io)?;
            copy_workspace_tree(root, &path, dst_root)?;
        } else if file_type.is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(CrabError::Io)?;
            }
            let link_target = fs::read_link(&path).map_err(CrabError::Io)?;
            create_symlink(&link_target, &target)?;
        } else if file_type.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(CrabError::Io)?;
            }
            fs::copy(&path, &target).map_err(CrabError::Io)?;
            preserve_mode(&path, &target)?;
        }
    }
    Ok(())
}

fn read_workspace_manifest_for_command(
    repo_root: &Path,
    exp_id: &ExperimentId,
    command: &str,
) -> Result<ExpWorkspaceManifest> {
    let path = workspace_manifest_path(repo_root, exp_id);
    let bytes = fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CrabError::Configuration {
                key: command.to_owned(),
                origin: format!(
                    "experiment {exp_id} has no workspace manifest; rerun it with a Crab version that records apply snapshots"
                ),
            }
        } else {
            CrabError::Io(e)
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        CrabError::Internal(format!(
            "experiment workspace manifest malformed JSON for {exp_id}: {e}"
        ))
    })
}

async fn push_workspace_snapshot(
    store: &Store,
    prefix: &str,
    repo_root: &Path,
    exp_id: &ExperimentId,
) -> Result<()> {
    let snapshot_dir = workspace_dir_path(repo_root, exp_id);
    let local_manifest = read_workspace_manifest_for_command(repo_root, exp_id, "exp push")?;
    let mut local_entries = Vec::new();
    collect_workspace_snapshot_entries(&snapshot_dir, &snapshot_dir, &mut local_entries)?;

    let mut entries = Vec::with_capacity(local_entries.len());
    for local in local_entries {
        match local.kind {
            ExpRemoteWorkspaceEntryKind::File => {
                let bytes = fs::read(&local.fs_path).map_err(CrabError::Io)?;
                let hash = blake3::hash(&bytes).to_hex().to_string();
                store
                    .put(
                        &remote_exp_workspace_blob_path(prefix, exp_id, &hash),
                        Bytes::from(bytes),
                    )
                    .await?;
                entries.push(ExpRemoteWorkspaceEntry {
                    path: local.path,
                    kind: local.kind,
                    mode: local.mode,
                    size: local.size,
                    hash: Some(hash),
                    link_target: None,
                });
            }
            ExpRemoteWorkspaceEntryKind::Dir => entries.push(ExpRemoteWorkspaceEntry {
                path: local.path,
                kind: local.kind,
                mode: local.mode,
                size: 0,
                hash: None,
                link_target: None,
            }),
            ExpRemoteWorkspaceEntryKind::Symlink => entries.push(ExpRemoteWorkspaceEntry {
                path: local.path,
                kind: local.kind,
                mode: local.mode,
                size: 0,
                hash: None,
                link_target: local.link_target,
            }),
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));

    let mut deleted = local_manifest
        .deleted
        .iter()
        .map(|path| path_to_remote_rel(path.as_path()))
        .collect::<Result<Vec<_>>>()?;
    deleted.sort();

    let manifest = ExpRemoteWorkspaceManifest {
        schema_version: EXP_REMOTE_WORKSPACE_SCHEMA_VERSION,
        deleted,
        entries,
    };
    let bytes = serde_json::to_vec(&manifest).map_err(|e| {
        CrabError::Internal(format!(
            "remote experiment workspace manifest serialization failed for {exp_id}: {e}"
        ))
    })?;
    store
        .put(
            &remote_exp_workspace_manifest_path(prefix, exp_id),
            Bytes::from(bytes),
        )
        .await?;
    Ok(())
}

async fn push_checkpoint_state(
    store: &Store,
    prefix: &str,
    repo_root: &Path,
    exp_id: &ExperimentId,
) -> Result<()> {
    let state_root = checkpoint_state_dir(repo_root, exp_id);
    if !state_root.is_dir() {
        return Ok(());
    }
    validate_checkpoint_state(&state_root)?;
    let mut files = Vec::new();
    collect_checkpoint_files(&state_root, &state_root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    for (relative, path) in files {
        let bytes = fs::read(&path).map_err(CrabError::Io)?;
        let remote_path = remote_exp_checkpoint_path(prefix, exp_id, &relative);
        if remote_object_exists(store, &remote_path).await? {
            let (existing, _) = store.get_with_etag(&remote_path).await?;
            if existing != bytes {
                return Err(CrabError::CorruptObject {
                    path: remote_path.as_ref().to_owned(),
                    reason: "immutable checkpoint object differs from local bytes".to_owned(),
                });
            }
            continue;
        }
        store.put(&remote_path, Bytes::from(bytes)).await?;
    }
    Ok(())
}

async fn pull_checkpoint_state(
    store: &Store,
    prefix: &str,
    repo_root: &Path,
    exp_id: &ExperimentId,
) -> Result<()> {
    let remote_prefix = remote_exp_checkpoint_prefix(prefix, exp_id);
    let remote_entries = store.list_prefix(&remote_prefix).await?;
    let state_parent = repo_root.join(".crab/workflow/checkpoints");
    fs::create_dir_all(&state_parent).map_err(CrabError::Io)?;
    let state_root = checkpoint_state_dir(repo_root, exp_id);
    if remote_entries.is_empty() {
        match fs::remove_dir_all(&state_root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CrabError::Io(error)),
        }
        return Ok(());
    }

    let temporary = state_parent.join(format!("{exp_id}.pull-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(&temporary).map_err(CrabError::Io)?;
    let result = async {
        for meta in remote_entries {
            let key = meta.location.as_ref();
            let relative = key.strip_prefix(remote_prefix.as_ref()).ok_or_else(|| {
                CrabError::CorruptObject {
                    path: key.to_owned(),
                    reason: "checkpoint object is outside its namespace".to_owned(),
                }
            })?;
            let relative = relative.trim_start_matches('/');
            let relative = remote_rel_to_path(relative)?;
            let target = temporary.join(&relative);
            let (bytes, _) = store.get_with_etag(&meta.location).await?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(CrabError::Io)?;
            }
            fs::write(&target, bytes).map_err(CrabError::Io)?;
        }
        validate_checkpoint_state(&temporary)?;
        Ok::<(), CrabError>(())
    }
    .await;
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }

    let backup = state_parent.join(format!("{exp_id}.backup-{}", uuid::Uuid::now_v7()));
    if state_root.exists() {
        fs::rename(&state_root, &backup).map_err(CrabError::Io)?;
    }
    if let Err(error) = fs::rename(&temporary, &state_root) {
        if backup.exists() {
            let _ = fs::rename(&backup, &state_root);
        }
        let _ = fs::remove_dir_all(&temporary);
        return Err(CrabError::Io(error));
    }
    let _ = fs::remove_dir_all(backup);
    Ok(())
}

fn collect_checkpoint_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(CrabError::Io)? {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|error| {
            CrabError::Internal(format!("checkpoint path is outside state: {error}"))
        })?;
        ensure_safe_workspace_relpath(relative)?;
        let is_transient = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(".tmp-") || name.contains(".backup-"))
            || path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("lock"));
        if is_transient {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(CrabError::Io)?;
        if metadata.file_type().is_symlink() {
            return Err(CrabError::Configuration {
                key: "workflow checkpoint state".to_owned(),
                origin: format!("symlink is not allowed: {}", path.display()),
            });
        }
        if metadata.is_dir() {
            collect_checkpoint_files(root, &path, files)?;
        } else if metadata.is_file() {
            files.push((relative.to_string_lossy().replace('\\', "/"), path));
        }
    }
    Ok(())
}

fn validate_checkpoint_state(state_root: &Path) -> Result<()> {
    let paths = checkpoint_lineage_paths(state_root)?;
    for path in paths {
        let lineage = load_checkpoint_lineage(&path)?;
        for record in lineage.records {
            for hash in record.outputs.values() {
                validate_checkpoint_object(state_root, &path, hash, "output")?;
            }
            for hash in record.metrics.values() {
                validate_checkpoint_object(state_root, &path, hash, "metric")?;
            }
        }
    }
    Ok(())
}

fn validate_checkpoint_object(
    state_root: &Path,
    lineage_path: &Path,
    hash: &str,
    kind: &str,
) -> Result<()> {
    let digest = hash
        .strip_prefix("b3:")
        .ok_or_else(|| CrabError::CorruptObject {
            path: lineage_path.display().to_string(),
            reason: format!("checkpoint {kind} hash is not a b3 digest"),
        })?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CrabError::CorruptObject {
            path: lineage_path.display().to_string(),
            reason: format!("checkpoint {kind} hash is malformed"),
        });
    }
    let object = state_root.join("objects").join(digest).join("payload");
    if !object.exists() {
        return Err(CrabError::CorruptObject {
            path: object.display().to_string(),
            reason: format!("checkpoint {kind} object is missing"),
        });
    }
    let actual = checkpoint_payload_hash(&object)?;
    if actual != digest {
        return Err(CrabError::CorruptObject {
            path: object.display().to_string(),
            reason: format!("expected {digest}, got {actual}"),
        });
    }
    Ok(())
}

async fn restore_workspace_snapshot(
    store: &Store,
    prefix: &str,
    repo_root: &Path,
    exp_id: &ExperimentId,
) -> Result<()> {
    let remote_manifest = read_remote_workspace_manifest(store, prefix, exp_id).await?;
    let parent = repo_root.join(EXP_META_PARENT_REL);
    fs::create_dir_all(&parent).map_err(CrabError::Io)?;

    let suffix = uuid::Uuid::now_v7();
    let tmp_dir = parent.join(format!("{exp_id}{EXP_WORKSPACE_SUFFIX}.pull-{suffix}"));
    let tmp_manifest_path = parent.join(format!(
        "{exp_id}{EXP_WORKSPACE_MANIFEST_SUFFIX}.pull-{suffix}"
    ));
    fs::create_dir_all(&tmp_dir).map_err(CrabError::Io)?;

    let result = restore_workspace_snapshot_inner(
        store,
        prefix,
        exp_id,
        &remote_manifest,
        &tmp_dir,
        &tmp_manifest_path,
    )
    .await;
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&tmp_dir);
        let _ = fs::remove_file(&tmp_manifest_path);
        return Err(e);
    }

    let snapshot_dir = workspace_dir_path(repo_root, exp_id);
    let manifest_path = workspace_manifest_path(repo_root, exp_id);
    if snapshot_dir.exists() {
        fs::remove_dir_all(&snapshot_dir).map_err(CrabError::Io)?;
    }
    match fs::remove_file(&manifest_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CrabError::Io(e)),
    }
    fs::rename(&tmp_dir, &snapshot_dir).map_err(CrabError::Io)?;
    fs::rename(&tmp_manifest_path, &manifest_path).map_err(CrabError::Io)?;
    Ok(())
}

async fn restore_workspace_snapshot_inner(
    store: &Store,
    prefix: &str,
    exp_id: &ExperimentId,
    remote_manifest: &ExpRemoteWorkspaceManifest,
    tmp_dir: &Path,
    tmp_manifest_path: &Path,
) -> Result<()> {
    for entry in &remote_manifest.entries {
        restore_remote_workspace_entry(store, prefix, exp_id, entry, tmp_dir).await?;
    }

    let deleted = remote_manifest
        .deleted
        .iter()
        .map(|path| remote_rel_to_path(path))
        .collect::<Result<Vec<_>>>()?;
    let local_manifest = ExpWorkspaceManifest { deleted };
    let bytes = serde_json::to_vec(&local_manifest).map_err(|e| {
        CrabError::Internal(format!(
            "local experiment workspace manifest serialization failed for {exp_id}: {e}"
        ))
    })?;
    fs::write(tmp_manifest_path, bytes).map_err(CrabError::Io)?;
    Ok(())
}

async fn restore_remote_workspace_entry(
    store: &Store,
    prefix: &str,
    exp_id: &ExperimentId,
    entry: &ExpRemoteWorkspaceEntry,
    tmp_dir: &Path,
) -> Result<()> {
    let rel = remote_rel_to_path(&entry.path)?;
    let target = tmp_dir.join(&rel);
    match entry.kind {
        ExpRemoteWorkspaceEntryKind::Dir => {
            fs::create_dir_all(&target).map_err(CrabError::Io)?;
            set_mode(&target, entry.mode)?;
        }
        ExpRemoteWorkspaceEntryKind::File => {
            let hash = entry
                .hash
                .as_deref()
                .ok_or_else(|| CrabError::CorruptObject {
                    path: entry.path.clone(),
                    reason: "file entry is missing content hash".to_owned(),
                })?;
            let blob_path = remote_exp_workspace_blob_path(prefix, exp_id, hash);
            let (bytes, _) = store.get_with_etag(&blob_path).await?;
            if bytes.len() as u64 != entry.size {
                return Err(CrabError::CorruptObject {
                    path: blob_path.as_ref().to_owned(),
                    reason: format!("expected {} bytes, got {}", entry.size, bytes.len()),
                });
            }
            let actual = blake3::hash(&bytes).to_hex().to_string();
            if actual != hash {
                return Err(CrabError::CorruptObject {
                    path: blob_path.as_ref().to_owned(),
                    reason: format!("expected blake3 {hash}, got {actual}"),
                });
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(CrabError::Io)?;
            }
            crate::workflow::materialize::write_atomic(
                &target,
                &bytes,
                uuid::Uuid::now_v7(),
                entry.mode,
            )?;
        }
        ExpRemoteWorkspaceEntryKind::Symlink => {
            let link_target =
                entry
                    .link_target
                    .as_deref()
                    .ok_or_else(|| CrabError::CorruptObject {
                        path: entry.path.clone(),
                        reason: "symlink entry is missing link target".to_owned(),
                    })?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(CrabError::Io)?;
            }
            create_symlink(Path::new(link_target), &target)?;
        }
    }
    Ok(())
}

async fn read_remote_workspace_manifest(
    store: &Store,
    prefix: &str,
    exp_id: &ExperimentId,
) -> Result<ExpRemoteWorkspaceManifest> {
    let path = remote_exp_workspace_manifest_path(prefix, exp_id);
    let (bytes, _) = store.get_with_etag(&path).await?;
    let manifest: ExpRemoteWorkspaceManifest =
        serde_json::from_slice(&bytes).map_err(|e| CrabError::CorruptObject {
            path: path.as_ref().to_owned(),
            reason: format!("workspace manifest is not valid JSON: {e}"),
        })?;
    if manifest.schema_version != EXP_REMOTE_WORKSPACE_SCHEMA_VERSION {
        return Err(CrabError::IncompatibleFormat {
            required: format!("experiment workspace schema {EXP_REMOTE_WORKSPACE_SCHEMA_VERSION}"),
            found: format!("experiment workspace schema {}", manifest.schema_version),
        });
    }
    Ok(manifest)
}

struct ExpLocalWorkspaceEntry {
    fs_path: PathBuf,
    path: String,
    kind: ExpRemoteWorkspaceEntryKind,
    mode: u32,
    size: u64,
    link_target: Option<String>,
}

fn collect_workspace_snapshot_entries(
    root: &Path,
    src: &Path,
    out: &mut Vec<ExpLocalWorkspaceEntry>,
) -> Result<()> {
    for entry in fs::read_dir(src).map_err(CrabError::Io)? {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let rel = path.strip_prefix(root).map_err(|e| {
            CrabError::Internal(format!(
                "snapshot path {} is not under {}: {e}",
                path.display(),
                root.display()
            ))
        })?;
        ensure_safe_workspace_relpath(rel)?;

        let metadata = fs::symlink_metadata(&path).map_err(CrabError::Io)?;
        let file_type = metadata.file_type();
        let remote_rel = path_to_remote_rel(rel)?;
        if file_type.is_dir() {
            out.push(ExpLocalWorkspaceEntry {
                fs_path: path.clone(),
                path: remote_rel,
                kind: ExpRemoteWorkspaceEntryKind::Dir,
                mode: file_mode(&path)?,
                size: 0,
                link_target: None,
            });
            collect_workspace_snapshot_entries(root, &path, out)?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&path).map_err(CrabError::Io)?;
            out.push(ExpLocalWorkspaceEntry {
                fs_path: path,
                path: remote_rel,
                kind: ExpRemoteWorkspaceEntryKind::Symlink,
                mode: 0o777,
                size: 0,
                link_target: Some(path_to_utf8_string(&link_target)?),
            });
        } else if file_type.is_file() {
            out.push(ExpLocalWorkspaceEntry {
                fs_path: path.clone(),
                path: remote_rel,
                kind: ExpRemoteWorkspaceEntryKind::File,
                mode: file_mode(&path)?,
                size: metadata.len(),
                link_target: None,
            });
        }
    }
    Ok(())
}

fn collect_deleted_base_paths(
    repo_root: &Path,
    tmpdir: &Path,
    base_commit: &str,
) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "-z", base_commit])
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git ls-tree: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "git ls-tree failed for commit {base_commit}: {}",
            stderr.trim()
        )));
    }

    let mut deleted = Vec::new();
    for raw in output.stdout.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        let rel = PathBuf::from(String::from_utf8_lossy(raw).into_owned());
        if should_skip_workspace_snapshot_path(&rel) {
            continue;
        }
        ensure_safe_workspace_relpath(&rel)?;
        if !tmpdir.join(&rel).exists() {
            deleted.push(rel);
        }
    }
    deleted.sort();
    Ok(deleted)
}

fn apply_workspace_tree(
    root: &Path,
    src: &Path,
    repo_root: &Path,
    applied: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(src).map_err(CrabError::Io)? {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let rel = path.strip_prefix(root).map_err(|e| {
            CrabError::Internal(format!(
                "snapshot path {} is not under {}: {e}",
                path.display(),
                root.display()
            ))
        })?;
        ensure_safe_workspace_relpath(rel)?;

        let target = repo_root.join(rel);
        let metadata = fs::symlink_metadata(&path).map_err(CrabError::Io)?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            if target.exists() && !target.is_dir() {
                remove_existing_path(&target)?;
            }
            fs::create_dir_all(&target).map_err(CrabError::Io)?;
            preserve_mode(&path, &target)?;
            apply_workspace_tree(root, &path, repo_root, applied)?;
        } else if file_type.is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(CrabError::Io)?;
            }
            remove_existing_path(&target)?;
            let link_target = fs::read_link(&path).map_err(CrabError::Io)?;
            create_symlink(&link_target, &target)?;
            applied.push(rel.to_path_buf());
        } else if file_type.is_file() {
            if target.is_dir() {
                remove_existing_path(&target)?;
            }
            let bytes = fs::read(&path).map_err(CrabError::Io)?;
            let mode = file_mode(&path)?;
            crate::workflow::materialize::write_atomic(
                &target,
                &bytes,
                uuid::Uuid::now_v7(),
                mode,
            )?;
            applied.push(rel.to_path_buf());
        }
    }
    Ok(())
}

fn should_skip_workspace_snapshot_path(rel: &Path) -> bool {
    rel.components().any(|component| match component {
        Component::Normal(name) => name == ".git" || name == ".crab",
        _ => false,
    })
}

fn ensure_safe_workspace_relpath(rel: &Path) -> Result<()> {
    if rel.as_os_str().is_empty() || rel.is_absolute() {
        return Err(CrabError::Configuration {
            key: "experiment workspace path".to_owned(),
            origin: format!("path must be relative: {}", rel.display()),
        });
    }
    for component in rel.components() {
        match component {
            Component::Normal(name) if name != ".git" && name != ".crab" => {}
            Component::CurDir => {}
            _ => {
                return Err(CrabError::Configuration {
                    key: "experiment workspace path".to_owned(),
                    origin: format!("unsafe path in experiment snapshot: {}", rel.display()),
                });
            }
        }
    }
    Ok(())
}

fn path_to_remote_rel(rel: &Path) -> Result<String> {
    ensure_safe_workspace_relpath(rel)?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(name) => {
                let value = name.to_str().ok_or_else(|| CrabError::Configuration {
                    key: "experiment workspace path".to_owned(),
                    origin: format!("path is not valid UTF-8: {}", rel.display()),
                })?;
                parts.push(value.to_owned());
            }
            Component::CurDir => {}
            _ => {
                return Err(CrabError::Configuration {
                    key: "experiment workspace path".to_owned(),
                    origin: format!("unsafe path in experiment snapshot: {}", rel.display()),
                });
            }
        }
    }
    if parts.is_empty() {
        return Err(CrabError::Configuration {
            key: "experiment workspace path".to_owned(),
            origin: "path must not be empty".to_owned(),
        });
    }
    Ok(parts.join("/"))
}

fn remote_rel_to_path(path: &str) -> Result<PathBuf> {
    let rel = PathBuf::from(path);
    ensure_safe_workspace_relpath(&rel)?;
    Ok(rel)
}

fn path_to_utf8_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(std::borrow::ToOwned::to_owned)
        .ok_or_else(|| CrabError::Configuration {
            key: "experiment workspace path".to_owned(),
            origin: format!("path is not valid UTF-8: {}", path.display()),
        })
}

fn remove_existing_path(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(CrabError::Io(e)),
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path).map_err(CrabError::Io)?;
    } else {
        fs::remove_file(path).map_err(CrabError::Io)?;
    }
    Ok(true)
}

fn remove_experiment_files(repo_root: &Path, exp_id: &ExperimentId) -> Result<()> {
    for path in [
        meta_file_path(repo_root, exp_id),
        workspace_manifest_path(repo_root, exp_id),
    ] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CrabError::Io(e)),
        }
    }
    let workspace_dir = workspace_dir_path(repo_root, exp_id);
    match fs::remove_dir_all(&workspace_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CrabError::Io(e)),
    }
    let checkpoint_dir = checkpoint_state_dir(repo_root, exp_id);
    match fs::remove_dir_all(&checkpoint_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CrabError::Io(e)),
    }
    Ok(())
}

fn file_mode(path: &Path) -> Result<u32> {
    let metadata = fs::symlink_metadata(path).map_err(CrabError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(0o644)
    }
}

fn preserve_mode(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::symlink_metadata(src)
            .map_err(CrabError::Io)?
            .permissions()
            .mode()
            & 0o777;
        fs::set_permissions(dst, fs::Permissions::from_mode(mode)).map_err(CrabError::Io)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (src, dst);
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
            .map_err(CrabError::Io)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(CrabError::Io)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, link).map_err(CrabError::Io)
}

/// Persist an experiment's metadata locally. The parent directory
/// is created on first use. The write is straight `fs::write` —
/// the metadata is small and the commit point of an experiment is
/// not atomic-persist-of-the-json-blob; losing a metadata blob
/// during a crash is recoverable by re-running the experiment.
fn write_local_metadata(repo_root: &Path, meta: &ExperimentMetadata) -> Result<()> {
    let path = meta_file_path(repo_root, &meta.exp_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    let bytes = meta.canonical_json()?;
    fs::write(&path, bytes).map_err(CrabError::Io)?;
    Ok(())
}

fn experiment_stage_refs_json(meta: &ExperimentMetadata) -> Result<Vec<u8>> {
    let refs: Vec<String> = meta
        .stages
        .values()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    serde_json::to_vec(&refs).map_err(|e| {
        CrabError::Internal(format!(
            "experiment stage refs serialization failed for {}: {e}",
            meta.exp_id
        ))
    })
}

fn remote_key(prefix: &str, rel: &str) -> String {
    let prefix = prefix.trim_matches('/');
    let rel = rel.trim_start_matches('/');
    if prefix.is_empty() {
        rel.to_owned()
    } else {
        format!("{prefix}/{rel}")
    }
}

fn remote_path(prefix: &str, rel: &str) -> ObjectPath {
    ObjectPath::from(remote_key(prefix, rel))
}

fn remote_exp_object_prefix(prefix: &str, id: &ExperimentId) -> ObjectPath {
    remote_path(prefix, &format!("workflow/exp/{id}/"))
}

fn remote_exp_meta_object_path(prefix: &str, id: &ExperimentId) -> ObjectPath {
    remote_path(prefix, &exp_meta_object_path(id))
}

fn remote_exp_stage_refs_object_path(prefix: &str, id: &ExperimentId) -> ObjectPath {
    remote_path(prefix, &exp_stage_refs_object_path(id))
}

fn remote_exp_meta_ref_path(prefix: &str, id: &ExperimentId) -> ObjectPath {
    remote_path(prefix, &exp_meta_ref(id))
}

fn remote_exp_workspace_manifest_path(prefix: &str, id: &ExperimentId) -> ObjectPath {
    remote_path(
        prefix,
        &format!("workflow/exp/{id}/workspace/manifest.json"),
    )
}

fn remote_exp_workspace_blob_path(prefix: &str, id: &ExperimentId, hash: &str) -> ObjectPath {
    remote_path(prefix, &format!("workflow/exp/{id}/workspace/blobs/{hash}"))
}

fn remote_exp_checkpoint_prefix(prefix: &str, id: &ExperimentId) -> ObjectPath {
    remote_path(prefix, &format!("workflow/exp/{id}/checkpoints/"))
}

fn remote_exp_checkpoint_path(prefix: &str, id: &ExperimentId, relative: &str) -> ObjectPath {
    remote_path(prefix, &format!("workflow/exp/{id}/checkpoints/{relative}"))
}

/// Read an experiment's metadata from the local cache.
///
/// Returns [`CrabError::ExperimentNotFound`] when the blob is
/// absent so `exp show` / `exp diff` / `exp promote` surface a
/// uniform error to the user.
fn read_local_metadata(repo_root: &Path, id: &ExperimentId) -> Result<ExperimentMetadata> {
    let path = meta_file_path(repo_root, id);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CrabError::ExperimentNotFound { id: id.to_string() });
        }
        Err(e) => return Err(CrabError::Io(e)),
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        CrabError::Internal(format!("experiment metadata malformed JSON for {id}: {e}"))
    })
}

fn parse_experiment_id(raw: &str) -> Result<ExperimentId> {
    raw.parse::<ExperimentId>().map_err(CrabError::from)
}

fn resolve_experiment_id(repo_root: &Path, raw: &str) -> Result<ExperimentId> {
    if let Ok(id) = raw.parse::<ExperimentId>() {
        return Ok(id);
    }

    let summaries = collect_summaries(repo_root)?;
    let matches: Vec<String> = summaries
        .iter()
        .filter(|summary| summary.id.starts_with(raw))
        .map(|summary| summary.id.clone())
        .collect();

    match matches.as_slice() {
        [id] => return parse_experiment_id(id),
        [] => {}
        many => {
            return Err(CrabError::Configuration {
                key: "experiment id".to_owned(),
                origin: format!(
                    "prefix '{raw}' matches multiple experiments: {}",
                    many.join(", ")
                ),
            });
        }
    }

    let name_matches: Vec<String> = summaries
        .iter()
        .filter(|summary| summary.name.as_deref() == Some(raw))
        .map(|summary| summary.id.clone())
        .collect();

    match name_matches.as_slice() {
        [id] => parse_experiment_id(id),
        [] => Err(CrabError::ExperimentNotFound { id: raw.to_owned() }),
        many => Err(CrabError::Configuration {
            key: "experiment name".to_owned(),
            origin: format!(
                "name '{raw}' matches multiple experiments: {}",
                many.join(", ")
            ),
        }),
    }
}

fn resolve_experiment_ids(repo_root: &Path, raw_ids: &[String]) -> Result<Vec<ExperimentId>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(raw_ids.len());
    for raw in raw_ids {
        let id = resolve_experiment_id(repo_root, raw)?;
        if seen.insert(id.to_string()) {
            out.push(id);
        }
    }
    Ok(out)
}

/// Enumerate every local metadata blob, parse it, and return
/// one-line summaries sorted newest-first.
///
/// UUIDv7's canonical string form sorts chronologically, so
/// sorting the `exp_id` strings descending gives us newest-first
/// directly; no separate timestamp comparison is needed.
fn collect_summaries(repo_root: &Path) -> Result<Vec<ExpSummary>> {
    let parent = repo_root.join(EXP_META_PARENT_REL);
    let entries = match fs::read_dir(&parent) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(CrabError::Io(e)),
    };

    let mut out: Vec<ExpSummary> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Only `.meta.json` files are considered — sibling tmpdirs
        // (directories named after experiment ids) are handled by
        // `sweep_orphan_experiment_tmpdirs`, not here.
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".meta.json") {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "exp ls: metadata read failed");
                continue;
            }
        };
        let meta: ExperimentMetadata = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "exp ls: metadata parse failed");
                continue;
            }
        };
        let mut metrics_keys: Vec<String> = meta.metrics.keys().cloned().collect();
        metrics_keys.sort();
        out.push(ExpSummary {
            id: meta.exp_id.to_string(),
            name: meta.name.clone(),
            message: meta.message.clone(),
            started_at: meta.started_at.clone(),
            base_commit: meta.base_commit.clone(),
            status: meta.status.clone(),
            stages: meta.stages.len(),
            params: meta.param_overrides.clone(),
            metrics: flatten_experiment_metrics(&meta.metrics),
            metrics_keys,
        });
    }

    // Newest-first by id. UUIDv7 canonical string form sorts
    // chronologically, so reverse-lex IS reverse-chronological.
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

fn collect_limited_summaries(repo_root: &Path, limit: Option<usize>) -> Result<Vec<ExpSummary>> {
    let summaries = collect_summaries(repo_root)?;
    Ok(match limit {
        Some(n) => summaries.into_iter().take(n).collect(),
        None => summaries,
    })
}

fn collect_show_summaries(
    repo_root: &Path,
    limit: Option<usize>,
    sort_by: Option<&str>,
    sort_order: ExpShowSortOrder,
    hide_failed: bool,
) -> Result<Vec<ExpSummary>> {
    let mut summaries = collect_summaries(repo_root)?;
    if hide_failed {
        summaries.retain(|summary| summary.status != "failed");
    }
    sort_show_summaries(&mut summaries, sort_by.unwrap_or("id"), sort_order)?;
    Ok(match limit {
        Some(n) => summaries.into_iter().take(n).collect(),
        None => summaries,
    })
}

fn sort_show_summaries(
    summaries: &mut [ExpSummary],
    sort_by: &str,
    sort_order: ExpShowSortOrder,
) -> Result<()> {
    if summaries.is_empty() {
        return Ok(());
    }
    if !is_known_show_sort_key(summaries, sort_by) {
        return Err(CrabError::Configuration {
            key: "exp show --sort-by".to_owned(),
            origin: format!("unknown summary, param, or metric column: {sort_by}"),
        });
    }

    summaries.sort_by(|left, right| {
        let left_value = show_sort_value(left, sort_by);
        let right_value = show_sort_value(right, sort_by);
        compare_show_sort_values(&left_value, &right_value, sort_order)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(())
}

fn is_known_show_sort_key(summaries: &[ExpSummary], sort_by: &str) -> bool {
    matches!(
        sort_by,
        "id" | "name" | "message" | "started_at" | "base_commit" | "status" | "stages"
    ) || summaries.iter().any(|summary| {
        summary.params.contains_key(sort_by) || summary.metrics.contains_key(sort_by)
    })
}

#[derive(Debug, Clone, PartialEq)]
enum ExpShowSortValue {
    Missing,
    Number(f64),
    Text(String),
}

fn show_sort_value(summary: &ExpSummary, sort_by: &str) -> ExpShowSortValue {
    match sort_by {
        "id" => ExpShowSortValue::Text(summary.id.clone()),
        "name" => summary
            .name
            .clone()
            .map_or(ExpShowSortValue::Missing, ExpShowSortValue::Text),
        "message" => summary
            .message
            .clone()
            .map_or(ExpShowSortValue::Missing, ExpShowSortValue::Text),
        "started_at" => ExpShowSortValue::Text(summary.started_at.clone()),
        "base_commit" => ExpShowSortValue::Text(summary.base_commit.clone()),
        "status" => ExpShowSortValue::Text(summary.status.clone()),
        "stages" => ExpShowSortValue::Number(summary.stages as f64),
        key => summary
            .params
            .get(key)
            .map(|value| ExpShowSortValue::Text(value.clone()))
            .or_else(|| summary.metrics.get(key).map(metric_sort_value))
            .unwrap_or(ExpShowSortValue::Missing),
    }
}

fn metric_sort_value(value: &serde_json::Value) -> ExpShowSortValue {
    match value {
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_f64() {
                ExpShowSortValue::Number(value)
            } else {
                ExpShowSortValue::Text(number.to_string())
            }
        }
        serde_json::Value::String(value) => ExpShowSortValue::Text(value.clone()),
        _ => ExpShowSortValue::Text(value.to_string()),
    }
}

fn compare_show_sort_values(
    left: &ExpShowSortValue,
    right: &ExpShowSortValue,
    sort_order: ExpShowSortOrder,
) -> Ordering {
    let ordering = match (left, right) {
        (ExpShowSortValue::Missing, ExpShowSortValue::Missing) => Ordering::Equal,
        (ExpShowSortValue::Missing, _) => return Ordering::Greater,
        (_, ExpShowSortValue::Missing) => return Ordering::Less,
        (ExpShowSortValue::Number(left), ExpShowSortValue::Number(right)) => {
            left.partial_cmp(right).unwrap_or(Ordering::Equal)
        }
        (ExpShowSortValue::Number(_), ExpShowSortValue::Text(_)) => Ordering::Less,
        (ExpShowSortValue::Text(_), ExpShowSortValue::Number(_)) => Ordering::Greater,
        (ExpShowSortValue::Text(left), ExpShowSortValue::Text(right)) => left.cmp(right),
    };
    match sort_order {
        ExpShowSortOrder::Asc => ordering,
        ExpShowSortOrder::Desc => ordering.reverse(),
    }
}

fn flatten_experiment_metrics(
    metrics: &BTreeMap<String, serde_json::Value>,
) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    for (path, value) in metrics {
        flatten_metric_value(path, value, &mut out);
    }
    out
}

fn flatten_metric_value(
    key: &str,
    value: &serde_json::Value,
    out: &mut BTreeMap<String, serde_json::Value>,
) {
    match value {
        serde_json::Value::Object(object) if !object.is_empty() => {
            for (child_key, child_value) in object {
                let joined = join_metric_key(key, child_key);
                flatten_metric_value(&joined, child_value, out);
            }
        }
        _ => {
            out.insert(key.to_owned(), value.clone());
        }
    }
}

fn join_metric_key(parent: &str, child: &str) -> String {
    if parent.contains(':') {
        format!("{parent}.{child}")
    } else {
        format!("{parent}:{child}")
    }
}

#[derive(Debug)]
struct ExpShowColumnFilters {
    only_changed: bool,
    drop: Vec<Regex>,
    keep: Vec<Regex>,
}

fn compile_show_column_filters(args: &ShowArgs) -> Result<ExpShowColumnFilters> {
    Ok(ExpShowColumnFilters {
        only_changed: args.only_changed,
        drop: compile_regexes("exp show --drop", &args.drop)?,
        keep: compile_regexes("exp show --keep", &args.keep)?,
    })
}

fn compile_regexes(context: &str, patterns: &[String]) -> Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|e| CrabError::Configuration {
                key: context.to_owned(),
                origin: format!("invalid regex '{pattern}': {e}"),
            })
        })
        .collect()
}

fn apply_show_column_filters(summaries: &mut [ExpSummary], filters: &ExpShowColumnFilters) {
    if !filters.only_changed && filters.drop.is_empty() && filters.keep.is_empty() {
        return;
    }

    let changed_params = changed_param_keys(summaries);
    let changed_metrics = changed_metric_keys(summaries);
    for summary in summaries {
        summary
            .params
            .retain(|key, _| show_column_is_visible("param", key, &changed_params, filters));
        summary
            .metrics
            .retain(|key, _| show_column_is_visible("metric", key, &changed_metrics, filters));
        summary.metrics_keys = summary.metrics.keys().cloned().collect();
    }
}

fn changed_param_keys(summaries: &[ExpSummary]) -> BTreeSet<String> {
    let keys: BTreeSet<String> = summaries
        .iter()
        .flat_map(|summary| summary.params.keys().cloned())
        .collect();
    keys.into_iter()
        .filter(|key| {
            value_varies(
                summaries
                    .iter()
                    .map(|summary| summary.params.get(key).map(ToOwned::to_owned)),
            )
        })
        .collect()
}

fn changed_metric_keys(summaries: &[ExpSummary]) -> BTreeSet<String> {
    let keys: BTreeSet<String> = summaries
        .iter()
        .flat_map(|summary| summary.metrics.keys().cloned())
        .collect();
    keys.into_iter()
        .filter(|key| {
            value_varies(
                summaries
                    .iter()
                    .map(|summary| summary.metrics.get(key).map(serde_json::Value::to_string)),
            )
        })
        .collect()
}

fn value_varies(values: impl IntoIterator<Item = Option<String>>) -> bool {
    let mut seen = BTreeSet::new();
    for value in values {
        seen.insert(value);
        if seen.len() > 1 {
            return true;
        }
    }
    false
}

fn show_column_is_visible(
    kind: &str,
    key: &str,
    changed_keys: &BTreeSet<String>,
    filters: &ExpShowColumnFilters,
) -> bool {
    if regex_matches_column(&filters.keep, kind, key) {
        return true;
    }
    if filters.only_changed && !changed_keys.contains(key) {
        return false;
    }
    if regex_matches_column(&filters.drop, kind, key) {
        return false;
    }
    true
}

fn regex_matches_column(regexes: &[Regex], kind: &str, key: &str) -> bool {
    let qualified = format!("{kind}:{key}");
    regexes
        .iter()
        .any(|regex| regex.is_match(key) || regex.is_match(&qualified))
}

/// Compute the diff between two metadata blobs.
///
/// - `param_overrides` → added/removed/changed by string equality.
/// - `stages` → entries whose hash differs, or stages present in
///   only one side.
/// - `metrics` → entries whose JSON value differs, or metric files
///   present in only one side.
fn build_diff_payload(
    a: &ExperimentMetadata,
    b: &ExperimentMetadata,
    include_unchanged: bool,
) -> ExpDiffPayload {
    let mut params_added = BTreeMap::new();
    let mut params_removed = BTreeMap::new();
    let mut params_changed = BTreeMap::new();
    let mut params_unchanged = BTreeMap::new();
    for (k, va) in &a.param_overrides {
        match b.param_overrides.get(k) {
            Some(vb) if vb == va => {
                if include_unchanged {
                    params_unchanged.insert(k.clone(), va.clone());
                }
            }
            Some(vb) => {
                params_changed.insert(k.clone(), (va.clone(), vb.clone()));
            }
            None => {
                params_removed.insert(k.clone(), va.clone());
            }
        }
    }
    for (k, vb) in &b.param_overrides {
        if !a.param_overrides.contains_key(k) {
            params_added.insert(k.clone(), vb.clone());
        }
    }

    let mut stages_changed: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    let mut stages_unchanged = BTreeMap::new();
    for (k, ha) in &a.stages {
        match b.stages.get(k) {
            Some(hb) if hb == ha => {
                if include_unchanged {
                    stages_unchanged.insert(k.clone(), ha.clone());
                }
            }
            Some(hb) => {
                stages_changed.insert(k.clone(), (Some(ha.clone()), Some(hb.clone())));
            }
            None => {
                stages_changed.insert(k.clone(), (Some(ha.clone()), None));
            }
        }
    }
    for (k, hb) in &b.stages {
        if !a.stages.contains_key(k) {
            stages_changed.insert(k.clone(), (None, Some(hb.clone())));
        }
    }

    let mut metrics_changed: BTreeMap<
        String,
        (Option<serde_json::Value>, Option<serde_json::Value>),
    > = BTreeMap::new();
    let mut metrics_unchanged = BTreeMap::new();
    for (k, va) in &a.metrics {
        match b.metrics.get(k) {
            Some(vb) if vb == va => {
                if include_unchanged {
                    metrics_unchanged.insert(k.clone(), va.clone());
                }
            }
            Some(vb) => {
                metrics_changed.insert(k.clone(), (Some(va.clone()), Some(vb.clone())));
            }
            None => {
                metrics_changed.insert(k.clone(), (Some(va.clone()), None));
            }
        }
    }
    for (k, vb) in &b.metrics {
        if !a.metrics.contains_key(k) {
            metrics_changed.insert(k.clone(), (None, Some(vb.clone())));
        }
    }

    ExpDiffPayload {
        id_a: a.exp_id.to_string(),
        id_b: b.exp_id.to_string(),
        params_added,
        params_removed,
        params_changed,
        params_unchanged,
        stages_changed,
        stages_unchanged,
        metrics_changed,
        metrics_unchanged,
    }
}

fn promote_branch_name(args: &PromoteArgs, metadata: &ExperimentMetadata) -> String {
    args.branch
        .clone()
        .or_else(|| args.branch_name.clone())
        .or_else(|| {
            metadata
                .name
                .clone()
                .filter(|name| git_branch_name_is_valid(name))
        })
        .unwrap_or_else(|| format!("exp-{}", &metadata.exp_id.to_string()[..12]))
}

fn git_branch_name_is_valid(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    Command::new("git")
        .args(["check-ref-format", "--branch", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn create_experiment_branch(
    repo_root: &Path,
    branch: &str,
    metadata: &ExperimentMetadata,
) -> Result<String> {
    let tmp_parent = tempfile::Builder::new()
        .prefix("crab-exp-branch-")
        .tempdir()
        .map_err(CrabError::Io)?;
    let tmp_path = tmp_parent.path().join("worktree");
    let tmp_arg = path_to_git_arg(&tmp_path, "exp branch")?;

    run_git(
        repo_root,
        &[
            "worktree",
            "add",
            "--detach",
            &tmp_arg,
            &metadata.base_commit,
        ],
        "git worktree add experiment branch",
    )?;

    let result = (|| {
        apply_experiment_snapshot_to(repo_root, &metadata.exp_id, &tmp_path, "exp branch")?;
        run_git(
            &tmp_path,
            &["add", "-A"],
            "git add experiment branch snapshot",
        )?;

        let commit = if git_has_staged_changes(&tmp_path)? {
            let message = format!("crab exp {}", metadata.exp_id);
            run_git(
                &tmp_path,
                &["commit", "-m", &message],
                "git commit experiment branch snapshot",
            )?;
            git_output(
                &tmp_path,
                &["rev-parse", "HEAD"],
                "git rev-parse experiment branch",
            )?
        } else {
            metadata.base_commit.clone()
        };

        create_git_branch(repo_root, branch, &commit)?;
        Ok(commit)
    })();

    if let Err(e) = run_git(
        repo_root,
        &["worktree", "remove", "--force", &tmp_arg],
        "git worktree remove experiment branch",
    ) {
        warn!(
            exp_id = %metadata.exp_id,
            path = %tmp_path.display(),
            error = %e,
            "exp branch: failed to remove temporary worktree",
        );
    }

    result
}

/// Invoke `git branch <name> <commit>` against `repo_root`.
///
/// Same pattern as [`crate::workflow::exp_worktree`]'s `run_git`
/// shell-outs. We don't use gitoxide here because branch creation
/// hooks, reflog, and worktree-aware ref checks are all handled by
/// the `git` binary for free.
///
/// Errors:
/// - [`CrabError::Internal`] — spawn failure or non-zero exit.
///   `git branch` itself rejects pre-existing branches, so the
///   user sees a clean error when the target name is taken.
fn create_git_branch(repo_root: &Path, branch: &str, commit: &str) -> Result<()> {
    run_git(
        repo_root,
        &["branch", branch, commit],
        "git branch experiment",
    )
}

fn git_has_staged_changes(repo_root: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git diff --cached: {e}")))?;
    if output.status.success() {
        return Ok(false);
    }
    if output.status.code() == Some(1) {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(CrabError::Internal(format!(
        "git diff --cached --quiet failed: {}",
        stderr.trim()
    )))
}

fn git_output(repo_root: &Path, args: &[&str], context: &str) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn {context}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "{context} failed: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn run_git(repo_root: &Path, args: &[&str], context: &str) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn {context}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "{context} failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

fn path_to_git_arg(path: &Path, command: &str) -> Result<String> {
    path.to_str()
        .map(std::borrow::ToOwned::to_owned)
        .ok_or_else(|| CrabError::Configuration {
            key: command.to_owned(),
            origin: format!("git worktree path is not valid UTF-8: {}", path.display()),
        })
}

fn emit_run(payload: &ExpRunPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_RUN_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!("exp run: {} ({})", payload.exp_id, payload.status);
            println!("  base_commit: {}", payload.base_commit);
            if let Some(name) = &payload.name {
                println!("  name: {name}");
            }
            if let Some(message) = &payload.message {
                println!("  message: {message}");
            }
            println!("  stages: {}", payload.stages.len());
            println!("  duration_ms: {}", payload.duration_ms);
        }
    }
}

fn emit_show(payload: &ExpShowPayload, m: &ExperimentMetadata, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_SHOW_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!("exp_id: {}", m.exp_id);
            if let Some(name) = &m.name {
                println!("name: {name}");
            }
            if let Some(message) = &m.message {
                println!("message: {message}");
            }
            println!("base_commit: {}", m.base_commit);
            println!("started_at: {}", m.started_at);
            if let Some(ended) = &m.ended_at {
                println!("ended_at: {ended}");
            }
            println!("host_fingerprint: {}", m.host_fingerprint);
            if m.param_overrides.is_empty() {
                println!("param_overrides: (none)");
            } else {
                println!("param_overrides:");
                for (k, v) in &m.param_overrides {
                    println!("  {k} = {v}");
                }
            }
            if m.stages.is_empty() {
                println!("stages: (none)");
            } else {
                println!("stages:");
                for (k, v) in &m.stages {
                    println!("  {k}: {v}");
                }
            }
            if m.metrics.is_empty() {
                println!("metrics: (none)");
            } else {
                println!("metrics:");
                for k in m.metrics.keys() {
                    println!("  {k}");
                }
            }
            match payload.metadata.get("checkpoints") {
                Some(serde_json::Value::Array(checkpoints)) if !checkpoints.is_empty() => {
                    println!("checkpoints:");
                    for checkpoint in checkpoints {
                        let id = checkpoint
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown");
                        let stage = checkpoint
                            .get("stage")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown");
                        let sequence = checkpoint
                            .get("sequence")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or_default();
                        println!("  {id} ({stage} #{sequence})");
                    }
                }
                _ => println!("checkpoints: (none)"),
            }
        }
    }
}

fn emit_show_list(
    payload: &ExpShowListPayload,
    mode: OutputMode,
    options: ExpShowListRenderOptions,
) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_SHOW_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text if options.markdown => {
            print!(
                "{}",
                render_experiment_table_markdown(&payload.experiments, options.precision)
            );
        }
        OutputMode::Text if options.csv => {
            print!(
                "{}",
                render_experiment_table_csv(&payload.experiments, options.precision)
            );
        }
        OutputMode::Text => print_experiment_table(&payload.experiments, options.precision),
    }
}

fn emit_diff(payload: &ExpDiffPayload, mode: OutputMode, options: ExpDiffRenderOptions) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_DIFF_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text if options.markdown => {
            print!("{}", render_exp_diff_markdown(payload, options));
        }
        OutputMode::Text => {
            println!("diff: {} ... {}", payload.id_a, payload.id_b);
            if !payload.params_added.is_empty() {
                println!("params added:");
                for (k, v) in &payload.params_added {
                    let key = exp_diff_display_key(k, options.no_path);
                    println!("  +{key} = {v}");
                }
            }
            if !payload.params_removed.is_empty() {
                println!("params removed:");
                for (k, v) in &payload.params_removed {
                    let key = exp_diff_display_key(k, options.no_path);
                    println!("  -{key} = {v}");
                }
            }
            if !payload.params_changed.is_empty() {
                println!("params changed:");
                for (k, (old, new)) in &payload.params_changed {
                    let key = exp_diff_display_key(k, options.no_path);
                    println!("  ~{key}: {old} -> {new}");
                }
            }
            if options.include_unchanged && !payload.params_unchanged.is_empty() {
                println!("params unchanged:");
                for (k, v) in &payload.params_unchanged {
                    let key = exp_diff_display_key(k, options.no_path);
                    println!("  ={key} = {v}");
                }
            }
            if !payload.stages_changed.is_empty() {
                println!("stages changed:");
                for (k, (a, b)) in &payload.stages_changed {
                    let left = a.as_deref().unwrap_or("<absent>");
                    let right = b.as_deref().unwrap_or("<absent>");
                    println!("  {k}: {left} -> {right}");
                }
            }
            if options.include_unchanged && !payload.stages_unchanged.is_empty() {
                println!("stages unchanged:");
                for (k, v) in &payload.stages_unchanged {
                    println!("  ={k}: {v}");
                }
            }
            if !payload.metrics_changed.is_empty() {
                println!("metrics changed:");
                for (k, (a, b)) in &payload.metrics_changed {
                    let left = format_optional_metric(a.as_ref(), options.precision, "<absent>");
                    let right = format_optional_metric(b.as_ref(), options.precision, "<absent>");
                    let key = exp_diff_display_key(k, options.no_path);
                    println!("  {key}: {left} -> {right}");
                }
            }
            if options.include_unchanged && !payload.metrics_unchanged.is_empty() {
                println!("metrics unchanged:");
                for (k, v) in &payload.metrics_unchanged {
                    let value = format_metric_value(v, options.precision);
                    let key = exp_diff_display_key(k, options.no_path);
                    println!("  ={key}: {value}");
                }
            }
            if payload.params_added.is_empty()
                && payload.params_removed.is_empty()
                && payload.params_changed.is_empty()
                && payload.params_unchanged.is_empty()
                && payload.stages_changed.is_empty()
                && payload.stages_unchanged.is_empty()
                && payload.metrics_changed.is_empty()
                && payload.metrics_unchanged.is_empty()
            {
                println!("(no differences)");
            }
        }
    }
}

fn render_exp_diff_markdown(payload: &ExpDiffPayload, options: ExpDiffRenderOptions) -> String {
    let mut out = String::new();
    out.push_str("# Experiment diff\n\n");
    out.push_str("| Old | New |\n");
    out.push_str("| --- | --- |\n");
    out.push_str("| `");
    out.push_str(&markdown_cell(&payload.id_a));
    out.push_str("` | `");
    out.push_str(&markdown_cell(&payload.id_b));
    out.push_str("` |\n");

    if diff_is_empty(payload) {
        out.push_str("\nNo experiment differences.\n");
        return out;
    }

    if !payload.params_added.is_empty()
        || !payload.params_removed.is_empty()
        || !payload.params_changed.is_empty()
        || (options.include_unchanged && !payload.params_unchanged.is_empty())
    {
        out.push_str("\n## Params\n\n");
        out.push_str("| Change | Key | Old | New |\n");
        out.push_str("| --- | --- | --- | --- |\n");
        for (key, value) in &payload.params_added {
            push_markdown_row(
                &mut out,
                &[
                    "added",
                    exp_diff_display_key(key, options.no_path),
                    "",
                    value,
                ],
            );
        }
        for (key, value) in &payload.params_removed {
            push_markdown_row(
                &mut out,
                &[
                    "removed",
                    exp_diff_display_key(key, options.no_path),
                    value,
                    "",
                ],
            );
        }
        for (key, (old, new)) in &payload.params_changed {
            push_markdown_row(
                &mut out,
                &[
                    "changed",
                    exp_diff_display_key(key, options.no_path),
                    old,
                    new,
                ],
            );
        }
        if options.include_unchanged {
            for (key, value) in &payload.params_unchanged {
                push_markdown_row(
                    &mut out,
                    &[
                        "unchanged",
                        exp_diff_display_key(key, options.no_path),
                        value,
                        value,
                    ],
                );
            }
        }
    }

    if !payload.stages_changed.is_empty()
        || (options.include_unchanged && !payload.stages_unchanged.is_empty())
    {
        out.push_str("\n## Stages\n\n");
        out.push_str("| Stage | Old | New |\n");
        out.push_str("| --- | --- | --- |\n");
        for (stage, (old, new)) in &payload.stages_changed {
            let old = old.as_deref().unwrap_or("");
            let new = new.as_deref().unwrap_or("");
            push_markdown_row(&mut out, &[stage, old, new]);
        }
        if options.include_unchanged {
            for (stage, hash) in &payload.stages_unchanged {
                push_markdown_row(&mut out, &[stage, hash, hash]);
            }
        }
    }

    if !payload.metrics_changed.is_empty()
        || (options.include_unchanged && !payload.metrics_unchanged.is_empty())
    {
        out.push_str("\n## Metrics\n\n");
        out.push_str("| Metric | Old | New | Change |\n");
        out.push_str("| --- | --- | --- | --- |\n");
        for (metric, (old, new)) in &payload.metrics_changed {
            let old_value = format_optional_metric(old.as_ref(), options.precision, "");
            let new_value = format_optional_metric(new.as_ref(), options.precision, "");
            let change = format_metric_change(old.as_ref(), new.as_ref(), options.precision);
            push_markdown_row(
                &mut out,
                &[
                    exp_diff_display_key(metric, options.no_path),
                    &old_value,
                    &new_value,
                    &change,
                ],
            );
        }
        if options.include_unchanged {
            for (metric, value) in &payload.metrics_unchanged {
                let value = format_metric_value(value, options.precision);
                push_markdown_row(
                    &mut out,
                    &[
                        exp_diff_display_key(metric, options.no_path),
                        &value,
                        &value,
                        "",
                    ],
                );
            }
        }
    }

    out
}

fn exp_diff_display_key(key: &str, no_path: bool) -> &str {
    if no_path {
        key.split_once(':').map_or(key, |(_, rest)| rest)
    } else {
        key
    }
}

fn push_markdown_row(out: &mut String, cells: &[&str]) {
    out.push('|');
    for cell in cells {
        out.push(' ');
        out.push_str(&markdown_cell(cell));
        out.push_str(" |");
    }
    out.push('\n');
}

fn markdown_cell(value: &str) -> String {
    value.replace('\n', "<br>").replace('|', "\\|")
}

fn diff_is_empty(payload: &ExpDiffPayload) -> bool {
    payload.params_added.is_empty()
        && payload.params_removed.is_empty()
        && payload.params_changed.is_empty()
        && payload.params_unchanged.is_empty()
        && payload.stages_changed.is_empty()
        && payload.stages_unchanged.is_empty()
        && payload.metrics_changed.is_empty()
        && payload.metrics_unchanged.is_empty()
}

fn format_optional_metric(
    value: Option<&serde_json::Value>,
    precision: usize,
    absent: &str,
) -> String {
    value.map_or_else(|| absent.to_owned(), |v| format_metric_value(v, precision))
}

fn format_metric_value(value: &serde_json::Value, precision: usize) -> String {
    match value {
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer.to_string()
            } else if let Some(integer) = number.as_u64() {
                integer.to_string()
            } else if let Some(float) = number.as_f64() {
                format_float(float, precision)
            } else {
                number.to_string()
            }
        }
        _ => value.to_string(),
    }
}

fn format_metric_change(
    old: Option<&serde_json::Value>,
    new: Option<&serde_json::Value>,
    precision: usize,
) -> String {
    match (
        old.and_then(serde_json::Value::as_f64),
        new.and_then(serde_json::Value::as_f64),
    ) {
        (Some(old), Some(new)) => format_float(new - old, precision),
        _ => String::new(),
    }
}

fn format_float(value: f64, precision: usize) -> String {
    let mut formatted = format!("{value:.precision$}");
    if formatted.contains('.') {
        while formatted.ends_with('0') {
            formatted.pop();
        }
        if formatted.ends_with('.') {
            formatted.pop();
        }
    }
    if formatted == "-0" {
        "0".to_owned()
    } else {
        formatted
    }
}

fn emit_ls(payload: &ExpLsPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_LS_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => print_experiment_table(&payload.experiments, 5),
    }
}

fn print_experiment_table(experiments: &[ExpSummary], precision: usize) {
    if experiments.is_empty() {
        println!("No experiments found.");
        return;
    }
    // Header row; column titles are pure literals by design so the
    // table aligns even when ids aren't the canonical 36-char length.
    #[expect(
        clippy::print_literal,
        reason = "table header row uses literal strings"
    )]
    {
        println!(
            "{:<36}  {:<20}  {:<24}  {:<24}  {:<10}  {:<8}  {:<12}  {:<24}  {}",
            "EXP_ID",
            "NAME",
            "MESSAGE",
            "STARTED_AT",
            "STAGES",
            "STATUS",
            "BASE_COMMIT",
            "PARAMS",
            "METRICS"
        );
    }
    for e in experiments {
        let short_commit = if e.base_commit.len() > 12 {
            &e.base_commit[..12]
        } else {
            e.base_commit.as_str()
        };
        let params = format_summary_params(&e.params);
        let metrics = format_summary_metrics(&e.metrics, precision);
        println!(
            "{:<36}  {:<20}  {:<24}  {:<24}  {:<10}  {:<8}  {:<12}  {:<24}  {}",
            e.id,
            e.name.as_deref().unwrap_or(""),
            e.message.as_deref().unwrap_or(""),
            e.started_at,
            e.stages,
            e.status.as_str(),
            short_commit,
            params,
            metrics
        );
    }
}

fn render_experiment_table_markdown(experiments: &[ExpSummary], precision: usize) -> String {
    if experiments.is_empty() {
        return "No experiments found.\n".to_owned();
    }

    let mut out = String::new();
    out.push_str(
        "| EXP_ID | NAME | MESSAGE | STARTED_AT | STAGES | STATUS | BASE_COMMIT | PARAMS | METRICS |\n",
    );
    out.push_str("| --- | --- | --- | --- | ---: | --- | --- | --- | --- |\n");
    for experiment in experiments {
        let short_commit = short_commit(&experiment.base_commit);
        let stages = experiment.stages.to_string();
        let params = format_summary_params(&experiment.params);
        let metrics = format_summary_metrics(&experiment.metrics, precision);
        let name = experiment.name.as_deref().unwrap_or("");
        let message = experiment.message.as_deref().unwrap_or("");
        push_markdown_row(
            &mut out,
            &[
                &experiment.id,
                name,
                message,
                &experiment.started_at,
                &stages,
                &experiment.status,
                short_commit,
                &params,
                &metrics,
            ],
        );
    }
    out
}

fn render_experiment_table_csv(experiments: &[ExpSummary], precision: usize) -> String {
    let mut out = String::new();
    push_csv_row(
        &mut out,
        &[
            "exp_id",
            "name",
            "message",
            "started_at",
            "stages",
            "status",
            "base_commit",
            "params",
            "metrics",
        ],
    );
    for experiment in experiments {
        let stages = experiment.stages.to_string();
        let params = format_summary_params(&experiment.params);
        let metrics = format_summary_metrics(&experiment.metrics, precision);
        push_csv_row(
            &mut out,
            &[
                &experiment.id,
                experiment.name.as_deref().unwrap_or(""),
                experiment.message.as_deref().unwrap_or(""),
                &experiment.started_at,
                &stages,
                &experiment.status,
                &experiment.base_commit,
                &params,
                &metrics,
            ],
        );
    }
    out
}

fn push_csv_row(out: &mut String, cells: &[&str]) {
    for (idx, cell) in cells.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(&csv_cell(cell));
    }
    out.push('\n');
}

fn csv_cell(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn short_commit(commit: &str) -> &str {
    if commit.len() > 12 {
        &commit[..12]
    } else {
        commit
    }
}

fn format_summary_params(params: &BTreeMap<String, String>) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_summary_metrics(
    metrics: &BTreeMap<String, serde_json::Value>,
    precision: usize,
) -> String {
    metrics
        .iter()
        .map(|(key, value)| format!("{key}={}", format_metric_value(value, precision)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn emit_promote(payload: &ExpPromotePayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_PROMOTE_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!(
                "promoted exp {} → branch {} at {}",
                payload.exp_id, payload.branch, payload.commit
            );
        }
    }
}

fn emit_apply(payload: &ExpApplyPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_APPLY_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            if let Some(checkpoint) = &payload.checkpoint {
                println!("applied checkpoint {checkpoint}");
            }
            println!(
                "applied exp {}: {} file(s), {} deletion(s)",
                payload.exp_id,
                payload.applied.len(),
                payload.deleted.len()
            );
            for path in &payload.applied {
                println!("  {}", path.display());
            }
            for path in &payload.deleted {
                println!("  deleted {}", path.display());
            }
        }
    }
}

fn emit_reset(payload: &ExpResetPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_RESET_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            match &payload.checkpoint {
                Some(checkpoint) => {
                    println!("reset exp {} to checkpoint {checkpoint}", payload.exp_id);
                }
                None => println!("reset exp {} to base", payload.exp_id),
            }
            for stage in &payload.reset_stages {
                println!("  {stage}");
            }
        }
    }
}

fn emit_save(payload: &ExpSavePayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_SAVE_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!("saved exp {} ({})", payload.exp_id, payload.status);
            println!("  base_commit: {}", payload.base_commit);
            if let Some(name) = &payload.name {
                println!("  name: {name}");
            }
            if let Some(message) = &payload.message {
                println!("  message: {message}");
            }
            println!("  stages: {}", payload.stages.len());
        }
    }
}

fn emit_rename(payload: &ExpRenamePayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_RENAME_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            let old = payload.old_name.as_deref().unwrap_or("(none)");
            println!(
                "renamed exp {}: {} -> {}",
                payload.exp_id, old, payload.new_name
            );
        }
    }
}

fn emit_push(payload: &ExpPushPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_PUSH_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!(
                "pushed {} experiment(s), skipped {}",
                payload.pushed.len(),
                payload.skipped.len()
            );
            for id in &payload.pushed {
                println!("  pushed {id}");
            }
            for id in &payload.skipped {
                println!("  skipped {id}");
            }
        }
    }
}

fn emit_pull(payload: &ExpPullPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_PULL_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!(
                "pulled {} experiment(s), skipped {}",
                payload.pulled.len(),
                payload.skipped.len()
            );
            for id in &payload.pulled {
                println!("  pulled {id}");
            }
            for id in &payload.skipped {
                println!("  skipped {id}");
            }
        }
    }
}

fn emit_remove(payload: &ExpRemovePayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_REMOVE_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            let label = if payload.dry_run {
                "would remove"
            } else {
                "removed"
            };
            if payload.removed.is_empty()
                && payload.removed_remote.is_empty()
                && payload.removed_queue.is_empty()
            {
                println!("No experiments matched.");
            } else {
                if !payload.removed.is_empty() {
                    println!(
                        "{} {} experiment(s); kept {}:",
                        label,
                        payload.removed.len(),
                        payload.kept.len()
                    );
                    for id in &payload.removed {
                        println!("  {id}");
                    }
                }
                if !payload.removed_remote.is_empty() {
                    println!(
                        "{} {} remote experiment(s); kept {} remote:",
                        label,
                        payload.removed_remote.len(),
                        payload.kept_remote.len()
                    );
                    for id in &payload.removed_remote {
                        println!("  {id}");
                    }
                }
                if !payload.removed_queue.is_empty() {
                    println!(
                        "{} {} queued experiment(s); kept {} queued:",
                        label,
                        payload.removed_queue.len(),
                        payload.kept_queue.len()
                    );
                    for id in &payload.removed_queue {
                        println!("  {id}");
                    }
                }
            }
        }
    }
}

fn emit_clean(payload: &ExpCleanPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_CLEAN_SCHEMA, EXP_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            println!(
                "Cleaned experiment temp files: {} tmpdir(s), {} active marker(s), {} kill request(s), {} log file(s).",
                payload.removed_tmpdirs,
                payload.removed_active_markers,
                payload.removed_kill_requests,
                payload.removed_logs
            );
        }
    }
}

fn emit_gc(payload: &ExpGcPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(EXP_GC_SCHEMA, EXP_SCHEMA_VERSION, payload);
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
                "exp gc",
            );
            if payload.removed.is_empty() {
                println!("No experiments beyond --keep={}.", payload.keep);
            } else {
                println!(
                    "{} {} experiment(s); kept {}:",
                    label,
                    payload.removed.len(),
                    payload.kept.len()
                );
                for id in &payload.removed {
                    println!("  {id}");
                }
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

    #[test]
    fn parse_overrides_accepts_valid_entries() {
        let out =
            parse_overrides(&["a=1".into(), "b.c=hello world".into(), "empty=".into()]).unwrap();
        assert_eq!(out.get("a").unwrap(), "1");
        assert_eq!(out.get("b.c").unwrap(), "hello world");
        assert_eq!(out.get("empty").unwrap(), "");
    }

    #[test]
    fn non_queued_exp_run_rejects_sweep_overrides() {
        for entry in [
            "model.arch=choice(resnet,efficientnet)",
            "model.lr=range(1,4)",
            "model.arch=resnet,efficientnet",
        ] {
            let err = reject_sweeps_without_queue(&[entry.to_owned()]).unwrap_err();
            assert!(matches!(err, CrabError::Configuration { .. }));
        }
    }

    #[test]
    fn non_queued_exp_run_accepts_scalar_values_with_commas_when_quoted_or_nested() {
        reject_sweeps_without_queue(&[
            "model.label='resnet,efficientnet'".to_owned(),
            "model.layers=[1, 2]".to_owned(),
        ])
        .unwrap();
    }

    #[test]
    fn run_args_accept_dvc_set_param_alias() {
        let args = RunArgs::try_parse_from(["run", "--set-param", "model.lr=0.01"]).unwrap();
        assert_eq!(args.set, vec!["model.lr=0.01"]);

        let args = RunArgs::try_parse_from([
            "run",
            "-S",
            "model.lr=0.02",
            "--queue",
            "-n",
            "sweep",
            "-m",
            "try lower lr",
            "--temp",
            "-C",
            "secrets.env",
        ])
        .unwrap();
        assert_eq!(args.set, vec!["model.lr=0.02"]);
        assert!(args.queue);
        assert_eq!(args.name.as_deref(), Some("sweep"));
        assert_eq!(args.message.as_deref(), Some("try lower lr"));
        assert!(args.temp);
        assert_eq!(args.copy_paths, vec![PathBuf::from("secrets.env")]);

        let args = RunArgs::try_parse_from([
            "run",
            "--run-all",
            "-j",
            "2",
            "-f",
            "--pull",
            "--allow-missing",
            "-k",
            "--ignore-errors",
        ])
        .unwrap();
        assert!(args.run_all);
        assert_eq!(args.jobs, Some(2));
        assert!(args.force);
        assert!(args.pull);
        assert!(args.allow_missing);
        assert!(args.keep_going);
        assert!(args.ignore_errors);

        let args = RunArgs::try_parse_from(["run", "--dry", "train"]).unwrap();
        assert!(args.dry_run);
        assert_eq!(args.targets, vec!["train".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "train", "--json"]).unwrap();
        assert!(args.json);
        assert_eq!(args.targets, vec!["train".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "--dry-run"]).unwrap();
        assert!(args.dry_run);

        let err = RunArgs::try_parse_from(["run", "--queue", "--dry"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        let args = RunArgs::try_parse_from(["run", "--single-item", "--glob", "train_*"]).unwrap();
        assert!(args.single_item);
        assert!(args.glob);
        assert_eq!(args.targets, vec!["train_*".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "--pipeline", "evaluate"]).unwrap();
        assert!(args.pipeline);
        assert_eq!(args.targets, vec!["evaluate".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "-R", "pipelines.train"]).unwrap();
        assert!(args.recursive);
        assert_eq!(args.targets, vec!["pipelines.train".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "--force-downstream", "train"]).unwrap();
        assert!(args.force_downstream);
        assert_eq!(args.targets, vec!["train".to_owned()]);

        let args = RunArgs::try_parse_from(["run", "-i", "train"]).unwrap();
        assert!(args.interactive);
        assert_eq!(args.targets, vec!["train".to_owned()]);

        let err = RunArgs::try_parse_from(["run", "--queue", "--run-all"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        let err =
            RunArgs::try_parse_from(["run", "--single-item", "--downstream", "train"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn checkpoint_selector_rejects_sequence_ambiguity_across_stages() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("train.json");
        let second = temp.path().join("evaluate.json");
        let record = |stage: &str| CheckpointRecord {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            id: format!("{stage}-checkpoint"),
            experiment: "exp".to_owned(),
            stage: stage.to_owned(),
            sequence: 0,
            parent: None,
            request_nonce: None,
            stage_hash: format!("b3:{}", "cd".repeat(32)),
            created_at_unix_ms: 0,
            outputs: BTreeMap::new(),
            metrics: BTreeMap::new(),
            terminal: false,
            resumable: true,
        };
        CheckpointLineage {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            records: vec![record("train")],
        }
        .save_atomic(&first)
        .unwrap();
        CheckpointLineage {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            records: vec![record("evaluate")],
        }
        .save_atomic(&second)
        .unwrap();
        assert!(select_checkpoint_from_paths(&[first, second], "0").is_err());
    }

    #[test]
    fn checkpoint_apply_rejects_corrupt_immutable_payload() {
        let temp = tempfile::tempdir().unwrap();
        let state_root = temp.path().join("state");
        let target_root = temp.path().join("target");
        let bytes = b"checkpoint-bytes";
        let digest = blake3::hash(bytes).to_hex().to_string();
        let object = state_root.join("objects").join(&digest).join("payload");
        fs::create_dir_all(object.parent().unwrap()).unwrap();
        fs::write(&object, bytes).unwrap();
        let record = CheckpointRecord {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            id: "checkpoint".to_owned(),
            experiment: "exp".to_owned(),
            stage: "train".to_owned(),
            sequence: 0,
            parent: None,
            request_nonce: None,
            stage_hash: format!("b3:{}", "cd".repeat(32)),
            created_at_unix_ms: 0,
            outputs: BTreeMap::from([("model.bin".to_owned(), format!("b3:{digest}"))]),
            metrics: BTreeMap::new(),
            terminal: false,
            resumable: true,
        };
        let applied = apply_checkpoint_record_to(&state_root, &target_root, &record).unwrap();
        assert_eq!(applied, vec![PathBuf::from("model.bin")]);
        assert_eq!(fs::read(target_root.join("model.bin")).unwrap(), bytes);
        fs::write(&object, b"corrupt").unwrap();
        assert!(apply_checkpoint_record_to(&state_root, &target_root, &record).is_err());
    }

    #[test]
    fn resume_checkpoint_state_copy_is_validated_and_drops_transient_locks() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let bytes = b"checkpoint-bytes";
        let digest = blake3::hash(bytes).to_hex().to_string();
        let object = source.join("objects").join(&digest).join("payload");
        fs::create_dir_all(object.parent().unwrap()).unwrap();
        fs::write(&object, bytes).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("train.lock"), b"stale lock").unwrap();
        let record = CheckpointRecord {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            id: "checkpoint".to_owned(),
            experiment: "source-exp".to_owned(),
            stage: "train".to_owned(),
            sequence: 0,
            parent: None,
            request_nonce: None,
            stage_hash: format!("b3:{}", "cd".repeat(32)),
            created_at_unix_ms: 0,
            outputs: BTreeMap::from([("model.bin".to_owned(), format!("b3:{digest}"))]),
            metrics: BTreeMap::new(),
            terminal: false,
            resumable: true,
        };
        CheckpointLineage {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            records: vec![record.clone()],
        }
        .save_atomic(&source.join("train.json"))
        .unwrap();

        copy_checkpoint_state(&source, &destination).unwrap();
        assert!(!destination.join("train.lock").exists());
        let copied = CheckpointLineage::load(&destination.join("train.json")).unwrap();
        assert_eq!(copied.records, vec![record]);
        assert_eq!(
            fs::read(destination.join("objects").join(digest).join("payload")).unwrap(),
            bytes
        );
    }

    #[test]
    fn save_args_accept_dvc_message_and_include_untracked() {
        let args = SaveArgs::try_parse_from([
            "save",
            "-R",
            "-f",
            "models/dvc.yaml",
            "-n",
            "manual",
            "-m",
            "record baseline",
            "-I",
            "notes.txt",
        ])
        .unwrap();

        assert!(args.recursive);
        assert!(args.force);
        assert_eq!(args.name.as_deref(), Some("manual"));
        assert_eq!(args.message.as_deref(), Some("record baseline"));
        assert_eq!(args.include_untracked, vec![PathBuf::from("notes.txt")]);
        assert_eq!(args.targets, vec!["models/dvc.yaml".to_owned()]);
    }

    #[test]
    fn clean_args_accept_json() {
        let args = CleanArgs::try_parse_from(["clean", "--json"]).unwrap();
        assert!(args.json);
    }

    #[test]
    fn exp_clean_removes_stale_runtime_files_and_keeps_active_queue_tmpdir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let active = ExperimentId::new_v7();
        let done = ExperimentId::new_v7();
        let orphan = ExperimentId::new_v7();
        let missing = ExperimentId::new_v7();
        let exp_parent = root.join(".crab/workflow/exp");
        let queue_parent = root.join(".crab/exp-queue");
        let running_dir = queue_parent.join("running");
        let kill_dir = queue_parent.join("kill");
        let logs_dir = queue_parent.join("logs");
        let active_tmpdir = exp_parent.join(active.to_string());
        let done_tmpdir = exp_parent.join(done.to_string());
        let orphan_tmpdir = exp_parent.join(orphan.to_string());

        std::fs::create_dir_all(&active_tmpdir).unwrap();
        std::fs::create_dir_all(&done_tmpdir).unwrap();
        std::fs::create_dir_all(&orphan_tmpdir).unwrap();
        std::fs::create_dir_all(&running_dir).unwrap();
        std::fs::create_dir_all(&kill_dir).unwrap();
        std::fs::create_dir_all(&logs_dir).unwrap();

        let queue = ExpQueue::new(queue_parent.clone());
        queue
            .enqueue(&ExpQueueEntry {
                id: active.to_string(),
                queued_at: "2026-06-17T00:00:00.000Z".to_owned(),
                base_commit: "abc123".to_owned(),
                name: None,
                message: None,
                param_overrides: BTreeMap::new(),
                targets: Vec::new(),
                recursive: false,
                single_item: false,
                downstream: false,
                force_downstream: false,
                pipeline: false,
                all_pipelines: false,
                glob: false,
                copy_paths: Vec::new(),
                status: ExpStatus::Running,
            })
            .unwrap();
        queue
            .enqueue(&ExpQueueEntry {
                id: done.to_string(),
                queued_at: "2026-06-17T00:00:01.000Z".to_owned(),
                base_commit: "abc123".to_owned(),
                name: None,
                message: None,
                param_overrides: BTreeMap::new(),
                targets: Vec::new(),
                recursive: false,
                single_item: false,
                downstream: false,
                force_downstream: false,
                pipeline: false,
                all_pipelines: false,
                glob: false,
                copy_paths: Vec::new(),
                status: ExpStatus::Done,
            })
            .unwrap();

        let active_marker = running_dir.join(format!("{active}.json"));
        let done_marker = running_dir.join(format!("{done}.json"));
        std::fs::write(
            &active_marker,
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": active.to_string(),
                "tmpdir": active_tmpdir,
                "started_at": "2026-06-17T00:00:02.000Z"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            &done_marker,
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": done.to_string(),
                "tmpdir": done_tmpdir,
                "started_at": "2026-06-17T00:00:03.000Z"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(kill_dir.join(format!("{active}.json")), "force\n").unwrap();
        std::fs::write(kill_dir.join(format!("{done}.json")), "force\n").unwrap();
        std::fs::write(logs_dir.join(format!("{done}.log")), "done\n").unwrap();
        std::fs::write(logs_dir.join(format!("{missing}.log")), "missing\n").unwrap();

        let payload = run_exp_clean(&CleanArgs { json: true }, root).unwrap();

        assert_eq!(payload.removed_tmpdirs, 2);
        assert_eq!(payload.removed_active_markers, 1);
        assert_eq!(payload.removed_kill_requests, 1);
        assert_eq!(payload.removed_logs, 1);
        assert!(active_tmpdir.exists());
        assert!(!done_tmpdir.exists());
        assert!(!orphan_tmpdir.exists());
        assert!(active_marker.exists());
        assert!(!done_marker.exists());
        assert!(kill_dir.join(format!("{active}.json")).exists());
        assert!(!kill_dir.join(format!("{done}.json")).exists());
        assert!(logs_dir.join(format!("{done}.log")).exists());
        assert!(!logs_dir.join(format!("{missing}.log")).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exp_run_dry_run_does_not_persist_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_dry_run_workflow_repo(root);

        let args = RunArgs::try_parse_from(["run", "--dry"]).unwrap();
        run_exp_run(&args, root)
            .await
            .expect("experiment dry run succeeds");

        let summaries = collect_summaries(root).expect("metadata scan succeeds");
        assert!(
            summaries.is_empty(),
            "dry-run experiment must not persist metadata"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exp_run_copy_paths_overlays_untracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_copy_paths_workflow_repo(root);

        let args = RunArgs::try_parse_from([
            "run",
            "--copy-paths",
            "secret.txt",
            "--message",
            "copied secret into temp run",
        ])
        .unwrap();
        run_exp_run(&args, root)
            .await
            .expect("experiment can read copied untracked file");

        let summaries = collect_summaries(root).expect("metadata scan succeeds");
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].message.as_deref(),
            Some("copied secret into temp run")
        );
        assert_eq!(summaries[0].stages, 1);
    }

    #[test]
    fn exp_save_path_target_filters_stage_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_exp_save_target_repo(root);
        write_test_lockfile(root, &[("root", 1), ("models.train", 2), ("eval.score", 3)]);

        let payload = run_exp_save(
            &SaveArgs {
                name: Some("targeted".to_owned()),
                message: Some("models only".to_owned()),
                force: true,
                recursive: false,
                include_untracked: Vec::new(),
                json: true,
                targets: vec!["models/dvc.yaml".to_owned()],
            },
            root,
        )
        .expect("targeted save succeeds");

        assert_eq!(payload.stages.len(), 1);
        let expected_hash = "02".repeat(32);
        assert_eq!(
            payload.stages.get("models.train").map(String::as_str),
            Some(expected_hash.as_str())
        );
        assert_eq!(
            payload.metrics_files,
            vec![PathBuf::from("models/metrics.json")]
        );

        let exp_id: ExperimentId = payload.exp_id.parse().unwrap();
        let metadata = read_local_metadata(root, &exp_id).unwrap();
        assert_eq!(metadata.stages, payload.stages);
        assert_eq!(
            metadata.metrics.keys().cloned().collect::<Vec<_>>(),
            vec!["models/metrics.json".to_owned()]
        );
    }

    fn init_dry_run_workflow_repo(root: &Path) {
        run_git(root, &["init", "--initial-branch=main"], "git init").unwrap();
        run_git(
            root,
            &["config", "user.email", "test@example.com"],
            "git config email",
        )
        .unwrap();
        run_git(root, &["config", "user.name", "Test"], "git config name").unwrap();
        run_git(
            root,
            &["config", "commit.gpgsign", "false"],
            "git config signing",
        )
        .unwrap();

        std::fs::create_dir_all(root.join(".crab")).unwrap();
        std::fs::write(
            root.join(".crab/config.toml"),
            "[workflow]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(root.join(".gitignore"), ".crab/\n").unwrap();
        std::fs::write(
            root.join("crab.yaml"),
            "stages:\n  write:\n    cmd: \"printf 'nope\\n' > out.txt\"\n    outs:\n      - out.txt\n",
        )
        .unwrap();
        run_git(
            root,
            &["add", ".gitignore", "crab.yaml"],
            "git add workflow",
        )
        .unwrap();
        run_git(root, &["commit", "-m", "initial"], "git commit workflow").unwrap();
    }

    fn init_copy_paths_workflow_repo(root: &Path) {
        run_git(root, &["init", "--initial-branch=main"], "git init").unwrap();
        run_git(
            root,
            &["config", "user.email", "test@example.com"],
            "git config email",
        )
        .unwrap();
        run_git(root, &["config", "user.name", "Test"], "git config name").unwrap();
        run_git(
            root,
            &["config", "commit.gpgsign", "false"],
            "git config signing",
        )
        .unwrap();

        std::fs::create_dir_all(root.join(".crab")).unwrap();
        std::fs::write(
            root.join(".crab/config.toml"),
            "[workflow]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crab.yaml"),
            "stages:\n  use_secret:\n    cmd: \"cp secret.txt out.txt\"\n    deps:\n      - secret.txt\n    outs:\n      - out.txt\n",
        )
        .unwrap();
        run_git(
            root,
            &["add", ".crab/config.toml", "crab.yaml"],
            "git add workflow",
        )
        .unwrap();
        run_git(root, &["commit", "-m", "initial"], "git commit workflow").unwrap();
        std::fs::write(root.join("secret.txt"), "copied\n").unwrap();
    }

    fn init_exp_save_target_repo(root: &Path) {
        run_git(root, &["init", "--initial-branch=main"], "git init").unwrap();
        run_git(
            root,
            &["config", "user.email", "test@example.com"],
            "git config email",
        )
        .unwrap();
        run_git(root, &["config", "user.name", "Test"], "git config name").unwrap();
        run_git(
            root,
            &["config", "commit.gpgsign", "false"],
            "git config signing",
        )
        .unwrap();

        std::fs::create_dir_all(root.join(".crab")).unwrap();
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::create_dir_all(root.join("eval")).unwrap();
        std::fs::write(
            root.join(".crab/config.toml"),
            "[workflow]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crab.yaml"),
            "metrics:\n  - root_metrics.json\nstages:\n  root:\n    cmd: \"printf root > root.txt\"\n    outs:\n      - root.txt\n",
        )
        .unwrap();
        std::fs::write(
            root.join("models/crab.yaml"),
            "metrics:\n  - metrics.json\nstages:\n  train:\n    cmd: \"printf model > model.txt\"\n    outs:\n      - model.txt\n",
        )
        .unwrap();
        std::fs::write(
            root.join("eval/crab.yaml"),
            "metrics:\n  - metrics.json\nstages:\n  score:\n    cmd: \"printf eval > score.txt\"\n    outs:\n      - score.txt\n",
        )
        .unwrap();
        std::fs::write(root.join("root_metrics.json"), "{\"accuracy\":0.1}\n").unwrap();
        std::fs::write(root.join("models/metrics.json"), "{\"accuracy\":0.9}\n").unwrap();
        std::fs::write(root.join("eval/metrics.json"), "{\"accuracy\":0.2}\n").unwrap();
        run_git(root, &["add", "-A"], "git add targeted save repo").unwrap();
        run_git(
            root,
            &["commit", "-m", "initial"],
            "git commit targeted save repo",
        )
        .unwrap();
    }

    fn write_test_lockfile(root: &Path, stages: &[(&str, u8)]) {
        let mut lockfile = Lockfile::new();
        for (name, byte) in stages {
            lockfile.stages.insert(
                StageName::parse_effective(name).unwrap(),
                crab_workflow::LockedStage {
                    stage_hash: crab_types::workflow::StageHash([*byte; 32]),
                    cmd: crate::workflow::cache::CachedCmd::Shell {
                        shell: "echo".to_owned(),
                    },
                    deps: Vec::new(),
                    params: BTreeMap::new(),
                    env: BTreeMap::new(),
                    outs: Vec::new(),
                    metrics: Vec::new(),
                    plots: Vec::new(),
                    executed_at: String::new(),
                    duration_ms: 0,
                    host_fingerprint: String::new(),
                    attempts: 1,
                    source: "Local".to_owned(),
                },
            );
        }
        lockfile.save(&root.join("crab.lock")).unwrap();
    }

    fn test_queue_entry(id: &str, status: ExpStatus) -> ExpQueueEntry {
        ExpQueueEntry {
            id: id.to_owned(),
            queued_at: "2026-06-17T00:00:00.000Z".to_owned(),
            base_commit: "0".repeat(40),
            name: None,
            message: None,
            param_overrides: BTreeMap::new(),
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

    fn test_exp_metadata(id: ExperimentId, base_commit: &str) -> ExperimentMetadata {
        ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: base_commit.to_owned(),
            queue_commit: None,
            name: None,
            message: None,
            status: "success".to_owned(),
            param_overrides: BTreeMap::new(),
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: Vec::new(),
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: None,
        }
    }

    async fn write_remote_metadata_for_test(
        store: &Store,
        prefix: &str,
        metadata: &ExperimentMetadata,
    ) {
        let meta_bytes = metadata.canonical_json().unwrap();
        let meta_hash = metadata.content_hash().unwrap();
        store
            .put(
                &remote_exp_meta_object_path(prefix, &metadata.exp_id),
                Bytes::from(meta_bytes),
            )
            .await
            .unwrap();
        store
            .put(
                &remote_exp_stage_refs_object_path(prefix, &metadata.exp_id),
                Bytes::from(experiment_stage_refs_json(metadata).unwrap()),
            )
            .await
            .unwrap();
        store
            .put(
                &remote_exp_workspace_blob_path(prefix, &metadata.exp_id, "abc"),
                Bytes::from_static(b"workspace"),
            )
            .await
            .unwrap();
        store
            .put(
                &remote_exp_meta_ref_path(prefix, &metadata.exp_id),
                Bytes::from(meta_hash),
            )
            .await
            .unwrap();
    }

    fn init_three_commit_repo(root: &Path) {
        run_git(root, &["init", "--initial-branch=main"], "git init").unwrap();
        run_git(
            root,
            &["config", "user.email", "test@example.com"],
            "git config email",
        )
        .unwrap();
        run_git(root, &["config", "user.name", "Test"], "git config name").unwrap();
        run_git(
            root,
            &["config", "commit.gpgsign", "false"],
            "git config signing",
        )
        .unwrap();

        for idx in 0..3 {
            std::fs::write(root.join("tracked.txt"), format!("{idx}\n")).unwrap();
            run_git(root, &["add", "tracked.txt"], "git add tracked").unwrap();
            run_git(root, &["commit", "-m", "commit"], "git commit tracked").unwrap();
        }
    }

    fn git_rev_list(root: &Path, rev: &str) -> Vec<String> {
        let output = Command::new("git")
            .args(["rev-list", "--first-parent", rev])
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    #[test]
    fn diff_args_reject_markdown_with_json() {
        let err = DiffArgs::try_parse_from(["diff", "--json", "--md", "a", "b"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn diff_args_accept_dvc_output_flags() {
        let args = DiffArgs::try_parse_from([
            "diff",
            "--all",
            "--param-deps",
            "--no-path",
            "--precision",
            "3",
            "old",
            "new",
        ])
        .unwrap();
        assert!(args.all);
        assert!(args.param_deps);
        assert!(args.no_path);
        assert_eq!(args.precision, 3);
        assert_eq!(args.id_a, "old");
        assert_eq!(args.id_b, "new");
    }

    #[test]
    fn remove_args_accept_dvc_queue_history_and_remote_flags() {
        let args = RemoveArgs::try_parse_from(["remove", "--queue", "01931b9e"]).unwrap();
        assert!(args.queue);
        assert_eq!(args.ids, vec!["01931b9e"]);

        let args =
            RemoveArgs::try_parse_from(["remove", "--rev", "HEAD~1", "--num", "-1"]).unwrap();
        assert_eq!(args.rev.as_deref(), Some("HEAD~1"));
        assert_eq!(args.limit, Some(-1));

        let args = RemoveArgs::try_parse_from(["remove", "-g", "origin", "winner"]).unwrap();
        assert_eq!(args.git_remote.as_deref(), Some("origin"));
        assert_eq!(args.ids, vec!["winner"]);
    }

    #[test]
    fn show_args_accept_no_id_and_dvc_list_aliases() {
        let args = ShowArgs::try_parse_from([
            "show",
            "--all-commits",
            "--all-branches",
            "--all-tags",
            "--rev",
            "HEAD~1",
            "--num",
            "2",
            "--no-pager",
            "--md",
            "--sort-by",
            "model.lr",
            "--sort-order",
            "asc",
            "--precision",
            "3",
            "--only-changed",
            "--param-deps",
            "--sha",
            "--hide-failed",
            "--hide-queued",
            "--hide-workspace",
            "--force",
            "--drop",
            "seed",
            "--keep",
            "model.lr",
        ])
        .unwrap();
        assert!(args.id.is_none());
        assert!(args.all);
        assert!(args.all_branches);
        assert!(args.all_tags);
        assert_eq!(args.rev.as_deref(), Some("HEAD~1"));
        assert_eq!(args.limit, Some(2));
        assert!(args.no_pager);
        assert!(args.md);
        assert_eq!(args.sort_by.as_deref(), Some("model.lr"));
        assert_eq!(args.sort_order, Some(ExpShowSortOrder::Asc));
        assert_eq!(args.precision, Some(3));
        assert!(args.only_changed);
        assert!(args.param_deps);
        assert!(args.sha);
        assert!(args.hide_failed);
        assert!(args.hide_queued);
        assert!(args.hide_workspace);
        assert!(args.force);
        assert_eq!(args.drop, vec!["seed"]);
        assert_eq!(args.keep, vec!["model.lr"]);
    }

    #[test]
    fn show_args_accept_dvc_negative_num_as_unbounded() {
        let args = ShowArgs::try_parse_from(["show", "--num", "-1"]).unwrap();
        assert_eq!(args.limit, Some(-1));
        assert_eq!(normalized_show_limit(args.limit), None);
    }

    #[test]
    fn show_rejects_list_selectors_with_detail_id() {
        let tmp = tempfile::tempdir().unwrap();
        let args = ShowArgs {
            id: Some("01931b9e-4b3c-7b2a-b9f0-0123456789ab".to_owned()),
            all: true,
            all_branches: false,
            all_tags: false,
            rev: None,
            limit: None,
            no_pager: false,
            json: false,
            md: false,
            csv: false,
            sort_by: None,
            sort_order: None,
            precision: None,
            only_changed: false,
            param_deps: false,
            sha: false,
            hide_failed: false,
            hide_queued: false,
            hide_workspace: false,
            force: false,
            drop: Vec::new(),
            keep: Vec::new(),
        };
        let err = run_exp_show(&args, tmp.path()).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn parse_overrides_accepts_remove_without_equals() {
        let out =
            parse_overrides(&["~model.dropout".into(), "custom.yaml:~data.window".into()]).unwrap();
        assert_eq!(out.get("~model.dropout").map(String::as_str), Some(""));
        assert_eq!(
            out.get("custom.yaml:~data.window").map(String::as_str),
            Some("")
        );
    }

    #[test]
    fn parse_overrides_rejects_missing_equals() {
        let err = parse_overrides(&["bad".into()]).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn parse_overrides_rejects_empty_key() {
        let err = parse_overrides(&["=value".into()]).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn parse_overrides_rejects_repeated_keys() {
        let err = parse_overrides(&["a=1".into(), "a=2".into()]).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn normalize_experiment_name_rejects_blank_labels() {
        let err = normalize_experiment_name("  ", "exp rename").unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
        assert_eq!(
            normalize_experiment_name(" winner ", "exp rename").unwrap(),
            "winner"
        );
    }

    #[test]
    fn diff_detects_param_changes() {
        let id_a = ExperimentId::new_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id_b = ExperimentId::new_v7();

        let mut a_overrides = BTreeMap::new();
        a_overrides.insert("lr".into(), "0.1".into());
        a_overrides.insert("dropout".into(), "0.2".into());
        let mut b_overrides = BTreeMap::new();
        b_overrides.insert("lr".into(), "0.2".into()); // changed
        b_overrides.insert("epochs".into(), "10".into()); // added

        let make = |id: ExperimentId, overrides: BTreeMap<String, String>| ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "0".repeat(40),
            queue_commit: None,
            name: None,
            message: None,
            status: "success".to_owned(),
            param_overrides: overrides,
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: Vec::new(),
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: None,
        };

        let a = make(id_a, a_overrides);
        let b = make(id_b, b_overrides);
        let diff = build_diff_payload(&a, &b, false);

        assert_eq!(diff.params_added.get("epochs").unwrap(), "10");
        assert_eq!(diff.params_removed.get("dropout").unwrap(), "0.2");
        let (old, new) = diff.params_changed.get("lr").unwrap();
        assert_eq!(old, "0.1");
        assert_eq!(new, "0.2");
        assert!(diff.params_unchanged.is_empty());
    }

    #[test]
    fn diff_all_includes_unchanged_params_stages_and_metrics() {
        let id_a = ExperimentId::new_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id_b = ExperimentId::new_v7();
        let make = |id: ExperimentId| ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "0".repeat(40),
            queue_commit: None,
            name: None,
            message: None,
            status: "success".to_owned(),
            param_overrides: BTreeMap::from([
                ("params.yaml:model.lr".to_owned(), "0.1".to_owned()),
                ("seed".to_owned(), "42".to_owned()),
            ]),
            stages: BTreeMap::from([("train".to_owned(), "aa".repeat(32))]),
            metrics: BTreeMap::from([(
                "metrics.json".to_owned(),
                serde_json::json!({ "accuracy": 0.9 }),
            )]),
            cli_args: Vec::new(),
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: None,
        };
        let diff = build_diff_payload(&make(id_a), &make(id_b), true);
        let expected_stage = "aa".repeat(32);

        assert_eq!(
            diff.params_unchanged
                .get("params.yaml:model.lr")
                .map(String::as_str),
            Some("0.1")
        );
        assert_eq!(
            diff.stages_unchanged.get("train").map(String::as_str),
            Some(expected_stage.as_str())
        );
        assert_eq!(
            diff.metrics_unchanged.get("metrics.json"),
            Some(&serde_json::json!({ "accuracy": 0.9 }))
        );
    }

    #[test]
    fn diff_markdown_renders_tables_with_metric_precision() {
        let mut params_added = BTreeMap::new();
        params_added.insert("epochs".to_owned(), "10".to_owned());
        let mut params_changed = BTreeMap::new();
        params_changed.insert("lr".to_owned(), ("0.1".to_owned(), "0.2".to_owned()));
        let mut stages_changed = BTreeMap::new();
        stages_changed.insert(
            "train".to_owned(),
            (Some("old-stage".to_owned()), Some("new-stage".to_owned())),
        );
        let mut metrics_changed = BTreeMap::new();
        metrics_changed.insert(
            "metrics.json:accuracy".to_owned(),
            (
                Some(serde_json::json!(0.123_456)),
                Some(serde_json::json!(0.987_654)),
            ),
        );

        let payload = ExpDiffPayload {
            id_a: "01931b9e-4b3c-7b2a-b9f0-0123456789ab".to_owned(),
            id_b: "01931b9e-4b3c-7b2a-b9f0-1123456789ab".to_owned(),
            params_added,
            params_removed: BTreeMap::new(),
            params_changed,
            params_unchanged: BTreeMap::new(),
            stages_changed,
            stages_unchanged: BTreeMap::new(),
            metrics_changed,
            metrics_unchanged: BTreeMap::new(),
        };

        let rendered = render_exp_diff_markdown(
            &payload,
            ExpDiffRenderOptions {
                markdown: true,
                precision: 2,
                include_unchanged: false,
                no_path: false,
            },
        );

        assert!(rendered.contains("| Change | Key | Old | New |"));
        assert!(rendered.contains("| added | epochs |  | 10 |"));
        assert!(rendered.contains("| changed | lr | 0.1 | 0.2 |"));
        assert!(rendered.contains("| train | old-stage | new-stage |"));
        assert!(rendered.contains("| metrics.json:accuracy | 0.12 | 0.99 | 0.86 |"));
    }

    #[test]
    fn diff_markdown_no_path_strips_file_prefixes() {
        let payload = ExpDiffPayload {
            id_a: "01931b9e-4b3c-7b2a-b9f0-0123456789ab".to_owned(),
            id_b: "01931b9e-4b3c-7b2a-b9f0-1123456789ab".to_owned(),
            params_added: BTreeMap::new(),
            params_removed: BTreeMap::new(),
            params_changed: BTreeMap::from([(
                "params.yaml:model.lr".to_owned(),
                ("0.1".to_owned(), "0.2".to_owned()),
            )]),
            params_unchanged: BTreeMap::new(),
            stages_changed: BTreeMap::new(),
            stages_unchanged: BTreeMap::new(),
            metrics_changed: BTreeMap::from([(
                "metrics.json:accuracy".to_owned(),
                (Some(serde_json::json!(0.1)), Some(serde_json::json!(0.2))),
            )]),
            metrics_unchanged: BTreeMap::new(),
        };

        let rendered = render_exp_diff_markdown(
            &payload,
            ExpDiffRenderOptions {
                markdown: true,
                precision: 2,
                include_unchanged: false,
                no_path: true,
            },
        );

        assert!(rendered.contains("| changed | model.lr | 0.1 | 0.2 |"));
        assert!(rendered.contains("| accuracy | 0.1 | 0.2 | 0.1 |"));
        assert!(!rendered.contains("params.yaml:model.lr"));
        assert!(!rendered.contains("metrics.json:accuracy"));
    }

    #[test]
    fn show_summaries_sort_by_metric_key() {
        let tmp = tempfile::tempdir().unwrap();
        let make_meta = |value: f64| {
            let id = ExperimentId::new_v7();
            let mut metrics = BTreeMap::new();
            metrics.insert(
                "metrics.json".to_owned(),
                serde_json::json!({ "accuracy": value }),
            );
            ExperimentMetadata {
                schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
                exp_id: id,
                base_commit: "0".repeat(40),
                queue_commit: None,
                name: None,
                message: None,
                status: "success".to_owned(),
                param_overrides: BTreeMap::new(),
                stages: BTreeMap::new(),
                metrics,
                cli_args: Vec::new(),
                host_fingerprint: "test".into(),
                started_at: "2024-01-01T00:00:00.000Z".into(),
                ended_at: None,
            }
        };
        let lower = make_meta(0.9);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let higher = make_meta(0.95);
        write_local_metadata(tmp.path(), &higher).unwrap();
        write_local_metadata(tmp.path(), &lower).unwrap();

        let summaries = collect_show_summaries(
            tmp.path(),
            None,
            Some("metrics.json:accuracy"),
            ExpShowSortOrder::Asc,
            false,
        )
        .unwrap();

        assert_eq!(summaries[0].id, lower.exp_id.to_string());
        assert_eq!(summaries[1].id, higher.exp_id.to_string());
    }

    #[test]
    fn show_summaries_sort_by_message() {
        let tmp = tempfile::tempdir().unwrap();
        let make_meta = |message: &str| ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: ExperimentId::new_v7(),
            base_commit: "0".repeat(40),
            queue_commit: None,
            name: None,
            message: Some(message.to_owned()),
            status: "success".to_owned(),
            param_overrides: BTreeMap::new(),
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: Vec::new(),
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: None,
        };
        let beta = make_meta("beta");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let alpha = make_meta("alpha");
        write_local_metadata(tmp.path(), &beta).unwrap();
        write_local_metadata(tmp.path(), &alpha).unwrap();

        let summaries = collect_show_summaries(
            tmp.path(),
            None,
            Some("message"),
            ExpShowSortOrder::Asc,
            false,
        )
        .unwrap();

        assert_eq!(summaries[0].id, alpha.exp_id.to_string());
        assert_eq!(summaries[1].id, beta.exp_id.to_string());
    }

    #[test]
    fn show_summaries_hide_failed_uses_metadata_status() {
        let tmp = tempfile::tempdir().unwrap();
        let make_meta = |status: &str| ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: ExperimentId::new_v7(),
            base_commit: "0".repeat(40),
            queue_commit: None,
            name: None,
            message: None,
            status: status.to_owned(),
            param_overrides: BTreeMap::new(),
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: Vec::new(),
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: None,
        };
        let failed = make_meta("failed");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let success = make_meta("success");
        write_local_metadata(tmp.path(), &failed).unwrap();
        write_local_metadata(tmp.path(), &success).unwrap();

        let summaries =
            collect_show_summaries(tmp.path(), None, None, ExpShowSortOrder::Desc, true).unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, success.exp_id.to_string());
        assert_eq!(summaries[0].status, "success");
    }

    #[test]
    fn promote_branch_name_uses_valid_name_or_id_fallback() {
        let id = ExperimentId::new_v7();
        let mut meta = ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "0".repeat(40),
            queue_commit: None,
            name: Some("winner".to_owned()),
            message: None,
            status: "success".to_owned(),
            param_overrides: BTreeMap::new(),
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: Vec::new(),
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: None,
        };
        let args = PromoteArgs {
            id: id.to_string(),
            branch_name: None,
            branch: None,
            json: false,
        };
        assert_eq!(promote_branch_name(&args, &meta), "winner");

        meta.name = Some("bad branch name".to_owned());
        assert_eq!(
            promote_branch_name(&args, &meta),
            format!("exp-{}", &id.to_string()[..12])
        );
    }

    #[test]
    fn show_column_filters_drop_unchanged_keys_and_keep_overrides_drop() {
        let mut summaries = vec![
            ExpSummary {
                id: "a".to_owned(),
                name: Some("small-lr".to_owned()),
                message: None,
                started_at: "2024-01-01T00:00:00.000Z".to_owned(),
                base_commit: "0".repeat(40),
                status: "success".to_owned(),
                stages: 1,
                params: BTreeMap::from([
                    ("model.lr".to_owned(), "0.001".to_owned()),
                    ("model.seed".to_owned(), "42".to_owned()),
                    ("data.window".to_owned(), "30".to_owned()),
                ]),
                metrics: BTreeMap::from([
                    ("metrics.json:accuracy".to_owned(), serde_json::json!(0.9)),
                    ("metrics.json:loss".to_owned(), serde_json::json!(0.2)),
                ]),
                metrics_keys: vec!["metrics.json".to_owned()],
            },
            ExpSummary {
                id: "b".to_owned(),
                name: Some("large-lr".to_owned()),
                message: None,
                started_at: "2024-01-01T00:00:01.000Z".to_owned(),
                base_commit: "1".repeat(40),
                status: "success".to_owned(),
                stages: 1,
                params: BTreeMap::from([
                    ("model.lr".to_owned(), "0.002".to_owned()),
                    ("model.seed".to_owned(), "42".to_owned()),
                    ("data.window".to_owned(), "30".to_owned()),
                ]),
                metrics: BTreeMap::from([
                    ("metrics.json:accuracy".to_owned(), serde_json::json!(0.9)),
                    ("metrics.json:loss".to_owned(), serde_json::json!(0.1)),
                ]),
                metrics_keys: vec!["metrics.json".to_owned()],
            },
        ];
        let filters = ExpShowColumnFilters {
            only_changed: true,
            drop: vec![Regex::new("model").unwrap()],
            keep: vec![Regex::new("model\\.lr").unwrap()],
        };

        apply_show_column_filters(&mut summaries, &filters);

        for summary in &summaries {
            assert!(summary.params.contains_key("model.lr"));
            assert!(!summary.params.contains_key("model.seed"));
            assert!(!summary.params.contains_key("data.window"));
            assert!(!summary.metrics.contains_key("metrics.json:accuracy"));
            assert!(summary.metrics.contains_key("metrics.json:loss"));
        }
    }

    #[test]
    fn read_local_metadata_missing_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ExperimentId::new_v7();
        let err = read_local_metadata(tmp.path(), &id).unwrap_err();
        assert!(matches!(err, CrabError::ExperimentNotFound { .. }));
    }

    #[test]
    fn write_then_read_metadata_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ExperimentId::new_v7();
        let meta = ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "a".repeat(40),
            queue_commit: None,
            name: Some("round-trip".to_owned()),
            message: Some("tracked message".to_owned()),
            status: "success".to_owned(),
            param_overrides: BTreeMap::new(),
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: Vec::new(),
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: None,
        };
        write_local_metadata(tmp.path(), &meta).unwrap();
        let round = read_local_metadata(tmp.path(), &id).unwrap();
        assert_eq!(round, meta);
    }

    #[test]
    fn exp_remove_resolves_exact_experiment_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id_named = ExperimentId::new_v7();
        let id_other = ExperimentId::new_v7();
        let mut named = test_exp_metadata(id_named, &"a".repeat(40));
        named.name = Some("major-mela".to_owned());
        write_local_metadata(root, &named).unwrap();
        write_local_metadata(root, &test_exp_metadata(id_other, &"b".repeat(40))).unwrap();

        let payload = run_exp_remove_local(
            &RemoveArgs::try_parse_from(["remove", "major-mela"]).unwrap(),
            root,
        )
        .unwrap();

        assert_eq!(payload.removed, vec![id_named.to_string()]);
        assert_eq!(payload.kept, vec![id_other.to_string()]);
        assert!(!meta_file_path(root, &id_named).exists());
        assert!(meta_file_path(root, &id_other).exists());
    }

    #[test]
    fn exp_remove_queue_removes_pending_entries_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let pending = ExperimentId::new_v7().to_string();
        let done = ExperimentId::new_v7().to_string();
        let running = ExperimentId::new_v7().to_string();
        let queue = ExpQueue::new(crate::cmd::exp_queue::queue_dir(root));
        queue
            .enqueue(&test_queue_entry(&pending, ExpStatus::Pending))
            .unwrap();
        queue
            .enqueue(&test_queue_entry(&done, ExpStatus::Done))
            .unwrap();
        queue
            .enqueue(&test_queue_entry(&running, ExpStatus::Running))
            .unwrap();

        let payload = run_exp_remove_local(
            &RemoveArgs::try_parse_from(["remove", "--queue"]).unwrap(),
            root,
        )
        .unwrap();

        assert_eq!(payload.removed_queue, vec![pending.clone()]);
        let remaining: BTreeSet<String> = queue
            .list_all()
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(remaining, BTreeSet::from([done, running]));
    }

    #[test]
    fn exp_remove_explicit_pending_queue_id_without_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let pending = ExperimentId::new_v7().to_string();
        let queue = ExpQueue::new(crate::cmd::exp_queue::queue_dir(root));
        queue
            .enqueue(&test_queue_entry(&pending, ExpStatus::Pending))
            .unwrap();

        let payload = run_exp_remove_local(
            &RemoveArgs::try_parse_from(["remove", &pending[..12]]).unwrap(),
            root,
        )
        .unwrap();

        assert_eq!(payload.removed_queue, vec![pending]);
        assert!(queue.list_all().unwrap().is_empty());
    }

    #[test]
    fn exp_remove_rev_num_selects_first_parent_base_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_three_commit_repo(root);
        let commits = git_rev_list(root, "HEAD");
        assert_eq!(commits.len(), 3);
        let id_head = ExperimentId::new_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id_parent = ExperimentId::new_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id_oldest = ExperimentId::new_v7();
        write_local_metadata(root, &test_exp_metadata(id_head, &commits[0])).unwrap();
        write_local_metadata(root, &test_exp_metadata(id_parent, &commits[1])).unwrap();
        write_local_metadata(root, &test_exp_metadata(id_oldest, &commits[2])).unwrap();

        let payload = run_exp_remove_local(
            &RemoveArgs::try_parse_from(["remove", "--rev", "HEAD", "--num", "2", "--dry-run"])
                .unwrap(),
            root,
        )
        .unwrap();
        let removed: BTreeSet<String> = payload.removed.into_iter().collect();

        assert_eq!(
            removed,
            BTreeSet::from([id_head.to_string(), id_parent.to_string()])
        );
        assert_eq!(payload.kept, vec![id_oldest.to_string()]);
    }

    #[tokio::test]
    async fn exp_remove_remote_deletes_named_experiment_refs_and_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inner: std::sync::Arc<dyn object_store::ObjectStore> =
            std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(inner);
        let prefix = "team/project";
        let id_removed = ExperimentId::new_v7();
        let id_kept = ExperimentId::new_v7();
        let mut removed = test_exp_metadata(id_removed, &"a".repeat(40));
        removed.name = Some("urban-sign".to_owned());
        let mut kept = test_exp_metadata(id_kept, &"b".repeat(40));
        kept.name = Some("conic-ease".to_owned());
        write_remote_metadata_for_test(&store, prefix, &removed).await;
        write_remote_metadata_for_test(&store, prefix, &kept).await;
        store
            .put(
                &remote_exp_checkpoint_path(prefix, &id_removed, "train.json"),
                Bytes::from_static(b"checkpoint"),
            )
            .await
            .unwrap();

        let payload = remove_remote_experiments(
            &store,
            prefix,
            &RemoveArgs::try_parse_from(["remove", "-g", "crab://bucket/repo", "urban-sign"])
                .unwrap(),
            root,
        )
        .await
        .unwrap();

        assert_eq!(payload.removed_remote, vec![id_removed.to_string()]);
        assert_eq!(payload.kept_remote, vec![id_kept.to_string()]);
        assert!(
            !remote_object_exists(&store, &remote_exp_meta_ref_path(prefix, &id_removed))
                .await
                .unwrap()
        );
        assert!(
            !remote_object_exists(&store, &remote_exp_meta_object_path(prefix, &id_removed))
                .await
                .unwrap()
        );
        assert!(
            !remote_object_exists(
                &store,
                &remote_exp_workspace_blob_path(prefix, &id_removed, "abc")
            )
            .await
            .unwrap()
        );
        assert!(
            !remote_object_exists(
                &store,
                &remote_exp_checkpoint_path(prefix, &id_removed, "train.json")
            )
            .await
            .unwrap()
        );
        assert!(
            remote_object_exists(&store, &remote_exp_meta_ref_path(prefix, &id_kept))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn exp_remove_remote_keep_rev_num_keeps_selected_baselines() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_three_commit_repo(root);
        let commits = git_rev_list(root, "HEAD");
        assert_eq!(commits.len(), 3);
        let inner: std::sync::Arc<dyn object_store::ObjectStore> =
            std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(inner);
        let prefix = "team/project";
        let id_head = ExperimentId::new_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id_parent = ExperimentId::new_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id_oldest = ExperimentId::new_v7();
        write_remote_metadata_for_test(&store, prefix, &test_exp_metadata(id_head, &commits[0]))
            .await;
        write_remote_metadata_for_test(&store, prefix, &test_exp_metadata(id_parent, &commits[1]))
            .await;
        write_remote_metadata_for_test(&store, prefix, &test_exp_metadata(id_oldest, &commits[2]))
            .await;

        let payload = remove_remote_experiments(
            &store,
            prefix,
            &RemoveArgs::try_parse_from([
                "remove",
                "-g",
                "crab://bucket/repo",
                "--rev",
                "HEAD",
                "--num",
                "2",
                "--keep",
                "--dry-run",
            ])
            .unwrap(),
            root,
        )
        .await
        .unwrap();
        let removed: BTreeSet<String> = payload.removed_remote.into_iter().collect();
        let kept: BTreeSet<String> = payload.kept_remote.into_iter().collect();

        assert_eq!(removed, BTreeSet::from([id_oldest.to_string()]));
        assert_eq!(
            kept,
            BTreeSet::from([id_head.to_string(), id_parent.to_string()])
        );
        assert!(
            remote_object_exists(&store, &remote_exp_meta_ref_path(prefix, &id_oldest))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn push_pull_round_trips_metadata_and_apply_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        let inner: std::sync::Arc<dyn object_store::ObjectStore> =
            std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(inner);
        let prefix = "team/project";
        let id = ExperimentId::new_v7();
        let mut stages = BTreeMap::new();
        stages.insert("train".to_owned(), "ab".repeat(32));
        let meta = ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "a".repeat(40),
            queue_commit: None,
            name: Some("remote-round-trip".to_owned()),
            message: None,
            status: "success".to_owned(),
            param_overrides: BTreeMap::from([("model.lr".to_owned(), "0.007".to_owned())]),
            stages,
            metrics: BTreeMap::from([(
                "metrics.json".to_owned(),
                serde_json::json!({ "accuracy": 0.98 }),
            )]),
            cli_args: vec!["exp".to_owned(), "run".to_owned()],
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: Some("2024-01-01T00:00:01.000Z".into()),
        };
        write_local_metadata(repo_root, &meta).unwrap();

        let snapshot_dir = workspace_dir_path(repo_root, &id);
        fs::create_dir_all(snapshot_dir.join("nested")).unwrap();
        fs::write(snapshot_dir.join("params.yaml"), "model:\n  lr: 0.007\n").unwrap();
        fs::write(snapshot_dir.join("nested/out.txt"), "score=0.98\n").unwrap();
        let local_manifest = ExpWorkspaceManifest {
            deleted: vec![PathBuf::from("obsolete.txt")],
        };
        fs::write(
            workspace_manifest_path(repo_root, &id),
            serde_json::to_vec(&local_manifest).unwrap(),
        )
        .unwrap();

        let pushed = push_experiments_to_remote(&store, prefix, repo_root, &[id], false)
            .await
            .unwrap();
        assert_eq!(pushed.pushed, vec![id.to_string()]);
        assert!(pushed.skipped.is_empty());

        let resolved = resolve_remote_experiment_ids(
            &store,
            prefix,
            &[id.to_string()[..12].to_owned()],
            false,
        )
        .await
        .unwrap();
        assert_eq!(resolved, vec![id]);

        // An older remote may have the experiment ref but no workflow root
        // registration yet. A no-op push must repair that protection before
        // reporting the experiment as skipped.
        store
            .delete(&ObjectPath::from(".crab/ref-registry"))
            .await
            .unwrap();
        let skipped = push_experiments_to_remote(&store, prefix, repo_root, &[id], false)
            .await
            .unwrap();
        assert!(skipped.pushed.is_empty());
        assert_eq!(skipped.skipped, vec![id.to_string()]);

        remove_experiment_files(repo_root, &id).unwrap();
        fs::write(repo_root.join("obsolete.txt"), "stale local file\n").unwrap();

        let remote = ExperimentRemote {
            store: store.clone(),
            prefix: prefix.to_owned(),
            primary_fallback: None,
        };
        let pulled = pull_experiments_from_remote(&remote, repo_root, &[id], false)
            .await
            .unwrap();
        assert_eq!(pulled.pulled, vec![id.to_string()]);
        assert!(pulled.skipped.is_empty());
        assert_eq!(read_local_metadata(repo_root, &id).unwrap(), meta);

        let apply = run_exp_apply(
            &ApplyArgs {
                id: id.to_string(),
                checkpoint: None,
                json: false,
            },
            repo_root,
        )
        .unwrap();
        assert!(
            apply
                .applied
                .iter()
                .any(|path| path == Path::new("params.yaml")),
        );
        assert!(
            apply
                .deleted
                .iter()
                .any(|path| path == Path::new("obsolete.txt")),
        );
        assert_eq!(
            fs::read_to_string(repo_root.join("params.yaml")).unwrap(),
            "model:\n  lr: 0.007\n",
        );
        assert_eq!(
            fs::read_to_string(repo_root.join("nested/out.txt")).unwrap(),
            "score=0.98\n",
        );
        assert!(!repo_root.join("obsolete.txt").exists());

        let (stage_refs, _) = store
            .get_with_etag(&remote_exp_stage_refs_object_path(prefix, &id))
            .await
            .unwrap();
        let refs: Vec<String> = serde_json::from_slice(&stage_refs).unwrap();
        assert_eq!(refs, vec!["ab".repeat(32)]);
        let (registry_bytes, _) = store
            .get_with_etag(&ObjectPath::from(".crab/ref-registry"))
            .await
            .unwrap();
        let registry: crab_metadata::ref_registry::RefRegistry =
            serde_json::from_slice(&registry_bytes).unwrap();
        assert_eq!(
            registry.workflow_experiment_ids[prefix],
            vec![id.to_string()]
        );
        assert_eq!(
            registry.workflow_stage_hashes[prefix],
            vec!["ab".repeat(32)]
        );
    }

    #[tokio::test]
    async fn push_pull_round_trips_checkpoint_lineage_and_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        let inner: std::sync::Arc<dyn object_store::ObjectStore> =
            std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(inner);
        let prefix = "team/checkpoints";
        let id = ExperimentId::new_v7();
        let metadata = ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "c".repeat(40),
            queue_commit: None,
            name: Some("checkpoint-transport".to_owned()),
            message: None,
            status: "failed".to_owned(),
            param_overrides: BTreeMap::new(),
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: vec!["exp".to_owned(), "run".to_owned()],
            host_fingerprint: "test".to_owned(),
            started_at: "2024-01-01T00:00:00.000Z".to_owned(),
            ended_at: None,
        };
        write_local_metadata(repo_root, &metadata).unwrap();
        fs::create_dir_all(workspace_dir_path(repo_root, &id)).unwrap();
        fs::write(
            workspace_dir_path(repo_root, &id).join("model.bin"),
            b"checkpoint",
        )
        .unwrap();
        fs::write(
            workspace_manifest_path(repo_root, &id),
            serde_json::to_vec(&ExpWorkspaceManifest {
                deleted: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let payload_hash = format!("b3:{}", blake3::hash(b"checkpoint").to_hex());
        let state_root = checkpoint_state_dir(repo_root, &id);
        let payload = state_root
            .join("objects")
            .join(payload_hash.strip_prefix("b3:").unwrap())
            .join("payload");
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        fs::write(&payload, b"checkpoint").unwrap();
        let mut lineage = CheckpointLineage::default();
        lineage
            .append(CheckpointRecord {
                schema_version: CHECKPOINT_SCHEMA_VERSION,
                id: "checkpoint-0".to_owned(),
                experiment: id.to_string(),
                stage: "train".to_owned(),
                sequence: 0,
                parent: None,
                request_nonce: Some("01".repeat(32)),
                stage_hash: format!("b3:{}", "aa".repeat(32)),
                created_at_unix_ms: 0,
                outputs: BTreeMap::from([("model.bin".to_owned(), payload_hash.clone())]),
                metrics: BTreeMap::new(),
                terminal: false,
                resumable: true,
            })
            .unwrap();
        lineage.save_atomic(&state_root.join("train.json")).unwrap();

        push_experiments_to_remote(&store, prefix, repo_root, &[id], false)
            .await
            .unwrap();
        remove_experiment_files(repo_root, &id).unwrap();

        let remote = ExperimentRemote {
            store,
            prefix: prefix.to_owned(),
            primary_fallback: None,
        };
        pull_experiments_from_remote(&remote, repo_root, &[id], false)
            .await
            .unwrap();
        let restored =
            CheckpointLineage::load(&checkpoint_state_dir(repo_root, &id).join("train.json"))
                .unwrap();
        assert_eq!(restored.records.len(), 1);
        assert_eq!(restored.records[0].outputs["model.bin"], payload_hash);
        assert_eq!(
            fs::read(
                checkpoint_state_dir(repo_root, &id)
                    .join("objects")
                    .join(payload_hash.strip_prefix("b3:").unwrap())
                    .join("payload"),
            )
            .unwrap(),
            b"checkpoint"
        );
    }

    #[tokio::test]
    async fn exp_pull_uses_primary_fallback_after_selected_replica_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        let primary_inner: std::sync::Arc<dyn object_store::ObjectStore> =
            std::sync::Arc::new(object_store::memory::InMemory::new());
        let replica_inner: std::sync::Arc<dyn object_store::ObjectStore> =
            std::sync::Arc::new(object_store::memory::InMemory::new());
        let primary = Store::new(primary_inner);
        let replica = Store::new(replica_inner);
        let prefix = "team/project";
        let id = ExperimentId::new_v7();
        let meta = ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "b".repeat(40),
            queue_commit: None,
            name: Some("replica-fallback".to_owned()),
            message: None,
            status: "saved".to_owned(),
            param_overrides: BTreeMap::new(),
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: vec!["exp".to_owned(), "save".to_owned()],
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: None,
        };
        write_local_metadata(repo_root, &meta).unwrap();

        let snapshot_dir = workspace_dir_path(repo_root, &id);
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(snapshot_dir.join("result.txt"), "from primary\n").unwrap();
        let local_manifest = ExpWorkspaceManifest {
            deleted: Vec::new(),
        };
        fs::write(
            workspace_manifest_path(repo_root, &id),
            serde_json::to_vec(&local_manifest).unwrap(),
        )
        .unwrap();

        push_experiments_to_remote(&primary, prefix, repo_root, &[id], false)
            .await
            .unwrap();
        remove_experiment_files(repo_root, &id).unwrap();

        let remote = ExperimentRemote {
            store: replica,
            prefix: prefix.to_owned(),
            primary_fallback: Some(ExperimentPrimaryFallback {
                store: primary,
                prefix: prefix.to_owned(),
            }),
        };
        let resolved = resolve_remote_experiment_ids_from_remote(
            &remote,
            &[id.to_string()[..12].to_owned()],
            false,
        )
        .await
        .unwrap();
        assert_eq!(resolved, vec![id]);

        let pulled = pull_experiments_from_remote(&remote, repo_root, &[id], false)
            .await
            .unwrap();
        assert_eq!(pulled.pulled, vec![id.to_string()]);
        assert_eq!(read_local_metadata(repo_root, &id).unwrap(), meta);
        assert_eq!(
            fs::read_to_string(workspace_dir_path(repo_root, &id).join("result.txt")).unwrap(),
            "from primary\n"
        );
    }

    #[test]
    fn resolve_experiment_id_rejects_ambiguous_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let id_a = ExperimentId::new_v7();
        let id_b = ExperimentId::new_v7();
        let make_meta = |id: ExperimentId| ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "0".repeat(40),
            queue_commit: None,
            name: None,
            message: None,
            status: "success".to_owned(),
            param_overrides: BTreeMap::new(),
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: Vec::new(),
            host_fingerprint: "test".into(),
            started_at: "2024-01-01T00:00:00.000Z".into(),
            ended_at: None,
        };
        write_local_metadata(tmp.path(), &make_meta(id_a)).unwrap();
        write_local_metadata(tmp.path(), &make_meta(id_b)).unwrap();

        let a = id_a.to_string();
        let b = id_b.to_string();
        let prefix_len = a
            .bytes()
            .zip(b.bytes())
            .take_while(|(left, right)| left == right)
            .count();
        assert!(prefix_len > 0, "UUIDv7 ids should share a timestamp prefix");
        let err = resolve_experiment_id(tmp.path(), &a[..prefix_len]).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn summaries_sort_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
            let id = ExperimentId::new_v7();
            let meta = ExperimentMetadata {
                schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
                exp_id: id,
                base_commit: "0".repeat(40),
                queue_commit: None,
                name: None,
                message: None,
                status: "success".to_owned(),
                param_overrides: BTreeMap::new(),
                stages: BTreeMap::new(),
                metrics: BTreeMap::new(),
                cli_args: Vec::new(),
                host_fingerprint: "test".into(),
                started_at: "2024-01-01T00:00:00.000Z".into(),
                ended_at: None,
            };
            write_local_metadata(tmp.path(), &meta).unwrap();
            ids.push(id);
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let summaries = collect_summaries(tmp.path()).unwrap();
        assert_eq!(summaries.len(), 3);
        // Newest first: last-created id is summaries[0].
        assert_eq!(summaries[0].id, ids[2].to_string());
        assert_eq!(summaries[1].id, ids[1].to_string());
        assert_eq!(summaries[2].id, ids[0].to_string());
    }

    #[test]
    fn summaries_absent_dir_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let summaries = collect_summaries(tmp.path()).unwrap();
        assert!(summaries.is_empty());
    }

    /// Under a skewed wall clock, two experiments can end up with
    /// a newer UUIDv7 but an older `started_at` (or vice versa).
    /// `collect_summaries` must sort by the embedded UUIDv7
    /// timestamp so `exp ls` / `exp show` surface experiments in
    /// true creation order regardless of the host clock. Inverts
    /// the `started_at` field relative to UUID order and asserts
    /// the newer UUID still wins.
    #[test]
    fn summaries_sort_by_uuid_even_when_started_at_disagrees() {
        let tmp = tempfile::tempdir().unwrap();

        // `id_older` is minted first, so its UUIDv7-embedded
        // timestamp is strictly smaller than `id_newer`'s.
        let id_older = ExperimentId::new_v7();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let id_newer = ExperimentId::new_v7();
        assert!(
            id_older.to_string() < id_newer.to_string(),
            "UUIDv7 lex order must follow creation order",
        );

        // Invert the wall-clock timestamps: the older UUID carries
        // a future `started_at`, the newer UUID carries a past
        // one. A sort by `started_at` would rank id_older first;
        // a sort by UUID ranks id_newer first.
        let make = |id: ExperimentId, started_at: &str| ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "0".repeat(40),
            queue_commit: None,
            name: None,
            message: None,
            status: "success".to_owned(),
            param_overrides: BTreeMap::new(),
            stages: BTreeMap::new(),
            metrics: BTreeMap::new(),
            cli_args: Vec::new(),
            host_fingerprint: "test".into(),
            started_at: started_at.to_owned(),
            ended_at: None,
        };

        let meta_older = make(id_older, "2099-12-31T23:59:59.000Z");
        let meta_newer = make(id_newer, "2020-01-01T00:00:00.000Z");
        write_local_metadata(tmp.path(), &meta_older).unwrap();
        write_local_metadata(tmp.path(), &meta_newer).unwrap();

        let summaries = collect_summaries(tmp.path()).unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(
            summaries[0].id,
            id_newer.to_string(),
            "newest UUID must sort first regardless of started_at",
        );
        assert_eq!(summaries[1].id, id_older.to_string());
        // Sanity: the wall-clock timestamps are indeed inverted
        // relative to the UUID order — if a future refactor ever
        // swaps the sort key to `started_at`, this test will
        // start failing with summaries[0].started_at being the
        // 2099 value.
        assert_eq!(summaries[0].started_at, "2020-01-01T00:00:00.000Z");
        assert_eq!(summaries[1].started_at, "2099-12-31T23:59:59.000Z");
    }
}
