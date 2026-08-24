//! `crab gc` — garbage collection for the remote object store.
//!
//! GC flow:
//! 1. Snapshot refs AND shard-list generation at T0
//! 2. Walk reachable set via `git::walk::walk_reachable`
//! 3. Enumerate storage candidates (prefix-sharded parallel LIST)
//! 4. Compute unreachable = listed − reachable (excluding post-T0 shards)
//! 5. Grace-period filter: retain objects with `last_modified >= T0 − grace`
//! 6. `--dry-run` → report and exit; otherwise parallel deletes + manifest CAS
//! 7. Structured outcome logging
//!
//! The `CancellationToken` is checked between sweep phases so that SIGINT
//! causes a clean exit without partial manifest corruption.

pub mod bucket;
pub mod class_aware;
pub mod closure;
pub mod inventory;
pub mod journal;
pub mod marks;
pub mod parallel_enum;

use std::collections::HashSet;
use std::future::Future;
use std::io::Stdout;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use futures_util::{StreamExt, TryStreamExt};
use object_store::path::Path as ObjectPath;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::event_payloads::{FileDonePayload, WarningPayload};
use crate::core::output::{JsonlStream, OutputMode};
use crate::storage::StoreLayout;
use crate::storage::store::Store;
use crate::tier::classes::StorageClass;

const REPO_GC_PREFIXES: &[&str] = &[
    "packs/",
    "metadata/",
    "manifests/",
    "workflow/artifacts/",
    "refs/crab/artifacts/",
    "workflow/stages/",
    "workflow/exp/",
    "workflow/xorbs/",
    "refs/crab/stages/",
    "refs/crab/exp/",
    "refs/crab/exp-meta/",
];
const DEFAULT_DELETE_CONCURRENCY: usize = 64;
const DEFAULT_LIST_CONCURRENCY: usize = 32;

// ---------------------------------------------------------------------------
// GC arguments
// ---------------------------------------------------------------------------

/// CLI arguments for `crab gc`.
#[derive(Debug, Clone)]
pub struct GcArgs {
    /// List unreachable objects without deleting anything.
    pub dry_run: bool,
    /// Bypass the grace period — delete all unreachable objects regardless
    /// of age. Requires `yes` or interactive confirmation.
    pub force: bool,
    /// Skip interactive confirmation when `--force` is used.
    pub yes: bool,
    /// Output mode resolved from `--json` / `--jsonl` flags.
    pub mode: OutputMode,
    /// Delete objects even if they are within their storage class's minimum
    /// retention window. Requires `yes_really` as a safety gate.
    pub force_early_delete: bool,
    /// Confirm destructive operations that bypass safety guards
    /// (`--force-early-delete`).
    pub yes_really: bool,
    /// Maximum concurrent object-store DELETE requests.
    pub delete_concurrency: usize,
    /// Maximum concurrent object-store LIST and history-closure reads.
    pub list_concurrency: usize,
    /// Resume a durable destructive run by UUIDv7 run id.
    pub resume_run_id: Option<String>,
}

impl Default for GcArgs {
    fn default() -> Self {
        Self {
            dry_run: false,
            force: false,
            yes: false,
            mode: OutputMode::Text,
            force_early_delete: false,
            yes_really: false,
            delete_concurrency: DEFAULT_DELETE_CONCURRENCY,
            list_concurrency: DEFAULT_LIST_CONCURRENCY,
            resume_run_id: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Object metadata returned by storage enumeration
// ---------------------------------------------------------------------------

/// Metadata for a single object discovered during storage enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    /// Full storage key (e.g. `xorbs/ab/abcdef...`).
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// Last-modified timestamp from the object store.
    pub last_modified: SystemTime,
    /// Provider ETag observed while planning the candidate.
    pub e_tag: Option<String>,
    /// Provider object version observed while planning the candidate.
    pub version: Option<String>,
    /// Provider-native storage class, if known.
    ///
    /// Populated when `gc.class_aware = true`; `None` otherwise.
    pub storage_class: Option<StorageClass>,
    /// When the object was last transitioned to a different storage class.
    ///
    /// Populated when `gc.class_aware = true`; `None` otherwise.
    /// Falls back to `last_modified` when the provider does not expose a
    /// dedicated transition timestamp (e.g. S3).
    pub transitioned_at: Option<SystemTime>,
}

// ---------------------------------------------------------------------------
// GC outcome
// ---------------------------------------------------------------------------

/// Structured outcome of a GC run, used for logging and reporting.
#[derive(Debug, Clone, Default)]
pub struct GcOutcome {
    pub packs_deleted: u64,
    pub xorbs_deleted: u64,
    pub shards_deleted: u64,
    pub bytes_reclaimed: u64,
    pub list_requests: u64,
    pub list_parallelism: usize,
    pub list_wall_seconds: f64,
    /// `true` when the run was dry-run (no mutations).
    pub dry_run: bool,
    /// `true` when the run was cancelled mid-sweep.
    pub cancelled: bool,
    /// `true` when one or more LIST requests failed — enumeration was
    /// partial and GC may not have considered all objects. See S1-P5-1.
    pub partial_enumeration: bool,
    /// Number of object DELETE requests that failed.
    pub delete_failures: u64,
    /// Whether post-delete metadata reconciliation failed.
    pub reconciliation_failed: bool,
    pub active_pack_bytes: u64,
    pub retained_history_pack_bytes: u64,
    pub grace_period_pack_bytes: u64,
    pub collectible_pack_bytes: u64,
}

impl GcOutcome {
    fn log(&self) {
        if self.dry_run {
            info!(
                packs = self.packs_deleted,
                xorbs = self.xorbs_deleted,
                shards = self.shards_deleted,
                bytes = self.bytes_reclaimed,
                list_requests = self.list_requests,
                list_parallelism = self.list_parallelism,
                list_wall_secs = format!("{:.2}", self.list_wall_seconds),
                "gc dry-run complete (no objects deleted)"
            );
        } else if self.cancelled || self.delete_failures > 0 || self.reconciliation_failed {
            warn!(
                packs = self.packs_deleted,
                xorbs = self.xorbs_deleted,
                shards = self.shards_deleted,
                bytes = self.bytes_reclaimed,
                delete_failures = self.delete_failures,
                reconciliation_failed = self.reconciliation_failed,
                cancelled = self.cancelled,
                "gc incomplete — partial results"
            );
        } else {
            info!(
                packs = self.packs_deleted,
                xorbs = self.xorbs_deleted,
                shards = self.shards_deleted,
                bytes = self.bytes_reclaimed,
                list_requests = self.list_requests,
                list_parallelism = self.list_parallelism,
                list_wall_secs = format!("{:.2}", self.list_wall_seconds),
                "gc complete"
            );
        }
    }

    /// Convert to the structured output summary payload.
    pub fn to_summary(&self) -> GcSummary {
        GcSummary {
            packs_deleted: self.packs_deleted,
            xorbs_deleted: self.xorbs_deleted,
            shards_deleted: self.shards_deleted,
            file_index_entries_deleted: 0,
            bytes_reclaimed: self.bytes_reclaimed,
            dry_run: self.dry_run,
            cancelled: self.cancelled,
            partial_enumeration: self.partial_enumeration,
            delete_failures: self.delete_failures,
            reconciliation_failed: self.reconciliation_failed,
            active_pack_bytes: self.active_pack_bytes,
            retained_history_pack_bytes: self.retained_history_pack_bytes,
            grace_period_pack_bytes: self.grace_period_pack_bytes,
            collectible_pack_bytes: self.collectible_pack_bytes,
        }
    }
}

/// Metadata about the LIST phase, fed into the outcome.
#[derive(Debug, Clone, Default)]
pub struct ListOutcome {
    pub requests: u64,
    pub parallelism: usize,
    pub wall_seconds: f64,
    /// Prefixes whose LIST call failed. When non-empty, enumeration was
    /// partial — GC may not have considered all objects for collection.
    /// Each entry is `"{dimension}:{prefix}"`. See finding S1-P5-1.
    pub failed_prefixes: Vec<String>,
}

/// Terminal result payload for `--json` / `--jsonl` structured output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GcSummary {
    /// Number of pack objects deleted (or would-be-deleted in dry-run).
    pub packs_deleted: u64,
    /// Number of xorb objects deleted.
    pub xorbs_deleted: u64,
    /// Number of shard objects deleted.
    pub shards_deleted: u64,
    /// Number of stale per-repository file-index rows tombstoned.
    #[serde(default)]
    pub file_index_entries_deleted: u64,
    /// Total bytes reclaimed.
    pub bytes_reclaimed: u64,
    /// Whether this was a dry-run (no mutations).
    pub dry_run: bool,
    /// Whether the run was cancelled mid-sweep.
    pub cancelled: bool,
    /// Whether enumeration was partial (some LIST requests failed).
    /// When `true`, GC may not have considered all objects for
    /// collection; re-running GC after transient S3 issues clear is
    /// recommended. See finding S1-P5-1.
    #[serde(default)]
    pub partial_enumeration: bool,
    /// Number of object deletions that failed.
    #[serde(default)]
    pub delete_failures: u64,
    /// Whether post-delete metadata reconciliation failed.
    #[serde(default)]
    pub reconciliation_failed: bool,
    /// Current-manifest Git pack bytes.
    #[serde(default)]
    pub active_pack_bytes: u64,
    /// Pack bytes retained only by history, workflows, or other recovery roots.
    #[serde(default)]
    pub retained_history_pack_bytes: u64,
    /// Unreachable pack bytes retained by the grace period.
    #[serde(default)]
    pub grace_period_pack_bytes: u64,
    /// Unreachable pack bytes eligible for collection.
    #[serde(default)]
    pub collectible_pack_bytes: u64,
}

// ---------------------------------------------------------------------------
// Shard-list snapshot (T0 safety)
// ---------------------------------------------------------------------------

/// A snapshot of the shard-list generation taken at T0.
///
/// Shards added after this generation are excluded from the unreachable set
/// so that concurrent pushes between the walk and the sweep are safe.
#[derive(Debug, Clone)]
pub struct ShardListSnapshot {
    /// The generation counter at snapshot time.
    pub generation: u64,
    /// The set of shard keys present at T0.
    pub shard_keys: HashSet<String>,
}

// ---------------------------------------------------------------------------
// Grace-period filtering
// ---------------------------------------------------------------------------

/// Minimum allowed grace period (1 hour).
const MIN_GRACE_PERIOD: Duration = Duration::from_secs(3600);

/// Filter out objects that are within the grace period relative to T0.
///
/// Objects with `last_modified >= t0 - grace` are retained (not deleted).
/// When `force` is true, the grace period is bypassed entirely.
///
/// Returns the subset of `candidates` that are eligible for deletion.
#[must_use]
pub fn apply_grace_filter(
    candidates: Vec<ObjectMeta>,
    t0: SystemTime,
    grace: Duration,
    force: bool,
) -> Vec<ObjectMeta> {
    if force {
        return candidates;
    }

    let effective_grace = grace.max(MIN_GRACE_PERIOD);
    let cutoff = t0 - effective_grace;

    candidates
        .into_iter()
        .filter(|obj| obj.last_modified < cutoff)
        .collect()
}

/// Partition unreachable objects into those eligible for deletion and those
/// retained by the grace period.
///
/// Returns `(to_delete, grace_skipped)`.
#[must_use]
pub fn partition_grace_filter(
    candidates: &[ObjectMeta],
    t0: SystemTime,
    grace: Duration,
    force: bool,
) -> (Vec<ObjectMeta>, Vec<ObjectMeta>) {
    if force {
        return (candidates.to_vec(), Vec::new());
    }

    let effective_grace = grace.max(MIN_GRACE_PERIOD);
    let cutoff = t0 - effective_grace;

    let mut to_delete = Vec::new();
    let mut skipped = Vec::new();
    for obj in candidates {
        if obj.last_modified < cutoff {
            to_delete.push(obj.clone());
        } else {
            skipped.push(obj.clone());
        }
    }
    (to_delete, skipped)
}

/// Compute the set of unreachable objects from the listed candidates.
///
/// An object is unreachable if its key is NOT in the `reachable_keys` set
/// AND it was present in the shard-list snapshot at T0 (for shard-type
/// objects). Non-shard objects (xorbs, packs) are unreachable if simply
/// absent from the reachable set.
#[must_use]
pub fn compute_unreachable(
    listed: Vec<ObjectMeta>,
    reachable_keys: &HashSet<String, impl std::hash::BuildHasher>,
    coordinator_protected_keys: &HashSet<String, impl std::hash::BuildHasher>,
    shard_snapshot: &ShardListSnapshot,
) -> Vec<ObjectMeta> {
    listed
        .into_iter()
        .filter(|obj| {
            if reachable_keys.contains(&obj.key) {
                return false;
            }
            if coordinator_protected_keys.contains(&obj.key) {
                return false;
            }
            // For shard-prefixed objects, exclude those added after T0.
            if obj.key.starts_with("shards/") && !shard_snapshot.shard_keys.contains(&obj.key) {
                return false;
            }
            true
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Deletion tracking
// ---------------------------------------------------------------------------

fn categorize_key(key: &str) -> ObjectCategory {
    if key.starts_with("packs/") || key.contains("/packs/") {
        ObjectCategory::Pack
    } else if key.starts_with("xorbs/") || key.contains("/xorbs/") {
        ObjectCategory::Xorb
    } else if key.starts_with("shards/") || key.contains("/shards/") {
        ObjectCategory::Shard
    } else {
        ObjectCategory::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectCategory {
    Pack,
    Xorb,
    Shard,
    Other,
}

fn tally(category: ObjectCategory, outcome: &mut GcOutcome, size: u64) {
    match category {
        ObjectCategory::Pack => outcome.packs_deleted += 1,
        ObjectCategory::Xorb => outcome.xorbs_deleted += 1,
        ObjectCategory::Shard => outcome.shards_deleted += 1,
        ObjectCategory::Other => {}
    }
    outcome.bytes_reclaimed += size;
}

// ---------------------------------------------------------------------------
// Force-mode confirmation
// ---------------------------------------------------------------------------

/// Check `--force` preconditions: require `--yes` or interactive confirmation.
///
/// Returns `Ok(true)` if the user confirmed, `Ok(false)` if they declined.
fn confirm_force(args: &GcArgs) -> Result<bool> {
    use std::io::Write;

    if !args.force {
        return Ok(true);
    }

    warn!("--force bypasses the grace period; concurrent pushes may lose data");

    if args.yes {
        return Ok(true);
    }

    eprint!("Proceed with force GC? [y/N] ");
    std::io::stderr().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

// ---------------------------------------------------------------------------
// Deleter trait — abstraction over object store DELETE + manifest CAS
// ---------------------------------------------------------------------------

/// Trait abstracting the DELETE operation for testability.
///
/// In production, this wraps `store.delete(&path)` and the manifest CAS.
/// In tests, a mock tracks which keys were deleted.
pub trait ObjectDeleter: Send + Sync {
    /// Delete a single object by key.
    fn delete(
        &self,
        key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// Revalidates a durable candidate before deleting it.
    fn delete_candidate<'a>(
        &'a self,
        object: &'a ObjectMeta,
        _policy: DeletePolicy,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CandidateDelete>> + Send + 'a>>
    {
        Box::pin(async move {
            self.delete(&object.key).await?;
            Ok(CandidateDelete::Deleted)
        })
    }

    /// Perform manifest CAS to remove deleted entries.
    ///
    /// Called once after all deletes complete when [`Self::reconciliation_required`]
    /// is true. The `deleted_keys` are the keys that were successfully deleted.
    fn reconcile_manifest(
        &self,
        deleted_keys: &[String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// Whether the caller must retain deleted keys for reconciliation.
    ///
    /// Store-only deletion has no manifest to update, so durable sweeps can
    /// keep memory bounded by the journal batch size. Implementations that
    /// maintain a secondary index keep the default and receive the complete
    /// durable key set at the reconciliation boundary.
    fn reconciliation_required(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DeletePolicy {
    snapshot_at: SystemTime,
    grace_period: Duration,
    force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateDelete {
    Deleted,
    Retained,
}

/// A no-op deleter for dry-run mode and tests.
pub struct NullDeleter;

impl ObjectDeleter for NullDeleter {
    fn delete(
        &self,
        _key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn reconcile_manifest(
        &self,
        _deleted_keys: &[String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn reconciliation_required(&self) -> bool {
        false
    }
}

/// Object-store deleter used by production repo-scope remote GC.
pub struct StoreObjectDeleter {
    store: Store,
}

impl StoreObjectDeleter {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

impl ObjectDeleter for StoreObjectDeleter {
    fn delete(
        &self,
        key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        let store = self.store.clone();
        let path = ObjectPath::from(key.to_owned());
        Box::pin(async move { store.delete(&path).await })
    }

    fn reconcile_manifest(
        &self,
        _deleted_keys: &[String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn reconciliation_required(&self) -> bool {
        false
    }

    fn delete_candidate<'a>(
        &'a self,
        object: &'a ObjectMeta,
        policy: DeletePolicy,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CandidateDelete>> + Send + 'a>>
    {
        Box::pin(async move {
            let path = ObjectPath::from(object.key.clone());
            let current = match self.store.head(&path).await {
                Ok(current) => current,
                Err(CrabError::NotFound { .. }) => return Ok(CandidateDelete::Deleted),
                Err(error) => return Err(error),
            };
            let has_identity = object.e_tag.is_some() || object.version.is_some();
            if !has_identity {
                return Err(CrabError::Configuration {
                    key: "gc.object_identity".to_owned(),
                    origin: format!(
                        "provider returned no stable ETag or version for {}",
                        object.key
                    ),
                });
            }
            let identity_matches = object
                .e_tag
                .as_ref()
                .is_none_or(|e_tag| current.e_tag.as_ref() == Some(e_tag))
                && object
                    .version
                    .as_ref()
                    .is_none_or(|version| current.version.as_ref() == Some(version));
            let cutoff = policy.snapshot_at - policy.grace_period.max(MIN_GRACE_PERIOD);
            if !identity_matches
                || current.size != object.size
                || (!policy.force && SystemTime::from(current.last_modified) >= cutoff)
            {
                return Ok(CandidateDelete::Retained);
            }
            self.store.delete(&path).await?;
            Ok(CandidateDelete::Deleted)
        })
    }
}

/// List repo-local immutable objects that repo-scope remote GC may delete.
///
/// Shared `.crab/` dedup objects remain bucket-scoped because they need a
/// bucket-wide reachability proof across every registered repo.
pub async fn list_repo_gc_candidates(
    store: &Store,
    router: &StoreLayout,
) -> Result<(Vec<ObjectMeta>, ListOutcome)> {
    list_repo_gc_candidates_with_concurrency(store, router, DEFAULT_LIST_CONCURRENCY).await
}

async fn list_repo_gc_candidates_with_concurrency(
    store: &Store,
    router: &StoreLayout,
    concurrency: usize,
) -> Result<(Vec<ObjectMeta>, ListOutcome)> {
    let started = Instant::now();
    let parallelism = concurrency.max(1).min(REPO_GC_PREFIXES.len());
    let batches =
        futures_util::stream::iter(REPO_GC_PREFIXES.iter().copied().map(|prefix| async move {
            let object_prefix = router.repo_path(prefix);
            store
                .inner()
                .list(Some(&object_prefix))
                .try_collect()
                .await
                .map_err(CrabError::Storage)
        }))
        .buffer_unordered(parallelism)
        .try_collect::<Vec<Vec<object_store::ObjectMeta>>>()
        .await?;
    let candidates = batches
        .into_iter()
        .flatten()
        .map(|meta| ObjectMeta {
            key: meta.location.to_string(),
            size: meta.size,
            last_modified: meta.last_modified.into(),
            e_tag: meta.e_tag,
            version: meta.version,
            storage_class: None,
            transitioned_at: None,
        })
        .collect();

    Ok((
        candidates,
        ListOutcome {
            requests: REPO_GC_PREFIXES.len() as u64,
            parallelism,
            wall_seconds: started.elapsed().as_secs_f64(),
            failed_prefixes: Vec::new(),
        },
    ))
}

/// Streams repo-local LIST results directly into the durable candidate plan.
/// The old helper remains available to callers that need a preview vector;
/// destructive runs never retain the full candidate namespace in memory.
async fn plan_repo_gc_candidates_streaming(
    store: &Store,
    router: &StoreLayout,
    reachable_keys: &mut marks::DurableMarkReader,
    coordinator_protected_keys: &HashSet<String>,
    cancel: &CancellationToken,
    t0: SystemTime,
    grace_period: Duration,
    force: bool,
    journal: &mut journal::GcRunJournal,
) -> Result<ListOutcome> {
    let started = Instant::now();
    let cutoff = t0 - grace_period.max(MIN_GRACE_PERIOD);
    let mut batch = Vec::with_capacity(journal::DEFAULT_BATCH_SIZE);
    for prefix in REPO_GC_PREFIXES {
        check_cancelled(cancel)?;
        let object_prefix = router.repo_path(prefix);
        let mut objects = store.inner().list(Some(&object_prefix));
        while let Some(meta) = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
            next = objects.try_next() => next.map_err(CrabError::Storage)?,
        } {
            let object = ObjectMeta {
                key: meta.location.to_string(),
                size: meta.size,
                last_modified: meta.last_modified.into(),
                e_tag: meta.e_tag,
                version: meta.version,
                storage_class: None,
                transitioned_at: None,
            };
            if reachable_keys.contains(&object.key).await?
                || coordinator_protected_keys.contains(&object.key)
                || (!force && object.last_modified >= cutoff)
            {
                continue;
            }
            batch.push(object);
            if batch.len() == journal::DEFAULT_BATCH_SIZE {
                journal.append_candidates(&batch).await?;
                batch.clear();
            }
        }
    }
    if !batch.is_empty() {
        journal.append_candidates(&batch).await?;
    }
    journal.finish_plan().await?;
    Ok(ListOutcome {
        requests: REPO_GC_PREFIXES.len() as u64,
        parallelism: 1,
        wall_seconds: started.elapsed().as_secs_f64(),
        failed_prefixes: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Main GC entry point
// ---------------------------------------------------------------------------

/// Run garbage collection.
///
/// Orchestrates the full GC flow: compute unreachable → grace filter →
/// confirm force → dry-run or delete → manifest CAS → log outcome.
///
/// The `cancel` token is checked between phases. On cancellation, partial
/// results are logged and the function returns `Err(CrabError::Cancelled)`.
///
/// # Errors
///
/// Returns [`CrabError::Cancelled`] on SIGINT, or propagates storage errors.
#[expect(
    clippy::too_many_arguments,
    reason = "GC orchestrator needs all these inputs; a context struct would just move the problem"
)]
pub async fn run_gc(
    args: &GcArgs,
    listed_objects: Vec<ObjectMeta>,
    reachable_keys: &HashSet<String, impl std::hash::BuildHasher>,
    coordinator_protected_keys: &HashSet<String, impl std::hash::BuildHasher>,
    shard_snapshot: &ShardListSnapshot,
    cancel: &CancellationToken,
    delete_concurrency: usize,
    grace_period: Duration,
    list_outcome: ListOutcome,
    deleter: &dyn ObjectDeleter,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
) -> Result<GcOutcome> {
    run_gc_impl(
        args,
        listed_objects,
        reachable_keys,
        coordinator_protected_keys,
        shard_snapshot,
        cancel,
        delete_concurrency,
        grace_period,
        list_outcome,
        deleter,
        jsonl_stream,
        None,
        None,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "The durable GC execution seam keeps storage, policy, cancellation, and output explicit"
)]
async fn finish_repo_gc_from_marks(
    args: &GcArgs,
    store: &Store,
    router: &StoreLayout,
    mut journal: journal::GcRunJournal,
    t0: SystemTime,
    coordinator_protected_keys: &HashSet<String>,
    cancel: &CancellationToken,
    grace_period: Duration,
    deleter: &dyn ObjectDeleter,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
    sweep_lease: Option<&crate::maintenance::GcSweepLease>,
) -> Result<GcOutcome> {
    if journal.state().phase != journal::GcRunPhase::Planning {
        return Err(CrabError::Configuration {
            key: "gc.journal".to_owned(),
            origin: "durable repository GC can only seal a plan from the planning phase".to_owned(),
        });
    }
    let mut reachable_reader = marks::DurableMarkReader::new_keys(
        store.clone(),
        journal.marks_prefix(),
        "reachable-objects",
    );
    let list_outcome = plan_repo_gc_candidates_streaming(
        store,
        router,
        &mut reachable_reader,
        coordinator_protected_keys,
        cancel,
        t0,
        grace_period,
        args.force,
        &mut journal,
    )
    .await?;
    if sweep_lease.is_none() {
        let seal = crate::maintenance::GcSweepLease::acquire_for_run(
            store,
            router.repo_prefix(),
            &journal.state().run_id,
            cancel,
        )
        .await?;
        let roots = stream_repo_reachability(
            store,
            router,
            args.list_concurrency,
            coordinator_protected_keys,
            cancel,
            None,
        )
        .await;
        let sealed = match roots {
            Ok(roots) => {
                journal.ensure_root_identity(&roots.root_identity)?;
                journal.seal_fence_epoch(seal.epoch()).await
            }
            Err(error) => Err(error),
        };
        let release = seal.release().await;
        match (sealed, release) {
            (Ok(()), Ok(())) => {}
            (Err(error), _) | (Ok(()), Err(error)) => return Err(error),
        }
    }
    let mut outcome = GcOutcome {
        list_requests: list_outcome.requests,
        list_parallelism: list_outcome.parallelism,
        list_wall_seconds: list_outcome.wall_seconds,
        dry_run: false,
        partial_enumeration: !list_outcome.failed_prefixes.is_empty(),
        ..GcOutcome::default()
    };
    for key in journal.deleted_keys().await? {
        match categorize_key(&key) {
            ObjectCategory::Pack => outcome.packs_deleted = outcome.packs_deleted.saturating_add(1),
            ObjectCategory::Xorb => outcome.xorbs_deleted = outcome.xorbs_deleted.saturating_add(1),
            ObjectCategory::Shard => {
                outcome.shards_deleted = outcome.shards_deleted.saturating_add(1)
            }
            ObjectCategory::Other => {}
        }
    }
    outcome.bytes_reclaimed = journal.deleted_bytes_reclaimed().await?;
    check_cancelled(cancel)?;
    let delete_outcome = execute_journaled_deletes(
        &mut journal,
        cancel,
        args.delete_concurrency,
        deleter,
        &mut outcome,
        jsonl_stream,
        sweep_lease
            .is_none()
            .then_some((store, router.repo_prefix())),
    )
    .await;
    if cancel.is_cancelled() {
        outcome.cancelled = true;
        outcome.log();
        return Err(CrabError::Cancelled);
    }
    check_cancelled(cancel)?;
    outcome.delete_failures = delete_outcome.failure_count;
    let reconciliation_error =
        if delete_outcome.first_error.is_none() && deleter.reconciliation_required() {
            deleter
                .reconcile_manifest(&delete_outcome.deleted_keys)
                .await
                .err()
        } else {
            None
        };
    outcome.reconciliation_failed = reconciliation_error.is_some();
    if delete_outcome.first_error.is_none() && reconciliation_error.is_none() {
        journal.complete().await?;
    }
    outcome.log();
    if let Some(source) = delete_outcome.first_error {
        if matches!(&source, CrabError::PushLockHeld { .. }) {
            return Err(source);
        }
        return Err(CrabError::GcPartialFailure {
            objects_deleted: delete_outcome.deleted_count,
            delete_failures: outcome.delete_failures,
            reconciliation_failed: outcome.reconciliation_failed,
            source: Box::new(source),
        });
    }
    if let Some(source) = reconciliation_error {
        return Err(CrabError::GcPartialFailure {
            objects_deleted: delete_outcome.deleted_count,
            delete_failures: outcome.delete_failures,
            reconciliation_failed: outcome.reconciliation_failed,
            source: Box::new(source),
        });
    }
    Ok(outcome)
}

#[expect(
    clippy::too_many_arguments,
    reason = "The repository sweep boundary keeps the durable journal, root walk, policy, and lease explicit"
)]
async fn run_repo_gc_durable_streaming_roots(
    args: &GcArgs,
    store: &Store,
    router: &StoreLayout,
    coordinator_protected_keys: &HashSet<String>,
    cancel: &CancellationToken,
    grace_period: Duration,
    deleter: &dyn ObjectDeleter,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
    sweep_lease: Option<&crate::maintenance::GcSweepLease>,
) -> Result<GcOutcome> {
    if args.force && !confirm_force(args)? {
        return Ok(GcOutcome {
            dry_run: false,
            ..GcOutcome::default()
        });
    }

    let (mut journal, t0, planning) = match args.resume_run_id.as_deref() {
        Some(run_id) => {
            let journal = journal::GcRunJournal::resume(
                store.clone(),
                router.repo_prefix(),
                run_id,
                "repo",
                router.repo_prefix(),
            )
            .await?;
            journal.ensure_policy(grace_period, args.force)?;
            let t0 = journal.snapshot_at()?;
            let planning = journal.state().phase == journal::GcRunPhase::Planning;
            (journal, t0, planning)
        }
        None => {
            let t0 = SystemTime::now();
            let journal = journal::GcRunJournal::start(
                store.clone(),
                router.repo_prefix(),
                "repo",
                router.repo_prefix(),
                t0,
                grace_period,
                args.force,
            )
            .await?;
            (journal, t0, true)
        }
    };

    if !planning {
        let roots = stream_repo_reachability(
            store,
            router,
            args.list_concurrency,
            coordinator_protected_keys,
            cancel,
            None,
        )
        .await?;
        journal.ensure_root_identity(&roots.root_identity)?;
        return resume_gc_run(
            args,
            &mut journal,
            cancel,
            args.delete_concurrency,
            deleter,
            jsonl_stream,
            ListOutcome::default(),
            sweep_lease
                .is_none()
                .then_some((store, router.repo_prefix())),
        )
        .await;
    }

    if args.resume_run_id.is_some() {
        journal.reset_partial_plan().await?;
    }
    let mut reachable_marks = marks::DurableMarkWriter::new_keys(
        store.clone(),
        journal.marks_prefix(),
        "reachable-objects",
    );
    let roots = stream_repo_reachability(
        store,
        router,
        args.list_concurrency,
        coordinator_protected_keys,
        cancel,
        Some(&mut reachable_marks),
    )
    .await?;
    reachable_marks.finish().await?;
    if journal.state().root_identity.is_empty() {
        journal.set_root_identity(&roots.root_identity).await?;
    } else {
        journal.ensure_root_identity(&roots.root_identity)?;
    }
    finish_repo_gc_from_marks(
        args,
        store,
        router,
        journal,
        t0,
        coordinator_protected_keys,
        cancel,
        grace_period,
        deleter,
        jsonl_stream,
        sweep_lease,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "GC implementation keeps the existing testable seam while adding an optional journal"
)]
async fn run_gc_impl(
    args: &GcArgs,
    listed_objects: Vec<ObjectMeta>,
    reachable_keys: &HashSet<String, impl std::hash::BuildHasher>,
    coordinator_protected_keys: &HashSet<String, impl std::hash::BuildHasher>,
    shard_snapshot: &ShardListSnapshot,
    cancel: &CancellationToken,
    delete_concurrency: usize,
    grace_period: Duration,
    list_outcome: ListOutcome,
    deleter: &dyn ObjectDeleter,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
    mut journal: Option<&mut journal::GcRunJournal>,
    sweep: Option<(&Store, &str)>,
) -> Result<GcOutcome> {
    let t0 = SystemTime::now();
    let partial_enumeration = !list_outcome.failed_prefixes.is_empty();
    let mut outcome = GcOutcome {
        list_requests: list_outcome.requests,
        list_parallelism: list_outcome.parallelism,
        list_wall_seconds: list_outcome.wall_seconds,
        dry_run: args.dry_run,
        partial_enumeration,
        ..GcOutcome::default()
    };

    // Phase 1: Compute unreachable set.
    check_cancelled(cancel)?;
    let unreachable = compute_unreachable(
        listed_objects,
        reachable_keys,
        coordinator_protected_keys,
        shard_snapshot,
    );
    debug!(
        unreachable_count = unreachable.len(),
        "computed unreachable set"
    );

    // Phase 2: Grace-period filter.
    check_cancelled(cancel)?;
    let (to_delete, grace_skipped) =
        partition_grace_filter(&unreachable, t0, grace_period, args.force);
    debug!(
        to_delete_count = to_delete.len(),
        grace_skipped = grace_skipped.len(),
        "after grace-period filter"
    );

    // Emit warning events for grace-period skips in JSONL mode.
    if let Some(stream) = jsonl_stream
        && let Ok(mut s) = stream.lock()
    {
        for obj in &grace_skipped {
            s.emit_warning(WarningPayload {
                code: "gc-grace-skip".to_owned(),
                message: format!("skipped (within grace period): {}", obj.key),
                path: Some(obj.key.clone()),
            });
        }
    }

    // Phase 3: Force confirmation.
    if args.force && !args.dry_run && !confirm_force(args)? {
        info!("force GC aborted by user");
        outcome.log();
        return Ok(outcome);
    }

    // Phase 4: Dry-run path — report only, no mutations.
    if args.dry_run {
        for obj in &to_delete {
            info!(key = %obj.key, size = obj.size, "would delete (dry-run)");
            tally(categorize_key(&obj.key), &mut outcome, obj.size);

            // Emit file_done per xorb considered in JSONL mode.
            if let Some(stream) = jsonl_stream
                && let Ok(mut s) = stream.lock()
            {
                s.emit_file_done(FileDonePayload {
                    path: obj.key.clone(),
                    bytes: obj.size,
                    duration_ms: 0,
                    status: "skipped".to_owned(),
                });
            }
        }
        outcome.log();
        return Ok(outcome);
    }

    // Phase 5: Parallel deletes bounded by delete_concurrency.
    check_cancelled(cancel)?;
    let delete_outcome = if let Some(journal) = journal.as_deref_mut() {
        journal.plan(&to_delete).await?;
        execute_journaled_deletes(
            journal,
            cancel,
            delete_concurrency,
            deleter,
            &mut outcome,
            jsonl_stream,
            sweep,
        )
        .await
    } else {
        execute_deletes(
            &to_delete,
            cancel,
            delete_concurrency,
            deleter,
            &mut outcome,
            jsonl_stream,
            None,
        )
        .await
    };

    if cancel.is_cancelled() {
        outcome.cancelled = true;
        outcome.log();
        return Err(CrabError::Cancelled);
    }

    // Phase 6: Manifest CAS to remove deleted entries.
    check_cancelled(cancel)?;
    outcome.delete_failures = delete_outcome.failure_count;
    let reconciliation_error =
        if delete_outcome.first_error.is_none() && deleter.reconciliation_required() {
            deleter
                .reconcile_manifest(&delete_outcome.deleted_keys)
                .await
                .err()
        } else {
            None
        };
    outcome.reconciliation_failed = reconciliation_error.is_some();

    if reconciliation_error.is_none()
        && delete_outcome.first_error.is_none()
        && let Some(journal) = journal
    {
        journal.complete().await?;
    }

    // Phase 7: Log structured outcome.
    outcome.log();
    if let Some(source) = delete_outcome.first_error {
        if matches!(&source, CrabError::PushLockHeld { .. }) {
            return Err(source);
        }
        return Err(CrabError::GcPartialFailure {
            objects_deleted: delete_outcome.deleted_count,
            delete_failures: outcome.delete_failures,
            reconciliation_failed: outcome.reconciliation_failed,
            source: Box::new(source),
        });
    }
    if let Some(source) = reconciliation_error {
        return Err(CrabError::GcPartialFailure {
            objects_deleted: delete_outcome.deleted_count,
            delete_failures: outcome.delete_failures,
            reconciliation_failed: outcome.reconciliation_failed,
            source: Box::new(source),
        });
    }
    Ok(outcome)
}

/// Execute deletes with bounded concurrency, checking cancellation between
/// batches. Returns the list of successfully deleted keys.
///
/// Each chunk of size `concurrency` is issued in parallel via a semaphore,
/// then we wait for all deletes in the chunk to complete before moving to
/// the next one. This gives up to `concurrency`-way parallelism while
/// keeping results and cancellation coordinated batch-by-batch. The
/// This keeps the concurrency setting effective while preserving coordinated
/// cancellation and result accounting between batches.
struct DeleteOutcome {
    deleted_keys: Vec<String>,
    deleted_count: u64,
    failure_count: u64,
    first_error: Option<CrabError>,
}

async fn execute_journaled_deletes(
    journal: &mut journal::GcRunJournal,
    cancel: &CancellationToken,
    concurrency: usize,
    deleter: &dyn ObjectDeleter,
    outcome: &mut GcOutcome,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
    sweep: Option<(&Store, &str)>,
) -> DeleteOutcome {
    let mut aggregate = DeleteOutcome {
        deleted_keys: Vec::new(),
        deleted_count: 0,
        failure_count: 0,
        first_error: None,
    };
    loop {
        let objects = match journal.next_batch().await {
            Ok(Some(objects)) => objects,
            Ok(None) => break,
            Err(error) => {
                aggregate.first_error = Some(error);
                break;
            }
        };
        let lease = match sweep {
            Some((store, domain)) => {
                match crate::maintenance::GcSweepLease::acquire_for_run(
                    store,
                    domain,
                    &journal.state().run_id,
                    cancel,
                )
                .await
                {
                    Ok(lease) => {
                        if let Err(error) = journal.ensure_next_fence_epoch(lease.epoch()) {
                            let _ = lease.release().await;
                            aggregate.first_error = Some(error);
                            break;
                        }
                        Some(lease)
                    }
                    Err(error) => {
                        aggregate.first_error = Some(error);
                        break;
                    }
                }
            }
            None => None,
        };
        let batch = execute_deletes(
            &objects,
            cancel,
            concurrency,
            deleter,
            outcome,
            jsonl_stream,
            Some(DeletePolicy {
                snapshot_at: match journal.snapshot_at() {
                    Ok(snapshot_at) => snapshot_at,
                    Err(error) => {
                        aggregate.first_error = Some(error);
                        if let Some(lease) = lease {
                            let _ = lease.release().await;
                        }
                        break;
                    }
                },
                grace_period: Duration::from_secs(journal.state().grace_secs),
                force: journal.state().force,
            }),
        )
        .await;
        aggregate.deleted_keys.extend(
            batch
                .deleted_keys
                .iter()
                .filter(|_| deleter.reconciliation_required())
                .cloned(),
        );
        aggregate.deleted_count = aggregate.deleted_count.saturating_add(batch.deleted_count);
        aggregate.failure_count = aggregate.failure_count.saturating_add(batch.failure_count);
        let batch_failed = batch.failure_count > 0 || batch.first_error.is_some();
        if aggregate.first_error.is_none() {
            aggregate.first_error = batch.first_error;
        }
        if cancel.is_cancelled() || batch_failed {
            if let Some(lease) = lease {
                if let Err(error) = lease.release().await
                    && aggregate.first_error.is_none()
                {
                    aggregate.first_error = Some(error);
                }
            }
            break;
        }
        if let Err(error) = check_cancelled(cancel) {
            aggregate.first_error = Some(error);
            if let Some(lease) = lease {
                let _ = lease.release().await;
            }
            break;
        }
        journal::crash_at("after-provider-delete");
        let bytes = batch
            .deleted_keys
            .iter()
            .filter_map(|key| objects.iter().find(|object| object.key == *key))
            .try_fold(0u64, |total, object| total.checked_add(object.size))
            .unwrap_or(u64::MAX);
        let fence_epoch = lease.as_ref().map(crate::maintenance::GcSweepLease::epoch);
        if let Err(error) = journal
            .complete_batch(&batch.deleted_keys, bytes, fence_epoch)
            .await
        {
            aggregate.first_error = Some(error);
            if let Some(lease) = lease {
                let _ = lease.release().await;
            }
            break;
        }
        if let Some(lease) = lease
            && let Err(error) = lease.release().await
        {
            aggregate.first_error = Some(error);
            break;
        }
    }
    aggregate
}

async fn resume_gc_run(
    args: &GcArgs,
    journal: &mut journal::GcRunJournal,
    cancel: &CancellationToken,
    delete_concurrency: usize,
    deleter: &dyn ObjectDeleter,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
    list_outcome: ListOutcome,
    sweep: Option<(&Store, &str)>,
) -> Result<GcOutcome> {
    if args.dry_run {
        return Err(CrabError::Configuration {
            key: "gc.resume".to_owned(),
            origin: "a durable destructive GC run cannot be resumed as a dry-run".to_owned(),
        });
    }
    let mut outcome = GcOutcome {
        list_requests: list_outcome.requests,
        list_parallelism: list_outcome.parallelism,
        list_wall_seconds: list_outcome.wall_seconds,
        dry_run: false,
        partial_enumeration: !list_outcome.failed_prefixes.is_empty(),
        ..GcOutcome::default()
    };
    check_cancelled(cancel)?;
    let delete_outcome = execute_journaled_deletes(
        journal,
        cancel,
        delete_concurrency,
        deleter,
        &mut outcome,
        jsonl_stream,
        sweep,
    )
    .await;
    if cancel.is_cancelled() {
        outcome.cancelled = true;
        outcome.log();
        return Err(CrabError::Cancelled);
    }
    check_cancelled(cancel)?;
    outcome.delete_failures = delete_outcome.failure_count;
    let (objects_deleted, reconciliation_error) = if deleter.reconciliation_required() {
        if delete_outcome.first_error.is_none() {
            let mut all_deleted_keys = journal.deleted_keys().await?;
            all_deleted_keys.extend(delete_outcome.deleted_keys.iter().cloned());
            all_deleted_keys.sort_unstable();
            all_deleted_keys.dedup();
            let objects_deleted =
                u64::try_from(all_deleted_keys.len()).map_err(|_| CrabError::CorruptObject {
                    path: "gc/runs".to_owned(),
                    reason: "GC deleted-key count overflows".to_owned(),
                })?;
            let reconciliation_error = deleter.reconcile_manifest(&all_deleted_keys).await.err();
            (objects_deleted, reconciliation_error)
        } else {
            let objects_deleted = journal
                .deleted_key_count()
                .await?
                .saturating_add(delete_outcome.deleted_count);
            (objects_deleted, None)
        }
    } else {
        let objects_deleted = journal
            .deleted_key_count()
            .await?
            .saturating_add(delete_outcome.deleted_count);
        (objects_deleted, None)
    };
    outcome.reconciliation_failed = reconciliation_error.is_some();
    if delete_outcome.first_error.is_none() && reconciliation_error.is_none() {
        journal.complete().await?;
    }
    outcome.log();
    if let Some(source) = delete_outcome.first_error.or(reconciliation_error) {
        if matches!(&source, CrabError::PushLockHeld { .. }) {
            return Err(source);
        }
        return Err(CrabError::GcPartialFailure {
            objects_deleted,
            delete_failures: outcome.delete_failures,
            reconciliation_failed: outcome.reconciliation_failed,
            source: Box::new(source),
        });
    }
    Ok(outcome)
}

async fn execute_deletes(
    objects: &[ObjectMeta],
    cancel: &CancellationToken,
    concurrency: usize,
    deleter: &dyn ObjectDeleter,
    outcome: &mut GcOutcome,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
    policy: Option<DeletePolicy>,
) -> DeleteOutcome {
    let mut deleted_keys = Vec::new();
    let mut deleted_count = 0u64;
    let mut failure_count = 0u64;
    let mut first_error = None;
    let start = Instant::now();
    let semaphore = Arc::new(Semaphore::new(concurrency.max(1)));

    for chunk in objects.chunks(concurrency.max(1)) {
        if cancel.is_cancelled() {
            break;
        }

        // Launch all deletes in this chunk concurrently.
        let mut tasks: Vec<_> = Vec::with_capacity(chunk.len());
        for obj in chunk {
            let permit = Arc::clone(&semaphore).acquire_owned().await;
            let object = obj.clone();
            tasks.push(async move {
                let _permit = permit;
                let t_start = Instant::now();
                let result = match policy {
                    Some(policy) => deleter.delete_candidate(&object, policy).await,
                    None => deleter
                        .delete(&object.key)
                        .await
                        .map(|()| CandidateDelete::Deleted),
                };
                (object.key, object.size, result, t_start.elapsed())
            });
        }

        // Collect results — tally outcome and emit events sequentially so
        // outcome/jsonl_stream remain single-writer.
        let results = futures_util::future::join_all(tasks).await;
        for (key, size, result, _elapsed) in results {
            match result {
                Ok(CandidateDelete::Deleted) => {
                    tally(categorize_key(&key), outcome, size);
                    deleted_keys.push(key.clone());
                    deleted_count = deleted_count.saturating_add(1);
                    debug!(key = %key, "deleted");

                    if let Some(stream) = jsonl_stream
                        && let Ok(mut s) = stream.lock()
                    {
                        s.emit_file_done(FileDonePayload {
                            path: key,
                            bytes: size,
                            duration_ms: start.elapsed().as_millis() as u64,
                            status: "ok".to_owned(),
                        });
                    }
                }
                Ok(CandidateDelete::Retained) => {
                    debug!(key = %key, "retained after delete-time revalidation");
                }
                Err(CrabError::NotFound { .. }) => {
                    // Deletes are idempotent across crash/retry boundaries.
                    tally(categorize_key(&key), outcome, size);
                    deleted_keys.push(key);
                    deleted_count = deleted_count.saturating_add(1);
                }
                Err(e) => {
                    warn!(key = %key, error = %e, "delete failed, skipping");
                    failure_count += 1;
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }
    }

    DeleteOutcome {
        deleted_keys,
        deleted_count,
        failure_count,
        first_error,
    }
}

/// Production-ready parallel delete using `Arc<dyn ObjectDeleter>`.
///
/// Spawns up to `concurrency` tasks at a time via a semaphore. Each task
/// independently deletes one object. Results are aggregated via atomics.
pub async fn execute_deletes_parallel(
    objects: &[ObjectMeta],
    cancel: &CancellationToken,
    concurrency: usize,
    deleter: Arc<dyn ObjectDeleter>,
) -> (Vec<String>, DeleteStats) {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let packs = Arc::new(AtomicU64::new(0));
    let xorbs = Arc::new(AtomicU64::new(0));
    let shards = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    let deleted_keys = Arc::new(tokio::sync::Mutex::new(Vec::new()));

    for chunk in objects.chunks(concurrency) {
        if cancel.is_cancelled() {
            break;
        }

        let mut handles = Vec::with_capacity(chunk.len());

        for obj in chunk {
            let key = obj.key.clone();
            let size = obj.size;
            let category = categorize_key(&key);
            let sem = Arc::clone(&semaphore);
            let deleter = Arc::clone(&deleter);
            let packs = Arc::clone(&packs);
            let xorbs = Arc::clone(&xorbs);
            let shards = Arc::clone(&shards);
            let bytes_c = Arc::clone(&bytes);
            let del_keys = Arc::clone(&deleted_keys);

            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await;
                if let Ok(()) = deleter.delete(&key).await {
                    match category {
                        ObjectCategory::Pack => {
                            packs.fetch_add(1, Ordering::Relaxed);
                        }
                        ObjectCategory::Xorb => {
                            xorbs.fetch_add(1, Ordering::Relaxed);
                        }
                        ObjectCategory::Shard => {
                            shards.fetch_add(1, Ordering::Relaxed);
                        }
                        ObjectCategory::Other => {}
                    }
                    bytes_c.fetch_add(size, Ordering::Relaxed);
                    del_keys.lock().await.push(key);
                }
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }
    }

    let stats = DeleteStats {
        packs: packs.load(Ordering::Relaxed),
        xorbs: xorbs.load(Ordering::Relaxed),
        shards: shards.load(Ordering::Relaxed),
        bytes: bytes.load(Ordering::Relaxed),
    };

    let keys = match Arc::try_unwrap(deleted_keys) {
        Ok(mutex) => mutex.into_inner(),
        Err(arc) => {
            // Fallback: block on the lock. This only happens if a spawned
            // task still holds a reference, which shouldn't occur after
            // joining all handles.
            arc.blocking_lock().clone()
        }
    };

    (keys, stats)
}

/// Aggregated delete statistics.
#[derive(Debug, Default)]
pub struct DeleteStats {
    pub packs: u64,
    pub xorbs: u64,
    pub shards: u64,
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// Manifest-aware reachability scan
// ---------------------------------------------------------------------------

/// Read the manifest pointer and follow content hashes to build the
/// reachable set of segmented shard/pack metadata objects.
///
/// Returns the manifest and the set of bulk manifest object keys that
/// are referenced by the current manifest pointer (e.g.
/// `{repo}/metadata/shard/indexes/{hash}.json` and referenced segments.
pub async fn reachable_bulk_objects_from_manifest(
    store: &Store,
    router: &StoreLayout,
) -> Result<(crate::metadata::manifest::Manifest, HashSet<String>)> {
    let (manifest, _etag) = crate::metadata::manifest::read_manifest(store, router).await?;
    let mut reachable = HashSet::new();

    extend_reachable_bulk_objects(store, router, &manifest, &mut reachable).await?;

    debug!(
        reachable_bulk_objects = reachable.len(),
        generation = manifest.generation,
        "manifest reachability scan complete"
    );

    Ok((manifest, reachable))
}

async fn extend_reachable_bulk_objects(
    store: &Store,
    router: &StoreLayout,
    manifest: &crate::metadata::manifest::Manifest,
    reachable: &mut HashSet<String>,
) -> Result<()> {
    if !manifest.shard_index_hash.is_empty() {
        let path = router.repo_path(&crab_metadata::segmented::index_relative_path(
            crab_metadata::segmented::SegmentKind::Shard,
            &manifest.shard_index_hash,
        ));
        reachable.insert(path.as_ref().to_string());
        let index =
            crate::metadata::manifest::read_shard_index(store, router, &manifest.shard_index_hash)
                .await?;
        for segment in index.segments {
            reachable.insert(router.repo_path(&segment.path).as_ref().to_string());
        }
    }

    if !manifest.pack_index_hash.is_empty() {
        let path = router.repo_path(&crab_metadata::segmented::index_relative_path(
            crab_metadata::segmented::SegmentKind::Pack,
            &manifest.pack_index_hash,
        ));
        reachable.insert(path.as_ref().to_string());
        let index =
            crate::metadata::manifest::read_pack_index(store, router, &manifest.pack_index_hash)
                .await?;
        for segment in index.segments {
            reachable.insert(router.repo_path(&segment.path).as_ref().to_string());
        }
    }

    if let Some(ref hash) = manifest.commit_graph_hash {
        let path = router.bulk_manifest_path("commit-graph", hash);
        reachable.insert(path.as_ref().to_string());
    }

    if let Some(ref hash) = manifest.ref_registry_hash {
        let path = router.bulk_manifest_path("ref-registry", hash);
        reachable.insert(path.as_ref().to_string());
    }

    if !manifest.refs.is_empty() && !manifest.pack_index_hash.is_empty() {
        reachable.insert(
            router
                .git_visibility_path(&manifest.git_validation_digest)
                .as_ref()
                .to_string(),
        );
        // Crab 1.0.15 readers still use the v1 key. Retain it while the
        // explicit read/backfill migration remains supported.
        reachable.insert(
            router
                .git_visibility_v1_path(manifest.generation, &manifest.pack_index_hash)
                .as_ref()
                .to_string(),
        );
        extend_shallow_closure_reachable(store, router, manifest, reachable).await?;
    }

    Ok(())
}

async fn extend_shallow_closure_reachable(
    store: &Store,
    router: &StoreLayout,
    manifest: &crate::metadata::manifest::Manifest,
    reachable: &mut HashSet<String>,
) -> Result<()> {
    let descriptor_path = router.shallow_closure_path(&manifest.git_validation_digest);
    match store.get_with_etag(&descriptor_path).await {
        Ok((bytes, _)) => {
            reachable.insert(descriptor_path.as_ref().to_owned());
            match crab_metadata::shallow_closure::decode_shallow_closure_descriptor(
                &bytes,
                descriptor_path.as_ref(),
            ) {
                Ok(descriptor) => {
                    for entry in descriptor.entries {
                        reachable.insert(router.repo_path(&entry.path).as_ref().to_owned());
                    }
                }
                Err(error) => {
                    warn!(
                        path = %descriptor_path,
                        error = %error,
                        "retaining all shallow closure entries after descriptor validation failure"
                    );
                    reachable.extend(
                        list_shallow_closure_entry_keys(store, router)
                            .await?
                            .into_iter(),
                    );
                }
            }
        }
        Err(CrabError::NotFound { .. }) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

async fn list_shallow_closure_entry_keys(
    store: &Store,
    router: &StoreLayout,
) -> Result<Vec<String>> {
    let prefix = router.repo_path("metadata/shallow-closure/entries");
    let mut objects = store.inner().list(Some(&prefix));
    let mut keys = Vec::new();
    while let Some(object) = objects.try_next().await.map_err(CrabError::Storage)? {
        keys.push(object.location.as_ref().to_owned());
    }
    Ok(keys)
}

/// Read the manifest and build the repo-local object set that must survive GC.
pub async fn reachable_repo_objects_from_manifest(
    store: &Store,
    router: &StoreLayout,
) -> Result<(crate::metadata::manifest::Manifest, HashSet<String>)> {
    let snapshot = reachable_repo_objects_from_manifest_with_concurrency(
        store,
        router,
        DEFAULT_LIST_CONCURRENCY,
    )
    .await?;
    Ok((snapshot.manifest, snapshot.reachable_keys))
}

struct RepoGcReachability {
    manifest: crate::metadata::manifest::Manifest,
    reachable_keys: HashSet<String>,
    shard_snapshot: ShardListSnapshot,
    current_pack_keys: HashSet<String>,
}

#[derive(Default)]
struct ReachabilityDigest {
    count: u64,
    xor: [u8; 32],
    sum: [u8; 32],
}

impl ReachabilityDigest {
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

/// Streams repository roots into a run-owned mark set while computing the
/// sealed root identity. The optional writer is absent when a deleting run is
/// resumed; in that case the same walk only revalidates the identity.
struct RepoReachabilitySink<'a> {
    writer: Option<&'a mut marks::DurableMarkWriter>,
    digest: &'a mut ReachabilityDigest,
    cancel: &'a CancellationToken,
}

impl RepoReachabilitySink<'_> {
    async fn add(&mut self, key: String) -> Result<()> {
        check_cancelled(self.cancel)?;
        self.digest.add("reachable", &key);
        if let Some(writer) = self.writer.as_deref_mut() {
            writer.add(&key).await?;
        }
        Ok(())
    }
}

struct StreamedRepoRootSnapshot {
    root_identity: String,
}

/// Walks the repository roots without constructing a process-wide reachable
/// key set. Mark chunks are flushed by [`DurableMarkWriter`] as they fill.
async fn stream_repo_reachability(
    store: &Store,
    router: &StoreLayout,
    concurrency: usize,
    coordinator_protected_keys: &HashSet<String>,
    cancel: &CancellationToken,
    mut writer: Option<&mut marks::DurableMarkWriter>,
) -> Result<StreamedRepoRootSnapshot> {
    check_cancelled(cancel)?;
    let snapshot = crate::metadata::manifest::read_repository_snapshot(store, router).await?;
    let manifest = snapshot.manifest;
    let mut digest = ReachabilityDigest::default();
    digest.add("generation", &manifest.generation.to_string());
    digest.add("git-validation", &manifest.git_validation_digest);
    digest.add("shard-index", &manifest.shard_index_hash);
    digest.add("pack-index", &manifest.pack_index_hash);
    let mut sink = RepoReachabilitySink {
        writer: writer.take(),
        digest: &mut digest,
        cancel,
    };

    stream_reachable_bulk_objects(store, router, &manifest, &mut sink).await?;
    stream_reachable_workflow_objects(store, router, &mut sink).await?;
    sink.add(router.manifest_path().as_ref().to_owned()).await?;

    for pack in &snapshot.journal.packs {
        for key in pack_object_keys(router, &pack.pack_id) {
            sink.add(key).await?;
        }
    }
    for edit in &snapshot.journal.ordered_edits {
        if let Some(hash) = &edit.visibility_evidence_hash {
            sink.add(router.git_visibility_edit_path(hash).as_ref().to_owned())
                .await?;
        }
    }

    // Journal objects remain recovery roots until the corresponding frontier
    // is compacted. Stream this prefix instead of collecting its listing.
    let journal_prefix = router.repo_path("refs/journal");
    let mut journal_objects = store.inner().list(Some(&journal_prefix));
    while let Some(object) = journal_objects
        .try_next()
        .await
        .map_err(CrabError::Storage)?
    {
        sink.add(object.location.as_ref().to_owned()).await?;
    }

    let storage_router =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    let mut history = crab_metadata::manifest_store::stream_manifest_history(
        store.as_storage(),
        &storage_router,
        concurrency,
    );
    while let Some(entry) = history.try_next().await.map_err(CrabError::from)? {
        sink.add(entry.path).await?;
        stream_reachable_bulk_objects(store, router, &entry.manifest, &mut sink).await?;
    }

    // Artifact validation already owns the registry/pending-promotion
    // contract. Its result is fed into the same durable sink so payloads and
    // manifests cannot be collected just because this path is streaming.
    let workflow_store = crab_workflow::WorkflowStore::from_storage(store.clone().into());
    let mut artifact_visitor = WorkflowArtifactReachabilityVisitor { sink: &mut sink };
    crab_workflow::visit_reachable_remote_artifact_objects(
        &workflow_store,
        router.repo_prefix(),
        &mut artifact_visitor,
    )
    .await?;

    for key in coordinator_protected_keys {
        digest.add("protected", key);
    }
    for hash in &snapshot.journal.shards {
        digest.add(
            "shard-snapshot",
            &format!("shards/{}/{hash}", &hash[..2.min(hash.len())]),
        );
    }
    Ok(StreamedRepoRootSnapshot {
        root_identity: digest.finish(),
    })
}

async fn stream_reachable_bulk_objects(
    store: &Store,
    router: &StoreLayout,
    manifest: &crate::metadata::manifest::Manifest,
    sink: &mut RepoReachabilitySink<'_>,
) -> Result<()> {
    if !manifest.shard_index_hash.is_empty() {
        sink.add(
            router
                .repo_path(&crab_metadata::segmented::index_relative_path(
                    crab_metadata::segmented::SegmentKind::Shard,
                    &manifest.shard_index_hash,
                ))
                .as_ref()
                .to_owned(),
        )
        .await?;
        let index =
            crate::metadata::manifest::read_shard_index(store, router, &manifest.shard_index_hash)
                .await?;
        for segment in index.segments {
            sink.add(router.repo_path(&segment.path).as_ref().to_owned())
                .await?;
        }
    }

    if !manifest.pack_index_hash.is_empty() {
        sink.add(
            router
                .repo_path(&crab_metadata::segmented::index_relative_path(
                    crab_metadata::segmented::SegmentKind::Pack,
                    &manifest.pack_index_hash,
                ))
                .as_ref()
                .to_owned(),
        )
        .await?;
        let index =
            crate::metadata::manifest::read_pack_index(store, router, &manifest.pack_index_hash)
                .await?;
        for segment in index.segments {
            sink.add(router.repo_path(&segment.path).as_ref().to_owned())
                .await?;
        }
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let mut visitor = PackReachabilityVisitor { router, sink };
        crab_metadata::manifest_store::visit_bulk_pack_list(
            store.as_storage(),
            &storage_router,
            &manifest.pack_index_hash,
            &mut visitor,
        )
        .await
        .map_err(CrabError::from)?;
    }

    if let Some(hash) = &manifest.commit_graph_hash {
        sink.add(
            router
                .bulk_manifest_path("commit-graph", hash)
                .as_ref()
                .to_owned(),
        )
        .await?;
    }
    if let Some(hash) = &manifest.ref_registry_hash {
        sink.add(
            router
                .bulk_manifest_path("ref-registry", hash)
                .as_ref()
                .to_owned(),
        )
        .await?;
    }
    if !manifest.refs.is_empty() && !manifest.pack_index_hash.is_empty() {
        sink.add(
            router
                .git_visibility_path(&manifest.git_validation_digest)
                .as_ref()
                .to_owned(),
        )
        .await?;
        sink.add(
            router
                .git_visibility_v1_path(manifest.generation, &manifest.pack_index_hash)
                .as_ref()
                .to_owned(),
        )
        .await?;
        stream_shallow_closure_reachable(store, router, manifest, sink).await?;
    }
    Ok(())
}

async fn stream_shallow_closure_reachable(
    store: &Store,
    router: &StoreLayout,
    manifest: &crate::metadata::manifest::Manifest,
    sink: &mut RepoReachabilitySink<'_>,
) -> Result<()> {
    let descriptor_path = router.shallow_closure_path(&manifest.git_validation_digest);
    match store.get_with_etag(&descriptor_path).await {
        Ok((bytes, _)) => {
            sink.add(descriptor_path.as_ref().to_owned()).await?;
            match crab_metadata::shallow_closure::decode_shallow_closure_descriptor(
                &bytes,
                descriptor_path.as_ref(),
            ) {
                Ok(descriptor) => {
                    for entry in descriptor.entries {
                        sink.add(router.repo_path(&entry.path).as_ref().to_owned())
                            .await?;
                    }
                }
                Err(error) => {
                    warn!(
                        path = %descriptor_path,
                        error = %error,
                        "retaining all shallow closure entries after descriptor validation failure"
                    );
                    for key in list_shallow_closure_entry_keys(store, router).await? {
                        sink.add(key).await?;
                    }
                }
            }
        }
        Err(CrabError::NotFound { .. }) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn pack_object_keys(router: &StoreLayout, pack_id: &str) -> [String; 4] {
    [
        router.pack_path(pack_id).as_ref().to_owned(),
        router.pack_index_path(pack_id).as_ref().to_owned(),
        router.pack_reverse_index_path(pack_id).as_ref().to_owned(),
        router.pack_metadata_path(pack_id).as_ref().to_owned(),
    ]
}

struct PackReachabilityVisitor<'router, 'sink, 'roots> {
    router: &'router StoreLayout,
    sink: &'sink mut RepoReachabilitySink<'roots>,
}

impl
    crab_metadata::segmented_store::AsyncRecordVisitor<
        crate::metadata::manifest::PackManifestEntry,
        CrabError,
    > for PackReachabilityVisitor<'_, '_, '_>
{
    fn visit<'a>(
        &'a mut self,
        pack: crate::metadata::manifest::PackManifestEntry,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async move {
            for key in pack_object_keys(self.router, &pack.pack_id) {
                self.sink.add(key).await?;
            }
            Ok(())
        })
    }
}

struct WorkflowArtifactReachabilityVisitor<'sink, 'roots> {
    sink: &'sink mut RepoReachabilitySink<'roots>,
}

impl crab_workflow::RemoteArtifactReachabilityVisitor<CrabError>
    for WorkflowArtifactReachabilityVisitor<'_, '_>
{
    fn visit<'a>(&'a mut self, key: String) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async move { self.sink.add(key).await })
    }
}

const MAX_WORKFLOW_ROOT_BODY_BYTES: usize = 8 * 1024 * 1024;

async fn stream_reachable_workflow_objects(
    store: &Store,
    router: &StoreLayout,
    sink: &mut RepoReachabilitySink<'_>,
) -> Result<()> {
    let stage_prefix = router.repo_path("refs/crab/stages/");
    let mut stage_refs = store.inner().list(Some(&stage_prefix));
    while let Some(object) = stage_refs.try_next().await.map_err(CrabError::Storage)? {
        let key = object.location.as_ref().to_owned();
        sink.add(key.clone()).await?;
        let relative = key
            .strip_prefix(&format!("{}/", router.repo_prefix()))
            .and_then(|value| value.strip_prefix("refs/crab/stages/"))
            .map(|value| value.trim_end_matches('/'))
            .ok_or_else(|| CrabError::CorruptObject {
                path: key.clone(),
                reason: "workflow stage ref is outside its repository namespace".to_owned(),
            })?;
        let stage_hash = crab_xet::hash::MerkleHash::from_hex(relative).map_err(|error| {
            CrabError::CorruptObject {
                path: key.clone(),
                reason: format!("invalid workflow stage ref hash: {error}"),
            }
        })?;
        let manifest_path =
            stream_workflow_stage_manifest(store, router, &stage_hash, sink).await?;
        let (body, _) = store.get_with_etag(&object.location).await?;
        if body.len() > MAX_WORKFLOW_ROOT_BODY_BYTES {
            return Err(CrabError::Configuration {
                key: "gc.workflow.root_bytes".to_owned(),
                origin: format!("workflow stage ref {} exceeds the bounded body budget", key),
            });
        }
        let target = std::str::from_utf8(&body).map_err(|error| CrabError::CorruptObject {
            path: key.clone(),
            reason: format!("workflow stage ref is not UTF-8: {error}"),
        })?;
        if target != manifest_path {
            return Err(CrabError::CorruptObject {
                path: key,
                reason: format!(
                    "workflow stage ref points to {target:?}, expected {manifest_path:?}"
                ),
            });
        }
    }

    for ref_prefix in ["refs/crab/exp/", "refs/crab/exp-meta/"] {
        let prefix = router.repo_path(ref_prefix);
        let mut refs = store.inner().list(Some(&prefix));
        while let Some(object) = refs.try_next().await.map_err(CrabError::Storage)? {
            let key = object.location.as_ref().to_owned();
            sink.add(key.clone()).await?;
            let raw_id = key
                .strip_prefix(&format!("{}/", router.repo_prefix()))
                .and_then(|value| value.strip_prefix(ref_prefix))
                .map(|value| value.trim_end_matches('/'))
                .ok_or_else(|| CrabError::CorruptObject {
                    path: key.clone(),
                    reason: "workflow experiment ref is outside its repository namespace"
                        .to_owned(),
                })?;
            let id = raw_id
                .parse::<crab_workflow::ExperimentId>()
                .map_err(|error| CrabError::CorruptObject {
                    path: key.clone(),
                    reason: format!("invalid workflow experiment ref: {error}"),
                })?;
            let experiment_prefix = router.repo_path(&format!("workflow/exp/{id}/"));
            let mut objects = store.inner().list(Some(&experiment_prefix));
            while let Some(object) = objects.try_next().await.map_err(CrabError::Storage)? {
                let object_key = object.location.as_ref().to_owned();
                sink.add(object_key.clone()).await?;
                if !object_key.ends_with("/stage-refs.json") {
                    continue;
                }
                let (body, _) = store.get_with_etag(&object.location).await?;
                if body.len() > MAX_WORKFLOW_ROOT_BODY_BYTES {
                    return Err(CrabError::Configuration {
                        key: "gc.workflow.root_bytes".to_owned(),
                        origin: format!(
                            "workflow stage refs {} exceed the bounded body budget",
                            object_key
                        ),
                    });
                }
                let stages: Vec<String> =
                    serde_json::from_slice(&body).map_err(|error| CrabError::CorruptObject {
                        path: object_key.clone(),
                        reason: format!("invalid experiment stage refs: {error}"),
                    })?;
                for stage in stages {
                    let stage_hash =
                        crab_xet::hash::MerkleHash::from_hex(&stage).map_err(|error| {
                            CrabError::CorruptObject {
                                path: object_key.clone(),
                                reason: format!("invalid experiment stage hash: {error}"),
                            }
                        })?;
                    stream_workflow_stage_manifest(store, router, &stage_hash, sink).await?;
                    sink.add(
                        router
                            .repo_path(&format!("refs/crab/stages/{}", stage_hash.hex()))
                            .as_ref()
                            .to_owned(),
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

async fn stream_workflow_stage_manifest(
    store: &Store,
    router: &StoreLayout,
    stage_hash: &crab_xet::hash::MerkleHash,
    sink: &mut RepoReachabilitySink<'_>,
) -> Result<String> {
    let hex = stage_hash.hex();
    let manifest_path = router
        .repo_path(&format!("workflow/stages/{}/{}.json", &hex[..2], hex))
        .as_ref()
        .to_owned();
    sink.add(manifest_path.clone()).await?;
    let (body, _) = store
        .get_with_etag(&ObjectPath::from(manifest_path.as_str()))
        .await?;
    if body.len() > MAX_WORKFLOW_ROOT_BODY_BYTES {
        return Err(CrabError::Configuration {
            key: "gc.workflow.root_bytes".to_owned(),
            origin: format!(
                "workflow stage manifest {} exceeds the bounded body budget",
                manifest_path
            ),
        });
    }
    let entry: crab_workflow::StageCacheEntry =
        serde_json::from_slice(&body).map_err(|error| CrabError::CorruptObject {
            path: manifest_path.clone(),
            reason: format!("invalid workflow stage manifest JSON: {error}"),
        })?;
    crab_workflow::validate_stage_cache_entry(&entry).map_err(|error| {
        CrabError::CorruptObject {
            path: manifest_path.clone(),
            reason: format!("invalid workflow stage manifest: {error}"),
        }
    })?;
    if entry.stage_hash.as_hex() != hex {
        return Err(CrabError::CorruptObject {
            path: manifest_path.clone(),
            reason: format!(
                "workflow stage manifest hash is {}, expected {hex}",
                entry.stage_hash.as_hex()
            ),
        });
    }
    for output in crab_workflow::cached_artifacts(&entry) {
        match output.kind {
            crab_workflow::OutKind::File | crab_workflow::OutKind::Stdout => {
                sink.add(
                    router
                        .repo_path(&format!("workflow/xorbs/{}.xorb", output.file_hash))
                        .as_ref()
                        .to_owned(),
                )
                .await?;
            }
            crab_workflow::OutKind::Directory => {
                let Some(tree) = output.tree_manifest.as_ref() else {
                    return Err(CrabError::CorruptObject {
                        path: manifest_path.clone(),
                        reason: format!("directory output {:?} has no tree manifest", output.path),
                    });
                };
                for entry in tree.iter().filter(|entry| entry.kind == "file") {
                    sink.add(
                        router
                            .repo_path(&format!("workflow/xorbs/{}.xorb", entry.hash))
                            .as_ref()
                            .to_owned(),
                    )
                    .await?;
                }
            }
        }
    }
    Ok(manifest_path)
}

async fn reachable_repo_objects_from_manifest_with_concurrency(
    store: &Store,
    router: &StoreLayout,
    concurrency: usize,
) -> Result<RepoGcReachability> {
    let snapshot = crate::metadata::manifest::read_repository_snapshot(store, router).await?;
    let manifest = snapshot.manifest;
    let mut reachable = HashSet::new();
    extend_reachable_bulk_objects(store, router, &manifest, &mut reachable).await?;
    extend_reachable_workflow_objects(store, router, &mut reachable).await?;
    reachable.insert(router.manifest_path().as_ref().to_string());

    let mut current_pack_keys = HashSet::new();
    for pack in &snapshot.journal.packs {
        insert_pack_objects(router, &pack.pack_id, &mut reachable);
        insert_pack_objects(router, &pack.pack_id, &mut current_pack_keys);
    }
    for edit in &snapshot.journal.ordered_edits {
        if let Some(hash) = &edit.visibility_evidence_hash {
            reachable.insert(router.git_visibility_edit_path(hash).as_ref().to_owned());
        }
    }
    // Journal metadata is the recovery root when publication succeeded but
    // derived manifest compaction did not. GC may compact it only with the
    // same frontier protocol as writers.
    for object in store.list_prefix(&router.repo_path("refs/journal")).await? {
        reachable.insert(object.location.as_ref().to_owned());
    }

    let storage_router =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    crab_metadata::manifest_store::stream_manifest_history(
        store.as_storage(),
        &storage_router,
        concurrency,
    )
    .map(|entry| async move {
        let entry = entry.map_err(CrabError::from)?;
        let mut keys = HashSet::new();
        keys.insert(entry.path);
        extend_reachable_bulk_objects(store, router, &entry.manifest, &mut keys).await?;
        extend_reachable_pack_objects(store, router, &entry.manifest, &mut keys).await?;
        Ok::<_, CrabError>(keys)
    })
    .buffer_unordered(concurrency.max(1))
    .try_for_each(|keys| {
        reachable.extend(keys);
        futures_util::future::ready(Ok::<_, CrabError>(()))
    })
    .await?;

    let shard_snapshot = ShardListSnapshot {
        generation: manifest.generation,
        shard_keys: snapshot
            .journal
            .shards
            .iter()
            .map(|hash| format!("shards/{}/{hash}", &hash[..2.min(hash.len())]))
            .collect(),
    };
    Ok(RepoGcReachability {
        manifest,
        reachable_keys: reachable,
        shard_snapshot,
        current_pack_keys,
    })
}

async fn extend_reachable_pack_objects(
    store: &Store,
    router: &StoreLayout,
    manifest: &crate::metadata::manifest::Manifest,
    reachable: &mut HashSet<String>,
) -> Result<()> {
    if !manifest.pack_index_hash.is_empty() {
        let packs = crate::metadata::manifest::read_bulk_pack_list(
            store,
            router,
            &manifest.pack_index_hash,
        )
        .await?;
        for pack in packs {
            insert_pack_objects(router, &pack.pack_id, reachable);
        }
    }

    Ok(())
}

fn insert_pack_objects(router: &StoreLayout, pack_id: &str, reachable: &mut HashSet<String>) {
    reachable.insert(router.pack_path(pack_id).as_ref().to_owned());
    reachable.insert(router.pack_index_path(pack_id).as_ref().to_owned());
    reachable.insert(router.pack_reverse_index_path(pack_id).as_ref().to_owned());
    reachable.insert(router.pack_metadata_path(pack_id).as_ref().to_owned());
}

/// Add live workflow refs and their immutable objects to the repo mark set.
/// Workflow refs are the authoritative roots for stage-cache and experiment
/// namespaces; malformed roots abort the mark phase instead of allowing a
/// partially parsed live set to authorize deletion.
async fn extend_reachable_workflow_objects(
    store: &Store,
    router: &StoreLayout,
    reachable: &mut HashSet<String>,
) -> Result<()> {
    let mut stage_manifests_seen = HashSet::new();
    let stage_ref_prefix = router.repo_path("refs/crab/stages/");
    for object in store.list_prefix(&stage_ref_prefix).await? {
        let key = object.location.as_ref().to_owned();
        let Some(hash) = key
            .strip_prefix(&format!("{}/", router.repo_prefix()))
            .map(str::to_owned)
        else {
            continue;
        };
        reachable.insert(key);
        let Some(hash) = hash.strip_prefix("refs/crab/stages/") else {
            continue;
        };
        let hash = hash.trim_end_matches('/');
        let parsed = crab_xet::hash::MerkleHash::from_hex(hash).map_err(|error| {
            CrabError::CorruptObject {
                path: object.location.to_string(),
                reason: format!("invalid workflow stage ref hash: {error}"),
            }
        })?;
        let manifest_path = protect_workflow_stage_manifest(
            store,
            router,
            &parsed,
            reachable,
            &mut stage_manifests_seen,
        )
        .await?;
        let (ref_body, _) = store.get_with_etag(&object.location).await?;
        let ref_target =
            std::str::from_utf8(&ref_body).map_err(|error| CrabError::CorruptObject {
                path: object.location.to_string(),
                reason: format!("workflow stage ref is not UTF-8: {error}"),
            })?;
        if ref_target != manifest_path {
            return Err(CrabError::CorruptObject {
                path: object.location.to_string(),
                reason: format!(
                    "workflow stage ref points to {ref_target:?}, expected {manifest_path:?}"
                ),
            });
        }
    }

    let mut experiment_ids = HashSet::new();
    for ref_prefix in ["refs/crab/exp/", "refs/crab/exp-meta/"] {
        let prefix = router.repo_path(ref_prefix);
        for object in store.list_prefix(&prefix).await? {
            let key = object.location.as_ref().to_owned();
            reachable.insert(key.clone());
            let Some(raw_id) = key.strip_prefix(&format!("{}/", router.repo_prefix())) else {
                continue;
            };
            let Some(raw_id) = raw_id.strip_prefix(ref_prefix) else {
                continue;
            };
            let raw_id = raw_id.trim_end_matches('/');
            let id = raw_id
                .parse::<crab_workflow::ExperimentId>()
                .map_err(|error| CrabError::CorruptObject {
                    path: object.location.to_string(),
                    reason: format!("invalid workflow experiment ref: {error}"),
                })?;
            experiment_ids.insert(id.to_string());
        }
    }

    for id in experiment_ids {
        let prefix = router.repo_path(&format!("workflow/exp/{id}/"));
        for object in store.list_prefix(&prefix).await? {
            let key = object.location.as_ref().to_owned();
            reachable.insert(key.clone());
            if !key.ends_with("/stage-refs.json") {
                continue;
            }
            let (body, _) = store.get_with_etag(&object.location).await?;
            let stages: Vec<String> =
                serde_json::from_slice(&body).map_err(|error| CrabError::CorruptObject {
                    path: key.clone(),
                    reason: format!("invalid experiment stage refs: {error}"),
                })?;
            for stage in stages {
                let parsed = crab_xet::hash::MerkleHash::from_hex(&stage).map_err(|error| {
                    CrabError::CorruptObject {
                        path: key.clone(),
                        reason: format!("invalid experiment stage hash: {error}"),
                    }
                })?;
                protect_workflow_stage_manifest(
                    store,
                    router,
                    &parsed,
                    reachable,
                    &mut stage_manifests_seen,
                )
                .await?;
                reachable.insert(
                    router
                        .repo_path(&format!("refs/crab/stages/{}", parsed.hex()))
                        .as_ref()
                        .to_owned(),
                );
            }
        }
    }
    Ok(())
}

async fn protect_workflow_stage_manifest(
    store: &Store,
    router: &StoreLayout,
    stage_hash: &crab_xet::hash::MerkleHash,
    reachable: &mut HashSet<String>,
    seen: &mut HashSet<String>,
) -> Result<String> {
    let hex = stage_hash.hex();
    let manifest_path = router
        .repo_path(&format!("workflow/stages/{}/{}.json", &hex[..2], hex))
        .as_ref()
        .to_owned();
    reachable.insert(manifest_path.clone());
    if !seen.insert(manifest_path.clone()) {
        return Ok(manifest_path);
    }

    let (body, _) = store
        .get_with_etag(&ObjectPath::from(manifest_path.as_str()))
        .await?;
    let entry: crab_workflow::StageCacheEntry =
        serde_json::from_slice(&body).map_err(|error| CrabError::CorruptObject {
            path: manifest_path.clone(),
            reason: format!("invalid workflow stage manifest JSON: {error}"),
        })?;
    crab_workflow::validate_stage_cache_entry(&entry).map_err(|error| {
        CrabError::CorruptObject {
            path: manifest_path.clone(),
            reason: format!("invalid workflow stage manifest: {error}"),
        }
    })?;
    if entry.stage_hash.as_hex() != hex {
        return Err(CrabError::CorruptObject {
            path: manifest_path.clone(),
            reason: format!(
                "workflow stage manifest hash is {}, expected {hex}",
                entry.stage_hash.as_hex()
            ),
        });
    }

    for output in crab_workflow::cached_artifacts(&entry) {
        match output.kind {
            crab_workflow::OutKind::File | crab_workflow::OutKind::Stdout => {
                reachable.insert(
                    router
                        .repo_path(&format!("workflow/xorbs/{}.xorb", output.file_hash))
                        .as_ref()
                        .to_owned(),
                );
            }
            crab_workflow::OutKind::Directory => {
                let Some(tree) = output.tree_manifest.as_ref() else {
                    return Err(CrabError::CorruptObject {
                        path: manifest_path.clone(),
                        reason: format!("directory output {:?} has no tree manifest", output.path),
                    });
                };
                for entry in tree {
                    if entry.kind == "file" {
                        reachable.insert(
                            router
                                .repo_path(&format!("workflow/xorbs/{}.xorb", entry.hash))
                                .as_ref()
                                .to_owned(),
                        );
                    }
                }
            }
        }
    }
    Ok(manifest_path)
}

/// Run remote repo-scope GC against the primary/write store.
pub async fn run_repo_remote_gc(
    args: &GcArgs,
    store: &Store,
    router: &StoreLayout,
    coordinator_protected_keys: &HashSet<String>,
    cancel: &CancellationToken,
    grace_period: Duration,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
) -> Result<GcOutcome> {
    run_repo_remote_gc_under_maintenance(
        args,
        store,
        router,
        coordinator_protected_keys,
        cancel,
        grace_period,
        jsonl_stream,
        None,
    )
    .await
}

async fn run_repo_remote_gc_under_maintenance(
    args: &GcArgs,
    store: &Store,
    router: &StoreLayout,
    coordinator_protected_keys: &HashSet<String>,
    cancel: &CancellationToken,
    grace_period: Duration,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
    sweep_lease: Option<&crate::maintenance::GcSweepLease>,
) -> Result<GcOutcome> {
    let deleter = StoreObjectDeleter::new(store.clone());
    if !args.dry_run {
        return run_repo_gc_durable_streaming_roots(
            args,
            store,
            router,
            coordinator_protected_keys,
            cancel,
            grace_period,
            &deleter,
            jsonl_stream,
            sweep_lease,
        )
        .await;
    }

    let reachability_started = Instant::now();
    let reachability =
        reachable_repo_objects_from_manifest_with_concurrency(store, router, args.list_concurrency)
            .await?;
    let mut reachable_keys = reachability.reachable_keys;
    let workflow_store = crab_workflow::WorkflowStore::from_storage(store.clone().into());
    reachable_keys.extend(
        crab_workflow::reachable_remote_artifact_objects(&workflow_store, router.repo_prefix())
            .await?
            .into_iter(),
    );
    debug!(
        reachable_objects = reachable_keys.len(),
        wall_seconds = reachability_started.elapsed().as_secs_f64(),
        "repo GC reachability scan complete"
    );

    let (listed_objects, list_outcome) =
        list_repo_gc_candidates_with_concurrency(store, router, args.list_concurrency).await?;
    let pack_classes = classify_pack_storage(
        &listed_objects,
        &reachability.current_pack_keys,
        &reachable_keys,
        coordinator_protected_keys,
        grace_period,
        args.force,
    );

    let mut outcome = run_gc(
        args,
        listed_objects,
        &reachable_keys,
        coordinator_protected_keys,
        &reachability.shard_snapshot,
        cancel,
        args.delete_concurrency,
        grace_period,
        list_outcome,
        &deleter,
        jsonl_stream,
    )
    .await?;
    outcome.active_pack_bytes = pack_classes.active;
    outcome.retained_history_pack_bytes = pack_classes.retained;
    outcome.grace_period_pack_bytes = pack_classes.grace;
    outcome.collectible_pack_bytes = pack_classes.collectible;
    Ok(outcome)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PackStorageClasses {
    active: u64,
    retained: u64,
    grace: u64,
    collectible: u64,
}

fn classify_pack_storage(
    objects: &[ObjectMeta],
    current_pack_keys: &HashSet<String>,
    reachable_keys: &HashSet<String>,
    coordinator_protected_keys: &HashSet<String>,
    grace_period: Duration,
    force: bool,
) -> PackStorageClasses {
    let cutoff = SystemTime::now() - grace_period.max(MIN_GRACE_PERIOD);
    let mut classes = PackStorageClasses::default();
    for object in objects {
        if categorize_key(&object.key) != ObjectCategory::Pack {
            continue;
        }
        if current_pack_keys.contains(&object.key) {
            classes.active = classes.active.saturating_add(object.size);
        } else if reachable_keys.contains(&object.key)
            || coordinator_protected_keys.contains(&object.key)
        {
            classes.retained = classes.retained.saturating_add(object.size);
        } else if !force && object.last_modified >= cutoff {
            classes.grace = classes.grace.saturating_add(object.size);
        } else {
            classes.collectible = classes.collectible.saturating_add(object.size);
        }
    }
    classes
}

/// Build a shard-list snapshot from the manifest for GC T0 safety.
///
/// Reads the manifest's bulk shard-list and constructs a
/// [`ShardListSnapshot`] so that shards added after T0 are excluded
/// from the unreachable set.
pub async fn shard_snapshot_from_manifest(
    store: &Store,
    router: &StoreLayout,
    manifest: &crate::metadata::manifest::Manifest,
) -> Result<ShardListSnapshot> {
    let shard_hashes = if manifest.shard_index_hash.is_empty() {
        Vec::new()
    } else {
        crate::metadata::manifest::read_bulk_shard_list(store, router, &manifest.shard_index_hash)
            .await?
    };

    let shard_keys: HashSet<String> = shard_hashes
        .iter()
        .map(|h| format!("shards/{}/{h}", &h[..2.min(h.len())]))
        .collect();

    Ok(ShardListSnapshot {
        generation: manifest.generation,
        shard_keys,
    })
}

// ---------------------------------------------------------------------------
// GC compaction
// ---------------------------------------------------------------------------

/// Result of a GC compaction operation.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Number of shard entries removed from the bulk shard-list.
    pub shards_removed: usize,
    /// Number of pack entries removed from the bulk pack-list.
    pub packs_removed: usize,
    /// Whether the manifest was updated (compaction produced smaller lists).
    pub manifest_updated: bool,
}

/// Compact the bulk shard-list and pack-list by removing entries that
/// are no longer reachable.
///
/// Builds new bulk lists excluding unreferenced entries, uploads them,
/// and CAS-updates the manifest pointer. Old bulk objects become
/// GC-eligible after the grace period.
///
/// # Errors
///
/// Returns errors from store operations or CAS conflicts.
#[cfg(test)]
pub async fn compact_bulk_lists(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    reachable_shard_hashes: &HashSet<String, impl std::hash::BuildHasher>,
    reachable_pack_ids: &HashSet<String, impl std::hash::BuildHasher>,
) -> Result<CompactionResult> {
    let (manifest, etag) = crate::metadata::manifest::read_manifest(store, router).await?;

    // Read current segmented indexes.
    let current_shards = if manifest.shard_index_hash.is_empty() {
        Vec::new()
    } else {
        crate::metadata::manifest::read_bulk_shard_list(store, router, &manifest.shard_index_hash)
            .await?
    };

    let current_packs = if manifest.pack_index_hash.is_empty() {
        Vec::new()
    } else {
        crate::metadata::manifest::read_bulk_pack_list(store, router, &manifest.pack_index_hash)
            .await?
    };

    let orig_shard_count = current_shards.len();
    let orig_pack_count = current_packs.len();

    // Filter to only reachable entries.
    let compacted_shards: Vec<String> = current_shards
        .into_iter()
        .filter(|h| reachable_shard_hashes.contains(h))
        .collect();

    let compacted_packs: Vec<crate::metadata::manifest::PackManifestEntry> = current_packs
        .into_iter()
        .filter(|p| reachable_pack_ids.contains(&p.pack_id))
        .collect();

    let actual_shards_removed = orig_shard_count.saturating_sub(compacted_shards.len());
    let actual_packs_removed = orig_pack_count.saturating_sub(compacted_packs.len());

    if actual_shards_removed == 0 && actual_packs_removed == 0 {
        debug!("compaction: no entries to remove");
        return Ok(CompactionResult {
            shards_removed: 0,
            packs_removed: 0,
            manifest_updated: false,
        });
    }

    let next_generation = manifest.generation.saturating_add(1);
    let (new_shard_hash, _shard_index, shard_write) =
        crate::metadata::manifest::compact_shard_index(next_generation, &compacted_shards)?;
    let (new_pack_hash, _pack_index, pack_write) =
        crate::metadata::manifest::compact_pack_index(next_generation, &compacted_packs)?;
    let bulk = crate::metadata::manifest::BulkData {
        shard_index: shard_write,
        pack_index: pack_write,
    };
    crate::metadata::manifest::upload_segmented_bulk(store, router, &bulk).await?;

    // CAS-update the manifest pointer.
    let mut new_manifest = manifest;
    new_manifest.generation = next_generation;
    new_manifest.shard_index_hash = new_shard_hash;
    new_manifest.pack_index_hash = new_pack_hash;
    // The caller-provided reachable set is the compaction proof. Bind the
    // resulting exact pack inventory before the manifest CAS.
    new_manifest.seal_git_validation();

    crate::metadata::manifest::write_manifest_cas(store, router, &new_manifest, &etag).await?;

    info!(
        shards_removed = actual_shards_removed,
        packs_removed = actual_packs_removed,
        "compaction complete, manifest updated"
    );

    Ok(CompactionResult {
        shards_removed: actual_shards_removed,
        packs_removed: actual_packs_removed,
        manifest_updated: true,
    })
}

// ---------------------------------------------------------------------------
// Orphaned bulk manifest object cleanup
// ---------------------------------------------------------------------------

/// Identify and delete orphaned bulk manifest objects not referenced by
/// the current manifest pointer (`shard-list-{hash}` and
/// `pack-list-{hash}` objects under `{repo}/manifests/`).
///
/// Objects within the grace period are retained. Returns the number of
/// orphaned objects deleted (or would-be-deleted in dry-run).
pub async fn cleanup_orphaned_bulk_objects(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    reachable_bulk_keys: &HashSet<String, impl std::hash::BuildHasher>,
    grace_period: Duration,
    dry_run: bool,
) -> Result<u64> {
    use futures_util::TryStreamExt;
    use object_store::ObjectStore;

    let manifests_prefix = router.repo_path("manifests/");
    let list_result = store.inner().list(Some(&manifests_prefix));

    let objects: Vec<_> = list_result.try_collect().await.map_err(|e| {
        CrabError::from(crab_storage::map_object_store_error(
            e,
            manifests_prefix.as_ref(),
        ))
    })?;

    let t0 = SystemTime::now();
    let cutoff = t0 - grace_period.max(MIN_GRACE_PERIOD);
    let mut deleted = 0u64;

    for meta in &objects {
        let key = meta.location.as_ref().to_string();

        if reachable_bulk_keys.contains(&key) {
            continue;
        }

        let last_modified: SystemTime = meta.last_modified.into();
        if last_modified >= cutoff {
            debug!(key = %key, "orphaned bulk object within grace period, skipping");
            continue;
        }

        if dry_run {
            info!(key = %key, "would delete orphaned bulk object (dry-run)");
            deleted += 1;
        } else {
            match store.delete(&meta.location).await {
                Ok(()) => {
                    info!(key = %key, "deleted orphaned bulk object");
                    deleted += 1;
                }
                Err(e) => {
                    warn!(key = %key, error = %e, "failed to delete orphaned bulk object");
                }
            }
        }
    }

    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingDeleter {
        fail_delete_for: Option<String>,
        fail_reconcile: bool,
    }

    impl ObjectDeleter for FailingDeleter {
        fn delete(
            &self,
            key: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
            let should_fail = self.fail_delete_for.as_deref() == Some(key);
            Box::pin(async move {
                if should_fail {
                    return Err(CrabError::Internal("injected delete failure".to_owned()));
                }
                Ok(())
            })
        }

        fn reconcile_manifest(
            &self,
            _deleted_keys: &[String],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
            let should_fail = self.fail_reconcile;
            Box::pin(async move {
                if should_fail {
                    return Err(CrabError::Internal(
                        "injected reconciliation failure".to_owned(),
                    ));
                }
                Ok(())
            })
        }
    }

    fn make_obj(key: &str, size: u64, age: Duration) -> ObjectMeta {
        ObjectMeta {
            key: key.to_string(),
            size,
            last_modified: SystemTime::now() - age,
            e_tag: None,
            version: None,
            storage_class: None,
            transitioned_at: None,
        }
    }

    fn make_obj_at(key: &str, size: u64, time: SystemTime) -> ObjectMeta {
        ObjectMeta {
            key: key.to_string(),
            size,
            last_modified: time,
            e_tag: None,
            version: None,
            storage_class: None,
            transitioned_at: None,
        }
    }

    // --- Grace-period filter ---

    #[test]
    fn grace_filter_retains_recent_objects() {
        let t0 = SystemTime::now();
        let grace = Duration::from_secs(24 * 3600);
        let recent = make_obj_at("xorbs/ab/obj1", 100, t0 - Duration::from_secs(3600));
        let result = apply_grace_filter(vec![recent], t0, grace, false);
        assert!(result.is_empty(), "recent object should be retained");
    }

    #[test]
    fn grace_filter_deletes_old_objects() {
        let t0 = SystemTime::now();
        let grace = Duration::from_secs(24 * 3600);
        let old = make_obj_at("xorbs/ab/obj2", 200, t0 - Duration::from_secs(48 * 3600));
        let result = apply_grace_filter(vec![old], t0, grace, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].key, "xorbs/ab/obj2");
    }

    #[test]
    fn grace_filter_force_bypasses_grace() {
        let t0 = SystemTime::now();
        let grace = Duration::from_secs(24 * 3600);
        let recent = make_obj_at("xorbs/ab/obj3", 50, t0 - Duration::from_secs(1));
        let result = apply_grace_filter(vec![recent], t0, grace, true);
        assert_eq!(result.len(), 1, "force should bypass grace period");
    }

    #[test]
    fn grace_filter_clamps_to_minimum_one_hour() {
        let t0 = SystemTime::now();
        let grace = Duration::from_secs(10);
        let obj = make_obj_at("xorbs/ab/obj4", 100, t0 - Duration::from_secs(1800));
        let result = apply_grace_filter(vec![obj], t0, grace, false);
        assert!(result.is_empty(), "should be retained by clamped 1h grace");
    }

    #[test]
    fn grace_filter_empty_input() {
        let t0 = SystemTime::now();
        let result = apply_grace_filter(vec![], t0, Duration::from_secs(3600), false);
        assert!(result.is_empty());
    }

    // --- Unreachable set computation ---

    #[test]
    fn unreachable_excludes_reachable_keys() {
        let reachable: HashSet<String> = ["xorbs/ab/obj1", "packs/cd/pack1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let snapshot = ShardListSnapshot {
            generation: 1,
            shard_keys: HashSet::new(),
        };
        let protected: HashSet<String> = HashSet::new();
        let listed = vec![
            make_obj("xorbs/ab/obj1", 100, Duration::from_secs(3600)),
            make_obj("xorbs/ef/obj2", 200, Duration::from_secs(3600)),
            make_obj("packs/cd/pack1", 300, Duration::from_secs(3600)),
        ];
        let unreachable = compute_unreachable(listed, &reachable, &protected, &snapshot);
        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0].key, "xorbs/ef/obj2");
    }

    #[test]
    fn unreachable_excludes_post_t0_shards() {
        let reachable: HashSet<String> = HashSet::new();
        let snapshot = ShardListSnapshot {
            generation: 1,
            shard_keys: ["shards/ab/shard1"].iter().map(|s| s.to_string()).collect(),
        };
        let protected: HashSet<String> = HashSet::new();
        let listed = vec![
            make_obj("shards/ab/shard1", 100, Duration::from_secs(3600)),
            make_obj("shards/cd/shard2", 200, Duration::from_secs(60)),
        ];
        let unreachable = compute_unreachable(listed, &reachable, &protected, &snapshot);
        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0].key, "shards/ab/shard1");
    }

    #[test]
    fn unreachable_non_shard_objects_not_filtered_by_snapshot() {
        let reachable: HashSet<String> = HashSet::new();
        let snapshot = ShardListSnapshot {
            generation: 1,
            shard_keys: HashSet::new(),
        };
        let protected: HashSet<String> = HashSet::new();
        let listed = vec![
            make_obj("xorbs/ab/obj1", 100, Duration::from_secs(60)),
            make_obj("packs/cd/pack1", 200, Duration::from_secs(60)),
        ];
        let unreachable = compute_unreachable(listed, &reachable, &protected, &snapshot);
        assert_eq!(unreachable.len(), 2);
    }

    #[test]
    fn unreachable_excludes_coordinator_protected_keys() {
        let reachable: HashSet<String> = HashSet::new();
        let protected: HashSet<String> = ["xorbs/ab/pending", "packs/cd/committed"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let snapshot = ShardListSnapshot {
            generation: 1,
            shard_keys: HashSet::new(),
        };
        let listed = vec![
            make_obj("xorbs/ab/pending", 100, Duration::from_secs(3600)),
            make_obj("packs/cd/committed", 200, Duration::from_secs(3600)),
            make_obj("xorbs/ef/free", 300, Duration::from_secs(3600)),
        ];

        let unreachable = compute_unreachable(listed, &reachable, &protected, &snapshot);

        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0].key, "xorbs/ef/free");
    }

    // --- Key categorization ---

    #[test]
    fn categorize_key_identifies_dimensions() {
        assert_eq!(categorize_key("packs/ab/pack1"), ObjectCategory::Pack);
        assert_eq!(categorize_key("xorbs/cd/xorb1"), ObjectCategory::Xorb);
        assert_eq!(categorize_key("shards/ef/shard1"), ObjectCategory::Shard);
        assert_eq!(categorize_key("file-index/gh/fi1"), ObjectCategory::Other);
    }

    #[test]
    fn pack_storage_classes_separate_active_history_grace_and_collectible() {
        let objects = vec![
            make_obj("packs/aa/active.pack", 10, Duration::from_secs(48 * 3600)),
            make_obj("packs/bb/history.pack", 20, Duration::from_secs(48 * 3600)),
            make_obj("packs/cc/grace.pack", 30, Duration::from_secs(30)),
            make_obj("packs/dd/collect.pack", 40, Duration::from_secs(48 * 3600)),
        ];
        let current = HashSet::from(["packs/aa/active.pack".to_owned()]);
        let reachable = HashSet::from([
            "packs/aa/active.pack".to_owned(),
            "packs/bb/history.pack".to_owned(),
        ]);

        let classes = classify_pack_storage(
            &objects,
            &current,
            &reachable,
            &HashSet::new(),
            Duration::from_secs(3600),
            false,
        );

        assert_eq!(
            classes,
            PackStorageClasses {
                active: 10,
                retained: 20,
                grace: 30,
                collectible: 40,
            }
        );
    }

    // --- Dry-run ---

    #[tokio::test]
    async fn dry_run_produces_zero_actual_deletes() {
        let args = GcArgs {
            dry_run: true,
            force: false,
            yes: false,
            ..GcArgs::default()
        };
        let objects = vec![
            make_obj("xorbs/ab/obj1", 100, Duration::from_secs(48 * 3600)),
            make_obj("packs/cd/pack1", 200, Duration::from_secs(48 * 3600)),
            make_obj("shards/ef/shard1", 300, Duration::from_secs(48 * 3600)),
        ];
        let reachable: HashSet<String> = HashSet::new();
        let snapshot = ShardListSnapshot {
            generation: 1,
            shard_keys: ["shards/ef/shard1"].iter().map(|s| s.to_string()).collect(),
        };
        let protected: HashSet<String> = HashSet::new();
        let cancel = CancellationToken::new();
        let list_outcome = ListOutcome {
            requests: 256,
            parallelism: 32,
            wall_seconds: 1.5,
            failed_prefixes: Vec::new(),
        };
        let deleter = NullDeleter;

        let outcome = run_gc(
            &args,
            objects,
            &reachable,
            &protected,
            &snapshot,
            &cancel,
            64,
            Duration::from_secs(24 * 3600),
            list_outcome,
            &deleter,
            None,
        )
        .await
        .expect("dry-run should succeed");

        assert!(outcome.dry_run);
        assert_eq!(outcome.xorbs_deleted, 1);
        assert_eq!(outcome.packs_deleted, 1);
        assert_eq!(outcome.shards_deleted, 1);
        assert_eq!(outcome.bytes_reclaimed, 600);
    }

    // --- Cancellation ---

    #[tokio::test]
    async fn cancellation_before_delete_returns_cancelled() {
        let args = GcArgs::default();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let reachable: HashSet<String> = HashSet::new();
        let protected: HashSet<String> = HashSet::new();

        let result = run_gc(
            &args,
            vec![],
            &reachable,
            &protected,
            &ShardListSnapshot {
                generation: 0,
                shard_keys: HashSet::new(),
            },
            &cancel,
            64,
            Duration::from_secs(3600),
            ListOutcome::default(),
            &NullDeleter,
            None,
        )
        .await;

        assert!(matches!(result, Err(CrabError::Cancelled)));
    }

    // --- GcOutcome ---

    #[test]
    fn outcome_default_is_zeroed() {
        let o = GcOutcome::default();
        assert_eq!(o.packs_deleted, 0);
        assert_eq!(o.xorbs_deleted, 0);
        assert_eq!(o.shards_deleted, 0);
        assert_eq!(o.bytes_reclaimed, 0);
        assert!(!o.dry_run);
        assert!(!o.cancelled);
    }

    // --- Force confirmation ---

    #[test]
    fn confirm_force_returns_true_when_not_force() {
        let args = GcArgs::default();
        assert!(confirm_force(&args).expect("should succeed"));
    }

    #[test]
    fn confirm_force_with_yes_flag() {
        let args = GcArgs {
            force: true,
            yes: true,
            ..GcArgs::default()
        };
        assert!(confirm_force(&args).expect("should succeed with --yes"));
    }

    // --- Full GC flow with deletes ---

    #[tokio::test]
    async fn gc_deletes_unreachable_objects() {
        let args = GcArgs::default();
        let objects = vec![
            make_obj("xorbs/ab/obj1", 100, Duration::from_secs(48 * 3600)),
            make_obj("xorbs/ab/obj2", 200, Duration::from_secs(48 * 3600)),
            make_obj("packs/cd/pack1", 300, Duration::from_secs(48 * 3600)),
        ];
        let reachable: HashSet<String> = ["xorbs/ab/obj1"].iter().map(|s| s.to_string()).collect();
        let protected: HashSet<String> = HashSet::new();
        let snapshot = ShardListSnapshot {
            generation: 1,
            shard_keys: HashSet::new(),
        };
        let cancel = CancellationToken::new();
        let deleter = NullDeleter;

        let outcome = run_gc(
            &args,
            objects,
            &reachable,
            &protected,
            &snapshot,
            &cancel,
            64,
            Duration::from_secs(24 * 3600),
            ListOutcome::default(),
            &deleter,
            None,
        )
        .await
        .expect("gc should succeed");

        assert!(!outcome.dry_run);
        assert_eq!(outcome.xorbs_deleted, 1);
        assert_eq!(outcome.packs_deleted, 1);
        assert_eq!(outcome.bytes_reclaimed, 500);
    }

    #[tokio::test]
    async fn gc_returns_error_when_any_delete_fails() {
        let objects = vec![
            make_obj("xorbs/ab/good", 100, Duration::from_secs(48 * 3600)),
            make_obj("xorbs/ab/bad", 200, Duration::from_secs(48 * 3600)),
        ];
        let deleter = FailingDeleter {
            fail_delete_for: Some("xorbs/ab/bad".to_owned()),
            fail_reconcile: false,
        };

        let result = run_gc(
            &GcArgs::default(),
            objects,
            &HashSet::<String>::new(),
            &HashSet::<String>::new(),
            &ShardListSnapshot {
                generation: 0,
                shard_keys: HashSet::new(),
            },
            &CancellationToken::new(),
            2,
            Duration::from_secs(3600),
            ListOutcome::default(),
            &deleter,
            None,
        )
        .await;

        assert!(matches!(
            result,
            Err(CrabError::GcPartialFailure {
                objects_deleted: 1,
                delete_failures: 1,
                reconciliation_failed: false,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn gc_returns_error_when_reconciliation_fails() {
        let deleter = FailingDeleter {
            fail_delete_for: None,
            fail_reconcile: true,
        };

        let result = run_gc(
            &GcArgs::default(),
            vec![make_obj(
                "packs/ab/good",
                100,
                Duration::from_secs(48 * 3600),
            )],
            &HashSet::<String>::new(),
            &HashSet::<String>::new(),
            &ShardListSnapshot {
                generation: 0,
                shard_keys: HashSet::new(),
            },
            &CancellationToken::new(),
            1,
            Duration::from_secs(3600),
            ListOutcome::default(),
            &deleter,
            None,
        )
        .await;

        assert!(matches!(
            result,
            Err(CrabError::GcPartialFailure {
                objects_deleted: 1,
                delete_failures: 0,
                reconciliation_failed: true,
                ..
            })
        ));
    }

    // --- Manifest-aware GC reachability (Task 8.4) ---

    #[tokio::test]
    async fn gc_identifies_unreachable_objects_via_manifest() {
        use crate::metadata::manifest::{
            BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index,
            create_manifest, upload_segmented_bulk,
        };
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_string());

        // Create a manifest with known shard and pack hashes.
        let shard_hashes = vec!["a".repeat(64), "b".repeat(64)];
        let pack_id = "c".repeat(64);
        let packs = vec![PackManifestEntry {
            pack_id: pack_id.clone(),
            size: 1024,
            content_hash: pack_id.clone(),
            ref_tips: vec!["a".repeat(40)],
            object_count: 1,
        }];
        let (shard_hash, _shard_index, shard_write) =
            compact_shard_index(1, &shard_hashes).unwrap();
        let (pack_hash, _pack_index, pack_write) = compact_pack_index(1, &packs).unwrap();
        let bulk = BulkData {
            shard_index: shard_write,
            pack_index: pack_write,
        };
        let shard_segment_path = bulk.shard_index.segments[0].reference.path.clone();
        let pack_segment_path = bulk.pack_index.segments[0].reference.path.clone();

        upload_segmented_bulk(&store, &router, &bulk).await.unwrap();

        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.shard_index_hash = shard_hash.clone();
        manifest.pack_index_hash = pack_hash.clone();
        manifest.generation = 1;
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "a".repeat(40));
        manifest.seal_git_validation();
        create_manifest(&store, &router, &manifest).await.unwrap();

        // Read reachable bulk objects.
        let (_m, reachable) = reachable_bulk_objects_from_manifest(&store, &router)
            .await
            .unwrap();

        // The reachable set should contain both segmented index objects and
        // the immutable segments they reference.
        assert_eq!(reachable.len(), 6);
        assert!(reachable.contains(&format!(
            "org/repo/metadata/shard/indexes/{shard_hash}.json"
        )));
        assert!(reachable.contains(&format!("org/repo/{shard_segment_path}")));
        assert!(reachable.contains(&format!("org/repo/metadata/pack/indexes/{pack_hash}.json")));
        assert!(reachable.contains(&format!("org/repo/{pack_segment_path}")));
        assert!(reachable.contains(&format!(
            "org/repo/metadata/git-visibility/v2/{}.json",
            manifest.git_validation_digest
        )));
        assert!(reachable.contains(&format!(
            "org/repo/metadata/git-visibility/{:020}-{pack_hash}.json",
            manifest.generation,
        )));

        // An object NOT in the reachable set is unreachable.
        assert!(!reachable.contains("org/repo/metadata/shard/indexes/deadbeef.json"));
    }

    #[tokio::test]
    async fn repo_gc_candidates_include_only_repo_local_immutable_prefixes() {
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use bytes::Bytes;
        use object_store::memory::InMemory;
        use object_store::path::Path as ObjectPath;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_string());
        for key in [
            "org/repo/packs/pack-old.pack",
            "org/repo/metadata/pack/indexes/old.json",
            "org/repo/manifests/pack-list-old",
            "org/repo/manifest",
            "org/repo/locks/refs/heads/main/lock",
            "org/repo/locks/internal/repack/lock",
            ".crab/xorbs/abc",
        ] {
            store
                .put(&ObjectPath::from(key), Bytes::from_static(b"data"))
                .await
                .unwrap();
        }

        let (candidates, outcome) = list_repo_gc_candidates(&store, &router).await.unwrap();
        let keys: HashSet<_> = candidates.into_iter().map(|object| object.key).collect();

        assert_eq!(outcome.requests, REPO_GC_PREFIXES.len() as u64);
        assert_eq!(outcome.parallelism, REPO_GC_PREFIXES.len());
        assert!(keys.contains("org/repo/packs/pack-old.pack"));
        assert!(keys.contains("org/repo/metadata/pack/indexes/old.json"));
        assert!(keys.contains("org/repo/manifests/pack-list-old"));
        assert!(!keys.contains("org/repo/manifest"));
        assert!(!keys.contains("org/repo/locks/refs/heads/main/lock"));
        assert!(!keys.contains("org/repo/locks/internal/repack/lock"));
        assert!(!keys.contains(".crab/xorbs/abc"));
    }

    #[tokio::test]
    async fn repo_gc_candidate_listing_honors_concurrency_limit() {
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());

        let (_, serial) = list_repo_gc_candidates_with_concurrency(&store, &router, 1)
            .await
            .unwrap();
        let (_, bounded) = list_repo_gc_candidates_with_concurrency(&store, &router, 2)
            .await
            .unwrap();

        assert_eq!(serial.parallelism, 1);
        assert_eq!(bounded.parallelism, 2);
    }

    #[tokio::test]
    async fn repo_gc_retains_referenced_artifacts_and_reclaims_orphan_payloads() {
        use crate::metadata::manifest::{Manifest, create_manifest};
        use bytes::Bytes;
        use crab_workflow::{
            ArtifactDecl, manifest_from_path, promote_remote_artifact, publish_remote_artifact,
        };
        use object_store::memory::InMemory;
        use object_store::path::Path as ObjectPath;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        create_manifest(
            &store,
            &router,
            &Manifest::default_for_repo("refs/heads/main"),
        )
        .await
        .unwrap();

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("model.bin");
        std::fs::write(&source, b"retained-model").unwrap();
        let declaration = ArtifactDecl {
            name: "model".to_owned(),
            path: "model.bin".to_owned(),
            kind: "model".to_owned(),
            description: None,
            labels: Vec::new(),
            metadata: std::collections::BTreeMap::new(),
        };
        let (artifact, source_path) = manifest_from_path(temp.path(), &declaration, None).unwrap();
        let workflow_store = crab_workflow::WorkflowStore::from_storage(store.clone().into());
        publish_remote_artifact(&workflow_store, "org/repo", &artifact, &source_path)
            .await
            .unwrap();
        promote_remote_artifact(
            &workflow_store,
            "org/repo",
            "model",
            &artifact.version_id,
            "production",
            None,
        )
        .await
        .unwrap();

        let orphan = ObjectPath::from("org/repo/workflow/artifacts/payloads/dead/file");
        store
            .put(&orphan, Bytes::from_static(b"orphan"))
            .await
            .unwrap();
        let args = GcArgs {
            force: true,
            yes: true,
            ..GcArgs::default()
        };
        let outcome = run_repo_remote_gc(
            &args,
            &store,
            &router,
            &HashSet::new(),
            &CancellationToken::new(),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

        assert!(outcome.bytes_reclaimed >= b"orphan".len() as u64);
        assert!(store.head(&orphan).await.is_err());
        let live_payload = ObjectPath::from(format!(
            "org/repo/workflow/artifacts/payloads/{}/file",
            artifact.content_hash.trim_start_matches("b3:")
        ));
        assert!(store.head(&live_payload).await.is_ok());
    }

    #[tokio::test]
    async fn reachable_repo_objects_include_live_pack_objects() {
        use crate::metadata::manifest::{
            BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index,
            create_manifest, upload_segmented_bulk,
        };
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_string());
        let pack_id = "c".repeat(64);
        let packs = vec![PackManifestEntry {
            pack_id: pack_id.clone(),
            size: 1024,
            content_hash: pack_id.clone(),
            ref_tips: vec!["a".repeat(40)],
            object_count: 1,
        }];
        let (shard_hash, _shard_index, shard_write) = compact_shard_index(1, &[]).unwrap();
        let (pack_hash, _pack_index, pack_write) = compact_pack_index(1, &packs).unwrap();
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: shard_write,
                pack_index: pack_write,
            },
        )
        .await
        .unwrap();

        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.shard_index_hash = shard_hash;
        manifest.pack_index_hash = pack_hash;
        manifest.generation = 1;
        manifest.seal_git_validation();
        create_manifest(&store, &router, &manifest).await.unwrap();

        let (_manifest, reachable) = reachable_repo_objects_from_manifest(&store, &router)
            .await
            .unwrap();

        assert!(reachable.contains("org/repo/manifest"));
        assert!(reachable.contains(&format!("org/repo/packs/pack-{pack_id}.pack")));
        assert!(reachable.contains(&format!("org/repo/packs/pack-{pack_id}.idx")));
        assert!(reachable.contains(&format!("org/repo/packs/pack-{pack_id}.rev")));
        assert!(reachable.contains(&format!("org/repo/packs/pack-{pack_id}.meta")));
    }

    #[tokio::test]
    async fn reachable_repo_objects_include_uncompacted_journal_pack_objects() {
        use std::collections::{BTreeMap, BTreeSet};
        use std::sync::Arc;

        use object_store::memory::InMemory;

        use crate::metadata::manifest::{
            Manifest, PackManifestEntry, RefJournalEdit, RefJournalTransaction,
            commit_ref_journal_transaction, create_manifest, read_ref_journal_head,
        };
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "a".repeat(40));
        manifest.seal_git_validation();
        create_manifest(&store, &router, &manifest).await.unwrap();
        let head = read_ref_journal_head(&store, &router, "refs/heads/side")
            .await
            .unwrap();
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let visibility_evidence_hash = crab_metadata::git_visibility::upload_edit(
            store.as_storage(),
            &storage_router,
            &crab_metadata::git_visibility::GitVisibilityEdit::replacement(
                None,
                "b".repeat(40),
                &BTreeSet::from(["b".repeat(40)]),
            ),
        )
        .await
        .unwrap();
        let pack_id = "c".repeat(64);
        let transaction = RefJournalTransaction::new(
            BTreeMap::from([("refs/heads/side".to_owned(), None)]),
            vec![RefJournalEdit {
                ref_name: "refs/heads/side".to_owned(),
                old_oid: None,
                new_oid: Some("b".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: Some(visibility_evidence_hash.clone()),
            }],
            None,
            vec![PackManifestEntry {
                pack_id: pack_id.clone(),
                size: 1024,
                content_hash: pack_id.clone(),
                ref_tips: vec!["b".repeat(40)],
                object_count: 1,
            }],
            Vec::new(),
        )
        .unwrap();
        commit_ref_journal_transaction(&store, &router, &transaction, &[head])
            .await
            .unwrap();

        let (_, reachable) = reachable_repo_objects_from_manifest(&store, &router)
            .await
            .unwrap();

        assert!(reachable.contains(&format!("org/repo/packs/pack-{pack_id}.pack")));
        assert!(
            reachable.contains(
                router
                    .git_visibility_edit_path(&visibility_evidence_hash)
                    .as_ref()
            )
        );
    }

    #[tokio::test]
    async fn reachable_repo_objects_include_history_only_pack_objects() {
        use crate::metadata::manifest::{
            BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index,
            create_manifest, read_manifest, upload_segmented_bulk, write_manifest_cas,
        };
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let old_pack_id = "c".repeat(64);
        let old_packs = vec![PackManifestEntry {
            pack_id: old_pack_id.clone(),
            size: 1024,
            content_hash: old_pack_id.clone(),
            ref_tips: vec!["a".repeat(40)],
            object_count: 1,
        }];
        let (old_shard_hash, _, old_shard_write) = compact_shard_index(1, &[]).unwrap();
        let (old_pack_hash, _, old_pack_write) = compact_pack_index(1, &old_packs).unwrap();
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

        let (_, reachable) = reachable_repo_objects_from_manifest(&store, &router)
            .await
            .unwrap();

        assert!(reachable.contains(&format!("org/repo/packs/pack-{old_pack_id}.pack")));
        assert!(
            reachable
                .iter()
                .any(|path| path.contains("manifests/history/"))
        );
    }

    #[tokio::test]
    async fn reachable_repo_objects_reject_corrupt_history() {
        use crate::metadata::manifest::{Manifest, create_manifest};
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use bytes::Bytes;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        create_manifest(
            &store,
            &router,
            &Manifest::default_for_repo("refs/heads/main"),
        )
        .await
        .unwrap();
        store
            .put(
                &router.repo_path(&format!(
                    "manifests/history/{:020}-{}.json",
                    0,
                    "0".repeat(64)
                )),
                Bytes::from_static(b"corrupt"),
            )
            .await
            .unwrap();

        assert!(
            reachable_repo_objects_from_manifest(&store, &router)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn repo_remote_gc_retains_coordinator_protected_orphans() {
        use crate::metadata::manifest::{Manifest, create_manifest};
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use bytes::Bytes;
        use object_store::memory::InMemory;
        use object_store::path::Path as ObjectPath;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_string());
        let manifest = Manifest::default_for_repo("refs/heads/main");
        create_manifest(&store, &router, &manifest).await.unwrap();

        let protected_key = "org/repo/packs/pack-protected.pack";
        let free_key = "org/repo/packs/pack-free.pack";
        store
            .put(
                &ObjectPath::from(protected_key),
                Bytes::from_static(b"protected"),
            )
            .await
            .unwrap();
        store
            .put(&ObjectPath::from(free_key), Bytes::from_static(b"free"))
            .await
            .unwrap();

        let args = GcArgs {
            force: true,
            yes: true,
            ..GcArgs::default()
        };
        let protected: HashSet<String> = [protected_key.to_owned()].into_iter().collect();
        let cancel = CancellationToken::new();

        let outcome = run_repo_remote_gc(
            &args,
            &store,
            &router,
            &protected,
            &cancel,
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

        assert_eq!(outcome.packs_deleted, 1);
        assert!(store.head(&ObjectPath::from(protected_key)).await.is_ok());
        assert!(matches!(
            store.head(&ObjectPath::from(free_key)).await,
            Err(CrabError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn gc_writer_fence_blocks_destructive_repo_gc_but_not_preview() {
        use crate::metadata::manifest::{Manifest, create_manifest};
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use crab_coordination::{DEFAULT_GC_FENCE_TTL, GcFenceLease};
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        create_manifest(
            &store,
            &router,
            &Manifest::default_for_repo("refs/heads/main"),
        )
        .await
        .unwrap();
        let writer =
            GcFenceLease::acquire_writer(store.inner(), router.repo_prefix(), DEFAULT_GC_FENCE_TTL)
                .await
                .unwrap();

        let error = run_repo_remote_gc(
            &GcArgs::default(),
            &store,
            &router,
            &HashSet::new(),
            &CancellationToken::new(),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CrabError::PushLockHeld { .. }));

        let preview = run_repo_remote_gc(
            &GcArgs {
                dry_run: true,
                ..GcArgs::default()
            },
            &store,
            &router,
            &HashSet::new(),
            &CancellationToken::new(),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();
        assert!(preview.dry_run);
        writer.release().await.unwrap();
    }

    #[tokio::test]
    async fn resumed_delete_retains_recreated_object_inside_grace_period() {
        use std::sync::Arc;

        use bytes::Bytes;
        use object_store::memory::InMemory;

        use crate::storage::store::Store;

        let store = Store::new(Arc::new(InMemory::new()));
        let key = "repo/packs/recreated.pack";
        let path = ObjectPath::from(key);
        store.put(&path, Bytes::from_static(b"old")).await.unwrap();
        let original = store.head(&path).await.unwrap();

        let snapshot_at = SystemTime::now();
        let mut journal = journal::GcRunJournal::start(
            store.clone(),
            "repo",
            "repo",
            "repo",
            snapshot_at,
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.set_root_identity("root").await.unwrap();
        journal
            .plan(&[ObjectMeta {
                key: key.to_owned(),
                size: original.size,
                last_modified: snapshot_at - Duration::from_secs(7200),
                e_tag: original.e_tag,
                version: original.version,
                storage_class: None,
                transitioned_at: None,
            }])
            .await
            .unwrap();

        // Model a crash after the provider accepted DELETE but before the
        // journal outcome advanced. A writer may then recreate the same key.
        store.delete(&path).await.unwrap();
        store.put(&path, Bytes::from_static(b"new")).await.unwrap();

        let mut outcome = GcOutcome::default();
        let deleter = StoreObjectDeleter::new(store.clone());
        let seal =
            crate::maintenance::GcSweepLease::acquire(&store, "repo", &CancellationToken::new())
                .await
                .unwrap();
        journal.seal_fence_epoch(seal.epoch()).await.unwrap();
        seal.release().await.unwrap();
        let delete_outcome = execute_journaled_deletes(
            &mut journal,
            &CancellationToken::new(),
            1,
            &deleter,
            &mut outcome,
            None,
            Some((&store, "repo")),
        )
        .await;

        assert!(delete_outcome.first_error.is_none());
        assert!(store.head(&path).await.is_ok());
    }

    // --- GC compaction (Task 8.5) ---

    #[tokio::test]
    async fn gc_compaction_reduces_bulk_list_size() {
        use crate::metadata::manifest::{
            BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index,
            create_manifest, read_bulk_pack_list, read_bulk_shard_list, read_manifest,
            upload_segmented_bulk,
        };
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_string());

        // Create a manifest with 3 shards and 2 packs.
        let shard_hashes = vec!["a".repeat(64), "b".repeat(64), "c".repeat(64)];
        let pack_1 = "d".repeat(64);
        let pack_2 = "e".repeat(64);
        let packs = vec![
            PackManifestEntry {
                pack_id: pack_1.clone(),
                size: 1024,
                content_hash: pack_1.clone(),
                ref_tips: vec!["a".repeat(40)],
                object_count: 1,
            },
            PackManifestEntry {
                pack_id: pack_2.clone(),
                size: 2048,
                content_hash: pack_2.clone(),
                ref_tips: vec!["b".repeat(40)],
                object_count: 2,
            },
        ];
        let (shard_hash, _shard_index, shard_write) =
            compact_shard_index(1, &shard_hashes).unwrap();
        let (pack_hash, _pack_index, pack_write) = compact_pack_index(1, &packs).unwrap();
        let bulk = BulkData {
            shard_index: shard_write,
            pack_index: pack_write,
        };

        upload_segmented_bulk(&store, &router, &bulk).await.unwrap();

        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.shard_index_hash = shard_hash;
        manifest.pack_index_hash = pack_hash;
        manifest.generation = 1;
        manifest.seal_git_validation();
        create_manifest(&store, &router, &manifest).await.unwrap();

        // Compact: only keep shard "a" and pack "pack_1".
        let reachable_shards: HashSet<String> = ["a".repeat(64)].into_iter().collect();
        let reachable_packs: HashSet<String> = [pack_1.clone()].into_iter().collect();

        let result = compact_bulk_lists(&store, &router, &reachable_shards, &reachable_packs)
            .await
            .unwrap();

        assert!(result.manifest_updated);
        assert_eq!(result.shards_removed, 2); // removed "b" and "c"
        assert_eq!(result.packs_removed, 1); // removed "pack_2"

        // Verify the manifest now points to smaller bulk lists.
        let (new_manifest, _etag) = read_manifest(&store, &router).await.unwrap();
        let new_shards = read_bulk_shard_list(&store, &router, &new_manifest.shard_index_hash)
            .await
            .unwrap();
        assert_eq!(new_shards.len(), 1);
        assert_eq!(new_shards[0], "a".repeat(64));

        let new_packs = read_bulk_pack_list(&store, &router, &new_manifest.pack_index_hash)
            .await
            .unwrap();
        assert_eq!(new_packs.len(), 1);
        assert_eq!(new_packs[0].pack_id, pack_1);
    }
}
