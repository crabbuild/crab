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
use futures_util::{StreamExt, TryStreamExt};
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cmd::gc::marks::{DurableMarkReader, DurableMarkWriter};
use crate::coordination::cas::cas_update_default;
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
        let registry = load_ref_registry(store, args.force).await?;
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

    // The registry and all reachability listings must be observed while the
    // exclusive bucket fence is held. A per-delete-batch fence would allow a
    // writer to publish a new registry root between planning and deletion.
    let sweep = crate::maintenance::GcSweepLease::acquire(store, GLOBAL_PREFIX, cancel).await?;
    let operation = async {
        let registry = load_ref_registry(store, args.force).await?;
        ensure_registry_complete_for_destructive_gc(&registry)?;
        ensure_active_active_bucket_gc_proof(&registry, coordinator_protected_repos)?;
        run_bucket_gc_under_maintenance(
            args,
            store,
            coordinator_protected_keys,
            &registry,
            cancel,
            Some(&sweep),
        )
        .await
    }
    .await;
    let release = sweep.release().await;
    match (operation, release) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
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
        let registry = load_ref_registry(store, args.force).await?;
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

    let sweep = crate::maintenance::GcSweepLease::acquire(store, GLOBAL_PREFIX, cancel).await?;
    let operation = async {
        let registry = load_ref_registry(store, args.force).await?;
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
            Some(&sweep),
        )
        .await
    }
    .await;
    let release = sweep.release().await;
    match (operation, release) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
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
        let registry = load_ref_registry(store, false).await?;
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
            if shard_meta.size > SHARD_REPAIR_BUDGET_BYTES {
                return Err(CrabError::Configuration {
                    key: "gc.repair_closures.memory_budget".to_owned(),
                    origin: format!(
                        "shard {hash_hex} is {} bytes, above the {}-byte repair budget; split or compact the shard before backfill",
                        shard_meta.size, SHARD_REPAIR_BUDGET_BYTES
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
                CrabError::Internal(
                    "shard closure body budget exceeds semaphore capacity".to_owned(),
                )
            })?;
            let _budget = body_budget
                .clone()
                .acquire_many_owned(units)
                .await
                .map_err(|_| CrabError::Cancelled)?;
            check_cancelled(cancel)?;
            let body = store
                .inner()
                .get(&shard_path)
                .await
                .map_err(CrabError::Storage)?
                .bytes()
                .await
                .map_err(CrabError::Storage)?;
            let closure = super::closure::build(&hash, body, shard_path.as_ref())?;
            if closure.content_size != shard_meta.size {
                return Err(CrabError::CorruptObject {
                    path: shard_path.to_string(),
                    reason: format!(
                        "shard body size {} does not match object metadata {}",
                        closure.content_size, shard_meta.size
                    ),
                });
            }
            let encoded =
                serde_json::to_vec(&closure).map_err(|error| CrabError::CorruptObject {
                    path: closure_path.to_string(),
                    reason: format!("failed to encode shard closure: {error}"),
                })?;
            super::closure::publish_encoded(store, &closure_path, bytes::Bytes::from(encoded))
                .await?;
            return Ok(true);
        }
        Err(error) => return Err(error),
    };
    let closure = super::closure::decode(&closure_body, &closure_path, &hash)?;
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

    let repo_shards = repository_referenced_shards(store, registry, args.list_concurrency).await?;
    let root_identity = bucket_root_identity(registry, &repo_shards, coordinator_protected_keys);
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
        journal.ensure_root_identity(&root_identity)?;
        Some(journal.state().phase)
    } else {
        None
    };

    let effective_grace = args.grace_period.max(MIN_GRACE_PERIOD);
    let now = match resume_phase {
        Some(super::journal::GcRunPhase::Planning) => {
            let run_id = args.resume_run_id.as_deref().ok_or_else(|| {
                CrabError::Internal("bucket GC planning resume lost its run id".to_owned())
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
        _ => SystemTime::now(),
    };
    let cutoff = now - effective_grace;

    let global_list_permits = Arc::new(Semaphore::new(args.list_concurrency.max(1)));
    // Keep global listings disjoint in memory. Closure candidates cannot be
    // classified until the shard listing has established the deletion set,
    // so listing both namespaces concurrently only increases peak RAM.
    let shard_listing = list_global_objects(
        store,
        "shards",
        args.list_profile,
        args.list_concurrency,
        Arc::clone(&global_list_permits),
        cancel,
    )
    .await?;
    outcome.list_requests = shard_listing.requests;
    outcome.list_parallelism = args.list_concurrency.max(1).min(shard_listing.parallelism);
    debug!(
        profile = args.list_profile.as_str(),
        shard_partitioned = shard_listing.partitioned,
        logical_list_streams = outcome.list_requests,
        "selected bucket-global listing strategy"
    );
    let shard_objects = shard_listing.objects;
    let existing_shards = shard_objects
        .iter()
        .map(|object| extract_hash_from_key(&object.location))
        .collect::<HashSet<_>>();
    let referenced_shards = repo_shards
        .values()
        .flat_map(|shards| shards.iter().cloned())
        .collect::<HashSet<_>>();
    info!(
        repos = registry.repos.len(),
        referenced_shards = referenced_shards.len(),
        "loaded ref-registry"
    );

    // Step 2: List shards, find unreferenced candidates.
    check_cancelled(cancel)?;
    let mut missing_referenced_shards = referenced_shards
        .difference(&existing_shards)
        .cloned()
        .collect::<Vec<_>>();
    missing_referenced_shards.sort();
    if let Some(missing) = missing_referenced_shards.first() {
        return Err(CrabError::CorruptObject {
            path: canonical_global_content_path("shards", &missing).to_string(),
            reason: format!(
                "ref-registry references {} missing shard object(s)",
                missing_referenced_shards.len()
            ),
        });
    }
    let ShardGcPartition {
        unreferenced: unreferenced_shards,
        referenced: referenced_shard_objects,
        protected_count: protected_shards,
    } = partition_shards_for_gc(
        shard_objects,
        &referenced_shards,
        coordinator_protected_keys,
    );

    let shard_candidates = filter_by_grace(unreferenced_shards, cutoff, args.force);
    debug!(
        shard_candidates = shard_candidates.len(),
        protected_shards, "unreferenced shards eligible for deletion"
    );

    let deletable_shards = shard_candidates
        .iter()
        .map(|object| extract_hash_from_key(&object.location))
        .collect::<HashSet<_>>();
    check_cancelled(cancel)?;
    let closure_listing = list_closure_objects(
        store,
        args.list_concurrency,
        Arc::clone(&global_list_permits),
        cancel,
    )
    .await?;
    outcome.list_requests = outcome
        .list_requests
        .saturating_add(closure_listing.requests);
    outcome.list_parallelism = outcome.list_parallelism.max(
        args.list_concurrency
            .max(1)
            .min(closure_listing.parallelism),
    );
    let closure_candidates = filter_by_grace(
        partition_closures_for_gc(
            closure_listing.objects,
            &referenced_shards,
            &deletable_shards,
            &existing_shards,
            coordinator_protected_keys,
        ),
        cutoff,
        args.force,
    );

    // Step 3: Download each referenced shard once, in parallel, and
    // extract both xorb hashes (for step 4) and file hashes (for step 5)
    // in a single pass so the shard objects are not downloaded twice.
    let ShardHashes {
        xorb_hashes: referenced_xorbs,
        file_hashes_by_shard,
    } = extract_hashes_from_shards(
        store,
        &referenced_shard_objects,
        args.list_concurrency,
        Arc::new(Semaphore::new(CLOSURE_READ_PARALLELISM)),
    )
    .await?;
    let referenced_file_hashes = file_hashes_by_shard
        .values()
        .map(HashSet::len)
        .sum::<usize>();
    check_cancelled(cancel)?;
    info!(
        referenced_xorbs = referenced_xorbs.len(),
        referenced_file_hashes, "computed referenced xorbs + file-index entries from shards"
    );

    // Step 4: List xorbs, find unreferenced candidates.
    check_cancelled(cancel)?;
    let xorb_listing = list_global_objects(
        store,
        "xorbs",
        args.list_profile,
        args.list_concurrency,
        Arc::clone(&global_list_permits),
        cancel,
    )
    .await?;
    outcome.list_requests = outcome.list_requests.saturating_add(xorb_listing.requests);
    outcome.list_parallelism = outcome
        .list_parallelism
        .max(args.list_concurrency.max(1).min(xorb_listing.parallelism));
    let xorb_partition = partition_xorbs_for_gc(
        xorb_listing.objects,
        &referenced_xorbs,
        coordinator_protected_keys,
    );
    let protected_xorbs = xorb_partition.protected_count;
    let unreferenced_xorbs = xorb_partition.unreferenced;
    let xorb_candidates = filter_by_grace(unreferenced_xorbs, cutoff, args.force);
    debug!(
        xorb_candidates = xorb_candidates.len(),
        protected_xorbs, "unreferenced xorbs eligible for deletion"
    );

    let referenced_file_hashes = file_hashes_by_shard
        .values()
        .flat_map(|files| files.iter().copied())
        .collect::<HashSet<_>>();
    let repo_prefixes = repo_shards.keys().cloned().collect::<Vec<_>>();
    outcome.file_index_deleted = gc_file_indexes(
        store,
        &repo_prefixes,
        &referenced_file_hashes,
        args.dry_run,
        args.list_concurrency,
    )
    .await?;

    // Step 6: Delete or report.
    check_cancelled(cancel)?;
    if args.dry_run {
        delete_or_report(
            store,
            "shards",
            &shard_candidates,
            true,
            args.delete_concurrency,
            &mut outcome,
        )
        .await?;
        delete_or_report(
            store,
            "xorbs",
            &xorb_candidates,
            true,
            args.delete_concurrency,
            &mut outcome,
        )
        .await?;
        delete_or_report(
            store,
            "closures",
            &closure_candidates,
            true,
            args.delete_concurrency,
            &mut outcome,
        )
        .await?;
    } else {
        let candidates = shard_candidates
            .into_iter()
            .chain(xorb_candidates)
            .chain(closure_candidates)
            .map(|object| super::ObjectMeta {
                key: object.location,
                size: object.size,
                last_modified: object.last_modified,
                storage_class: None,
                transitioned_at: None,
            })
            .collect();
        run_bucket_object_gc(
            args,
            store,
            candidates,
            cancel,
            &mut outcome,
            &root_identity,
            now,
            sweep_lease,
        )
        .await?;
    }

    outcome.log();
    Ok(outcome)
}

/// Destructive bucket planning path. It creates the journal before any global
/// listing and feeds candidates into bounded batches as each object arrives;
/// process death during planning therefore leaves a durable, non-executable
/// run that can be replayed after root validation.
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
                "bucket",
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
        outcome.file_index_deleted =
            gc_file_indexes_partitioned(store, &repo_prefixes, &mut file_reader, false).await?;
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

    journal.finish_plan().await?;
    if !journal.file_index_complete() {
        let mut file_reader = DurableMarkReader::new_hash_width(
            store.clone(),
            journal.marks_prefix(),
            "referenced-files",
            4,
        );
        outcome.file_index_deleted =
            gc_file_indexes_partitioned(store, &repo_prefixes, &mut file_reader, false).await?;
        journal.mark_file_index_complete().await?;
    }
    execute_bucket_journal(args, store, &mut journal, cancel, outcome, sweep_lease).await?;
    outcome.log();
    Ok(outcome.clone())
}

async fn run_bucket_object_gc(
    args: &BucketGcArgs,
    store: &Store,
    candidates: Vec<super::ObjectMeta>,
    cancel: &CancellationToken,
    outcome: &mut BucketGcOutcome,
    root_identity: &str,
    snapshot_at: SystemTime,
    sweep_lease: Option<&crate::maintenance::GcSweepLease>,
) -> Result<()> {
    let mut journal = match args.resume_run_id.as_deref() {
        Some(run_id) => {
            super::journal::GcRunJournal::resume(
                store.clone(),
                GLOBAL_PREFIX,
                run_id,
                "bucket",
                GLOBAL_PREFIX,
            )
            .await?
        }
        None => {
            let mut journal = super::journal::GcRunJournal::start(
                store.clone(),
                GLOBAL_PREFIX,
                "bucket",
                GLOBAL_PREFIX,
                snapshot_at,
                args.grace_period,
                args.force,
            )
            .await?;
            journal.set_root_identity(root_identity).await?;
            journal.plan(&candidates).await?;
            journal
        }
    };
    if args.resume_run_id.is_some() {
        journal.ensure_policy(args.grace_period, args.force)?;
        journal.ensure_root_identity(root_identity)?;
        if journal.state().phase == super::journal::GcRunPhase::Planning {
            journal.reset_partial_plan().await?;
            journal.plan(&candidates).await?;
        }
    }

    execute_bucket_journal(args, store, &mut journal, cancel, outcome, sweep_lease).await
}

async fn execute_bucket_journal(
    args: &BucketGcArgs,
    store: &Store,
    journal: &mut super::journal::GcRunJournal,
    cancel: &CancellationToken,
    outcome: &mut BucketGcOutcome,
    sweep_lease: Option<&crate::maintenance::GcSweepLease>,
) -> Result<()> {
    loop {
        check_cancelled(cancel)?;
        // Destructive callers hold the bucket sweep from registry snapshot
        // through the final journal commit. The fallback is retained for
        // direct internal callers that do not already own that lease.
        let lease = if sweep_lease.is_some() {
            None
        } else {
            Some(crate::maintenance::GcSweepLease::acquire(store, GLOBAL_PREFIX, cancel).await?)
        };
        let Some(objects) = journal.next_batch().await? else {
            if let Some(lease) = lease {
                lease.release().await?;
            }
            break;
        };
        let results = futures_util::stream::iter(objects.iter().cloned())
            .map(|object| async move {
                let result = store.delete(&ObjectPath::from(object.key.as_str())).await;
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
                Ok(()) | Err(CrabError::NotFound { .. }) => {
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
        let journal_result = journal.complete_batch(&deleted_keys, batch_bytes).await;
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
    for repo_prefix in &repo_prefixes {
        if let Some(shards) = registry.repos.get(repo_prefix) {
            for hash in shards {
                MerkleHash::from_hex(hash).map_err(|error| CrabError::CorruptObject {
                    path: ".crab/ref-registry".to_owned(),
                    reason: format!("invalid current shard hash for {repo_prefix}: {error}"),
                })?;
                digest.add("current-shard", &format!("{repo_prefix}\0{hash}"));
            }
        }
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
    let mut repo_prefixes = registry.repos.keys().cloned().collect::<Vec<_>>();
    repo_prefixes.sort_unstable();
    for repo_prefix in repo_prefixes {
        if let Some(shards) = registry.repos.get(&repo_prefix) {
            for hash in shards {
                MerkleHash::from_hex(hash).map_err(|error| CrabError::CorruptObject {
                    path: ".crab/ref-registry".to_owned(),
                    reason: format!("invalid current shard hash for {repo_prefix}: {error}"),
                })?;
                marks.lock().await.add(hash).await?;
            }
        }
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
    let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
    match store.get_with_etag(&path).await {
        Ok((body, _etag)) => {
            let registry: RefRegistry =
                serde_json::from_slice(&body).map_err(|e| CrabError::CorruptObject {
                    path: format!("{GLOBAL_PREFIX}/ref-registry"),
                    reason: format!("invalid JSON: {e}"),
                })?;
            Ok(registry)
        }
        Err(CrabError::NotFound { .. }) => {
            if force {
                warn!("ref-registry not found; --force specified, treating as incomplete");
                let mut registry = RefRegistry::default();
                registry.schema_version = 0;
                Ok(registry)
            } else {
                Err(CrabError::NotFound {
                    path: format!(
                        "{GLOBAL_PREFIX}/ref-registry (use --force to proceed without registry)"
                    ),
                })
            }
        }
        Err(e) => Err(e),
    }
}

/// Metadata for a listed object.
#[derive(Debug, Clone)]
struct ListedObject {
    location: String,
    size: u64,
    last_modified: SystemTime,
}

fn bucket_root_identity(
    registry: &RefRegistry,
    repo_shards: &HashMap<String, HashSet<String>>,
    coordinator_protected_keys: &HashSet<String>,
) -> String {
    let mut records = vec![format!(
        "registry:{}:{}:{}",
        registry.schema_version, registry.generation, registry.coverage_complete
    )];
    let mut repos = registry.repos.keys().collect::<Vec<_>>();
    repos.sort_unstable();
    for repo in repos {
        let mut shards = registry.repos.get(repo).cloned().unwrap_or_default();
        shards.sort_unstable();
        records.push(format!("registry-repo:{repo}:{shards:?}"));
    }
    let mut workflow_repos = registry
        .workflow_stage_hashes
        .keys()
        .chain(registry.workflow_experiment_ids.keys())
        .collect::<Vec<_>>();
    workflow_repos.sort_unstable();
    workflow_repos.dedup();
    for repo in workflow_repos {
        let mut stages = registry
            .workflow_stage_hashes
            .get(repo)
            .cloned()
            .unwrap_or_default();
        let mut experiments = registry
            .workflow_experiment_ids
            .get(repo)
            .cloned()
            .unwrap_or_default();
        stages.sort_unstable();
        experiments.sort_unstable();
        records.push(format!("workflow:{repo}:{stages:?}:{experiments:?}"));
    }
    let mut shard_repos = repo_shards.keys().collect::<Vec<_>>();
    shard_repos.sort_unstable();
    for repo in shard_repos {
        let mut shards = repo_shards.get(repo).cloned().unwrap_or_default();
        let mut shards = shards.drain().collect::<Vec<_>>();
        shards.sort_unstable();
        records.push(format!("history:{repo}:{shards:?}"));
    }
    let mut protected = coordinator_protected_keys.iter().collect::<Vec<_>>();
    protected.sort_unstable();
    records.extend(protected.into_iter().map(|key| format!("protected:{key}")));
    let mut hasher = blake3::Hasher::new();
    for record in records {
        hasher.update(record.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

struct ShardGcPartition {
    unreferenced: Vec<ListedObject>,
    referenced: Vec<ListedObject>,
    protected_count: usize,
}

fn partition_shards_for_gc(
    shard_objects: Vec<ListedObject>,
    referenced_shards: &HashSet<String>,
    coordinator_protected_keys: &HashSet<String>,
) -> ShardGcPartition {
    let mut unreferenced = Vec::new();
    let mut referenced = Vec::new();
    let mut protected_count = 0;

    for obj in shard_objects {
        if coordinator_protected_keys.contains(&obj.location) {
            protected_count += 1;
            referenced.push(obj);
            continue;
        }
        let hash = extract_hash_from_key(&obj.location);
        if referenced_shards.contains(&hash) {
            referenced.push(obj);
        } else {
            unreferenced.push(obj);
        }
    }

    ShardGcPartition {
        unreferenced,
        referenced,
        protected_count,
    }
}

struct XorbGcPartition {
    unreferenced: Vec<ListedObject>,
    protected_count: usize,
}

fn partition_xorbs_for_gc(
    xorb_objects: Vec<ListedObject>,
    referenced_xorbs: &HashSet<String>,
    coordinator_protected_keys: &HashSet<String>,
) -> XorbGcPartition {
    let mut unreferenced = Vec::new();
    let mut protected_count = 0;

    for obj in xorb_objects {
        if coordinator_protected_keys.contains(&obj.location) {
            protected_count += 1;
            continue;
        }
        let hash = extract_hash_from_key(&obj.location);
        if !referenced_xorbs.contains(&hash) {
            unreferenced.push(obj);
        }
    }

    XorbGcPartition {
        unreferenced,
        protected_count,
    }
}

fn partition_closures_for_gc(
    closure_objects: Vec<ListedObject>,
    referenced_shards: &HashSet<String>,
    deletable_shards: &HashSet<String>,
    existing_shards: &HashSet<String>,
    coordinator_protected_keys: &HashSet<String>,
) -> Vec<ListedObject> {
    closure_objects
        .into_iter()
        .filter(|object| {
            if coordinator_protected_keys.contains(&object.location) {
                return false;
            }
            let hash = extract_hash_from_key(&object.location)
                .strip_suffix(".json")
                .unwrap_or_default()
                .to_owned();
            !referenced_shards.contains(&hash)
                && (deletable_shards.contains(&hash) || !existing_shards.contains(&hash))
        })
        .collect()
}

struct GlobalListOutcome {
    objects: Vec<ListedObject>,
    /// Logical list streams. Provider pagination and retries are internal to
    /// `object_store` and are not counted here.
    requests: u64,
    parallelism: usize,
    partitioned: bool,
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
    _concurrency: usize,
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
    // The streaming path intentionally processes one partition at a time so
    // consumer state and journal batches remain bounded without a shared
    // async mutex. Report the actual, rather than configured, parallelism.
    let parallelism = 1;
    let partition_count = partitions.len() as u64;
    let mut objects = 0u64;
    for partition in partitions {
        let partition_prefix = global_content_partition_prefix(GLOBAL_PREFIX, kind, &partition);
        let (_, count) = scan_global_prefix(
            store,
            kind,
            &partition_prefix,
            None,
            Arc::clone(&permits),
            cancel,
            consumer,
        )
        .await?;
        objects = objects.saturating_add(count);
    }
    Ok(GlobalScanStats {
        objects,
        requests: 1 + partition_count,
        parallelism,
        partitioned: true,
    })
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

/// List a global namespace using the selected cost/latency policy.
async fn list_global_objects(
    store: &Store,
    kind: &str,
    profile: GcListProfile,
    concurrency: usize,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Result<GlobalListOutcome> {
    let prefix = global_content_prefix(GLOBAL_PREFIX, kind);
    match profile {
        GcListProfile::Cost => {
            list_global_prefix(store, kind, &prefix, None, permits, cancel).await
        }
        GcListProfile::Latency => {
            list_global_partitions(store, kind, concurrency, permits, cancel).await
        }
        GcListProfile::Adaptive if concurrency <= 1 => {
            list_global_prefix(store, kind, &prefix, None, permits, cancel).await
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
                return Ok(probe);
            }

            let probe_requests = probe.requests;
            drop(probe);
            let mut partitioned =
                list_global_partitions(store, kind, concurrency, permits, cancel).await?;
            partitioned.requests += probe_requests;
            Ok(partitioned)
        }
    }
}

async fn list_closure_objects(
    store: &Store,
    _concurrency: usize,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Result<GlobalListOutcome> {
    let _permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        permit = permits.acquire() => {
            permit.map_err(|_| CrabError::Internal("closure LIST semaphore closed".to_owned()))?
        }
    };
    let prefix = format!("{GLOBAL_PREFIX}/gc/closures/");
    let mut stream = store.inner().list(Some(&ObjectPath::from(prefix.as_str())));
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
        objects.push(ListedObject {
            location,
            size: meta.size,
            last_modified: meta.last_modified.into(),
        });
    }
    Ok(GlobalListOutcome {
        objects,
        requests: 1,
        parallelism: 1,
        partitioned: false,
    })
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
        });
        if max_objects.is_some_and(|limit| objects.len() > limit) {
            break;
        }
    }

    Ok(GlobalListOutcome {
        objects,
        requests: 1,
        parallelism: 1,
        partitioned: false,
    })
}

/// Discover populated hash partitions, then scan them with bounded concurrency.
async fn list_global_partitions(
    store: &Store,
    kind: &str,
    concurrency: usize,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Result<GlobalListOutcome> {
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
    let batches =
        futures_util::stream::iter(partitions.iter().map(|partition| {
            let partition_prefix = global_content_partition_prefix(GLOBAL_PREFIX, kind, partition);
            let permits = Arc::clone(&permits);
            async move {
                list_global_prefix(store, kind, &partition_prefix, None, permits, cancel).await
            }
        }))
        .buffer_unordered(concurrency.max(1))
        .try_collect::<Vec<_>>()
        .await?;
    let objects = batches
        .into_iter()
        .flat_map(|batch| batch.objects)
        .collect::<Vec<_>>();

    Ok(GlobalListOutcome {
        objects,
        requests: 1 + partitions.len() as u64,
        parallelism,
        partitioned: true,
    })
}

/// Extract the hash portion from a canonical global content key.
fn extract_hash_from_key(key: &str) -> String {
    key.rsplit('/').next().unwrap_or("").to_string()
}

/// Filter objects by age unless the operator explicitly bypassed grace.
fn filter_by_grace(
    objects: Vec<ListedObject>,
    cutoff: SystemTime,
    force: bool,
) -> Vec<ListedObject> {
    if force {
        return objects;
    }
    objects
        .into_iter()
        .filter(|obj| obj.last_modified < cutoff)
        .collect()
}

/// Hashes extracted from a batch of shards.
struct ShardHashes {
    xorb_hashes: HashSet<String>,
    file_hashes_by_shard: HashMap<String, HashSet<MerkleHash>>,
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
    let (closure_body, _) = store.get_with_etag(&closure_path).await.map_err(|error| {
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
    let closure = super::closure::decode(&closure_body, &closure_path, &hash)?;
    if closure.content_size != object.size {
        return Err(CrabError::CorruptObject {
            path: closure_path.to_string(),
            reason: format!(
                "shard closure size {} does not match listed shard size {}",
                closure.content_size, object.size
            ),
        });
    }
    let xorbs = closure.xorb_hashes.into_iter().collect::<HashSet<_>>();
    let files = closure
        .file_hashes
        .into_iter()
        .map(|file_hash| {
            MerkleHash::from_hex(&file_hash).map_err(|error| CrabError::CorruptObject {
                path: closure_path.to_string(),
                reason: format!("invalid file hash in shard closure: {error}"),
            })
        })
        .collect::<Result<HashSet<_>>>()?;
    Ok((hash_hex, xorbs, files))
}

/// Read each referenced shard's durable closure.
///
/// Destructive bucket GC never derives a closure from the shard body. Older
/// repositories must run the explicit closure-repair command first; keeping
/// that migration outside the delete path makes missing coverage fail closed.
async fn extract_hashes_from_shards(
    store: &Store,
    shard_objects: &[ListedObject],
    concurrency: usize,
    closure_budget: Arc<Semaphore>,
) -> Result<ShardHashes> {
    let (xorb_hashes, file_hashes_by_shard) = futures_util::stream::iter(shard_objects.iter())
        .map(|obj| {
            let closure_budget = Arc::clone(&closure_budget);
            async move {
                extract_hashes_from_shard(store, obj, Some(closure_budget.as_ref())).await
            }
        })
        .buffer_unordered(concurrency.max(1))
        .try_fold(
            (HashSet::new(), HashMap::new()),
            |(mut xorb_hashes, mut file_hashes_by_shard), (shard_hash, x, f)| async move {
                xorb_hashes.extend(x);
                file_hashes_by_shard.insert(shard_hash, f);
                Ok::<_, CrabError>((xorb_hashes, file_hashes_by_shard))
            },
        )
        .await?;

    Ok(ShardHashes {
        xorb_hashes,
        file_hashes_by_shard,
    })
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
            let (_shard_hash, xorbs, files) =
                extract_hashes_from_shard(self.store, &object, None).await?;
            for xorb in xorbs {
                self.referenced_xorbs.add(&xorb).await?;
            }
            for file in files {
                self.referenced_files.add(&file.hex()).await?;
            }
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
    referenced_shards: DurableMarkReader,
    deletable_shards: DurableMarkReader,
    existing_shards: DurableMarkReader,
    coordinator_protected_keys: &'a HashSet<String>,
    cutoff: SystemTime,
    force: bool,
    sink: CandidateBatchSink<'a>,
}

impl<'a> ClosureStreamingPlanner<'a> {
    fn new(
        referenced_shards: DurableMarkReader,
        deletable_shards: DurableMarkReader,
        existing_shards: DurableMarkReader,
        coordinator_protected_keys: &'a HashSet<String>,
        cutoff: SystemTime,
        force: bool,
        journal: &'a mut super::journal::GcRunJournal,
    ) -> Self {
        Self {
            referenced_shards,
            deletable_shards,
            existing_shards,
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
impl GlobalObjectConsumer for ClosureStreamingPlanner<'_> {
    async fn consume(&mut self, object: ListedObject) -> Result<()> {
        if self.coordinator_protected_keys.contains(&object.location) {
            return Ok(());
        }
        let hash = extract_hash_from_key(&object.location)
            .strip_suffix(".json")
            .unwrap_or_default()
            .to_owned();
        if self.referenced_shards.contains(&hash).await?
            || (!self.deletable_shards.contains(&hash).await?
                && self.existing_shards.contains(&hash).await?)
        {
            return Ok(());
        }
        if self.force || object.last_modified < self.cutoff {
            self.sink.push(&object).await?;
        }
        Ok(())
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

async fn gc_file_indexes(
    store: &Store,
    repo_prefixes: &[String],
    referenced_file_hashes: &HashSet<MerkleHash>,
    dry_run: bool,
    concurrency: usize,
) -> Result<u64> {
    futures_util::stream::iter(repo_prefixes.iter())
        .map(|repo_prefix| async move {
            let db_prefix = ObjectPath::from(format!(
                "{}/file_index_db/",
                repo_prefix.trim_end_matches('/')
            ));
            let mut objects = store.inner().list(Some(&db_prefix));
            match objects.next().await {
                None => return Ok(0),
                Some(Err(error)) => return Err(CrabError::Storage(error)),
                Some(Ok(_)) => {}
            }

            let config =
                crate::metadata::MetaDbConfig::for_repo(repo_prefix).with_read_only(dry_run);
            let metadb = crate::metadata::MetaDb::new(
                Arc::clone(store.inner()),
                repo_prefix.clone(),
                config,
            );
            let guard = crate::metadata::MetaDbGuard::new(metadb);
            let operation = async {
                guard
                    .file_index()
                    .await?
                    .gc_unreferenced_committed(
                        referenced_file_hashes,
                        dry_run,
                        FILE_INDEX_GC_BATCH_SIZE,
                    )
                    .await
            }
            .await;
            let close = guard.close().await;
            match (operation, close) {
                (Ok(removed), Ok(())) => Ok(removed),
                (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            }
        })
        .buffer_unordered(concurrency.max(1))
        .try_fold(0u64, |total, removed| async move {
            total
                .checked_add(removed)
                .ok_or_else(|| CrabError::Internal("file-index GC count overflow".to_owned()))
        })
        .await
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
                let count = file_index
                    .gc_unreferenced_committed_prefix(
                        &prefix,
                        &referenced,
                        dry_run,
                        FILE_INDEX_GC_BATCH_SIZE,
                    )
                    .await?;
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

/// Delete or report candidates depending on dry-run mode.
async fn delete_or_report(
    store: &Store,
    kind: &str,
    candidates: &[ListedObject],
    dry_run: bool,
    concurrency: usize,
    outcome: &mut BucketGcOutcome,
) -> Result<()> {
    let deleted = futures_util::stream::iter(candidates.iter())
        .map(|obj| async move {
            let hash = extract_hash_from_key(&obj.location);
            if dry_run {
                info!(kind = %kind, hash = %hash, size = obj.size, "would delete (dry-run)");
                return Ok::<_, CrabError>(obj);
            }
            let path = ObjectPath::from(obj.location.as_str());
            match store.delete(&path).await {
                Ok(()) => debug!(kind = %kind, hash = %hash, "deleted"),
                Err(CrabError::NotFound { .. }) => {
                    debug!(kind = %kind, hash = %hash, "already deleted");
                }
                Err(error) => return Err(error),
            }
            Ok(obj)
        })
        .buffer_unordered(concurrency.max(1))
        .try_collect::<Vec<_>>()
        .await?;

    for obj in deleted {
        match kind {
            "shards" => outcome.shards_deleted += 1,
            "xorbs" => outcome.xorbs_deleted += 1,
            "file-index" => outcome.file_index_deleted += 1,
            _ => {}
        }
        outcome.bytes_reclaimed += obj.size;
    }
    Ok(())
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
        let registry_path = format!("{GLOBAL_PREFIX}/ref-registry");
        let updated: RefRegistry =
            cas_update_default(store, &registry_path, |reg: &mut RefRegistry| {
                let had_entry = reg.repos.contains_key(repo_prefix);
                reg.deregister(repo_prefix);
                reg.generation += 1;
                if had_entry {
                    info!(repo = %repo_prefix, generation = reg.generation, "deregistered repo");
                } else {
                    warn!(repo = %repo_prefix, "repo not found in ref-registry");
                }
            })
            .await?;

        info!(
            generation = updated.generation,
            remaining_repos = updated.repos.len(),
            "ref-registry updated"
        );
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
    let lease =
        crate::maintenance::GcGlobalWriterLease::acquire(store, GLOBAL_PREFIX, &cancel).await?;
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

    #[test]
    fn filter_by_grace_retains_recent() {
        let cutoff = SystemTime::now() - Duration::from_secs(3600);
        let recent = ListedObject {
            location: ".crab/shards/abc".to_string(),
            size: 100,
            last_modified: SystemTime::now(),
        };
        let result = filter_by_grace(vec![recent], cutoff, false);
        assert!(result.is_empty());
    }

    #[test]
    fn filter_by_grace_passes_old() {
        let cutoff = SystemTime::now() - Duration::from_secs(3600);
        let old = ListedObject {
            location: ".crab/shards/abc".to_string(),
            size: 100,
            last_modified: SystemTime::now() - Duration::from_secs(7200),
        };
        let result = filter_by_grace(vec![old], cutoff, false);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn filter_by_grace_force_includes_recent() {
        let cutoff = SystemTime::now() - Duration::from_secs(3600);
        let recent = ListedObject {
            location: ".crab/xorbs/recent".to_owned(),
            size: 100,
            last_modified: SystemTime::now(),
        };

        let result = filter_by_grace(vec![recent], cutoff, true);

        assert_eq!(result.len(), 1);
    }

    #[test]
    fn closure_gc_keeps_live_and_grace_retained_sources() {
        let live = "a".repeat(64);
        let retained = "b".repeat(64);
        let orphan = "c".repeat(64);
        let objects = [live.as_str(), retained.as_str(), orphan.as_str()]
            .into_iter()
            .map(|hash| ListedObject {
                location: format!(".crab/gc/closures/{hash}.json"),
                size: 1,
                last_modified: std::time::UNIX_EPOCH,
            })
            .collect();
        let candidates = partition_closures_for_gc(
            objects,
            &HashSet::from([live.clone()]),
            &HashSet::from([orphan.clone()]),
            &HashSet::from([live.clone(), retained.clone()]),
            &HashSet::new(),
        );
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].location.contains(&orphan));
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
        let body = serde_json::to_vec(&reg).unwrap();
        let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&path, Bytes::from(body)).await.unwrap();

        let loaded = load_ref_registry(&store, false).await.unwrap();
        assert_eq!(loaded.generation, 5);
        assert_eq!(loaded.repos.len(), 1);
    }

    #[tokio::test]
    async fn deregister_creates_registry_if_missing() {
        let store = memory_store();
        deregister_repo(&store, "org/old-repo").await.unwrap();

        let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        let (body, _) = store.get_with_etag(&path).await.unwrap();
        let reg: RefRegistry = serde_json::from_slice(&body).unwrap();
        assert_eq!(reg.generation, 1);
        assert!(reg.repos.is_empty());
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
        let body = serde_json::to_vec(&reg).unwrap();
        let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&path, Bytes::from(body)).await.unwrap();

        deregister_repo(&store, "org/models").await.unwrap();

        let (body, _) = store.get_with_etag(&path).await.unwrap();
        let updated: RefRegistry = serde_json::from_slice(&body).unwrap();
        assert_eq!(updated.generation, 4);
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
        // Put an empty registry so GC can proceed.
        let reg = RefRegistry::default();
        let body = serde_json::to_vec(&reg).unwrap();
        let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&path, Bytes::from(body)).await.unwrap();

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
    }

    #[tokio::test]
    async fn destructive_bucket_gc_preserves_recent_xorb_without_force() {
        let store = memory_store();
        let mut registry = RefRegistry::default();
        registry.mark_coverage_complete();
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store
            .put(
                &registry_path,
                Bytes::from(serde_json::to_vec(&registry).unwrap()),
            )
            .await
            .unwrap();
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
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store
            .put(
                &registry_path,
                Bytes::from(serde_json::to_vec(&registry).unwrap()),
            )
            .await
            .unwrap();
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
        let removed = gc_file_indexes_partitioned(&store, &[repo.to_owned()], &mut reader, false)
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
        store
            .put(
                &ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry")),
                Bytes::from(serde_json::to_vec(&registry).unwrap()),
            )
            .await
            .unwrap();
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
        store
            .put(
                &ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry")),
                Bytes::from(serde_json::to_vec(&registry).unwrap()),
            )
            .await
            .unwrap();

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
        store
            .put(
                &ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry")),
                Bytes::from(serde_json::to_vec(&registry).unwrap()),
            )
            .await
            .unwrap();

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
        let body = serde_json::to_vec(&reg).unwrap();
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&registry_path, Bytes::from(body)).await.unwrap();
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
        let body = serde_json::to_vec(&reg).unwrap();
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&registry_path, Bytes::from(body)).await.unwrap();
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

    #[test]
    fn bucket_gc_excludes_coordinator_protected_shared_objects() {
        let now = SystemTime::now();
        let shard_objects = vec![
            ListedObject {
                location: ".crab/shards/protected".to_owned(),
                size: 10,
                last_modified: now,
            },
            ListedObject {
                location: ".crab/shards/free".to_owned(),
                size: 20,
                last_modified: now,
            },
        ];
        let xorb_objects = vec![
            ListedObject {
                location: ".crab/xorbs/protected".to_owned(),
                size: 30,
                last_modified: now,
            },
            ListedObject {
                location: ".crab/xorbs/free".to_owned(),
                size: 40,
                last_modified: now,
            },
        ];
        let referenced = HashSet::new();
        let protected: HashSet<String> = [".crab/shards/protected", ".crab/xorbs/protected"]
            .into_iter()
            .map(str::to_owned)
            .collect();

        let shard_partition = partition_shards_for_gc(shard_objects, &referenced, &protected);
        assert_eq!(shard_partition.protected_count, 1);
        assert_eq!(shard_partition.unreferenced.len(), 1);
        assert_eq!(
            shard_partition.unreferenced[0].location,
            ".crab/shards/free"
        );
        assert_eq!(shard_partition.referenced.len(), 1);
        assert_eq!(
            shard_partition.referenced[0].location,
            ".crab/shards/protected"
        );

        let xorb_partition = partition_xorbs_for_gc(xorb_objects, &referenced, &protected);
        assert_eq!(xorb_partition.protected_count, 1);
        assert_eq!(xorb_partition.unreferenced.len(), 1);
        assert_eq!(xorb_partition.unreferenced[0].location, ".crab/xorbs/free");
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
        let body = serde_json::to_vec(&reg).unwrap();
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&registry_path, Bytes::from(body)).await.unwrap();

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
