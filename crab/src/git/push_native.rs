//! Native discovery and lock handoff for the canonical push pipeline.
//!
//! This owner performs incremental Git discovery, acquires per-ref locks, and
//! hands the precomputed walk to [`crate::git::push::PushPipeline`]. Chunk
//! classification, packing, uploads, dependency proof, and manifest commit all
//! remain in that single state machine.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::metrics::Metrics;
use crate::core::perf_phase::PerfPhaseSink;
use crate::git::discover;
use crate::git::progress::NativePushProgress;
use crate::git::push::{
    PrePopulatedWalk, PushConfig, PushFailureStage, PushLockLease, PushResult,
    acquire_push_lock_leases, duplicate_destination_result, release_push_lock_leases,
    run_push_batch_with_locks,
};
use crate::git::push_staging::PushStaging;
use crate::git::push_state::PushState;
use crate::git::remote_helper::PushSpec;
use crate::git::walk::PointerBlob;
use crate::storage::StoreLayout;
use crate::storage::store::Store;
use crab_staging::StagingAreaReadOnly;

pub(crate) const MIRROR_GIT_ONLY_ENV: &str = "CRAB_INTERNAL_MIRROR_GIT_ONLY";
pub(crate) const MIRROR_PLAN_ID_ENV: &str = "CRAB_INTERNAL_MIRROR_PLAN_ID";

/// Configuration for the native push pipeline.
#[derive(Debug, Clone)]
pub struct NativePushConfig {
    /// Base push config (upload concurrency, lock TTL, heartbeat, etc.).
    pub push: PushConfig,
    /// Whether to use incremental walk (push-state lookup).
    pub incremental: bool,
    /// Whether to enable progress output (ticker and phase reports).
    pub progress: bool,
    /// Whether to print the final human summary.
    pub emit_summary: bool,
    /// Whether to use ANSI color codes in progress output.
    pub color: bool,
    /// Whether to show per-file and per-xorb detail.
    pub verbose: bool,
    /// Output mode for structured progress. When `Jsonl` with a stderr
    /// stream, progress events go to stderr (remote helper context where
    /// git owns stdout).
    pub output_mode: Option<crate::core::output::OutputMode>,
    /// Shared JSONL stream writing to stderr. Set by the remote helper
    /// when `CRAB_PROGRESS_FORMAT=jsonl` — JSONL events MUST go to
    /// stderr because git owns stdout in the remote helper context.
    pub jsonl_stderr_stream:
        Option<std::sync::Arc<std::sync::Mutex<crate::core::output::JsonlStream<std::io::Stderr>>>>,
    /// When `true`, `run_native_push` augments the explicit spec
    /// list with extra `PushSpec` entries for annotated tags whose
    /// targets are in the pushed commit set. Mirrors git's
    /// `push.followTags` / `--follow-tags` behaviour so annotated
    /// tags pointing at pushed commits don't get silently dropped.
    pub followtags: bool,
    /// Internal mode used by `crab mirror` for arbitrary Git remotes.
    ///
    /// Mirror preserves Git history exactly and does not upload local Crab
    /// pointer payloads, so the native helper can skip Crab pointer discovery
    /// and delegate only the Git pack/ref update work to the shared pipeline.
    pub mirror_git_only: bool,
}

/// Data-plane inputs consumed by one native push invocation.
pub struct NativePushInputs<'a> {
    pub store: Option<Store>,
    pub caching_store: Option<crab_cache_store::CachingStore>,
    pub staging: PushStaging,
    pub router: StoreLayout,
    pub push_state: &'a mut PushState,
    pub remote_name: &'a str,
    pub remote_url: &'a str,
    pub metrics: Option<Arc<Metrics>>,
    pub cancel: CancellationToken,
    pub(crate) pre_acquired_locks: Option<Vec<PushLockLease>>,
}

impl<'a> NativePushInputs<'a> {
    /// Creates native push inputs without a pre-acquired remote lock handoff.
    pub fn new(
        store: Option<Store>,
        caching_store: Option<crab_cache_store::CachingStore>,
        staging: PushStaging,
        router: StoreLayout,
        push_state: &'a mut PushState,
        remote_name: &'a str,
        remote_url: &'a str,
        metrics: Option<Arc<Metrics>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            store,
            caching_store,
            staging,
            router,
            push_state,
            remote_name,
            remote_url,
            metrics,
            cancel,
            pre_acquired_locks: None,
        }
    }

    pub(crate) fn with_pre_acquired_locks(
        mut self,
        pre_acquired_locks: Option<Vec<PushLockLease>>,
    ) -> Self {
        self.pre_acquired_locks = pre_acquired_locks;
        self
    }
}

type DiscoveryOutcome = (
    Vec<super::walk::PointerBlob>,
    Vec<crab_metadata::commit_graph::CommitEntry>,
    HashMap<String, String>,
);

#[derive(Debug, Clone)]
struct NativeGitDirs {
    per_worktree: PathBuf,
    common: PathBuf,
}

fn resolve_native_git_dirs(git_dir_override: Option<&Path>) -> Result<NativeGitDirs> {
    if let Some(per_worktree) = git_dir_override {
        let per_worktree = per_worktree.to_path_buf();
        let common = discover::resolve_common_dir(&per_worktree);
        return Ok(NativeGitDirs {
            per_worktree,
            common,
        });
    }

    let has_git_dir = std::env::var_os("GIT_DIR").is_some();
    let has_git_work_tree = std::env::var_os("GIT_WORK_TREE").is_some();
    if (!has_git_dir || has_git_work_tree)
        && let Ok(ctx) = crate::git::worktree::WorktreeContext::resolve()
    {
        return Ok(NativeGitDirs {
            per_worktree: ctx.per_worktree_git_dir,
            common: ctx.common_git_dir,
        });
    }

    let per_worktree = discover::discover_git_dir()?;
    let common = discover::resolve_common_dir(&per_worktree);
    Ok(NativeGitDirs {
        per_worktree,
        common,
    })
}

impl NativePushConfig {
    /// Build from a base `PushConfig` with native-push defaults.
    #[must_use]
    pub fn new(push: PushConfig) -> Self {
        Self {
            push,
            incremental: true,
            progress: true,
            emit_summary: true,
            color: true,
            verbose: false,
            output_mode: None,
            jsonl_stderr_stream: None,
            followtags: false,
            mirror_git_only: false,
        }
    }
}

/// Run the native push pipeline.
///
/// Orchestrates five phases with early lock acquisition to reduce lock
/// contention. Phase 1 uses incremental pointer discovery when push
/// state or manifest frontiers are available; phases 3-5 are delegated
/// to the shared push pipeline after the native orchestrator hands over
/// its precomputed walk and any acquired locks.
///
/// Produces identical remote state to [`run_push_batch`] — same xorbs,
/// shards, packs, manifests, and refs.
pub async fn run_native_push(
    config: &NativePushConfig,
    specs: &[PushSpec],
    inputs: NativePushInputs<'_>,
) -> Result<PushResult> {
    let NativePushInputs {
        store,
        caching_store,
        staging,
        router,
        push_state,
        remote_name,
        remote_url,
        metrics,
        cancel,
        mut pre_acquired_locks,
    } = inputs;
    let operation_metrics = metrics.clone();

    // Inputs transfer lease ownership even if validation or an empty batch
    // exits before discovery. Await cleanup before allowing a caller's retry.
    release_native_locks_on_error(
        validate_mirror_plan_context(config),
        &mut pre_acquired_locks,
    )
    .await?;
    if specs.is_empty() {
        debug!("native push: empty spec list, nothing to do");
        if let Some(leases) = pre_acquired_locks.take() {
            release_push_lock_leases(leases).await;
        }
        return Ok(PushResult::empty());
    }

    if let Some(result) = duplicate_destination_result(specs) {
        if let Some(leases) = pre_acquired_locks.take() {
            release_push_lock_leases(leases).await;
        }
        return Ok(result);
    }

    if let Err(error) = check_cancelled(&cancel) {
        if let Some(leases) = pre_acquired_locks.take() {
            release_push_lock_leases(leases).await;
        }
        return Err(error);
    }

    let Some(store) = store else {
        if let Some(leases) = pre_acquired_locks.take() {
            release_push_lock_leases(leases).await;
        }
        return Err(CrabError::Configuration {
            key: "push store".to_owned(),
            origin: "native push requires a canonical remote store".to_owned(),
        });
    };

    if config.followtags && config.push.protected_push.is_some() {
        if let Some(leases) = pre_acquired_locks.take() {
            release_push_lock_leases(leases).await;
        }
        return Err(CrabError::AuthFailed {
            path: "protected push does not support --follow-tags; push tag refs explicitly"
                .to_owned(),
        });
    }

    let pipeline_start = Instant::now();

    // Create the progress tracker, shared across all pipeline stages.
    // When the remote helper sets `CRAB_PROGRESS_FORMAT=jsonl`, progress
    // events go to stderr via JsonlStream<Stderr> because git owns stdout.
    let progress =
        if let (Some(mode), Some(stream)) = (config.output_mode, &config.jsonl_stderr_stream) {
            Arc::new(NativePushProgress::with_mode_stderr(
                config.color,
                config.verbose,
                mode,
                Some(Arc::clone(stream)),
            ))
        } else {
            Arc::new(NativePushProgress::new(
                config.progress,
                config.color,
                config.verbose,
            ))
        };

    info!(
        specs = specs.len(),
        incremental = config.incremental,
        "native push: starting pipeline"
    );

    let git_dirs = release_native_locks_on_error(
        resolve_native_git_dirs(config.push.git_dir.as_deref()),
        &mut pre_acquired_locks,
    )
    .await?;
    let mut delegated_push = config.push.clone();
    if delegated_push.git_dir.is_none() {
        delegated_push.git_dir = Some(git_dirs.per_worktree.clone());
    }
    if delegated_push.perf_phase_sink.is_none()
        && let Some(stream) = &config.jsonl_stderr_stream
    {
        delegated_push.perf_phase_sink = Some(PerfPhaseSink::Stderr(Arc::clone(stream)));
    }
    if pre_acquired_locks.is_some() && (config.followtags || config.push.protected_push.is_some()) {
        let leases = pre_acquired_locks.take().unwrap_or_default();
        release_push_lock_leases(leases).await;
        return Err(CrabError::Internal(
            "pre-acquired push locks cannot be combined with this native push mode".into(),
        ));
    }

    // ── Phase 1: Discover ──────────────────────────────────────────
    release_native_locks_on_error(check_cancelled(&cancel), &mut pre_acquired_locks).await?;
    let phase_start = Instant::now();
    let remote_refs_for_discovery = if let Some(session) = config.push.protected_push.as_ref() {
        // Prepare returns refs from the caller's filtered view. Those OIDs are
        // the only safe and locally resolvable frontier for a path-scoped
        // client; the protected upload store is not a canonical read handle.
        Some(prepared_ref_frontier(&session.ref_updates))
    } else {
        match crate::metadata::manifest::read_repository_snapshot(&store, &router).await {
            Ok(snapshot) => Some(snapshot.journal.refs),
            Err(CrabError::NotFound { path })
                if config.followtags && path == router.manifest_path().as_ref() =>
            {
                Some(BTreeMap::new())
            }
            Err(error) if config.followtags => return Err(error),
            Err(error) => {
                debug!(
                    error = %error,
                    "native push: remote manifest unavailable for discovery boundary; using push-state"
                );
                None
            }
        }
    };
    if pre_acquired_locks.is_none() && !config.followtags && config.push.protected_push.is_none() {
        match acquire_push_lock_leases(&store, router.repo_prefix(), specs, &config.push, &cancel)
            .await
        {
            Ok(leases) => {
                debug!(
                    lock_count = leases.len(),
                    "native push: acquired push locks before discovery"
                );
                pre_acquired_locks = Some(leases);
            }
            Err(e) => {
                warn!(error = %e, "native push: failed to acquire push lock before discovery");
                return Ok(push_lock_rejection_result(specs, &e));
            }
        }
    }
    let discovery = if config.mirror_git_only {
        phase_discover_git_only(specs, &git_dirs)
    } else {
        phase_discover(
            specs,
            push_state,
            remote_url,
            remote_refs_for_discovery.as_ref(),
            config.incremental,
            &git_dirs,
        )
    };
    let (mut pointers, mut commit_entries, mut sha_map) =
        release_native_locks_on_error(discovery, &mut pre_acquired_locks).await?;

    // If the incremental walk found no pointers but staging has live
    // files, the push state is stale — a prior push recorded the tip
    // SHA without actually uploading the xorb data (e.g. the chunks
    // were incorrectly classified as "remote only" or the push was
    // interrupted after ref CAS but before xorb upload completed on a
    // different code path). Retry with a full walk so the staged
    // content gets picked up.
    if !config.mirror_git_only && pointers.is_empty() && config.incremental {
        let staging_has_files = staging
            .reader()
            .and_then(|s| s.list_files().ok())
            .is_some_and(|files| !files.is_empty());

        if staging_has_files {
            info!(
                "native push: incremental walk found 0 pointers but staging has live files; \
                 retrying with full walk"
            );
            let full_discovery = phase_discover(
                specs,
                push_state,
                remote_url,
                remote_refs_for_discovery.as_ref(),
                false,
                &git_dirs,
            );
            let (full_pointers, full_entries, full_sha_map) =
                release_native_locks_on_error(full_discovery, &mut pre_acquired_locks).await?;
            pointers = full_pointers;
            commit_entries = full_entries;
            sha_map = full_sha_map;
        }
    }

    progress.report_discover(
        pointers.len() as u64,
        commit_entries.len() as u64,
        phase_start.elapsed(),
    );

    info!(
        pointers = pointers.len(),
        commits = commit_entries.len(),
        "native push: phase 1 (discover) complete"
    );

    if pointers.is_empty() {
        debug!("native push: no pointers discovered, nothing to upload");
    }

    // Tagged v1.0.1 allows pointer-free pushes while an exclusive staging
    // holder is busy. Pointer publication must instead return that exact lock
    // outcome and release the remote leases already acquired for discovery.
    let staging = match staging {
        PushStaging::Ready(reader) => Some(reader),
        PushStaging::Missing => None,
        PushStaging::Locked { .. } if pointers.is_empty() => None,
        PushStaging::Locked { holder_pid } => {
            return release_native_locks_on_error(
                Err(CrabError::StagingLocked { holder_pid }),
                &mut pre_acquired_locks,
            )
            .await;
        }
    };

    // ── Follow-tags synthesis ──────────────────────────────────────
    //
    // When the client asked for `--follow-tags`, walk the local
    // `refs/tags/*` namespace and synthesise extra `PushSpec` entries
    // for annotated tags whose peeled commit is reachable from a ref
    // being pushed. Placing the synthesis between discover and the lock
    // acquisition means the expensive walks are still read-only and
    // contention-free, while the augmented spec list flows into every
    // downstream phase (manifest CAS, push-state update, ref CAS).
    //
    let mut effective_specs: Vec<PushSpec> = specs.to_vec();
    if config.followtags {
        check_cancelled(&cancel)?;
        let remote_refs = remote_refs_for_discovery.as_ref().ok_or_else(|| {
            CrabError::Internal("follow-tags discovery lost its remote ref snapshot".to_owned())
        })?;

        let tag_specs =
            collect_followtag_specs(&effective_specs, &sha_map, remote_refs, &git_dirs)?;
        if tag_specs.is_empty() {
            debug!("followtags: no eligible annotated tags for pushed refs");
        } else {
            info!(
                tag_count = tag_specs.len(),
                "followtags: synthesised annotated tag specs"
            );
            for tag in tag_specs {
                sha_map.insert(tag.spec.src.clone(), tag.sha);
                effective_specs.push(tag.spec);
            }
        }
    }
    let specs: &[PushSpec] = &effective_specs;
    if let Some(result) = duplicate_destination_result(specs) {
        if let Some(leases) = pre_acquired_locks.take() {
            release_push_lock_leases(leases).await;
        }
        return Ok(result);
    }

    // ── Phase 2: Shard sync ────────────────────────────────────────
    //
    // Pre-push shard sync has been removed. `ShardSynchronizer` is now
    // invoked from clone/pull/fetch, and the delegated push pipeline
    // reads chunk-index classifications directly from the global
    // `chunk_index_db`. The phase tag and progress event are retained
    // for compatibility with existing progress watchers.
    let phase_start = Instant::now();
    progress.report_shard_sync(0, 0, phase_start.elapsed());
    debug!("native push: phase 2 (shard sync) skipped");

    // ── Hand push lock to delegated pipeline ───────────────────────
    //
    // The common case acquired locks before phase 1 so same-ref
    // contenders fail before discovery. Modes whose final ref list is
    // not known up front still acquire here, before any upload.
    //
    // Ownership of the locks (and their heartbeats) is handed to the
    // delegated pipeline via `run_push_batch_with_locks`, which releases
    // them on all exit paths — success, failure, and cancellation.
    release_native_locks_on_error(check_cancelled(&cancel), &mut pre_acquired_locks).await?;

    let result = if config.push.protected_push.is_some() {
        debug!("native push: protected push skips client-owned push lock");
        let prepopulated = PrePopulatedWalk {
            pointers: pointers.clone(),
            commit_entries: commit_entries.clone(),
            resolved_shas: sha_map.clone(),
            remote_alias: remote_name.to_owned(),
        };
        Box::pin(crate::git::push::run_push_batch_with_prepopulated(
            specs,
            &delegated_push,
            Some(store.clone()),
            caching_store,
            staging,
            router,
            metrics,
            cancel,
            Some(Arc::clone(&progress)),
            Some(prepopulated),
        ))
        .await
    } else {
        match pre_acquired_locks {
            Some(leases) => {
                run_native_push_with_locks(
                    specs,
                    &delegated_push,
                    Some(store.clone()),
                    caching_store,
                    staging,
                    router,
                    metrics,
                    cancel,
                    Arc::clone(&progress),
                    leases,
                    pointers.clone(),
                    commit_entries.clone(),
                    sha_map.clone(),
                    remote_name,
                )
                .await
            }
            None => {
                match acquire_push_lock_leases(
                    &store,
                    router.repo_prefix(),
                    specs,
                    &config.push,
                    &cancel,
                )
                .await
                {
                    Ok(leases) => {
                        run_native_push_with_locks(
                            specs,
                            &delegated_push,
                            Some(store.clone()),
                            caching_store,
                            staging,
                            router,
                            metrics,
                            cancel,
                            Arc::clone(&progress),
                            leases,
                            pointers.clone(),
                            commit_entries.clone(),
                            sha_map.clone(),
                            remote_name,
                        )
                        .await
                    }
                    Err(e) => {
                        warn!(error = %e, "native push: failed to acquire push lock");
                        push_lock_rejection_result(specs, &e)
                    }
                }
            }
        }
    };

    // ── Update push state on success ───────────────────────────────
    if result.all_ok() {
        update_push_state_on_success(push_state, specs, remote_url, &sha_map);

        if config.emit_summary {
            // Pull bytes/xorb counts from the shared progress tracker —
            // the counters were populated by the packing and upload
            // phases above.
            progress.report_summary(
                pointers.len() as u64,
                progress.upload_bytes_done(),
                progress.upload_xorbs_done(),
                remote_name,
                pipeline_start.elapsed(),
                None,
            );
        }
    }

    if let Some(metrics) = operation_metrics {
        metrics.add_push_duration_ms(pipeline_start.elapsed().as_millis() as u64);
    }

    Ok(result)
}

fn validate_mirror_plan_context(config: &NativePushConfig) -> Result<()> {
    if let Some(plan_id) = config.push.mirror_plan_id.as_deref() {
        if !config.mirror_git_only {
            return Err(CrabError::Protocol(
                "mirror plan identity is only valid for mirror reconciliation".to_owned(),
            ));
        }
        if config.push.active_active_replication.is_some()
            || config
                .push
                .protected_push
                .as_ref()
                .and_then(|session| session.active_active_writer.as_ref())
                .is_some()
        {
            return Err(CrabError::Protocol(
                "mirror plan receipts are not supported by active-active finalize".to_owned(),
            ));
        }
        if plan_id.len() != 64
            || !plan_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(CrabError::Protocol(
                "mirror plan identity must be 64 lowercase hexadecimal characters".to_owned(),
            ));
        }
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "native lock handoff carries the precomputed walk plus independent pipeline resources"
)]
async fn run_native_push_with_locks(
    specs: &[PushSpec],
    delegated_push: &PushConfig,
    store: Option<Store>,
    caching_store: Option<crab_cache_store::CachingStore>,
    staging: Option<Arc<StagingAreaReadOnly>>,
    router: StoreLayout,
    metrics: Option<Arc<Metrics>>,
    cancel: CancellationToken,
    progress: Arc<NativePushProgress>,
    leases: Vec<PushLockLease>,
    pointers: Vec<PointerBlob>,
    commit_entries: Vec<crab_metadata::commit_graph::CommitEntry>,
    sha_map: HashMap<String, String>,
    remote_name: &str,
) -> PushResult {
    let prepopulated = PrePopulatedWalk {
        pointers,
        commit_entries,
        resolved_shas: sha_map,
        remote_alias: remote_name.to_owned(),
    };
    Box::pin(run_push_batch_with_locks(
        specs,
        delegated_push,
        store,
        caching_store,
        staging,
        router,
        metrics,
        cancel,
        Some(progress),
        leases,
        Some(prepopulated),
    ))
    .await
}

fn push_lock_rejection_result(specs: &[PushSpec], err: &CrabError) -> PushResult {
    let reason = super::push::PushRejectReason::from_error(err);
    let mut outcomes = HashMap::new();
    for spec in specs {
        outcomes.insert(
            spec.dst.clone(),
            super::push::RefPushOutcome::Rejected(reason.clone()),
        );
    }
    PushResult::new(outcomes).with_failure_stage(PushFailureStage::Lock)
}

async fn release_native_locks_on_error<T>(
    result: Result<T>,
    locks: &mut Option<Vec<PushLockLease>>,
) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            if let Some(leases) = locks.take() {
                release_push_lock_leases(leases).await;
            }
            Err(error)
        }
    }
}

fn push_unique_hidden_sha(hidden_shas: &mut Vec<String>, sha: &str) {
    if hidden_shas.iter().any(|existing| existing == sha) {
        return;
    }
    hidden_shas.push(sha.to_owned());
}

fn prepared_ref_frontier(updates: &[crab_auth::PushRefUpdate]) -> BTreeMap<String, String> {
    updates
        .iter()
        .filter_map(|update| {
            update
                .old_oid
                .as_ref()
                .map(|oid| (update.ref_name.clone(), oid.clone()))
        })
        .collect()
}

/// Synthesise extra [`PushSpec`] entries for annotated tags whose target
/// commits are reachable from the refs being pushed.
///
/// Mirrors git's `push.followTags` / `--follow-tags` behaviour: when the
/// user pushes a branch, any annotated tags that point at commits inside
/// that branch's reachable history are shipped alongside so the
/// receiver sees them at the same generation as their targets.
///
/// Lightweight tags (direct commit references with no wrapping tag
/// object) are intentionally skipped — git's own `--follow-tags`
/// treats them the same way, on the rationale that a lightweight tag
/// is a private bookmark and not part of the release surface.
///
/// # Filters
///
/// A tag is included only when all of:
/// 1. It's an annotated tag (direct target is a tag object, not a commit).
/// 2. Its peeled commit is reachable from a non-delete ref being pushed.
/// 3. The remote doesn't already list the tag at the same SHA.
/// 4. The tag ref isn't already an explicit spec in `explicit_specs`.
///
#[derive(Debug)]
struct FollowTagSpec {
    spec: PushSpec,
    sha: String,
}

fn collect_followtag_specs(
    explicit_specs: &[PushSpec],
    resolved_shas: &HashMap<String, String>,
    remote_refs: &std::collections::BTreeMap<String, String>,
    git_dirs: &NativeGitDirs,
) -> Result<Vec<FollowTagSpec>> {
    let direct_tips = explicit_specs
        .iter()
        .filter(|spec| !spec.src.is_empty())
        .filter_map(|spec| {
            resolved_shas
                .get(&spec.src)
                .map(|sha| (spec.src.clone(), sha.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    if direct_tips.is_empty() {
        return Ok(Vec::new());
    }

    let peeled_tips = crab_git::tag::peeled_revision_targets_at(&git_dirs.common, &direct_tips)?;
    let mut reachable_commits = std::collections::HashSet::new();
    let mut walked_tips = std::collections::HashSet::new();
    for (ref_name, direct_sha) in &direct_tips {
        let tip = peeled_tips.get(ref_name).unwrap_or(direct_sha);
        if !walked_tips.insert(tip.as_str()) {
            continue;
        }
        reachable_commits.extend(
            super::incremental_walk::collect_entries_from_walk(&git_dirs.common, tip)?
                .into_iter()
                .map(|entry| entry.oid),
        );
    }

    let annotated_tags = crab_git::tag::annotated_tag_refs_strict_at(&git_dirs.common)?;

    let mut extra_specs: Vec<FollowTagSpec> = Vec::new();
    for tag in annotated_tags {
        let tag_ref_name = tag.name;

        // Skip refs already carried by an explicit spec — the pusher
        // asked for them already, and re-adding would produce
        // duplicate entries in the pipeline.
        if explicit_specs.iter().any(|s| s.dst == tag_ref_name) {
            continue;
        }

        if !reachable_commits.contains(&tag.peeled_commit) {
            // The tag points outside the commits being pushed — the
            // pusher didn't ask for this tag transitively, so skip it.
            continue;
        }

        // Skip tags already present on the remote at the same SHA;
        // differing SHAs (tag rewrite) still go through so the user
        // sees the non-fast-forward if applicable.
        if remote_refs
            .get(&tag_ref_name)
            .is_some_and(|existing_sha| existing_sha == &tag.tag_sha)
        {
            debug!(
                tag = %tag_ref_name,
                sha = %tag.tag_sha,
                "followtags: tag already on remote at same SHA, skipping"
            );
            continue;
        }

        debug!(
            tag = %tag_ref_name,
            tag_sha = %tag.tag_sha,
            commit = %tag.peeled_commit,
            "followtags: synthesising push spec for annotated tag"
        );

        extra_specs.push(FollowTagSpec {
            spec: PushSpec {
                force: false,
                src: tag_ref_name.clone(),
                dst: tag_ref_name,
            },
            sha: tag.tag_sha,
        });
    }

    Ok(extra_specs)
}

/// Phase 1: Incremental pointer discovery.
///
/// When `incremental` is true and push state has a last-pushed SHA for
/// the (remote, ref) pair AND that SHA is still resolvable in the local
/// ODB, walks only new commits via [`walk_incremental`]. Otherwise falls
/// back to the full graph walk.
///
/// Returns the pointer set, commit entries, and the src-ref → SHA map
/// built via `resolve_refs`. Callers propagate the SHA map into
/// `update_push_state_on_success` so the tip-recording path doesn't
/// spawn a second `git rev-parse`.
fn phase_discover(
    specs: &[PushSpec],
    push_state: &PushState,
    remote_url: &str,
    remote_refs: Option<&BTreeMap<String, String>>,
    incremental: bool,
    git_dirs: &NativeGitDirs,
) -> Result<DiscoveryOutcome> {
    use gix_object::Exists;

    // Resolve src refs to SHAs.
    let src_refs: Vec<&str> = specs
        .iter()
        .filter(|s| !s.src.is_empty())
        .map(|s| s.src.as_str())
        .collect();

    if src_refs.is_empty() {
        debug!(reason = "no_src_refs", "phase 1: no src refs to discover");
        return Ok((Vec::new(), Vec::new(), HashMap::new()));
    }

    let sha_map = resolve_refs(&src_refs, git_dirs)?;
    let src_tag_targets: BTreeMap<String, String> = sha_map
        .iter()
        .map(|(name, sha)| (name.clone(), sha.clone()))
        .collect();
    // Source keys can be frozen OIDs, not only refs/tags names. Preserve the
    // tag object for publication while walking its captured commit target.
    let peeled_src_tags =
        crab_git::tag::peeled_revision_targets_at(&git_dirs.common, &src_tag_targets)?;

    // Open the local ODB once so we can probe old_sha validity for each
    // spec. When the probe fails, fall back to a full walk — rewritten
    // histories or pruned objects would otherwise produce an error
    // downstream from `walk_incremental`.
    let objects_dir = git_dirs.common.join("objects");
    let odb = if incremental && objects_dir.is_dir() {
        match gix_odb::at(&objects_dir) {
            Ok(odb) => Some(odb),
            Err(e) => {
                warn!(
                    error = %e,
                    path = %objects_dir.display(),
                    "phase 1: failed to open ODB for old_sha probe, falling back to full walk"
                );
                None
            }
        }
    } else {
        None
    };
    let remote_peeled_tags = match remote_refs {
        Some(refs) => {
            crab_git::tag::peeled_tag_refs_at(&git_dirs.common, refs).unwrap_or_else(|error| {
                debug!(
                    error = %error,
                    "phase 1: remote tag peeling unavailable, skipping tag frontiers"
                );
                BTreeMap::new()
            })
        }
        None => BTreeMap::new(),
    };
    let mut remote_hidden_shas = Vec::new();
    if let (true, Some(refs), Some(db)) = (incremental, remote_refs, odb.as_ref()) {
        let mut seen_remote_tips = std::collections::BTreeSet::new();
        for (ref_name, direct_sha) in refs {
            let sha = if ref_name.starts_with("refs/tags/") {
                let Some(peeled) = remote_peeled_tags.get(ref_name) else {
                    debug!(
                        ref_name = %ref_name,
                        "phase 1: remote tag is not commit-peeled, skipping hidden frontier"
                    );
                    continue;
                };
                peeled
            } else {
                direct_sha
            };
            let oid = match gix_hash::ObjectId::from_hex(sha.as_bytes()) {
                Ok(oid) => oid,
                Err(e) => {
                    debug!(
                        ref_name = %ref_name,
                        sha = %sha,
                        error = %e,
                        "phase 1: remote ref tip is not a valid SHA, skipping hidden frontier"
                    );
                    continue;
                }
            };
            if db.exists(&oid) && seen_remote_tips.insert(sha.clone()) {
                remote_hidden_shas.push(sha.clone());
            } else {
                debug!(
                    ref_name = %ref_name,
                    sha = %sha,
                    "phase 1: remote ref tip unavailable or duplicate, skipping hidden frontier"
                );
            }
        }
        debug!(
            hidden_tips = remote_hidden_shas.len(),
            remote_refs = refs.len(),
            "phase 1: collected local remote tips for incremental frontier"
        );
    }

    let mut all_pointers = Vec::new();
    let mut all_entries = Vec::new();
    // Delete specs carry no new objects to walk, but they still need
    // to flow through the pipeline so `build_manifest` can remove the
    // ref from the new manifest. The caller passes the full `specs`
    // slice into the delegated pipeline downstream; we just track the
    // deletes here for observability and an explicit per-phase
    // accounting.
    let mut delete_specs: Vec<&PushSpec> = Vec::new();

    for spec in specs {
        let old_sha_for_kind = sha_map.get(&spec.src).map(String::as_str);
        match crate::git::push::SpecKind::classify(spec, old_sha_for_kind) {
            crate::git::push::SpecKind::Delete => {
                // No local history to walk — the ref simply leaves
                // the manifest in the CAS phase. Record the spec
                // for tracing and fall through to the next one.
                delete_specs.push(spec);
                debug!(
                    dst = %spec.dst,
                    reason = "delete_spec",
                    "phase 1: carrying delete spec forward without walk"
                );
                continue;
            }
            crate::git::push::SpecKind::Create | crate::git::push::SpecKind::Update => {}
        }

        let direct_sha = match sha_map.get(&spec.src) {
            Some(sha) if !sha.is_empty() => sha.as_str(),
            _ => {
                warn!(src = %spec.src, "could not resolve src ref, skipping");
                continue;
            }
        };
        let new_sha = peeled_src_tags
            .get(&spec.src)
            .map_or(direct_sha, String::as_str);

        // Prefer the remote manifest's current destination tip when it
        // is locally available. `.crab/push-state` remains the fallback
        // for first-push manifests, offline tests, and stores that could
        // not be read before discovery.
        let selected_boundary = if incremental {
            let remote_tip = remote_refs
                .and_then(|refs| refs.get(&spec.dst))
                .and_then(|sha| {
                    if spec.dst.starts_with("refs/tags/") {
                        remote_peeled_tags.get(&spec.dst).map(String::as_str)
                    } else {
                        Some(sha.as_str())
                    }
                });
            let push_state_tip = if spec.dst.starts_with("refs/tags/") {
                None
            } else {
                push_state.last_pushed(remote_url, &spec.dst)
            };
            let candidates = [
                remote_tip.map(|sha| (sha, "remote_manifest")),
                push_state_tip.map(|sha| (sha, "push_state")),
            ];
            let mut saw_candidate = false;
            let mut selected = None;

            for (sha, source) in candidates.into_iter().flatten() {
                saw_candidate = true;
                let oid = gix_hash::ObjectId::from_hex(sha.as_bytes()).ok();
                let present = match (oid, odb.as_ref()) {
                    (Some(o), Some(db)) => db.exists(&o),
                    _ => false,
                };
                if present {
                    debug!(
                        src = %spec.src,
                        dst = %spec.dst,
                        old_sha = %sha,
                        source,
                        reason = "incremental_ok",
                        "phase 1: old_sha present in ODB, using incremental walk"
                    );
                    selected = Some(sha.to_owned());
                    break;
                }

                debug!(
                    src = %spec.src,
                    dst = %spec.dst,
                    old_sha = %sha,
                    source,
                    reason = "unresolvable_old_sha",
                    "phase 1: incremental boundary not in ODB"
                );
            }

            if selected.is_some() {
                selected
            } else if saw_candidate {
                None
            } else {
                debug!(
                    src = %spec.src,
                    dst = %spec.dst,
                    reason = "no_incremental_boundary",
                    "phase 1: no manifest or push-state entry, falling back to full walk"
                );
                None
            }
        } else {
            None
        };
        let mut hidden_shas = Vec::new();
        if let Some(boundary) = &selected_boundary {
            push_unique_hidden_sha(&mut hidden_shas, boundary);
        }
        if incremental {
            for sha in &remote_hidden_shas {
                push_unique_hidden_sha(&mut hidden_shas, sha);
            }
        }

        debug!(
            src = %spec.src,
            dst = %spec.dst,
            new_sha = %new_sha,
            old_sha = selected_boundary.as_deref().unwrap_or("(none)"),
            hidden_tips = hidden_shas.len(),
            "phase 1: walking ref"
        );

        let hidden_refs: Vec<&str> = hidden_shas.iter().map(String::as_str).collect();
        let (pointers, entries) = super::incremental_walk::walk_incremental_with_hidden(
            &git_dirs.common,
            &hidden_refs,
            new_sha,
        )?;

        debug!(
            dst = %spec.dst,
            pointers = pointers.len(),
            commits = entries.len(),
            "phase 1: ref walk complete"
        );

        all_pointers.extend(pointers);
        all_entries.extend(entries);
    }

    if !delete_specs.is_empty() {
        debug!(
            delete_count = delete_specs.len(),
            "phase 1: delete specs will be applied by build_manifest"
        );
    }

    Ok((all_pointers, all_entries, sha_map))
}

fn phase_discover_git_only(
    specs: &[PushSpec],
    git_dirs: &NativeGitDirs,
) -> Result<DiscoveryOutcome> {
    let src_refs: Vec<&str> = specs
        .iter()
        .filter(|s| !s.src.is_empty())
        .map(|s| s.src.as_str())
        .collect();

    if src_refs.is_empty() {
        debug!(
            reason = "no_src_refs",
            "phase 1: mirror Git-only mode has no source refs to resolve"
        );
        return Ok((Vec::new(), Vec::new(), HashMap::new()));
    }

    let sha_map = resolve_refs(&src_refs, git_dirs)?;
    debug!(
        resolved_shas = sha_map.len(),
        "phase 1: mirror Git-only mode skipped Crab pointer discovery"
    );
    Ok((Vec::new(), Vec::new(), sha_map))
}

/// Resolve multiple refs to their SHAs via `gix-ref`.
///
/// Delegates to the shared [`crab_git::ref_resolve`] helper so both
/// the native push and the legacy remote-helper push path take the
/// same subprocess-free code path.
fn resolve_refs(refs: &[&str], git_dirs: &NativeGitDirs) -> Result<HashMap<String, String>> {
    Ok(crab_git::ref_resolve::resolve_refs_batch_at(
        &git_dirs.per_worktree,
        refs,
    )?)
}

/// Update push state with new SHAs after a successful push.
///
/// Reuses the SHA map already resolved by [`phase_discover`] instead of
/// spawning a second `git rev-parse`. Delete specs (`spec.src.is_empty`)
/// prune the entry for `(remote_url, dst)` so a later incremental walk
/// does not try to hide against a SHA that no longer exists on the
/// remote — leaving a stale entry forces a full walk on the next push
/// to that ref name and can mask legitimate deletes behind a cache.
fn update_push_state_on_success(
    push_state: &mut PushState,
    specs: &[PushSpec],
    remote_url: &str,
    sha_map: &HashMap<String, String>,
) {
    for spec in specs {
        if spec.src.is_empty() {
            // Delete: drop the cached tip so the next push walks
            // cleanly if the ref is later re-created.
            push_state.remove(remote_url, &spec.dst);
            continue;
        }
        if let Some(sha) = sha_map.get(&spec.src) {
            push_state.set(remote_url, &spec.dst, sha);
        } else {
            debug!(
                src = %spec.src,
                dst = %spec.dst,
                "update_push_state_on_success: no SHA in map, skipping"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn mirror_plan_context_accepts_a_valid_direct_plan() {
        let mut config = NativePushConfig::new(PushConfig::default());
        config.mirror_git_only = true;
        config.push.mirror_plan_id = Some("a".repeat(64));

        assert!(validate_mirror_plan_context(&config).is_ok());
    }

    #[test]
    fn mirror_plan_context_rejects_identity_outside_mirror_mode() {
        let mut non_mirror = NativePushConfig::new(PushConfig::default());
        non_mirror.push.mirror_plan_id = Some("a".repeat(64));

        assert!(validate_mirror_plan_context(&non_mirror).is_err());
    }

    #[test]
    fn mirror_plan_context_rejects_noncanonical_identity() {
        let mut uppercase = NativePushConfig::new(PushConfig::default());
        uppercase.mirror_git_only = true;
        uppercase.push.mirror_plan_id = Some("A".repeat(64));

        assert!(validate_mirror_plan_context(&uppercase).is_err());
    }

    #[derive(Debug)]
    struct ManifestReadFailingStore {
        inner: object_store::memory::InMemory,
    }

    impl std::fmt::Display for ManifestReadFailingStore {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("ManifestReadFailingStore")
        }
    }

    #[async_trait::async_trait]
    impl object_store::ObjectStore for ManifestReadFailingStore {
        async fn put_opts(
            &self,
            location: &object_store::path::Path,
            payload: object_store::PutPayload,
            options: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.inner.put_opts(location, payload, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &object_store::path::Path,
            options: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        async fn get_opts(
            &self,
            _location: &object_store::path::Path,
            _options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            Err(object_store::Error::NotSupported {
                source: Box::<dyn std::error::Error + Send + Sync>::from(
                    "injected manifest read failure",
                ),
            })
        }

        fn delete_stream(
            &self,
            locations: futures_util::stream::BoxStream<
                'static,
                object_store::Result<object_store::path::Path>,
            >,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
        {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &object_store::path::Path,
            to: &object_store::path::Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    struct TinyGitFixture {
        _git_env: crate::test::git_repo::CleanGitEnvGuard,
        _dir: tempfile::TempDir,
        work_tree: PathBuf,
        git_dir: PathBuf,
    }

    impl TinyGitFixture {
        fn new() -> Self {
            let git_env = crate::test::git_repo::CleanGitEnvGuard::new();
            let dir = tempfile::tempdir().unwrap();
            Self::run_git(dir.path(), &["init", "--initial-branch=main"]);
            Self::run_git(dir.path(), &["config", "user.email", "test@test.com"]);
            Self::run_git(dir.path(), &["config", "user.name", "Test"]);
            Self::run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
            Self {
                _git_env: git_env,
                work_tree: dir.path().to_path_buf(),
                git_dir: dir.path().join(".git"),
                _dir: dir,
            }
        }

        fn commit_text(&self, name: &str, content: &str) -> String {
            std::fs::write(self.work_tree.join(name), content).unwrap();
            Self::run_git(&self.work_tree, &["add", name]);
            Self::run_git(&self.work_tree, &["commit", "-m", content]);
            Self::git_output(&self.work_tree, &["rev-parse", "HEAD"])
        }

        fn run_git(cwd: &Path, args: &[&str]) {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn git_output(cwd: &Path, args: &[&str]) -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        }
    }

    fn followtags_config(fixture: &TinyGitFixture) -> NativePushConfig {
        let mut push = PushConfig {
            git_dir: Some(fixture.git_dir.clone()),
            ..PushConfig::default()
        };
        push.metadb.chunk_index.local_path =
            Some(fixture.work_tree.join("metadb/chunk-index.sqlite"));
        let mut config = NativePushConfig::new(push);
        config.followtags = true;
        config.progress = false;
        config
    }

    fn main_push_spec() -> PushSpec {
        PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }
    }

    #[test]
    fn frozen_tag_oid_discovers_its_commit_after_local_tag_is_deleted() {
        let fixture = TinyGitFixture::new();
        fixture.commit_text("a.txt", "one");
        TinyGitFixture::run_git(&fixture.work_tree, &["tag", "-a", "v1", "-m", "tagged"]);
        let tag_oid =
            TinyGitFixture::git_output(&fixture.work_tree, &["rev-parse", "refs/tags/v1"]);
        TinyGitFixture::run_git(&fixture.work_tree, &["tag", "-d", "v1"]);
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };
        let spec = PushSpec {
            force: false,
            src: tag_oid.clone(),
            dst: "refs/tags/published".to_owned(),
        };
        let (_, entries, resolved) = phase_discover(
            &[spec],
            &PushState::default(),
            "crab://bucket/repo",
            None,
            false,
            &git_dirs,
        )
        .unwrap();
        assert_eq!((entries.len(), resolved.get(&tag_oid)), (1, Some(&tag_oid)));
    }

    #[test]
    fn native_push_config_defaults() {
        let base = PushConfig::default();
        let config = NativePushConfig::new(base);
        assert!(config.incremental);
        assert!(config.emit_summary);
    }

    #[test]
    fn followtag_collection_rejects_missing_tag_object() {
        let fixture = TinyGitFixture::new();
        let commit = fixture.commit_text("a.txt", "one");
        TinyGitFixture::run_git(
            &fixture.work_tree,
            &["tag", "-a", "v1", "-m", "version one"],
        );
        let tag_oid =
            TinyGitFixture::git_output(&fixture.work_tree, &["rev-parse", "refs/tags/v1"]);
        let tag_object = fixture
            .git_dir
            .join("objects")
            .join(&tag_oid[..2])
            .join(&tag_oid[2..]);
        std::fs::remove_file(tag_object).unwrap();
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };
        let resolved = HashMap::from([("refs/heads/main".to_owned(), commit)]);

        let error =
            collect_followtag_specs(&[main_push_spec()], &resolved, &BTreeMap::new(), &git_dirs)
                .expect_err("missing annotated tag object must fail the push extension");

        assert!(matches!(error, CrabError::GitTag(_)));
    }

    #[test]
    fn followtag_collection_excludes_lightweight_tag() {
        let fixture = TinyGitFixture::new();
        let commit = fixture.commit_text("a.txt", "one");
        TinyGitFixture::run_git(&fixture.work_tree, &["tag", "lightweight"]);
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };
        let resolved = HashMap::from([("refs/heads/main".to_owned(), commit)]);

        let tags =
            collect_followtag_specs(&[main_push_spec()], &resolved, &BTreeMap::new(), &git_dirs)
                .expect("lightweight tag is a legitimate non-candidate");

        assert!(tags.is_empty());
    }

    #[test]
    fn followtag_collection_includes_tag_on_older_reachable_commit() {
        let fixture = TinyGitFixture::new();
        fixture.commit_text("a.txt", "one");
        TinyGitFixture::run_git(
            &fixture.work_tree,
            &["tag", "-a", "v1", "-m", "version one"],
        );
        let tag_oid =
            TinyGitFixture::git_output(&fixture.work_tree, &["rev-parse", "refs/tags/v1"]);
        let tip = fixture.commit_text("a.txt", "two");
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };
        let resolved = HashMap::from([("refs/heads/main".to_owned(), tip)]);

        let tags =
            collect_followtag_specs(&[main_push_spec()], &resolved, &BTreeMap::new(), &git_dirs)
                .expect("tagged ancestor is reachable from the pushed ref");

        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].spec.dst, "refs/tags/v1");
        assert_eq!(tags[0].sha, tag_oid);
    }

    #[tokio::test]
    async fn followtags_manifest_read_failure_publishes_nothing() {
        let fixture = TinyGitFixture::new();
        fixture.commit_text("a.txt", "one");
        let store = Store::new(Arc::new(ManifestReadFailingStore {
            inner: object_store::memory::InMemory::new(),
        }));
        let router = StoreLayout::new(store.clone(), "followtags-read-failure".to_owned());
        let config = followtags_config(&fixture);
        let specs = vec![main_push_spec()];
        let mut push_state = PushState::default();

        let error = run_native_push(
            &config,
            &specs,
            NativePushInputs::new(
                Some(store.clone()),
                None,
                PushStaging::Missing,
                router.clone(),
                &mut push_state,
                "origin",
                "crab://bucket/followtags-read-failure",
                None,
                CancellationToken::new(),
            ),
        )
        .await
        .expect_err("manifest read failure must reject follow-tags push");

        match error {
            CrabError::Storage(object_store::Error::NotSupported { source }) => {
                assert_eq!(source.to_string(), "injected manifest read failure");
            }
            other => panic!("expected original storage error, got {other:?}"),
        }
        assert!(
            store
                .list_prefix(&router.repo_path(""))
                .await
                .expect("list remote prefix")
                .is_empty(),
            "failed follow-tags discovery must not publish remote state"
        );
    }

    #[tokio::test]
    async fn malformed_followtag_publishes_nothing() {
        let fixture = TinyGitFixture::new();
        fixture.commit_text("a.txt", "one");
        std::fs::write(
            fixture.work_tree.join("malformed.tag"),
            b"not a tag object\n",
        )
        .expect("write malformed tag body");
        let malformed_oid = TinyGitFixture::git_output(
            &fixture.work_tree,
            &[
                "hash-object",
                "--literally",
                "-t",
                "tag",
                "-w",
                "malformed.tag",
            ],
        );
        std::fs::write(
            fixture.git_dir.join("refs/tags/broken-tag"),
            format!("{malformed_oid}\n"),
        )
        .expect("write ref to malformed tag object");
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let router = StoreLayout::new(store.clone(), "followtags-malformed".to_owned());
        let config = followtags_config(&fixture);
        let specs = vec![main_push_spec()];
        let mut push_state = PushState::default();

        let error = run_native_push(
            &config,
            &specs,
            NativePushInputs::new(
                Some(store.clone()),
                None,
                PushStaging::Missing,
                router.clone(),
                &mut push_state,
                "origin",
                "crab://bucket/followtags-malformed",
                None,
                CancellationToken::new(),
            ),
        )
        .await
        .expect_err("malformed annotated tag must reject follow-tags push");

        assert!(matches!(
            error,
            CrabError::GitTag(crab_git::tag::TagPeelError::DecodeTag { .. })
        ));
        assert!(
            store
                .list_prefix(&router.repo_path(""))
                .await
                .expect("list remote prefix")
                .is_empty(),
            "malformed follow-tag must not publish remote state"
        );
    }

    #[tokio::test]
    async fn cancelled_followtags_push_publishes_nothing() {
        let fixture = TinyGitFixture::new();
        fixture.commit_text("a.txt", "one");
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let router = StoreLayout::new(store.clone(), "followtags-cancelled".to_owned());
        let config = followtags_config(&fixture);
        let specs = vec![main_push_spec()];
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut push_state = PushState::default();

        let error = run_native_push(
            &config,
            &specs,
            NativePushInputs::new(
                Some(store.clone()),
                None,
                PushStaging::Missing,
                router.clone(),
                &mut push_state,
                "origin",
                "crab://bucket/followtags-cancelled",
                None,
                cancel,
            ),
        )
        .await
        .expect_err("cancelled follow-tags push must fail");

        assert!(matches!(error, CrabError::Cancelled));
        assert!(
            store
                .list_prefix(&router.repo_path(""))
                .await
                .expect("list remote prefix")
                .is_empty(),
            "cancelled follow-tags push must not publish remote state"
        );
    }

    #[tokio::test]
    async fn early_native_return_releases_pre_acquired_locks_before_returning() {
        for case in ["invalid-plan", "non-mirror-plan", "empty-batch"] {
            let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
            let router = StoreLayout::new(store.clone(), case.to_owned());
            let mut config = NativePushConfig::new(PushConfig::default());
            config.mirror_git_only = case != "non-mirror-plan";
            config.push.mirror_plan_id = match case {
                "invalid-plan" => Some("invalid".to_owned()),
                "non-mirror-plan" => Some("a".repeat(64)),
                _ => None,
            };
            let specs = vec![main_push_spec()];
            let cancel = CancellationToken::new();
            let leases = acquire_push_lock_leases(&store, case, &specs, &config.push, &cancel)
                .await
                .unwrap();
            let mut state = PushState::default();
            let requested_specs = if case == "empty-batch" {
                &[][..]
            } else {
                &specs
            };
            let result = run_native_push(
                &config,
                requested_specs,
                NativePushInputs::new(
                    Some(store.clone()),
                    None,
                    PushStaging::Missing,
                    router,
                    &mut state,
                    "origin",
                    "crab://bucket/early-return",
                    None,
                    cancel,
                )
                .with_pre_acquired_locks(Some(leases)),
            )
            .await;
            assert_eq!(result.is_ok(), case == "empty-batch", "{case}");
            assert!(
                !crab_coordination::PushLock::ref_lease_is_claimed(
                    store.inner(),
                    case,
                    "refs/heads/main",
                )
                .await
                .unwrap(),
                "{case}: cleanup must complete before returning to the caller"
            );
        }
    }

    #[tokio::test]
    async fn followtags_mode_releases_pre_acquired_locks() {
        let fixture = TinyGitFixture::new();
        fixture.commit_text("a.txt", "one");
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let prefix = "followtags-prelocked";
        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        let mut push_config = PushConfig {
            git_dir: Some(fixture.git_dir.clone()),
            ..PushConfig::default()
        };
        push_config.metadb.chunk_index.local_path =
            Some(fixture.work_tree.join("metadb/chunk-index.sqlite"));
        let specs = vec![main_push_spec()];
        let cancel = CancellationToken::new();
        let leases = acquire_push_lock_leases(&store, prefix, &specs, &push_config, &cancel)
            .await
            .expect("pre-acquire push lock");
        let mut config = NativePushConfig::new(push_config);
        config.followtags = true;
        config.progress = false;
        let mut push_state = PushState::default();

        let error = run_native_push(
            &config,
            &specs,
            NativePushInputs::new(
                Some(store.clone()),
                None,
                PushStaging::Missing,
                router,
                &mut push_state,
                "origin",
                "crab://bucket/followtags-prelocked",
                None,
                cancel,
            )
            .with_pre_acquired_locks(Some(leases)),
        )
        .await
        .expect_err("follow-tags cannot consume an incomplete lock set");

        assert!(matches!(error, CrabError::Internal(_)));
        let reacquired = acquire_push_lock_leases(
            &store,
            prefix,
            &specs,
            &config.push,
            &CancellationToken::new(),
        )
        .await
        .expect("rejected mode transition must release every pre-acquired lock");
        release_push_lock_leases(reacquired).await;
    }

    #[test]
    fn prepared_ref_frontier_uses_view_old_oids() {
        let updates = vec![
            crab_auth::PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some("a".repeat(40)),
                new_oid: "b".repeat(40),
            },
            crab_auth::PushRefUpdate {
                ref_name: "refs/heads/new".to_owned(),
                old_oid: None,
                new_oid: "c".repeat(40),
            },
        ];

        assert_eq!(
            prepared_ref_frontier(&updates),
            BTreeMap::from([("refs/heads/main".to_owned(), "a".repeat(40))])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn staging_contention_rejects_pointer_push_and_releases_remote_locks() {
        let fixture = TinyGitFixture::new();
        let pointer = crab_types::pointer::Pointer {
            file_hash: [9; 32],
            size: 1024,
            shard_hint: None,
        };
        fixture.commit_text(
            "large.bin",
            &String::from_utf8(pointer.serialize()).unwrap(),
        );
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let prefix = "staging-contention";
        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        crate::core::remote_layout::initialize(&store, &router)
            .await
            .unwrap();
        crate::cmd::init::create_initial_manifest(&store, &router, "refs/heads/main")
            .await
            .unwrap();
        let mut config = NativePushConfig::new(PushConfig {
            git_dir: Some(fixture.git_dir.clone()),
            ..PushConfig::default()
        });
        config.progress = false;
        let specs = vec![main_push_spec()];
        let mut state = PushState::default();

        let error = run_native_push(
            &config,
            &specs,
            NativePushInputs::new(
                Some(store.clone()),
                None,
                PushStaging::Locked {
                    holder_pid: Some(4321),
                },
                router.clone(),
                &mut state,
                "origin",
                "crab://bucket/staging-contention",
                None,
                CancellationToken::new(),
            ),
        )
        .await
        .expect_err("pointer push must retain contention");

        assert!(matches!(
            error,
            CrabError::StagingLocked {
                holder_pid: Some(4321)
            }
        ));
        let snapshot = crate::metadata::manifest::read_repository_snapshot(&store, &router)
            .await
            .unwrap();
        assert!(
            snapshot.journal.refs.is_empty(),
            "contention must not publish a ref"
        );
        let reacquired = acquire_push_lock_leases(
            &store,
            prefix,
            &specs,
            &config.push,
            &CancellationToken::new(),
        )
        .await
        .expect("staging rejection must release every remote lease");
        release_push_lock_leases(reacquired).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initial_mirror_push_records_the_exact_multi_ref_transaction() {
        let fixture = TinyGitFixture::new();
        let tip = fixture.commit_text("readme.txt", "initial mirror content");
        TinyGitFixture::run_git(&fixture.work_tree, &["tag", "v1"]);
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let router = StoreLayout::new(store.clone(), "initial-mirror-plan".to_owned());
        crate::core::remote_layout::initialize(&store, &router)
            .await
            .unwrap();
        crate::cmd::init::create_initial_manifest(&store, &router, "refs/heads/main")
            .await
            .unwrap();
        let plan_id = "a".repeat(64);
        let mut config = NativePushConfig::new(PushConfig {
            git_dir: Some(fixture.git_dir.clone()),
            mirror_plan_id: Some(plan_id.clone()),
            atomic: true,
            ..PushConfig::default()
        });
        config.push.metadb.chunk_index.local_path =
            Some(fixture.work_tree.join("metadb/chunks.sqlite"));
        config.mirror_git_only = true;
        config.progress = false;
        config.emit_summary = false;
        let specs = vec![
            main_push_spec(),
            PushSpec {
                force: false,
                src: "refs/tags/v1".to_owned(),
                dst: "refs/tags/v1".to_owned(),
            },
        ];
        let mut state = PushState::default();
        let result = run_native_push(
            &config,
            &specs,
            NativePushInputs::new(
                Some(store.clone()),
                None,
                PushStaging::Missing,
                router.clone(),
                &mut state,
                "origin",
                "crab://bucket/initial-mirror-plan",
                None,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("initial mirror push");
        assert!(result.all_ok());
        let receipt =
            crate::metadata::manifest::resolve_mirror_plan_receipt(&store, &router, &plan_id)
                .await
                .unwrap()
                .expect("successful mirror push must have a terminal receipt");
        let crab_metadata::plan_receipt::MirrorPlanCommit::RefJournal { transaction_id, .. } =
            receipt.commit
        else {
            panic!("direct mirror push must use journal authority");
        };
        let transaction = crate::metadata::manifest::read_ref_journal_transaction(
            &store,
            &router,
            &transaction_id,
        )
        .await
        .unwrap();
        let expected = BTreeMap::from([
            ("refs/heads/main".to_owned(), (None, Some(tip.clone()))),
            ("refs/tags/v1".to_owned(), (None, Some(tip))),
        ]);
        let edits = transaction
            .edits
            .into_iter()
            .map(|edit| (edit.ref_name, (edit.old_oid, edit.new_oid)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(edits, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pointer_free_push_ignores_staging_contention() {
        let fixture = TinyGitFixture::new();
        let tip = fixture.commit_text("readme.txt", "ordinary Git content");
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let router = StoreLayout::new(store.clone(), "staging-busy-plain-git".to_owned());
        crate::core::remote_layout::initialize(&store, &router)
            .await
            .unwrap();
        crate::cmd::init::create_initial_manifest(&store, &router, "refs/heads/main")
            .await
            .unwrap();
        let mut config = NativePushConfig::new(PushConfig {
            git_dir: Some(fixture.git_dir.clone()),
            ..PushConfig::default()
        });
        config.push.metadb.chunk_index.local_path =
            Some(fixture.work_tree.join("metadb/chunks.sqlite"));
        config.progress = false;
        config.emit_summary = false;
        let mut state = PushState::default();
        let result = run_native_push(
            &config,
            &[main_push_spec()],
            NativePushInputs::new(
                Some(store.clone()),
                None,
                PushStaging::Locked { holder_pid: None },
                router.clone(),
                &mut state,
                "origin",
                "crab://bucket/staging-busy-plain-git",
                None,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("pointer-free push");
        assert!(
            result.all_ok(),
            "pointer-free push must preserve tagged contention behavior"
        );
        let snapshot = crate::metadata::manifest::read_repository_snapshot(&store, &router)
            .await
            .unwrap();
        assert_eq!(snapshot.journal.refs.get("refs/heads/main"), Some(&tip));
    }

    #[test]
    fn lock_rejection_retains_failure_stage() {
        let result = push_lock_rejection_result(
            &[main_push_spec()],
            &CrabError::PushLockHeld {
                ref_name: "refs/heads/main".to_owned(),
                holder: "other-writer".to_owned(),
                expires_at_unix: Some(1),
            },
        );

        assert_eq!(result.failure_stage, Some(PushFailureStage::Lock));
        assert_eq!(
            result.outcomes["refs/heads/main"].protocol_tag(),
            "lock-contention"
        );
    }

    #[tokio::test]
    async fn run_native_push_empty_specs_returns_empty() {
        use object_store::memory::InMemory;

        let config = NativePushConfig::new(PushConfig::default());
        let mut push_state = PushState::default();
        let cancel = CancellationToken::new();
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = crate::storage::store::Store::new(inner);
        let router = StoreLayout::new(store, String::new());

        let result = run_native_push(
            &config,
            &[],
            NativePushInputs {
                store: None,
                caching_store: None,
                staging: PushStaging::Missing,
                router,
                push_state: &mut push_state,
                remote_name: "origin",
                remote_url: "crab://bucket/repo",
                metrics: None,
                cancel,
                pre_acquired_locks: None,
            },
        )
        .await
        .unwrap();

        assert!(result.outcomes.is_empty());
    }

    #[test]
    fn phase_discover_prefers_remote_manifest_tip_over_push_state() {
        let fixture = TinyGitFixture::new();
        let first = fixture.commit_text("a.txt", "one");
        let second = fixture.commit_text("b.txt", "two");
        let third = fixture.commit_text("c.txt", "three");

        let mut push_state = PushState::default();
        push_state.set("crab://bucket/repo", "refs/heads/main", &first);
        let remote_refs = BTreeMap::from([("refs/heads/main".to_owned(), second)]);
        let specs = vec![PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }];
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };

        let (_pointers, entries, _sha_map) = phase_discover(
            &specs,
            &push_state,
            "crab://bucket/repo",
            Some(&remote_refs),
            true,
            &git_dirs,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].oid, third);
    }

    #[test]
    fn phase_discover_uses_push_state_when_manifest_tip_is_not_local() {
        let fixture = TinyGitFixture::new();
        let first = fixture.commit_text("a.txt", "one");
        let second = fixture.commit_text("b.txt", "two");

        let mut push_state = PushState::default();
        push_state.set("crab://bucket/repo", "refs/heads/main", &first);
        let remote_refs = BTreeMap::from([("refs/heads/main".to_owned(), "f".repeat(40))]);
        let specs = vec![PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }];
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };

        let (_pointers, entries, _sha_map) = phase_discover(
            &specs,
            &push_state,
            "crab://bucket/repo",
            Some(&remote_refs),
            true,
            &git_dirs,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].oid, second);
    }

    #[test]
    fn phase_discover_retargeted_url_does_not_reuse_old_boundary() {
        let fixture = TinyGitFixture::new();
        let first = fixture.commit_text("a.txt", "one");
        let second = fixture.commit_text("b.txt", "two");

        let mut push_state = PushState::default();
        push_state.set("crab://bucket/old", "refs/heads/main", &first);
        let specs = vec![PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }];
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };

        let (_pointers, entries, _sha_map) = phase_discover(
            &specs,
            &push_state,
            "crab://bucket/new",
            None,
            true,
            &git_dirs,
        )
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].oid, second);
        assert_eq!(entries[1].oid, first);
    }

    #[test]
    fn phase_discover_uses_remote_ref_frontier_for_new_branch() {
        let fixture = TinyGitFixture::new();
        let _first = fixture.commit_text("a.txt", "one");
        let second = fixture.commit_text("b.txt", "two");
        TinyGitFixture::run_git(&fixture.work_tree, &["checkout", "-b", "feature"]);
        let third = fixture.commit_text("c.txt", "three");

        let push_state = PushState::default();
        let remote_refs = BTreeMap::from([("refs/heads/main".to_owned(), second)]);
        let specs = vec![PushSpec {
            force: false,
            src: "refs/heads/feature".to_owned(),
            dst: "refs/heads/feature".to_owned(),
        }];
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };

        let (_pointers, entries, _sha_map) = phase_discover(
            &specs,
            &push_state,
            "crab://bucket/repo",
            Some(&remote_refs),
            true,
            &git_dirs,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].oid, third);
    }

    #[test]
    fn phase_discover_walks_annotated_tag_target_and_preserves_tag_sha() {
        let fixture = TinyGitFixture::new();
        let commit = fixture.commit_text("a.txt", "one");
        TinyGitFixture::run_git(
            &fixture.work_tree,
            &["tag", "-a", "v1", "-m", "version one"],
        );
        let tag_sha =
            TinyGitFixture::git_output(&fixture.work_tree, &["rev-parse", "refs/tags/v1"]);
        let specs = vec![PushSpec {
            force: false,
            src: "refs/tags/v1".to_owned(),
            dst: "refs/tags/v1".to_owned(),
        }];
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };

        let (_pointers, entries, sha_map) = phase_discover(
            &specs,
            &PushState::default(),
            "crab://bucket/repo",
            None,
            false,
            &git_dirs,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].oid, commit);
        assert_eq!(sha_map.get("refs/tags/v1"), Some(&tag_sha));
    }

    #[test]
    fn phase_discover_peels_annotated_tag_hidden_frontier() {
        let fixture = TinyGitFixture::new();
        let first = fixture.commit_text("a.txt", "one");
        TinyGitFixture::run_git(
            &fixture.work_tree,
            &["tag", "-a", "v1", "-m", "version one"],
        );
        let tag_sha =
            TinyGitFixture::git_output(&fixture.work_tree, &["rev-parse", "refs/tags/v1"]);
        let second = fixture.commit_text("b.txt", "two");
        let remote_refs = BTreeMap::from([("refs/tags/v1".to_owned(), tag_sha)]);
        let specs = vec![PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }];
        let git_dirs = NativeGitDirs {
            per_worktree: fixture.git_dir.clone(),
            common: fixture.git_dir.clone(),
        };

        let (_pointers, entries, _sha_map) = phase_discover(
            &specs,
            &PushState::default(),
            "crab://bucket/repo",
            Some(&remote_refs),
            true,
            &git_dirs,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].oid, second);
        assert_ne!(entries[0].oid, first);
    }
}
