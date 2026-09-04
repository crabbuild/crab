//! CLI surface for `crab metadb` — operator tooling for the two
//! SlateDB metadata databases.
//!
//! Subcommands:
//!
//! - `diagnose` — read-only health snapshot of the system keys
//!   (`sys:format_version`, `sys:epoch`, `sys:created_at`,
//!   `sys:gc_generation`). Optional `--db` filter narrows to a single
//!   instance. Deeper integrity checks (WAL replay, bloom validity)
//!   would live here too, but the public `slatedb` crate does not
//!   expose those surfaces yet; the diagnose output records the gap
//!   rather than claiming a check ran.
//! - `rebuild` — disaster-recovery reconstruction of one or both
//!   databases from the durable shards under `.crab/shards/`. The
//!   MVP implementation is append-only: every entry is
//!   content-addressed so re-writes are no-ops, which makes the
//!   command safely retriable.
//! - `compact` — request immediate SlateDB compaction. SlateDB drives
//!   compaction in the background and the current public crate does
//!   not expose an imperative trigger, so this subcommand logs a
//!   warning and exits successfully.
//! - `owner` — hold repository-scoped derived-index ownership and publish
//!   locator checkpoints plus visibility proofs independently of push clients.
//! - `cache {stats | clear}` — inspect or wipe the local
//!   `PersistentChunkIndex` SQLite cache.
//!
//! The `--metadb` branch of `crab doctor` lives here too
//! ([`run_doctor_metadb_in`]) so every metadb-facing report shares
//! one helper set.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use clap::Subcommand;
use futures_util::TryStreamExt;
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::git::url::CrabUrl;
use crate::metadata::{MetaDb, MetaDbGuard, XorbRef};
use crab_staging::shard_replay::{REPLAY_BATCH_ENTRIES, ShardReplaySpool};
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::format::MAX_XORB_SIZE;
use crab_xet::xorb::parser::XorbParser;

/// Which databases the subcommand operates on.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DbSelector {
    FileIndex,
    ChunkIndex,
    Both,
}

impl DbSelector {
    fn includes_file_index(self) -> bool {
        matches!(self, Self::FileIndex | Self::Both)
    }

    fn includes_chunk_index(self) -> bool {
        matches!(self, Self::ChunkIndex | Self::Both)
    }
}

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
    /// Print `sys:*` key snapshots for one or both databases.
    Diagnose {
        /// Target database (defaults to both).
        #[arg(long, value_enum, default_value_t = DbSelector::Both)]
        db: DbSelector,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
        /// Run deep integrity checks: full key/value scan and
        /// object-store enumeration. Slower but catches corruption
        /// that sys-key reads alone cannot detect.
        #[arg(long)]
        deep: bool,
    },
    /// Rebuild one or both databases from the durable shards under
    /// `.crab/shards/` (disaster recovery).
    Rebuild {
        /// Target database.
        #[arg(long, value_enum, default_value_t = DbSelector::Both)]
        db: DbSelector,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Own and repair derived Git indexes independently of push clients.
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
    /// Request immediate compaction of one or both databases.
    Compact {
        /// Target database.
        #[arg(long, value_enum, default_value_t = DbSelector::Both)]
        db: DbSelector,
    },
    /// Inspect or wipe the local chunk-index cache.
    #[command(subcommand)]
    Cache(CacheCommand),
}

/// Structured payload for `crab metadb diagnose --json`.
#[derive(Debug, Serialize)]
pub struct DiagnosePayload {
    pub file_index: Option<DbDiagnosis>,
    pub chunk_index: Option<DbDiagnosis>,
}

/// Per-database system-key summary.
#[derive(Debug, Serialize)]
pub struct DbDiagnosis {
    pub label: &'static str,
    pub path: String,
    /// Whether the `Db::open` call succeeded. `false` entries still
    /// carry an `error` string describing the failure.
    pub opened: bool,
    pub error: Option<String>,
    pub format_version: Option<u32>,
    pub epoch: Option<u64>,
    pub created_at: Option<String>,
    pub gc_generation: Option<u64>,
    /// Deep integrity check results. `None` when `--deep` was not
    /// requested or the database failed to open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deep_integrity: Option<DeepIntegrityResult>,
}

/// Results of a full-scan deep integrity check over one database.
#[derive(Debug, Serialize)]
pub struct DeepIntegrityResult {
    /// Total number of keys scanned (content + system + unknown).
    pub total_keys: u64,
    /// Number of well-formed content keys (prefix 0x01, 33 bytes).
    pub content_keys: u64,
    /// Number of system keys (prefix 0xFF).
    pub system_keys: u64,
    /// Keys that don't match either known prefix.
    pub unknown_keys: u64,
    /// Content keys whose value has an unexpected length.
    pub corrupt_values: u64,
    /// First few corruption details (capped to avoid flooding output).
    pub corruption_samples: Vec<String>,
    /// Number of object-store files under the database path.
    pub object_store_files: u64,
    /// Total bytes across all object-store files.
    pub object_store_bytes: u64,
    /// Whether the scan completed without iterator invalidation.
    pub scan_completed: bool,
    /// Human-readable verdict.
    pub verdict: String,
}

/// Structured payload for `crab doctor --metadb --json`.
#[derive(Debug, Serialize)]
pub struct DoctorMetadbPayload {
    pub repo_prefix: String,
    pub file_index: DbDiagnosis,
    pub chunk_index: DbDiagnosis,
    pub shards_prefix: String,
    pub shard_count: Option<u64>,
    pub shard_enumeration_error: Option<String>,
    pub cache: CacheStatsPayload,
    pub acceleration: AccelerationHealth,
}

#[derive(Debug, Serialize)]
pub struct AccelerationHealth {
    pub manifest_generation: Option<u64>,
    pub generation_receipt_valid: bool,
    pub ref_registry_repo_complete: bool,
    pub ref_registry_bucket_complete: bool,
    pub git_locator_index_available: bool,
    pub git_locator_covered_generation: Option<u64>,
    pub git_locator_covered_pack_index_hash: Option<String>,
    pub git_visibility_index_available: bool,
    pub git_visibility_covered_generation: Option<u64>,
    pub git_visibility_covered_pack_index_hash: Option<String>,
    pub git_visibility_coverage_current: bool,
    pub git_commit_graph_available: bool,
    pub git_commit_graph_commits: Option<u64>,
    pub git_commit_graph_layers: Option<u64>,
    pub git_commit_graph_current: bool,
    pub git_locator_writer_lease_active: bool,
    pub repair_required: bool,
    pub notes: Vec<String>,
}

impl AccelerationHealth {
    fn unavailable(note: impl Into<String>) -> Self {
        Self {
            manifest_generation: None,
            generation_receipt_valid: false,
            ref_registry_repo_complete: false,
            ref_registry_bucket_complete: false,
            git_locator_index_available: false,
            git_locator_covered_generation: None,
            git_locator_covered_pack_index_hash: None,
            git_visibility_index_available: false,
            git_visibility_covered_generation: None,
            git_visibility_covered_pack_index_hash: None,
            git_visibility_coverage_current: false,
            git_commit_graph_available: false,
            git_commit_graph_commits: None,
            git_commit_graph_layers: None,
            git_commit_graph_current: false,
            git_locator_writer_lease_active: false,
            repair_required: true,
            notes: vec![note.into()],
        }
    }
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
        MetadbCommand::Diagnose { db, json, deep } => {
            let mode = if json { OutputMode::Json } else { mode };
            run_diagnose(db, mode, deep, cancel).await
        }
        MetadbCommand::Rebuild { db, json } => {
            let mode = if json { OutputMode::Json } else { mode };
            run_rebuild(db, mode, cancel).await
        }
        MetadbCommand::Owner {
            once,
            interval,
            jsonl,
        } => Box::pin(run_generation_owner(once, interval, jsonl, cancel)).await,
        MetadbCommand::Compact { db } => run_compact(db, cancel).await,
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

/// Resolve the `(store, repo_prefix, bucket_identity, config)` tuple for the current
/// working directory. Returns a user-facing error when no remote is
/// configured — every metadb subcommand needs the bucket.
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

/// Open a metadb session anchored at `repo_prefix`.
///
/// `read_only` selects the SlateDB open mode. Subcommands that only
/// read system keys (`diagnose`, `doctor`) pass `true` so they can run
/// alongside a concurrent `crab push` without fencing it. `rebuild`
/// passes `false` because it emits batched writes against both
/// databases.
///
/// The per-DB tunables come from `config.metadb`; the local chunk cache
/// is bucket-global unless the operator set `metadb.chunk_index.local_path`.
fn build_metadb(
    store: Arc<dyn ObjectStore>,
    repo_prefix: String,
    bucket_identity: &crate::storage::store::BucketIdentity,
    read_only: bool,
    config: &Config,
) -> MetaDb {
    let mut metadb_config = config.build_metadb_config(&repo_prefix);
    metadb_config.read_only = read_only;

    if config.metadb.chunk_index.local_path.is_none() {
        metadb_config.local_chunk_index_path = crate::cache::chunk_index_cache_path(
            &crate::cache::default_cache_root(),
            bucket_identity,
        );
    }

    MetaDb::new(store, repo_prefix, metadb_config)
}

#[derive(Debug, Serialize)]
struct GenerationOwnerSample {
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

#[derive(Debug, Clone, Copy)]
struct CommitGraphMaintenance {
    action: &'static str,
    layers: u64,
    bytes: u64,
    bytes_read: u64,
    bytes_written: u64,
}

const GENERATION_OWNER_GRAPH_REBUILD_MAX_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const GENERATION_OWNER_ONCE_RETRY_LIMIT: u32 = 3;
const GENERATION_OWNER_ONCE_RETRY_INTERVAL_SECS: u64 = 2;

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
    let router = crate::storage::StoreLayout::new(store.clone(), repo_prefix.clone());
    let lock_ttl = std::time::Duration::from_secs(config.push_lock_ttl_secs);
    let owner_cancel = cancel.child_token();
    let mut owner = crab_coordination::PushLock::acquire_internal(
        store.inner(),
        &repo_prefix,
        crab_coordination::GIT_GENERATION_OWNER_RESOURCE,
        lock_ttl,
    )
    .await?;
    info!(%repo_prefix, once, interval_secs, "Git generation owner acquired");
    let operation = Box::pin(
        crate::git::push::while_renewing_internal_lock_with_cancellation(
            &mut owner,
            &owner_cancel,
            generation_owner_loop(
                &store,
                &router,
                once,
                interval_secs,
                jsonl,
                lock_ttl,
                &config,
                &owner_cancel,
            ),
        ),
    )
    .await;
    if let Err(error) = owner.release().await {
        if operation.is_ok() {
            return Err(error.into());
        }
        warn!(%error, "Git generation owner lock release also failed");
    }
    operation
}

async fn acquire_generation_owner_locator_lock(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    lock_ttl: std::time::Duration,
    cancel: &CancellationToken,
) -> Result<crab_coordination::PushLock> {
    let mut context = crab_coordination::PushLockAcquireContext::new(Arc::clone(store.inner()));
    let mut attempt = 0_u32;
    loop {
        match context
            .acquire_internal(
                router.repo_prefix(),
                crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
                lock_ttl,
            )
            .await
            .map_err(CrabError::from)
        {
            Ok(lock) => return Ok(lock),
            Err(CrabError::PushLockHeld { .. }) => {
                let delay = std::time::Duration::from_millis(
                    100_u64
                        .saturating_mul(1_u64.checked_shl(attempt.min(6)).unwrap_or(u64::MAX))
                        .min(5_000),
                );
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    () = cancel.cancelled() => return Err(CrabError::Cancelled),
                    () = tokio::time::sleep(delay) => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn generation_owner_loop(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    once: bool,
    interval_secs: u64,
    jsonl: bool,
    lock_ttl: std::time::Duration,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut consecutive_errors = 0_u32;
    let mut once_errors = 0_u32;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        match Box::pin(generation_owner_sample(
            store,
            router,
            lock_ttl,
            interval_secs,
            config,
            cancel,
        ))
        .await
        {
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
                if sample.superseded {
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

fn generation_owner_once_retryable(error: &CrabError) -> bool {
    matches!(
        error,
        CrabError::NetworkTransient(_) | CrabError::Throttled { .. }
    )
}

async fn generation_owner_sample(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    lock_ttl: std::time::Duration,
    interval_secs: u64,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<GenerationOwnerSample> {
    let started = std::time::Instant::now();
    let (manifest, _) = crate::metadata::manifest::read_manifest(store, router).await?;
    let generation = manifest.generation;
    if crate::git::push::compact_ref_journal_for_owner(
        store,
        router,
        lock_ttl,
        manifest.pusher.clone(),
        cancel,
    )
    .await?
    {
        return Ok(GenerationOwnerSample {
            generation,
            action: "ref_journal_compaction",
            maintenance_reason: generation_owner_reason("ref_journal_compaction"),
            next_eligibility_secs: 0,
            locator_advanced: false,
            visibility: "deferred",
            active_packs: 0,
            active_pack_bytes: 0,
            geometric_repack_packs: 0,
            catalog_layers: 0,
            catalog_bytes: 0,
            locator_sweep: Default::default(),
            commit_graph_layers: 0,
            commit_graph_bytes: 0,
            maintenance_bytes_read: 0,
            maintenance_bytes_written: 0,
            superseded: true,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
    }
    let packs = if manifest.pack_index_hash.is_empty() {
        Vec::new()
    } else {
        crate::metadata::manifest::read_bulk_pack_list(store, router, &manifest.pack_index_hash)
            .await?
    };
    let active_packs = u64::try_from(packs.len()).unwrap_or(u64::MAX);
    let active_pack_bytes = packs.iter().map(|pack| pack.size).sum();
    let geometric_repack_packs =
        u64::try_from(crate::cmd::repack::generation_owner_repack_count(&packs))
            .unwrap_or(u64::MAX);
    let anchor = crate::git::push::committed_manifest_anchor(&manifest)?;
    let (locator_advanced, catalog, locator_sweep) =
        maintain_object_catalog(store, router, anchor, &packs, lock_ttl, cancel)
            .await
            .map_err(|error| {
                warn!(
                    generation,
                    %error,
                    error_debug = ?error,
                    "Git generation owner catalog maintenance failed"
                );
                error
            })?;
    // The owner is also the repair path for imports and Git-only pushes that
    // had no post-CAS MetaDb writer. Once locator coverage is current, publish
    // the receipt so doctor and GC can distinguish complete derived state from
    // a silently unverified generation. Empty repositories have no index
    // anchor and therefore do not need a receipt.
    if anchor.is_some() {
        write_generation_index_receipt(store, router, &manifest)
            .await
            .map_err(|error| {
                warn!(
                    generation,
                    %error,
                    error_debug = ?error,
                    "Git generation owner receipt publication failed"
                );
                error
            })?;
    }
    if locator_advanced {
        return Ok(GenerationOwnerSample {
            generation,
            action: "catalog_advance",
            maintenance_reason: generation_owner_reason("catalog_advance"),
            next_eligibility_secs: interval_secs,
            locator_advanced,
            visibility: "deferred",
            active_packs,
            active_pack_bytes,
            geometric_repack_packs,
            catalog_layers: catalog.active_layers,
            catalog_bytes: catalog.active_bytes,
            locator_sweep,
            commit_graph_layers: 0,
            commit_graph_bytes: 0,
            maintenance_bytes_read: 0,
            maintenance_bytes_written: 0,
            superseded: false,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
    }
    let visibility_current = manifest.refs.is_empty()
        || crate::git::push::git_visibility_index_exists_for_manifest(store, router, &manifest)
            .await?;
    let visibility = Box::pin(
        crate::git::push::repair_git_visibility_after_locator_if_current_with_limit(
            store,
            router,
            generation,
            crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS,
            lock_ttl,
            cancel,
        ),
    )
    .await?;
    let (after, _) = crate::metadata::manifest::read_manifest(store, router).await?;
    let mut superseded = after.generation != generation
        || after.pack_index_hash != manifest.pack_index_hash
        || after.git_validation_digest != manifest.git_validation_digest;
    let visibility = match visibility {
        Some(crate::git::push::GitVisibilityPublication::Published) => "published",
        Some(crate::git::push::GitVisibilityPublication::CatalogBound) => "catalog_bound",
        Some(crate::git::push::GitVisibilityPublication::CompletePackOnly(_)) => {
            "complete_pack_only"
        }
        None => "superseded",
    };
    if superseded || !visibility_current {
        let action = if superseded {
            "superseded"
        } else if visibility == "catalog_bound" {
            "catalog_visibility_handoff"
        } else {
            "visibility_repair"
        };
        return Ok(GenerationOwnerSample {
            generation,
            action,
            maintenance_reason: generation_owner_reason(action),
            next_eligibility_secs: if superseded { 0 } else { interval_secs },
            locator_advanced,
            visibility,
            active_packs,
            active_pack_bytes,
            geometric_repack_packs,
            catalog_layers: catalog.active_layers,
            catalog_bytes: catalog.active_bytes,
            locator_sweep,
            commit_graph_layers: 0,
            commit_graph_bytes: 0,
            maintenance_bytes_read: 0,
            maintenance_bytes_written: 0,
            superseded,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
    }
    let mut graph = maintain_split_commit_graph(store, router, &manifest, cancel).await?;
    // The owner performs one derived-state action per cycle. A graph rebuild
    // or compaction must not be followed by shallow-closure work in the same
    // pass, otherwise a large repository can monopolize the owner lease.
    if graph.action == "none" {
        match crate::git::push::rebuild_shallow_closure_index_from_remote_packs_if_current(
            store,
            router,
            manifest.generation,
            GENERATION_OWNER_GRAPH_REBUILD_MAX_BYTES,
            cancel,
        )
        .await?
        {
            None => {
                graph.action = "superseded";
                superseded = true;
            }
            Some(true) => {
                graph.action = "shallow_closure_rebuild";
            }
            Some(_) => {}
        }
    }
    if graph.action == "none" && geometric_repack_packs > 0 {
        crate::replication::ensure_active_active_maintenance_admitted(
            config,
            "generation-owner geometric repack",
        )?;
        let repack_config = crate::cmd::repack::RepackConfig {
            lock_ttl,
            ..Default::default()
        };
        let repack = crate::cmd::repack::run_bounded_repack(
            store,
            router.repo_prefix(),
            &repack_config,
            crate::cmd::repack::RepackBudget::generation_owner(),
            cancel,
        )
        .await?;
        match repack {
            crate::cmd::repack::RepackRunResult::Completed { outcome, bounded } => {
                graph.action = if bounded {
                    "geometric_repack_bounded"
                } else {
                    "geometric_repack"
                };
                graph.bytes_read = outcome.bytes_read;
                graph.bytes_written = outcome.bytes_written;
                superseded = true;
            }
            crate::cmd::repack::RepackRunResult::Deferred {
                resource,
                actual,
                maximum,
            } => {
                graph.action = "geometric_repack_deferred";
                info!(
                    generation,
                    resource,
                    actual,
                    maximum,
                    "generation-owner geometric repack deferred by maintenance budget"
                );
            }
        }
    }
    Ok(GenerationOwnerSample {
        generation,
        action: graph.action,
        maintenance_reason: generation_owner_reason(graph.action),
        next_eligibility_secs: if superseded { 0 } else { interval_secs },
        locator_advanced,
        visibility,
        active_packs,
        active_pack_bytes,
        geometric_repack_packs,
        catalog_layers: catalog.active_layers,
        catalog_bytes: catalog.active_bytes,
        locator_sweep,
        commit_graph_layers: graph.layers,
        commit_graph_bytes: graph.bytes,
        maintenance_bytes_read: graph.bytes_read,
        maintenance_bytes_written: graph.bytes_written,
        superseded,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

fn generation_owner_reason(action: &str) -> &'static str {
    match action {
        "ref_journal_compaction" => "active_ref_journal",
        "catalog_advance" => "catalog_coverage_stale",
        "catalog_visibility_handoff" => "catalog_proof_handoff",
        "visibility_repair" => "visibility_missing_or_stale",
        "commit_graph_incremental" => "commit_graph_missing_incremental",
        "commit_graph_rebuild" => "commit_graph_missing",
        "commit_graph_compaction" => "commit_graph_layers_due",
        "shallow_closure_rebuild" => "shallow_closure_missing",
        "geometric_repack" => "geometric_pack_threshold",
        "geometric_repack_bounded" => "geometric_pack_budget",
        "geometric_repack_deferred" => "maintenance_budget",
        "superseded" => "manifest_superseded",
        _ => "no_maintenance_due",
    }
}

async fn maintain_object_catalog(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    anchor: Option<crate::git::push::CommittedManifestAnchor>,
    packs: &[crab_metadata::manifests::PackManifestEntry],
    lock_ttl: std::time::Duration,
    cancel: &CancellationToken,
) -> Result<(
    bool,
    crab_metadata::git_object_locator::GitObjectCatalogStats,
    crab_metadata::git_object_locator::LocatorSweepStats,
)> {
    let mut lock = acquire_generation_owner_locator_lock(store, router, lock_ttl, cancel).await?;
    let operation = Box::pin(
        crate::git::push::while_renewing_internal_lock_with_cancellation(
            &mut lock,
            cancel,
            async {
                // Plan under the publication lock. An unlocked snapshot could
                // miss a concurrent push and incorrectly skip compaction.
                // Recheck the anchor first so a push that won the race before
                // lock acquisition cannot trigger a repository-sized stale plan.
                let (current_manifest, _) =
                    crate::metadata::manifest::read_manifest(store, router).await?;
                let current_anchor =
                    crate::git::push::committed_manifest_anchor(&current_manifest)?;
                if current_anchor != anchor {
                    return Ok((
                        false,
                        crab_metadata::git_object_locator::GitObjectCatalogStats::default(),
                        crab_metadata::git_object_locator::LocatorSweepStats::default(),
                    ));
                }
                let (coverage, bindings) = {
                    let session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
                        Arc::clone(store.inner()),
                        router.repo_prefix(),
                    )
                    .await
                    .map_err(CrabError::from)?;
                    let coverage = session.coverage();
                    let bindings = session.pack_bindings().await.map_err(CrabError::from);
                    let close = session.close().await.map_err(CrabError::from);
                    match (bindings, close) {
                        (Ok(bindings), Ok(())) => (coverage, bindings),
                        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
                        (Err(error), Err(close_error)) => {
                            warn!(
                                error = %close_error,
                                "Git locator session close also failed after reading pack bindings"
                            );
                            return Err(error);
                        }
                    }
                };
                let planned_object_rows =
                    crate::git::push::uncovered_locator_object_rows(coverage, &bindings, packs);
                let pack_inventory_unchanged = anchor.is_some_and(|anchor| {
                    coverage.is_some_and(|coverage| coverage.pack_index_hash == anchor.pack_index_hash)
                }) && planned_object_rows == 0;
                let mut writer = if pack_inventory_unchanged {
                    crab_metadata::git_object_locator::GitObjectLocatorWriter::open_for_coverage_update(
                        Arc::clone(store.inner()),
                        router.repo_prefix(),
                    )
                    .await?
                } else {
                    crab_metadata::git_object_locator::GitObjectLocatorWriter::open_for_publication(
                        Arc::clone(store.inner()),
                        router.repo_prefix(),
                        planned_object_rows,
                    )
                    .await?
                };
                let result = async {
                    let current = anchor.is_none_or(|anchor| {
                        writer.coverage()
                            == Some(crab_metadata::git_object_locator::GitLocatorCoverage {
                                generation: anchor.generation,
                                pack_index_hash: anchor.pack_index_hash,
                            })
                    });
                    let (advanced, sweep) = if current {
                        (
                            false,
                            crab_metadata::git_object_locator::LocatorSweepStats::default(),
                        )
                    } else if let Some(anchor) = anchor {
                        crate::git::push::publish_pack_locator_inventory_for_owner(
                            &mut writer,
                            store,
                            router,
                            anchor,
                            packs,
                            cancel,
                        )
                        .await?
                    } else {
                        (
                            false,
                            crab_metadata::git_object_locator::LocatorSweepStats::default(),
                        )
                    };
                    Ok::<_, CrabError>((advanced, writer.catalog_stats().await?, sweep))
                }
                .await;
                let close = writer.close().await.map_err(CrabError::from);
                match (result, close) {
                    (Ok(result), Ok(_)) => Ok(result),
                    (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
                    (Err(error), Err(close_error)) => {
                        warn!(
                            error = %close_error,
                            "Git locator close also failed after owner publication"
                        );
                        Err(error)
                    }
                }
            },
        ),
    )
    .await;
    let release = lock.release().await.map_err(CrabError::from);
    match (operation, release) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => {
            warn!(
                %error,
                error_debug = ?error,
                "Git generation owner locator lock release failed"
            );
            Err(error)
        }
    }
}

async fn maintain_split_commit_graph(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    manifest: &crab_metadata::manifests::Manifest,
    cancel: &CancellationToken,
) -> Result<CommitGraphMaintenance> {
    if manifest.refs.is_empty() {
        return Ok(CommitGraphMaintenance {
            action: "none",
            layers: 0,
            bytes: 0,
            bytes_read: 0,
            bytes_written: 0,
        });
    }
    let storage = store.as_storage();
    let storage_router =
        crab_storage::StoreLayout::new(storage.clone(), router.repo_prefix().to_owned());
    let rebuilt_hash;
    let (hash, rebuild) = if let Some(hash) = manifest.commit_graph_hash.as_deref() {
        (hash, None)
    } else {
        let Some(rebuild) =
            crate::git::push::rebuild_split_commit_graph_from_remote_packs_if_current(
                store,
                router,
                manifest.generation,
                GENERATION_OWNER_GRAPH_REBUILD_MAX_BYTES,
                cancel,
            )
            .await?
        else {
            return Ok(CommitGraphMaintenance {
                action: "superseded",
                layers: 0,
                bytes: 0,
                bytes_read: 0,
                bytes_written: 0,
            });
        };
        rebuilt_hash = rebuild.hash.clone();
        (rebuilt_hash.as_str(), Some(rebuild))
    };
    if rebuild.is_none() {
        let descriptor = crab_metadata::split_commit_graph::load_split_commit_graph_descriptor(
            storage,
            &storage_router,
            hash,
            GENERATION_OWNER_GRAPH_REBUILD_MAX_BYTES,
        )
        .await?;
        if descriptor.generation != manifest.generation
            || descriptor.pack_index_hash != manifest.pack_index_hash
            || descriptor.git_validation_digest != manifest.git_validation_digest
        {
            return Err(CrabError::CorruptObject {
                path: storage_router
                    .bulk_manifest_path("commit-graph", hash)
                    .to_string(),
                reason: "commit graph descriptor does not match the complete committed Git state"
                    .to_owned(),
            });
        }
        if !crab_metadata::split_commit_graph::split_commit_graph_compaction_due(&descriptor) {
            return Ok(CommitGraphMaintenance {
                action: "none",
                layers: u64::try_from(descriptor.layers.len()).unwrap_or(u64::MAX),
                bytes: descriptor.layers.iter().map(|layer| layer.bytes).sum(),
                bytes_read: 0,
                bytes_written: 0,
            });
        }
    }
    if let Some(rebuild) = rebuild {
        return Ok(CommitGraphMaintenance {
            action: if rebuild.incremental {
                "commit_graph_incremental"
            } else {
                "commit_graph_rebuild"
            },
            layers: rebuild.layers,
            bytes: rebuild.bytes,
            bytes_read: rebuild.bytes_read,
            bytes_written: rebuild.bytes_written,
        });
    }
    let graph = crab_metadata::split_commit_graph::load_split_commit_graph(
        storage,
        &storage_router,
        hash,
        GENERATION_OWNER_GRAPH_REBUILD_MAX_BYTES,
    )
    .await?;
    if graph.descriptor.generation != manifest.generation
        || graph.descriptor.pack_index_hash != manifest.pack_index_hash
        || graph.descriptor.git_validation_digest != manifest.git_validation_digest
        || manifest
            .refs
            .iter()
            .map(|(name, oid)| manifest.peeled_refs.get(name).unwrap_or(oid))
            .map(|oid| parse_commit_graph_oid(oid))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .any(|root| !graph.contains(root))
    {
        return Err(CrabError::CorruptObject {
            path: storage_router
                .bulk_manifest_path("commit-graph", hash)
                .to_string(),
            reason: "commit graph does not match the complete committed Git state".to_owned(),
        });
    }
    let current_layers = u64::try_from(graph.descriptor.layers.len()).unwrap_or(u64::MAX);
    let current_bytes = graph
        .descriptor
        .layers
        .iter()
        .map(|layer| layer.bytes)
        .sum();
    let Some(write) = crab_metadata::split_commit_graph::compact_split_commit_graph(graph)? else {
        return Ok(CommitGraphMaintenance {
            action: "none",
            layers: current_layers,
            bytes: current_bytes,
            bytes_read: current_bytes,
            bytes_written: 0,
        });
    };
    crab_metadata::split_commit_graph::upload_split_commit_graph(storage, &storage_router, &write)
        .await?;
    for _ in 0..3 {
        let (mut current, etag) = crate::metadata::manifest::read_manifest(store, router).await?;
        if current.generation != manifest.generation
            || current.pack_index_hash != manifest.pack_index_hash
            || current.git_validation_digest != manifest.git_validation_digest
            || current.commit_graph_hash.as_deref() != Some(hash)
        {
            return Ok(CommitGraphMaintenance {
                action: "superseded",
                layers: current_layers,
                bytes: current_bytes,
                bytes_read: current_bytes,
                bytes_written: 0,
            });
        }
        current.commit_graph_hash = Some(write.descriptor_hash.clone());
        match crate::metadata::manifest::write_manifest_cas(store, router, &current, &etag).await {
            Ok(_) => {
                let descriptor = crab_metadata::split_commit_graph::decode_commit_graph_descriptor(
                    &write.descriptor_bytes,
                    "commit graph descriptor",
                )?;
                return Ok(CommitGraphMaintenance {
                    action: "commit_graph_compaction",
                    layers: u64::try_from(descriptor.layers.len()).unwrap_or(u64::MAX),
                    bytes: descriptor.layers.iter().map(|layer| layer.bytes).sum(),
                    bytes_read: current_bytes,
                    bytes_written: write
                        .layers
                        .iter()
                        .map(|layer| layer.bytes.len() as u64)
                        .sum::<u64>()
                        .saturating_add(write.descriptor_bytes.len() as u64),
                });
            }
            Err(CrabError::CasConflict { .. }) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CrabError::CasConflict {
        path: router.manifest_path().to_string(),
        expected_etag: None,
    })
}

fn parse_commit_graph_oid(value: &str) -> Result<[u8; 20]> {
    let oid = gix_hash::ObjectId::from_hex(value.as_bytes()).map_err(|error| {
        CrabError::Internal(format!("invalid commit graph SHA-1 {value}: {error}"))
    })?;
    oid.as_bytes()
        .try_into()
        .map_err(|_| CrabError::Internal(format!("commit graph object ID is not SHA-1: {value}")))
}

fn render_generation_owner_sample(sample: &GenerationOwnerSample, jsonl: bool) -> Result<()> {
    if jsonl {
        let mut stream = JsonlStream::new("metadb.owner", "1.0", std::io::stdout());
        stream.emit_snapshot(sample);
    } else {
        info!(
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

// --- diagnose -------------------------------------------------------

async fn run_diagnose(
    db: DbSelector,
    mode: OutputMode,
    deep: bool,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    let (store, repo_prefix, bucket_identity, config) = resolve_repo_store(cancel).await?;
    let metadb_config = config.build_metadb_config(&repo_prefix);
    // Diagnose only reads sys:* keys — open read-only so a
    // concurrent push is not fenced.
    let metadb = build_metadb(
        Arc::clone(&store),
        repo_prefix,
        &bucket_identity,
        true,
        &config,
    );
    let guard = MetaDbGuard::new(metadb);

    let file_index = if db.includes_file_index() {
        Some(diagnose_file_index(&guard, deep, &store, &metadb_config.file_index_path).await)
    } else {
        None
    };
    let chunk_index = if db.includes_chunk_index() {
        Some(diagnose_chunk_index(&guard, deep, &store, &metadb_config.chunk_index_path).await)
    } else {
        None
    };

    let payload = DiagnosePayload {
        file_index,
        chunk_index,
    };

    guard.close().await?;
    check_cancelled(cancel)?;
    render_diagnose(&payload, mode);
    Ok(())
}

async fn diagnose_file_index(
    guard: &MetaDbGuard,
    deep: bool,
    store: &Arc<dyn ObjectStore>,
    db_path: &str,
) -> DbDiagnosis {
    match guard.file_index_system_keys().await {
        Ok(snap) => {
            let deep_integrity = if deep {
                Some(
                    run_deep_integrity(
                        guard.file_index_db_handle().await.ok(),
                        store,
                        db_path,
                        DbKind::FileIndex,
                    )
                    .await,
                )
            } else {
                None
            };
            DbDiagnosis {
                label: "file_index_db",
                path: String::from("file_index_db/"),
                opened: true,
                error: None,
                format_version: snap.format_version,
                epoch: snap.epoch,
                created_at: snap.created_at_unix_ms.map(unix_ms_to_iso8601),
                gc_generation: snap.gc_generation,
                deep_integrity,
            }
        }
        Err(CrabError::MetaDb(crate::core::error::MetaDbError::ReadOnlyUninitialized {
            ..
        })) => DbDiagnosis {
            label: "file_index_db",
            path: String::from("file_index_db/"),
            opened: false,
            error: Some(String::from(
                "database not initialized (no manifest on object storage yet)",
            )),
            format_version: None,
            epoch: None,
            created_at: None,
            gc_generation: None,
            deep_integrity: None,
        },
        Err(e) => DbDiagnosis {
            label: "file_index_db",
            path: String::from("file_index_db/"),
            opened: false,
            error: Some(e.to_string()),
            format_version: None,
            epoch: None,
            created_at: None,
            gc_generation: None,
            deep_integrity: None,
        },
    }
}

async fn diagnose_chunk_index(
    guard: &MetaDbGuard,
    deep: bool,
    store: &Arc<dyn ObjectStore>,
    db_path: &str,
) -> DbDiagnosis {
    match guard.chunk_index_system_keys().await {
        Ok(snap) => {
            let deep_integrity = if deep {
                Some(
                    run_deep_integrity(
                        guard.chunk_index_db_handle().await.ok(),
                        store,
                        db_path,
                        DbKind::ChunkIndex,
                    )
                    .await,
                )
            } else {
                None
            };
            DbDiagnosis {
                label: "chunk_index_db",
                path: String::from(".crab/chunk_index_db/"),
                opened: true,
                error: None,
                format_version: snap.format_version,
                epoch: snap.epoch,
                created_at: snap.created_at_unix_ms.map(unix_ms_to_iso8601),
                gc_generation: snap.gc_generation,
                deep_integrity,
            }
        }
        Err(CrabError::MetaDb(crate::core::error::MetaDbError::ReadOnlyUninitialized {
            ..
        })) => DbDiagnosis {
            label: "chunk_index_db",
            path: String::from(".crab/chunk_index_db/"),
            opened: false,
            error: Some(String::from(
                "database not initialized (no manifest on object storage yet)",
            )),
            format_version: None,
            epoch: None,
            created_at: None,
            gc_generation: None,
            deep_integrity: None,
        },
        Err(e) => DbDiagnosis {
            label: "chunk_index_db",
            path: String::from(".crab/chunk_index_db/"),
            opened: false,
            error: Some(e.to_string()),
            format_version: None,
            epoch: None,
            created_at: None,
            gc_generation: None,
            deep_integrity: None,
        },
    }
}

// --- deep integrity -------------------------------------------------

use crate::metadata::metadb::stores;
use crab_metadata::key_codec::{CONTENT_KEY_LEN, PREFIX_CONTENT, PREFIX_SYSTEM};
use crab_metadata::value_codec::{CHUNK_INDEX_VALUE_LEN, FILE_INDEX_VALUE_LEN};

/// Which logical database we're checking — determines expected value
/// sizes.
#[derive(Debug, Clone, Copy)]
enum DbKind {
    /// file_index_db: values are 32-byte shard hashes.
    FileIndex,
    /// chunk_index_db: values are 40-byte XorbRef encodings.
    ChunkIndex,
}

impl DbKind {
    fn expected_value_len(self) -> usize {
        match self {
            Self::FileIndex => FILE_INDEX_VALUE_LEN,
            Self::ChunkIndex => CHUNK_INDEX_VALUE_LEN,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::FileIndex => stores::file_index::DB_LABEL,
            Self::ChunkIndex => stores::chunk_index::DB_LABEL,
        }
    }
}

/// Maximum number of corruption samples to collect before stopping
/// detailed recording. Keeps output bounded for badly damaged DBs.
const MAX_CORRUPTION_SAMPLES: usize = 20;

/// Lowercase hex encoding for short byte slices in diagnostic output.
fn short_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Run a full-scan deep integrity check over one database.
///
/// Two independent checks run in parallel:
/// 1. **Key/value scan** — iterates every entry via `Db::scan`,
///    classifies keys by prefix, and validates value lengths.
/// 2. **Object-store enumeration** — lists all files under the
///    database path to report storage-level health (file count,
///    total bytes).
async fn run_deep_integrity(
    db_handle: Option<std::sync::Arc<crate::metadata::metadb::Db>>,
    store: &Arc<dyn ObjectStore>,
    db_path: &str,
    kind: DbKind,
) -> DeepIntegrityResult {
    let (scan_result, storage_result) = tokio::join!(
        scan_all_keys(db_handle.as_ref(), kind),
        enumerate_storage(store, db_path),
    );

    let (
        total_keys,
        content_keys,
        system_keys,
        unknown_keys,
        corrupt_values,
        corruption_samples,
        scan_completed,
    ) = scan_result;
    let (object_store_files, object_store_bytes) = storage_result;

    let verdict = if !scan_completed {
        String::from("INCOMPLETE — iterator was invalidated before scan finished")
    } else if corrupt_values > 0 {
        format!(
            "CORRUPT — {corrupt_values} value(s) have unexpected length out of {content_keys} content entries"
        )
    } else if unknown_keys > 0 {
        format!("WARNING — {unknown_keys} key(s) with unrecognized prefix byte (not 0x01 or 0xFF)")
    } else if content_keys == 0 && total_keys == 0 {
        String::from("EMPTY — database contains no entries")
    } else {
        format!("OK — {content_keys} content entries verified, all values well-formed")
    };

    DeepIntegrityResult {
        total_keys,
        content_keys,
        system_keys,
        unknown_keys,
        corrupt_values,
        corruption_samples,
        object_store_files,
        object_store_bytes,
        scan_completed,
        verdict,
    }
}

/// Iterate every key in the database, classify by prefix, and validate
/// value lengths for content keys.
async fn scan_all_keys(
    db_handle: Option<&std::sync::Arc<crate::metadata::metadb::Db>>,
    kind: DbKind,
) -> (u64, u64, u64, u64, u64, Vec<String>, bool) {
    let Some(db) = db_handle else {
        return (0, 0, 0, 0, 0, Vec::new(), false);
    };

    let mut iter = match db.scan().await {
        Ok(it) => it,
        Err(e) => {
            warn!(db = kind.label(), error = %e, "deep integrity: scan open failed");
            return (0, 0, 0, 0, 0, vec![format!("scan open failed: {e}")], false);
        }
    };

    let expected_value_len = kind.expected_value_len();
    let mut total_keys: u64 = 0;
    let mut content_keys: u64 = 0;
    let mut system_keys: u64 = 0;
    let mut unknown_keys: u64 = 0;
    let mut corrupt_values: u64 = 0;
    let mut corruption_samples: Vec<String> = Vec::new();

    loop {
        match iter.next().await {
            Ok(Some(kv)) => {
                total_keys += 1;
                let key = kv.key.as_ref();

                if key.first() == Some(&PREFIX_CONTENT) {
                    content_keys += 1;

                    // Validate key length.
                    if key.len() != CONTENT_KEY_LEN {
                        corrupt_values += 1;
                        if corruption_samples.len() < MAX_CORRUPTION_SAMPLES {
                            corruption_samples.push(format!(
                                "key length {}, expected {CONTENT_KEY_LEN} (key prefix: {:02x?})",
                                key.len(),
                                &key[..key.len().min(4)]
                            ));
                        }
                        continue;
                    }

                    // Validate value length.
                    let value = kv.value.as_ref();
                    if value.len() != expected_value_len {
                        corrupt_values += 1;
                        if corruption_samples.len() < MAX_CORRUPTION_SAMPLES {
                            let key_hex = short_hex(&key[1..key.len().min(9)]);
                            corruption_samples.push(format!(
                                "key 0x01{key_hex}…: value length {}, expected {expected_value_len}",
                                value.len()
                            ));
                        }
                    }
                } else if key.first() == Some(&PREFIX_SYSTEM) {
                    system_keys += 1;
                } else {
                    unknown_keys += 1;
                    if corruption_samples.len() < MAX_CORRUPTION_SAMPLES {
                        corruption_samples.push(format!(
                            "unknown prefix byte {:#04x} on key of length {}",
                            key.first().copied().unwrap_or(0),
                            key.len()
                        ));
                    }
                }
            }
            Ok(None) => {
                // Scan completed normally.
                break;
            }
            Err(e) => {
                // Iterator invalidated (resource reclamation).
                warn!(
                    db = kind.label(),
                    error = %e,
                    scanned = total_keys,
                    "deep integrity: iterator invalidated mid-scan"
                );
                if corruption_samples.len() < MAX_CORRUPTION_SAMPLES {
                    corruption_samples
                        .push(format!("iterator invalidated after {total_keys} keys: {e}"));
                }
                return (
                    total_keys,
                    content_keys,
                    system_keys,
                    unknown_keys,
                    corrupt_values,
                    corruption_samples,
                    false,
                );
            }
        }
    }

    (
        total_keys,
        content_keys,
        system_keys,
        unknown_keys,
        corrupt_values,
        corruption_samples,
        true,
    )
}

/// Enumerate all object-store files under the database path and sum
/// their sizes.
async fn enumerate_storage(store: &Arc<dyn ObjectStore>, db_path: &str) -> (u64, u64) {
    let prefix = ObjectPath::from(db_path);
    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    let mut stream = store.list(Some(&prefix));
    loop {
        match stream.try_next().await {
            Ok(Some(meta)) => {
                file_count += 1;
                total_bytes += meta.size as u64;
            }
            Ok(None) => break,
            Err(e) => {
                warn!(path = db_path, error = %e, "deep integrity: object-store list failed");
                break;
            }
        }
    }

    (file_count, total_bytes)
}

/// Render a Unix millisecond timestamp as an ISO 8601 UTC string
/// (`YYYY-MM-DDTHH:MM:SSZ`). Sub-second precision is dropped so the
/// output stays stable across locales. Inline so the diagnose text
/// render doesn't need a date-time crate.
fn unix_ms_to_iso8601(ms: u64) -> String {
    let secs = ms / 1000;
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let hours = tod / 3600;
    let minutes = (tod % 3600) / 60;
    let seconds = tod % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since the Unix epoch into a `(year, month, day)`
/// triple using Howard Hinnant's civil-from-days algorithm. Matches
/// the helper in `git::push::days_to_ymd` so the two formatters stay
/// consistent.
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn render_diagnose(payload: &DiagnosePayload, mode: OutputMode) {
    if matches!(mode, OutputMode::Json) {
        emit_json("metadb.diagnose", "1.0", payload);
        return;
    }

    println!("crab metadb diagnose\n");
    for db in [payload.file_index.as_ref(), payload.chunk_index.as_ref()]
        .into_iter()
        .flatten()
    {
        render_db_diagnosis(db);
    }
}

fn render_db_diagnosis(d: &DbDiagnosis) {
    println!("[{}]  path={}", d.label, d.path);
    if !d.opened {
        let err = d.error.as_deref().unwrap_or("<unknown>");
        println!("  status: FAILED TO OPEN — {err}");
        println!();
        return;
    }
    println!("  status: open");
    match d.format_version {
        Some(v) => println!("  format_version: {v}"),
        None => println!("  format_version: <unset>"),
    }
    match d.epoch {
        Some(v) => println!("  epoch: {v}"),
        None => println!("  epoch: <unset>"),
    }
    match d.created_at.as_deref() {
        Some(v) => println!("  created_at: {v}"),
        None => println!("  created_at: <unset>"),
    }
    match d.gc_generation {
        Some(v) => println!("  gc_generation: {v}"),
        None => println!("  gc_generation: <not applicable>"),
    }
    match &d.deep_integrity {
        Some(di) => {
            println!("  deep_integrity:");
            println!("    verdict: {}", di.verdict);
            println!("    total_keys: {}", di.total_keys);
            println!("    content_keys: {}", di.content_keys);
            println!("    system_keys: {}", di.system_keys);
            if di.unknown_keys > 0 {
                println!("    unknown_keys: {}", di.unknown_keys);
            }
            if di.corrupt_values > 0 {
                println!("    corrupt_values: {}", di.corrupt_values);
            }
            println!("    object_store_files: {}", di.object_store_files);
            println!(
                "    object_store_bytes: {} ({:.2} MiB)",
                di.object_store_bytes,
                di.object_store_bytes as f64 / (1024.0 * 1024.0)
            );
            println!("    scan_completed: {}", di.scan_completed);
            if !di.corruption_samples.is_empty() {
                println!("    corruption_samples:");
                for sample in &di.corruption_samples {
                    println!("      - {sample}");
                }
            }
        }
        None => {
            println!("  deep_integrity: not requested (use --deep to enable)");
        }
    }
    println!();
}

// --- rebuild --------------------------------------------------------

#[derive(Debug, Serialize)]
struct RebuildPayload {
    repo_prefix: String,
    file_index_entries_written: u64,
    chunk_index_entries_written: u64,
    shards_processed: u64,
    shards_failed: u64,
    git_packs_processed: u64,
    git_packs_failed: u64,
    git_objects_written: u64,
    elapsed_ms: u64,
    notes: Vec<String>,
}

/// Batch size used to flush accumulated entries into SlateDB during a
/// rebuild pass. Chosen to keep a single `Transaction` bounded in
/// memory without making the commit fan-out run at tiny-batch
/// granularity.
const MAX_METADATA_SHARD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_METADATA_SHARDS: u64 = 1_000_000;
const MAX_METADATA_CONTROL_BYTES: u64 = 8 * 1024 * 1024;

fn create_shard_scan_workspace() -> Result<tempfile::TempDir> {
    let root = crate::cache::default_cache_root().join("maintenance");
    std::fs::create_dir_all(&root).map_err(CrabError::Io)?;
    tempfile::Builder::new()
        .prefix("crab-metadb-shard-")
        .tempdir_in(root)
        .map_err(CrabError::Io)
}

struct ParsedShardSpool {
    inner: Arc<ShardReplaySpool>,
    file_entries: u64,
    chunk_entries: u64,
}

async fn download_and_spool_shard(
    storage: &crab_storage::Store,
    shard_path: &ObjectPath,
    expected_hash: MerkleHash,
    workspace: &Path,
    include_file_index: bool,
    include_chunk_index: bool,
    cancel: &CancellationToken,
) -> Result<ParsedShardSpool> {
    check_cancelled(cancel)?;
    let expected_size = tokio::select! {
        result = storage.head(shard_path) => result?.size,
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
    };
    if expected_size > MAX_METADATA_SHARD_BYTES {
        return Err(CrabError::Configuration {
            key: "metadata shard size".to_owned(),
            origin: format!(
                "shard {expected_hash} is {expected_size} bytes; bounded index scans support at most {MAX_METADATA_SHARD_BYTES} bytes"
            ),
        });
    }
    let local_path = workspace.join(format!("shard-{}", expected_hash.hex()));
    let workspace_root = workspace.to_owned();
    let downloaded_size = tokio::select! {
        result = storage.download_to_path_bounded(shard_path, &local_path, MAX_METADATA_SHARD_BYTES) => result?,
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
    };
    if downloaded_size != expected_size {
        return Err(CrabError::CorruptObject {
            path: shard_path.to_string(),
            reason: format!(
                "shard size changed during download: expected {expected_size} bytes, downloaded {downloaded_size}"
            ),
        });
    }
    let result = tokio::task::spawn_blocking(move || {
        let source = std::fs::File::open(&local_path).map_err(CrabError::Io)?;
        let result = ShardReplaySpool::from_reader_in(
            source,
            &workspace_root,
            expected_hash,
            include_file_index,
            include_chunk_index,
        )
        .map_err(CrabError::from);
        let _ = std::fs::remove_file(&local_path);
        result
    })
    .await
    .map_err(|error| CrabError::Internal(format!("shard scan worker failed: {error}")))?;
    let inner = Arc::new(result?);
    Ok(ParsedShardSpool {
        file_entries: inner.file_entries,
        chunk_entries: inner.chunk_entries,
        inner,
    })
}

async fn read_spooled_file_batch(
    spool: Arc<ShardReplaySpool>,
    after_id: i64,
) -> Result<Vec<(i64, MerkleHash, [u8; 32])>> {
    tokio::task::spawn_blocking(move || {
        spool
            .file_batch(after_id, REPLAY_BATCH_ENTRIES)
            .map(|rows| {
                rows.into_iter()
                    .map(|row| (row.id, row.file_hash, row.recipe_hash))
                    .collect()
            })
            .map_err(CrabError::from)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("file replay worker failed: {error}")))?
}

async fn read_spooled_chunk_batch(
    spool: Arc<ShardReplaySpool>,
    after_id: i64,
) -> Result<Vec<(i64, MerkleHash, XorbRef)>> {
    tokio::task::spawn_blocking(move || {
        spool
            .chunk_batch(after_id, REPLAY_BATCH_ENTRIES)
            .map(|rows| {
                rows.into_iter()
                    .map(|row| {
                        (
                            row.id,
                            row.chunk_hash,
                            XorbRef {
                                xorb_hash: row.xorb_hash,
                                chunk_index: row.chunk_index,
                                uncompressed_size: row.uncompressed_size,
                            },
                        )
                    })
                    .collect()
            })
            .map_err(CrabError::from)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("chunk replay worker failed: {error}")))?
}

#[expect(
    clippy::too_many_arguments,
    reason = "replay rows are bound to independent repository, generation, registry, shard, and database anchors"
)]
async fn replay_spooled_shard(
    spool: &ParsedShardSpool,
    storage: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    repo_prefix: &str,
    shard_hash: MerkleHash,
    committed_generation: u64,
    shard_index_hash: MerkleHash,
    gc_registry_generation: u64,
    file_store: Option<&crate::metadata::FileIndexStore>,
    chunk_store: Option<&crate::metadata::ChunkIndexStore>,
    guard: &MetaDbGuard,
    cancel: &CancellationToken,
    verified_xorb: &mut Option<(MerkleHash, RebuildVerifiedXorb)>,
) -> Result<(u64, u64)> {
    let mut file_entries_written = 0_u64;
    let mut after_file_id = 0_i64;
    loop {
        check_cancelled(cancel)?;
        let rows = read_spooled_file_batch(Arc::clone(&spool.inner), after_file_id).await?;
        if rows.is_empty() {
            break;
        }
        let mut pending_file = Vec::with_capacity(rows.len());
        for (id, file_hash, recipe_hash) in rows {
            after_file_id = id;
            pending_file.push((
                file_hash,
                crab_metadata::value_codec::CommittedFileRecord {
                    recipe_hash,
                    shard_hash,
                    committed_generation,
                    shard_index_hash,
                },
            ));
        }
        let mut pending_chunk = Vec::new();
        let mut pending_committed_chunk = Vec::new();
        let (files, _) = flush_rebuild_batch(
            guard,
            file_store,
            None,
            &mut pending_file,
            &mut pending_chunk,
            &mut pending_committed_chunk,
            cancel,
        )
        .await?;
        file_entries_written = file_entries_written
            .checked_add(files)
            .ok_or_else(|| CrabError::Internal("rebuilt file entry count overflow".to_owned()))?;
    }
    if file_entries_written != spool.file_entries {
        return Err(CrabError::CorruptObject {
            path: format!("shard {} replay spool", shard_hash.hex()),
            reason: format!(
                "shard replay emitted {file_entries_written} file rows, expected {}",
                spool.file_entries
            ),
        });
    }

    let mut chunk_entries_written = 0_u64;
    let mut after_chunk_id = 0_i64;
    while chunk_store.is_some() {
        check_cancelled(cancel)?;
        let rows = read_spooled_chunk_batch(Arc::clone(&spool.inner), after_chunk_id).await?;
        if rows.is_empty() {
            break;
        }
        let mut pending_chunk = Vec::with_capacity(rows.len());
        for (id, chunk_hash, xorb_ref) in rows {
            after_chunk_id = id;
            pending_chunk.push((chunk_hash, xorb_ref));
        }
        let mut pending_committed_chunk = rebuild_committed_chunk_receipts(
            storage,
            router,
            repo_prefix,
            shard_hash,
            committed_generation,
            shard_index_hash,
            gc_registry_generation,
            cancel,
            &pending_chunk,
            verified_xorb,
        )
        .await?;
        let mut pending_file = Vec::new();
        let (_, chunks) = flush_rebuild_batch(
            guard,
            None,
            chunk_store,
            &mut pending_file,
            &mut pending_chunk,
            &mut pending_committed_chunk,
            cancel,
        )
        .await?;
        chunk_entries_written = chunk_entries_written
            .checked_add(chunks)
            .ok_or_else(|| CrabError::Internal("rebuilt chunk entry count overflow".to_owned()))?;
    }
    if chunk_entries_written != spool.chunk_entries {
        return Err(CrabError::CorruptObject {
            path: format!("shard {} replay spool", shard_hash.hex()),
            reason: format!(
                "shard replay emitted {chunk_entries_written} chunk rows, expected {}",
                spool.chunk_entries
            ),
        });
    }
    Ok((file_entries_written, chunk_entries_written))
}

async fn run_rebuild(db: DbSelector, mode: OutputMode, cancel: &CancellationToken) -> Result<()> {
    let (store, repo_prefix, bucket_identity, config) = resolve_repo_store(cancel).await?;
    crate::replication::ensure_active_active_maintenance_admitted(
        &config,
        "metadata index rebuild",
    )?;
    let storage = crate::storage::Store::new(Arc::clone(&store))
        .with_bucket_identity(bucket_identity.clone());
    let lease = crate::maintenance::RepositoryMaintenanceLease::acquire(
        &storage,
        crab_storage::GLOBAL_PREFIX,
        &repo_prefix,
        cancel,
    )
    .await?;
    let operation = run_rebuild_in(
        store,
        repo_prefix,
        &bucket_identity,
        db,
        mode,
        &config,
        cancel,
    )
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

/// Core rebuild entry point parameterised on the object store and
/// repo prefix so tests can drive it against an in-memory store
/// without touching `resolve_repo_store`.
async fn run_rebuild_in(
    store: Arc<dyn ObjectStore>,
    repo_prefix: String,
    bucket_identity: &crate::storage::store::BucketIdentity,
    db: DbSelector,
    mode: OutputMode,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    // Rebuild writes fresh entries into one or both databases — must
    // use read-write mode, which fences any concurrent writer on
    // purpose.
    let metadb = build_metadb(
        Arc::clone(&store),
        repo_prefix.clone(),
        bucket_identity,
        false,
        config,
    );
    let guard = MetaDbGuard::new(metadb);
    let emit_progress = !matches!(mode, OutputMode::Json);
    let result = rebuild_with_guard(&store, &repo_prefix, db, emit_progress, &guard, cancel).await;
    let result = close_rebuild_guard(guard, result).await;
    let payload = result?;
    render_rebuild_payload(&payload, mode);
    Ok(())
}

/// Rebuild `file_index_db` for the current repository and verify that
/// selected file-to-shard mappings are present afterwards.
pub(crate) async fn rebuild_file_index_for_current_repo_and_verify(
    entries: &[(MerkleHash, MerkleHash)],
) -> Result<Vec<bool>> {
    let cancel = CancellationToken::new();
    let (store, repo_prefix, bucket_identity, config) = resolve_repo_store(&cancel).await?;
    let storage = crate::storage::Store::new(Arc::clone(&store));
    let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.clone());
    let gc_writer = crate::maintenance::GcWriterLeases::acquire(
        &storage,
        router.global_prefix(),
        router.repo_prefix(),
        &cancel,
    )
    .await?;
    let metadb = build_metadb(
        Arc::clone(&store),
        repo_prefix.clone(),
        &bucket_identity,
        false,
        &config,
    );
    let guard = MetaDbGuard::new(metadb);

    let result: Result<Vec<bool>> = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(CrabError::Cancelled),
        result = async {
            rebuild_with_guard(
                &store,
                &repo_prefix,
                DbSelector::FileIndex,
                false,
                &guard,
                &cancel,
            )
            .await?;
            let file_store = guard.file_index().await?;
            let file_hashes: Vec<MerkleHash> =
                entries.iter().map(|(file_hash, _)| *file_hash).collect();
            let rebuilt = file_store.get_committed_batch(&file_hashes).await?;
            Ok(rebuilt
                .into_iter()
                .zip(entries.iter())
                .map(|(actual, (_, expected))| {
                    actual.is_some_and(|record| record.shard_hash == *expected)
                })
                .collect())
        } => result,
    };

    let result = close_rebuild_guard(guard, result).await;
    let release = gc_writer.release().await;
    match (result, release) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn close_rebuild_guard<T>(guard: MetaDbGuard, result: Result<T>) -> Result<T> {
    let close_result = guard.close().await;
    match (result, close_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), Ok(())) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(close_err)) => {
            warn!(error = %close_err, "metadb rebuild close failed after rebuild error");
            Err(err)
        }
    }
}

/// Counts for generation-pinned rows written before a candidate manifest is visible.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CandidateShardIndexPublication {
    pub(crate) gc_registry_generation: u64,
    pub(crate) file_entries_written: u64,
    pub(crate) chunk_entries_written: u64,
}

/// Write the immutable receipt proving that a manifest's derived indexes are complete.
pub(crate) async fn write_generation_index_receipt(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    manifest: &crab_metadata::manifests::Manifest,
) -> Result<()> {
    let parse_hash = |value: &str, label: &str| -> Result<MerkleHash> {
        if value.is_empty() {
            return Ok(MerkleHash::default());
        }
        MerkleHash::from_hex(value)
            .map_err(|error| CrabError::Internal(format!("{label} hash invalid: {error}")))
    };
    let shard_index_hash = parse_hash(&manifest.shard_index_hash, "manifest shard-index")?;
    let pack_index_hash = parse_hash(&manifest.pack_index_hash, "manifest pack-index")?;
    let receipt = crab_metadata::receipts::GenerationIndexReceipt {
        schema_version: crab_metadata::receipts::RECEIPT_SCHEMA_VERSION,
        generation: manifest.generation,
        shard_index_hash: shard_index_hash.into(),
        pack_index_hash: pack_index_hash.into(),
        file_index_digest: crab_metadata::receipts::generation_file_index_digest(
            shard_index_hash.into(),
        ),
        git_object_locator_digest: crab_metadata::receipts::generation_git_object_locator_digest(
            pack_index_hash.into(),
        ),
    };
    receipt
        .validate(
            manifest.generation,
            shard_index_hash.into(),
            pack_index_hash.into(),
        )
        .map_err(CrabError::from)?;
    let path = router.repo_path(&format!(
        "metadata/generation-receipts/{:020}.json",
        manifest.generation
    ));
    let body = serde_json::to_vec(&receipt)
        .map_err(|error| CrabError::Internal(format!("receipt serialize: {error}")))?;
    match store.put(&path, Bytes::from(body)).await {
        Ok(()) => Ok(()),
        Err(CrabError::CasConflict { .. }) => {
            let (existing, _) = store
                .get_with_etag_bounded(&path, MAX_METADATA_CONTROL_BYTES)
                .await?;
            let existing: crab_metadata::receipts::GenerationIndexReceipt =
                serde_json::from_slice(&existing).map_err(|error| CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: format!("generation-index receipt decode failed: {error}"),
                })?;
            existing
                .validate(
                    manifest.generation,
                    shard_index_hash.into(),
                    pack_index_hash.into(),
                )
                .map_err(CrabError::from)?;
            if existing != receipt {
                return Err(CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: "generation-index receipt conflicts with the committed index digest"
                        .to_owned(),
                });
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Publish generation-bound file/chunk rows before exposing a candidate shard set.
pub(crate) async fn publish_candidate_shard_indexes(
    store: &crate::storage::store::Store,
    repo_prefix: &str,
    candidate: &crab_metadata::manifests::Manifest,
    shard_hashes: &[String],
    config: &Config,
    cancel: &CancellationToken,
) -> Result<CandidateShardIndexPublication> {
    if candidate.generation == 0 || candidate.shard_index_hash.is_empty() {
        return Err(CrabError::Internal(
            "candidate shard indexes require a non-zero manifest anchor".to_owned(),
        ));
    }
    let metadb = build_metadb(
        Arc::clone(store.inner()),
        repo_prefix.to_owned(),
        &store.bucket_identity(),
        false,
        config,
    );
    let guard = MetaDbGuard::new(metadb);
    let result = publish_candidate_shard_indexes_with_guard(
        store,
        repo_prefix,
        candidate,
        shard_hashes,
        &guard,
        cancel,
    )
    .await;
    close_rebuild_guard(guard, result).await
}

async fn publish_candidate_shard_indexes_with_guard(
    store: &crate::storage::store::Store,
    repo_prefix: &str,
    candidate: &crab_metadata::manifests::Manifest,
    shard_hashes: &[String],
    guard: &MetaDbGuard,
    cancel: &CancellationToken,
) -> Result<CandidateShardIndexPublication> {
    check_cancelled(cancel)?;
    let storage = store.as_storage();
    let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
    let shard_index_hash = MerkleHash::from_hex(&candidate.shard_index_hash).map_err(|error| {
        CrabError::Internal(format!("candidate shard-index hash invalid: {error}"))
    })?;
    let gc_registry_generation = crab_metadata::ref_registry::union_register_repo_shards(
        storage,
        &router,
        shard_hashes.to_vec(),
    )
    .await?;
    check_cancelled(cancel)?;
    let file_store = guard.file_index().await?;
    let chunk_store = guard.chunk_index().await?;
    let mut file_entries_written = 0_u64;
    let mut chunk_entries_written = 0_u64;
    let mut verified_xorb = None;
    let workspace = create_shard_scan_workspace()?;

    for shard_hash_hex in shard_hashes {
        check_cancelled(cancel)?;
        let shard_hash =
            MerkleHash::from_hex(shard_hash_hex).map_err(|error| CrabError::CorruptObject {
                path: router.shard_path(shard_hash_hex).to_string(),
                reason: format!("candidate shard hash is invalid: {error}"),
            })?;
        let shard_path = router.shard_path(shard_hash_hex);
        let spool = download_and_spool_shard(
            storage,
            &shard_path,
            shard_hash,
            workspace.path(),
            true,
            true,
            cancel,
        )
        .await?;
        let (files, chunks) = replay_spooled_shard(
            &spool,
            storage,
            &router,
            repo_prefix,
            shard_hash,
            candidate.generation,
            shard_index_hash,
            gc_registry_generation,
            Some(&file_store),
            Some(&chunk_store),
            guard,
            cancel,
            &mut verified_xorb,
        )
        .await?;
        file_entries_written = file_entries_written
            .checked_add(files)
            .ok_or_else(|| CrabError::Internal("candidate file count overflow".to_owned()))?;
        chunk_entries_written = chunk_entries_written
            .checked_add(chunks)
            .ok_or_else(|| CrabError::Internal("candidate chunk count overflow".to_owned()))?;
    }
    Ok(CandidateShardIndexPublication {
        gc_registry_generation,
        file_entries_written,
        chunk_entries_written,
    })
}

/// Inner rebuild driver. Kept separate so tests can feed a tempdir-
/// anchored `MetaDb` in without going through the `build_metadb`
/// cache-root plumbing.
async fn rebuild_with_guard(
    store: &Arc<dyn ObjectStore>,
    repo_prefix: &str,
    db: DbSelector,
    emit_progress: bool,
    guard: &MetaDbGuard,
    cancel: &CancellationToken,
) -> Result<RebuildPayload> {
    check_cancelled(cancel)?;
    let start = std::time::Instant::now();
    let storage = crab_storage::Store::new(Arc::clone(store));
    let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
    let (manifest, _) = crab_metadata::manifest_store::read_manifest(&storage, &router).await?;
    let committed_shards = if manifest.shard_index_hash.is_empty() {
        Vec::new()
    } else {
        crab_metadata::manifest_store::read_bulk_shard_list_with_limit(
            &storage,
            &router,
            &manifest.shard_index_hash,
            MAX_METADATA_SHARDS,
        )
        .await?
    };
    let shard_index_hash = if manifest.shard_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        MerkleHash::from_hex(&manifest.shard_index_hash).map_err(|error| {
            CrabError::Internal(format!("manifest shard-index hash invalid: {error}"))
        })?
    };
    let gc_registry_generation = if db.includes_chunk_index() && !committed_shards.is_empty() {
        let generation = crab_metadata::ref_registry::union_register_repo_shards(
            &storage,
            &router,
            committed_shards.clone(),
        )
        .await?;
        check_cancelled(cancel)?;
        generation
    } else {
        0
    };

    let file_store = if db.includes_file_index() {
        Some(guard.file_index().await?)
    } else {
        None
    };
    let chunk_store = if db.includes_chunk_index() {
        Some(guard.chunk_index().await?)
    } else {
        None
    };

    let mut shards_processed: u64 = 0;
    let shards_failed: u64 = 0;
    let mut file_entries_written: u64 = 0;
    let mut chunk_entries_written: u64 = 0;
    let mut notes: Vec<String> = Vec::new();
    let mut verified_xorb = None;
    let workspace = create_shard_scan_workspace()?;

    for shard_hash_hex in committed_shards {
        check_cancelled(cancel)?;
        let shard_hash =
            MerkleHash::from_hex(&shard_hash_hex).map_err(|error| CrabError::CorruptObject {
                path: router.shard_path(&shard_hash_hex).to_string(),
                reason: format!("committed shard hash is invalid: {error}"),
            })?;
        let shard_path = router.shard_path(&shard_hash_hex);
        let spool = download_and_spool_shard(
            &storage,
            &shard_path,
            shard_hash,
            workspace.path(),
            db.includes_file_index(),
            db.includes_chunk_index(),
            cancel,
        )
        .await?;
        let (files, chunks) = replay_spooled_shard(
            &spool,
            &storage,
            &router,
            repo_prefix,
            shard_hash,
            manifest.generation,
            shard_index_hash,
            gc_registry_generation,
            file_store.as_ref(),
            chunk_store.as_ref(),
            guard,
            cancel,
            &mut verified_xorb,
        )
        .await?;

        shards_processed += 1;
        file_entries_written = file_entries_written
            .checked_add(files)
            .ok_or_else(|| CrabError::Internal("rebuilt file count overflow".to_owned()))?;
        chunk_entries_written = chunk_entries_written
            .checked_add(chunks)
            .ok_or_else(|| CrabError::Internal("rebuilt chunk count overflow".to_owned()))?;

        if emit_progress {
            println!(
                "  rebuilding: {shards_processed} shard(s) processed, \
                 {file_entries_written} file entries / {chunk_entries_written} chunk entries emitted",
            );
        }
    }

    let (git_packs_processed, git_packs_failed, git_objects_written, git_object_locator_digest) =
        if matches!(db, DbSelector::Both) {
            rebuild_git_object_locators(&storage, &router, &manifest).await?
        } else {
            (0, 0, 0, [0; 32])
        };
    if git_packs_failed > 0 {
        return Err(CrabError::CorruptObject {
            path: router.manifest_path().to_string(),
            reason: format!(
                "metadata rebuild could not verify {git_packs_failed} committed Git pack(s)"
            ),
        });
    }

    if matches!(db, DbSelector::Both) {
        let pack_index_hash = if manifest.pack_index_hash.is_empty() {
            MerkleHash::default()
        } else {
            MerkleHash::from_hex(&manifest.pack_index_hash).map_err(|error| {
                CrabError::Internal(format!("manifest pack-index hash invalid: {error}"))
            })?
        };
        let receipt = crab_metadata::receipts::GenerationIndexReceipt {
            schema_version: crab_metadata::receipts::RECEIPT_SCHEMA_VERSION,
            generation: manifest.generation,
            shard_index_hash: shard_index_hash.into(),
            pack_index_hash: pack_index_hash.into(),
            file_index_digest: crab_metadata::receipts::generation_file_index_digest(
                shard_index_hash.into(),
            ),
            git_object_locator_digest,
        };
        receipt
            .validate(
                manifest.generation,
                shard_index_hash.into(),
                pack_index_hash.into(),
            )
            .map_err(CrabError::from)?;
        let receipt_path = router.repo_path(&format!(
            "metadata/generation-receipts/{:020}.json",
            manifest.generation
        ));
        let body = serde_json::to_vec(&receipt)
            .map_err(|error| CrabError::Internal(format!("receipt serialize: {error}")))?;
        match storage.put(&receipt_path, Bytes::from(body.clone())).await {
            Ok(()) => {}
            Err(crab_storage::StorageError::StateConflict { .. }) => {
                let (existing, _) = storage.get_with_etag(&receipt_path).await?;
                let existing: crab_metadata::receipts::GenerationIndexReceipt =
                    serde_json::from_slice(&existing).map_err(|error| {
                        CrabError::CorruptObject {
                            path: receipt_path.to_string(),
                            reason: format!("generation-index receipt decode failed: {error}"),
                        }
                    })?;
                existing
                    .validate(
                        manifest.generation,
                        shard_index_hash.into(),
                        pack_index_hash.into(),
                    )
                    .map_err(CrabError::from)?;
                if existing != receipt {
                    return Err(CrabError::CorruptObject {
                        path: receipt_path.to_string(),
                        reason:
                            "generation-index receipt conflicts with the committed index digest"
                                .to_owned(),
                    });
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    if shards_processed == 0 {
        notes.push("manifest has no committed shards; nothing to rebuild".to_owned());
    }
    if !db.includes_file_index() {
        notes.push(String::from("--db chunk_index: file-index entries skipped"));
    }
    if !db.includes_chunk_index() {
        notes.push(String::from("--db file_index: chunk-index entries skipped"));
    }

    let payload = RebuildPayload {
        repo_prefix: String::from(repo_prefix),
        file_index_entries_written: file_entries_written,
        chunk_index_entries_written: chunk_entries_written,
        shards_processed,
        shards_failed,
        git_packs_processed,
        git_packs_failed,
        git_objects_written,
        elapsed_ms: start.elapsed().as_millis() as u64,
        notes,
    };

    Ok(payload)
}

async fn rebuild_git_object_locators(
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    manifest: &crab_metadata::manifests::Manifest,
) -> Result<(u64, u64, u64, [u8; 32])> {
    let pack_index_hash = if manifest.pack_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        MerkleHash::from_hex(&manifest.pack_index_hash).map_err(|error| {
            CrabError::Internal(format!("manifest pack-index hash invalid: {error}"))
        })?
    };
    let packs = if manifest.pack_index_hash.is_empty() {
        Vec::new()
    } else {
        crab_metadata::manifest_store::read_bulk_pack_list(store, router, &manifest.pack_index_hash)
            .await?
    };
    let visibility_temp = tempfile::tempdir()?;
    crab_git::initialize_bare_git_dir(visibility_temp.path())?;
    let visibility_pack_dir = visibility_temp.path().join("objects/pack");
    std::fs::create_dir_all(&visibility_pack_dir)?;
    let mut failed = 0u64;
    let mut derived = Vec::with_capacity(packs.len());
    for pack in packs {
        let pack_id = match MerkleHash::from_hex(&pack.pack_id) {
            Ok(pack_id) => pack_id,
            Err(error) => {
                warn!(pack_id = %pack.pack_id, error = %error, "skipping invalid committed pack id during locator rebuild");
                failed += 1;
                continue;
            }
        };
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pack");
        let downloaded = match store
            .download_to_path_bounded(&router.pack_path(&pack.pack_id), &source, pack.size)
            .await
        {
            Ok(size) => size,
            Err(error) => {
                warn!(pack_id = %pack.pack_id, error = %error, "failed to read committed pack during locator rebuild");
                failed += 1;
                continue;
            }
        };
        if downloaded != pack.size {
            warn!(pack_id = %pack.pack_id, downloaded, expected = pack.size, "committed pack size mismatch during locator rebuild");
            failed += 1;
            continue;
        }
        let canonical_name = pack.pack_id.clone();
        let expected_object_count = pack.object_count;
        let visibility_pack_dir_for_pack = visibility_pack_dir.clone();
        let verified = tokio::task::spawn_blocking(move || -> Result<_> {
            let pack_dir = temp.path().join("objects/pack");
            std::fs::create_dir_all(&pack_dir)?;
            let installed = crab_git::pack::install_pack_file_from_path(
                &pack_dir,
                &source,
                &canonical_name,
                0,
                false,
            )?;
            let mut locations = crab_git::pack_locator::PackLocationIter::open(
                &installed.idx_path,
                &installed.rev_path,
                downloaded,
            )
            .map_err(crab_git::pack::PackError::from)?;
            if locations.object_count() != expected_object_count {
                return Err(CrabError::CorruptObject {
                    path: source.display().to_string(),
                    reason: format!(
                        "manifest records {expected_object_count} objects but index contains {}",
                        locations.object_count()
                    ),
                });
            }
            if locations.pack_checksum().to_string() != installed.git_sha1 {
                return Err(CrabError::CorruptObject {
                    path: source.display().to_string(),
                    reason: "pack index checksum disagrees with pack trailer".to_owned(),
                });
            }
            crab_git::pack::install_pack_file_from_path(
                &visibility_pack_dir_for_pack,
                &source,
                &canonical_name,
                0,
                false,
            )?;
            let sample_indexes = sampled_location_indexes(locations.len());
            let mut samples = Vec::with_capacity(sample_indexes.len());
            for (index, location) in (&mut locations).enumerate() {
                let location = location.map_err(crab_git::pack::PackError::from)?;
                if sample_indexes.binary_search(&index).is_ok() {
                    samples.push(location);
                }
            }
            verify_sampled_pack_ranges(&source, &samples)?;
            let mut file = std::fs::File::open(&source)?;
            let mut hasher = blake3::Hasher::new();
            std::io::copy(&mut file, &mut hasher)?;
            let mut index_file = std::fs::File::open(&installed.idx_path)?;
            let index_size = index_file.metadata()?.len();
            let mut index_hasher = blake3::Hasher::new();
            std::io::copy(&mut index_file, &mut index_hasher)?;
            Ok((
                hasher.finalize().to_hex().to_string(),
                temp,
                installed.idx_path,
                installed.rev_path,
                installed.git_sha1,
                index_size,
                *index_hasher.finalize().as_bytes(),
            ))
        })
        .await
        .map_err(|error| CrabError::Internal(format!("pack rebuild join failed: {error}")))?;
        let (
            actual_pack_id,
            temp,
            index_path,
            reverse_index_path,
            git_sha1,
            index_size,
            index_hash,
        ) = match verified {
            Ok(verified) => verified,
            Err(error) => {
                warn!(pack_id = %pack.pack_id, error = %error, "committed pack verification failed during locator rebuild");
                failed += 1;
                continue;
            }
        };
        if actual_pack_id != pack.pack_id {
            warn!(pack_id = %pack.pack_id, "committed pack hash mismatch during locator rebuild");
            failed += 1;
            continue;
        }
        if let Err(error) = store
            .put_multipart_file_retry(
                &router.pack_index_path(&pack.pack_id),
                &index_path,
                index_size,
                index_hash,
                8 * 1024 * 1024,
                &tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
        {
            warn!(pack_id = %pack.pack_id, error = %error, "failed to upload rebuilt canonical pack index");
            failed += 1;
            continue;
        }
        derived.push((
            pack,
            pack_id,
            temp,
            index_path,
            reverse_index_path,
            git_sha1,
        ));
    }

    if failed != 0 {
        return Ok((
            0,
            failed,
            0,
            crab_metadata::receipts::generation_git_object_locator_digest(pack_index_hash.into()),
        ));
    }

    let mut kind_by_pack = HashMap::with_capacity(derived.len());
    for (pack, pack_id, _, index_path, reverse_index_path, _) in &derived {
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            index_path,
            reverse_index_path,
            pack.size,
        )
        .map_err(crab_git::pack::PackError::from)?;
        let object_ids = locations
            .by_ref()
            .map(|location| {
                location
                    .map(|location| location.oid)
                    .map_err(crab_git::pack::PackError::from)
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(CrabError::from)?;
        let object_count = object_ids.len();
        let git_dir = visibility_temp.path().to_owned();
        let kinds = tokio::task::spawn_blocking(move || {
            crab_git::object_kinds_from_git_dir(&git_dir, &object_ids)
        })
        .await
        .map_err(|error| {
            CrabError::Internal(format!(
                "Git object-kind catalog worker failed during locator rebuild: {error}"
            ))
        })?
        .map_err(CrabError::from)?;
        if kinds.len() != object_count {
            return Err(CrabError::Internal(format!(
                "Git object-kind catalog returned {} objects for a pack containing {}",
                kinds.len(),
                object_count
            )));
        }
        let kinds = kinds
            .into_iter()
            .map(|(oid, kind)| {
                let oid: [u8; 20] = oid.as_bytes().try_into().map_err(|_| {
                    CrabError::Internal(
                        "Git object-kind catalog returned a non-SHA1 object".to_owned(),
                    )
                })?;
                let kind = match kind {
                    gix_object::Kind::Commit => {
                        crab_metadata::git_object_locator::GitObjectKind::Commit
                    }
                    gix_object::Kind::Tree => {
                        crab_metadata::git_object_locator::GitObjectKind::Tree
                    }
                    gix_object::Kind::Blob => {
                        crab_metadata::git_object_locator::GitObjectKind::Blob
                    }
                    gix_object::Kind::Tag => crab_metadata::git_object_locator::GitObjectKind::Tag,
                };
                Ok((oid, kind))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        kind_by_pack.insert(*pack_id, kinds);
    }

    let mut lock = crab_coordination::PushLock::acquire_internal_default(
        store.inner(),
        router.repo_prefix(),
        crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
    )
    .await?;
    let write_result = crate::git::push::while_renewing_internal_lock(&mut lock, async {
        let (current, _) = crab_metadata::manifest_store::read_manifest(store, router).await?;
        if current.generation != manifest.generation
            || current.pack_index_hash != manifest.pack_index_hash
        {
            return Err(CrabError::CasConflict {
                path: router.manifest_path().as_ref().to_owned(),
                expected_etag: None,
            });
        }
        let records = derived
            .iter()
            .map(|(pack, pack_id, _, _, _, _)| {
                crab_metadata::git_object_locator::GitPackLocatorRecord {
                    pack_id: *pack_id,
                    committed_generation: manifest.generation,
                    pack_index_hash,
                    object_count: pack.object_count,
                    pack_size: pack.size,
                }
            })
            .collect::<Vec<_>>();
        let mut writer = crab_metadata::git_object_locator::GitObjectLocatorWriter::open(
            Arc::clone(store.inner()),
            router.repo_prefix(),
        )
        .await?;
        let operation = async {
            let bindings = writer.bind_packs(&records).await?;
            let retained_slots: HashSet<_> =
                bindings.iter().map(|binding| binding.pack_slot).collect();
            // Rebuild is the explicit format-migration boundary. Resetting the
            // canonical object universe before replay makes retries idempotent
            // and prevents ordinals from depending on an interrupted attempt.
            writer.replace_object_catalog(&retained_slots).await?;
            for (binding, (_, pack_id, _, index_path, reverse_index_path, git_sha1)) in
                bindings.into_iter().zip(&derived)
            {
                let mut locations = crab_git::pack_locator::PackLocationIter::open(
                    index_path,
                    reverse_index_path,
                    binding.record.pack_size,
                )
                .map_err(crab_git::pack::PackError::from)?;
                if locations.pack_checksum().to_string() != *git_sha1 {
                    return Err(CrabError::CorruptObject {
                        path: index_path.display().to_string(),
                        reason: "pack index checksum changed during locator rebuild".to_owned(),
                    });
                }
                let mut entries = Vec::with_capacity(25_000);
                for location in &mut locations {
                    let location = location.map_err(crab_git::pack::PackError::from)?;
                    let oid: [u8; 20] = location.oid.as_bytes().try_into().map_err(|_| {
                        CrabError::Internal(
                            "rebuilt pack index contains non-SHA1 object".to_owned(),
                        )
                    })?;
                    entries.push(crab_metadata::git_object_locator::GitObjectLocatorEntry {
                        oid,
                        location: crab_metadata::git_object_locator::GitObjectLocation {
                            pack_offset: location.pack_offset,
                            entry_len: location.entry_len,
                            crc32: location.crc32,
                        },
                        metadata: crab_metadata::git_object_locator::GitObjectMetadata {
                            kind: kind_by_pack
                                .get(pack_id)
                                .and_then(|kinds| kinds.get(&oid).copied()),
                            ..Default::default()
                        },
                    });
                    if entries.len() == 25_000 {
                        writer.write_locations(binding, &entries).await?;
                        entries.clear();
                    }
                }
                if !entries.is_empty() {
                    writer.write_locations(binding, &entries).await?;
                }
            }
            writer.complete_object_catalog_rebuild().await?;
            writer.flush_objects().await?;
            writer.sweep_unreferenced(&retained_slots).await?;
            let (after, _) = crab_metadata::manifest_store::read_manifest(store, router).await?;
            if after.generation != manifest.generation
                || after.pack_index_hash != manifest.pack_index_hash
            {
                return Err(CrabError::CasConflict {
                    path: router.manifest_path().as_ref().to_owned(),
                    expected_etag: None,
                });
            }
            writer
                .set_coverage(crab_metadata::git_object_locator::GitLocatorCoverage {
                    generation: manifest.generation,
                    pack_index_hash,
                })
                .await?;
            Ok::<_, CrabError>(())
        }
        .await;
        let close_result = writer.close().await.map_err(CrabError::from);
        match (operation, close_result) {
            (Ok(()), Ok(stats)) if stats.coverage_updated => Ok(stats),
            (Ok(()), Ok(_)) => Err(CrabError::Internal(
                "rebuilt Git locator did not advance coverage".to_owned(),
            )),
            (Err(error), Ok(_)) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(close_error)) => {
                warn!(error = %close_error, "Git locator close also failed after rebuild error");
                Err(error)
            }
        }
    })
    .await;
    let release_result = lock.release().await.map_err(CrabError::from);
    let _stats = write_result?;
    release_result?;
    crate::git::push::publish_git_visibility_index_from_storage_git_dir(
        visibility_temp.path(),
        manifest,
        store,
        router,
    )
    .await?;
    if !manifest.refs.is_empty() {
        let graph_hash = crate::git::push::rebuild_split_commit_graph_from_storage_git_dir(
            visibility_temp.path(),
            manifest,
            store,
            router,
        )
        .await?;
        let mut graph_published = false;
        for _ in 0..3 {
            let (mut current, etag) =
                crab_metadata::manifest_store::read_manifest(store, router).await?;
            if current.generation != manifest.generation
                || current.pack_index_hash != manifest.pack_index_hash
                || current.git_validation_digest != manifest.git_validation_digest
            {
                return Err(CrabError::CasConflict {
                    path: router.manifest_path().to_string(),
                    expected_etag: Some(etag),
                });
            }
            if current.commit_graph_hash.as_deref() == Some(graph_hash.as_str()) {
                graph_published = true;
                break;
            }
            current.commit_graph_hash = Some(graph_hash.clone());
            match crab_metadata::manifest_store::write_manifest_cas(store, router, &current, &etag)
                .await
            {
                Ok(_) => {
                    graph_published = true;
                    break;
                }
                Err(crab_metadata::error::MetadataError::ManifestCasConflict { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        if !graph_published {
            return Err(CrabError::CasConflict {
                path: router.manifest_path().to_string(),
                expected_etag: None,
            });
        }
    }
    let processed = u64::try_from(derived.len()).map_err(|_| {
        CrabError::Internal("rebuilt Git pack count cannot be represented".to_owned())
    })?;
    let objects_written = derived
        .iter()
        .map(|(pack, _, _, _, _, _)| pack.object_count)
        .sum();
    Ok((
        processed,
        failed,
        objects_written,
        crab_metadata::receipts::generation_git_object_locator_digest(pack_index_hash.into()),
    ))
}

fn sampled_location_indexes(location_count: usize) -> Vec<usize> {
    if location_count == 0 {
        return Vec::new();
    }
    let mut indexes = vec![0, location_count / 2, location_count - 1];
    indexes.sort_unstable();
    indexes.dedup();
    indexes
}

fn verify_sampled_pack_ranges(
    pack_path: &Path,
    locations: &[crab_git::pack_locator::PackObjectLocation],
) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};

    if locations.is_empty() {
        return Ok(());
    }
    let mut sample_indexes = vec![0, locations.len() / 2, locations.len() - 1];
    sample_indexes.sort_unstable();
    sample_indexes.dedup();
    let mut file = std::fs::File::open(pack_path)?;
    let mut buffer = vec![0u8; 64 * 1024];
    for index in sample_indexes {
        let location = &locations[index];
        file.seek(SeekFrom::Start(location.pack_offset))?;
        let mut remaining = location.entry_len;
        let mut hasher = crc32fast::Hasher::new();
        while remaining > 0 {
            let read_len = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| CrabError::Internal("sample range length overflow".to_owned()))?;
            let bytes_read = file.read(&mut buffer[..read_len])?;
            if bytes_read == 0 {
                return Err(CrabError::CorruptObject {
                    path: pack_path.display().to_string(),
                    reason: format!(
                        "sampled object {} ends before its indexed range",
                        location.oid
                    ),
                });
            }
            hasher.update(&buffer[..bytes_read]);
            remaining = remaining.saturating_sub(bytes_read as u64);
        }
        let actual = hasher.finalize();
        if actual != location.crc32 {
            return Err(CrabError::CorruptObject {
                path: pack_path.display().to_string(),
                reason: format!(
                    "sampled object {} CRC mismatch: expected {:08x}, got {actual:08x}",
                    location.oid, location.crc32
                ),
            });
        }
    }
    Ok(())
}

fn render_rebuild_payload(payload: &RebuildPayload, mode: OutputMode) {
    if matches!(mode, OutputMode::Json) {
        emit_json("metadb.rebuild", "1.0", &payload);
    } else {
        println!("\ncrab metadb rebuild\n");
        println!("  repo_prefix:                 {}", payload.repo_prefix);
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
        shards_processed = payload.shards_processed,
        shards_failed = payload.shards_failed,
        file_entries_written = payload.file_index_entries_written,
        chunk_entries_written = payload.chunk_index_entries_written,
        git_packs_processed = payload.git_packs_processed,
        git_packs_failed = payload.git_packs_failed,
        git_objects_written = payload.git_objects_written,
        elapsed_ms = payload.elapsed_ms,
        "metadb rebuild complete"
    );
}

/// Flush the pending per-database entry buffers through one
/// `MetaDb::commit`, returning `(file_written, chunk_written)`.
///
/// Empty buffers produce a zero-op commit which the session
/// short-circuits, so this is safe to call unconditionally after
/// every shard.
#[derive(Clone)]
struct RebuildVerifiedXorb {
    origin: crab_metadata::receipts::OriginReceipt,
    chunks: Vec<(MerkleHash, u32)>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "repair proof binds independent repository, manifest, registry, shard, and placement anchors"
)]
async fn rebuild_committed_chunk_receipts(
    storage: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    repo_prefix: &str,
    source_shard_hash: MerkleHash,
    committed_generation: u64,
    shard_index_hash: MerkleHash,
    gc_registry_generation: u64,
    cancel: &CancellationToken,
    entries: &[(MerkleHash, XorbRef)],
    verified_xorb: &mut Option<(MerkleHash, RebuildVerifiedXorb)>,
) -> Result<Vec<(MerkleHash, crab_metadata::receipts::CommittedChunkReceipt)>> {
    if committed_generation == 0 || gc_registry_generation == 0 {
        return Err(CrabError::Internal(
            "committed chunk rebuild requires manifest and GC registry generations".to_owned(),
        ));
    }
    let mut receipts = Vec::with_capacity(entries.len());
    for (chunk_hash, xorb_ref) in entries {
        check_cancelled(cancel)?;
        if verified_xorb
            .as_ref()
            .is_none_or(|(hash, _)| *hash != xorb_ref.xorb_hash)
        {
            let path = router.xorb_path(&xorb_ref.xorb_hash);
            let (body, etag) = tokio::select! {
                result = storage.get_with_etag_bounded(&path, MAX_XORB_SIZE as u64) => result?,
                () = cancel.cancelled() => return Err(CrabError::Cancelled),
            };
            let parser = XorbParser::parse(body.clone())?;
            if parser.hash() != xorb_ref.xorb_hash {
                return Err(CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: format!(
                        "xorb metadata hash mismatch: expected {}, got {}",
                        xorb_ref.xorb_hash.hex(),
                        parser.hash().hex()
                    ),
                });
            }
            parser.verify_payload_digest()?;
            parser.verify_all_chunks()?;
            let mut chunks = Vec::with_capacity(parser.num_chunks() as usize);
            for index in 0..parser.num_chunks() {
                let meta = parser.chunk_meta(index)?;
                chunks.push((meta.hash, meta.uncompressed_len));
            }
            *verified_xorb = Some((
                xorb_ref.xorb_hash,
                RebuildVerifiedXorb {
                    origin: crab_metadata::receipts::OriginReceipt::new(
                        "canonical-origin".to_owned(),
                        path.to_string(),
                        xorb_ref.xorb_hash.into(),
                        parser.payload_digest(),
                        body.len() as u64,
                        etag.e_tag,
                        etag.version,
                    ),
                    chunks,
                },
            ));
        }
        let verified = verified_xorb
            .as_ref()
            .filter(|(hash, _)| *hash == xorb_ref.xorb_hash)
            .map(|(_, verified)| verified)
            .ok_or_else(|| {
                CrabError::Internal("verified xorb cache insertion was lost".to_owned())
            })?;
        let index =
            usize::try_from(xorb_ref.chunk_index).map_err(|_| CrabError::CorruptObject {
                path: router.xorb_path(&xorb_ref.xorb_hash).to_string(),
                reason: "chunk index cannot be represented".to_owned(),
            })?;
        if verified.chunks.get(index) != Some(&(*chunk_hash, xorb_ref.uncompressed_size)) {
            return Err(CrabError::CorruptObject {
                path: router.xorb_path(&xorb_ref.xorb_hash).to_string(),
                reason: format!(
                    "shard placement for chunk {} does not match xorb index {}",
                    chunk_hash.hex(),
                    xorb_ref.chunk_index
                ),
            });
        }
        receipts.push((
            *chunk_hash,
            crab_metadata::receipts::CommittedChunkReceipt {
                schema_version: crab_metadata::receipts::RECEIPT_SCHEMA_VERSION,
                chunk_hash: (*chunk_hash).into(),
                xorb_hash: xorb_ref.xorb_hash.into(),
                chunk_index: xorb_ref.chunk_index,
                uncompressed_size: xorb_ref.uncompressed_size,
                origin: verified.origin.clone(),
                source_repo_prefix: repo_prefix.to_owned(),
                source_shard_hash: source_shard_hash.into(),
                committed_generation,
                shard_index_hash: shard_index_hash.into(),
                gc_registry_generation,
            },
        ));
    }
    Ok(receipts)
}

async fn flush_rebuild_batch(
    guard: &MetaDbGuard,
    file_store: Option<&crate::metadata::FileIndexStore>,
    chunk_store: Option<&crate::metadata::ChunkIndexStore>,
    pending_file: &mut Vec<(MerkleHash, crab_metadata::value_codec::CommittedFileRecord)>,
    pending_chunk: &mut Vec<(MerkleHash, XorbRef)>,
    pending_committed_chunk: &mut Vec<(MerkleHash, crab_metadata::receipts::CommittedChunkReceipt)>,
    cancel: &CancellationToken,
) -> Result<(u64, u64)> {
    check_cancelled(cancel)?;
    if pending_file.is_empty() && pending_chunk.is_empty() && pending_committed_chunk.is_empty() {
        return Ok((0, 0));
    }

    let mut txn = guard.new_transaction()?;
    let file_written = match file_store {
        Some(store) if !pending_file.is_empty() => {
            store.save_committed_batch(&mut txn, pending_file);
            pending_file.len() as u64
        }
        _ => 0,
    };
    let chunk_written = match chunk_store {
        Some(store) if !pending_chunk.is_empty() || !pending_committed_chunk.is_empty() => {
            store.save_committed_receipts(&mut txn, pending_committed_chunk)?;
            pending_chunk.len() as u64
        }
        _ => 0,
    };

    guard.commit(txn).await?;
    pending_file.clear();
    pending_chunk.clear();
    pending_committed_chunk.clear();
    Ok((file_written, chunk_written))
}

// --- compact --------------------------------------------------------

async fn run_compact(_db: DbSelector, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    warn!(
        "crab metadb compact: SlateDB compaction runs automatically in the background; \
         this subcommand currently has no effect. It is provided so operator runbooks can \
         call the command without \"unknown subcommand\" errors."
    );
    info!("metadb compact invoked (no-op)");
    Ok(())
}

// --- cache ----------------------------------------------------------

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
        emit_json("metadb.cache.stats", "1.0", &payload);
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
    if !path.exists() {
        return Ok(CacheStatsPayload {
            cache_path,
            exists: false,
            file_size_bytes: 0,
            entry_count: 0,
            installed_shard_count: 0,
            cache_gc_generation: 0,
        });
    }
    let file_size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    // Reuse the process-shared handle so diagnostics run through the
    // same SQLite connection queue as push/import code in this process.
    let index = crab_metadata::persistent_chunk_index::PersistentChunkIndex::open_shared(path)?;
    let entries = index.load_all()?.len() as u64;
    let installed_shards = index.installed_shards()?.len() as u64;
    let cache_gc_generation = index.cache_gc_generation()?;

    Ok(CacheStatsPayload {
        cache_path,
        exists: true,
        file_size_bytes,
        entry_count: entries,
        installed_shard_count: installed_shards,
        cache_gc_generation,
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

// --- `crab doctor --metadb` --------------------------------------

/// Entry point for the `crab doctor --metadb` subcommand.
///
/// Emits a text (or JSON) report covering open-state for both
/// databases, a rough shard count under `.crab/shards/`, and the
/// local cache stats. The report intentionally stays shallow:
/// anything deeper (WAL replay, bloom validation) belongs to
/// `crab metadb diagnose`.
pub async fn run_doctor_metadb_in(root: &Path, mode: OutputMode) -> Result<()> {
    let cancel = CancellationToken::new();
    let (store, repo_prefix, bucket_identity, config) =
        match resolve_repo_store_in(root, &cancel).await {
            Ok(v) => v,
            Err(e) => {
                // No remote configured — still emit something useful.
                let empty = DoctorMetadbPayload {
                    repo_prefix: String::from("<unconfigured>"),
                    file_index: DbDiagnosis {
                        label: "file_index_db",
                        path: String::new(),
                        opened: false,
                        error: Some(e.to_string()),
                        format_version: None,
                        epoch: None,
                        created_at: None,
                        gc_generation: None,
                        deep_integrity: None,
                    },
                    chunk_index: DbDiagnosis {
                        label: "chunk_index_db",
                        path: String::new(),
                        opened: false,
                        error: Some(String::from("skipped: no remote configured")),
                        format_version: None,
                        epoch: None,
                        created_at: None,
                        gc_generation: None,
                        deep_integrity: None,
                    },
                    shards_prefix: String::from("<unknown>"),
                    shard_count: None,
                    shard_enumeration_error: None,
                    cache: cache_stats_for(&crate::cache::chunk_index_cache_path(
                        &crate::cache::default_cache_root(),
                        &crate::storage::store::BucketIdentity::local_unset(),
                    ))
                    .unwrap_or_else(|_| CacheStatsPayload {
                        cache_path: String::new(),
                        exists: false,
                        file_size_bytes: 0,
                        entry_count: 0,
                        installed_shard_count: 0,
                        cache_gc_generation: 0,
                    }),
                    acceleration: AccelerationHealth::unavailable(
                        "remote is not configured; generation/index proof unavailable",
                    ),
                };
                render_doctor_metadb(&empty, mode);
                return Ok(());
            }
        };

    let metadb = build_metadb(
        Arc::clone(&store),
        repo_prefix.clone(),
        &bucket_identity,
        true,
        &config,
    );
    let guard = MetaDbGuard::new(metadb);

    let metadb_config = config.build_metadb_config(&repo_prefix);
    let file_index =
        diagnose_file_index(&guard, false, &store, &metadb_config.file_index_path).await;
    let chunk_index =
        diagnose_chunk_index(&guard, false, &store, &metadb_config.chunk_index_path).await;

    // Shard count (best effort). Shards live at the bucket-global
    // `.crab/shards/` prefix, never under the per-repo prefix —
    // content-addressed xorbs and shards are shared across every
    // repo in the bucket.
    let shards_prefix = String::from(".crab/shards/");
    let shards_path = ObjectPath::from(shards_prefix.as_str());
    let (shard_count, shard_enumeration_error) = match count_shards(&store, &shards_path).await {
        Ok(n) => (Some(n), None),
        Err(e) => (None, Some(e.to_string())),
    };

    let cache_path =
        crate::cache::chunk_index_cache_path(&crate::cache::default_cache_root(), &bucket_identity);
    let cache = cache_stats_for(&cache_path).unwrap_or_else(|_| CacheStatsPayload {
        cache_path: cache_path.display().to_string(),
        exists: false,
        file_size_bytes: 0,
        entry_count: 0,
        installed_shard_count: 0,
        cache_gc_generation: 0,
    });
    let acceleration = diagnose_acceleration_health(&store, &repo_prefix).await;

    let payload = DoctorMetadbPayload {
        repo_prefix,
        file_index,
        chunk_index,
        shards_prefix,
        shard_count,
        shard_enumeration_error,
        cache,
        acceleration,
    };

    render_doctor_metadb(&payload, mode);
    guard.close().await?;
    Ok(())
}

async fn diagnose_acceleration_health(
    store: &Arc<dyn ObjectStore>,
    repo_prefix: &str,
) -> AccelerationHealth {
    let storage = crab_storage::Store::new(Arc::clone(store));
    let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
    let (manifest, _) = match crab_metadata::manifest_store::read_manifest(&storage, &router).await
    {
        Ok(manifest) => manifest,
        Err(error) => {
            return AccelerationHealth::unavailable(format!(
                "manifest unavailable: {error}; retry remote access before repair"
            ));
        }
    };
    let shard_index_hash = if manifest.shard_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        match MerkleHash::from_hex(&manifest.shard_index_hash) {
            Ok(hash) => hash,
            Err(error) => {
                return AccelerationHealth::unavailable(format!(
                    "manifest shard-index hash is invalid: {error}"
                ));
            }
        }
    };
    let pack_index_hash = if manifest.pack_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        match MerkleHash::from_hex(&manifest.pack_index_hash) {
            Ok(hash) => hash,
            Err(error) => {
                return AccelerationHealth::unavailable(format!(
                    "manifest pack-index hash is invalid: {error}"
                ));
            }
        }
    };
    let mut notes = Vec::new();
    let (
        git_visibility_index_available,
        git_visibility_covered_generation,
        git_visibility_covered_pack_index_hash,
        git_visibility_coverage_current,
    ) = if manifest.refs.is_empty() {
        // Empty repositories have no pack-index identity to bind. The
        // protocol uses an empty in-memory proof for this immutable state.
        (true, Some(manifest.generation), None, true)
    } else if manifest.pack_index_hash.is_empty() {
        notes.push(
            "Git visibility proof has no pack-index identity; run `crab metadb rebuild`".to_owned(),
        );
        (false, None, None, false)
    } else {
        match crab_metadata::git_visibility::read_for_manifest(&storage, &router, &manifest).await {
            Ok(Some(read)) => {
                let current =
                    read.format == crab_metadata::git_visibility::GitVisibilityFormat::CatalogV1;
                if !current {
                    notes.push(
                        "Git visibility proof is not catalog-bound; run `crab metadb rebuild`"
                            .to_owned(),
                    );
                }
                let index = read.index;
                (
                    true,
                    Some(index.generation),
                    Some(index.pack_index_hash),
                    current,
                )
            }
            Ok(None) => {
                notes.push("Git visibility proof is missing; run `crab metadb rebuild`".to_owned());
                (false, None, None, false)
            }
            Err(error) => {
                notes.push(format!(
                    "Git visibility proof unavailable: {error}; run `crab metadb rebuild`"
                ));
                (false, None, None, false)
            }
        }
    };
    let receipt_path = router.repo_path(&format!(
        "metadata/generation-receipts/{:020}.json",
        manifest.generation
    ));
    let generation_receipt_valid = if manifest.refs.is_empty()
        && manifest.shard_index_hash.is_empty()
        && manifest.pack_index_hash.is_empty()
    {
        // An empty repository has no file or Git object indexes to bind, so a
        // generation receipt would be both unnecessary and unrepresentable.
        true
    } else {
        match storage.get_with_etag(&receipt_path).await {
            Ok((body, _)) => {
                serde_json::from_slice::<crab_metadata::receipts::GenerationIndexReceipt>(&body)
                    .map_err(|error| error.to_string())
                    .and_then(|receipt| {
                        receipt
                            .validate(
                                manifest.generation,
                                shard_index_hash.into(),
                                pack_index_hash.into(),
                            )
                            .map_err(|error| error.to_string())
                    })
                    .map(|()| true)
                    .unwrap_or_else(|error| {
                        notes.push(format!("generation-index receipt invalid: {error}"));
                        false
                    })
            }
            Err(crab_storage::StorageError::NotFound { .. }) => {
                notes
                    .push("generation-index receipt missing; run `crab metadb rebuild`".to_owned());
                false
            }
            Err(error) => {
                notes.push(format!("generation-index receipt unreadable: {error}"));
                false
            }
        }
    };
    let (
        git_commit_graph_available,
        git_commit_graph_commits,
        git_commit_graph_layers,
        git_commit_graph_current,
    ) = match manifest.commit_graph_hash.as_deref() {
        None if manifest.refs.is_empty() => (true, Some(0), Some(0), true),
        None => {
            notes
                .push("complete Git commit graph is missing; run `crab metadb rebuild`".to_owned());
            (false, None, None, false)
        }
        Some(hash) => match crab_metadata::split_commit_graph::load_split_commit_graph(
            &storage,
            &router,
            hash,
            crab_metadata::split_commit_graph::DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
        )
        .await
        {
            Ok(graph) => {
                let roots_complete = manifest
                    .refs
                    .iter()
                    .map(|(name, oid)| manifest.peeled_refs.get(name).unwrap_or(oid))
                    .all(|value| {
                        gix_hash::ObjectId::from_hex(value.as_bytes())
                            .ok()
                            .and_then(|oid| match oid {
                                gix_hash::ObjectId::Sha1(bytes) => Some(bytes),
                                _ => None,
                            })
                            .is_some_and(|oid| graph.contains(&oid))
                    });
                let current = graph.descriptor.generation == manifest.generation
                    && graph.descriptor.pack_index_hash == manifest.pack_index_hash
                    && graph.descriptor.git_validation_digest == manifest.git_validation_digest
                    && roots_complete;
                if !current {
                    notes.push(
                        "complete Git commit graph is stale; run `crab metadb rebuild`".to_owned(),
                    );
                }
                (
                    true,
                    Some(u64::from(graph.descriptor.commit_count)),
                    Some(graph.descriptor.layers.len() as u64),
                    current,
                )
            }
            Err(error) => {
                notes.push(format!(
                    "Git commit graph unavailable: {error}; run `crab metadb rebuild`"
                ));
                (false, None, None, false)
            }
        },
    };

    let ref_registry_repo_complete = match crab_metadata::ref_registry::repo_ref_registry_complete(
        &storage,
        &router,
        repo_prefix,
    )
    .await
    {
        Ok(complete) => complete,
        Err(error) => {
            notes.push(format!("repository ref registry unavailable: {error}"));
            false
        }
    };
    if !ref_registry_repo_complete {
        notes.push(
            "repository GC roots are incomplete; run `crab gc --repair-registry --bucket <bucket>`"
                .to_owned(),
        );
    }
    let ref_registry_bucket_complete =
        match crab_metadata::ref_registry::load_ref_registry_coverage_marker(&storage, &router)
            .await
        {
            Ok(complete) => complete,
            Err(error) => {
                notes.push(format!("bucket registry coverage unavailable: {error}"));
                false
            }
        };
    if !ref_registry_bucket_complete {
        notes.push(
            "bucket registry discovery is incomplete; destructive bucket GC remains disabled"
                .to_owned(),
        );
    }

    let git_session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
        Arc::clone(store),
        repo_prefix,
    )
    .await;
    let (
        git_locator_index_available,
        git_locator_covered_generation,
        git_locator_covered_pack_index_hash,
    ) = match git_session {
        Ok(session) => {
            let available = session.is_available();
            let coverage = session.coverage();
            if let Err(error) = session.close().await {
                notes.push(format!("Git locator index close failed: {error}"));
            }
            (
                available,
                coverage.map(|coverage| coverage.generation),
                coverage.map(|coverage| coverage.pack_index_hash.hex()),
            )
        }
        Err(error) => {
            notes.push(format!("Git locator index unavailable: {error}"));
            (false, None, None)
        }
    };
    let git_locator_writer_lease_active = match crab_coordination::internal_lock_path(
        repo_prefix,
        crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
    ) {
        Ok(path) => match storage.get_with_etag(&ObjectPath::from(path)).await {
            Ok((body, _)) => serde_json::from_slice::<crab_coordination::PushLockPayload>(&body)
                .is_ok_and(|payload| {
                    !payload.is_released() && !payload.is_expired_at(crab_coordination::unix_now())
                }),
            Err(crab_storage::StorageError::NotFound { .. }) => false,
            Err(error) => {
                notes.push(format!("Git locator writer lease unavailable: {error}"));
                false
            }
        },
        Err(error) => {
            notes.push(format!("Git locator writer lease path invalid: {error}"));
            false
        }
    };
    let expected_pack_index_hash = if manifest.pack_index_hash.is_empty() {
        MerkleHash::default().hex()
    } else {
        manifest.pack_index_hash.clone()
    };
    let git_locator_coverage_current = git_locator_covered_generation == Some(manifest.generation)
        && git_locator_covered_pack_index_hash.as_deref()
            == Some(expected_pack_index_hash.as_str());
    if !git_locator_index_available {
        notes.push("Git locator index missing; run `crab metadb rebuild`".to_owned());
    } else if !git_locator_coverage_current {
        notes.push("Git locator coverage is stale; run `crab metadb rebuild`".to_owned());
    }
    // Bucket-wide discovery is reported independently because it is not
    // required for repository-local acceleration or safe repo-scoped repair.
    let repair_required = !generation_receipt_valid
        || !ref_registry_repo_complete
        || !git_locator_index_available
        || !git_locator_coverage_current
        || !git_visibility_index_available
        || !git_visibility_coverage_current
        || !git_commit_graph_available
        || !git_commit_graph_current;
    AccelerationHealth {
        manifest_generation: Some(manifest.generation),
        generation_receipt_valid,
        ref_registry_repo_complete,
        ref_registry_bucket_complete,
        git_locator_index_available,
        git_locator_covered_generation,
        git_locator_covered_pack_index_hash,
        git_visibility_index_available,
        git_visibility_covered_generation,
        git_visibility_covered_pack_index_hash,
        git_visibility_coverage_current,
        git_commit_graph_available,
        git_commit_graph_commits,
        git_commit_graph_layers,
        git_commit_graph_current,
        git_locator_writer_lease_active,
        repair_required,
        notes,
    }
}

async fn count_shards(store: &Arc<dyn ObjectStore>, prefix: &ObjectPath) -> Result<u64> {
    let mut total: u64 = 0;
    let mut stream = store.list(Some(prefix));
    while let Some(_meta) = stream
        .try_next()
        .await
        .map_err(|e| CrabError::Internal(format!("listing shards: {e}")))?
    {
        total += 1;
    }
    Ok(total)
}

fn render_doctor_metadb(payload: &DoctorMetadbPayload, mode: OutputMode) {
    if matches!(mode, OutputMode::Json) {
        emit_json("doctor.metadb", "1.0", payload);
        return;
    }

    println!("crab doctor --metadb\n");
    println!("  repo_prefix: {}", payload.repo_prefix);
    println!();
    render_db_diagnosis(&payload.file_index);
    render_db_diagnosis(&payload.chunk_index);

    println!("[shards]  prefix={}", payload.shards_prefix);
    match payload.shard_count {
        Some(n) => println!("  shard_count: {n}"),
        None => {
            let err = payload
                .shard_enumeration_error
                .as_deref()
                .unwrap_or("<unknown>");
            println!("  shard_count: <failed to enumerate> — {err}");
        }
    }
    println!();

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
    println!();
    println!("[generation acceleration]");
    println!(
        "  manifest_generation: {}",
        payload.acceleration.manifest_generation.map_or_else(
            || "<unknown>".to_owned(),
            |generation| generation.to_string()
        )
    );
    println!(
        "  generation_receipt_valid: {}",
        payload.acceleration.generation_receipt_valid
    );
    println!(
        "  ref_registry_repo_complete: {}",
        payload.acceleration.ref_registry_repo_complete
    );
    println!(
        "  ref_registry_bucket_complete: {}",
        payload.acceleration.ref_registry_bucket_complete
    );
    println!(
        "  git_locator_index_available: {}",
        payload.acceleration.git_locator_index_available
    );
    println!(
        "  git_locator_covered_generation: {}",
        payload
            .acceleration
            .git_locator_covered_generation
            .map_or_else(|| "<none>".to_owned(), |generation| generation.to_string())
    );
    println!(
        "  git_locator_covered_pack_index_hash: {}",
        payload
            .acceleration
            .git_locator_covered_pack_index_hash
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "  git_visibility_index_available: {}",
        payload.acceleration.git_visibility_index_available
    );
    println!(
        "  git_visibility_covered_generation: {}",
        payload
            .acceleration
            .git_visibility_covered_generation
            .map_or_else(|| "<none>".to_owned(), |generation| generation.to_string())
    );
    println!(
        "  git_visibility_covered_pack_index_hash: {}",
        payload
            .acceleration
            .git_visibility_covered_pack_index_hash
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "  git_visibility_coverage_current: {}",
        payload.acceleration.git_visibility_coverage_current
    );
    println!(
        "  git_commit_graph_available: {}",
        payload.acceleration.git_commit_graph_available
    );
    println!(
        "  git_commit_graph_commits: {}",
        payload
            .acceleration
            .git_commit_graph_commits
            .map_or_else(|| "<none>".to_owned(), |commits| commits.to_string())
    );
    println!(
        "  git_commit_graph_layers: {}",
        payload
            .acceleration
            .git_commit_graph_layers
            .map_or_else(|| "<none>".to_owned(), |layers| layers.to_string())
    );
    println!(
        "  git_commit_graph_current: {}",
        payload.acceleration.git_commit_graph_current
    );
    println!(
        "  git_locator_writer_lease_active: {}",
        payload.acceleration.git_locator_writer_lease_active
    );
    println!(
        "  repair_required: {}",
        payload.acceleration.repair_required
    );
    for note in &payload.acceleration.notes {
        println!("  note: {note}");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt};
    use tempfile::TempDir;

    use super::*;
    use crate::metadata::metadb::{Db, MetaDb, MetaDbConfig, stores};
    use crab_metadata::key_codec::{self, SYS_CREATED_AT, SYS_EPOCH, SYS_FORMAT_VERSION};

    /// Build a `MetaDb` anchored at a temp cache path and an in-memory
    /// object store. Returns the store handle too so tests can seed
    /// raw sys:* values through a short-lived `Db` handle before the
    /// diagnose helper opens its own.
    fn test_metadb(store: Arc<dyn ObjectStore>) -> (MetaDb, TempDir) {
        let cache_dir = TempDir::new().expect("tempdir");
        let cache_path = cache_dir.path().join("chunk-index.sqlite");
        let cfg = MetaDbConfig {
            local_chunk_index_path: cache_path,
            ..MetaDbConfig::for_repo("org/test-repo")
        };
        (
            MetaDb::new(store, String::from("org/test-repo"), cfg),
            cache_dir,
        )
    }

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
    fn generation_owner_reason_is_stable_for_each_action() {
        let reasons = [
            ("ref_journal_compaction", "active_ref_journal"),
            ("catalog_advance", "catalog_coverage_stale"),
            ("visibility_repair", "visibility_missing_or_stale"),
            (
                "commit_graph_incremental",
                "commit_graph_missing_incremental",
            ),
            ("commit_graph_rebuild", "commit_graph_missing"),
            ("commit_graph_compaction", "commit_graph_layers_due"),
            ("shallow_closure_rebuild", "shallow_closure_missing"),
            ("geometric_repack", "geometric_pack_threshold"),
            ("geometric_repack_bounded", "geometric_pack_budget"),
            ("geometric_repack_deferred", "maintenance_budget"),
            ("superseded", "manifest_superseded"),
            ("none", "no_maintenance_due"),
        ];

        for (action, reason) in reasons {
            assert_eq!(generation_owner_reason(action), reason);
        }
    }

    #[test]
    fn locator_planning_counts_only_uncovered_pack_objects() {
        let covered_pack_id = "a".repeat(64);
        let new_pack_id = "b".repeat(64);
        let covered_pack = crab_metadata::manifests::PackManifestEntry {
            pack_id: covered_pack_id.clone(),
            size: 128,
            content_hash: covered_pack_id.clone(),
            ref_tips: Vec::new(),
            object_count: 1_000_000,
        };
        let new_pack = crab_metadata::manifests::PackManifestEntry {
            pack_id: new_pack_id.clone(),
            size: 256,
            content_hash: new_pack_id,
            ref_tips: Vec::new(),
            object_count: 7,
        };
        let covered_id =
            crab_xet::hash::MerkleHash::from_hex(&covered_pack_id).expect("covered pack id");
        let bindings = [crab_metadata::git_object_locator::GitPackLocatorBinding {
            pack_slot: 1,
            record: crab_metadata::git_object_locator::GitPackLocatorRecord {
                pack_id: covered_id,
                committed_generation: 4,
                pack_index_hash: crab_xet::hash::MerkleHash::from_hex(&"c".repeat(64))
                    .expect("pack index hash"),
                object_count: covered_pack.object_count,
                pack_size: covered_pack.size,
            },
        }];
        let coverage = Some(crab_metadata::git_object_locator::GitLocatorCoverage {
            generation: 5,
            pack_index_hash: crab_xet::hash::MerkleHash::from_hex(&"d".repeat(64))
                .expect("coverage hash"),
        });

        assert_eq!(
            crate::git::push::uncovered_locator_object_rows(
                coverage,
                &bindings,
                &[covered_pack, new_pack],
            ),
            7
        );
    }

    #[tokio::test]
    async fn generation_owner_accepts_an_empty_current_generation() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = crate::storage::store::Store::new(inner);
        let router = crate::storage::StoreLayout::new(store.clone(), "org/repo".to_owned());
        crate::metadata::manifest::create_manifest_with_etag(
            &store,
            &router,
            &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
        )
        .await
        .expect("create manifest");
        let sample = generation_owner_sample(
            &store,
            &router,
            std::time::Duration::from_secs(60),
            60,
            &Config::default(),
            &CancellationToken::new(),
        )
        .await
        .expect("sample empty generation");

        assert_eq!(sample.generation, 0);
        assert_eq!(sample.action, "none");
        assert_eq!(sample.maintenance_reason, "no_maintenance_due");
        assert_eq!(sample.next_eligibility_secs, 60);
        assert!(!sample.locator_advanced);
        assert_eq!(sample.visibility, "published");
        assert!(!sample.superseded);
    }

    #[tokio::test]
    async fn generation_owner_compacts_active_ref_journal_before_derived_work() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = crate::storage::store::Store::new(inner);
        let router = crate::storage::StoreLayout::new(store.clone(), "org/repo".to_owned());
        crate::core::remote_layout::initialize(&store, &router)
            .await
            .expect("initialize layout");
        crate::metadata::manifest::create_manifest_with_etag(
            &store,
            &router,
            &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
        )
        .await
        .expect("create manifest");
        let head =
            crate::metadata::manifest::read_ref_journal_head(&store, &router, "refs/heads/main")
                .await
                .expect("read ref head");
        let transaction = crab_metadata::ref_journal::RefJournalTransaction::new(
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                head.visible_transaction.clone(),
            )]),
            vec![crate::metadata::manifest::RefJournalEdit {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: None,
                new_oid: Some("a".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: None,
            }],
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("build ref transaction");
        crate::metadata::manifest::commit_ref_journal_transaction(
            &store,
            &router,
            &transaction,
            &[head],
        )
        .await
        .expect("publish active ref transaction");

        let sample = generation_owner_sample(
            &store,
            &router,
            std::time::Duration::from_secs(60),
            60,
            &Config::default(),
            &CancellationToken::new(),
        )
        .await
        .expect("owner should compact active journal");

        assert_eq!(sample.action, "ref_journal_compaction");
        assert_eq!(sample.maintenance_reason, "active_ref_journal");
        assert_eq!(sample.next_eligibility_secs, 0);
        assert!(sample.superseded);
        let snapshot = crate::metadata::manifest::read_repository_snapshot(&store, &router)
            .await
            .expect("read compacted repository");
        assert!(snapshot.journal.transactions.is_empty());
        assert_eq!(
            snapshot.manifest.refs.get("refs/heads/main"),
            Some(&"a".repeat(40))
        );
    }

    #[tokio::test]
    async fn owner_skips_locator_planning_for_superseded_manifest_snapshot() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = crate::storage::store::Store::new(inner);
        let router = crate::storage::StoreLayout::new(store.clone(), "org/repo".to_owned());

        let mut initial = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
        initial.generation = 1;
        initial.shard_index_hash = "a".repeat(64);
        initial.pack_index_hash = "b".repeat(64);
        initial.seal_git_validation();
        crate::metadata::manifest::create_manifest_with_etag(&store, &router, &initial)
            .await
            .expect("create initial manifest");
        let anchor = crate::git::push::committed_manifest_anchor(&initial)
            .expect("parse initial manifest anchor");

        let (mut newer, etag) = crate::metadata::manifest::read_manifest(&store, &router)
            .await
            .expect("read initial manifest");
        newer.generation += 1;
        newer.shard_index_hash = "c".repeat(64);
        newer.pack_index_hash = "d".repeat(64);
        newer.seal_git_validation();
        crate::metadata::manifest::write_manifest_cas(&store, &router, &newer, &etag)
            .await
            .expect("advance manifest");

        let missing_pack_id = "e".repeat(64);
        let result = maintain_object_catalog(
            &store,
            &router,
            anchor,
            &[crab_metadata::manifests::PackManifestEntry {
                pack_id: missing_pack_id.clone(),
                size: 1,
                content_hash: missing_pack_id,
                ref_tips: Vec::new(),
                object_count: 1,
            }],
            std::time::Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await;

        let (advanced, stats, sweep) = result.expect("stale owner snapshot must be skipped");
        assert!(!advanced);
        assert_eq!(
            stats,
            crab_metadata::git_object_locator::GitObjectCatalogStats::default()
        );
        assert_eq!(
            sweep,
            crab_metadata::git_object_locator::LocatorSweepStats::default()
        );
    }

    #[test]
    fn pack_locator_rebuild_uses_manifest_blake3_identity() {
        let bytes = b"pack bytes use raw blake3 identity, not the Xet file hash domain";
        let raw_pack_id = blake3::hash(bytes).to_hex().to_string();
        let expected = blake3::Hash::from_hex(&raw_pack_id).expect("parse raw pack hash");

        assert_eq!(blake3::hash(bytes), expected);
        assert_ne!(
            <[u8; 32]>::from(MerkleHash::from_hex(&raw_pack_id).expect("parse index pack id")),
            *expected.as_bytes(),
            "Xet MerkleHash wire order must not be used to validate raw Blake3 pack IDs"
        );
    }

    #[test]
    fn sampled_locator_crc_detects_corrupt_pack_bytes() {
        let temp = TempDir::new().expect("tempdir");
        let pack_path = temp.path().join("sample.pack");
        let mut bytes = (0u8..64).collect::<Vec<_>>();
        std::fs::write(&pack_path, &bytes).expect("write sample pack");
        let location = crab_git::pack_locator::PackObjectLocation {
            oid: gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
                .expect("test oid"),
            pack_offset: 12,
            entry_len: 32,
            crc32: crc32fast::hash(&bytes[12..44]),
        };

        verify_sampled_pack_ranges(&pack_path, std::slice::from_ref(&location))
            .expect("valid sampled range");
        bytes[20] ^= 0xff;
        std::fs::write(&pack_path, &bytes).expect("corrupt sample pack");
        assert!(matches!(
            verify_sampled_pack_ranges(&pack_path, &[location]),
            Err(CrabError::CorruptObject { .. })
        ));
    }

    #[tokio::test]
    async fn diagnose_chunk_index_surfaces_seeded_sys_values() {
        // Seed sys:format_version = 1, sys:epoch = 42, and
        // sys:created_at = 1_700_000_000_000 (approx 2023-11-14) into
        // an in-memory chunk_index_db. A diagnose pass must decode
        // each value and surface it on the returned DbDiagnosis.
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));

        // Seed BEFORE the MetaDb-owned handle opens the same path —
        // SlateDB fences older handles on a reopen, so establishing
        // the remote state up front keeps the diagnose path
        // authoritative.
        {
            let seed_db = Db::open(
                Arc::clone(&store),
                ObjectPath::from(metadb.config().chunk_index_path.as_str()),
                stores::chunk_index::DB_LABEL,
            )
            .await
            .expect("seed open");
            let mut batch = slatedb::WriteBatch::new();
            batch.put(
                key_codec::encode_system_key(SYS_FORMAT_VERSION).as_slice(),
                1u32.to_le_bytes().as_slice(),
            );
            batch.put(
                key_codec::encode_system_key(SYS_EPOCH).as_slice(),
                42u64.to_le_bytes().as_slice(),
            );
            batch.put(
                key_codec::encode_system_key(SYS_CREATED_AT).as_slice(),
                1_700_000_000_000u64.to_le_bytes().as_slice(),
            );
            seed_db.write(batch).await.expect("seed write");
            seed_db.close().await.expect("seed close");
        }

        let guard = MetaDbGuard::new(metadb);
        let d = diagnose_chunk_index(&guard, false, &store, ".crab/chunk_index_db/").await;

        assert!(d.opened, "diagnose should have opened chunk_index_db");
        assert_eq!(d.error, None);
        assert_eq!(d.label, "chunk_index_db");
        assert_eq!(d.format_version, Some(1));
        assert_eq!(d.epoch, Some(42));
        // 1_700_000_000_000 ms = 2023-11-14T22:13:20Z
        assert_eq!(d.created_at.as_deref(), Some("2023-11-14T22:13:20Z"));
        // gc_generation was not seeded, so the key is absent and the
        // accessor returns None (NOT a corrupt-value error).
        assert_eq!(d.gc_generation, None);

        guard.close().await.expect("guard close");
    }

    #[tokio::test]
    async fn diagnose_file_index_on_fresh_db_reports_canonical_format() {
        // Opening a fresh MetaDb publishes the strict v1 format marker.
        // Optional operational metadata remains absent.
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));
        let guard = MetaDbGuard::new(metadb);

        let d = diagnose_file_index(&guard, false, &store, "org/test-repo/file_index_db/").await;

        assert!(d.opened, "fresh file_index_db must open cleanly");
        assert_eq!(d.error, None);
        assert_eq!(d.label, "file_index_db");
        assert_eq!(d.format_version, Some(1));
        assert_eq!(d.epoch, None);
        assert_eq!(d.created_at, None);
        assert_eq!(d.gc_generation, None);

        guard.close().await.expect("guard close");
    }

    // --- rebuild -------------------------------------------------------

    /// A minimal shard with one real xorb and two reconstructable files.
    fn build_test_shard(seed: u64) -> (bytes::Bytes, MerkleHash, bytes::Bytes, MerkleHash) {
        use crab_xet::shard::{
            FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo,
            XorbChunkSequenceEntry, XorbChunkSequenceHeader,
        };
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;
        use std::sync::Arc;

        let chunks = [
            vec![seed as u8; 1024],
            vec![seed.wrapping_add(1) as u8; 1024],
        ];
        let mut builder = XorbBuilder::new();
        for data in &chunks {
            builder
                .push(
                    &Chunk {
                        hash: crab_xet::hash::compute_data_hash(data),
                        data: bytes::Bytes::copy_from_slice(data),
                    },
                    RunId(0),
                )
                .expect("pack test chunk");
        }
        let mut packed = builder.finalize().expect("finalize test xorb");
        assert_eq!(packed.len(), 1, "small test chunks should share one xorb");
        let packed = packed.pop().expect("one test xorb");

        let mut writer = crab_xet::shard::ShardWriter::new();
        let xorb_entries = packed
            .placements
            .iter()
            .enumerate()
            .map(|(index, placement)| {
                XorbChunkSequenceEntry::new(
                    placement.chunk_hash,
                    placement.uncompressed_size,
                    u32::try_from(index * 1024).expect("test offset fits u32"),
                )
            })
            .collect();
        writer
            .add_xorb(Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(packed.hash, 2, 2 * 1024),
                chunks: xorb_entries,
            }))
            .expect("add xorb");

        for (index, data) in chunks.iter().enumerate() {
            let file_hash = crab_xet::hash::compute_data_hash(data);
            writer
                .add_file(MDBFileInfo {
                    metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
                    segments: vec![FileDataSequenceEntry::new(
                        packed.hash,
                        1024u32,
                        u32::try_from(index).expect("test chunk index fits u32"),
                        u32::try_from(index + 1).expect("test chunk end fits u32"),
                    )],
                    verification: vec![],
                    metadata_ext: None,
                })
                .expect("add file");
        }

        let (bytes, hash) = writer.finalize().expect("finalize");
        (bytes::Bytes::from(bytes), hash, packed.bytes, packed.hash)
    }

    async fn seed_committed_shard_index(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        shard_hashes: &[MerkleHash],
    ) {
        let storage = crab_storage::Store::new(store);
        let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
        let hashes: Vec<String> = shard_hashes.iter().map(MerkleHash::hex).collect();
        let (index_hash, _, write) = crab_metadata::manifests::append_shard_index(
            crab_metadata::segmented::SegmentIndex::default(),
            1,
            &hashes,
        )
        .unwrap();
        crab_metadata::manifest_store::upload_segmented_bulk(
            &storage,
            &router,
            &crab_metadata::manifests::BulkData {
                shard_index: write,
                pack_index: crab_metadata::segmented::SegmentWrite::default(),
            },
        )
        .await
        .unwrap();
        let mut manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.shard_index_hash = index_hash;
        manifest.seal_git_validation();
        crab_metadata::manifest_store::create_manifest(&storage, &router, &manifest)
            .await
            .unwrap();
    }

    /// Rebuild end-to-end: seed two shards under `.crab/shards/`,
    /// run the rebuild driver with `--db both`, and verify that both
    /// SlateDB instances answer the synthesised file and chunk
    /// lookups correctly.
    #[tokio::test]
    async fn rebuild_repopulates_both_databases_from_shards() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        // Seed two distinct shards at their content-addressed keys.
        let (shard_a_bytes, shard_a_hash, xorb_a_bytes, xorb_a_hash) = build_test_shard(7);
        let (shard_b_bytes, shard_b_hash, xorb_b_bytes, xorb_b_hash) = build_test_shard(77);
        let shard_a_path =
            crab_storage::canonical_global_content_path("shards", &shard_a_hash.hex());
        let shard_b_path =
            crab_storage::canonical_global_content_path("shards", &shard_b_hash.hex());
        let xorb_a_path = crab_storage::canonical_global_content_path("xorbs", &xorb_a_hash.hex());
        let xorb_b_path = crab_storage::canonical_global_content_path("xorbs", &xorb_b_hash.hex());
        store
            .put(&shard_a_path, shard_a_bytes.clone().into())
            .await
            .expect("put shard a");
        store
            .put(&shard_b_path, shard_b_bytes.clone().into())
            .await
            .expect("put shard b");
        store
            .put(&xorb_a_path, xorb_a_bytes.into())
            .await
            .expect("put xorb a");
        store
            .put(&xorb_b_path, xorb_b_bytes.into())
            .await
            .expect("put xorb b");

        seed_committed_shard_index(
            Arc::clone(&store),
            "org/test-repo",
            &[shard_a_hash, shard_b_hash],
        )
        .await;

        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));
        let guard = MetaDbGuard::new(metadb);

        let cancel = CancellationToken::new();
        rebuild_with_guard(
            &store,
            "org/test-repo",
            DbSelector::Both,
            true,
            &guard,
            &cancel,
        )
        .await
        .expect("rebuild");

        // Expected state: each shard contributes 2 chunks (one xorb
        // with 2 chunks) and 2 file entries.
        let file_index = guard.file_index().await.expect("file index");
        let chunk_index = guard.chunk_index().await.expect("chunk index");

        // Pull every (file_hash, shard_hash) pair back via the
        // streaming extractor and verify it round-trips through
        // file_index_db.
        let expected_files = {
            let mut v =
                crab_xet::shard_parse::extract_file_entries_streaming(&shard_a_bytes, shard_a_hash);
            v.extend(crab_xet::shard_parse::extract_file_entries_streaming(
                &shard_b_bytes,
                shard_b_hash,
            ));
            v
        };
        let expected_recipes = [shard_a_bytes.clone(), shard_b_bytes.clone()]
            .into_iter()
            .flat_map(|bytes| {
                crab_xet::shard_parse::extract_file_recipes(&bytes).expect("extract recipes")
            })
            .map(|recipe| {
                let file_size = recipe.chunks.iter().map(|(_, size)| size).sum();
                let recipe_hash = crab_staging::recipe::FileRecipe::from_staged_chunks(
                    crab_staging::recipe::ChunkingPolicyId::XetGearV1_64KiB,
                    recipe.file_hash,
                    file_size,
                    &recipe.chunks,
                )
                .expect("build expected recipe")
                .hash();
                (recipe.file_hash, recipe_hash)
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(
            expected_files.len(),
            4,
            "2 shards with 2 files each must yield 4 entries"
        );
        for (file_hash, shard_hash) in &expected_files {
            let got = file_index
                .get_committed_batch(&[*file_hash])
                .await
                .expect("file_index get")
                .into_iter()
                .next()
                .flatten()
                .expect("file entry present after rebuild");
            assert_eq!(got.shard_hash, *shard_hash, "file→shard pair round-trips");
            assert_eq!(
                Some(&got.recipe_hash),
                expected_recipes.get(file_hash),
                "disk-spooled replay preserves the canonical ordered recipe identity"
            );
        }

        let expected_chunks = {
            let mut v = crab_xet::shard_parse::extract_chunk_entries_streaming(&shard_a_bytes);
            v.extend(crab_xet::shard_parse::extract_chunk_entries_streaming(
                &shard_b_bytes,
            ));
            v
        };
        assert_eq!(
            expected_chunks.len(),
            4,
            "2 shards with 2 chunks each must yield 4 chunk entries"
        );
        for (chunk_hash, expected_ref) in &expected_chunks {
            let got = chunk_index
                .get(chunk_hash)
                .await
                .expect("chunk_index get")
                .expect("chunk entry present after rebuild");
            assert_eq!(got, *expected_ref, "chunk→xorb ref round-trips");
        }

        guard.close().await.expect("close");
    }

    #[tokio::test]
    async fn rebuild_fails_when_a_committed_shard_cannot_be_replayed() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let shard_bytes = bytes::Bytes::from_static(b"not a metadata shard");
        let shard_hash = crab_xet::hash::compute_data_hash(&shard_bytes);
        let shard_path = crab_storage::canonical_global_content_path("shards", &shard_hash.hex());
        store
            .put(&shard_path, shard_bytes.into())
            .await
            .expect("put corrupt committed shard");
        seed_committed_shard_index(Arc::clone(&store), "org/test-repo", &[shard_hash]).await;

        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));
        let guard = MetaDbGuard::new(metadb);
        let error = rebuild_with_guard(
            &store,
            "org/test-repo",
            DbSelector::FileIndex,
            false,
            &guard,
            &CancellationToken::new(),
        )
        .await
        .expect_err("a committed shard parse failure must fail the rebuild");

        assert!(matches!(error, CrabError::CorruptObject { .. }));
        guard.close().await.expect("close");
    }

    /// Re-running `rebuild` over the same bucket must converge:
    /// content-addressed writes make the second pass a no-op in
    /// terms of final state.
    #[tokio::test]
    async fn rebuild_is_idempotent_across_reruns() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (shard_bytes, shard_hash, xorb_bytes, xorb_hash) = build_test_shard(5);
        let shard_path = crab_storage::canonical_global_content_path("shards", &shard_hash.hex());
        let xorb_path = crab_storage::canonical_global_content_path("xorbs", &xorb_hash.hex());
        store
            .put(&shard_path, shard_bytes.clone().into())
            .await
            .expect("put");
        store
            .put(&xorb_path, xorb_bytes.into())
            .await
            .expect("put xorb");
        seed_committed_shard_index(Arc::clone(&store), "org/test-repo", &[shard_hash]).await;

        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));
        let guard = MetaDbGuard::new(metadb);

        let cancel = CancellationToken::new();
        for _ in 0..2 {
            rebuild_with_guard(
                &store,
                "org/test-repo",
                DbSelector::Both,
                true,
                &guard,
                &cancel,
            )
            .await
            .expect("rebuild");
        }

        // After two passes, every file and chunk key must still
        // resolve to the same value.
        let file_index = guard.file_index().await.expect("file index");
        let chunk_index = guard.chunk_index().await.expect("chunk index");

        for (f, s) in
            crab_xet::shard_parse::extract_file_entries_streaming(&shard_bytes, shard_hash)
        {
            assert_eq!(
                file_index
                    .get_committed_batch(&[f])
                    .await
                    .expect("get")
                    .into_iter()
                    .next()
                    .flatten()
                    .expect("present")
                    .shard_hash,
                s,
                "file entry still correct after second rebuild pass"
            );
        }
        for (c, r) in crab_xet::shard_parse::extract_chunk_entries_streaming(&shard_bytes) {
            assert_eq!(
                chunk_index.get(&c).await.expect("get").expect("present"),
                r,
                "chunk entry still correct after second rebuild pass"
            );
        }

        guard.close().await.expect("close");
    }
}
