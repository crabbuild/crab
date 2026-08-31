//! File-backed consolidation of the Git packs selected by a repository manifest.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

#[cfg(test)]
use std::process::Command;

use futures_util::StreamExt;
use schemars::JsonSchema;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::coordination::heartbeat::LockHeartbeat;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::push::{
    CommittedManifestAnchor, CommittedPackIndex, publish_committed_pack_locators,
};
use crate::metadata::manifest::{
    BulkData, Manifest, PackManifestEntry, compact_pack_index, read_manifest,
    upload_segmented_bulk, write_manifest_cas,
};
use crate::storage::StoreLayout;
use crate::storage::store::Store;
use crab_coordination::PushLock;
use crab_storage::{repo_pack_index_path, repo_pack_path, repo_pack_reverse_index_path};
use crab_xet::hash::MerkleHash;

const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;
const REPACK_DISK_RESERVE: u64 = 1024 * 1024 * 1024;
const MAX_PACKS_PER_OPERATION: u64 = 1_000_000;
const MAX_REPACK_DOWNLOAD_CONCURRENCY: usize = 16;
// The owner rolls up a bounded suffix so repeated cycles make progress
// without allowing one repository to monopolize maintenance or disk I/O.
// A batch is restartable after a lease interruption and accounts for all
// three immutable source artifacts per pack.
const GENERATION_OWNER_REPACK_MAX_SOURCE_PACKS: usize = 128;
const GENERATION_OWNER_REPACK_MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const GENERATION_OWNER_REPACK_MAX_SOURCE_REQUESTS: u64 = 384;
const GENERATION_OWNER_REPACK_MAX_ELAPSED: Duration = Duration::from_mins(10);
const SOURCE_ARTIFACT_REQUESTS_PER_PACK: u64 = 3;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RepackBudget {
    source_pack_limit: usize,
    source_byte_limit: u64,
    source_request_limit: u64,
    elapsed_limit: Duration,
}

impl RepackBudget {
    pub(crate) const fn generation_owner() -> Self {
        Self {
            source_pack_limit: GENERATION_OWNER_REPACK_MAX_SOURCE_PACKS,
            source_byte_limit: GENERATION_OWNER_REPACK_MAX_SOURCE_BYTES,
            source_request_limit: GENERATION_OWNER_REPACK_MAX_SOURCE_REQUESTS,
            elapsed_limit: GENERATION_OWNER_REPACK_MAX_ELAPSED,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RepackDeferral {
    resource: &'static str,
    actual: u64,
    maximum: u64,
}

#[derive(Debug)]
pub(crate) enum RepackRunResult {
    Completed {
        outcome: RepackOutcome,
        bounded: bool,
    },
    Deferred {
        resource: &'static str,
        actual: u64,
        maximum: u64,
    },
}

#[derive(Debug)]
struct RepackSelection {
    count: usize,
    bytes: u64,
    bounded: bool,
}
/// Configuration for the repack operation.
#[derive(Debug, Clone)]
pub struct RepackConfig {
    /// Push lock TTL during repack.
    pub lock_ttl: Duration,
    /// Whether this is a dry-run (report stats without modifying remote).
    pub dry_run: bool,
    /// Maximum number of concurrent pack/index downloads.
    pub download_concurrency: usize,
    /// Maximum CAS retries while repairing pack metadata sidecars.
    pub max_cas_retries: u32,
    /// Parent directory for bounded, automatically removed repack files.
    pub workspace_root: std::path::PathBuf,
}

impl Default for RepackConfig {
    fn default() -> Self {
        Self {
            lock_ttl: Duration::from_mins(5),
            dry_run: false,
            download_concurrency: 8,
            max_cas_retries: 64,
            workspace_root: crate::cache::default_cache_root().join("maintenance"),
        }
    }
}

/// Outcome of a repack operation.
#[derive(Debug, Clone)]
pub struct RepackOutcome {
    /// Number of packs before repack.
    pub packs_before: usize,
    /// Number of packs after repack.
    pub packs_after: usize,
    /// Total bytes across all packs before repack.
    pub bytes_before: u64,
    /// Total bytes across all packs after repack.
    pub bytes_after: u64,
    /// Pack body bytes downloaded by this bounded roll-up.
    pub bytes_read: u64,
    /// New pack body bytes uploaded by this bounded roll-up.
    pub bytes_written: u64,
    /// Wall-clock time for the operation.
    pub elapsed: Duration,
}

impl RepackOutcome {
    /// Convert to the structured output summary payload.
    pub fn to_summary(&self) -> RepackSummary {
        RepackSummary {
            packs_before: self.packs_before as u64,
            packs_after: self.packs_after as u64,
            bytes_before: self.bytes_before,
            bytes_after: self.bytes_after,
            bytes_read: self.bytes_read,
            bytes_written: self.bytes_written,
            elapsed_ms: self.elapsed.as_millis() as u64,
        }
    }
}

/// Terminal result payload for structured output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RepackSummary {
    /// Number of packs before repack.
    pub packs_before: u64,
    /// Number of packs after repack.
    pub packs_after: u64,
    /// Total bytes across all packs before repack.
    pub bytes_before: u64,
    /// Total bytes across all packs after repack.
    pub bytes_after: u64,
    /// Pack body bytes read from object storage.
    #[serde(default)]
    pub bytes_read: u64,
    /// New pack body bytes written to object storage.
    #[serde(default)]
    pub bytes_written: u64,
    /// Wall-clock duration in milliseconds.
    pub elapsed_ms: u64,
}

struct RepackedPack<'a> {
    generated: &'a crab_git::repack::GeometricRepackedPack,
    entry: PackManifestEntry,
}

/// Consolidate all packs selected by one pinned manifest generation.
pub async fn run_repack(
    store: &Store,
    prefix: &str,
    config: &RepackConfig,
    cancel: &CancellationToken,
) -> Result<RepackOutcome> {
    match run_repack_with_budget(store, prefix, config, cancel, None).await? {
        RepackRunResult::Completed { outcome, .. } => Ok(outcome),
        RepackRunResult::Deferred { .. } => Err(CrabError::Internal(
            "unbounded repack unexpectedly exceeded a maintenance budget".to_owned(),
        )),
    }
}

pub(crate) async fn run_bounded_repack(
    store: &Store,
    prefix: &str,
    config: &RepackConfig,
    budget: RepackBudget,
    cancel: &CancellationToken,
) -> Result<RepackRunResult> {
    run_repack_with_budget(store, prefix, config, cancel, Some(budget)).await
}

async fn run_repack_with_budget(
    store: &Store,
    prefix: &str,
    config: &RepackConfig,
    cancel: &CancellationToken,
    budget: Option<RepackBudget>,
) -> Result<RepackRunResult> {
    let start = Instant::now();
    check_cancelled(cancel)?;
    let router = StoreLayout::new(store.clone(), prefix.to_owned());
    if config.download_concurrency == 0 {
        return Err(CrabError::Configuration {
            key: "download_concurrency".to_owned(),
            origin: "must be greater than zero".to_owned(),
        });
    }
    let download_concurrency = config
        .download_concurrency
        .min(MAX_REPACK_DOWNLOAD_CONCURRENCY);
    let gc_writer = if config.dry_run {
        None
    } else {
        Some(
            crate::maintenance::GcWriterLeases::acquire(
                store,
                router.global_prefix(),
                router.repo_prefix(),
                cancel,
            )
            .await?,
        )
    };
    let lock = match PushLock::acquire_internal(
        store.inner(),
        router.repo_prefix(),
        crab_coordination::REPOSITORY_MAINTENANCE_RESOURCE,
        config.lock_ttl,
    )
    .await
    {
        Ok(lock) => lock,
        Err(error) => {
            if let Some(lease) = gc_writer {
                let _ = lease.release().await;
            }
            return Err(error.into());
        }
    };
    let operation_cancel = cancel.child_token();
    let heartbeat = LockHeartbeat::spawn(
        store.clone(),
        lock.path().to_owned(),
        lock.holder().to_owned(),
        lock.ttl(),
        lock.ttl() / 3,
        operation_cancel.clone(),
    );
    let result = run_repack_locked(
        store,
        &router,
        config,
        download_concurrency,
        &operation_cancel,
        start,
        budget.as_ref(),
    )
    .await;
    heartbeat.stop().await;
    let release_result = lock.release().await.map_err(CrabError::from);
    let gc_release_result = match gc_writer {
        Some(lease) => lease.release().await,
        None => Ok(()),
    };
    match (result, release_result, gc_release_result) {
        (Ok(outcome), Ok(()), Ok(())) => Ok(outcome),
        (Err(error), _, _) | (Ok(_), Err(error), _) | (Ok(_), Ok(()), Err(error)) => Err(error),
    }
}

async fn run_repack_locked(
    store: &Store,
    router: &StoreLayout,
    config: &RepackConfig,
    download_concurrency: usize,
    cancel: &CancellationToken,
    start: Instant,
    budget: Option<&RepackBudget>,
) -> Result<RepackRunResult> {
    let (manifest, manifest_etag) = read_manifest(store, router).await?;
    let mut packs = tokio::select! {
        result = crate::metadata::manifest::read_bulk_pack_list_with_limit(
            store,
            router,
            &manifest.pack_index_hash,
            MAX_PACKS_PER_OPERATION,
        ) => result?,
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
    };
    validate_pack_inventory(router, &packs)?;
    let packs_before = packs.len();
    let bytes_before = packs.iter().try_fold(0u64, |total, pack| {
        total
            .checked_add(pack.size)
            .ok_or_else(|| CrabError::Internal("pack inventory byte total overflow".to_owned()))
    })?;
    if budget.is_some() {
        packs.sort_unstable_by(|left, right| {
            right
                .size
                .cmp(&left.size)
                .then_with(|| right.object_count.cmp(&left.object_count))
                .then_with(|| left.pack_id.cmp(&right.pack_id))
        });
    } else {
        packs.sort_unstable_by(|left, right| {
            right
                .object_count
                .cmp(&left.object_count)
                .then_with(|| left.pack_id.cmp(&right.pack_id))
        });
    }
    let geometric_count = if budget.is_some() {
        generation_owner_repack_count(&packs)
    } else {
        crab_git::repack::geometric_repack_cut(
            &packs
                .iter()
                .map(|pack| pack.object_count)
                .collect::<Vec<_>>(),
            2,
        )
    };
    let selection = match select_repack_packs(&packs, geometric_count, budget, start.elapsed())? {
        Ok(selection) => selection,
        Err(deferral) => {
            return Ok(deferral_result(deferral));
        }
    };
    let selected_count = selection.count;
    if selected_count == 0 {
        return Ok(RepackRunResult::Completed {
            outcome: outcome(
                packs_before,
                packs_before,
                bytes_before,
                bytes_before,
                0,
                0,
                start,
            ),
            bounded: false,
        });
    }
    if config.dry_run {
        return Ok(RepackRunResult::Completed {
            outcome: outcome(
                packs_before,
                packs_before
                    .saturating_sub(selected_count)
                    .saturating_add(1),
                bytes_before,
                bytes_before,
                0,
                0,
                start,
            ),
            bounded: false,
        });
    }
    let selected_at = packs.len().saturating_sub(selected_count);
    let (stable_packs, selected_packs) = packs.split_at(selected_at);
    let stable_packs = stable_packs.to_vec();
    let selected_packs = selected_packs.to_vec();
    let bytes_read = selection.bytes;
    if let Some(deferral) = elapsed_budget_deferral(budget, start.elapsed()) {
        return Ok(deferral_result(deferral));
    }
    let visibility = read_current_visibility(store, router, &manifest).await?;
    let commit_graph = read_current_commit_graph(store, router, &manifest).await?;
    let shallow_closure = read_current_shallow_closure(store, router, &manifest).await?;

    std::fs::create_dir_all(&config.workspace_root).map_err(CrabError::Io)?;
    let sidecar_bytes = selected_pack_sidecar_bytes(&selected_packs)?;
    // Hard-link staging is normally zero-copy, but a cross-device workspace
    // can require a second local copy of every downloaded artifact.
    let required_space = bytes_read
        .checked_mul(2)
        .and_then(|bytes| {
            sidecar_bytes
                .checked_mul(2)
                .and_then(|sidecars| bytes.checked_add(sidecars))
        })
        .and_then(|bytes| bytes.checked_add(REPACK_DISK_RESERVE))
        .ok_or_else(|| CrabError::Internal("repack workspace size overflow".to_owned()))?;
    let available = crate::workflow::cache::available_disk_space(&config.workspace_root)
        .ok_or_else(|| CrabError::Configuration {
            key: "repack workspace capacity".to_owned(),
            origin: format!(
                "cannot determine free disk space for {}; refusing a large-repository mutation",
                config.workspace_root.display()
            ),
        })?;
    if available < required_space {
        return Err(CrabError::InsufficientSpace {
            needed: required_space,
            available,
        });
    }
    let temp = tempfile::Builder::new()
        .prefix("crab-repack-")
        .tempdir_in(&config.workspace_root)
        .map_err(CrabError::Io)?;
    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&download_dir)?;
    download_source_packs(
        store,
        router,
        &selected_packs,
        &download_dir,
        download_concurrency,
        cancel,
    )
    .await?;
    check_cancelled(cancel)?;
    if let Some(result) = elapsed_budget_result(budget, start) {
        return Ok(result);
    }

    let refs = manifest.refs.values().cloned().collect::<BTreeSet<_>>();
    let download_dir_for_pack = download_dir.clone();
    let source_packs = selected_packs;
    let repacked_repository = tokio::task::spawn_blocking(move || {
        let sources = source_packs
            .iter()
            .map(|pack| crab_git::repack::RepackSource {
                canonical_id: pack.pack_id.clone(),
                path: download_dir_for_pack.join(format!("pack-{}.pack", pack.pack_id)),
                index_path: download_dir_for_pack.join(format!("pack-{}.idx", pack.pack_id)),
                reverse_index_path: download_dir_for_pack
                    .join(format!("pack-{}.rev", pack.pack_id)),
                size: pack.size,
                object_count: pack.object_count,
                verified_identity: None,
            })
            .collect::<Vec<_>>();
        crab_git::repack::consolidate_pack_suffix_with_concurrency(&sources, download_concurrency)
            .map_err(CrabError::from)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("repack worker join failed: {error}")))??;
    check_cancelled(cancel)?;
    if let Some(result) = elapsed_budget_result(budget, start) {
        return Ok(result);
    }

    let mut repacked = repacked_repository
        .packs()
        .iter()
        .map(|generated| {
            let entry = PackManifestEntry {
                pack_id: generated.pack_id.clone(),
                size: generated.pack_size,
                content_hash: generated.pack_id.clone(),
                ref_tips: refs.iter().cloned().collect(),
                object_count: generated.object_count,
            };
            RepackedPack { generated, entry }
        })
        .collect::<Vec<_>>();
    if let Some(result) = elapsed_budget_result(budget, start) {
        return Ok(result);
    }
    for pack in repacked.iter().filter(|pack| pack.generated.is_new) {
        upload_generated_pack(store, router, pack.generated, cancel).await?;
        crab_metadata::pack_origin::record_verified_pack_origin(
            store.as_storage(),
            router.repo_prefix(),
            &pack.entry,
        )
        .await?;
    }
    for pack in &mut repacked {
        if !pack.generated.is_new {
            ensure_remote_reverse_index(store, router, pack.generated, cancel).await?;
        }
        let metadata = crate::git::push::upsert_pack_metadata(
            store,
            &router.pack_metadata_path(&pack.entry.pack_id),
            &pack.entry.pack_id,
            pack.entry.object_count,
            pack.entry.ref_tips.clone(),
            config.max_cas_retries,
        )
        .await?;
        pack.entry.ref_tips = metadata.ref_tips;
    }
    let replacement_entries = stable_packs
        .into_iter()
        .chain(repacked.iter().map(|pack| pack.entry.clone()))
        .collect::<Vec<_>>();
    let new_generation = manifest.generation.checked_add(1).ok_or_else(|| {
        CrabError::Internal("manifest generation overflow during repack".to_owned())
    })?;
    let (pack_index_hash, _index, pack_write) =
        compact_pack_index(new_generation, &replacement_entries)?;
    upload_segmented_bulk(
        store,
        router,
        &BulkData {
            shard_index: crab_metadata::segmented::SegmentWrite::default(),
            pack_index: pack_write,
        },
    )
    .await?;
    check_cancelled(cancel)?;
    if let Some(result) = elapsed_budget_result(budget, start) {
        return Ok(result);
    }

    let mut committed = repack_manifest(manifest, new_generation, pack_index_hash);
    if let Some(graph) = commit_graph {
        let write = crab_metadata::split_commit_graph::rebind_split_commit_graph(
            &graph,
            committed.generation,
            committed.pack_index_hash.clone(),
            committed.git_validation_digest.clone(),
        )?;
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        crab_metadata::split_commit_graph::upload_split_commit_graph(
            store.as_storage(),
            &storage_router,
            &write,
        )
        .await?;
        committed.commit_graph_hash = Some(write.descriptor_hash);
    }
    if let Some(shallow_closure) = shallow_closure {
        let write = crab_metadata::shallow_closure::rebind_shallow_closure_write(
            &shallow_closure,
            committed.generation,
            committed.pack_index_hash.clone(),
            committed.git_validation_digest.clone(),
        )
        .map_err(CrabError::from)?;
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        crab_metadata::shallow_closure::upload_shallow_closure(
            store.as_storage(),
            &storage_router,
            &committed.git_validation_digest,
            &write,
        )
        .await
        .map_err(CrabError::from)?;
    }
    if let Some(result) = elapsed_budget_result(budget, start) {
        return Ok(result);
    }
    write_manifest_cas(store, router, &committed, &manifest_etag).await?;
    let visibility_expected = visibility.is_some();
    if let Some(visibility) = visibility {
        let visibility = rebind_visibility(visibility, &committed);
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        if let Err(error) = crab_metadata::git_visibility::upload_if_absent(
            store.as_storage(),
            &storage_router,
            &visibility,
        )
        .await
        {
            warn!(
                error = %error,
                generation = committed.generation,
                "repack committed; Git visibility proof requires repair"
            );
        }
    } else {
        debug!(
            generation = committed.generation,
            "repack preserved repository without a Git visibility proof"
        );
    }
    let anchor = CommittedManifestAnchor {
        generation: committed.generation,
        shard_index_hash: manifest_hash_or_default(&committed.shard_index_hash)?,
        pack_index_hash: manifest_hash_or_default(&committed.pack_index_hash)?,
    };
    let committed_packs = repacked
        .iter()
        .map(|pack| CommittedPackIndex {
            pack: &pack.entry,
            idx_path: pack.generated.index_path(),
            rev_path: pack.generated.reverse_index_path(),
            git_sha1: &pack.generated.git_sha1,
            kind_by_oid: None,
        })
        .collect::<Vec<_>>();
    let locator_published = match publish_committed_pack_locators(
        store,
        router,
        &committed_packs,
        anchor,
        Some(&replacement_entries),
        config.lock_ttl,
        cancel,
    )
    .await
    {
        Ok(stats) if stats.coverage_updated => true,
        Ok(_) => false,
        Err(error) => {
            warn!(error = %error, "repack committed; locator publication requires repair");
            false
        }
    };
    if visibility_expected && locator_published {
        match crab_metadata::git_visibility::ensure_catalog_bound(
            store.as_storage(),
            &crab_storage::StoreLayout::new(
                store.as_storage().clone(),
                router.repo_prefix().to_owned(),
            ),
            &committed,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => {
                warn!("repack committed; catalog visibility proof was not published");
            }
            Err(error) => {
                warn!(error = %error, "repack committed; catalog visibility requires repair");
            }
        }
    }

    info!(
        generation = committed.generation,
        packs_after = replacement_entries.len(),
        packs_before,
        "repack manifest committed"
    );
    Ok(RepackRunResult::Completed {
        outcome: outcome(
            packs_before,
            replacement_entries.len(),
            bytes_before,
            replacement_entries.iter().map(|pack| pack.size).sum(),
            bytes_read,
            repacked.iter().map(|pack| pack.entry.size).sum(),
            start,
        ),
        bounded: selection.bounded,
    })
}

pub(crate) fn generation_owner_repack_count(packs: &[PackManifestEntry]) -> usize {
    crab_git::repack::incremental_repack_cut(
        &packs.iter().map(|pack| pack.size).collect::<Vec<_>>(),
        2,
    )
}

fn outcome(
    packs_before: usize,
    packs_after: usize,
    bytes_before: u64,
    bytes_after: u64,
    bytes_read: u64,
    bytes_written: u64,
    start: Instant,
) -> RepackOutcome {
    RepackOutcome {
        packs_before,
        packs_after,
        bytes_before,
        bytes_after,
        bytes_read,
        bytes_written,
        elapsed: start.elapsed(),
    }
}

fn select_repack_packs(
    packs: &[PackManifestEntry],
    geometric_count: usize,
    budget: Option<&RepackBudget>,
    elapsed: Duration,
) -> Result<std::result::Result<RepackSelection, RepackDeferral>> {
    if geometric_count == 0 {
        return Ok(Ok(RepackSelection {
            count: 0,
            bytes: 0,
            bounded: false,
        }));
    }
    if let Some(deferral) = elapsed_budget_deferral(budget, elapsed) {
        return Ok(Err(deferral));
    }
    let Some(budget) = budget else {
        return Ok(Ok(RepackSelection {
            count: geometric_count,
            bytes: selected_pack_bytes(packs, geometric_count)?,
            bounded: false,
        }));
    };
    if budget.source_pack_limit < 2 {
        return Ok(Err(RepackDeferral {
            resource: "source_packs",
            actual: 2,
            maximum: budget.source_pack_limit as u64,
        }));
    }
    let request_limited_packs =
        usize::try_from(budget.source_request_limit / SOURCE_ARTIFACT_REQUESTS_PER_PACK)
            .unwrap_or(usize::MAX);
    if request_limited_packs < 2 {
        return Ok(Err(RepackDeferral {
            resource: "source_storage_requests",
            actual: SOURCE_ARTIFACT_REQUESTS_PER_PACK.saturating_mul(2),
            maximum: budget.source_request_limit,
        }));
    }
    let max_count = geometric_count
        .min(budget.source_pack_limit)
        .min(request_limited_packs);
    let minimum_count = geometric_count.min(2);
    let minimum_bytes = selected_pack_bytes(packs, minimum_count)?;
    if minimum_bytes > budget.source_byte_limit {
        return Ok(Err(RepackDeferral {
            resource: "source_bytes",
            actual: minimum_bytes,
            maximum: budget.source_byte_limit,
        }));
    }

    let mut count = 0;
    let mut bytes = 0u64;
    for pack in packs.iter().rev().take(geometric_count) {
        if count == max_count {
            break;
        }
        let next_bytes = bytes.checked_add(pack.size).ok_or_else(|| {
            CrabError::Internal("selected repack source byte total overflow".to_owned())
        })?;
        if next_bytes > budget.source_byte_limit {
            break;
        }
        count += 1;
        bytes = next_bytes;
    }
    if count < 2 {
        return Ok(Err(RepackDeferral {
            resource: "source_bytes",
            actual: minimum_bytes,
            maximum: budget.source_byte_limit,
        }));
    }
    Ok(Ok(RepackSelection {
        count,
        bytes,
        bounded: count < geometric_count,
    }))
}

fn selected_pack_bytes(packs: &[PackManifestEntry], count: usize) -> Result<u64> {
    packs
        .iter()
        .rev()
        .take(count)
        .try_fold(0u64, |total, pack| {
            total.checked_add(pack.size).ok_or_else(|| {
                CrabError::Internal("selected repack source byte total overflow".to_owned())
            })
        })
}

fn elapsed_budget_deferral(
    budget: Option<&RepackBudget>,
    elapsed: Duration,
) -> Option<RepackDeferral> {
    let budget = budget?;
    (elapsed >= budget.elapsed_limit).then(|| RepackDeferral {
        resource: "elapsed_ms",
        actual: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        maximum: u64::try_from(budget.elapsed_limit.as_millis()).unwrap_or(u64::MAX),
    })
}

fn elapsed_budget_result(budget: Option<&RepackBudget>, start: Instant) -> Option<RepackRunResult> {
    elapsed_budget_deferral(budget, start.elapsed()).map(deferral_result)
}

fn deferral_result(deferral: RepackDeferral) -> RepackRunResult {
    RepackRunResult::Deferred {
        resource: deferral.resource,
        actual: deferral.actual,
        maximum: deferral.maximum,
    }
}

fn repack_manifest(mut manifest: Manifest, generation: u64, pack_index_hash: String) -> Manifest {
    manifest.generation = generation;
    manifest.created_at = now_iso8601();
    manifest.pusher = None;
    manifest.session_id = format!("repack-{generation}");
    manifest.pack_index_hash = pack_index_hash;
    manifest.commit_graph_hash = None;
    // `run` validates every replacement pack against the complete temporary
    // ODB before this helper commits its compacted inventory.
    manifest.seal_git_validation();
    manifest
}

async fn read_current_visibility(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<Option<crab_metadata::git_visibility::GitVisibilityIndex>> {
    if manifest.refs.is_empty() || manifest.pack_index_hash.is_empty() {
        return Ok(None);
    }
    let storage_router =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    match crab_metadata::git_visibility::read_for_manifest(
        store.as_storage(),
        &storage_router,
        manifest,
    )
    .await
    {
        Ok(Some(read)) => Ok(Some(read.index)),
        Ok(None) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn read_current_commit_graph(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<Option<crab_metadata::split_commit_graph::SplitCommitGraph>> {
    let Some(hash) = manifest.commit_graph_hash.as_deref() else {
        return Ok(None);
    };
    let storage_router =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    let graph = crab_metadata::split_commit_graph::load_split_commit_graph(
        store.as_storage(),
        &storage_router,
        hash,
        crab_metadata::split_commit_graph::DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await?;
    if graph.descriptor.generation != manifest.generation
        || graph.descriptor.pack_index_hash != manifest.pack_index_hash
        || graph.descriptor.git_validation_digest != manifest.git_validation_digest
        || manifest
            .refs
            .iter()
            .map(|(name, oid)| manifest.peeled_refs.get(name).unwrap_or(oid))
            .any(|root| !graph.contains_hex(root))
    {
        return Err(CrabError::CorruptObject {
            path: storage_router
                .bulk_manifest_path("commit-graph", hash)
                .to_string(),
            reason: "commit graph identity does not match the repack source manifest".to_owned(),
        });
    }
    Ok(Some(graph))
}

async fn read_current_shallow_closure(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<Option<crab_metadata::shallow_closure::ShallowClosureDescriptor>> {
    if manifest.refs.is_empty() || manifest.pack_index_hash.is_empty() {
        return Ok(None);
    }
    let storage_router =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    crab_metadata::shallow_closure::load_shallow_closure_descriptor(
        store.as_storage(),
        &storage_router,
        &manifest.git_validation_digest,
        manifest.generation,
        &manifest.pack_index_hash,
        crab_metadata::shallow_closure::DEFAULT_MAX_SHALLOW_CLOSURE_DESCRIPTOR_BYTES,
    )
    .await
    .map_err(CrabError::from)
}

fn rebind_visibility(
    mut visibility: crab_metadata::git_visibility::GitVisibilityIndex,
    manifest: &Manifest,
) -> crab_metadata::git_visibility::GitVisibilityIndex {
    visibility.generation = manifest.generation;
    visibility
        .pack_index_hash
        .clone_from(&manifest.pack_index_hash);
    visibility
        .git_validation_digest
        .clone_from(&manifest.git_validation_digest);
    visibility
}

fn manifest_hash_or_default(value: &str) -> Result<MerkleHash> {
    if value.is_empty() {
        return Ok(MerkleHash::default());
    }
    MerkleHash::from_hex(value)
        .map_err(|error| CrabError::Internal(format!("invalid manifest content hash: {error}")))
}

async fn download_source_packs(
    store: &Store,
    router: &StoreLayout,
    packs: &[PackManifestEntry],
    pack_dir: &Path,
    concurrency: usize,
    cancel: &CancellationToken,
) -> Result<()> {
    let results = futures_util::stream::iter(packs.iter().cloned().map(|pack| {
        let store = store.clone();
        let cancel = cancel.clone();
        let pack_path = repo_pack_path(router.repo_prefix(), &pack.pack_id);
        let index_path = repo_pack_index_path(router.repo_prefix(), &pack.pack_id);
        let reverse_index_path = repo_pack_reverse_index_path(router.repo_prefix(), &pack.pack_id);
        let local_pack = pack_dir.join(format!("pack-{}.pack", pack.pack_id));
        let local_index = pack_dir.join(format!("pack-{}.idx", pack.pack_id));
        let local_reverse_index = pack_dir.join(format!("pack-{}.rev", pack.pack_id));
        async move {
            if cancel.is_cancelled() {
                return Err(CrabError::Cancelled);
            }
            let pack_size = tokio::select! {
                result = store.download_to_path_bounded(&pack_path, &local_pack, pack.size) => result?,
                () = cancel.cancelled() => return Err(CrabError::Cancelled),
            };
            if pack_size != pack.size {
                return Err(CrabError::CorruptObject {
                    path: pack_path.as_ref().to_owned(),
                    reason: format!("pack has {pack_size} bytes, manifest records {}", pack.size),
                });
            }
            let index_maximum = crab_git::pack_locator::max_pack_index_size(pack.object_count)
                .ok_or_else(|| CrabError::CorruptObject {
                    path: index_path.as_ref().to_owned(),
                    reason: "pack index size bound overflowed".to_owned(),
                })?;
            tokio::select! {
                result = store.download_to_path_bounded(&index_path, &local_index, index_maximum) => result?,
                () = cancel.cancelled() => return Err(CrabError::Cancelled),
            };
            let reverse_maximum = crab_git::pack_locator::pack_reverse_index_size(pack.object_count)
                .ok_or_else(|| CrabError::CorruptObject {
                    path: reverse_index_path.as_ref().to_owned(),
                    reason: "pack reverse-index size bound overflowed".to_owned(),
                })?;
            let reverse_size = tokio::select! {
                result = store.download_to_path_bounded(
                    &reverse_index_path,
                    &local_reverse_index,
                    reverse_maximum,
                ) => result?,
                () = cancel.cancelled() => return Err(CrabError::Cancelled),
            };
            if reverse_size != reverse_maximum {
                return Err(CrabError::CorruptObject {
                    path: reverse_index_path.as_ref().to_owned(),
                    reason: format!(
                        "reverse index has {reverse_size} bytes, expected {reverse_maximum}"
                    ),
                });
            }
            Ok(())
        }
    }))
    .buffer_unordered(concurrency.max(1))
    .collect::<Vec<_>>()
    .await;
    check_cancelled(cancel)?;
    for result in results {
        result?;
    }
    Ok(())
}

fn selected_pack_sidecar_bytes(packs: &[PackManifestEntry]) -> Result<u64> {
    packs.iter().try_fold(0_u64, |total, pack| {
        let index_size = crab_git::pack_locator::max_pack_index_size(pack.object_count)
            .ok_or_else(|| CrabError::CorruptObject {
                path: format!("pack-{}/idx", pack.pack_id),
                reason: "pack index size bound overflowed".to_owned(),
            })?;
        let reverse_index_size = crab_git::pack_locator::pack_reverse_index_size(pack.object_count)
            .ok_or_else(|| CrabError::CorruptObject {
                path: format!("pack-{}/rev", pack.pack_id),
                reason: "pack reverse-index size bound overflowed".to_owned(),
            })?;
        total
            .checked_add(index_size)
            .and_then(|value| value.checked_add(reverse_index_size))
            .ok_or_else(|| CrabError::Internal("repack sidecar byte total overflow".to_owned()))
    })
}

fn validate_pack_inventory(router: &StoreLayout, packs: &[PackManifestEntry]) -> Result<()> {
    let mut ids = HashSet::with_capacity(packs.len());
    for pack in packs {
        if pack.pack_id.is_empty() || !ids.insert(pack.pack_id.clone()) {
            return Err(CrabError::CorruptObject {
                path: router.manifest_path().to_string(),
                reason: format!(
                    "pack inventory contains duplicate or empty id {}",
                    pack.pack_id
                ),
            });
        }
        if pack.size == 0 {
            return Err(CrabError::Configuration {
                key: "repack pack size".to_owned(),
                origin: format!(
                    "pack {} has invalid size {}; expected a non-zero manifest size",
                    pack.pack_id, pack.size
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
fn run_git(command: &mut Command, operation: &str) -> Result<()> {
    debug!(operation, command = ?command, "running git repack subprocess");
    let status = command.status()?;
    if status.success() {
        return Ok(());
    }
    Err(CrabError::Internal(format!(
        "{operation} failed with {status}"
    )))
}

#[cfg(test)]
fn hash_file(path: &Path) -> Result<([u8; 32], u64)> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok((*hasher.finalize().as_bytes(), size))
}

async fn upload_generated_pack(
    store: &Store,
    router: &StoreLayout,
    generated: &crab_git::repack::GeometricRepackedPack,
    cancel: &CancellationToken,
) -> Result<()> {
    let pack_path = repo_pack_path(router.repo_prefix(), &generated.pack_id);
    store
        .put_multipart_file_retry(
            &pack_path,
            generated.pack_path(),
            generated.pack_size,
            generated.pack_hash,
            MULTIPART_PART_SIZE,
            cancel,
            None,
        )
        .await?;
    verify_remote_file(
        store,
        &pack_path,
        generated.pack_size,
        generated.pack_hash,
        cancel,
    )
    .await?;

    let index_path = repo_pack_index_path(router.repo_prefix(), &generated.pack_id);
    store
        .put_multipart_file_retry(
            &index_path,
            generated.index_path(),
            generated.index_size,
            generated.index_hash,
            MULTIPART_PART_SIZE,
            cancel,
            None,
        )
        .await?;
    verify_remote_file(
        store,
        &index_path,
        generated.index_size,
        generated.index_hash,
        cancel,
    )
    .await?;

    let reverse_path = repo_pack_reverse_index_path(router.repo_prefix(), &generated.pack_id);
    store
        .put_multipart_file_retry(
            &reverse_path,
            generated.reverse_index_path(),
            generated.reverse_index_size,
            generated.reverse_index_hash,
            MULTIPART_PART_SIZE,
            cancel,
            None,
        )
        .await?;
    verify_remote_file(
        store,
        &reverse_path,
        generated.reverse_index_size,
        generated.reverse_index_hash,
        cancel,
    )
    .await
}

async fn verify_remote_file(
    store: &Store,
    path: &object_store::path::Path,
    expected_size: u64,
    expected_hash: [u8; 32],
    cancel: &CancellationToken,
) -> Result<()> {
    let mut hasher = blake3::Hasher::new();
    let actual_size = tokio::select! {
        result = store.stream_to_writer(path, &mut hasher) => result?,
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
    };
    let actual_hash = *hasher.finalize().as_bytes();
    if actual_size != expected_size || actual_hash != expected_hash {
        return Err(CrabError::CorruptObject {
            path: path.to_string(),
            reason: format!(
                "remote repack object verification failed: expected {expected_size} bytes and {}, got {actual_size} bytes and {}",
                blake3::Hash::from_bytes(expected_hash).to_hex(),
                blake3::Hash::from_bytes(actual_hash).to_hex()
            ),
        });
    }
    Ok(())
}

async fn ensure_remote_reverse_index(
    store: &Store,
    router: &StoreLayout,
    generated: &crab_git::repack::GeometricRepackedPack,
    cancel: &CancellationToken,
) -> Result<()> {
    let path = repo_pack_reverse_index_path(router.repo_prefix(), &generated.pack_id);
    match tokio::select! {
        result = store.head(&path) => result,
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
    } {
        Ok(meta) => {
            if meta.size != generated.reverse_index_size {
                return Err(CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: format!(
                        "existing reverse index has {} bytes, expected {}",
                        meta.size, generated.reverse_index_size
                    ),
                });
            }
            verify_remote_file(
                store,
                &path,
                generated.reverse_index_size,
                generated.reverse_index_hash,
                cancel,
            )
            .await
        }
        Err(CrabError::NotFound { .. }) => {
            store
                .put_multipart_file_retry(
                    &path,
                    generated.reverse_index_path(),
                    generated.reverse_index_size,
                    generated.reverse_index_hash,
                    MULTIPART_PART_SIZE,
                    cancel,
                    None,
                )
                .await
        }
        Err(error) => Err(error),
    }
}

fn now_iso8601() -> String {
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs();
    let days = seconds / 86_400;
    let time_of_day = seconds % 86_400;
    let hours = time_of_day / 3_600;
    let minutes = (time_of_day % 3_600) / 60;
    let seconds = time_of_day % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::ObjectStore;
    use object_store::memory::InMemory;

    use crab_git::pack_locator::{PackLocationIter, write_pack_reverse_index};

    use super::*;

    fn budget_pack(size: u64, object_count: u64) -> PackManifestEntry {
        PackManifestEntry {
            pack_id: format!("{object_count:064x}"),
            size,
            content_hash: format!("{object_count:064x}"),
            ref_tips: vec!["a".to_owned()],
            object_count,
        }
    }

    #[test]
    fn bounded_repack_selection_preserves_progress_under_source_limits() {
        let packs = vec![
            budget_pack(100, 100),
            budget_pack(10, 10),
            budget_pack(10, 9),
            budget_pack(10, 8),
            budget_pack(10, 7),
        ];
        let budget = RepackBudget {
            source_pack_limit: 2,
            source_byte_limit: 20,
            source_request_limit: 6,
            elapsed_limit: Duration::from_secs(60),
        };

        let selection = select_repack_packs(&packs, 4, Some(&budget), Duration::ZERO)
            .expect("selection calculation")
            .expect("selection should fit");
        assert_eq!(selection.count, 2);
        assert_eq!(selection.bytes, 20);
        assert!(selection.bounded);
    }

    #[test]
    fn bounded_repack_selection_counts_pack_sidecar_requests() {
        let packs = vec![budget_pack(10, 10), budget_pack(10, 9)];
        let budget = RepackBudget {
            source_pack_limit: 4,
            source_byte_limit: 100,
            source_request_limit: 5,
            elapsed_limit: Duration::from_secs(60),
        };

        let deferral = select_repack_packs(&packs, 2, Some(&budget), Duration::ZERO)
            .expect("selection calculation")
            .expect_err("selection should require pack, index, and reverse-index requests");
        assert_eq!(deferral.resource, "source_storage_requests");
        assert_eq!(deferral.actual, 6);
        assert_eq!(deferral.maximum, 5);
    }

    #[test]
    fn generation_owner_budget_bounds_large_consolidation_batches() {
        let packs = (0..902)
            .map(|index| budget_pack(1_024 * 1_024, 1_000 + index))
            .collect::<Vec<_>>();
        let geometric_count = generation_owner_repack_count(&packs);

        let selection = select_repack_packs(
            &packs,
            geometric_count,
            Some(&RepackBudget::generation_owner()),
            Duration::ZERO,
        )
        .expect("selection calculation")
        .expect("large consolidation should make bounded progress");

        assert_eq!(geometric_count, 901);
        assert_eq!(selection.count, GENERATION_OWNER_REPACK_MAX_SOURCE_PACKS);
        assert_eq!(
            selection.bytes,
            (GENERATION_OWNER_REPACK_MAX_SOURCE_PACKS as u64) * 1_024 * 1_024
        );
        assert!(selection.bounded);
    }

    #[test]
    fn bounded_repack_selection_defers_when_two_packs_do_not_fit() {
        let packs = vec![
            budget_pack(100, 100),
            budget_pack(10, 10),
            budget_pack(10, 9),
        ];
        let budget = RepackBudget {
            source_pack_limit: 4,
            source_byte_limit: 19,
            source_request_limit: 8,
            elapsed_limit: Duration::from_secs(60),
        };

        let deferral = select_repack_packs(&packs, 2, Some(&budget), Duration::ZERO)
            .expect("selection calculation")
            .expect_err("selection should be deferred");
        assert_eq!(deferral.resource, "source_bytes");
        assert_eq!(deferral.actual, 20);
        assert_eq!(deferral.maximum, 19);
    }

    #[test]
    fn bounded_repack_selection_defers_after_deadline() {
        let packs = vec![budget_pack(10, 10), budget_pack(10, 9)];
        let budget = RepackBudget {
            source_pack_limit: 4,
            source_byte_limit: 100,
            source_request_limit: 8,
            elapsed_limit: Duration::from_secs(1),
        };

        let deferral = select_repack_packs(&packs, 2, Some(&budget), Duration::from_secs(1))
            .expect("selection calculation")
            .expect_err("selection should be deferred");
        assert_eq!(deferral.resource, "elapsed_ms");
        assert_eq!(deferral.maximum, 1_000);
    }

    #[test]
    fn generation_owner_repack_waits_for_small_pack_accumulation() {
        let stable = vec![budget_pack(900, 900), budget_pack(9, 9)];
        assert_eq!(generation_owner_repack_count(&stable), 0);

        let packs = vec![
            budget_pack(900, 900),
            budget_pack(700, 700),
            budget_pack(9, 9),
        ];
        assert_eq!(generation_owner_repack_count(&packs), 0);

        let mut accumulated = packs;
        accumulated.push(budget_pack(9, 8));
        assert_eq!(generation_owner_repack_count(&accumulated), 2);
    }

    #[test]
    fn generation_owner_repack_finds_collisions_above_a_stable_tail() {
        let packs = vec![
            budget_pack(1_000_000, 1_000_000),
            budget_pack(100, 100),
            budget_pack(60, 60),
            budget_pack(1, 1),
        ];

        assert_eq!(generation_owner_repack_count(&packs), 3);
    }

    #[test]
    fn repack_manifest_invalidates_generation_bound_commit_graph() {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "a".repeat(40));
        manifest.shard_index_hash = "b".repeat(64);
        manifest.commit_graph_hash = Some("c".repeat(64));
        manifest.ref_registry_hash = Some("d".repeat(64));

        let updated = repack_manifest(manifest.clone(), 9, "e".repeat(64));

        assert_eq!(updated.generation, 9);
        assert_eq!(updated.pack_index_hash, "e".repeat(64));
        assert_eq!(updated.refs, manifest.refs);
        assert_eq!(updated.shard_index_hash, manifest.shard_index_hash);
        assert_eq!(updated.commit_graph_hash, None);
        assert_eq!(updated.ref_registry_hash, manifest.ref_registry_hash);
    }

    #[test]
    fn rebind_visibility_changes_only_generation_anchor() {
        let mut refs = std::collections::BTreeMap::new();
        refs.insert("refs/heads/main".to_owned(), vec!["a".repeat(40)]);
        let visibility = crab_metadata::git_visibility::GitVisibilityIndex::new(
            3,
            "b".repeat(64),
            "d".repeat(64),
            refs.clone(),
        )
        .expect("valid visibility proof");
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 4;
        manifest.pack_index_hash = "c".repeat(64);
        manifest.seal_git_validation();

        let rebound = rebind_visibility(visibility, &manifest);

        assert_eq!(rebound.generation, 4);
        assert_eq!(rebound.pack_index_hash, "c".repeat(64));
        assert_eq!(
            rebound.git_validation_digest,
            manifest.git_validation_digest
        );
        assert_eq!(rebound.ref_closures(), refs);
    }

    #[tokio::test]
    async fn missing_visibility_remains_optional() -> Result<()> {
        let backend: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(backend);
        let router = StoreLayout::new(store.clone(), "org/repack-test".to_owned());
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.pack_index_hash = "a".repeat(64);
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "b".repeat(40));

        assert!(
            read_current_visibility(&store, &router, &manifest)
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn repack_source_download_requires_committed_pack_artifacts() -> Result<()> {
        let backend: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(backend);
        let router = StoreLayout::new(store.clone(), "org/repack-download".to_owned());
        let source = tempfile::tempdir()?;
        let repository = source.path().join("repository");
        initialize_work_repository(&repository)?;
        std::fs::write(repository.join("file.txt"), b"pack body\n")?;
        commit_all(&repository, "pack")?;
        let pack = snapshot_repository_pack(&repository, source.path(), "source")?;
        let (hash, size) = hash_file(&pack.pack_path)?;
        let pack_id = blake3::Hash::from_bytes(hash).to_hex().to_string();
        let reverse_index = pack.index_path.with_extension("rev");
        write_pack_reverse_index(&pack.index_path, &reverse_index)
            .map_err(crab_git::pack::PackError::from)?;
        let locations = PackLocationIter::open(&pack.index_path, &reverse_index, size)
            .map_err(crab_git::pack::PackError::from)?;
        let entry = PackManifestEntry {
            pack_id: pack_id.clone(),
            size,
            content_hash: pack_id.clone(),
            ref_tips: Vec::new(),
            object_count: locations.object_count(),
        };
        store
            .put(
                &router.pack_path(&pack_id),
                Bytes::from(std::fs::read(&pack.pack_path)?),
            )
            .await?;
        store
            .put(
                &router.pack_index_path(&pack_id),
                Bytes::from(std::fs::read(&pack.index_path)?),
            )
            .await?;
        store
            .put(
                &router.pack_reverse_index_path(&pack_id),
                Bytes::from(std::fs::read(&reverse_index)?),
            )
            .await?;
        let download_dir = source.path().join("downloads");
        std::fs::create_dir_all(&download_dir)?;

        download_source_packs(
            &store,
            &router,
            &[entry],
            &download_dir,
            1,
            &CancellationToken::new(),
        )
        .await?;

        assert_eq!(
            std::fs::read(download_dir.join(format!("pack-{pack_id}.pack")))?,
            std::fs::read(pack.pack_path)?,
        );
        assert_eq!(
            std::fs::read(download_dir.join(format!("pack-{pack_id}.idx")))?,
            std::fs::read(pack.index_path)?,
        );
        assert_eq!(
            std::fs::read(download_dir.join(format!("pack-{pack_id}.rev")))?,
            std::fs::read(reverse_index)?,
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repack_commits_verified_pack_set_and_locator_generation() -> Result<()> {
        let backend: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(Arc::clone(&backend));
        let prefix = "org/repack-test";
        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        let source = tempfile::tempdir()?;
        let repository = source.path().join("repository");
        initialize_work_repository(&repository)?;

        std::fs::write(repository.join("first.txt"), b"first\n")?;
        commit_all(&repository, "first")?;
        let first = snapshot_repository_pack(&repository, source.path(), "first")?;
        std::fs::write(repository.join("second.txt"), b"second\n")?;
        commit_all(&repository, "second")?;
        let second = snapshot_repository_pack(&repository, source.path(), "second")?;
        std::fs::write(repository.join("third.txt"), b"third\n")?;
        commit_all(&repository, "third")?;
        let third = snapshot_repository_pack(&repository, source.path(), "third")?;
        let tip = git_output(
            isolated_test_git_command()
                .arg("-C")
                .arg(&repository)
                .arg("rev-parse")
                .arg("HEAD"),
            "resolve test tip",
        )?;

        let entries = vec![
            upload_test_pack(&store, &router, &first, &tip).await?,
            upload_test_pack(&store, &router, &second, &tip).await?,
            upload_test_pack(&store, &router, &third, &tip).await?,
        ];
        let (shard_index_hash, _shard_index, shard_write) =
            crate::metadata::manifest::compact_shard_index(1, &[])?;
        let (pack_index_hash, _pack_index, pack_write) = compact_pack_index(1, &entries)?;
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: shard_write,
                pack_index: pack_write,
            },
        )
        .await?;
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.created_at = now_iso8601();
        manifest.session_id = "fixture".to_owned();
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), tip.clone());
        manifest.shard_index_hash = shard_index_hash;
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        crate::metadata::manifest::create_manifest(&store, &router, &manifest).await?;
        crate::git::push::publish_git_visibility_index_from_git_dir(
            &repository.join(".git"),
            &manifest,
            &store,
            &router,
        )
        .await?;

        let outcome = run_repack(
            &store,
            prefix,
            &RepackConfig {
                lock_ttl: Duration::from_secs(60),
                dry_run: false,
                download_concurrency: 2,
                max_cas_retries: 4,
                workspace_root: crate::cache::default_cache_root().join("maintenance"),
            },
            &CancellationToken::new(),
        )
        .await?;

        assert_eq!(outcome.packs_before, 3);
        assert!((1..=outcome.packs_before).contains(&outcome.packs_after));
        let (committed, _) = read_manifest(&store, &router).await?;
        assert_eq!(committed.generation, 2);
        assert_eq!(committed.refs.get("refs/heads/main"), Some(&tip));
        let replacement = crate::metadata::manifest::read_bulk_pack_list(
            &store,
            &router,
            &committed.pack_index_hash,
        )
        .await?;
        assert_eq!(replacement.len(), outcome.packs_after);
        for pack in &replacement {
            store.head(&router.pack_path(&pack.pack_id)).await?;
            store.head(&router.pack_index_path(&pack.pack_id)).await?;
            store
                .head(&router.pack_reverse_index_path(&pack.pack_id))
                .await?;
            store
                .head(&router.pack_metadata_path(&pack.pack_id))
                .await?;
        }
        let session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
            Arc::clone(store.inner()),
            prefix,
        )
        .await?;
        assert_eq!(
            session.coverage(),
            Some(crab_metadata::git_object_locator::GitLocatorCoverage {
                generation: 2,
                pack_index_hash: manifest_hash_or_default(&committed.pack_index_hash)?,
            })
        );
        session.close().await?;
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let visibility = crab_metadata::git_visibility::read(
            store.as_storage(),
            &storage_router,
            committed.generation,
            &committed.pack_index_hash,
            &committed.git_validation_digest,
        )
        .await?;
        assert_eq!(visibility.ref_count(), 1);
        assert!(
            visibility
                .objects_for_ref("refs/heads/main")
                .expect("main closure")
                .binary_search(&tip)
                .is_ok()
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn geometric_repack_does_not_read_or_replace_stable_prefix() -> Result<()> {
        let backend: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(backend);
        let prefix = "org/repack-stable-prefix";
        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        let source = tempfile::tempdir()?;

        let mut packs = Vec::new();
        for (name, file_count) in [("stable", 12), ("recent-a", 2), ("recent-b", 1)] {
            let repository = source.path().join(name);
            initialize_work_repository(&repository)?;
            for index in 0..file_count {
                std::fs::write(
                    repository.join(format!("{name}-{index}.txt")),
                    format!("{name}-{index}\n"),
                )?;
            }
            commit_all(&repository, name)?;
            let tip = git_output(
                isolated_test_git_command()
                    .arg("-C")
                    .arg(&repository)
                    .arg("rev-parse")
                    .arg("HEAD"),
                "resolve geometric fixture tip",
            )?;
            let pack = snapshot_repository_pack(&repository, source.path(), name)?;
            packs.push(upload_test_pack(&store, &router, &pack, &tip).await?);
        }
        packs.sort_unstable_by(|left, right| right.object_count.cmp(&left.object_count));
        assert_eq!(
            packs
                .iter()
                .map(|pack| pack.object_count)
                .collect::<Vec<_>>(),
            vec![14, 4, 3]
        );
        let stable_id = packs[0].pack_id.clone();
        let selected_ids = packs[1..]
            .iter()
            .map(|pack| pack.pack_id.clone())
            .collect::<HashSet<_>>();
        let selected_bytes = packs[1..].iter().map(|pack| pack.size).sum::<u64>();

        let (shard_index_hash, _shard_index, shard_write) =
            crate::metadata::manifest::compact_shard_index(1, &[])?;
        let (pack_index_hash, _pack_index, pack_write) = compact_pack_index(1, &packs)?;
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: shard_write,
                pack_index: pack_write,
            },
        )
        .await?;
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.created_at = now_iso8601();
        manifest.session_id = "geometric-fixture".to_owned();
        manifest.shard_index_hash = shard_index_hash;
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        crate::metadata::manifest::create_manifest(&store, &router, &manifest).await?;

        let outcome = run_repack(
            &store,
            prefix,
            &RepackConfig {
                lock_ttl: Duration::from_secs(60),
                dry_run: false,
                download_concurrency: 2,
                max_cas_retries: 4,
                ..RepackConfig::default()
            },
            &CancellationToken::new(),
        )
        .await?;

        assert_eq!(outcome.packs_before, 3);
        assert_eq!(outcome.packs_after, 2);
        assert_eq!(outcome.bytes_read, selected_bytes);
        assert!(outcome.bytes_read < outcome.bytes_before);
        let (committed, _) = read_manifest(&store, &router).await?;
        let replacement = crate::metadata::manifest::read_bulk_pack_list(
            &store,
            &router,
            &committed.pack_index_hash,
        )
        .await?;
        assert!(replacement.iter().any(|pack| pack.pack_id == stable_id));
        assert!(
            replacement
                .iter()
                .all(|pack| !selected_ids.contains(&pack.pack_id))
        );
        Ok(())
    }

    fn initialize_work_repository(repository: &Path) -> Result<()> {
        run_git(
            isolated_test_git_command()
                .arg("init")
                .arg("--quiet")
                .arg(repository),
            "initialize test repository",
        )?;
        run_git(
            isolated_test_git_command().arg("-C").arg(repository).args([
                "config",
                "user.name",
                "Crab Test",
            ]),
            "configure test user name",
        )?;
        run_git(
            isolated_test_git_command().arg("-C").arg(repository).args([
                "config",
                "user.email",
                "crab@example.invalid",
            ]),
            "configure test user email",
        )
    }

    fn commit_all(repository: &Path, message: &str) -> Result<()> {
        run_git(
            isolated_test_git_command()
                .arg("-C")
                .arg(repository)
                .args(["add", "."]),
            "stage test commit",
        )?;
        run_git(
            isolated_test_git_command()
                .arg("-C")
                .arg(repository)
                .args(["commit", "--quiet", "-m", message]),
            "create test commit",
        )
    }

    fn isolated_test_git_command() -> Command {
        let mut command = Command::new("git");
        command
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR");
        command
    }

    struct TestPack {
        pack_path: PathBuf,
        index_path: PathBuf,
    }

    fn snapshot_repository_pack(
        repository: &Path,
        destination: &Path,
        name: &str,
    ) -> Result<TestPack> {
        run_git(
            isolated_test_git_command()
                .arg("-C")
                .arg(repository)
                .args(["repack", "--quiet", "-a", "-d"]),
            "pack test repository",
        )?;
        let pack_dir = repository.join(".git/objects/pack");
        let source_pack = std::fs::read_dir(&pack_dir)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .ok_or_else(|| CrabError::Internal("test repository has no pack".to_owned()))?;
        let source_index = source_pack.with_extension("idx");
        let pack_path = destination.join(format!("{name}.pack"));
        let index_path = destination.join(format!("{name}.idx"));
        std::fs::copy(source_pack, &pack_path)?;
        std::fs::copy(source_index, &index_path)?;
        Ok(TestPack {
            pack_path,
            index_path,
        })
    }

    async fn upload_test_pack(
        store: &Store,
        router: &StoreLayout,
        pack: &TestPack,
        tip: &str,
    ) -> Result<PackManifestEntry> {
        let (hash, size) = hash_file(&pack.pack_path)?;
        let pack_id = blake3::Hash::from_bytes(hash).to_hex().to_string();
        let reverse_index_path = pack.index_path.with_extension("rev");
        write_pack_reverse_index(&pack.index_path, &reverse_index_path)
            .map_err(crab_git::pack::PackError::from)?;
        let locations = PackLocationIter::open(&pack.index_path, &reverse_index_path, size)
            .map_err(crab_git::pack::PackError::from)?;
        store
            .put(
                &router.pack_path(&pack_id),
                Bytes::from(std::fs::read(&pack.pack_path)?),
            )
            .await?;
        store
            .put(
                &router.pack_index_path(&pack_id),
                Bytes::from(std::fs::read(&pack.index_path)?),
            )
            .await?;
        store
            .put(
                &router.pack_reverse_index_path(&pack_id),
                Bytes::from(std::fs::read(&reverse_index_path)?),
            )
            .await?;
        Ok(PackManifestEntry {
            pack_id: pack_id.clone(),
            size,
            content_hash: pack_id,
            ref_tips: vec![tip.to_owned()],
            object_count: locations.object_count(),
        })
    }

    fn git_output(command: &mut Command, operation: &str) -> Result<String> {
        let output = command.output()?;
        if !output.status.success() {
            return Err(CrabError::Internal(format!(
                "{operation} failed with {}",
                output.status
            )));
        }
        String::from_utf8(output.stdout)
            .map(|value| value.trim().to_owned())
            .map_err(|error| CrabError::Internal(format!("{operation} returned non-UTF8: {error}")))
    }
}
