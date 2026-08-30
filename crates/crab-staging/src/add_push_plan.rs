use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::StagingArea;
use crate::error::{Result, StagingError};
use crate::push_plan::{
    ExistingChunkCandidate, FilePushPlan, PlannedExistingChunk, PlannedPlacement, PlannedXorb,
    PreparedXorbCache, PreparedXorbCandidate, PreparedXorbSource, link_prepared_xorb,
    materialize_prepared_xorb, write_prepared_xorb,
};
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use crab_xet::xorb::format::Chunk;
#[cfg(test)]
use crab_xet::xorb::format::XorbRef;

const ADD_PLAN_READ_BATCH_CHUNKS: usize = 128;

/// Staged file input for add-time push-plan preparation.
pub struct AddPlanFile<'a> {
    pub file_hash: [u8; 32],
    pub size: u64,
    pub chunks: &'a [(MerkleHash, u64)],
}

/// Add-time push-plan preparation totals.
#[derive(Default)]
pub struct AddPushPlanSummary {
    pub files: u64,
    pub chunks: u64,
    pub remote_lookup: bool,
    pub existing_candidates: u64,
    pub prepared_cache_chunks: u64,
    pub prepared_cache_xorbs: u64,
    pub prepared_cache_link_misses: u64,
    pub prepared_xorbs: u64,
    pub prepared_bytes: u64,
}

/// Looks up already-uploaded chunk placements for staged chunks.
#[async_trait]
pub trait ExistingChunkLookup: Send + Sync {
    async fn lookup_existing_candidates(
        &self,
        chunks: &[(MerkleHash, u64)],
    ) -> Result<Vec<Option<ExistingChunkCandidate>>>;
}

/// Adds local prepared-xorb candidates to the staging cache.
#[async_trait]
pub trait LocalXorbCandidateLookup: Send + Sync {
    async fn load_candidates(
        &self,
        prepared_cache: &mut PreparedXorbCache,
        wanted_chunks: &HashSet<MerkleHash>,
    ) -> Result<()>;
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(StagingError::Cancelled);
    }
    Ok(())
}

pub async fn prepare_file_push_plans(
    staging: &StagingArea,
    files: &[AddPlanFile<'_>],
    build_xorb_builder: &(dyn Fn() -> XorbBuilder + Send + Sync),
    remote_lookup: Option<&dyn ExistingChunkLookup>,
    local_lookup: Option<&dyn LocalXorbCandidateLookup>,
    cancel: &CancellationToken,
) -> Result<AddPushPlanSummary> {
    prepare_file_push_plans_with_progress(
        staging,
        files,
        build_xorb_builder,
        remote_lookup,
        local_lookup,
        cancel,
        None,
    )
    .await
}

pub async fn prepare_file_push_plans_with_progress(
    staging: &StagingArea,
    files: &[AddPlanFile<'_>],
    build_xorb_builder: &(dyn Fn() -> XorbBuilder + Send + Sync),
    remote_lookup: Option<&dyn ExistingChunkLookup>,
    local_lookup: Option<&dyn LocalXorbCandidateLookup>,
    cancel: &CancellationToken,
    mut on_progress: Option<&mut (dyn FnMut(&AddPushPlanSummary) + Send)>,
) -> Result<AddPushPlanSummary> {
    if files.is_empty() {
        return Ok(AddPushPlanSummary::default());
    }

    let wanted_prepared_chunks: HashSet<MerkleHash> = files
        .iter()
        .flat_map(|file| file.chunks.iter().map(|(chunk_hash, _)| *chunk_hash))
        .collect();
    let mut summary = AddPushPlanSummary {
        remote_lookup: remote_lookup.is_some(),
        ..AddPushPlanSummary::default()
    };
    if let Some(callback) = on_progress.as_deref_mut() {
        callback(&summary);
    }
    let mut prepared_cache =
        staging.load_prepared_xorb_cache_for_chunks(&wanted_prepared_chunks)?;
    if let Some(local_lookup) = local_lookup {
        local_lookup
            .load_candidates(&mut prepared_cache, &wanted_prepared_chunks)
            .await?;
    }
    if files.len() > 1 {
        let all_chunks = all_file_chunks(files);
        let existing_refs = lookup_existing_candidates(&all_chunks, remote_lookup).await?;
        if prepared_cache.is_empty() {
            return prepare_uncached_file_plans_with_progress(
                staging,
                files,
                &existing_refs,
                build_xorb_builder,
                summary.remote_lookup,
                cancel,
                on_progress,
            )
            .await;
        }
        return prepare_cached_file_plans_with_progress(
            staging,
            files,
            &existing_refs,
            build_xorb_builder,
            &mut prepared_cache,
            summary,
            cancel,
            on_progress,
        )
        .await;
    }
    for file in files {
        check_cancelled(cancel)?;
        let file_summary = prepare_one_file_plan(
            staging,
            file,
            build_xorb_builder,
            remote_lookup,
            &mut prepared_cache,
            cancel,
        )
        .await?;
        accumulate_file_summary(&mut summary, file_summary);
        if let Some(callback) = on_progress.as_deref_mut() {
            callback(&summary);
        }
    }

    debug_file_summary(&summary);
    Ok(summary)
}

#[derive(Default)]
struct FilePlanSummary {
    chunks: u64,
    existing_candidates: u64,
    prepared_cache_chunks: u64,
    prepared_cache_xorbs: u64,
    prepared_cache_link_misses: u64,
    prepared_xorbs: u64,
    prepared_bytes: u64,
}

async fn prepare_cached_file_plans_with_progress(
    staging: &StagingArea,
    files: &[AddPlanFile<'_>],
    existing_refs: &[Option<ExistingChunkCandidate>],
    build_xorb_builder: &(dyn Fn() -> XorbBuilder + Send + Sync),
    prepared_cache: &mut PreparedXorbCache,
    mut summary: AddPushPlanSummary,
    cancel: &CancellationToken,
    mut on_progress: Option<&mut (dyn FnMut(&AddPushPlanSummary) + Send)>,
) -> Result<AddPushPlanSummary> {
    let mut ref_offset = 0usize;
    for file in files {
        check_cancelled(cancel)?;
        let next_offset = ref_offset.checked_add(file.chunks.len()).ok_or_else(|| {
            StagingError::Internal("add push-plan chunk count overflow".to_owned())
        })?;
        let file_existing_refs = existing_refs.get(ref_offset..next_offset).ok_or_else(|| {
            StagingError::Internal("add push-plan remote lookup length changed".to_owned())
        })?;
        let file_summary = prepare_one_file_plan_with_existing_refs(
            staging,
            file,
            file_existing_refs,
            build_xorb_builder,
            prepared_cache,
            cancel,
        )
        .await?;
        ref_offset = next_offset;
        accumulate_file_summary(&mut summary, file_summary);
        if let Some(callback) = on_progress.as_deref_mut() {
            callback(&summary);
        }
    }
    if ref_offset != existing_refs.len() {
        return Err(StagingError::Internal(
            "add push-plan remote lookup returned extra candidates".to_owned(),
        ));
    }

    debug_file_summary(&summary);
    Ok(summary)
}

fn accumulate_file_summary(summary: &mut AddPushPlanSummary, file_summary: FilePlanSummary) {
    summary.files += 1;
    summary.chunks += file_summary.chunks;
    summary.existing_candidates += file_summary.existing_candidates;
    summary.prepared_cache_chunks += file_summary.prepared_cache_chunks;
    summary.prepared_cache_xorbs += file_summary.prepared_cache_xorbs;
    summary.prepared_cache_link_misses += file_summary.prepared_cache_link_misses;
    summary.prepared_xorbs += file_summary.prepared_xorbs;
    summary.prepared_bytes += file_summary.prepared_bytes;
}

fn debug_file_summary(summary: &AddPushPlanSummary) {
    debug!(
        files = summary.files,
        chunks = summary.chunks,
        remote_lookup = summary.remote_lookup,
        existing_candidates = summary.existing_candidates,
        prepared_cache_chunks = summary.prepared_cache_chunks,
        prepared_cache_xorbs = summary.prepared_cache_xorbs,
        prepared_cache_link_misses = summary.prepared_cache_link_misses,
        prepared_xorbs = summary.prepared_xorbs,
        prepared_bytes = summary.prepared_bytes,
        "add push-plan: prepared file plans"
    );
}

struct PreparedCandidateChoice {
    candidate: Arc<PreparedXorbCandidate>,
    covered_chunks: Vec<MerkleHash>,
}

struct UncachedFilePlan<'a> {
    file_hash: MerkleHash,
    chunks: &'a [(MerkleHash, u64)],
    located_chunks: Vec<(MerkleHash, u64)>,
    uncovered_chunks: HashSet<MerkleHash>,
    plan: FilePushPlan,
}

struct VerifiedStagedChunks {
    chunks: Vec<(MerkleHash, u64)>,
    local_authority_xorbs: Vec<PlannedXorb>,
}

fn all_file_chunks(files: &[AddPlanFile<'_>]) -> Vec<(MerkleHash, u64)> {
    files
        .iter()
        .flat_map(|file| file.chunks.iter().copied())
        .collect()
}

async fn prepare_uncached_file_plans_with_progress(
    staging: &StagingArea,
    files: &[AddPlanFile<'_>],
    existing_refs: &[Option<ExistingChunkCandidate>],
    build_xorb_builder: &(dyn Fn() -> XorbBuilder + Send + Sync),
    remote_lookup: bool,
    cancel: &CancellationToken,
    mut on_progress: Option<&mut (dyn FnMut(&AddPushPlanSummary) + Send)>,
) -> Result<AddPushPlanSummary> {
    let mut file_plans = Vec::with_capacity(files.len());
    let mut ref_offset = 0usize;
    for file in files {
        check_cancelled(cancel)?;
        let next_offset = ref_offset.checked_add(file.chunks.len()).ok_or_else(|| {
            StagingError::Internal("add push-plan chunk count overflow".to_owned())
        })?;
        let file_existing_refs = existing_refs.get(ref_offset..next_offset).ok_or_else(|| {
            StagingError::Internal("add push-plan remote lookup length changed".to_owned())
        })?;
        file_plans.push(verified_uncached_file_plan(staging, file, file_existing_refs).await?);
        ref_offset = next_offset;
    }
    if ref_offset != existing_refs.len() {
        return Err(StagingError::Internal(
            "add push-plan remote lookup returned extra candidates".to_owned(),
        ));
    }

    let mut builder = build_xorb_builder();
    let mut queued_chunks = HashSet::new();
    let mut read_batch = Vec::with_capacity(ADD_PLAN_READ_BATCH_CHUNKS);
    for file_idx in 0..file_plans.len() {
        let run_id = RunId(file_idx as u64);
        for chunk_idx in 0..file_plans[file_idx].located_chunks.len() {
            let chunk = file_plans[file_idx].located_chunks[chunk_idx];
            if !file_plans[file_idx].uncovered_chunks.contains(&chunk.0) {
                continue;
            }
            if !queued_chunks.insert(chunk.0) {
                continue;
            }
            read_batch.push((chunk, run_id));
            if read_batch.len() >= ADD_PLAN_READ_BATCH_CHUNKS {
                flush_uncached_read_batch(staging, &mut read_batch, &mut builder, cancel).await?;
                write_completed_uncached_xorbs(staging, &mut file_plans, &mut builder).await?;
            }
        }
    }
    flush_uncached_read_batch(staging, &mut read_batch, &mut builder, cancel).await?;
    write_completed_uncached_xorbs(staging, &mut file_plans, &mut builder).await?;
    for result in builder.finalize()? {
        record_uncached_prepared_xorb(staging, &mut file_plans, result).await?;
    }

    let mut summary = AddPushPlanSummary {
        remote_lookup,
        ..AddPushPlanSummary::default()
    };
    let mut summarized_prepared_xorbs = HashSet::new();
    for file_plan in &file_plans {
        let recipe = crate::recipe::FileRecipe::from_staged_chunks(
            crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            file_plan.file_hash,
            file_plan.plan.file_size,
            file_plan.chunks,
        )?;
        staging
            .write_file_push_plan_for_recipe(&file_plan.plan, &recipe)
            .await?;
        summary.files += 1;
        summary.chunks += file_plan.chunks.len() as u64;
        summary.existing_candidates += file_plan.plan.existing.len() as u64;
        for xorb in &file_plan.plan.prepared_xorbs {
            if summarized_prepared_xorbs.insert(xorb.hash.clone()) {
                summary.prepared_xorbs += 1;
                summary.prepared_bytes += xorb.bytes;
            }
        }
        if let Some(callback) = on_progress.as_deref_mut() {
            callback(&summary);
        }
    }
    Ok(summary)
}

async fn verified_uncached_file_plan<'a>(
    staging: &StagingArea,
    file: &'a AddPlanFile<'_>,
    existing_refs: &[Option<ExistingChunkCandidate>],
) -> Result<UncachedFilePlan<'a>> {
    let file_hash = MerkleHash::from(file.file_hash);
    let verified = verified_staged_chunks(staging, file_hash, file.size, file.chunks).await?;
    let mut plan = FilePushPlan::new_verified_staging(file_hash, file.size, file.chunks);
    plan.prepared_xorbs = verified.local_authority_xorbs;
    let mut uncovered_chunks = HashSet::new();
    for ((chunk_hash, size), existing_ref) in file.chunks.iter().zip(existing_refs.iter()) {
        if let Some(candidate) = existing_ref
            && u64::from(candidate.xorb_ref.uncompressed_size) == *size
        {
            plan.existing.push(PlannedExistingChunk::from_candidate(
                *chunk_hash,
                *candidate,
            ));
            continue;
        }
        uncovered_chunks.insert(*chunk_hash);
    }
    Ok(UncachedFilePlan {
        file_hash,
        chunks: file.chunks,
        located_chunks: verified.chunks,
        uncovered_chunks,
        plan,
    })
}

async fn verified_staged_chunks(
    staging: &StagingArea,
    file_hash: MerkleHash,
    file_size: u64,
    expected_chunks: &[(MerkleHash, u64)],
) -> Result<VerifiedStagedChunks> {
    if let Some(plan) = staging.load_file_push_plan(&file_hash).await?
        && plan.chunk_count == expected_chunks.len() as u64
        && plan.sequence_hash()? == crate::push_plan::chunk_sequence_hash(expected_chunks)
        && plan.file_size == file_size
    {
        return Ok(VerifiedStagedChunks {
            chunks: expected_chunks.to_vec(),
            local_authority_xorbs: plan
                .prepared_xorbs
                .into_iter()
                .map(|mut xorb| {
                    xorb.upload = false;
                    xorb
                })
                .collect(),
        });
    }
    let located_chunks = staging.chunks_for_file_with_locators(&file_hash)?;
    if located_chunks.len() != expected_chunks.len()
        || located_chunks
            .iter()
            .zip(expected_chunks.iter())
            .any(|(located, expected)| located.hash != expected.0 || located.size != expected.1)
    {
        return Err(StagingError::StagingCorrupt(format!(
            "staged chunk rows for file {} changed while preparing add push plan",
            file_hash.hex()
        )));
    }
    let planned_size = located_chunks.iter().try_fold(0u64, |acc, chunk| {
        acc.checked_add(chunk.size).ok_or_else(|| {
            StagingError::StagingCorrupt(format!(
                "staged chunk sizes overflow for file {} while preparing add push plan",
                file_hash.hex()
            ))
        })
    })?;
    if planned_size != file_size {
        return Err(StagingError::StagingCorrupt(format!(
            "staged chunk rows for file {} total {planned_size} bytes, expected {file_size}",
            file_hash.hex()
        )));
    }
    Ok(VerifiedStagedChunks {
        chunks: located_chunks
            .into_iter()
            .map(|chunk| (chunk.hash, chunk.size))
            .collect(),
        local_authority_xorbs: Vec::new(),
    })
}

async fn flush_uncached_read_batch(
    staging: &StagingArea,
    read_batch: &mut Vec<((MerkleHash, u64), RunId)>,
    builder: &mut XorbBuilder,
    cancel: &CancellationToken,
) -> Result<()> {
    if read_batch.is_empty() {
        return Ok(());
    }
    check_cancelled(cancel)?;
    let hashes = read_batch
        .iter()
        .map(|((hash, _), _)| *hash)
        .collect::<Vec<_>>();
    let payloads = staging.get_chunks_batch(&hashes).await?;
    let mut to_pack = Vec::with_capacity(read_batch.len());
    for ((expected, run_id), (actual_hash, data)) in read_batch.iter().zip(payloads) {
        to_pack.push((
            Chunk {
                hash: actual_hash,
                data,
            },
            *run_id,
        ));
        debug_assert_eq!(actual_hash, expected.0);
    }
    builder.push_batch(&to_pack)?;
    read_batch.clear();
    Ok(())
}

async fn write_completed_uncached_xorbs(
    staging: &StagingArea,
    file_plans: &mut [UncachedFilePlan<'_>],
    builder: &mut XorbBuilder,
) -> Result<()> {
    while let Some(result) = builder.take_completed() {
        record_uncached_prepared_xorb(staging, file_plans, result).await?;
    }
    Ok(())
}

async fn record_uncached_prepared_xorb(
    staging: &StagingArea,
    file_plans: &mut [UncachedFilePlan<'_>],
    result: crab_xet::xorb::builder::XorbResult,
) -> Result<()> {
    let bytes = result.bytes.len() as u64;
    let payload_hash = blake3::hash(&result.bytes).to_hex().to_string();
    let placements: Vec<PlannedPlacement> = result
        .placements
        .iter()
        .map(PlannedPlacement::from_placement)
        .collect();
    let planned = PlannedXorb {
        hash: result.hash.hex(),
        payload_hash,
        bytes,
        upload: true,
        placements,
    };

    let recipients: Vec<usize> = file_plans
        .iter()
        .enumerate()
        .filter_map(|(idx, file_plan)| {
            result
                .placements
                .iter()
                .any(|placement| file_plan.uncovered_chunks.contains(&placement.chunk_hash))
                .then_some(idx)
        })
        .collect();
    let Some((&owner_idx, linked_idxs)) = recipients.split_first() else {
        return Err(StagingError::Internal(
            "prepared xorb has no owning add file".to_owned(),
        ));
    };
    let owner_hash = file_plans[owner_idx].file_hash;
    write_prepared_xorb(
        staging.root(),
        &owner_hash,
        &result.hash,
        result.bytes.clone(),
    )
    .await?;
    file_plans[owner_idx]
        .plan
        .prepared_xorbs
        .push(planned.clone());

    for idx in linked_idxs {
        let target_hash = file_plans[*idx].file_hash;
        if !link_prepared_xorb(staging.root(), &owner_hash, &target_hash, &planned).await? {
            write_prepared_xorb(
                staging.root(),
                &target_hash,
                &result.hash,
                result.bytes.clone(),
            )
            .await?;
        }
        file_plans[*idx].plan.prepared_xorbs.push(planned.clone());
    }
    Ok(())
}

async fn prepare_one_file_plan(
    staging: &StagingArea,
    file: &AddPlanFile<'_>,
    build_xorb_builder: &(dyn Fn() -> XorbBuilder + Send + Sync),
    remote_lookup: Option<&dyn ExistingChunkLookup>,
    prepared_cache: &mut PreparedXorbCache,
    cancel: &CancellationToken,
) -> Result<FilePlanSummary> {
    let existing_refs = lookup_existing_candidates(file.chunks, remote_lookup).await?;
    prepare_one_file_plan_with_existing_refs(
        staging,
        file,
        &existing_refs,
        build_xorb_builder,
        prepared_cache,
        cancel,
    )
    .await
}

async fn prepare_one_file_plan_with_existing_refs(
    staging: &StagingArea,
    file: &AddPlanFile<'_>,
    existing_refs: &[Option<ExistingChunkCandidate>],
    build_xorb_builder: &(dyn Fn() -> XorbBuilder + Send + Sync),
    prepared_cache: &mut PreparedXorbCache,
    cancel: &CancellationToken,
) -> Result<FilePlanSummary> {
    if existing_refs.len() != file.chunks.len() {
        return Err(StagingError::Internal(format!(
            "add push-plan remote lookup returned {} candidates for {} requested chunks",
            existing_refs.len(),
            file.chunks.len()
        )));
    }
    let file_hash = MerkleHash::from(file.file_hash);
    let chunks = file.chunks;
    let verified = verified_staged_chunks(staging, file_hash, file.size, chunks).await?;
    let located_chunks = verified.chunks;
    let mut plan = FilePushPlan::new_verified_staging(file_hash, file.size, chunks);
    plan.prepared_xorbs = verified.local_authority_xorbs;

    let mut file_chunk_sizes = HashMap::new();
    for (chunk_hash, size) in chunks {
        file_chunk_sizes.entry(*chunk_hash).or_insert(*size);
    }
    let remote_existing_chunks: HashSet<MerkleHash> = chunks
        .iter()
        .zip(existing_refs.iter())
        .filter_map(|((chunk_hash, size), existing_ref)| {
            existing_ref
                .as_ref()
                .filter(|candidate| u64::from(candidate.xorb_ref.uncompressed_size) == *size)
                .map(|_| *chunk_hash)
        })
        .collect();
    let mut new_chunks = HashSet::new();
    let mut seen = HashSet::new();
    let mut planned_prepared_xorbs = HashSet::new();
    let mut covered_by_prepared_cache = HashSet::new();
    let mut cache_chunks = 0u64;
    let mut cache_xorbs = 0u64;
    let mut cache_link_misses = 0u64;
    let mut unusable_cached_xorb_sources = HashSet::new();
    for ((chunk_hash, size), existing_ref) in chunks.iter().zip(existing_refs.iter()) {
        if let Some(candidate) = existing_ref
            && u64::from(candidate.xorb_ref.uncompressed_size) == *size
        {
            plan.existing.push(PlannedExistingChunk::from_candidate(
                *chunk_hash,
                *candidate,
            ));
            seen.insert(*chunk_hash);
            continue;
        }
        if !seen.insert(*chunk_hash) {
            continue;
        }

        if covered_by_prepared_cache.contains(chunk_hash) {
            continue;
        }

        let choices = ranked_prepared_candidates(
            prepared_cache,
            chunk_hash,
            *size,
            &file_chunk_sizes,
            &remote_existing_chunks,
            &covered_by_prepared_cache,
            &unusable_cached_xorb_sources,
        );
        let mut used_cached_candidate = false;
        for choice in choices {
            let candidate = choice.candidate;
            if planned_prepared_xorbs.contains(&candidate.xorb_hash) {
                cache_chunks += mark_prepared_cache_coverage(
                    &choice.covered_chunks,
                    &mut covered_by_prepared_cache,
                    &mut new_chunks,
                );
                used_cached_candidate = true;
                break;
            }

            if materialize_prepared_xorb(staging.root(), &candidate, &file_hash).await? {
                let mut planned = candidate.planned.clone();
                planned.upload = true;
                if let Some(authority) = plan
                    .prepared_xorbs
                    .iter_mut()
                    .find(|authority| authority.hash == planned.hash)
                {
                    *authority = planned;
                } else {
                    plan.prepared_xorbs.push(planned);
                }
                planned_prepared_xorbs.insert(candidate.xorb_hash);
                cache_chunks += mark_prepared_cache_coverage(
                    &choice.covered_chunks,
                    &mut covered_by_prepared_cache,
                    &mut new_chunks,
                );
                cache_xorbs += 1;
                used_cached_candidate = true;
                break;
            }

            unusable_cached_xorb_sources.insert((candidate.xorb_hash, candidate.source.clone()));
            cache_link_misses += 1;
        }
        if used_cached_candidate {
            continue;
        }

        new_chunks.insert(*chunk_hash);
    }

    let mut builder = build_xorb_builder();

    let mut pending_new_chunks = Vec::with_capacity(new_chunks.len());
    let mut queued_new_chunks = HashSet::new();
    for chunk in located_chunks {
        if new_chunks.contains(&chunk.0) && queued_new_chunks.insert(chunk.0) {
            pending_new_chunks.push(chunk);
        }
    }

    for batch in pending_new_chunks.chunks(ADD_PLAN_READ_BATCH_CHUNKS) {
        check_cancelled(cancel)?;
        let hashes = batch.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        let payloads = staging.get_chunks_batch(&hashes).await?;
        let mut to_pack = Vec::with_capacity(payloads.len());
        for (actual_hash, data) in payloads {
            to_pack.push((
                Chunk {
                    hash: actual_hash,
                    data,
                },
                RunId(0),
            ));
        }

        if !to_pack.is_empty() {
            builder.push_batch(&to_pack)?;
            write_completed_xorbs(staging, &file_hash, &mut builder, &mut plan, prepared_cache)
                .await?;
        }
    }

    for result in builder.finalize()? {
        record_prepared_xorb(staging, &file_hash, &mut plan, prepared_cache, result).await?;
    }

    let file_summary = FilePlanSummary {
        chunks: chunks.len() as u64,
        existing_candidates: plan.existing.len() as u64,
        prepared_cache_chunks: cache_chunks,
        prepared_cache_xorbs: cache_xorbs,
        prepared_cache_link_misses: cache_link_misses,
        prepared_xorbs: plan.prepared_xorbs.len() as u64,
        prepared_bytes: plan.prepared_xorbs.iter().map(|xorb| xorb.bytes).sum(),
    };
    let recipe = crate::recipe::FileRecipe::from_staged_chunks(
        crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
        file_hash,
        file.size,
        chunks,
    )?;
    staging
        .write_file_push_plan_for_recipe(&plan, &recipe)
        .await?;
    Ok(file_summary)
}

fn ranked_prepared_candidates(
    prepared_cache: &PreparedXorbCache,
    chunk_hash: &MerkleHash,
    expected_size: u64,
    file_chunk_sizes: &HashMap<MerkleHash, u64>,
    remote_existing_chunks: &HashSet<MerkleHash>,
    covered_by_prepared_cache: &HashSet<MerkleHash>,
    unusable_cached_xorb_sources: &HashSet<(MerkleHash, PreparedXorbSource)>,
) -> Vec<PreparedCandidateChoice> {
    let mut choices = Vec::new();
    for candidate in prepared_cache.candidates_for_chunk(chunk_hash) {
        if unusable_cached_xorb_sources.contains(&(candidate.xorb_hash, candidate.source.clone())) {
            continue;
        }
        let Some(placement) = candidate.placement_for(chunk_hash) else {
            continue;
        };
        if u64::from(placement.uncompressed_size) != expected_size {
            continue;
        }
        let covered_chunks = matching_file_chunks(
            &candidate,
            file_chunk_sizes,
            remote_existing_chunks,
            covered_by_prepared_cache,
        );
        if covered_chunks.is_empty() {
            continue;
        }
        choices.push(PreparedCandidateChoice {
            candidate,
            covered_chunks,
        });
    }
    choices.sort_by(|left, right| {
        right
            .covered_chunks
            .len()
            .cmp(&left.covered_chunks.len())
            .then_with(|| {
                left.candidate
                    .planned
                    .bytes
                    .cmp(&right.candidate.planned.bytes)
            })
            .then_with(|| {
                left.candidate
                    .xorb_hash
                    .hex()
                    .cmp(&right.candidate.xorb_hash.hex())
            })
    });
    choices
}

fn matching_file_chunks(
    candidate: &PreparedXorbCandidate,
    file_chunk_sizes: &HashMap<MerkleHash, u64>,
    remote_existing_chunks: &HashSet<MerkleHash>,
    covered_by_prepared_cache: &HashSet<MerkleHash>,
) -> Vec<MerkleHash> {
    let mut seen = HashSet::new();
    let mut covered = Vec::new();
    for placement in &candidate.placements {
        if !seen.insert(placement.chunk_hash)
            || remote_existing_chunks.contains(&placement.chunk_hash)
            || covered_by_prepared_cache.contains(&placement.chunk_hash)
        {
            continue;
        }
        let Some(expected_size) = file_chunk_sizes.get(&placement.chunk_hash) else {
            continue;
        };
        if u64::from(placement.uncompressed_size) == *expected_size {
            covered.push(placement.chunk_hash);
        }
    }
    covered
}

fn mark_prepared_cache_coverage(
    covered_chunks: &[MerkleHash],
    covered_by_prepared_cache: &mut HashSet<MerkleHash>,
    new_chunks: &mut HashSet<MerkleHash>,
) -> u64 {
    let mut newly_covered = 0;
    for chunk_hash in covered_chunks {
        if covered_by_prepared_cache.insert(*chunk_hash) {
            new_chunks.remove(chunk_hash);
            newly_covered += 1;
        }
    }
    newly_covered
}

async fn write_completed_xorbs(
    staging: &StagingArea,
    file_hash: &MerkleHash,
    builder: &mut XorbBuilder,
    plan: &mut FilePushPlan,
    prepared_cache: &mut PreparedXorbCache,
) -> Result<()> {
    while let Some(result) = builder.take_completed() {
        record_prepared_xorb(staging, file_hash, plan, prepared_cache, result).await?;
    }
    Ok(())
}

async fn record_prepared_xorb(
    staging: &StagingArea,
    file_hash: &MerkleHash,
    plan: &mut FilePushPlan,
    prepared_cache: &mut PreparedXorbCache,
    result: crab_xet::xorb::builder::XorbResult,
) -> Result<()> {
    let bytes = result.bytes.len() as u64;
    let payload_hash = blake3::hash(&result.bytes).to_hex().to_string();
    let placements: Vec<PlannedPlacement> = result
        .placements
        .iter()
        .map(PlannedPlacement::from_placement)
        .collect();
    write_prepared_xorb(staging.root(), file_hash, &result.hash, result.bytes).await?;
    let planned = PlannedXorb {
        hash: result.hash.hex(),
        payload_hash,
        bytes,
        upload: true,
        placements,
    };
    prepared_cache.insert_prepared_xorb(*file_hash, &planned)?;
    plan.prepared_xorbs.push(planned);
    Ok(())
}

async fn lookup_existing_candidates(
    chunks: &[(MerkleHash, u64)],
    remote_lookup: Option<&dyn ExistingChunkLookup>,
) -> Result<Vec<Option<ExistingChunkCandidate>>> {
    let Some(remote_lookup) = remote_lookup else {
        return Ok(vec![None; chunks.len()]);
    };
    let refs = remote_lookup.lookup_existing_candidates(chunks).await?;
    if refs.len() != chunks.len() {
        return Err(StagingError::Internal(format!(
            "existing chunk lookup returned {} candidates for {} requested chunks",
            refs.len(),
            chunks.len()
        )));
    }
    Ok(refs)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use bytes::Bytes;
    use crab_xet::hash::compute_data_hash;
    use crab_xet::xorb::builder::{CompressionPolicy, FixedCompression};
    use crab_xet::xorb::format::CompressionScheme;

    const CHUNKS_ABOVE_OLD_REMOTE_LOOKUP_CAP: usize = 100_001;

    fn recipe_pairs(
        staging: &StagingArea,
        recipe: &crate::recipe::FileRecipe,
    ) -> Vec<(MerkleHash, u64)> {
        let mut pairs = Vec::new();
        let mut next = 0u64;
        while next < recipe.chunk_count() {
            let page = staging.recipe_page(recipe, next).expect("recipe page");
            pairs.extend(
                page.chunks
                    .iter()
                    .map(|chunk| (chunk.chunk_hash, chunk.len)),
            );
            next = page.next_occurrence();
        }
        pairs
    }

    fn remote_chunks(
        staging: &StagingArea,
        recipe: &crate::recipe::FileRecipe,
    ) -> Vec<(MerkleHash, ExistingChunkCandidate)> {
        let mut chunks = Vec::new();
        let mut next = 0u64;
        while next < recipe.chunk_count() {
            let page = staging
                .recipe_remote_chunk_page(recipe, next)
                .expect("remote authority page");
            assert!(page.len() <= crate::recipe::RECIPE_PAGE_ENTRIES);
            chunks.extend(page);
            next += crate::recipe::RECIPE_PAGE_ENTRIES as u64;
        }
        chunks
    }

    struct AllExistingLookup {
        calls: AtomicUsize,
        chunks_seen: AtomicUsize,
    }

    impl AllExistingLookup {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                chunks_seen: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ExistingChunkLookup for AllExistingLookup {
        async fn lookup_existing_candidates(
            &self,
            chunks: &[(MerkleHash, u64)],
        ) -> Result<Vec<Option<ExistingChunkCandidate>>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.chunks_seen.fetch_add(chunks.len(), Ordering::Relaxed);
            Ok(chunks
                .iter()
                .enumerate()
                .map(|(idx, (_, size))| {
                    Some(existing_candidate(
                        XorbRef {
                            xorb_hash: numbered_hash(idx + 1_000_000),
                            chunk_index: idx as u32,
                            uncompressed_size: (*size)
                                .try_into()
                                .expect("test chunk size fits u32"),
                        },
                        idx,
                    ))
                })
                .collect())
        }
    }

    struct NoExistingLookup {
        calls: AtomicUsize,
        chunks_seen: AtomicUsize,
    }

    impl NoExistingLookup {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                chunks_seen: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ExistingChunkLookup for NoExistingLookup {
        async fn lookup_existing_candidates(
            &self,
            chunks: &[(MerkleHash, u64)],
        ) -> Result<Vec<Option<ExistingChunkCandidate>>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.chunks_seen.fetch_add(chunks.len(), Ordering::Relaxed);
            Ok(vec![None; chunks.len()])
        }
    }

    struct SelectiveExistingLookup {
        calls: AtomicUsize,
        chunks_seen: AtomicUsize,
        existing_hash: MerkleHash,
    }

    impl SelectiveExistingLookup {
        fn new(existing_hash: MerkleHash) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                chunks_seen: AtomicUsize::new(0),
                existing_hash,
            }
        }
    }

    #[async_trait]
    impl ExistingChunkLookup for SelectiveExistingLookup {
        async fn lookup_existing_candidates(
            &self,
            chunks: &[(MerkleHash, u64)],
        ) -> Result<Vec<Option<ExistingChunkCandidate>>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.chunks_seen.fetch_add(chunks.len(), Ordering::Relaxed);
            Ok(chunks
                .iter()
                .enumerate()
                .map(|(idx, (chunk_hash, size))| {
                    (*chunk_hash == self.existing_hash).then(|| {
                        existing_candidate(
                            XorbRef {
                                xorb_hash: numbered_hash(idx + 2_000_000),
                                chunk_index: idx as u32,
                                uncompressed_size: (*size)
                                    .try_into()
                                    .expect("test chunk size fits u32"),
                            },
                            idx,
                        )
                    })
                })
                .collect())
        }
    }

    fn numbered_hash(idx: usize) -> MerkleHash {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&(idx as u64).to_le_bytes());
        bytes[8] = 0xA5;
        MerkleHash::from(bytes)
    }

    fn existing_candidate(xorb_ref: XorbRef, seed: usize) -> ExistingChunkCandidate {
        ExistingChunkCandidate {
            xorb_ref,
            placement_id: numbered_hash(seed + 2_000_000).into(),
            origin_proof_id: numbered_hash(seed + 3_000_000).into(),
        }
    }

    async fn stage_synthetic_file(
        staging: &StagingArea,
        chunks: &[(MerkleHash, u64)],
    ) -> MerkleHash {
        let mut bytes = [0xF1; 32];
        bytes[..8].copy_from_slice(&(chunks.len() as u64).to_le_bytes());
        let file_hash = MerkleHash::from(bytes);
        staging
            .pre_register_file(&file_hash, chunks.len() as u64)
            .expect("pre-register synthetic file");

        let data = [0u8; 1];
        for (batch_idx, batch) in chunks.chunks(1024).enumerate() {
            let offset = batch_idx * 1024;
            let refs: Vec<(&MerkleHash, &[u8])> =
                batch.iter().map(|(hash, _)| (hash, &data[..])).collect();
            staging
                .stage_chunks_batch(&refs, &file_hash, offset as u64)
                .await
                .expect("stage synthetic chunk batch");
        }
        let recipe = crate::recipe::FileRecipe::from_staged_chunks(
            crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            file_hash,
            chunks.len() as u64,
            chunks,
        )
        .expect("synthetic recipe");
        staging
            .publish_verified_recipe_lease(
                &std::path::PathBuf::from(format!("synthetic-{}.bin", file_hash.hex())),
                &recipe,
            )
            .expect("publish synthetic recipe");
        file_hash
    }

    async fn stage_file_with_data(
        staging: &StagingArea,
        chunks: &[(MerkleHash, Vec<u8>)],
    ) -> MerkleHash {
        let mut hasher = blake3::Hasher::new();
        for (_, data) in chunks {
            hasher.update(data);
        }
        let file_hash = MerkleHash::from(*hasher.finalize().as_bytes());
        let total_bytes = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        staging
            .pre_register_file(&file_hash, total_bytes)
            .expect("pre-register test file");
        let refs: Vec<(&MerkleHash, &[u8])> = chunks
            .iter()
            .map(|(hash, data)| (hash, data.as_slice()))
            .collect();
        staging
            .stage_chunks_batch(&refs, &file_hash, 0)
            .await
            .expect("stage test file");
        let chunk_pairs = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect::<Vec<_>>();
        let recipe = crate::recipe::FileRecipe::from_staged_chunks(
            crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            file_hash,
            total_bytes,
            &chunk_pairs,
        )
        .expect("test recipe");
        staging
            .publish_verified_recipe_lease(
                &std::path::PathBuf::from(format!("test-{}.bin", file_hash.hex())),
                &recipe,
            )
            .expect("publish test recipe");
        file_hash
    }

    fn small_raw_xorb_builder() -> XorbBuilder {
        let policy =
            Arc::new(FixedCompression::new(CompressionScheme::None)) as Arc<dyn CompressionPolicy>;
        let mut builder = XorbBuilder::with_policy(policy)
            .with_size_bounds(1, 1024)
            .with_max_overshoot(0);
        builder.set_target_size(64);
        builder
    }

    async fn write_json_only_prepared_plan(
        staging: &StagingArea,
        source_file_hash: MerkleHash,
        chunk_hash: MerkleHash,
        data: &[u8],
    ) -> MerkleHash {
        let mut builder = small_raw_xorb_builder();
        builder
            .push(
                &Chunk {
                    hash: chunk_hash,
                    data: Bytes::copy_from_slice(data),
                },
                RunId(0),
            )
            .expect("push JSON-only chunk");
        let mut results = builder.finalize().expect("finalize JSON-only xorb");
        assert_eq!(results.len(), 1);
        let result = results.pop().expect("JSON-only xorb result");
        let payload_hash = blake3::hash(&result.bytes).to_hex().to_string();

        write_prepared_xorb(
            staging.root(),
            &source_file_hash,
            &result.hash,
            result.bytes.clone(),
        )
        .await
        .expect("write legacy prepared xorb");

        let chunk_pairs = [(chunk_hash, data.len() as u64)];
        let mut plan =
            FilePushPlan::new_verified_staging(source_file_hash, data.len() as u64, &chunk_pairs);
        plan.prepared_xorbs.push(PlannedXorb {
            hash: result.hash.hex(),
            payload_hash,
            bytes: result.bytes.len() as u64,
            upload: true,
            placements: result
                .placements
                .iter()
                .map(PlannedPlacement::from_placement)
                .collect(),
        });
        crate::push_plan::write_file_push_plan(staging.root(), &plan)
            .await
            .expect("write JSON-only plan");
        result.hash
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn large_add_batches_still_use_remote_existing_lookup() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let chunks: Vec<(MerkleHash, u64)> = (0..CHUNKS_ABOVE_OLD_REMOTE_LOOKUP_CAP)
            .map(|idx| (numbered_hash(idx), 1))
            .collect();
        let file_hash = stage_synthetic_file(&staging, &chunks).await;
        let file_hash_bytes: [u8; 32] = file_hash.into();
        let lookup = AllExistingLookup::new();
        let cancel = CancellationToken::new();

        let summary = prepare_file_push_plans(
            &staging,
            &[AddPlanFile {
                file_hash: file_hash_bytes,
                size: chunks.len() as u64,
                chunks: &chunks,
            }],
            &XorbBuilder::new,
            Some(&lookup),
            None,
            &cancel,
        )
        .await
        .expect("prepare push plan");

        assert!(summary.remote_lookup);
        assert_eq!(summary.chunks, CHUNKS_ABOVE_OLD_REMOTE_LOOKUP_CAP as u64);
        assert_eq!(
            summary.existing_candidates,
            CHUNKS_ABOVE_OLD_REMOTE_LOOKUP_CAP as u64
        );
        assert_eq!(summary.prepared_xorbs, 0);
        assert_eq!(summary.prepared_bytes, 0);
        assert_eq!(lookup.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            lookup.chunks_seen.load(Ordering::Relaxed),
            CHUNKS_ABOVE_OLD_REMOTE_LOOKUP_CAP
        );

        let plan = staging
            .load_file_push_plan(&file_hash)
            .await
            .expect("load file push plan")
            .expect("file push plan exists");
        assert!(plan.existing.is_empty());
        let recipe = staging
            .published_recipe_for_file(&file_hash)
            .expect("load published recipe")
            .expect("published recipe exists");
        let mut remote_chunks = 0usize;
        let mut next = 0u64;
        while next < recipe.chunk_count() {
            let page = staging
                .recipe_remote_chunk_page(&recipe, next)
                .expect("remote authority page");
            assert!(page.len() <= crate::recipe::RECIPE_PAGE_ENTRIES);
            remote_chunks += page.len();
            next += crate::recipe::RECIPE_PAGE_ENTRIES as u64;
        }
        assert_eq!(remote_chunks, CHUNKS_ABOVE_OLD_REMOTE_LOOKUP_CAP);
        assert!(plan.prepared_xorbs.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn all_new_multi_file_plan_packs_across_file_boundaries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let first_data = vec![0x11; 10];
        let second_data = vec![0x22; 11];
        let first_chunks = vec![(compute_data_hash(&first_data), first_data)];
        let second_chunks = vec![(compute_data_hash(&second_data), second_data)];
        let first_hash = stage_file_with_data(&staging, &first_chunks).await;
        let second_hash = stage_file_with_data(&staging, &second_chunks).await;
        let first_pairs: Vec<(MerkleHash, u64)> = first_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let second_pairs: Vec<(MerkleHash, u64)> = second_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let summary = prepare_file_push_plans(
            &staging,
            &[
                AddPlanFile {
                    file_hash: first_hash.into(),
                    size: 10,
                    chunks: &first_pairs,
                },
                AddPlanFile {
                    file_hash: second_hash.into(),
                    size: 11,
                    chunks: &second_pairs,
                },
            ],
            &small_raw_xorb_builder,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare push plans");

        assert_eq!(summary.files, 2);
        assert_eq!(summary.prepared_xorbs, 1);

        let first_plan = staging
            .load_file_push_plan(&first_hash)
            .await
            .expect("load first plan")
            .expect("first plan exists");
        let second_plan = staging
            .load_file_push_plan(&second_hash)
            .await
            .expect("load second plan")
            .expect("second plan exists");

        assert_eq!(first_plan.prepared_xorbs.len(), 1);
        assert_eq!(second_plan.prepared_xorbs.len(), 1);
        assert_eq!(
            first_plan.prepared_xorbs[0].hash,
            second_plan.prepared_xorbs[0].hash
        );
        assert_eq!(first_plan.prepared_xorbs[0].placements.len(), 2);
        assert_eq!(second_plan.prepared_xorbs[0].placements.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_add_reuses_indexed_prepared_xorb_without_json_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let data = vec![0x77; 10];
        let chunks = vec![(compute_data_hash(&data), data)];
        let file_hash = stage_file_with_data(&staging, &chunks).await;
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let file_hash_bytes = file_hash.into();

        let first_summary = prepare_file_push_plans(
            &staging,
            &[AddPlanFile {
                file_hash: file_hash_bytes,
                size: 10,
                chunks: &chunk_pairs,
            }],
            &small_raw_xorb_builder,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare initial push plan");
        assert_eq!(first_summary.prepared_xorbs, 1);

        let first_plan = staging
            .load_file_push_plan(&file_hash)
            .await
            .expect("load first plan")
            .expect("first plan exists");
        let first_xorb_hash = first_plan.prepared_xorbs[0].hash.clone();
        assert!(
            !crate::push_plan::file_plan_path(staging.root(), &file_hash).exists(),
            "add-time plans should be stored in the staging index only"
        );

        let second_summary = prepare_file_push_plans(
            &staging,
            &[AddPlanFile {
                file_hash: file_hash_bytes,
                size: 10,
                chunks: &chunk_pairs,
            }],
            &small_raw_xorb_builder,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare repeated push plan");

        assert_eq!(second_summary.prepared_cache_chunks, 1);
        assert_eq!(second_summary.prepared_cache_xorbs, 1);
        assert_eq!(second_summary.prepared_xorbs, 1);

        let second_plan = staging
            .load_file_push_plan(&file_hash)
            .await
            .expect("load second plan")
            .expect("second plan exists");
        assert_eq!(second_plan.prepared_xorbs[0].hash, first_xorb_hash);

        assert!(
            staging
                .unregister_file(&file_hash)
                .expect("unregister file")
        );
        let wanted_chunks = chunk_pairs
            .iter()
            .map(|(chunk_hash, _)| *chunk_hash)
            .collect::<std::collections::HashSet<_>>();
        assert!(
            staging
                .load_prepared_xorb_cache_for_chunks(&wanted_chunks)
                .expect("load prepared cache after unregister")
                .is_empty()
        );
    }

    #[test]
    fn ranked_prepared_candidates_keeps_alternate_sources_for_same_xorb() {
        let chunk_hash = numbered_hash(30_001);
        let xorb_hash = numbered_hash(30_002);
        let planned = PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: blake3::hash(b"same prepared payload").to_hex().to_string(),
            bytes: 128,
            upload: true,
            placements: vec![PlannedPlacement {
                chunk_hash: chunk_hash.hex(),
                xorb_hash: xorb_hash.hex(),
                chunk_index: 0,
                uncompressed_size: 10,
            }],
        };
        let first_source = PreparedXorbSource::LocalCache("first.xorb".into());
        let second_source = PreparedXorbSource::LocalCache("second.xorb".into());
        let mut cache = PreparedXorbCache::default();
        cache
            .insert_cached_xorb("first.xorb".into(), &planned)
            .expect("insert first source");
        cache
            .insert_cached_xorb("second.xorb".into(), &planned)
            .expect("insert second source");

        let file_chunk_sizes = HashMap::from([(chunk_hash, 10)]);
        let empty_chunks = HashSet::new();
        let choices = ranked_prepared_candidates(
            &cache,
            &chunk_hash,
            10,
            &file_chunk_sizes,
            &empty_chunks,
            &empty_chunks,
            &HashSet::new(),
        );
        assert_eq!(choices.len(), 2);

        let filtered = ranked_prepared_candidates(
            &cache,
            &chunk_hash,
            10,
            &file_chunk_sizes,
            &empty_chunks,
            &empty_chunks,
            &HashSet::from([(xorb_hash, first_source.clone())]),
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].candidate.source, second_source);
        assert_ne!(filtered[0].candidate.source, first_source);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retire_file_removes_indexed_prepared_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let data = vec![0x7A; 10];
        let chunks = vec![(compute_data_hash(&data), data)];
        let file_hash = stage_file_with_data(&staging, &chunks).await;
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let summary = prepare_file_push_plans(
            &staging,
            &[AddPlanFile {
                file_hash: file_hash.into(),
                size: 10,
                chunks: &chunk_pairs,
            }],
            &small_raw_xorb_builder,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare push plan");
        assert_eq!(summary.prepared_xorbs, 1);
        assert!(
            staging
                .load_file_push_plan(&file_hash)
                .await
                .expect("load push plan")
                .is_some()
        );
        let plan = staging
            .load_file_push_plan(&file_hash)
            .await
            .expect("load prepared plan")
            .expect("prepared plan exists");
        let prepared_xorb_hash = plan.prepared_xorbs[0].hash().expect("prepared xorb hash");
        let prepared_xorb_path =
            crate::push_plan::prepared_xorb_path(staging.root(), &file_hash, &prepared_xorb_hash);
        assert!(prepared_xorb_path.exists());

        let retired = staging.retire_file(&file_hash).expect("retire file");
        assert_eq!(retired.rows_deleted, 1);

        let wanted_chunks = chunk_pairs
            .iter()
            .map(|(chunk_hash, _)| *chunk_hash)
            .collect::<std::collections::HashSet<_>>();
        assert!(
            staging
                .load_prepared_xorb_cache_for_chunks(&wanted_chunks)
                .expect("load prepared cache after retire")
                .is_empty()
        );
        assert!(
            staging
                .load_file_push_plan(&file_hash)
                .await
                .expect("load retired push plan")
                .is_none()
        );
        assert!(!prepared_xorb_path.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prepared_cache_ignores_json_only_candidates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");

        let indexed_data = vec![0x88; 10];
        let indexed_chunks = vec![(compute_data_hash(&indexed_data), indexed_data.clone())];
        let indexed_file_hash = stage_file_with_data(&staging, &indexed_chunks).await;
        let indexed_pairs: Vec<(MerkleHash, u64)> = indexed_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let indexed_summary = prepare_file_push_plans(
            &staging,
            &[AddPlanFile {
                file_hash: indexed_file_hash.into(),
                size: indexed_data.len() as u64,
                chunks: &indexed_pairs,
            }],
            &small_raw_xorb_builder,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare indexed source plan");
        assert_eq!(indexed_summary.prepared_xorbs, 1);

        let indexed_plan = staging
            .load_file_push_plan(&indexed_file_hash)
            .await
            .expect("load indexed source plan")
            .expect("indexed source plan exists");
        let indexed_xorb_hash = indexed_plan.prepared_xorbs[0].hash.clone();
        assert!(
            !crate::push_plan::file_plan_path(staging.root(), &indexed_file_hash).exists(),
            "indexed source should not have a JSON plan mirror"
        );

        let json_only_data = vec![0x99; 11];
        let json_only_chunk_hash = compute_data_hash(&json_only_data);
        let json_only_source_hash = compute_data_hash(b"json-only-source");
        let json_only_xorb_hash = write_json_only_prepared_plan(
            &staging,
            json_only_source_hash,
            json_only_chunk_hash,
            &json_only_data,
        )
        .await;

        let target_chunks = vec![
            (indexed_chunks[0].0, indexed_data),
            (json_only_chunk_hash, json_only_data),
        ];
        let target_file_hash = stage_file_with_data(&staging, &target_chunks).await;
        let target_pairs: Vec<(MerkleHash, u64)> = target_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let target_size = target_pairs.iter().map(|(_, size)| *size).sum();

        let summary = prepare_file_push_plans(
            &staging,
            &[AddPlanFile {
                file_hash: target_file_hash.into(),
                size: target_size,
                chunks: &target_pairs,
            }],
            &small_raw_xorb_builder,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare target plan");

        assert_eq!(summary.prepared_cache_chunks, 1);
        assert_eq!(summary.prepared_cache_xorbs, 1);
        assert_eq!(summary.prepared_xorbs, 2);

        let target_plan = staging
            .load_file_push_plan(&target_file_hash)
            .await
            .expect("load target plan")
            .expect("target plan exists");
        let planned_hashes = target_plan
            .prepared_xorbs
            .iter()
            .map(|planned| planned.hash.clone())
            .collect::<std::collections::HashSet<_>>();
        assert!(planned_hashes.contains(&indexed_xorb_hash));
        assert!(
            planned_hashes.contains(&json_only_xorb_hash.hex()),
            "the target should build its own deterministic xorb for the uncached chunk"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_remote_multi_file_plan_packs_across_file_boundaries_after_one_lookup() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let first_data = vec![0x33; 10];
        let second_data = vec![0x44; 11];
        let first_chunks = vec![(compute_data_hash(&first_data), first_data)];
        let second_chunks = vec![(compute_data_hash(&second_data), second_data)];
        let first_hash = stage_file_with_data(&staging, &first_chunks).await;
        let second_hash = stage_file_with_data(&staging, &second_chunks).await;
        let first_pairs: Vec<(MerkleHash, u64)> = first_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let second_pairs: Vec<(MerkleHash, u64)> = second_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let lookup = NoExistingLookup::new();

        let summary = prepare_file_push_plans(
            &staging,
            &[
                AddPlanFile {
                    file_hash: first_hash.into(),
                    size: 10,
                    chunks: &first_pairs,
                },
                AddPlanFile {
                    file_hash: second_hash.into(),
                    size: 11,
                    chunks: &second_pairs,
                },
            ],
            &small_raw_xorb_builder,
            Some(&lookup),
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare push plans");

        assert!(summary.remote_lookup);
        assert_eq!(summary.files, 2);
        assert_eq!(summary.prepared_xorbs, 1);
        assert_eq!(lookup.calls.load(Ordering::Relaxed), 1);
        assert_eq!(lookup.chunks_seen.load(Ordering::Relaxed), 2);

        let first_plan = staging
            .load_file_push_plan(&first_hash)
            .await
            .expect("load first plan")
            .expect("first plan exists");
        let second_plan = staging
            .load_file_push_plan(&second_hash)
            .await
            .expect("load second plan")
            .expect("second plan exists");
        assert_eq!(
            first_plan.prepared_xorbs[0].hash,
            second_plan.prepared_xorbs[0].hash
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn partial_remote_multi_file_plan_packs_uncovered_chunks_after_one_lookup() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let first_data = vec![0x55; 10];
        let second_data = vec![0x66; 11];
        let first_chunks = vec![(compute_data_hash(&first_data), first_data)];
        let second_chunks = vec![(compute_data_hash(&second_data), second_data)];
        let first_hash = stage_file_with_data(&staging, &first_chunks).await;
        let second_hash = stage_file_with_data(&staging, &second_chunks).await;
        let first_pairs: Vec<(MerkleHash, u64)> = first_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let second_pairs: Vec<(MerkleHash, u64)> = second_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let lookup = SelectiveExistingLookup::new(first_pairs[0].0);

        let summary = prepare_file_push_plans(
            &staging,
            &[
                AddPlanFile {
                    file_hash: first_hash.into(),
                    size: 10,
                    chunks: &first_pairs,
                },
                AddPlanFile {
                    file_hash: second_hash.into(),
                    size: 11,
                    chunks: &second_pairs,
                },
            ],
            &small_raw_xorb_builder,
            Some(&lookup),
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare push plans");

        assert!(summary.remote_lookup);
        assert_eq!(summary.existing_candidates, 1);
        assert_eq!(summary.prepared_xorbs, 1);
        assert_eq!(lookup.calls.load(Ordering::Relaxed), 1);
        assert_eq!(lookup.chunks_seen.load(Ordering::Relaxed), 2);

        let first_plan = staging
            .load_file_push_plan(&first_hash)
            .await
            .expect("load first plan")
            .expect("first plan exists");
        let second_plan = staging
            .load_file_push_plan(&second_hash)
            .await
            .expect("load second plan")
            .expect("second plan exists");
        assert!(first_plan.existing.is_empty());
        assert!(first_plan.prepared_xorbs.is_empty());
        assert!(second_plan.existing.is_empty());
        assert_eq!(second_plan.prepared_xorbs.len(), 1);
        let first_recipe = staging
            .published_recipe_for_file(&first_hash)
            .expect("load first recipe")
            .expect("first recipe exists");
        assert_eq!(remote_chunks(&staging, &first_recipe).len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn partial_remote_plan_repacks_from_direct_stream_xorb_authority() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("direct.bin");
        let mut data = vec![0u8; 2 * 1024 * 1024];
        let mut state = 0x9E37_79B9_u32;
        for byte in &mut data {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        std::fs::write(&path, &data).expect("write direct fixture");
        let staging = StagingArea::open(tmp.path().join("staging"))
            .await
            .expect("open staging");
        let staged = crate::stream::stage_file_streaming(
            &path,
            tmp.path(),
            &staging,
            crate::stream::StreamStageProgress {
                xorb_builder: Some(crate::stream::StreamStageXorbBuilder::new(
                    1,
                    small_raw_xorb_builder,
                )),
                ..crate::stream::StreamStageProgress::default()
            },
            &CancellationToken::new(),
        )
        .await
        .expect("stage direct xorb authority");
        staging
            .mark_batch_published(&staged.batch_id)
            .expect("publish direct recipe");
        let staged_pairs = recipe_pairs(&staging, &staged.recipe);
        assert!(staged_pairs.len() > 1, "fixture must span chunks");
        let file_hash = MerkleHash::from(staged.file_hash);
        assert!(
            staging
                .chunks_for_file_with_locators(&file_hash)
                .expect("raw chunk rows")
                .is_empty(),
            "direct staging must not retain a raw segment copy"
        );

        let lookup = SelectiveExistingLookup::new(staged_pairs[0].0);
        let summary = prepare_file_push_plans(
            &staging,
            &[AddPlanFile {
                file_hash: staged.file_hash,
                size: staged.size,
                chunks: &staged_pairs,
            }],
            &small_raw_xorb_builder,
            Some(&lookup),
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("repack uncovered chunks from direct authority");

        assert_eq!(summary.existing_candidates, 1);
        assert!(summary.prepared_xorbs > 0);
        let plan = staging
            .load_file_push_plan(&file_hash)
            .await
            .expect("load repacked plan")
            .expect("repacked plan exists");
        assert!(plan.existing.is_empty());
        assert_eq!(remote_chunks(&staging, &staged.recipe).len(), 1);
        assert!(
            plan.prepared_xorbs
                .iter()
                .filter(|xorb| xorb.upload)
                .all(|xorb| {
                    xorb.placements
                        .iter()
                        .all(|placement| placement.chunk_hash != staged_pairs[0].0.hex())
                }),
            "the remotely present chunk must not be repacked"
        );
        assert_eq!(plan.chunk_count, staged.recipe.chunk_count());
        assert_eq!(
            plan.sequence_hash().expect("plan sequence"),
            staged.recipe.sequence_hash()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prepared_cache_multi_file_plan_batches_remote_lookup_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let cached_data = vec![0x77; 10];
        let cached_chunk = (compute_data_hash(&cached_data), cached_data.clone());
        let source_hash = stage_file_with_data(&staging, std::slice::from_ref(&cached_chunk)).await;
        let source_pairs = [(cached_chunk.0, cached_chunk.1.len() as u64)];

        prepare_file_push_plans(
            &staging,
            &[AddPlanFile {
                file_hash: source_hash.into(),
                size: cached_chunk.1.len() as u64,
                chunks: &source_pairs,
            }],
            &small_raw_xorb_builder,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare cached source plan");

        let first_new = vec![0x88; 11];
        let second_new = vec![0x99; 12];
        let first_chunks = vec![
            cached_chunk.clone(),
            (compute_data_hash(&first_new), first_new),
        ];
        let second_chunks = vec![(compute_data_hash(&second_new), second_new)];
        let first_hash = stage_file_with_data(&staging, &first_chunks).await;
        let second_hash = stage_file_with_data(&staging, &second_chunks).await;
        let first_pairs: Vec<(MerkleHash, u64)> = first_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let second_pairs: Vec<(MerkleHash, u64)> = second_chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let lookup = NoExistingLookup::new();

        let summary = prepare_file_push_plans(
            &staging,
            &[
                AddPlanFile {
                    file_hash: first_hash.into(),
                    size: first_pairs.iter().map(|(_, size)| *size).sum(),
                    chunks: &first_pairs,
                },
                AddPlanFile {
                    file_hash: second_hash.into(),
                    size: second_pairs.iter().map(|(_, size)| *size).sum(),
                    chunks: &second_pairs,
                },
            ],
            &small_raw_xorb_builder,
            Some(&lookup),
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("prepare cached multi-file push plans");

        assert!(summary.remote_lookup);
        assert_eq!(summary.files, 2);
        assert_eq!(summary.prepared_cache_chunks, 1);
        assert_eq!(summary.prepared_cache_xorbs, 1);
        assert_eq!(lookup.calls.load(Ordering::Relaxed), 1);
        assert_eq!(lookup.chunks_seen.load(Ordering::Relaxed), 3);
    }
}
