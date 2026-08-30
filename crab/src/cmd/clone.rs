//! `crab clone` — clone a repository in one step.
//!
//! For `crab://` remotes, wraps `git clone` with automatic filter
//! driver setup, lazy checkout configuration, and optional post-clone
//! hydration. Replaces the manual sequence of `git clone` → `crab init`
//! → `crab hydrate`. For ordinary Git remotes, delegates to `git clone`
//! without writing Crab configuration into the cloned repository.
//!
//! The `--lazy` flag (default) leaves files as pointer blobs so the clone
//! is fast even for multi-GB repos. Users can then selectively hydrate
//! with `crab hydrate *.safetensors`.

use std::future::Future;
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::event_payloads::{
    FileDonePayload, PERF_PHASE_SCHEMA, PerfPhasePayload, ProgressPayload,
};
use crate::core::output::{JsonlStream, OutputMode};
use crate::core::perf_phase::PhaseTimer;

/// Arguments for the `crab clone` command.
#[derive(Clone)]
pub struct CloneArgs {
    /// Remote URL or path (e.g. `crab://bucket/repo` or a Git URL).
    pub url: String,
    /// Target directory (defaults to repo name extracted from URL).
    pub directory: Option<PathBuf>,
    /// Branch to check out after cloning.
    pub branch: Option<String>,
    /// Shallow clone depth (number of commits).
    pub depth: Option<u32>,
    /// Leave files as pointers — skip automatic hydration (default: true).
    pub lazy: bool,
    /// Glob patterns to hydrate immediately after clone (implies not fully lazy).
    pub include: Vec<String>,
    /// Glob patterns to exclude from post-clone hydration.
    pub exclude: Vec<String>,
    /// Warm the local chunk-index cache after clone.
    ///
    /// Disabled by default because it is a push-read optimization, not
    /// required for clone correctness.
    pub sync_chunk_index: bool,
    /// Output mode resolved from `--json` / `--jsonl` flags.
    pub mode: OutputMode,
}

/// Terminal result payload for `--json` / `--jsonl` structured output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CloneSummary {
    /// Remote URL that was cloned.
    pub url: String,
    /// Directory the repository was cloned into.
    pub directory: String,
    /// Branch checked out (if specified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Whether the clone used lazy (pointer-only) checkout.
    pub lazy: bool,
    /// Wall-clock duration of the entire clone in milliseconds.
    pub duration_ms: u64,
}

/// Run `crab clone` from the current working directory.
pub async fn run_clone(args: &CloneArgs, cancel: &CancellationToken) -> Result<CloneSummary> {
    let cwd = std::env::current_dir()?;
    run_clone_in(&cwd, args, cancel).await
}

fn emit_phase(stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>, payload: PerfPhasePayload) {
    if let Some(stream) = stream
        && let Ok(mut s) = stream.lock()
    {
        s.emit_schema_event(PERF_PHASE_SCHEMA, "event", payload);
    }
}

/// Clone a repository, creating the target directory under `parent`.
pub async fn run_clone_in(
    parent: &Path,
    args: &CloneArgs,
    cancel: &CancellationToken,
) -> Result<CloneSummary> {
    let start = Instant::now();

    check_cancelled(cancel)?;

    let resolved_args = CloneArgs {
        url: resolve_clone_url(&args.url)?,
        directory: args.directory.clone(),
        branch: args.branch.clone(),
        depth: args.depth,
        lazy: args.lazy,
        include: args.include.clone(),
        exclude: args.exclude.clone(),
        sync_chunk_index: args.sync_chunk_index,
        mode: args.mode,
    };
    let args = &resolved_args;

    let crab_remote = is_crab_url(&args.url);

    // Resolve the target directory name.
    let target_dir = match &args.directory {
        Some(d) => parent.join(d),
        None => parent.join(repo_name_from_url(&args.url)?),
    };

    let _span = tracing::info_span!(
        "clone",
        url = %args.url,
        target = %target_dir.display(),
        lazy = args.lazy,
    )
    .entered();

    if !crab_remote {
        run_git_clone(parent, args, &target_dir)?;
        check_cancelled(cancel)?;

        let clone_summary = CloneSummary {
            url: args.url.clone(),
            directory: target_dir.display().to_string(),
            branch: args.branch.clone(),
            lazy: false,
            duration_ms: start.elapsed().as_millis() as u64,
        };

        if !args.mode.is_machine() {
            let counts = count_clone_files(&target_dir);
            eprintln!("Cloned {} ({} files).", args.url, counts.total);
        }

        return Ok(clone_summary);
    }

    // Set up JSONL stream for streaming mode.
    let jsonl_stream: Option<std::sync::Mutex<JsonlStream<Stdout>>> =
        if args.mode == OutputMode::Jsonl {
            Some(std::sync::Mutex::new(JsonlStream::new(
                "clone.event",
                "1.0",
                std::io::stdout(),
            )))
        } else {
            None
        };

    // Step 1: Fetch the repository without populating the worktree yet.
    // The checkout happens only after local checkout settings are
    // configured, otherwise lazy clones pay the non-lazy smudge cost during
    // their first materialization.
    // Note: git itself prints "Cloning into '...'" via inherited stderr,
    // so we don't duplicate that message here.

    // Emit progress for the git clone phase.
    if let Some(stream) = &jsonl_stream
        && let Ok(mut s) = stream.lock()
    {
        s.emit_progress(ProgressPayload {
            operation: "cloning".to_owned(),
            current: 0,
            total: 0,
            bytes: 0,
            total_bytes: 0,
            rate_bytes_per_sec: 0.0,
            xorbs_produced: None,
        });
    }

    let phase = PhaseTimer::start("clone", "pack_fetch");
    run_git_clone_no_checkout(parent, args, &target_dir)?;
    scrub_git_pack_appledouble_files(&target_dir)?;
    emit_phase(jsonl_stream.as_ref(), phase.finish(0, 0, 1));

    check_cancelled(cancel)?;

    // Step 2: Set up the .crab/ local config directory.
    if !args.mode.is_machine() {
        eprintln!("Configuring crab...");
    }
    setup_crab_config(&target_dir).await?;

    check_cancelled(cancel)?;

    // Step 3: Register the filter driver in the cloned repo's local config.
    crate::cmd::init::install_filter_driver(&target_dir)?;

    // Step 4: Read committed project config before checkout. The worktree is
    // still empty because we cloned with --no-checkout, so read crab.toml
    // from HEAD when available.
    let project_config = project_config_for_checkout(&target_dir)?;

    // Determine effective hydration behavior before checkout so the filter
    // process sees the right lazy/eager state during first materialization.
    let (effective_lazy, effective_include) =
        resolve_hydration_from_config(args.lazy, &args.include, project_config.as_ref());
    let checkout_lazy = effective_lazy || !effective_include.is_empty();

    if checkout_lazy {
        configure_lazy_checkout(&target_dir)?;
    }

    let has_lfs_pointers = configure_lfs_for_checkout(&target_dir, checkout_lazy)?;

    check_cancelled(cancel)?;

    // Step 5: Populate the working tree after crab config is ready.
    let phase = PhaseTimer::start("clone", "checkout");
    checkout_head(&target_dir, &args.url)?;
    emit_phase(jsonl_stream.as_ref(), phase.finish(0, 0, 1));

    // Step 3b: Auto-track extensions for any pointer blobs that landed
    // in the working tree. Cloned repositories sometimes ship a stale
    // or missing `.gitattributes` — if we only relied on the remote's
    // file, `crab hydrate <file>` would fail with "No crab-tracked
    // patterns found" until the user manually runs `crab track`.
    // Scanning for actual pointer content is authoritative: if a blob
    // parsed as a crab pointer, its extension must go through the
    // crab filter driver on subsequent clean passes anyway.
    autotrack_pointer_extensions(&target_dir, args.mode)?;

    check_cancelled(cancel)?;

    // Step 3c: Warm the local chunk-index cache via a post-clone shard
    // sync. The manifest's shard-list is downloaded once, installed
    // shards are skipped (empty set on a fresh clone), and the delta
    // is fetched + parsed into the local `PersistentChunkIndex`. This
    // is what makes subsequent pushes classify most chunks as already
    // remote without hitting `chunk_index_db` per chunk.
    //
    // CI workflows that push once and never read back can skip this
    // step with `--sync-chunk-index`.
    if args.sync_chunk_index {
        let phase = PhaseTimer::start("clone", "shard_sync");
        if let Err(e) = run_post_clone_shard_sync(&target_dir, &args.url, args.mode, cancel).await {
            // Non-fatal: the clone itself succeeded, the cache is an
            // optimisation. The next push's classifier will still hit
            // the remote `chunk_index_db` for chunks that are missing
            // from the local cache.
            tracing::warn!(error = %e, "clone: post-clone shard sync failed (non-fatal)");
        }
        emit_phase(jsonl_stream.as_ref(), phase.finish(0, 0, 1));
    }

    check_cancelled(cancel)?;

    // Step 6: Report and optionally hydrate after checkout.
    if effective_lazy && effective_include.is_empty() {
        if !args.mode.is_machine() {
            eprintln!(
                "Clone complete (lazy). Pointer files remain dehydrated.\n\
                 Hydrate Crab files selectively:  crab hydrate '*.safetensors'\n\
                 Hydrate all Crab files:          crab hydrate --all"
            );
            if has_lfs_pointers {
                eprintln!("Hydrate Git LFS files:            crab lfs pull");
            }
            eprintln!("Crab-native files also hydrate when opened.");
        }
    } else if !effective_include.is_empty() {
        check_cancelled(cancel)?;

        if !args.mode.is_machine() {
            eprintln!("Hydrating matching files...");
        }

        // Emit progress for the hydration phase.
        if let Some(stream) = &jsonl_stream
            && let Ok(mut s) = stream.lock()
        {
            s.emit_progress(ProgressPayload {
                operation: "hydrating".to_owned(),
                current: 0,
                total: 0,
                bytes: 0,
                total_bytes: 0,
                rate_bytes_per_sec: 0.0,
                xorbs_produced: None,
            });
        }

        let hydrate_args = crate::cmd::hydrate::HydrateArgs {
            patterns: effective_include.clone(),
            include: vec![],
            exclude: args.exclude.clone(),
            all: false,
            mode: crate::core::output::OutputMode::Text,
            manifest: None,
            manifest_ref: None,
            profile: None,
            ignore_sparse: false,
            recover_from: None,
        };
        let phase = PhaseTimer::start("clone", "hydration");
        run_post_checkout_hydrate(&target_dir, &hydrate_args, cancel).await?;
        emit_phase(
            jsonl_stream.as_ref(),
            phase.finish(0, 0, effective_include.len() as u64),
        );

        // Emit file_done event for the hydration phase completion.
        if let Some(stream) = &jsonl_stream
            && let Ok(mut s) = stream.lock()
        {
            s.emit_file_done(FileDonePayload {
                path: "(hydration complete)".to_owned(),
                bytes: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                status: "ok".to_owned(),
            });
        }

        if !args.mode.is_machine() {
            eprintln!("Clone complete. Matched files hydrated, rest are pointers.");
        }
    } else {
        // Full hydration (non-lazy clone).
        check_cancelled(cancel)?;

        if !args.mode.is_machine() {
            eprintln!("Hydrating files...");
        }

        let hydrate_args = crate::cmd::hydrate::HydrateArgs {
            patterns: vec![],
            include: vec![],
            exclude: args.exclude.clone(),
            all: true,
            mode: crate::core::output::OutputMode::Text,
            manifest: None,
            manifest_ref: None,
            profile: None,
            ignore_sparse: false,
            recover_from: None,
        };
        let phase = PhaseTimer::start("clone", "hydration");
        run_post_checkout_hydrate(&target_dir, &hydrate_args, cancel).await?;
        emit_phase(jsonl_stream.as_ref(), phase.finish(0, 0, 1));

        if !args.mode.is_machine() {
            eprintln!("Clone complete. All files hydrated.");
        }
    }

    let clone_summary = CloneSummary {
        url: args.url.clone(),
        directory: target_dir.display().to_string(),
        branch: args.branch.clone(),
        lazy: checkout_lazy,
        duration_ms: start.elapsed().as_millis() as u64,
    };

    // Print clone summary with file counts. On very large worktrees, avoid
    // re-reading every file just to count pointer stubs; the clone itself is
    // the work users are waiting on.
    if !args.mode.is_machine() {
        let counts = count_clone_files(&target_dir);
        if let Some(pointer_count) = counts.pointers {
            let hydrated_count = counts.total.saturating_sub(pointer_count);
            eprintln!(
                "Cloned {} ({} files, {} dehydrated). Hydrated {} files.",
                args.url, counts.total, pointer_count, hydrated_count
            );
        } else {
            eprintln!(
                "Cloned {} ({} files; dehydrated count skipped for large repo).",
                args.url, counts.total
            );
        }
    }

    // Step 6: Auto-hydrate the `always` prefetch profile if configured.
    // Runs after the working tree is fully set up (all branches above)
    // so that crab.toml is available on disk. Errors are warnings,
    // not fatal — the clone itself succeeded.
    auto_hydrate_always_profile(&target_dir, args.mode, cancel).await;

    Ok(clone_summary)
}

fn resolve_clone_url(input: &str) -> Result<String> {
    let Some((organization, repository)) = managed_shorthand_parts(input) else {
        return Ok(input.to_owned());
    };
    let config = crate::core::config::Config::resolve_local()?;
    let token_cache =
        crab_auth::token_cache::expand_token_cache_path(&config.auth.token_cache_path);
    let profiles =
        crab_auth::ServiceProfileStore::new(crab_auth::service_profile_directory(&token_cache));
    expand_managed_shorthand(input, organization, repository, &profiles)
}

fn expand_managed_shorthand(
    input: &str,
    organization: &str,
    repository: &str,
    profiles: &crab_auth::ServiceProfileStore,
) -> Result<String> {
    let Some(profile) = profiles.active()? else {
        return Ok(input.to_owned());
    };
    crab_git::ManagedRepository::new(&profile.authority, organization, repository)
        .map(|repository| repository.canonical_url())
        .map_err(Into::into)
}

fn managed_shorthand_parts(input: &str) -> Option<(&str, &str)> {
    if input.contains("://") || input.starts_with(['.', '/', '\\', '~']) {
        return None;
    }
    let (organization, repository) = input.split_once('/')?;
    if repository.contains('/')
        || crab_git::ManagedRepository::new("crab.build", organization, repository).is_err()
    {
        return None;
    }
    Some((organization, repository))
}

/// Run a shard sync against the remote after `git clone` finishes.
///
/// Builds a store from the cloned repo's config, resolves the repo
/// prefix from the remote URL, and delegates to
/// [`crate::metadata::shard_sync::run_post_fetch_shard_sync`]. A
/// sub-second no-op when the remote shard-list already matches the
/// locally-installed set.
async fn run_post_clone_shard_sync(
    repo_root: &Path,
    url: &str,
    mode: OutputMode,
    cancel: &CancellationToken,
) -> Result<()> {
    run_post_clone_shard_sync_with_selector(
        repo_root,
        url,
        mode,
        cancel,
        |config, parsed, cancel| async move {
            crate::replication::select_read_store(&config, &parsed, "clone:shard-sync", &cancel)
                .await
        },
    )
    .await
}

async fn run_post_checkout_hydrate(
    target_dir: &Path,
    args: &crate::cmd::hydrate::HydrateArgs,
    cancel: &CancellationToken,
) -> Result<()> {
    let config = crate::core::config::Config::resolve_for_repo(target_dir)?;
    let remote = config
        .remote_url
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: "remote.url".to_owned(),
            origin: "crab.toml does not declare [remote].url".to_owned(),
        })?;
    let parsed = crate::git::url::CrabUrl::parse(remote)?;
    let selection =
        crate::replication::select_read_store(&config, parsed, "hydrate", cancel).await?;
    let caching_store = crab_cache_store::CachingStore::new(selection.store, &config.cache)?;
    let hydrator = crate::cmd::hydrate::ShardHydrator::with_config_from_cli_layout(
        caching_store,
        selection.router,
        &config,
    )?;
    crate::cmd::hydrate::run_hydrate_in(target_dir, args, &config, &hydrator, cancel).await
}

async fn run_post_clone_shard_sync_with_selector<F, Fut>(
    repo_root: &Path,
    url: &str,
    mode: OutputMode,
    cancel: &CancellationToken,
    select_read: F,
) -> Result<()>
where
    F: FnOnce(crate::core::config::Config, crate::git::url::CrabUrl, CancellationToken) -> Fut,
    Fut: Future<Output = Result<crate::replication::ReadStoreSelection>>,
{
    check_cancelled(cancel)?;

    let parsed = crate::git::url::CrabUrl::parse(url)?;

    // Prefer the freshly-cloned repo's local config; fall back to defaults if
    // it isn't readable. Do not rely on process cwd: clone runs from the
    // parent directory, while the repo-local config lives in the target.
    let config = crate::core::config::Config::resolve_for_repo(repo_root).unwrap_or_default();

    let selection = select_read(config, parsed.clone(), cancel.clone()).await?;
    if let crate::replication::ReadSource::Replica { name } = &selection.source {
        tracing::debug!(replica = %name, "selected read replica for clone shard sync");
    }
    let router = selection.router;

    let cache_dir = crate::cache::default_cache_root();
    let repo_hash = crate::git::push::compute_repo_hash(&parsed.repo_path);

    let emit_progress = matches!(mode, OutputMode::Text);
    crate::metadata::shard_sync::run_post_fetch_shard_sync(
        router,
        &repo_hash,
        &cache_dir,
        None,
        emit_progress,
    )
    .await?;
    // The sync updates the persistent chunk index and local shard cache;
    // let that derived-state mutation settle before honoring cancellation.
    check_cancelled(cancel)?;

    Ok(())
}

/// Extract a directory name from a crab URL.
///
/// `crab://bucket/org/repo` → `repo`
/// `crab://bucket/repo` → `repo`
fn repo_name_from_url(url: &str) -> Result<String> {
    if !is_crab_url(url) {
        return repo_name_from_git_clone_source(url);
    }

    let parsed = crate::git::url::CrabUrl::parse(url)?;
    let name = parsed
        .repo_path
        .rsplit('/')
        .next()
        .unwrap_or(&parsed.repo_path)
        .to_owned();

    if name.is_empty() {
        return Err(CrabError::Configuration {
            key: "cannot derive directory name from URL".into(),
            origin: url.to_owned(),
        });
    }

    Ok(name)
}

fn is_crab_url(url: &str) -> bool {
    url.trim()
        .split_once("://")
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("crab"))
}

/// Extract Git's default destination name for common clone sources.
///
/// This mirrors the user-visible behavior of `git clone <repo>` for
/// URL, scp-like, and local-path sources closely enough that `crab
/// clone` can pass an explicit target path while still reporting the
/// directory it created.
fn repo_name_from_git_clone_source(source: &str) -> Result<String> {
    let trimmed = source.trim().trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return Err(CrabError::Configuration {
            key: "cannot derive directory name from empty clone source".into(),
            origin: source.to_owned(),
        });
    }

    if let Ok(parsed) = url::Url::parse(trimmed)
        && let Some(name) =
            repo_name_from_path_segments(parsed.path_segments().into_iter().flatten())
    {
        return Ok(name);
    }

    let path_like = if let Some((left, right)) = trimmed.rsplit_once(':') {
        if left.contains('@') { right } else { trimmed }
    } else {
        trimmed
    };

    if let Some(name) = repo_name_from_path_segments(path_like.split(['/', '\\'])) {
        return Ok(name);
    }

    Err(CrabError::Configuration {
        key: "cannot derive directory name from URL".into(),
        origin: source.to_owned(),
    })
}

fn repo_name_from_path_segments<'a>(segments: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut parts: Vec<&str> = segments.into_iter().filter(|s| !s.is_empty()).collect();
    if parts.last().is_some_and(|s| *s == ".git") {
        parts.pop();
    }

    let raw_name = parts.last()?.trim_end_matches(".git");
    if raw_name.is_empty() || raw_name == "." || raw_name == ".." {
        return None;
    }

    Some(raw_name.to_owned())
}

/// Run a regular `git clone` for non-Crab repositories.
fn run_git_clone(parent: &Path, args: &CloneArgs, target: &Path) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone");

    if let Some(ref branch) = args.branch {
        cmd.arg("--branch").arg(branch);
    }

    if let Some(depth) = args.depth {
        cmd.arg("--depth").arg(depth.to_string());
    }

    cmd.arg("--");
    cmd.arg(&args.url);
    cmd.arg(target);
    cmd.current_dir(parent);

    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    tracing::info!(url = %args.url, "running git clone");

    let status = cmd.status()?;
    if !status.success() {
        return Err(CrabError::Protocol(format!(
            "git clone exited with status {}",
            status.code().unwrap_or(-1),
        )));
    }

    Ok(())
}

/// Run `git clone` without materializing the worktree.
///
/// Uses `--config filter.crab.process=...` to inject the filter driver
/// into the new repo. Checkout is intentionally deferred until `.crab/`
/// config has been written so lazy clones do not accidentally hydrate
/// during their first worktree update.
fn run_git_clone_no_checkout(parent: &Path, args: &CloneArgs, target: &Path) -> Result<()> {
    let bin = crate::cmd::init::crab_binary_path();

    let mut cmd = Command::new("git");
    cmd.arg("clone");

    // Inject the filter driver config so the initial checkout uses crab.
    cmd.arg("--config")
        .arg(format!("filter.crab.process={bin} filter-process"));
    cmd.arg("--config")
        .arg(format!("filter.crab.clean={bin} filter-process"));
    cmd.arg("--config")
        .arg(format!("filter.crab.smudge={bin} filter-process"));
    cmd.arg("--config").arg("filter.crab.required=true");

    if let Some(ref branch) = args.branch {
        cmd.arg("--branch").arg(branch);
    }

    if let Some(depth) = args.depth {
        cmd.arg("--depth").arg(depth.to_string());
    }

    cmd.arg("--no-checkout");

    cmd.arg(&args.url);
    cmd.arg(target);
    cmd.current_dir(parent);

    // Inherit stdout/stderr so the user sees git's progress output.
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    tracing::info!(url = %args.url, "running git clone");

    let status = cmd.status()?;
    if !status.success() {
        return Err(CrabError::Protocol(format!(
            "git clone exited with status {}",
            status.code().unwrap_or(-1),
        )));
    }

    Ok(())
}

/// Populate the worktree from HEAD after crab config is ready.
fn checkout_head(target: &Path, remote_url: &str) -> Result<()> {
    scrub_git_pack_appledouble_files(target)?;

    let checkout_status = Command::new("git")
        .args(["checkout", "HEAD"])
        .env(crate::core::config::CLONE_REMOTE_URL_ENV, remote_url)
        .current_dir(target)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;

    if !checkout_status.success() {
        tracing::warn!("git checkout after clone returned non-zero, continuing");
    }

    Ok(())
}

fn configure_lfs_for_checkout(target: &Path, skip_smudge: bool) -> Result<bool> {
    let head = Command::new("git")
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .current_dir(target)
        .output()?;
    if head.status.code() == Some(1) {
        return Ok(false);
    }
    if !head.status.success() {
        return Err(CrabError::Protocol(format!(
            "failed to inspect cloned repository HEAD: {}",
            String::from_utf8_lossy(&head.stderr).trim()
        )));
    }

    let output = Command::new("git")
        .args([
            "grep",
            "-I",
            "-q",
            "-e",
            "version https://git-lfs.github.com/spec/v1",
            "HEAD",
            "--",
            ".",
        ])
        .current_dir(target)
        .output()?;

    if output.status.code() == Some(1) {
        return Ok(false);
    }
    if !output.status.success() {
        return Err(CrabError::Protocol(format!(
            "failed to inspect cloned repository for Git LFS pointers: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    crate::cmd::lfs::install::run_lfs_install_in(
        target,
        crate::cmd::lfs::install::LfsInstallOptions {
            local: true,
            skip_smudge,
            ..crate::cmd::lfs::install::LfsInstallOptions::default()
        },
    )?;
    Ok(true)
}

fn scrub_git_pack_appledouble_files(target: &Path) -> Result<usize> {
    let pack_dir = target.join(".git").join("objects").join("pack");
    let entries = match std::fs::read_dir(&pack_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(CrabError::Io(e)),
    };

    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("._") {
            continue;
        }

        // Git scans every `*.idx` in this directory. macOS AppleDouble
        // `._pack-*.idx` sidecars are metadata, not pack indexes, and make
        // checkout fail with "non-monotonic index" on external volumes.
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CrabError::Io(e)),
        }
    }

    if removed > 0 {
        tracing::warn!(
            pack_dir = %pack_dir.display(),
            removed,
            "removed macOS AppleDouble sidecar files from Git pack directory"
        );
    }
    Ok(removed)
}

/// Load committed `crab.toml` before checkout when possible.
fn project_config_for_checkout(
    target: &Path,
) -> Result<Option<crate::core::project_config::ProjectConfig>> {
    match project_config_from_head(target)? {
        Some(config) => Ok(Some(config)),
        None => crate::core::project_config::ProjectConfig::load_for_repo(target),
    }
}

fn project_config_from_head(
    target: &Path,
) -> Result<Option<crate::core::project_config::ProjectConfig>> {
    let output = Command::new("git")
        .args(["show", "HEAD:crab.toml"])
        .current_dir(target)
        .output()
        .map_err(CrabError::Io)?;

    if !output.status.success() {
        return Ok(None);
    }

    let content = String::from_utf8(output.stdout).map_err(|error| CrabError::Configuration {
        key: "crab.toml".to_owned(),
        origin: format!("committed config is not UTF-8: {error}"),
    })?;
    crate::core::project_config::ProjectConfig::parse(&content, "HEAD:crab.toml").map(Some)
}

/// Create the `.crab/` directory and its local settings file.
async fn setup_crab_config(target: &Path) -> Result<()> {
    let crab_dir = target.join(".crab");
    tokio::fs::create_dir_all(&crab_dir).await?;
    crate::cmd::init::ensure_crab_dir_excluded(target)?;

    let config_path = crab_dir.join("local.toml");
    if !config_path.exists() {
        tokio::fs::write(&config_path, b"# Crab local settings (not committed)\n").await?;
    }

    Ok(())
}

/// Set `checkout.lazy = true` in the local crab config.
fn configure_lazy_checkout(target: &Path) -> Result<()> {
    let config_path = target.join(".crab/local.toml");
    crate::cmd::config::run_config_set_at("checkout.lazy", "true", &config_path)
}

/// Auto-hydrate the `always` prefetch profile after a clone.
///
/// Loads the `prefetch.profiles.always` entry from `crab.toml`. If the `always`
/// profile exists and `hydrate.auto_prefetch` is not `false` (default
/// is `true`), expands the profile's globs against the working tree
/// and hydrates matching pointer files.
///
/// Errors during auto-hydrate are logged as warnings and never
/// propagated — the clone itself already succeeded and the user can
/// always run `crab hydrate --profile=always` manually.
async fn auto_hydrate_always_profile(
    repo_root: &Path,
    mode: OutputMode,
    cancel: &CancellationToken,
) {
    let config = crate::core::config::Config::resolve_local().unwrap_or_default();

    if !config.hydrate.auto_prefetch {
        tracing::debug!("auto_prefetch disabled, skipping always-profile hydration");
        return;
    }

    let prefetch = match crate::hydrate::profile::load_prefetch(repo_root) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load prefetch config, skipping auto-hydrate");
            return;
        }
    };

    let always_globs = match prefetch.profiles.get("always") {
        Some(globs) if !globs.is_empty() => globs,
        Some(_) => {
            tracing::debug!("always profile has no patterns, skipping auto-hydrate");
            return;
        }
        None => {
            tracing::debug!("no always prefetch profile in crab.toml, skipping auto-hydrate");
            return;
        }
    };

    // Convert the profile's globs into pattern strings for the hydrate
    // command. The hydrate infrastructure already handles glob expansion
    // against the working tree.
    let patterns: Vec<String> = always_globs.iter().map(|g| g.glob().to_owned()).collect();

    tracing::info!(
        profile = "always",
        patterns = ?patterns,
        "auto-hydrating always prefetch profile"
    );

    if !mode.is_machine() {
        eprintln!("Auto-hydrating 'always' prefetch profile...");
    }

    let hydrate_args = crate::cmd::hydrate::HydrateArgs {
        patterns,
        include: vec![],
        exclude: vec![],
        all: false,
        mode: crate::core::output::OutputMode::Text,
        manifest: None,
        manifest_ref: None,
        profile: None,
        ignore_sparse: false,
        recover_from: None,
    };

    if let Err(e) = crate::cmd::hydrate::run_hydrate(&hydrate_args, &config, cancel).await {
        tracing::warn!(
            error = %e,
            "auto-hydrate of always profile failed; clone succeeded, run \
             `crab hydrate --profile=always` to retry"
        );
        if !mode.is_machine() {
            eprintln!(
                "Warning: auto-hydrate of 'always' profile failed: {e}\n\
                 Run `crab hydrate --profile=always` to retry."
            );
        }
    } else {
        tracing::info!("auto-hydrate of always profile complete");
        if !mode.is_machine() {
            eprintln!("Auto-hydrate of 'always' profile complete.");
        }
    }
}

/// Walk the working tree, detect crab pointer blobs, and ensure every
/// pointer path has an effective `filter=crab` rule in `.gitattributes`.
///
/// Idempotent — already-tracked patterns are skipped. Honours the
/// remote's `.gitattributes` content and only appends rules that are
/// missing. Files without an extension do not trigger any writes.
///
/// The walk skips `.git/`, `.crab/`, and symlinks to keep scan cost
/// proportional to actual working-tree content. I/O errors on
/// individual files are logged at `debug!` and the walk continues —
/// auto-tracking is a best-effort convenience, not a correctness
/// requirement.
///
/// Used by `crab clone` (first checkout) and `crab hydrate` /
/// `crab dehydrate` (to pick up new pointer extensions introduced by
/// a subsequent `git pull` without forcing the user to re-run
/// `crab track`).
pub(crate) fn autotrack_pointer_extensions(target: &Path, mode: OutputMode) -> Result<()> {
    use std::collections::BTreeSet;
    use std::fmt::Write as _;

    use crate::engine::pointer::is_working_tree_pointer;

    let ga_path = target.join(".gitattributes");
    #[cfg(not(feature = "gix-pathmatch"))]
    let already_tracked = read_tracked_globs(&ga_path)?;
    #[cfg(feature = "gix-pathmatch")]
    let classifier = crate::core::attrs::TrackedClassifier::open(target, "crab")?;
    #[cfg(feature = "gix-pathmatch")]
    let is_tracked = |path: &Path| {
        path.strip_prefix(target)
            .ok()
            .is_some_and(|rel| classifier.is_tracked(&rel.to_string_lossy()))
    };
    #[cfg(not(feature = "gix-pathmatch"))]
    let is_tracked = |path: &Path| {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| already_tracked.contains(&format!("*.{ext}")))
    };

    let mut new_exts: BTreeSet<String> = BTreeSet::new();
    if let Some(files) = git_tracked_files(target) {
        for path in files {
            record_pointer_extension(&path, &mut new_exts, &is_tracked, &|p| {
                is_working_tree_pointer(p).unwrap_or(false)
            });
        }
    } else {
        walk_for_pointers(target, target, &is_tracked, &mut new_exts, &|p| {
            is_working_tree_pointer(p).unwrap_or(false)
        })?;
    }

    if new_exts.is_empty() {
        return Ok(());
    }

    // Append missing rules in a single write. `run_track_in` would
    // re-take the advisory flock for each pattern; batching avoids
    // that flock churn.
    let mut content = std::fs::read_to_string(&ga_path).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    for ext in &new_exts {
        // Infallible: writing to a `String` never returns `Err`.
        let _ = writeln!(content, "*{ext} filter=crab diff=crab merge=crab -text");
    }
    std::fs::write(&ga_path, content)?;

    if !mode.is_machine() {
        let joined: Vec<String> = new_exts.iter().map(|e| format!("*{e}")).collect();
        eprintln!(
            "Auto-tracked {} extension(s) for crab: {}",
            new_exts.len(),
            joined.join(", "),
        );
    }
    tracing::info!(
        extensions = ?new_exts,
        "auto-tracked pointer file extensions"
    );
    Ok(())
}

/// Resolve effective hydration behavior by merging CLI flags with `crab.toml` config.
///
/// Returns `(effective_lazy, effective_include_patterns)`. The `crab.toml`
/// settings only apply when the user didn't pass explicit CLI flags.
fn resolve_hydration_from_config(
    user_lazy: bool,
    user_include: &[String],
    project_config: Option<&crate::core::project_config::ProjectConfig>,
) -> (bool, Vec<String>) {
    let Some(config) = project_config else {
        return (user_lazy, user_include.to_vec());
    };

    let Some(ref hydrate_config) = config.hydrate else {
        return (user_lazy, user_include.to_vec());
    };

    // If user passed explicit --include patterns, those take precedence.
    if !user_include.is_empty() {
        return (user_lazy, user_include.to_vec());
    }

    // If crab.toml says eager and user didn't pass explicit --lazy,
    // hydrate everything (lazy=false).
    let effective_lazy = match hydrate_config.default {
        crate::core::project_config::HydrateMode::Eager => {
            // Only override if user didn't explicitly pass --lazy (which
            // would be the default value of true). Since we can't distinguish
            // "user passed --lazy" from "default true", we treat Eager config
            // as overriding the default.
            false
        }
        crate::core::project_config::HydrateMode::Lazy => user_lazy,
    };

    // If crab.toml has auto_patterns, use them as include patterns.
    let effective_include = match &hydrate_config.auto_patterns {
        Some(patterns) if !patterns.is_empty() => patterns.clone(),
        _ => vec![],
    };

    (effective_lazy, effective_include)
}

const CLONE_POINTER_SUMMARY_SCAN_LIMIT: usize = 10_000;

struct CloneFileCounts {
    total: usize,
    pointers: Option<usize>,
}

/// Count total files and, for reasonably-sized worktrees, pointer files.
fn count_clone_files(target: &Path) -> CloneFileCounts {
    if let Some(files) = git_tracked_files(target) {
        let total = files.len();
        if total > CLONE_POINTER_SUMMARY_SCAN_LIMIT {
            return CloneFileCounts {
                total,
                pointers: None,
            };
        }
        let pointers = files.iter().filter(|p| is_clone_pointer(p)).count();
        return CloneFileCounts {
            total,
            pointers: Some(pointers),
        };
    }

    let mut total = 0usize;
    let mut pointers = 0usize;

    walk_clone_files_for_summary(target, &mut total, &mut pointers);
    CloneFileCounts {
        total,
        pointers: Some(pointers),
    }
}

fn walk_clone_files_for_summary(dir: &Path, total: &mut usize, pointers: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str == ".git" || name_str == ".crab" {
            continue;
        }

        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            walk_clone_files_for_summary(&path, total, pointers);
        } else if ft.is_file() {
            *total += 1;
            if is_clone_pointer(&path) {
                *pointers += 1;
            }
        }
    }
}

fn is_clone_pointer(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if metadata.len() > crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE as u64 {
        return false;
    }

    let Ok(content) = std::fs::read(path) else {
        return false;
    };
    !matches!(
        crab_git::pointer_detect::classify(&content),
        crab_git::pointer_detect::PointerKind::NotAPointer
    )
}

fn git_tracked_files(target: &Path) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(target)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_nul_paths(target, &output.stdout))
}

fn parse_nul_paths(root: &Path, bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|b| *b == 0)
        .filter(|raw| !raw.is_empty())
        .map(|raw| root.join(String::from_utf8_lossy(raw).as_ref()))
        .collect()
}

/// Read `.gitattributes` and return the set of glob patterns that are
/// already wired to `filter=crab`. Missing files yield an empty set.
#[cfg(not(feature = "gix-pathmatch"))]
fn read_tracked_globs(ga_path: &Path) -> Result<std::collections::HashSet<String>> {
    let content = match std::fs::read_to_string(ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(std::collections::HashSet::new());
        }
        Err(e) => return Err(e.into()),
    };
    Ok(content
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#') && t.contains("filter=crab")
        })
        .filter_map(|line| line.split_whitespace().next().map(String::from))
        .collect())
}

/// Recursively walk `dir` under `root`, calling `is_pointer` on each
/// regular file. When a pointer is found whose extension is not covered
/// by any already-tracked glob, the extension (with leading `.`) is
/// recorded in `new_exts`.
fn walk_for_pointers(
    root: &Path,
    dir: &Path,
    is_tracked: &dyn Fn(&Path) -> bool,
    new_exts: &mut std::collections::BTreeSet<String>,
    is_pointer: &dyn Fn(&Path) -> bool,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) => {
            tracing::debug!(path = %dir.display(), error = %e, "skip dir during autotrack");
            return Ok(());
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip git/crab internals — they never contain pointer blobs
        // and walking them wastes stat calls on large repos.
        if path == root.join(".git") || path == root.join(".crab") {
            continue;
        }
        if name_str == ".git" || name_str == ".crab" {
            continue;
        }

        let Ok(ft) = entry.file_type() else {
            continue;
        };

        if ft.is_symlink() {
            continue;
        }

        if ft.is_dir() {
            walk_for_pointers(root, &path, is_tracked, new_exts, is_pointer)?;
            continue;
        }

        if !ft.is_file() {
            continue;
        }

        record_pointer_extension(&path, new_exts, is_tracked, is_pointer);
    }

    Ok(())
}

fn record_pointer_extension(
    path: &Path,
    new_exts: &mut std::collections::BTreeSet<String>,
    is_tracked: &dyn Fn(&Path) -> bool,
    is_pointer: &dyn Fn(&Path) -> bool,
) {
    if is_tracked(path) {
        return;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return;
    };

    if is_pointer(path) {
        new_exts.insert(format!(".{ext}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_in(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .expect("git command should spawn");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn managed_shorthand_expands_through_the_exact_active_profile() {
        let directory = tempfile::tempdir().unwrap();
        let profiles = crab_auth::ServiceProfileStore::new(directory.path().to_path_buf());
        let profile =
            crab_auth::ServiceProfile::new("code.corp.example", crab_auth::ServiceTrust::default())
                .unwrap();
        profiles.store(&profile).unwrap();
        profiles.set_active("code.corp.example").unwrap();

        let expanded =
            expand_managed_shorthand("acme/models", "acme", "models", &profiles).unwrap();

        assert_eq!(expanded, "crab://code.corp.example/acme/models");
    }

    #[test]
    fn managed_shorthand_requires_an_active_profile_and_exact_slug_pair() {
        let directory = tempfile::tempdir().unwrap();
        let profiles = crab_auth::ServiceProfileStore::new(directory.path().to_path_buf());

        assert_eq!(
            expand_managed_shorthand("acme/models", "acme", "models", &profiles).unwrap(),
            "acme/models"
        );
        assert!(managed_shorthand_parts("acme/models").is_some());
        for direct_or_invalid in [
            "./acme/models",
            "../acme/models",
            "/acme/models",
            "acme/models/extra",
            "Acme/models",
            "https://example.com/acme/models",
        ] {
            assert!(managed_shorthand_parts(direct_or_invalid).is_none());
        }
    }

    #[test]
    fn repo_name_simple_url() {
        let name = repo_name_from_url("crab://bucket/my-repo").unwrap();
        assert_eq!(name, "my-repo");
    }

    #[test]
    fn repo_name_nested_path() {
        let name = repo_name_from_url("crab://bucket/org/project/repo").unwrap();
        assert_eq!(name, "repo");
    }

    #[test]
    fn repo_name_strips_trailing_slash() {
        let name = repo_name_from_url("crab://bucket/repo/").unwrap();
        // CrabUrl::parse strips trailing slashes, so this should work.
        assert_eq!(name, "repo");
    }

    #[test]
    fn repo_name_https_git_url() {
        let name = repo_name_from_url("https://github.com/openclaw/openclaw.git").unwrap();
        assert_eq!(name, "openclaw");
    }

    #[test]
    fn repo_name_scp_like_git_url() {
        let name = repo_name_from_url("git@github.com:openclaw/openclaw.git").unwrap();
        assert_eq!(name, "openclaw");
    }

    #[test]
    fn repo_name_local_git_dir_path() {
        let name = repo_name_from_url("/tmp/openclaw/.git").unwrap();
        assert_eq!(name, "openclaw");
    }

    #[test]
    fn repo_name_rejects_empty() {
        let err = repo_name_from_url("crab://bucket/");
        assert!(err.is_err());
    }

    #[test]
    fn clone_configures_crab_lfs_filter_before_checkout() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        for (skip_smudge, expected_suffix) in [
            (false, "lfs filter-process"),
            (true, "lfs filter-process --skip"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            git_in(root, &["init"]);
            git_in(root, &["config", "user.email", "test@example.com"]);
            git_in(root, &["config", "user.name", "Test User"]);
            std::fs::write(
                root.join("model.dat"),
                b"version https://git-lfs.github.com/spec/v1\n\
                  oid sha256:0000000000000000000000000000000000000000000000000000000000000000\n\
                  size 1\n",
            )
            .unwrap();
            git_in(root, &["add", "model.dat"]);
            git_in(root, &["commit", "-m", "add lfs pointer"]);

            assert!(configure_lfs_for_checkout(root, skip_smudge).unwrap());

            let process = std::process::Command::new("git")
                .args(["config", "--local", "--get", "filter.lfs.process"])
                .current_dir(root)
                .output()
                .unwrap();
            assert!(process.status.success());
            assert!(
                String::from_utf8_lossy(&process.stdout)
                    .trim()
                    .ends_with(expected_suffix)
            );
            let transfer = std::process::Command::new("git")
                .args(["config", "--local", "--get", "lfs.standalonetransferagent"])
                .current_dir(root)
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&transfer.stdout).trim(), "crab");
        }
    }

    #[test]
    fn clone_leaves_lfs_config_untouched_without_lfs_pointers() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_in(root, &["init"]);
        git_in(root, &["config", "user.email", "test@example.com"]);
        git_in(root, &["config", "user.name", "Test User"]);
        std::fs::write(root.join("README.md"), b"ordinary Git content\n").unwrap();
        git_in(root, &["add", "README.md"]);
        git_in(root, &["commit", "-m", "add ordinary file"]);

        assert!(!configure_lfs_for_checkout(root, false).unwrap());

        let process = std::process::Command::new("git")
            .args(["config", "--local", "--get", "filter.lfs.process"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(!process.status.success());
    }

    #[test]
    fn clone_leaves_lfs_config_untouched_for_empty_repository() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_in(root, &["init"]);

        assert!(!configure_lfs_for_checkout(root, false).unwrap());

        let process = std::process::Command::new("git")
            .args(["config", "--local", "--get", "filter.lfs.process"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(!process.status.success());
    }

    #[tokio::test]
    async fn clone_plain_git_repo_does_not_write_crab_config() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path();
        git_in(source, &["init"]);
        std::fs::write(source.join("README.md"), b"# plain git\n").unwrap();
        git_in(source, &["add", "README.md"]);
        git_in(
            source,
            &[
                "-c",
                "user.name=Crab Test",
                "-c",
                "user.email=crab@example.test",
                "commit",
                "-m",
                "initial",
            ],
        );

        let parent_dir = tempfile::tempdir().unwrap();
        let target_name = "plain-clone";
        let args = CloneArgs {
            url: source.display().to_string(),
            directory: Some(PathBuf::from(target_name)),
            branch: None,
            depth: None,
            lazy: true,
            include: vec![],
            exclude: vec![],
            sync_chunk_index: false,
            mode: OutputMode::Json,
        };

        let summary = run_clone_in(parent_dir.path(), &args, &CancellationToken::new())
            .await
            .unwrap();
        let target = parent_dir.path().join(target_name);

        assert_eq!(summary.lazy, false);
        assert!(target.join("README.md").exists());
        assert!(!target.join(".crab").exists());

        let filter_config = std::process::Command::new("git")
            .args(["config", "--local", "--get", "filter.crab.process"])
            .current_dir(&target)
            .output()
            .unwrap();
        assert!(!filter_config.status.success());
    }

    #[tokio::test]
    async fn clone_setup_excludes_local_crab_dir_from_add_all() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_in(root, &["init"]);

        setup_crab_config(root).await.unwrap();

        let ignored = std::process::Command::new("git")
            .args(["check-ignore", "-v", ".crab/local.toml"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            ignored.status.success(),
            ".crab/local.toml should be ignored by local git exclude: {}",
            String::from_utf8_lossy(&ignored.stderr)
        );

        git_in(root, &["add", "."]);
        let output = std::process::Command::new("git")
            .args(["ls-files"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git ls-files failed");
        let tracked = String::from_utf8_lossy(&output.stdout);
        assert!(
            !tracked.lines().any(|path| path.starts_with(".crab/")),
            "local .crab state must not be tracked after git add ., got: {tracked}",
        );
    }

    #[tokio::test]
    async fn clone_setup_preserves_local_checkout_settings() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_in(root, &["init"]);
        let crab_dir = root.join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();
        std::fs::write(crab_dir.join("local.toml"), "[checkout]\nlazy = true\n").unwrap();

        setup_crab_config(root).await.unwrap();

        let config = std::fs::read_to_string(crab_dir.join("local.toml")).unwrap();
        assert!(config.contains("[checkout]"));
        assert!(config.contains("lazy = true"));
        assert!(!crab_dir.join("remote").exists());
        assert!(!config.contains("[remote]"));
        assert!(!config.contains("gateway"));
        assert!(!config.contains("credential"));
    }

    #[tokio::test]
    async fn clone_shard_sync_uses_selected_replica_store() {
        let cache_tmp = tempfile::tempdir().unwrap();
        let _cache_guard = crate::test::git_repo::CacheDirGuard::new(cache_tmp.path());
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".crab")).unwrap();

        let prefix = "org/repo";
        let primary = crate::storage::store::Store::new(std::sync::Arc::new(
            object_store::memory::InMemory::new(),
        ));
        let primary_router = crate::storage::StoreLayout::new(primary.clone(), prefix.to_owned());
        let primary_hash =
            write_sync_manifest_with_shard(&primary, &primary_router, 11, b"primary shard").await;

        let replica = crate::storage::store::Store::new(std::sync::Arc::new(
            object_store::memory::InMemory::new(),
        ));
        let replica_router = crate::storage::StoreLayout::new(replica.clone(), prefix.to_owned());
        let replica_hash =
            write_sync_manifest_with_shard(&replica, &replica_router, 11, b"replica shard").await;

        run_post_clone_shard_sync_with_selector(
            repo.path(),
            "crab://primary/org/repo",
            OutputMode::Json,
            &CancellationToken::new(),
            move |_, _, _| {
                let replica = replica.clone();
                async move {
                    Ok(crate::replication::ReadStoreSelection {
                        store: replica.clone(),
                        router: crate::storage::StoreLayout::new(replica, prefix.to_owned()),
                        source: crate::replication::ReadSource::Replica {
                            name: "west".to_owned(),
                        },
                    })
                }
            },
        )
        .await
        .unwrap();

        assert!(
            shard_cache_path(cache_tmp.path(), &replica_hash).exists(),
            "clone shard sync must cache the shard from the selected replica"
        );
        assert!(
            !shard_cache_path(cache_tmp.path(), &primary_hash).exists(),
            "clone shard sync must not silently read the primary when a replica is selected"
        );
    }

    async fn write_sync_manifest_with_shard(
        store: &crate::storage::store::Store,
        router: &crate::storage::StoreLayout,
        generation: u64,
        shard_bytes: &'static [u8],
    ) -> String {
        let shard_hash = crab_xet::hash::compute_data_hash(shard_bytes);
        store
            .put(
                &router.shard_path(&shard_hash),
                bytes::Bytes::from_static(shard_bytes),
            )
            .await
            .unwrap();

        let shard_hex = shard_hash.hex();
        let (shard_index_hash, _index, shard_write) =
            crate::metadata::manifest::compact_shard_index(
                generation,
                std::slice::from_ref(&shard_hex),
            )
            .unwrap();
        crate::metadata::segmented::upload_write(store, router, &shard_write)
            .await
            .unwrap();

        let mut manifest = crate::metadata::manifest::Manifest::default_for_repo("refs/heads/main");
        manifest.generation = generation;
        manifest.shard_index_hash = shard_index_hash;
        manifest.seal_git_validation();
        store
            .put(
                &router.manifest_path(),
                bytes::Bytes::from(serde_json::to_vec(&manifest).unwrap()),
            )
            .await
            .unwrap();

        shard_hex
    }

    fn shard_cache_path(cache_root: &Path, hash: &str) -> PathBuf {
        cache_root.join("shards").join(&hash[..2]).join(hash)
    }

    #[test]
    fn scrub_git_pack_appledouble_files_removes_only_pack_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let pack_dir = root.join(".git/objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        std::fs::write(pack_dir.join("._pack-a.idx"), b"appledouble").unwrap();
        std::fs::write(pack_dir.join("._pack-a.pack"), b"appledouble").unwrap();
        std::fs::write(pack_dir.join("pack-a.idx"), b"real idx").unwrap();
        std::fs::write(pack_dir.join("pack-a.pack"), b"real pack").unwrap();

        let removed = scrub_git_pack_appledouble_files(root).unwrap();

        assert_eq!(removed, 2);
        assert!(!pack_dir.join("._pack-a.idx").exists());
        assert!(!pack_dir.join("._pack-a.pack").exists());
        assert!(pack_dir.join("pack-a.idx").exists());
        assert!(pack_dir.join("pack-a.pack").exists());
    }

    /// Helper: write a sample pointer blob at `path`.
    fn write_pointer(path: &Path) {
        let p = crab_types::pointer::Pointer {
            file_hash: [1u8; 32],
            size: 123,
            shard_hint: None,
        };
        std::fs::write(path, p.serialize()).unwrap();
    }

    #[test]
    fn clone_summary_counts_crab_and_lfs_pointers() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_in(root, &["init"]);
        write_pointer(&root.join("native.bin"));
        std::fs::write(
            root.join("model.dat"),
            b"version https://git-lfs.github.com/spec/v1\n\
              oid sha256:0000000000000000000000000000000000000000000000000000000000000000\n\
              size 1\n",
        )
        .unwrap();
        std::fs::write(root.join("README.md"), b"ordinary Git content\n").unwrap();
        git_in(root, &["add", "native.bin", "model.dat", "README.md"]);

        let counts = count_clone_files(root);

        assert_eq!(counts.total, 3);
        assert_eq!(counts.pointers, Some(2));
    }

    #[test]
    fn autotrack_adds_rules_for_pointer_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_pointer(&root.join("model.bin"));
        write_pointer(&root.join("weights.safetensors"));
        std::fs::write(root.join("README.md"), b"# hello").unwrap();

        autotrack_pointer_extensions(root, OutputMode::Text).unwrap();

        let ga = std::fs::read_to_string(root.join(".gitattributes")).unwrap();
        assert!(ga.contains("*.bin filter=crab"));
        assert!(ga.contains("*.safetensors filter=crab"));
        assert!(!ga.contains("*.md"), "plain-text files must not be tracked");
    }

    #[test]
    fn autotrack_preserves_existing_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(".gitattributes"),
            "*.dmg filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        write_pointer(&root.join("big.bin"));

        autotrack_pointer_extensions(root, OutputMode::Text).unwrap();

        let ga = std::fs::read_to_string(root.join(".gitattributes")).unwrap();
        // Existing rule stays, new extension is appended once.
        assert_eq!(ga.matches("*.dmg filter=crab").count(), 1);
        assert_eq!(ga.matches("*.bin filter=crab").count(), 1);
    }

    #[test]
    fn autotrack_is_idempotent_on_already_tracked_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        write_pointer(&root.join("a.bin"));
        write_pointer(&root.join("b.bin"));

        autotrack_pointer_extensions(root, OutputMode::Text).unwrap();

        let ga = std::fs::read_to_string(root.join(".gitattributes")).unwrap();
        assert_eq!(ga.matches("*.bin filter=crab").count(), 1);
    }

    #[test]
    fn autotrack_preserves_directory_qualified_pointer_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("models")).unwrap();
        std::fs::write(
            root.join(".gitattributes"),
            "models/*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        write_pointer(&root.join("models/model.bin"));

        autotrack_pointer_extensions(root, OutputMode::Text).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join(".gitattributes")).unwrap(),
            "models/*.bin filter=crab diff=crab merge=crab -text\n"
        );
    }

    #[test]
    fn autotrack_covers_pointer_outside_directory_qualified_rule() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("other")).unwrap();
        std::fs::write(
            root.join(".gitattributes"),
            "models/*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        write_pointer(&root.join("other/model.bin"));

        autotrack_pointer_extensions(root, OutputMode::Text).unwrap();

        let attributes = std::fs::read_to_string(root.join(".gitattributes")).unwrap();
        assert!(attributes.contains("*.bin filter=crab diff=crab merge=crab -text"));
    }

    #[test]
    fn autotrack_skips_git_and_crab_internals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".crab")).unwrap();
        // Pointer-looking blob in .git must NOT trigger a track rule.
        write_pointer(&root.join(".git/FAKE.bin"));

        autotrack_pointer_extensions(root, OutputMode::Text).unwrap();

        let ga = std::fs::read_to_string(root.join(".gitattributes")).unwrap_or_default();
        assert!(!ga.contains("*.bin"));
    }

    #[test]
    fn autotrack_no_pointers_noop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("plain.txt"), b"nothing to see").unwrap();

        autotrack_pointer_extensions(root, OutputMode::Text).unwrap();

        // No .gitattributes should be written when nothing matches.
        assert!(!root.join(".gitattributes").exists());
    }

    #[test]
    fn project_config_from_head_reads_before_checkout() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path();
        git_in(src_root, &["init"]);
        git_in(src_root, &["config", "user.email", "test@example.com"]);
        git_in(src_root, &["config", "user.name", "Test User"]);
        std::fs::write(
            src_root.join("crab.toml"),
            "[remote]\nurl = \"crab://bucket/repo\"\n\n[hydrate]\ndefault = \"eager\"\n",
        )
        .unwrap();
        git_in(src_root, &["add", "crab.toml"]);
        git_in(src_root, &["commit", "-m", "add crab config"]);

        let dst = tempfile::tempdir().unwrap();
        let target = dst.path().join("repo");
        let status = std::process::Command::new("git")
            .args([
                "clone",
                "--no-checkout",
                src_root.to_str().unwrap(),
                target.to_str().unwrap(),
            ])
            .status()
            .expect("git clone should spawn");
        assert!(status.success(), "git clone --no-checkout failed");
        assert!(
            !target.join("crab.toml").exists(),
            "worktree should still be empty before checkout"
        );

        let config = project_config_for_checkout(&target)
            .expect("config should parse")
            .expect("config should load from HEAD");
        assert!(matches!(
            config.hydrate.as_ref().unwrap().default,
            crate::core::project_config::HydrateMode::Eager
        ));
    }

    // --- auto_hydrate_always_profile tests ---

    #[tokio::test]
    async fn auto_hydrate_skips_when_no_prefetch_toml() {
        // When crab.toml doesn't exist, auto-hydrate should
        // complete silently without error.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".crab")).unwrap();

        let cancel = CancellationToken::new();
        // Should not panic or error — just silently skip.
        auto_hydrate_always_profile(root, OutputMode::Text, &cancel).await;
    }

    #[tokio::test]
    async fn auto_hydrate_skips_when_no_always_profile() {
        // When crab.toml has no `always` profile,
        // auto-hydrate should skip silently.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("crab.toml"),
            "version = 1\n\n[remote]\nurl = \"crab://bucket/repo\"\n\n[prefetch.profiles.ci]\npaths = [\"tests/**\"]\n",
        )
        .unwrap();

        let cancel = CancellationToken::new();
        auto_hydrate_always_profile(root, OutputMode::Text, &cancel).await;
    }

    #[tokio::test]
    async fn auto_hydrate_skips_when_always_profile_empty() {
        // When the `always` profile exists but has no patterns,
        // auto-hydrate should skip.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("crab.toml"),
            "version = 1\n\n[remote]\nurl = \"crab://bucket/repo\"\n\n[prefetch.profiles.always]\npaths = []\n",
        )
        .unwrap();

        let cancel = CancellationToken::new();
        auto_hydrate_always_profile(root, OutputMode::Text, &cancel).await;
    }
}
