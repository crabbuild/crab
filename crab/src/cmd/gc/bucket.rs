//! Bucket-scope garbage collection for globally deduplicated storage.
//!
//! Enumerates all content-addressed objects under `.crab/` and deletes
//! those not referenced by any repo in the ref-registry, subject to a
//! configurable grace period.
//!
//! Algorithm:
//! 1. Load ref-registry → compute `referenced_shards`.
//! 2. List `.crab/shards/` → unreferenced + expired = shard candidates.
//! 3. For each referenced shard, validate its durable closure and extract
//!    xorb/file relationships → `referenced_xorbs`.
//! 4. List `.crab/xorbs/` and `.crab/gc/closures/` → unreferenced + expired
//!    candidates, retaining closures while their source shard is retained.
//! 5. Dry-run reports; otherwise journal and delete candidates.
//!
//! The legacy `.crab/file-index/` enumeration is gone — per-file
//! objects don't exist anymore. Each repository's `file_index_db` is
//! swept against the file hashes reachable from its retained shards.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cmd::gc::marks::{DurableMarkReader, DurableMarkWriter};
use crate::core::config::GcListProfile;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::storage::store::Store;
use crab_metadata::ref_registry::RefRegistry;
use crab_storage::{
    StorageProviderKind, StoreLayout, canonical_global_content_path, content_hash_from_path,
    global_content_partition_prefix, global_content_prefix,
};
use crab_xet::hash::MerkleHash;

/// CLI arguments for `crab gc --scope=bucket`.
#[derive(Debug, Clone)]
pub struct BucketGcArgs {
    /// S3 bucket name (e.g. `my-bucket`).
    pub bucket: String,
    /// Report candidates without deleting.
    pub dry_run: bool,
    /// Minimum age before an unreferenced object is eligible for deletion.
    pub grace_period: Duration,
    /// Bypass object age checks, but never registry or coordinator safety proof.
    pub force: bool,
    /// Skip the interactive confirmation required by `force` mode.
    pub yes: bool,
    /// Maximum concurrent LIST, history, and metadata-read operations.
    pub list_concurrency: usize,
    /// Cost/latency policy for bucket-global object enumeration.
    pub list_profile: GcListProfile,
    /// Maximum concurrent object DELETE requests.
    pub delete_concurrency: usize,
    /// Resume a durable destructive run by UUIDv7 run id.
    pub resume_run_id: Option<String>,
}

/// Structured outcome of a bucket-scope GC run.
#[derive(Debug, Clone, Default)]
pub struct BucketGcOutcome {
    pub shards_deleted: u64,
    pub xorbs_deleted: u64,
    pub file_index_deleted: u64,
    pub bytes_reclaimed: u64,
    pub list_requests: u64,
    pub list_parallelism: usize,
    pub dry_run: bool,
}

impl BucketGcOutcome {
    pub fn log(&self) {
        if self.dry_run {
            info!(
                shards = self.shards_deleted,
                xorbs = self.xorbs_deleted,
                file_index = self.file_index_deleted,
                bytes = self.bytes_reclaimed,
                list_requests = self.list_requests,
                list_parallelism = self.list_parallelism,
                "bucket gc dry-run complete (no objects deleted)"
            );
        } else {
            info!(
                shards = self.shards_deleted,
                xorbs = self.xorbs_deleted,
                file_index = self.file_index_deleted,
                bytes = self.bytes_reclaimed,
                list_requests = self.list_requests,
                list_parallelism = self.list_parallelism,
                "bucket gc complete"
            );
        }
    }

    /// Convert bucket GC counters to the command's shared output schema.
    pub fn to_summary(&self) -> super::GcSummary {
        super::GcSummary {
            packs_deleted: 0,
            xorbs_deleted: self.xorbs_deleted,
            shards_deleted: self.shards_deleted,
            file_index_entries_deleted: self.file_index_deleted,
            bytes_reclaimed: self.bytes_reclaimed,
            dry_run: self.dry_run,
            cancelled: false,
            partial_enumeration: false,
            delete_failures: 0,
            reconciliation_failed: false,
        }
    }
}

/// Minimum allowed grace period (1 hour).
const MIN_GRACE_PERIOD: Duration = Duration::from_secs(3600);

/// Global prefix for content-addressed objects.
const GLOBAL_PREFIX: &str = ".crab";
const FILE_INDEX_GC_BATCH_SIZE: usize = 4_096;
const SHARD_REPAIR_BUDGET_BYTES: u64 = 128 * 1024 * 1024;
const SHARD_REPAIR_UNIT_BYTES: u64 = 1024 * 1024;
/// Closure sidecars are capped at 128 MiB; two permits keep concurrent
/// readers below a 256 MiB body budget even when the LIST concurrency is high.
const CLOSURE_READ_PARALLELISM: usize = 2;

/// Run bucket-scope garbage collection.
///
/// Loads the ref-registry, computes reachable sets, lists all global
/// objects, and deletes (or reports) unreferenced objects past the grace
/// period.
pub async fn run_bucket_gc(
    args: &BucketGcArgs,
    store: &Store,
    coordinator_protected_keys: &HashSet<String>,
    coordinator_protected_repos: &HashSet<String>,
    cancel: &CancellationToken,
) -> Result<BucketGcOutcome> {
    if args.dry_run && args.resume_run_id.is_some() {
        return Err(CrabError::Configuration {
            key: "gc.resume".to_owned(),
            origin: "a durable destructive GC run cannot be resumed as a dry-run".to_owned(),
        });
    }
    if args.dry_run {
        let registry = load_ref_registry_summary(store, args.force).await?;
        return run_bucket_gc_under_maintenance(
            args,
            store,
            coordinator_protected_keys,
            &registry,
            cancel,
            None,
        )
        .await;
    }

    if args.force && !confirm_force(args)? {
        return Ok(BucketGcOutcome::default());
    }

    let registry = load_ref_registry_summary(store, args.force).await?;
    ensure_registry_complete_for_destructive_gc(&registry)?;
    ensure_active_active_bucket_gc_proof(&registry, coordinator_protected_repos)?;
    run_bucket_gc_under_maintenance(
        args,
        store,
        coordinator_protected_keys,
        &registry,
        cancel,
        None,
    )
    .await
}

/// Run bucket GC while deriving active-active protection after the sweep fence
/// is held. The caller supplies the resolved configuration only so coordinator
/// snapshots cannot race the registry/root snapshot used for deletion.
pub async fn run_bucket_gc_with_config(
    args: &BucketGcArgs,
    store: &Store,
    config: &crate::core::config::Config,
    current_repo_prefix: Option<&str>,
    cancel: &CancellationToken,
) -> Result<BucketGcOutcome> {
    if args.dry_run && args.resume_run_id.is_some() {
        return Err(CrabError::Configuration {
            key: "gc.resume".to_owned(),
            origin: "a durable destructive GC run cannot be resumed as a dry-run".to_owned(),
        });
    }
    if args.dry_run {
        let registry = load_ref_registry_summary(store, args.force).await?;
        let protection = crate::replication::active_active_bucket_gc_protection(
            config,
            &registry,
            current_repo_prefix,
        )
        .await?;
        return run_bucket_gc_under_maintenance(
            args,
            store,
            &protection.protected_keys,
            &registry,
            cancel,
            None,
        )
        .await;
    }

    if args.force && !confirm_force(args)? {
        return Ok(BucketGcOutcome::default());
    }

    let registry = load_ref_registry_summary(store, args.force).await?;
    ensure_registry_complete_for_destructive_gc(&registry)?;
    let protection = crate::replication::active_active_bucket_gc_protection(
        config,
        &registry,
        current_repo_prefix,
    )
    .await?;
    ensure_active_active_bucket_gc_proof(&registry, &protection.protected_repos)?;
    run_bucket_gc_under_maintenance(
        args,
        store,
        &protection.protected_keys,
        &registry,
        cancel,
        None,
    )
    .await
}

fn confirm_force(args: &BucketGcArgs) -> Result<bool> {
    use std::io::Write;

    if !args.force {
        return Ok(true);
    }
    warn!("--force bypasses the grace period; concurrent pushes may lose data");
    if args.yes {
        return Ok(true);
    }
    eprint!("Proceed with force bucket GC? [y/N] ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Backfill and verify durable closures for every shard rooted by the
/// complete bucket registry. This is an explicit repair operation; normal
/// destructive GC never downloads a shard to fill a missing closure.
pub async fn repair_bucket_closures(
    store: &Store,
    coordinator_protected_repos: &HashSet<String>,
    list_concurrency: usize,
    cancel: &CancellationToken,
) -> Result<u64> {
    let sweep = crate::maintenance::GcSweepLease::acquire(store, GLOBAL_PREFIX, cancel).await?;
    let operation = repair_bucket_closures_under_maintenance(
        store,
        coordinator_protected_repos,
        list_concurrency,
        cancel,
    )
    .await;
    let release = sweep.release().await;
    match (operation, release) {
        (Ok(repaired), Ok(())) => Ok(repaired),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

/// Derive active-active protection only after the caller has acquired the
/// global sweep. This keeps the repair root snapshot and coordinator proof in
/// the same fenced interval as closure publication.
pub async fn repair_bucket_closures_with_config(
    store: &Store,
    config: &crate::core::config::Config,
    current_repo_prefix: Option<&str>,
    list_concurrency: usize,
    cancel: &CancellationToken,
) -> Result<u64> {
    let sweep = crate::maintenance::GcSweepLease::acquire(store, GLOBAL_PREFIX, cancel).await?;
    let operation = async {
        let registry = load_ref_registry_summary(store, false).await?;
        ensure_registry_complete_for_destructive_gc(&registry)?;
        let protection = crate::replication::active_active_bucket_gc_protection(
            config,
            &registry,
            current_repo_prefix,
        )
        .await?;
        ensure_active_active_bucket_gc_proof(&registry, &protection.protected_repos)?;
        repair_bucket_closures_under_maintenance(
            store,
            &protection.protected_repos,
            list_concurrency,
            cancel,
        )
        .await
    }
    .await;
    let release = sweep.release().await;
    match (operation, release) {
        (Ok(repaired), Ok(())) => Ok(repaired),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn repair_bucket_closures_under_maintenance(
    store: &Store,
    coordinator_protected_repos: &HashSet<String>,
    list_concurrency: usize,
    cancel: &CancellationToken,
) -> Result<u64> {
    let registry = load_ref_registry(store, false).await?;
    ensure_registry_complete_for_destructive_gc(&registry)?;
    ensure_active_active_bucket_gc_proof(&registry, coordinator_protected_repos)?;
    check_cancelled(cancel)?;

    let repo_shards = repository_referenced_shards(store, &registry, list_concurrency).await?;
    let shard_hashes = repo_shards
        .values()
        .flat_map(|shards| shards.iter().cloned())
        .collect::<HashSet<_>>();
    let body_budget = Arc::new(Semaphore::new(
        usize::try_from(SHARD_REPAIR_BUDGET_BYTES / SHARD_REPAIR_UNIT_BYTES)
            .map_err(|_| CrabError::Internal("shard closure budget overflow".to_owned()))?,
    ));
    futures_util::stream::iter(shard_hashes.into_iter().map(|hash_hex| {
        let body_budget = Arc::clone(&body_budget);
        async move {
            check_cancelled(cancel)?;
            repair_one_closure(store, &hash_hex, &body_budget, cancel).await
        }
    }))
    .buffer_unordered(list_concurrency.max(1))
    .try_fold(0u64, |total, repaired| async move {
        total
            .checked_add(u64::from(repaired))
            .ok_or_else(|| CrabError::Internal("closure repair count overflow".to_owned()))
    })
    .await
}

async fn repair_one_closure(
    store: &Store,
    hash_hex: &str,
    body_budget: &Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Result<bool> {
    let hash = MerkleHash::from_hex(hash_hex).map_err(|error| CrabError::CorruptObject {
        path: canonical_global_content_path("shards", hash_hex).to_string(),
        reason: format!("invalid referenced shard hash: {error}"),
    })?;
    let shard_path = canonical_global_content_path("shards", hash_hex);
    let closure_path = super::closure::path(GLOBAL_PREFIX, hash_hex);
    let shard_meta = store.head(&shard_path).await?;
    let closure_body = match store.get_with_etag(&closure_path).await {
        Ok((body, _)) => body,
        Err(CrabError::NotFound { .. }) => {
            rebuild_closure(store, &hash, &shard_path, &shard_meta, body_budget, cancel).await?;
            return Ok(true);
        }
        Err(error) => return Err(error),
    };
    let schema = serde_json::from_slice::<serde_json::Value>(&closure_body)
        .ok()
        .and_then(|value| {
            value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
        });
    if schema == Some(1) {
        store.delete(&closure_path).await?;
        rebuild_closure(store, &hash, &shard_path, &shard_meta, body_budget, cancel).await?;
        return Ok(true);
    }
    let closure = super::closure::read_manifest(store, GLOBAL_PREFIX, &hash).await?;
    if closure.content_size != shard_meta.size {
        return Err(CrabError::CorruptObject {
            path: closure_path.to_string(),
            reason: format!(
                "shard closure size {} does not match object metadata {}",
                closure.content_size, shard_meta.size
            ),
        });
    }
    Ok(false)
}

async fn rebuild_closure(
    store: &Store,
    hash: &MerkleHash,
    shard_path: &ObjectPath,
    shard_meta: &object_store::ObjectMeta,
    body_budget: &Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Result<()> {
    if shard_meta.size > SHARD_REPAIR_BUDGET_BYTES {
        return Err(CrabError::Configuration {
            key: "gc.repair_closures.memory_budget".to_owned(),
            origin: format!(
                "shard {} is {} bytes, above the {}-byte repair budget; split or compact the shard before backfill",
                hash.hex(),
                shard_meta.size,
                SHARD_REPAIR_BUDGET_BYTES
            ),
        });
    }
    let units = shard_meta
        .size
        .saturating_add(SHARD_REPAIR_UNIT_BYTES - 1)
        .saturating_div(SHARD_REPAIR_UNIT_BYTES)
        .max(1)
        .min(SHARD_REPAIR_BUDGET_BYTES / SHARD_REPAIR_UNIT_BYTES);
    let units = u32::try_from(units).map_err(|_| {
        CrabError::Internal("shard closure body budget exceeds semaphore capacity".to_owned())
    })?;
    let _budget = body_budget
        .clone()
        .acquire_many_owned(units)
        .await
        .map_err(|_| CrabError::Cancelled)?;
    check_cancelled(cancel)?;
    let body = store
        .inner()
        .get(shard_path)
        .await
        .map_err(CrabError::Storage)?
        .bytes()
        .await
        .map_err(CrabError::Storage)?;
    if body.len() as u64 != shard_meta.size {
        return Err(CrabError::CorruptObject {
            path: shard_path.to_string(),
            reason: format!(
                "shard body size {} does not match object metadata {}",
                body.len(),
                shard_meta.size
            ),
        });
    }
    super::closure::publish(store, GLOBAL_PREFIX, hash, body, shard_path.as_ref()).await
}

async fn run_bucket_gc_under_maintenance(
    args: &BucketGcArgs,
    store: &Store,
    coordinator_protected_keys: &HashSet<String>,
    registry: &RefRegistry,
    cancel: &CancellationToken,
    sweep_lease: Option<&crate::maintenance::GcSweepLease>,
) -> Result<BucketGcOutcome> {
    check_cancelled(cancel)?;
    let mut outcome = BucketGcOutcome {
        dry_run: args.dry_run,
        ..BucketGcOutcome::default()
    };

    if !args.dry_run {
        let roots = bucket_root_snapshot_streaming(
            store,
            registry,
            args.list_concurrency,
            coordinator_protected_keys,
        )
        .await?;
        let resume_phase = if let Some(run_id) = args.resume_run_id.as_deref() {
            let journal = super::journal::GcRunJournal::resume(
                store.clone(),
                GLOBAL_PREFIX,
                run_id,
                "bucket",
                GLOBAL_PREFIX,
            )
            .await?;
            journal.ensure_policy(args.grace_period, args.force)?;
            journal.ensure_root_identity(&roots.root_identity)?;
            Some(journal.state().phase)
        } else {
            None
        };
        return run_bucket_gc_streaming(
            args,
            store,
            coordinator_protected_keys,
            registry,
            roots.repo_prefixes,
            roots.root_identity,
            resume_phase,
            cancel,
            &mut outcome,
            sweep_lease,
        )
        .await;
    }

    let roots = bucket_root_snapshot_streaming(
        store,
        registry,
        args.list_concurrency,
        coordinator_protected_keys,
    )
    .await?;
    return run_bucket_gc_streaming(
        args,
        store,
        coordinator_protected_keys,
        registry,
        roots.repo_prefixes,
        roots.root_identity,
        None,
        cancel,
        &mut outcome,
        sweep_lease,
    )
    .await;
}

/// Bounded bucket planning path. It creates the journal before any global
/// listing and feeds candidates into bounded batches as each object arrives;
/// destructive plans can therefore be replayed after root validation.
#[expect(
    clippy::too_many_arguments,
    reason = "bucket planning keeps safety inputs explicit"
)]
async fn run_bucket_gc_streaming(
    args: &BucketGcArgs,
    store: &Store,
    coordinator_protected_keys: &HashSet<String>,
    registry: &RefRegistry,
    repo_prefixes: Vec<String>,
    root_identity: String,
    resume_phase: Option<super::journal::GcRunPhase>,
    cancel: &CancellationToken,
    outcome: &mut BucketGcOutcome,
    sweep_lease: Option<&crate::maintenance::GcSweepLease>,
) -> Result<BucketGcOutcome> {
    let snapshot_at = match resume_phase {
        Some(super::journal::GcRunPhase::Planning | super::journal::GcRunPhase::Deleting) => {
            let run_id = args.resume_run_id.as_deref().ok_or_else(|| {
                CrabError::Internal("bucket GC resume lost its run id".to_owned())
            })?;
            super::journal::GcRunJournal::resume(
                store.clone(),
                GLOBAL_PREFIX,
                run_id,
                "bucket",
                GLOBAL_PREFIX,
            )
            .await?
            .snapshot_at()?
        }
        None => SystemTime::now(),
        Some(super::journal::GcRunPhase::Complete) => {
            return Err(CrabError::Configuration {
                key: "gc.resume".to_owned(),
                origin: "a complete GC run cannot be resumed".to_owned(),
            });
        }
    };
    let mut journal = match args.resume_run_id.as_deref() {
        Some(run_id) => {
            let journal = super::journal::GcRunJournal::resume(
                store.clone(),
                GLOBAL_PREFIX,
                run_id,
                "bucket",
                GLOBAL_PREFIX,
            )
            .await?;
            journal.ensure_policy(args.grace_period, args.force)?;
            journal.ensure_root_identity(&root_identity)?;
            journal
        }
        None => {
            let mut journal = super::journal::GcRunJournal::start(
                store.clone(),
                GLOBAL_PREFIX,
                if args.dry_run {
                    "bucket-preview"
                } else {
                    "bucket"
                },
                GLOBAL_PREFIX,
                snapshot_at,
                args.grace_period,
                args.force,
            )
            .await?;
            journal.set_root_identity(&root_identity).await?;
            journal
        }
    };

    if resume_phase == Some(super::journal::GcRunPhase::Deleting) && !journal.file_index_complete()
    {
        let mut file_marks = DurableMarkWriter::new_hash_width(
            store.clone(),
            journal.marks_prefix(),
            "referenced-files",
            4,
        );
        write_referenced_file_marks_from_marked_shards(
            store,
            &mut DurableMarkReader::new_keys(
                store.clone(),
                journal.marks_prefix(),
                "referenced-shards",
            ),
            args.list_concurrency,
            cancel,
            &mut file_marks,
        )
        .await?;
        file_marks.finish().await?;
        let mut file_reader = DurableMarkReader::new_hash_width(
            store.clone(),
            journal.marks_prefix(),
            "referenced-files",
            4,
        );
        outcome.file_index_deleted = gc_file_indexes_partitioned(
            store,
            &repo_prefixes,
            &mut file_reader,
            false,
            Some(&mut journal),
            cancel,
        )
        .await?;
        journal.mark_file_index_complete().await?;
    }
    if resume_phase == Some(super::journal::GcRunPhase::Deleting) {
        execute_bucket_journal(args, store, &mut journal, cancel, outcome, sweep_lease).await?;
        outcome.log();
        return Ok(outcome.clone());
    }
    if resume_phase == Some(super::journal::GcRunPhase::Planning) {
        journal.reset_partial_plan().await?;
    }

    let mut root_reader =
        DurableMarkReader::new_keys(store.clone(), journal.marks_prefix(), "referenced-shards");
    if resume_phase != Some(super::journal::GcRunPhase::Deleting) {
        write_bucket_root_marks(store, registry, args.list_concurrency, &journal).await?;
        root_reader =
            DurableMarkReader::new_keys(store.clone(), journal.marks_prefix(), "referenced-shards");
    }
    let expected_referenced_shards = usize::try_from(root_reader.key_count().await?)
        .map_err(|_| CrabError::Internal("GC referenced shard count overflows usize".to_owned()))?;

    let effective_grace = args.grace_period.max(MIN_GRACE_PERIOD);
    let cutoff = snapshot_at - effective_grace;
    let permits = Arc::new(Semaphore::new(args.list_concurrency.max(1)));

    let mut shard_planner = ShardStreamingPlanner::new(
        store,
        root_reader,
        journal.marks_prefix(),
        coordinator_protected_keys,
        cutoff,
        args.force,
        &mut journal,
    );
    let shard_stats = scan_global_objects(
        store,
        "shards",
        args.list_profile,
        args.list_concurrency,
        Arc::clone(&permits),
        cancel,
        &mut shard_planner,
    )
    .await?;
    let shard_plan = shard_planner.finish().await?;
    debug!(
        objects = shard_stats.objects,
        partitioned = shard_stats.partitioned,
        "streamed bucket shard namespace"
    );
    outcome.list_requests = outcome.list_requests.saturating_add(shard_stats.requests);
    outcome.list_parallelism = outcome.list_parallelism.max(shard_stats.parallelism.max(1));

    if shard_plan.referenced_seen != expected_referenced_shards {
        return Err(CrabError::CorruptObject {
            path: ".crab/shards/".to_owned(),
            reason: format!(
                "ref-registry references {expected_referenced_shards} shards but only {} exist",
                shard_plan.referenced_seen
            ),
        });
    }

    let mut closure_planner = ClosureStreamingPlanner::new(
        store,
        DurableMarkReader::new_keys(store.clone(), journal.marks_prefix(), "referenced-shards"),
        DurableMarkReader::new(store.clone(), journal.marks_prefix(), "deletable-shards"),
        DurableMarkReader::new(store.clone(), journal.marks_prefix(), "existing-shards"),
        coordinator_protected_keys,
        cutoff,
        args.force,
        &mut journal,
    );
    let closure_stats =
        scan_closure_objects(store, Arc::clone(&permits), cancel, &mut closure_planner).await?;
    closure_planner.finish().await?;
    debug!(
        objects = closure_stats.objects,
        "streamed bucket closure namespace"
    );
    outcome.list_requests = outcome.list_requests.saturating_add(closure_stats.requests);
    outcome.list_parallelism = outcome
        .list_parallelism
        .max(closure_stats.parallelism.max(1));

    let mut closure_segment_planner = ClosureSegmentStreamingPlanner::new(
        DurableMarkReader::new_keys(
            store.clone(),
            journal.marks_prefix(),
            "live-closure-segments",
        ),
        coordinator_protected_keys,
        cutoff,
        args.force,
        &mut journal,
    );
    let segment_stats = scan_closure_segment_objects(
        store,
        Arc::clone(&permits),
        cancel,
        &mut closure_segment_planner,
    )
    .await?;
    closure_segment_planner.finish().await?;
    debug!(
        objects = segment_stats.objects,
        "streamed bucket closure segment namespace"
    );
    outcome.list_requests = outcome.list_requests.saturating_add(segment_stats.requests);
    outcome.list_parallelism = outcome
        .list_parallelism
        .max(segment_stats.parallelism.max(1));

    let mut xorb_planner = XorbStreamingPlanner::new(
        DurableMarkReader::new(store.clone(), journal.marks_prefix(), "referenced-xorbs"),
        coordinator_protected_keys,
        cutoff,
        args.force,
        &mut journal,
    );
    let xorb_stats = scan_global_objects(
        store,
        "xorbs",
        args.list_profile,
        args.list_concurrency,
        Arc::clone(&permits),
        cancel,
        &mut xorb_planner,
    )
    .await?;
    xorb_planner.finish().await?;
    debug!(
        objects = xorb_stats.objects,
        partitioned = xorb_stats.partitioned,
        "streamed bucket xorb namespace"
    );
    outcome.list_requests = outcome.list_requests.saturating_add(xorb_stats.requests);
    outcome.list_parallelism = outcome.list_parallelism.max(xorb_stats.parallelism.max(1));

    if args.dry_run {
        let preview = async {
            let current_roots = bucket_root_snapshot_streaming(
                store,
                registry,
                args.list_concurrency,
                coordinator_protected_keys,
            )
            .await?;
            journal.ensure_root_identity(&current_roots.root_identity)?;
            let mut file_reader = DurableMarkReader::new_hash_width(
                store.clone(),
                journal.marks_prefix(),
                "referenced-files",
                4,
            );
            outcome.file_index_deleted = gc_file_indexes_partitioned(
                store,
                &repo_prefixes,
                &mut file_reader,
                true,
                None,
                cancel,
            )
            .await?;
            for batch in 0..journal.state().planned_batches {
                for object in journal.planned_batch(batch).await? {
                    match object.key.split('/').nth(1) {
                        Some("shards") => outcome.shards_deleted += 1,
                        Some("xorbs") => outcome.xorbs_deleted += 1,
                        _ => {}
                    }
                    outcome.bytes_reclaimed = outcome
                        .bytes_reclaimed
                        .checked_add(object.size)
                        .ok_or_else(|| {
                            CrabError::Internal("bucket GC preview byte count overflow".to_owned())
                        })?;
                    info!(key = %object.key, size = object.size, "would delete (dry-run)");
                }
            }
            Ok::<(), CrabError>(())
        }
        .await;
        let cleanup = journal.discard_preview().await;
        match (preview, cleanup) {
            (Ok(()), Ok(())) => {
                outcome.log();
                return Ok(outcome.clone());
            }
            (Err(error), _) | (Ok(()), Err(error)) => return Err(error),
        }
    }

    journal.finish_plan().await?;
    if sweep_lease.is_none() {
        seal_bucket_journal(
            args,
            store,
            coordinator_protected_keys,
            &mut journal,
            cancel,
        )
        .await?;
    }
    if !journal.file_index_complete() {
        let mut file_reader = DurableMarkReader::new_hash_width(
            store.clone(),
            journal.marks_prefix(),
            "referenced-files",
            4,
        );
        outcome.file_index_deleted = gc_file_indexes_partitioned(
            store,
            &repo_prefixes,
            &mut file_reader,
            false,
            Some(&mut journal),
            cancel,
        )
        .await?;
        journal.mark_file_index_complete().await?;
    }
    execute_bucket_journal(args, store, &mut journal, cancel, outcome, sweep_lease).await?;
    outcome.log();
    Ok(outcome.clone())
}

async fn seal_bucket_journal(
    args: &BucketGcArgs,
    store: &Store,
    coordinator_protected_keys: &HashSet<String>,
    journal: &mut super::journal::GcRunJournal,
    cancel: &CancellationToken,
) -> Result<()> {
    let seal = crate::maintenance::GcSweepLease::acquire_for_run(
        store,
        GLOBAL_PREFIX,
        &journal.state().run_id,
        cancel,
    )
    .await?;
    let current = async {
        let registry = load_ref_registry_summary(store, false).await?;
        ensure_registry_complete_for_destructive_gc(&registry)?;
        let roots = bucket_root_snapshot_streaming(
            store,
            &registry,
            args.list_concurrency,
            coordinator_protected_keys,
        )
        .await?;
        journal.ensure_root_identity(&roots.root_identity)?;
        journal.seal_fence_epoch(seal.epoch()).await
    }
    .await;
    let release = seal.release().await;
    match (current, release) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

async fn execute_bucket_journal(
    args: &BucketGcArgs,
    store: &Store,
    journal: &mut super::journal::GcRunJournal,
    cancel: &CancellationToken,
    outcome: &mut BucketGcOutcome,
    sweep_lease: Option<&crate::maintenance::GcSweepLease>,
) -> Result<()> {
    let deleter = super::StoreObjectDeleter::new(store.clone());
    let prior_keys = journal.deleted_keys().await?;
    for key in prior_keys {
        match key.split('/').nth(1) {
            Some("shards") => outcome.shards_deleted += 1,
            Some("xorbs") => outcome.xorbs_deleted += 1,
            _ => {}
        }
    }
    outcome.bytes_reclaimed = outcome
        .bytes_reclaimed
        .checked_add(journal.deleted_bytes_reclaimed().await?)
        .ok_or_else(|| CrabError::Internal("bucket GC byte count overflow".to_owned()))?;
    loop {
        check_cancelled(cancel)?;
        let Some(objects) = journal.next_batch().await? else {
            break;
        };
        let lease = if sweep_lease.is_some() {
            None
        } else {
            let lease = crate::maintenance::GcSweepLease::acquire_for_run(
                store,
                GLOBAL_PREFIX,
                &journal.state().run_id,
                cancel,
            )
            .await?;
            if let Err(error) = journal.ensure_next_fence_epoch(lease.epoch()) {
                let _ = lease.release().await;
                return Err(error);
            }
            Some(lease)
        };
        let policy = super::DeletePolicy {
            snapshot_at: journal.snapshot_at()?,
            grace_period: Duration::from_secs(journal.state().grace_secs),
            force: journal.state().force,
        };
        let deleter = &deleter;
        let results = futures_util::stream::iter(objects.iter().cloned())
            .map(|object| async move {
                let result = super::ObjectDeleter::delete_candidate(deleter, &object, policy).await;
                (object, result)
            })
            .buffer_unordered(args.delete_concurrency.max(1))
            .collect::<Vec<_>>()
            .await;
        if cancel.is_cancelled() {
            if let Some(lease) = lease {
                let _ = lease.release().await;
            }
            return Err(CrabError::Cancelled);
        }
        let mut deleted_keys = Vec::with_capacity(results.len());
        let mut batch_error = None;
        let mut batch_bytes = 0u64;
        for (object, result) in results {
            match result {
                Ok(super::CandidateDelete::Deleted) | Err(CrabError::NotFound { .. }) => {
                    deleted_keys.push(object.key.clone());
                    match object.key.split('/').nth(1) {
                        Some("shards") => outcome.shards_deleted += 1,
                        Some("xorbs") => outcome.xorbs_deleted += 1,
                        _ => {}
                    }
                    batch_bytes = match batch_bytes.checked_add(object.size) {
                        Some(bytes) => bytes,
                        None => {
                            if let Some(lease) = lease {
                                let _ = lease.release().await;
                            }
                            return Err(CrabError::Internal(
                                "bucket GC byte count overflow".to_owned(),
                            ));
                        }
                    };
                }
                Ok(super::CandidateDelete::Retained) => {}
                Err(error) => {
                    if batch_error.is_none() {
                        batch_error = Some(error);
                    }
                }
            }
        }
        if let Some(error) = batch_error {
            if let Some(lease) = lease {
                let _ = lease.release().await;
            }
            return Err(CrabError::GcPartialFailure {
                objects_deleted: deleted_keys.len() as u64,
                delete_failures: 1,
                reconciliation_failed: false,
                source: Box::new(error),
            });
        }
        outcome.bytes_reclaimed = match outcome.bytes_reclaimed.checked_add(batch_bytes) {
            Some(bytes) => bytes,
            None => {
                if let Some(lease) = lease {
                    let _ = lease.release().await;
                }
                return Err(CrabError::Internal(
                    "bucket GC byte count overflow".to_owned(),
                ));
            }
        };
        if cancel.is_cancelled() {
            if let Some(lease) = lease {
                let _ = lease.release().await;
            }
            return Err(CrabError::Cancelled);
        }
        super::journal::crash_at("after-provider-delete");
        let fence_epoch = lease.as_ref().map(crate::maintenance::GcSweepLease::epoch);
        let journal_result = journal
            .complete_batch(&deleted_keys, batch_bytes, fence_epoch)
            .await;
        let release_result = match lease {
            Some(lease) => lease.release().await,
            None => Ok(()),
        };
        journal_result?;
        release_result?;
    }
    journal.complete().await
}

struct BucketRootSnapshot {
    repo_prefixes: Vec<String>,
    root_identity: String,
}

#[derive(Default)]
struct RootDigest {
    count: u64,
    xor: [u8; 32],
    sum: [u8; 32],
}

impl RootDigest {
    fn add(&mut self, category: &str, value: &str) {
        let mut hasher = blake3::Hasher::new();
        hasher.update(category.as_bytes());
        hasher.update(&[0]);
        hasher.update(value.as_bytes());
        let digest = hasher.finalize();
        for (index, byte) in digest.as_bytes().iter().copied().enumerate() {
            self.xor[index] ^= byte;
            self.sum[index] = self.sum[index].wrapping_add(byte);
        }
        self.count = self.count.saturating_add(1);
    }

    fn finish(self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.count.to_le_bytes());
        hasher.update(&self.xor);
        hasher.update(&self.sum);
        hasher.finalize().to_hex().to_string()
    }
}

async fn visit_repository_historical_shards<F, Fut>(
    store: &Store,
    repo_prefix: &str,
    concurrency: usize,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let storage = store.clone().into_storage();
    let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
    let mut history =
        crab_metadata::manifest_store::stream_manifest_history(&storage, &router, concurrency);
    while let Some(entry) = history.try_next().await.map_err(CrabError::from)? {
        if entry.manifest.shard_index_hash.is_empty() {
            continue;
        }
        crab_metadata::manifest_store::visit_bulk_shard_list(
            &storage,
            &router,
            &entry.manifest.shard_index_hash,
            |record| visit(record.shard_hash),
        )
        .await?;
    }
    Ok(())
}

async fn bucket_root_snapshot_streaming(
    store: &Store,
    registry: &RefRegistry,
    concurrency: usize,
    coordinator_protected_keys: &HashSet<String>,
) -> Result<BucketRootSnapshot> {
    let mut repo_prefixes = registry.repos.keys().cloned().collect::<Vec<_>>();
    repo_prefixes.sort_unstable();
    let mut digest = RootDigest::default();
    digest.add("registry-schema", &registry.schema_version.to_string());
    digest.add("registry-generation", &registry.generation.to_string());
    digest.add("registry-complete", &registry.coverage_complete.to_string());
    let storage = store.as_storage().clone();
    let router = crab_storage::StoreLayout::new(storage.clone(), String::new());
    crab_metadata::ref_registry::visit_ref_registry_shard_roots(
        &storage,
        &router,
        |repo_prefix, hash| {
            let result = if !registry.repos.contains_key(&repo_prefix) {
                Err(CrabError::CorruptObject {
                    path: ".crab/ref-registry/shard-roots".to_owned(),
                    reason: format!("shard roots have no repo record for {repo_prefix}"),
                })
            } else {
                MerkleHash::from_hex(&hash)
                    .map_err(|error| CrabError::CorruptObject {
                        path: ".crab/ref-registry/shard-roots".to_owned(),
                        reason: format!("invalid current shard hash for {repo_prefix}: {error}"),
                    })
                    .map(|_| {
                        digest.add("current-shard", &format!("{repo_prefix}\0{hash}"));
                    })
            };
            std::future::ready(result)
        },
    )
    .await?;
    for repo_prefix in &repo_prefixes {
        let mut visit = |hash: String| {
            let result = MerkleHash::from_hex(&hash)
                .map_err(|error| CrabError::CorruptObject {
                    path: format!("{repo_prefix}/metadata/shard"),
                    reason: format!("invalid historical shard hash: {error}"),
                })
                .map(|_| {
                    digest.add("historical-shard", &format!("{repo_prefix}\0{hash}"));
                });
            std::future::ready(result)
        };
        visit_repository_historical_shards(store, repo_prefix, concurrency, &mut visit).await?;
    }
    for (repo_prefix, stages) in &registry.workflow_stage_hashes {
        for stage in stages {
            digest.add("workflow-stage", &format!("{repo_prefix}\0{stage}"));
        }
    }
    for (repo_prefix, experiments) in &registry.workflow_experiment_ids {
        for experiment in experiments {
            digest.add(
                "workflow-experiment",
                &format!("{repo_prefix}\0{experiment}"),
            );
        }
    }
    for key in coordinator_protected_keys {
        digest.add("coordinator-protected", key);
    }
    Ok(BucketRootSnapshot {
        repo_prefixes,
        root_identity: digest.finish(),
    })
}

async fn write_bucket_root_marks(
    store: &Store,
    registry: &RefRegistry,
    concurrency: usize,
    journal: &super::journal::GcRunJournal,
) -> Result<()> {
    let marks = Arc::new(tokio::sync::Mutex::new(DurableMarkWriter::new_keys(
        store.clone(),
        journal.marks_prefix(),
        "referenced-shards",
    )));
    let storage = store.as_storage().clone();
    let router = crab_storage::StoreLayout::new(storage.clone(), String::new());
    crab_metadata::ref_registry::visit_ref_registry_shard_roots(
        &storage,
        &router,
        |repo_prefix, hash| {
            let marks = Arc::clone(&marks);
            let registered = registry.repos.contains_key(&repo_prefix);
            async move {
                if !registered {
                    return Err(CrabError::CorruptObject {
                        path: ".crab/ref-registry/shard-roots".to_owned(),
                        reason: format!("shard roots have no repo record for {repo_prefix}"),
                    });
                }
                MerkleHash::from_hex(&hash).map_err(|error| CrabError::CorruptObject {
                    path: ".crab/ref-registry/shard-roots".to_owned(),
                    reason: format!("invalid current shard hash for {repo_prefix}: {error}"),
                })?;
                marks.lock().await.add(&hash).await
            }
        },
    )
    .await?;
    let mut repo_prefixes = registry.repos.keys().cloned().collect::<Vec<_>>();
    repo_prefixes.sort_unstable();
    for repo_prefix in repo_prefixes {
        let marks_for_visit = Arc::clone(&marks);
        let mut visit = move |hash: String| {
            let marks = Arc::clone(&marks_for_visit);
            async move { marks.lock().await.add(&hash).await }
        };
        visit_repository_historical_shards(store, &repo_prefix, concurrency, &mut visit).await?;
    }
    let marks = Arc::try_unwrap(marks)
        .map_err(|_| {
            CrabError::Internal("GC root mark writer still has active readers".to_owned())
        })?
        .into_inner();
    marks.finish().await
}

async fn repository_referenced_shards(
    store: &Store,
    registry: &RefRegistry,
    concurrency: usize,
) -> Result<HashMap<String, HashSet<String>>> {
    let storage = store.clone().into_storage();
    let parallelism = concurrency.max(1);
    futures_util::stream::iter(registry.repos.iter().map(|(repo_prefix, current)| {
        let storage = storage.clone();
        let repo_prefix = repo_prefix.clone();
        let mut shards = current.iter().cloned().collect::<HashSet<_>>();
        async move {
            let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.clone());
            let mut history = crab_metadata::manifest_store::stream_manifest_history(
                &storage,
                &router,
                parallelism,
            );
            while let Some(entry) = history.try_next().await? {
                if entry.manifest.shard_index_hash.is_empty() {
                    continue;
                }
                let historical = crab_metadata::manifest_store::read_bulk_shard_list(
                    &storage,
                    &router,
                    &entry.manifest.shard_index_hash,
                )
                .await?;
                shards.extend(historical);
            }
            Ok::<_, crab_metadata::error::MetadataError>((repo_prefix, shards))
        }
    }))
    .buffer_unordered(parallelism)
    .map(|result| result.map_err(CrabError::from))
    .try_collect()
    .await
}

fn ensure_registry_complete_for_destructive_gc(registry: &RefRegistry) -> Result<()> {
    if registry.is_complete_for_destructive_gc() {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "gc.bucket.ref_registry_completeness".into(),
        origin: "destructive bucket garbage collection requires a schema-current ref-registry produced by a complete manifest backfill; run registry repair before retrying"
            .into(),
    })
}

fn ensure_active_active_bucket_gc_proof(
    registry: &RefRegistry,
    coordinator_protected_repos: &HashSet<String>,
) -> Result<()> {
    let missing = registry.active_active_repos_missing_gc_proof(coordinator_protected_repos);
    if missing.is_empty() {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "gc.bucket.active_active_proof".into(),
        origin: format!(
            "bucket garbage collection requires coordinator GC safety snapshots for every active-active repo before deleting shared .crab/ objects; missing proof for {}",
            missing.join(", ")
        ),
    })
}

/// Load the ref-registry from `.crab/ref-registry`.
///
/// If the registry doesn't exist and `force` is false, returns an error
/// advising the user to use `--force`. If `force` is true, returns an
/// explicitly incomplete registry. Dry-run can inspect it, but destructive
/// GC still fails closed until a manifest backfill establishes coverage.
pub async fn load_ref_registry(store: &Store, force: bool) -> Result<RefRegistry> {
    let storage = store.as_storage().clone();
    let router = crab_storage::StoreLayout::new(storage.clone(), String::new());
    let mut registry = crab_metadata::ref_registry::load_ref_registry(&storage, &router).await?;
    if !registry.coverage_complete && registry.repos.is_empty() {
        if !force {
            return Err(CrabError::NotFound {
                path: format!(
                    "{GLOBAL_PREFIX}/ref-registry/coverage.json (run crab gc --repair-registry)"
                ),
            });
        }
        warn!("partitioned ref-registry is not repaired; treating it as incomplete");
        registry.schema_version = 0;
    }
    Ok(registry)
}

async fn load_ref_registry_summary(store: &Store, force: bool) -> Result<RefRegistry> {
    let storage = store.as_storage().clone();
    let router = crab_storage::StoreLayout::new(storage.clone(), String::new());
    let mut registry =
        crab_metadata::ref_registry::load_ref_registry_summary(&storage, &router).await?;
    if !registry.coverage_complete && registry.repos.is_empty() {
        if !force {
            return Err(CrabError::NotFound {
                path: format!(
                    "{GLOBAL_PREFIX}/ref-registry/coverage.json (run crab gc --repair-registry)"
                ),
            });
        }
        warn!("partitioned ref-registry is not repaired; treating it as incomplete");
        registry.schema_version = 0;
    }
    Ok(registry)
}

/// Metadata for a listed object.
#[derive(Debug, Clone)]
struct ListedObject {
    location: String,
    size: u64,
    last_modified: SystemTime,
    e_tag: Option<String>,
    version: Option<String>,
}

struct GlobalListOutcome {
    objects: Vec<ListedObject>,
    /// Logical list streams. Provider pagination and retries are internal to
    /// `object_store` and are not counted here.
    requests: u64,
}

#[derive(Debug, Clone, Default)]
struct GlobalScanStats {
    objects: u64,
    requests: u64,
    parallelism: usize,
    partitioned: bool,
}

#[async_trait(?Send)]
trait GlobalObjectConsumer {
    async fn consume(&mut self, object: ListedObject) -> Result<()>;
}

/// Streams one global namespace to a consumer without retaining the complete
/// provider listing. The adaptive probe is the only bounded look-ahead; once
/// it crosses the provider-aware threshold, the probe is discarded and only
/// populated hash partitions are scanned.
async fn scan_global_objects<C: GlobalObjectConsumer + ?Sized>(
    store: &Store,
    kind: &str,
    profile: GcListProfile,
    concurrency: usize,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
    consumer: &mut C,
) -> Result<GlobalScanStats> {
    let prefix = global_content_prefix(GLOBAL_PREFIX, kind);
    match profile {
        GcListProfile::Adaptive
            if concurrency <= 1 || store.bucket_identity().cloud == StorageProviderKind::Local =>
        {
            let (requests, objects) =
                scan_global_prefix(store, kind, &prefix, None, permits, cancel, consumer).await?;
            Ok(GlobalScanStats {
                objects,
                requests,
                parallelism: 1,
                partitioned: false,
            })
        }
        GcListProfile::Cost => {
            let (requests, objects) =
                scan_global_prefix(store, kind, &prefix, None, permits, cancel, consumer).await?;
            Ok(GlobalScanStats {
                objects,
                requests,
                parallelism: 1,
                partitioned: false,
            })
        }
        GcListProfile::Latency => {
            scan_global_partitions(store, kind, concurrency, permits, cancel, consumer).await
        }
        GcListProfile::Adaptive => {
            let probe_limit = adaptive_probe_limit(store);
            let probe = list_global_prefix(
                store,
                kind,
                &prefix,
                Some(probe_limit),
                Arc::clone(&permits),
                cancel,
            )
            .await?;
            if probe.objects.len() <= probe_limit {
                let objects = probe.objects.len() as u64;
                for object in probe.objects {
                    consumer.consume(object).await?;
                }
                return Ok(GlobalScanStats {
                    objects,
                    requests: probe.requests,
                    parallelism: 1,
                    partitioned: false,
                });
            }
            let probe_requests = probe.requests;
            let mut stats =
                scan_global_partitions(store, kind, concurrency, permits, cancel, consumer).await?;
            stats.requests = stats.requests.saturating_add(probe_requests);
            Ok(stats)
        }
    }
}

async fn scan_global_prefix<C: GlobalObjectConsumer + ?Sized>(
    store: &Store,
    kind: &str,
    prefix: &ObjectPath,
    max_objects: Option<usize>,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
    consumer: &mut C,
) -> Result<(u64, u64)> {
    let _permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        permit = permits.acquire() => {
            permit.map_err(|_| CrabError::Internal("global LIST semaphore closed".to_owned()))?
        }
    };
    let mut stream = store.inner().list(Some(prefix));
    let mut objects = 0u64;
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
            next = stream.try_next() => next.map_err(CrabError::Storage)?,
        };
        let Some(meta) = next else {
            break;
        };
        let location = meta.location.to_string();
        if content_hash_from_path(&location, kind).is_none() {
            return Err(CrabError::CorruptObject {
                path: location,
                reason: format!("global {kind} object does not match its hash partition"),
            });
        }
        consumer
            .consume(ListedObject {
                location,
                size: meta.size,
                last_modified: meta.last_modified.into(),
                e_tag: meta.e_tag,
                version: meta.version,
            })
            .await?;
        objects = objects.saturating_add(1);
        if max_objects.is_some_and(|limit| objects as usize > limit) {
            break;
        }
    }
    Ok((1, objects))
}

async fn scan_global_partitions<C: GlobalObjectConsumer + ?Sized>(
    store: &Store,
    kind: &str,
    concurrency: usize,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
    consumer: &mut C,
) -> Result<GlobalScanStats> {
    let prefix = global_content_prefix(GLOBAL_PREFIX, kind);
    let discovery_permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        permit = permits.acquire() => {
            permit.map_err(|_| CrabError::Internal("global LIST semaphore closed".to_owned()))?
        }
    };
    let discovery = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        result = store.inner().list_with_delimiter(Some(&prefix)) => {
            result.map_err(CrabError::Storage)?
        }
    };
    drop(discovery_permit);
    if let Some(object) = discovery.objects.first() {
        return Err(CrabError::CorruptObject {
            path: object.location.to_string(),
            reason: format!("global {kind} object is outside the required two-hex hash partition"),
        });
    }
    let mut partitions = discovery
        .common_prefixes
        .into_iter()
        .map(|partition| {
            let value = partition
                .as_ref()
                .strip_prefix(prefix.as_ref())
                .and_then(|suffix| suffix.strip_prefix('/'))
                .unwrap_or_default();
            if value.len() != 2
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            {
                return Err(CrabError::CorruptObject {
                    path: partition.to_string(),
                    reason: format!(
                        "global {kind} partition must be exactly two lowercase hex characters"
                    ),
                });
            }
            Ok(value.to_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    partitions.sort_unstable();
    partitions.dedup();
    let parallelism = concurrency.max(1).min(partitions.len().max(1));
    let partition_count = partitions.len() as u64;
    let mut objects = 0u64;
    for partitions in partitions.chunks(parallelism) {
        let prefixes = partitions
            .iter()
            .map(|partition| global_content_partition_prefix(GLOBAL_PREFIX, kind, partition))
            .collect::<Vec<_>>();
        let streams = futures_util::stream::iter(prefixes.iter())
            .map(|prefix| {
                global_partition_stream(store, kind, prefix, Arc::clone(&permits), cancel)
            })
            .buffer_unordered(parallelism)
            .try_collect::<Vec<_>>()
            .await?;
        let mut merged = futures_util::stream::select_all(streams);
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(CrabError::Cancelled),
                next = merged.next() => next,
            };
            let Some(object) = next else {
                break;
            };
            consumer.consume(object?).await?;
            objects = objects.saturating_add(1);
        }
    }
    Ok(GlobalScanStats {
        objects,
        requests: 1 + partition_count,
        parallelism,
        partitioned: true,
    })
}

async fn global_partition_stream<'a>(
    store: &'a Store,
    kind: &'a str,
    prefix: &'a ObjectPath,
    permits: Arc<Semaphore>,
    cancel: &'a CancellationToken,
) -> Result<BoxStream<'a, Result<ListedObject>>> {
    let permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        permit = permits.acquire_owned() => {
            permit.map_err(|_| CrabError::Internal("global LIST semaphore closed".to_owned()))?
        }
    };
    Ok(Box::pin(store.inner().list(Some(prefix)).map(
        move |result| {
            let _permit = &permit;
            let meta = result.map_err(CrabError::Storage)?;
            let location = meta.location.to_string();
            if content_hash_from_path(&location, kind).is_none() {
                return Err(CrabError::CorruptObject {
                    path: location,
                    reason: format!("global {kind} object does not match its hash partition"),
                });
            }
            Ok(ListedObject {
                location,
                size: meta.size,
                last_modified: meta.last_modified.into(),
                e_tag: meta.e_tag,
                version: meta.version,
            })
        },
    )))
}

// object_store leaves max-keys unset, so each provider applies these service
// page capacities. Probe by object count because object_store hides pages.
const S3_GCS_LIST_PAGE_OBJECTS: usize = 1_000;
const AZURE_LIST_PAGE_OBJECTS: usize = 5_000;
const GLOBAL_HASH_PARTITIONS: usize = 256;

fn adaptive_probe_limit(store: &Store) -> usize {
    let page_objects = match store.bucket_identity().cloud {
        StorageProviderKind::Azure => AZURE_LIST_PAGE_OBJECTS,
        StorageProviderKind::S3 | StorageProviderKind::Gcs => S3_GCS_LIST_PAGE_OBJECTS,
        StorageProviderKind::Local => return usize::MAX,
    };
    // Switch only once recursive pagination costs as many calls as the full
    // fan-out. Replaying the bounded probe then caps crossover cost near 2x.
    page_objects.saturating_mul(GLOBAL_HASH_PARTITIONS)
}

async fn scan_closure_objects<C: GlobalObjectConsumer + ?Sized>(
    store: &Store,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
    consumer: &mut C,
) -> Result<GlobalScanStats> {
    let _permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        permit = permits.acquire() => {
            permit.map_err(|_| CrabError::Internal("closure LIST semaphore closed".to_owned()))?
        }
    };
    let prefix = format!("{GLOBAL_PREFIX}/gc/closures/");
    let mut stream = store.inner().list(Some(&ObjectPath::from(prefix.as_str())));
    let mut objects = 0u64;
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
            next = stream.try_next() => next.map_err(CrabError::Storage)?,
        };
        let Some(meta) = next else {
            break;
        };
        let location = meta.location.to_string();
        let Some(hash) = location
            .strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix(".json"))
        else {
            return Err(CrabError::CorruptObject {
                path: location,
                reason: "closure object is outside the canonical hash key shape".to_owned(),
            });
        };
        let parsed = MerkleHash::from_hex(hash).map_err(|error| CrabError::CorruptObject {
            path: meta.location.to_string(),
            reason: format!("invalid closure hash: {error}"),
        })?;
        if parsed.hex() != hash {
            return Err(CrabError::CorruptObject {
                path: meta.location.to_string(),
                reason: "closure hash is not canonical lowercase hex".to_owned(),
            });
        }
        consumer
            .consume(ListedObject {
                location,
                size: meta.size,
                last_modified: meta.last_modified.into(),
                e_tag: meta.e_tag,
                version: meta.version,
            })
            .await?;
        objects = objects.saturating_add(1);
    }
    Ok(GlobalScanStats {
        objects,
        requests: 1,
        parallelism: 1,
        partitioned: false,
    })
}

async fn scan_closure_segment_objects<C: GlobalObjectConsumer + ?Sized>(
    store: &Store,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
    consumer: &mut C,
) -> Result<GlobalScanStats> {
    let _permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        permit = permits.acquire() => {
            permit.map_err(|_| CrabError::Internal("closure segment LIST semaphore closed".to_owned()))?
        }
    };
    let prefix = format!("{GLOBAL_PREFIX}/gc/closure-segments/");
    let mut stream = store.inner().list(Some(&ObjectPath::from(prefix.as_str())));
    let mut objects = 0u64;
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
            next = stream.try_next() => next.map_err(CrabError::Storage)?,
        };
        let Some(meta) = next else {
            break;
        };
        let location = meta.location.to_string();
        let Some((hash, index)) = location
            .strip_prefix(&prefix)
            .and_then(|value| value.split_once('/'))
            .and_then(|(hash, file)| file.strip_suffix(".json").map(|index| (hash, index)))
        else {
            return Err(CrabError::CorruptObject {
                path: location,
                reason: "closure segment is outside the canonical key shape".to_owned(),
            });
        };
        let parsed = MerkleHash::from_hex(hash).map_err(|error| CrabError::CorruptObject {
            path: meta.location.to_string(),
            reason: format!("invalid closure segment shard hash: {error}"),
        })?;
        let parsed_index = index
            .parse::<u64>()
            .map_err(|error| CrabError::CorruptObject {
                path: meta.location.to_string(),
                reason: format!("invalid closure segment index: {error}"),
            })?;
        if parsed.hex() != hash || index.len() != 20 || format!("{parsed_index:020}") != index {
            return Err(CrabError::CorruptObject {
                path: meta.location.to_string(),
                reason: "closure segment key is not canonical".to_owned(),
            });
        }
        consumer
            .consume(ListedObject {
                location,
                size: meta.size,
                last_modified: meta.last_modified.into(),
                e_tag: meta.e_tag,
                version: meta.version,
            })
            .await?;
        objects = objects.saturating_add(1);
    }
    Ok(GlobalScanStats {
        objects,
        requests: 1,
        parallelism: 1,
        partitioned: false,
    })
}

async fn list_global_prefix(
    store: &Store,
    kind: &str,
    prefix: &ObjectPath,
    max_objects: Option<usize>,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Result<GlobalListOutcome> {
    let _permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        permit = permits.acquire() => {
            permit.map_err(|_| CrabError::Internal("global LIST semaphore closed".to_owned()))?
        }
    };
    let mut stream = store.inner().list(Some(prefix));
    let mut objects = Vec::new();
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
            next = stream.try_next() => next.map_err(CrabError::Storage)?,
        };
        let Some(meta) = next else {
            break;
        };
        let location = meta.location.to_string();
        if content_hash_from_path(&location, kind).is_none() {
            return Err(CrabError::CorruptObject {
                path: location,
                reason: format!("global {kind} object does not match its hash partition"),
            });
        }
        objects.push(ListedObject {
            location,
            size: meta.size,
            last_modified: meta.last_modified.into(),
            e_tag: meta.e_tag,
            version: meta.version,
        });
        if max_objects.is_some_and(|limit| objects.len() > limit) {
            break;
        }
    }

    Ok(GlobalListOutcome {
        objects,
        requests: 1,
    })
}

/// Extract the hash portion from a canonical global content key.
fn extract_hash_from_key(key: &str) -> String {
    key.rsplit('/').next().unwrap_or("").to_string()
}

async fn extract_hashes_from_shard(
    store: &Store,
    object: &ListedObject,
    closure_budget: Option<&Semaphore>,
) -> Result<(String, HashSet<String>, HashSet<MerkleHash>)> {
    let hash_hex = extract_hash_from_key(&object.location);
    let hash = MerkleHash::from_hex(&hash_hex).map_err(|error| CrabError::CorruptObject {
        path: object.location.clone(),
        reason: format!("invalid referenced shard hash: {error}"),
    })?;
    let closure_path = super::closure::path(GLOBAL_PREFIX, &hash_hex);
    let _closure_permit = match closure_budget {
        Some(budget) => Some(budget.acquire().await.map_err(|_| CrabError::Cancelled)?),
        None => None,
    };
    let closure = super::closure::read_manifest(store, GLOBAL_PREFIX, &hash)
        .await
        .map_err(|error| {
        if matches!(error, CrabError::NotFound { .. }) {
            CrabError::CorruptObject {
                path: closure_path.to_string(),
                reason: format!(
                    "referenced shard {hash_hex} has no durable closure; run `crab gc --scope=bucket --repair-closures` before destructive GC"
                ),
            }
        } else {
            error
        }
    })?;
    if closure.content_size != object.size {
        return Err(CrabError::CorruptObject {
            path: closure_path.to_string(),
            reason: format!(
                "shard closure size {} does not match listed shard size {}",
                closure.content_size, object.size
            ),
        });
    }
    let mut xorbs = HashSet::new();
    let mut files = HashSet::new();
    for segment_ref in &closure.segments {
        let segment =
            super::closure::read_segment(store, GLOBAL_PREFIX, &closure, segment_ref).await?;
        xorbs.extend(segment.xorb_hashes().iter().cloned());
        for file_hash in segment.file_hashes() {
            files.insert(MerkleHash::from_hex(file_hash).map_err(|error| {
                CrabError::CorruptObject {
                    path: closure_path.to_string(),
                    reason: format!("invalid file hash in shard closure: {error}"),
                }
            })?);
        }
    }
    Ok((hash_hex, xorbs, files))
}

async fn mark_hashes_from_shard(
    store: &Store,
    object: &ListedObject,
    xorb_marks: &mut DurableMarkWriter,
    file_marks: &mut DurableMarkWriter,
) -> Result<()> {
    let hash_hex = extract_hash_from_key(&object.location);
    let hash = MerkleHash::from_hex(&hash_hex).map_err(|error| CrabError::CorruptObject {
        path: object.location.clone(),
        reason: format!("invalid referenced shard hash: {error}"),
    })?;
    let closure_path = super::closure::path(GLOBAL_PREFIX, &hash_hex);
    let manifest = super::closure::read_manifest(store, GLOBAL_PREFIX, &hash)
        .await
        .map_err(|error| {
            if matches!(error, CrabError::NotFound { .. }) {
                CrabError::CorruptObject {
                    path: closure_path.to_string(),
                    reason: format!(
                        "referenced shard {hash_hex} has no durable closure; run `crab gc --scope=bucket --repair-closures` before destructive GC"
                    ),
                }
            } else {
                error
            }
        })?;
    if manifest.content_size != object.size {
        return Err(CrabError::CorruptObject {
            path: closure_path.to_string(),
            reason: format!(
                "shard closure size {} does not match listed shard size {}",
                manifest.content_size, object.size
            ),
        });
    }
    for segment_ref in &manifest.segments {
        let segment =
            super::closure::read_segment(store, GLOBAL_PREFIX, &manifest, segment_ref).await?;
        for xorb in segment.xorb_hashes() {
            xorb_marks.add(xorb).await?;
        }
        for file in segment.file_hashes() {
            file_marks.add(file).await?;
        }
    }
    Ok(())
}

struct CandidateBatchSink<'a> {
    journal: &'a mut super::journal::GcRunJournal,
    batch: Vec<super::ObjectMeta>,
}

impl<'a> CandidateBatchSink<'a> {
    fn new(journal: &'a mut super::journal::GcRunJournal) -> Self {
        Self {
            journal,
            batch: Vec::with_capacity(super::journal::DEFAULT_BATCH_SIZE),
        }
    }

    async fn push(&mut self, object: &ListedObject) -> Result<()> {
        self.batch.push(super::ObjectMeta {
            key: object.location.clone(),
            size: object.size,
            last_modified: object.last_modified,
            e_tag: object.e_tag.clone(),
            version: object.version.clone(),
            storage_class: None,
            transitioned_at: None,
        });
        if self.batch.len() == super::journal::DEFAULT_BATCH_SIZE {
            self.journal.append_candidates(&self.batch).await?;
            self.batch.clear();
        }
        Ok(())
    }

    async fn finish(self) -> Result<()> {
        if !self.batch.is_empty() {
            self.journal.append_candidates(&self.batch).await?;
        }
        Ok(())
    }
}

struct ShardStreamingPlan {
    referenced_seen: usize,
}

struct ShardStreamingPlanner<'a> {
    store: &'a Store,
    referenced_shards: DurableMarkReader,
    existing_shards: DurableMarkWriter,
    deletable_shards: DurableMarkWriter,
    referenced_xorbs: DurableMarkWriter,
    referenced_files: DurableMarkWriter,
    coordinator_protected_keys: &'a HashSet<String>,
    cutoff: SystemTime,
    force: bool,
    referenced_seen: usize,
    sink: CandidateBatchSink<'a>,
}

impl<'a> ShardStreamingPlanner<'a> {
    fn new(
        store: &'a Store,
        referenced_shards: DurableMarkReader,
        marks_prefix: String,
        coordinator_protected_keys: &'a HashSet<String>,
        cutoff: SystemTime,
        force: bool,
        journal: &'a mut super::journal::GcRunJournal,
    ) -> Self {
        Self {
            store,
            referenced_shards,
            existing_shards: DurableMarkWriter::new(
                store.clone(),
                marks_prefix.clone(),
                "existing-shards",
            ),
            deletable_shards: DurableMarkWriter::new(
                store.clone(),
                marks_prefix.clone(),
                "deletable-shards",
            ),
            referenced_xorbs: DurableMarkWriter::new(
                store.clone(),
                marks_prefix,
                "referenced-xorbs",
            ),
            referenced_files: DurableMarkWriter::new_hash_width(
                store.clone(),
                journal.marks_prefix(),
                "referenced-files",
                4,
            ),
            coordinator_protected_keys,
            cutoff,
            force,
            referenced_seen: 0,
            sink: CandidateBatchSink::new(journal),
        }
    }

    async fn finish(self) -> Result<ShardStreamingPlan> {
        let Self {
            existing_shards,
            deletable_shards,
            referenced_xorbs,
            referenced_seen,
            referenced_files,
            referenced_shards: _,
            sink,
            ..
        } = self;
        existing_shards.finish().await?;
        deletable_shards.finish().await?;
        referenced_xorbs.finish().await?;
        referenced_files.finish().await?;
        sink.finish().await?;
        Ok(ShardStreamingPlan { referenced_seen })
    }
}

#[async_trait(?Send)]
impl GlobalObjectConsumer for ShardStreamingPlanner<'_> {
    async fn consume(&mut self, object: ListedObject) -> Result<()> {
        let hash = extract_hash_from_key(&object.location);
        self.existing_shards.add(&hash).await?;
        let is_protected = self.coordinator_protected_keys.contains(&object.location);
        let is_referenced = self.referenced_shards.contains(&hash).await?;
        if is_protected || is_referenced {
            if is_referenced {
                self.referenced_seen = self.referenced_seen.saturating_add(1);
            }
            mark_hashes_from_shard(
                self.store,
                &object,
                &mut self.referenced_xorbs,
                &mut self.referenced_files,
            )
            .await?;
            return Ok(());
        }
        if self.force || object.last_modified < self.cutoff {
            self.deletable_shards.add(&hash).await?;
            self.sink.push(&object).await?;
        }
        Ok(())
    }
}

struct ClosureStreamingPlanner<'a> {
    store: &'a Store,
    referenced_shards: DurableMarkReader,
    deletable_shards: DurableMarkReader,
    existing_shards: DurableMarkReader,
    coordinator_protected_keys: &'a HashSet<String>,
    cutoff: SystemTime,
    force: bool,
    live_segments: DurableMarkWriter,
    sink: CandidateBatchSink<'a>,
}

impl<'a> ClosureStreamingPlanner<'a> {
    fn new(
        store: &'a Store,
        referenced_shards: DurableMarkReader,
        deletable_shards: DurableMarkReader,
        existing_shards: DurableMarkReader,
        coordinator_protected_keys: &'a HashSet<String>,
        cutoff: SystemTime,
        force: bool,
        journal: &'a mut super::journal::GcRunJournal,
    ) -> Self {
        Self {
            store,
            referenced_shards,
            deletable_shards,
            existing_shards,
            coordinator_protected_keys,
            cutoff,
            force,
            live_segments: DurableMarkWriter::new_keys(
                store.clone(),
                journal.marks_prefix(),
                "live-closure-segments",
            ),
            sink: CandidateBatchSink::new(journal),
        }
    }

    async fn finish(self) -> Result<()> {
        self.live_segments.finish().await?;
        self.sink.finish().await
    }
}

#[async_trait(?Send)]
impl GlobalObjectConsumer for ClosureStreamingPlanner<'_> {
    async fn consume(&mut self, object: ListedObject) -> Result<()> {
        let hash = extract_hash_from_key(&object.location)
            .strip_suffix(".json")
            .unwrap_or_default()
            .to_owned();
        let retained = self.coordinator_protected_keys.contains(&object.location)
            || self.referenced_shards.contains(&hash).await?
            || (!self.deletable_shards.contains(&hash).await?
                && self.existing_shards.contains(&hash).await?)
            || (!self.force && object.last_modified >= self.cutoff);
        if retained {
            let parsed = MerkleHash::from_hex(&hash).map_err(|error| CrabError::CorruptObject {
                path: object.location.clone(),
                reason: format!("invalid closure shard hash: {error}"),
            })?;
            let manifest =
                super::closure::read_manifest(self.store, GLOBAL_PREFIX, &parsed).await?;
            for segment in &manifest.segments {
                let path = super::closure::segment_path(GLOBAL_PREFIX, &hash, segment.index);
                self.live_segments.add(path.as_ref()).await?;
            }
            return Ok(());
        }
        self.sink.push(&object).await?;
        Ok(())
    }
}

struct ClosureSegmentStreamingPlanner<'a> {
    live_segments: DurableMarkReader,
    coordinator_protected_keys: &'a HashSet<String>,
    cutoff: SystemTime,
    force: bool,
    sink: CandidateBatchSink<'a>,
}

impl<'a> ClosureSegmentStreamingPlanner<'a> {
    fn new(
        live_segments: DurableMarkReader,
        coordinator_protected_keys: &'a HashSet<String>,
        cutoff: SystemTime,
        force: bool,
        journal: &'a mut super::journal::GcRunJournal,
    ) -> Self {
        Self {
            live_segments,
            coordinator_protected_keys,
            cutoff,
            force,
            sink: CandidateBatchSink::new(journal),
        }
    }

    async fn finish(self) -> Result<()> {
        self.sink.finish().await
    }
}

#[async_trait(?Send)]
impl GlobalObjectConsumer for ClosureSegmentStreamingPlanner<'_> {
    async fn consume(&mut self, object: ListedObject) -> Result<()> {
        if self.coordinator_protected_keys.contains(&object.location)
            || self.live_segments.contains(&object.location).await?
            || (!self.force && object.last_modified >= self.cutoff)
        {
            return Ok(());
        }
        self.sink.push(&object).await
    }
}

struct XorbStreamingPlanner<'a> {
    referenced_xorbs: DurableMarkReader,
    coordinator_protected_keys: &'a HashSet<String>,
    cutoff: SystemTime,
    force: bool,
    sink: CandidateBatchSink<'a>,
}

impl<'a> XorbStreamingPlanner<'a> {
    fn new(
        referenced_xorbs: DurableMarkReader,
        coordinator_protected_keys: &'a HashSet<String>,
        cutoff: SystemTime,
        force: bool,
        journal: &'a mut super::journal::GcRunJournal,
    ) -> Self {
        Self {
            referenced_xorbs,
            coordinator_protected_keys,
            cutoff,
            force,
            sink: CandidateBatchSink::new(journal),
        }
    }

    async fn finish(self) -> Result<()> {
        self.sink.finish().await
    }
}

#[async_trait(?Send)]
impl GlobalObjectConsumer for XorbStreamingPlanner<'_> {
    async fn consume(&mut self, object: ListedObject) -> Result<()> {
        if self.coordinator_protected_keys.contains(&object.location) {
            return Ok(());
        }
        let hash = extract_hash_from_key(&object.location);
        if self.referenced_xorbs.contains(&hash).await? {
            return Ok(());
        }
        if self.force || object.last_modified < self.cutoff {
            self.sink.push(&object).await?;
        }
        Ok(())
    }
}

async fn write_referenced_file_marks_from_marked_shards(
    store: &Store,
    referenced_shards: &mut DurableMarkReader,
    concurrency: usize,
    cancel: &CancellationToken,
    file_marks: &mut DurableMarkWriter,
) -> Result<()> {
    let closure_budget = Arc::new(Semaphore::new(CLOSURE_READ_PARALLELISM));
    for partition in referenced_shards.key_partitions().await? {
        let hashes = referenced_shards.key_partition_hashes(&partition).await?;
        let mut files = futures_util::stream::iter(hashes.into_iter())
            .map(|hash_hex| {
                let closure_budget = Arc::clone(&closure_budget);
                async move {
                    check_cancelled(cancel)?;
                    let shard_path = canonical_global_content_path("shards", &hash_hex);
                    let shard_meta = store.head(&shard_path).await?;
                    let object = ListedObject {
                        location: shard_path.to_string(),
                        size: shard_meta.size,
                        last_modified: shard_meta.last_modified.into(),
                        e_tag: shard_meta.e_tag,
                        version: shard_meta.version,
                    };
                    let (_, _, files) =
                        extract_hashes_from_shard(store, &object, Some(closure_budget.as_ref()))
                            .await?;
                    Ok::<_, CrabError>(files)
                }
            })
            .buffer_unordered(concurrency.max(1));
        while let Some(shard_files) = files.try_next().await? {
            check_cancelled(cancel)?;
            for file in shard_files {
                file_marks.add(&file.hex()).await?;
            }
        }
    }
    Ok(())
}

/// Reconcile committed file-index rows one hash partition at a time. The
/// closure mark reader keeps only a bounded partition in memory, while the
/// MetaDb scan uses the same first-four-byte prefix so a large file-index database
/// never needs a process-wide referenced-hash set.
async fn gc_file_indexes_partitioned(
    store: &Store,
    repo_prefixes: &[String],
    file_marks: &mut DurableMarkReader,
    dry_run: bool,
    mut journal: Option<&mut super::journal::GcRunJournal>,
    cancel: &CancellationToken,
) -> Result<u64> {
    let mut total = 0u64;
    for repo_prefix in repo_prefixes {
        let db_prefix = ObjectPath::from(format!(
            "{}/file_index_db/",
            repo_prefix.trim_end_matches('/')
        ));
        let mut objects = store.inner().list(Some(&db_prefix));
        match objects.next().await {
            None => continue,
            Some(Err(error)) => return Err(CrabError::Storage(error)),
            Some(Ok(_)) => {}
        }

        let config = crate::metadata::MetaDbConfig::for_repo(repo_prefix).with_read_only(dry_run);
        let metadb =
            crate::metadata::MetaDb::new(Arc::clone(store.inner()), repo_prefix.clone(), config);
        let guard = crate::metadata::MetaDbGuard::new(metadb);
        let operation = async {
            let file_index = guard.file_index().await?;
            let mut occupied = file_index
                .committed_hash_prefixes()
                .await?
                .into_iter()
                .collect::<Vec<_>>();
            occupied.sort_unstable();
            let mut removed = 0u64;
            for [a, b, c, d] in occupied {
                let partition = format!("{a:02x}{b:02x}{c:02x}{d:02x}");
                let hashes = file_marks.partition_hashes(&partition).await?;
                let referenced = hashes
                    .into_iter()
                    .map(|hash| {
                        MerkleHash::from_hex(&hash).map_err(|error| CrabError::CorruptObject {
                            path: format!("gc/marks/referenced-files/{partition}"),
                            reason: format!("invalid referenced file hash in mark set: {error}"),
                        })
                    })
                    .collect::<Result<HashSet<_>>>()?;
                let prefix = [crab_metadata::key_codec::PREFIX_COMMITTED, a, b, c, d];
                let lease = if dry_run || journal.is_none() {
                    None
                } else {
                    let run_id = journal
                        .as_deref()
                        .ok_or_else(|| {
                            CrabError::Internal("file-index GC lost its journal".to_owned())
                        })?
                        .state()
                        .run_id
                        .clone();
                    let lease = crate::maintenance::GcSweepLease::acquire_for_run(
                        store,
                        GLOBAL_PREFIX,
                        &run_id,
                        cancel,
                    )
                    .await?;
                    if let Some(journal) = journal.as_deref() {
                        if let Err(error) = journal.ensure_next_fence_epoch(lease.epoch()) {
                            let _ = lease.release().await;
                            return Err(error);
                        }
                    }
                    Some(lease)
                };
                let count_result = file_index
                    .gc_unreferenced_committed_prefix(
                        &prefix,
                        &referenced,
                        dry_run,
                        FILE_INDEX_GC_BATCH_SIZE,
                    )
                    .await;
                let count = match count_result {
                    Ok(count) => count,
                    Err(error) => {
                        if let Some(lease) = lease {
                            let _ = lease.release().await;
                        }
                        return Err(error.into());
                    }
                };
                if let Some(lease) = lease {
                    if let Some(journal) = journal.as_deref_mut() {
                        journal.advance_fence_epoch(lease.epoch()).await?;
                    }
                    lease.release().await?;
                }
                removed = removed.checked_add(count).ok_or_else(|| {
                    CrabError::Internal("file-index GC count overflow".to_owned())
                })?;
            }
            Ok::<_, CrabError>(removed)
        }
        .await;
        let close = guard.close().await;
        let removed = match (operation, close) {
            (Ok(removed), Ok(())) => removed,
            (Err(error), _) | (Ok(_), Err(error)) => return Err(error),
        };
        total = total
            .checked_add(removed)
            .ok_or_else(|| CrabError::Internal("file-index GC count overflow".to_owned()))?;
    }
    Ok(total)
}

/// Deregister a repo from the ref-registry.
///
/// Loads the current ref-registry via CAS, removes the repo's entry,
/// and writes back. After deregistration, the next bucket-scope GC run
/// will clean up objects exclusively referenced by that repo.
pub async fn deregister_repo(store: &Store, repo_prefix: &str) -> Result<()> {
    let cancel = CancellationToken::new();
    let lease =
        crate::maintenance::GcGlobalWriterLease::acquire(store, GLOBAL_PREFIX, &cancel).await?;
    let result = async {
        let storage = store.as_storage().clone();
        let router = crab_storage::StoreLayout::new(storage.clone(), String::new());
        let removed =
            crab_metadata::ref_registry::deregister_repo(&storage, &router, repo_prefix).await?;
        if removed {
            info!(repo = %repo_prefix, "deregistered repo");
        } else {
            warn!(repo = %repo_prefix, "repo not found in ref-registry");
        }
        Ok(())
    }
    .await;
    let release = lease.release().await;
    match (result, release) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

/// Rebuild the bucket ref-registry from every discoverable repo manifest.
///
/// This is the explicit administrative proof required before destructive
/// bucket GC. Any unreadable manifest or shard index aborts the repair; a
/// partial scan is never marked complete.
pub async fn repair_ref_registry(store: &Store) -> Result<(usize, usize)> {
    use futures_util::StreamExt;

    let cancel = CancellationToken::new();
    let lease = crate::maintenance::GcSweepLease::acquire(store, GLOBAL_PREFIX, &cancel).await?;
    let result = async {
        let mut manifests = store.inner().list(None);
        let mut repo_prefixes = Vec::new();
        while let Some(item) = manifests.next().await {
            let meta = item.map_err(CrabError::from)?;
            let location = meta.location.as_ref();
            let Some(repo_prefix) = location.strip_suffix("/manifest") else {
                continue;
            };
            if repo_prefix.is_empty() || repo_prefix.starts_with(".crab/") {
                continue;
            }
            repo_prefixes.push(repo_prefix.to_owned());
        }
        repo_prefixes.sort();
        repo_prefixes.dedup();

        let mut repos = std::collections::HashMap::with_capacity(repo_prefixes.len());
        let mut shard_count = 0usize;
        for repo_prefix in &repo_prefixes {
            let router = StoreLayout::new(store.clone(), repo_prefix.clone());
            let snapshot =
                crate::metadata::manifest::read_repository_snapshot(store, &router).await?;
            let shards = snapshot.journal.shards;
            shard_count = shard_count.checked_add(shards.len()).ok_or_else(|| {
                CrabError::Internal("ref-registry repair shard count overflow".to_owned())
            })?;
            repos.insert(repo_prefix.clone(), shards);
        }

        let storage = store.clone().into_storage();
        let router = crab_storage::StoreLayout::new(storage.clone(), String::new());
        crab_metadata::ref_registry::repair_ref_registry_from_manifests(&storage, &router, repos)
            .await?;
        info!(
            repos = repo_prefixes.len(),
            shards = shard_count,
            "ref-registry manifest backfill complete"
        );
        Ok((repo_prefixes.len(), shard_count))
    }
    .await;
    let release = lease.release().await;
    match (result, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use object_store::memory::InMemory;
    use std::sync::Arc;

    use crab_metadata::ref_registry::ActiveActiveCoordinatorRegistration;

    fn memory_store() -> Store {
        Store::new(Arc::new(InMemory::new()))
    }

    struct TestListOutcome {
        objects: Vec<ListedObject>,
        requests: u64,
        parallelism: usize,
    }

    #[derive(Default)]
    struct TestObjectCollector(Vec<ListedObject>);

    #[async_trait(?Send)]
    impl GlobalObjectConsumer for TestObjectCollector {
        async fn consume(&mut self, object: ListedObject) -> Result<()> {
            self.0.push(object);
            Ok(())
        }
    }

    async fn list_global_objects(
        store: &Store,
        kind: &str,
        profile: GcListProfile,
        concurrency: usize,
        permits: Arc<Semaphore>,
        cancel: &CancellationToken,
    ) -> Result<TestListOutcome> {
        let mut collector = TestObjectCollector::default();
        let stats = scan_global_objects(
            store,
            kind,
            profile,
            concurrency,
            permits,
            cancel,
            &mut collector,
        )
        .await?;
        Ok(TestListOutcome {
            objects: collector.0,
            requests: stats.requests,
            parallelism: stats.parallelism,
        })
    }

    async fn seed_registry(store: &Store, registry: &RefRegistry) {
        let storage = store.as_storage().clone();
        let bucket_router = crab_storage::StoreLayout::new(storage.clone(), String::new());
        let mut repos = registry.repos.clone();
        for repo in registry
            .workflow_stage_hashes
            .keys()
            .chain(registry.workflow_experiment_ids.keys())
            .chain(registry.active_active_coordinators.keys())
        {
            repos.entry(repo.clone()).or_default();
        }
        crab_metadata::ref_registry::repair_ref_registry_from_manifests(
            &storage,
            &bucket_router,
            repos,
        )
        .await
        .unwrap();
        for repo in registry.repos.keys().chain(
            registry
                .workflow_stage_hashes
                .keys()
                .chain(registry.workflow_experiment_ids.keys()),
        ) {
            let router = crab_storage::StoreLayout::new(storage.clone(), repo.clone());
            crab_metadata::ref_registry::register_workflow_roots_exact(
                &storage,
                &router,
                registry
                    .workflow_stage_hashes
                    .get(repo)
                    .cloned()
                    .unwrap_or_default(),
                registry
                    .workflow_experiment_ids
                    .get(repo)
                    .cloned()
                    .unwrap_or_default(),
            )
            .await
            .unwrap();
        }
        for (repo, coordinator) in &registry.active_active_coordinators {
            let router = crab_storage::StoreLayout::new(storage.clone(), repo.clone());
            crab_metadata::ref_registry::register_active_active_coordinator_for_repo(
                &storage,
                &router,
                coordinator.clone(),
            )
            .await
            .unwrap();
        }
    }

    #[test]
    fn extract_hash_from_key_works() {
        assert_eq!(
            extract_hash_from_key(&format!(".crab/shards/ab/{}", "ab".repeat(32))),
            "ab".repeat(32)
        );
        assert_eq!(
            extract_hash_from_key(&format!(".crab/xorbs/de/{}", "de".repeat(32))),
            "de".repeat(32)
        );
        assert_eq!(extract_hash_from_key(""), "");
    }

    #[tokio::test]
    async fn adaptive_global_listing_keeps_small_namespace_on_one_stream() {
        let store = memory_store();
        let hashes = [
            "aa".repeat(32),
            "ff".repeat(32),
            format!("aa{}", "1".repeat(62)),
        ];
        for hash in &hashes {
            store
                .put(
                    &crab_storage::canonical_global_content_path("xorbs", hash),
                    Bytes::from(hash.clone()),
                )
                .await
                .unwrap();
        }

        let mut outcome = list_global_objects(
            &store,
            "xorbs",
            GcListProfile::Adaptive,
            8,
            Arc::new(Semaphore::new(8)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        outcome
            .objects
            .sort_by(|left, right| left.location.cmp(&right.location));

        assert_eq!(outcome.objects.len(), 3);
        assert_eq!(outcome.requests, 1);
        assert_eq!(outcome.parallelism, 1);
        assert!(
            outcome
                .objects
                .iter()
                .all(|object| { content_hash_from_path(&object.location, "xorbs").is_some() })
        );
    }

    #[tokio::test]
    async fn latency_global_listing_scans_only_populated_hash_partitions() {
        let store = memory_store();
        for hash in ["aa".repeat(32), "ff".repeat(32)] {
            store
                .put(
                    &crab_storage::canonical_global_content_path("xorbs", &hash),
                    Bytes::from(hash),
                )
                .await
                .unwrap();
        }

        let outcome = list_global_objects(
            &store,
            "xorbs",
            GcListProfile::Latency,
            8,
            Arc::new(Semaphore::new(8)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.requests, 3);
        assert_eq!(outcome.parallelism, 2);
    }

    #[tokio::test]
    async fn adaptive_global_listing_partitions_large_namespace_after_bounded_probe() {
        let store = memory_store().with_bucket_identity(crab_storage::BucketIdentity::new(
            StorageProviderKind::S3,
            "bucket",
            "bucket",
        ));
        for index in 0..=256_000_u64 {
            let hash = format!("{:02x}{index:062x}", index % 256);
            store
                .put(
                    &crab_storage::canonical_global_content_path("xorbs", &hash),
                    Bytes::new(),
                )
                .await
                .unwrap();
        }

        let outcome = list_global_objects(
            &store,
            "xorbs",
            GcListProfile::Adaptive,
            16,
            Arc::new(Semaphore::new(16)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.objects.len(), 256_001);
        assert_eq!(outcome.requests, 258);
    }

    #[tokio::test]
    async fn adaptive_global_listing_keeps_local_and_serial_stores_recursive() {
        let local = memory_store();
        let s3 = memory_store().with_bucket_identity(crab_storage::BucketIdentity::new(
            StorageProviderKind::S3,
            "bucket",
            "bucket",
        ));
        for index in 0..=2_000_u64 {
            let hash = format!("{index:064x}");
            let path = crab_storage::canonical_global_content_path("xorbs", &hash);
            local.put(&path, Bytes::new()).await.unwrap();
            s3.put(&path, Bytes::new()).await.unwrap();
        }

        let local_outcome = list_global_objects(
            &local,
            "xorbs",
            GcListProfile::Adaptive,
            8,
            Arc::new(Semaphore::new(8)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let serial_outcome = list_global_objects(
            &s3,
            "xorbs",
            GcListProfile::Adaptive,
            1,
            Arc::new(Semaphore::new(1)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(local_outcome.requests, 1);
        assert_eq!(serial_outcome.requests, 1);
    }

    #[test]
    fn adaptive_probe_uses_provider_page_capacity() {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let s3 = Store::new(Arc::clone(&inner)).with_bucket_identity(
            crab_storage::BucketIdentity::new(StorageProviderKind::S3, "bucket", "bucket"),
        );
        let azure = Store::new(Arc::clone(&inner)).with_bucket_identity(
            crab_storage::BucketIdentity::new(StorageProviderKind::Azure, "account", "container"),
        );
        let local = Store::new(inner);

        assert_eq!(adaptive_probe_limit(&s3), 256_000);
        assert_eq!(adaptive_probe_limit(&azure), 1_280_000);
        assert_eq!(adaptive_probe_limit(&local), usize::MAX);
    }

    #[tokio::test]
    async fn global_listing_honors_cancellation_during_enumeration() {
        let store = memory_store();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = list_global_objects(
            &store,
            "xorbs",
            GcListProfile::Cost,
            8,
            Arc::new(Semaphore::new(8)),
            &cancel,
        )
        .await;

        assert!(matches!(result, Err(CrabError::Cancelled)));
    }

    #[tokio::test]
    async fn global_listing_rejects_flat_legacy_objects() {
        let store = memory_store();
        let hash = "ab".repeat(32);
        store
            .put(
                &ObjectPath::from(format!(".crab/xorbs/{hash}")),
                Bytes::from_static(b"legacy"),
            )
            .await
            .unwrap();

        let result = list_global_objects(
            &store,
            "xorbs",
            GcListProfile::Adaptive,
            8,
            Arc::new(Semaphore::new(8)),
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(CrabError::CorruptObject { .. })));
    }

    #[tokio::test]
    async fn destructive_plan_collects_orphan_closure_segments() {
        let store = memory_store();
        let hash = "a".repeat(64);
        let path = super::super::closure::segment_path(GLOBAL_PREFIX, &hash, 7);
        store
            .put(&path, Bytes::from_static(b"orphan"))
            .await
            .unwrap();
        let mut journal = super::super::journal::GcRunJournal::start(
            store.clone(),
            GLOBAL_PREFIX,
            "bucket",
            GLOBAL_PREFIX,
            SystemTime::now(),
            Duration::from_secs(3600),
            true,
        )
        .await
        .unwrap();
        journal.set_root_identity("root").await.unwrap();
        let protected = HashSet::new();
        let mut planner = ClosureSegmentStreamingPlanner::new(
            DurableMarkReader::new_keys(
                store.clone(),
                journal.marks_prefix(),
                "live-closure-segments",
            ),
            &protected,
            SystemTime::now(),
            true,
            &mut journal,
        );

        scan_closure_segment_objects(
            &store,
            Arc::new(Semaphore::new(1)),
            &CancellationToken::new(),
            &mut planner,
        )
        .await
        .unwrap();
        planner.finish().await.unwrap();
        journal.finish_plan().await.unwrap();

        let batch = journal.next_batch().await.unwrap().unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].key, path.to_string());
    }

    #[tokio::test]
    async fn streaming_plan_retains_coordinator_protected_xorb() {
        let store = memory_store();
        let hash = "a".repeat(64);
        let path = crab_storage::canonical_global_content_path("xorbs", &hash).to_string();
        let mut journal = super::super::journal::GcRunJournal::start(
            store.clone(),
            GLOBAL_PREFIX,
            "bucket",
            GLOBAL_PREFIX,
            SystemTime::now(),
            Duration::from_secs(3600),
            true,
        )
        .await
        .unwrap();
        journal.set_root_identity("root").await.unwrap();
        let protected = [path.clone()].into_iter().collect();
        let mut planner = XorbStreamingPlanner::new(
            DurableMarkReader::new(store.clone(), journal.marks_prefix(), "referenced-xorbs"),
            &protected,
            SystemTime::now(),
            true,
            &mut journal,
        );

        planner
            .consume(ListedObject {
                location: path,
                size: 7,
                last_modified: SystemTime::UNIX_EPOCH,
                e_tag: Some("planned".to_owned()),
                version: None,
            })
            .await
            .unwrap();
        planner.finish().await.unwrap();
        journal.finish_plan().await.unwrap();

        assert!(journal.next_batch().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_registry_missing_without_force_errors() {
        let store = memory_store();
        let result = load_ref_registry(&store, false).await;
        assert!(matches!(result, Err(CrabError::NotFound { .. })));
    }

    #[tokio::test]
    async fn load_registry_missing_with_force_returns_empty() {
        let store = memory_store();
        let reg = load_ref_registry(&store, true).await.unwrap();
        assert!(reg.repos.is_empty());
        assert_eq!(reg.generation, 0);
        assert_eq!(reg.schema_version, 0);
        assert!(!reg.is_complete_for_destructive_gc());
    }

    #[tokio::test]
    async fn load_registry_valid_json() {
        let store = memory_store();
        let reg = RefRegistry {
            generation: 5,
            repos: [("org/models".to_string(), vec!["aaa".to_string()])]
                .into_iter()
                .collect(),
            ..RefRegistry::default()
        };
        seed_registry(&store, &reg).await;

        let loaded = load_ref_registry(&store, false).await.unwrap();
        assert_eq!(loaded.repos.len(), 1);
    }

    #[tokio::test]
    async fn deregister_missing_repo_is_a_noop() {
        let store = memory_store();
        deregister_repo(&store, "org/old-repo").await.unwrap();

        assert!(matches!(
            load_ref_registry(&store, false).await,
            Err(CrabError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn deregister_removes_existing_repo() {
        let store = memory_store();
        let reg = RefRegistry {
            generation: 3,
            repos: [
                ("org/models".to_string(), vec!["aaa".to_string()]),
                ("org/datasets".to_string(), vec!["bbb".to_string()]),
            ]
            .into_iter()
            .collect(),
            ..RefRegistry::default()
        };
        seed_registry(&store, &reg).await;

        deregister_repo(&store, "org/models").await.unwrap();

        let updated = load_ref_registry(&store, false).await.unwrap();
        assert_eq!(updated.repos.len(), 1);
        assert!(!updated.repos.contains_key("org/models"));
        assert!(updated.repos.contains_key("org/datasets"));
    }

    #[tokio::test]
    async fn registry_repair_discovers_manifests_and_marks_coverage_complete() {
        let store = memory_store();
        for repo in ["org/a", "org/b"] {
            let router = StoreLayout::new(store.clone(), repo.to_owned());
            let manifest = crate::metadata::manifest::Manifest::default_for_repo("refs/heads/main");
            crate::metadata::manifest::create_manifest(&store, &router, &manifest)
                .await
                .unwrap();
        }

        let (repos, shards) = repair_ref_registry(&store).await.unwrap();

        assert_eq!((repos, shards), (2, 0));
        let registry = load_ref_registry(&store, false).await.unwrap();
        assert!(registry.is_complete_for_destructive_gc());
        assert!(registry.complete_repos.contains("org/a"));
        assert!(registry.complete_repos.contains("org/b"));
    }

    #[tokio::test]
    async fn bucket_gc_roots_include_history_only_shards() {
        use crate::metadata::manifest::{
            BulkData, Manifest, compact_pack_index, compact_shard_index, create_manifest,
            read_manifest, upload_segmented_bulk, write_manifest_cas,
        };

        let store = memory_store();
        let router = StoreLayout::new(store.clone(), "org/models".to_owned());
        let historical_shard = "a".repeat(64);
        let (old_shard_hash, _, old_shard_write) =
            compact_shard_index(1, std::slice::from_ref(&historical_shard)).unwrap();
        let (old_pack_hash, _, old_pack_write) = compact_pack_index(1, &[]).unwrap();
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: old_shard_write,
                pack_index: old_pack_write,
            },
        )
        .await
        .unwrap();
        let mut old = Manifest::default_for_repo("refs/heads/main");
        old.generation = 1;
        old.shard_index_hash = old_shard_hash;
        old.pack_index_hash = old_pack_hash;
        old.seal_git_validation();
        create_manifest(&store, &router, &old).await.unwrap();
        let (_, etag) = read_manifest(&store, &router).await.unwrap();

        let (new_shard_hash, _, new_shard_write) = compact_shard_index(2, &[]).unwrap();
        let (new_pack_hash, _, new_pack_write) = compact_pack_index(2, &[]).unwrap();
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: new_shard_write,
                pack_index: new_pack_write,
            },
        )
        .await
        .unwrap();
        let mut current = old.clone();
        current.generation = 2;
        current.shard_index_hash = new_shard_hash;
        current.pack_index_hash = new_pack_hash;
        current.seal_git_validation();
        write_manifest_cas(&store, &router, &current, &etag)
            .await
            .unwrap();
        let mut registry = RefRegistry::default();
        registry.register("org/models", Vec::new());

        let historical = repository_referenced_shards(&store, &registry, 4)
            .await
            .unwrap();

        assert_eq!(
            historical["org/models"],
            [historical_shard].into_iter().collect()
        );
    }

    #[tokio::test]
    async fn dry_run_bucket_gc_with_empty_store() {
        let store = memory_store();
        let reg = RefRegistry::default();
        seed_registry(&store, &reg).await;

        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: true,
            grace_period: Duration::from_secs(3600),
            force: false,
            yes: false,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        };

        let protected = HashSet::new();
        let protected_repos = HashSet::new();
        let outcome = run_bucket_gc(
            &args,
            &store,
            &protected,
            &protected_repos,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(outcome.dry_run);
        assert_eq!(outcome.shards_deleted, 0);
        assert_eq!(outcome.xorbs_deleted, 0);
        assert_eq!(outcome.file_index_deleted, 0);

        let mut scratch = store
            .inner()
            .list(Some(&ObjectPath::from(".crab/gc/runs/")));
        assert!(scratch.try_next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn destructive_bucket_gc_preserves_recent_xorb_without_force() {
        let store = memory_store();
        let mut registry = RefRegistry::default();
        registry.mark_coverage_complete();
        seed_registry(&store, &registry).await;
        let xorb_path = canonical_global_content_path("xorbs", &"a".repeat(64));
        store
            .put(&xorb_path, Bytes::from_static(b"recent orphan"))
            .await
            .unwrap();

        let outcome = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                grace_period: Duration::from_secs(3600),
                force: false,
                yes: false,
                list_concurrency: 16,
                list_profile: GcListProfile::Adaptive,
                delete_concurrency: 64,
                resume_run_id: None,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.xorbs_deleted, 0);
        store.get_with_etag(&xorb_path).await.unwrap();
    }

    #[tokio::test]
    async fn destructive_bucket_gc_force_deletes_recent_xorb() {
        let store = memory_store();
        let mut registry = RefRegistry::default();
        registry.mark_coverage_complete();
        seed_registry(&store, &registry).await;
        let xorb_path = canonical_global_content_path("xorbs", &"b".repeat(64));
        let xorb = Bytes::from_static(b"recent orphan");
        store.put(&xorb_path, xorb.clone()).await.unwrap();

        let outcome = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                grace_period: Duration::from_secs(3600),
                force: true,
                yes: true,
                list_concurrency: 16,
                list_profile: GcListProfile::Adaptive,
                delete_concurrency: 64,
                resume_run_id: None,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.xorbs_deleted, 1);
        assert_eq!(outcome.bytes_reclaimed, xorb.len() as u64);
        assert!(matches!(
            store.get_with_etag(&xorb_path).await,
            Err(CrabError::NotFound { .. })
        ));
        let summary = outcome.to_summary();
        assert_eq!(summary.packs_deleted, 0);
        assert_eq!(summary.xorbs_deleted, 1);
        assert_eq!(summary.shards_deleted, 0);
        assert_eq!(summary.bytes_reclaimed, xorb.len() as u64);
        assert!(!summary.dry_run);
        assert!(!summary.cancelled);
        assert!(!summary.partial_enumeration);
    }

    #[tokio::test]
    async fn bucket_gc_tombstones_file_rows_outside_each_repo_closure() {
        use crab_metadata::value_codec::CommittedFileRecord;

        let store = memory_store();
        let repo = "org/models";
        let retained = MerkleHash::from([1, 2, 3, 4]);
        let stale = MerkleHash::from([5, 6, 7, 8]);
        let shard = MerkleHash::from([9, 10, 11, 12]);
        let config = crate::metadata::MetaDbConfig::for_repo(repo);
        let guard = crate::metadata::MetaDbGuard::new(crate::metadata::MetaDb::new(
            Arc::clone(store.inner()),
            repo.to_owned(),
            config.clone(),
        ));
        let file_index = guard.file_index().await.unwrap();
        let mut transaction = guard.new_transaction().unwrap();
        file_index.save_committed_batch(
            &mut transaction,
            &[
                (
                    retained,
                    CommittedFileRecord {
                        recipe_hash: [1; 32],
                        shard_hash: shard,
                        committed_generation: 1,
                        shard_index_hash: shard,
                    },
                ),
                (
                    stale,
                    CommittedFileRecord {
                        recipe_hash: [2; 32],
                        shard_hash: shard,
                        committed_generation: 1,
                        shard_index_hash: shard,
                    },
                ),
            ],
        );
        guard.commit(transaction).await.unwrap();
        guard.close().await.unwrap();

        let marks_prefix = ".crab/gc/runs/test/marks".to_owned();
        let mut writer = DurableMarkWriter::new_hash_width(
            store.clone(),
            marks_prefix.clone(),
            "referenced-files",
            4,
        );
        writer.add(&retained.hex()).await.unwrap();
        writer.finish().await.unwrap();
        let mut reader =
            DurableMarkReader::new_hash_width(store.clone(), marks_prefix, "referenced-files", 4);
        let removed = gc_file_indexes_partitioned(
            &store,
            &[repo.to_owned()],
            &mut reader,
            false,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(removed, 1);
        let guard = crate::metadata::MetaDbGuard::new(crate::metadata::MetaDb::new(
            Arc::clone(store.inner()),
            repo.to_owned(),
            config.with_read_only(true),
        ));
        let values = guard
            .file_index()
            .await
            .unwrap()
            .get_committed_batch(&[retained, stale])
            .await
            .unwrap();
        assert!(values[0].is_some());
        assert!(values[1].is_none());
        guard.close().await.unwrap();
    }

    #[tokio::test]
    async fn gc_writer_fence_blocks_destructive_bucket_gc_but_not_preview() {
        let store = memory_store();
        let mut registry = RefRegistry::default();
        registry.register("org/models", Vec::new());
        registry.mark_coverage_complete();
        seed_registry(&store, &registry).await;
        let held = crab_coordination::GcFenceLease::acquire_writer(
            store.inner(),
            GLOBAL_PREFIX,
            crab_coordination::DEFAULT_GC_FENCE_TTL,
        )
        .await
        .unwrap();

        let destructive = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                grace_period: Duration::from_secs(3600),
                force: false,
                yes: false,
                list_concurrency: 16,
                list_profile: GcListProfile::Adaptive,
                delete_concurrency: 64,
                resume_run_id: None,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(destructive, Err(CrabError::PushLockHeld { .. })));

        let preview = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: true,
                grace_period: Duration::from_secs(3600),
                force: false,
                yes: false,
                list_concurrency: 16,
                list_profile: GcListProfile::Adaptive,
                delete_concurrency: 64,
                resume_run_id: None,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(preview.dry_run);

        held.release().await.unwrap();
    }

    #[tokio::test]
    async fn destructive_gc_aborts_when_referenced_shard_is_corrupt() {
        let store = memory_store();
        let corrupt_shard = Bytes::from_static(b"not a shard");
        let shard_hash = crab_xet::hash::compute_data_hash(&corrupt_shard).hex();
        store
            .put(
                &canonical_global_content_path("shards", &shard_hash),
                corrupt_shard,
            )
            .await
            .unwrap();

        let mut registry = RefRegistry::default();
        registry.register("org/models", vec![shard_hash]);
        registry.mark_coverage_complete();
        seed_registry(&store, &registry).await;

        let error = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                grace_period: Duration::from_secs(3600),
                force: false,
                yes: false,
                list_concurrency: 16,
                list_profile: GcListProfile::Adaptive,
                delete_concurrency: 64,
                resume_run_id: None,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn destructive_gc_aborts_when_registry_root_is_missing() {
        let store = memory_store();
        let mut registry = RefRegistry::default();
        registry.register("org/models", vec!["a".repeat(64)]);
        registry.mark_coverage_complete();
        seed_registry(&store, &registry).await;

        let error = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                grace_period: Duration::from_secs(3600),
                force: false,
                yes: false,
                list_concurrency: 16,
                list_profile: GcListProfile::Adaptive,
                delete_concurrency: 64,
                resume_run_id: None,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn bucket_gc_destructive_run_requires_proof_for_registered_active_active_repos() {
        let store = memory_store();
        let mut reg = RefRegistry::default();
        reg.mark_coverage_complete();
        reg.register_active_active_coordinator(
            "org/models",
            ActiveActiveCoordinatorRegistration {
                provider: "dynamodb".to_owned(),
                url: "dynamodb://crab-coordinator".to_owned(),
                region: "us-east-1".to_owned(),
                failover_regions: vec!["us-west-2".to_owned()],
            },
        );
        seed_registry(&store, &reg).await;
        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: false,
            yes: false,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        };
        let protected = HashSet::new();
        let protected_repos = HashSet::new();

        let err = run_bucket_gc(
            &args,
            &store,
            &protected,
            &protected_repos,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("org/models"));
    }

    #[tokio::test]
    async fn bucket_gc_destructive_run_accepts_all_repo_coordinator_proof() {
        let store = memory_store();
        let mut reg = RefRegistry::default();
        reg.mark_coverage_complete();
        reg.register_active_active_coordinator(
            "org/models",
            ActiveActiveCoordinatorRegistration {
                provider: "dynamodb".to_owned(),
                url: "dynamodb://crab-coordinator".to_owned(),
                region: "us-east-1".to_owned(),
                failover_regions: Vec::new(),
            },
        );
        seed_registry(&store, &reg).await;
        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: false,
            yes: false,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        };
        let protected = HashSet::new();
        let protected_repos = ["org/models".to_owned()].into_iter().collect();

        let outcome = run_bucket_gc(
            &args,
            &store,
            &protected,
            &protected_repos,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.shards_deleted, 0);
        assert_eq!(outcome.xorbs_deleted, 0);
    }

    #[tokio::test]
    async fn bucket_gc_destructive_run_rejects_incomplete_legacy_registry() {
        let store = memory_store();
        let legacy = br#"{"generation":1,"repos":{"org/models":["shard-a"]}}"#;
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store
            .put(&registry_path, Bytes::from_static(legacy))
            .await
            .unwrap();
        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: true,
            yes: true,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        };

        let err = run_bucket_gc(
            &args,
            &store,
            &HashSet::new(),
            &HashSet::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("ref-registry"));
    }

    /// Regression: the bucket GC walker MUST NOT enumerate or delete
    /// objects under `.crab/workflow/**`.
    ///
    /// The workflow push path (task 4.8) ships a fresh object
    /// namespace (`workflow/stages/…`, `workflow/exp/…`) that the
    /// current walker is intentionally blind to. This test seeds
    /// representative objects in that namespace, runs the GC with
    /// force=true (so a missing workflow entry in the registry
    /// doesn't error out) and dry_run=true (so we observe what
    /// *would* be deleted without needing to backdate
    /// `last_modified`), and asserts the workflow objects remain
    /// intact regardless of registry contents.
    ///
    /// If someone later adds a generic `list_global_objects(store, "*")`
    /// call to the walker without filtering, this test fails
    /// loudly: the workflow objects would show up in
    /// `bytes_reclaimed` / deletion counters.
    #[tokio::test]
    async fn bucket_gc_does_not_touch_workflow_objects() {
        let store = memory_store();

        // Empty ref-registry — worst case for the walker: every
        // workflow object is unreferenced by construction.
        let reg = RefRegistry::default();
        seed_registry(&store, &reg).await;

        // Seed representative workflow objects. Synthetic bytes
        // are fine — the walker never inspects object content,
        // only keys, so the test exercises key-matching behavior.
        let stage_key = ObjectPath::from(format!("{GLOBAL_PREFIX}/workflow/stages/ab/abcdef.json"));
        let meta_key = ObjectPath::from(format!(
            "{GLOBAL_PREFIX}/workflow/exp/01931b9e-4b3c-7b2a-b9f0-0123456789ab/meta.json"
        ));
        let stage_refs_key = ObjectPath::from(format!(
            "{GLOBAL_PREFIX}/workflow/exp/01931b9e-4b3c-7b2a-b9f0-0123456789ab/stage-refs.json"
        ));
        store
            .put(&stage_key, Bytes::from_static(b"{\"stage\": \"payload\"}"))
            .await
            .unwrap();
        store
            .put(&meta_key, Bytes::from_static(b"{\"exp\": \"payload\"}"))
            .await
            .unwrap();
        store
            .put(&stage_refs_key, Bytes::from_static(b"[\"deadbeef\"]"))
            .await
            .unwrap();

        // Run GC in dry-run mode. Nothing should be deleted (dry
        // run), and regardless of what *would* be deleted, the
        // workflow keys must not appear in any deletion counter.
        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: true,
            grace_period: Duration::from_secs(3600),
            force: true,
            yes: false,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        };
        let protected = HashSet::new();
        let protected_repos = HashSet::new();
        let outcome = run_bucket_gc(
            &args,
            &store,
            &protected,
            &protected_repos,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(outcome.dry_run);
        assert_eq!(
            outcome.shards_deleted, 0,
            "workflow stage entries must not be counted as shard deletions",
        );
        assert_eq!(
            outcome.xorbs_deleted, 0,
            "workflow stage entries must not be counted as xorb deletions",
        );
        assert_eq!(
            outcome.file_index_deleted, 0,
            "workflow stage entries must not be counted as file-index deletions",
        );
        assert_eq!(outcome.bytes_reclaimed, 0);

        // All three workflow objects survive — the walker never
        // saw them.
        assert!(
            store.get_with_etag(&stage_key).await.is_ok(),
            "workflow stage entry was touched by bucket GC",
        );
        assert!(
            store.get_with_etag(&meta_key).await.is_ok(),
            "workflow experiment meta was touched by bucket GC",
        );
        assert!(
            store.get_with_etag(&stage_refs_key).await.is_ok(),
            "workflow experiment stage-refs blob was touched by bucket GC",
        );
    }
}
