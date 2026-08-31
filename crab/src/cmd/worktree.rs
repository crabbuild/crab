//! Native `crab worktree` command.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{ArgAction, Args, Subcommand, ValueEnum};
use schemars::JsonSchema;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::cmd::hydrate::HydrateArgs;
use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::git::worktree::{
    GitWorktreeRecord, WorktreeContext, linked_identity_map_from_current_repo,
    normalize_identity_path, parse_worktree_list_porcelain, worktree_identity_for_path,
};
use crate::git::worktree_hydration::{
    HYDRATION_POLICY_FILENAME, WorktreeHydrationMode as ResolvedHydrationMode,
    WorktreeHydrationPolicyFile, WorktreeHydrationPolicySource as HydrationPolicySource,
    WorktreeHydrationPolicyStatus as HydrationPolicyStatus,
    WorktreeHydrationSelector as HydrationSelector,
};

pub const WORKTREE_ADD_SCHEMA: &str = "worktree.add";
pub const WORKTREE_LIST_SCHEMA: &str = "worktree.list";
pub const WORKTREE_LOCK_SCHEMA: &str = "worktree.lock";
pub const WORKTREE_MOVE_SCHEMA: &str = "worktree.move";
pub const WORKTREE_PRUNE_SCHEMA: &str = "worktree.prune";
pub const WORKTREE_REMOVE_SCHEMA: &str = "worktree.remove";
pub const WORKTREE_REPAIR_SCHEMA: &str = "worktree.repair";
pub const WORKTREE_UNLOCK_SCHEMA: &str = "worktree.unlock";

#[derive(Debug, Clone, Subcommand)]
pub enum WorktreeCommand {
    /// Create a linked worktree.
    Add(AddArgs),
    /// List worktrees.
    List(ListArgs),
    /// Lock a worktree.
    Lock(LockArgs),
    /// Move a worktree.
    Move(MoveArgs),
    /// Prune stale worktree metadata.
    Prune(PruneArgs),
    /// Remove a worktree.
    Remove(RemoveArgs),
    /// Repair worktree administrative links.
    Repair(RepairArgs),
    /// Unlock a worktree.
    Unlock(UnlockArgs),
}

#[derive(Debug, Clone, Args)]
pub struct AddArgs {
    /// Checkout a branch even if already checked out in another worktree.
    #[arg(short = 'f', long, action = ArgAction::Count)]
    pub force: u8,
    /// Create a new branch.
    #[arg(
        short = 'b',
        value_name = "NEW_BRANCH",
        conflicts_with = "branch_reset"
    )]
    pub branch: Option<String>,
    /// Create or reset a branch.
    #[arg(short = 'B', value_name = "NEW_BRANCH", conflicts_with = "branch")]
    pub branch_reset: Option<String>,
    /// Detach HEAD at the named commit.
    #[arg(short = 'd', long)]
    pub detach: bool,
    /// Populate the new working tree.
    #[arg(long, conflicts_with = "no_checkout")]
    pub checkout: bool,
    /// Do not populate the new working tree.
    #[arg(long = "no-checkout", conflicts_with = "checkout")]
    pub no_checkout: bool,
    /// Keep the new worktree locked after creation.
    #[arg(long)]
    pub lock: bool,
    /// Reason for locking.
    #[arg(long, value_name = "STRING")]
    pub reason: Option<String>,
    /// Suppress progress reporting.
    #[arg(short = 'q', long)]
    pub quiet: bool,
    /// Set up tracking mode.
    #[arg(long)]
    pub track: bool,
    /// Try to match the new branch name with a remote-tracking branch.
    #[arg(long = "guess-remote")]
    pub guess_remote: bool,
    /// Create a new orphan branch, when supported by installed Git.
    #[arg(long)]
    pub orphan: bool,
    /// Use relative worktree links, when supported by installed Git.
    #[arg(long = "relative-paths")]
    pub relative_paths: bool,
    /// Use absolute worktree links, when supported by installed Git.
    #[arg(long = "no-relative-paths")]
    pub no_relative_paths: bool,
    /// Crab hydration policy for the new worktree.
    #[arg(long, value_enum, value_name = "POLICY")]
    pub hydrate: Option<WorktreeHydrationArg>,
    /// Crab hydrate include patterns for selective worktree hydration.
    #[arg(long = "hydrate-include", value_name = "PATTERN")]
    pub hydrate_include: Vec<String>,
    /// Crab hydrate exclude patterns for selective worktree hydration.
    #[arg(long = "hydrate-exclude", value_name = "PATTERN")]
    pub hydrate_exclude: Vec<String>,
    /// Newline-delimited Crab hydrate manifest for selective hydration.
    #[arg(
        long = "hydrate-manifest",
        value_name = "PATH",
        conflicts_with_all = ["hydrate_manifest_ref", "hydrate_profile", "hydrate_include"]
    )]
    pub hydrate_manifest: Option<String>,
    /// Git ref containing a Crab hydrate manifest.
    #[arg(
        long = "hydrate-manifest-ref",
        value_name = "REF",
        conflicts_with_all = ["hydrate_manifest", "hydrate_profile", "hydrate_include"]
    )]
    pub hydrate_manifest_ref: Option<String>,
    /// Named profile from `crab.toml` for selective hydration.
    #[arg(
        long = "hydrate-profile",
        value_name = "NAME",
        conflicts_with_all = ["hydrate_manifest", "hydrate_manifest_ref", "hydrate_include"]
    )]
    pub hydrate_profile: Option<String>,
    /// Warm selected content without claiming it is materialized.
    #[arg(long)]
    pub prefetch: bool,
    /// Path where Git should create the worktree.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
    /// Commit-ish to check out.
    #[arg(value_name = "COMMIT_ISH")]
    pub commit_ish: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WorktreeHydrationArg {
    Lazy,
    PointerOnly,
    Full,
}

#[derive(Debug, Clone, Args)]
pub struct ListArgs {
    /// Emit Git-compatible porcelain output.
    #[arg(long, conflicts_with = "json")]
    pub porcelain: bool,
    /// Terminate porcelain records with NUL bytes.
    #[arg(short = 'z', requires = "porcelain", conflicts_with = "json")]
    pub zero: bool,
    /// Include extra Git information in text output.
    #[arg(short = 'v', long, conflicts_with_all = ["porcelain", "json"])]
    pub verbose: bool,
    /// Emit Crab JSON output.
    #[arg(long, conflicts_with_all = ["porcelain", "zero", "verbose"])]
    pub json: bool,
    /// Include slower Crab hydration/cache summaries in JSON output.
    #[arg(long, requires = "json")]
    pub with_crab_state: bool,
}

#[derive(Debug, Clone, Args)]
pub struct LockArgs {
    /// Reason for locking.
    #[arg(long, value_name = "STRING")]
    pub reason: Option<String>,
    /// Worktree to lock.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct MoveArgs {
    /// Force move even if the worktree is dirty or locked.
    #[arg(short = 'f', long, action = ArgAction::Count)]
    pub force: u8,
    /// Existing worktree path.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,
    /// New worktree path.
    #[arg(value_name = "NEW_PATH")]
    pub new_path: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct PruneArgs {
    /// Do not remove, show only.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    /// Report pruned working trees.
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Expire working trees older than this time.
    #[arg(long, value_name = "EXPIRE")]
    pub expire: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct RemoveArgs {
    /// Force removal even if the worktree is dirty or locked.
    #[arg(short = 'f', long, action = ArgAction::Count)]
    pub force: u8,
    /// Worktree to remove.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct RepairArgs {
    /// Use relative worktree links, when supported by installed Git.
    #[arg(long = "relative-paths")]
    pub relative_paths: bool,
    /// Use absolute worktree links, when supported by installed Git.
    #[arg(long = "no-relative-paths")]
    pub no_relative_paths: bool,
    /// Worktree paths to repair.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct UnlockArgs {
    /// Worktree to unlock.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorktreeListPayload {
    pub git_version: Option<String>,
    pub worktrees: Vec<WorktreeListEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorktreeListEntry {
    #[serde(flatten)]
    pub git: GitWorktreeRecord,
    pub crab: CrabWorktreeIdentity,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrabWorktreeIdentity {
    pub identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<CrabWorktreeStateSummary>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrabWorktreeStateSummary {
    pub state_dir: String,
    pub state_dir_exists: bool,
    pub hydration_policy: CrabStateFileSummary,
    pub hydrated_pointer_cache: CrabHydratedPointerCacheSummary,
    pub pointer_summary: CrabPointerSummary,
    pub access_db: CrabStateFileSummary,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrabStateFileSummary {
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrabHydratedPointerCacheSummary {
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub entries: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrabPointerSummary {
    pub hydrated_pointer_entries: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorktreeAddPayload {
    pub created: bool,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeListEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorktreePathPayload {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeListEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorktreeMovePayload {
    pub old_path: String,
    pub new_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeListEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorktreePrunePayload {
    pub dry_run: bool,
    pub pruned_lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorktreeRepairPayload {
    pub repaired: bool,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone)]
struct WorktreeStateLocation {
    identity: String,
    record_path: String,
    state_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StateCleanupOutcome {
    Deleted {
        identity: String,
    },
    Missing {
        identity: String,
    },
    Locked {
        identity: String,
        state_dir: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedHydrationPolicy {
    source: HydrationPolicySource,
    mode: ResolvedHydrationMode,
    selector: HydrationSelector,
    prefetch: bool,
    status: HydrationPolicyStatus,
    checkout_suppressed: bool,
}

pub async fn run(command: WorktreeCommand, cancel: &CancellationToken, json: bool) -> Result<()> {
    match command {
        WorktreeCommand::Add(args) => run_add(&args, cancel, json).await,
        WorktreeCommand::List(args) => run_list(&args, json),
        WorktreeCommand::Lock(args) => run_lock(&args, json),
        WorktreeCommand::Move(args) => run_move(&args, json),
        WorktreeCommand::Prune(args) => run_prune(&args, json),
        WorktreeCommand::Remove(args) => run_remove(&args, json),
        WorktreeCommand::Repair(args) => run_repair(&args, json),
        WorktreeCommand::Unlock(args) => run_unlock(&args, json),
    }
}

pub async fn run_add(args: &AddArgs, cancel: &CancellationToken, json: bool) -> Result<()> {
    reject_unsupported_version_gated_options(
        "add",
        &[
            ("--orphan", args.orphan),
            ("--relative-paths", args.relative_paths),
            ("--no-relative-paths", args.no_relative_paths),
        ],
    )?;
    let hydration_policy = resolve_add_hydration_policy(args)?;
    validate_add_hydration_policy(hydration_policy.as_ref())?;

    let mut git_args = vec![OsString::from("worktree"), OsString::from("add")];
    for _ in 0..args.force {
        git_args.push(OsString::from("--force"));
    }
    if let Some(branch) = &args.branch {
        git_args.push(OsString::from("-b"));
        git_args.push(OsString::from(branch));
    }
    if let Some(branch) = &args.branch_reset {
        git_args.push(OsString::from("-B"));
        git_args.push(OsString::from(branch));
    }
    if args.detach {
        git_args.push(OsString::from("--detach"));
    }
    if args.checkout {
        git_args.push(OsString::from("--checkout"));
    }
    if args.no_checkout {
        git_args.push(OsString::from("--no-checkout"));
    }
    if args.lock {
        git_args.push(OsString::from("--lock"));
    }
    if let Some(reason) = &args.reason {
        git_args.push(OsString::from("--reason"));
        git_args.push(OsString::from(reason));
    }
    if args.quiet {
        git_args.push(OsString::from("--quiet"));
    }
    if args.track {
        git_args.push(OsString::from("--track"));
    }
    if args.guess_remote {
        git_args.push(OsString::from("--guess-remote"));
    }
    if args.orphan {
        git_args.push(OsString::from("--orphan"));
    }
    if args.relative_paths {
        git_args.push(OsString::from("--relative-paths"));
    }
    if args.no_relative_paths {
        git_args.push(OsString::from("--no-relative-paths"));
    }
    git_args.push(args.path.as_os_str().to_owned());
    if let Some(commit_ish) = &args.commit_ish {
        git_args.push(OsString::from(commit_ish));
    }

    if hydration_policy
        .as_ref()
        .is_some_and(|policy| should_disable_filters_for_add(policy))
    {
        let git_args = with_crab_filters_disabled(&git_args);
        passthrough_git_for_mode(&git_args, !json)?;
    } else {
        passthrough_git_for_mode(&git_args, !json)?;
    }

    let created_path = created_worktree_path(&args.path)?;
    if let Some(policy) = hydration_policy {
        let ctx = WorktreeContext::resolve_from_path(&created_path)?;
        write_hydration_policy(&ctx, &policy)?;
        report_add_hydration_policy(&ctx, &policy);
        if policy.prefetch && !should_run_post_create_hydration(&policy) {
            if let Err(err) = run_post_create_prefetch(&ctx, &policy, cancel).await {
                let mut retry_policy = policy.clone();
                retry_policy.status = HydrationPolicyStatus::Pending;
                let _ = write_hydration_policy(&ctx, &retry_policy);
                return Err(err);
            }
        }
        if should_run_post_create_hydration(&policy) {
            run_post_create_hydration(&ctx, json)?;
        }
    }

    if json {
        emit_json(
            WORKTREE_ADD_SCHEMA,
            "1.0",
            WorktreeAddPayload {
                created: true,
                path: created_path.to_string_lossy().into_owned(),
                worktree: list_entry_for_path(&created_path)?,
            },
        );
    }

    Ok(())
}

fn resolve_add_hydration_policy(args: &AddArgs) -> Result<Option<ResolvedHydrationPolicy>> {
    let explicit_selector = hydration_selector_from_args(args)?;
    let has_crab_policy = args.hydrate.is_some() || explicit_selector.is_some() || args.prefetch;

    if !has_crab_policy {
        let policy = clone_default_hydration_policy(args.no_checkout, false)?;
        let policy = finalize_add_hydration_policy(policy);
        if policy.status == HydrationPolicyStatus::Pending
            || clone_default_policy_has_effect(&policy)
        {
            return Ok(Some(policy));
        }
        return Ok(None);
    }

    let checkout_suppressed = args.no_checkout;

    if args.prefetch && args.hydrate.is_none() && explicit_selector.is_none() {
        let defaults = clone_default_hydration_policy(checkout_suppressed, true)?;
        let policy = ResolvedHydrationPolicy {
            source: HydrationPolicySource::CloneDefaults,
            mode: ResolvedHydrationMode::Lazy,
            selector: defaults.selector,
            prefetch: true,
            status: HydrationPolicyStatus::Applied,
            checkout_suppressed,
        };
        return Ok(Some(finalize_add_hydration_policy(policy)));
    }

    if args.prefetch
        && args.hydrate.is_none()
        && let Some(selector) = explicit_selector
    {
        return Ok(Some(finalize_add_hydration_policy(
            ResolvedHydrationPolicy {
                source: HydrationPolicySource::Explicit,
                mode: ResolvedHydrationMode::Lazy,
                selector,
                prefetch: true,
                status: HydrationPolicyStatus::Applied,
                checkout_suppressed,
            },
        )));
    }

    if args.hydrate == Some(WorktreeHydrationArg::Full) && explicit_selector.is_some() {
        return Err(CrabError::Protocol(
            "crab worktree add: --hydrate=full cannot be combined with selective hydrate selectors; omit --hydrate=full for manifest, profile, or pattern hydration"
                .to_owned(),
        ));
    }

    let (mode, selector) = match (args.hydrate, explicit_selector) {
        (Some(WorktreeHydrationArg::Lazy), selector) => (
            ResolvedHydrationMode::Lazy,
            selector.unwrap_or(HydrationSelector::CloneDefaults),
        ),
        (Some(WorktreeHydrationArg::PointerOnly), selector) => (
            ResolvedHydrationMode::PointerOnly,
            selector.unwrap_or(HydrationSelector::CloneDefaults),
        ),
        (Some(WorktreeHydrationArg::Full), None) => {
            (ResolvedHydrationMode::Full, HydrationSelector::All)
        }
        (Some(WorktreeHydrationArg::Full), Some(_)) => {
            return Err(CrabError::Protocol(
                "crab worktree add: --hydrate=full cannot be combined with selective hydrate selectors; omit --hydrate=full for manifest, profile, or pattern hydration"
                    .to_owned(),
            ));
        }
        (None, Some(selector)) => (ResolvedHydrationMode::Selective, selector),
        (None, None) => (
            ResolvedHydrationMode::Lazy,
            HydrationSelector::CloneDefaults,
        ),
    };

    Ok(Some(finalize_add_hydration_policy(
        ResolvedHydrationPolicy {
            source: HydrationPolicySource::Explicit,
            mode,
            selector,
            prefetch: args.prefetch,
            status: HydrationPolicyStatus::Applied,
            checkout_suppressed,
        },
    )))
}

fn hydration_selector_from_args(args: &AddArgs) -> Result<Option<HydrationSelector>> {
    if let Some(path) = &args.hydrate_manifest {
        return Ok(Some(HydrationSelector::Manifest {
            path: path.clone(),
            exclude: args.hydrate_exclude.clone(),
        }));
    }
    if let Some(spec) = &args.hydrate_manifest_ref {
        return Ok(Some(HydrationSelector::ManifestRef {
            spec: spec.clone(),
            exclude: args.hydrate_exclude.clone(),
        }));
    }
    if let Some(name) = &args.hydrate_profile {
        return Ok(Some(HydrationSelector::Profile {
            name: name.clone(),
            exclude: args.hydrate_exclude.clone(),
        }));
    }
    if !args.hydrate_include.is_empty() {
        return Ok(Some(HydrationSelector::Patterns {
            include: args.hydrate_include.clone(),
            exclude: args.hydrate_exclude.clone(),
        }));
    }
    if !args.hydrate_exclude.is_empty() {
        return Err(CrabError::Protocol(
            "crab worktree add: --hydrate-exclude requires --hydrate-include, --hydrate-manifest, --hydrate-manifest-ref, or --hydrate-profile"
                .to_owned(),
        ));
    }
    Ok(None)
}

fn clone_default_hydration_policy(
    checkout_suppressed: bool,
    prefetch: bool,
) -> Result<ResolvedHydrationPolicy> {
    let project_config = match std::env::current_dir() {
        Ok(cwd) => crate::core::project_config::ProjectConfig::load_for_repo(&cwd)?,
        Err(_) => None,
    };
    let (mode, selector) = project_config.and_then(|config| config.hydrate).map_or(
        (
            ResolvedHydrationMode::Lazy,
            HydrationSelector::CloneDefaults,
        ),
        |hydrate| {
            if let Some(patterns) = hydrate.auto_patterns
                && !patterns.is_empty()
            {
                return (
                    ResolvedHydrationMode::Selective,
                    HydrationSelector::Patterns {
                        include: patterns,
                        exclude: Vec::new(),
                    },
                );
            }
            match hydrate.default {
                crate::core::project_config::HydrateMode::Eager => {
                    (ResolvedHydrationMode::Full, HydrationSelector::All)
                }
                crate::core::project_config::HydrateMode::Lazy => (
                    ResolvedHydrationMode::Lazy,
                    HydrationSelector::CloneDefaults,
                ),
            }
        },
    );

    Ok(ResolvedHydrationPolicy {
        source: HydrationPolicySource::CloneDefaults,
        mode,
        selector,
        prefetch,
        status: if checkout_suppressed {
            HydrationPolicyStatus::Pending
        } else {
            HydrationPolicyStatus::Applied
        },
        checkout_suppressed,
    })
}

fn validate_add_hydration_policy(policy: Option<&ResolvedHydrationPolicy>) -> Result<()> {
    let Some(policy) = policy else {
        return Ok(());
    };

    if policy.prefetch && !hydration_selector_is_bounded(&policy.selector) {
        return Err(CrabError::Protocol(
            "crab worktree add --prefetch requires a bounded selection: use --hydrate=full, --hydrate-include, --hydrate-manifest, --hydrate-manifest-ref, --hydrate-profile, or project clone defaults with eager hydration or auto patterns"
                .to_owned(),
        ));
    }

    Ok(())
}

fn clone_default_policy_has_effect(policy: &ResolvedHydrationPolicy) -> bool {
    matches!(
        policy.mode,
        ResolvedHydrationMode::Full | ResolvedHydrationMode::Selective
    )
}

fn finalize_add_hydration_policy(mut policy: ResolvedHydrationPolicy) -> ResolvedHydrationPolicy {
    policy.status = if should_run_post_create_hydration(&policy)
        || (policy.checkout_suppressed && has_actionable_hydration(&policy))
    {
        HydrationPolicyStatus::Pending
    } else {
        HydrationPolicyStatus::Applied
    };
    policy
}

fn has_actionable_hydration(policy: &ResolvedHydrationPolicy) -> bool {
    hydrate_args_for_policy(policy).is_some()
}

fn should_run_post_create_hydration(policy: &ResolvedHydrationPolicy) -> bool {
    !policy.checkout_suppressed
        && matches!(
            policy.mode,
            ResolvedHydrationMode::Full | ResolvedHydrationMode::Selective
        )
}

fn hydration_selector_is_bounded(selector: &HydrationSelector) -> bool {
    match selector {
        HydrationSelector::CloneDefaults => false,
        HydrationSelector::All
        | HydrationSelector::Manifest { .. }
        | HydrationSelector::ManifestRef { .. }
        | HydrationSelector::Profile { .. } => true,
        HydrationSelector::Patterns { include, .. } => !include.is_empty(),
    }
}

fn should_disable_filters_for_add(policy: &ResolvedHydrationPolicy) -> bool {
    policy.status == HydrationPolicyStatus::Applied
        && matches!(
            policy.mode,
            ResolvedHydrationMode::Lazy | ResolvedHydrationMode::PointerOnly
        )
}

fn write_hydration_policy(ctx: &WorktreeContext, policy: &ResolvedHydrationPolicy) -> Result<()> {
    WorktreeHydrationPolicyFile {
        version: 1,
        source: policy.source,
        status: policy.status,
        mode: policy.mode,
        checkout_suppressed: policy.checkout_suppressed,
        prefetch: policy.prefetch,
        selector: policy.selector.clone(),
    }
    .write_for_context(ctx)
}

fn report_add_hydration_policy(ctx: &WorktreeContext, policy: &ResolvedHydrationPolicy) {
    match policy.status {
        HydrationPolicyStatus::Pending if policy.checkout_suppressed => {
            eprintln!(
                "crab worktree: hydration deferred because checkout was suppressed; run `crab hydrate` from {} to materialize selected Crab pointer files",
                ctx.current_worktree_root.display()
            );
        }
        HydrationPolicyStatus::Pending => {
            eprintln!(
                "crab worktree: hydrating selected Crab pointer files in {}",
                ctx.current_worktree_root.display()
            );
        }
        HydrationPolicyStatus::Applied
            if policy.checkout_suppressed
                && matches!(
                    policy.mode,
                    ResolvedHydrationMode::Lazy | ResolvedHydrationMode::PointerOnly
                ) =>
        {
            eprintln!(
                "crab worktree: checkout suppressed; {} policy recorded with no pending hydration",
                hydration_mode_label(policy.mode)
            );
        }
        HydrationPolicyStatus::Applied
            if matches!(
                policy.mode,
                ResolvedHydrationMode::Lazy | ResolvedHydrationMode::PointerOnly
            ) =>
        {
            eprintln!(
                "crab worktree: created pointer-only worktree; run `crab hydrate` from {} to materialize selected Crab pointer files",
                ctx.current_worktree_root.display()
            );
        }
        HydrationPolicyStatus::Applied => {}
    }
}

fn hydration_mode_label(mode: ResolvedHydrationMode) -> &'static str {
    match mode {
        ResolvedHydrationMode::Lazy => "lazy",
        ResolvedHydrationMode::PointerOnly => "pointer-only",
        ResolvedHydrationMode::Full => "full",
        ResolvedHydrationMode::Selective => "selective",
    }
}

fn run_post_create_hydration(ctx: &WorktreeContext, suppress_stdout_on_error: bool) -> Result<()> {
    let exe = std::env::current_exe().map_err(CrabError::Io)?;
    let output = Command::new(exe)
        .arg("hydrate")
        .arg("--json")
        .current_dir(&ctx.current_worktree_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(CrabError::Io)?;
    std::io::stderr()
        .write_all(&output.stderr)
        .map_err(CrabError::Io)?;
    if !output.status.success() {
        if !suppress_stdout_on_error {
            std::io::stdout()
                .write_all(&output.stdout)
                .map_err(CrabError::Io)?;
        }
        return Err(CrabError::Protocol(format!(
            "crab worktree add: post-create hydration failed after creating worktree at {}; run `crab hydrate` from that worktree to retry",
            ctx.current_worktree_root.display()
        )));
    }
    let summary = parse_post_create_hydration_summary(&output.stdout)?;
    if summary.failed == 0 {
        if summary.hydrated == 0 && summary.skipped == 0 {
            println!("No pointer files match the given patterns.");
        }
        return Ok(());
    }
    Err(CrabError::Protocol(format!(
        "crab worktree add: post-create hydration failed for {} file(s) after creating worktree at {}; run `crab hydrate` from that worktree to retry",
        summary.failed,
        ctx.current_worktree_root.display()
    )))
}

async fn run_post_create_prefetch(
    ctx: &WorktreeContext,
    policy: &ResolvedHydrationPolicy,
    cancel: &CancellationToken,
) -> Result<()> {
    let Some(args) = hydrate_args_for_policy(policy) else {
        return Ok(());
    };
    let config = Config::resolve_for_repo(&ctx.current_worktree_root)?;
    let candidates = crate::cmd::hydrate::resolve_git_pointer_prefetch_candidates(
        &ctx.current_worktree_root,
        &args,
        &config,
        cancel,
    )?;
    if candidates.is_empty() {
        println!("No pointer files match the given prefetch selection.");
        return Ok(());
    }

    let parsed = read_worktree_crab_remote(ctx)?;
    let selection =
        crate::replication::select_read_store(&config, &parsed, "worktree-prefetch", cancel)
            .await?;
    if let crate::replication::ReadSource::Replica { name } = &selection.source {
        tracing::debug!(replica = %name, "selected read replica for worktree prefetch");
    }
    let caching_store = crab_cache_store::CachingStore::new(selection.store, &config.cache)?;
    let mut hydrator = crate::cmd::hydrate::ShardHydrator::with_config_from_cli_layout(
        caching_store,
        selection.router,
        &config,
    )?;
    match crate::cache::xet_chunk_cache_from_config(&config) {
        Ok(handle) => {
            hydrator = hydrator.with_xet_chunk_cache(handle.cache);
        }
        Err(e) => {
            tracing::debug!(error = %e, "worktree prefetch: failed to open xet-core chunk cache");
        }
    }

    let summary = hydrator.prefetch_batch(&candidates, cancel).await?;
    if summary.failed > 0 {
        return Err(CrabError::Protocol(format!(
            "crab worktree add: post-create prefetch failed for {} file(s) after creating worktree at {}; run `crab hydrate` from that worktree to materialize selected Crab pointer files",
            summary.failed,
            ctx.current_worktree_root.display()
        )));
    }
    println!(
        "Prefetched {} file(s) ({}) into Crab cache.",
        summary.prefetched,
        format_prefetch_bytes(summary.bytes_prefetched)
    );
    Ok(())
}

fn hydrate_args_for_policy(policy: &ResolvedHydrationPolicy) -> Option<HydrateArgs> {
    let mut args = HydrateArgs {
        patterns: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        all: false,
        mode: OutputMode::Text,
        manifest: None,
        manifest_ref: None,
        profile: None,
        ignore_sparse: false,
        recover_from: None,
    };
    match &policy.selector {
        HydrationSelector::All => {
            args.all = true;
        }
        HydrationSelector::Patterns { include, exclude } => {
            args.include = include.clone();
            args.exclude = exclude.clone();
        }
        HydrationSelector::Manifest { path, exclude } => {
            args.manifest = Some(path.clone());
            args.exclude = exclude.clone();
        }
        HydrationSelector::ManifestRef { spec, exclude } => {
            args.manifest_ref = Some(spec.clone());
            args.exclude = exclude.clone();
        }
        HydrationSelector::Profile { name, exclude } => {
            args.profile = Some(name.clone());
            args.exclude = exclude.clone();
        }
        HydrationSelector::CloneDefaults => match policy.mode {
            ResolvedHydrationMode::Full => {
                args.all = true;
            }
            ResolvedHydrationMode::Lazy
            | ResolvedHydrationMode::PointerOnly
            | ResolvedHydrationMode::Selective => return None,
        },
    }
    Some(args)
}

fn read_worktree_crab_remote(ctx: &WorktreeContext) -> Result<crate::git::url::CrabUrl> {
    let url = crate::core::project_config::ProjectConfig::remote_url(&ctx.current_worktree_root)?;
    crate::git::url::CrabUrl::parse(&url)
}

fn format_prefetch_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PostCreateHydrationSummary {
    hydrated: u64,
    skipped: u64,
    failed: u64,
}

fn parse_post_create_hydration_summary(stdout: &[u8]) -> Result<PostCreateHydrationSummary> {
    let stdout = std::str::from_utf8(stdout).map_err(|err| {
        CrabError::Protocol(format!(
            "crab worktree add: hydrate returned non-UTF-8 JSON summary: {err}"
        ))
    })?;
    let Some(line) = stdout
        .lines()
        .find(|line| line.trim_start().starts_with('{'))
    else {
        return Err(CrabError::Protocol(
            "crab worktree add: hydrate did not return a JSON summary".to_owned(),
        ));
    };
    let value: serde_json::Value = serde_json::from_str(line).map_err(|err| {
        CrabError::Protocol(format!(
            "crab worktree add: failed to parse hydrate JSON summary: {err}"
        ))
    })?;
    let data = value.get("data").ok_or_else(|| {
        CrabError::Protocol("crab worktree add: hydrate JSON summary is missing data".to_owned())
    })?;
    Ok(PostCreateHydrationSummary {
        hydrated: json_u64(data, "hydrated")?,
        skipped: json_u64(data, "skipped")?,
        failed: json_u64(data, "failed")?,
    })
}

fn json_u64(value: &serde_json::Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            CrabError::Protocol(format!(
                "crab worktree add: hydrate JSON summary is missing numeric `{key}`"
            ))
        })
}

pub fn run_list(args: &ListArgs, force_json: bool) -> Result<()> {
    if args.json || force_json {
        let payload = list_json_payload(args.with_crab_state)?;
        emit_json(WORKTREE_LIST_SCHEMA, "1.0", payload);
        return Ok(());
    }

    let mut git_args = vec![OsString::from("worktree"), OsString::from("list")];
    if args.verbose {
        git_args.push(OsString::from("--verbose"));
    }
    if args.porcelain {
        git_args.push(OsString::from("--porcelain"));
    }
    if args.zero {
        git_args.push(OsString::from("-z"));
    }
    passthrough_git(&git_args)
}

pub fn run_lock(args: &LockArgs, json: bool) -> Result<()> {
    let mut git_args = vec![OsString::from("worktree"), OsString::from("lock")];
    if let Some(reason) = &args.reason {
        git_args.push(OsString::from("--reason"));
        git_args.push(OsString::from(reason));
    }
    git_args.push(args.worktree.as_os_str().to_owned());
    passthrough_git_for_mode(&git_args, !json)?;
    if json {
        let path = resolved_arg_path(&args.worktree)?;
        emit_json(
            WORKTREE_LOCK_SCHEMA,
            "1.0",
            WorktreePathPayload {
                path: path.to_string_lossy().into_owned(),
                worktree: list_entry_for_path(&path)?,
            },
        );
    }
    Ok(())
}

pub fn run_move(args: &MoveArgs, json: bool) -> Result<()> {
    let mut git_args = vec![OsString::from("worktree"), OsString::from("move")];
    for _ in 0..args.force {
        git_args.push(OsString::from("--force"));
    }
    git_args.push(args.worktree.as_os_str().to_owned());
    git_args.push(args.new_path.as_os_str().to_owned());
    let old_path = resolved_arg_path(&args.worktree)?;
    passthrough_git_for_mode(&git_args, !json)?;
    if json {
        let new_path = created_worktree_path(&args.new_path)?;
        emit_json(
            WORKTREE_MOVE_SCHEMA,
            "1.0",
            WorktreeMovePayload {
                old_path: old_path.to_string_lossy().into_owned(),
                new_path: new_path.to_string_lossy().into_owned(),
                worktree: list_entry_for_path(&new_path)?,
            },
        );
    }
    Ok(())
}

pub fn run_prune(args: &PruneArgs, json: bool) -> Result<()> {
    let state_locations = if args.dry_run {
        Vec::new()
    } else {
        worktree_state_locations_from_current_repo()?
    };

    let mut git_args = vec![OsString::from("worktree"), OsString::from("prune")];
    if args.dry_run {
        git_args.push(OsString::from("--dry-run"));
    }
    if args.verbose {
        git_args.push(OsString::from("--verbose"));
    }
    if let Some(expire) = &args.expire {
        git_args.push(OsString::from("--expire"));
        git_args.push(OsString::from(expire));
    }
    let output = passthrough_git_for_mode(&git_args, !json)?;

    if !state_locations.is_empty() {
        report_state_cleanup(cleanup_removed_worktree_states(&state_locations)?);
    }
    if json {
        emit_json(
            WORKTREE_PRUNE_SCHEMA,
            "1.0",
            WorktreePrunePayload {
                dry_run: args.dry_run,
                pruned_lines: command_output_lines(&output),
            },
        );
    }
    Ok(())
}

pub fn run_remove(args: &RemoveArgs, json: bool) -> Result<()> {
    let path = resolved_arg_path(&args.worktree)?;
    let state_location = worktree_state_location_for_path(&args.worktree)?;
    let mut git_args = vec![OsString::from("worktree"), OsString::from("remove")];
    for _ in 0..args.force {
        git_args.push(OsString::from("--force"));
    }
    git_args.push(args.worktree.as_os_str().to_owned());
    run_remove_git(&git_args, !json)?;

    if let Some(location) = state_location {
        report_state_cleanup(cleanup_removed_worktree_states(&[location])?);
    }
    if json {
        emit_json(
            WORKTREE_REMOVE_SCHEMA,
            "1.0",
            WorktreePathPayload {
                path: path.to_string_lossy().into_owned(),
                worktree: None,
            },
        );
    }
    Ok(())
}

pub fn run_repair(args: &RepairArgs, json: bool) -> Result<()> {
    reject_unsupported_version_gated_options(
        "repair",
        &[
            ("--relative-paths", args.relative_paths),
            ("--no-relative-paths", args.no_relative_paths),
        ],
    )?;

    let mut git_args = vec![OsString::from("worktree"), OsString::from("repair")];
    if args.relative_paths {
        git_args.push(OsString::from("--relative-paths"));
    }
    if args.no_relative_paths {
        git_args.push(OsString::from("--no-relative-paths"));
    }
    for path in &args.paths {
        git_args.push(path.as_os_str().to_owned());
    }
    passthrough_git_for_mode(&git_args, !json)?;
    if json {
        emit_json(
            WORKTREE_REPAIR_SCHEMA,
            "1.0",
            WorktreeRepairPayload {
                repaired: true,
                paths: args
                    .paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
            },
        );
    }
    Ok(())
}

pub fn run_unlock(args: &UnlockArgs, json: bool) -> Result<()> {
    let git_args = vec![
        OsString::from("worktree"),
        OsString::from("unlock"),
        args.worktree.as_os_str().to_owned(),
    ];
    passthrough_git_for_mode(&git_args, !json)?;
    if json {
        let path = resolved_arg_path(&args.worktree)?;
        emit_json(
            WORKTREE_UNLOCK_SCHEMA,
            "1.0",
            WorktreePathPayload {
                path: path.to_string_lossy().into_owned(),
                worktree: list_entry_for_path(&path)?,
            },
        );
    }
    Ok(())
}

fn list_json_payload(include_crab_state: bool) -> Result<WorktreeListPayload> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()?;
    if !output.status.success() {
        return Err(git_failure(
            "git worktree list --porcelain -z",
            &output.stderr,
        ));
    }

    let records = parse_worktree_list_porcelain(&output.stdout, true)?;
    let identity_map = linked_identity_map_from_current_repo()?;
    let shared_crab_dir = if include_crab_state {
        current_shared_crab_dir()?
    } else {
        None
    };
    let mut worktrees = Vec::with_capacity(records.len());

    for (index, record) in records.into_iter().enumerate() {
        let identity = identity_for_record(index, &record, &identity_map)?;
        let state =
            shared_crab_dir
                .as_ref()
                .zip(identity.as_ref())
                .map(|(shared_crab_dir, identity)| {
                    state_summary(&shared_crab_dir.join("worktrees").join(identity))
                });
        worktrees.push(WorktreeListEntry {
            git: record,
            crab: CrabWorktreeIdentity { identity, state },
        });
    }

    Ok(WorktreeListPayload {
        git_version: crate::git::worktree::installed_git_version()?.map(|version| version.original),
        worktrees,
    })
}

fn identity_for_record(
    index: usize,
    record: &GitWorktreeRecord,
    identity_map: &std::collections::HashMap<String, String>,
) -> Result<Option<String>> {
    if index == 0 && !record.bare {
        return Ok(Some("main".to_owned()));
    }

    let normalized = normalize_identity_path(Path::new(&record.path));
    if let Some(identity) = identity_map.get(&normalized) {
        return Ok(Some(identity.clone()));
    }

    worktree_identity_for_path(Path::new(&record.path))
}

fn current_shared_crab_dir() -> Result<Option<PathBuf>> {
    match WorktreeContext::resolve() {
        Ok(ctx) => Ok(Some(ctx.shared_crab_dir)),
        Err(_) => Ok(None),
    }
}

fn state_summary(state_dir: &Path) -> CrabWorktreeStateSummary {
    let hydrated_pointer_cache = hydrated_pointer_cache_summary(state_dir);
    let pointer_summary = CrabPointerSummary {
        hydrated_pointer_entries: hydrated_pointer_cache.entries,
    };
    CrabWorktreeStateSummary {
        state_dir: state_dir.to_string_lossy().into_owned(),
        state_dir_exists: state_dir.is_dir(),
        hydration_policy: state_file_summary(&state_dir.join(HYDRATION_POLICY_FILENAME)),
        hydrated_pointer_cache,
        pointer_summary,
        access_db: state_file_summary(&state_dir.join("access.db")),
    }
}

fn state_file_summary(path: &Path) -> CrabStateFileSummary {
    let bytes = fs::metadata(path)
        .ok()
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len());
    CrabStateFileSummary {
        exists: bytes.is_some(),
        bytes,
    }
}

fn hydrated_pointer_cache_summary(state_dir: &Path) -> CrabHydratedPointerCacheSummary {
    let path = state_dir.join(crate::cache::hydrated_pointer::HYDRATED_POINTERS_FILENAME);
    let bytes = fs::metadata(&path)
        .ok()
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len());
    let entries = if bytes.is_some() {
        crate::cache::HydratedPointerCache::count_on_disk(&path)
    } else {
        0
    };
    CrabHydratedPointerCacheSummary {
        exists: bytes.is_some(),
        bytes,
        entries,
    }
}

fn worktree_state_location_for_path(path: &Path) -> Result<Option<WorktreeStateLocation>> {
    let normalized_arg = normalize_identity_path(&absolute_arg_path(path)?);
    for location in worktree_state_locations_from_current_repo()? {
        if location.record_path == normalized_arg {
            return Ok(Some(location));
        }
    }

    let Ok(ctx) = WorktreeContext::resolve_from_path(path) else {
        return Ok(None);
    };
    if ctx.identity == "main" {
        return Ok(None);
    }
    Ok(Some(WorktreeStateLocation {
        identity: ctx.identity,
        record_path: normalize_identity_path(&ctx.current_worktree_root),
        state_dir: ctx.per_worktree_crab_dir,
    }))
}

fn created_worktree_path(path: &Path) -> Result<PathBuf> {
    match WorktreeContext::resolve_from_path(path) {
        Ok(ctx) => Ok(ctx.current_worktree_root),
        Err(_) => resolved_arg_path(path),
    }
}

fn resolved_arg_path(path: &Path) -> Result<PathBuf> {
    let absolute = absolute_arg_path(path)?;
    Ok(absolute
        .canonicalize()
        .unwrap_or_else(|_| normalize_existing_parent(&absolute)))
}

fn normalize_existing_parent(path: &Path) -> PathBuf {
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    parent.canonicalize().map_or_else(
        |_| path.to_path_buf(),
        |resolved_parent| {
            path.file_name()
                .map_or(resolved_parent.clone(), |name| resolved_parent.join(name))
        },
    )
}

fn absolute_arg_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir().map_err(CrabError::Io)?.join(path))
    }
}

fn list_entry_for_path(path: &Path) -> Result<Option<WorktreeListEntry>> {
    let normalized = normalize_identity_path(path);
    Ok(list_json_payload(false)?
        .worktrees
        .into_iter()
        .find(|entry| normalize_identity_path(Path::new(&entry.git.path)) == normalized))
}

fn worktree_state_locations_from_current_repo() -> Result<Vec<WorktreeStateLocation>> {
    let Some(shared_crab_dir) = current_shared_crab_dir()? else {
        return Ok(Vec::new());
    };

    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()?;
    if !output.status.success() {
        return Err(git_failure(
            "git worktree list --porcelain -z",
            &output.stderr,
        ));
    }

    let records = parse_worktree_list_porcelain(&output.stdout, true)?;
    let identity_map = linked_identity_map_from_current_repo()?;
    let mut locations = Vec::new();
    for (index, record) in records.into_iter().enumerate() {
        let Some(identity) = identity_for_record(index, &record, &identity_map)? else {
            continue;
        };
        if identity == "main" {
            continue;
        }
        locations.push(WorktreeStateLocation {
            record_path: normalize_identity_path(Path::new(&record.path)),
            state_dir: shared_crab_dir.join("worktrees").join(&identity),
            identity,
        });
    }
    Ok(locations)
}

fn cleanup_removed_worktree_states(
    candidates: &[WorktreeStateLocation],
) -> Result<Vec<StateCleanupOutcome>> {
    let active = worktree_state_locations_from_current_repo()?
        .into_iter()
        .map(|location| location.identity)
        .collect::<HashSet<_>>();
    let mut outcomes = Vec::new();
    for candidate in candidates {
        if active.contains(&candidate.identity) {
            continue;
        }
        outcomes.push(cleanup_worktree_state(candidate)?);
    }
    Ok(outcomes)
}

fn cleanup_worktree_state(location: &WorktreeStateLocation) -> Result<StateCleanupOutcome> {
    if !location.state_dir.exists() {
        return Ok(StateCleanupOutcome::Missing {
            identity: location.identity.clone(),
        });
    }

    let Some(_lock) = try_lock_worktree_state(&location.state_dir)? else {
        return Ok(StateCleanupOutcome::Locked {
            identity: location.identity.clone(),
            state_dir: location.state_dir.clone(),
        });
    };

    fs::remove_dir_all(&location.state_dir).map_err(CrabError::Io)?;
    Ok(StateCleanupOutcome::Deleted {
        identity: location.identity.clone(),
    })
}

fn try_lock_worktree_state(state_dir: &Path) -> Result<Option<File>> {
    let lock_path = state_dir.join("state.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(CrabError::Io)?;

    match try_flock_exclusive(&file) {
        Ok(()) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(CrabError::Io(e)),
    }
}

#[cfg(unix)]
fn try_flock_exclusive(file: &File) -> std::io::Result<()> {
    // SAFETY: `flock` is advisory and the fd is valid because `file`
    // remains open for the cleanup critical section.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn try_flock_exclusive(_file: &File) -> std::io::Result<()> {
    Ok(())
}

fn report_state_cleanup(outcomes: Vec<StateCleanupOutcome>) {
    for outcome in outcomes {
        match outcome {
            StateCleanupOutcome::Deleted { identity } => {
                eprintln!("crab worktree: removed Crab state for worktree {identity}");
            }
            StateCleanupOutcome::Missing { .. } => {}
            StateCleanupOutcome::Locked {
                identity,
                state_dir,
            } => {
                eprintln!(
                    "crab worktree: skipped Crab state cleanup for worktree {identity}; state is locked at {}",
                    state_dir.display()
                );
            }
        }
    }
}

fn reject_unsupported_version_gated_options(
    subcommand: &str,
    requested: &[(&str, bool)],
) -> Result<()> {
    if !requested.iter().any(|(_, requested)| *requested) {
        return Ok(());
    }

    let help = git_worktree_subcommand_help(subcommand)?;
    for (option, is_requested) in requested {
        if !is_requested || help.contains(option) {
            continue;
        }
        let introduced_by = crate::git::worktree::LATEST_TRACKED_VERSION_GATED_OPTIONS
            .iter()
            .find(|tracked| tracked.subcommand == subcommand && tracked.option == *option)
            .map_or(
                crate::git::worktree::TRACKED_LATEST_MANUAL_VERSION,
                |tracked| tracked.introduced_by_manual,
            );
        return Err(CrabError::Protocol(format!(
            "crab worktree {subcommand}: option {option} is not supported by the installed Git; tracked from Git {introduced_by} manual"
        )));
    }

    Ok(())
}

fn git_worktree_subcommand_help(subcommand: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["worktree", subcommand, "-h"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if help.is_empty() && !output.status.success() {
        return Err(git_failure(
            &format!("git worktree {subcommand} -h"),
            &output.stderr,
        ));
    }
    Ok(help)
}

fn passthrough_git(args: &[OsString]) -> Result<()> {
    passthrough_git_for_mode(args, true).map(|_| ())
}

fn passthrough_git_for_mode(args: &[OsString], emit_output: bool) -> Result<std::process::Output> {
    let output = run_git_capture(args)?;

    if emit_output {
        write_git_output(&output)?;
    }
    if output.status.success() {
        Ok(output)
    } else {
        Err(git_failure(&format_git_command(args), &output.stderr))
    }
}

fn run_remove_git(args: &[OsString], emit_output: bool) -> Result<()> {
    let output = run_git_capture(args)?;
    if output.status.success() {
        if emit_output {
            write_git_output(&output)?;
        }
        return Ok(());
    }

    if is_crab_filter_remove_failure(&output.stderr) {
        eprintln!(
            "crab worktree: git remove hit a Crab filter failure; retrying with Crab filters disabled"
        );
        let fallback_args = with_crab_filters_disabled(args);
        let fallback = run_git_capture(&fallback_args)?;
        if emit_output {
            write_git_output(&fallback)?;
        }
        if fallback.status.success() {
            return Ok(());
        }
        return Err(git_failure(
            &format_git_command(&fallback_args),
            &fallback.stderr,
        ));
    }

    if emit_output {
        write_git_output(&output)?;
    }
    Err(git_failure(&format_git_command(args), &output.stderr))
}

fn run_git_capture(args: &[OsString]) -> Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(CrabError::Io)
}

fn write_git_output(output: &std::process::Output) -> Result<()> {
    std::io::stdout()
        .write_all(&output.stdout)
        .map_err(CrabError::Io)?;
    std::io::stderr()
        .write_all(&output.stderr)
        .map_err(CrabError::Io)?;
    Ok(())
}

fn command_output_lines(output: &std::process::Output) -> Vec<String> {
    let mut lines = Vec::new();
    lines.extend(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned),
    );
    lines.extend(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned),
    );
    lines
}

fn with_crab_filters_disabled(args: &[OsString]) -> Vec<OsString> {
    let mut out = vec![
        OsString::from("-c"),
        OsString::from("filter.crab.process="),
        OsString::from("-c"),
        OsString::from("filter.crab.required=false"),
        OsString::from("-c"),
        OsString::from("filter.crab.clean="),
        OsString::from("-c"),
        OsString::from("filter.crab.smudge="),
    ];
    out.extend(args.iter().cloned());
    out
}

fn is_crab_filter_remove_failure(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if stderr.contains("crab-e0081") {
        return true;
    }

    let mentions_crab_filter = stderr.contains("filter.crab")
        || stderr.contains("filter-process")
        || stderr.contains("crab filter")
        || stderr.contains("clean filter 'crab'")
        || stderr.contains("smudge filter 'crab'");
    if !mentions_crab_filter {
        return false;
    }

    let lock_failure = stderr.contains("staging") && stderr.contains("lock");
    let setup_failure = stderr.contains("required filter")
        || stderr.contains("unable to fork")
        || stderr.contains("permission denied")
        || stderr.contains("no such file")
        || stderr.contains("not found")
        || stderr.contains("failed");
    lock_failure || setup_failure
}

fn format_git_command(args: &[OsString]) -> String {
    let args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    format!("git {args}")
}

fn git_failure(command: &str, stderr: &[u8]) -> CrabError {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    let message = if detail.is_empty() {
        format!("{command} failed")
    } else {
        format!("{command} failed: {detail}")
    };
    CrabError::Protocol(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_fallback_classifier_accepts_crab_filter_lock_failures() {
        assert!(is_crab_filter_remove_failure(
            b"error: external filter 'crab filter-process' failed\nCRAB-E0081: staging area is locked"
        ));
        assert!(is_crab_filter_remove_failure(
            b"fatal: model.bin: clean filter 'crab' failed: staging lock held"
        ));
    }

    #[test]
    fn remove_fallback_classifier_rejects_unrelated_git_failures() {
        assert!(!is_crab_filter_remove_failure(
            b"fatal: 'linked' contains modified or untracked files, use --force to delete it"
        ));
        assert!(!is_crab_filter_remove_failure(
            b"fatal: cannot remove a locked working tree, lock reason: testing"
        ));
        assert!(!is_crab_filter_remove_failure(
            b"fatal: working trees containing submodules cannot be moved or removed"
        ));
    }
}
