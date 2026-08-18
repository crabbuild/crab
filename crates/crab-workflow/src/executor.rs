//! Local stage execution — walks the state machine for one stage
//! attempt on the miss path.
//!
//! The executor is the choreographer: it sequences journal
//! transitions around the actual work (spawn child, hash outs,
//! write cache entry). It deliberately stays narrow — no retry
//! loop, no DAG scheduling, no cache-hit materialization. Those
//! land in higher-level functions that call [`run_local`]
//! repeatedly.
//!
//! The happy path for a miss is:
//!
//! ```text
//!  Resolved ─► CacheChecked ─► Running ─► Produced ─► Hashed ─►
//!  Staged ─► EntryWritten ─► RefPublished ─► LockfileUpdated ─►
//!  Committed
//! ```
//!
//! On any failure the executor writes a `Failed` transition with
//! the right error payload and returns. The caller (retry layer or
//! DAG scheduler) decides what to do next.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use futures_util::StreamExt;
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;
use serde::Serialize;
use tokio::process::Command;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use crate::{RunState, StageState};
use crab_types::time::now_rfc3339_millis;

use crate::Lockfile;
use crate::WorkflowMetrics;
use crate::cache::{
    CachedCmd, CachedOut, ENTRY_SCHEMA_VERSION, RemoteArtifactStores, StageCacheEntry, read_local,
    write_local,
};
use crate::env::sanitize;
use crate::hasher::ResolvedStage;
use crate::journal::Journal;
use crate::sandbox::{self, HermeticSandboxPolicy};
use crate::signals::{
    ChildSupervisor, DEFAULT_GRACEFUL_SHUTDOWN, DEFAULT_STDERR_TAIL_BYTES, EventSink,
    SupervisorEvent, SupervisorOutcome, stage_log_path,
};
use crate::stage::{
    Cmd, Dep, DepUrlHashExt, Out, OutKind, Stage, StageName, expand_external_url_out_alias,
};
use crate::stage_cmd::platform_shell;
use crate::{Result, WorkflowError as CrabError};
use crab_types::workflow::StageHash;

/// Tunable knobs for a single stage attempt.
///
/// Kept as a plain struct (not borrowed from `WorkflowConfig`)
/// so unit tests can instantiate it without dragging in the full
/// config subsystem.
#[derive(Clone)]
pub struct ExecutorConfig {
    /// Root of `.crab/workflow/` — logs and sidecar tempfiles
    /// live under it.
    pub workflow_root: PathBuf,
    /// Root of the local chunk cache. Stage entries persist under
    /// `<cache_root>/stages/…`.
    pub cache_root: PathBuf,
    /// SIGTERM→SIGKILL escalation window. Default matches
    /// `workflow.graceful_shutdown_timeout_secs`.
    pub graceful_shutdown: Duration,
    /// Max bytes retained from the child's stderr for failure
    /// reporting. Default matches the journal `stderr_tail` width.
    pub stderr_tail_bytes: usize,
    /// Whether child stdout/stderr should also be mirrored to this
    /// process. Logs and stderr tails are still captured when false.
    pub mirror_child_output: bool,
    /// Optional file watched by the supervisor to interrupt a
    /// running child from an external queue command.
    pub external_kill_path: Option<PathBuf>,
    /// Optional callback fired once the supervised child is spawned
    /// and its stdout/stderr pumps are attached.
    pub child_started: Option<Arc<dyn Fn(u32) + Send + Sync>>,
    /// Ceiling on the number of declared outs — surfaced as
    /// `StageOutCountExceeded` before hashing to catch runaway
    /// directory expansions.
    pub max_outs_per_stage: usize,
    /// Per-out byte limit. `None` means "apply only the Out's own
    /// `max_bytes` declaration".
    pub default_max_out_bytes: Option<u64>,
    /// Host fingerprint stamped into the cache entry.
    pub host_fingerprint: String,
    /// Working directory for spawned stage commands.
    ///
    /// `None` means "inherit the parent's cwd" — the historical
    /// behavior, fine for `crab run` because the CLI runs from
    /// the repo root. Set to `Some(tmpdir)` by `crab exp run` so
    /// the child `cp`, `python`, etc. resolve relative paths
    /// against the experiment worktree rather than the user's
    /// main working tree (R23).
    pub working_dir: Option<PathBuf>,
    /// Optional process-wide perf counters. When `Some`, the executor
    /// bumps the `workflow_*` counters on each relevant transition
    /// (R21). `None` keeps the executor dep-free for narrow unit tests
    /// that don't care about observability.
    pub metrics: Option<Arc<dyn WorkflowMetrics>>,
    /// When `true`, the executor uploads stage outputs as xorbs and
    /// writes a remote ref after `EntryWritten`. When `false`
    /// (default), the `Staged` and `RefPublished` transitions remain
    /// no-ops for backward compatibility.
    pub cache_push: bool,
    /// Disable local and remote run-cache lookup for this invocation.
    /// Fresh executions still write cache entries when the stage's
    /// own cache policy allows it.
    pub no_run_cache: bool,
    /// Skip writing local/remote cache artifacts for fresh executions.
    /// The caller still receives a cache-entry-shaped record so
    /// lockfiles can capture output hashes.
    pub no_commit: bool,
    /// Selected remote store for cache pulls, and the primary remote when
    /// `cache_push` is `true`.
    pub remote_store: Option<Arc<crate::WorkflowStore>>,
    /// Remote prefix (repo path) for `remote_store`.
    pub remote_prefix: Option<String>,
    /// Primary remote used when a selected read replica misses a cache object.
    pub remote_primary_fallback_store: Option<Arc<crate::WorkflowStore>>,
    /// Primary remote prefix used with `remote_primary_fallback_store`.
    pub remote_primary_fallback_prefix: Option<String>,
    /// Named artifact stores selected by `outs.remote`.
    pub remote_artifact_stores: Option<RemoteArtifactStores>,
    /// DVC-style `remote://name/path` aliases backed by
    /// `[workflow.remotes.<name>]` URLs.
    pub remote_aliases: BTreeMap<String, String>,
    /// Minimum free disk space (bytes) before skipping cache writes.
    /// Defaults to 100 MB.
    pub min_cache_headroom: u64,
    /// Allow checkpoint outputs in an experiment-owned execution.
    /// Ordinary `run` and `repro` keep the fail-closed policy.
    pub allow_checkpoints: bool,
    /// Private checkpoint control directory inherited by stage processes.
    pub checkpoint_control_dir: Option<PathBuf>,
    /// Stable experiment identity used by the checkpoint supervisor.
    pub checkpoint_run_id: Option<String>,
    /// Per-run checkpoint authentication token. Never included in hashes or logs.
    pub checkpoint_token: Option<String>,
}

impl std::fmt::Debug for ExecutorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorConfig")
            .field("workflow_root", &self.workflow_root)
            .field("cache_root", &self.cache_root)
            .field("graceful_shutdown", &self.graceful_shutdown)
            .field("stderr_tail_bytes", &self.stderr_tail_bytes)
            .field("mirror_child_output", &self.mirror_child_output)
            .field("external_kill_path", &self.external_kill_path)
            .field(
                "child_started",
                &self.child_started.as_ref().map(|_| "callback"),
            )
            .field("max_outs_per_stage", &self.max_outs_per_stage)
            .field("default_max_out_bytes", &self.default_max_out_bytes)
            .field("host_fingerprint", &self.host_fingerprint)
            .field("working_dir", &self.working_dir)
            .field("metrics", &self.metrics.as_ref().map(|_| "Metrics(...)"))
            .field("cache_push", &self.cache_push)
            .field("no_run_cache", &self.no_run_cache)
            .field("no_commit", &self.no_commit)
            .field(
                "remote_store",
                &self.remote_store.as_ref().map(|_| "Store(...)"),
            )
            .field("remote_prefix", &self.remote_prefix)
            .field(
                "remote_primary_fallback_store",
                &self
                    .remote_primary_fallback_store
                    .as_ref()
                    .map(|_| "Store(...)"),
            )
            .field(
                "remote_primary_fallback_prefix",
                &self.remote_primary_fallback_prefix,
            )
            .field("remote_artifact_stores", &self.remote_artifact_stores)
            .field(
                "remote_aliases",
                &self.remote_aliases.keys().collect::<Vec<_>>(),
            )
            .field("min_cache_headroom", &self.min_cache_headroom)
            .field("allow_checkpoints", &self.allow_checkpoints)
            .field("checkpoint_control_dir", &self.checkpoint_control_dir)
            .field("checkpoint_run_id", &self.checkpoint_run_id)
            .field(
                "checkpoint_token",
                &self.checkpoint_token.as_ref().map(|_| "redacted"),
            )
            .finish()
    }
}

impl ExecutorConfig {
    /// Build with sensible defaults. Callers override fields
    /// (workflow_root / cache_root) as needed.
    pub fn new(workflow_root: PathBuf, cache_root: PathBuf) -> Self {
        Self {
            workflow_root,
            cache_root,
            graceful_shutdown: DEFAULT_GRACEFUL_SHUTDOWN,
            stderr_tail_bytes: DEFAULT_STDERR_TAIL_BYTES,
            mirror_child_output: true,
            external_kill_path: None,
            child_started: None,
            max_outs_per_stage: 10_000,
            default_max_out_bytes: None,
            host_fingerprint: default_host_fingerprint(),
            working_dir: None,
            metrics: None,
            cache_push: false,
            no_run_cache: false,
            no_commit: false,
            remote_store: None,
            remote_prefix: None,
            remote_primary_fallback_store: None,
            remote_primary_fallback_prefix: None,
            remote_artifact_stores: None,
            remote_aliases: BTreeMap::new(),
            min_cache_headroom: crate::cache::DEFAULT_MIN_CACHE_HEADROOM_BYTES,
            allow_checkpoints: false,
            checkpoint_control_dir: None,
            checkpoint_run_id: None,
            checkpoint_token: None,
        }
    }
}

fn default_host_fingerprint() -> String {
    format!(
        "{}-{}-crab-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
    )
}

/// Run a stage locally and commit its outputs to the local cache.
///
/// `run_id` + `attempt` identify the journal row for this attempt
/// (the caller must have already inserted the row via
/// [`Journal::insert_stage_start`] and transitioned it to
/// [`StageState::Resolved`]). `run_local` drives the rest of the
/// state machine forward.
///
/// On a cache hit this short-circuits: the executor fast-forwards
/// the journal with virtual transitions carrying the cached entry's
/// hash and returns the existing entry. The caller is responsible
/// for materializing its outs (that's a separate concern wired in
/// task 1.17).
///
/// Errors propagate after the journal has recorded the `Failed`
/// transition. Callers receive the same error value they would see
/// had they asked the journal directly.
#[instrument(
    skip(resolved, cfg, journal),
    fields(stage = %resolved.stage.name, attempt)
)]
pub async fn run_local(
    resolved: &ResolvedStage,
    cfg: &ExecutorConfig,
    journal: &Journal,
    run_id: Uuid,
    attempt: u32,
) -> Result<StageCacheEntry> {
    let stage_name = resolved.stage.name.as_str().to_owned();

    // Each retry attempt beyond the first is counted as a retry.
    // The caller drives the retry loop (task 1.8 / 3.14); here we
    // just report what the journal already reflects.
    if attempt > 1
        && let Some(m) = cfg.metrics.as_deref()
    {
        m.inc_workflow_stage_retry_attempts();
    }

    // Wrap the real work so every error path records `Failed` in the
    // journal with a meaningful payload. The journal transition is
    // best-effort — if even that fails (corrupt DB) we still return
    // the original error so the user sees the real cause.
    match run_inner(resolved, cfg, journal, run_id, attempt).await {
        Ok(entry) => Ok(entry),
        Err(err) => {
            record_failure(journal, run_id, &stage_name, attempt, &err);
            if let Some(m) = cfg.metrics.as_deref() {
                m.inc_workflow_stages_failed();
            }
            Err(err)
        }
    }
}

async fn run_inner(
    resolved: &ResolvedStage,
    cfg: &ExecutorConfig,
    journal: &Journal,
    run_id: Uuid,
    attempt: u32,
) -> Result<StageCacheEntry> {
    let stage = &resolved.stage;
    let stage_name = stage.name.as_str();
    if !cfg.allow_checkpoints && stage.outs.iter().any(Out::is_checkpoint) {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_requires_exp_run".to_owned(),
            origin: format!("stage '{stage_name}' declares checkpoint outputs"),
        });
    }
    let stage_hash = crate::hasher::compute(resolved);
    let run_cache_write_enabled = stage.run_cache_enabled() && !cfg.no_commit;
    let run_cache_lookup_enabled = stage.run_cache_lookup_enabled() && !cfg.no_run_cache;
    let remote_cache_push_enabled = stage.remote_cache_push_enabled() && !cfg.no_commit;

    // CacheChecked. Read-local returns None on miss; we treat
    // newer-schema entries as miss with a warn so the user knows to
    // upgrade their binary.
    let hit = if run_cache_lookup_enabled {
        match read_local(&cfg.cache_root, &stage_hash) {
            Ok(h) => h,
            Err(CrabError::CacheEntrySchemaNewer {
                found, supported, ..
            }) => {
                warn!(
                    stage = %stage_name,
                    found, supported,
                    "workflow stage cache entry on disk uses a newer schema than this binary supports; \
                     treating as miss and recomputing"
                );
                None
            }
            Err(other) => return Err(other),
        }
    } else {
        None
    };

    journal.transition(
        run_id,
        stage_name,
        attempt,
        StageState::CacheChecked,
        &cache_checked_payload(&stage_hash, hit.is_some(), hit.is_some()),
    )?;

    if let Some(entry) = hit {
        // Cache-hit fast-forward. The executor doesn't materialize
        // outs on this code path — the caller coordinates
        // cache-hit materialization through `materialize::` helpers
        // (task 1.17). Walking the state machine here still keeps
        // the journal honest for resume.
        for next in [
            StageState::Produced,
            StageState::Hashed,
            StageState::Staged,
            StageState::EntryWritten,
            StageState::RefPublished,
            StageState::LockfileUpdated,
            StageState::Committed,
        ] {
            journal.transition(run_id, stage_name, attempt, next, r#"{"source":"Cache"}"#)?;
        }
        if let Some(m) = cfg.metrics.as_deref() {
            // Phase 1 only serves hits from the local chunk cache.
            // TODO: phase 3 splits this into local vs remote once the
            // remote-ref fetch path lands and bumps
            // `workflow_stage_cache_hits_remote` instead.
            m.inc_workflow_stage_cache_hits_local();
        }
        info!(stage = %stage_name, stage_hash = %stage_hash, "workflow stage cache hit (local)");
        return Ok(entry);
    }

    // Remote cache pull: check the remote for a matching stage entry
    // before falling through to local execution. Network errors are
    // logged at debug! and fall through transparently.
    let remote_candidates = [
        cfg.remote_store
            .as_ref()
            .zip(cfg.remote_prefix.as_ref())
            .map(|(store, prefix)| (store, prefix, "selected")),
        cfg.remote_primary_fallback_store
            .as_ref()
            .zip(cfg.remote_primary_fallback_prefix.as_ref())
            .map(|(store, prefix)| (store, prefix, "primary-fallback")),
    ];
    if run_cache_lookup_enabled {
        for (store, prefix, source) in remote_candidates.into_iter().flatten() {
            match crate::cache::pull_remote_with_artifact_stores(
                store,
                prefix,
                cfg.remote_artifact_stores.as_ref(),
                &stage_hash,
                &cfg.cache_root,
                cfg.working_dir.as_deref(),
            )
            .await
            {
                Ok(Some(entry)) => {
                    // Remote hit — fast-forward the journal to Committed.
                    for next in [
                        StageState::Produced,
                        StageState::Hashed,
                        StageState::Staged,
                        StageState::EntryWritten,
                        StageState::RefPublished,
                        StageState::LockfileUpdated,
                        StageState::Committed,
                    ] {
                        journal.transition(
                            run_id,
                            stage_name,
                            attempt,
                            next,
                            r#"{"source":"Remote"}"#,
                        )?;
                    }
                    if let Some(m) = cfg.metrics.as_deref() {
                        m.inc_workflow_stage_cache_hits_remote();
                    }
                    info!(
                        stage = %stage_name,
                        stage_hash = %stage_hash,
                        remote_source = source,
                        "workflow stage cache hit (remote)"
                    );
                    return Ok(entry);
                }
                Ok(None) => {
                    // Remote miss — fall through to local execution.
                    debug!(
                        stage = %stage_name,
                        stage_hash = %stage_hash,
                        remote_source = source,
                        "remote cache miss"
                    );
                }
                Err(e) => {
                    // Remote pull error — log and fall through.
                    debug!(
                        stage = %stage_name,
                        stage_hash = %stage_hash,
                        remote_source = source,
                        error = %e,
                        "remote cache pull error"
                    );
                }
            }
        }
    }

    // Miss path. Pre-exec cleanup: nuke any stale outs unless the
    // stage (or the individual Out) asked to keep them.
    if !stage.persist {
        for out in &stage.outs {
            if out.persist || out.is_external_url() {
                continue;
            }
            // When the stage has a wdir, resolve out paths relative
            // to repo_root/wdir/ for filesystem operations.
            let effective_out_path = if out.path.is_absolute() {
                out.path.clone()
            } else if let Some(wdir) = &stage.wdir {
                let base = cfg.working_dir.as_deref().unwrap_or_else(|| Path::new("."));
                base.join(wdir).join(&out.path)
            } else {
                out.path.clone()
            };
            remove_existing(&effective_out_path, stage_name)?;
        }
    }

    // Running. Compute the effective working directory: if the stage
    // declares `wdir`, join it onto the repo root (cfg.working_dir).
    // When `wdir` is absent, fall back to the repo root itself.
    let effective_cwd = if let Some(wdir) = &stage.wdir {
        let base = cfg.working_dir.as_deref().unwrap_or_else(|| Path::new("."));
        let dir = base.join(wdir);
        if !dir.is_dir() {
            return Err(CrabError::Configuration {
                key: format!("stage '{stage_name}' wdir"),
                origin: format!(
                    "working directory '{}' does not exist or is not a directory",
                    dir.display()
                ),
            });
        }
        Some(dir)
    } else {
        cfg.working_dir.clone()
    };

    let sandbox_policy = if stage.hermetic {
        sandbox::ensure_supported(stage_name)?;
        Some(HermeticSandboxPolicy::for_stage(
            stage,
            cfg.working_dir.as_deref(),
            effective_cwd.as_deref(),
            &cfg.workflow_root,
        )?)
    } else {
        None
    };

    let log_path = stage_log_path(&cfg.workflow_root, &run_id.to_string(), stage_name);

    // Determine the stdout capture target:
    // 1. Explicit `kind: stdout` out → capture directly to that path.
    // 2. Exactly one file out → capture to a temp sidecar so we can
    //    use it as a fallback if the command doesn't create the file.
    // 3. Otherwise → no capture (stdout goes to log only).
    let stdout_capture_target: Option<PathBuf>;
    if let Some(stdout_out) = stage.outs.iter().find(|o| o.kind == OutKind::Stdout) {
        let capture_path = if stdout_out.path.is_absolute() {
            stdout_out.path.clone()
        } else if let Some(ref cwd) = effective_cwd {
            cwd.join(&stdout_out.path)
        } else {
            stdout_out.path.clone()
        };
        stdout_capture_target = Some(capture_path);
    } else {
        let file_outs: Vec<_> = stage
            .outs
            .iter()
            .filter(|o| o.kind == OutKind::File && !o.is_external_url())
            .collect();
        if file_outs.len() == 1 {
            // Capture stdout to a sidecar temp file next to the log.
            let sidecar = log_path.with_extension("stdout");
            stdout_capture_target = Some(sidecar);
        } else {
            stdout_capture_target = None;
        }
    }

    let started_at = now_rfc3339_millis();
    let instant_start = Instant::now();

    journal.transition(
        run_id,
        stage_name,
        attempt,
        StageState::Running,
        &running_payload(&started_at, attempt),
    )?;
    maybe_crash_at("Running");

    let outcome = run_stage_commands(
        stage,
        &stage_hash,
        cfg,
        run_id,
        effective_cwd.as_deref(),
        sandbox_policy.as_ref(),
        &log_path,
        stdout_capture_target.as_deref(),
    )
    .await?;
    let duration = instant_start.elapsed();

    classify_stage_outcome(&outcome, stage_name, sandbox_policy.as_ref())?;

    journal.transition(
        run_id,
        stage_name,
        attempt,
        StageState::Produced,
        r#"{"exit_code":0}"#,
    )?;
    maybe_crash_at("Produced");

    // Stdout fallback: if the stage has exactly one file out that
    // doesn't exist on disk, and we captured non-empty stdout to a
    // sidecar, copy the captured stdout to the declared output path.
    // This makes simple commands like `echo hello` work without
    // explicit shell redirection when a single output is declared.
    if let Some(ref capture_path) = stdout_capture_target {
        // Only apply the fallback for non-explicit-stdout outs
        // (explicit stdout outs are already written directly).
        let has_explicit_stdout = stage.outs.iter().any(|o| o.kind == OutKind::Stdout);
        if !has_explicit_stdout {
            let file_outs: Vec<_> = stage
                .outs
                .iter()
                .filter(|o| o.kind == OutKind::File && !o.is_external_url())
                .collect();
            if file_outs.len() == 1 {
                let target_path = if file_outs[0].path.is_absolute() {
                    file_outs[0].path.clone()
                } else if let Some(ref cwd) = effective_cwd {
                    cwd.join(&file_outs[0].path)
                } else {
                    file_outs[0].path.clone()
                };
                // Only apply fallback if the output file is missing
                // and the captured stdout is non-empty.
                if !target_path.exists() {
                    let capture_non_empty = capture_path
                        .metadata()
                        .map(|m| m.len() > 0)
                        .unwrap_or(false);
                    if capture_non_empty {
                        if let Err(e) = std::fs::copy(capture_path, &target_path) {
                            debug!(
                                stage = %stage_name,
                                error = %e,
                                "stdout fallback: failed to copy captured stdout to output path"
                            );
                        } else {
                            debug!(
                                stage = %stage_name,
                                path = %target_path.display(),
                                "stdout fallback: wrote captured stdout to declared output"
                            );
                        }
                    }
                }
            }
        }
    }

    // Hashed. Verify every declared out exists, is the right kind,
    // fits the size ceiling, and compute its blake3.
    let cached_outs = verify_and_hash_outs(stage, cfg).await?;
    let cached_metrics = verify_and_hash_metrics(stage, cfg)?;
    let cached_plots = verify_and_hash_plots(stage, cfg)?;

    journal.transition(
        run_id,
        stage_name,
        attempt,
        StageState::Hashed,
        &hashed_payload(&cached_outs),
    )?;
    maybe_crash_at("Hashed");

    if run_cache_write_enabled {
        crate::cache::store_local_xorbs(
            &cfg.cache_root,
            cached_outs
                .iter()
                .chain(cached_metrics.iter())
                .chain(cached_plots.iter()),
            cfg.working_dir.as_deref(),
        )?;
    }

    // Cache-disabled or --no-commit stages still return this entry
    // for lockfile updates, but intentionally skip the durable run cache.
    let entry = StageCacheEntry {
        schema_version: ENTRY_SCHEMA_VERSION,
        stage_hash,
        stage_name: stage_name.to_owned(),
        cmd: cached_cmd(&stage.cmd),
        outs: cached_outs,
        metrics: cached_metrics,
        plots: cached_plots,
        executed_at: started_at,
        duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
        exec_id: None,
        attempts: attempt,
        host_fingerprint: cfg.host_fingerprint.clone(),
    };

    // Staged. When `--cache-push` is active, pack stage outputs as
    // xorbs and upload to the configured remote. Otherwise this
    // remains a pass-through for backward compatibility.
    if cfg.cache_push && remote_cache_push_enabled {
        if let (Some(store), Some(prefix)) = (cfg.remote_store.as_ref(), cfg.remote_prefix.as_ref())
        {
            let staged = crate::cache::push_entry_xorbs_remote_with_artifact_stores(
                store,
                prefix,
                cfg.remote_artifact_stores.as_ref(),
                &cfg.cache_root,
                &entry,
            )
            .await;
            match staged {
                Ok(()) => {
                    journal.transition(
                        run_id,
                        stage_name,
                        attempt,
                        StageState::Staged,
                        r#"{"remote":"uploaded"}"#,
                    )?;
                }
                Err(e) => {
                    debug!(
                        stage = %stage_name,
                        error = %e,
                        "remote xorb staging failed; continuing without remote cache"
                    );
                    journal.transition(
                        run_id,
                        stage_name,
                        attempt,
                        StageState::Staged,
                        r#"{"remote":"failed"}"#,
                    )?;
                }
            }
        } else {
            journal.transition(
                run_id,
                stage_name,
                attempt,
                StageState::Staged,
                r#"{"phase":"1-noop"}"#,
            )?;
        }
    } else {
        journal.transition(
            run_id,
            stage_name,
            attempt,
            StageState::Staged,
            r#"{"phase":"1-noop"}"#,
        )?;
    }
    maybe_crash_at("Staged");

    // EntryWritten — the commit point for cache-enabled stages.
    if run_cache_write_enabled {
        write_entry(&cfg.cache_root, &entry, stage_name, cfg.min_cache_headroom)?;
    }

    journal.transition(
        run_id,
        stage_name,
        attempt,
        StageState::EntryWritten,
        &entry_written_payload(&stage_hash),
    )?;
    maybe_crash_at("EntryWritten");

    // RefPublished. When `--cache-push` is active, upload the manifest
    // and write a ref via conditional put (CAS). Otherwise no-op.
    if cfg.cache_push && remote_cache_push_enabled {
        if let (Some(store), Some(prefix)) = (cfg.remote_store.as_ref(), cfg.remote_prefix.as_ref())
        {
            match crate::cache::push_remote_with_artifact_stores(
                store,
                prefix,
                cfg.remote_artifact_stores.as_ref(),
                &entry,
                &cfg.cache_root,
            )
            .await
            {
                Ok(wrote) => {
                    let payload = if wrote {
                        r#"{"remote":"published"}"#
                    } else {
                        r#"{"remote":"already_exists"}"#
                    };
                    journal.transition(
                        run_id,
                        stage_name,
                        attempt,
                        StageState::RefPublished,
                        payload,
                    )?;
                }
                Err(e) => {
                    // Remote push failure is non-fatal — log and continue.
                    debug!(
                        stage = %stage_name,
                        error = %e,
                        "remote cache push failed; continuing without remote ref"
                    );
                    journal.transition(
                        run_id,
                        stage_name,
                        attempt,
                        StageState::RefPublished,
                        r#"{"remote":"failed"}"#,
                    )?;
                }
            }
        } else {
            journal.transition(
                run_id,
                stage_name,
                attempt,
                StageState::RefPublished,
                r#"{"phase":"1-noop"}"#,
            )?;
        }
    } else {
        journal.transition(
            run_id,
            stage_name,
            attempt,
            StageState::RefPublished,
            r#"{"phase":"1-noop"}"#,
        )?;
    }
    journal.transition(
        run_id,
        stage_name,
        attempt,
        StageState::LockfileUpdated,
        r#"{"phase":"1-noop"}"#,
    )?;
    journal.transition(
        run_id,
        stage_name,
        attempt,
        StageState::Committed,
        &committed_payload(entry.duration_ms),
    )?;

    if let Some(m) = cfg.metrics.as_deref() {
        m.inc_workflow_stages_executed();
    }

    info!(
        stage = %stage_name,
        stage_hash = %stage_hash,
        duration_ms = entry.duration_ms,
        "workflow stage committed"
    );
    Ok(entry)
}

/// Delete a pre-existing out, if present. `NotFound` is fine —
/// it's the normal case on a first run.
fn remove_existing(path: &Path, stage: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_dir() {
                fs::remove_dir_all(path).map_err(|e| map_fs_err(stage, path, e))
            } else {
                fs::remove_file(path).map_err(|e| map_fs_err(stage, path, e))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(map_fs_err(stage, path, e)),
    }
}

struct SupervisedCommandOptions<'a> {
    log_path: &'a Path,
    timeout: Option<Duration>,
    stdout_capture_path: Option<&'a Path>,
    append_stdout_capture: bool,
}

async fn run_stage_commands(
    stage: &Stage,
    stage_hash: &StageHash,
    cfg: &ExecutorConfig,
    run_id: Uuid,
    cwd: Option<&Path>,
    sandbox_policy: Option<&HermeticSandboxPolicy>,
    log_path: &Path,
    stdout_capture_path: Option<&Path>,
) -> Result<SupervisorOutcome> {
    match &stage.cmd {
        Cmd::ShellList(commands) => {
            let deadline = stage.timeout.map(|timeout| Instant::now() + timeout);
            let mut last_outcome = None;
            for (index, shell) in commands.iter().enumerate() {
                let timeout = match deadline {
                    Some(deadline) => {
                        let Some(remaining) = deadline.checked_duration_since(Instant::now())
                        else {
                            return Ok(SupervisorOutcome {
                                exit_status: None,
                                signal: None,
                                timed_out: true,
                                stderr_tail: String::new(),
                            });
                        };
                        Some(remaining)
                    }
                    None => None,
                };
                let mut command = build_shell_command(shell, &stage.env, cwd, sandbox_policy)?;
                apply_checkpoint_environment(
                    &mut command,
                    cfg,
                    run_id,
                    stage.name.as_str(),
                    stage_hash,
                );
                let outcome = run_supervised_command(
                    command,
                    cfg,
                    SupervisedCommandOptions {
                        log_path,
                        timeout,
                        stdout_capture_path,
                        append_stdout_capture: index > 0,
                    },
                )
                .await?;
                if classify_stage_outcome(&outcome, stage.name.as_str(), sandbox_policy).is_err() {
                    return Ok(outcome);
                }
                last_outcome = Some(outcome);
            }
            last_outcome.ok_or_else(|| CrabError::Configuration {
                key: format!("stage '{}' cmd", stage.name),
                origin: "cmd list must contain at least one command".to_owned(),
            })
        }
        Cmd::Argv(_) | Cmd::Shell(_) => {
            let mut command = build_command(&stage.cmd, &stage.env, cwd, sandbox_policy)?;
            apply_checkpoint_environment(
                &mut command,
                cfg,
                run_id,
                stage.name.as_str(),
                stage_hash,
            );
            run_supervised_command(
                command,
                cfg,
                SupervisedCommandOptions {
                    log_path,
                    timeout: stage.timeout,
                    stdout_capture_path,
                    append_stdout_capture: false,
                },
            )
            .await
        }
    }
}

fn apply_checkpoint_environment(
    command: &mut Command,
    cfg: &ExecutorConfig,
    run_id: Uuid,
    stage: &str,
    stage_hash: &StageHash,
) {
    let (Some(control_dir), Some(token)) = (
        cfg.checkpoint_control_dir.as_ref(),
        cfg.checkpoint_token.as_ref(),
    ) else {
        return;
    };
    command.env("CRAB_WORKFLOW_CONTROL_DIR", control_dir);
    command.env(
        "CRAB_WORKFLOW_RUN_ID",
        cfg.checkpoint_run_id
            .as_deref()
            .map_or_else(|| run_id.to_string(), ToOwned::to_owned),
    );
    command.env("CRAB_WORKFLOW_STAGE", stage);
    command.env("CRAB_WORKFLOW_STAGE_HASH", format!("b3:{stage_hash}"));
    command.env("CRAB_WORKFLOW_TOKEN", token);
    if let Ok(executable) = std::env::current_exe() {
        command.env("CRAB_WORKFLOW_EXECUTABLE", executable);
    }
}

async fn run_supervised_command(
    command: Command,
    cfg: &ExecutorConfig,
    options: SupervisedCommandOptions<'_>,
) -> Result<SupervisorOutcome> {
    let mut supervisor = ChildSupervisor::new(command, options.log_path.to_path_buf())
        .with_graceful_shutdown(cfg.graceful_shutdown)
        .with_stderr_tail_bytes(cfg.stderr_tail_bytes)
        .with_output_mirroring(cfg.mirror_child_output);
    if let Some(path) = cfg.external_kill_path.clone() {
        supervisor = supervisor.with_external_kill_path(path);
    }
    if let Some(child_started) = cfg.child_started.clone() {
        let sink: EventSink = Arc::new(move |event| {
            if let SupervisorEvent::Started { pid } = event {
                child_started(pid);
            }
        });
        supervisor = supervisor.with_event_sink(sink);
    }
    if let Some(timeout) = options.timeout {
        supervisor = supervisor.with_timeout(timeout);
    }
    if let Some(path) = options.stdout_capture_path {
        supervisor = supervisor.with_stdout_capture(path.to_path_buf());
        if options.append_stdout_capture {
            supervisor = supervisor.with_stdout_capture_append();
        }
    }
    supervisor.run().await
}

/// Build the Tokio command for a `Cmd`. We deliberately avoid any
/// implicit shell — `Cmd::Shell` routes through the platform shell explicitly
/// so the stage hash differs from `Cmd::Argv(["sh","-c","…"])` only
/// by the canonicalized discriminator, matching the hasher.
fn build_command(
    cmd: &Cmd,
    env: &crate::stage::EnvSpec,
    cwd: Option<&Path>,
    sandbox_policy: Option<&HermeticSandboxPolicy>,
) -> Result<Command> {
    let mut command = match cmd {
        Cmd::Argv(argv) => build_argv_command(argv, sandbox_policy)?,
        Cmd::Shell(shell) => shell_command(shell, sandbox_policy)?,
        Cmd::ShellList(_) => {
            return Err(CrabError::Internal(
                "shell lists must be sequenced one command at a time".to_owned(),
            ));
        }
    };
    apply_command_context(&mut command, env, cwd);
    if let Some(policy) = sandbox_policy {
        command.env("TMPDIR", policy.temp_dir());
    }
    Ok(command)
}

fn build_shell_command(
    shell: &str,
    env: &crate::stage::EnvSpec,
    cwd: Option<&Path>,
    sandbox_policy: Option<&HermeticSandboxPolicy>,
) -> Result<Command> {
    let mut command = shell_command(shell, sandbox_policy)?;
    apply_command_context(&mut command, env, cwd);
    if let Some(policy) = sandbox_policy {
        command.env("TMPDIR", policy.temp_dir());
    }
    Ok(command)
}

fn build_argv_command(
    argv: &[String],
    sandbox_policy: Option<&HermeticSandboxPolicy>,
) -> Result<Command> {
    if let Some((program, rest)) = argv.split_first() {
        let mut command = if let Some(policy) = sandbox_policy {
            policy.wrap_command(program, rest)?
        } else {
            Command::new(program)
        };
        if sandbox_policy.is_none() {
            command.args(rest);
        }
        Ok(command)
    } else {
        Err(CrabError::Configuration {
            key: "workflow command argv".to_owned(),
            origin: "argv must contain a program".to_owned(),
        })
    }
}

fn shell_command(shell: &str, sandbox_policy: Option<&HermeticSandboxPolicy>) -> Result<Command> {
    let descriptor = platform_shell();
    let args = descriptor.args(shell);
    if let Some(policy) = sandbox_policy {
        return policy.wrap_command(descriptor.program, &args);
    }
    let mut command = Command::new(descriptor.program);
    command.args(args);
    Ok(command)
}

fn apply_command_context(command: &mut Command, env: &crate::stage::EnvSpec, cwd: Option<&Path>) {
    command.env_clear();
    for (k, v) in sanitize(env) {
        command.env(k, v);
    }
    command.stdin(Stdio::null());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
}

/// Execute an `on_cache_hit` hook command via the native platform shell when
/// the hook is shell-form. Returns
/// the process exit status. The hook inherits the stage's sanitized
/// environment and runs in the executor's working directory (repo
/// root for `crab run`, experiment tmpdir for `crab exp run`).
pub async fn execute_hook(
    cmd: &Cmd,
    env: &crate::stage::EnvSpec,
    cwd: Option<&Path>,
) -> Result<std::process::ExitStatus> {
    if let Cmd::ShellList(commands) = cmd {
        let mut last_status = None;
        for shell in commands {
            let mut command = build_shell_command(shell, env, cwd, None)?;
            command.stdout(Stdio::inherit());
            command.stderr(Stdio::inherit());
            let mut child = command.spawn().map_err(CrabError::Io)?;
            let status = child.wait().await.map_err(CrabError::Io)?;
            if !status.success() {
                return Ok(status);
            }
            last_status = Some(status);
        }
        if let Some(status) = last_status {
            return Ok(status);
        }
    }
    let mut command = build_command(cmd, env, cwd, None)?;
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    let mut child = command.spawn().map_err(CrabError::Io)?;
    child.wait().await.map_err(CrabError::Io)
}

/// Map a supervisor outcome onto the workflow error vocabulary.
/// A clean exit (code 0) returns `Ok`; everything else returns
/// the matching `CrabError::Stage*` variant so the caller's
/// retry layer can branch on it.
fn classify_stage_outcome(
    outcome: &SupervisorOutcome,
    stage: &str,
    sandbox_policy: Option<&HermeticSandboxPolicy>,
) -> Result<()> {
    if let Some(policy) = sandbox_policy
        && command_failed_without_timeout_or_signal(outcome)
        && let Some(path) = policy.violation_path(&outcome.stderr_tail)
    {
        return Err(CrabError::WorkflowHermeticViolation {
            stage: policy.stage().to_owned(),
            path,
        });
    }
    classify_exit(outcome, stage)
}

fn command_failed_without_timeout_or_signal(outcome: &SupervisorOutcome) -> bool {
    if outcome.timed_out || outcome.signal.is_some() {
        return false;
    }
    match outcome.exit_status.as_ref() {
        Some(status) => !status.success(),
        None => true,
    }
}

fn classify_exit(outcome: &SupervisorOutcome, stage: &str) -> Result<()> {
    if outcome.timed_out {
        // Elapsed-ms on the error lines up with the journal payload
        // but we don't have it directly — the supervisor doesn't
        // expose the deadline. Carry a sentinel of 0 here; the
        // error is primarily keyed on "timeout occurred" and the
        // journal's own timestamps preserve the real elapsed time.
        return Err(CrabError::StageExecTimeout {
            stage: stage.to_owned(),
            elapsed_ms: 0,
        });
    }
    if let Some(sig) = outcome.signal {
        return Err(CrabError::StageExecSignaled {
            stage: stage.to_owned(),
            signal: sig,
        });
    }
    match &outcome.exit_status {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(CrabError::StageExecFailed {
            stage: stage.to_owned(),
            exit_code: status.code().unwrap_or(-1),
        }),
        None => Err(CrabError::StageExecFailed {
            stage: stage.to_owned(),
            exit_code: -1,
        }),
    }
}

/// Walk declared outs, check filesystem kind matches the declared
/// `OutKind`, enforce size limits, and hash file outs.
///
/// Directory outs need the tree-manifest hasher that lands in
/// task 3.10 — phase 1 refuses them with a clear error pointing at
/// the phase-3 task so the user isn't left guessing.
async fn verify_and_hash_outs(stage: &Stage, cfg: &ExecutorConfig) -> Result<Vec<CachedOut>> {
    if stage.outs.len() > cfg.max_outs_per_stage {
        return Err(CrabError::StageOutCountExceeded {
            stage: stage.name.as_str().to_owned(),
            count: stage.outs.len(),
            limit: cfg.max_outs_per_stage,
        });
    }

    // Compute the effective base directory for resolving out paths.
    // When the stage has a wdir, outs are relative to wdir (which
    // itself is relative to the executor's working_dir / repo root).
    let base_dir = if let Some(wdir) = &stage.wdir {
        let base = cfg.working_dir.as_deref().unwrap_or_else(|| Path::new("."));
        Some(base.join(wdir))
    } else {
        cfg.working_dir.clone()
    };

    let mut total_entry_count = 0usize;
    let mut out = Vec::with_capacity(stage.outs.len());
    for declared in &stage.outs {
        if declared.is_external_url() {
            let cached = hash_external_url_out(stage.name.as_str(), declared, cfg).await?;
            if let Some(manifest) = cached.tree_manifest.as_ref() {
                let entry_count = manifest.len();
                if total_entry_count + entry_count > cfg.max_outs_per_stage {
                    return Err(CrabError::StageOutCountExceeded {
                        stage: stage.name.as_str().to_owned(),
                        count: total_entry_count + entry_count,
                        limit: cfg.max_outs_per_stage,
                    });
                }
                total_entry_count += entry_count;
            }
            out.push(cached);
            continue;
        }

        // Resolve the out path: if relative and wdir is set, resolve
        // against the wdir base. The stored path in CachedOut is
        // repo-relative (wdir/out_path).
        let (effective_path, lockfile_path) = if declared.path.is_absolute() {
            (declared.path.clone(), declared.path.clone())
        } else if let Some(ref base) = base_dir {
            let abs = base.join(&declared.path);
            let repo_rel = if let Some(wdir) = &stage.wdir {
                PathBuf::from(wdir).join(&declared.path)
            } else {
                declared.path.clone()
            };
            (abs, repo_rel)
        } else {
            (declared.path.clone(), declared.path.clone())
        };

        let meta = fs::symlink_metadata(&effective_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CrabError::StageOutMalformed {
                    stage: stage.name.as_str().to_owned(),
                    path: declared.path.clone(),
                    reason: "declared out missing after stage execution",
                }
            } else {
                map_fs_err(stage.name.as_str(), &effective_path, e)
            }
        })?;
        let ft = meta.file_type();

        if ft.is_symlink() {
            return Err(CrabError::StageOutMalformed {
                stage: stage.name.as_str().to_owned(),
                path: declared.path.clone(),
                reason: "symlinks are not allowed as stage outputs",
            });
        }

        match declared.kind {
            OutKind::File | OutKind::Stdout => {
                if !ft.is_file() {
                    return Err(CrabError::StageOutMalformed {
                        stage: stage.name.as_str().to_owned(),
                        path: declared.path.clone(),
                        reason: "expected a regular file",
                    });
                }
                let size = meta.len();
                let limit = declared.max_bytes.or(cfg.default_max_out_bytes);
                if let Some(l) = limit
                    && size > l
                {
                    return Err(CrabError::StageOutTooLarge {
                        stage: stage.name.as_str().to_owned(),
                        path: declared.path.clone(),
                        size,
                        limit: l,
                    });
                }
                let hash = hash_file_contents(&effective_path, stage.name.as_str())?;
                out.push(CachedOut {
                    path: lockfile_path.clone(),
                    kind: OutKind::File,
                    push: declared.push,
                    remote: declared.remote.clone(),
                    file_hash: format!("b3:{hash}"),
                    size,
                    mode: unix_mode(&meta),
                    tree_manifest: None,
                });
            }
            OutKind::Directory => {
                if !ft.is_dir() {
                    return Err(CrabError::StageOutMalformed {
                        stage: stage.name.as_str().to_owned(),
                        path: declared.path.clone(),
                        reason: "expected a directory",
                    });
                }
                // Directory out: compute a canonical tree-manifest
                // hash via `hasher::hash_directory`. Reject the same
                // non-regular entries inside the tree (symlinks,
                // FIFOs) that the executor rejects at the top
                // level — the tree hasher surfaces these as
                // `Io(InvalidInput)` so we translate to the
                // stage-scoped variant here.
                let tree = crate::hasher::hash_directory(&effective_path, true)
                    .map_err(|e| convert_tree_err(stage.name.as_str(), &effective_path, e))?;

                // Check entry count against the per-stage limit.
                // The design specifies that max_outs_per_stage applies
                // to the total entry count across all directory outs.
                let entry_count = tree.manifest.len();
                if total_entry_count + entry_count > cfg.max_outs_per_stage {
                    return Err(CrabError::StageOutCountExceeded {
                        stage: stage.name.as_str().to_owned(),
                        count: total_entry_count + entry_count,
                        limit: cfg.max_outs_per_stage,
                    });
                }
                total_entry_count += entry_count;

                let total_size: u64 = tree.manifest.iter().map(|e| e.size).sum();
                let limit = declared.max_bytes.or(cfg.default_max_out_bytes);
                if let Some(l) = limit
                    && total_size > l
                {
                    return Err(CrabError::StageOutTooLarge {
                        stage: stage.name.as_str().to_owned(),
                        path: declared.path.clone(),
                        size: total_size,
                        limit: l,
                    });
                }
                let hex = hex_of(&tree.hash);

                let manifest_entries = tree_manifest_entries(&tree.manifest);

                out.push(CachedOut {
                    path: lockfile_path.clone(),
                    kind: OutKind::Directory,
                    push: declared.push,
                    remote: declared.remote.clone(),
                    file_hash: format!("b3:{hex}"),
                    size: total_size,
                    mode: unix_mode(&meta),
                    tree_manifest: Some(manifest_entries),
                });
            }
        }
    }
    Ok(out)
}

async fn hash_external_url_out(
    stage_name: &str,
    declared: &Out,
    cfg: &ExecutorConfig,
) -> Result<CachedOut> {
    let raw_url = declared.path.to_string_lossy();
    let expanded = expand_external_url_out_alias(raw_url.as_ref(), &cfg.remote_aliases)?;
    let parsed = url::Url::parse(&expanded).map_err(|_| CrabError::StageOutMalformed {
        stage: stage_name.to_owned(),
        path: declared.path.clone(),
        reason: "invalid external output URL",
    })?;
    if matches!(
        parsed.scheme(),
        "ssh" | "sftp" | "hdfs" | "webhdfs" | "webdav" | "webdavs" | "gdrive" | "oss"
    ) {
        return Err(CrabError::StageRemoteExecutionUnsupported);
    }

    let (file_hash, size, mode, tree_manifest) = match declared.kind {
        OutKind::File => {
            let ((file_hash, size), mode) = if matches!(parsed.scheme(), "http" | "https") {
                (
                    fetch_http_external_url_out(&expanded).await?,
                    default_external_mode(),
                )
            } else if parsed.scheme() == "file" {
                let path = external_file_url_path(stage_name, declared, &parsed)?;
                let meta = fs::symlink_metadata(&path)
                    .map_err(|e| map_fs_err(stage_name, &declared.path, e))?;
                if meta.file_type().is_symlink() {
                    return Err(CrabError::StageOutMalformed {
                        stage: stage_name.to_owned(),
                        path: declared.path.clone(),
                        reason: "symlinks are not allowed as external outputs",
                    });
                }
                if !meta.is_file() {
                    return Err(CrabError::StageOutMalformed {
                        stage: stage_name.to_owned(),
                        path: declared.path.clone(),
                        reason: "expected a regular file",
                    });
                }
                (hash_local_external_file(&path)?, unix_mode(&meta))
            } else {
                let (store, location) = external_object_store(&parsed)?;
                (
                    hash_object_store_external_file(store.as_ref(), &location).await?,
                    default_external_mode(),
                )
            };
            enforce_out_size(stage_name, declared, size, cfg)?;
            (format!("b3:{}", hex_of(&file_hash)), size, mode, None)
        }
        OutKind::Directory => {
            if matches!(parsed.scheme(), "http" | "https") {
                return Err(CrabError::StageRemoteExecutionUnsupported);
            }
            if parsed.scheme() == "file" {
                let path = external_file_url_path(stage_name, declared, &parsed)?;
                let meta = fs::symlink_metadata(&path)
                    .map_err(|e| map_fs_err(stage_name, &declared.path, e))?;
                if !meta.is_dir() {
                    return Err(CrabError::StageOutMalformed {
                        stage: stage_name.to_owned(),
                        path: declared.path.clone(),
                        reason: "expected a directory",
                    });
                }
                let tree = crate::hasher::hash_directory(&path, true)
                    .map_err(|e| convert_tree_err(stage_name, &path, e))?;
                let size = tree.manifest.iter().map(|entry| entry.size).sum();
                enforce_out_size(stage_name, declared, size, cfg)?;
                let hash = format!("b3:{}", hex_of(&tree.hash));
                let manifest = tree_manifest_entries(&tree.manifest);
                return Ok(CachedOut {
                    path: declared.path.clone(),
                    kind: declared.kind,
                    push: false,
                    remote: None,
                    file_hash: hash,
                    size,
                    mode: unix_mode(&meta),
                    tree_manifest: Some(manifest),
                });
            }
            let (store, location) = external_object_store(&parsed)?;
            let (hash, size, manifest) =
                hash_external_directory_url_out(store.as_ref(), &location).await?;
            enforce_out_size(stage_name, declared, size, cfg)?;
            (hash, size, default_external_mode(), Some(manifest))
        }
        OutKind::Stdout => {
            return Err(CrabError::StageOutMalformed {
                stage: stage_name.to_owned(),
                path: declared.path.clone(),
                reason: "external URL outs cannot use kind: stdout",
            });
        }
    };

    Ok(CachedOut {
        path: declared.path.clone(),
        kind: declared.kind,
        push: false,
        remote: None,
        file_hash,
        size,
        mode,
        tree_manifest,
    })
}

fn external_object_store(
    parsed: &url::Url,
) -> Result<(Box<dyn object_store::ObjectStore>, ObjectPath)> {
    let options: Vec<(String, String)> = std::env::vars()
        .map(|(key, value)| (key.to_ascii_lowercase(), value))
        .collect();
    object_store::parse_url_opts(
        parsed,
        options
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
    .map_err(CrabError::Storage)
}

async fn fetch_http_external_url_out(url: &str) -> Result<([u8; 32], u64)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(format!("crab/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(external_url_network_error)?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(external_url_network_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(CrabError::Storage(object_store::Error::Generic {
            store: "workflow external output",
            source: Box::new(std::io::Error::other(format!(
                "GET {url} failed with HTTP {status}"
            ))),
        }));
    }
    let mut stream = response.bytes_stream();
    let mut hasher = blake3::Hasher::new();
    let mut size = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(external_url_network_error)?;
        size = size.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        hasher.update(&chunk);
    }
    Ok((*hasher.finalize().as_bytes(), size))
}

fn external_url_network_error(source: reqwest::Error) -> CrabError {
    CrabError::NetworkTransient(object_store::Error::Generic {
        store: "workflow external output",
        source: Box::new(source),
    })
}

fn external_file_url_path(stage_name: &str, declared: &Out, parsed: &url::Url) -> Result<PathBuf> {
    parsed
        .to_file_path()
        .map_err(|()| CrabError::StageOutMalformed {
            stage: stage_name.to_owned(),
            path: declared.path.clone(),
            reason: "file:// external output URL must resolve to a local filesystem path",
        })
}

fn hash_local_external_file(path: &Path) -> Result<([u8; 32], u64)> {
    let mut file = fs::File::open(path).map_err(CrabError::Io)?;
    let size = file.metadata().map_err(CrabError::Io)?.len();
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher).map_err(CrabError::Io)?;
    Ok((*hasher.finalize().as_bytes(), size))
}

async fn hash_object_store_external_file(
    store: &dyn object_store::ObjectStore,
    location: &ObjectPath,
) -> Result<([u8; 32], u64)> {
    let result = store.get(location).await?;
    let mut stream = result.into_stream();
    let mut hasher = blake3::Hasher::new();
    let mut size = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        size = size.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        hasher.update(&chunk);
    }
    Ok((*hasher.finalize().as_bytes(), size))
}

fn tree_manifest_entries(
    entries: &[crate::hasher::TreeEntry],
) -> Vec<crate::cache::TreeManifestEntry> {
    entries
        .iter()
        .map(|entry| {
            let kind = match entry.kind {
                crate::hasher::TreeEntryKind::File => "file",
                crate::hasher::TreeEntryKind::Directory => "dir",
            };
            let hash = if entry.kind == crate::hasher::TreeEntryKind::File {
                format!("b3:{}", hex_of(&entry.file_hash))
            } else {
                String::new()
            };
            crate::cache::TreeManifestEntry {
                path: entry.path.to_string_lossy().into_owned(),
                kind: kind.to_owned(),
                hash,
                size: entry.size,
                mode: entry.mode,
            }
        })
        .collect()
}

async fn hash_external_directory_url_out(
    store: &dyn object_store::ObjectStore,
    root: &ObjectPath,
) -> Result<(String, u64, Vec<crate::cache::TreeManifestEntry>)> {
    let root_prefix = root.as_ref().trim_end_matches('/');
    let root_child_prefix = if root_prefix.is_empty() {
        String::new()
    } else {
        format!("{root_prefix}/")
    };
    let mut stream = store.list(Some(root));
    let mut tree_entries = Vec::new();

    while let Some(item) = stream.next().await {
        let meta = item?;
        let key = meta.location.as_ref();
        let rel = if root_prefix.is_empty() {
            key
        } else if let Some(rel) = key.strip_prefix(&root_child_prefix) {
            rel
        } else {
            continue;
        };
        if rel.is_empty() || rel.ends_with('/') {
            continue;
        }

        let (file_hash, streamed_size) =
            hash_object_store_external_file(store, &meta.location).await?;
        let size = if streamed_size == meta.size {
            meta.size
        } else {
            return Err(CrabError::Internal(format!(
                "external object {} changed size while hashing: metadata {}, streamed {}",
                meta.location, meta.size, streamed_size
            )));
        };
        tree_entries.push(crate::hasher::TreeEntry {
            path: PathBuf::from(rel),
            kind: crate::hasher::TreeEntryKind::File,
            file_hash,
            size,
            mode: default_external_mode(),
        });
    }

    if tree_entries.is_empty() {
        return Err(CrabError::StageRemoteExecutionUnsupported);
    }

    tree_entries.sort_by(|a, b| a.path.cmp(&b.path));
    let total_size = tree_entries.iter().map(|entry| entry.size).sum();
    let tree_hash = crate::hasher::hash_tree_entries(&tree_entries);
    let manifest = tree_entries
        .iter()
        .map(|entry| crate::cache::TreeManifestEntry {
            path: entry.path.to_string_lossy().into_owned(),
            kind: "file".to_owned(),
            hash: format!("b3:{}", hex_of(&entry.file_hash)),
            size: entry.size,
            mode: entry.mode,
        })
        .collect();

    Ok((format!("b3:{}", hex_of(&tree_hash)), total_size, manifest))
}

fn enforce_out_size(
    stage_name: &str,
    declared: &Out,
    size: u64,
    cfg: &ExecutorConfig,
) -> Result<()> {
    let limit = declared.max_bytes.or(cfg.default_max_out_bytes);
    if let Some(limit) = limit
        && size > limit
    {
        return Err(CrabError::StageOutTooLarge {
            stage: stage_name.to_owned(),
            path: declared.path.clone(),
            size,
            limit,
        });
    }
    Ok(())
}

fn default_external_mode() -> u32 {
    0o644
}

fn verify_and_hash_metrics(stage: &Stage, cfg: &ExecutorConfig) -> Result<Vec<CachedOut>> {
    let base_dir = if let Some(wdir) = &stage.wdir {
        let base = cfg.working_dir.as_deref().unwrap_or_else(|| Path::new("."));
        Some(base.join(wdir))
    } else {
        cfg.working_dir.clone()
    };

    let mut metrics = Vec::with_capacity(stage.metrics.len());
    for declared in &stage.metrics {
        let (effective_path, lockfile_path) = if declared.is_absolute() {
            (declared.clone(), declared.clone())
        } else if let Some(ref base) = base_dir {
            let abs = base.join(declared);
            let repo_rel = if let Some(wdir) = &stage.wdir {
                PathBuf::from(wdir).join(declared)
            } else {
                declared.clone()
            };
            (abs, repo_rel)
        } else {
            (declared.clone(), declared.clone())
        };

        let meta = fs::symlink_metadata(&effective_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CrabError::StageOutMalformed {
                    stage: stage.name.as_str().to_owned(),
                    path: declared.clone(),
                    reason: "declared metric missing after stage execution",
                }
            } else {
                map_fs_err(stage.name.as_str(), &effective_path, e)
            }
        })?;

        if !meta.is_file() {
            return Err(CrabError::StageOutMalformed {
                stage: stage.name.as_str().to_owned(),
                path: declared.clone(),
                reason: "declared metric must be a regular file",
            });
        }

        let hash = hash_file_contents(&effective_path, stage.name.as_str())?;
        metrics.push(CachedOut {
            path: lockfile_path,
            kind: OutKind::File,
            push: true,
            remote: None,
            file_hash: format!("b3:{hash}"),
            size: meta.len(),
            mode: unix_mode(&meta),
            tree_manifest: None,
        });
    }
    Ok(metrics)
}

fn verify_and_hash_plots(stage: &Stage, cfg: &ExecutorConfig) -> Result<Vec<CachedOut>> {
    let base_dir = if let Some(wdir) = &stage.wdir {
        let base = cfg.working_dir.as_deref().unwrap_or_else(|| Path::new("."));
        Some(base.join(wdir))
    } else {
        cfg.working_dir.clone()
    };

    let mut total_entry_count = 0usize;
    let mut plots = Vec::with_capacity(stage.plots.len());
    for declared in &stage.plots {
        let (effective_path, lockfile_path) = if declared.is_absolute() {
            (declared.clone(), declared.clone())
        } else if let Some(ref base) = base_dir {
            let abs = base.join(declared);
            let repo_rel = if let Some(wdir) = &stage.wdir {
                PathBuf::from(wdir).join(declared)
            } else {
                declared.clone()
            };
            (abs, repo_rel)
        } else {
            (declared.clone(), declared.clone())
        };

        let meta = fs::symlink_metadata(&effective_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CrabError::StageOutMalformed {
                    stage: stage.name.as_str().to_owned(),
                    path: declared.clone(),
                    reason: "declared plot missing after stage execution",
                }
            } else {
                map_fs_err(stage.name.as_str(), &effective_path, e)
            }
        })?;
        let ft = meta.file_type();

        if ft.is_symlink() {
            return Err(CrabError::StageOutMalformed {
                stage: stage.name.as_str().to_owned(),
                path: declared.clone(),
                reason: "symlinks are not allowed as stage plots",
            });
        }

        if ft.is_file() {
            let hash = hash_file_contents(&effective_path, stage.name.as_str())?;
            plots.push(CachedOut {
                path: lockfile_path,
                kind: OutKind::File,
                push: true,
                remote: None,
                file_hash: format!("b3:{hash}"),
                size: meta.len(),
                mode: unix_mode(&meta),
                tree_manifest: None,
            });
            continue;
        }

        if ft.is_dir() {
            let tree = crate::hasher::hash_directory(&effective_path, true)
                .map_err(|e| convert_tree_err(stage.name.as_str(), &effective_path, e))?;
            let entry_count = tree.manifest.len();
            if total_entry_count + entry_count > cfg.max_outs_per_stage {
                return Err(CrabError::StageOutCountExceeded {
                    stage: stage.name.as_str().to_owned(),
                    count: total_entry_count + entry_count,
                    limit: cfg.max_outs_per_stage,
                });
            }
            total_entry_count += entry_count;

            let total_size: u64 = tree.manifest.iter().map(|e| e.size).sum();
            let hex = hex_of(&tree.hash);
            let manifest_entries: Vec<crate::cache::TreeManifestEntry> = tree
                .manifest
                .iter()
                .map(|e| {
                    let kind_str = match e.kind {
                        crate::hasher::TreeEntryKind::File => "file",
                        crate::hasher::TreeEntryKind::Directory => "dir",
                    };
                    let hash_str = if e.kind == crate::hasher::TreeEntryKind::File {
                        format!("b3:{}", hex_of(&e.file_hash))
                    } else {
                        String::new()
                    };
                    crate::cache::TreeManifestEntry {
                        path: e.path.to_string_lossy().into_owned(),
                        kind: kind_str.to_owned(),
                        hash: hash_str,
                        size: e.size,
                        mode: e.mode,
                    }
                })
                .collect();

            plots.push(CachedOut {
                path: lockfile_path,
                kind: OutKind::Directory,
                push: true,
                remote: None,
                file_hash: format!("b3:{hex}"),
                size: total_size,
                mode: unix_mode(&meta),
                tree_manifest: Some(manifest_entries),
            });
            continue;
        }

        return Err(CrabError::StageOutMalformed {
            stage: stage.name.as_str().to_owned(),
            path: declared.clone(),
            reason: "declared plot must be a regular file or directory",
        });
    }
    Ok(plots)
}

/// Whole-file blake3 hash. Matches the `blake3::Hasher` pattern
/// used by `git/clean.rs`; returned as lowercase hex without the
/// `b3:` prefix so callers can attach their own scheme tag.
fn hash_file_contents(path: &Path, stage: &str) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = fs::File::open(path).map_err(|e| map_fs_err(stage, path, e))?;
    std::io::copy(&mut file, &mut hasher).map_err(|e| map_fs_err(stage, path, e))?;
    let digest = hasher.finalize();
    Ok(digest.to_hex().to_string())
}

#[cfg(unix)]
fn unix_mode(meta: &std::fs::Metadata) -> u32 {
    // POSIX permission + special bits. `mode()` also returns the
    // file-type bits; mask to the bits we actually restore.
    meta.mode() & 0o7777
}

#[cfg(not(unix))]
fn unix_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

/// Lower-case hex encoding of a 32-byte digest. Matches the
/// `b3:<hex>` format the cache entry stores for file outs.
fn hex_of(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in bytes {
        // `write!` to a `String` is infallible; `unwrap` stays out
        // of the hot path and `let _` keeps the lint honest.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Translate a tree-hasher error into the stage-scoped variant the
/// executor surfaces. `hash_directory` uses `Io(InvalidInput)` for
/// non-regular, non-directory entries (symlinks, FIFOs, devices)
/// because it lacks the stage / path context the caller has; we
/// convert here so the error carries the declaring stage and the
/// top-level out path the user wrote in their yaml.
fn convert_tree_err(stage: &str, path: &Path, err: CrabError) -> CrabError {
    if let CrabError::Io(io_err) = &err
        && matches!(io_err.kind(), std::io::ErrorKind::InvalidInput)
    {
        let msg = io_err.to_string();
        let reason: &'static str = if msg.contains("symlink") {
            "directory contains a symlink entry"
        } else {
            "directory contains a non-regular entry"
        };
        return CrabError::StageOutMalformed {
            stage: stage.to_owned(),
            path: path.to_path_buf(),
            reason,
        };
    }
    err
}

/// Translate a low-level I/O error into a workflow error. ENOSPC /
/// EDQUOT surface as `StageDiskFull` so operators can distinguish
/// disk-full from a corrupt filesystem.
fn map_fs_err(stage: &str, path: &Path, err: std::io::Error) -> CrabError {
    if is_disk_full(&err) {
        return CrabError::StageDiskFull {
            stage: stage.to_owned(),
            path: path.to_path_buf(),
        };
    }
    CrabError::Io(err)
}

/// ENOSPC / EDQUOT detection. `StorageFull` is the stable enum; on
/// older platforms we fall back to raw errno comparison so we don't
/// miss a real out-of-disk condition just because stdlib hasn't
/// classified it yet.
fn is_disk_full(err: &std::io::Error) -> bool {
    if matches!(err.kind(), std::io::ErrorKind::StorageFull) {
        return true;
    }
    #[cfg(unix)]
    {
        if let Some(code) = err.raw_os_error() {
            return code == libc::ENOSPC || code == libc::EDQUOT;
        }
    }
    false
}

/// Persist the cache entry, translating disk-full into the
/// workflow error variant — the commit point MUST NOT leave a
/// half-written entry behind. Checks available disk space first
/// and emits a warning if below the headroom threshold.
fn write_entry(
    cache_root: &Path,
    entry: &StageCacheEntry,
    stage: &str,
    min_cache_headroom: u64,
) -> Result<()> {
    // Low disk warning: check available space before writing.
    if let Some(available) = crate::cache::available_disk_space(cache_root)
        && available < min_cache_headroom
    {
        warn!(
            available_bytes = available,
            threshold_bytes = min_cache_headroom,
            stage = %stage,
            "low disk: {available} bytes available, skipping cache write"
        );
        // Skip cache write but don't fail the stage.
        return Ok(());
    }

    match write_local(cache_root, entry) {
        Ok(()) => Ok(()),
        Err(CrabError::Io(e)) if is_disk_full(&e) => Err(CrabError::StageDiskFull {
            stage: stage.to_owned(),
            path: cache_root.to_path_buf(),
        }),
        Err(other) => Err(other),
    }
}

fn cached_cmd(cmd: &Cmd) -> CachedCmd {
    match cmd {
        Cmd::Argv(v) => CachedCmd::Argv { argv: v.clone() },
        Cmd::Shell(s) => CachedCmd::Shell { shell: s.clone() },
        Cmd::ShellList(commands) => CachedCmd::ShellList {
            commands: commands.clone(),
        },
    }
}

// --- Journal payload builders -------------------------------------
//
// Payloads are small structured JSON blobs — typed structs beat
// string concatenation here because the schema accidentally stays
// consistent across call sites and serde takes care of escaping.

#[derive(Serialize)]
struct CacheCheckedPayload<'a> {
    stage_hash: &'a str,
    hit: bool,
    hit_source: &'static str,
}

fn cache_checked_payload(hash: &StageHash, hit: bool, local: bool) -> String {
    let hex = hash.as_hex();
    let payload = CacheCheckedPayload {
        stage_hash: &hex,
        hit,
        hit_source: if hit {
            if local { "local" } else { "remote" }
        } else {
            "none"
        },
    };
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
}

#[derive(Serialize)]
struct RunningPayload<'a> {
    started_at: &'a str,
    attempt: u32,
}

fn running_payload(started_at: &str, attempt: u32) -> String {
    serde_json::to_string(&RunningPayload {
        started_at,
        attempt,
    })
    .unwrap_or_else(|_| "{}".to_owned())
}

#[derive(Serialize)]
struct HashedOut<'a> {
    path: &'a Path,
    hash: &'a str,
    size: u64,
}

#[derive(Serialize)]
struct HashedPayload<'a> {
    outs: Vec<HashedOut<'a>>,
}

fn hashed_payload(outs: &[CachedOut]) -> String {
    let payload = HashedPayload {
        outs: outs
            .iter()
            .map(|o| HashedOut {
                path: &o.path,
                hash: &o.file_hash,
                size: o.size,
            })
            .collect(),
    };
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
}

#[derive(Serialize)]
struct EntryWrittenPayload<'a> {
    stage_hash: &'a str,
}

fn entry_written_payload(hash: &StageHash) -> String {
    let hex = hash.as_hex();
    serde_json::to_string(&EntryWrittenPayload { stage_hash: &hex })
        .unwrap_or_else(|_| "{}".to_owned())
}

#[derive(Serialize)]
struct CommittedPayload {
    duration_ms: u64,
}

fn committed_payload(duration_ms: u64) -> String {
    serde_json::to_string(&CommittedPayload { duration_ms }).unwrap_or_else(|_| "{}".to_owned())
}

#[derive(Serialize)]
struct FailedPayload<'a> {
    reason: &'a str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    violation_stage: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    violation_path: Option<String>,
}

impl<'a> FailedPayload<'a> {
    fn new(reason: &'a str) -> Self {
        Self {
            reason,
            exit_code: None,
            signal: None,
            timed_out: false,
            violation_stage: None,
            violation_path: None,
        }
    }

    fn exit(reason: &'a str, exit_code: i32) -> Self {
        Self {
            exit_code: Some(exit_code),
            ..Self::new(reason)
        }
    }

    fn signal(reason: &'a str, signal: i32) -> Self {
        Self {
            signal: Some(signal),
            ..Self::new(reason)
        }
    }

    fn timed_out(reason: &'a str) -> Self {
        Self {
            timed_out: true,
            ..Self::new(reason)
        }
    }

    fn hermetic_violation(stage: &'a str, path: &Path) -> Self {
        Self {
            reason: "hermetic_violation",
            violation_stage: Some(stage),
            violation_path: Some(path.display().to_string()),
            ..Self::new("hermetic_violation")
        }
    }
}

/// Record a `Failed` transition on the best-effort path. If the
/// journal itself errors we log and swallow — we'd rather surface
/// the original exec error than mask it behind a DB write failure.
fn record_failure(journal: &Journal, run_id: Uuid, stage: &str, attempt: u32, err: &CrabError) {
    let payload = match err {
        CrabError::StageExecFailed { exit_code, .. } => {
            FailedPayload::exit("exit_nonzero", *exit_code)
        }
        CrabError::StageExecSignaled { signal, .. } => FailedPayload::signal("signal", *signal),
        CrabError::StageExecTimeout { .. } => FailedPayload::timed_out("timeout"),
        CrabError::StageDiskFull { .. } => FailedPayload::new("disk_full"),
        CrabError::StageOutMalformed { .. } => FailedPayload::new("out_malformed"),
        CrabError::StageOutTooLarge { .. } => FailedPayload::new("out_too_large"),
        CrabError::StageOutCountExceeded { .. } => FailedPayload::new("out_count_exceeded"),
        CrabError::WorkflowHermeticViolation { stage, path } => {
            FailedPayload::hermetic_violation(stage, path)
        }
        _ => FailedPayload::new("other"),
    };
    let body = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned());
    if let Err(e) = journal.transition(run_id, stage, attempt, StageState::Failed, &body) {
        debug!(
            stage = %stage,
            error = %e,
            "workflow executor: could not record Failed transition"
        );
    }
}

// --- Stage-out dep resolution -------------------------------------
//
// Multi-stage runs (design §"Execution Model" step 6a) resolve
// `Dep::StageOut { stage, out }` by consulting three sources in
// priority order:
//
//   1. In-memory `RunState` — stages already committed in this run.
//      Authoritative because the bytes they produced are what the
//      downstream stage will actually consume.
//   2. `Lockfile` — the previous run's record. Useful when the
//      producer stage has not re-run (cache hit on a sibling
//      branch, or a single-stage `crab run <downstream>` invocation
//      where the producer isn't part of the DAG selection).
//   3. Working tree — the fallback: read the file at the producer's
//      out path and blake3 it. Catches the edge case where the user
//      manually repaired an out between runs but hasn't re-committed
//      the lockfile yet.
//
// On a miss the caller surfaces `StageDepMissing` so the scheduler
// can stop the DAG with a clear pointer at the stage whose out is
// absent.

/// Resolve a `Dep::StageOut` reference to the 32-byte Blake3 of the
/// named out.
///
/// `None` means "no match in this source" — the composite resolver
/// chains sources together and only surfaces `StageDepMissing` after
/// all three miss. `Err` is reserved for genuine I/O or parse
/// failures (e.g. a working-tree read that hits `EACCES`); it does
/// *not* short-circuit the fallback chain.
pub trait DepResolver {
    fn resolve_stage_out(&self, stage: &StageName, out: &Path) -> Result<Option<[u8; 32]>>;
}

/// Composite resolver wired with all three sources. Missing sources
/// are allowed — e.g. the first stage in a DAG has no lockfile
/// entry, and a CI runner without a working tree copy has nothing
/// on disk. Any source that returns `None` yields to the next.
pub struct StageOutResolver<'a> {
    run_state: &'a RunState,
    lockfile: Option<&'a Lockfile>,
    repo_root: &'a Path,
}

impl<'a> StageOutResolver<'a> {
    /// Build a composite resolver. `repo_root` anchors working-tree
    /// lookups: a producer declaring `models/train.pkl` resolves to
    /// `<repo_root>/models/train.pkl` on disk.
    pub fn new(
        run_state: &'a RunState,
        lockfile: Option<&'a Lockfile>,
        repo_root: &'a Path,
    ) -> Self {
        Self {
            run_state,
            lockfile,
            repo_root,
        }
    }
}

impl DepResolver for StageOutResolver<'_> {
    fn resolve_stage_out(&self, stage: &StageName, out: &Path) -> Result<Option<[u8; 32]>> {
        // 1. Run state — this run's freshly-committed stages win.
        if let Some(entry) = self.run_state.get(stage)
            && let Some(cached) = entry.outs.iter().find(|o| o.path == *out)
        {
            return Ok(Some(parse_b3_file_hash(&cached.file_hash, stage.as_str())?));
        }

        // 2. Lockfile — prior run's record. `LockedOut.hash` is
        //    already a `[u8; 32]` so no parse step is needed.
        if let Some(lock) = self.lockfile
            && let Some(locked_stage) = lock.get(stage)
            && let Some(locked_out) = locked_stage.outs.iter().find(|o| o.path == *out)
        {
            return Ok(Some(locked_out.hash));
        }

        // 3. Working tree — hash whatever sits on disk at the
        //    declared path. A missing file here is the final miss,
        //    not an error: the caller maps it to `StageDepMissing`
        //    with the producer's identity attached.
        let abs = if out.is_absolute() {
            out.to_path_buf()
        } else {
            self.repo_root.join(out)
        };
        match fs::symlink_metadata(&abs) {
            Ok(meta) if meta.file_type().is_file() => Ok(Some(hash_file_for_resolver(&abs)?)),
            // Symlink / directory / fifo at the out path isn't a
            // resolvable regular-file hash. Yield to the miss
            // branch rather than guessing — the producer stage
            // would have rejected this itself at hash time.
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CrabError::Io(e)),
        }
    }
}

/// Resolve a multi-stage `Stage`'s deps to content hashes.
///
/// Mirrors the single-stage resolver in `cmd/run.rs` but also
/// handles `Dep::StageOut` by delegating to a [`DepResolver`].
/// Returns `StageDepMissing` when the stage-out reference doesn't
/// resolve in any of the resolver's tiers, matching the single-
/// stage error vocabulary so structured output and retry
/// classification stay uniform.
///
/// URL deps resolve from their declared `b3:` digest or by hashing
/// live HTTP(S), file, S3, GCS, or Azure bytes. Other remote /
/// cross-repo / SSH/HDFS-style URL / OCI deps still surface
/// `StageRemoteExecutionUnsupported` until those fetchers land.
pub fn resolve_dep_hashes(
    stage: &StageName,
    deps: &[Dep],
    repo_root: &Path,
    resolver: &dyn DepResolver,
) -> Result<std::collections::BTreeMap<String, [u8; 32]>> {
    resolve_dep_hashes_with_wdir(stage, deps, repo_root, resolver, None)
}

/// Resolve deps with optional `wdir` support.
///
/// When `wdir` is `Some`, relative `Dep::Path` entries are resolved
/// against `repo_root/wdir/` for filesystem operations and stored in
/// the returned map with the repo-relative key `wdir/dep_path`. This
/// matches DVC's behavior: the lockfile is unambiguous regardless of
/// which directory you read it from.
pub fn resolve_dep_hashes_with_wdir(
    stage: &StageName,
    deps: &[Dep],
    repo_root: &Path,
    resolver: &dyn DepResolver,
    wdir: Option<&Path>,
) -> Result<std::collections::BTreeMap<String, [u8; 32]>> {
    resolve_dep_hashes_with_wdir_remote_aliases(
        stage,
        deps,
        repo_root,
        resolver,
        wdir,
        &BTreeMap::new(),
    )
}

pub fn resolve_dep_hashes_with_wdir_remote_aliases(
    stage: &StageName,
    deps: &[Dep],
    repo_root: &Path,
    resolver: &dyn DepResolver,
    wdir: Option<&Path>,
    remote_aliases: &BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, [u8; 32]>> {
    let mut out = std::collections::BTreeMap::new();
    for dep in deps {
        match dep {
            Dep::Path(p) => {
                let abs = if p.is_absolute() {
                    p.clone()
                } else if let Some(wdir) = wdir {
                    repo_root.join(wdir).join(p)
                } else {
                    repo_root.join(p)
                };
                let meta = fs::metadata(&abs).map_err(|e| {
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
                    // Directory deps hash via the tree-manifest
                    // hasher. `hash_directory` rejects non-regular
                    // entries as `Io(InvalidInput)`; translate to
                    // the stage-scoped variant so structured output
                    // and retry classification see the producer.
                    let tree = crate::hasher::hash_directory(&abs, true).map_err(|e| match &e {
                        CrabError::Io(io_err)
                            if matches!(io_err.kind(), std::io::ErrorKind::InvalidInput) =>
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
                    })?;
                    let key = repo_relative_dep_key(p, wdir);
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
                let digest = hash_file_for_resolver(&abs)?;
                let key = repo_relative_dep_key(p, wdir);
                out.insert(key, digest);
            }
            Dep::StageOut {
                stage: producer,
                out: producer_out,
            } => {
                let Some(digest) = resolver.resolve_stage_out(producer, producer_out)? else {
                    return Err(CrabError::StageDepMissing {
                        stage: stage.as_str().to_owned(),
                        path: producer_out.clone(),
                    });
                };
                // Key the dep-hash map by `"<producer>:<out>"` so
                // two stage-outs with different producers but
                // identical relative paths don't collide in the
                // stage-hash input. The single-stage resolver keys
                // plain paths directly; stage-outs need the producer
                // prefix to stay unambiguous.
                let key = format!("{}:{}", producer.as_str(), producer_out.to_string_lossy());
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
            Dep::CrabRef { .. } | Dep::GitRef { .. } | Dep::OciImage { .. } => {
                return Err(CrabError::StageRemoteExecutionUnsupported);
            }
        }
    }
    Ok(out)
}

/// Resolve deps with optional `wdir` support and `--allow-missing`
/// semantics.
///
/// When `allow_missing` is `true` and a `Dep::Path` file is missing
/// from the workspace, the resolver checks the lockfile for a
/// recorded hash for that dep. If the lockfile has an entry, the
/// stage is treated as up-to-date (the lockfile hash is used as the
/// dep hash for comparison purposes). If there is no lockfile entry,
/// the stage is still "not-run" and errors normally with
/// `StageDepMissing`.
pub fn resolve_dep_hashes_with_wdir_allow_missing(
    stage: &StageName,
    deps: &[Dep],
    repo_root: &Path,
    resolver: &dyn DepResolver,
    wdir: Option<&Path>,
    allow_missing: bool,
    lockfile: Option<&crate::Lockfile>,
    lockfile_stage_name: Option<&crate::stage::StageName>,
) -> Result<std::collections::BTreeMap<String, [u8; 32]>> {
    resolve_dep_hashes_with_wdir_allow_missing_remote_aliases(
        stage,
        deps,
        repo_root,
        resolver,
        wdir,
        allow_missing,
        lockfile,
        lockfile_stage_name,
        &BTreeMap::new(),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "dep resolver mirrors the existing allow-missing entry point plus remote aliases"
)]
pub fn resolve_dep_hashes_with_wdir_allow_missing_remote_aliases(
    stage: &StageName,
    deps: &[Dep],
    repo_root: &Path,
    resolver: &dyn DepResolver,
    wdir: Option<&Path>,
    allow_missing: bool,
    lockfile: Option<&crate::Lockfile>,
    lockfile_stage_name: Option<&crate::stage::StageName>,
    remote_aliases: &BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, [u8; 32]>> {
    if !allow_missing {
        return resolve_dep_hashes_with_wdir_remote_aliases(
            stage,
            deps,
            repo_root,
            resolver,
            wdir,
            remote_aliases,
        );
    }

    let mut out = std::collections::BTreeMap::new();
    for dep in deps {
        match dep {
            Dep::Path(p) => {
                let abs = if p.is_absolute() {
                    p.clone()
                } else if let Some(wdir) = wdir {
                    repo_root.join(wdir).join(p)
                } else {
                    repo_root.join(p)
                };
                let meta_result = fs::metadata(&abs);
                match meta_result {
                    Ok(meta) => {
                        if meta.is_dir() {
                            let tree =
                                crate::hasher::hash_directory(&abs, true).map_err(
                                    |e| match &e {
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
                                    },
                                )?;
                            let key = repo_relative_dep_key(p, wdir);
                            out.insert(key, tree.hash);
                        } else if !meta.is_file() {
                            return Err(CrabError::StageDepMalformed {
                                stage: stage.as_str().to_owned(),
                                path: p.clone(),
                                reason: "phase 1 only supports regular-file deps",
                            });
                        } else {
                            let digest = hash_file_for_resolver(&abs)?;
                            let key = repo_relative_dep_key(p, wdir);
                            out.insert(key, digest);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // File is missing — check the lockfile for a
                        // recorded hash. If found, use it (the dep
                        // hasn't changed from the lockfile's
                        // perspective). If not found, error normally.
                        let key = repo_relative_dep_key(p, wdir);
                        let lockfile_hash = lockfile
                            .and_then(|lf| lockfile_stage_name.and_then(|sn| lf.get(sn)))
                            .and_then(|locked_stage| {
                                locked_stage
                                    .deps
                                    .iter()
                                    .find(|d| d.path.to_string_lossy() == key)
                                    .map(|d| d.hash)
                            });
                        match lockfile_hash {
                            Some(hash) => {
                                debug!(
                                    stage = %stage,
                                    dep = %key,
                                    "allow-missing: using lockfile hash for missing dep"
                                );
                                out.insert(key, hash);
                            }
                            None => {
                                return Err(CrabError::StageDepMissing {
                                    stage: stage.as_str().to_owned(),
                                    path: p.clone(),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        return Err(CrabError::Io(e));
                    }
                }
            }
            Dep::StageOut {
                stage: producer,
                out: producer_out,
            } => {
                let Some(digest) = resolver.resolve_stage_out(producer, producer_out)? else {
                    return Err(CrabError::StageDepMissing {
                        stage: stage.as_str().to_owned(),
                        path: producer_out.clone(),
                    });
                };
                let key = format!("{}:{}", producer.as_str(), producer_out.to_string_lossy());
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
            Dep::CrabRef { .. } | Dep::GitRef { .. } | Dep::OciImage { .. } => {
                return Err(CrabError::StageRemoteExecutionUnsupported);
            }
        }
    }
    Ok(out)
}

/// Parse a `"b3:<64-hex>"` string back into raw bytes. The cached
/// entry stores hashes as strings for lockfile-round-trip
/// simplicity; the resolver hands them out as `[u8; 32]` so the
/// stage hasher can mix them in without another decode.
fn parse_b3_file_hash(s: &str, stage: &str) -> Result<[u8; 32]> {
    let hex = s.strip_prefix("b3:").ok_or_else(|| {
        CrabError::Internal(format!(
            "stage '{stage}' cache entry hash '{s}' is missing the 'b3:' prefix"
        ))
    })?;
    if hex.len() != 64 {
        return Err(CrabError::Internal(format!(
            "stage '{stage}' cache entry hash '{s}' has {} hex chars, expected 64",
            hex.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = &hex[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(pair, 16).map_err(|_| {
            CrabError::Internal(format!(
                "stage '{stage}' cache entry hash '{s}' has non-hex chars"
            ))
        })?;
    }
    Ok(out)
}

/// Streaming Blake3 of an existing regular file. Same pattern the
/// single-stage dep resolver in `cmd/run.rs` uses — we duplicate it
/// here rather than share because this resolver lives in the
/// workflow crate and doesn't want a dep on `cmd`.
fn hash_file_for_resolver(path: &Path) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut file = fs::File::open(path).map_err(CrabError::Io)?;
    std::io::copy(&mut file, &mut hasher).map_err(CrabError::Io)?;
    Ok(*hasher.finalize().as_bytes())
}

/// Build the repo-relative key for a dep path. When `wdir` is set
/// and the path is relative, prepend `wdir/` so the lockfile stores
/// an unambiguous repo-relative path.
fn repo_relative_dep_key(p: &Path, wdir: Option<&Path>) -> String {
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

// --- Crash-injection harness --------------------------------------
//
// Compiled in only under the `crash-injection` cargo feature. The
// helper checks `CRAB_CRASH_AT=<StateName>` and calls
// `std::process::abort()` when the variable matches the just-recorded
// transition. It is exclusively a test affordance — exercising the
// resume path requires a real process-level crash, and `abort()` is
// the only way to reliably skip Drop handlers (SQLite WAL
// checkpoints, tempfile cleanup) the way a SIGKILL would.

/// Abort the process if `CRAB_CRASH_AT` names the state we just
/// transitioned through. Case-insensitive to keep CI scripts honest —
/// `Staged`, `staged`, `STAGED` all work.
///
/// Disabled in non-feature builds so the release binary carries zero
/// overhead and zero risk of an environment variable triggering an
/// unexpected abort.
#[cfg(feature = "crash-injection")]
fn maybe_crash_at(state: &str) {
    if let Ok(target) = std::env::var("CRAB_CRASH_AT")
        && target.eq_ignore_ascii_case(state)
    {
        eprintln!("CRAB_CRASH_AT={state}: aborting");
        std::process::abort();
    }
}

#[cfg(not(feature = "crash-injection"))]
fn maybe_crash_at(_state: &str) {}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use crate::metrics::test_support::TestWorkflowMetrics as Metrics;
    use crate::stage::{Cmd, Dep, Out, OutKind, Stage, StageName};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn make_resolved(stage: Stage) -> ResolvedStage {
        let cmd = stage.cmd.clone();
        let env = stage.env.clone();
        let outs = stage.outs.clone();
        ResolvedStage {
            stage,
            dep_hashes: BTreeMap::new(),
            params: BTreeMap::new(),
            env,
            cmd,
            outs,
        }
    }

    fn test_cfg(tmp: &TempDir) -> ExecutorConfig {
        ExecutorConfig {
            workflow_root: tmp.path().join(".crab/workflow"),
            cache_root: tmp.path().join(".crab/cache"),
            graceful_shutdown: Duration::from_millis(200),
            stderr_tail_bytes: 1024,
            mirror_child_output: true,
            external_kill_path: None,
            child_started: None,
            max_outs_per_stage: 10_000,
            default_max_out_bytes: None,
            host_fingerprint: "test-host".to_owned(),
            working_dir: None,
            metrics: None,
            cache_push: false,
            no_run_cache: false,
            no_commit: false,
            remote_store: None,
            remote_prefix: None,
            remote_primary_fallback_store: None,
            remote_primary_fallback_prefix: None,
            remote_artifact_stores: None,
            remote_aliases: BTreeMap::new(),
            min_cache_headroom: crate::cache::DEFAULT_MIN_CACHE_HEADROOM_BYTES,
            allow_checkpoints: false,
            checkpoint_control_dir: None,
            checkpoint_run_id: None,
            checkpoint_token: None,
        }
    }

    /// Same as [`test_cfg`] but wires in a fresh [`Metrics`] so
    /// counter-assertions can read back the bumped values.
    fn test_cfg_with_metrics(tmp: &TempDir) -> (ExecutorConfig, Arc<Metrics>) {
        let metrics = Arc::new(Metrics::new());
        let cfg = ExecutorConfig {
            metrics: Some(metrics.clone()),
            ..test_cfg(tmp)
        };
        (cfg, metrics)
    }

    fn open_journal(tmp: &TempDir, run_id: Uuid) -> Journal {
        let path = tmp
            .path()
            .join(".crab/workflow/runs")
            .join(run_id.to_string())
            .join("journal.db");
        let j = Journal::open(&path).unwrap();
        j.insert_run_start(run_id, env!("CARGO_PKG_VERSION"), "test-host")
            .unwrap();
        j
    }

    fn prepare_stage(j: &Journal, run_id: Uuid, stage_name: &str) {
        j.insert_stage_start(run_id, stage_name).unwrap();
        j.transition(run_id, stage_name, 1, StageState::Resolved, "{}")
            .unwrap();
    }

    #[test]
    fn failure_payload_records_hermetic_violation_details() {
        let tmp = TempDir::new().unwrap();
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "copy");
        let err = CrabError::WorkflowHermeticViolation {
            stage: "copy".to_owned(),
            path: PathBuf::from("outside.txt"),
        };

        record_failure(&j, run_id, "copy", 1, &err);

        let row = j.latest_stage_row(run_id, "copy").unwrap().unwrap();
        let payload: serde_json::Value = serde_json::from_str(&row.payload_json).unwrap();
        assert_eq!(payload["reason"], "hermetic_violation");
        assert_eq!(payload["violation_stage"], "copy");
        assert_eq!(payload["violation_path"], "outside.txt");
        assert_eq!(payload["timed_out"], false);
        assert_eq!(payload["exit_code"], serde_json::Value::Null);
        assert_eq!(payload["signal"], serde_json::Value::Null);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn happy_path_commits_entry() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"hello").unwrap();
        let dest = tmp.path().join("dest.txt");

        let stage = Stage {
            outs: vec![Out::new(dest.clone(), OutKind::File)],
            ..Stage::new(
                StageName::parse("copy").unwrap(),
                Cmd::Argv(vec![
                    "/bin/cp".into(),
                    src.to_string_lossy().into(),
                    dest.to_string_lossy().into(),
                ]),
            )
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "copy");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1).await.unwrap();

        assert_eq!(entry.stage_name, "copy");
        assert_eq!(entry.outs.len(), 1);
        assert_eq!(entry.outs[0].path, dest);
        assert_eq!(entry.outs[0].size, 5);
        assert!(entry.outs[0].file_hash.starts_with("b3:"));

        // On-disk entry readable and matches in memory.
        let reread = read_local(&cfg.cache_root, &entry.stage_hash)
            .unwrap()
            .unwrap();
        assert_eq!(reread, entry);

        // Journal should show no non-committed stages.
        assert!(j.stages_not_committed(run_id).unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shell_list_runs_each_command_with_fresh_shell_state() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();

        let stage = Stage {
            outs: vec![Out::new(PathBuf::from("pwd.txt"), OutKind::File)],
            ..Stage::new(
                StageName::parse("steps").unwrap(),
                Cmd::ShellList(vec![
                    "cd subdir".to_owned(),
                    "printf root > pwd.txt".to_owned(),
                ]),
            )
        };
        let resolved = make_resolved(stage);
        let mut cfg = test_cfg(&tmp);
        cfg.working_dir = Some(tmp.path().to_path_buf());
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "steps");

        run_local(&resolved, &cfg, &j, run_id, 1).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("pwd.txt")).unwrap(),
            "root"
        );
        assert!(!tmp.path().join("subdir/pwd.txt").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_zero_exit_maps_to_stage_exec_failed() {
        let tmp = TempDir::new().unwrap();
        let stage = Stage::new(
            StageName::parse("fail").unwrap(),
            Cmd::Shell("exit 7".into()),
        );
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "fail");

        let err = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect_err("non-zero exit must fail");
        match err {
            CrabError::StageExecFailed { exit_code, .. } => assert_eq!(exit_code, 7),
            other => panic!("wrong variant: {other}"),
        }

        // Failed transition recorded.
        let rows = j.stages_not_committed(run_id).unwrap();
        assert!(rows.is_empty(), "Failed is terminal; no rows should remain");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signal_termination_maps_to_stage_exec_signaled() {
        let tmp = TempDir::new().unwrap();
        let stage = Stage::new(
            StageName::parse("killed").unwrap(),
            // Self-signal SIGKILL to avoid a test-time flake around
            // waiting for an external signal source.
            Cmd::Shell("kill -9 $$".into()),
        );
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "killed");

        let err = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect_err("signalled child must fail");
        assert!(
            matches!(err, CrabError::StageExecSignaled { signal: 9, .. }),
            "wrong variant: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_maps_to_stage_exec_timeout() {
        let tmp = TempDir::new().unwrap();
        let mut stage = Stage::new(
            StageName::parse("timeout").unwrap(),
            Cmd::Shell("trap '' TERM; sleep 30".into()),
        );
        stage.timeout = Some(Duration::from_millis(150));
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "timeout");

        let err = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect_err("timeout must fail");
        assert!(
            matches!(err, CrabError::StageExecTimeout { .. }),
            "wrong variant: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_out_reports_stage_out_malformed() {
        let tmp = TempDir::new().unwrap();
        let stage = Stage {
            outs: vec![Out::new(
                tmp.path().join("never_created.txt"),
                OutKind::File,
            )],
            ..Stage::new(
                StageName::parse("nomaterials").unwrap(),
                Cmd::Shell("true".into()),
            )
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "nomaterials");

        let err = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect_err("missing out must fail");
        assert!(
            matches!(err, CrabError::StageOutMalformed { reason, .. } if reason.contains("missing")),
            "wrong variant or message: {err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn symlink_out_reports_stage_out_malformed() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target.txt");
        std::fs::write(&target, b"x").unwrap();
        let symlink_path = tmp.path().join("out.txt");
        // Create the symlink before the run; stage declares it as
        // a file out and the hasher must reject it.
        symlink(&target, &symlink_path).unwrap();

        let stage = Stage {
            // persist: true so the pre-exec cleanup doesn't delete
            // the symlink we care about testing. Tests the rejection
            // that happens at hash time, not cleanup time.
            persist: true,
            outs: vec![Out::new(symlink_path.clone(), OutKind::File)],
            ..Stage::new(StageName::parse("sym").unwrap(), Cmd::Shell("true".into()))
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "sym");

        let err = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect_err("symlink out must fail");
        assert!(
            matches!(err, CrabError::StageOutMalformed { reason, .. } if reason.contains("symlink")),
            "wrong variant: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn journal_records_happy_path_transitions_in_order() {
        use rusqlite::Connection;

        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"x").unwrap();
        let dest = tmp.path().join("dest.txt");

        let stage = Stage {
            outs: vec![Out::new(dest.clone(), OutKind::File)],
            ..Stage::new(
                StageName::parse("cp").unwrap(),
                Cmd::Argv(vec![
                    "/bin/cp".into(),
                    src.to_string_lossy().into(),
                    dest.to_string_lossy().into(),
                ]),
            )
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "cp");

        run_local(&resolved, &cfg, &j, run_id, 1).await.unwrap();

        // Read the final state; the stage should be `Committed`.
        let db_path = cfg
            .workflow_root
            .join("runs")
            .join(run_id.to_string())
            .join("journal.db");
        let conn = Connection::open(&db_path).unwrap();
        let state: i64 = conn
            .query_row(
                "SELECT state FROM stage_runs WHERE stage_name = 'cp' AND attempt = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            state,
            i64::from(StageState::Committed.sql_tag()),
            "final state must be Committed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_exec_cleanup_deletes_existing_out() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest.txt");
        std::fs::write(&dest, b"stale").unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"fresh").unwrap();

        let stage = Stage {
            outs: vec![Out::new(dest.clone(), OutKind::File)],
            ..Stage::new(
                StageName::parse("refresh").unwrap(),
                Cmd::Argv(vec![
                    "/bin/cp".into(),
                    src.to_string_lossy().into(),
                    dest.to_string_lossy().into(),
                ]),
            )
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "refresh");

        run_local(&resolved, &cfg, &j, run_id, 1).await.unwrap();

        // The stale file was deleted pre-spawn; the copy wrote fresh
        // bytes to the clean path.
        assert_eq!(std::fs::read(&dest).unwrap(), b"fresh");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn persist_true_keeps_existing_out_alive_during_cleanup() {
        // If persist is true, the executor must not delete the
        // pre-existing file. We use `true` (no-op command) to
        // verify the file survives execution — then the stage
        // fails hashing because the no-op didn't produce anything
        // matching the declared out. Either the file is there at
        // hash time (test passes this assertion) or the test flakes.
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest.txt");
        std::fs::write(&dest, b"keep-me").unwrap();

        let stage = Stage {
            persist: true,
            outs: vec![Out::new(dest.clone(), OutKind::File)],
            ..Stage::new(StageName::parse("keep").unwrap(), Cmd::Shell("true".into()))
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "keep");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("stage with persisted pre-existing out should hash cleanly");
        assert_eq!(entry.outs[0].size, b"keep-me".len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), b"keep-me");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cache_hit_skips_child_execution() {
        // Populate the cache by running once, then run again with a
        // sentinel command that would fail if executed. The second
        // call must hit and succeed without re-running.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"y").unwrap();
        let dest = tmp.path().join("dest.txt");

        let cfg = test_cfg(&tmp);

        let first_stage = Stage {
            outs: vec![Out::new(dest.clone(), OutKind::File)],
            ..Stage::new(
                StageName::parse("hitme").unwrap(),
                Cmd::Argv(vec![
                    "/bin/cp".into(),
                    src.to_string_lossy().into(),
                    dest.to_string_lossy().into(),
                ]),
            )
        };
        let resolved_first = make_resolved(first_stage.clone());
        let run_1 = Uuid::now_v7();
        let j1 = open_journal(&tmp, run_1);
        prepare_stage(&j1, run_1, "hitme");

        let entry_first = run_local(&resolved_first, &cfg, &j1, run_1, 1)
            .await
            .unwrap();

        // Second run: same resolved stage (same hash), but replace
        // cmd with a failure so we can prove the child never ran.
        let mut second = first_stage;
        second.cmd = Cmd::Shell("exit 99".into());
        let mut resolved_second = make_resolved(second);
        // Keep the hash identical to the first attempt by borrowing
        // the original command back in the hash inputs. (In real use
        // the ResolvedStage always reflects what actually runs — we
        // force it here to exercise the hit branch deterministically.)
        resolved_second.cmd = resolved_first.cmd.clone();

        let run_2 = Uuid::now_v7();
        let j2 = open_journal(&tmp, run_2);
        prepare_stage(&j2, run_2, "hitme");
        let entry_second = run_local(&resolved_second, &cfg, &j2, run_2, 1)
            .await
            .expect("cache hit must not execute the failing cmd");
        assert_eq!(entry_second.stage_hash, entry_first.stage_hash);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nondeterministic_stage_runs_instead_of_cache_hit() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.txt");
        let dest = tmp.path().join("dest.txt");
        let marker = tmp.path().join("marker.log");
        std::fs::write(&src, b"y").unwrap();

        let mut stage = Stage {
            deps: vec![Dep::Path(src.clone())],
            outs: vec![Out::new(dest.clone(), OutKind::File)],
            ..Stage::new(
                StageName::parse("always").unwrap(),
                Cmd::Shell(format!(
                    "cp '{}' '{}' && printf 'run\\n' >> '{}'",
                    src.display(),
                    dest.display(),
                    marker.display()
                )),
            )
        };
        stage.nondeterministic = true;

        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);

        let run_1 = Uuid::now_v7();
        let j1 = open_journal(&tmp, run_1);
        prepare_stage(&j1, run_1, "always");
        let first = run_local(&resolved, &cfg, &j1, run_1, 1).await.unwrap();

        let run_2 = Uuid::now_v7();
        let j2 = open_journal(&tmp, run_2);
        prepare_stage(&j2, run_2, "always");
        let second = run_local(&resolved, &cfg, &j2, run_2, 1).await.unwrap();

        assert_eq!(first.stage_hash, second.stage_hash);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "run\nrun\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uncached_out_disables_stage_cache_reads_and_writes() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"y").unwrap();
        let dest = tmp.path().join("dest.txt");
        let mut uncached_out = Out::new(dest.clone(), OutKind::File);
        uncached_out.cache = false;

        let cfg = test_cfg(&tmp);
        let first_stage = Stage {
            outs: vec![uncached_out],
            ..Stage::new(
                StageName::parse("nocache").unwrap(),
                Cmd::Argv(vec![
                    "/bin/cp".into(),
                    src.to_string_lossy().into(),
                    dest.to_string_lossy().into(),
                ]),
            )
        };
        let resolved_first = make_resolved(first_stage.clone());
        let run_1 = Uuid::now_v7();
        let j1 = open_journal(&tmp, run_1);
        prepare_stage(&j1, run_1, "nocache");

        let entry_first = run_local(&resolved_first, &cfg, &j1, run_1, 1)
            .await
            .unwrap();

        assert_eq!(entry_first.outs.len(), 1);
        assert!(
            read_local(&cfg.cache_root, &entry_first.stage_hash)
                .unwrap()
                .is_none()
        );

        let mut second = first_stage;
        second.cmd = Cmd::Shell("exit 99".into());
        let mut resolved_second = make_resolved(second);
        resolved_second.cmd = resolved_first.cmd.clone();
        let run_2 = Uuid::now_v7();
        let j2 = open_journal(&tmp, run_2);
        prepare_stage(&j2, run_2, "nocache");

        let err = run_local(&resolved_second, &cfg, &j2, run_2, 1)
            .await
            .expect_err("cache-disabled stage must execute instead of hitting cache");
        match err {
            CrabError::StageExecFailed { exit_code, .. } => assert_eq!(exit_code, 99),
            other => panic!("wrong variant: {other}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_false_out_keeps_local_cache_but_skips_remote_publish() {
        let tmp = TempDir::new().unwrap();
        let out_rel = PathBuf::from("local-only.txt");
        let out_path = tmp.path().join(&out_rel);
        let mut local_only = Out::new(out_rel.clone(), OutKind::File);
        local_only.push = false;

        let stage = Stage {
            outs: vec![local_only],
            ..Stage::new(
                StageName::parse("localonly").unwrap(),
                Cmd::Shell("printf x > local-only.txt".into()),
            )
        };
        let resolved = make_resolved(stage);
        let remote = Arc::new(crate::WorkflowStore::new(Arc::new(
            object_store::memory::InMemory::new(),
        )));
        let mut cfg = test_cfg(&tmp);
        cfg.working_dir = Some(tmp.path().to_path_buf());
        cfg.cache_push = true;
        cfg.remote_store = Some(remote.clone());
        cfg.remote_prefix = Some("org/repo".into());

        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "localonly");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("push=false output should still execute and write local cache");

        assert!(
            read_local(&cfg.cache_root, &entry.stage_hash)
                .unwrap()
                .is_some()
        );
        assert!(!entry.remote_push_enabled());
        assert!(!entry.outs[0].push);

        std::fs::remove_file(&out_path).unwrap();
        let pulled = crate::cache::pull_remote(
            remote.as_ref(),
            "org/repo",
            &entry.stage_hash,
            &cfg.cache_root,
            Some(tmp.path()),
        )
        .await
        .unwrap();
        assert!(pulled.is_none());
        assert!(!out_path.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn declared_output_remote_is_recorded_in_cache_entry() {
        let tmp = TempDir::new().unwrap();
        let mut out = Out::new(PathBuf::from("model.pkl"), OutKind::File);
        out.remote = Some("cold-storage".to_owned());

        let stage = Stage {
            outs: vec![out],
            ..Stage::new(
                StageName::parse("remoteout").unwrap(),
                Cmd::Shell("printf model > model.pkl".into()),
            )
        };
        let resolved = make_resolved(stage);
        let mut cfg = test_cfg(&tmp);
        cfg.working_dir = Some(tmp.path().to_path_buf());

        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "remoteout");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("stage with output remote should execute");

        assert_eq!(entry.outs[0].remote.as_deref(), Some("cold-storage"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_cache_pull_uses_primary_fallback_after_selected_miss() {
        let tmp = TempDir::new().unwrap();
        let out_rel = PathBuf::from("dest.txt");
        let out_path = tmp.path().join(&out_rel);
        std::fs::write(&out_path, b"r").unwrap();

        let stage = Stage {
            outs: vec![Out::new(out_rel.clone(), OutKind::File)],
            ..Stage::new(
                StageName::parse("remotehit").unwrap(),
                Cmd::Shell("exit 99".into()),
            )
        };
        let resolved = make_resolved(stage);
        let stage_hash = crate::hasher::compute(&resolved);
        let out_hash = *blake3::hash(b"r").as_bytes();
        let mut remote_entry = make_cache_entry("remotehit", &out_rel, out_hash);
        remote_entry.stage_hash = stage_hash;
        let (mut cfg, metrics) = test_cfg_with_metrics(&tmp);

        let replica = Arc::new(crate::WorkflowStore::new(Arc::new(
            object_store::memory::InMemory::new(),
        )));
        let primary = Arc::new(crate::WorkflowStore::new(Arc::new(
            object_store::memory::InMemory::new(),
        )));
        crate::cache::push_remote(primary.as_ref(), "org/repo", &remote_entry, &cfg.cache_root)
            .await
            .unwrap();
        std::fs::remove_file(&out_path).unwrap();

        cfg.working_dir = Some(tmp.path().to_path_buf());
        cfg.remote_store = Some(replica);
        cfg.remote_prefix = Some("org/repo".into());
        cfg.remote_primary_fallback_store = Some(primary);
        cfg.remote_primary_fallback_prefix = Some("org/repo".into());

        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "remotehit");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("primary fallback remote hit must skip the failing command");

        assert_eq!(entry.stage_hash, stage_hash);
        assert_eq!(std::fs::read(&out_path).unwrap(), b"r");
        let snap = metrics.snapshot();
        assert_eq!(snap.workflow_stage_cache_hits_remote, 1);
        assert_eq!(snap.workflow_stages_executed, 0);
    }

    #[test]
    fn rfc3339_millis_produces_expected_shape() {
        let s = now_rfc3339_millis();
        // 2026-04-27T14:23:11.083Z is 24 chars; shape must match.
        assert_eq!(s.len(), 24, "{s}");
        assert!(s.ends_with('Z'));
        assert!(s.contains('T'));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn miss_path_bumps_workflow_stages_executed() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"hello").unwrap();
        let dest = tmp.path().join("dest.txt");

        let stage = Stage {
            outs: vec![Out::new(dest.clone(), OutKind::File)],
            ..Stage::new(
                StageName::parse("copy").unwrap(),
                Cmd::Argv(vec![
                    "/bin/cp".into(),
                    src.to_string_lossy().into(),
                    dest.to_string_lossy().into(),
                ]),
            )
        };
        let resolved = make_resolved(stage);
        let (cfg, metrics) = test_cfg_with_metrics(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "copy");

        run_local(&resolved, &cfg, &j, run_id, 1).await.unwrap();

        let snap = metrics.snapshot();
        assert_eq!(snap.workflow_stages_executed, 1);
        assert_eq!(snap.workflow_stage_cache_hits_local, 0);
        assert_eq!(snap.workflow_stages_failed, 0);
        assert_eq!(snap.workflow_stage_retry_attempts, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hit_path_bumps_workflow_stage_cache_hits_local() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"y").unwrap();
        let dest = tmp.path().join("dest.txt");

        let (cfg, metrics) = test_cfg_with_metrics(&tmp);

        let first_stage = Stage {
            outs: vec![Out::new(dest.clone(), OutKind::File)],
            ..Stage::new(
                StageName::parse("hitme").unwrap(),
                Cmd::Argv(vec![
                    "/bin/cp".into(),
                    src.to_string_lossy().into(),
                    dest.to_string_lossy().into(),
                ]),
            )
        };
        let resolved_first = make_resolved(first_stage.clone());
        let run_1 = Uuid::now_v7();
        let j1 = open_journal(&tmp, run_1);
        prepare_stage(&j1, run_1, "hitme");
        run_local(&resolved_first, &cfg, &j1, run_1, 1)
            .await
            .unwrap();

        // Second run: force the hash to stay identical so the
        // executor takes the hit branch.
        let mut second = first_stage;
        second.cmd = Cmd::Shell("exit 99".into());
        let mut resolved_second = make_resolved(second);
        resolved_second.cmd = resolved_first.cmd.clone();

        let run_2 = Uuid::now_v7();
        let j2 = open_journal(&tmp, run_2);
        prepare_stage(&j2, run_2, "hitme");
        run_local(&resolved_second, &cfg, &j2, run_2, 1)
            .await
            .unwrap();

        let snap = metrics.snapshot();
        assert_eq!(snap.workflow_stages_executed, 1, "first run is a miss");
        assert_eq!(
            snap.workflow_stage_cache_hits_local, 1,
            "second run must count as a local hit"
        );
        assert_eq!(snap.workflow_stage_cache_hits_remote, 0);
        assert_eq!(snap.workflow_stages_failed, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failure_path_bumps_workflow_stages_failed() {
        let tmp = TempDir::new().unwrap();
        let stage = Stage::new(
            StageName::parse("fail").unwrap(),
            Cmd::Shell("exit 1".into()),
        );
        let resolved = make_resolved(stage);
        let (cfg, metrics) = test_cfg_with_metrics(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "fail");

        let err = run_local(&resolved, &cfg, &j, run_id, 1).await;
        assert!(err.is_err());

        let snap = metrics.snapshot();
        assert_eq!(snap.workflow_stages_failed, 1);
        assert_eq!(snap.workflow_stages_executed, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retry_attempt_bumps_workflow_stage_retry_attempts() {
        // `attempt > 1` is the signal for "this is a retry": the
        // outer retry loop owns the loop, the executor just reports.
        // We seed a second attempt row directly via SQL because the
        // journal API today only exposes `insert_stage_start` for
        // attempt 1; the retry-loop wiring that creates attempt > 1
        // rows lands in task 1.8.
        use rusqlite::{Connection, params};

        let tmp = TempDir::new().unwrap();
        let stage = Stage::new(
            StageName::parse("retry").unwrap(),
            Cmd::Shell("exit 1".into()),
        );
        let resolved = make_resolved(stage);
        let (cfg, metrics) = test_cfg_with_metrics(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        j.insert_stage_start(run_id, "retry").unwrap();

        // Seed an attempt=2 row in state `Resolved` so the executor's
        // first `transition(CacheChecked)` succeeds and the miss-path
        // flow can proceed to produce a genuine `Failed` transition.
        let db_path = cfg
            .workflow_root
            .join("runs")
            .join(run_id.to_string())
            .join("journal.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO stage_runs(
                run_id, stage_name, attempt, state, updated_at, payload_json
             ) VALUES (?1, ?2, 2, ?3, ?4, '{}')",
            params![
                run_id.to_string(),
                "retry",
                i64::from(StageState::Resolved.sql_tag()),
                0_i64,
            ],
        )
        .unwrap();

        let _ = run_local(&resolved, &cfg, &j, run_id, 2).await;

        let snap = metrics.snapshot();
        assert_eq!(snap.workflow_stage_retry_attempts, 1);
        assert_eq!(snap.workflow_stages_failed, 1);
    }

    /// Multi-attempt invariant: invoking `run_local` three times
    /// against the same `(run_id, stage_name)` with distinct
    /// `attempt` values yields three distinct journal rows, one per
    /// attempt. This mirrors what the retry layer (task 1.8 /
    /// phase-3 loop wiring) will orchestrate end-to-end; we exercise
    /// the executor-level invariant here so the row-per-attempt
    /// guarantee doesn't regress while the loop remains unwritten.
    ///
    /// All three attempts run the same failing command so we can
    /// avoid hash drift across attempts (each attempt re-computes
    /// `stage_hash` from its resolved inputs — using distinct
    /// commands across attempts would make the third attempt's
    /// `CacheChecked` miss semantics tangled with the shared run_id
    /// without adding coverage the `happy_path_commits_entry` test
    /// already provides). The important invariant is the row-count
    /// and per-attempt terminal state.
    #[tokio::test(flavor = "multi_thread")]
    async fn three_attempts_land_three_stage_run_rows() {
        use rusqlite::{Connection, params};

        let tmp = TempDir::new().unwrap();
        let stage = Stage::new(
            StageName::parse("flaky").unwrap(),
            Cmd::Shell("exit 1".into()),
        );
        let resolved = make_resolved(stage);
        let (cfg, metrics) = test_cfg_with_metrics(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);

        // Attempt 1 — insert_stage_start seeds the row at Resolving,
        // then transition to Resolved so the executor can drive the
        // state machine forward.
        j.insert_stage_start(run_id, "flaky").unwrap();
        j.transition(run_id, "flaky", 1, StageState::Resolved, "{}")
            .unwrap();

        let _ = run_local(&resolved, &cfg, &j, run_id, 1).await;

        // Attempts 2 and 3 need their rows seeded directly — the
        // journal's public API only exposes `insert_stage_start` for
        // attempt 1 (matching the production call site in
        // `cmd/run.rs`; the retry loop that creates attempt > 1 rows
        // lands in phase 3). Mirror the SQL the retry loop will emit.
        let db_path = cfg
            .workflow_root
            .join("runs")
            .join(run_id.to_string())
            .join("journal.db");
        let conn = Connection::open(&db_path).unwrap();
        for attempt in [2_u32, 3_u32] {
            conn.execute(
                "INSERT INTO stage_runs(
                    run_id, stage_name, attempt, state, updated_at, payload_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, '{}')",
                params![
                    run_id.to_string(),
                    "flaky",
                    i64::from(attempt),
                    i64::from(StageState::Resolved.sql_tag()),
                    0_i64,
                ],
            )
            .unwrap();

            let _ = run_local(&resolved, &cfg, &j, run_id, attempt).await;
        }

        // Three distinct rows exist for this run.
        let rows: Vec<(i64, i64)> = conn
            .prepare(
                "SELECT attempt, state FROM stage_runs
                 WHERE run_id = ?1 AND stage_name = ?2
                 ORDER BY attempt",
            )
            .unwrap()
            .query_map(params![run_id.to_string(), "flaky"], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();

        assert_eq!(rows.len(), 3, "one row per attempt; got {rows:?}");
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows[1].0, 2);
        assert_eq!(rows[2].0, 3);
        let failed = i64::from(StageState::Failed.sql_tag());
        for (attempt, state) in &rows {
            assert_eq!(
                *state, failed,
                "attempt {attempt} must end in Failed (tag {failed}), got {state}"
            );
        }

        // Metrics: attempts 2 and 3 count as retries (attempt > 1);
        // all three failed so the failure counter reads 3.
        let snap = metrics.snapshot();
        assert_eq!(
            snap.workflow_stage_retry_attempts, 2,
            "retry counter must bump for attempts 2 and 3"
        );
        assert_eq!(
            snap.workflow_stages_failed, 3,
            "every attempt failed so the stage-failure counter must read 3"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn none_metrics_is_a_no_op() {
        // The executor must still work end-to-end when callers
        // decline to wire in a Metrics arc — phase 1 unit tests do
        // this and we don't want to force a metrics dep on them.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"x").unwrap();
        let dest = tmp.path().join("dest.txt");

        let stage = Stage {
            outs: vec![Out::new(dest.clone(), OutKind::File)],
            ..Stage::new(
                StageName::parse("nomx").unwrap(),
                Cmd::Argv(vec![
                    "/bin/cp".into(),
                    src.to_string_lossy().into(),
                    dest.to_string_lossy().into(),
                ]),
            )
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp); // metrics = None
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "nomx");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("executor must not require metrics");
        assert_eq!(entry.stage_name, "nomx");
    }

    // --- Dep resolver tests ---------------------------------------
    //
    // Each test covers one fallback tier in isolation, then a final
    // priority test proves the ordering matches the design spec.

    fn make_cache_entry(stage: &str, out_path: &Path, hash: [u8; 32]) -> StageCacheEntry {
        use crate::cache::{CachedCmd, CachedOut, ENTRY_SCHEMA_VERSION};

        StageCacheEntry {
            schema_version: ENTRY_SCHEMA_VERSION,
            stage_hash: StageHash([0u8; 32]),
            stage_name: stage.to_owned(),
            cmd: CachedCmd::Shell {
                shell: "true".into(),
            },
            outs: vec![CachedOut {
                path: out_path.to_path_buf(),
                kind: OutKind::File,
                push: true,
                remote: None,
                file_hash: format!("b3:{}", hex_of(&hash)),
                size: 1,
                mode: 0o644,
                tree_manifest: None,
            }],
            metrics: Vec::new(),
            plots: Vec::new(),
            executed_at: "1970-01-01T00:00:00.000Z".into(),
            duration_ms: 0,
            exec_id: None,
            attempts: 1,
            host_fingerprint: "test".into(),
        }
    }

    fn hex_of(bytes: &[u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in bytes {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    #[test]
    fn resolver_hits_in_memory_run_state_first() {
        let tmp = TempDir::new().unwrap();
        let out_path = PathBuf::from("artifact.bin");
        let hash = [0xAAu8; 32];

        let mut state = RunState::new();
        let name = StageName::parse("producer").unwrap();
        state.insert(name.clone(), make_cache_entry("producer", &out_path, hash));

        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let resolved = resolver.resolve_stage_out(&name, &out_path).unwrap();
        assert_eq!(resolved, Some(hash));
    }

    #[test]
    fn resolver_falls_back_to_lockfile_when_run_state_empty() {
        use std::collections::BTreeMap;

        let tmp = TempDir::new().unwrap();
        let out_path = PathBuf::from("artifact.bin");
        let hash = [0xBBu8; 32];

        // Build a lockfile by upserting from a synthetic cache entry.
        let entry = make_cache_entry("producer", &out_path, hash);
        let mut lock = Lockfile::new();
        lock.upsert(&entry, Vec::new(), BTreeMap::new(), BTreeMap::new())
            .unwrap();

        let state = RunState::new();
        let name = StageName::parse("producer").unwrap();
        let resolver = StageOutResolver::new(&state, Some(&lock), tmp.path());
        let resolved = resolver.resolve_stage_out(&name, &out_path).unwrap();
        assert_eq!(resolved, Some(hash));
    }

    #[test]
    fn resolver_falls_back_to_working_tree_when_neither_source_matches() {
        let tmp = TempDir::new().unwrap();
        let out_path = PathBuf::from("artifact.bin");
        let bytes = b"on-disk bytes";
        std::fs::write(tmp.path().join(&out_path), bytes).unwrap();
        let expected: [u8; 32] = *blake3::hash(bytes).as_bytes();

        let state = RunState::new();
        let name = StageName::parse("producer").unwrap();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let resolved = resolver.resolve_stage_out(&name, &out_path).unwrap();
        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn resolver_returns_none_when_no_source_has_the_out() {
        let tmp = TempDir::new().unwrap();
        let state = RunState::new();
        let name = StageName::parse("producer").unwrap();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let resolved = resolver
            .resolve_stage_out(&name, Path::new("never-produced.bin"))
            .unwrap();
        assert_eq!(
            resolved, None,
            "missing on every tier must be None so the caller can emit StageDepMissing"
        );
    }

    #[test]
    fn resolver_prefers_run_state_over_lockfile_and_working_tree() {
        use std::collections::BTreeMap;

        let tmp = TempDir::new().unwrap();
        let out_path = PathBuf::from("artifact.bin");
        let memory_hash = [0x11u8; 32];
        let lockfile_hash = [0x22u8; 32];

        // Working tree has yet a third set of bytes — the resolver
        // must prefer run state and never even look here.
        std::fs::write(tmp.path().join(&out_path), b"working tree").unwrap();

        let mut lock = Lockfile::new();
        let entry_for_lock = make_cache_entry("producer", &out_path, lockfile_hash);
        lock.upsert(
            &entry_for_lock,
            Vec::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();

        let mut state = RunState::new();
        let name = StageName::parse("producer").unwrap();
        state.insert(
            name.clone(),
            make_cache_entry("producer", &out_path, memory_hash),
        );

        let resolver = StageOutResolver::new(&state, Some(&lock), tmp.path());
        let resolved = resolver.resolve_stage_out(&name, &out_path).unwrap();
        assert_eq!(resolved, Some(memory_hash), "run state must win");
    }

    #[test]
    fn resolver_prefers_lockfile_over_working_tree() {
        use std::collections::BTreeMap;

        let tmp = TempDir::new().unwrap();
        let out_path = PathBuf::from("artifact.bin");
        let lockfile_hash = [0x33u8; 32];

        std::fs::write(tmp.path().join(&out_path), b"different bytes on disk").unwrap();

        let mut lock = Lockfile::new();
        let entry_for_lock = make_cache_entry("producer", &out_path, lockfile_hash);
        lock.upsert(
            &entry_for_lock,
            Vec::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();

        let state = RunState::new();
        let name = StageName::parse("producer").unwrap();
        let resolver = StageOutResolver::new(&state, Some(&lock), tmp.path());
        let resolved = resolver.resolve_stage_out(&name, &out_path).unwrap();
        assert_eq!(
            resolved,
            Some(lockfile_hash),
            "lockfile must win over working-tree fallback"
        );
    }

    // --- resolve_dep_hashes multi-stage integration ---------------

    #[test]
    fn resolve_dep_hashes_mixes_path_and_stage_out() {
        let tmp = TempDir::new().unwrap();

        // A Path dep that exists on disk.
        let input_rel = PathBuf::from("input.csv");
        std::fs::write(tmp.path().join(&input_rel), b"csv bytes").unwrap();
        let input_expected: [u8; 32] = *blake3::hash(b"csv bytes").as_bytes();

        // A StageOut dep resolved from in-memory run state.
        let out_rel = PathBuf::from("model.pkl");
        let stage_out_hash = [0x77u8; 32];
        let producer = StageName::parse("train").unwrap();
        let mut state = RunState::new();
        state.insert(
            producer.clone(),
            make_cache_entry("train", &out_rel, stage_out_hash),
        );

        let consumer = StageName::parse("evaluate").unwrap();
        let deps = vec![
            Dep::Path(input_rel.clone()),
            Dep::StageOut {
                stage: producer.clone(),
                out: out_rel.clone(),
            },
        ];

        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let hashes = resolve_dep_hashes(&consumer, &deps, tmp.path(), &resolver).unwrap();

        assert_eq!(hashes.len(), 2);
        // Path dep keyed by the plain relative path.
        assert_eq!(hashes.get("input.csv"), Some(&input_expected));
        // Stage-out keyed as `<producer>:<out>` so stage-outs can't
        // collide with a same-named path dep.
        assert_eq!(hashes.get("train:model.pkl"), Some(&stage_out_hash));
    }

    #[test]
    fn resolve_dep_hashes_stage_out_miss_raises_stage_dep_missing() {
        let tmp = TempDir::new().unwrap();
        let state = RunState::new();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let consumer = StageName::parse("evaluate").unwrap();
        let deps = vec![Dep::StageOut {
            stage: StageName::parse("never-ran").unwrap(),
            out: PathBuf::from("ghost.bin"),
        }];

        let err = resolve_dep_hashes(&consumer, &deps, tmp.path(), &resolver)
            .expect_err("missing stage-out on every tier must fail");
        match err {
            CrabError::StageDepMissing { stage, path } => {
                assert_eq!(stage, "evaluate");
                assert_eq!(path, PathBuf::from("ghost.bin"));
            }
            other => panic!("wrong variant: {other}"),
        }
    }

    #[test]
    fn resolve_dep_hashes_accepts_pinned_url_digest() {
        let tmp = TempDir::new().unwrap();
        let state = RunState::new();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let consumer = StageName::parse("evaluate").unwrap();
        let deps = vec![Dep::Url {
            url: "https://example.com/file".into(),
            digest: Some(format!("b3:{}", "12".repeat(32))),
        }];

        let hashes = resolve_dep_hashes(&consumer, &deps, tmp.path(), &resolver)
            .expect("pinned URL deps should resolve from digest");
        assert_eq!(hashes.get("https://example.com/file"), Some(&[0x12; 32]));
    }

    #[test]
    fn resolve_dep_hashes_accepts_unpinned_http_url_dep() {
        let tmp = TempDir::new().unwrap();
        let state = RunState::new();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let consumer = StageName::parse("evaluate").unwrap();
        let url = crate::stage::test_support::serve_http_body_once(b"executor-url-body");
        let deps = vec![Dep::Url {
            url: url.clone(),
            digest: None,
        }];

        let hashes = resolve_dep_hashes(&consumer, &deps, tmp.path(), &resolver)
            .expect("HTTP URL deps should resolve from fetched bytes");
        assert_eq!(
            hashes.get(&url),
            Some(blake3::hash(b"executor-url-body").as_bytes())
        );
    }

    #[test]
    fn resolve_dep_hashes_refuses_unpinned_unsupported_url_deps() {
        let tmp = TempDir::new().unwrap();
        let state = RunState::new();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let consumer = StageName::parse("evaluate").unwrap();
        let deps = vec![Dep::Url {
            url: "ssh://example.com/file".into(),
            digest: None,
        }];

        let err = resolve_dep_hashes(&consumer, &deps, tmp.path(), &resolver)
            .expect_err("unsupported URL deps need provider fetch support");
        assert!(matches!(err, CrabError::StageRemoteExecutionUnsupported));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn directory_out_is_hashed_via_tree_manifest() {
        // Stage declares a directory out; the command populates it
        // with two files. The executor must hash the tree, not fail
        // on the kind, and produce a `b3:` prefixed hash matching
        // `hasher::hash_directory`.
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("outdir");

        let stage = Stage {
            outs: vec![Out::new(dest.clone(), OutKind::Directory)],
            ..Stage::new(
                StageName::parse("mkdir").unwrap(),
                Cmd::Shell(format!(
                    "mkdir -p {d} && echo a > {d}/a.txt && echo b > {d}/b.txt",
                    d = dest.to_string_lossy(),
                )),
            )
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "mkdir");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("directory out should hash successfully");
        assert_eq!(entry.outs.len(), 1);
        assert_eq!(entry.outs[0].kind, OutKind::Directory);
        assert!(entry.outs[0].file_hash.starts_with("b3:"));

        // Recomputing the tree hash directly should match what the
        // executor recorded on the cache entry.
        let direct = crate::hasher::hash_directory(&dest, true).unwrap();
        let expected = format!("b3:{}", hex_of(&direct.hash));
        assert_eq!(entry.outs[0].file_hash, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uncached_absolute_out_is_hashed_as_external_local_output() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("external.txt");
        let mut external = Out::new(dest.clone(), OutKind::File);
        external.cache = false;
        external.push = false;

        let stage = Stage {
            outs: vec![external],
            ..Stage::new(
                StageName::parse("external").unwrap(),
                Cmd::Shell(format!("printf external > {}", dest.to_string_lossy())),
            )
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "external");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("absolute uncached out should hash successfully");
        assert_eq!(entry.outs.len(), 1);
        assert_eq!(entry.outs[0].path, dest);
        assert_eq!(
            entry.outs[0].file_hash,
            format!("b3:{}", blake3::hash(b"external").to_hex())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uncached_http_url_out_is_hashed_as_external_output() {
        let tmp = TempDir::new().unwrap();
        let url = crate::stage::test_support::serve_http_body_once(b"external-url");
        let mut external = Out::new(PathBuf::from(&url), OutKind::File);
        external.cache = false;
        external.push = false;

        let stage = Stage {
            outs: vec![external],
            ..Stage::new(
                StageName::parse("external_url").unwrap(),
                Cmd::Shell("true".into()),
            )
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "external_url");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("HTTP URL output should hash successfully");

        assert_eq!(entry.outs.len(), 1);
        assert_eq!(entry.outs[0].path, PathBuf::from(&url));
        assert!(!entry.outs[0].push);
        assert_eq!(
            entry.outs[0].file_hash,
            format!("b3:{}", blake3::hash(b"external-url").to_hex())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uncached_remote_alias_url_out_is_hashed_as_external_output() {
        let tmp = TempDir::new().unwrap();
        let base_url = crate::stage::test_support::serve_http_body_once(b"alias-external-url");
        let base_url = base_url.trim_end_matches("data.bin").to_owned();
        let mut external = Out::new(PathBuf::from("remote://exports/model.bin"), OutKind::File);
        external.cache = false;
        external.push = false;

        let stage = Stage {
            outs: vec![external],
            ..Stage::new(
                StageName::parse("external_alias_url").unwrap(),
                Cmd::Shell("true".into()),
            )
        };
        let resolved = make_resolved(stage);
        let mut cfg = test_cfg(&tmp);
        cfg.remote_aliases = BTreeMap::from([("exports".to_owned(), base_url)]);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "external_alias_url");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("remote alias URL output should hash successfully");

        assert_eq!(entry.outs.len(), 1);
        assert_eq!(
            entry.outs[0].path,
            PathBuf::from("remote://exports/model.bin")
        );
        assert_eq!(
            entry.outs[0].file_hash,
            format!("b3:{}", blake3::hash(b"alias-external-url").to_hex())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uncached_remote_alias_local_out_is_hashed_as_external_output() {
        let tmp = TempDir::new().unwrap();
        let external_root = tempfile::TempDir::new().unwrap();
        let dest = external_root.path().join("model.bin");
        let mut external = Out::new(PathBuf::from("remote://exports/model.bin"), OutKind::File);
        external.cache = false;
        external.push = false;

        let stage = Stage {
            outs: vec![external],
            ..Stage::new(
                StageName::parse("external_alias_local").unwrap(),
                Cmd::Shell(format!("printf alias-local > {}", dest.to_string_lossy())),
            )
        };
        let resolved = make_resolved(stage);
        let mut cfg = test_cfg(&tmp);
        cfg.remote_aliases = BTreeMap::from([(
            "exports".to_owned(),
            external_root.path().to_string_lossy().into_owned(),
        )]);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "external_alias_local");

        let entry = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect("remote alias local output should hash successfully");

        assert_eq!(entry.outs.len(), 1);
        assert_eq!(
            entry.outs[0].file_hash,
            format!("b3:{}", blake3::hash(b"alias-local").to_hex())
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn directory_out_with_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("outdir");
        std::fs::create_dir_all(&dest).unwrap();
        let target = tmp.path().join("outside.txt");
        std::fs::write(&target, b"t").unwrap();

        // `persist: true` so the pre-exec cleanup leaves the symlink
        // alone; the hasher must reject it at verify time.
        symlink(&target, dest.join("link")).unwrap();

        let stage = Stage {
            persist: true,
            outs: vec![Out::new(dest.clone(), OutKind::Directory)],
            ..Stage::new(
                StageName::parse("symdir").unwrap(),
                Cmd::Shell("true".into()),
            )
        };
        let resolved = make_resolved(stage);
        let cfg = test_cfg(&tmp);
        let run_id = Uuid::now_v7();
        let j = open_journal(&tmp, run_id);
        prepare_stage(&j, run_id, "symdir");

        let err = run_local(&resolved, &cfg, &j, run_id, 1)
            .await
            .expect_err("symlink inside directory out must fail");
        assert!(
            matches!(err, CrabError::StageOutMalformed { reason, .. } if reason.contains("symlink")),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn resolve_dep_hashes_handles_directory_dep() {
        // Directory dep should hash via the tree manifest; the
        // resulting digest must match `hash_directory` run directly
        // against the same directory.
        let tmp = TempDir::new().unwrap();
        let dep_dir = tmp.path().join("inputs");
        std::fs::create_dir_all(&dep_dir).unwrap();
        std::fs::write(dep_dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dep_dir.join("b.txt"), b"world").unwrap();

        let state = RunState::new();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let consumer = StageName::parse("reader").unwrap();
        let deps = vec![Dep::Path(dep_dir.clone())];

        let hashes = resolve_dep_hashes(&consumer, &deps, tmp.path(), &resolver)
            .expect("directory dep should resolve");
        assert_eq!(hashes.len(), 1);

        let direct = crate::hasher::hash_directory(&dep_dir, true).unwrap();
        let (_, got) = hashes.into_iter().next().unwrap();
        assert_eq!(got, direct.hash);
    }

    #[test]
    fn resolve_dep_hashes_with_wdir_resolves_relative_to_wdir() {
        // When wdir is set, relative dep paths should be resolved
        // against repo_root/wdir/ and stored with the wdir/ prefix.
        let tmp = TempDir::new().unwrap();
        let wdir = Path::new("training");
        let training_dir = tmp.path().join("training");
        std::fs::create_dir_all(&training_dir).unwrap();
        std::fs::write(training_dir.join("data.csv"), b"col1,col2\n1,2\n").unwrap();

        let state = RunState::new();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let consumer = StageName::parse("train").unwrap();
        let deps = vec![Dep::Path(PathBuf::from("data.csv"))];

        let hashes =
            resolve_dep_hashes_with_wdir(&consumer, &deps, tmp.path(), &resolver, Some(wdir))
                .expect("wdir-relative dep should resolve");
        assert_eq!(hashes.len(), 1);

        // The key should be repo-relative: "training/data.csv"
        let key = hashes.keys().next().unwrap();
        assert_eq!(key, "training/data.csv");

        // The hash should match hashing the actual file
        let actual_hash = {
            let mut hasher = blake3::Hasher::new();
            let mut file = std::fs::File::open(training_dir.join("data.csv")).unwrap();
            std::io::copy(&mut file, &mut hasher).unwrap();
            *hasher.finalize().as_bytes()
        };
        assert_eq!(hashes["training/data.csv"], actual_hash);
    }

    #[test]
    fn resolve_dep_hashes_with_wdir_none_behaves_like_original() {
        // When wdir is None, behavior should be identical to the
        // original resolve_dep_hashes.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("input.txt"), b"hello").unwrap();

        let state = RunState::new();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let consumer = StageName::parse("stage").unwrap();
        let deps = vec![Dep::Path(PathBuf::from("input.txt"))];

        let hashes_original = resolve_dep_hashes(&consumer, &deps, tmp.path(), &resolver)
            .expect("original should resolve");
        let hashes_wdir =
            resolve_dep_hashes_with_wdir(&consumer, &deps, tmp.path(), &resolver, None)
                .expect("wdir=None should resolve");

        assert_eq!(hashes_original, hashes_wdir);
    }

    #[test]
    fn resolve_dep_hashes_with_wdir_absolute_path_unchanged() {
        // Absolute dep paths should not be affected by wdir.
        let tmp = TempDir::new().unwrap();
        let abs_file = tmp.path().join("absolute.txt");
        std::fs::write(&abs_file, b"absolute content").unwrap();

        let state = RunState::new();
        let resolver = StageOutResolver::new(&state, None, tmp.path());
        let consumer = StageName::parse("stage").unwrap();
        let deps = vec![Dep::Path(abs_file.clone())];

        let hashes = resolve_dep_hashes_with_wdir(
            &consumer,
            &deps,
            tmp.path(),
            &resolver,
            Some(Path::new("subdir")),
        )
        .expect("absolute path should resolve regardless of wdir");

        // Key should be the absolute path, not prefixed with wdir
        let key = hashes.keys().next().unwrap();
        assert_eq!(key, &abs_file.to_string_lossy().into_owned());
    }
}
