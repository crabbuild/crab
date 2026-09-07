use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::StagingArea;
use crate::error::{Result, StagingError};
#[cfg(test)]
use crate::push_plan::PreparedXorbSource;
use crate::push_plan::{
    ExistingChunkCandidate, FilePushPlan, PlannedExistingChunk, PlannedPlacement, PlannedXorb,
    PreparedXorbCache, PreparedXorbCandidate, materialize_prepared_xorb,
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
        wanted_chunks: &[(MerkleHash, u64)],
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

    let unique_file_chunks = (files.len() > 1).then(|| collect_unique_file_chunks(files));
    let wanted_prepared_chunks = unique_file_chunks.as_deref().unwrap_or(files[0].chunks);
    let mut summary = AddPushPlanSummary {
        remote_lookup: remote_lookup.is_some(),
        ..AddPushPlanSummary::default()
    };
    if let Some(callback) = on_progress.as_deref_mut() {
        callback(&summary);
    }
    let mut prepared_cache = staging.load_prepared_xorb_cache_for_chunks(wanted_prepared_chunks)?;
    if let Some(local_lookup) = local_lookup {
        local_lookup
            .load_candidates(&mut prepared_cache, wanted_prepared_chunks)
            .await?;
    }
    if files.len() > 1 {
        let mut verified_sequences = HashSet::new();
        let unique_file_chunks = unique_file_chunks.as_deref().ok_or_else(|| {
            StagingError::Internal("missing unique multi-file chunk collection".to_owned())
        })?;
        let unique_existing_refs =
            lookup_existing_candidates(unique_file_chunks, remote_lookup).await?;
        let existing_refs =
            index_existing_refs(unique_file_chunks, unique_existing_refs.as_deref())?;
        if prepared_cache.is_empty() {
            return prepare_uncached_file_plans_with_progress(
                staging,
                files,
                &existing_refs,
                build_xorb_builder,
                summary.remote_lookup,
                &mut verified_sequences,
                cancel,
                on_progress,
            )
            .await;
        }
        return prepare_cached_file_plans_with_progress(
            staging,
            files,
            CachedFilePlanContext {
                existing_refs: &existing_refs,
                build_xorb_builder,
                prepared_cache: &mut prepared_cache,
                summary,
                verified_sequences: &mut verified_sequences,
                cancel,
                on_progress,
            },
        )
        .await;
    }
    let mut verified_sequences = HashSet::new();
    let mut ownership_cache = HashMap::new();
    for file in files {
        check_cancelled(cancel)?;
        let file_summary = prepare_one_file_plan(
            staging,
            file,
            build_xorb_builder,
            remote_lookup,
            &mut prepared_cache,
            &mut verified_sequences,
            &mut ownership_cache,
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

struct PreparedFilePlan {
    plan: FilePushPlan,
    recipe: crate::recipe::FileRecipe,
    summary: FilePlanSummary,
}

struct CachedFilePlanContext<'existing, 'builder, 'cache, 'verified, 'cancel, 'progress> {
    existing_refs: &'existing Option<HashMap<MerkleHash, Option<ExistingChunkCandidate>>>,
    build_xorb_builder: &'builder (dyn Fn() -> XorbBuilder + Send + Sync),
    prepared_cache: &'cache mut PreparedXorbCache,
    summary: AddPushPlanSummary,
    verified_sequences: &'verified mut HashSet<(MerkleHash, [u8; 32], u64)>,
    cancel: &'cancel CancellationToken,
    on_progress: Option<&'progress mut (dyn FnMut(&AddPushPlanSummary) + Send)>,
}

async fn prepare_cached_file_plans_with_progress(
    staging: &StagingArea,
    files: &[AddPlanFile<'_>],
    context: CachedFilePlanContext<'_, '_, '_, '_, '_, '_>,
) -> Result<AddPushPlanSummary> {
    let CachedFilePlanContext {
        existing_refs,
        build_xorb_builder,
        prepared_cache,
        mut summary,
        verified_sequences,
        cancel,
        mut on_progress,
    } = context;
    let mut prepared_plans = Vec::with_capacity(files.len());
    let mut ownership_cache = HashMap::new();
    for file in files {
        check_cancelled(cancel)?;
        let file_existing_refs = existing_refs
            .as_ref()
            .map(|existing_refs| {
                file.chunks
                    .iter()
                    .map(|(chunk_hash, _)| {
                        existing_refs.get(chunk_hash).copied().ok_or_else(|| {
                            StagingError::Internal(
                                "existing chunk lookup lost a requested chunk".to_owned(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        let file_existing_refs = file_existing_refs
            .as_deref()
            .map(ExistingRefs::Values)
            .unwrap_or(ExistingRefs::Missing {
                len: file.chunks.len(),
            });
        let prepared = prepare_one_file_plan_with_existing_refs(
            staging,
            file,
            file_existing_refs,
            build_xorb_builder,
            prepared_cache,
            verified_sequences,
            &mut ownership_cache,
            cancel,
        )
        .await?;
        prepared_plans.push(prepared);
    }
    let plan_pairs = prepared_plans
        .iter()
        .map(|prepared| (&prepared.plan, &prepared.recipe))
        .collect::<Vec<_>>();
    staging
        .write_file_push_plans_for_recipes(&plan_pairs)
        .await?;
    for prepared in prepared_plans {
        accumulate_file_summary(&mut summary, prepared.summary);
        if let Some(callback) = on_progress.as_deref_mut() {
            callback(&summary);
        }
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

struct PreparedCandidateChoice<'a> {
    candidate: &'a PreparedXorbCandidate,
    covered_chunks: Vec<MerkleHash>,
}

#[derive(Clone, Copy, Default)]
struct FileChunkState {
    size: u64,
    remote: bool,
    covered: bool,
    read_needed: bool,
}

#[derive(Clone, Copy)]
// Keep all-missing lookups allocation-free when no remote classifier is configured.
enum ExistingRefs<'a> {
    Missing { len: usize },
    Values(&'a [Option<ExistingChunkCandidate>]),
}

impl ExistingRefs<'_> {
    fn len(self) -> usize {
        match self {
            Self::Missing { len } => len,
            Self::Values(values) => values.len(),
        }
    }

    fn at(self, index: usize) -> Option<Option<ExistingChunkCandidate>> {
        match self {
            Self::Missing { len } => (index < len).then_some(None),
            Self::Values(values) => values.get(index).copied(),
        }
    }
}

struct ReadBatchScratch {
    hashes: Vec<MerkleHash>,
    to_pack: Vec<(Chunk, RunId)>,
}

impl ReadBatchScratch {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            hashes: Vec::with_capacity(capacity),
            to_pack: Vec::with_capacity(capacity),
        }
    }
}

struct UncachedFilePlan<'a> {
    file_hash: MerkleHash,
    chunks: &'a [(MerkleHash, u64)],
    uncovered_chunks: HashSet<MerkleHash>,
    plan: FilePushPlan,
}

fn collect_unique_file_chunks(files: &[AddPlanFile<'_>]) -> Vec<(MerkleHash, u64)> {
    let mut wanted = HashSet::new();
    let mut unique = Vec::new();
    for file in files {
        for &(chunk_hash, size) in file.chunks {
            if wanted.insert(chunk_hash) {
                unique.push((chunk_hash, size));
            }
        }
    }
    unique
}

fn index_existing_refs(
    unique_chunks: &[(MerkleHash, u64)],
    unique_refs: Option<&[Option<ExistingChunkCandidate>]>,
) -> Result<Option<HashMap<MerkleHash, Option<ExistingChunkCandidate>>>> {
    let Some(unique_refs) = unique_refs else {
        return Ok(None);
    };
    if unique_chunks.len() != unique_refs.len() {
        return Err(StagingError::Internal(
            "existing chunk lookup returned a different unique chunk count".to_owned(),
        ));
    }
    Ok(Some(
        unique_chunks
            .iter()
            .map(|(chunk_hash, _)| *chunk_hash)
            .zip(unique_refs.iter().copied())
            .collect(),
    ))
}

async fn prepare_uncached_file_plans_with_progress(
    staging: &StagingArea,
    files: &[AddPlanFile<'_>],
    existing_refs: &Option<HashMap<MerkleHash, Option<ExistingChunkCandidate>>>,
    build_xorb_builder: &(dyn Fn() -> XorbBuilder + Send + Sync),
    remote_lookup: bool,
    verified_sequences: &mut HashSet<(MerkleHash, [u8; 32], u64)>,
    cancel: &CancellationToken,
    mut on_progress: Option<&mut (dyn FnMut(&AddPushPlanSummary) + Send)>,
) -> Result<AddPushPlanSummary> {
    let mut file_plans = Vec::with_capacity(files.len());
    for file in files {
        check_cancelled(cancel)?;
        let file_existing_refs = existing_refs
            .as_ref()
            .map(|existing_refs| {
                file.chunks
                    .iter()
                    .map(|(chunk_hash, _)| {
                        existing_refs.get(chunk_hash).copied().ok_or_else(|| {
                            StagingError::Internal(
                                "existing chunk lookup lost a requested chunk".to_owned(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        let file_existing_refs = file_existing_refs
            .as_deref()
            .map(ExistingRefs::Values)
            .unwrap_or(ExistingRefs::Missing {
                len: file.chunks.len(),
            });
        file_plans.push(
            verified_uncached_file_plan(staging, file, file_existing_refs, verified_sequences)
                .await?,
        );
    }

    let mut builder = build_xorb_builder();
    let chunk_owners = build_uncached_chunk_owners(&file_plans);
    let mut queued_chunks = HashSet::new();
    let mut read_batch = Vec::with_capacity(ADD_PLAN_READ_BATCH_CHUNKS);
    let mut read_scratch = ReadBatchScratch::with_capacity(ADD_PLAN_READ_BATCH_CHUNKS);
    for file_idx in 0..file_plans.len() {
        let run_id = RunId(file_idx as u64);
        for &chunk in file_plans[file_idx].chunks {
            if !file_plans[file_idx].uncovered_chunks.contains(&chunk.0) {
                continue;
            }
            if !queued_chunks.insert(chunk.0) {
                continue;
            }
            read_batch.push((chunk, run_id));
            if read_batch.len() >= ADD_PLAN_READ_BATCH_CHUNKS {
                flush_uncached_read_batch(
                    staging,
                    &mut read_batch,
                    &mut read_scratch,
                    &mut builder,
                    cancel,
                )
                .await?;
                write_completed_uncached_xorbs(
                    staging,
                    &mut file_plans,
                    &chunk_owners,
                    &mut builder,
                )
                .await?;
            }
        }
    }
    flush_uncached_read_batch(
        staging,
        &mut read_batch,
        &mut read_scratch,
        &mut builder,
        cancel,
    )
    .await?;
    write_completed_uncached_xorbs(staging, &mut file_plans, &chunk_owners, &mut builder).await?;
    for result in builder.finalize()? {
        record_uncached_prepared_xorb(staging, &mut file_plans, &chunk_owners, result).await?;
    }

    let mut summary = AddPushPlanSummary {
        remote_lookup,
        ..AddPushPlanSummary::default()
    };
    let recipes = file_plans
        .iter()
        .map(|file_plan| {
            crate::recipe::FileRecipe::from_staged_chunks(
                crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
                file_plan.file_hash,
                file_plan.plan.file_size,
                file_plan.chunks,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let plan_pairs = file_plans
        .iter()
        .zip(recipes.iter())
        .map(|(file_plan, recipe)| (&file_plan.plan, recipe))
        .collect::<Vec<_>>();
    staging
        .write_file_push_plans_for_recipes(&plan_pairs)
        .await?;
    let mut summarized_prepared_xorbs = HashSet::new();
    for file_plan in &file_plans {
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
    existing_refs: ExistingRefs<'_>,
    verified_sequences: &mut HashSet<(MerkleHash, [u8; 32], u64)>,
) -> Result<UncachedFilePlan<'a>> {
    if existing_refs.len() != file.chunks.len() {
        return Err(StagingError::Internal(format!(
            "add push-plan remote lookup returned {} candidates for {} requested chunks",
            existing_refs.len(),
            file.chunks.len()
        )));
    }
    let file_hash = MerkleHash::from(file.file_hash);
    let mut plan = FilePushPlan::new_verified_staging(file_hash, file.size, file.chunks);
    let verification_key = (file_hash, plan.sequence_hash()?, file.size);
    if verified_sequences.insert(verification_key) {
        verified_staged_chunks(
            staging,
            file_hash,
            file.size,
            file.chunks,
            plan.sequence_hash()?,
        )
        .await?;
    }
    let mut uncovered_chunks = HashSet::new();
    for (index, (chunk_hash, size)) in file.chunks.iter().enumerate() {
        let existing_ref = existing_refs
            .at(index)
            .ok_or_else(|| StagingError::Internal("missing existing chunk candidate".to_owned()))?;
        if let Some(candidate) = existing_ref
            && u64::from(candidate.xorb_ref.uncompressed_size) == *size
        {
            plan.existing
                .push(PlannedExistingChunk::from_candidate(*chunk_hash, candidate));
            continue;
        }
        uncovered_chunks.insert(*chunk_hash);
    }
    Ok(UncachedFilePlan {
        file_hash,
        chunks: file.chunks,
        uncovered_chunks,
        plan,
    })
}

async fn verified_staged_chunks(
    staging: &StagingArea,
    file_hash: MerkleHash,
    file_size: u64,
    expected_chunks: &[(MerkleHash, u64)],
    expected_sequence_hash: [u8; 32],
) -> Result<()> {
    if let Some(plan) = staging.load_file_push_plan(&file_hash).await?
        && plan.staged_chunk_sequence_verified
        && plan.chunk_count == expected_chunks.len() as u64
        && plan.sequence_hash()? == expected_sequence_hash
        && plan.file_size == file_size
    {
        return Ok(());
    }
    if !staging.file_chunks_match(&file_hash, expected_chunks, file_size)? {
        return Err(StagingError::StagingCorrupt(format!(
            "staged chunk rows for file {} changed while preparing add push plan",
            file_hash.hex()
        )));
    }
    Ok(())
}

async fn flush_uncached_read_batch(
    staging: &StagingArea,
    read_batch: &mut Vec<((MerkleHash, u64), RunId)>,
    scratch: &mut ReadBatchScratch,
    builder: &mut XorbBuilder,
    cancel: &CancellationToken,
) -> Result<()> {
    if read_batch.is_empty() {
        return Ok(());
    }
    check_cancelled(cancel)?;
    scratch.hashes.clear();
    scratch
        .hashes
        .extend(read_batch.iter().map(|((hash, _), _)| *hash));
    let payloads = staging.get_chunks_batch(&scratch.hashes).await?;
    scratch.to_pack.clear();
    for ((expected, run_id), (actual_hash, data)) in read_batch.iter().zip(payloads) {
        scratch.to_pack.push((
            Chunk {
                hash: actual_hash,
                data,
            },
            *run_id,
        ));
        debug_assert_eq!(actual_hash, expected.0);
    }
    builder.push_batch(&scratch.to_pack)?;
    scratch.to_pack.clear();
    read_batch.clear();
    Ok(())
}

async fn write_completed_uncached_xorbs(
    staging: &StagingArea,
    file_plans: &mut [UncachedFilePlan<'_>],
    chunk_owners: &HashMap<MerkleHash, Vec<usize>>,
    builder: &mut XorbBuilder,
) -> Result<()> {
    while let Some(result) = builder.take_completed() {
        record_uncached_prepared_xorb(staging, file_plans, chunk_owners, result).await?;
    }
    Ok(())
}

fn build_uncached_chunk_owners(
    file_plans: &[UncachedFilePlan<'_>],
) -> HashMap<MerkleHash, Vec<usize>> {
    let mut owners = HashMap::new();
    for (file_idx, file_plan) in file_plans.iter().enumerate() {
        for chunk_hash in &file_plan.uncovered_chunks {
            owners
                .entry(*chunk_hash)
                .or_insert_with(Vec::new)
                .push(file_idx);
        }
    }
    owners
}

async fn record_uncached_prepared_xorb(
    staging: &StagingArea,
    file_plans: &mut [UncachedFilePlan<'_>],
    chunk_owners: &HashMap<MerkleHash, Vec<usize>>,
    result: crab_xet::xorb::builder::XorbResult,
) -> Result<()> {
    let bytes = result.bytes.len() as u64;
    let payload_hash_bytes = *blake3::hash(&result.bytes).as_bytes();
    let payload_hash = blake3::Hash::from(payload_hash_bytes).to_hex().to_string();
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

    let recipients = recipient_indices(file_plans.len(), &result.placements, chunk_owners);
    let Some((&owner_idx, linked_idxs)) = recipients.split_first() else {
        return Err(StagingError::Internal(
            "prepared xorb has no owning add file".to_owned(),
        ));
    };
    crate::push_plan::write_prepared_xorb_with_payload_hash(
        staging.root(),
        &result.hash,
        result.bytes,
        payload_hash_bytes,
    )
    .await?;
    file_plans[owner_idx].plan.prepared_xorbs.push(planned);

    if !linked_idxs.is_empty() {
        let linked_plan = file_plans[owner_idx]
            .plan
            .prepared_xorbs
            .last()
            .cloned()
            .ok_or_else(|| {
                StagingError::Internal("prepared xorb owner record disappeared".to_owned())
            })?;
        for idx in linked_idxs {
            file_plans[*idx]
                .plan
                .prepared_xorbs
                .push(linked_plan.clone());
        }
    }
    Ok(())
}

fn recipient_indices(
    file_count: usize,
    placements: &[crab_xet::xorb::format::ChunkPlacement],
    chunk_owners: &HashMap<MerkleHash, Vec<usize>>,
) -> Vec<usize> {
    let mut recipient_flags = vec![false; file_count];
    for placement in placements {
        if let Some(owners) = chunk_owners.get(&placement.chunk_hash) {
            for &owner in owners {
                recipient_flags[owner] = true;
            }
        }
    }
    recipient_flags
        .into_iter()
        .enumerate()
        .filter_map(|(idx, is_recipient)| is_recipient.then_some(idx))
        .collect()
}

async fn prepare_one_file_plan(
    staging: &StagingArea,
    file: &AddPlanFile<'_>,
    build_xorb_builder: &(dyn Fn() -> XorbBuilder + Send + Sync),
    remote_lookup: Option<&dyn ExistingChunkLookup>,
    prepared_cache: &mut PreparedXorbCache,
    verified_sequences: &mut HashSet<(MerkleHash, [u8; 32], u64)>,
    ownership_cache: &mut HashMap<(MerkleHash, [u8; 32]), bool>,
    cancel: &CancellationToken,
) -> Result<FilePlanSummary> {
    let existing_refs = lookup_existing_candidates(file.chunks, remote_lookup).await?;
    let existing_refs = existing_refs
        .as_deref()
        .map(ExistingRefs::Values)
        .unwrap_or(ExistingRefs::Missing {
            len: file.chunks.len(),
        });
    let prepared = prepare_one_file_plan_with_existing_refs(
        staging,
        file,
        existing_refs,
        build_xorb_builder,
        prepared_cache,
        verified_sequences,
        ownership_cache,
        cancel,
    )
    .await?;
    staging
        .write_file_push_plan_for_recipe(&prepared.plan, &prepared.recipe)
        .await?;
    Ok(prepared.summary)
}

async fn prepare_one_file_plan_with_existing_refs(
    staging: &StagingArea,
    file: &AddPlanFile<'_>,
    existing_refs: ExistingRefs<'_>,
    build_xorb_builder: &(dyn Fn() -> XorbBuilder + Send + Sync),
    prepared_cache: &mut PreparedXorbCache,
    verified_sequences: &mut HashSet<(MerkleHash, [u8; 32], u64)>,
    ownership_cache: &mut HashMap<(MerkleHash, [u8; 32]), bool>,
    cancel: &CancellationToken,
) -> Result<PreparedFilePlan> {
    if existing_refs.len() != file.chunks.len() {
        return Err(StagingError::Internal(format!(
            "add push-plan remote lookup returned {} candidates for {} requested chunks",
            existing_refs.len(),
            file.chunks.len()
        )));
    }
    let file_hash = MerkleHash::from(file.file_hash);
    let chunks = file.chunks;
    let recipe = crate::recipe::FileRecipe::from_staged_chunks(
        crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
        file_hash,
        file.size,
        chunks,
    )?;
    let verification_key = (file_hash, recipe.sequence_hash(), file.size);
    if verified_sequences.insert(verification_key) {
        verified_staged_chunks(
            staging,
            file_hash,
            file.size,
            chunks,
            recipe.sequence_hash(),
        )
        .await?;
    }
    let mut plan = FilePushPlan::new_verified_recipe(&recipe);
    let recipe_hash = recipe.hash();

    let mut chunk_states = HashMap::<MerkleHash, FileChunkState>::new();
    let mut planned_prepared_xorbs = HashSet::new();
    let mut cache_chunks = 0u64;
    let mut cache_xorbs = 0u64;
    let mut cache_link_misses = 0u64;
    let mut unusable_cached_candidates = HashSet::new();
    let mut matching_scratch = HashSet::new();
    for (index, (chunk_hash, size)) in chunks.iter().enumerate() {
        let existing_ref = existing_refs
            .at(index)
            .ok_or_else(|| StagingError::Internal("missing existing chunk candidate".to_owned()))?;
        let state = chunk_states.entry(*chunk_hash).or_insert(FileChunkState {
            size: *size,
            ..FileChunkState::default()
        });
        if existing_ref
            .as_ref()
            .is_some_and(|candidate| u64::from(candidate.xorb_ref.uncompressed_size) == *size)
        {
            state.remote = true;
        }
    }
    for (index, (chunk_hash, size)) in chunks.iter().enumerate() {
        let existing_ref = existing_refs
            .at(index)
            .ok_or_else(|| StagingError::Internal("missing existing chunk candidate".to_owned()))?;
        let state = chunk_states.entry(*chunk_hash).or_insert(FileChunkState {
            size: *size,
            ..FileChunkState::default()
        });
        if let Some(candidate) = existing_ref
            && u64::from(candidate.xorb_ref.uncompressed_size) == *size
        {
            plan.existing
                .push(PlannedExistingChunk::from_candidate(*chunk_hash, candidate));
            continue;
        }
        if state.remote || state.covered || state.read_needed {
            continue;
        }
        state.read_needed = true;

        let choices = ranked_prepared_candidates_with_scratch(
            prepared_cache,
            chunk_hash,
            *size,
            &chunk_states,
            &unusable_cached_candidates,
            &mut matching_scratch,
        );
        let mut used_cached_candidate = false;
        for choice in choices {
            let candidate = choice.candidate;
            let contains_remote = candidate.placements.iter().any(|placement| {
                chunk_states
                    .get(&placement.chunk_hash)
                    .is_some_and(|state| state.remote)
            });
            let exclusive = if contains_remote {
                let key = (candidate.xorb_hash, recipe_hash);
                if let Some(exclusive) = ownership_cache.get(&key) {
                    *exclusive
                } else {
                    let exclusive = staging
                        .prepared_payload_exclusive_to_recipe(&candidate.xorb_hash, &recipe_hash)?;
                    ownership_cache.insert(key, exclusive);
                    exclusive
                }
            } else {
                false
            };
            if exclusive {
                continue;
            }
            if planned_prepared_xorbs.contains(&candidate.xorb_hash) {
                cache_chunks +=
                    mark_prepared_cache_coverage(&choice.covered_chunks, &mut chunk_states);
                used_cached_candidate = true;
                break;
            }

            if materialize_prepared_xorb(staging.root(), candidate).await? {
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
                cache_chunks +=
                    mark_prepared_cache_coverage(&choice.covered_chunks, &mut chunk_states);
                cache_xorbs += 1;
                used_cached_candidate = true;
                break;
            }

            unusable_cached_candidates.insert(prepared_candidate_id(candidate));
            cache_link_misses += 1;
        }
        if used_cached_candidate {
            continue;
        }
    }

    let mut builder = build_xorb_builder();
    let mut read_batch = Vec::with_capacity(ADD_PLAN_READ_BATCH_CHUNKS);
    let mut read_scratch = ReadBatchScratch::with_capacity(ADD_PLAN_READ_BATCH_CHUNKS);
    for &chunk in chunks {
        let Some(state) = chunk_states.get_mut(&chunk.0) else {
            return Err(StagingError::Internal(
                "add push-plan chunk state disappeared".to_owned(),
            ));
        };
        if !state.read_needed {
            continue;
        }
        state.read_needed = false;
        read_batch.push((chunk, RunId(0)));
        if read_batch.len() == ADD_PLAN_READ_BATCH_CHUNKS {
            flush_uncached_read_batch(
                staging,
                &mut read_batch,
                &mut read_scratch,
                &mut builder,
                cancel,
            )
            .await?;
            write_completed_xorbs(staging, &mut builder, &mut plan, prepared_cache).await?;
        }
    }

    if !read_batch.is_empty() {
        flush_uncached_read_batch(
            staging,
            &mut read_batch,
            &mut read_scratch,
            &mut builder,
            cancel,
        )
        .await?;
        write_completed_xorbs(staging, &mut builder, &mut plan, prepared_cache).await?;
    }

    for result in builder.finalize()? {
        record_prepared_xorb(staging, &mut plan, prepared_cache, result).await?;
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
    Ok(PreparedFilePlan {
        plan,
        recipe,
        summary: file_summary,
    })
}

fn ranked_prepared_candidates_with_scratch<'a>(
    prepared_cache: &'a PreparedXorbCache,
    chunk_hash: &MerkleHash,
    expected_size: u64,
    chunk_states: &HashMap<MerkleHash, FileChunkState>,
    unusable_cached_candidates: &HashSet<*const PreparedXorbCandidate>,
    matching_scratch: &mut HashSet<MerkleHash>,
) -> Vec<PreparedCandidateChoice<'a>> {
    let mut choices = Vec::new();
    for (candidate, placement) in prepared_cache.candidate_placements_for_chunk(chunk_hash) {
        if unusable_cached_candidates.contains(&prepared_candidate_id(candidate)) {
            continue;
        }
        if u64::from(placement.uncompressed_size) != expected_size {
            continue;
        }
        let covered_chunks = matching_file_chunks(candidate, chunk_states, matching_scratch);
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
                    .as_bytes()
                    .cmp(right.candidate.xorb_hash.as_bytes())
            })
    });
    choices
}

fn prepared_candidate_id(candidate: &PreparedXorbCandidate) -> *const PreparedXorbCandidate {
    // Candidates live in Arc allocations owned by the cache for this plan;
    // identity avoids cloning LocalCache paths on every chunk lookup.
    std::ptr::from_ref(candidate)
}

fn matching_file_chunks(
    candidate: &PreparedXorbCandidate,
    chunk_states: &HashMap<MerkleHash, FileChunkState>,
    seen: &mut HashSet<MerkleHash>,
) -> Vec<MerkleHash> {
    seen.clear();
    let mut covered = Vec::new();
    for placement in &candidate.placements {
        if !seen.insert(placement.chunk_hash) {
            continue;
        }
        let Some(state) = chunk_states.get(&placement.chunk_hash) else {
            continue;
        };
        if state.remote || state.covered {
            continue;
        }
        if u64::from(placement.uncompressed_size) == state.size {
            covered.push(placement.chunk_hash);
        }
    }
    covered
}

fn mark_prepared_cache_coverage(
    covered_chunks: &[MerkleHash],
    chunk_states: &mut HashMap<MerkleHash, FileChunkState>,
) -> u64 {
    let mut newly_covered = 0;
    for chunk_hash in covered_chunks {
        let Some(state) = chunk_states.get_mut(chunk_hash) else {
            continue;
        };
        if !state.covered {
            state.covered = true;
            state.read_needed = false;
            newly_covered += 1;
        }
    }
    newly_covered
}

async fn write_completed_xorbs(
    staging: &StagingArea,
    builder: &mut XorbBuilder,
    plan: &mut FilePushPlan,
    prepared_cache: &mut PreparedXorbCache,
) -> Result<()> {
    while let Some(result) = builder.take_completed() {
        record_prepared_xorb(staging, plan, prepared_cache, result).await?;
    }
    Ok(())
}

async fn record_prepared_xorb(
    staging: &StagingArea,
    plan: &mut FilePushPlan,
    prepared_cache: &mut PreparedXorbCache,
    result: crab_xet::xorb::builder::XorbResult,
) -> Result<()> {
    let bytes = result.bytes.len() as u64;
    let payload_hash_bytes = *blake3::hash(&result.bytes).as_bytes();
    let payload_hash = blake3::Hash::from(payload_hash_bytes).to_hex().to_string();
    let placements: Vec<PlannedPlacement> = result
        .placements
        .iter()
        .map(PlannedPlacement::from_placement)
        .collect();
    crate::push_plan::write_prepared_xorb_with_payload_hash(
        staging.root(),
        &result.hash,
        result.bytes,
        payload_hash_bytes,
    )
    .await?;
    let planned = PlannedXorb {
        hash: result.hash.hex(),
        payload_hash,
        bytes,
        upload: true,
        placements,
    };
    prepared_cache.insert_prepared_xorb(&planned)?;
    plan.prepared_xorbs.push(planned);
    Ok(())
}

async fn lookup_existing_candidates(
    chunks: &[(MerkleHash, u64)],
    remote_lookup: Option<&dyn ExistingChunkLookup>,
) -> Result<Option<Vec<Option<ExistingChunkCandidate>>>> {
    let Some(remote_lookup) = remote_lookup else {
        return Ok(None);
    };
    let refs = remote_lookup.lookup_existing_candidates(chunks).await?;
    if refs.len() != chunks.len() {
        return Err(StagingError::Internal(format!(
            "existing chunk lookup returned {} candidates for {} requested chunks",
            refs.len(),
            chunks.len()
        )));
    }
    Ok(Some(refs))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
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

    #[test]
    fn uncached_xorb_recipients_follow_chunk_ownership() {
        let shared = numbered_hash(1);
        let first_only = numbered_hash(2);
        let mut first_chunks = HashSet::new();
        first_chunks.insert(shared);
        first_chunks.insert(first_only);
        let mut second_chunks = HashSet::new();
        second_chunks.insert(shared);
        let file_plans = vec![
            UncachedFilePlan {
                file_hash: numbered_hash(10),
                chunks: &[],
                uncovered_chunks: first_chunks,
                plan: FilePushPlan::new_verified_staging(numbered_hash(10), 0, &[]),
            },
            UncachedFilePlan {
                file_hash: numbered_hash(11),
                chunks: &[],
                uncovered_chunks: second_chunks,
                plan: FilePushPlan::new_verified_staging(numbered_hash(11), 0, &[]),
            },
        ];
        let owners = build_uncached_chunk_owners(&file_plans);
        let placements = vec![
            crab_xet::xorb::format::ChunkPlacement {
                chunk_hash: shared,
                xorb_hash: numbered_hash(20),
                chunk_index: 0,
                uncompressed_size: 1,
            },
            crab_xet::xorb::format::ChunkPlacement {
                chunk_hash: first_only,
                xorb_hash: numbered_hash(20),
                chunk_index: 1,
                uncompressed_size: 1,
            },
        ];

        assert_eq!(owners.get(&shared), Some(&vec![0, 1]));
        assert_eq!(
            recipient_indices(file_plans.len(), &placements, &owners),
            vec![0, 1]
        );
    }

    #[test]
    fn multi_file_remote_lookup_deduplicates_shared_chunks() {
        let shared = numbered_hash(3);
        let first_only = numbered_hash(4);
        let second_only = numbered_hash(5);
        let first_chunks = [(shared, 1), (first_only, 1)];
        let second_chunks = [(shared, 1), (second_only, 1)];
        let files = [
            AddPlanFile {
                file_hash: numbered_hash(6).into(),
                size: 2,
                chunks: &first_chunks,
            },
            AddPlanFile {
                file_hash: numbered_hash(7).into(),
                size: 2,
                chunks: &second_chunks,
            },
        ];
        let unique = collect_unique_file_chunks(&files);
        assert_eq!(unique.len(), 3);
        assert_eq!(
            unique
                .iter()
                .map(|(chunk_hash, _)| *chunk_hash)
                .collect::<HashSet<_>>()
                .len(),
            unique.len()
        );
        let shared_candidate = Some(existing_candidate(
            XorbRef {
                xorb_hash: numbered_hash(8),
                chunk_index: 0,
                uncompressed_size: 1,
            },
            8,
        ));
        let unique_refs = unique
            .iter()
            .map(|(chunk_hash, _)| {
                (*chunk_hash == shared)
                    .then_some(shared_candidate)
                    .flatten()
            })
            .collect::<Vec<_>>();
        let indexed = index_existing_refs(&unique, Some(&unique_refs))
            .expect("index refs")
            .expect("indexed refs");
        assert_eq!(indexed.len(), 3);
        assert_eq!(indexed.get(&shared), Some(&shared_candidate));
        assert_eq!(indexed.get(&first_only), Some(&None));
        assert_eq!(indexed.get(&second_only), Some(&None));
    }

    #[test]
    fn absent_remote_lookup_keeps_candidate_index_unallocated() {
        let unique = vec![(numbered_hash(9), 1)];

        let indexed = index_existing_refs(&unique, None).expect("index refs");

        assert!(indexed.is_none());
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
    async fn repeated_add_reuses_global_prepared_xorb() {
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
        let first_xorb_hash = first_plan.prepared_xorbs[0]
            .hash()
            .expect("first xorb hash");
        assert!(crate::push_plan::prepared_xorb_path(staging.root(), &first_xorb_hash).is_file());
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

        let chunk_states = HashMap::from([(
            chunk_hash,
            FileChunkState {
                size: 10,
                ..FileChunkState::default()
            },
        )]);
        let mut matching_scratch = HashSet::new();
        let choices = ranked_prepared_candidates_with_scratch(
            &cache,
            &chunk_hash,
            10,
            &chunk_states,
            &HashSet::new(),
            &mut matching_scratch,
        );
        assert_eq!(choices.len(), 2);
        let first_candidate = choices
            .iter()
            .find(|choice| choice.candidate.source == first_source)
            .expect("first source candidate")
            .candidate;

        let filtered = ranked_prepared_candidates_with_scratch(
            &cache,
            &chunk_hash,
            10,
            &chunk_states,
            &HashSet::from([prepared_candidate_id(first_candidate)]),
            &mut matching_scratch,
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].candidate.source, second_source);
        assert_ne!(filtered[0].candidate.source, first_source);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retiring_segment_rows_preserves_recipe_prepared_authority() {
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
            crate::push_plan::prepared_xorb_path(staging.root(), &prepared_xorb_hash);
        assert!(prepared_xorb_path.exists());

        let retired = staging.retire_file(&file_hash).expect("retire file");
        assert_eq!(retired.rows_deleted, 1);

        assert!(
            !staging
                .load_prepared_xorb_cache_for_chunks(&chunk_pairs)
                .expect("load prepared cache after retire")
                .is_empty()
        );
        assert!(
            staging
                .load_file_push_plan(&file_hash)
                .await
                .expect("load retired push plan")
                .is_some()
        );
        assert!(prepared_xorb_path.exists());
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
                .segment_payloads_exist(&staged_pairs)
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
