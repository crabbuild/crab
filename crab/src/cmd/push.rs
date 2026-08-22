//! `crab push [<remote>] [<refspec>...]` — native concurrent push that
//! bypasses git's serial remote helper protocol.
//!
//! Accepts either a Git remote name or a direct `crab://` URL, opens the
//! staging area, resolves refspecs, and drives the push pipeline directly.
//! Produces identical remote state to `git push` via the remote helper,
//! just faster for multi-file pushes.

use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use rand::Rng as _;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::audit::default_log_path;
use crate::core::error::{CrabError, Result};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::core::perf_phase::PerfPhaseSink;
use crate::git::push::{
    PushConfig, PushRejectReason, PushResult, RefPushOutcome, acquire_push_lock_leases,
    configure_active_active_push_coordinator, record_push_audit_event, release_push_lock_leases,
};
use crate::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
use crate::git::push_state::PushState;
use crate::git::remote_helper::{AGENT_REBASE_FETCH_REF_FILTERING_ENV, PushSpec};
use crate::git::url::CrabUrl;
use crate::replication::StoreResolver;
use crate::storage::StoreLayout;
use crab_staging::StagingAreaReadOnly;

const INTEGRATION_RETRY_BACKOFF_BASE: Duration = Duration::from_millis(250);
const INTEGRATION_RETRY_BACKOFF_CAP: Duration = Duration::from_secs(3);
const AGENT_INTEGRATION_LOCK_WAIT: Duration = Duration::from_secs(300);
const DEFAULT_AGENT_REBASE_RETRY_LIMIT: u32 = 256;

/// Arguments for `crab push`.
#[derive(Debug, Clone, Parser)]
pub struct PushArgs {
    /// Remote name or URL (default: upstream remote or "origin").
    pub remote: Option<String>,

    /// Refspecs to push (default: current branch).
    pub refspecs: Vec<String>,

    /// Override the configured maximum concurrent xorb uploads.
    #[arg(long)]
    pub upload_concurrency: Option<usize>,

    /// Seconds to wait for contested push locks before failing.
    #[arg(long, value_name = "SECONDS")]
    pub lock_wait_secs: Option<u64>,

    /// Maximum manifest CAS retries before returning retryable stale-info.
    #[arg(long, value_name = "COUNT")]
    pub manifest_cas_retries: Option<u32>,

    /// Integrate the current branch and retry after non-fast-forward or lock contention.
    #[arg(long)]
    pub rebase_on_non_fast_forward: bool,

    /// Maximum integration retry attempts for --rebase-on-non-fast-forward.
    #[arg(long, default_value_t = DEFAULT_AGENT_REBASE_RETRY_LIMIT, requires = "rebase_on_non_fast_forward")]
    pub rebase_retry_limit: u32,

    /// Show what would be pushed without uploading.
    #[arg(long)]
    pub dry_run: bool,

    /// Bypass fast-forward checks.
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Push annotated tags that point into the history being pushed.
    #[arg(long)]
    pub follow_tags: bool,

    /// Show per-step timing and per-file progress.
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Force full graph walk, bypassing incremental optimization.
    #[arg(long)]
    pub no_incremental: bool,

    /// Disable colored output and ANSI escape codes.
    #[arg(long)]
    pub no_color: bool,

    /// Structured JSON output (single envelope with terminal result).
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,

    /// Streaming JSONL output (one event per line).
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

/// Summary payload for `--json` / `--jsonl` result events.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PushSummaryPayload {
    /// Number of refs pushed.
    pub refs_pushed: u64,
    /// Per-ref outcomes.
    pub refs: Vec<PushRefOutcome>,
    /// Wall-clock duration of the operation in milliseconds.
    pub duration_ms: u64,
    /// Remote URL that was pushed to.
    pub remote_url: String,
    /// Command-layer integration retries consumed before this terminal result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration_retries: Option<u32>,
    /// Configured retry budget for agent integration mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration_retry_limit: Option<u32>,
    /// Active-active operation id when the push is coordinator-backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Coordinator epoch that accepted the push.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_epoch: Option<u64>,
    /// Writer region used by active-active push ingress.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writer_region: Option<String>,
    /// Coordinator transaction state for active-active push.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_state: Option<String>,
}

/// Serializable per-ref outcome for structured output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PushRefOutcome {
    /// Source refspec (e.g. `refs/heads/main`).
    pub src: String,
    /// Destination refspec.
    pub dst: String,
    /// `"ok"` or `"error"`.
    pub status: String,
    /// Error message when status is `"error"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether retrying can reasonably succeed without user edits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// Suggested delay before retrying, in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

struct PushAttemptFailure {
    repo_root: PathBuf,
    remote_name: String,
    remote_url: String,
    specs: Vec<PushSpec>,
    result: PushResult,
    elapsed: Duration,
    integration: Option<PushIntegrationSummary>,
    agent_integration_lock: bool,
}

#[derive(Debug)]
struct PushTarget {
    remote: String,
    url: String,
    parsed_url: CrabUrl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfiguredRemote {
    name: String,
    url: String,
}

enum PushAttempt {
    Done(PushSummaryPayload),
    Failed(Box<PushAttemptFailure>),
}

#[derive(Clone, Copy)]
struct PushIntegrationSummary {
    retries: u32,
    retry_limit: u32,
}

/// Entry point for `crab push`, following the existing subcommand pattern.
///
/// Resolves the remote, validates the URL, opens staging, resolves refspecs,
/// and runs the push pipeline. Updates push state on success.
pub async fn run_push(args: &PushArgs, cancel: &CancellationToken) -> Result<()> {
    execute_push(args, cancel, true).await.map(|_| ())
}

/// Run push without emitting its terminal result envelope.
pub(crate) async fn run_push_without_terminal_output(
    args: &PushArgs,
    cancel: &CancellationToken,
) -> Result<PushSummaryPayload> {
    execute_push(args, cancel, false).await
}

async fn execute_push(
    args: &PushArgs,
    cancel: &CancellationToken,
    emit_terminal: bool,
) -> Result<PushSummaryPayload> {
    let mode = OutputMode::from_flags(args.json, args.jsonl);
    let mut retry_attempts = 0u32;

    loop {
        match run_push_once(args, cancel, retry_attempts, emit_terminal).await? {
            PushAttempt::Done(summary) => return Ok(summary),
            PushAttempt::Failed(mut failure) => {
                if args.rebase_on_non_fast_forward
                    && retry_attempts < args.rebase_retry_limit
                    && let Some(branch) = rebase_retry_branch(&failure.specs, &failure.result)
                    && current_branch().as_deref() == Some(branch)
                {
                    let branch = branch.to_owned();
                    retry_attempts += 1;
                    if !mode.is_machine() {
                        eprintln!(
                            "non-fast-forward; rebasing on {}/{} and retrying ({}/{})...",
                            failure.remote_name, branch, retry_attempts, args.rebase_retry_limit
                        );
                    }
                    rebase_for_integration(
                        &mut failure,
                        &branch,
                        retry_attempts,
                        mode,
                        emit_terminal,
                    )?;
                    tokio::time::sleep(integration_retry_delay(retry_attempts)).await;
                    continue;
                }
                if args.rebase_on_non_fast_forward
                    && retry_attempts < args.rebase_retry_limit
                    && let Some(branch) = lock_retry_branch(&failure.specs, &failure.result)
                    && current_branch().as_deref() == Some(branch)
                {
                    let branch = branch.to_owned();
                    retry_attempts += 1;
                    if !mode.is_machine() {
                        let action = if failure.agent_integration_lock {
                            "waiting before retry"
                        } else {
                            "rebasing before retry"
                        };
                        eprintln!(
                            "push lock busy for {}; {action} ({}/{})...",
                            branch, retry_attempts, args.rebase_retry_limit
                        );
                    }
                    if !failure.agent_integration_lock {
                        rebase_for_integration(
                            &mut failure,
                            &branch,
                            retry_attempts,
                            mode,
                            emit_terminal,
                        )?;
                    }
                    tokio::time::sleep(integration_retry_delay(retry_attempts)).await;
                    continue;
                }

                if emit_terminal || mode == OutputMode::Text {
                    emit_push_failure(&failure, mode);
                }
                let source = push_failure_source(&failure.specs, &failure.result);
                return Err(CrabError::PushPartialOutcome {
                    outcomes: Box::new(failure.result),
                    source: Box::new(source),
                });
            }
        }
    }
}

fn push_failure_source(specs: &[PushSpec], result: &PushResult) -> CrabError {
    for spec in specs {
        let Some(outcome) = result.outcomes.get(&spec.dst) else {
            continue;
        };
        #[expect(
            deprecated,
            reason = "preserves the deprecated outcome until its callers are migrated"
        )]
        match outcome {
            RefPushOutcome::Ok => {}
            RefPushOutcome::Error(message) => return CrabError::Internal(message.clone()),
            RefPushOutcome::Rejected(PushRejectReason::NonFastForward { have, want }) => {
                return CrabError::NonFastForward {
                    ref_name: spec.dst.clone(),
                    have: have.clone(),
                    want: want.clone(),
                };
            }
            RefPushOutcome::Rejected(PushRejectReason::StaleInfo) => {
                return CrabError::CasConflict {
                    path: spec.dst.clone(),
                    expected_etag: None,
                };
            }
            RefPushOutcome::Rejected(PushRejectReason::IntegrationFailed { command, message }) => {
                return CrabError::PushIntegrationFailed {
                    command: command.clone(),
                    message: message.clone(),
                };
            }
            RefPushOutcome::Rejected(PushRejectReason::NetworkTransient(message)) => {
                return CrabError::NetworkTransient(object_store::Error::Generic {
                    store: "push",
                    source: Box::new(std::io::Error::other(message.clone())),
                });
            }
            RefPushOutcome::Rejected(PushRejectReason::Throttled { retry_after_secs }) => {
                return CrabError::Throttled {
                    retry_after: retry_after_secs.map(std::time::Duration::from_secs),
                };
            }
            RefPushOutcome::Rejected(reason) => {
                return CrabError::Internal(reason.to_string());
            }
        }
    }
    CrabError::Internal("push failed without a per-ref failure outcome".to_owned())
}

/// Push explicit refspecs without emitting command output.
///
/// Used by recovery flows after they have staged verified replacement
/// bytes. The normal push pipeline still owns every remote mutation:
/// xorb uploads, shard/index writes, manifest CAS, ref CAS, push-state
/// update, and audit logging.
pub(crate) async fn run_push_repair_refspecs(
    remote: Option<&str>,
    refspecs: &[String],
    cancel: &CancellationToken,
) -> Result<PushSummaryPayload> {
    let start = Instant::now();
    let repo_root = resolve_push_repo_root()?;
    let target = resolve_push_target(remote)?;
    let remote_name = target.remote;
    let remote_url = target.url;
    let parsed_url = target.parsed_url;
    let config = crate::core::config::Config::resolve_local()?;
    let staging = open_optional_staging_for_push(&repo_root).await;
    let specs = resolve_push_specs(refspecs, &remote_name, false)?;
    if specs.is_empty() {
        return Ok(PushSummaryPayload {
            refs_pushed: 0,
            refs: Vec::new(),
            duration_ms: start.elapsed().as_millis() as u64,
            remote_url,
            integration_retries: None,
            integration_retry_limit: None,
            operation_id: None,
            coordinator_epoch: None,
            writer_region: None,
            commit_state: None,
        });
    }

    let mut push_state = PushState::load(&repo_root);
    let mut push_config = PushConfig::from_config(&config);
    configure_active_active_push_coordinator(
        &config,
        Some(&remote_url),
        &parsed_url.repo_path,
        &mut push_config,
    )
    .await?;

    let (store, router) = if matches!(
        config.auth.provider,
        crate::core::config::AuthProvider::CrabAuth
    ) {
        let protected = crate::git::protected_push::prepare_crab_auth_push(
            &config,
            &parsed_url,
            &specs,
            cancel,
        )
        .await?;
        push_config.atomic = true;
        push_config.protected_push = Some(protected.session);
        let store = protected.store;
        let router = StoreLayout::new(store.clone(), parsed_url.repo_path.clone());
        (store, router)
    } else {
        let selection = StoreResolver::new(&config, &parsed_url, cancel)
            .write_store("recover.repair_remote")
            .await?;
        (selection.store, selection.router)
    };
    let repo_prefix = router.repo_prefix().to_owned();
    let caching_store = crab_cache_store::CachingStore::try_build_healthy(
        store.as_storage().clone(),
        &config.cache,
    )
    .await;

    let mut native_config = NativePushConfig::new(push_config);
    native_config.progress = false;
    native_config.color = false;
    let result = run_native_push(
        &native_config,
        &specs,
        NativePushInputs::new(
            Some(store),
            caching_store,
            staging,
            router,
            &mut push_state,
            &remote_name,
            &remote_url,
            None,
            cancel.clone(),
        ),
    )
    .await?;

    if let Err(err) = record_push_audit_event(
        &repo_root.join(default_log_path()),
        Some(&remote_url),
        &repo_prefix,
        &specs,
        &result,
        Some(start.elapsed().as_millis() as u64),
    ) {
        warn!(%err, "failed to append recovery push audit event");
    }

    if !result.all_ok() {
        return Err(CrabError::Internal(
            "remote recovery push failed for one or more refs".to_owned(),
        ));
    }

    for spec in &specs {
        if spec.src.is_empty() {
            continue;
        }
        if let Some(sha) = resolve_rev(&spec.src) {
            push_state.set(&remote_url, &spec.dst, &sha);
        }
    }
    push_state.save(&repo_root)?;

    Ok(build_push_summary(
        &specs,
        &result,
        &remote_url,
        start.elapsed(),
        None,
    ))
}

async fn run_push_once(
    args: &PushArgs,
    cancel: &CancellationToken,
    integration_retries: u32,
    emit_terminal: bool,
) -> Result<PushAttempt> {
    let start = Instant::now();
    let mode = OutputMode::from_flags(args.json, args.jsonl);

    // Discover the shared Crab state root. The current worktree root is
    // where the user's files are; the main worktree root owns `.crab/`.
    let repo_root = resolve_push_repo_root()?;

    let target = resolve_push_target(args.remote.as_deref())?;
    let remote_name = target.remote;
    let remote_url = target.url;
    let parsed_url = target.parsed_url;
    debug!(remote = %remote_name, url = %remote_url, "resolved push target");
    debug!(
        bucket = %parsed_url.bucket,
        prefix = %parsed_url.repo_path,
        "parsed crab URL"
    );

    // Resolve config early — needed for both store construction and push config.
    let config = crate::core::config::Config::resolve_local()?;

    // Open staging area (read-only for push).
    //
    // Uses the blocking variant so a concurrent clean filter session
    // (e.g. `git status` running in parallel) queues the push instead
    // of failing it — without this, an IDE that polls `git status`
    // while you push would race and sometimes kill the push with
    // E0081. Step 2 (`lookup_staging`) still refuses if a timeout
    // ultimately happens with pointers to upload.
    let staging = open_optional_staging_for_push(&repo_root).await;

    // Resolve refspecs.
    let specs = resolve_push_specs(&args.refspecs, &remote_name, args.force)?;
    if specs.is_empty() {
        if mode == OutputMode::Text {
            println!("Everything up-to-date");
        }
        let integration = push_integration_summary(args, integration_retries);
        let summary = PushSummaryPayload {
            refs_pushed: 0,
            refs: vec![],
            duration_ms: start.elapsed().as_millis() as u64,
            remote_url: remote_url.clone(),
            integration_retries: integration.map(|summary| summary.retries),
            integration_retry_limit: integration.map(|summary| summary.retry_limit),
            operation_id: None,
            coordinator_epoch: None,
            writer_region: None,
            commit_state: None,
        };
        if emit_terminal && mode != OutputMode::Text {
            match mode {
                OutputMode::Json => emit_json("push", "1.0", &summary),
                OutputMode::Jsonl => {
                    let mut stream = JsonlStream::new("push.event", "1.0", std::io::stdout());
                    stream.emit_result(&summary);
                }
                OutputMode::Text => unreachable!(),
            }
        }
        return Ok(PushAttempt::Done(summary));
    }

    info!(
        remote = %remote_name,
        specs = specs.len(),
        "starting push"
    );

    // Load push state for incremental walk.
    let mut push_state = PushState::load(&repo_root);

    // Dry-run: print what would be pushed and return.
    if args.dry_run {
        print_dry_run(&remote_name, &remote_url, &specs);
        let integration = push_integration_summary(args, integration_retries);
        return Ok(PushAttempt::Done(PushSummaryPayload {
            refs_pushed: 0,
            refs: Vec::new(),
            duration_ms: start.elapsed().as_millis() as u64,
            remote_url,
            integration_retries: integration.map(|summary| summary.retries),
            integration_retry_limit: integration.map(|summary| summary.retry_limit),
            operation_id: None,
            coordinator_epoch: None,
            writer_region: None,
            commit_state: None,
        }));
    }

    // Build push config from resolved Config + CLI overrides.
    let mut push_config = PushConfig::from_config(&config);
    apply_push_cli_overrides(args, &mut push_config);
    configure_active_active_push_coordinator(
        &config,
        Some(&remote_url),
        &parsed_url.repo_path,
        &mut push_config,
    )
    .await?;
    let (store, router) = if matches!(
        config.auth.provider,
        crate::core::config::AuthProvider::CrabAuth
    ) {
        let protected = crate::git::protected_push::prepare_crab_auth_push(
            &config,
            &parsed_url,
            &specs,
            cancel,
        )
        .await?;
        push_config.atomic = true;
        push_config.protected_push = Some(protected.session);
        let store = protected.store;
        let router = StoreLayout::new(store.clone(), parsed_url.repo_path.clone());
        (store, router)
    } else {
        let selection = StoreResolver::new(&config, &parsed_url, cancel)
            .write_store("push")
            .await?;
        (selection.store, selection.router)
    };
    let repo_prefix = router.repo_prefix().to_owned();

    // Build CachingStore when a cache service is configured and healthy.
    let caching_store = crab_cache_store::CachingStore::try_build_healthy(
        store.as_storage().clone(),
        &config.cache,
    )
    .await;

    // Build the optional JSONL stream for streaming mode.
    let jsonl_stream: Option<Arc<Mutex<JsonlStream<Stdout>>>> = match mode {
        OutputMode::Jsonl if emit_terminal => Some(Arc::new(Mutex::new(JsonlStream::new(
            "push.event",
            "1.0",
            std::io::stdout(),
        )))),
        _ => None,
    };
    if let Some(stream) = &jsonl_stream {
        push_config.perf_phase_sink = Some(PerfPhaseSink::Stdout(Arc::clone(stream)));
    }

    let mut pre_acquired_locks = None;
    if push_config.protected_push.is_none()
        && let Some(branch) = agent_integration_lock_branch(args, &specs)
        && current_branch().as_deref() == Some(branch)
    {
        let branch = branch.to_owned();
        match acquire_push_lock_leases(&store, router.repo_prefix(), &specs, &push_config, cancel)
            .await
        {
            Ok(leases) => {
                let integration_error =
                    match remote_branch_exists(&repo_root, &remote_name, &branch) {
                        Ok(true) => run_git_pull_rebase(&repo_root, &remote_name, &branch)
                            .err()
                            .map(|message| (integration_command(&remote_name, &branch), message)),
                        Ok(false) => None,
                        Err(message) => {
                            Some((remote_branch_probe_command(&remote_name, &branch), message))
                        }
                    };

                if let Some((command, message)) = integration_error {
                    release_push_lock_leases(leases).await;
                    let result = push_result_from_reason(
                        &specs,
                        PushRejectReason::IntegrationFailed {
                            command: command.clone(),
                            message: message.clone(),
                        },
                    );
                    if let Err(err) = record_push_audit_event(
                        &repo_root.join(default_log_path()),
                        Some(&remote_url),
                        &repo_prefix,
                        &specs,
                        &result,
                        Some(start.elapsed().as_millis() as u64),
                    ) {
                        warn!(%err, "failed to append push audit event");
                    }
                    let failure = PushAttemptFailure {
                        repo_root,
                        remote_name,
                        remote_url,
                        specs: specs.clone(),
                        result,
                        elapsed: start.elapsed(),
                        integration: push_integration_summary(args, integration_retries),
                        agent_integration_lock: true,
                    };
                    if emit_terminal || mode == OutputMode::Text {
                        emit_push_failure(&failure, mode);
                    }
                    return Err(CrabError::PushIntegrationFailed { command, message });
                }

                pre_acquired_locks = Some(leases);
            }
            Err(e) => {
                let result = push_result_from_error(&specs, &e);
                if let Err(err) = record_push_audit_event(
                    &repo_root.join(default_log_path()),
                    Some(&remote_url),
                    &repo_prefix,
                    &specs,
                    &result,
                    Some(start.elapsed().as_millis() as u64),
                ) {
                    warn!(%err, "failed to append push audit event");
                }
                return Ok(PushAttempt::Failed(Box::new(PushAttemptFailure {
                    repo_root,
                    remote_name,
                    remote_url,
                    specs: specs.clone(),
                    result,
                    elapsed: start.elapsed(),
                    integration: push_integration_summary(args, integration_retries),
                    agent_integration_lock: true,
                })));
            }
        }
    }

    let mut native_config = NativePushConfig::new(push_config);
    native_config.incremental = !args.no_incremental;
    native_config.color = !args.no_color && crate::git::progress::is_tty();
    native_config.verbose = args.verbose;
    native_config.progress = mode == OutputMode::Text;
    native_config.followtags = args.follow_tags;

    let result: PushResult = run_native_push(
        &native_config,
        &specs,
        NativePushInputs::new(
            Some(store),
            caching_store,
            staging,
            router,
            &mut push_state,
            &remote_name,
            &remote_url,
            None,
            cancel.clone(),
        )
        .with_pre_acquired_locks(pre_acquired_locks),
    )
    .await?;

    if result.all_ok() {
        if let Err(err) = record_push_audit_event(
            &repo_root.join(default_log_path()),
            Some(&remote_url),
            &repo_prefix,
            &specs,
            &result,
            Some(start.elapsed().as_millis() as u64),
        ) {
            warn!(%err, "failed to append push audit event");
        }
        push_state.save(&repo_root)?;

        let elapsed = start.elapsed();
        let integration = push_integration_summary(args, integration_retries);

        let summary = build_push_summary(&specs, &result, &remote_url, elapsed, integration);
        match mode {
            OutputMode::Text => {
                print_push_summary(&remote_name, &remote_url, &specs, &result, elapsed);
            }
            OutputMode::Json => {
                if emit_terminal {
                    emit_json("push", "1.0", &summary);
                }
            }
            OutputMode::Jsonl => {
                if emit_terminal
                    && let Some(ref stream) = jsonl_stream
                    && let Ok(mut s) = stream.lock()
                {
                    s.emit_result(&summary);
                }
            }
        }
        return Ok(PushAttempt::Done(summary));
    }

    let elapsed = start.elapsed();
    if let Err(err) = record_push_audit_event(
        &repo_root.join(default_log_path()),
        Some(&remote_url),
        &repo_prefix,
        &specs,
        &result,
        Some(elapsed.as_millis() as u64),
    ) {
        warn!(%err, "failed to append push audit event");
    }
    Ok(PushAttempt::Failed(Box::new(PushAttemptFailure {
        repo_root,
        remote_name,
        remote_url,
        specs,
        result,
        elapsed,
        integration: push_integration_summary(args, integration_retries),
        agent_integration_lock: false,
    })))
}

fn emit_push_failure(failure: &PushAttemptFailure, mode: OutputMode) {
    match mode {
        OutputMode::Text => {
            for (ref_name, outcome) in &failure.result.outcomes {
                if !matches!(outcome, RefPushOutcome::Ok) {
                    eprintln!("error: {ref_name}: {outcome}");
                }
            }
        }
        OutputMode::Json => {
            let summary = build_push_summary(
                &failure.specs,
                &failure.result,
                &failure.remote_url,
                failure.elapsed,
                failure.integration,
            );
            emit_json("push", "1.0", &summary);
        }
        OutputMode::Jsonl => {
            let summary = build_push_summary(
                &failure.specs,
                &failure.result,
                &failure.remote_url,
                failure.elapsed,
                failure.integration,
            );
            let mut stream = JsonlStream::new("push.event", "1.0", std::io::stdout());
            stream.emit_result(&summary);
        }
    }
}

fn push_integration_summary(args: &PushArgs, retries: u32) -> Option<PushIntegrationSummary> {
    args.rebase_on_non_fast_forward
        .then_some(PushIntegrationSummary {
            retries,
            retry_limit: args.rebase_retry_limit,
        })
}

fn current_head_push_branch(specs: &[PushSpec]) -> Option<&str> {
    let [spec] = specs else {
        return None;
    };
    let branch = spec.dst.strip_prefix("refs/heads/")?;
    let pushes_current_head = spec.src == "HEAD" || spec.src == spec.dst || spec.src == branch;
    if spec.force || !pushes_current_head {
        return None;
    }

    Some(branch)
}

fn agent_integration_lock_branch<'a>(args: &PushArgs, specs: &'a [PushSpec]) -> Option<&'a str> {
    args.rebase_on_non_fast_forward
        .then(|| current_head_push_branch(specs))
        .flatten()
}

fn rebase_retry_branch<'a>(specs: &'a [PushSpec], result: &PushResult) -> Option<&'a str> {
    let branch = current_head_push_branch(specs)?;
    let [spec] = specs else {
        return None;
    };
    let outcome = result.outcomes.get(&spec.dst)?;
    if !matches!(
        outcome,
        RefPushOutcome::Rejected(PushRejectReason::NonFastForward { .. })
    ) {
        return None;
    }

    Some(branch)
}

fn lock_retry_branch<'a>(specs: &'a [PushSpec], result: &PushResult) -> Option<&'a str> {
    let branch = current_head_push_branch(specs)?;
    let [spec] = specs else {
        return None;
    };
    let outcome = result.outcomes.get(&spec.dst)?;
    if !matches!(
        outcome,
        RefPushOutcome::Rejected(PushRejectReason::LockContention { .. })
    ) {
        return None;
    }

    Some(branch)
}

fn push_result_from_error(specs: &[PushSpec], error: &CrabError) -> PushResult {
    push_result_from_reason(specs, PushRejectReason::from_error(error))
}

fn push_result_from_reason(specs: &[PushSpec], reason: PushRejectReason) -> PushResult {
    let outcomes = specs
        .iter()
        .map(|spec| (spec.dst.clone(), RefPushOutcome::Rejected(reason.clone())))
        .collect();
    PushResult::new(outcomes)
}

fn integration_retry_delay(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1);
    let shift = 1u32.checked_shl(exponent).unwrap_or(u32::MAX);
    let bound = INTEGRATION_RETRY_BACKOFF_BASE
        .saturating_mul(shift)
        .min(INTEGRATION_RETRY_BACKOFF_CAP);
    let bound_nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
    if bound_nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(rand::rng().random_range(1..=bound_nanos))
}

fn apply_push_cli_overrides(args: &PushArgs, push_config: &mut PushConfig) {
    if let Some(upload_concurrency) = args.upload_concurrency {
        push_config.upload_concurrency = effective_push_upload_concurrency(upload_concurrency);
    }
    if let Some(lock_wait_secs) = args.lock_wait_secs {
        push_config.lock_wait = Duration::from_secs(lock_wait_secs);
    } else if args.rebase_on_non_fast_forward && push_config.lock_wait.is_zero() {
        // Agent integration mode expects many clients to lose the first lock
        // race. Wait inside the push pipeline before retrying the whole command.
        push_config.lock_wait = AGENT_INTEGRATION_LOCK_WAIT;
    }
    if let Some(manifest_cas_retries) = args.manifest_cas_retries {
        push_config.max_cas_retries = manifest_cas_retries;
    }
}

fn effective_push_upload_concurrency(upload_concurrency: usize) -> usize {
    upload_concurrency.max(1)
}

fn run_git_pull_rebase(
    repo_root: &Path,
    remote: &str,
    branch: &str,
) -> std::result::Result<(), String> {
    let output = Command::new("git")
        .args(["pull", "--rebase", "--autostash", remote, branch])
        .current_dir(repo_root)
        .env(AGENT_REBASE_FETCH_REF_FILTERING_ENV, "1")
        .output()
        .map_err(|e| format!("failed to spawn git pull --rebase: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    Err(git_command_diagnostics(&output.stdout, &output.stderr))
}

fn remote_branch_exists(
    repo_root: &Path,
    remote: &str,
    branch: &str,
) -> std::result::Result<bool, String> {
    let ref_name = format!("refs/heads/{branch}");
    let output = Command::new("git")
        .args(["ls-remote", "--exit-code", remote, &ref_name])
        .current_dir(repo_root)
        .env(AGENT_REBASE_FETCH_REF_FILTERING_ENV, "1")
        .output()
        .map_err(|e| format!("failed to spawn git ls-remote: {e}"))?;

    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(2) {
        return Ok(false);
    }

    Err(git_command_diagnostics(&output.stdout, &output.stderr))
}

fn rebase_for_integration(
    failure: &mut PushAttemptFailure,
    branch: &str,
    retries: u32,
    mode: OutputMode,
    emit_terminal: bool,
) -> Result<()> {
    if let Err(message) = run_git_pull_rebase(&failure.repo_root, &failure.remote_name, branch) {
        let command = integration_command(&failure.remote_name, branch);
        mark_integration_failed(failure, command.clone(), message.clone(), retries);
        if emit_terminal || mode == OutputMode::Text {
            emit_push_failure(failure, mode);
        }
        return Err(CrabError::PushIntegrationFailed { command, message });
    }

    Ok(())
}

fn mark_integration_failed(
    failure: &mut PushAttemptFailure,
    command: String,
    message: String,
    retries: u32,
) {
    let Some(spec) = failure.specs.first() else {
        return;
    };
    failure.result.outcomes.insert(
        spec.dst.clone(),
        RefPushOutcome::Rejected(PushRejectReason::IntegrationFailed { command, message }),
    );
    if let Some(integration) = failure.integration.as_mut() {
        integration.retries = retries;
    }
}

fn integration_command(remote: &str, branch: &str) -> String {
    format!("git pull --rebase --autostash {remote} {branch}")
}

fn remote_branch_probe_command(remote: &str, branch: &str) -> String {
    format!("git ls-remote --exit-code {remote} refs/heads/{branch}")
}

fn git_command_diagnostics(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    let mut parts = Vec::new();
    if !stderr.is_empty() {
        parts.push(stderr);
    }
    if !stdout.is_empty() {
        parts.push(stdout);
    }
    if parts.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        parts.join("\n")
    }
}

async fn open_optional_staging_for_push(repo_root: &Path) -> Option<Arc<StagingAreaReadOnly>> {
    let staging_root = repo_root.join(".crab").join("staging");
    if !staging_root.exists() {
        return None;
    }

    match StagingAreaReadOnly::open_blocking_default(staging_root).await {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            warn!(
                error = %e,
                "staging area unavailable; push will only succeed if this ref \
                 introduces no new pointer blobs. Resolve the lock holder to \
                 push new large-file content."
            );
            None
        }
    }
}

fn resolve_push_repo_root() -> Result<PathBuf> {
    if let Ok(ctx) = crate::git::worktree::WorktreeContext::resolve() {
        return Ok(ctx.main_worktree_root);
    }

    crate::git::discover::resolve_main_worktree_root()
        .ok_or_else(|| CrabError::Internal("could not resolve main worktree root".into()))
}

/// Build a structured push summary from the result and specs.
fn build_push_summary(
    specs: &[PushSpec],
    result: &PushResult,
    remote_url: &str,
    elapsed: std::time::Duration,
    integration: Option<PushIntegrationSummary>,
) -> PushSummaryPayload {
    let refs: Vec<PushRefOutcome> = specs
        .iter()
        .map(|spec| {
            let outcome = result.outcomes.get(&spec.dst);
            #[allow(
                deprecated,
                reason = "pattern-matches the deprecated Error variant for backward compat"
            )]
            let (status, error, retryable, retry_after_secs) = match outcome {
                Some(crate::git::push::RefPushOutcome::Ok) | None => {
                    ("ok".to_owned(), None, None, None)
                }
                Some(crate::git::push::RefPushOutcome::Error(msg)) => {
                    ("error".to_owned(), Some(msg.clone()), Some(false), None)
                }
                Some(crate::git::push::RefPushOutcome::Rejected(reason)) => {
                    // Structured reject reasons expose a stable tag
                    // first; the human-readable detail is still
                    // available via the reason's Display impl and lands
                    // in `error` for the JSON envelope.
                    (
                        reason.protocol_tag().to_owned(),
                        Some(reason.to_string()),
                        Some(reason.is_retryable()),
                        reason.retry_after_secs(),
                    )
                }
            };
            PushRefOutcome {
                src: spec.src.clone(),
                dst: spec.dst.clone(),
                status,
                error,
                retryable,
                retry_after_secs,
            }
        })
        .collect();

    let refs_pushed = refs.iter().filter(|r| r.status == "ok").count() as u64;

    PushSummaryPayload {
        refs_pushed,
        refs,
        duration_ms: elapsed.as_millis() as u64,
        remote_url: remote_url.to_owned(),
        integration_retries: integration.map(|summary| summary.retries),
        integration_retry_limit: integration.map(|summary| summary.retry_limit),
        operation_id: result
            .active_active_commit
            .as_ref()
            .map(|commit| commit.operation_id.clone()),
        coordinator_epoch: result
            .active_active_commit
            .as_ref()
            .map(|commit| commit.coordinator_epoch),
        writer_region: result
            .active_active_commit
            .as_ref()
            .map(|commit| commit.writer_region.clone()),
        commit_state: result
            .active_active_commit
            .as_ref()
            .map(|commit| commit.commit_state_name().to_owned()),
    }
}

/// Resolve the remote name from the user argument or Git configuration.
///
/// When no remote is specified, a Crab-compatible upstream is preferred,
/// followed by a remote named `crab` or the only other Crab-compatible
/// remote. Non-Crab upstream/origin fallback is retained so the final target
/// validation can provide the existing actionable error.
pub(crate) fn resolve_remote_name(explicit: Option<&str>) -> Result<String> {
    if let Some(name) = explicit {
        return Ok(name.to_owned());
    }

    let branch_remote =
        current_branch().and_then(|branch| git_config_value(&format!("branch.{branch}.remote")));
    let configured_remotes = configured_git_remotes();
    if let Some(remote) = select_default_crab_remote(branch_remote.as_deref(), &configured_remotes)?
    {
        return Ok(remote);
    }

    Ok(branch_remote.unwrap_or_else(|| "origin".to_owned()))
}

/// Resolve and validate the target used by a higher-level command before it
/// performs local mutations such as staging or committing.
pub(crate) fn resolve_push_remote(explicit: Option<&str>) -> Result<String> {
    Ok(resolve_push_target(explicit)?.remote)
}

fn configured_git_remotes() -> Vec<ConfiguredRemote> {
    let output = match Command::new("git")
        .args(["remote"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let name = line.trim();
            if name.is_empty() {
                return None;
            }
            let url = git_config_value(&format!("remote.{name}.url"))?;
            Some(ConfiguredRemote {
                name: name.to_owned(),
                url,
            })
        })
        .collect()
}

fn select_default_crab_remote(
    branch_remote: Option<&str>,
    configured_remotes: &[ConfiguredRemote],
) -> Result<Option<String>> {
    let crab_remotes: Vec<&ConfiguredRemote> = configured_remotes
        .iter()
        .filter(|remote| remote.url.starts_with("crab://"))
        .collect();

    if let Some(branch_remote) = branch_remote
        && crab_remotes
            .iter()
            .any(|remote| remote.name == branch_remote)
    {
        return Ok(Some(branch_remote.to_owned()));
    }

    if let Some(remote) = crab_remotes.iter().find(|remote| remote.name == "crab") {
        return Ok(Some(remote.name.clone()));
    }

    match crab_remotes.as_slice() {
        [] => Ok(None),
        [remote] => Ok(Some(remote.name.clone())),
        _ => {
            let names = crab_remotes
                .iter()
                .map(|remote| format!("'{}'", remote.name))
                .collect::<Vec<_>>()
                .join(", ");
            Err(CrabError::Configuration {
                key: format!(
                    "multiple Crab remotes detected ({names}); choose one with `--remote <name>`"
                ),
                origin: "git remotes".into(),
            })
        }
    }
}

fn resolve_push_target(explicit: Option<&str>) -> Result<PushTarget> {
    let remote = resolve_remote_name(explicit)?;
    if remote.contains("://") {
        let parsed_url = CrabUrl::parse(&remote)?;
        return Ok(PushTarget {
            url: remote.clone(),
            remote,
            parsed_url,
        });
    }

    let url = git_config_value(&format!("remote.{remote}.url")).ok_or_else(|| {
        CrabError::Configuration {
            key: format!("remote.{remote}.url"),
            origin: "git config".into(),
        }
    })?;
    if !url.starts_with("crab://") {
        return Err(CrabError::Configuration {
            key: format!(
                "remote '{remote}' is not a crab remote (url: {url}). Use 'git push' instead."
            ),
            origin: url,
        });
    }
    let parsed_url = CrabUrl::parse(&url)?;
    Ok(PushTarget {
        remote,
        url,
        parsed_url,
    })
}

/// Resolve push refspecs from user arguments or git config defaults.
///
/// When no refspecs are given, pushes the current branch to its upstream
/// tracking ref (from `git config branch.<name>.merge`).
fn resolve_push_specs(refspecs: &[String], remote: &str, force: bool) -> Result<Vec<PushSpec>> {
    if !refspecs.is_empty() {
        return refspecs.iter().map(|rs| parse_refspec(rs, force)).collect();
    }

    // Default: current branch → upstream tracking ref.
    let branch = current_branch().ok_or_else(|| CrabError::Configuration {
        key: "cannot determine current branch (detached HEAD?)".into(),
        origin: "git symbolic-ref HEAD".into(),
    })?;

    // Try to get the upstream merge ref (e.g. refs/heads/main).
    let dst = git_config_value(&format!("branch.{branch}.merge"))
        .unwrap_or_else(|| format!("refs/heads/{branch}"));

    let src = format!("refs/heads/{branch}");

    debug!(src = %src, dst = %dst, remote = %remote, "resolved default refspec");

    let spec = PushSpec { force, src, dst };
    validate_push_spec(&spec)?;
    Ok(vec![spec])
}

/// Parse a single refspec string into a `PushSpec`.
///
/// Supports `+src:dst` (force), `src:dst`, and bare `src` (dst = src).
fn parse_refspec(spec: &str, global_force: bool) -> Result<PushSpec> {
    let (force, rest) = if let Some(stripped) = spec.strip_prefix('+') {
        (true, stripped)
    } else {
        (global_force, spec)
    };

    let (src, dst) = if let Some((s, d)) = rest.split_once(':') {
        (s.to_owned(), d.to_owned())
    } else {
        let full_ref = if rest.starts_with("refs/") {
            rest.to_owned()
        } else {
            format!("refs/heads/{rest}")
        };
        (full_ref.clone(), full_ref)
    };

    let spec = PushSpec { force, src, dst };
    validate_push_spec(&spec)?;
    Ok(spec)
}

fn validate_push_spec(spec: &PushSpec) -> Result<()> {
    if spec.dst.is_empty() {
        return Err(CrabError::Configuration {
            key: "push refspec".into(),
            origin: "empty destination ref".into(),
        });
    }
    validate_refspec_name("destination", &spec.dst)?;
    if !spec.src.is_empty() {
        validate_refspec_name("source", &spec.src)?;
    }
    Ok(())
}

fn validate_refspec_name(position: &str, name: &str) -> Result<()> {
    crate::git::refname::validate_push_refname(name).map_err(|_| CrabError::Configuration {
        key: format!("invalid {position} ref name"),
        origin: name.to_owned(),
    })?;
    Ok(())
}

/// Get the current branch name (short form, e.g. "main").
///
/// On `--features gix-facade`, resolves through `gix::Repository::head_ref()`.
/// Default builds shell out to `git symbolic-ref HEAD`.
fn current_branch() -> Option<String> {
    #[cfg(feature = "gix-facade")]
    {
        let repo = crate::git::facade::open().ok()?;
        crate::git::facade::current_branch_name(&repo)
            .ok()
            .flatten()
    }

    #[cfg(not(feature = "gix-facade"))]
    {
        let output = Command::new("git")
            .args(["symbolic-ref", "HEAD"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let full_ref = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        // Strip refs/heads/ prefix to get the short branch name.
        full_ref
            .strip_prefix("refs/heads/")
            .map(ToOwned::to_owned)
            .or(Some(full_ref))
            .filter(|s| !s.is_empty())
    }
}

/// Read a single git config value.
///
/// On `--features gix-config`, reads go through [`GixConfigResolver`].
/// Default builds shell out to `git config <key>`.
pub(crate) fn git_config_value(key: &str) -> Option<String> {
    #[cfg(feature = "gix-config")]
    {
        let git_dir = crate::git::discover::discover_git_dir().ok()?;
        let resolver = crate::core::config_resolver::GixConfigResolver::open(&git_dir).ok()?;
        resolver.string(key)
    }

    #[cfg(not(feature = "gix-config"))]
    {
        let output = Command::new("git")
            .args(["config", key])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if value.is_empty() { None } else { Some(value) }
    }
}

/// Resolve a ref to its SHA via `git rev-parse`.
///
/// On `--features gix-facade`, resolves through `repo.rev_parse_single()`.
/// Default builds shell out to `git rev-parse <spec>`.
fn resolve_rev(refspec: &str) -> Option<String> {
    #[cfg(feature = "gix-facade")]
    {
        let repo = crate::git::facade::open().ok()?;
        crate::git::facade::rev_parse_hex(&repo, refspec)
            .ok()
            .flatten()
    }

    #[cfg(not(feature = "gix-facade"))]
    {
        let output = Command::new("git")
            .args(["rev-parse", refspec])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if sha.is_empty() { None } else { Some(sha) }
    }
}

/// Print dry-run summary showing what would be pushed.
fn print_dry_run(remote: &str, url: &str, specs: &[PushSpec]) {
    println!("Would push to {remote} ({url}):");
    for spec in specs {
        if spec.src.is_empty() {
            println!("  (delete) {}", spec.dst);
        } else {
            let force_marker = if spec.force { " (force)" } else { "" };
            println!("  {} → {}{}", spec.src, spec.dst, force_marker);
        }
    }
}

/// Print the final push summary to stdout.
fn print_push_summary(
    _remote: &str,
    url: &str,
    specs: &[PushSpec],
    _result: &PushResult,
    elapsed: std::time::Duration,
) {
    println!("\nTo {url}");
    for spec in specs {
        if spec.src.is_empty() {
            println!("   (deleted) {}", spec.dst);
        } else {
            println!("   {} → {}", spec.src, spec.dst);
        }
    }
    println!(
        "   {} ref(s) pushed in {:.1}s",
        specs.len(),
        elapsed.as_secs_f64()
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_refspec_simple() {
        let spec = parse_refspec("refs/heads/main:refs/heads/main", false).unwrap();
        assert_eq!(spec.src, "refs/heads/main");
        assert_eq!(spec.dst, "refs/heads/main");
        assert!(!spec.force);
    }

    #[test]
    fn parse_refspec_force_prefix() {
        let spec = parse_refspec("+refs/heads/main:refs/heads/main", false).unwrap();
        assert_eq!(spec.src, "refs/heads/main");
        assert_eq!(spec.dst, "refs/heads/main");
        assert!(spec.force);
    }

    #[test]
    fn parse_refspec_global_force() {
        let spec = parse_refspec("refs/heads/main:refs/heads/main", true).unwrap();
        assert!(spec.force);
    }

    #[test]
    fn parse_refspec_bare_branch() {
        let spec = parse_refspec("main", false).unwrap();
        assert_eq!(spec.src, "refs/heads/main");
        assert_eq!(spec.dst, "refs/heads/main");
    }

    #[test]
    fn parse_refspec_bare_full_ref() {
        let spec = parse_refspec("refs/tags/v1.0", false).unwrap();
        assert_eq!(spec.src, "refs/tags/v1.0");
        assert_eq!(spec.dst, "refs/tags/v1.0");
    }

    #[test]
    fn parse_refspec_delete() {
        let spec = parse_refspec(":refs/heads/old", false).unwrap();
        assert_eq!(spec.src, "");
        assert_eq!(spec.dst, "refs/heads/old");
    }

    #[test]
    fn parse_refspec_short_names() {
        let spec = parse_refspec("feature:main", false).unwrap();
        assert_eq!(spec.src, "feature");
        assert_eq!(spec.dst, "main");
    }

    #[test]
    fn parse_refspec_rejects_empty_destination() {
        let err = parse_refspec(":", false).unwrap_err();
        assert!(
            err.to_string().contains("empty destination ref"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_refspec_rejects_bad_destination_refname() {
        let err = parse_refspec("main:bad..ref", false).unwrap_err();
        assert!(
            err.to_string().contains("invalid destination ref name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_refspec_rejects_bad_source_refname() {
        let err = parse_refspec("bad..ref:refs/heads/main", false).unwrap_err();
        assert!(
            err.to_string().contains("invalid source ref name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rebase_retry_branch_accepts_single_non_fast_forward_branch() {
        use std::collections::HashMap;

        let spec = PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(
            spec.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::NonFastForward {
                have: "old".to_owned(),
                want: "new".to_owned(),
            }),
        );
        let result = PushResult::new(outcomes);

        assert_eq!(rebase_retry_branch(&[spec], &result), Some("main"));
    }

    #[test]
    fn push_failure_source_preserves_non_fast_forward_classification() {
        use std::collections::HashMap;

        let spec = PushSpec {
            force: false,
            src: "HEAD".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let result = PushResult::new(HashMap::from([(
            spec.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::NonFastForward {
                have: "old".to_owned(),
                want: "new".to_owned(),
            }),
        )]));

        assert!(matches!(
            push_failure_source(std::slice::from_ref(&spec), &result),
            CrabError::NonFastForward {
                ref_name,
                have,
                want,
            } if ref_name == "refs/heads/main" && have == "old" && want == "new"
        ));
    }

    #[test]
    fn rebase_retry_branch_accepts_head_to_current_branch() {
        use std::collections::HashMap;

        let spec = PushSpec {
            force: false,
            src: "HEAD".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(
            spec.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::NonFastForward {
                have: "old".to_owned(),
                want: "new".to_owned(),
            }),
        );
        let result = PushResult::new(outcomes);

        assert_eq!(rebase_retry_branch(&[spec], &result), Some("main"));
    }

    #[test]
    fn rebase_retry_branch_rejects_force_or_multi_ref_pushes() {
        use std::collections::HashMap;

        let forced = PushSpec {
            force: true,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let other = PushSpec {
            force: false,
            src: "refs/heads/other".to_owned(),
            dst: "refs/heads/other".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(
            forced.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::NonFastForward {
                have: "old".to_owned(),
                want: "new".to_owned(),
            }),
        );
        let result = PushResult::new(outcomes);

        assert_eq!(
            rebase_retry_branch(std::slice::from_ref(&forced), &result),
            None
        );
        assert_eq!(rebase_retry_branch(&[forced, other], &result), None);
    }

    #[test]
    fn rebase_retry_branch_rejects_different_source_branch() {
        use std::collections::HashMap;

        let spec = PushSpec {
            force: false,
            src: "refs/heads/other".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(
            spec.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::NonFastForward {
                have: "old".to_owned(),
                want: "new".to_owned(),
            }),
        );
        let result = PushResult::new(outcomes);

        assert_eq!(rebase_retry_branch(&[spec], &result), None);
    }

    #[test]
    fn lock_retry_branch_accepts_single_current_branch_lock_contention() {
        use std::collections::HashMap;

        let spec = PushSpec {
            force: false,
            src: "HEAD".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(
            spec.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::LockContention {
                holder: "push-1".to_owned(),
                ttl_remaining_secs: 30,
            }),
        );
        let result = PushResult::new(outcomes);

        assert_eq!(lock_retry_branch(&[spec], &result), Some("main"));
    }

    #[test]
    fn integration_retry_delay_stays_within_cap() {
        for attempt in 1..32 {
            let delay = integration_retry_delay(attempt);
            assert!(delay > Duration::ZERO);
            assert!(delay <= INTEGRATION_RETRY_BACKOFF_CAP);
        }
    }

    #[test]
    fn push_args_agent_retry_default_supports_large_swarms() {
        let args = PushArgs::try_parse_from(["crab-push", "--rebase-on-non-fast-forward"]).unwrap();

        assert_eq!(args.rebase_retry_limit, DEFAULT_AGENT_REBASE_RETRY_LIMIT);
    }

    #[test]
    fn push_args_accept_follow_tags() {
        let args = PushArgs::try_parse_from(["crab-push", "--follow-tags"]).unwrap();

        assert!(args.follow_tags);
    }

    #[test]
    fn push_args_reject_removed_jobs_flag() {
        let err = PushArgs::try_parse_from(["crab-push", "--jobs", "4"])
            .expect_err("push --jobs was a no-op and should stay removed");

        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn direct_crab_url_resolves_without_git_remote_config() {
        let target = resolve_push_target(Some("crab://bucket/repo")).unwrap();

        assert_eq!(target.remote, "crab://bucket/repo");
        assert_eq!(target.url, "crab://bucket/repo");
        assert_eq!(target.parsed_url.bucket, "bucket");
        assert_eq!(target.parsed_url.repo_path, "repo");
    }

    #[test]
    fn direct_non_crab_url_is_rejected_as_a_url() {
        let err = resolve_push_target(Some("https://example.com/repo")).unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(!err.to_string().contains("remote.https://"));
    }

    fn configured_remote(name: &str, url: &str) -> ConfiguredRemote {
        ConfiguredRemote {
            name: name.to_owned(),
            url: url.to_owned(),
        }
    }

    #[test]
    fn default_remote_prefers_crab_compatible_upstream() {
        let remotes = vec![
            configured_remote("origin", "https://github.com/example/repo.git"),
            configured_remote("backup", "crab://bucket/backup"),
        ];

        assert_eq!(
            select_default_crab_remote(Some("backup"), &remotes).unwrap(),
            Some("backup".to_owned())
        );
    }

    #[test]
    fn default_remote_prefers_named_crab_when_upstream_is_not_compatible() {
        let remotes = vec![
            configured_remote("origin", "https://github.com/example/repo.git"),
            configured_remote("crab", "crab://bucket/repo"),
            configured_remote("backup", "crab://bucket/backup"),
        ];

        assert_eq!(
            select_default_crab_remote(Some("origin"), &remotes).unwrap(),
            Some("crab".to_owned())
        );
    }

    #[test]
    fn default_remote_detects_the_only_crab_compatible_remote() {
        let remotes = vec![
            configured_remote("origin", "https://github.com/example/repo.git"),
            configured_remote("storage", "crab://bucket/repo"),
        ];

        assert_eq!(
            select_default_crab_remote(Some("origin"), &remotes).unwrap(),
            Some("storage".to_owned())
        );
    }

    #[test]
    fn default_remote_requires_explicit_choice_for_ambiguous_crab_remotes() {
        let remotes = vec![
            configured_remote("origin", "https://github.com/example/repo.git"),
            configured_remote("primary", "crab://bucket/primary"),
            configured_remote("backup", "crab://bucket/backup"),
        ];

        let err = select_default_crab_remote(Some("origin"), &remotes).unwrap_err();
        assert!(err.to_string().contains("'primary'"));
        assert!(err.to_string().contains("'backup'"));
        assert!(err.to_string().contains("--remote <name>"));
    }

    #[test]
    fn default_remote_returns_no_crab_match_for_non_crab_remotes() {
        let remotes = vec![configured_remote(
            "origin",
            "https://github.com/example/repo.git",
        )];

        assert_eq!(
            select_default_crab_remote(Some("origin"), &remotes).unwrap(),
            None
        );
    }

    fn test_push_args() -> PushArgs {
        PushArgs {
            remote: None,
            refspecs: Vec::new(),
            upload_concurrency: None,
            lock_wait_secs: None,
            manifest_cas_retries: None,
            rebase_on_non_fast_forward: false,
            rebase_retry_limit: DEFAULT_AGENT_REBASE_RETRY_LIMIT,
            dry_run: false,
            force: false,
            follow_tags: false,
            verbose: false,
            no_incremental: false,
            no_color: false,
            json: false,
            jsonl: false,
        }
    }

    #[test]
    fn agent_integration_sets_default_lock_wait_when_unconfigured() {
        let mut args = test_push_args();
        args.rebase_on_non_fast_forward = true;
        let mut config = PushConfig::default();

        apply_push_cli_overrides(&args, &mut config);

        assert_eq!(config.lock_wait, AGENT_INTEGRATION_LOCK_WAIT);
    }

    #[test]
    fn normal_push_keeps_configured_lock_wait_default() {
        let args = test_push_args();
        let mut config = PushConfig::default();

        apply_push_cli_overrides(&args, &mut config);

        assert_eq!(config.lock_wait, Duration::ZERO);
    }

    #[test]
    fn explicit_lock_wait_overrides_agent_integration_default() {
        let mut args = test_push_args();
        args.rebase_on_non_fast_forward = true;
        args.lock_wait_secs = Some(7);
        let mut config = PushConfig::default();

        apply_push_cli_overrides(&args, &mut config);

        assert_eq!(config.lock_wait, Duration::from_secs(7));
    }

    #[test]
    fn push_upload_concurrency_default_preserves_config() {
        let args = test_push_args();
        let mut config = PushConfig {
            upload_concurrency: 32,
            ..PushConfig::default()
        };

        apply_push_cli_overrides(&args, &mut config);

        assert_eq!(config.upload_concurrency, 32);
    }

    #[test]
    fn explicit_push_upload_concurrency_overrides_config() {
        let mut args = test_push_args();
        args.upload_concurrency = Some(4);
        let mut config = PushConfig {
            upload_concurrency: 32,
            ..PushConfig::default()
        };

        apply_push_cli_overrides(&args, &mut config);

        assert_eq!(config.upload_concurrency, 4);
    }

    #[test]
    fn push_upload_concurrency_zero_clamps_to_single_worker() {
        assert_eq!(effective_push_upload_concurrency(0), 1);
        assert_eq!(effective_push_upload_concurrency(4), 4);

        let mut args = test_push_args();
        args.upload_concurrency = Some(0);
        let mut config = PushConfig {
            upload_concurrency: 32,
            ..PushConfig::default()
        };

        apply_push_cli_overrides(&args, &mut config);

        assert_eq!(config.upload_concurrency, 1);
    }

    #[test]
    fn structured_summary_includes_active_active_commit_metadata() {
        use std::collections::HashMap;

        use crate::git::push::{PushCommitMetadata, RefPushOutcome};
        use crab_coordination::write_coordinator::PushTransactionState;

        let spec = PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(spec.dst.clone(), RefPushOutcome::Ok);
        let result = PushResult::new(outcomes).with_active_active_commit(PushCommitMetadata {
            operation_id: "op-123".to_owned(),
            coordinator_epoch: 42,
            writer: "east".to_owned(),
            writer_region: "us-east-1".to_owned(),
            manifest_generation: 7,
            commit_state: PushTransactionState::Materialized,
        });

        let summary = build_push_summary(
            &[spec],
            &result,
            "crab://primary/org/repo",
            std::time::Duration::from_millis(9),
            None,
        );

        assert_eq!(summary.operation_id.as_deref(), Some("op-123"));
        assert_eq!(summary.coordinator_epoch, Some(42));
        assert_eq!(summary.writer_region.as_deref(), Some("us-east-1"));
        assert_eq!(summary.commit_state.as_deref(), Some("materialized"));
    }

    #[test]
    fn structured_summary_marks_lock_contention_retryable() {
        use std::collections::HashMap;

        use crate::git::push::{PushRejectReason, RefPushOutcome};

        let spec = PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(
            spec.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::LockContention {
                holder: "agent-a".to_owned(),
                ttl_remaining_secs: 7,
            }),
        );
        let result = PushResult::new(outcomes);

        let summary = build_push_summary(
            &[spec],
            &result,
            "crab://primary/org/repo",
            std::time::Duration::from_millis(9),
            Some(PushIntegrationSummary {
                retries: 5,
                retry_limit: 256,
            }),
        );

        assert_eq!(summary.refs[0].status, "lock-contention");
        assert_eq!(summary.refs[0].retryable, Some(true));
        assert_eq!(summary.refs[0].retry_after_secs, Some(7));
        assert_eq!(summary.integration_retries, Some(5));
        assert_eq!(summary.integration_retry_limit, Some(256));
    }

    #[test]
    fn structured_summary_marks_stale_info_retryable() {
        use std::collections::HashMap;

        use crate::git::push::{PushRejectReason, RefPushOutcome};

        let spec = PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(
            spec.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::StaleInfo),
        );
        let result = PushResult::new(outcomes);

        let summary = build_push_summary(
            &[spec],
            &result,
            "crab://primary/org/repo",
            std::time::Duration::from_millis(9),
            None,
        );

        assert_eq!(summary.refs[0].status, "stale info");
        assert_eq!(summary.refs[0].retryable, Some(true));
        assert_eq!(summary.refs[0].retry_after_secs, None);
    }

    #[test]
    fn structured_summary_and_source_preserve_transient_failures() {
        use std::collections::HashMap;

        use crate::git::push::{PushRejectReason, RefPushOutcome};

        let spec = PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(
            spec.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::Throttled {
                retry_after_secs: Some(3),
            }),
        );
        let result = PushResult::new(outcomes);

        let summary = build_push_summary(
            std::slice::from_ref(&spec),
            &result,
            "crab://primary/org/repo",
            std::time::Duration::from_millis(9),
            None,
        );

        assert_eq!(summary.refs[0].status, "transient");
        assert_eq!(summary.refs[0].retryable, Some(true));
        assert_eq!(summary.refs[0].retry_after_secs, Some(3));
        assert!(matches!(
            push_failure_source(std::slice::from_ref(&spec), &result),
            CrabError::Throttled {
                retry_after: Some(delay)
            } if delay == std::time::Duration::from_secs(3)
        ));

        let network_result = PushResult::new(HashMap::from([(
            "refs/heads/main".to_owned(),
            RefPushOutcome::Rejected(PushRejectReason::NetworkTransient(
                "connection reset".to_owned(),
            )),
        )]));
        assert!(matches!(
            push_failure_source(std::slice::from_ref(&spec), &network_result),
            CrabError::NetworkTransient(_)
        ));
    }

    #[test]
    fn integration_failure_surfaces_as_structured_non_retryable_ref_status() {
        use std::collections::HashMap;

        use crate::git::push::{PushRejectReason, RefPushOutcome};

        let spec = PushSpec {
            force: false,
            src: "HEAD".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(
            spec.dst.clone(),
            RefPushOutcome::Rejected(PushRejectReason::NonFastForward {
                have: "old".to_owned(),
                want: "new".to_owned(),
            }),
        );
        let mut failure = PushAttemptFailure {
            repo_root: PathBuf::new(),
            remote_name: "origin".to_owned(),
            remote_url: "crab://primary/org/repo".to_owned(),
            specs: vec![spec],
            result: PushResult::new(outcomes),
            elapsed: std::time::Duration::from_millis(9),
            integration: Some(PushIntegrationSummary {
                retries: 1,
                retry_limit: 256,
            }),
            agent_integration_lock: false,
        };

        mark_integration_failed(
            &mut failure,
            "git pull --rebase --autostash origin main".to_owned(),
            "CONFLICT (content): Merge conflict".to_owned(),
            2,
        );
        let summary = build_push_summary(
            &failure.specs,
            &failure.result,
            &failure.remote_url,
            failure.elapsed,
            failure.integration,
        );

        assert_eq!(summary.refs[0].status, "integration-failed");
        assert_eq!(summary.refs[0].retryable, Some(false));
        assert_eq!(summary.integration_retries, Some(2));
        assert!(
            summary.refs[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("CONFLICT"))
        );
    }

    #[test]
    fn resolve_push_repo_root_uses_repo_root_from_nested_cwd() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .expect("run git init");
        assert!(status.success());

        let nested = tmp.path().join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();

        let saved_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(&nested).unwrap();
        let root = resolve_push_repo_root().unwrap().canonicalize().unwrap();
        let expected = tmp.path().canonicalize().unwrap();

        if let Some(cwd) = saved_cwd {
            let _ = std::env::set_current_dir(cwd);
        }

        assert_eq!(root, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_staging_dir_is_normal_for_commit_only_pushes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".crab")).unwrap();

        let staging = open_optional_staging_for_push(dir.path()).await;

        assert!(staging.is_none());
        assert!(!dir.path().join(".crab/staging").exists());
    }
}
