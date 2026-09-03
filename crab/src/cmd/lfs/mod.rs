//! `crab lfs` — Git LFS compatibility commands.
//!
//! Provides a full set of LFS subcommands that operate against cloud object
//! storage without a centralized LFS server. Each subcommand delegates to
//! the corresponding module in `crate::lfs`.

pub mod checkout;
pub mod clone;
pub mod completion;
pub mod convert;
pub mod dedup;
pub mod env;
pub mod ext;
pub mod fetch;
pub mod filter_process;
pub mod fsck;
pub mod hooks;
pub mod install;
pub mod locks;
pub mod logs;
pub mod ls_files;
pub mod merge_driver;
pub mod migrate;
pub mod pointer;
pub mod progress;
pub mod prune;
pub mod push;
pub mod standalone;
pub mod standalone_file;
pub mod status;
pub mod store_setup;
pub mod transfer_agent;
pub mod update;

use std::path::{Path, PathBuf};

use clap::Subcommand;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::OutputMode;
use tokio_util::sync::CancellationToken;

pub(super) fn hooks_dir_from(root: &Path) -> Result<PathBuf> {
    crate::cmd::install::resolve_hooks_dir(root)
}

/// LFS subcommands dispatched from `crab lfs <cmd>`.
#[derive(Subcommand)]
pub enum LfsCmd {
    /// Configure git to use crab for LFS filters and transfers.
    Install {
        /// Overwrite existing hooks even if modified.
        #[arg(long)]
        force: bool,
        /// Write configuration to the local `.git/config` only.
        #[arg(long)]
        local: bool,
        /// Write configuration to the current worktree config.
        #[arg(long)]
        worktree: bool,
        /// Print commands instead of modifying configuration.
        #[arg(long)]
        manual: bool,
        /// Write configuration to the system git config.
        #[arg(long)]
        system: bool,
        /// Set the smudge filter to skip mode (pointers not expanded on checkout).
        #[arg(long)]
        skip_smudge: bool,
        /// Skip installing repository hooks.
        #[arg(long)]
        skip_repo: bool,
    },
    /// Remove crab LFS filter and transfer configuration.
    Uninstall {
        /// Remove configuration from the local `.git/config` only.
        #[arg(long)]
        local: bool,
        /// Remove configuration from the current worktree config.
        #[arg(long)]
        worktree: bool,
        /// Remove configuration from the system git config.
        #[arg(long)]
        system: bool,
        /// Skip removing repository hooks.
        #[arg(long)]
        skip_repo: bool,
    },
    /// Update git hooks and filter configuration for LFS.
    Update {
        /// Overwrite existing hooks even if modified.
        #[arg(long)]
        force: bool,
        /// Display commands instead of modifying configuration.
        #[arg(long)]
        manual: bool,
    },
    /// Deprecated Git LFS clone compatibility wrapper.
    Clone {
        /// Include only paths matching this pattern during the post-clone LFS pull.
        #[arg(long, short = 'I', value_name = "PATTERN")]
        include: Option<String>,
        /// Exclude paths matching this pattern during the post-clone LFS pull.
        #[arg(long, short = 'X', value_name = "PATTERN")]
        exclude: Option<String>,
        /// Skip installing repository-level LFS hooks and config.
        #[arg(long)]
        skip_repo: bool,
        /// Arguments passed through to `git clone`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "GIT_CLONE_ARG"
        )]
        args: Vec<String>,
    },
    /// Track files matching a pattern with LFS.
    Track {
        /// Glob patterns to track (omit to list tracked patterns).
        #[arg(value_name = "PATTERN")]
        patterns: Vec<String>,
        /// Override existing Crab/XET tracking for the same pattern.
        #[arg(long)]
        force: bool,
        /// Preview changes without modifying `.gitattributes`.
        #[arg(long, short = 'd')]
        dry_run: bool,
        /// Treat arguments as literal filenames.
        #[arg(long)]
        filename: bool,
        /// Mark tracked paths as lockable.
        #[arg(long, short = 'l', conflicts_with = "not_lockable")]
        lockable: bool,
        /// Remove the lockable attribute from tracked paths.
        #[arg(long, conflicts_with = "lockable")]
        not_lockable: bool,
        /// List only tracked patterns.
        #[arg(long)]
        no_excluded: bool,
        /// Print files checked for matching existing Git index entries.
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Mark matching tracked files stat-dirty without editing `.gitattributes`.
        #[arg(long = "no-modify-attrs")]
        no_modify_attrs: bool,
    },
    /// Stop tracking files matching patterns with LFS.
    Untrack {
        /// Glob patterns to untrack.
        #[arg(required = true, value_name = "PATTERN")]
        patterns: Vec<String>,
    },
    /// Download LFS objects from the remote store.
    Fetch {
        /// Git remote name. Crab reads this Git remote URL when provided.
        remote: Option<String>,
        /// Refs to fetch LFS objects for.
        #[arg(value_name = "REF")]
        refs: Vec<String>,
        /// Include only paths matching this pattern.
        #[arg(long, short = 'I', value_name = "PATTERN")]
        include: Option<String>,
        /// Exclude paths matching this pattern.
        #[arg(long, short = 'X', value_name = "PATTERN")]
        exclude: Option<String>,
        /// Fetch objects for recently updated refs.
        #[arg(long, short = 'r')]
        recent: bool,
        /// Fetch all LFS objects ever referenced by any local ref.
        #[arg(long, short = 'a')]
        all: bool,
        /// Read refs from stdin.
        #[arg(long)]
        stdin: bool,
        /// Prune unreferenced local LFS objects after fetching.
        #[arg(long, short = 'p')]
        prune: bool,
        /// Re-fetch objects already present locally.
        #[arg(long)]
        refetch: bool,
        /// Report what would be fetched without downloading.
        #[arg(long, short = 'd')]
        dry_run: bool,
        /// Output transfer details as JSON.
        #[arg(long, short = 'j')]
        json: bool,
    },
    /// Fetch LFS objects and replace pointers in the working tree.
    Pull {
        /// Git remote name. Crab reads this Git remote URL when provided.
        remote: Option<String>,
        /// Include only paths matching this pattern.
        #[arg(long, short = 'I', value_name = "PATTERN")]
        include: Option<String>,
        /// Exclude paths matching this pattern.
        #[arg(long, short = 'X', value_name = "PATTERN")]
        exclude: Option<String>,
    },
    /// Convert files between LFS and Crab native formats.
    Convert {
        /// Source format: "lfs" or "xet".
        #[arg(long)]
        from: Option<String>,
        /// Destination format: "lfs" or "xet".
        #[arg(long)]
        to: Option<String>,
        /// Glob pattern for files to convert.
        #[arg(value_name = "PATTERN")]
        pattern: Option<String>,
        /// Preview without modifying files.
        #[arg(long)]
        dry_run: bool,
        /// Rollback a previous conversion.
        #[arg(long, conflicts_with_all = ["from", "to", "pattern", "dry_run"])]
        rollback: bool,
    },
    /// Generate cloud lifecycle policy for LFS objects.
    LifecyclePolicy {
        /// Cloud backend: s3, gcs, or azure.
        #[arg(long, value_name = "BACKEND")]
        backend: String,
        /// Expire objects after N days.
        #[arg(long, value_name = "DAYS")]
        expire_days: u32,
    },
    /// Upload LFS objects to the remote store.
    Push {
        /// Git remote name. Crab reads this Git remote URL when provided.
        remote: Option<String>,
        /// Refs or additional object IDs to push.
        #[arg(value_name = "REF_OR_OID")]
        args: Vec<String>,
        /// Upload all locally-known LFS objects.
        #[arg(long, short = 'a')]
        all: bool,
        /// Upload a specific LFS object by OID.
        #[arg(long, short = 'o', value_name = "OID", num_args = 0..=1)]
        object_id: Option<Option<String>>,
        /// Read refs or object IDs from stdin.
        #[arg(long)]
        stdin: bool,
        /// Report what would be pushed without uploading.
        #[arg(long, short = 'd')]
        dry_run: bool,
    },
    /// Pre-push hook: upload missing LFS objects before push completes.
    PrePush {
        /// Remote name passed by git's pre-push hook.
        #[arg(hide = true)]
        remote: Option<String>,
        /// Remote URL passed by git's pre-push hook.
        #[arg(hide = true)]
        url: Option<String>,
    },
    /// Replace LFS pointers in the working tree with actual content.
    Checkout {
        /// Paths or glob patterns to check out.
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        /// Write resolved content to this path (for conflicts).
        #[arg(long, value_name = "PATH")]
        to: Option<String>,
        /// Check out the merge base conflict stage.
        #[arg(long)]
        base: bool,
        /// Check out our conflict stage.
        #[arg(long)]
        ours: bool,
        /// Check out their conflict stage.
        #[arg(long)]
        theirs: bool,
    },
    /// Create an advisory lock on an LFS-tracked file.
    Lock {
        /// Path to lock.
        path: String,
        /// Git remote name. Crab reads this Git remote URL when provided.
        #[arg(long, short = 'r', value_name = "NAME")]
        remote: Option<String>,
        /// Output lock info as JSON.
        #[arg(long, short = 'j')]
        json: bool,
        /// Lock expiration duration (e.g. "24h", "7d").
        #[arg(long, value_name = "DURATION")]
        expires_in: Option<String>,
    },
    /// Remove an advisory lock from an LFS-tracked file.
    Unlock {
        /// Path to unlock.
        path: Option<String>,
        /// Git remote name. Crab reads this Git remote URL when provided.
        #[arg(long, short = 'r', value_name = "NAME")]
        remote: Option<String>,
        /// Remove the lock regardless of the current owner.
        #[arg(long, short = 'f')]
        force: bool,
        /// Unlock by lock ID instead of path.
        #[arg(long, short = 'i', value_name = "ID")]
        id: Option<String>,
        /// Output lock info as JSON.
        #[arg(long, short = 'j')]
        json: bool,
    },
    /// List all active LFS file locks.
    Locks {
        /// Git remote name. Crab reads this Git remote URL when provided.
        #[arg(long, short = 'r', value_name = "NAME")]
        remote: Option<String>,
        /// Return only the lock with this ID.
        #[arg(long, short = 'i', value_name = "ID")]
        id: Option<String>,
        /// Return only the lock for this path.
        #[arg(long, short = 'p', value_name = "PATH")]
        path: Option<String>,
        /// List only local cached locks.
        #[arg(long)]
        local: bool,
        /// List cached locks from the last remote call.
        #[arg(long)]
        cached: bool,
        /// Output lock records as JSON.
        #[arg(long, short = 'j')]
        json: bool,
        /// Verify lock record integrity.
        #[arg(long)]
        verify: bool,
        /// Limit number of lock records returned.
        #[arg(long, short = 'l', value_name = "N")]
        limit: Option<usize>,
    },
    /// List LFS-tracked files and their status.
    LsFiles {
        /// Explicit ref to scan instead of HEAD.
        #[arg(value_name = "REF")]
        refs: Vec<String>,
        /// List across all local refs.
        #[arg(long, short = 'a')]
        all: bool,
        /// Show full OIDs.
        #[arg(long, short = 'l')]
        long: bool,
        /// Show only filenames.
        #[arg(long, short = 'n')]
        name_only: bool,
        /// Include file sizes in the output.
        #[arg(long, short = 's')]
        size: bool,
        /// Include full OID, version, and pointer details.
        #[arg(long, short = 'd')]
        debug: bool,
        /// Include deleted files when scanning a ref.
        #[arg(long)]
        deleted: bool,
        /// Include only paths matching this pattern list.
        #[arg(long, short = 'I', value_name = "PATTERN")]
        include: Option<String>,
        /// Exclude paths matching this pattern list.
        #[arg(long, short = 'X', value_name = "PATTERN")]
        exclude: Option<String>,
        /// Output stable JSON.
        #[arg(long, short = 'j')]
        json: bool,
    },
    /// Show staged and modified LFS-tracked files.
    Status {
        /// Output as JSON.
        #[arg(long, short = 'j')]
        json: bool,
        /// Machine-parseable output.
        #[arg(long, short = 'p')]
        porcelain: bool,
    },
    /// Verify integrity of local LFS objects.
    Fsck {
        /// Revision or A..B range to inspect.
        #[arg(value_name = "REVISION")]
        revision: Option<String>,
        /// Verify that LFS pointers in the selected revision are well-formed.
        #[arg(long)]
        pointers: bool,
        /// Verify only selected LFS objects (skip pointer checks).
        #[arg(long)]
        objects: bool,
        /// Compatibility no-op: Crab reports corrupt objects but does not move them.
        #[arg(long, short = 'd')]
        dry_run: bool,
    },
    /// Remove unreferenced LFS objects from local storage.
    Prune {
        /// Require remote object verification before local delete.
        #[arg(long, short = 'c', conflicts_with = "no_verify_remote")]
        verify_remote: bool,
        /// Disable remote verification.
        #[arg(long, conflicts_with = "verify_remote")]
        no_verify_remote: bool,
        /// Compatibility flag; remote verification always covers every candidate.
        #[arg(long, conflicts_with = "no_verify_unreachable")]
        verify_unreachable: bool,
        /// Disable unreachable verification only when remote verification is disabled.
        #[arg(long, conflicts_with = "verify_unreachable")]
        no_verify_unreachable: bool,
        /// Continue or halt when verification is disabled; remote verification always halts.
        #[arg(long, value_name = "MODE", value_parser = ["halt", "continue"])]
        when_unverified: Option<String>,
        /// Prune objects retained only by recent-ref protection.
        #[arg(long)]
        recent: bool,
        /// Report what would be pruned without deleting.
        #[arg(long, short = 'd')]
        dry_run: bool,
        /// Skip confirmation prompts.
        #[arg(long, short = 'f')]
        force: bool,
        /// Print full object IDs in prune output.
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// Rewrite history to convert files to/from LFS pointers.
    #[command(subcommand)]
    Migrate(LfsMigrateCmd),
    /// Generate, validate, or inspect LFS pointers.
    Pointer {
        /// Generate the LFS pointer for a file.
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// Compare generated pointer output against this pointer file.
        #[arg(long, value_name = "PATH")]
        pointer: Option<String>,
        /// Read a pointer from stdin and display parsed fields.
        #[arg(long)]
        stdin: bool,
        /// Validate the pointer (exit 0 if valid, 1 if invalid).
        #[arg(long)]
        check: bool,
        /// Reject non-canonical pointers (use with --check).
        #[arg(long, conflicts_with = "no_strict")]
        strict: bool,
        /// Accept valid but non-canonical pointers (use with --check).
        #[arg(long = "no-strict", conflicts_with = "strict")]
        no_strict: bool,
    },
    /// Standalone clean filter (stdin → stdout).
    Clean {
        /// Path being cleaned. Accepted for Git LFS CLI compatibility.
        path: Option<String>,
    },
    /// Standalone smudge filter (stdin → stdout).
    Smudge {
        /// Path being smudged. Accepted for Git LFS CLI compatibility.
        path: Option<String>,
        /// Pass the pointer through unchanged (lazy mode).
        #[arg(long, short = 's')]
        skip: bool,
    },
    /// Generate shell completion scripts.
    Completion {
        /// Shell to generate completions for (bash, zsh, fish, powershell).
        shell: String,
    },
    /// View configured Git LFS extension details.
    Ext {
        #[command(subcommand)]
        command: Option<LfsExtCmd>,
    },
    /// Git LFS process filter protocol.
    FilterProcess {
        /// Skip automatic smudge downloads and pass pointers through.
        #[arg(long, short = 's')]
        skip: bool,
    },
    /// Git LFS merge driver for text LFS files.
    MergeDriver {
        /// File with the ancestor version.
        #[arg(long, value_name = "PATH")]
        ancestor: Option<String>,
        /// File with the current version.
        #[arg(long, value_name = "PATH")]
        current: Option<String>,
        /// File with the other version.
        #[arg(long, value_name = "PATH")]
        other: Option<String>,
        /// Merge marker size.
        #[arg(long = "marker-size", value_name = "N", default_value_t = 12)]
        marker_size: usize,
        /// File with the output version.
        #[arg(long, value_name = "PATH")]
        output: Option<String>,
        /// Program to run to perform the merge.
        #[arg(long, value_name = "PROGRAM")]
        program: Option<String>,
    },
    /// Git LFS post-checkout hook.
    PostCheckout {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Git LFS post-commit hook.
    PostCommit {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Git LFS post-merge hook.
    PostMerge {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Git LFS standalone-file transfer adapter endpoint.
    StandaloneFile {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print LFS diagnostic environment information.
    Env,
    /// Print crab LFS version information.
    Version,
    /// Deduplicate checked-out LFS files.
    Dedup {
        /// Report what would be deduplicated without modifying files.
        #[arg(long)]
        dry_run: bool,
        /// Test whether this platform and repository support LFS deduplication.
        #[arg(long, short = 't')]
        test: bool,
        /// Run Crab's cache cleanup mode for LFS objects duplicated by verified Crab content.
        #[arg(long)]
        crab_cache: bool,
    },
    /// Display Git LFS error logs.
    Logs {
        /// Show Crab transfer history instead of Git LFS-style error logs.
        #[arg(long)]
        transfer_history: bool,
        /// Show only the last N transfer-history entries.
        #[arg(long, value_name = "N")]
        last: Option<usize>,
        /// Clear logs. With --transfer-history, clears only the transfer log.
        #[arg(long)]
        clear: bool,
        /// Error log name or command: last, clear, show <file>, boomtown.
        #[arg(
            value_name = "LOG",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
    },
}

/// Subcommands for `crab lfs migrate`.
#[derive(Subcommand)]
pub enum LfsMigrateCmd {
    /// Convert large files in history to LFS pointers.
    Import {
        /// Glob pattern for files to convert.
        #[arg(long, short = 'I', value_name = "PATTERN")]
        include: Option<String>,
        /// Exclude files matching this pattern.
        #[arg(long, short = 'X', value_name = "PATTERN")]
        exclude: Option<String>,
        /// Only migrate files whose individual size is above this threshold.
        #[arg(long, value_name = "SIZE")]
        above: Option<String>,
        /// Infer paths from existing `.gitattributes` LFS tracking rules.
        #[arg(long)]
        fixup: bool,
        /// Convert files in a new commit without rewriting history.
        #[arg(long = "no-rewrite")]
        no_rewrite: bool,
        /// Commit message for --no-rewrite.
        #[arg(long, short = 'm', value_name = "MESSAGE", requires = "no_rewrite")]
        message: Option<String>,
        /// Write a CSV mapping old commit IDs to rewritten commit IDs.
        #[arg(long = "object-map", value_name = "PATH")]
        object_map: Option<String>,
        /// Process all local branches.
        #[arg(long)]
        everything: bool,
        /// Include commits reachable from this ref.
        #[arg(long = "include-ref", value_name = "REF")]
        include_refs: Vec<String>,
        /// Exclude commits reachable from this ref.
        #[arg(long = "exclude-ref", value_name = "REF")]
        exclude_refs: Vec<String>,
        /// Do not refresh remote refs before selecting commits.
        #[arg(long)]
        skip_fetch: bool,
        /// Continue without prompting when the working tree is dirty.
        #[arg(long)]
        yes: bool,
        /// Print commit and filename for each migrated file.
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Convert Crab pointers to LFS pointers.
        #[arg(long)]
        from_crab: bool,
        /// Branches or refs to migrate, or files when --no-rewrite is used.
        #[arg(value_name = "BRANCH|FILE")]
        operands: Vec<String>,
    },
    /// Convert LFS pointers back to regular files in history.
    Export {
        /// Glob pattern for files to convert back.
        #[arg(long, short = 'I', value_name = "PATTERN")]
        include: String,
        /// Exclude files matching this pattern.
        #[arg(long, short = 'X', value_name = "PATTERN")]
        exclude: Option<String>,
        /// Write a CSV mapping old commit IDs to rewritten commit IDs.
        #[arg(long = "object-map", value_name = "PATH")]
        object_map: Option<String>,
        /// Download LFS objects from this Git remote.
        #[arg(long, value_name = "GIT_REMOTE")]
        remote: Option<String>,
        /// Process all local branches.
        #[arg(long)]
        everything: bool,
        /// Include commits reachable from this ref.
        #[arg(long = "include-ref", value_name = "REF")]
        include_refs: Vec<String>,
        /// Exclude commits reachable from this ref.
        #[arg(long = "exclude-ref", value_name = "REF")]
        exclude_refs: Vec<String>,
        /// Do not refresh remote refs before selecting commits.
        #[arg(long)]
        skip_fetch: bool,
        /// Continue without prompting when the working tree is dirty.
        #[arg(long)]
        yes: bool,
        /// Print commit and filename for each migrated file.
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Convert LFS pointers to Crab pointers.
        #[arg(long)]
        to_crab: bool,
        /// Branches or refs to migrate. Prefix with ^ to exclude.
        #[arg(value_name = "BRANCH")]
        branches: Vec<String>,
    },
    /// Analyze the repository for files that would benefit from LFS.
    Info {
        /// Only include files larger than this size (e.g. `1mb`, `500kb`).
        #[arg(long, value_name = "SIZE")]
        above: Option<String>,
        /// Only analyze files matching this pattern.
        #[arg(long, short = 'I', value_name = "PATTERN")]
        include: Option<String>,
        /// Exclude files matching this pattern.
        #[arg(long, short = 'X', value_name = "PATTERN")]
        exclude: Option<String>,
        /// Process all local branches.
        #[arg(long)]
        everything: bool,
        /// Include commits reachable from this ref.
        #[arg(long = "include-ref", value_name = "REF")]
        include_refs: Vec<String>,
        /// Exclude commits reachable from this ref.
        #[arg(long = "exclude-ref", value_name = "REF")]
        exclude_refs: Vec<String>,
        /// Do not refresh remote refs before selecting commits.
        #[arg(long)]
        skip_fetch: bool,
        /// Only display the top N regular file entries (default: 5).
        #[arg(long, value_name = "N")]
        top: Option<usize>,
        /// Format sizes using this storage unit.
        #[arg(long, value_name = "UNIT")]
        unit: Option<String>,
        /// How to treat existing LFS pointers. Bare --pointers lists pointers.
        #[arg(long, num_args = 0..=1, default_missing_value = "only", value_parser = ["only", "follow", "no-follow", "ignore"])]
        pointers: Option<String>,
        /// Infer paths from existing `.gitattributes` LFS tracking rules.
        #[arg(long, conflicts_with_all = ["include", "exclude"])]
        fixup: bool,
        /// Branches or refs to analyze. Prefix with ^ to exclude.
        #[arg(value_name = "BRANCH")]
        branches: Vec<String>,
    },
}

/// Subcommands for `crab lfs ext`.
#[derive(Subcommand)]
pub enum LfsExtCmd {
    /// List configured extension details.
    List {
        /// Extension names to list. Omit to list all extensions.
        names: Vec<String>,
    },
}

/// Dispatch an LFS subcommand.
///
/// Run an async future from the synchronous LFS command dispatcher.
///
/// `crab` enters this dispatcher from an async Tokio main runtime. Creating
/// and blocking a second runtime on that same thread panics, so nested calls
/// run on a short-lived OS thread with its own runtime.
pub(crate) fn block_on_runtime<F, T>(f: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send,
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(move || {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| CrabError::Internal(format!("tokio: {e}")))?;
                rt.block_on(f)
            })
            .join()
            .map_err(|_| CrabError::Internal("lfs runtime thread panicked".into()))?
        })
    } else {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CrabError::Internal(format!("tokio: {e}")))?;
        rt.block_on(f)
    }
}

/// Returns the process exit code. Most subcommands return `SUCCESS`;
/// `pointer --check` returns a non-zero code for invalid pointers.
pub fn run_lfs(cmd: &LfsCmd) -> Result<std::process::ExitCode> {
    run_lfs_with_cancel(cmd, &CancellationToken::new())
}

/// Dispatch an LFS command with caller cancellation for supported operations.
pub fn run_lfs_with_cancel(
    cmd: &LfsCmd,
    cancel: &CancellationToken,
) -> Result<std::process::ExitCode> {
    check_cancelled(cancel)?;
    match cmd {
        LfsCmd::Install {
            force,
            local,
            worktree,
            manual,
            system,
            skip_smudge,
            skip_repo,
        } => {
            install::run_lfs_install(install::LfsInstallOptions {
                local: *local,
                worktree: *worktree,
                system: *system,
                force: *force,
                manual: *manual,
                skip_smudge: *skip_smudge,
                skip_repo: *skip_repo,
            })?;
        }
        LfsCmd::Uninstall {
            local,
            worktree,
            system,
            skip_repo,
        } => {
            install::run_lfs_uninstall(install::LfsUninstallOptions {
                local: *local,
                worktree: *worktree,
                system: *system,
                skip_repo: *skip_repo,
            })?;
        }
        LfsCmd::Update { force, manual } => {
            update::run_lfs_update(*force, *manual)?;
        }
        LfsCmd::Clone {
            include,
            exclude,
            skip_repo,
            args,
        } => {
            return clone::run_lfs_clone(
                clone::LfsCloneOptions {
                    args: args.clone(),
                    include: include.clone(),
                    exclude: exclude.clone(),
                    skip_repo: *skip_repo,
                },
                cancel,
            );
        }
        LfsCmd::Track {
            patterns,
            force,
            dry_run,
            filename,
            lockable,
            not_lockable,
            no_excluded,
            verbose,
            no_modify_attrs,
        } => {
            let lockable_mode = if *lockable {
                crate::lfs::track::LockableMode::Enable
            } else if *not_lockable {
                crate::lfs::track::LockableMode::Disable
            } else {
                crate::lfs::track::LockableMode::Preserve
            };
            let track_options = crate::lfs::track::TrackOptions {
                force: *force,
                dry_run: *dry_run,
                lockable: lockable_mode,
            };

            if !patterns.is_empty() {
                let repo_root = std::env::current_dir()?;
                for p in patterns {
                    let should_log_matches = *verbose || *dry_run;
                    if *no_modify_attrs {
                        println!("Tracking \"{p}\"");
                        if should_log_matches {
                            println!("Searching for files matching pattern: {p}");
                        }
                        let matched_paths = if *dry_run {
                            crate::lfs::track::matching_index_paths(p, &repo_root, *filename)?
                        } else {
                            crate::lfs::track::mark_matches_stat_dirty_paths(
                                p, &repo_root, *filename,
                            )?
                        };
                        if should_log_matches {
                            println!(
                                "Found {} files previously added to Git matching pattern: {p}",
                                matched_paths.len()
                            );
                            if !*dry_run {
                                for path in &matched_paths {
                                    println!("Touching \"{path}\"");
                                }
                            }
                        }
                        continue;
                    }

                    let conflict = if *filename {
                        crate::lfs::track::check_conflict_filename(p, &repo_root)
                    } else {
                        crate::lfs::track::check_conflict(p, &repo_root)
                    };
                    match conflict {
                        crate::lfs::track::ConflictCheck::CrabConflict if !force => {
                            eprintln!("\"{p}\" is already tracked by crab/XET.");
                            eprintln!(
                                "Use --force to override, or `crab lfs untrack` to remove the crab entry first."
                            );
                            return Ok(std::process::ExitCode::SUCCESS);
                        }
                        crate::lfs::track::ConflictCheck::AlreadyLfs => {
                            if lockable_mode == crate::lfs::track::LockableMode::Preserve {
                                println!("\"{p}\" already tracked by LFS");
                                continue;
                            }
                        }
                        _ => {}
                    }
                    let outcome = if *filename {
                        crate::lfs::track::track_filename_with_options(
                            p,
                            &repo_root,
                            track_options,
                        )?
                    } else {
                        crate::lfs::track::track_with_options(p, &repo_root, track_options)?
                    };
                    match outcome {
                        crate::lfs::track::TrackOutcome::Tracked => println!("Tracking \"{p}\""),
                        crate::lfs::track::TrackOutcome::SwitchedFromCrab => {
                            println!("\"{p}\" switched from crab/XET to LFS");
                        }
                        crate::lfs::track::TrackOutcome::Updated => {
                            println!("Updated \"{p}\" to LFS tracking");
                        }
                        crate::lfs::track::TrackOutcome::AlreadyTracked => {
                            println!("\"{p}\" already tracked");
                        }
                    }

                    if should_log_matches {
                        println!("Searching for files matching pattern: {p}");
                    }
                    let matched_paths = if *dry_run {
                        crate::lfs::track::matching_index_paths(p, &repo_root, *filename)?
                    } else {
                        crate::lfs::track::mark_matches_stat_dirty_paths(p, &repo_root, *filename)?
                    };
                    if should_log_matches {
                        println!(
                            "Found {} files previously added to Git matching pattern: {p}",
                            matched_paths.len()
                        );
                        if !*dry_run {
                            for path in &matched_paths {
                                println!("Touching \"{path}\"");
                            }
                        }
                    }
                }
            } else if *no_modify_attrs {
                return Ok(std::process::ExitCode::SUCCESS);
            } else {
                let repo_root = std::env::current_dir()?;
                let all = if *no_excluded {
                    crate::lfs::track::list(&repo_root)?
                        .into_iter()
                        .map(|pattern| crate::lfs::track::TrackedPattern {
                            pattern,
                            filter: crate::lfs::track::FilterType::Lfs,
                        })
                        .collect()
                } else {
                    crate::lfs::track::list_all(&repo_root)?
                };
                if all.is_empty() {
                    println!("No tracked patterns");
                } else {
                    // Detect conflicts: patterns appearing with both filters.
                    let mut seen: std::collections::HashMap<
                        String,
                        Vec<crate::lfs::track::FilterType>,
                    > = std::collections::HashMap::new();
                    for p in &all {
                        seen.entry(p.pattern.clone()).or_default().push(p.filter);
                    }
                    for p in &all {
                        let types = &seen[&p.pattern];
                        if types.len() > 1 {
                            if let Some(last) = types.last() {
                                println!(
                                    "    {} ({}+{} CONFLICT — last match: {})",
                                    p.pattern, types[0], types[1], last
                                );
                            }
                        } else {
                            println!("    {} ({})", p.pattern, p.filter);
                        }
                    }
                }
            }
        }
        LfsCmd::Untrack { patterns } => {
            let cwd = std::env::current_dir()?;
            let repo_root = crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)?
                .current_worktree_root;
            for pattern in patterns {
                crate::lfs::track::untrack(pattern, &repo_root)?;
                println!("Untracking \"{pattern}\"");
            }
        }
        LfsCmd::Fetch {
            remote,
            refs,
            include,
            exclude,
            recent,
            all,
            stdin,
            prune,
            refetch,
            dry_run,
            json,
        } => {
            fetch::run_lfs_fetch(
                fetch::LfsFetchOptions {
                    remote: remote.clone(),
                    refs: refs.clone(),
                    include: include.clone(),
                    exclude: exclude.clone(),
                    recent: *recent,
                    all: *all,
                    stdin: *stdin,
                    prune: *prune,
                    refetch: *refetch,
                    dry_run: *dry_run,
                    json: *json,
                },
                cancel,
            )?;
        }
        LfsCmd::Pull {
            remote,
            include,
            exclude,
        } => {
            fetch::run_lfs_pull(
                fetch::LfsPullOptions {
                    remote: remote.clone(),
                    include: include.clone(),
                    exclude: exclude.clone(),
                },
                cancel,
            )?;
        }
        LfsCmd::Push {
            remote,
            args,
            all,
            object_id,
            stdin,
            dry_run,
        } => {
            push::run_lfs_push(
                push::LfsPushOptions {
                    remote: remote.clone(),
                    args: args.clone(),
                    all: *all,
                    object_id: object_id.clone(),
                    stdin: *stdin,
                    dry_run: *dry_run,
                },
                cancel,
            )?;
        }
        LfsCmd::PrePush { remote, url } => {
            push::run_lfs_pre_push(remote.as_deref(), url.as_deref(), cancel)?;
        }
        LfsCmd::Checkout {
            paths,
            to,
            base,
            ours,
            theirs,
        } => {
            checkout::run_lfs_checkout(checkout::LfsCheckoutOptions {
                paths: paths.clone(),
                to: to.clone(),
                base: *base,
                ours: *ours,
                theirs: *theirs,
            })?;
        }
        LfsCmd::Lock {
            path,
            remote,
            json,
            expires_in,
        } => {
            block_on_runtime(locks::run_lfs_lock(locks::LfsLockOptions {
                path: path.clone(),
                remote: remote.clone(),
                json: *json,
                expires_in: expires_in.clone(),
            }))?;
        }
        LfsCmd::Unlock {
            path,
            remote,
            force,
            id,
            json,
        } => {
            block_on_runtime(locks::run_lfs_unlock(locks::LfsUnlockOptions {
                path: path.clone(),
                remote: remote.clone(),
                force: *force,
                id: id.clone(),
                json: *json,
            }))?;
        }
        LfsCmd::Locks {
            remote,
            id,
            path,
            local,
            cached,
            json,
            verify,
            limit,
        } => {
            let mode = OutputMode::from_flags(*json, false);
            block_on_runtime(locks::run_lfs_locks(locks::LfsLocksOptions {
                remote: remote.clone(),
                id: id.clone(),
                path: path.clone(),
                local: *local,
                cached: *cached,
                mode,
                verify: *verify,
                limit: *limit,
            }))?;
        }
        LfsCmd::LsFiles {
            refs,
            all,
            long,
            name_only,
            size,
            debug,
            deleted,
            include,
            exclude,
            json,
        } => {
            ls_files::run_lfs_ls_files(ls_files::LfsLsFilesOptions {
                refs: refs.clone(),
                all: *all,
                long: *long,
                name_only: *name_only,
                size: *size,
                debug: *debug,
                deleted: *deleted,
                include: include.clone(),
                exclude: exclude.clone(),
                json: *json,
            })?;
        }
        LfsCmd::Status { json, porcelain } => {
            let mode = OutputMode::from_flags(*json, false);
            status::run_lfs_status(mode, *porcelain)?;
        }
        LfsCmd::Fsck {
            revision,
            pointers,
            objects,
            dry_run,
        } => {
            fsck::run_lfs_fsck(fsck::LfsFsckOptions {
                revision: revision.clone(),
                pointers: *pointers,
                objects: *objects,
                dry_run: *dry_run,
            })?;
        }
        LfsCmd::Prune {
            verify_remote,
            no_verify_remote,
            verify_unreachable,
            no_verify_unreachable,
            when_unverified,
            recent,
            dry_run,
            force,
            verbose,
        } => {
            prune::run_lfs_prune_with_cancel(
                prune::LfsPruneOptions {
                    verify_remote: *verify_remote,
                    no_verify_remote: *no_verify_remote,
                    verify_unreachable: *verify_unreachable,
                    no_verify_unreachable: *no_verify_unreachable,
                    when_unverified: when_unverified.clone(),
                    recent: *recent,
                    dry_run: *dry_run,
                    force: *force,
                    verbose: *verbose,
                },
                cancel,
            )?;
        }
        LfsCmd::Migrate(migrate_cmd) => match migrate_cmd {
            LfsMigrateCmd::Import {
                include,
                exclude,
                above,
                fixup,
                no_rewrite,
                message,
                object_map,
                everything,
                include_refs,
                exclude_refs,
                skip_fetch,
                yes,
                verbose,
                from_crab,
                operands,
            } => {
                migrate::run_migrate_import(migrate::LfsMigrateImportOptions {
                    include: include.clone(),
                    exclude: exclude.clone(),
                    above: above.clone(),
                    fixup: *fixup,
                    no_rewrite: *no_rewrite,
                    no_rewrite_files: if *no_rewrite {
                        operands.clone()
                    } else {
                        Vec::new()
                    },
                    message: message.clone(),
                    object_map: object_map.clone(),
                    refs: migrate::LfsMigrateRefSelection {
                        everything: *everything,
                        include_refs: include_refs.clone(),
                        exclude_refs: exclude_refs.clone(),
                        branches: if *no_rewrite {
                            Vec::new()
                        } else {
                            operands.clone()
                        },
                        skip_fetch: *skip_fetch,
                    },
                    yes: *yes,
                    verbose: *verbose,
                    from_crab: *from_crab,
                })?;
            }
            LfsMigrateCmd::Export {
                include,
                exclude,
                object_map,
                remote,
                everything,
                include_refs,
                exclude_refs,
                skip_fetch,
                yes,
                verbose,
                to_crab,
                branches,
            } => {
                migrate::run_migrate_export(migrate::LfsMigrateExportOptions {
                    include: include.clone(),
                    exclude: exclude.clone(),
                    object_map: object_map.clone(),
                    remote: remote.clone(),
                    refs: migrate::LfsMigrateRefSelection {
                        everything: *everything,
                        include_refs: include_refs.clone(),
                        exclude_refs: exclude_refs.clone(),
                        branches: branches.clone(),
                        skip_fetch: *skip_fetch,
                    },
                    yes: *yes,
                    verbose: *verbose,
                    to_crab: *to_crab,
                })?;
            }
            LfsMigrateCmd::Info {
                above,
                include,
                exclude,
                everything,
                include_refs,
                exclude_refs,
                skip_fetch,
                top,
                unit,
                pointers,
                fixup,
                branches,
            } => {
                migrate::run_migrate_info(migrate::LfsMigrateInfoOptions {
                    above: above.clone(),
                    include: include.clone(),
                    exclude: exclude.clone(),
                    top: *top,
                    unit: unit.clone(),
                    pointers: pointers.clone(),
                    fixup: *fixup,
                    refs: migrate::LfsMigrateRefSelection {
                        everything: *everything,
                        include_refs: include_refs.clone(),
                        exclude_refs: exclude_refs.clone(),
                        branches: branches.clone(),
                        skip_fetch: *skip_fetch,
                    },
                })?;
            }
        },
        LfsCmd::Pointer {
            file,
            pointer,
            stdin,
            check,
            strict,
            no_strict,
        } => {
            return pointer::run_lfs_pointer(
                file.as_deref(),
                pointer.as_deref(),
                *stdin,
                *check,
                *strict,
                *no_strict,
            );
        }
        LfsCmd::Clean { path } => {
            standalone::run_lfs_clean(path.as_deref())?;
        }
        LfsCmd::Smudge { path, skip } => {
            standalone::run_lfs_smudge(path.as_deref(), *skip)?;
        }
        LfsCmd::Completion { shell } => {
            let mut cmd = completion::lfs_completion_command();
            return completion::run_lfs_completion(shell, &mut cmd);
        }
        LfsCmd::Ext { command } => {
            let names = match command {
                Some(LfsExtCmd::List { names }) => names.as_slice(),
                None => &[],
            };
            return ext::run_lfs_ext(names);
        }
        LfsCmd::FilterProcess { skip } => return filter_process::run_lfs_filter_process(*skip),
        LfsCmd::MergeDriver {
            ancestor,
            current,
            other,
            marker_size,
            output,
            program,
        } => {
            return merge_driver::run_lfs_merge_driver(merge_driver::LfsMergeDriverOptions {
                ancestor: ancestor.clone(),
                current: current.clone(),
                other: other.clone(),
                marker_size: *marker_size,
                output: output.clone(),
                program: program.clone(),
            });
        }
        LfsCmd::PostCheckout { args } => return hooks::run_post_checkout(args),
        LfsCmd::PostCommit { args } => return hooks::run_post_commit(args),
        LfsCmd::PostMerge { args } => return hooks::run_post_merge(args),
        LfsCmd::StandaloneFile { args } => return standalone_file::run_lfs_standalone_file(args),
        LfsCmd::Env => {
            env::run_lfs_env()?;
        }
        LfsCmd::Version => {
            env::run_lfs_version()?;
        }
        LfsCmd::Dedup {
            dry_run,
            test,
            crab_cache,
        } => {
            dedup::run_lfs_dedup_with_cancel(
                dedup::LfsDedupOptions {
                    dry_run: *dry_run,
                    test: *test,
                    crab_cache: *crab_cache,
                },
                cancel,
            )?;
        }
        LfsCmd::Logs {
            transfer_history,
            last,
            clear,
            args,
        } => {
            logs::run_lfs_logs(logs::LfsLogsOptions {
                args: args.clone(),
                transfer_history: *transfer_history,
                last: *last,
                clear: *clear,
            })?;
        }
        LfsCmd::Convert {
            from,
            to,
            pattern,
            dry_run,
            rollback,
        } => {
            let cwd = std::env::current_dir()?;
            let repo_root = crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)?
                .current_worktree_root;
            if *rollback {
                convert::run_rollback_with_cancel(&repo_root, cancel)?;
                println!("Rollback complete.");
                return Ok(std::process::ExitCode::SUCCESS);
            }
            let pattern = pattern.as_deref().unwrap_or("*");
            let direction = match (from.as_deref(), to.as_deref()) {
                (Some("lfs"), Some("xet")) => convert::ConvertDirection::LfsToXet,
                (Some("xet"), Some("lfs")) => convert::ConvertDirection::XetToLfs,
                _ => {
                    eprintln!("Usage: crab lfs convert --from lfs --to xet <pattern>");
                    eprintln!("       crab lfs convert --from xet --to lfs <pattern>");
                    return Ok(std::process::ExitCode::FAILURE);
                }
            };
            convert::run_convert_with_cancel(direction, pattern, *dry_run, &repo_root, cancel)?;
        }
        LfsCmd::LifecyclePolicy {
            backend,
            expire_days,
        } => {
            let policy =
                crate::lfs::lifecycle::generate_lifecycle_policy(backend, "lfs", *expire_days);
            println!("{policy}");
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: LfsCmd,
    }

    #[test]
    fn migrate_include_accepts_short_alias() {
        let import = TestCli::parse_from(["crab", "migrate", "import", "-I", "*.bin"]);
        match import.command {
            LfsCmd::Migrate(LfsMigrateCmd::Import { include, .. }) => {
                assert_eq!(include.as_deref(), Some("*.bin"));
            }
            _ => panic!("expected migrate import"),
        }

        let info = TestCli::parse_from(["crab", "migrate", "info", "-I", "*.dat"]);
        match info.command {
            LfsCmd::Migrate(LfsMigrateCmd::Info { include, .. }) => {
                assert_eq!(include.as_deref(), Some("*.dat"));
            }
            _ => panic!("expected migrate info"),
        }

        let export = TestCli::parse_from(["crab", "migrate", "export", "-I", "*.psd"]);
        match export.command {
            LfsCmd::Migrate(LfsMigrateCmd::Export { include, .. }) => {
                assert_eq!(include, "*.psd");
            }
            _ => panic!("expected migrate export"),
        }
    }

    #[test]
    fn migrate_no_rewrite_accepts_ignored_rewrite_options() {
        let parsed = TestCli::parse_from([
            "crab",
            "migrate",
            "import",
            "--no-rewrite",
            "--include",
            "*.ignored",
            "--above",
            "1b",
            "--object-map",
            "map.csv",
            "--include-ref",
            "HEAD",
            "--skip-fetch",
            "--message",
            "import files",
            "file.bin",
        ]);

        match parsed.command {
            LfsCmd::Migrate(LfsMigrateCmd::Import {
                no_rewrite,
                include,
                above,
                object_map,
                include_refs,
                skip_fetch,
                operands,
                ..
            }) => {
                assert!(no_rewrite);
                assert_eq!(include.as_deref(), Some("*.ignored"));
                assert_eq!(above.as_deref(), Some("1b"));
                assert_eq!(object_map.as_deref(), Some("map.csv"));
                assert_eq!(include_refs, vec!["HEAD"]);
                assert!(skip_fetch);
                assert_eq!(operands, vec!["file.bin"]);
            }
            _ => panic!("expected migrate import"),
        }
    }

    #[test]
    fn track_dry_run_accepts_short_alias() {
        let parsed = TestCli::parse_from(["crab", "track", "-d", "*.bin"]);

        match parsed.command {
            LfsCmd::Track {
                patterns, dry_run, ..
            } => {
                assert_eq!(patterns, vec!["*.bin"]);
                assert!(dry_run);
            }
            _ => panic!("expected track"),
        }
    }

    #[test]
    fn untrack_accepts_multiple_patterns() {
        let parsed = TestCli::parse_from(["crab", "untrack", "*.bin", "*.psd"]);

        match parsed.command {
            LfsCmd::Untrack { patterns } => {
                assert_eq!(patterns, vec!["*.bin", "*.psd"]);
            }
            _ => panic!("expected untrack"),
        }
    }

    #[test]
    fn status_accepts_git_lfs_short_aliases() {
        let json = TestCli::parse_from(["crab", "status", "-j"]);
        match json.command {
            LfsCmd::Status {
                json, porcelain, ..
            } => {
                assert!(json);
                assert!(!porcelain);
            }
            _ => panic!("expected status"),
        }

        let porcelain = TestCli::parse_from(["crab", "status", "-p"]);
        match porcelain.command {
            LfsCmd::Status {
                json, porcelain, ..
            } => {
                assert!(!json);
                assert!(porcelain);
            }
            _ => panic!("expected status"),
        }
    }
}
