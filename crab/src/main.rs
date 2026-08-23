//! Binary entry point for `crab` and (via symlink) `git-remote-crab`.
//!
//! Git invokes remote helpers by exec-ing an executable named
//! `git-remote-<transport>` with argv[0] set to that name, so the same
//! binary wears two hats: the user-facing `crab` CLI, and the remote
//! helper Git shells out to for `crab://` URLs. We pick the branch off
//! argv[0]'s file stem rather than a subcommand because Git controls that
//! invocation, not the user.
//!
//! Symlinks like `crab-gc`, `crab-fsck`, `crab-init` etc. also

// The full-pipeline async state machine layout exceeds the default
// rustc recursion limit after the two-DB metadata wiring. Bumping to
// 512 covers the current depth without materially increasing compile
// time.
#![recursion_limit = "512"]

//! dispatch to the corresponding subcommand without requiring the user
//! to type it — `crab-gc` behaves identically to `crab gc`.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![warn(clippy::perf, clippy::pedantic)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

#[cfg(unix)]
use std::os::fd::AsRawFd;

use clap::{CommandFactory, Parser, Subcommand};
use tokio::io::{BufReader, Stdin, Stdout};
use tokio_util::sync::CancellationToken;

use crab::core::config::Config;
use crab::core::context::AppContext;
use crab::core::error::{CrabError, Result};
use crab::core::output::{ErrorInfo, JsonlStream, OutputMode, emit_error_json};
use crab::git::filter_process::run_filter_process;
use crab::git::remote_helper::{StdIo, run_remote_helper};

/// Name Git uses when exec-ing the remote helper for `crab://` URLs.
const REMOTE_HELPER_STEM: &str = "git-remote-crab";

/// macOS FUSE mount binary while `crab` stays loader-safe.
const FUSE_MOUNT_STEM: &str = "crab-fuse-mount";

/// NFS mount helper binary.
const NFS_MOUNT_STEM: &str = "crab-nfs-mount";

/// Prefix for symlink-based subcommand dispatch (e.g. `crab-gc` → `gc`).
const SYMLINK_PREFIX: &str = "crab-";

/// Default parallelism for file-processing commands that stream local files.
const DEFAULT_FILE_PROCESSING_JOBS: usize = 16;

/// The full Clap command tree exceeds the default Windows main-thread stack.
const CLI_STACK_SIZE: usize = 16 * 1024 * 1024;

#[derive(Parser)]
#[command(
    name = "crab",
    version,
    about = "Serverless Git for large files, datasets, and reproducible workflows"
)]
struct Cli {
    /// Set the log verbosity level.
    ///
    /// Accepts any `tracing` filter directive: `error`, `warn`, `info`,
    /// `debug`, `trace`, or a module-level filter like
    /// `crab::engine=debug`. Overrides the `CRAB_LOG` env var.
    #[arg(long, global = true)]
    log_level: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Guided cloud, credential, repository, and large-file setup.
    Configure {
        /// Remote URL, or omit it for an interactive prompt.
        #[arg(value_name = "REMOTE")]
        remote: Option<String>,
        /// Cloud storage provider.
        #[arg(long, value_name = "PROVIDER", value_parser = ["s3", "gcs", "azure"])]
        provider: Option<String>,
        /// Bucket-GC listing policy configured locally for this operator.
        #[arg(long, value_name = "PROFILE", value_parser = ["adaptive", "cost", "latency"])]
        gc_list_profile: Option<String>,
        /// Track an explicit large-file pattern (can repeat).
        #[arg(long = "track", value_name = "PATTERN")]
        track: Vec<String>,
        /// Install Crab without scanning for large files.
        #[arg(long)]
        no_auto_track: bool,
        /// Preview the setup plan without changing files or Git config.
        #[arg(long)]
        dry_run: bool,
    },
    /// Initialize a new crab repository at a remote URL.
    Init {
        /// Remote URL to initialize (e.g. `crab://bucket/repo`).
        /// If omitted, re-applies configuration from an existing `.crab.toml`.
        url: Option<String>,
        /// Storage backend used by the Crab remote.
        #[arg(long, value_name = "PROVIDER", value_parser = ["s3", "gcs", "azure", "auto"])]
        storage_provider: Option<String>,
        /// Bucket-GC listing policy configured locally for this operator.
        #[arg(long, value_name = "PROFILE", value_parser = ["adaptive", "cost", "latency"])]
        gc_list_profile: Option<String>,
        /// Enable mirror mode: sync large files to Crab transparently on push.
        /// Value is the name of the existing git remote (typically "origin").
        #[arg(long)]
        mirror: Option<String>,
        /// Emit structured JSON output.
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Emit streaming JSONL output.
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Configure large-file tracking for a repository.
    ///
    /// Configure large-file tracking for a repository.
    ///
    /// Scans for large files, writes `.gitattributes` patterns, installs
    /// the filter driver, and updates `.crab.toml`. Run after `crab init`
    /// to complete repository setup.
    Setup {
        /// Skip scanning for large files; only install the filter driver.
        #[arg(long)]
        no_auto_track: bool,
        /// Explicit patterns to track instead of auto-detecting.
        #[arg(long = "track", value_name = "PATTERN")]
        track: Vec<String>,
        /// Only scan these subdirectories (can repeat).
        #[arg(long = "include", value_name = "DIR")]
        include: Vec<String>,
        /// Skip these subdirectories during scanning (can repeat).
        #[arg(long = "exclude", value_name = "DIR")]
        exclude: Vec<String>,
        /// Preview changes without writing anything to disk.
        #[arg(long)]
        dry_run: bool,
        /// Replace existing crab entries instead of appending.
        #[arg(long)]
        force: bool,
        /// Emit structured JSON output.
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Emit streaming JSONL output.
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Stage files for crab, bypassing git's serial filter protocol.
    ///
    /// Processes matching files in parallel: hash + CDC chunk + stage +
    /// write pointer. Much faster than `git add` for many large files.
    Add {
        /// Glob patterns to add (e.g. `*.safetensors`, `models/`).
        #[arg(required = true)]
        patterns: Vec<String>,
        /// Maximum number of concurrent file-processing tasks.
        #[arg(long, short, default_value_t = DEFAULT_FILE_PROCESSING_JOBS)]
        jobs: usize,
        /// Show what would be added without staging or writing pointers.
        #[arg(long)]
        dry_run: bool,
        /// Skip the final `git add` step (stage chunks only).
        #[arg(long)]
        skip_git_add: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Unstage files from git's index and clean crab staging data.
    ///
    /// Mirrors `git reset HEAD -- <paths>` and also removes the
    /// corresponding chunk data from the local staging area.
    Reset {
        /// Glob patterns to unstage (e.g. `*.safetensors`, `models/`).
        patterns: Vec<String>,
        /// Show what would be unstaged without modifying anything.
        #[arg(long)]
        dry_run: bool,
        /// Scan for files removed from git's index and clean orphaned
        /// staging data. Use after `git reset`, `git rm`, etc.
        #[arg(long)]
        sync: bool,
    },
    /// Clone a repository; crab:// remotes also get filter setup and optional hydration.
    Clone {
        /// Remote URL or path (e.g. `crab://bucket/repo` or a Git URL).
        url: String,
        /// Target directory (defaults to repo name from URL).
        #[arg(value_name = "DIR")]
        directory: Option<std::path::PathBuf>,
        /// Branch to check out.
        #[arg(long, short)]
        branch: Option<String>,
        /// Shallow clone depth.
        #[arg(long)]
        depth: Option<u32>,
        /// Leave files as pointers (default). Use --no-lazy for full hydration.
        #[arg(long, default_value = "true")]
        lazy: bool,
        /// Hydrate everything immediately.
        #[arg(long = "no-lazy")]
        no_lazy: bool,
        /// Hydrate everything immediately (convenience alias for --no-lazy).
        #[arg(long)]
        eager: bool,
        /// Glob patterns to hydrate immediately after clone.
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// Glob patterns to exclude from post-clone hydration.
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Warm the local chunk-index cache after clone.
        ///
        /// Disabled by default because it is not required for clone correctness.
        #[arg(long = "sync-chunk-index")]
        sync_chunk_index: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Mirror a Git remote into a Crab remote.
    Mirror(crab::cmd::mirror::MirrorArgs),
    /// Download selected files from a Crab repository without cloning it.
    #[command(visible_alias = "get")]
    Download {
        /// Remote object URL or local repository path.
        repo: String,
        /// Exact repo-relative files, or trailing-slash subtree selectors.
        #[arg(value_name = "PATHS")]
        paths: Vec<String>,
        /// Branch, tag, ref, or full commit SHA to read from.
        #[arg(long)]
        revision: Option<String>,
        /// Glob patterns to include.
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// Glob patterns to exclude.
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Override Crab's cache root.
        #[arg(long, value_name = "DIR")]
        cache_dir: Option<std::path::PathBuf>,
        /// Write files under this local directory.
        #[arg(long, value_name = "DIR")]
        local_dir: Option<std::path::PathBuf>,
        /// Download again even when the destination is already fresh.
        #[arg(long)]
        force_download: bool,
        /// Preview selected files without writing destinations or metadata.
        #[arg(long)]
        dry_run: bool,
        /// Maximum parallel file downloads.
        #[arg(long, value_name = "N")]
        max_workers: Option<usize>,
        /// Suppress human progress and summary; still prints final paths.
        #[arg(long)]
        quiet: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Manage Git worktrees with Crab-aware extensions.
    Worktree {
        /// Structured JSON output for Crab worktree commands.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: crab::cmd::worktree::WorktreeCommand,
    },
    /// Run a comprehensive health check on the crab setup.
    Doctor {
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
        /// Run the cost optimizer report instead of the standard health check.
        #[arg(long, conflicts_with_all = ["metadb", "support_bundle"])]
        cost: bool,
        /// Report `SlateDB` metadata state instead of the standard health check.
        #[arg(long, conflicts_with_all = ["cost", "support_bundle"])]
        metadb: bool,
        /// Collect a redacted cache-service support bundle.
        #[arg(long, conflicts_with_all = ["cost", "metadb"])]
        support_bundle: bool,
        /// Run an opt-in cache-service write/read/cleanup probe.
        #[arg(long, conflicts_with_all = ["cost", "metadb", "support_bundle"])]
        cache_service_active_probe: bool,
        /// Write the support bundle as pretty JSON.
        #[arg(long, value_name = "PATH", requires = "support_bundle")]
        output: Option<PathBuf>,
        /// Path to a pricing override YAML file (used with --cost).
        #[arg(long, value_name = "PATH")]
        pricing_file: Option<String>,
        /// Inventory source: auto, live, or report (used with --cost).
        #[arg(long, value_name = "SOURCE")]
        inventory_source: Option<String>,
        /// Sample ratio for live inventory (0.0-1.0, used with --cost).
        #[arg(long, value_name = "RATIO")]
        sample: Option<f64>,
        /// Number of heaviest cold objects to report (used with --cost).
        #[arg(long, value_name = "K")]
        top_k: Option<usize>,
    },
    /// Show disk usage breakdown for crab-managed storage.
    Du {
        /// Include remote storage size (requires network access).
        #[arg(long)]
        remote: bool,
        /// Machine-readable JSON output.
        #[arg(long, short)]
        json: bool,
    },
    /// Track files matching a glob pattern for crab storage.
    ///
    /// Without arguments, lists currently tracked patterns.
    Track {
        /// Glob pattern to track (e.g. `*.bin`). Omit to list.
        glob: Option<String>,
        /// List currently tracked patterns (same as omitting the glob).
        #[arg(long)]
        list: bool,
        /// Structured JSON output (envelope format, list mode only).
        #[arg(long)]
        json: bool,
    },
    /// Stop tracking files matching a glob pattern.
    Untrack {
        /// Glob pattern to untrack.
        glob: String,
    },
    /// Print staging area statistics, or perf counters with `stat perf`.
    Stat {
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        sub: Option<StatCmd>,
    },
    /// Run garbage collection on the remote store.
    Gc {
        /// List unreachable objects without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Bypass the grace period — delete all unreachable objects.
        #[arg(long)]
        force: bool,
        /// Skip interactive confirmation when --force is used.
        #[arg(long)]
        yes: bool,
        /// GC scope: `repo` (per-repo local cache) or `bucket` (global GC).
        #[arg(long, default_value = "repo")]
        scope: String,
        /// S3 bucket name (required for --scope=bucket).
        #[arg(long)]
        bucket: Option<String>,
        /// Override the bucket-GC listing policy.
        #[arg(long, value_name = "PROFILE", value_parser = ["adaptive", "cost", "latency"])]
        list_profile: Option<String>,
        /// Minimum age before unreferenced objects are deleted (e.g. `1h`, `24h`).
        /// Defaults to the resolved `gc_grace_period` config value.
        #[arg(long)]
        grace_period: Option<String>,
        /// Resume an interrupted destructive GC run by its durable UUIDv7 id.
        #[arg(long, value_name = "RUN_ID")]
        resume: Option<String>,
        /// Remove a repo's entry from the ref-registry.
        #[arg(long, conflicts_with = "repair_registry")]
        deregister: Option<String>,
        /// Rebuild the bucket ref-registry from every current repo manifest.
        #[arg(long, conflicts_with = "deregister")]
        repair_registry: bool,
        /// Backfill and verify durable shard reachability closures.
        #[arg(long, conflicts_with_all = ["deregister", "repair_registry", "resume"])]
        repair_closures: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Check repository integrity.
    Fsck {
        /// Attempt safe repairs for detected issues.
        #[arg(long)]
        repair: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Compact shards: merge small shards into fewer large ones.
    Compact {
        /// Repo prefix (e.g. `org/models`).
        #[arg(long)]
        repo: String,
        /// S3 bucket name.
        #[arg(long)]
        bucket: String,
        /// Report what would happen without mutating.
        #[arg(long)]
        dry_run: bool,
        /// Maximum compacted shard size (e.g. `100MiB`, `50MB`).
        #[arg(long, default_value = "100MiB")]
        max_shard_size: String,
    },
    /// Consolidate remote Git pack files.
    Repack {
        /// Report pack statistics without modifying the remote.
        #[arg(long)]
        dry_run: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Optimize Crab storage layout, caches, indexes, and replicas.
    #[command(subcommand)]
    Optimize(OptimizeCmd),
    /// Manage lifecycle tiering rules for the bucket.
    #[command(subcommand)]
    Tier(crab::cmd::tier::TierCommand),
    /// Metadata database administration: diagnose, rebuild, compact,
    /// and cache control.
    #[command(subcommand)]
    Metadb(crab::cmd::metadb::MetadbCommand),
    /// Manage the local chunk cache.
    #[command(subcommand)]
    Cache(CacheCmd),
    /// Manage the staging area.
    #[command(subcommand)]
    Staging(StagingCmd),
    /// Look up a crab error code.
    Errors {
        /// Error code to look up (e.g. `CRAB-E0017`).
        code: Option<String>,
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
    /// Print version information.
    Version {
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
    /// List and install the self-contained skills shipped with Crab.
    #[command(subcommand)]
    Skills(crab::cmd::skills::SkillsCommand),
    /// Update crab from the latest GitHub release.
    Update {
        /// Only check whether an update is available.
        #[arg(long)]
        check: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
        /// Reinstall the latest release even when versions match.
        #[arg(long)]
        force: bool,
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
    /// Manage crab configuration.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Manage read replicas for Crab remotes.
    #[command(subcommand)]
    Replica(crab::cmd::replica::ReplicaCommand),
    /// Generate shell completion scripts.
    Completions {
        /// Shell to generate completions for (bash, zsh, fish, powershell).
        shell: String,
        /// Write the completion script to the shell-specific directory.
        #[arg(long)]
        install: bool,
    },
    /// Report hydration state of the working tree.
    Status {
        /// Machine-readable output (one line per file).
        #[arg(long)]
        porcelain: bool,
        /// Report workflow stage state instead of hydration state.
        #[arg(long, conflicts_with = "porcelain")]
        workflow: bool,
        /// Drill into one workflow stage and explain why it is stale.
        #[arg(long, value_name = "STAGE", requires = "workflow")]
        why: Option<String>,
        /// Discover nested workflow files when using `--workflow`.
        #[arg(long, short = 'R', requires = "workflow")]
        recursive: bool,
        /// Workflow lockfile path when using `--workflow`.
        #[arg(long, value_name = "PATH", requires = "workflow")]
        lockfile: Option<std::path::PathBuf>,
        /// Include upstream stages for any selected workflow targets.
        #[arg(long, short = 'd', requires = "workflow", conflicts_with = "why")]
        with_deps: bool,
        /// Compare local stage-cache entries with the configured Crab remote.
        #[arg(long, short = 'c', requires = "workflow", conflicts_with = "why")]
        cloud: bool,
        /// Crab git remote name to compare against. Implies `--cloud`.
        #[arg(
            long,
            short = 'r',
            value_name = "NAME",
            requires = "workflow",
            conflicts_with = "why"
        )]
        remote: Option<String>,
        /// Structured JSON output (envelope format).
        #[arg(long, conflicts_with = "porcelain")]
        json: bool,
        /// Workflow stage names or declared output paths to inspect.
        #[arg(value_name = "TARGET", requires = "workflow", conflicts_with = "why")]
        targets: Vec<String>,
    },
    /// Explain why a file is tracked and its current hydration state.
    Why {
        /// File path to inspect.
        file: String,
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
    /// Materialize pointer files into full content.
    Hydrate {
        /// Glob patterns to hydrate (e.g. `*.safetensors`).
        #[arg(value_name = "GLOBS")]
        patterns: Vec<String>,
        /// Additional include patterns (composable).
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// Exclude patterns (subtract from includes).
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Hydrate all tracked pointer files.
        #[arg(long)]
        all: bool,
        /// Path to a newline-delimited manifest file, or `-` for stdin.
        #[arg(long, value_name = "PATH", conflicts_with_all = ["manifest_ref", "profile"])]
        manifest: Option<String>,
        /// Read the manifest from a Git ref (e.g. `HEAD:.crab/manifests/ci.txt`).
        #[arg(long, value_name = "REF", conflicts_with_all = ["manifest", "profile"])]
        manifest_ref: Option<String>,
        /// Named prefetch profile from `.crab/prefetch.toml`.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["manifest", "manifest_ref"])]
        profile: Option<String>,
        /// Ignore sparse-checkout config during hydrate.
        ///
        /// When the `gix-worktree` feature gates on the worktree-state
        /// integration, `crab hydrate --all` honors `.git/info/sparse-checkout`
        /// by default. Pass `--ignore-sparse` to restore the legacy
        /// "everything" behavior that pre-adoption hydrate always used.
        #[arg(long)]
        ignore_sparse: bool,
        /// Recover from a local source directory or file before fetching from the remote.
        ///
        /// For each pointer, look for a candidate file (either the
        /// supplied path itself when it's a regular file, or
        /// `<dir>/<basename>` when it's a directory). If the candidate's
        /// blake3 hash matches the pointer's `file-hash`, copy it into
        /// place and skip the remote fetch entirely. Pointers without
        /// a matching candidate fall through to the normal hydrate
        /// path. Useful when the remote is missing data (incomplete
        /// push, partial GC) but the original files are still
        /// available on local disk or another mounted volume.
        #[arg(long = "recover-from", value_name = "PATH")]
        recover_from: Option<std::path::PathBuf>,
        /// Restore archived xorbs before downloading them.
        #[arg(long, conflicts_with = "no_restore")]
        restore: bool,
        /// Fail instead of restoring archived xorbs.
        #[arg(long = "no-restore", conflicts_with = "restore")]
        no_restore: bool,
        /// Restore tier for archived xorbs.
        #[arg(long = "restore-tier", value_name = "TIER")]
        restore_tier: Option<String>,
        /// Number of days restored archive copies remain readable.
        #[arg(long = "restore-duration-days", value_name = "DAYS")]
        restore_duration_days: Option<u32>,
        /// Wipe the speculation database and exit.
        #[arg(long)]
        clear_speculation: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Chunk-level diff between two git refs.
    ///
    /// Compares crab-tracked files using only metadata (file-index + shards),
    /// showing which chunks changed, bytes affected, and reuse ratio — with
    /// zero data transfer.
    Diff {
        /// First git ref (branch, tag, SHA, HEAD~N).
        ref1: String,
        /// Second git ref (defaults to HEAD when omitted).
        ref2: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
        /// Summary-only output.
        #[arg(long)]
        stat: bool,
        /// List only changed file names.
        #[arg(long)]
        name_only: bool,
        /// Show per-segment detail (xorb hashes, chunk ranges, sizes).
        #[arg(long)]
        verbose: bool,
        /// Show changed byte offset ranges within each file.
        #[arg(long)]
        byte_ranges: bool,
        /// Disable colored output.
        #[arg(long)]
        no_color: bool,
        /// Disable format-aware annotations.
        #[arg(long)]
        no_annotations: bool,
        /// Restrict diff to specific paths.
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// Git external diff driver for chunk-level diffs.
    ///
    /// Conforms to git's external diff driver protocol. Not intended for
    /// direct user invocation — register via .gitattributes and .git/config.
    #[command(name = "diff-driver", hide = true)]
    DiffDriver {
        /// File path being diffed.
        path: String,
        /// Path to the old file version.
        old_file: std::path::PathBuf,
        /// Old file hex hash.
        old_hex: String,
        /// Old file mode.
        old_mode: String,
        /// Path to the new file version.
        new_file: std::path::PathBuf,
        /// New file hex hash.
        new_hex: String,
        /// New file mode.
        new_mode: String,
    },
    /// Replace hydrated files with pointer blobs, freeing disk space.
    Dehydrate {
        /// Glob patterns to dehydrate (e.g. `*.safetensors`).
        #[arg(value_name = "GLOBS")]
        patterns: Vec<String>,
        /// Dehydrate all tracked hydrated files.
        #[arg(long)]
        all: bool,
        /// Ignore prefetch profiles — dehydrate even files protected by the `always` profile.
        #[arg(long)]
        ignore_profiles: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Print diagnostic environment information.
    Env {
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
    /// List files tracked by crab with their hydration state.
    LsFiles {
        /// Show full 64-char hashes.
        #[arg(long, short)]
        long: bool,
        /// Show file sizes.
        #[arg(long, short)]
        size: bool,
        /// Show only file names.
        #[arg(long, short)]
        name_only: bool,
        /// Machine-readable JSON output.
        #[arg(long, short)]
        json: bool,
        /// Show all fields for debugging.
        #[arg(long, short)]
        debug: bool,
    },
    /// Pre-fetch objects from the remote into the local cache.
    Fetch {
        /// Include patterns (e.g. `*.safetensors`).
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// Exclude patterns.
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Fetch objects for all refs, not just HEAD.
        #[arg(long)]
        all: bool,
        /// Report what would be fetched without downloading.
        #[arg(long)]
        dry_run: bool,
        /// Skip the local chunk-index warm-up that normally runs after
        /// packs are on disk. Intended for CI workloads that push once
        /// and never read back.
        #[arg(long = "no-sync-chunk-index")]
        no_sync_chunk_index: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Evict local cache objects until configured budgets are satisfied.
    Prune {
        /// Report what would be pruned without deleting.
        #[arg(long)]
        dry_run: bool,
        /// Print each object as it is pruned.
        #[arg(long)]
        verbose: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Manage diagnostic log files.
    #[command(subcommand)]
    Logs(LogsCmd),
    /// Install the crab filter driver in git config.
    Install {
        /// Install globally (all repos for this user).
        #[arg(long)]
        global: bool,
        /// Install system-wide.
        #[arg(long)]
        system: bool,
        /// Overwrite existing config.
        #[arg(long, short)]
        force: bool,
        /// Skip the smudge filter (defer hydration).
        #[arg(long)]
        skip_smudge: bool,
        /// Install git aliases (git ship, git crab-status, git crab-hydrate).
        #[arg(long)]
        aliases: bool,
        /// Skip shell completion installation (for CI/headless environments).
        #[arg(long)]
        no_completions: bool,
    },
    /// Remove the crab filter driver from git config.
    Uninstall {
        /// Remove from global config.
        #[arg(long)]
        global: bool,
        /// Remove from system config.
        #[arg(long)]
        system: bool,
    },
    /// Acquire an advisory lock on one or more files.
    Lock {
        /// File paths to lock.
        #[arg(required = true)]
        paths: Vec<String>,
        /// Machine-readable JSON output.
        #[arg(long, short)]
        json: bool,
    },
    /// Release an advisory lock on one or more files.
    Unlock {
        /// File paths to unlock.
        #[arg(required = true)]
        paths: Vec<String>,
        /// Force-break another user's lock.
        #[arg(long, short)]
        force: bool,
        /// Machine-readable JSON output.
        #[arg(long, short)]
        json: bool,
    },
    /// List active advisory file locks.
    Locks {
        /// Filter by file path.
        #[arg(long, short)]
        path: Option<String>,
        /// Filter by owner ("self" for your own locks).
        #[arg(long, short = 'o')]
        owner: Option<String>,
        /// Maximum number of locks to display.
        #[arg(long, short)]
        limit: Option<usize>,
        /// Machine-readable JSON output.
        #[arg(long, short)]
        json: bool,
    },
    /// Rewrite history to move files into or out of crab tracking.
    #[command(subcommand)]
    Migrate(MigrateCmd),
    /// Manage immutable workflow artifacts and promotion labels.
    #[command(subcommand)]
    Artifacts(crab::cmd::artifacts::ArtifactsCommand),
    /// Native concurrent push that bypasses git's serial remote helper protocol.
    Push(crab::cmd::push::PushArgs),
    /// Git pull + automatic hydration of newly-fetched pointer blobs.
    ///
    /// Wraps `git pull` and conditionally hydrates any new pointer files
    /// that match the hydration filter. A symmetric counterpart to `crab ship`.
    Pull {
        /// Remote name (default: origin).
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Branch to pull (default: current branch).
        #[arg(long)]
        branch: Option<String>,
        /// Skip automatic hydration after pulling.
        #[arg(long)]
        no_hydrate: bool,
        /// Structured JSON output.
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output.
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// One-shot add + commit + push for quick file uploads.
    ///
    /// Combines `crab add`, `git commit`, and `crab push` into a single
    /// command. Ideal for ML/data workflows where you just want files
    /// in the cloud without thinking in git primitives.
    Ship {
        /// Glob patterns to ship (e.g. `*.safetensors`, `.`).
        /// Defaults to `.` (all files) when omitted.
        patterns: Vec<String>,
        /// Commit message.
        #[arg(long, short)]
        message: String,
        /// Maximum number of concurrent file-processing tasks.
        #[arg(long, short = 'j', default_value_t = DEFAULT_FILE_PROCESSING_JOBS)]
        jobs: usize,
        /// Push to this Git remote name or crab:// URL. Auto-detects a Crab
        /// remote when omitted.
        #[arg(long, value_name = "REMOTE")]
        remote: Option<String>,
        /// Push to this branch (default: current branch).
        #[arg(long, short)]
        branch: Option<String>,
        /// Integrate the current branch and retry after non-fast-forward or lock contention.
        #[arg(long)]
        rebase_on_non_fast_forward: bool,
        /// Maximum integration retry attempts for --rebase-on-non-fast-forward.
        #[arg(long, default_value = "256", requires = "rebase_on_non_fast_forward")]
        rebase_retry_limit: u32,
        /// Skip the push step (just add + commit).
        #[arg(long)]
        no_push: bool,
        /// Show what would be shipped without making changes.
        #[arg(long)]
        dry_run: bool,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Convert existing large files to crab pointers in the working tree.
    ///
    /// Walks the working tree for files matching patterns, replaces them
    /// with pointer blobs, and stages the original content as chunks.
    /// Use `--dry-run` to preview what would be converted.
    Adopt {
        /// Glob patterns to adopt (e.g. `*.bin`, `*.safetensors`).
        #[arg(long, short)]
        pattern: Vec<String>,
        /// Rewrite git history (requires --force). Not yet implemented.
        #[arg(long)]
        rewrite_history: bool,
        /// Required with --rewrite-history.
        #[arg(long)]
        force: bool,
        /// Show what would be converted without making changes.
        #[arg(long)]
        dry_run: bool,
        /// Show candidate files and prompt for confirmation before converting.
        #[arg(long, short)]
        interactive: bool,
        /// Maximum number of concurrent file-processing tasks.
        #[arg(long, short = 'j', default_value_t = DEFAULT_FILE_PROCESSING_JOBS)]
        jobs: usize,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Restore pointer files back to their full content using staged chunks.
    ///
    /// Reverses a `crab adopt` operation before commit: reads pointer blobs,
    /// retrieves the original chunks from the staging area, writes the full
    /// content back to disk, and unstages the files from the git index.
    Unadopt {
        /// Glob patterns to match pointer files for restoration.
        #[arg(long, short)]
        pattern: Vec<String>,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Undo the last crab operation (adopt, add) detected from staged changes.
    ///
    /// Inspects git staged changes for pointer files and delegates to the
    /// unadopt logic to restore them. If no reversible operation is detected,
    /// returns an error.
    Undo {
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Import a raw object-storage prefix into a fresh Crab-backed git repo.
    Import(crab::cmd::import::ImportArgs),
    /// Manage versioned external data sources and source provenance.
    #[command(subcommand)]
    Data(crab::cmd::data::DataCommand),
    /// Export one Crab snapshot as materialized files into raw object storage.
    Export(crab::cmd::export::ExportArgs),
    /// Execute a single workflow stage with content-addressed caching.
    ///
    /// Computes a deterministic hash over the command, deps, params and
    /// env, looks it up in the local stage cache, and either replays the
    /// cached outputs or runs the command. Workflows are enabled by default;
    /// `[workflow] enabled = false` is an explicit opt-out.
    Run(crab::cmd::run::RunArgs),
    /// DVC-compatible spelling for `crab run`.
    ///
    /// Reproduces complete or partial workflows using the same target
    /// selection, cache, lockfile, and output behavior as `crab run`.
    Repro(crab::cmd::run::RunArgs),
    /// DVC-style workflow stage helper commands.
    #[command(subcommand)]
    Stage(StageCmd),
    /// Freeze workflow stages so `crab run` skips them until unfreezing.
    Freeze(crab::cmd::freeze::FreezeArgs),
    /// Unfreeze workflow stages so `crab run` can execute them again.
    Unfreeze(crab::cmd::freeze::FreezeArgs),
    /// Experiment management: run, show, diff, promote, gc, ls.
    ///
    /// Each experiment is a DAG run against a throwaway worktree with
    /// optional `--set key=value` parameter overrides. Metadata is
    /// persisted under `.crab/workflow/exp/<uuid>.meta.json`.
    #[command(subcommand)]
    Exp(ExpCmd),
    /// DVC-style experiment task queue management.
    #[command(subcommand)]
    Queue(QueueCmd),
    /// Workflow-layer administrative commands (lockfile, journal, …).
    #[command(subcommand)]
    Workflow(WorkflowCmd),
    /// Read or diff parameter files across git refs.
    #[command(subcommand)]
    Params(ParamsCmd),
    /// Read or diff metrics files across git refs.
    #[command(subcommand)]
    Metrics(MetricsCmd),
    /// DVC-style plot helpers.
    #[command(subcommand)]
    Plots(PlotsCmd),
    /// Git LFS compatibility commands.
    #[command(subcommand)]
    Lfs(crab::cmd::lfs::LfsCmd),
    /// Standalone LFS transfer agent (invoked by git-lfs, not users).
    #[command(name = "lfs-transfer-agent", hide = true)]
    LfsTransferAgent,
    /// Mount a virtual filesystem for on-demand file access.
    #[command(after_help = "\x1b[1mExamples:\x1b[0m\n  \
            # Remote mount\n  \
            crab mount --repo crab://bucket/ml-models --mountpoint /mnt/models\n\n  \
            # Local mount (view another branch without checkout)\n  \
            crab mount --repo /home/user/my-repo --mountpoint /tmp/view --ref=dev\n\n  \
            # Read-only mount with no background refresh\n  \
            crab mount -r crab://bucket/data -m /mnt/data --read-only --no-refresh\n\n  \
            # Foreground mode (blocks until Ctrl+C)\n  \
            crab mount --repo ./my-repo --mountpoint /mnt/view --foreground\n")]
    Mount {
        #[command(subcommand)]
        sub: Option<MountCmd>,
        /// Source repository — remote URL or local path.
        ///
        /// Remote: <crab://bucket/repo>, <s3://bucket/prefix>
        /// Local:  /path/to/local/repo, ./relative/repo
        #[arg(long, short = 'r', value_name = "SOURCE")]
        repo: Option<String>,
        /// Local path for the mount.
        #[arg(long, short = 'm', value_name = "PATH")]
        mountpoint: Option<std::path::PathBuf>,
        /// Mount backend to use (default: NFS when available, otherwise FUSE).
        #[arg(long, value_enum, default_value_t = crab::cmd::mount::MountBackend::Auto)]
        backend: crab::cmd::mount::MountBackend,
        /// Human-friendly name for this mount (default: derived from repo).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Branch or ref to mount (default: HEAD).
        #[arg(long = "ref", value_name = "BRANCH")]
        git_ref: Option<String>,
        /// Run in foreground (block until SIGINT) instead of backgrounding.
        #[arg(long)]
        foreground: bool,
        /// Disable writes (no overlay). All write operations return EROFS.
        #[arg(long)]
        read_only: bool,
        /// Disable automatic remote polling (static snapshot).
        #[arg(long)]
        no_refresh: bool,
        /// Allow mounting inside a git or crab working tree.
        #[arg(long)]
        allow_nested: bool,
        /// Discard existing overlay (local modifications) before mounting.
        #[arg(long)]
        clean_overlay: bool,
    },
    /// Unmount a Crab virtual filesystem mount.
    #[command(after_help = "\x1b[1mExamples:\x1b[0m\n  \
            crab unmount --mountpoint /mnt/models\n  \
            crab unmount -m /tmp/view\n  \
            crab unmount --all\n")]
    Unmount {
        /// Path where the filesystem is mounted.
        #[arg(
            long,
            short = 'm',
            value_name = "PATH",
            required_unless_present = "all"
        )]
        mountpoint: Option<std::path::PathBuf>,
        /// Unmount all active crab mounts.
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Multi-repo daemon: shared cache, hydration pool, multiple mounts.
    Daemon {
        #[command(subcommand)]
        sub: Option<DaemonCmd>,
        /// Root directory for daemon state (default: ~/.crab/daemon).
        #[arg(long, value_name = "PATH")]
        root: Option<std::path::PathBuf>,
        /// Number of hydration worker tasks (default: 4).
        #[arg(long, value_name = "N")]
        hydration_concurrency: Option<usize>,
    },
    /// Authenticate with your identity provider.
    Login {
        /// Managed service origin. Defaults to https://crab.build.
        #[arg(value_name = "SERVICE_ORIGIN")]
        service: Option<String>,
        /// Use device code flow (for headless/SSH sessions).
        #[arg(long)]
        headless: bool,
        /// Override the configured auth provider for this login.
        #[arg(
            long,
            value_name = "NAME",
            conflicts_with_all = ["service", "enterprise_ca", "private_ca_only"]
        )]
        provider: Option<String>,
        /// Trust an administrator-installed PEM CA bundle for this service.
        #[arg(long, value_name = "PATH")]
        enterprise_ca: Option<PathBuf>,
        /// Trust only --enterprise-ca, excluding public system roots.
        #[arg(long, requires = "enterprise_ca")]
        private_ca_only: bool,
    },
    /// Clear cached credentials.
    Logout {
        /// Managed service authority or HTTPS origin. Defaults to the active profile.
        #[arg(value_name = "SERVICE")]
        service: Option<String>,
        /// Delete tokens for all providers.
        #[arg(long, conflicts_with = "service")]
        all: bool,
    },
    /// Manage organizations on the selected Crab service.
    Organization(crab::cmd::managed_admin::OrganizationArgs),
    /// Manage logical repositories on the selected Crab service.
    Repo(crab::cmd::managed_admin::RepositoryArgs),
    /// Manage organization memberships on the selected Crab service.
    Member(crab::cmd::managed_admin::MemberArgs),
    /// Manage service accounts on the selected Crab service.
    ServiceAccount(crab::cmd::managed_admin::ServiceAccountArgs),
    /// Authentication management.
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Inspect and verify Crab audit events.
    #[command(subcommand)]
    Audit(crab::cmd::audit::AuditCmd),
    /// Create and verify dataset release manifests.
    #[command(subcommand)]
    Release(crab::cmd::release::ReleaseCmd),
    /// Plan and apply repository recovery.
    #[command(subcommand)]
    Recover(crab::cmd::recover::RecoverCmd),
    /// Internal coordinator management (hidden).
    #[command(subcommand, hide = true)]
    Coordinator(crab::cmd::coordinator::CoordinatorCmd),
    /// Git clean/smudge filter driver (invoked by Git, not users).
    #[command(hide = true)]
    FilterProcess,
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Display authentication status and configuration.
    Status {
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Force an immediate token refresh.
    Refresh,
}

#[derive(Subcommand)]
enum OptimizeCmd {
    /// Build a combined cost optimization plan.
    Plan(crab::cmd::optimize::OptimizePlanArgs),
    /// Apply the combined cost optimization workflow.
    Apply(crab::cmd::optimize::OptimizeApplyArgs),
    /// Rewrite content-addressed xorbs for the selected size/grouping profile.
    Xorbs(crab::cmd::restripe::RestripeArgs),
    /// Consolidate remote Git pack files.
    Packs {
        /// Report pack statistics without modifying the remote.
        #[arg(long)]
        dry_run: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Compact metadata shards into fewer larger shards.
    Shards {
        /// Repo prefix (e.g. `org/models`).
        #[arg(long)]
        repo: String,
        /// S3 bucket name.
        #[arg(long)]
        bucket: String,
        /// Report what would happen without mutating.
        #[arg(long)]
        dry_run: bool,
        /// Maximum compacted shard size (e.g. `100MiB`, `50MB`).
        #[arg(long, default_value = "100MiB")]
        max_shard_size: String,
    },
    /// Plan, apply, or roll back lifecycle tiering rules.
    Tiers {
        #[command(subcommand)]
        command: crab::cmd::tier::TierCommand,
    },
    /// Verify, clean, prune, or prewarm the local object cache.
    #[command(subcommand)]
    Cache(OptimizeCacheCmd),
    /// Diagnose, rebuild, compact, or warm metadata indexes.
    #[command(subcommand)]
    Indexes(OptimizeIndexesCmd),
    /// Optimize Git LFS compatibility storage.
    #[command(subcommand)]
    Lfs(OptimizeLfsCmd),
    /// Maintain workflow stage cache entries and journals.
    #[command(subcommand)]
    WorkflowCache(OptimizeWorkflowCacheCmd),
    /// Inspect and repair replica health, lag, backfill, and evidence.
    #[command(subcommand)]
    Replicas(OptimizeReplicasCmd),
    /// Build an optimization plan for the whole repository.
    Repo(OptimizeRepoArgs),
}

#[derive(Subcommand)]
enum OptimizeCacheCmd {
    /// Print cache statistics.
    Stats,
    /// Verify cached chunks, shards, and xorbs, evicting corrupt entries.
    Verify,
    /// Clear the local cache.
    Clean,
    /// Evict local cache objects until configured budgets are satisfied.
    Prune {
        /// Report what would be pruned without deleting.
        #[arg(long)]
        dry_run: bool,
        /// Print each object as it is pruned.
        #[arg(long)]
        verbose: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Prewarm the local cache for selected refs and paths.
    Warm {
        /// Include patterns (e.g. `*.safetensors`).
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// Exclude patterns.
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Fetch objects for all refs, not just HEAD.
        #[arg(long)]
        all: bool,
        /// Report what would be fetched without downloading.
        #[arg(long)]
        dry_run: bool,
        /// Skip the local chunk-index warm-up.
        #[arg(long = "no-sync-chunk-index")]
        no_sync_chunk_index: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
}

#[derive(Subcommand)]
enum OptimizeIndexesCmd {
    /// Print metadata database snapshots and optional deep integrity checks.
    Diagnose {
        /// Target database (defaults to both).
        #[arg(long, value_enum, default_value_t = crab::cmd::metadb::DbSelector::Both)]
        db: crab::cmd::metadb::DbSelector,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
        /// Run deep integrity checks.
        #[arg(long)]
        deep: bool,
    },
    /// Rebuild metadata databases from durable shards.
    Rebuild {
        /// Target database.
        #[arg(long, value_enum, default_value_t = crab::cmd::metadb::DbSelector::Both)]
        db: crab::cmd::metadb::DbSelector,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Compact one or both metadata databases.
    Compact {
        /// Target database.
        #[arg(long, value_enum, default_value_t = crab::cmd::metadb::DbSelector::Both)]
        db: crab::cmd::metadb::DbSelector,
    },
    /// Report local chunk-index cache state.
    CacheStats,
    /// Clear the local chunk-index cache.
    CacheClear,
    /// Warm file/chunk indexes by fetching selected object metadata.
    Warm {
        /// Include patterns (e.g. `*.safetensors`).
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// Exclude patterns.
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Fetch objects for all refs, not just HEAD.
        #[arg(long)]
        all: bool,
        /// Report what would be fetched without downloading.
        #[arg(long)]
        dry_run: bool,
        /// Skip the local chunk-index warm-up.
        #[arg(long = "no-sync-chunk-index")]
        no_sync_chunk_index: bool,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
}

#[derive(Subcommand)]
enum OptimizeLfsCmd {
    /// Deduplicate checked-out LFS files.
    Dedup {
        /// Report what would be deduplicated without modifying files.
        #[arg(long)]
        dry_run: bool,
        /// Test whether this platform and repository support LFS deduplication.
        #[arg(long, short = 't')]
        test: bool,
        /// Cleanup LFS objects duplicated by verified Crab content.
        #[arg(long)]
        crab_cache: bool,
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
        #[arg(long)]
        rollback: bool,
    },
    /// Remove unreferenced LFS objects from local storage.
    Prune {
        /// Require remote object verification before local delete.
        #[arg(long, short = 'c', conflicts_with = "no_verify_remote")]
        verify_remote: bool,
        /// Disable remote verification.
        #[arg(long, conflicts_with = "verify_remote")]
        no_verify_remote: bool,
        /// Also require remote verification for unreachable candidates.
        #[arg(long, conflicts_with = "no_verify_unreachable")]
        verify_unreachable: bool,
        /// Do not require remote verification for unreachable candidates.
        #[arg(long, conflicts_with = "verify_unreachable")]
        no_verify_unreachable: bool,
        /// Continue or halt when a candidate cannot be verified remotely.
        #[arg(long, value_name = "MODE", value_parser = ["halt", "continue"])]
        when_unverified: Option<String>,
        /// Accept Git LFS recent-pruning flag.
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
}

#[derive(Subcommand)]
enum OptimizeWorkflowCacheCmd {
    /// Upload missing local stage cache entries to the configured remote.
    Push(crab::cmd::workflow::PushCacheArgs),
    /// Prune old terminal workflow run journals.
    JournalGc(crab::cmd::workflow_journal::GcArgs),
}

#[derive(Subcommand)]
enum OptimizeReplicasCmd {
    /// Show configured replica health and lag.
    Status(crab::cmd::replica::StatusArgs),
    /// Diagnose replica configuration and readiness.
    Doctor(crab::cmd::replica::DoctorArgs),
    /// Verify replica manifest-referenced objects with live checks.
    Verify(crab::cmd::replica::VerifyArgs),
    /// Inspect historical object backfill before enabling replica reads.
    #[command(subcommand)]
    Backfill(crab::cmd::replica::BackfillCommand),
    /// Wait for a replica to become read-ready.
    Wait(crab::cmd::replica::WaitArgs),
    /// Repair regional state from coordinator truth.
    Repair(crab::cmd::replica::RepairArgs),
    /// Estimate billable replication usage quantities.
    Cost(crab::cmd::replica::CostArgs),
    /// Show incident recovery steps for enterprise replication.
    Runbook(crab::cmd::replica::RunbookArgs),
    /// Collect a portable replica diagnostics bundle.
    Diagnostics(crab::cmd::replica::DiagnosticsArgs),
    /// Run strict enterprise production-readiness certification gates.
    Certify(crab::cmd::replica::CertifyArgs),
    /// Verify retained enterprise replication evidence artifacts.
    #[command(subcommand)]
    Evidence(crab::cmd::replica::EvidenceCommand),
    /// Plan or run active-active failover automation.
    #[command(subcommand)]
    Failover(crab::cmd::replica::FailoverCommand),
}

#[derive(Debug, Clone, Parser)]
struct OptimizeRepoArgs {
    /// Structured JSON output.
    #[arg(long)]
    json: bool,
    /// Path to a pricing override YAML file.
    #[arg(long, value_name = "PATH")]
    pricing_file: Option<String>,
    /// Inventory source: auto, live, or report.
    #[arg(long, value_name = "SOURCE")]
    inventory_source: Option<String>,
    /// Sample ratio for live inventory (0.0-1.0).
    #[arg(long, value_name = "RATIO")]
    sample: Option<f64>,
    /// Number of heaviest cold objects to report.
    #[arg(long, value_name = "K")]
    top_k: Option<usize>,
}

impl OptimizeCmd {
    fn output_mode(&self) -> OutputMode {
        match self {
            Self::Plan(args) => OutputMode::from_flags(args.json, false),
            Self::Apply(args) => OutputMode::from_flags(args.json, false),
            Self::Xorbs(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::Packs { json, jsonl, .. } => OutputMode::from_flags(*json, *jsonl),
            Self::Tiers { command } => match command {
                crab::cmd::tier::TierCommand::Plan { json, jsonl, .. } => {
                    OutputMode::from_flags(*json, *jsonl)
                }
                crab::cmd::tier::TierCommand::Rollback { .. } => OutputMode::Text,
            },
            Self::Cache(command) => match command {
                OptimizeCacheCmd::Prune { json, jsonl, .. }
                | OptimizeCacheCmd::Warm { json, jsonl, .. } => {
                    OutputMode::from_flags(*json, *jsonl)
                }
                OptimizeCacheCmd::Stats | OptimizeCacheCmd::Verify | OptimizeCacheCmd::Clean => {
                    OutputMode::Text
                }
            },
            Self::Indexes(command) => match command {
                OptimizeIndexesCmd::Diagnose { json, .. }
                | OptimizeIndexesCmd::Rebuild { json, .. } => OutputMode::from_flags(*json, false),
                OptimizeIndexesCmd::Warm { json, jsonl, .. } => {
                    OutputMode::from_flags(*json, *jsonl)
                }
                OptimizeIndexesCmd::Compact { .. }
                | OptimizeIndexesCmd::CacheStats
                | OptimizeIndexesCmd::CacheClear => OutputMode::Text,
            },
            Self::WorkflowCache(command) => match command {
                OptimizeWorkflowCacheCmd::Push(args) => args.output_mode(),
                OptimizeWorkflowCacheCmd::JournalGc(args) => args.output_mode(),
            },
            Self::Replicas(command) => optimize_replica_output_mode(command),
            Self::Repo(args) => OutputMode::from_flags(args.json, false),
            Self::Shards { .. } | Self::Lfs(_) => OutputMode::Text,
        }
    }

    fn schema_name(&self) -> &'static str {
        match self {
            Self::Plan(_) => crab::cmd::optimize::OPTIMIZE_PLAN_SCHEMA,
            Self::Apply(_) => crab::cmd::optimize::OPTIMIZE_APPLY_SCHEMA,
            Self::Xorbs(_) => "optimize.xorbs",
            Self::Packs { .. } => "repack",
            Self::Shards { .. } => "compact",
            Self::Tiers { command } => match command {
                crab::cmd::tier::TierCommand::Plan { .. } => "tier.plan",
                crab::cmd::tier::TierCommand::Rollback { .. } => "tier.rollback",
            },
            Self::Cache(command) => match command {
                OptimizeCacheCmd::Stats => "cache.stats",
                OptimizeCacheCmd::Verify => "cache.verify",
                OptimizeCacheCmd::Clean => "cache.clean",
                OptimizeCacheCmd::Prune { .. } => "prune",
                OptimizeCacheCmd::Warm { .. } => "fetch",
            },
            Self::Indexes(command) => match command {
                OptimizeIndexesCmd::Diagnose { .. } => "metadb.diagnose",
                OptimizeIndexesCmd::Rebuild { .. } => "metadb.rebuild",
                OptimizeIndexesCmd::Compact { .. } => "metadb.compact",
                OptimizeIndexesCmd::CacheStats => "metadb.cache.stats",
                OptimizeIndexesCmd::CacheClear => "metadb.cache.clear",
                OptimizeIndexesCmd::Warm { .. } => "fetch",
            },
            Self::Lfs(_) => "optimize.lfs",
            Self::WorkflowCache(command) => match command {
                OptimizeWorkflowCacheCmd::Push(_) => "workflow.push_cache",
                OptimizeWorkflowCacheCmd::JournalGc(_) => "workflow.journal.gc",
            },
            Self::Replicas(_) => "replica",
            Self::Repo(_) => "cost",
        }
    }
}

fn optimize_replica_output_mode(command: &OptimizeReplicasCmd) -> OutputMode {
    match command {
        OptimizeReplicasCmd::Status(args) => OutputMode::from_flags(args.json, args.jsonl),
        OptimizeReplicasCmd::Doctor(args) => OutputMode::from_flags(args.json, false),
        OptimizeReplicasCmd::Verify(args) => OutputMode::from_flags(args.json, false),
        OptimizeReplicasCmd::Backfill(crab::cmd::replica::BackfillCommand::Status(args)) => {
            OutputMode::from_flags(args.json, false)
        }
        OptimizeReplicasCmd::Wait(args) => OutputMode::from_flags(args.json, false),
        OptimizeReplicasCmd::Repair(args) => OutputMode::from_flags(args.json, args.jsonl),
        OptimizeReplicasCmd::Cost(args) => OutputMode::from_flags(args.json, false),
        OptimizeReplicasCmd::Runbook(args) => OutputMode::from_flags(args.json, false),
        OptimizeReplicasCmd::Diagnostics(args) => OutputMode::from_flags(args.json, false),
        OptimizeReplicasCmd::Certify(args) => OutputMode::from_flags(args.json, false),
        OptimizeReplicasCmd::Evidence(crab::cmd::replica::EvidenceCommand::Verify(args)) => {
            OutputMode::from_flags(args.json, false)
        }
        OptimizeReplicasCmd::Failover(command) => match command {
            crab::cmd::replica::FailoverCommand::Status(args) => {
                OutputMode::from_flags(args.json, false)
            }
            crab::cmd::replica::FailoverCommand::Plan(args) => {
                OutputMode::from_flags(args.json, false)
            }
            crab::cmd::replica::FailoverCommand::Run(args) => {
                OutputMode::from_flags(args.json, false)
            }
            crab::cmd::replica::FailoverCommand::Fence(args) => {
                OutputMode::from_flags(args.json, false)
            }
            crab::cmd::replica::FailoverCommand::Resume(args) => {
                OutputMode::from_flags(args.json, false)
            }
        },
    }
}

#[derive(Subcommand)]
enum ParamsCmd {
    /// Print the flattened params map at a ref.
    Show(crab::cmd::params::ShowArgs),
    /// Structured diff of params between two refs.
    Diff(crab::cmd::params::DiffArgs),
}

#[derive(Subcommand)]
enum ExpCmd {
    /// Run an experiment: materialize HEAD into a tmpdir, apply
    /// `--set` overrides, execute the DAG, persist metadata.
    Run(crab::cmd::exp::RunArgs),
    /// Print an experiment's metadata.
    Show(crab::cmd::exp::ShowArgs),
    /// Diff two experiments (params, stage hashes, metrics).
    Diff(crab::cmd::exp::DiffArgs),
    /// List local experiments newest-first.
    #[command(alias = "list")]
    Ls(crab::cmd::exp::LsArgs),
    /// Create a git branch pointing at an experiment's base commit.
    #[command(alias = "branch")]
    Promote(crab::cmd::exp::PromoteArgs),
    /// Apply a completed experiment snapshot to the workspace.
    Apply(crab::cmd::exp::ApplyArgs),
    /// Reset an experiment checkpoint lineage to a selected point or base.
    Reset(crab::cmd::exp::ResetArgs),
    /// Save the current workspace as an experiment without running.
    Save(crab::cmd::exp::SaveArgs),
    /// Rename a local experiment label.
    Rename(crab::cmd::exp::RenameArgs),
    /// Upload experiments to the configured Crab remote.
    Push(crab::cmd::exp::PushArgs),
    /// Download experiments from the configured Crab remote.
    Pull(crab::cmd::exp::PullArgs),
    /// Remove local experiment metadata by id, or keep selected ids.
    #[command(alias = "rm")]
    Remove(crab::cmd::exp::RemoveArgs),
    /// Clean temporary experiment files and stale queue housekeeping.
    Clean(crab::cmd::exp::CleanArgs),
    /// Prune local experiment metadata beyond `--keep`.
    Gc(crab::cmd::exp::GcArgs),
    /// Queue experiments with parameter overrides for batch execution.
    Queue(crab::cmd::exp_queue::QueueArgs),
    /// Start processing queued experiments with parallel workers.
    Start(crab::cmd::exp_queue::StartArgs),
    /// Show experiment queue status.
    Status(crab::cmd::exp_queue::StatusArgs),
    /// Signal running workers to stop gracefully.
    Stop(crab::cmd::exp_queue::StopArgs),
}

#[derive(Subcommand)]
enum QueueCmd {
    /// Start processing queued experiments with parallel workers.
    Start(crab::cmd::exp_queue::StartArgs),
    /// Show experiment queue status.
    Status(crab::cmd::exp_queue::StatusArgs),
    /// Show console output logs for a queued task.
    Logs(crab::cmd::exp_queue::QueueLogsArgs),
    /// Interrupt running queued task(s).
    Kill(crab::cmd::exp_queue::QueueKillArgs),
    /// Remove queued or completed task entries.
    Remove(crab::cmd::exp_queue::QueueRemoveArgs),
    /// Signal running workers to stop gracefully.
    Stop(crab::cmd::exp_queue::StopArgs),
}

#[derive(Subcommand)]
enum StageCmd {
    /// Create or update a workflow stage in crab.yaml.
    Add(crab::cmd::stage::StageAddArgs),
    /// List stages declared in workflow files.
    List(crab::cmd::stage::StageListArgs),
}

#[derive(Subcommand)]
enum MetricsCmd {
    /// Print the flattened metrics map at a ref.
    Show(crab::cmd::metrics::ShowArgs),
    /// Structured diff of metrics between refs or the workspace.
    Diff(crab::cmd::metrics::DiffArgs),
    /// Render plots from workflow plot configuration.
    Plot(crab::cmd::metrics::PlotArgs),
}

#[derive(Subcommand)]
enum PlotsCmd {
    /// Render current plots from workflow plot configuration or target files.
    Show(crab::cmd::metrics::PlotArgs),
    /// Render a plot overlay between two refs.
    Diff(crab::cmd::metrics::PlotDiffArgs),
    /// List built-in/local plot templates or print one template's Vega-Lite JSON.
    Templates(crab::cmd::metrics::PlotTemplatesArgs),
}

#[derive(Subcommand)]
enum WorkflowCmd {
    /// Manage `crab.lock`.
    #[command(subcommand)]
    Lockfile(WorkflowLockfileCmd),
    /// Report per-stage workflow state (up-to-date / stale /
    /// never-run / in-flight) against `crab.lock`.
    Status(crab::cmd::status_workflow::StatusArgs),
    /// Render the workflow DAG as ASCII, Mermaid, DOT, or artifacts. Structured
    /// output (`--json`) emits the `workflow.dag` schema: an
    /// ordered list of stages plus every producer → consumer edge.
    Dag(crab::cmd::dag::DagArgs),
    /// Inspect and prune workflow run journals under
    /// `.crab/workflow/runs/`.
    #[command(subcommand)]
    Journal(WorkflowJournalCmd),
    /// Push local stage cache entries to the configured remote.
    PushCache(crab::cmd::workflow::PushCacheArgs),
    /// Internal stage-to-supervisor checkpoint control protocol.
    #[command(hide = true)]
    Checkpoint(crab::cmd::workflow_checkpoint::WorkflowCheckpointArgs),
}

#[derive(Subcommand)]
enum WorkflowLockfileCmd {
    /// Resolve a git-merge-conflict in `crab.lock`.
    Resolve(crab::cmd::workflow_lockfile::ResolveArgs),
    /// Split a monolithic `crab.lock` into per-workflow lockfiles
    /// (`<name>.workflow.lock`) alongside each `*.workflow.yaml`.
    /// One-shot migration for repos opting into
    /// `[workflow] lockfile = "split"`.
    Split(crab::cmd::workflow_lockfile::SplitArgs),
}

#[derive(Subcommand)]
enum WorkflowJournalCmd {
    /// Print one run's full stage trajectory.
    Show(crab::cmd::workflow_journal::ShowArgs),
    /// List every journal under `.crab/workflow/runs/` with its
    /// outcome.
    Ls(crab::cmd::workflow_journal::LsArgs),
    /// Remove the oldest terminal journals beyond `--keep` (default 50).
    Gc(crab::cmd::workflow_journal::GcArgs),
}

#[derive(Subcommand)]
enum MountCmd {
    /// Diagnose mount backend readiness.
    Doctor {
        /// Mount backend to diagnose.
        #[arg(long, value_enum, default_value_t = crab::cmd::mount::MountBackend::Auto)]
        backend: crab::cmd::mount::MountBackend,
        /// Mountpoint to validate. Defaults to a temporary empty directory.
        #[arg(long, short = 'm', value_name = "PATH")]
        mountpoint: Option<std::path::PathBuf>,
        /// Output as JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Report mount state and hydration progress.
    Status {
        /// Path to the mounted filesystem.
        #[arg(long, short = 'm', value_name = "PATH", default_value = ".")]
        mountpoint: std::path::PathBuf,
        /// Require a live backend control response instead of persisted fallback.
        #[arg(long)]
        live_only: bool,
        /// Show individual dirty paths.
        #[arg(long, short)]
        verbose: bool,
        /// Show only overlay mutations.
        #[arg(long)]
        dirty: bool,
        /// Output as JSON object.
        #[arg(long)]
        json: bool,
    },
    /// List all active mounts.
    List {
        /// Output as JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Trigger an immediate fetch + snapshot rebuild for a mount.
    Refresh {
        /// Mountpoint to refresh.
        #[arg(long, short = 'm', value_name = "PATH")]
        mountpoint: std::path::PathBuf,
    },
    /// Switch a mount to a different branch or ref.
    Switch {
        /// Mountpoint to switch.
        #[arg(long, short = 'm', value_name = "PATH")]
        mountpoint: std::path::PathBuf,
        /// Branch or ref to switch to.
        #[arg(long = "ref", value_name = "BRANCH")]
        git_ref: String,
    },
    /// Remove inactive mount caches to free disk space.
    Clean {
        /// Delete everything under ~/.crab/mounts/ (requires no active mounts).
        #[arg(long)]
        all: bool,
    },
    /// Show overlay mutations for a writable mount.
    Diff {
        /// Path to the mounted filesystem.
        #[arg(long, short = 'm', value_name = "PATH", default_value = ".")]
        mountpoint: std::path::PathBuf,
        /// Output as JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Export overlay mutations to a directory for inspection.
    Export {
        /// Path to the mounted filesystem.
        #[arg(long, short = 'm', value_name = "PATH", default_value = ".")]
        mountpoint: std::path::PathBuf,
        /// Destination directory.
        #[arg(long, value_name = "PATH")]
        to: std::path::PathBuf,
        /// Output as JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Discard local overlay mutations.
    Reset {
        /// Path to the mounted filesystem.
        #[arg(long, short = 'm', value_name = "PATH", default_value = ".")]
        mountpoint: std::path::PathBuf,
        /// Confirm that the writable overlay should be discarded.
        #[arg(long)]
        overlay: bool,
        /// Required confirmation for destructive reset.
        #[arg(long)]
        yes: bool,
        /// Output as JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Commit overlay mutations back to the mounted repository.
    Commit {
        /// Path to the mounted filesystem.
        #[arg(long, value_name = "PATH", default_value = ".")]
        mountpoint: std::path::PathBuf,
        /// Commit message.
        #[arg(long, short = 'm', value_name = "MESSAGE")]
        message: String,
        /// Push the new commit to the repository's origin.
        #[arg(long)]
        push: bool,
        /// Output as JSON object.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Register a repository for the daemon to mount.
    AddRepo {
        /// Unique name for this repo.
        #[arg(long, value_name = "NAME")]
        name: String,
        /// Remote URL to clone from.
        #[arg(long, value_name = "URL")]
        remote: String,
        /// Branch to track (default: main).
        #[arg(long, value_name = "BRANCH", default_value = "main")]
        branch: String,
        /// Root directory for the mount (`{mount-root}/{name}/`).
        #[arg(long, value_name = "PATH")]
        mount_root: String,
        /// Filesystem backend used for this repo.
        #[arg(long, value_enum, default_value_t = DaemonMountBackendArg::Fuse)]
        backend: DaemonMountBackendArg,
    },
    /// Deregister and unmount a repository.
    RemoveRepo {
        /// Name of the repo to remove.
        #[arg(long, value_name = "NAME")]
        name: String,
    },
    /// List all registered repositories.
    List {
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
    /// Report per-repo status.
    Status {
        /// Name of the repo to query.
        #[arg(long, value_name = "NAME")]
        name: String,
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
    /// Tune the refresh interval for a repo.
    SetRefresh {
        /// Name of the repo.
        #[arg(long, value_name = "NAME")]
        name: String,
        /// Refresh interval (e.g. "30s", "5m"). Parsed as seconds.
        #[arg(long, value_name = "SECONDS")]
        interval: u64,
    },
    /// Unmount and re-mount a repo with a fresh snapshot.
    Remount {
        /// Name of the repo to remount.
        #[arg(long, value_name = "NAME")]
        name: String,
        /// Discard existing overlay before remounting.
        #[arg(long)]
        clean_overlay: bool,
    },
    /// Trigger an immediate git fetch for a repo.
    Fetch {
        /// Name of the repo to fetch.
        #[arg(long, value_name = "NAME")]
        name: String,
    },
    /// Enable a disabled repo for mounting.
    Enable {
        /// Name of the repo to enable.
        #[arg(long, value_name = "NAME")]
        name: String,
    },
    /// Disable a repo (unmount on next sync, keep registration).
    Disable {
        /// Name of the repo to disable.
        #[arg(long, value_name = "NAME")]
        name: String,
    },
    /// Commit overlay mutations for a daemon-managed repo.
    Commit {
        /// Name of the repo.
        #[arg(long, value_name = "NAME")]
        name: String,
        /// Commit message.
        #[arg(long, short = 'm', value_name = "MESSAGE")]
        message: String,
        /// Push the new commit to origin.
        #[arg(long)]
        push: bool,
        /// Output as JSON object.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
enum DaemonMountBackendArg {
    Fuse,
    Nfs,
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Print cache statistics.
    Stats,
    /// Verify cached chunks, shards, and xorbs, evicting corrupt entries.
    Verify,
    /// Clear the local cache.
    Clean,
}

#[derive(Subcommand)]
enum StatCmd {
    /// Print performance counters.
    Perf,
    /// Print add-time push-plan inventory.
    PushPlan {
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
        /// Verify prepared xorb payload hashes and metadata.
        #[arg(long)]
        verify: bool,
    },
    /// Print per-storage-class bytes and object counts.
    Classes {
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum StagingCmd {
    /// Print staging area statistics.
    Stats {
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
    /// Verify staged file metadata and chunk payloads.
    Verify,
    /// Purge stale staging data.
    Clean {
        /// Force-break a stale lock held by a dead process.
        #[arg(long)]
        force: bool,
        /// Also reclaim segments that were rolled over but never
        /// sealed (e.g. left behind by a crashed `crab add` or an
        /// older binary). Safe because abandoned segments hold only
        /// pending rows that were never promoted to committed chunks.
        #[arg(long)]
        prune_abandoned: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Get a config value.
    Get {
        /// Dotted config key (e.g. checkout.lazy).
        key: String,
        /// Structured JSON output (envelope format).
        #[arg(long)]
        json: bool,
    },
    /// Set a config value.
    Set {
        /// Dotted config key (e.g. checkout.lazy).
        key: String,
        /// Value to set.
        value: String,
    },
}

#[derive(Subcommand)]
enum LogsCmd {
    /// List all log files.
    List,
    /// Show the most recent log file.
    Last,
    /// Show a specific log file.
    Show {
        /// Name of the log file to display.
        name: String,
    },
    /// Delete all log files.
    Clear,
}

#[derive(Subcommand)]
enum MigrateCmd {
    /// Show which file types would benefit from crab tracking.
    Info {
        /// Only consider files above this size in bytes (default: 1MB).
        #[arg(long, default_value = "1048576")]
        above: u64,
        /// Show the top N extensions (default: 10).
        #[arg(long, default_value = "10")]
        top: usize,
    },
    /// Convert large files in history to crab pointers.
    Import {
        /// Glob patterns for files to convert (e.g. `*.bin`).
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// Glob patterns to exclude.
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Only migrate files above this size in bytes.
        #[arg(long, default_value = "1048576")]
        above: u64,
        /// Report what would be migrated without rewriting.
        #[arg(long)]
        dry_run: bool,
        /// Rewrite all branches, not just the current one.
        #[arg(long)]
        everything: bool,
    },
    /// Convert crab pointers back to full files in history.
    Export {
        /// Glob patterns for files to convert back.
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// Report what would be exported without rewriting.
        #[arg(long)]
        dry_run: bool,
    },
    /// Convert a DVC pipeline (`dvc.yaml`) to `crab.yaml`.
    FromDvc {
        /// Directory containing `dvc.yaml` (default: current directory).
        #[arg(long, value_name = "PATH")]
        dir: Option<std::path::PathBuf>,
        /// Print the converted YAML to stdout instead of writing a file.
        #[arg(long)]
        stdout: bool,
        /// Output file path (default: `crab.yaml` in the same directory).
        #[arg(long, short, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
        /// Inspect and report without writing YAML, a journal, or Crab data.
        #[arg(long)]
        plan: bool,
        /// Resume from the migration journal after verifying source identity.
        #[arg(long)]
        resume: bool,
        /// Map a named DVC remote explicitly as NAME=CRAB_DESTINATION.
        #[arg(long = "remote-map", value_name = "NAME=DESTINATION")]
        remote_map: Vec<String>,
        /// Emit one JSON envelope instead of text.
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Emit one terminal JSONL result event.
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
}

impl MigrateCmd {
    fn output_mode(&self) -> OutputMode {
        match self {
            Self::FromDvc { json, jsonl, .. } => OutputMode::from_flags(*json, *jsonl),
            _ => OutputMode::Text,
        }
    }
}

impl Cmd {
    /// Resolve the [`OutputMode`] from whichever flags this command carries.
    ///
    /// Commands that already have `--json` (and eventually `--jsonl`)
    /// return the corresponding mode. Commands without structured-output
    /// flags return [`OutputMode::Text`].
    fn output_mode(&self) -> OutputMode {
        match self {
            Self::Du { json, .. }
            | Self::Diff { json, .. }
            | Self::LsFiles { json, .. }
            | Self::Lock { json, .. }
            | Self::Unlock { json, .. }
            | Self::Locks { json, .. }
            | Self::Status { json, .. }
            | Self::Env { json, .. }
            | Self::Doctor { json, .. }
            | Self::Errors { json, .. }
            | Self::Version { json, .. }
            | Self::Update { json, .. }
            | Self::Track { json, .. } => OutputMode::from_flags(*json, false),
            Self::Skills(command) => command.output_mode(),
            Self::Stat {
                json,
                sub:
                    Some(
                        StatCmd::Classes { json: sub_json }
                        | StatCmd::PushPlan { json: sub_json, .. },
                    ),
            } => OutputMode::from_flags(*json || *sub_json, false),
            Self::Stat { json, .. } => OutputMode::from_flags(*json, false),
            Self::Add { json, jsonl, .. }
            | Self::Clone { json, jsonl, .. }
            | Self::Download { json, jsonl, .. }
            | Self::Hydrate { json, jsonl, .. }
            | Self::Dehydrate { json, jsonl, .. }
            | Self::Fetch { json, jsonl, .. }
            | Self::Gc { json, jsonl, .. }
            | Self::Fsck { json, jsonl, .. }
            | Self::Repack { json, jsonl, .. }
            | Self::Prune { json, jsonl, .. } => OutputMode::from_flags(*json, *jsonl),
            Self::Optimize(command) => command.output_mode(),
            Self::Mirror(args) => args.output_mode(),
            Self::Push(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::Ship { json, .. } => OutputMode::from_flags(*json, false),
            Self::Init { json, jsonl, .. } => OutputMode::from_flags(*json, *jsonl),
            Self::Setup { json, jsonl, .. } => OutputMode::from_flags(*json, *jsonl),
            Self::Adopt { json, .. } => OutputMode::from_flags(*json, false),
            Self::Unadopt { json, .. } => OutputMode::from_flags(*json, false),
            Self::Undo { json, .. } => OutputMode::from_flags(*json, false),
            Self::Why { json, .. } => OutputMode::from_flags(*json, false),
            Self::Import(args) => args.output_mode(),
            Self::Data(command) => command.output_mode(),
            Self::Export(args) => args.output_mode(),
            Self::Migrate(command) => command.output_mode(),
            Self::Artifacts(command) => command.output_mode(),
            Self::Run(args) | Self::Repro(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::Stage(StageCmd::Add(args)) => OutputMode::from_flags(args.json, false),
            Self::Stage(StageCmd::List(args)) => OutputMode::from_flags(args.json, false),
            Self::Freeze(args) => OutputMode::from_flags(args.json, false),
            Self::Unfreeze(args) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Run(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Show(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Diff(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Ls(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Promote(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Apply(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Reset(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Save(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Rename(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Push(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Pull(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Remove(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Clean(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Gc(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Queue(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Start(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Status(args)) => OutputMode::from_flags(args.json, false),
            Self::Exp(ExpCmd::Stop(args)) => OutputMode::from_flags(args.json, false),
            Self::Queue(QueueCmd::Start(args)) => OutputMode::from_flags(args.json, false),
            Self::Queue(QueueCmd::Status(args)) => OutputMode::from_flags(args.json, false),
            Self::Queue(QueueCmd::Logs(args)) => OutputMode::from_flags(args.json, false),
            Self::Queue(QueueCmd::Kill(args)) => OutputMode::from_flags(args.json, false),
            Self::Queue(QueueCmd::Remove(args)) => OutputMode::from_flags(args.json, false),
            Self::Queue(QueueCmd::Stop(args)) => OutputMode::from_flags(args.json, false),
            Self::Params(ParamsCmd::Show(args)) => OutputMode::from_flags(args.json, false),
            Self::Params(ParamsCmd::Diff(args)) => OutputMode::from_flags(args.json, false),
            Self::Metrics(MetricsCmd::Show(args)) => OutputMode::from_flags(args.json, false),
            Self::Metrics(MetricsCmd::Diff(args)) => OutputMode::from_flags(args.json, false),
            Self::Metrics(MetricsCmd::Plot(args)) => OutputMode::from_flags(args.json, false),
            Self::Plots(PlotsCmd::Show(args)) => OutputMode::from_flags(args.json, false),
            Self::Plots(PlotsCmd::Diff(args)) => OutputMode::from_flags(args.json, false),
            Self::Plots(PlotsCmd::Templates(args)) => OutputMode::from_flags(args.json, false),
            Self::Workflow(WorkflowCmd::Lockfile(WorkflowLockfileCmd::Resolve(args))) => {
                args.output_mode()
            }
            Self::Workflow(WorkflowCmd::Lockfile(WorkflowLockfileCmd::Split(args))) => {
                args.output_mode()
            }
            Self::Workflow(WorkflowCmd::Status(args)) => args.output_mode(),
            Self::Workflow(WorkflowCmd::Dag(args)) => args.output_mode(),
            Self::Workflow(WorkflowCmd::Journal(WorkflowJournalCmd::Show(args))) => {
                args.output_mode()
            }
            Self::Workflow(WorkflowCmd::Journal(WorkflowJournalCmd::Ls(args))) => {
                args.output_mode()
            }
            Self::Workflow(WorkflowCmd::Journal(WorkflowJournalCmd::Gc(args))) => {
                args.output_mode()
            }
            Self::Workflow(WorkflowCmd::PushCache(args)) => args.output_mode(),
            Self::Workflow(WorkflowCmd::Checkpoint(args)) => args.output_mode(),
            Self::Auth(AuthCmd::Status { json }) => OutputMode::from_flags(*json, false),
            Self::Organization(args) => args.output_mode(),
            Self::Repo(args) => args.output_mode(),
            Self::Member(args) => args.output_mode(),
            Self::ServiceAccount(args) => args.output_mode(),
            Self::Audit(command) => command.output_mode(),
            Self::Release(command) => command.output_mode(),
            Self::Recover(command) => command.output_mode(),
            Self::Config(ConfigCmd::Get { json, .. }) => OutputMode::from_flags(*json, false),
            Self::Replica(crab::cmd::replica::ReplicaCommand::Add(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Export(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Wait(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Verify(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Backfill(
                crab::cmd::replica::BackfillCommand::Status(args),
            )) => OutputMode::from_flags(args.json, false),
            Self::Replica(crab::cmd::replica::ReplicaCommand::Enable(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Disable(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Mode(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Writers(command)) => match command {
                crab::cmd::replica::WritersCommand::Status(args) => {
                    OutputMode::from_flags(args.json, false)
                }
                crab::cmd::replica::WritersCommand::Enable(args)
                | crab::cmd::replica::WritersCommand::Disable(args) => {
                    OutputMode::from_flags(args.json, false)
                }
            },
            Self::Replica(crab::cmd::replica::ReplicaCommand::Coordinator(command)) => {
                match command {
                    crab::cmd::replica::CoordinatorCommand::Add(args) => {
                        OutputMode::from_flags(args.json, false)
                    }
                    crab::cmd::replica::CoordinatorCommand::Status(args) => {
                        OutputMode::from_flags(args.json, false)
                    }
                    crab::cmd::replica::CoordinatorCommand::Remove(args) => {
                        OutputMode::from_flags(args.json, false)
                    }
                }
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Failover(
                crab::cmd::replica::FailoverCommand::Status(args),
            )) => OutputMode::from_flags(args.json, false),
            Self::Replica(crab::cmd::replica::ReplicaCommand::Repair(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Promote(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Status(args)) => {
                OutputMode::from_flags(args.json, args.jsonl)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Doctor(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Replica(crab::cmd::replica::ReplicaCommand::Remove(args)) => {
                OutputMode::from_flags(args.json, false)
            }
            Self::Worktree {
                json,
                command: crab::cmd::worktree::WorktreeCommand::List(args),
            } => OutputMode::from_flags(*json || args.json, false),
            Self::Worktree { json, .. } => OutputMode::from_flags(*json, false),
            Self::Staging(StagingCmd::Stats { json }) => OutputMode::from_flags(*json, false),
            Self::Daemon {
                sub: Some(DaemonCmd::List { json }),
                ..
            } => OutputMode::from_flags(*json, false),
            Self::Daemon {
                sub: Some(DaemonCmd::Status { json, .. }),
                ..
            } => OutputMode::from_flags(*json, false),
            Self::Tier(sub) => match sub {
                crab::cmd::tier::TierCommand::Plan { json, jsonl, .. } => {
                    OutputMode::from_flags(*json, *jsonl)
                }
                crab::cmd::tier::TierCommand::Rollback { .. } => OutputMode::Text,
            },
            _ => OutputMode::Text,
        }
    }

    /// Schema name used in error envelopes for this command.
    ///
    /// Matches the canonical names from the structured-output spec.
    fn schema_name(&self) -> &'static str {
        match self {
            Self::Configure { .. } => "configure",
            Self::Init { .. } => "init",
            Self::Setup { .. } => "setup",
            Self::Add { .. } => "add",
            Self::Reset { .. } => "reset",
            Self::Clone { .. } => "clone",
            Self::Mirror(_) => "mirror",
            Self::Download { .. } => "download",
            Self::Worktree {
                command: crab::cmd::worktree::WorktreeCommand::Add(_),
                ..
            } => crab::cmd::worktree::WORKTREE_ADD_SCHEMA,
            Self::Worktree {
                command: crab::cmd::worktree::WorktreeCommand::List(_),
                ..
            } => crab::cmd::worktree::WORKTREE_LIST_SCHEMA,
            Self::Worktree {
                command: crab::cmd::worktree::WorktreeCommand::Lock(_),
                ..
            } => crab::cmd::worktree::WORKTREE_LOCK_SCHEMA,
            Self::Worktree {
                command: crab::cmd::worktree::WorktreeCommand::Move(_),
                ..
            } => crab::cmd::worktree::WORKTREE_MOVE_SCHEMA,
            Self::Worktree {
                command: crab::cmd::worktree::WorktreeCommand::Prune(_),
                ..
            } => crab::cmd::worktree::WORKTREE_PRUNE_SCHEMA,
            Self::Worktree {
                command: crab::cmd::worktree::WorktreeCommand::Remove(_),
                ..
            } => crab::cmd::worktree::WORKTREE_REMOVE_SCHEMA,
            Self::Worktree {
                command: crab::cmd::worktree::WorktreeCommand::Repair(_),
                ..
            } => crab::cmd::worktree::WORKTREE_REPAIR_SCHEMA,
            Self::Worktree {
                command: crab::cmd::worktree::WorktreeCommand::Unlock(_),
                ..
            } => crab::cmd::worktree::WORKTREE_UNLOCK_SCHEMA,
            Self::Doctor { .. } => "doctor",
            Self::Du { .. } => "du",
            Self::Track { .. } => "track",
            Self::Untrack { .. } => "untrack",
            Self::Stat {
                sub: Some(StatCmd::Perf),
                ..
            } => "stat.perf",
            Self::Stat {
                sub: Some(StatCmd::Classes { .. }),
                ..
            } => "stat.classes",
            Self::Stat {
                sub: Some(StatCmd::PushPlan { .. }),
                ..
            } => "stat.push-plan",
            Self::Stat { .. } => "stat",
            Self::Gc { .. } => "gc",
            Self::Compact { .. } => "compact",
            Self::Fsck { .. } => "fsck",
            Self::Repack { .. } => "repack",
            Self::Optimize(command) => command.schema_name(),
            Self::Tier(..) => "tier",
            Self::Metadb(..) => "metadb",
            Self::Cache(CacheCmd::Stats) => "cache.stats",
            Self::Cache(CacheCmd::Verify) => "cache.verify",
            Self::Cache(CacheCmd::Clean) => "cache.clean",
            Self::Config(ConfigCmd::Get { .. }) => "config.get",
            Self::Config(ConfigCmd::Set { .. }) => "config.set",
            Self::Replica(_) => "replica",
            Self::Staging(StagingCmd::Stats { .. }) => "staging.stats",
            Self::Staging(StagingCmd::Verify) => "staging.verify",
            Self::Staging(StagingCmd::Clean { .. }) => "staging.clean",
            Self::Errors { .. } => "errors",
            Self::Status { workflow: true, .. } => {
                crab::cmd::status_workflow::WORKFLOW_STATUS_SCHEMA
            }
            Self::Status { .. } => "status",
            Self::Why { .. } => crab::cmd::why::WHY_SCHEMA,
            Self::Hydrate { .. } => "hydrate",
            Self::Diff { .. } => "diff",
            Self::DiffDriver { .. } => "diff-driver",
            Self::Dehydrate { .. } => "dehydrate",
            Self::Env { .. } => "env",
            Self::LsFiles { .. } => "ls-files",
            Self::Fetch { .. } => "fetch",
            Self::Prune { .. } => "prune",
            Self::Logs(_) => "logs",
            Self::Install { .. } => "install",
            Self::Uninstall { .. } => "uninstall",
            Self::Lock { .. } => "lock",
            Self::Unlock { .. } => "unlock",
            Self::Locks { .. } => "locks",
            Self::Migrate(MigrateCmd::FromDvc { .. }) => crab::cmd::migrate::DVC_MIGRATION_SCHEMA,
            Self::Migrate(_) => "migrate",
            Self::Artifacts(command) => command.schema_name(),
            Self::Push(_) => "push",
            Self::Ship { .. } => "ship",
            Self::Adopt { .. } => "adopt",
            Self::Unadopt { .. } => crab::cmd::unadopt::UNADOPT_SCHEMA,
            Self::Undo { .. } => crab::cmd::undo::UNDO_SCHEMA,
            Self::Import(_) => "import",
            Self::Data(_) => crab::cmd::data::DATA_SCHEMA,
            Self::Export(_) => "export.summary",
            Self::Run(_) | Self::Repro(_) => "workflow.stage_result",
            Self::Stage(StageCmd::Add(_)) => crab::cmd::stage::STAGE_ADD_SCHEMA,
            Self::Stage(StageCmd::List(_)) => crab::cmd::stage::STAGE_LIST_SCHEMA,
            Self::Freeze(_) => crab::cmd::freeze::FREEZE_SCHEMA,
            Self::Unfreeze(_) => crab::cmd::freeze::UNFREEZE_SCHEMA,
            Self::Exp(ExpCmd::Run(_)) => crab::cmd::exp::EXP_RUN_SCHEMA,
            Self::Exp(ExpCmd::Show(_)) => crab::cmd::exp::EXP_SHOW_SCHEMA,
            Self::Exp(ExpCmd::Diff(_)) => crab::cmd::exp::EXP_DIFF_SCHEMA,
            Self::Exp(ExpCmd::Ls(_)) => crab::cmd::exp::EXP_LS_SCHEMA,
            Self::Exp(ExpCmd::Promote(_)) => crab::cmd::exp::EXP_PROMOTE_SCHEMA,
            Self::Exp(ExpCmd::Apply(_)) => crab::cmd::exp::EXP_APPLY_SCHEMA,
            Self::Exp(ExpCmd::Reset(_)) => crab::cmd::exp::EXP_RESET_SCHEMA,
            Self::Exp(ExpCmd::Save(_)) => crab::cmd::exp::EXP_SAVE_SCHEMA,
            Self::Exp(ExpCmd::Rename(_)) => crab::cmd::exp::EXP_RENAME_SCHEMA,
            Self::Exp(ExpCmd::Push(_)) => crab::cmd::exp::EXP_PUSH_SCHEMA,
            Self::Exp(ExpCmd::Pull(_)) => crab::cmd::exp::EXP_PULL_SCHEMA,
            Self::Exp(ExpCmd::Remove(_)) => crab::cmd::exp::EXP_REMOVE_SCHEMA,
            Self::Exp(ExpCmd::Clean(_)) => crab::cmd::exp::EXP_CLEAN_SCHEMA,
            Self::Exp(ExpCmd::Gc(_)) => crab::cmd::exp::EXP_GC_SCHEMA,
            Self::Exp(ExpCmd::Queue(_)) => crab::cmd::exp_queue::EXP_QUEUE_SCHEMA,
            Self::Exp(ExpCmd::Start(_)) => crab::cmd::exp_queue::EXP_START_SCHEMA,
            Self::Exp(ExpCmd::Status(_)) => crab::cmd::exp_queue::EXP_STATUS_SCHEMA,
            Self::Exp(ExpCmd::Stop(_)) => crab::cmd::exp_queue::EXP_STOP_SCHEMA,
            Self::Queue(QueueCmd::Start(_)) => crab::cmd::exp_queue::EXP_START_SCHEMA,
            Self::Queue(QueueCmd::Status(_)) => crab::cmd::exp_queue::EXP_STATUS_SCHEMA,
            Self::Queue(QueueCmd::Logs(_)) => crab::cmd::exp_queue::EXP_QUEUE_LOGS_SCHEMA,
            Self::Queue(QueueCmd::Kill(_)) => crab::cmd::exp_queue::EXP_QUEUE_KILL_SCHEMA,
            Self::Queue(QueueCmd::Remove(_)) => crab::cmd::exp_queue::EXP_QUEUE_REMOVE_SCHEMA,
            Self::Queue(QueueCmd::Stop(_)) => crab::cmd::exp_queue::EXP_STOP_SCHEMA,
            Self::Params(ParamsCmd::Show(_)) => "params.show",
            Self::Params(ParamsCmd::Diff(_)) => "params.diff",
            Self::Metrics(MetricsCmd::Show(_)) => "metrics.show",
            Self::Metrics(MetricsCmd::Diff(_)) => "metrics.diff",
            Self::Metrics(MetricsCmd::Plot(_)) => "metrics.plot",
            Self::Plots(PlotsCmd::Show(_)) => "metrics.plot",
            Self::Plots(PlotsCmd::Diff(_)) => "metrics.plot",
            Self::Plots(PlotsCmd::Templates(_)) => crab::cmd::metrics::SCHEMA_PLOT_TEMPLATES,
            Self::Workflow(WorkflowCmd::Lockfile(WorkflowLockfileCmd::Resolve(_))) => {
                crab::cmd::workflow_lockfile::WORKFLOW_LOCKFILE_RESOLVE_SCHEMA
            }
            Self::Workflow(WorkflowCmd::Lockfile(WorkflowLockfileCmd::Split(_))) => {
                crab::cmd::workflow_lockfile::WORKFLOW_LOCKFILE_SPLIT_SCHEMA
            }
            Self::Workflow(WorkflowCmd::Status(_)) => {
                crab::cmd::status_workflow::WORKFLOW_STATUS_SCHEMA
            }
            Self::Workflow(WorkflowCmd::Dag(_)) => crab::cmd::dag::WORKFLOW_DAG_SCHEMA,
            Self::Workflow(WorkflowCmd::Journal(WorkflowJournalCmd::Show(_))) => {
                crab::cmd::workflow_journal::WORKFLOW_JOURNAL_SHOW_SCHEMA
            }
            Self::Workflow(WorkflowCmd::Journal(WorkflowJournalCmd::Ls(_))) => {
                crab::cmd::workflow_journal::WORKFLOW_JOURNAL_LS_SCHEMA
            }
            Self::Workflow(WorkflowCmd::Journal(WorkflowJournalCmd::Gc(_))) => {
                crab::cmd::workflow_journal::WORKFLOW_JOURNAL_GC_SCHEMA
            }
            Self::Workflow(WorkflowCmd::PushCache(_)) => {
                crab::cmd::workflow::WORKFLOW_PUSH_CACHE_SCHEMA
            }
            Self::Workflow(WorkflowCmd::Checkpoint(_)) => {
                crab::cmd::workflow_checkpoint::WORKFLOW_CHECKPOINT_SCHEMA
            }
            Self::Lfs(_) => "lfs",
            Self::LfsTransferAgent => "lfs-transfer-agent",
            Self::Mount { .. } => "mount",
            Self::Unmount { .. } => "unmount",
            Self::Daemon {
                sub: Some(DaemonCmd::List { .. }),
                ..
            } => "daemon.list",
            Self::Daemon {
                sub: Some(DaemonCmd::Status { .. }),
                ..
            } => "daemon.status",
            Self::Daemon { .. } => "daemon",
            Self::FilterProcess => "filter-process",
            Self::Version { .. } => "version",
            Self::Skills(crab::cmd::skills::SkillsCommand::List { .. }) => "skills.list",
            Self::Skills(crab::cmd::skills::SkillsCommand::Install(_)) => "skills.install",
            Self::Update { .. } => "update",
            Self::Login { .. } => "login",
            Self::Logout { .. } => "logout",
            Self::Organization(_) => "managed.organization",
            Self::Repo(_) => "managed.repository",
            Self::Member(_) => "managed.member",
            Self::ServiceAccount(_) => "managed.service_account",
            Self::Auth(AuthCmd::Status { .. }) => "auth.status",
            Self::Auth(AuthCmd::Refresh) => "auth.refresh",
            Self::Audit(command) => command.schema_name(),
            Self::Release(command) => command.schema_name(),
            Self::Recover(command) => command.schema_name(),
            Self::Coordinator(_) => "coordinator",
            Self::Completions { .. } => "completions",
            Self::Pull { .. } => "pull",
        }
    }
}

/// Auto-configure `.crab/config.toml` from `.crab.toml` when the filter
/// driver is invoked globally but the repo hasn't been explicitly initialized.
///
/// This runs once per repo — subsequent filter invocations find `.crab/config.toml`
/// and skip. If neither config exists, the filter passes through unchanged
/// (non-crab repo). If `.crab.toml` exists but has no `[remote]` section,
/// logs a warning and passes through.
fn auto_configure_from_project_config() {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return,
    };

    let crab_dir = cwd.join(".crab");
    let config_path = crab_dir.join("config.toml");

    // If .crab/config.toml already exists, nothing to do.
    if config_path.exists() {
        return;
    }

    let crab_toml_path = cwd.join(".crab.toml");
    if !crab_toml_path.exists() {
        // Neither config exists — non-crab repo, filter passes through.
        return;
    }

    // Try to load .crab.toml.
    let project_config = match crab::core::project_config::ProjectConfig::load(&crab_toml_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, ".crab.toml exists but failed to parse, passing through");
            return;
        }
    };

    // Verify the remote section has a URL.
    if project_config.remote.url.is_empty() {
        tracing::warn!(".crab.toml exists but [remote] URL is empty, passing through");
        return;
    }

    // Auto-configure: create .crab/ directory and write config.
    if let Err(e) = std::fs::create_dir_all(&crab_dir) {
        tracing::warn!(error = %e, "failed to create .crab/ for auto-config");
        return;
    }

    let config_content = "# Crab configuration (auto-generated from .crab.toml)\n";
    if let Err(e) = std::fs::write(&config_path, config_content) {
        tracing::warn!(error = %e, "failed to write .crab/config.toml");
        return;
    }

    let remote_path = crab_dir.join("remote");
    if let Err(e) = std::fs::write(&remote_path, &project_config.remote.url) {
        tracing::warn!(error = %e, "failed to write .crab/remote");
        return;
    }

    tracing::debug!("auto-configured from .crab.toml");
}

/// Sync `.gitattributes` with `[track]` patterns from `.crab.toml`.
///
/// Ensures each pattern has a corresponding `filter=crab` rule. Patterns
/// already present are skipped.
fn sync_gitattributes_from_track(root: &Path, patterns: &[String]) {
    use std::fmt::Write as _;

    let ga_path = root.join(".gitattributes");
    let existing = std::fs::read_to_string(&ga_path).unwrap_or_default();

    let already_tracked: std::collections::HashSet<&str> = existing
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#') && t.contains("filter=crab")
        })
        .filter_map(|line| line.split_whitespace().next())
        .collect();

    let mut content = existing.clone();
    let mut added = 0usize;
    for pattern in patterns {
        if already_tracked.contains(pattern.as_str()) {
            continue;
        }
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        let _ = writeln!(content, "{pattern} filter=crab diff=crab merge=crab -text");
        added += 1;
    }

    if added > 0 {
        let _ = std::fs::write(&ga_path, &content);
    }
}

fn main() -> ExitCode {
    // Load .env files so credentials and config (AWS_ACCESS_KEY_ID,
    // AWS_ENDPOINT_URL, CRAB_STORAGE_PROVIDER, etc.) are available
    // without requiring the user to manually `source .env` in every
    // shell session. dotenvy does NOT override variables already set in
    // the environment, so explicit exports always win.
    //
    // Layered credential resolution (highest priority first):
    //   Layer 1: Env vars already set in the process environment
    //            (dotenvy never overrides existing vars)
    //   Layer 2: .env in the git repo root (discovered via gix)
    //   Layer 3: Walk up from CWD looking for a .env or .crab/.env
    //            (covers monorepo / workspace roots above the git repo)
    //   Layer 4: ~/.config/crab/.env (user-global XDG fallback)
    //   Layer 5: AWS default credential chain (handled by object_store's
    //            from_env() — no action needed here)
    //
    // Each layer only fills in values not already set by a higher layer.
    // Silently ignored if no .env file exists at any layer.
    load_env_layered();

    // Pre-scan argv for `--log-level <value>` so we can configure
    // tracing before clap (and the tokio runtime) are built. This
    // keeps runtime-internal spans captured at the requested level.
    let cli_level = pre_scan_log_level();

    // Install tracing FIRST, before any async work, so that
    // runtime-internal spans are captured. `coordinator start` owns its
    // logger because the background coordinator initializes logging after fork.
    let _guard = if coordinator_start_owns_logging(std::env::args()) {
        None
    } else {
        Some(crab::core::tracing_init::install_tracing_subscriber(
            cli_level.as_deref(),
        ))
    };

    // Register gix-tempfile signal handlers for non-cooperative tempfile
    // cleanup. If the process is killed between tempfile creation and
    // atomic rename, gix-tempfile deletes orphan tempfiles automatically.
    // Use `DeleteTempfilesOnTermination` (not the default which restores
    // the default signal handler and aborts) so our cooperative shutdown
    // via CancellationToken can run first.
    gix_tempfile::signal::setup(gix_tempfile::signal::handler::Mode::DeleteTempfilesOnTermination);

    let argv0 = std::env::args_os().next().unwrap_or_default();
    let stem = Path::new(&argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let is_remote_helper = stem == REMOTE_HELPER_STEM;

    // Detect `crab-{subcommand}` symlink patterns (e.g. `crab-gc` → `gc`).
    let symlink_subcmd = symlink_subcommand_for_stem(stem);

    let run_result = match std::thread::Builder::new()
        .name("crab-cli".to_owned())
        .stack_size(CLI_STACK_SIZE)
        .spawn(move || run(is_remote_helper, symlink_subcmd))
    {
        Ok(handle) => match handle.join() {
            Ok(result) => result,
            Err(_) => Err((
                OutputMode::Text,
                "error",
                CrabError::Internal("CLI worker thread panicked".to_owned()),
            )),
        },
        Err(error) => Err((
            OutputMode::Text,
            "error",
            CrabError::Internal(format!("failed to start CLI worker thread: {error}")),
        )),
    };

    match run_result {
        Ok(code) => code,
        Err((mode, schema, err)) => {
            tracing::error!(%err, "fatal error");
            match mode {
                OutputMode::Json => {
                    emit_error_json(schema, "1.0", &err);
                }
                OutputMode::Jsonl => {
                    // JSONL mode: emit a terminal result event with the
                    // structured error through a JsonlStream so consumers
                    // see a well-formed final event on the stream.
                    // The leak is harmless — this runs once on the exit path.
                    let event_schema: &'static str = format!("{schema}.event").leak();
                    let mut stream = JsonlStream::new(event_schema, "1.0", std::io::stdout());
                    stream.emit_error_info(ErrorInfo::from(&err));
                }
                OutputMode::Text => {
                    eprintln!("ERROR: {err}");
                }
            }
            ExitCode::from(err.exit_code())
        }
    }
}

fn symlink_subcommand_for_stem(stem: &str) -> Option<String> {
    if stem == REMOTE_HELPER_STEM || stem == FUSE_MOUNT_STEM || stem == NFS_MOUNT_STEM {
        return None;
    }
    stem.strip_prefix(SYMLINK_PREFIX).map(String::from)
}

/// Lightweight pre-scan of `std::env::args` for `--log-level <value>`.
///
/// We need the log level before clap parses (which happens inside the
/// tokio runtime) so that tracing is configured at the right verbosity
/// from the very start. This only looks for the long form; clap handles
/// the full validation later.
fn pre_scan_log_level() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--log-level" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--log-level=") {
            return Some(value.to_string());
        }
    }
    None
}

fn coordinator_start_owns_logging<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut saw_coordinator = false;
    let mut iter = args.into_iter().skip(1);

    while let Some(arg) = iter.next() {
        let arg = arg.as_ref();
        if arg == "--log-level" {
            let _ = iter.next();
            continue;
        }
        if arg.starts_with("--log-level=") {
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if !saw_coordinator {
            if arg == "coordinator" {
                saw_coordinator = true;
                continue;
            }
            return false;
        }

        return arg == "start";
    }

    false
}

/// Layered credential resolution: repo root → walk-up → XDG global.
///
/// Each call to `dotenvy::from_path` only fills in variables that are
/// NOT already set, so earlier layers win. The load order is:
///   1. Git repo root `.env` (via gix discovery)
///   2. Walk-up from CWD: each ancestor is checked for `.env` and `.crab/.env`
///   3. `~/.config/crab/.env` (user-global fallback)
///
/// Layer 1 (process env) wins implicitly because dotenvy never overrides.
/// Layer 5 (AWS SDK credential chain) is handled by `object_store::from_env()`.
fn load_env_layered() {
    // Track which paths we've already loaded to avoid double-loading.
    let mut loaded: Vec<std::path::PathBuf> = Vec::new();

    // Layer 2: Git repo root .env (the most specific project-level source).
    // This is the fix for the common case where CWD != repo root (e.g.
    // running from a subdirectory, or git invoking the remote helper
    // from a different CWD than where the .env lives).
    if let Some(repo_root) = discover_git_repo_root() {
        let repo_env = repo_root.join(".env");
        if repo_env.is_file() {
            let _ = dotenvy::from_path(&repo_env);
            loaded.push(repo_env);
        }
        // Also check .crab/.env in the repo root for users who prefer
        // to keep credentials separate from the project .env.
        let crab_env = repo_root.join(".crab/.env");
        if crab_env.is_file() {
            let _ = dotenvy::from_path(&crab_env);
            loaded.push(crab_env);
        }
    }

    // Layer 3: Walk up from CWD looking for .env or .crab/.env.
    // This covers workspace roots above the git repo (monorepos)
    // and cases where gix discovery failed (not in a git repo yet).
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd.as_path();
        loop {
            let candidate = dir.join(".env");
            if candidate.is_file() && !loaded.contains(&candidate) {
                let _ = dotenvy::from_path(&candidate);
                loaded.push(candidate);
                break; // Stop at the first ancestor with a .env
            }
            let crab_candidate = dir.join(".crab/.env");
            if crab_candidate.is_file() && !loaded.contains(&crab_candidate) {
                let _ = dotenvy::from_path(&crab_candidate);
                loaded.push(crab_candidate);
                break;
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
    }

    // Layer 4: User-global XDG fallback (~/.config/crab/.env).
    if let Some(global_env) = dirs_env_path()
        && global_env.is_file()
        && !loaded.contains(&global_env)
    {
        let _ = dotenvy::from_path(&global_env);
    }
}

/// Discover the git repository root by walking up from CWD.
///
/// Uses `gix_discover` for correct handling of linked worktrees and
/// nested repos. Returns the worktree root (the directory containing
/// the working tree), not the `.git` directory itself.
fn discover_git_repo_root() -> Option<std::path::PathBuf> {
    if let Ok(ctx) = crab::git::worktree::WorktreeContext::resolve() {
        return Some(ctx.current_worktree_root);
    }

    let (repo_path, _trust) = gix_discover::upwards(std::path::Path::new(".")).ok()?;
    let (_git_dir, work_tree) = repo_path.into_repository_and_work_tree_directories();
    work_tree
}

/// Returns `~/.config/crab/.env` if the home directory is resolvable.
///
/// This provides a user-global fallback for credentials so that repos
/// without a local `.env` still pick up storage config.
fn dirs_env_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|home| home.join(".config/crab/.env"))
}

fn root_help_requested(args: &[OsString]) -> bool {
    let mut visible = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let Some(value) = args[index].to_str() else {
            return false;
        };
        if value == "--log-level" {
            index += 2;
            continue;
        }
        if value.starts_with("--log-level=") {
            index += 1;
            continue;
        }
        visible.push(value);
        index += 1;
    }
    matches!(visible.as_slice(), ["-h" | "--help" | "help"])
}

fn run(
    is_remote_helper: bool,
    symlink_subcmd: Option<String>,
) -> std::result::Result<ExitCode, (OutputMode, &'static str, CrabError)> {
    if !is_remote_helper && symlink_subcmd.is_none() {
        let args: Vec<OsString> = std::env::args_os().collect();
        if root_help_requested(&args) {
            crab::cmd::help::print_root_help(&Cli::command());
            return Ok(ExitCode::SUCCESS);
        }
    }

    if !is_remote_helper
        && symlink_subcmd.is_none()
        && let Some(code) = maybe_run_coordinator_start_standalone()?
    {
        return Ok(code);
    }

    let cancel = CancellationToken::new();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        // FUSE/VFS workloads block FUSE callback threads via rt.block_on.
        // We ensure the tokio runtime has at least 4 worker threads so
        // hydration workers always have headroom while FUSE callbacks
        // are blocked. See finding S1-P12-1 in the e2e code review.
        .worker_threads(std::thread::available_parallelism().map_or(4, |n| n.get().max(4)))
        .enable_all()
        .build()
        .map_err(|e| (OutputMode::Text, "error", CrabError::from(e)))?;

    runtime.block_on(async move {
        spawn_signal_handler(cancel.clone());

        if is_remote_helper {
            // Remote helper never uses machine-mode error envelopes on
            // stdout — git owns that fd.
            run_remote_helper_dispatch(cancel)
                .await
                .map_err(|e| (OutputMode::Text, "error", e))
        } else {
            let cli = match symlink_subcmd {
                Some(subcmd) => {
                    let mut args = vec![OsString::from("crab"), OsString::from(subcmd)];
                    args.extend(std::env::args_os().skip(1));
                    Cli::parse_from(args)
                }
                None => Cli::parse(),
            };

            // Resolve the output mode and schema name before dispatching
            // so we can emit a structured error envelope when a
            // machine-mode command fails.
            let mode = cli.cmd.as_ref().map_or(OutputMode::Text, Cmd::output_mode);
            let schema = cli.cmd.as_ref().map_or("error", Cmd::schema_name);

            crab::core::first_run::maybe_show_welcome(mode);

            run_cli_stub(cli, cancel)
                .await
                .map_err(|e| (mode, schema, e))
        }
    })
}

fn maybe_run_coordinator_start_standalone()
-> std::result::Result<Option<ExitCode>, (OutputMode, &'static str, CrabError)> {
    if !coordinator_start_owns_logging(std::env::args()) {
        return Ok(None);
    }

    let cli = Cli::parse();
    let mode = cli.cmd.as_ref().map_or(OutputMode::Text, Cmd::output_mode);
    let schema = cli.cmd.as_ref().map_or("error", Cmd::schema_name);

    crab::core::first_run::maybe_show_welcome(mode);

    match cli.cmd {
        Some(Cmd::Coordinator(crab::cmd::coordinator::CoordinatorCmd::Start { foreground })) => {
            crab::cmd::coordinator::run_coordinator_start_standalone(foreground)
                .map(Some)
                .map_err(|e| (mode, schema, e))
        }
        _ => Ok(None),
    }
}

/// Spawn a background task that listens for SIGINT (ctrl-c) and SIGTERM.
///
/// - First signal: cancel the token and log a graceful-shutdown message.
///   Pipelines holding a [`MetaDbGuard`] poll the token and close both
///   `SlateDB` instances on their way out, preserving the close-on-exit
///   invariant that prevents WAL corruption.
/// - Second signal of either kind: force-exit immediately for the case
///   where graceful shutdown hangs.
fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        // First shutdown signal — any of SIGINT or SIGTERM triggers it.
        let signal_name = wait_for_shutdown_signal().await;
        tracing::warn!(
            signal = signal_name,
            "received shutdown signal, cancelling in-flight work"
        );
        cancel.cancel();

        // Second signal — no more patience.
        let second_name = wait_for_shutdown_signal().await;
        tracing::error!(
            signal = second_name,
            "received second shutdown signal, force exiting"
        );
        std::process::exit(1);
    });
}

/// Await either SIGINT or SIGTERM, whichever fires first, and return a
/// static name for logging. On non-Unix platforms SIGTERM is not
/// available through `tokio::signal::unix`, so we fall back to ctrl-c
/// only.
async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = term.recv() => "SIGTERM",
                }
            }
            Err(e) => {
                // Registering a SIGTERM handler is vanishingly rare to
                // fail on Unix, but degrade gracefully to SIGINT-only
                // rather than killing the whole signal loop.
                tracing::warn!(error = %e, "failed to register SIGTERM handler, falling back to SIGINT-only");
                tokio::signal::ctrl_c().await.ok();
                "SIGINT"
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
        "SIGINT"
    }
}

/// Dispatch the remote helper protocol loop when invoked as
/// `git-remote-crab <remote> <url>`.
async fn run_remote_helper_dispatch(cancel: CancellationToken) -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().collect();

    let remote = args.get(1).ok_or_else(|| {
        CrabError::Protocol("git-remote-crab requires a remote name argument".into())
    })?;

    let url_str = args
        .get(2)
        .ok_or_else(|| CrabError::Protocol("git-remote-crab requires a URL argument".into()))?;

    let url = gix_url::Url::from_bytes(url_str.as_bytes().into()).map_err(|e| {
        CrabError::Configuration {
            key: format!("invalid URL: {e}"),
            origin: url_str.clone(),
        }
    })?;

    let _span = tracing::info_span!(
        "remote_helper",
        remote = %remote,
        url = %url_str,
    )
    .entered();

    tracing::debug!("starting remote helper");

    let io = RealStdIo;
    run_remote_helper(remote, &url, io, cancel).await?;

    Ok(ExitCode::SUCCESS)
}

/// [`StdIo`] implementation backed by real stdin/stdout for production use.
struct RealStdIo;

impl StdIo for RealStdIo {
    type Reader = BufReader<Stdin>;
    type Writer = Stdout;

    fn split(self) -> (Self::Reader, Self::Writer) {
        (BufReader::new(tokio::io::stdin()), tokio::io::stdout())
    }
}

#[allow(clippy::too_many_lines)]
async fn run_cli_stub(cli: Cli, cancel: CancellationToken) -> Result<ExitCode> {
    match cli.cmd {
        Some(Cmd::Configure {
            remote,
            provider,
            gc_list_profile,
            track,
            no_auto_track,
            dry_run,
        }) => {
            let storage_provider = provider
                .as_deref()
                .map(crab::cmd::init::parse_storage_provider_arg)
                .transpose()?;
            let gc_list_profile = gc_list_profile
                .as_deref()
                .map(crab::core::config::GcListProfile::parse)
                .transpose()?;
            crab::cmd::configure::run_configure(
                crab::cmd::configure::ConfigureArgs {
                    remote,
                    storage_provider,
                    gc_list_profile,
                    track,
                    no_auto_track,
                    dry_run,
                },
                &cancel,
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Stat { json, sub }) => match sub {
            Some(StatCmd::Perf) => {
                let _span = tracing::info_span!("stat_perf").entered();
                let mode = OutputMode::from_flags(json, false);
                let config = Config::resolve_local()?;
                crab::cmd::stat::run_perf(&config.perf_path, mode)?;
                Ok(ExitCode::SUCCESS)
            }
            Some(StatCmd::Classes { json: classes_json }) => {
                let _span = tracing::info_span!("stat_classes").entered();
                let mode = OutputMode::from_flags(json || classes_json, false);
                crab::cmd::stat::run_classes(mode).await?;
                Ok(ExitCode::SUCCESS)
            }
            Some(StatCmd::PushPlan {
                json: push_plan_json,
                verify,
            }) => {
                let root = std::path::PathBuf::from(".crab/staging");
                let _span = tracing::info_span!("stat_push_plan", root = %root.display()).entered();
                let mode = OutputMode::from_flags(json || push_plan_json, false);
                crab::cmd::stat::run_push_plan(&root, verify, mode).await?;
                Ok(ExitCode::SUCCESS)
            }
            None => {
                let root = std::path::PathBuf::from(".crab/staging");
                let mode = OutputMode::from_flags(json, false);
                let _span = tracing::info_span!("stat", root = %root.display()).entered();
                crab::cmd::stat::run(&root, mode).await?;
                Ok(ExitCode::SUCCESS)
            }
        },
        Some(Cmd::Version { json }) => {
            let _span = tracing::info_span!("version").entered();
            let mode = OutputMode::from_flags(json, false);
            crab::cmd::version::run_version(mode)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Skills(sub)) => {
            let _span = tracing::info_span!("skills").entered();
            crab::cmd::skills::run(sub)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Update {
            check,
            yes,
            force,
            json,
        }) => {
            let _span = tracing::info_span!("update").entered();
            let mode = OutputMode::from_flags(json, false);
            let args = crab::cmd::update::UpdateArgs {
                check,
                yes,
                force,
                mode,
            };
            crab::cmd::update::run_update(args).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Login {
            service,
            headless,
            provider,
            enterprise_ca,
            private_ca_only,
        }) => {
            let _span = tracing::info_span!("login").entered();
            let config = Config::resolve_local()?;
            crab::cmd::login::run_login(
                crab::cmd::login::LoginArgs {
                    service,
                    headless,
                    provider,
                    enterprise_ca,
                    private_ca_only,
                },
                &config,
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Logout { service, all }) => {
            let _span = tracing::info_span!("logout").entered();
            let config = Config::resolve_local()?;
            crab::cmd::logout::run_logout(service, all, &config).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Organization(args)) => {
            let _span = tracing::info_span!("managed_organization").entered();
            let config = Config::resolve_local()?;
            crab::cmd::managed_admin::run_organization(args, &config, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Repo(args)) => {
            let _span = tracing::info_span!("managed_repository_admin").entered();
            let config = Config::resolve_local()?;
            crab::cmd::managed_admin::run_repository(args, &config, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Member(args)) => {
            let _span = tracing::info_span!("managed_member").entered();
            let config = Config::resolve_local()?;
            crab::cmd::managed_admin::run_member(args, &config, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::ServiceAccount(args)) => {
            let _span = tracing::info_span!("managed_service_account").entered();
            let config = Config::resolve_local()?;
            crab::cmd::managed_admin::run_service_account(args, &config, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Auth(sub)) => {
            let _span = tracing::info_span!("auth").entered();
            let config = Config::resolve_local()?;
            match sub {
                AuthCmd::Status { json } => {
                    crab::cmd::auth_status::run_auth_status(json, &config)?;
                }
                AuthCmd::Refresh => {
                    run_auth_refresh(&config).await?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Audit(sub)) => {
            let _span = tracing::info_span!("audit").entered();
            crab::cmd::audit::run(&sub)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Release(sub)) => {
            let _span = tracing::info_span!("release").entered();
            crab::cmd::release::run(&sub).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Recover(sub)) => {
            let _span = tracing::info_span!("recover").entered();
            if let crab::cmd::recover::RecoverCmd::History { command } = &sub {
                let config = Config::resolve_local()?;
                if command.applies_restore() || command.applies_prune() {
                    crab::replication::ensure_active_active_maintenance_admitted(
                        &config,
                        "historical manifest maintenance",
                    )?;
                }
                let remote_url =
                    config
                        .remote_url
                        .as_deref()
                        .ok_or_else(|| CrabError::Configuration {
                            key: "remote.url".to_owned(),
                            origin: "historical manifest recovery requires a configured remote"
                                .to_owned(),
                        })?;
                let parsed = crab::git::url::CrabUrl::parse(remote_url)?;
                let selection = crab::replication::StoreResolver::new(&config, parsed, &cancel)
                    .write_store("recover-history")
                    .await?;
                crab::cmd::history_recovery::run(
                    command,
                    &selection.store,
                    selection.router.repo_prefix(),
                    &cancel,
                )
                .await?;
            } else {
                crab::cmd::recover::run(&sub, &cancel).await?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::FilterProcess) => {
            let _span = tracing::info_span!("filter_process").entered();

            // Auto-configure from .crab.toml if .crab/config.toml is missing.
            // This enables the global filter driver to activate automatically
            // in repos that have .crab.toml but haven't run `crab init`.
            auto_configure_from_project_config();

            let config = crab::core::config::Config::resolve_local().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to load config, using defaults");
                crab::core::config::Config::default()
            });
            let ctx = AppContext::new(config.clone(), cancel.clone());

            // Lazy smudge passes pointer bytes through unchanged, so avoid
            // remote setup on the hot checkout path unless content may be
            // materialized inline or through delayed smudge.
            let needs_remote_smudge = filter_process_should_wire_remote_smudge(&config);

            // Try to resolve the LFS remote store for content staging.
            // If not configured, the filter-process still works — content
            // is cached locally and uploaded by the pre-push hook later.
            let lfs_store = if config.checkout.lazy {
                None
            } else {
                crab::cmd::lfs::store_setup::resolve_lfs_remote()
                    .await
                    .ok()
                    .map(|remote_ctx| remote_ctx.store)
            };

            let remote_smudge =
                build_filter_process_remote_smudge(&config, &cancel, needs_remote_smudge).await;

            run_filter_process(
                std::io::stdin(),
                std::io::stdout(),
                ctx,
                lfs_store,
                remote_smudge.prefetch,
                remote_smudge.hydrator,
                #[cfg(unix)]
                Some((
                    std::io::stdin().as_raw_fd(),
                    crab::git::filter_process::FILTER_IDLE_TIMEOUT,
                )),
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Init {
            url,
            storage_provider,
            gc_list_profile,
            mirror,
            json,
            jsonl,
        }) => {
            let cwd = std::env::current_dir()?;
            let mode = OutputMode::from_flags(json, jsonl);
            let storage_provider = storage_provider
                .as_deref()
                .map(crab::cmd::init::parse_storage_provider_arg)
                .transpose()?;
            let gc_list_profile = gc_list_profile
                .as_deref()
                .map(crab::core::config::GcListProfile::parse)
                .transpose()?;
            let resolved_url = match url {
                Some(u) => u,
                None => {
                    // Re-apply mode: discover URL from .crab.toml
                    // Collaborator onboarding: after cloning from GitHub, collaborators
                    // just run `crab init` (no URL needed) to get fully configured with
                    // filter driver, hooks, and hydration from the committed .crab.toml.
                    match crab::core::project_config::ProjectConfig::discover(&cwd) {
                        Some(config) => {
                            let u = config.remote.url.clone();
                            let _span = tracing::info_span!("init", %u).entered();
                            crab::cmd::init::run_init_with_storage_provider(
                                &u,
                                &cwd,
                                &cancel,
                                mode,
                                storage_provider.clone(),
                                gc_list_profile,
                            )
                            .await?;
                            // Sync .gitattributes with [track] patterns from .crab.toml
                            if let Some(ref track) = config.track {
                                sync_gitattributes_from_track(&cwd, &track.patterns);
                            }
                            // Mirror mode: install hooks and add crab remote if [mirror] section present.
                            if config.mirror.is_some() {
                                crab::cmd::install::install_mirror_hooks(&cwd)?;
                                // Ensure crab remote is configured.
                                let _ = std::process::Command::new("git")
                                    .args(["remote", "add", "crab", &u])
                                    .current_dir(&cwd)
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .status();
                                eprintln!(
                                    "Mirror mode detected. Configured crab remote + hooks from .crab.toml"
                                );
                            }
                            // Collaborator onboarding: if [hydrate] config is present, hydrate
                            // according to its settings. Best-effort — requires a valid config.
                            if let Some(ref hydrate_cfg) = config.hydrate
                                && let Some(ref patterns) = hydrate_cfg.auto_patterns
                                && !patterns.is_empty()
                            {
                                let rt_config = Config::resolve_local().unwrap_or_default();
                                let hydrate_args = crab::cmd::hydrate::HydrateArgs {
                                    patterns: patterns.clone(),
                                    include: Vec::new(),
                                    exclude: Vec::new(),
                                    all: false,
                                    manifest: None,
                                    manifest_ref: None,
                                    profile: None,
                                    ignore_sparse: false,
                                    recover_from: None,
                                    mode: crab::core::output::OutputMode::Text,
                                };
                                let _ = crab::cmd::hydrate::run_hydrate(
                                    &hydrate_args,
                                    &rt_config,
                                    &cancel,
                                )
                                .await;
                            }
                            eprintln!("Re-applied configuration from .crab.toml");
                            return Ok(ExitCode::SUCCESS);
                        }
                        None => {
                            // Try interactive prompt if TTY and text mode.
                            match crab::cmd::init::prompt_init_url_interactive(mode) {
                                Ok(url) => url,
                                Err(e) => return Err(e),
                            }
                        }
                    }
                }
            };
            let _span = tracing::info_span!("init", url = %resolved_url).entered();
            crab::cmd::init::run_init_with_storage_provider(
                &resolved_url,
                &cwd,
                &cancel,
                mode,
                storage_provider,
                gc_list_profile,
            )
            .await?;

            // Mirror mode: validate remote, add crab remote, install hooks, write config.
            if let Some(ref mirror_remote) = mirror {
                crab::cmd::init::setup_mirror_mode(&cwd, &resolved_url, mirror_remote)?;
            }

            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Setup {
            no_auto_track,
            track,
            include,
            exclude,
            dry_run,
            force,
            json,
            jsonl,
        }) => {
            let mode = OutputMode::from_flags(json, jsonl);
            let args = crab::cmd::setup::SetupArgs {
                no_auto_track,
                track,
                include,
                exclude,
                dry_run,
                force,
                mode,
            };
            crab::cmd::setup::run_setup(&args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Add {
            patterns,
            jobs,
            dry_run,
            skip_git_add,
            json,
            jsonl,
        }) => {
            let _span = tracing::info_span!("add").entered();
            let mode = OutputMode::from_flags(json, jsonl);
            let args = crab::cmd::add::AddArgs {
                patterns,
                jobs,
                dry_run,
                skip_git_add,
                mode,
            };
            crab::cmd::add::run_add(&args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Reset {
            patterns,
            dry_run,
            sync,
        }) => {
            let _span = tracing::info_span!("reset").entered();
            let args = crab::cmd::reset::ResetArgs {
                patterns,
                dry_run,
                sync,
            };
            crab::cmd::reset::run_reset(&args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Clone {
            url,
            directory,
            branch,
            depth,
            lazy,
            no_lazy,
            eager,
            include,
            exclude,
            sync_chunk_index,
            json,
            jsonl,
        }) => {
            let _span = tracing::info_span!("clone", %url).entered();
            let mode = OutputMode::from_flags(json, jsonl);
            // --eager is a convenience alias: equivalent to --no-lazy.
            let effective_lazy = if eager || no_lazy { false } else { lazy };
            let args = crab::cmd::clone::CloneArgs {
                url,
                directory,
                branch,
                depth,
                lazy: effective_lazy,
                include,
                exclude,
                sync_chunk_index,
                mode,
            };
            let summary = crab::cmd::clone::run_clone(&args, &cancel).await?;

            match mode {
                OutputMode::Json => {
                    crab::core::output::emit_json("clone", "1.0", &summary);
                }
                OutputMode::Jsonl => {
                    // The JSONL stream was created inside run_clone_in;
                    // emit the terminal result here on a fresh stream
                    // (the inner stream is dropped by now).
                    let mut stream = JsonlStream::new("clone.event", "1.0", std::io::stdout());
                    stream.emit_result(&summary);
                }
                OutputMode::Text => {
                    // Text output already handled inside run_clone.
                }
            }

            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Mirror(ref args)) => {
            let _span = tracing::info_span!("mirror", source = %args.source).entered();
            let mode = args.output_mode();
            let summary = crab::cmd::mirror::run_mirror(args, &cancel)?;

            match mode {
                OutputMode::Json => {
                    crab::core::output::emit_json("mirror", "1.0", &summary);
                }
                OutputMode::Jsonl => {
                    let mut stream = JsonlStream::new("mirror.event", "1.0", std::io::stdout());
                    stream.emit_result(&summary);
                }
                OutputMode::Text => {}
            }

            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Download {
            repo,
            paths,
            revision,
            include,
            exclude,
            cache_dir,
            local_dir,
            force_download,
            dry_run,
            max_workers,
            quiet,
            json,
            jsonl,
        }) => {
            let _span = tracing::info_span!("download", %repo).entered();
            let mode = OutputMode::from_flags(json, jsonl);
            let args = crab::cmd::download::DownloadArgs {
                repo,
                paths,
                revision,
                include,
                exclude,
                cache_dir,
                local_dir,
                force_download,
                dry_run,
                max_workers,
                quiet,
                mode,
            };
            crab::cmd::download::run_download(&args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Worktree { json, command }) => {
            let _span = tracing::info_span!("worktree").entered();
            crab::cmd::worktree::run(command, &cancel, json).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Doctor {
            json,
            cost,
            metadb,
            support_bundle,
            cache_service_active_probe,
            output,
            pricing_file,
            inventory_source,
            sample,
            top_k,
        }) => {
            let _span = tracing::info_span!(
                "doctor",
                json,
                cost,
                metadb,
                support_bundle,
                cache_service_active_probe
            )
            .entered();
            let mode = OutputMode::from_flags(json, false);
            if cost {
                let config = Config::resolve_local()?;
                crab::cmd::doctor::run_cost_report(
                    mode,
                    pricing_file,
                    inventory_source,
                    sample,
                    top_k,
                    &config,
                    &cancel,
                )
                .await?;
            } else if metadb {
                crab::cmd::doctor::run_doctor_metadb(mode).await?;
            } else if support_bundle {
                crab::cmd::doctor::run_cache_service_support_bundle(mode, output).await?;
            } else {
                crab::cmd::doctor::run_doctor(mode, cache_service_active_probe).await?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Du { remote, json }) => {
            let _span = tracing::info_span!("du").entered();
            let mode = OutputMode::from_flags(json, false);
            let args = crab::cmd::du::DuArgs { remote, mode };
            crab::cmd::du::run_du(&args).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Track { glob, list, json }) => {
            let mode = OutputMode::from_flags(json, false);
            match glob {
                Some(g) if !list => {
                    let _span = tracing::info_span!("track", glob = %g).entered();
                    crab::cmd::track::run_track(&g)?;
                }
                _ => {
                    let _span = tracing::info_span!("track-list").entered();
                    crab::cmd::track::run_track_list(mode)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Untrack { glob }) => {
            let _span = tracing::info_span!("untrack", %glob).entered();
            crab::cmd::track::run_untrack(&glob)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Gc {
            dry_run,
            force,
            yes,
            scope,
            bucket,
            list_profile,
            grace_period,
            resume,
            deregister,
            repair_registry,
            repair_closures,
            json,
            jsonl,
        }) => {
            let _span = tracing::info_span!("gc").entered();

            let mode = OutputMode::from_flags(json, jsonl);
            let parsed_grace_override = grace_period
                .as_deref()
                .map(parse_duration_str)
                .transpose()?;

            if repair_closures && dry_run {
                return Err(CrabError::Configuration {
                    key: "gc.repair_closures".into(),
                    origin: "closure repair writes durable sidecars; remove --dry-run".into(),
                });
            }
            if repair_closures && scope != "bucket" {
                return Err(CrabError::Configuration {
                    key: "gc.repair_closures".into(),
                    origin: "closure repair is only available for --scope=bucket".into(),
                });
            }

            if repair_registry {
                let bucket_name = bucket.as_deref().ok_or_else(|| CrabError::Configuration {
                    key: "--bucket is required for --repair-registry".into(),
                    origin: "cli".into(),
                })?;
                let config = Config::resolve_local()?;
                crab::replication::ensure_active_active_maintenance_admitted(
                    &config,
                    "registry repair",
                )?;
                let store = create_cli_store(bucket_name, &config, "gc", &cancel).await?;
                let (repos, shards) = crab::cmd::gc::bucket::repair_ref_registry(&store).await?;
                if !mode.is_machine() {
                    eprintln!(
                        "crab gc: ref-registry repaired from {repos} repo manifest(s), {shards} shard root(s)."
                    );
                }
                return Ok(ExitCode::SUCCESS);
            }

            // --deregister mode: remove a repo from the ref-registry.
            if let Some(repo_prefix) = deregister {
                let bucket_name = bucket.as_deref().ok_or_else(|| CrabError::Configuration {
                    key: "--bucket is required for --deregister".into(),
                    origin: "cli".into(),
                })?;
                let config = Config::resolve_local().unwrap_or_default();
                crab::replication::ensure_active_active_maintenance_admitted(
                    &config,
                    "registry deregistration",
                )?;
                let store = create_cli_store(bucket_name, &config, "gc", &cancel).await?;
                crab::cmd::gc::bucket::deregister_repo(&store, &repo_prefix).await?;
                return Ok(ExitCode::SUCCESS);
            }

            match scope.as_str() {
                "bucket" => {
                    let bucket_name =
                        bucket.as_deref().ok_or_else(|| CrabError::Configuration {
                            key: "--bucket is required for --scope=bucket".into(),
                            origin: "cli".into(),
                        })?;
                    let config = Config::resolve_local()?;
                    let parsed_grace = parsed_grace_override.unwrap_or(config.gc_grace_period);
                    let list_profile = list_profile
                        .as_deref()
                        .map(crab::core::config::GcListProfile::parse)
                        .transpose()?
                        .unwrap_or(config.gc.list_profile);
                    let store = create_cli_store(bucket_name, &config, "gc", &cancel).await?;
                    let current_repo_prefix = if config
                        .replication
                        .as_ref()
                        .is_some_and(crab::replication::ReplicationConfig::is_active_active)
                    {
                        let remote_url =
                            config
                                .remote_url
                                .as_deref()
                                .ok_or_else(|| CrabError::Configuration {
                                    key: "remote.url".into(),
                                    origin: "active-active bucket garbage collection requires a configured primary remote to verify coordinator registrations".into(),
                                })?;
                        Some(crab::git::url::CrabUrl::parse(remote_url)?.repo_path)
                    } else {
                        None
                    };
                    if repair_closures {
                        let repaired = crab::cmd::gc::bucket::repair_bucket_closures_with_config(
                            &store,
                            &config,
                            current_repo_prefix.as_deref(),
                            config.gc_list_concurrency,
                            &cancel,
                        )
                        .await?;
                        if mode.is_machine() {
                            let payload = serde_json::json!({
                                "repaired_closures": repaired,
                                "dry_run": false,
                            });
                            match mode {
                                OutputMode::Json => {
                                    crab::core::output::emit_json(
                                        "gc.repair_closures",
                                        "1.0",
                                        &payload,
                                    );
                                }
                                OutputMode::Jsonl => {
                                    let mut stream = JsonlStream::new(
                                        "gc.repair_closures.event",
                                        "1.0",
                                        std::io::stdout(),
                                    );
                                    stream.emit_result(&payload);
                                }
                                OutputMode::Text => {}
                            }
                        } else {
                            eprintln!(
                                "crab gc: repaired and verified {repaired} shard closure(s)."
                            );
                        }
                        return Ok(ExitCode::SUCCESS);
                    }
                    let args = crab::cmd::gc::bucket::BucketGcArgs {
                        bucket: bucket_name.to_string(),
                        dry_run,
                        grace_period: parsed_grace,
                        force,
                        yes,
                        list_concurrency: config.gc_list_concurrency,
                        list_profile,
                        delete_concurrency: config.gc_delete_concurrency,
                        resume_run_id: resume,
                    };
                    let outcome = crab::cmd::gc::bucket::run_bucket_gc_with_config(
                        &args,
                        &store,
                        &config,
                        current_repo_prefix.as_deref(),
                        &cancel,
                    )
                    .await?;
                    let summary = outcome.to_summary();
                    match mode {
                        OutputMode::Text => {
                            let verb = if dry_run { "would delete" } else { "deleted" };
                            eprintln!(
                                "crab gc: bucket GC complete; {verb} {} xorb(s), {} shard(s), tombstoned {} file-index row(s), reclaimed {} byte(s).",
                                summary.xorbs_deleted,
                                summary.shards_deleted,
                                summary.file_index_entries_deleted,
                                summary.bytes_reclaimed,
                            );
                        }
                        OutputMode::Json => {
                            crab::core::output::emit_json("gc", "1.0", &summary);
                        }
                        OutputMode::Jsonl => {
                            let mut stream = JsonlStream::new("gc.event", "1.0", std::io::stdout());
                            stream.emit_result(&summary);
                        }
                    }
                }
                "repo" => {
                    let config = Config::resolve_local().unwrap_or_default();
                    let parsed_grace = parsed_grace_override.unwrap_or(config.gc_grace_period);
                    let args = crab::cmd::gc::GcArgs {
                        dry_run,
                        force,
                        yes,
                        mode,
                        force_early_delete: false,
                        yes_really: false,
                        delete_concurrency: config.gc_delete_concurrency,
                        list_concurrency: config.gc_list_concurrency,
                        resume_run_id: resume,
                    };
                    tracing::info!(
                        dry_run = args.dry_run,
                        force = args.force,
                        "starting repo-scope garbage collection"
                    );

                    // Clean expired on-disk shard files from the per-repo
                    // shard cache. These are MDBShardFile handles created
                    // when the in-memory ChunkIndex exceeds its ceiling.
                    let cache_dir = crab::cache::default_cache_root();
                    let repos_dir = cache_dir.join("repos");
                    if repos_dir.is_dir() {
                        let expiration_secs = parsed_grace.as_secs();
                        if let Ok(entries) = std::fs::read_dir(&repos_dir) {
                            for entry in entries.flatten() {
                                let shards_dir = entry.path().join("shards");
                                if shards_dir.is_dir() {
                                    if args.dry_run {
                                        tracing::info!(
                                            path = %shards_dir.display(),
                                            "would clean shard cache (dry-run)"
                                        );
                                    } else {
                                        let shard_file_cache =
                                            crab_xet::shard::new_shard_file_cache();
                                        match crab_xet::shard::MDBShardFile::clean_shard_cache(
                                            &shards_dir,
                                            expiration_secs,
                                            &shard_file_cache,
                                        ) {
                                            Ok(()) => {
                                                tracing::debug!(
                                                    path = %shards_dir.display(),
                                                    "cleaned shard cache"
                                                );
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    path = %shards_dir.display(),
                                                    error = %e,
                                                    "failed to clean shard cache"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if let Some(remote_url) = config.remote_url.as_deref() {
                        let parsed_url = crab::git::url::CrabUrl::parse(remote_url)?;
                        let selection =
                            crab::replication::StoreResolver::new(&config, &parsed_url, &cancel)
                                .write_store("gc")
                                .await?;
                        let active_active_fence = if args.dry_run {
                            None
                        } else {
                            crab::replication::ActiveActiveGcFence::acquire(
                                &config,
                                selection.router.repo_prefix(),
                            )
                            .await?
                        };
                        let coordinator_protected_keys = match &active_active_fence {
                            Some(fence) => fence.protected_keys().clone(),
                            None => {
                                crab::replication::active_active_gc_protected_keys(
                                    &config,
                                    selection.router.repo_prefix(),
                                )
                                .await?
                            }
                        };
                        let jsonl_stream = if mode == OutputMode::Jsonl {
                            Some(std::sync::Mutex::new(JsonlStream::new(
                                "gc.event",
                                "1.0",
                                std::io::stdout(),
                            )))
                        } else {
                            None
                        };
                        let operation = crab::cmd::gc::run_repo_remote_gc(
                            &args,
                            &selection.store,
                            &selection.router,
                            &coordinator_protected_keys,
                            &cancel,
                            parsed_grace,
                            jsonl_stream.as_ref(),
                        )
                        .await;
                        let release = match active_active_fence {
                            Some(fence) => fence.release().await,
                            None => Ok(()),
                        };
                        let outcome = match (operation, release) {
                            (Ok(outcome), Ok(())) => outcome,
                            (Err(error), _) | (Ok(_), Err(error)) => return Err(error),
                        };
                        let summary = outcome.to_summary();
                        match mode {
                            OutputMode::Text => {
                                let verb = if args.dry_run {
                                    "would delete"
                                } else {
                                    "deleted"
                                };
                                eprintln!(
                                    "crab gc: repo remote GC complete; {verb} {} pack(s), {} xorb(s), {} shard(s), reclaimed {} byte(s).",
                                    summary.packs_deleted,
                                    summary.xorbs_deleted,
                                    summary.shards_deleted,
                                    summary.bytes_reclaimed,
                                );
                            }
                            OutputMode::Json => {
                                crab::core::output::emit_json("gc", "1.0", &summary);
                            }
                            OutputMode::Jsonl => {
                                if let Some(stream) = &jsonl_stream
                                    && let Ok(mut stream) = stream.lock()
                                {
                                    stream.emit_result(&summary);
                                }
                            }
                        }
                    } else if !mode.is_machine() {
                        eprintln!(
                            "crab gc: local shard cache cleaned. Configure [remote].url to enable repo remote GC.",
                        );
                    }
                }
                other => {
                    return Err(CrabError::Configuration {
                        key: format!(
                            "unknown --scope value: {other} (expected 'repo' or 'bucket')"
                        ),
                        origin: "cli".into(),
                    });
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Compact {
            repo,
            bucket,
            dry_run,
            max_shard_size,
        }) => run_compact_command(repo, bucket, dry_run, max_shard_size, &cancel).await,
        Some(Cmd::Fsck {
            repair,
            json,
            jsonl,
        }) => {
            let _span = tracing::info_span!("fsck").entered();
            let mode = OutputMode::from_flags(json, jsonl);
            let args = crab::cmd::fsck::FsckArgs { repair, mode };
            tracing::info!(repair = args.repair, "starting fsck");

            let config = Config::resolve_local()?;
            if repair {
                crab::replication::ensure_active_active_maintenance_admitted(
                    &config,
                    "fsck repair",
                )?;
            }

            let url = config
                .remote_url
                .as_deref()
                .ok_or_else(|| CrabError::Configuration {
                    key: "missing [remote] url in .crab/config.toml".into(),
                    origin: ".crab/config.toml".into(),
                })?;
            let parsed = crab::git::url::CrabUrl::parse(url)?;
            let prefix = parsed.repo_path.clone();
            let store = create_cli_store(&parsed.bucket, &config, "fsck", &cancel).await?;

            let checker = crab::cmd::fsck_store::StoreChecker::new(store.clone(), prefix.clone());
            let repairer: Box<dyn crab::cmd::fsck::FsckRepairer> = if repair {
                Box::new(crab::cmd::fsck_store::StoreRepairer::new(store, prefix))
            } else {
                Box::new(crab::cmd::fsck::NullRepairer)
            };

            let grace_period = config.gc_grace_period;

            // Set up JSONL stream for streaming mode.
            let jsonl_stream = if mode == OutputMode::Jsonl {
                Some(std::sync::Mutex::new(JsonlStream::new(
                    "fsck.event",
                    "1.0",
                    std::io::stdout(),
                )))
            } else {
                None
            };

            let (issues, outcome) = crab::cmd::fsck::run_fsck(
                &args,
                &checker,
                repairer.as_ref(),
                &cancel,
                grace_period,
                jsonl_stream.as_ref(),
            )
            .await?;

            let summary = outcome.to_summary();

            match mode {
                OutputMode::Json => {
                    crab::core::output::emit_json("fsck", "1.0", &summary);
                }
                OutputMode::Jsonl => {
                    if let Some(stream) = &jsonl_stream
                        && let Ok(mut s) = stream.lock()
                    {
                        s.emit_result(&summary);
                    }
                }
                OutputMode::Text => {
                    // Print summary to stderr.
                    if outcome.errors == 0 && outcome.info_count == 0 {
                        eprintln!("crab fsck: repository is clean");
                    } else {
                        eprintln!(
                            "crab fsck: {} error(s), {} info, {} repaired, {} repair failure(s)",
                            outcome.errors,
                            outcome.info_count,
                            outcome.repaired,
                            outcome.repair_failures,
                        );
                    }
                }
            }

            // Exit non-zero when errors are found and not all repaired.
            if outcome.errors > 0 && outcome.errors > outcome.repaired {
                let _ = issues; // consumed by run_fsck logging
                Ok(ExitCode::from(1))
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
        Some(Cmd::Repack {
            dry_run,
            json,
            jsonl,
        }) => run_repack_command(dry_run, json, jsonl, &cancel).await,
        Some(Cmd::Optimize(sub)) => run_optimize_command(sub, &cancel).await,
        Some(Cmd::Tier(sub)) => run_tier_command(sub, &cancel).await,
        Some(Cmd::Metadb(sub)) => run_metadb_command(sub, &cancel).await,
        Some(Cmd::Cache(sub)) => run_cache_command(sub).await,
        Some(Cmd::Config(sub)) => {
            let _span = tracing::info_span!("config").entered();
            match sub {
                ConfigCmd::Get { key, json } => {
                    let mode = OutputMode::from_flags(json, false);
                    crab::cmd::config::run_config_get(&key, mode)?;
                }
                ConfigCmd::Set { key, value } => {
                    crab::cmd::config::run_config_set(&key, &value)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Replica(sub)) => {
            let _span = tracing::info_span!("replica").entered();
            crab::cmd::replica::exec(sub, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Completions { shell, install }) => {
            let _span = tracing::info_span!("completions").entered();
            let mut cmd = Cli::command();
            crab::cmd::completions::run_completions(&mut cmd, &shell, install)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Staging(sub)) => {
            let _span = tracing::info_span!("staging").entered();
            match sub {
                StagingCmd::Stats { json } => {
                    let mode = OutputMode::from_flags(json, false);
                    crab::cmd::staging::run_staging_stats(mode).await?;
                }
                StagingCmd::Verify => {
                    crab::cmd::staging::run_staging_verify().await?;
                }
                StagingCmd::Clean {
                    force,
                    prune_abandoned,
                } => {
                    crab::cmd::staging::run_staging_clean(&cancel, force, prune_abandoned).await?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Errors { code, json }) => {
            let _span = tracing::info_span!("errors").entered();
            let mode = OutputMode::from_flags(json, false);
            if crab::cmd::errors::run_errors(mode, code.as_deref())? {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(1))
            }
        }
        Some(Cmd::Status {
            porcelain,
            workflow,
            why,
            recursive,
            lockfile,
            with_deps,
            cloud,
            remote,
            json,
            targets,
        }) => {
            let _span = tracing::info_span!("status", porcelain, workflow, json).entered();
            let mode = OutputMode::from_flags(json, false);
            if workflow {
                let args = crab::cmd::status_workflow::StatusArgs {
                    why,
                    recursive,
                    lockfile,
                    with_deps,
                    cloud,
                    remote,
                    json,
                    targets,
                };
                crab::cmd::status_workflow::exec_async(args).await?;
                return Ok(ExitCode::SUCCESS);
            }
            crab::cmd::status::run_unified_status(porcelain, mode)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Why { file, json }) => {
            let _span = tracing::info_span!("why", %file).entered();
            let mode = OutputMode::from_flags(json, false);
            crab::cmd::why::run_why(&file, mode)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Hydrate {
            patterns,
            include,
            exclude,
            all,
            manifest,
            manifest_ref,
            profile,
            ignore_sparse,
            recover_from,
            restore,
            no_restore,
            restore_tier,
            restore_duration_days,
            clear_speculation,
            json,
            jsonl,
        }) => {
            let _span = tracing::info_span!("hydrate").entered();

            // --clear-speculation: wipe the speculation DB and exit.
            if clear_speculation {
                crab::cmd::hydrate::run_clear_speculation()?;
                return Ok(ExitCode::SUCCESS);
            }

            let mode = OutputMode::from_flags(json, jsonl);
            let args = crab::cmd::hydrate::HydrateArgs {
                patterns,
                include,
                exclude,
                all,
                mode,
                manifest,
                manifest_ref,
                profile,
                ignore_sparse,
                recover_from,
            };
            let config = match Config::resolve_local() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load config for hydrate, using defaults");
                    Config::default()
                }
            };

            // Try to create a cloud-backed hydrator from .crab/remote.
            // Falls back to the SmudgeSession hydrator only when the remote is
            // absent. Configured remotes fail closed so forced replica policies
            // cannot silently hydrate from another source.
            let remote_path = crab::git::discover::resolve_crab_dir().map_or_else(
                || std::path::PathBuf::from(".crab/remote"),
                |d| d.join("remote"),
            );
            if let Some(parsed) = resolve_hydrate_remote_url(&remote_path)? {
                let selection =
                    crab::replication::select_read_store(&config, &parsed, "hydrate", &cancel)
                        .await?;
                if let crab::replication::ReadSource::Replica { name } = &selection.source {
                    tracing::debug!(replica = %name, "selected read replica for hydrate");
                }
                let router = selection.router;
                let caching_store =
                    crab_cache_store::CachingStore::new(selection.store, &config.cache)?;
                // Bulk hydrate is a one-pass stream already backed by the full-xorb cache.
                // A bounded decoded-range cache only adds writes and eviction churn here.
                let mut hydrator = crab::cmd::hydrate::ShardHydrator::with_config_from_cli_layout(
                    caching_store,
                    router,
                    &config,
                )?;
                let restore_flags = crab::cmd::hydrate_restore::RestoreFlags {
                    restore,
                    no_restore,
                    restore_tier: restore_tier.clone(),
                    restore_duration_days,
                };
                let requested_restore =
                    restore_flags.resolve_auto_restore(config.hydrate.auto_restore);
                if restore && !config.tier.enabled {
                    return Err(crab::core::error::CrabError::Configuration {
                        key: "tier.enabled is false; cannot restore archived xorbs".into(),
                        origin: "hydrate --restore".into(),
                    });
                }
                if requested_restore && config.tier.enabled {
                    let mut options = crab::tier::runtime::restore_options_from_config(&config)?;
                    if let Some(tier) = &restore_flags.restore_tier {
                        options.tier = crab::tier::runtime::parse_restore_tier(tier)?;
                    }
                    if let Some(days) = restore_flags.restore_duration_days {
                        options.duration = std::time::Duration::from_secs(u64::from(days) * 86_400);
                    }
                    let backend =
                        crab::tier::runtime::build_restore_backend(&config, &parsed).await?;
                    let orchestrator = std::sync::Arc::new(
                        crab::tier::restore::RestoreOrchestrator::with_options(
                            backend,
                            config.tier.restore_max_concurrency,
                            std::time::Duration::from_secs(config.tier.restore_timeout_secs),
                            options,
                        ),
                    );
                    hydrator = hydrator.with_restore(Some(orchestrator), true);
                } else {
                    hydrator = hydrator.with_restore(None, false);
                }
                let cwd = std::env::current_dir()?;
                crab::cmd::hydrate::run_hydrate_in(&cwd, &args, &config, &hydrator, &cancel)
                    .await?;
                return Ok(ExitCode::SUCCESS);
            }

            // Fallback to default hydrator.
            crab::cmd::hydrate::run_hydrate(&args, &config, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Diff {
            ref1,
            ref2,
            json,
            stat,
            name_only,
            verbose,
            byte_ranges,
            no_color,
            no_annotations,
            paths,
        }) => {
            let _span = tracing::info_span!("diff", %ref1).entered();
            let args = crab::cmd::diff::DiffArgs {
                ref1,
                ref2,
                paths,
                mode: OutputMode::from_flags(json, false),
                stat,
                name_only,
                verbose,
                byte_ranges,
                no_color,
                no_annotations,
            };
            let config = match Config::resolve_local() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load config for diff, using defaults");
                    Config::default()
                }
            };
            crab::cmd::diff::run_diff(args, config, cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::DiffDriver {
            path,
            old_file,
            new_file,
            old_hex,
            old_mode,
            new_hex,
            new_mode,
        }) => {
            let _span = tracing::info_span!("diff_driver", %path).entered();
            let args = crab::cmd::diff_driver::DiffDriverArgs {
                path,
                old_file,
                new_file,
                old_hex,
                old_mode,
                new_hex,
                new_mode,
            };
            let config = match Config::resolve_local() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load config for diff-driver, using defaults");
                    Config::default()
                }
            };
            crab::cmd::diff_driver::run_diff_driver(args, config, cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Dehydrate {
            patterns,
            all,
            ignore_profiles,
            json,
            jsonl,
        }) => {
            let _span = tracing::info_span!("dehydrate").entered();
            let mode = OutputMode::from_flags(json, jsonl);
            let args = crab::cmd::dehydrate::DehydrateArgs {
                patterns,
                all,
                ignore_profiles,
                mode,
            };
            crab::cmd::dehydrate::run_dehydrate(&args, &cancel)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Env { json }) => {
            let _span = tracing::info_span!("env", json).entered();
            let mode = OutputMode::from_flags(json, false);
            crab::cmd::env::run_env(mode)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::LsFiles {
            long,
            size,
            name_only,
            json,
            debug,
        }) => {
            let _span = tracing::info_span!("ls_files").entered();
            let mode = OutputMode::from_flags(json, false);
            let args = crab::cmd::ls_files::LsFilesArgs {
                long,
                size,
                name_only,
                mode,
                debug,
            };
            crab::cmd::ls_files::run_ls_files(&args)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Fetch {
            include,
            exclude,
            all,
            dry_run,
            no_sync_chunk_index,
            json,
            jsonl,
        }) => {
            run_fetch_command(
                include,
                exclude,
                all,
                dry_run,
                no_sync_chunk_index,
                json,
                jsonl,
                &cancel,
            )
            .await
        }
        Some(Cmd::Prune {
            dry_run,
            verbose,
            json,
            jsonl,
        }) => run_prune_command(dry_run, verbose, json, jsonl).await,
        Some(Cmd::Logs(sub)) => {
            let _span = tracing::info_span!("logs").entered();
            match sub {
                LogsCmd::List => crab::cmd::logs::run_logs_list()?,
                LogsCmd::Last => crab::cmd::logs::run_logs_last()?,
                LogsCmd::Show { name } => crab::cmd::logs::run_logs_show(&name)?,
                LogsCmd::Clear => crab::cmd::logs::run_logs_clear()?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Install {
            global,
            system,
            force,
            skip_smudge,
            aliases,
            no_completions,
        }) => {
            let _span = tracing::info_span!("install").entered();
            let scope = if system {
                crab::cmd::install::InstallScope::System
            } else if global {
                crab::cmd::install::InstallScope::Global
            } else {
                crab::cmd::install::InstallScope::Local
            };
            let args = crab::cmd::install::InstallArgs {
                scope,
                force,
                skip_smudge,
                aliases,
                no_completions,
            };
            crab::cmd::install::run_install(&args)?;

            // After global install: run credential discovery as informational check.
            if scope == crab::cmd::install::InstallScope::Global {
                let result = crab::core::credential_discovery::discover_credentials(
                    "crab://example/repo",
                    None,
                )
                .await;
                if result.valid {
                    eprintln!("✓ Credentials: {}", result.description);
                } else {
                    eprintln!("ℹ Credentials: {}", result.description);
                }
            }

            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Uninstall { global, system }) => {
            let _span = tracing::info_span!("uninstall").entered();
            let scope = if system {
                crab::cmd::install::InstallScope::System
            } else if global {
                crab::cmd::install::InstallScope::Global
            } else {
                crab::cmd::install::InstallScope::Local
            };
            crab::cmd::install::run_uninstall(scope)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Lock { paths, json }) => {
            let _span = tracing::info_span!("lock").entered();
            let mode = OutputMode::from_flags(json, false);
            crab::cmd::lock::run_lock(&paths, mode).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Unlock { paths, force, json }) => {
            let _span = tracing::info_span!("unlock").entered();
            let mode = OutputMode::from_flags(json, false);
            crab::cmd::lock::run_unlock(&paths, force, mode).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Locks {
            path,
            owner,
            limit,
            json,
        }) => {
            let _span = tracing::info_span!("locks").entered();
            let mode = OutputMode::from_flags(json, false);
            crab::cmd::lock::run_locks(path.as_deref(), owner.as_deref(), mode, limit).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Migrate(sub)) => {
            let _span = tracing::info_span!("migrate").entered();
            match sub {
                MigrateCmd::Info { above, top } => {
                    let args = crab::cmd::migrate::MigrateInfoArgs { above, top };
                    crab::cmd::migrate::run_migrate_info(&args)?;
                }
                MigrateCmd::Import {
                    include,
                    exclude,
                    above,
                    dry_run,
                    everything,
                } => {
                    let args = crab::cmd::migrate::MigrateImportArgs {
                        include,
                        exclude,
                        above,
                        dry_run,
                        everything,
                    };
                    crab::cmd::migrate::run_migrate_import(&args)?;
                }
                MigrateCmd::Export { include, dry_run } => {
                    let args = crab::cmd::migrate::MigrateExportArgs { include, dry_run };
                    crab::cmd::migrate::run_migrate_export(&args)?;
                }
                MigrateCmd::FromDvc {
                    dir,
                    stdout,
                    output,
                    plan,
                    resume,
                    remote_map,
                    json,
                    jsonl,
                } => {
                    crab::cmd::migrate::run_migrate_from_dvc_with_options(
                        dir.as_deref(),
                        stdout,
                        output.as_deref(),
                        crab::cmd::migrate::DvcMigrationOptions {
                            plan,
                            resume,
                            mode: OutputMode::from_flags(json, jsonl),
                            remote_map,
                        },
                    )?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Artifacts(command)) => {
            let _span = tracing::info_span!("artifacts").entered();
            command.run()?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Push(ref args)) => {
            let _span = tracing::info_span!("push").entered();
            crab::cmd::push::run_push(args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Pull {
            remote,
            branch,
            no_hydrate,
            json,
            jsonl,
        }) => {
            let _span = tracing::info_span!("pull").entered();
            let mode = OutputMode::from_flags(json, jsonl);
            let args = crab::cmd::pull::PullArgs {
                remote,
                branch,
                no_hydrate,
                mode,
            };
            crab::cmd::pull::run_pull(&args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Ship {
            patterns,
            message,
            jobs,
            remote,
            branch,
            rebase_on_non_fast_forward,
            rebase_retry_limit,
            no_push,
            dry_run,
            json,
        }) => {
            let _span = tracing::info_span!("ship").entered();
            let mode = OutputMode::from_flags(json, false);
            let effective_patterns = if patterns.is_empty() {
                vec![".".to_string()]
            } else {
                patterns
            };
            let args = crab::cmd::ship::ShipArgs {
                patterns: effective_patterns,
                message,
                jobs,
                remote,
                branch,
                rebase_on_non_fast_forward,
                rebase_retry_limit,
                no_push,
                dry_run,
                mode,
            };
            crab::cmd::ship::run_ship(&args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Adopt {
            pattern,
            rewrite_history,
            force,
            dry_run,
            interactive,
            jobs,
            json,
        }) => {
            let _span = tracing::info_span!("adopt").entered();
            let mode = OutputMode::from_flags(json, false);
            let args = crab::cmd::adopt::AdoptArgs {
                patterns: pattern,
                rewrite_history,
                force,
                dry_run,
                jobs,
                mode,
                interactive,
            };
            crab::cmd::adopt::run_adopt(&args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Unadopt { pattern, json }) => {
            let _span = tracing::info_span!("unadopt").entered();
            let mode = OutputMode::from_flags(json, false);
            let args = crab::cmd::unadopt::UnadoptArgs {
                patterns: pattern,
                mode,
            };
            crab::cmd::unadopt::run_unadopt(&args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Undo { json }) => {
            let _span = tracing::info_span!("undo").entered();
            let mode = OutputMode::from_flags(json, false);
            crab::cmd::undo::run_undo(mode, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Import(ref args)) => {
            let _span = tracing::info_span!("import").entered();
            crab::cmd::import::run_import(args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Data(command)) => {
            let _span = tracing::info_span!("data").entered();
            command.run()?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Export(ref args)) => {
            let _span = tracing::info_span!("export").entered();
            crab::cmd::export::run_export(args, &cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Run(args)) => {
            let _span = tracing::info_span!("run").entered();
            crab::cmd::run::exec(args).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Repro(args)) => {
            let _span = tracing::info_span!("repro").entered();
            crab::cmd::run::exec(args).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Stage(sub)) => {
            let _span = tracing::info_span!("stage").entered();
            match sub {
                StageCmd::Add(args) => crab::cmd::stage::exec_add(args).await?,
                StageCmd::List(args) => crab::cmd::stage::exec_list(args)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Freeze(args)) => {
            let _span = tracing::info_span!("freeze").entered();
            crab::cmd::freeze::exec_freeze(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Unfreeze(args)) => {
            let _span = tracing::info_span!("unfreeze").entered();
            crab::cmd::freeze::exec_unfreeze(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Exp(sub)) => {
            let _span = tracing::info_span!("exp").entered();
            match sub {
                ExpCmd::Run(args) => crab::cmd::exp::exec_run(args).await?,
                ExpCmd::Show(args) => crab::cmd::exp::exec_show(args)?,
                ExpCmd::Diff(args) => crab::cmd::exp::exec_diff(args)?,
                ExpCmd::Ls(args) => crab::cmd::exp::exec_ls(args)?,
                ExpCmd::Promote(args) => crab::cmd::exp::exec_promote(args)?,
                ExpCmd::Apply(args) => crab::cmd::exp::exec_apply(args)?,
                ExpCmd::Reset(args) => crab::cmd::exp::exec_reset(args)?,
                ExpCmd::Save(args) => crab::cmd::exp::exec_save(args)?,
                ExpCmd::Rename(args) => crab::cmd::exp::exec_rename(args)?,
                ExpCmd::Push(args) => crab::cmd::exp::exec_push(args).await?,
                ExpCmd::Pull(args) => crab::cmd::exp::exec_pull(args).await?,
                ExpCmd::Remove(args) => crab::cmd::exp::exec_remove(args).await?,
                ExpCmd::Clean(args) => crab::cmd::exp::exec_clean(args)?,
                ExpCmd::Gc(args) => crab::cmd::exp::exec_gc(args)?,
                ExpCmd::Queue(args) => crab::cmd::exp_queue::exec_queue(args)?,
                ExpCmd::Start(args) => crab::cmd::exp_queue::exec_start(args).await?,
                ExpCmd::Status(args) => crab::cmd::exp_queue::exec_status(args)?,
                ExpCmd::Stop(args) => crab::cmd::exp_queue::exec_stop(args)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Queue(sub)) => {
            let _span = tracing::info_span!("queue").entered();
            match sub {
                QueueCmd::Start(args) => crab::cmd::exp_queue::exec_start(args).await?,
                QueueCmd::Status(args) => crab::cmd::exp_queue::exec_status(args)?,
                QueueCmd::Logs(args) => crab::cmd::exp_queue::exec_queue_logs(args)?,
                QueueCmd::Kill(args) => crab::cmd::exp_queue::exec_queue_kill(args)?,
                QueueCmd::Remove(args) => crab::cmd::exp_queue::exec_queue_remove(args)?,
                QueueCmd::Stop(args) => crab::cmd::exp_queue::exec_stop(args)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Workflow(sub)) => {
            let _span = tracing::info_span!("workflow").entered();
            match sub {
                WorkflowCmd::Lockfile(WorkflowLockfileCmd::Resolve(args)) => {
                    crab::cmd::workflow_lockfile::exec(args)?;
                }
                WorkflowCmd::Lockfile(WorkflowLockfileCmd::Split(args)) => {
                    crab::cmd::workflow_lockfile::exec_split(args)?;
                }
                WorkflowCmd::Status(args) => {
                    crab::cmd::status_workflow::exec_async(args).await?;
                }
                WorkflowCmd::Dag(args) => {
                    crab::cmd::dag::exec(args)?;
                }
                WorkflowCmd::Journal(WorkflowJournalCmd::Show(args)) => {
                    crab::cmd::workflow_journal::exec_show(args)?;
                }
                WorkflowCmd::Journal(WorkflowJournalCmd::Ls(args)) => {
                    crab::cmd::workflow_journal::exec_ls(args)?;
                }
                WorkflowCmd::Journal(WorkflowJournalCmd::Gc(args)) => {
                    crab::cmd::workflow_journal::exec_gc(args)?;
                }
                WorkflowCmd::PushCache(args) => {
                    crab::cmd::workflow::exec_push_cache(args).await?;
                }
                WorkflowCmd::Checkpoint(args) => {
                    crab::cmd::workflow_checkpoint::run(&args)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Params(sub)) => {
            let _span = tracing::info_span!("params").entered();
            match sub {
                ParamsCmd::Show(args) => crab::cmd::params::exec_show(args)?,
                ParamsCmd::Diff(args) => crab::cmd::params::exec_diff(args)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Metrics(sub)) => {
            let _span = tracing::info_span!("metrics").entered();
            match sub {
                MetricsCmd::Show(args) => crab::cmd::metrics::exec_show(args)?,
                MetricsCmd::Diff(args) => crab::cmd::metrics::exec_diff(args)?,
                MetricsCmd::Plot(args) => crab::cmd::metrics::exec_plot(args)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Plots(sub)) => {
            let _span = tracing::info_span!("plots").entered();
            match sub {
                PlotsCmd::Show(args) => crab::cmd::metrics::exec_plot_show(args)?,
                PlotsCmd::Diff(args) => crab::cmd::metrics::exec_plot_diff(args)?,
                PlotsCmd::Templates(args) => crab::cmd::metrics::exec_plot_templates(args)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Lfs(sub)) => {
            let _span = tracing::info_span!("lfs").entered();
            if let crab::cmd::lfs::LfsCmd::Completion { shell } = &sub {
                let mut cmd = Cli::command();
                return crab::cmd::lfs::completion::run_lfs_completion(shell, &mut cmd);
            }
            let exit = crab::cmd::lfs::run_lfs(&sub)?;
            Ok(exit)
        }
        Some(Cmd::LfsTransferAgent) => {
            let _span = tracing::info_span!("lfs_transfer_agent").entered();
            crab::cmd::lfs::transfer_agent::run_lfs_transfer_agent().await?;
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(not(any(feature = "fuse", feature = "nfs")))]
        Some(Cmd::Mount {
            sub,
            repo,
            mountpoint,
            backend,
            name: mount_name,
            foreground,
            git_ref,
            read_only,
            no_refresh,
            clean_overlay,
            allow_nested,
        }) => {
            let _ = (
                sub,
                repo,
                mountpoint,
                backend,
                mount_name,
                foreground,
                git_ref,
                read_only,
                no_refresh,
                clean_overlay,
                allow_nested,
                cancel,
            );
            Ok(crab::cmd::mount::run_fuse_mount_or_print())
        }
        #[cfg(any(feature = "fuse", feature = "nfs"))]
        Some(Cmd::Mount {
            sub,
            repo,
            mountpoint,
            backend,
            name: mount_name,
            foreground,
            git_ref,
            read_only,
            no_refresh,
            clean_overlay,
            allow_nested,
        }) => {
            // Handle `crab mount status` subcommand first.
            if let Some(MountCmd::Status {
                mountpoint: status_path,
                live_only,
                verbose,
                dirty,
                json: status_json,
            }) = sub
            {
                let _span = tracing::info_span!("mount_status").entered();
                if dirty {
                    #[cfg(any(feature = "fuse", feature = "nfs"))]
                    {
                        crab::cmd::mount::run_mount_diff(&status_path, status_json).await?;
                        return Ok(ExitCode::SUCCESS);
                    }
                    #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                    {
                        eprintln!("error: `crab mount status --dirty` requires mount support");
                        return Ok(ExitCode::from(1));
                    }
                }
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    crab::cmd::mount::run_mount_status_live_or_persisted(
                        &status_path,
                        verbose,
                        status_json,
                        live_only,
                    )
                    .await?;
                    return Ok(ExitCode::SUCCESS);
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    if live_only {
                        eprintln!("error: `crab mount status --live-only` requires mount support");
                        return Ok(ExitCode::from(1));
                    }
                    crab::cmd::mount::run_mount_status(&status_path, verbose, status_json)?;
                    return Ok(ExitCode::SUCCESS);
                }
            }

            // Handle `crab mount list` subcommand.
            if let Some(MountCmd::List { json }) = sub {
                let _span = tracing::info_span!("mount_list").entered();
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    crab::cmd::mount::run_mount_list_live_or_persisted(json).await?;
                    return Ok(ExitCode::SUCCESS);
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    crab::cmd::mount::run_mount_list(json)?;
                    return Ok(ExitCode::SUCCESS);
                }
            }

            // Handle `crab mount doctor` subcommand.
            if let Some(MountCmd::Doctor {
                backend,
                mountpoint,
                json,
            }) = sub
            {
                let _span = tracing::info_span!("mount_doctor", ?backend).entered();
                crab::cmd::mount::run_mount_doctor(backend, mountpoint.as_deref(), json)?;
                return Ok(ExitCode::SUCCESS);
            }

            // Handle `crab mount refresh` subcommand.
            if let Some(MountCmd::Refresh {
                mountpoint: ref refresh_mp,
            }) = sub
            {
                let _span = tracing::info_span!("mount_refresh").entered();
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    crab::cmd::mount::run_mount_control_refresh(refresh_mp).await?;
                    return Ok(ExitCode::SUCCESS);
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    let _ = refresh_mp;
                    crab::cmd::mount::print_fuse_not_compiled();
                    return Ok(ExitCode::from(1));
                }
            }

            // Handle `crab mount switch` subcommand.
            if let Some(MountCmd::Switch {
                mountpoint: ref switch_mp,
                git_ref: ref switch_ref,
            }) = sub
            {
                let _span = tracing::info_span!("mount_switch").entered();
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    crab::cmd::mount::run_mount_control_switch(switch_mp, switch_ref).await?;
                    return Ok(ExitCode::SUCCESS);
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    let _ = (switch_mp, switch_ref);
                    crab::cmd::mount::print_fuse_not_compiled();
                    return Ok(ExitCode::from(1));
                }
            }

            // Handle `crab mount clean` subcommand.
            if let Some(MountCmd::Clean { all }) = sub {
                let _span = tracing::info_span!("mount_clean").entered();
                crab::cmd::mount::run_mount_clean(all)?;
                return Ok(ExitCode::SUCCESS);
            }

            if let Some(MountCmd::Diff { mountpoint, json }) = sub {
                let _span = tracing::info_span!("mount_diff").entered();
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    crab::cmd::mount::run_mount_diff(&mountpoint, json).await?;
                    return Ok(ExitCode::SUCCESS);
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    let _ = (mountpoint, json);
                    crab::cmd::mount::print_fuse_not_compiled();
                    return Ok(ExitCode::from(1));
                }
            }

            if let Some(MountCmd::Export {
                mountpoint,
                to,
                json,
            }) = sub
            {
                let _span = tracing::info_span!("mount_export").entered();
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    crab::cmd::mount::run_mount_export(&mountpoint, &to, json).await?;
                    return Ok(ExitCode::SUCCESS);
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    let _ = (mountpoint, to, json);
                    crab::cmd::mount::print_fuse_not_compiled();
                    return Ok(ExitCode::from(1));
                }
            }

            if let Some(MountCmd::Reset {
                mountpoint,
                overlay,
                yes,
                json,
            }) = sub
            {
                let _span = tracing::info_span!("mount_reset").entered();
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    crab::cmd::mount::run_mount_reset(&mountpoint, overlay, yes, json).await?;
                    return Ok(ExitCode::SUCCESS);
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    let _ = (mountpoint, overlay, yes, json);
                    crab::cmd::mount::print_fuse_not_compiled();
                    return Ok(ExitCode::from(1));
                }
            }

            if let Some(MountCmd::Commit {
                mountpoint,
                message,
                push,
                json,
            }) = sub
            {
                let _span = tracing::info_span!("mount_commit").entered();
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    crab::cmd::mount::run_mount_commit(&mountpoint, &message, push, json).await?;
                    return Ok(ExitCode::SUCCESS);
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    let _ = (mountpoint, message, push, json);
                    crab::cmd::mount::print_fuse_not_compiled();
                    return Ok(ExitCode::from(1));
                }
            }

            #[cfg(not(feature = "fuse"))]
            if backend == crab::cmd::mount::MountBackend::Fuse {
                let _ = (clean_overlay, allow_nested);
                return Ok(crab::cmd::mount::run_fuse_mount_or_print());
            }

            let Some(mount_path) = mountpoint else {
                eprintln!("error: --mountpoint / -m is required");
                eprintln!("Usage: crab mount --repo <source> --mountpoint <path> [--ref=<branch>]");
                return Ok(ExitCode::from(1));
            };

            let Some(repo_source) = repo else {
                eprintln!("error: --repo / -r is required");
                eprintln!("Usage: crab mount --repo <source> --mountpoint <path> [--ref=<branch>]");
                return Ok(ExitCode::from(1));
            };

            let _span = tracing::info_span!("mount", path = %mount_path.display()).entered();

            // Clean overlay if requested.
            if clean_overlay {
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    // Compute the cache dir the same way the pipeline will.
                    use crab::vfs::clone_cache::compute_cache_hash;
                    let abs_path = std::fs::canonicalize(std::path::Path::new(&repo_source))
                        .unwrap_or_else(|_| std::path::PathBuf::from(&repo_source));
                    let cache_hash = compute_cache_hash(&abs_path.to_string_lossy());
                    let home = std::env::var("HOME").unwrap_or_default();
                    let cache_dir = std::path::PathBuf::from(&home)
                        .join(".crab/mounts/repos")
                        .join(&cache_hash);
                    let overlay_db = cache_dir.join("overlay.db");
                    let upper_dir = cache_dir.join("overlay/upper");
                    if let Err(e) = crab::vfs::overlay::OverlayStore::clean(&overlay_db, &upper_dir)
                    {
                        tracing::warn!(error = %e, "failed to clean overlay");
                    } else {
                        tracing::info!("overlay cleaned");
                    }
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    // Without fuse feature, we can't compute the cache hash.
                    tracing::warn!("--clean-overlay requires the fuse feature");
                }
            }

            #[cfg(any(feature = "fuse", feature = "nfs"))]
            {
                let opts = crab::cmd::mount::NewMountOpts {
                    repo: repo_source,
                    mountpoint: mount_path,
                    backend,
                    git_ref,
                    foreground,
                    read_only,
                    no_refresh,
                    allow_nested,
                    name: mount_name,
                    cancel,
                };
                crab::cmd::mount::run_mount_with_new_cli(opts).await?;
                Ok(ExitCode::SUCCESS)
            }
        }
        Some(Cmd::Unmount { mountpoint, all }) => {
            if all {
                let _span = tracing::info_span!("unmount_all").entered();
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    crab::cmd::mount::run_unmount_all_live_or_persisted().await?;
                }
                #[cfg(not(any(feature = "fuse", feature = "nfs")))]
                {
                    crab::cmd::mount::run_unmount_all()?;
                }
            } else if let Some(ref mp) = mountpoint {
                let _span = tracing::info_span!("unmount", path = %mp.display()).entered();
                #[cfg(any(feature = "fuse", feature = "nfs"))]
                {
                    if let Ok(true) = crab::cmd::mount::try_mount_control_unmount(mp).await {
                        return Ok(ExitCode::SUCCESS);
                    }
                }
                crab::cmd::mount::run_unmount(mp)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Daemon {
            sub,
            root,
            hydration_concurrency: _,
        }) => {
            let _span = tracing::info_span!("daemon").entered();

            #[cfg(any(feature = "fuse", feature = "nfs"))]
            {
                crab::cmd::mount::run_daemon(
                    sub.map(|s| match s {
                        DaemonCmd::AddRepo {
                            name,
                            remote,
                            branch,
                            mount_root,
                            backend,
                        } => crab::cmd::mount::DaemonAction::AddRepo {
                            name,
                            remote,
                            branch,
                            mount_root,
                            backend: match backend {
                                DaemonMountBackendArg::Fuse => {
                                    crab::vfs::daemon::DaemonMountBackend::Fuse
                                }
                                DaemonMountBackendArg::Nfs => {
                                    crab::vfs::daemon::DaemonMountBackend::Nfs
                                }
                            },
                        },
                        DaemonCmd::RemoveRepo { name } => {
                            crab::cmd::mount::DaemonAction::RemoveRepo { name }
                        }
                        DaemonCmd::List { json } => crab::cmd::mount::DaemonAction::List {
                            mode: OutputMode::from_flags(json, false),
                        },
                        DaemonCmd::Status { name, json } => {
                            crab::cmd::mount::DaemonAction::Status {
                                name,
                                mode: OutputMode::from_flags(json, false),
                            }
                        }
                        DaemonCmd::SetRefresh { name, interval } => {
                            crab::cmd::mount::DaemonAction::SetRefresh {
                                name,
                                interval_secs: interval,
                            }
                        }
                        DaemonCmd::Remount {
                            name,
                            clean_overlay,
                        } => crab::cmd::mount::DaemonAction::Remount {
                            name,
                            clean_overlay,
                        },
                        DaemonCmd::Fetch { name } => crab::cmd::mount::DaemonAction::Fetch { name },
                        DaemonCmd::Enable { name } => {
                            crab::cmd::mount::DaemonAction::Enable { name }
                        }
                        DaemonCmd::Disable { name } => {
                            crab::cmd::mount::DaemonAction::Disable { name }
                        }
                        DaemonCmd::Commit {
                            name,
                            message,
                            push,
                            json,
                        } => crab::cmd::mount::DaemonAction::Commit {
                            name,
                            message,
                            push,
                            mode: OutputMode::from_flags(json, false),
                        },
                    }),
                    root,
                    cancel,
                )
                .await?;
                Ok(ExitCode::SUCCESS)
            }
            #[cfg(not(any(feature = "fuse", feature = "nfs")))]
            {
                let _ = (sub, root, cancel);
                crab::cmd::mount::print_fuse_not_compiled();
                Ok(ExitCode::from(1))
            }
        }
        Some(Cmd::Coordinator(sub)) => {
            let _span = tracing::info_span!("coordinator").entered();
            crab::cmd::coordinator::run_coordinator(sub).await
        }
        // No subcommand — print help.
        None => {
            crab::cmd::help::print_root_help(&Cli::command());
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn run_optimize_command(
    command: OptimizeCmd,
    cancel: &CancellationToken,
) -> Result<ExitCode> {
    match command {
        OptimizeCmd::Plan(args) => {
            let _span = tracing::info_span!("optimize_plan").entered();
            let mode = OutputMode::from_flags(args.json, false);
            let config = Config::resolve_local()?;
            crab::cmd::optimize::run_plan(&args, &config, mode);
            Ok(ExitCode::SUCCESS)
        }
        OptimizeCmd::Apply(args) => {
            let _span = tracing::info_span!("optimize_apply").entered();
            run_optimize_apply(args, cancel).await
        }
        OptimizeCmd::Xorbs(args) => {
            let _span = tracing::info_span!("optimize_xorbs").entered();
            tracing::info!(
                profile = ?args.profile,
                dry_run = args.dry_run,
                apply = args.apply,
                "starting xorb optimization"
            );

            let config = Config::resolve_local()?;
            crab::cmd::restripe::run_restripe(&args, &config, cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        OptimizeCmd::Packs {
            dry_run,
            json,
            jsonl,
        } => run_repack_command(dry_run, json, jsonl, cancel).await,
        OptimizeCmd::Shards {
            repo,
            bucket,
            dry_run,
            max_shard_size,
        } => run_compact_command(repo, bucket, dry_run, max_shard_size, cancel).await,
        OptimizeCmd::Tiers { command } => run_tier_command(command, cancel).await,
        OptimizeCmd::Cache(command) => match command {
            OptimizeCacheCmd::Stats => run_cache_command(CacheCmd::Stats).await,
            OptimizeCacheCmd::Verify => run_cache_command(CacheCmd::Verify).await,
            OptimizeCacheCmd::Clean => run_cache_command(CacheCmd::Clean).await,
            OptimizeCacheCmd::Prune {
                dry_run,
                verbose,
                json,
                jsonl,
            } => run_prune_command(dry_run, verbose, json, jsonl).await,
            OptimizeCacheCmd::Warm {
                include,
                exclude,
                all,
                dry_run,
                no_sync_chunk_index,
                json,
                jsonl,
            } => {
                run_fetch_command(
                    include,
                    exclude,
                    all,
                    dry_run,
                    no_sync_chunk_index,
                    json,
                    jsonl,
                    cancel,
                )
                .await
            }
        },
        OptimizeCmd::Indexes(command) => match command {
            OptimizeIndexesCmd::Diagnose { db, json, deep } => {
                run_metadb_command(
                    crab::cmd::metadb::MetadbCommand::Diagnose { db, json, deep },
                    cancel,
                )
                .await
            }
            OptimizeIndexesCmd::Rebuild { db, json } => {
                run_metadb_command(
                    crab::cmd::metadb::MetadbCommand::Rebuild { db, json },
                    cancel,
                )
                .await
            }
            OptimizeIndexesCmd::Compact { db } => {
                run_metadb_command(crab::cmd::metadb::MetadbCommand::Compact { db }, cancel).await
            }
            OptimizeIndexesCmd::CacheStats => {
                run_metadb_command(
                    crab::cmd::metadb::MetadbCommand::Cache(crab::cmd::metadb::CacheCommand::Stats),
                    cancel,
                )
                .await
            }
            OptimizeIndexesCmd::CacheClear => {
                run_metadb_command(
                    crab::cmd::metadb::MetadbCommand::Cache(crab::cmd::metadb::CacheCommand::Clear),
                    cancel,
                )
                .await
            }
            OptimizeIndexesCmd::Warm {
                include,
                exclude,
                all,
                dry_run,
                no_sync_chunk_index,
                json,
                jsonl,
            } => {
                run_fetch_command(
                    include,
                    exclude,
                    all,
                    dry_run,
                    no_sync_chunk_index,
                    json,
                    jsonl,
                    cancel,
                )
                .await
            }
        },
        OptimizeCmd::Lfs(command) => {
            let _span = tracing::info_span!("optimize_lfs").entered();
            let lfs_command = match command {
                OptimizeLfsCmd::Dedup {
                    dry_run,
                    test,
                    crab_cache,
                } => crab::cmd::lfs::LfsCmd::Dedup {
                    dry_run,
                    test,
                    crab_cache,
                },
                OptimizeLfsCmd::Convert {
                    from,
                    to,
                    pattern,
                    dry_run,
                    rollback,
                } => crab::cmd::lfs::LfsCmd::Convert {
                    from,
                    to,
                    pattern,
                    dry_run,
                    rollback,
                },
                OptimizeLfsCmd::Prune {
                    verify_remote,
                    no_verify_remote,
                    verify_unreachable,
                    no_verify_unreachable,
                    when_unverified,
                    recent,
                    dry_run,
                    force,
                    verbose,
                } => crab::cmd::lfs::LfsCmd::Prune {
                    verify_remote,
                    no_verify_remote,
                    verify_unreachable,
                    no_verify_unreachable,
                    when_unverified,
                    recent,
                    dry_run,
                    force,
                    verbose,
                },
            };
            crab::cmd::lfs::run_lfs(&lfs_command)
        }
        OptimizeCmd::WorkflowCache(command) => {
            let _span = tracing::info_span!("optimize_workflow_cache").entered();
            match command {
                OptimizeWorkflowCacheCmd::Push(args) => {
                    crab::cmd::workflow::exec_push_cache(args).await?;
                }
                OptimizeWorkflowCacheCmd::JournalGc(args) => {
                    crab::cmd::workflow_journal::exec_gc(args)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        OptimizeCmd::Replicas(command) => {
            let _span = tracing::info_span!("optimize_replicas").entered();
            let replica_command = match command {
                OptimizeReplicasCmd::Status(args) => {
                    crab::cmd::replica::ReplicaCommand::Status(args)
                }
                OptimizeReplicasCmd::Doctor(args) => {
                    crab::cmd::replica::ReplicaCommand::Doctor(args)
                }
                OptimizeReplicasCmd::Verify(args) => {
                    crab::cmd::replica::ReplicaCommand::Verify(args)
                }
                OptimizeReplicasCmd::Backfill(command) => {
                    crab::cmd::replica::ReplicaCommand::Backfill(command)
                }
                OptimizeReplicasCmd::Wait(args) => crab::cmd::replica::ReplicaCommand::Wait(args),
                OptimizeReplicasCmd::Repair(args) => {
                    crab::cmd::replica::ReplicaCommand::Repair(args)
                }
                OptimizeReplicasCmd::Cost(args) => crab::cmd::replica::ReplicaCommand::Cost(args),
                OptimizeReplicasCmd::Runbook(args) => {
                    crab::cmd::replica::ReplicaCommand::Runbook(args)
                }
                OptimizeReplicasCmd::Diagnostics(args) => {
                    crab::cmd::replica::ReplicaCommand::Diagnostics(args)
                }
                OptimizeReplicasCmd::Certify(args) => {
                    crab::cmd::replica::ReplicaCommand::Certify(args)
                }
                OptimizeReplicasCmd::Evidence(command) => {
                    crab::cmd::replica::ReplicaCommand::Evidence(command)
                }
                OptimizeReplicasCmd::Failover(command) => {
                    crab::cmd::replica::ReplicaCommand::Failover(command)
                }
            };
            crab::cmd::replica::exec(replica_command, cancel).await?;
            Ok(ExitCode::SUCCESS)
        }
        OptimizeCmd::Repo(args) => {
            let _span = tracing::info_span!("optimize_repo").entered();
            let mode = OutputMode::from_flags(args.json, false);
            let config = Config::resolve_local()?;
            crab::cmd::doctor::run_cost_report(
                mode,
                args.pricing_file,
                args.inventory_source,
                args.sample,
                args.top_k,
                &config,
                cancel,
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn run_optimize_apply(
    args: crab::cmd::optimize::OptimizeApplyArgs,
    cancel: &CancellationToken,
) -> Result<ExitCode> {
    let mode = OutputMode::from_flags(args.json, false);
    let config = Config::resolve_local()?;
    let mut payload = crab::cmd::optimize::build_apply_payload(&args, &config);

    for index in 0..payload.steps.len() {
        if cancel.is_cancelled() {
            let step = &mut payload.steps[index];
            step.status = crab::cmd::optimize::OptimizeStepStatus::Failed;
            "cancelled".clone_into(&mut step.detail);
            crab::cmd::optimize::refresh_summary(&mut payload);
            crab::cmd::optimize::render_apply(&payload, mode);
            return Ok(ExitCode::FAILURE);
        }
        let result = {
            let step = &mut payload.steps[index];
            if !mode.is_machine() && step.status != crab::cmd::optimize::OptimizeStepStatus::Skipped
            {
                eprintln!("optimize: running {}", step.title);
            }

            match step.kind {
                crab::cmd::optimize::OptimizeStepKind::CostReport => {
                    let cmd = crab::cmd::optimize::cost_command_args(&args);
                    crab::cmd::optimize::run_child_step(step, mode, &cmd, cancel).await
                }
                crab::cmd::optimize::OptimizeStepKind::SafetyChecks => {
                    if step.status == crab::cmd::optimize::OptimizeStepStatus::Skipped {
                        Ok(())
                    } else {
                        crab::replication::ensure_active_active_maintenance_admitted(
                            &config,
                            "cost optimization",
                        )
                        .map(|()| {
                            crab::cmd::optimize::mark_succeeded(
                                step,
                                "active-active maintenance admission passed",
                            );
                        })
                    }
                }
                crab::cmd::optimize::OptimizeStepKind::LifecycleTiering => {
                    let cmd = crab::cmd::optimize::tier_apply_command_args();
                    crab::cmd::optimize::run_child_step(step, mode, &cmd, cancel).await
                }
                crab::cmd::optimize::OptimizeStepKind::XorbRestripe => {
                    let cmd = crab::cmd::optimize::xorb_apply_command_args(&args);
                    crab::cmd::optimize::run_child_step(step, mode, &cmd, cancel).await
                }
                crab::cmd::optimize::OptimizeStepKind::CachePrune => {
                    let cmd = crab::cmd::optimize::cache_prune_command_args();
                    crab::cmd::optimize::run_child_step(step, mode, &cmd, cancel).await
                }
                crab::cmd::optimize::OptimizeStepKind::ReplicaPolicy => {
                    let cmd = crab::cmd::optimize::replica_policy_command_args();
                    crab::cmd::optimize::run_child_step(step, mode, &cmd, cancel).await
                }
            }
        };
        if let Err(error) = result {
            let step = &mut payload.steps[index];
            if step.status != crab::cmd::optimize::OptimizeStepStatus::Failed {
                step.status = crab::cmd::optimize::OptimizeStepStatus::Failed;
                step.detail = error.to_string();
            }
            crab::cmd::optimize::refresh_summary(&mut payload);
            crab::cmd::optimize::render_apply(&payload, mode);
            return Ok(ExitCode::FAILURE);
        }
    }

    crab::cmd::optimize::refresh_summary(&mut payload);
    crab::cmd::optimize::render_apply(&payload, mode);
    Ok(ExitCode::SUCCESS)
}

async fn run_compact_command(
    repo: String,
    bucket: String,
    dry_run: bool,
    max_shard_size: String,
    cancel: &CancellationToken,
) -> Result<ExitCode> {
    let _span = tracing::info_span!("compact").entered();
    let max_size = crab::cmd::compact::parse_size_str(&max_shard_size)?;
    let config = Config::resolve_local().unwrap_or_default();
    if !dry_run {
        crab::replication::ensure_active_active_maintenance_admitted(&config, "compaction")?;
    }
    let store = create_cli_store(&bucket, &config, "compact", cancel).await?;
    let args = crab::cmd::compact::CompactArgs {
        repo,
        bucket,
        dry_run,
        max_shard_size: max_size,
    };
    crab::cmd::compact::run_compact(&args, &store).await?;
    Ok(ExitCode::SUCCESS)
}

async fn run_repack_command(
    dry_run: bool,
    json: bool,
    jsonl: bool,
    cancel: &CancellationToken,
) -> Result<ExitCode> {
    let _span = tracing::info_span!("repack").entered();
    let mode = OutputMode::from_flags(json, jsonl);
    tracing::info!(dry_run, "starting repack");

    let config = Config::resolve_local()?;
    if !dry_run {
        crab::replication::ensure_active_active_maintenance_admitted(&config, "repack")?;
    }

    let url = config
        .remote_url
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: "missing [remote] url in .crab/config.toml".into(),
            origin: ".crab/config.toml".into(),
        })?;
    let parsed = crab::git::url::CrabUrl::parse(url)?;
    let prefix = parsed.repo_path.clone();

    let store = create_cli_store(&parsed.bucket, &config, "repack", cancel).await?;

    let repack_config = crab::cmd::repack::RepackConfig {
        lock_ttl: std::time::Duration::from_secs(config.push_lock_ttl_secs),
        dry_run,
        download_concurrency: config.download_concurrency,
        max_cas_retries: config.push_max_cas_retries,
    };

    let outcome = crab::cmd::repack::run_repack(&store, &prefix, &repack_config, cancel).await?;
    let summary = outcome.to_summary();

    match mode {
        OutputMode::Json => {
            crab::core::output::emit_json("repack", "1.0", &summary);
        }
        OutputMode::Jsonl => {
            let mut stream = JsonlStream::new("repack.event", "1.0", std::io::stdout());
            stream.emit_result(&summary);
        }
        OutputMode::Text => {
            if dry_run {
                eprintln!(
                    "repack dry run: {} packs, {} bytes, {:.1}s",
                    outcome.packs_before,
                    outcome.bytes_before,
                    outcome.elapsed.as_secs_f64(),
                );
            } else {
                eprintln!(
                    "repack complete: {} → {} packs, {} → {} bytes, {:.1}s",
                    outcome.packs_before,
                    outcome.packs_after,
                    outcome.bytes_before,
                    outcome.bytes_after,
                    outcome.elapsed.as_secs_f64(),
                );
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

async fn run_tier_command(
    sub: crab::cmd::tier::TierCommand,
    cancel: &CancellationToken,
) -> Result<ExitCode> {
    let _span = tracing::info_span!("tier").entered();
    let mode = crab::cmd::tier::plan_output_mode(
        matches!(&sub, crab::cmd::tier::TierCommand::Plan { json: true, .. }),
        matches!(&sub, crab::cmd::tier::TierCommand::Plan { jsonl: true, .. }),
    );
    let config = Config::resolve_local().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to load config for tier, using defaults");
        Config::default()
    });
    let ctx = crab::core::context::AppContext::new(config, cancel.clone());
    crab::cmd::tier::run_tier(sub, &ctx, mode).await?;
    Ok(ExitCode::SUCCESS)
}

async fn run_metadb_command(
    sub: crab::cmd::metadb::MetadbCommand,
    cancel: &CancellationToken,
) -> Result<ExitCode> {
    let _span = tracing::info_span!("metadb").entered();
    crab::cmd::metadb::run_metadb(sub, OutputMode::Text, cancel).await?;
    Ok(ExitCode::SUCCESS)
}

async fn run_cache_command(sub: CacheCmd) -> Result<ExitCode> {
    let _span = tracing::info_span!("cache").entered();
    match sub {
        CacheCmd::Stats => run_cache_stats().await?,
        CacheCmd::Verify => {
            let mode = OutputMode::from_flags(false, false);
            crab::cmd::cache::run_cache_verify(mode).await?;
        }
        CacheCmd::Clean => {
            let mode = OutputMode::from_flags(false, false);
            crab::cmd::cache::run_cache_clean(false, mode)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_fetch_command(
    include: Vec<String>,
    exclude: Vec<String>,
    all: bool,
    dry_run: bool,
    no_sync_chunk_index: bool,
    json: bool,
    jsonl: bool,
    cancel: &CancellationToken,
) -> Result<ExitCode> {
    let _span = tracing::info_span!("fetch").entered();
    let mode = OutputMode::from_flags(json, jsonl);
    let args = crab::cmd::fetch::FetchArgs {
        include,
        exclude,
        all,
        dry_run,
        no_sync_chunk_index,
        mode,
    };
    crab::cmd::fetch::run_fetch(&args, cancel).await?;
    Ok(ExitCode::SUCCESS)
}

async fn run_prune_command(
    dry_run: bool,
    verbose: bool,
    json: bool,
    jsonl: bool,
) -> Result<ExitCode> {
    let _span = tracing::info_span!("prune").entered();
    let mode = OutputMode::from_flags(json, jsonl);
    let args = crab::cmd::prune::PruneArgs {
        dry_run,
        verbose,
        mode,
    };

    let jsonl_stream = if mode == OutputMode::Jsonl {
        Some(std::sync::Mutex::new(JsonlStream::new(
            "prune.event",
            "1.0",
            std::io::stdout(),
        )))
    } else {
        None
    };

    let summary = crab::cmd::prune::run_prune(&args, jsonl_stream.as_ref()).await?;

    match mode {
        OutputMode::Json => {
            crab::core::output::emit_json("prune", "1.0", &summary);
        }
        OutputMode::Jsonl => {
            if let Some(stream) = &jsonl_stream
                && let Ok(mut s) = stream.lock()
            {
                s.emit_result(&summary);
            }
        }
        OutputMode::Text => {}
    }

    Ok(ExitCode::SUCCESS)
}

/// Parse a human-readable duration string like `1h`, `30m`, `24h`, `90s`.
///
/// Supports `h` (hours), `m` (minutes), `s` (seconds) suffixes.
fn parse_duration_str(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(CrabError::Configuration {
            key: "empty duration string".into(),
            origin: "cli".into(),
        });
    }

    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('h') {
        (n, 3600u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else {
        // Default to seconds if no suffix.
        (s, 1u64)
    };

    let value: u64 = num_str.parse().map_err(|_| CrabError::Configuration {
        key: format!("invalid duration: {s}"),
        origin: "cli".into(),
    })?;

    Ok(std::time::Duration::from_secs(value * multiplier))
}

/// Run `crab auth refresh` — force an immediate token refresh.
///
/// Loads cached tokens, discovers the `IdP`, refreshes via the refresh
/// token grant, and stores the new tokens.
async fn run_auth_refresh(config: &Config) -> Result<()> {
    let provider = config.auth.provider;
    let provider_name = provider.as_str();

    let issuer_url = config
        .auth
        .issuer_url
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: "auth.issuer_url".into(),
            origin: "issuer_url is required for token refresh".into(),
        })?;
    let client_id = config
        .auth
        .client_id
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: "auth.client_id".into(),
            origin: "client_id is required for token refresh".into(),
        })?;

    let cache_dir = crab_auth::token_cache::expand_token_cache_path(&config.auth.token_cache_path);
    let cache = crab_auth::token_cache::TokenCache::new(cache_dir)?;
    let tokens = cache
        .load_any(provider.token_cache_keys())?
        .ok_or(CrabError::NoCredentials)?;

    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .ok_or_else(|| CrabError::AuthExpired {
            path: "no refresh token available — run `crab login`".into(),
        })?;

    let discovery = crab::auth::oidc::discover(issuer_url).await?;
    let new_tokens =
        crab::auth::oidc::refresh_tokens(&discovery.token_endpoint, client_id, refresh_token)
            .await?;

    cache.store(
        provider_name,
        &new_tokens.id_token,
        new_tokens.refresh_token.as_deref(),
    )?;

    eprintln!("Tokens refreshed ({provider_name})");
    if let Err(err) = record_auth_refresh_audit(provider_name, issuer_url) {
        tracing::warn!(%err, "failed to append auth refresh audit event");
    }
    if let Err(err) = record_auth_grant_audit(provider_name, issuer_url, "refresh_token") {
        tracing::warn!(%err, "failed to append auth grant audit event");
    }
    Ok(())
}

fn record_auth_refresh_audit(provider: &str, issuer_url: &str) -> Result<()> {
    let event = crab::audit::AuditEvent::new(crab::audit::NewAuditEvent {
        operation: "auth.refresh".to_owned(),
        outcome: crab::audit::AuditOutcome::Success,
        actor: None,
        repository: None,
        details: serde_json::json!({
            "provider": provider,
            "issuer_url": issuer_url,
        }),
    });
    crab::audit::append_event(&crab::audit::default_log_path(), &event)
}

fn record_auth_grant_audit(provider: &str, issuer_url: &str, grant_type: &str) -> Result<()> {
    let event = crab::audit::AuditEvent::new(crab::audit::NewAuditEvent {
        operation: "auth.grant".to_owned(),
        outcome: crab::audit::AuditOutcome::Success,
        actor: None,
        repository: None,
        details: serde_json::json!({
            "provider": provider,
            "issuer_url": issuer_url,
            "grant_type": grant_type,
        }),
    });
    crab::audit::append_event(&crab::audit::default_log_path(), &event)
}

/// Create a cloud-backed [`Store`] for CLI commands that need remote access.
///
/// Delegates to [`crab::auth::build_store`] so that the configured auth
/// provider and storage backend are respected. Commands that only have a
/// bucket name (GC, compact) pass a synthetic `CrabUrl` with a placeholder
/// repo path — `build_store` only uses the bucket for builder construction.
async fn create_cli_store(
    bucket: &str,
    config: &Config,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<crab::storage::store::Store> {
    let url = crab::git::url::CrabUrl {
        bucket: bucket.to_owned(),
        repo_path: "_cli".to_owned(),
    };
    crab::auth::build_store(config, &url, operation, cancel).await
}

/// Create a [`CachingStore`] wrapping a cloud-backed [`Store`].
///
/// When `config.cache.service_url` is set the returned store routes
/// immutable reads through the cache service. Otherwise it delegates
/// everything to the cloud store directly.
#[expect(
    dead_code,
    reason = "available for CLI commands migrating to CachingStore"
)]
async fn create_cli_caching_store(
    bucket: &str,
    config: &Config,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<crab_cache_store::CachingStore> {
    let store = create_cli_store(bucket, config, operation, cancel).await?;
    Ok(crab_cache_store::CachingStore::new(store, &config.cache)?)
}

#[derive(Default)]
struct FilterProcessRemoteSmudge {
    hydrator: Option<Arc<crab::cmd::hydrate::ShardHydrator>>,
    prefetch: Option<Arc<crab::git::prefetch::PrefetchQueue>>,
}

fn filter_process_should_wire_remote_smudge(config: &Config) -> bool {
    !config.checkout.lazy || config.hydrate.auto
}

async fn build_filter_process_remote_smudge(
    config: &Config,
    cancel: &CancellationToken,
    enabled: bool,
) -> FilterProcessRemoteSmudge {
    if !enabled {
        tracing::debug!(
            "filter-process: lazy checkout without auto-hydrate, skipping remote smudge setup"
        );
        return FilterProcessRemoteSmudge::default();
    }

    let remote_path = crab::git::discover::resolve_crab_dir().map_or_else(
        || std::path::PathBuf::from(".crab/remote"),
        |d| d.join("remote"),
    );
    let parsed = match resolve_hydrate_remote_url(&remote_path) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            tracing::debug!(
                "filter-process: no Crab remote configured, smudge will defer to hydrate"
            );
            return FilterProcessRemoteSmudge::default();
        }
        Err(e) => {
            tracing::debug!(error = %e, "filter-process: failed to read Crab remote, smudge will defer to hydrate");
            return FilterProcessRemoteSmudge::default();
        }
    };

    let selection = match crab::replication::select_read_store(config, &parsed, "smudge", cancel)
        .await
    {
        Ok(selection) => selection,
        Err(e) => {
            tracing::debug!(error = %e, "filter-process: failed to build read store, smudge will defer to hydrate");
            return FilterProcessRemoteSmudge::default();
        }
    };
    if let crab::replication::ReadSource::Replica { name } = &selection.source {
        tracing::debug!(replica = %name, "selected read replica for filter-process smudge");
    }

    let caching_store = match crab_cache_store::CachingStore::new(selection.store, &config.cache) {
        Ok(store) => store,
        Err(e) => {
            tracing::debug!(error = %e, "filter-process: failed to build CachingStore, smudge will defer to hydrate");
            return FilterProcessRemoteSmudge::default();
        }
    };
    let mut hydrator = match crab::cmd::hydrate::ShardHydrator::with_config_from_cli_layout(
        caching_store,
        selection.router,
        config,
    ) {
        Ok(hydrator) => hydrator,
        Err(e) => {
            tracing::debug!(error = %e, "filter-process: failed to build ShardHydrator, smudge will defer to hydrate");
            return FilterProcessRemoteSmudge::default();
        }
    };

    match crab::cache::xet_chunk_cache_from_config(config) {
        Ok(handle) => {
            hydrator = hydrator.with_xet_chunk_cache(handle.cache);
        }
        Err(e) => {
            tracing::debug!(error = %e, "filter-process: failed to open xet-core chunk cache, continuing without it");
        }
    }

    let prefetch = Arc::new(hydrator.prefetch_queue(
        config,
        cancel.clone(),
        tokio::runtime::Handle::current(),
    ));
    tracing::debug!("filter-process: ShardHydrator and delayed-smudge PrefetchQueue wired");
    FilterProcessRemoteSmudge {
        hydrator: Some(Arc::new(hydrator)),
        prefetch: Some(prefetch),
    }
}

/// Format a byte count with a human-readable unit suffix. Used by the
/// hydrate and `cache stats` chunk-cache summaries so the two outputs
/// agree on formatting.
fn format_bytes_size(bytes: u64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "cache sizes fit in f64 without meaningful precision loss"
    )]
    let b = bytes as f64;
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut idx = 0;
    let mut scaled = b;
    while scaled >= 1024.0 && idx < UNITS.len() - 1 {
        scaled /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes} B")
    } else {
        format!("{scaled:.1} {unit}", unit = UNITS[idx])
    }
}

fn resolve_hydrate_remote_url(path: &Path) -> Result<Option<crab::git::url::CrabUrl>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CrabError::Io(e)),
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(CrabError::Configuration {
            key: ".crab/remote".into(),
            origin: format!("{} is empty", path.display()),
        });
    }
    crab::git::url::CrabUrl::parse(trimmed).map(Some)
}

/// Implementation of `crab cache stats`. Reports both cache families:
/// xet-core's range cache for reconstruction reads, and Crab's object
/// cache for shards, xorbs, manifests, and stages.
async fn run_cache_stats() -> Result<()> {
    let config = Config::resolve_local().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to load config for cache stats, using defaults");
        Config::default()
    });

    let handle = match crab::cache::xet_chunk_cache_from_config(&config) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("chunk cache unavailable: {e}");
            return Ok(());
        }
    };

    let stats = handle.stats().await;
    println!("Chunk cache:");
    println!("  directory:   {}", handle.directory.display());
    println!(
        "  size_limit:  {} ({} bytes)",
        format_bytes_size(handle.size_bytes),
        handle.size_bytes,
    );
    println!("  entries:     {}", stats.entries);
    println!(
        "  used_bytes:  {} ({} bytes)",
        format_bytes_size(stats.total_bytes),
        stats.total_bytes,
    );

    let object_cache = crab::cache::LocalCache::new(crab::cache::default_cache_root());
    let object_stats = object_cache.stats().await?;
    let object_bytes =
        object_stats.shard_bytes + object_stats.xorb_bytes + object_stats.stage_bytes;
    println!();
    println!("Object cache:");
    println!("  directory:   {}", object_cache.root().display());
    println!(
        "  used_bytes:  {} ({} bytes)",
        format_bytes_size(object_bytes),
        object_bytes,
    );
    println!(
        "  shards:      {} entries, {}",
        object_stats.shard_count,
        format_bytes_size(object_stats.shard_bytes),
    );
    println!(
        "  xorbs:       {} entries, {}",
        object_stats.xorb_count,
        format_bytes_size(object_stats.xorb_bytes),
    );
    println!(
        "  stages:      {} entries, {}",
        object_stats.stage_count,
        format_bytes_size(object_stats.stage_bytes),
    );
    println!("  manifests:   {} entries", object_stats.manifest_count);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::{CommandFactory as _, Parser as _};

    use crab::cmd::help::{COMMAND_SECTIONS, render_root_help};
    use crab::core::config::Config;
    use crab::core::error::CrabError;
    use crab::core::output::OutputMode;

    use super::{
        Cli, Cmd, OptimizeCacheCmd, OptimizeCmd, OptimizeIndexesCmd, OptimizeLfsCmd,
        OptimizeReplicasCmd, OptimizeWorkflowCacheCmd, REMOTE_HELPER_STEM,
        coordinator_start_owns_logging, filter_process_should_wire_remote_smudge,
        resolve_hydrate_remote_url, root_help_requested, symlink_subcommand_for_stem,
    };

    fn with_cli_command(test: impl FnOnce(clap::Command) + Send + 'static) {
        std::thread::Builder::new()
            .name("cli-command-test".to_owned())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || test(Cli::command()))
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn root_help_groups_every_user_facing_command_once() {
        with_cli_command(|command| {
            let internal = [
                "coordinator",
                "filter-process",
                "lfs-transfer-agent",
                "diff-driver",
                "help",
            ];
            let mut expected: Vec<&str> = command
                .get_subcommands()
                .map(clap::Command::get_name)
                .filter(|name| !internal.contains(name))
                .collect();
            expected.sort_unstable();

            let mut categorized: Vec<&str> = COMMAND_SECTIONS
                .iter()
                .flat_map(|(_, names)| names.iter().copied())
                .collect();
            let categorized_count = categorized.len();
            categorized.sort_unstable();
            categorized.dedup();

            assert_eq!(categorized.len(), categorized_count);
            assert_eq!(categorized, expected);
        });
    }

    #[test]
    fn root_help_renders_top_level_commands_in_sections() {
        with_cli_command(|command| {
            let help = render_root_help(&command);

            assert!(help.contains("Get started"));
            assert!(help.contains("Large files and working tree"));
            assert!(help.contains("Workflows, data, and experiments"));
            assert!(help.contains("  hydrate"));
            assert!(help.contains("  exp"));
            assert!(!help.contains("crab files"));
        });
    }

    #[test]
    fn root_help_flags_are_intercepted_without_hiding_command_help() {
        assert!(root_help_requested(&["crab".into(), "--help".into()]));
        assert!(root_help_requested(&[
            "crab".into(),
            "--log-level".into(),
            "debug".into(),
            "help".into(),
        ]));
        assert!(!root_help_requested(&[
            "crab".into(),
            "hydrate".into(),
            "--help".into(),
        ]));
    }

    #[test]
    fn bare_name_detected_as_remote_helper() {
        let argv0 = "git-remote-crab";
        assert!(
            Path::new(argv0)
                .file_stem()
                .is_some_and(|s| s == REMOTE_HELPER_STEM)
        );
    }

    #[test]
    fn absolute_path_detected_as_remote_helper() {
        let argv0 = "/usr/local/bin/git-remote-crab";
        assert!(
            Path::new(argv0)
                .file_stem()
                .is_some_and(|s| s == REMOTE_HELPER_STEM)
        );
    }

    #[test]
    fn relative_path_detected_as_remote_helper() {
        let argv0 = "./bin/git-remote-crab";
        assert!(
            Path::new(argv0)
                .file_stem()
                .is_some_and(|s| s == REMOTE_HELPER_STEM)
        );
    }

    #[test]
    fn normal_binary_not_detected_as_remote_helper() {
        let argv0 = "crab";
        assert!(
            !Path::new(argv0)
                .file_stem()
                .is_some_and(|s| s == REMOTE_HELPER_STEM)
        );
    }

    #[test]
    fn normal_binary_with_path_not_detected() {
        let argv0 = "/usr/local/bin/crab";
        assert!(
            !Path::new(argv0)
                .file_stem()
                .is_some_and(|s| s == REMOTE_HELPER_STEM)
        );
    }

    /// Extract the subcommand from a `crab-{cmd}` file stem, mirroring
    /// the logic in `main()`.
    fn extract_symlink_subcmd(argv0: &str) -> Option<String> {
        let stem = Path::new(argv0).file_stem()?.to_str()?;
        symlink_subcommand_for_stem(stem)
    }

    #[test]
    fn symlink_gc_dispatches() {
        assert_eq!(extract_symlink_subcmd("crab-gc").as_deref(), Some("gc"));
    }

    #[test]
    fn symlink_fsck_dispatches() {
        assert_eq!(extract_symlink_subcmd("crab-fsck").as_deref(), Some("fsck"));
    }

    #[test]
    fn symlink_init_dispatches() {
        assert_eq!(extract_symlink_subcmd("crab-init").as_deref(), Some("init"));
    }

    #[test]
    fn symlink_with_absolute_path() {
        assert_eq!(
            extract_symlink_subcmd("/usr/local/bin/crab-gc").as_deref(),
            Some("gc")
        );
    }

    #[test]
    fn symlink_with_relative_path() {
        assert_eq!(
            extract_symlink_subcmd("./bin/crab-fsck").as_deref(),
            Some("fsck")
        );
    }

    #[test]
    fn symlink_filter_process_dispatches() {
        assert_eq!(
            extract_symlink_subcmd("crab-filter-process").as_deref(),
            Some("filter-process")
        );
    }

    #[test]
    fn plain_crab_no_symlink_subcmd() {
        assert_eq!(extract_symlink_subcmd("crab"), None);
    }

    #[test]
    fn remote_helper_not_treated_as_symlink() {
        assert_eq!(extract_symlink_subcmd("git-remote-crab"), None);
    }

    #[test]
    fn fuse_mount_not_treated_as_symlink() {
        assert_eq!(extract_symlink_subcmd("crab-fuse-mount"), None);
    }

    #[test]
    fn nfs_mount_not_treated_as_symlink() {
        assert_eq!(extract_symlink_subcmd("crab-nfs-mount"), None);
    }

    #[test]
    fn coordinator_start_owns_logging_for_background_and_foreground() {
        assert!(coordinator_start_owns_logging([
            "crab",
            "coordinator",
            "start"
        ]));
        assert!(coordinator_start_owns_logging([
            "crab",
            "--log-level",
            "debug",
            "coordinator",
            "start",
            "--foreground"
        ]));
    }

    #[test]
    fn coordinator_stop_and_other_commands_use_root_logging() {
        assert!(!coordinator_start_owns_logging([
            "crab",
            "coordinator",
            "stop"
        ]));
        assert!(!coordinator_start_owns_logging(["crab", "mount", "list"]));
    }

    #[test]
    fn hydrate_remote_missing_allows_local_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let remote_path = dir.path().join(".crab").join("remote");

        let resolved = resolve_hydrate_remote_url(&remote_path).unwrap();

        assert!(resolved.is_none());
    }

    #[test]
    fn hydrate_remote_parses_crab_url() {
        let dir = tempfile::tempdir().unwrap();
        let remote_path = dir.path().join("remote");
        std::fs::write(&remote_path, "crab://primary-bucket/org/repo\n").unwrap();

        let resolved = resolve_hydrate_remote_url(&remote_path).unwrap().unwrap();

        assert_eq!(resolved.bucket, "primary-bucket");
        assert_eq!(resolved.repo_path, "org/repo");
    }

    #[test]
    fn hydrate_remote_rejects_empty_configured_remote() {
        let dir = tempfile::tempdir().unwrap();
        let remote_path = dir.path().join("remote");
        std::fs::write(&remote_path, "\n").unwrap();

        let err = resolve_hydrate_remote_url(&remote_path).unwrap_err();

        assert!(matches!(
            err,
            CrabError::Configuration { ref key, .. } if key == ".crab/remote"
        ));
    }

    #[test]
    fn hydrate_remote_rejects_malformed_configured_remote() {
        let dir = tempfile::tempdir().unwrap();
        let remote_path = dir.path().join("remote");
        std::fs::write(&remote_path, "not-a-crab-url\n").unwrap();

        let err = resolve_hydrate_remote_url(&remote_path).unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn filter_process_skips_remote_smudge_for_plain_lazy_checkout() {
        let mut config = Config::default();
        config.checkout.lazy = true;
        config.hydrate.auto = false;

        assert!(!filter_process_should_wire_remote_smudge(&config));
    }

    #[test]
    fn filter_process_wires_remote_smudge_for_eager_or_auto_checkout() {
        let mut eager = Config::default();
        eager.checkout.lazy = false;
        eager.hydrate.auto = false;

        let mut auto = Config::default();
        auto.checkout.lazy = true;
        auto.hydrate.auto = true;

        assert!(filter_process_should_wire_remote_smudge(&eager));
        assert!(filter_process_should_wire_remote_smudge(&auto));
    }

    fn parse_cli_on_large_stack<T>(f: impl FnOnce() -> T + Send + 'static) -> T
    where
        T: Send + 'static,
    {
        // The top-level Clap tree is large enough to exceed the default
        // test-thread stack on some platforms.
        std::thread::Builder::new()
            .name("cli-parse".to_owned())
            .stack_size(16 * 1024 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn managed_administration_cli_parses_all_command_trees() {
        parse_cli_on_large_stack(|| {
            let organization = Cli::try_parse_from([
                "crab",
                "organization",
                "--service",
                "cloud.example.com",
                "create",
                "acme",
                "--json",
            ])
            .unwrap();
            assert!(matches!(
                organization.cmd,
                Some(Cmd::Organization(
                    crab::cmd::managed_admin::OrganizationArgs {
                        service: Some(ref service),
                        json: true,
                        command:
                            crab::cmd::managed_admin::OrganizationCommand::Create {
                                ref organization
                            },
                    }
                )) if service == "cloud.example.com" && organization == "acme"
            ));

            let repository = Cli::try_parse_from([
                "crab",
                "repo",
                "rename",
                "acme/models",
                "models-v2",
                "--revision",
                "4",
            ])
            .unwrap();
            assert!(matches!(
                repository.cmd,
                Some(Cmd::Repo(crab::cmd::managed_admin::RepositoryArgs {
                    command: crab::cmd::managed_admin::RepositoryCommand::Rename {
                        revision: 4,
                        ..
                    },
                    ..
                }))
            ));

            let member = Cli::try_parse_from([
                "crab",
                "member",
                "add",
                "acme",
                "018f3f80-7b2d-7c3a-8b1f-a0b1c2d3e4f5",
                "--role",
                "writer",
            ])
            .unwrap();
            assert!(matches!(member.cmd, Some(Cmd::Member(_))));

            let service_account = Cli::try_parse_from([
                "crab",
                "service-account",
                "create-token",
                "acme",
                "ci",
                "--role",
                "writer",
                "--json",
            ])
            .unwrap();
            assert!(matches!(
                service_account.cmd,
                Some(Cmd::ServiceAccount(
                    crab::cmd::managed_admin::ServiceAccountArgs { json: true, .. }
                ))
            ));
        });
    }

    #[test]
    fn download_cli_parses_hf_shaped_options() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from([
                "crab",
                "download",
                "crab://bucket/org/repo",
                "config.json",
                "models/",
                "--revision",
                "main",
                "--include",
                "*.gguf",
                "--exclude",
                "*Q8_0*",
                "--cache-dir",
                "cache",
                "--local-dir",
                "out",
                "--force-download",
                "--dry-run",
                "--max-workers",
                "16",
                "--quiet",
                "--jsonl",
            ])
            .unwrap();

            match cli.cmd {
                Some(Cmd::Download {
                    repo,
                    paths,
                    revision,
                    include,
                    exclude,
                    cache_dir,
                    local_dir,
                    force_download,
                    dry_run,
                    max_workers,
                    quiet,
                    json,
                    jsonl,
                }) => {
                    assert_eq!(repo, "crab://bucket/org/repo");
                    assert_eq!(paths, vec!["config.json", "models/"]);
                    assert_eq!(revision.as_deref(), Some("main"));
                    assert_eq!(include, vec!["*.gguf"]);
                    assert_eq!(exclude, vec!["*Q8_0*"]);
                    assert_eq!(cache_dir.as_deref(), Some(Path::new("cache")));
                    assert_eq!(local_dir.as_deref(), Some(Path::new("out")));
                    assert!(force_download);
                    assert!(dry_run);
                    assert_eq!(max_workers, Some(16));
                    assert!(quiet);
                    assert!(!json);
                    assert!(jsonl);
                }
                _ => unreachable!("download command should parse"),
            }
        });
    }

    #[test]
    fn get_alias_parses_as_download() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from(["crab", "get", "repo", "file.txt"]).unwrap();

            match cli.cmd {
                Some(Cmd::Download { repo, paths, .. }) => {
                    assert_eq!(repo, "repo");
                    assert_eq!(paths, vec!["file.txt"]);
                }
                _ => unreachable!("get should parse as download"),
            }
        });
    }

    #[test]
    fn workflow_data_and_artifact_commands_keep_dispatch_contracts() {
        parse_cli_on_large_stack(|| {
            let artifacts = Cli::try_parse_from([
                "crab",
                "artifacts",
                "get",
                "model",
                "--version",
                &format!("b3:{}", "aa".repeat(32)),
                "--json",
            ])
            .unwrap();
            let artifact_command = artifacts.cmd.as_ref().unwrap();
            assert_eq!(artifact_command.schema_name(), "artifacts");
            assert_eq!(artifact_command.output_mode(), OutputMode::Json);

            let data = Cli::try_parse_from(["crab", "data", "list", "--jsonl"]).unwrap();
            let data_command = data.cmd.as_ref().unwrap();
            assert_eq!(data_command.schema_name(), "data");
            assert_eq!(data_command.output_mode(), OutputMode::Jsonl);

            let checkpoint =
                Cli::try_parse_from(["crab", "workflow", "checkpoint", "--json"]).unwrap();
            let checkpoint_command = checkpoint.cmd.as_ref().unwrap();
            assert_eq!(checkpoint_command.schema_name(), "workflow.checkpoint");
            assert_eq!(checkpoint_command.output_mode(), OutputMode::Json);
        });
    }

    #[test]
    fn recover_history_parses_list_prune_verify_and_explicit_restore() {
        parse_cli_on_large_stack(|| {
            let list =
                Cli::try_parse_from(["crab", "recover", "history", "list", "--json"]).unwrap();
            assert!(matches!(
                list.cmd,
                Some(Cmd::Recover(crab::cmd::recover::RecoverCmd::History {
                    command: crab::cmd::history_recovery::HistoryCmd::List(_)
                }))
            ));

            let prune = Cli::try_parse_from([
                "crab",
                "recover",
                "history",
                "prune",
                "--keep-last",
                "10",
                "--apply",
                "--json",
            ])
            .unwrap();
            assert!(matches!(
                prune.cmd,
                Some(Cmd::Recover(crab::cmd::recover::RecoverCmd::History {
                    command: crab::cmd::history_recovery::HistoryCmd::Prune(args)
                })) if args.keep_last == 10 && args.apply && args.json
            ));
            assert!(
                Cli::try_parse_from(["crab", "recover", "history", "prune", "--keep-last", "0",])
                    .is_err()
            );

            let verify = Cli::try_parse_from([
                "crab",
                "recover",
                "history",
                "verify",
                "42",
                "--digest",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ])
            .unwrap();
            assert!(matches!(
                verify.cmd,
                Some(Cmd::Recover(crab::cmd::recover::RecoverCmd::History {
                    command: crab::cmd::history_recovery::HistoryCmd::Verify(_)
                }))
            ));

            let restore =
                Cli::try_parse_from(["crab", "recover", "history", "restore", "42", "--apply"])
                    .unwrap();
            assert!(matches!(
                restore.cmd,
                Some(Cmd::Recover(
                    crab::cmd::recover::RecoverCmd::History {
                        command: crab::cmd::history_recovery::HistoryCmd::Restore(args)
                    }
                )) if args.apply
            ));
        });
    }

    #[test]
    fn optimize_xorbs_parses_profile_args() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from([
                "crab",
                "optimize",
                "xorbs",
                "--profile",
                "ml",
                "--dry-run",
                "--json",
            ])
            .unwrap();

            match cli.cmd {
                Some(Cmd::Optimize(OptimizeCmd::Xorbs(args))) => {
                    assert_eq!(args.profile.as_deref(), Some("ml"));
                    assert!(args.dry_run);
                    assert!(args.json);
                }
                _ => unreachable!("optimize xorbs should parse"),
            }
        });
    }

    #[test]
    fn optimize_plan_and_apply_parse_operator_workflow_args() {
        parse_cli_on_large_stack(|| {
            let plan = Cli::try_parse_from([
                "crab",
                "optimize",
                "plan",
                "--inventory-source",
                "live",
                "--sample",
                "0.25",
                "--include-xorbs",
                "--profile",
                "dataset",
                "--json",
            ])
            .unwrap();
            match plan.cmd {
                Some(Cmd::Optimize(OptimizeCmd::Plan(args))) => {
                    assert_eq!(args.inventory_source.as_deref(), Some("live"));
                    assert_eq!(args.sample, Some(0.25));
                    assert!(args.include_xorbs);
                    assert_eq!(args.profile.as_deref(), Some("dataset"));
                    assert!(args.json);
                }
                _ => unreachable!("optimize plan should parse"),
            }

            let apply = Cli::try_parse_from([
                "crab",
                "optimize",
                "apply",
                "--skip-tiers",
                "--skip-cache",
                "--skip-replicas",
                "--json",
            ])
            .unwrap();
            match apply.cmd {
                Some(Cmd::Optimize(OptimizeCmd::Apply(args))) => {
                    assert!(args.skip_tiers);
                    assert!(args.skip_cache);
                    assert!(args.skip_replicas);
                    assert!(args.json);
                }
                _ => unreachable!("optimize apply should parse"),
            }
        });
    }

    #[test]
    fn top_level_restripe_is_removed() {
        parse_cli_on_large_stack(|| {
            assert!(Cli::try_parse_from(["crab", "restripe", "--dry-run"]).is_err());
        });
    }

    #[test]
    fn metadb_generation_owner_parses_runtime_controls() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from([
                "crab",
                "metadb",
                "owner",
                "--once",
                "--interval",
                "7",
                "--jsonl",
            ])
            .unwrap();
            assert!(matches!(
                cli.cmd,
                Some(Cmd::Metadb(crab::cmd::metadb::MetadbCommand::Owner {
                    once: true,
                    interval: 7,
                    jsonl: true,
                }))
            ));
        });
    }

    #[test]
    fn optimize_groups_parse_existing_maintenance_surfaces() {
        parse_cli_on_large_stack(|| {
            let packs =
                Cli::try_parse_from(["crab", "optimize", "packs", "--dry-run", "--json"]).unwrap();
            assert!(matches!(
                packs.cmd,
                Some(Cmd::Optimize(OptimizeCmd::Packs {
                    dry_run: true,
                    json: true,
                    ..
                }))
            ));

            let shards = Cli::try_parse_from([
                "crab",
                "optimize",
                "shards",
                "--repo",
                "org/models",
                "--bucket",
                "bucket",
                "--dry-run",
            ])
            .unwrap();
            assert!(matches!(
                shards.cmd,
                Some(Cmd::Optimize(OptimizeCmd::Shards {
                    repo,
                    bucket,
                    dry_run: true,
                    ..
                })) if repo == "org/models" && bucket == "bucket"
            ));

            let tiers =
                Cli::try_parse_from(["crab", "optimize", "tiers", "plan", "--dry-run"]).unwrap();
            assert!(matches!(
                tiers.cmd,
                Some(Cmd::Optimize(OptimizeCmd::Tiers { .. }))
            ));

            let cache =
                Cli::try_parse_from(["crab", "optimize", "cache", "prune", "--dry-run"]).unwrap();
            assert!(matches!(
                cache.cmd,
                Some(Cmd::Optimize(OptimizeCmd::Cache(OptimizeCacheCmd::Prune {
                    dry_run: true,
                    ..
                })))
            ));

            let indexes =
                Cli::try_parse_from(["crab", "optimize", "indexes", "diagnose", "--deep"]).unwrap();
            assert!(matches!(
                indexes.cmd,
                Some(Cmd::Optimize(OptimizeCmd::Indexes(
                    OptimizeIndexesCmd::Diagnose { deep: true, .. }
                )))
            ));

            let lfs =
                Cli::try_parse_from(["crab", "optimize", "lfs", "dedup", "--dry-run"]).unwrap();
            assert!(matches!(
                lfs.cmd,
                Some(Cmd::Optimize(OptimizeCmd::Lfs(OptimizeLfsCmd::Dedup {
                    dry_run: true,
                    ..
                })))
            ));

            let workflow_cache = Cli::try_parse_from([
                "crab",
                "optimize",
                "workflow-cache",
                "journal-gc",
                "--dry-run",
            ])
            .unwrap();
            assert!(matches!(
                workflow_cache.cmd,
                Some(Cmd::Optimize(OptimizeCmd::WorkflowCache(
                    OptimizeWorkflowCacheCmd::JournalGc(_)
                )))
            ));

            let replicas =
                Cli::try_parse_from(["crab", "optimize", "replicas", "status", "--json"]).unwrap();
            assert!(matches!(
                replicas.cmd,
                Some(Cmd::Optimize(OptimizeCmd::Replicas(
                    OptimizeReplicasCmd::Status(_)
                )))
            ));

            let repo = Cli::try_parse_from(["crab", "optimize", "repo", "--json"]).unwrap();
            assert!(matches!(
                repo.cmd,
                Some(Cmd::Optimize(OptimizeCmd::Repo(args))) if args.json
            ));
        });
    }

    #[test]
    fn mount_commit_uses_m_for_message() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from([
                "crab",
                "mount",
                "commit",
                "-m",
                "publish overlay",
                "--mountpoint",
                "view",
                "--push",
                "--json",
            ])
            .unwrap();

            match cli.cmd {
                Some(Cmd::Mount {
                    sub:
                        Some(super::MountCmd::Commit {
                            mountpoint,
                            message,
                            push,
                            json,
                        }),
                    ..
                }) => {
                    assert_eq!(mountpoint, Path::new("view"));
                    assert_eq!(message, "publish overlay");
                    assert!(push);
                    assert!(json);
                }
                _ => unreachable!("mount commit should parse"),
            }
        });
    }

    #[test]
    fn daemon_commit_parses_message_push_and_json() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from([
                "crab",
                "daemon",
                "commit",
                "--name",
                "repo-a",
                "-m",
                "publish overlay",
                "--push",
                "--json",
            ])
            .unwrap();

            match cli.cmd {
                Some(Cmd::Daemon {
                    sub:
                        Some(super::DaemonCmd::Commit {
                            name,
                            message,
                            push,
                            json,
                        }),
                    ..
                }) => {
                    assert_eq!(name, "repo-a");
                    assert_eq!(message, "publish overlay");
                    assert!(push);
                    assert!(json);
                }
                _ => unreachable!("daemon commit should parse"),
            }
        });
    }

    #[test]
    fn daemon_add_repo_parses_nfs_backend() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from([
                "crab",
                "daemon",
                "add-repo",
                "--name",
                "repo-a",
                "--remote",
                "https://github.com/example/repo-a.git",
                "--mount-root",
                "/mnt/repos",
                "--backend",
                "nfs",
            ])
            .unwrap();

            match cli.cmd {
                Some(Cmd::Daemon {
                    sub: Some(super::DaemonCmd::AddRepo { backend, .. }),
                    ..
                }) => assert_eq!(backend, super::DaemonMountBackendArg::Nfs),
                _ => unreachable!("daemon add-repo should parse"),
            }
        });
    }

    #[test]
    fn ship_rebase_retry_default_supports_large_swarms() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from([
                "crab",
                "ship",
                ".",
                "-m",
                "agent update",
                "--rebase-on-non-fast-forward",
            ])
            .unwrap();

            match cli.cmd {
                Some(Cmd::Ship {
                    rebase_retry_limit,
                    remote,
                    ..
                }) => {
                    assert_eq!(rebase_retry_limit, 256);
                    assert_eq!(remote, None);
                }
                _ => unreachable!("ship command should parse"),
            }
        });
    }

    #[test]
    fn parse_duration_hours() {
        let d = super::parse_duration_str("1h").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(3600));
    }

    #[test]
    fn parse_duration_minutes() {
        let d = super::parse_duration_str("30m").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(1800));
    }

    #[test]
    fn parse_duration_seconds() {
        let d = super::parse_duration_str("90s").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(90));
    }

    #[test]
    fn parse_duration_bare_number_defaults_to_seconds() {
        let d = super::parse_duration_str("120").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(120));
    }

    #[test]
    fn parse_duration_24h() {
        let d = super::parse_duration_str("24h").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(86400));
    }

    #[test]
    fn parse_duration_empty_errors() {
        assert!(super::parse_duration_str("").is_err());
    }

    #[test]
    fn parse_duration_invalid_errors() {
        assert!(super::parse_duration_str("abc").is_err());
    }

    #[test]
    fn gc_grace_period_uses_config_when_omitted() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from(["crab", "gc"]).unwrap();
            match cli.cmd {
                Some(Cmd::Gc { grace_period, .. }) => assert_eq!(grace_period, None),
                _ => unreachable!("gc command should parse"),
            }
        });
    }

    #[test]
    fn gc_grace_period_preserves_explicit_override() {
        parse_cli_on_large_stack(|| {
            let cli = Cli::try_parse_from(["crab", "gc", "--grace-period", "2h"]).unwrap();
            match cli.cmd {
                Some(Cmd::Gc { grace_period, .. }) => {
                    assert_eq!(grace_period.as_deref(), Some("2h"));
                }
                _ => unreachable!("gc command should parse"),
            }
        });
    }
}
