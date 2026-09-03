//! `crab mirror` - mirror a Git remote into a Crab remote.
//!
//! The command intentionally shells out to Git for Git graph transfer and uses
//! Crab's existing LFS push path for object-store-backed LFS uploads.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use crab_cache::lifecycle::CacheUseGuard;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::OutputMode;
use crate::git::process::GIT_ENV_REMOVALS;
use crate::git::url::CrabUrl;

mod cache;
mod hook;
mod pre_push;
mod process;
mod reconcile;
mod types;

use cache::prepare_cache;
use process::SystemCommandRunner;

pub use hook::mirror_hook_status;
pub use pre_push::{MirrorPrePushArgs, run_mirror_pre_push};
pub use types::{
    MirrorApplySummary, MirrorCheckSummary, MirrorCommandOutcome, MirrorDriftState,
    MirrorHookState, MirrorHookStatus, MirrorPlanAction, MirrorPlanActionKind, MirrorPointerIssue,
    MirrorPointerState, MirrorPointerStatus, MirrorReconciliationPlan, MirrorRefState,
    MirrorRefStatus,
};

const CRAB_REMOTE: &str = "crab";
const ORIGIN_REMOTE: &str = "origin";
const REMOTE_HELPER: &str = "git-remote-crab";
const LFS_FETCH_REF_CHUNK_SIZE: usize = 128;

/// Arguments for `crab mirror`.
#[derive(Debug, Clone, Parser)]
pub struct MirrorArgs {
    /// Source Git remote URL or local path.
    pub source: String,

    /// Destination Crab remote URL.
    pub destination: String,

    /// Exact bare mirror cache directory to use.
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Push refs without Git's atomic push option.
    #[arg(long)]
    pub no_atomic: bool,

    /// Skip Git LFS object mirroring.
    #[arg(long)]
    pub skip_lfs: bool,

    /// Verify all LFS objects even when Git refs are already in sync.
    #[arg(long)]
    pub force_lfs_check: bool,

    /// Inspect ref drift and Crab pointer availability without publishing refs.
    #[arg(long)]
    pub check: bool,

    /// Write a create-once reconciliation plan from `--check`.
    #[arg(long, value_name = "PATH")]
    pub write_plan: Option<PathBuf>,

    /// Apply an immutable reconciliation plan after revalidating both remotes.
    #[arg(long, value_name = "PATH")]
    pub apply_plan: Option<PathBuf>,

    /// Approve deletion of destination-only refs in both plan and apply invocations.
    #[arg(long)]
    pub allow_delete_refs: bool,

    /// Return a failing exit status unless refs and pointer data are healthy.
    #[arg(long)]
    pub ci: bool,

    /// Structured JSON output (single envelope with terminal result).
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,

    /// Streaming JSONL output (one event per line).
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

impl MirrorArgs {
    /// Resolve the structured output mode for this invocation.
    #[must_use]
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, self.jsonl)
    }

    /// Whether this invocation uses the integrity/reconciliation contract.
    #[must_use]
    pub fn is_integrity_operation(&self) -> bool {
        self.check
            || self.apply_plan.is_some()
            || self.write_plan.is_some()
            || self.allow_delete_refs
            || self.ci
    }
}

/// Summary payload for `crab mirror --json` / `--jsonl`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MirrorSummary {
    /// Source Git remote URL or local path.
    pub source: String,
    /// Destination Crab remote URL.
    pub destination: String,
    /// Bare mirror cache directory used by this invocation.
    pub cache_dir: String,
    /// Whether the command created the mirror cache during this invocation.
    pub created_cache: bool,
    /// Whether LFS objects were fetched from source and uploaded to Crab.
    pub lfs_enabled: bool,
    /// Whether Git atomic push was requested.
    pub atomic: bool,
    /// Wall-clock duration of the operation in milliseconds.
    pub duration_ms: u64,
}

/// Run `crab mirror` from the current process environment.
pub fn run_mirror(args: &MirrorArgs, cancel: &CancellationToken) -> Result<MirrorSummary> {
    let mode = args.output_mode();
    let helper_path = helper_path_override();
    let crab_binary = crate::cmd::init::crab_binary_path();
    let options = MirrorExecution {
        mode,
        require_remote_helper: true,
        helper_path,
        crab_binary,
        lfs_object_id_collector: crate::cmd::lfs::push::collect_lfs_object_ids_from_range_in,
    };
    let mut runner = SystemCommandRunner::new(cancel.clone());
    run_mirror_with_runner(args, cancel, options, &mut runner)
}

/// Inspect mirror integrity or apply a prevalidated reconciliation plan.
pub async fn run_mirror_integrity(
    args: &MirrorArgs,
    cancel: &CancellationToken,
) -> Result<MirrorCommandOutcome> {
    let mode = args.output_mode();
    let options = MirrorExecution {
        mode,
        require_remote_helper: true,
        helper_path: helper_path_override(),
        crab_binary: crate::cmd::init::crab_binary_path(),
        lfs_object_id_collector: crate::cmd::lfs::push::collect_lfs_object_ids_from_range_in,
    };
    let mut runner = SystemCommandRunner::new(cancel.clone());
    let store = async {
        let destination = crate::git::url::CrabUrl::parse(&args.destination)?;
        let config = crate::core::config::Config::resolve_local()?;
        crate::auth::build_repository_url_store(&config, destination, "mirror-check", cancel).await
    }
    .await;
    reconcile::run_integrity_command(args, cancel, options, &mut runner, store).await
}

#[derive(Debug, Clone)]
struct MirrorExecution {
    mode: OutputMode,
    require_remote_helper: bool,
    helper_path: Option<OsString>,
    crab_binary: String,
    lfs_object_id_collector: LfsObjectIdCollector,
}

type LfsObjectIdCollector =
    fn(&Path, &[String], &[String], &CancellationToken) -> Result<Vec<String>>;

fn run_mirror_with_runner(
    args: &MirrorArgs,
    cancel: &CancellationToken,
    options: MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<MirrorSummary> {
    let start = Instant::now();
    let invocation_dir = env::current_dir()?;
    let mut args = args.clone();
    args.source = resolve_source(&args.source, &invocation_dir)?;
    let lfs_enabled = !args.skip_lfs;
    let atomic = !args.no_atomic;

    let _parsed = CrabUrl::parse(&args.destination)?;
    let cache_dir = resolve_cache_dir(&args, &invocation_dir);

    preflight(runner, &options, lfs_enabled)?;
    check_cancelled(cancel)?;

    // Hold the same ownership as inspection/apply until LFS and ref publication
    // finish; a refresh during either operation could change the selected refs.
    let cache = CacheUseGuard::acquire(&cache_dir, cancel)?;
    let cache_dir = cache.path();
    let created_cache = prepare_cache(&args, &cache, cancel, &options, runner)?;
    check_cancelled(cancel)?;

    ensure_crab_remote(cache_dir, &args.destination, &options, runner)?;
    check_cancelled(cancel)?;

    let ref_delta = load_ref_delta(cache_dir, &options, runner)?;
    check_cancelled(cancel)?;

    if ref_delta.is_empty() {
        if options.mode == OutputMode::Text {
            eprintln!("mirror: destination refs already match source");
        }
        if lfs_enabled && args.force_lfs_check {
            mirror_lfs_full(cache_dir, &options, runner)?;
        } else if lfs_enabled && options.mode == OutputMode::Text {
            eprintln!("mirror: skipping LFS scan because refs are unchanged");
        }
        check_cancelled(cancel)?;
        return finish_mirror_summary(
            &args,
            cache_dir,
            created_cache,
            lfs_enabled,
            atomic,
            start,
            options.mode,
        );
    }

    if lfs_enabled {
        mirror_lfs_incremental(cache_dir, &ref_delta, &options, runner, cancel)?;
        check_cancelled(cancel)?;
    }

    push_git_refs(cache_dir, atomic, &options, runner)?;

    finish_mirror_summary(
        &args,
        cache_dir,
        created_cache,
        lfs_enabled,
        atomic,
        start,
        options.mode,
    )
}

fn resolve_source(source: &str, invocation_dir: &Path) -> Result<String> {
    let path = Path::new(source);
    if path.is_absolute() {
        return Ok(source.to_owned());
    }

    let candidate = invocation_dir.join(path);
    if !candidate.exists() {
        return Ok(source.to_owned());
    }

    Ok(candidate.canonicalize()?.display().to_string())
}

fn finish_mirror_summary(
    args: &MirrorArgs,
    cache_dir: &Path,
    created_cache: bool,
    lfs_enabled: bool,
    atomic: bool,
    start: Instant,
    mode: OutputMode,
) -> Result<MirrorSummary> {
    let summary = MirrorSummary {
        source: args.source.clone(),
        destination: args.destination.clone(),
        cache_dir: cache_dir.display().to_string(),
        created_cache,
        lfs_enabled,
        atomic,
        duration_ms: start.elapsed().as_millis() as u64,
    };

    if mode == OutputMode::Text {
        eprintln!(
            "mirror: mirrored {} to {} using {}",
            summary.source, summary.destination, summary.cache_dir
        );
    }

    Ok(summary)
}

fn preflight(
    runner: &mut dyn CommandRunner,
    options: &MirrorExecution,
    lfs_enabled: bool,
) -> Result<()> {
    run_preflight(
        runner,
        git_command(["--version"], None, options, false),
        "git",
        "install Git and ensure it is on PATH",
        options.mode,
    )?;

    if lfs_enabled {
        run_preflight(
            runner,
            git_command(["lfs", "version"], None, options, false),
            "git-lfs",
            "install Git LFS or pass --skip-lfs",
            options.mode,
        )?;
    }

    if options.require_remote_helper && !remote_helper_available(options.helper_path.as_ref()) {
        return Err(CrabError::Configuration {
            key: format!("{REMOTE_HELPER} is not on PATH"),
            origin: "run `crab install`, `make install`, or place git-remote-crab beside crab"
                .to_owned(),
        });
    }

    Ok(())
}

fn run_preflight(
    runner: &mut dyn CommandRunner,
    command: ProcessCommand,
    key: &str,
    origin: &str,
    mode: OutputMode,
) -> Result<()> {
    match runner.run(&command, mode) {
        Ok(output) if output.status.success => Ok(()),
        Ok(output) => Err(CrabError::Configuration {
            key: format!("{key} preflight failed"),
            origin: command_failure_detail(&command, &output, origin),
        }),
        Err(CrabError::Io(source)) => Err(CrabError::Configuration {
            key: format!("{key} not found"),
            origin: format!("{origin}: {source}"),
        }),
        Err(error) => Err(error),
    }
}

fn ensure_crab_remote(
    cache_dir: &Path,
    destination: &str,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<()> {
    let get_url = runner.run(
        &git_command(
            ["remote", "get-url", CRAB_REMOTE],
            Some(cache_dir),
            options,
            false,
        ),
        options.mode,
    )?;

    let args = if get_url.status.success {
        ["remote", "set-url", CRAB_REMOTE, destination]
    } else {
        ["remote", "add", CRAB_REMOTE, destination]
    };

    run_required(
        runner,
        git_command(args, Some(cache_dir), options, true),
        options.mode,
    )?;

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefDelta {
    changed: Vec<ChangedRef>,
}

impl RefDelta {
    fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }

    fn lfs_ranges(&self) -> (Vec<String>, Vec<String>) {
        let mut local_shas = Vec::new();
        let mut remote_shas = Vec::new();
        for changed in &self.changed {
            if let Some(local) = &changed.local {
                local_shas.push(local.clone());
            }
            if let Some(remote) = &changed.remote {
                remote_shas.push(remote.clone());
            }
        }
        local_shas.sort();
        local_shas.dedup();
        remote_shas.sort();
        remote_shas.dedup();
        (local_shas, remote_shas)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangedRef {
    local: Option<String>,
    remote: Option<String>,
}

fn load_ref_delta(
    cache_dir: &Path,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<RefDelta> {
    let local = load_local_refs(cache_dir, options, runner)?;
    let remote = load_remote_refs(cache_dir, options, runner)?;
    Ok(diff_refs(&local, &remote))
}

fn load_local_refs(
    cache_dir: &Path,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<BTreeMap<String, String>> {
    let command = git_command(["show-ref"], Some(cache_dir), options, false);
    let output = runner.run(&command, options.mode)?;
    if !output.status.success && output.stdout.trim().is_empty() && output.stderr.trim().is_empty()
    {
        return Ok(BTreeMap::new());
    }
    if !output.status.success {
        return Err(CrabError::Protocol(command_failure_detail(
            &command,
            &output,
            "failed to read local mirror refs",
        )));
    }
    Ok(parse_ref_lines(&output.stdout, true))
}

fn load_remote_refs(
    cache_dir: &Path,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<BTreeMap<String, String>> {
    let command = git_command(
        ["ls-remote", "--refs", CRAB_REMOTE],
        Some(cache_dir),
        options,
        false,
    );
    let output = run_required(runner, command, options.mode)?;
    Ok(parse_ref_lines(&output.stdout, false))
}

fn parse_ref_lines(output: &str, filter_local_crab_tracking: bool) -> BTreeMap<String, String> {
    let mut refs = BTreeMap::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut parts = line.split_whitespace();
        let Some(sha) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if sha == "ref:" || name == "HEAD" || name.ends_with("^{}") {
            continue;
        }
        if filter_local_crab_tracking && name.starts_with("refs/remotes/crab/") {
            continue;
        }
        refs.insert(name.to_owned(), sha.to_owned());
    }
    refs
}

fn diff_refs(local: &BTreeMap<String, String>, remote: &BTreeMap<String, String>) -> RefDelta {
    let mut changed = Vec::new();
    for (name, local_sha) in local {
        match remote.get(name) {
            Some(remote_sha) if remote_sha == local_sha => {}
            remote_sha => changed.push(ChangedRef {
                local: Some(local_sha.clone()),
                remote: remote_sha.cloned(),
            }),
        }
    }
    for (name, remote_sha) in remote {
        if !local.contains_key(name) {
            changed.push(ChangedRef {
                local: None,
                remote: Some(remote_sha.clone()),
            });
        }
    }
    RefDelta { changed }
}

fn mirror_lfs_full(
    cache_dir: &Path,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<()> {
    if !lfs_pointers_present(cache_dir, options, runner)? {
        if options.mode == OutputMode::Text {
            eprintln!("mirror: no LFS objects found in source");
        }
        return Ok(());
    }

    if options.mode == OutputMode::Text {
        eprintln!("mirror: fetching LFS objects from source");
    }
    run_required(
        runner,
        git_command(
            ["lfs", "fetch", "--all", ORIGIN_REMOTE],
            Some(cache_dir),
            options,
            true,
        ),
        options.mode,
    )?;

    if options.mode == OutputMode::Text {
        eprintln!("mirror: uploading LFS objects to Crab");
    }
    let lfs_dir = cache_dir.join("lfs");
    run_required(
        runner,
        crab_command(
            &options.crab_binary,
            ["lfs", "push", "--all", CRAB_REMOTE],
            Some(cache_dir),
            [
                ("GIT_DIR", cache_dir.as_os_str().to_os_string()),
                ("GIT_LFS_DIR", lfs_dir.as_os_str().to_os_string()),
            ],
            true,
        ),
        options.mode,
    )?;

    Ok(())
}

fn mirror_lfs_incremental(
    cache_dir: &Path,
    ref_delta: &RefDelta,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
    cancel: &CancellationToken,
) -> Result<()> {
    let (local_shas, remote_shas) = ref_delta.lfs_ranges();
    if local_shas.is_empty() {
        if options.mode == OutputMode::Text {
            eprintln!("mirror: no new LFS objects to upload for ref deletions");
        }
        return Ok(());
    }

    if options.mode == OutputMode::Text {
        eprintln!("mirror: scanning changed Git objects for LFS pointers");
    }
    let object_ids =
        (options.lfs_object_id_collector)(cache_dir, &local_shas, &remote_shas, cancel)?;
    if object_ids.is_empty() {
        if options.mode == OutputMode::Text {
            eprintln!("mirror: no changed LFS objects found");
        }
        return Ok(());
    }

    if options.mode == OutputMode::Text {
        eprintln!("mirror: fetching changed LFS objects from source");
    }
    fetch_changed_lfs_objects(cache_dir, &local_shas, options, runner)?;

    if options.mode == OutputMode::Text {
        eprintln!(
            "mirror: uploading {} changed LFS object(s) to Crab",
            object_ids.len()
        );
    }
    let lfs_dir = cache_dir.join("lfs");
    run_required(
        runner,
        crab_command(
            &options.crab_binary,
            ["lfs", "push", CRAB_REMOTE, "--object-id", "--stdin"],
            Some(cache_dir),
            [
                ("GIT_DIR", cache_dir.as_os_str().to_os_string()),
                ("GIT_LFS_DIR", lfs_dir.as_os_str().to_os_string()),
            ],
            true,
        )
        .stdin(stdin_lines(&object_ids)),
        options.mode,
    )?;

    Ok(())
}

fn fetch_changed_lfs_objects(
    cache_dir: &Path,
    local_shas: &[String],
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<()> {
    for chunk in local_shas.chunks(LFS_FETCH_REF_CHUNK_SIZE) {
        let mut args = vec![
            "lfs".to_owned(),
            "fetch".to_owned(),
            "--all".to_owned(),
            ORIGIN_REMOTE.to_owned(),
        ];
        args.extend(chunk.iter().cloned());
        run_required(
            runner,
            git_command_from_vec(args, Some(cache_dir), options, true),
            options.mode,
        )?;
    }
    Ok(())
}

fn stdin_lines(lines: &[String]) -> String {
    let mut body = lines.join("\n");
    body.push('\n');
    body
}

fn lfs_pointers_present(
    cache_dir: &Path,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<bool> {
    let output = run_required(
        runner,
        git_command(
            ["lfs", "ls-files", "--all"],
            Some(cache_dir),
            options,
            false,
        ),
        options.mode,
    )?;
    Ok(!output.stdout.trim().is_empty())
}

fn push_git_refs(
    cache_dir: &Path,
    atomic: bool,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<()> {
    if options.mode == OutputMode::Text {
        eprintln!("mirror: pushing Git refs and objects to Crab");
    }

    let args = if atomic {
        vec![
            "push".to_owned(),
            "--mirror".to_owned(),
            "--atomic".to_owned(),
            CRAB_REMOTE.to_owned(),
        ]
    } else {
        vec![
            "push".to_owned(),
            "--mirror".to_owned(),
            CRAB_REMOTE.to_owned(),
        ]
    };

    let command = git_command_from_vec(args, Some(cache_dir), options, true).env(
        crate::git::push_native::MIRROR_GIT_ONLY_ENV,
        OsString::from("1"),
    );
    run_required(runner, command, options.mode)?;

    Ok(())
}

fn run_required(
    runner: &mut dyn CommandRunner,
    command: ProcessCommand,
    mode: OutputMode,
) -> Result<ProcessOutput> {
    let output = runner.run(&command, mode)?;
    if output.status.success {
        return Ok(output);
    }

    Err(CrabError::Protocol(command_failure_detail(
        &command,
        &output,
        "command failed",
    )))
}

fn command_failure_detail(
    command: &ProcessCommand,
    output: &ProcessOutput,
    fallback: &str,
) -> String {
    let detail = if !output.stderr.trim().is_empty() {
        output.stderr.trim()
    } else if !output.stdout.trim().is_empty() {
        output.stdout.trim()
    } else {
        fallback
    };
    format!(
        "{} exited with status {}: {detail}",
        command.display(),
        output.status.display()
    )
}

fn resolve_cache_dir(args: &MirrorArgs, invocation_dir: &Path) -> PathBuf {
    let path = args.cache_dir.clone().unwrap_or_else(|| {
        crate::cache::default_cache_root()
            .join("mirrors")
            .join(format!(
                "mirror-{}.git",
                mirror_cache_key(&args.source, &args.destination)
            ))
    });
    // Git initialization runs from the cache parent, not the invocation directory.
    // Resolve relative input first so a nested path is not applied twice.
    invocation_dir.join(path)
}

fn mirror_cache_key(source: &str, destination: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(source.as_bytes());
    hasher.update(&[0]);
    hasher.update(destination.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn git_command<'a, I>(
    args: I,
    current_dir: Option<&Path>,
    options: &MirrorExecution,
    replay_output: bool,
) -> ProcessCommand
where
    I: IntoIterator<Item = &'a str>,
{
    git_command_from_vec(
        args.into_iter().map(ToOwned::to_owned).collect(),
        current_dir,
        options,
        replay_output,
    )
}

fn git_command_from_vec(
    args: Vec<String>,
    current_dir: Option<&Path>,
    options: &MirrorExecution,
    replay_output: bool,
) -> ProcessCommand {
    let mut command = ProcessCommand::new("git", args)
        .current_dir(current_dir)
        .env_remove(GIT_ENV_REMOVALS)
        .replay_output(replay_output);
    if let Some(path) = &options.helper_path {
        command = command.env("PATH", path.clone());
    }
    command
}

fn crab_command<'a, I, E>(
    crab_binary: &str,
    args: I,
    current_dir: Option<&Path>,
    envs: E,
    replay_output: bool,
) -> ProcessCommand
where
    I: IntoIterator<Item = &'a str>,
    E: IntoIterator<Item = (&'static str, OsString)>,
{
    ProcessCommand::new(
        crab_binary,
        args.into_iter().map(ToOwned::to_owned).collect(),
    )
    .current_dir(current_dir)
    .env_remove(GIT_ENV_REMOVALS)
    .envs(envs)
    .replay_output(replay_output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessCommand {
    program: String,
    args: Vec<String>,
    current_dir: Option<PathBuf>,
    envs: Vec<(String, OsString)>,
    env_remove: Vec<String>,
    stdin: Option<String>,
    verify_blobs: Vec<crab_git::batch::BlobHeader>,
    replay_output: bool,
}

impl ProcessCommand {
    fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            current_dir: None,
            envs: Vec::new(),
            env_remove: Vec::new(),
            stdin: None,
            verify_blobs: Vec::new(),
            replay_output: false,
        }
    }

    fn current_dir(mut self, current_dir: Option<&Path>) -> Self {
        self.current_dir = current_dir.map(Path::to_path_buf);
        self
    }

    fn env(mut self, key: impl Into<String>, value: OsString) -> Self {
        self.envs.push((key.into(), value));
        self
    }

    fn envs<I>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (&'static str, OsString)>,
    {
        self.envs
            .extend(envs.into_iter().map(|(key, value)| (key.to_owned(), value)));
        self
    }

    fn env_remove(mut self, keys: &[&str]) -> Self {
        self.env_remove
            .extend(keys.iter().map(|key| (*key).to_owned()));
        self
    }

    fn stdin(mut self, stdin: String) -> Self {
        self.stdin = Some(stdin);
        self
    }

    fn verify_blobs(mut self, blobs: Vec<crab_git::batch::BlobHeader>) -> Self {
        self.verify_blobs = blobs;
        self
    }

    fn replay_output(mut self, replay_output: bool) -> Self {
        self.replay_output = replay_output;
        self
    }

    fn display(&self) -> String {
        let mut parts = Vec::with_capacity(self.args.len() + 1);
        parts.push(self.program.clone());
        parts.extend(self.args.clone());
        let command = parts.join(" ");
        if let Some(dir) = &self.current_dir {
            return format!("cd {} && {command}", dir.display());
        }
        command
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessStatus {
    success: bool,
    code: Option<i32>,
}

impl ProcessStatus {
    fn display(self) -> String {
        match self.code {
            Some(code) => code.to_string(),
            None => "terminated by signal".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessOutput {
    status: ProcessStatus,
    stdout: String,
    stderr: String,
}

trait CommandRunner {
    fn run(&mut self, command: &ProcessCommand, mode: OutputMode) -> Result<ProcessOutput>;
}

fn replay_stdout(output: &str) {
    if output.is_empty() {
        return;
    }
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(output.as_bytes());
    let _ = lock.flush();
}

fn replay_stderr(output: &str) {
    if output.is_empty() {
        return;
    }
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_all(output.as_bytes());
    let _ = lock.flush();
}

fn helper_path_override() -> Option<OsString> {
    let exe = env::current_exe().ok()?;
    let parent = exe.parent()?;
    if !executable_candidates(parent, REMOTE_HELPER)
        .iter()
        .any(|helper| is_executable_file(helper))
    {
        return None;
    }

    let current_path = env::var_os("PATH").unwrap_or_default();
    env::join_paths(std::iter::once(parent.to_path_buf()).chain(env::split_paths(&current_path)))
        .ok()
}

fn remote_helper_available(path_override: Option<&OsString>) -> bool {
    executable_in_path(REMOTE_HELPER, path_override)
}

fn executable_in_path(name: &str, path_override: Option<&OsString>) -> bool {
    let path = path_override
        .cloned()
        .or_else(|| env::var_os("PATH"))
        .unwrap_or_default();

    env::split_paths(&path)
        .flat_map(|dir| executable_candidates(&dir, name))
        .any(|candidate| is_executable_file(&candidate))
}

fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut candidates = vec![dir.join(name)];
        let lower_name = name.to_ascii_lowercase();
        let pathext = env::var_os("PATHEXT").unwrap_or_else(|| ".EXE;.BAT;.CMD".into());
        for ext in env::split_paths(&pathext)
            .filter_map(|path| path.into_os_string().into_string().ok())
            .filter(|ext| !ext.is_empty())
        {
            let lower_ext = ext.to_ascii_lowercase();
            if !lower_name.ends_with(&lower_ext) {
                candidates.push(dir.join(format!("{name}{ext}")));
            }
        }
        candidates
    }

    #[cfg(not(windows))]
    {
        vec![dir.join(name)]
    }
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests;
