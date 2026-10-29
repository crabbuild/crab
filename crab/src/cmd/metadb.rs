//! Capsule metadata diagnostics and fenced maintenance, plus local cache tools.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Subcommand;
use object_store::ObjectStore;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::git::url::CrabUrl;
use crab_xet::hash::MerkleHash;

/// `crab metadb cache {stats | clear}` subsubcommands.
#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Report on-disk size, entry count, and installed shard count
    /// for the local chunk-index SQLite cache.
    Stats,
    /// Remove the local chunk-index SQLite file on disk. The next
    /// crab operation will re-open it cold.
    Clear,
}

/// Top-level subcommands for `crab metadb`.
#[derive(Debug, Subcommand)]
pub enum MetadbCommand {
    /// Diagnose the captured Capsule root and optional complete content closure.
    Diagnose {
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
        /// Authenticate the complete metadata, Git and content closure.
        #[arg(long)]
        deep: bool,
    },
    /// Verify the complete Capsule closure and publish a current checkpoint.
    Rebuild {
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Maintain Capsule checkpoints and Git pack layout.
    Owner {
        /// Repair the current generation once and exit.
        #[arg(long)]
        once: bool,
        /// Idle polling interval in seconds.
        #[arg(long, default_value_t = 30, value_name = "SECONDS")]
        interval: u64,
        /// Emit one JSON object per repair sample.
        #[arg(long)]
        jsonl: bool,
    },
    /// Inspect or wipe the local chunk-index cache.
    #[command(subcommand)]
    Cache(CacheCommand),
}

/// Structured metadata diagnosis from one captured Capsule authority.
#[derive(Debug, Serialize)]
pub struct DiagnosePayload {
    pub protocol: &'static str,
    pub capsule: CapsuleDiagnosis,
}

/// Protocol-v2 authority and optional full-catalog diagnosis.
#[derive(Debug, Serialize)]
pub struct CapsuleDiagnosis {
    pub root_path: String,
    pub generation: u64,
    pub root_digest: String,
    pub state_digest: String,
    pub visible_refs: u64,
    pub visible_capsules: u64,
    pub checkpoint_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deep_integrity: Option<CapsuleDeepIntegrity>,
}

/// Results of authenticating every v2 metadata and Git-pack container.
#[derive(Debug, Serialize)]
pub struct CapsuleDeepIntegrity {
    pub git_packs: u64,
    pub git_pack_bytes: u64,
    pub git_closure_verified: bool,
    pub file_entries: u64,
    pub shard_entries: u64,
    pub xorb_entries: u64,
    pub pointer_objects_read: u64,
    pub verdict: &'static str,
}

/// Capsule authority and local cache health for `crab doctor --metadb`.
#[derive(Debug, Serialize)]
pub struct DoctorMetadbPayload {
    pub protocol: &'static str,
    pub repo_prefix: String,
    pub capsule: CapsuleDiagnosis,
    pub cache: CacheStatsPayload,
}

/// Structured payload for `crab metadb cache stats`.
#[derive(Debug, Serialize)]
pub struct CacheStatsPayload {
    pub cache_path: String,
    pub exists: bool,
    pub file_size_bytes: u64,
    pub entry_count: u64,
    pub installed_shard_count: u64,
    pub cache_gc_generation: u64,
}

/// Dispatch for `crab metadb <sub>`.
pub async fn run_metadb(
    cmd: MetadbCommand,
    mode: OutputMode,
    cancel: &CancellationToken,
) -> Result<()> {
    match cmd {
        MetadbCommand::Diagnose { json, deep } => {
            let mode = if json { OutputMode::Json } else { mode };
            run_diagnose(mode, deep, cancel).await
        }
        MetadbCommand::Rebuild { json } => {
            let mode = if json { OutputMode::Json } else { mode };
            run_rebuild(mode, cancel).await
        }
        MetadbCommand::Owner {
            once,
            interval,
            jsonl,
        } => Box::pin(run_generation_owner(once, interval, jsonl, cancel)).await,
        MetadbCommand::Cache(sub) => match sub {
            CacheCommand::Stats => {
                check_cancelled(cancel)?;
                run_cache_stats(mode)
            }
            CacheCommand::Clear => {
                check_cancelled(cancel)?;
                run_cache_clear(cancel)
            }
        },
    }
}

async fn resolve_repo_store(
    cancel: &CancellationToken,
) -> Result<(
    Arc<dyn ObjectStore>,
    String,
    crate::storage::store::BucketIdentity,
    Config,
)> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    resolve_repo_store_in(&cwd, cancel).await
}

async fn resolve_repo_store_in(
    root: &Path,
    cancel: &CancellationToken,
) -> Result<(
    Arc<dyn ObjectStore>,
    String,
    crate::storage::store::BucketIdentity,
    Config,
)> {
    let url = crate::core::project_config::ProjectConfig::remote_url(root)?;
    let parsed = CrabUrl::parse(&url)?;
    let config = Config::resolve_for_repo(root)?;
    let store = crate::auth::build_repository_url_store(&config, &parsed, "metadb", cancel).await?;
    // `build_store` hands back a `ProbingStoreHandle` which wraps an
    // `Arc<dyn ObjectStore>` via `inner()`. Clone the inner handle out
    // so the metadb layer holds a plain `Arc<dyn ObjectStore>`.
    let bucket_identity = crate::git::url::ObjectUrl::parse(url.trim())?.bucket_identity();
    Ok((
        Arc::clone(store.inner()),
        parsed.repo_path,
        bucket_identity,
        config,
    ))
}

#[derive(Debug, Clone, Serialize)]
struct GenerationOwnerSample {
    #[serde(skip)]
    identity: String,
    protocol: &'static str,
    inventory_loaded: bool,
    generation: u64,
    action: &'static str,
    maintenance_reason: &'static str,
    next_eligibility_secs: u64,
    locator_advanced: bool,
    visibility: &'static str,
    active_packs: u64,
    active_pack_bytes: u64,
    geometric_repack_packs: u64,
    catalog_layers: u64,
    catalog_bytes: u64,
    locator_sweep: crab_metadata::git_object_locator::LocatorSweepStats,
    commit_graph_layers: u64,
    commit_graph_bytes: u64,
    maintenance_bytes_read: u64,
    maintenance_bytes_written: u64,
    superseded: bool,
    elapsed_ms: u64,
}

#[derive(Debug)]
struct CompletedGenerationOwnerWork {
    sample: GenerationOwnerSample,
    completed_at: std::time::Instant,
}

const GENERATION_OWNER_ONCE_RETRY_LIMIT: u32 = 3;
const GENERATION_OWNER_ONCE_RETRY_INTERVAL_SECS: u64 = 2;
const GENERATION_OWNER_MIN_QUIESCENCE: std::time::Duration = std::time::Duration::from_secs(5);
const GENERATION_OWNER_STABLE_REVALIDATION: std::time::Duration =
    std::time::Duration::from_mins(10);
const CAPSULE_OWNER_CHECKPOINT_THRESHOLD: u32 = 32;
const CAPSULE_OWNER_MAX_CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

fn generation_owner_quiescence(interval_secs: u64) -> std::time::Duration {
    std::time::Duration::from_secs(interval_secs).max(GENERATION_OWNER_MIN_QUIESCENCE)
}

fn capsule_checkpoint_due(once: bool, has_checkpoint: bool, capsule_count: u64) -> bool {
    if has_checkpoint && capsule_count == 0 {
        return false;
    }
    once || capsule_count >= u64::from(CAPSULE_OWNER_CHECKPOINT_THRESHOLD)
}

async fn run_generation_owner(
    once: bool,
    interval_secs: u64,
    jsonl: bool,
    cancel: &CancellationToken,
) -> Result<()> {
    if interval_secs == 0 {
        return Err(CrabError::Configuration {
            key: "metadb.owner.interval".to_owned(),
            origin: "Git generation-owner interval must be at least one second".to_owned(),
        });
    }
    let (inner, repo_prefix, bucket_identity, config) = resolve_repo_store(cancel).await?;
    let store = crate::storage::store::Store::new(inner).with_bucket_identity(bucket_identity);
    let router = crate::storage::StoreLayout::new(store, repo_prefix);
    run_generation_owner_in(
        &router,
        once,
        interval_secs,
        jsonl,
        std::time::Duration::from_secs(config.push_lock_ttl_secs),
        cancel,
    )
    .await
}

async fn run_generation_owner_in(
    router: &crate::storage::StoreLayout,
    once: bool,
    interval_secs: u64,
    jsonl: bool,
    lock_ttl: std::time::Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    let layout = crab_storage::StoreLayout::with_global_prefix(
        router.store().as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    // Reject missing or corrupt authority before claiming ownership. Samples
    // reload inside the renewed owner lease rather than reusing this preflight.
    crab_metadata::capsule_protocol::load_root(&layout).await?;
    let owner_cancel = cancel.child_token();
    let mut owner = crab_coordination::PushLock::acquire_internal(
        router.store().inner(),
        router.repo_prefix(),
        crab_coordination::GIT_GENERATION_OWNER_RESOURCE,
        lock_ttl,
    )
    .await?;
    info!(repo_prefix = %router.repo_prefix(), once, interval_secs, "Git generation owner acquired");
    let operation = Box::pin(crab_coordination::while_renewing(
        &mut owner,
        Some(&owner_cancel),
        generation_owner_loop(&layout, once, interval_secs, jsonl, &owner_cancel),
    ))
    .await;
    if let Err(error) = owner.release().await {
        if operation.is_ok() {
            return Err(error.into());
        }
        warn!(%error, "Git generation owner lock release also failed");
    }
    operation
}

async fn capsule_generation_owner_sample(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    once: bool,
    interval_secs: u64,
    quiescence: std::time::Duration,
    observed_activity: &mut Option<(String, std::time::Instant)>,
    completed_work: Option<&CompletedGenerationOwnerWork>,
    cancel: &CancellationToken,
) -> Result<GenerationOwnerSample> {
    let started = std::time::Instant::now();
    let root = crab_metadata::capsule_protocol::load_root(layout).await?;
    let generation = root.record().root().generation();
    let activity = crab_read::capsule_protocol::read_activity_from_root(layout, &root).await?;
    let identity = activity.state_digest().to_owned();
    let quiet = once
        || generation_owner_activity_is_quiet(
            identity.clone(),
            observed_activity,
            std::time::Instant::now(),
            quiescence,
        );
    if !quiet {
        return Ok(capsule_owner_sample(
            identity,
            generation,
            "quiescence_wait",
            interval_secs,
            0,
            0,
            false,
            false,
            started,
        ));
    }
    if let Some(completed) = completed_work
        && completed.sample.identity == identity
        && completed.completed_at.elapsed() < GENERATION_OWNER_STABLE_REVALIDATION
    {
        let mut sample = completed.sample.clone();
        sample.action = "idle";
        sample.maintenance_reason = generation_owner_reason("idle");
        sample.next_eligibility_secs = interval_secs;
        sample.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        return Ok(sample);
    }
    let checkpoint_due = capsule_checkpoint_due(
        once,
        root.record().root().checkpoint().is_some(),
        activity.capsule_count(),
    );
    // Physical debt survives a completed logical checkpoint. Revisit a multi-pack
    // checkpoint even with no new pushes, including after interrupted maintenance.
    let may_need_repack = root
        .record()
        .root()
        .checkpoint()
        .is_some_and(|pointer| pointer.pack_count() > 1);
    if !checkpoint_due && !may_need_repack {
        return Ok(capsule_owner_sample(
            identity,
            generation,
            "none",
            interval_secs,
            0,
            0,
            false,
            false,
            started,
        ));
    }
    let view = crab_read::capsule_protocol::open_view_from_root_with_metadata(
        layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
            max_frontier_bytes: CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
        },
    )
    .await?;
    let view_identity = view.state_digest();
    if !once
        && !generation_owner_activity_is_quiet(
            view_identity.clone(),
            observed_activity,
            std::time::Instant::now(),
            quiescence,
        )
    {
        return Ok(capsule_owner_sample(
            view_identity,
            view.root().root().generation(),
            "quiescence_wait",
            interval_secs,
            u64::try_from(view.git_pack_count()).unwrap_or(u64::MAX),
            view.git_pack_bytes()?,
            true,
            false,
            started,
        ));
    }
    let active_packs = u64::try_from(view.git_pack_count()).unwrap_or(u64::MAX);
    let active_pack_bytes = view.git_pack_bytes()?;
    if active_packs == 0 {
        return Ok(capsule_owner_sample(
            view_identity,
            view.root().root().generation(),
            "none",
            interval_secs,
            active_packs,
            active_pack_bytes,
            true,
            false,
            started,
        ));
    }
    let threshold = if once {
        0
    } else {
        CAPSULE_OWNER_CHECKPOINT_THRESHOLD
    };
    let maintenance = crab_remote::checkpoint::maintain_capsule_repository_from_view(
        layout,
        &view,
        threshold,
        CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
        cancel,
    )
    .await?;
    let checkpointed = maintenance.checkpointed.published;
    let repacked = maintenance.repacked.published;
    let changed = checkpointed || repacked;
    Ok(capsule_owner_sample(
        view_identity,
        view.root().root().generation(),
        if repacked {
            "geometric_repack"
        } else if checkpointed {
            "capsule_checkpoint"
        } else {
            "none"
        },
        if changed { 0 } else { interval_secs },
        active_packs,
        active_pack_bytes,
        true,
        changed,
        started,
    ))
}

fn capsule_owner_sample(
    identity: String,
    generation: u64,
    action: &'static str,
    next_eligibility_secs: u64,
    active_packs: u64,
    active_pack_bytes: u64,
    inventory_loaded: bool,
    superseded: bool,
    started: std::time::Instant,
) -> GenerationOwnerSample {
    GenerationOwnerSample {
        identity,
        protocol: "capsule-v2",
        inventory_loaded,
        generation,
        action,
        maintenance_reason: generation_owner_reason(action),
        next_eligibility_secs,
        locator_advanced: false,
        visibility: "embedded",
        active_packs,
        active_pack_bytes,
        geometric_repack_packs: 0,
        catalog_layers: 0,
        catalog_bytes: 0,
        locator_sweep: Default::default(),
        commit_graph_layers: 0,
        commit_graph_bytes: 0,
        maintenance_bytes_read: 0,
        maintenance_bytes_written: 0,
        superseded,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

async fn generation_owner_loop(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    once: bool,
    interval_secs: u64,
    jsonl: bool,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut consecutive_errors = 0_u32;
    let mut once_errors = 0_u32;
    let quiescence = generation_owner_quiescence(interval_secs);
    let mut observed_activity = None;
    let mut completed_work = None;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let sample = capsule_generation_owner_sample(
            layout,
            once,
            interval_secs,
            quiescence,
            &mut observed_activity,
            completed_work.as_ref(),
            cancel,
        )
        .await;
        match sample {
            Ok(mut sample) => {
                consecutive_errors = 0;
                once_errors = 0;
                if once {
                    sample.next_eligibility_secs = 0;
                }
                render_generation_owner_sample(&sample, jsonl)?;
                if once {
                    return Ok(());
                }
                match sample.action {
                    "quiescence_wait" => {}
                    "none" => {
                        completed_work = Some(CompletedGenerationOwnerWork {
                            sample: sample.clone(),
                            completed_at: std::time::Instant::now(),
                        });
                    }
                    "idle" => {}
                    _ => completed_work = None,
                }
                // A compaction restarts the elapsed quiet window instead of
                // immediately opening the large catalog/repack path.
                if sample.superseded && sample.action != "geometric_repack" {
                    continue;
                }
            }
            Err(CrabError::Cancelled) if cancel.is_cancelled() => {
                return Ok(());
            }
            Err(error)
                if once
                    && generation_owner_once_retryable(&error)
                    && once_errors < GENERATION_OWNER_ONCE_RETRY_LIMIT =>
            {
                once_errors = once_errors.saturating_add(1);
                let delay = generation_owner_retry_delay(
                    GENERATION_OWNER_ONCE_RETRY_INTERVAL_SECS,
                    once_errors - 1,
                );
                warn!(
                    %error,
                    retry_attempt = once_errors,
                    retry_limit = GENERATION_OWNER_ONCE_RETRY_LIMIT,
                    delay_ms = delay.as_millis(),
                    "Git generation owner one-shot sample encountered a transient error; retrying"
                );
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    () = tokio::time::sleep(delay) => {}
                }
                continue;
            }
            Err(error) if once => return Err(error),
            Err(error) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                warn!(%error, consecutive_errors, "Git generation owner sample failed");
            }
        }
        let delay = generation_owner_retry_delay(interval_secs, consecutive_errors);
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(delay) => {}
        }
    }
}

fn generation_owner_activity_is_quiet(
    activity: String,
    observed: &mut Option<(String, std::time::Instant)>,
    now: std::time::Instant,
    quiescence: std::time::Duration,
) -> bool {
    match observed {
        Some((previous, since)) if previous == &activity => {
            now.duration_since(*since) >= quiescence
        }
        _ => {
            *observed = Some((activity, now));
            false
        }
    }
}

fn generation_owner_once_retryable(error: &CrabError) -> bool {
    matches!(
        error,
        CrabError::NetworkTransient(_) | CrabError::Throttled { .. }
    )
}

fn generation_owner_reason(action: &str) -> &'static str {
    match action {
        "quiescence_wait" => "repository_activity_quieting",
        "idle" => "repository_identity_unchanged",
        "geometric_repack" => "geometric_pack_threshold",
        "capsule_checkpoint" => "capsule_frontier_threshold",
        _ => "no_maintenance_due",
    }
}

fn render_generation_owner_sample(sample: &GenerationOwnerSample, jsonl: bool) -> Result<()> {
    if jsonl {
        let mut stream = JsonlStream::new("metadb.owner", "1.0", std::io::stdout());
        stream.emit_snapshot(sample)?;
    } else {
        info!(
            protocol = sample.protocol,
            inventory_loaded = sample.inventory_loaded,
            generation = sample.generation,
            action = sample.action,
            locator_advanced = sample.locator_advanced,
            visibility = sample.visibility,
            active_packs = sample.active_packs,
            active_pack_bytes = sample.active_pack_bytes,
            geometric_repack_packs = sample.geometric_repack_packs,
            catalog_layers = sample.catalog_layers,
            catalog_bytes = sample.catalog_bytes,
            commit_graph_layers = sample.commit_graph_layers,
            commit_graph_bytes = sample.commit_graph_bytes,
            locator_object_rows_scanned = sample.locator_sweep.object_rows_scanned,
            locator_object_rows_deleted = sample.locator_sweep.object_rows_deleted,
            locator_pack_rows_scanned = sample.locator_sweep.pack_rows_scanned,
            locator_pack_rows_deleted = sample.locator_sweep.pack_rows_deleted,
            maintenance_bytes_read = sample.maintenance_bytes_read,
            maintenance_bytes_written = sample.maintenance_bytes_written,
            maintenance_reason = sample.maintenance_reason,
            next_eligibility_secs = sample.next_eligibility_secs,
            superseded = sample.superseded,
            elapsed_ms = sample.elapsed_ms,
            "Git generation owner sample completed"
        );
    }
    Ok(())
}

fn generation_owner_retry_delay(
    interval_secs: u64,
    consecutive_errors: u32,
) -> std::time::Duration {
    let multiplier = 1_u64
        .checked_shl(consecutive_errors.min(6))
        .unwrap_or(u64::MAX);
    std::time::Duration::from_secs(interval_secs.saturating_mul(multiplier).min(300))
}

async fn run_diagnose(mode: OutputMode, deep: bool, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let (inner, repo_prefix, bucket_identity, _config) = resolve_repo_store(cancel).await?;
    let store = crate::storage::store::Store::new(inner).with_bucket_identity(bucket_identity);
    let router = crate::storage::StoreLayout::new(store.clone(), repo_prefix);
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
    let payload = DiagnosePayload {
        protocol: "capsule-v2",
        capsule: diagnose_capsule(&layout, root, deep, cancel).await?,
    };
    check_cancelled(cancel)?;
    render_diagnose(&payload, mode)
}

async fn diagnose_capsule(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    deep: bool,
    cancel: &CancellationToken,
) -> Result<CapsuleDiagnosis> {
    let (state_digest, visible_refs, visible_capsules, deep_integrity) = if deep {
        let view = crab_read::capsule_protocol::open_view_from_root(
            layout,
            root.clone(),
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
                max_frontier_bytes: CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
            },
        )
        .await?;
        view.git_visibility_index().await?;
        // Verification must read layered sources and prove reachable content,
        // not rebuild a complete pack or accept catalog membership as file proof.
        let dependencies = crab_read::capsule_protocol::verify_reachable_dependencies(
            layout,
            &view,
            crab_read::capsule_protocol::CapsuleDependencyLimits {
                max_git_bytes: CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
                pointer_scan: crate::cmd::fsck_store::CAPSULE_GIT_SCAN_LIMITS,
            },
            cancel,
        )
        .await?;
        let git_pack_count = view.git_pack_count();
        check_cancelled(cancel)?;
        let current_root = crab_metadata::capsule_protocol::load_root(layout).await?;
        let current_activity =
            crab_read::capsule_protocol::read_activity_from_root(layout, &current_root).await?;
        if current_activity.state_digest() != view.state_digest() {
            return Err(CrabError::Protocol(
                "repository state changed during deep metadata diagnosis; retry against one stable view"
                    .to_owned(),
            ));
        }
        (
            view.state_digest(),
            diagnosis_count(view.refs().len(), "capsule ref")?,
            view.capsule_count()?,
            Some(CapsuleDeepIntegrity {
                git_packs: u64::try_from(git_pack_count).map_err(|_| {
                    CrabError::Internal("capsule Git pack count overflowed".to_owned())
                })?,
                git_pack_bytes: view.git_pack_bytes()?,
                git_closure_verified: true,
                file_entries: dependencies.catalog_files,
                shard_entries: dependencies.catalog_shards,
                xorb_entries: dependencies.catalog_xorbs,
                pointer_objects_read: dependencies.catalog_objects_read,
                verdict: "OK — root, ref heads, capsules, checkpoint, catalogs, visibility, and Git packs authenticated",
            }),
        )
    } else {
        let activity = crab_read::capsule_protocol::read_activity_from_root(layout, &root).await?;
        (
            activity.state_digest().to_owned(),
            activity.ref_count(),
            activity.capsule_count(),
            None,
        )
    };
    Ok(CapsuleDiagnosis {
        root_path: layout.capsule_root_path().to_string(),
        generation: root.record().root().generation(),
        root_digest: root.record().digest().to_owned(),
        state_digest,
        visible_refs,
        visible_capsules,
        checkpoint_present: root.record().root().checkpoint().is_some(),
        deep_integrity,
    })
}

fn diagnosis_count(count: usize, label: &str) -> Result<u64> {
    u64::try_from(count).map_err(|_| CrabError::Internal(format!("{label} count overflowed")))
}

fn render_diagnose(payload: &DiagnosePayload, mode: OutputMode) -> Result<()> {
    if matches!(mode, OutputMode::Json) {
        emit_json("metadb.diagnose", "1.0", payload)?;
        return Ok(());
    }

    println!("crab metadb diagnose\n");
    println!("protocol: {}\n", payload.protocol);
    render_capsule_diagnosis(&payload.capsule);
    Ok(())
}

fn render_capsule_diagnosis(diagnosis: &CapsuleDiagnosis) {
    println!("[capsule_repository]  path={}", diagnosis.root_path);
    println!("  status: open");
    println!("  generation: {}", diagnosis.generation);
    println!("  root_digest: {}", diagnosis.root_digest);
    println!("  state_digest: {}", diagnosis.state_digest);
    println!("  visible_refs: {}", diagnosis.visible_refs);
    println!("  visible_capsules: {}", diagnosis.visible_capsules);
    println!("  checkpoint_present: {}", diagnosis.checkpoint_present);
    match &diagnosis.deep_integrity {
        Some(deep) => {
            println!("  deep_integrity:");
            println!("    verdict: {}", deep.verdict);
            println!("    git_packs: {}", deep.git_packs);
            println!("    git_pack_bytes: {}", deep.git_pack_bytes);
            println!("    git_closure_verified: {}", deep.git_closure_verified);
            println!("    file_entries: {}", deep.file_entries);
            println!("    shard_entries: {}", deep.shard_entries);
            println!("    xorb_entries: {}", deep.xorb_entries);
            println!("    pointer_objects_read: {}", deep.pointer_objects_read);
        }
        None => println!("  deep_integrity: not requested (use --deep to enable)"),
    }
    println!();
}

#[derive(Debug, Serialize)]
struct RebuildPayload {
    protocol: &'static str,
    repo_prefix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint_published: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog_files_verified: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog_shards_verified: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog_xorbs_verified: Option<u64>,
    file_index_entries_written: u64,
    chunk_index_entries_written: u64,
    shards_processed: u64,
    shards_failed: u64,
    git_packs_processed: u64,
    git_packs_failed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_objects_verified: Option<u64>,
    git_objects_written: u64,
    elapsed_ms: u64,
    notes: Vec<String>,
}

async fn run_rebuild(mode: OutputMode, cancel: &CancellationToken) -> Result<()> {
    let (inner, repo_prefix, bucket_identity, config) = resolve_repo_store(cancel).await?;
    crate::replication::ensure_active_active_maintenance_admitted(
        &config,
        "metadata index rebuild",
    )?;
    let store = crate::storage::Store::new(inner).with_bucket_identity(bucket_identity);
    let router = crate::storage::StoreLayout::new(store, repo_prefix);
    run_rebuild_in(&router, mode, cancel).await
}

async fn run_rebuild_in(
    router: &crate::storage::StoreLayout,
    mode: OutputMode,
    cancel: &CancellationToken,
) -> Result<()> {
    let lease = crate::maintenance::RepositoryMaintenanceLease::acquire(
        router.store(),
        router.global_prefix(),
        router.repo_prefix(),
        cancel,
    )
    .await?;
    // Capture authority after acquiring the maintenance fence: queued work must
    // not publish from a root observed before the preceding owner finished.
    let operation = async {
        let layout = crab_storage::StoreLayout::with_global_prefix(
            router.store().as_storage().clone(),
            router.repo_prefix().to_owned(),
            router.global_prefix().to_owned(),
        );
        let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
        run_capsule_rebuild_in(&layout, root, router.repo_prefix(), mode, cancel).await
    }
    .await;
    let release = lease.release().await;
    match (operation, release) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(release_error)) => {
            warn!(error = %release_error, "metadb rebuild lease release also failed");
            Err(error)
        }
    }
}

async fn run_capsule_rebuild_in(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    repo_prefix: &str,
    mode: OutputMode,
    cancel: &CancellationToken,
) -> Result<()> {
    let started = std::time::Instant::now();
    let view = crab_read::capsule_protocol::open_view_from_root(
        layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
            max_frontier_bytes: CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
        },
    )
    .await?;
    view.git_visibility_index().await?;
    // Authenticate the complete closure before either a no-op success or a new
    // checkpoint; immutable publication alone does not verify reachable files.
    let dependencies = crab_read::capsule_protocol::verify_reachable_dependencies(
        layout,
        &view,
        crab_read::capsule_protocol::CapsuleDependencyLimits {
            max_git_bytes: CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
            pointer_scan: crate::cmd::fsck_store::CAPSULE_GIT_SCAN_LIMITS,
        },
        cancel,
    )
    .await?;
    let git_packs = diagnosis_count(view.git_pack_count(), "capsule Git pack")?;
    let git_objects = view.git_object_count()?;
    let visible_capsules = view.capsule_count()?;
    let checkpoint_published = if git_packs == 0 || visible_capsules == 0 {
        false
    } else {
        let published = crab_remote::checkpoint::publish_capsule_checkpoint_from_view(
            layout,
            &view,
            0,
            CAPSULE_OWNER_MAX_CHECKPOINT_BYTES,
            cancel,
        )
        .await?;
        if !published.published {
            return Err(CrabError::CasConflict {
                path: layout.capsule_root_path().to_string(),
                expected_etag: None,
            });
        }
        true
    };
    let mut notes = vec![
        "v2 catalogs are one authenticated checkpoint unit; rebuild verified the complete file, shard, xorb, visibility, and Git closure".to_owned(),
    ];
    if git_packs == 0 {
        notes.push("repository has no Git packs; no checkpoint was published".to_owned());
    } else if visible_capsules == 0 {
        notes.push(
            "the current checkpoint already covers the capsule frontier; no checkpoint was published"
                .to_owned(),
        );
    }
    let payload = RebuildPayload {
        protocol: "capsule-v2",
        repo_prefix: repo_prefix.to_owned(),
        checkpoint_published: Some(checkpoint_published),
        catalog_files_verified: Some(dependencies.catalog_files),
        catalog_shards_verified: Some(dependencies.catalog_shards),
        catalog_xorbs_verified: Some(dependencies.catalog_xorbs),
        file_index_entries_written: 0,
        chunk_index_entries_written: 0,
        shards_processed: dependencies.catalog_shards,
        shards_failed: 0,
        git_packs_processed: git_packs,
        git_packs_failed: 0,
        git_objects_verified: Some(git_objects),
        git_objects_written: 0,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        notes,
    };
    render_rebuild_payload(&payload, mode)
}

/// Verify selected file-to-shard bindings against one captured Capsule authority.
pub(crate) async fn verify_current_file_shard_bindings(
    entries: &[(MerkleHash, MerkleHash)],
) -> Result<Vec<bool>> {
    let cancel = CancellationToken::new();
    let (inner, repo_prefix, bucket_identity, _config) = resolve_repo_store(&cancel).await?;
    let store = crate::storage::Store::new(inner).with_bucket_identity(bucket_identity);
    let router = crate::storage::StoreLayout::new(store.clone(), repo_prefix);
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
    verify_capsule_file_index(&layout, root, entries).await
}

async fn verify_capsule_file_index(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    entries: &[(MerkleHash, MerkleHash)],
) -> Result<Vec<bool>> {
    // Recovery verifies selected mappings, not global payload health. Capture
    // once so every batch sees the same file-binding precedence during pushes.
    let lookup =
        crab_metadata::capsule_protocol::load_pointer_catalog_lookup_from_root(layout, &root)
            .await?;
    let mut verified = Vec::with_capacity(entries.len());
    for batch in entries.chunks(64) {
        let hashes = batch.iter().map(|(file, _)| file.hex()).collect::<Vec<_>>();
        let catalog = lookup.files(layout.store(), &hashes).await?;
        verified.extend(batch.iter().map(|(file, expected)| {
            catalog
                .files()
                .get(&file.hex())
                .is_some_and(|entry| entry.shard_hash() == expected.hex())
        }));
    }
    Ok(verified)
}

fn render_rebuild_payload(payload: &RebuildPayload, mode: OutputMode) -> Result<()> {
    if matches!(mode, OutputMode::Json) {
        emit_json("metadb.rebuild", "1.0", payload)?;
    } else {
        println!("\ncrab metadb rebuild\n");
        println!("  protocol:                    {}", payload.protocol);
        println!("  repo_prefix:                 {}", payload.repo_prefix);
        if let Some(published) = payload.checkpoint_published {
            println!("  checkpoint_published:        {published}");
        }
        if let Some(count) = payload.catalog_files_verified {
            println!("  catalog_files_verified:      {count}");
        }
        if let Some(count) = payload.catalog_shards_verified {
            println!("  catalog_shards_verified:     {count}");
        }
        if let Some(count) = payload.catalog_xorbs_verified {
            println!("  catalog_xorbs_verified:      {count}");
        }
        println!(
            "  shards_processed:            {}",
            payload.shards_processed
        );
        println!("  shards_failed:               {}", payload.shards_failed);
        println!(
            "  git_packs_processed:         {}",
            payload.git_packs_processed
        );
        println!(
            "  git_packs_failed:            {}",
            payload.git_packs_failed
        );
        if let Some(count) = payload.git_objects_verified {
            println!("  git_objects_verified:        {count}");
        }
        println!(
            "  git_objects_written:         {}",
            payload.git_objects_written
        );
        println!(
            "  file_index_entries_written:  {}",
            payload.file_index_entries_written
        );
        println!(
            "  chunk_index_entries_written: {}",
            payload.chunk_index_entries_written
        );
        println!("  elapsed_ms:                  {}", payload.elapsed_ms);
        if !payload.notes.is_empty() {
            println!("  notes:");
            for note in &payload.notes {
                println!("    - {note}");
            }
        }
    }

    info!(
        protocol = payload.protocol,
        checkpoint_published = payload.checkpoint_published,
        shards_processed = payload.shards_processed,
        shards_failed = payload.shards_failed,
        file_entries_written = payload.file_index_entries_written,
        chunk_entries_written = payload.chunk_index_entries_written,
        git_packs_processed = payload.git_packs_processed,
        git_packs_failed = payload.git_packs_failed,
        git_objects_verified = payload.git_objects_verified,
        git_objects_written = payload.git_objects_written,
        elapsed_ms = payload.elapsed_ms,
        "metadb rebuild complete"
    );
    Ok(())
}

fn default_local_chunk_index_path() -> Result<PathBuf> {
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(url) = crate::core::project_config::ProjectConfig::remote_url(&cwd)
    {
        if let Ok(parsed) = crate::git::url::ObjectUrl::parse(&url) {
            return Ok(crate::cache::chunk_index_cache_path(
                &crate::cache::default_cache_root(),
                &parsed.bucket_identity(),
            ));
        }
    }
    // Fall back to a generic path under the cache root so `cache
    // stats` still works outside a repo.
    Ok(crate::cache::chunk_index_cache_path(
        &crate::cache::default_cache_root(),
        &crate::storage::store::BucketIdentity::local_unset(),
    ))
}

fn run_cache_stats(mode: OutputMode) -> Result<()> {
    let path = default_local_chunk_index_path()?;
    let payload = cache_stats_for(&path)?;

    if matches!(mode, OutputMode::Json) {
        emit_json("metadb.cache.stats", "1.0", &payload)?;
    } else {
        println!("crab metadb cache stats\n");
        println!("  path:              {}", payload.cache_path);
        println!("  exists:            {}", payload.exists);
        println!("  file_size_bytes:   {}", payload.file_size_bytes);
        println!("  entry_count:       {}", payload.entry_count);
        println!("  installed_shards:  {}", payload.installed_shard_count);
        println!("  cache_gc_generation: {}", payload.cache_gc_generation);
    }
    Ok(())
}

fn cache_stats_for(path: &Path) -> Result<CacheStatsPayload> {
    let cache_path = path.display().to_string();
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CacheStatsPayload {
                cache_path,
                exists: false,
                file_size_bytes: 0,
                entry_count: 0,
                installed_shard_count: 0,
                cache_gc_generation: 0,
            });
        }
        Err(error) => return Err(error.into()),
    };
    // Inspect through the existing read-only API: diagnostics must not repair
    // the cache or materialize every chunk and shard merely to count rows.
    let stats = crab_metadata::persistent_chunk_index::PersistentChunkIndex::inspect(path)?
        .ok_or_else(|| {
            CrabError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("cache disappeared during inspection: {}", path.display()),
            ))
        })?;
    Ok(CacheStatsPayload {
        cache_path,
        exists: true,
        file_size_bytes: metadata.len(),
        entry_count: stats.entry_count,
        installed_shard_count: stats.installed_shard_count,
        cache_gc_generation: stats.cache_gc_generation,
    })
}

fn run_cache_clear(cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let path = default_local_chunk_index_path()?;
    if !path.exists() {
        println!(
            "crab metadb cache clear: nothing to do (no cache file at {})",
            path.display()
        );
        return Ok(());
    }
    std::fs::remove_file(&path)
        .map_err(|e| CrabError::Internal(format!("could not remove {}: {e}", path.display())))?;
    println!("crab metadb cache clear: removed {}", path.display());
    info!(path = %path.display(), "metadb cache cleared");
    Ok(())
}

/// Report current Capsule authority and local cache health without legacy scans.
pub async fn run_doctor_metadb_in(root: &Path, mode: OutputMode) -> Result<()> {
    let cancel = CancellationToken::new();
    let (inner, repo_prefix, bucket_identity, _config) =
        resolve_repo_store_in(root, &cancel).await?;
    let cache_path =
        crate::cache::chunk_index_cache_path(&crate::cache::default_cache_root(), &bucket_identity);
    let store = crate::storage::Store::new(inner).with_bucket_identity(bucket_identity);
    let router = crate::storage::StoreLayout::new(store.clone(), repo_prefix);
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
    let payload = DoctorMetadbPayload {
        protocol: "capsule-v2",
        repo_prefix: layout.repo_prefix().to_owned(),
        capsule: diagnose_capsule(&layout, root, false, &cancel).await?,
        cache: cache_stats_for(&cache_path)?,
    };
    render_doctor_metadb(&payload, mode)
}

fn render_doctor_metadb(payload: &DoctorMetadbPayload, mode: OutputMode) -> Result<()> {
    if matches!(mode, OutputMode::Json) {
        emit_json("doctor.metadb", "1.0", payload)?;
        return Ok(());
    }
    println!("crab doctor --metadb\n");
    println!("protocol: {}\n", payload.protocol);
    render_capsule_diagnosis(&payload.capsule);
    println!("[local cache]  path={}", payload.cache.cache_path);
    println!("  exists: {}", payload.cache.exists);
    println!("  file_size_bytes: {}", payload.cache.file_size_bytes);
    println!("  entry_count: {}", payload.cache.entry_count);
    println!(
        "  installed_shards: {}",
        payload.cache.installed_shard_count
    );
    println!(
        "  cache_gc_generation: {}",
        payload.cache.cache_gc_generation
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;
    use object_store::{ObjectStoreExt, memory::InMemory, path::Path as ObjectPath};

    #[test]
    fn generation_owner_backoff_is_bounded() {
        assert_eq!(
            generation_owner_retry_delay(5, 0),
            std::time::Duration::from_secs(5)
        );
        assert_eq!(
            generation_owner_retry_delay(5, 1),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            generation_owner_retry_delay(5, 6),
            std::time::Duration::from_secs(300)
        );
    }

    #[test]
    fn generation_owner_quiescence_has_a_real_time_floor() {
        assert_eq!(
            generation_owner_quiescence(1),
            GENERATION_OWNER_MIN_QUIESCENCE
        );
        assert_eq!(
            generation_owner_quiescence(30),
            std::time::Duration::from_secs(30)
        );
    }

    #[test]
    fn generation_owner_quiet_window_restarts_when_repository_activity_changes() {
        let started = std::time::Instant::now();
        let first = "first state digest".to_owned();
        let changed = "changed state digest".to_owned();
        let quiescence = std::time::Duration::from_secs(60);
        let mut observed = None;

        assert!(!generation_owner_activity_is_quiet(
            first.clone(),
            &mut observed,
            started,
            quiescence,
        ));
        assert!(generation_owner_activity_is_quiet(
            first,
            &mut observed,
            started + quiescence,
            quiescence,
        ));
        assert!(!generation_owner_activity_is_quiet(
            changed.clone(),
            &mut observed,
            started + quiescence,
            quiescence,
        ));
        assert!(generation_owner_activity_is_quiet(
            changed,
            &mut observed,
            started + quiescence + quiescence,
            quiescence,
        ));
    }

    #[tokio::test]
    async fn capsule_owner_uses_v2_authority_without_creating_a_manifest() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let storage = crab_storage::Store::new(Arc::clone(&inner));
        let layout = crab_storage::StoreLayout::new(storage, "org/v2-owner".to_owned());
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .expect("initialize capsule root");
        let mut observed = None;
        let sample = capsule_generation_owner_sample(
            &layout,
            true,
            30,
            std::time::Duration::ZERO,
            &mut observed,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("sample capsule owner");

        assert_eq!(sample.protocol, "capsule-v2");
        assert!(sample.inventory_loaded);
        assert_eq!(sample.action, "none");
        assert_eq!(sample.visibility, "embedded");
        assert_eq!(sample.active_packs, 0);
        assert!(!sample.superseded);
        let legacy_manifest = layout.manifest_path();
        assert!(matches!(
            inner.head(&legacy_manifest).await,
            Err(object_store::Error::NotFound { .. })
        ));
    }

    #[test]
    fn capsule_owner_one_shot_skips_an_already_checkpointed_empty_suffix() {
        assert!(capsule_checkpoint_due(true, false, 0));
        assert!(capsule_checkpoint_due(true, true, 1));
        assert!(!capsule_checkpoint_due(true, true, 0));
        assert!(!capsule_checkpoint_due(false, true, 31));
        assert!(capsule_checkpoint_due(false, true, 32));
    }

    #[tokio::test]
    async fn capsule_owner_fails_closed_on_corrupt_v2_authority() {
        let inner = Arc::new(InMemory::new());
        let storage = crab_storage::Store::new(inner);
        let layout = crab_storage::StoreLayout::new(storage, "org/corrupt-owner".to_owned());
        layout
            .store()
            .put(
                &layout.capsule_root_path(),
                bytes::Bytes::from_static(b"not a capsule root"),
            )
            .await
            .expect("seed corrupt root");

        let mut observed = None;
        assert!(matches!(
            capsule_generation_owner_sample(
                &layout,
                true,
                30,
                std::time::Duration::ZERO,
                &mut observed,
                None,
                &CancellationToken::new()
            )
            .await,
            Err(CrabError::CorruptObject { .. })
        ));
    }

    #[tokio::test]
    async fn capsule_diagnose_verifies_v2_without_legacy_metadata() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let storage = crab_storage::Store::new(Arc::clone(&inner));
        let layout = crab_storage::StoreLayout::new(storage, "org/v2-diagnose".to_owned());
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .expect("initialize capsule root");

        let diagnosis = diagnose_capsule(&layout, root, true, &CancellationToken::new())
            .await
            .expect("diagnose capsule repository");

        assert_eq!(diagnosis.generation, 0);
        assert_eq!(diagnosis.visible_refs, 0);
        assert_eq!(diagnosis.visible_capsules, 0);
        assert!(!diagnosis.checkpoint_present);
        let deep = diagnosis.deep_integrity.expect("deep diagnosis");
        assert_eq!(deep.git_packs, 0);
        assert_eq!(deep.git_pack_bytes, 0);
        assert!(deep.git_closure_verified);
        assert_eq!(deep.file_entries, 0);
        assert_eq!(deep.shard_entries, 0);
        assert_eq!(deep.xorb_entries, 0);
        assert_eq!(deep.pointer_objects_read, 0);
        let legacy_objects = inner
            .list(Some(&ObjectPath::from("org/v2-diagnose/file_index_db")))
            .try_collect::<Vec<_>>()
            .await
            .expect("list legacy metadata prefix");
        assert!(legacy_objects.is_empty());
    }

    #[tokio::test]
    async fn capsule_rebuild_verifies_empty_repository_without_legacy_metadata() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let storage = crab_storage::Store::new(Arc::clone(&inner));
        let layout = crab_storage::StoreLayout::new(storage, "org/v2-rebuild".to_owned());
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .expect("initialize capsule root");
        let digest = root.record().digest().to_owned();

        run_capsule_rebuild_in(
            &layout,
            root,
            "org/v2-rebuild",
            OutputMode::Text,
            &CancellationToken::new(),
        )
        .await
        .expect("rebuild empty capsule repository");

        let current = crab_metadata::capsule_protocol::load_root(&layout)
            .await
            .expect("load capsule root");
        assert_eq!(current.record().digest(), digest);
        let legacy_objects = inner
            .list(Some(&ObjectPath::from("org/v2-rebuild/file_index_db")))
            .try_collect::<Vec<_>>()
            .await
            .expect("list legacy metadata prefix");
        assert!(legacy_objects.is_empty());
    }
}

#[cfg(test)]
#[path = "metadb_capsule_tests.rs"]
mod capsule_tests;

#[cfg(test)]
#[path = "metadb_capsule_lookup_tests.rs"]
mod capsule_lookup_tests;
