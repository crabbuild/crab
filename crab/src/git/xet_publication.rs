//! Protocol-v2 publication of external Xet xorbs and shards.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use crab_staging::StagingAreaReadOnly;
use crab_xet::hash::MerkleHash;
use crab_xet::reconstruction::ChunkPlacementMap;
use crab_xet::shard::{PushShardSession, file_info_from_placements, xorb_info_from_placements};
use crab_xet::xorb::builder::{RunId, XorbBuilder, XorbResult};
use crab_xet::xorb::format::{Chunk, ChunkPlacement, MAX_XORB_SIZE, XorbRef};
use crab_xet::xorb::parser::XorbParser;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};

const PREPARED_XORB_UPLOAD_CONCURRENCY: usize = 4;

pub(crate) async fn prepare_delta(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    base: &crab_metadata::capsule_protocol::RootSnapshot,
    pointers: &[crab_types::pointer::Pointer],
    staging: Option<&Arc<StagingAreaReadOnly>>,
    cancel: &CancellationToken,
) -> Result<crab_metadata::capsule_protocol::PointerCatalog> {
    use crab_metadata::capsule_protocol::{
        FileCatalogEntry, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry,
    };

    if pointers.is_empty() {
        return Ok(PointerCatalog::new());
    }
    let view = crab_read::capsule_protocol::open_view_from_root(
        layout,
        base.clone(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 8 * 1024 * 1024 * 1024,
        },
    )
    .await?;
    let base_catalog = view.pointer_catalog()?;
    let mut xorb_entries = HashMap::<MerkleHash, XorbCatalogEntry>::new();
    let mut placements = ChunkPlacementMap::new();
    for (hash, entry) in base_catalog.xorbs() {
        let hash = parse_merkle_hash(hash, "base xorb")?;
        let xorb_placements = placements_for_catalog_xorb(hash, entry)?;
        for placement in &xorb_placements {
            placements
                .entry(placement.chunk_hash)
                .or_insert_with(|| placement.clone());
        }
        xorb_entries.insert(hash, entry.clone());
    }

    let mut pointer_sizes = BTreeMap::<MerkleHash, u64>::new();
    for pointer in pointers {
        let file_hash = MerkleHash::from(pointer.file_hash);
        match pointer_sizes.insert(file_hash, pointer.size) {
            Some(previous) if previous != pointer.size => {
                return Err(CrabError::StagingCorrupt(format!(
                    "pointer file {} declares both {previous} and {} bytes",
                    file_hash.hex(),
                    pointer.size
                )));
            }
            _ => {}
        }
    }

    let unresolved = pointer_sizes
        .iter()
        .filter_map(|(hash, size)| match base_catalog.files().get(&hash.hex()) {
            Some(entry) if entry.size() == *size => None,
            Some(entry) => Some(Err(CrabError::CorruptObject {
                path: "capsule-protocol pointer catalog".to_owned(),
                reason: format!(
                    "file {} has catalog size {}, pointer declares {size}",
                    hash.hex(),
                    entry.size()
                ),
            })),
            None => Some(Ok((*hash, *size))),
        })
        .collect::<Result<Vec<_>>>()?;
    if unresolved.is_empty() {
        return Ok(PointerCatalog::new());
    }
    let staging = staging.ok_or_else(|| {
        let (file_hash, size) = unresolved[0];
        CrabError::PointerMissingStaging {
            total: unresolved.len(),
            missing: unresolved.len(),
            example_file_hash: file_hash.hex(),
            example_size: size,
        }
    })?;

    let mut delta = PointerCatalog::new();
    let mut shard_session = PushShardSession::new();
    let mut pending_files = Vec::with_capacity(unresolved.len());
    let mut uploaded_xorbs = HashSet::new();
    let mut prepared_files = Vec::with_capacity(unresolved.len());
    let mut prepared_candidates = Vec::<crab_staging::push_plan::PlannedXorb>::new();
    let mut candidate_indices = HashMap::<MerkleHash, usize>::new();
    let mut external_candidates = HashMap::<MerkleHash, HashMap<MerkleHash, XorbRef>>::new();

    for (file_hash, file_size) in unresolved {
        check_cancelled(cancel)?;
        let chunks = staging.chunks_for_file_with_sizes(&file_hash)?;
        if chunks.is_empty() {
            return Err(CrabError::StagingCorrupt(format!(
                "pointer file {} has no staged chunk recipe",
                file_hash.hex()
            )));
        }
        let plan = staging.load_file_push_plan(&file_hash).await?;
        if plan.is_none() {
            staging
                .verify_file_reconstruction_from_chunks(&file_hash, file_size, &chunks)
                .await?;
        }

        // An indexed plan is rebuilt from the caller-verified canonical recipe.
        // Reconstructing the whole file here would duplicate add's stable-stream
        // proof; payloads newly consumed below still receive hash verification.
        if plan.is_some()
            && let Some(recipe) = staging.published_recipe_for_file(&file_hash)?
        {
            let expected_sizes = chunks.iter().copied().collect::<HashMap<_, _>>();
            let mut next_occurrence = 0_u64;
            while next_occurrence < recipe.chunk_count() {
                let page_end = next_occurrence
                    .checked_add(crab_staging::recipe::RECIPE_PAGE_ENTRIES as u64)
                    .ok_or_else(|| {
                        CrabError::StagingCorrupt(
                            "remote xorb authority page range overflowed".to_owned(),
                        )
                    })?
                    .min(recipe.chunk_count());
                for (chunk_hash, candidate) in
                    staging.recipe_remote_chunk_range(&recipe, next_occurrence, page_end)?
                {
                    let expected_size = expected_sizes.get(&chunk_hash).ok_or_else(|| {
                        CrabError::StagingCorrupt(format!(
                            "remote xorb authority for chunk {} escaped file {}",
                            chunk_hash.hex(),
                            file_hash.hex()
                        ))
                    })?;
                    if u64::from(candidate.xorb_ref.uncompressed_size) != *expected_size {
                        return Err(CrabError::StagingCorrupt(format!(
                            "remote xorb authority for chunk {} has size {}, expected {}",
                            chunk_hash.hex(),
                            candidate.xorb_ref.uncompressed_size,
                            expected_size
                        )));
                    }
                    let refs = external_candidates
                        .entry(candidate.xorb_ref.xorb_hash)
                        .or_default();
                    match refs.insert(chunk_hash, candidate.xorb_ref) {
                        Some(existing) if existing != candidate.xorb_ref => {
                            return Err(CrabError::StagingCorrupt(format!(
                                "remote xorb authority disagrees for chunk {}",
                                chunk_hash.hex()
                            )));
                        }
                        _ => {}
                    }
                }
                next_occurrence = page_end;
            }
        }
        prepared_files.push((file_hash, file_size, chunks, plan));
    }

    let mut external_candidates = external_candidates
        .into_iter()
        .filter(|(hash, refs)| {
            !xorb_entries.contains_key(hash)
                && refs
                    .keys()
                    .any(|chunk_hash| !placements.contains_key(chunk_hash))
        })
        .collect::<Vec<_>>();
    external_candidates.sort_by_key(|(hash, _)| hash.hex());
    let external_reads =
        futures_util::stream::iter(external_candidates.into_iter().enumerate().map(
            |(ordinal, (expected_hash, expected_refs))| async move {
                check_cancelled(cancel)?;
                let path = layout.xorb_path(&expected_hash);
                let bytes = match layout
                    .store()
                    .get_with_etag_bounded(&path, MAX_XORB_SIZE as u64)
                    .await
                {
                    Ok((bytes, _)) => bytes,
                    Err(crab_storage::StorageError::NotFound { .. }) => {
                        return Ok::<_, CrabError>((ordinal, None));
                    }
                    Err(error) => return Err(error.into()),
                };
                let verified = verify_external_xorb(&path, expected_hash, &expected_refs, bytes)?;
                Ok((ordinal, verified))
            },
        ))
        .buffer_unordered(PREPARED_XORB_UPLOAD_CONCURRENCY);
    tokio::pin!(external_reads);
    let mut verified_external = Vec::new();
    while let Some(result) = external_reads.next().await {
        verified_external.push(result?);
    }
    verified_external.sort_by_key(|(ordinal, _)| *ordinal);
    let mut reused_xorbs = 0_usize;
    for (_, verified) in verified_external {
        let Some((xorb_hash, entry, xorb_placements)) = verified else {
            continue;
        };
        reused_xorbs += 1;
        for placement in xorb_placements {
            placements.entry(placement.chunk_hash).or_insert(placement);
        }
        xorb_entries.insert(xorb_hash, entry);
    }

    let mut candidate_chunks = placements.keys().copied().collect::<HashSet<_>>();
    for (_, _, _, plan) in &prepared_files {
        if let Some(plan) = plan {
            for planned in &plan.prepared_xorbs {
                let planned_hash = planned.hash()?;
                if let Some(index) = candidate_indices.get(&planned_hash).copied() {
                    let existing = prepared_candidates.get(index).ok_or_else(|| {
                        CrabError::Internal("prepared xorb candidate index is invalid".to_owned())
                    })?;
                    if !planned_xorbs_match(existing, planned) {
                        return Err(CrabError::StagingCorrupt(format!(
                            "prepared xorb {} has conflicting plans",
                            planned_hash.hex()
                        )));
                    }
                    continue;
                }
                if xorb_entries.contains_key(&planned_hash) {
                    continue;
                }
                let planned_placements = planned
                    .placements
                    .iter()
                    .map(crab_staging::push_plan::PlannedPlacement::to_placement)
                    .collect::<crab_staging::Result<Vec<_>>>()?;
                if planned_placements
                    .iter()
                    .all(|placement| candidate_chunks.contains(&placement.chunk_hash))
                {
                    continue;
                }
                candidate_chunks.extend(
                    planned_placements
                        .iter()
                        .map(|placement| placement.chunk_hash),
                );
                candidate_indices.insert(planned_hash, prepared_candidates.len());
                prepared_candidates.push(planned.clone());
            }
        }
    }

    let uploads = futures_util::stream::iter(prepared_candidates.into_iter().enumerate().map(
        |(ordinal, planned)| async move {
            check_cancelled(cancel)?;
            let planned_hash = planned.hash()?;
            let path = crab_staging::push_plan::prepared_xorb_path(staging.root(), &planned_hash);
            let bytes = Bytes::from(tokio::fs::read(&path).await?);
            let (entry, xorb_placements) =
                verify_prepared_xorb(&path, planned_hash, &planned, bytes.clone())?;
            let (entry, xorb_placements, created) =
                publish_xorb_candidate(layout, planned_hash, bytes, entry, xorb_placements).await?;
            Ok::<_, CrabError>((ordinal, planned_hash, entry, xorb_placements, created))
        },
    ))
    .buffer_unordered(PREPARED_XORB_UPLOAD_CONCURRENCY);
    tokio::pin!(uploads);
    let mut verified_candidates = Vec::new();
    while let Some(result) = uploads.next().await {
        verified_candidates.push(result?);
    }
    verified_candidates.sort_by_key(|(ordinal, _, _, _, _)| *ordinal);
    for (_, planned_hash, entry, xorb_placements, created) in verified_candidates {
        if created {
            uploaded_xorbs.insert(planned_hash);
        }
        for placement in xorb_placements {
            placements.entry(placement.chunk_hash).or_insert(placement);
        }
        xorb_entries.insert(planned_hash, entry);
    }

    for (file_ordinal, (file_hash, file_size, chunks, _)) in prepared_files.into_iter().enumerate()
    {
        check_cancelled(cancel)?;
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for (hash, _) in &chunks {
            if !placements.contains_key(hash) && seen.insert(*hash) {
                missing.push(*hash);
            }
        }
        if !missing.is_empty() {
            let mut builder = XorbBuilder::new();
            for batch in missing.chunks(256) {
                let loaded = staging.get_chunks_batch(batch).await?;
                let by_hash = loaded.into_iter().collect::<HashMap<_, _>>();
                for hash in batch {
                    let data = by_hash
                        .get(hash)
                        .cloned()
                        .ok_or_else(|| CrabError::ChunkNotFound { hash: hash.hex() })?;
                    builder.push(
                        &Chunk { hash: *hash, data },
                        RunId(u64::try_from(file_ordinal).map_err(|_| {
                            CrabError::Internal("pointer file ordinal overflowed".to_owned())
                        })?),
                    )?;
                    while let Some(result) = builder.take_completed() {
                        publish_built_xorb(
                            layout,
                            result,
                            &mut placements,
                            &mut xorb_entries,
                            &mut uploaded_xorbs,
                        )
                        .await?;
                    }
                }
            }
            for result in builder.finalize()? {
                publish_built_xorb(
                    layout,
                    result,
                    &mut placements,
                    &mut xorb_entries,
                    &mut uploaded_xorbs,
                )
                .await?;
            }
        }

        let ordered_hashes = chunks.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        let file_info = file_info_from_placements(file_hash, &ordered_hashes, &placements)?;
        let dependency_hashes = file_info
            .segments
            .iter()
            .map(|segment| segment.xorb_hash)
            .collect::<HashSet<_>>();
        let mut dependencies = dependency_hashes
            .iter()
            .map(|hash| {
                let entry = xorb_entries.get(hash).ok_or_else(|| {
                    CrabError::Internal(format!(
                        "file {} resolved absent xorb descriptor {}",
                        file_hash.hex(),
                        hash.hex()
                    ))
                })?;
                let xorb_placements = placements_for_catalog_xorb(*hash, entry)?;
                Ok(Arc::new(xorb_info_from_placements(
                    *hash,
                    &xorb_placements,
                )?))
            })
            .collect::<Result<Vec<_>>>()?;
        dependencies.sort_by_key(|dependency| dependency.metadata.xorb_hash);
        let shard_index = shard_session.add_file_bundle(file_info, &dependencies)?;
        pending_files.push((file_hash, file_size, shard_index, dependency_hashes));
    }

    let shards = shard_session.finalize()?;
    let mut shard_hashes = Vec::with_capacity(shards.len());
    for (bytes, hash) in &shards {
        layout
            .store()
            .put_if_absent_verified(&layout.shard_path(hash), Bytes::from(bytes.clone()))
            .await?;
        shard_hashes.push(hash.hex());
    }
    // Registry partitions are monotonic. Re-registering the pinned catalog
    // would rewrite every historical partition on each push without adding
    // protection; only this transaction's candidate roots need unioning.
    crab_metadata::ref_registry::union_register_repo_shards(
        layout.store(),
        layout,
        shard_hashes.iter().cloned().collect(),
    )
    .await?;

    let mut shard_closures = vec![HashSet::new(); shards.len()];
    for (file_hash, _, shard_index, dependency_hashes) in &pending_files {
        let closure = shard_closures.get_mut(*shard_index).ok_or_else(|| {
            CrabError::Internal(format!(
                "file {} resolved absent finalized shard {shard_index}",
                file_hash.hex()
            ))
        })?;
        closure.extend(dependency_hashes.iter().copied());
    }
    for ((shard_bytes, shard_hash), dependency_hashes) in shards.iter().zip(shard_closures) {
        let mut closure = dependency_hashes
            .into_iter()
            .map(|hash| hash.hex())
            .collect::<Vec<_>>();
        closure.sort();
        for xorb_hash in &closure {
            if base_catalog.xorbs().contains_key(xorb_hash) {
                continue;
            }
            let hash = parse_merkle_hash(xorb_hash, "file xorb")?;
            let entry = xorb_entries.get(&hash).ok_or_else(|| {
                CrabError::Internal(format!("catalog lost xorb descriptor {xorb_hash}"))
            })?;
            delta.insert_xorb(xorb_hash.clone(), entry.clone())?;
        }
        delta.insert_shard(
            shard_hash.hex(),
            ShardCatalogEntry::new(shard_bytes.len() as u64, closure),
        )?;
    }
    for (file_hash, file_size, shard_index, _) in pending_files {
        let (_, shard_hash) = shards.get(shard_index).ok_or_else(|| {
            CrabError::Internal(format!(
                "file {} resolved absent finalized shard {shard_index}",
                file_hash.hex()
            ))
        })?;
        delta.insert_file(
            file_hash.hex(),
            FileCatalogEntry::new(file_size, shard_hash.hex()),
        )?;
    }
    tracing::debug!(
        files = delta.files().len(),
        shards = delta.shards().len(),
        xorbs = delta.xorbs().len(),
        uploaded_xorbs = uploaded_xorbs.len(),
        reused_xorbs,
        "prepared capsule-protocol pointer dependency closure"
    );
    Ok(delta)
}

async fn publish_built_xorb(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    result: XorbResult,
    placements: &mut ChunkPlacementMap,
    xorb_entries: &mut HashMap<MerkleHash, crab_metadata::capsule_protocol::XorbCatalogEntry>,
    uploaded_xorbs: &mut HashSet<MerkleHash>,
) -> Result<()> {
    let body_digest = blake3::hash(&result.bytes).to_hex().to_string();
    let entry = catalog_xorb_entry(result.bytes.len() as u64, body_digest, &result.placements)?;
    let (entry, xorb_placements, created) =
        publish_xorb_candidate(layout, result.hash, result.bytes, entry, result.placements).await?;
    if created {
        uploaded_xorbs.insert(result.hash);
    }
    for placement in xorb_placements {
        placements.entry(placement.chunk_hash).or_insert(placement);
    }
    xorb_entries.insert(result.hash, entry);
    Ok(())
}

async fn publish_xorb_candidate(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    xorb_hash: MerkleHash,
    bytes: Bytes,
    local_entry: crab_metadata::capsule_protocol::XorbCatalogEntry,
    local_placements: Vec<ChunkPlacement>,
) -> Result<(
    crab_metadata::capsule_protocol::XorbCatalogEntry,
    Vec<ChunkPlacement>,
    bool,
)> {
    let path = layout.xorb_path(&xorb_hash);
    match layout
        .store()
        .create_or_read_immutable(&path, bytes, MAX_XORB_SIZE as u64)
        .await?
    {
        crab_storage::ImmutableCreateOutcome::Created => Ok((local_entry, local_placements, true)),
        crab_storage::ImmutableCreateOutcome::Existing(existing) => {
            let expected_refs = local_placements
                .iter()
                .map(|placement| {
                    (
                        placement.chunk_hash,
                        XorbRef {
                            xorb_hash,
                            chunk_index: placement.chunk_index,
                            uncompressed_size: placement.uncompressed_size,
                        },
                    )
                })
                .collect::<HashMap<_, _>>();
            let verified = verify_external_xorb(&path, xorb_hash, &expected_refs, existing)?
                .ok_or_else(|| CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: format!(
                        "existing xorb {} does not authenticate as the requested logical content",
                        xorb_hash.hex()
                    ),
                })?;
            Ok((verified.1, verified.2, false))
        }
    }
}

fn planned_xorbs_match(
    left: &crab_staging::push_plan::PlannedXorb,
    right: &crab_staging::push_plan::PlannedXorb,
) -> bool {
    left.hash == right.hash
        && left.payload_hash == right.payload_hash
        && left.bytes == right.bytes
        && left.upload == right.upload
        && left.placements.len() == right.placements.len()
        && left
            .placements
            .iter()
            .zip(&right.placements)
            .all(|(left, right)| {
                left.chunk_hash == right.chunk_hash
                    && left.xorb_hash == right.xorb_hash
                    && left.chunk_index == right.chunk_index
                    && left.uncompressed_size == right.uncompressed_size
            })
}

fn verify_external_xorb(
    path: &object_store::path::Path,
    expected_hash: MerkleHash,
    expected_refs: &HashMap<MerkleHash, XorbRef>,
    bytes: Bytes,
) -> Result<
    Option<(
        MerkleHash,
        crab_metadata::capsule_protocol::XorbCatalogEntry,
        Vec<ChunkPlacement>,
    )>,
> {
    let encoded_size = bytes.len() as u64;
    let body_digest = blake3::hash(&bytes).to_hex().to_string();
    let parser = match XorbParser::parse(bytes) {
        Ok(parser) if parser.hash() == expected_hash => parser,
        Ok(parser) => {
            tracing::warn!(
                expected = %expected_hash.hex(),
                actual = %parser.hash().hex(),
                path = %path,
                "ignored stale remote xorb authority with the wrong identity"
            );
            return Ok(None);
        }
        Err(error) => {
            tracing::warn!(
                expected = %expected_hash.hex(),
                path = %path,
                error = %error,
                "ignored corrupt remote xorb authority"
            );
            return Ok(None);
        }
    };
    if let Err(error) = parser
        .verify_payload_digest()
        .and_then(|()| parser.verify_all_chunks())
    {
        tracing::warn!(
            expected = %expected_hash.hex(),
            path = %path,
            error = %error,
            "ignored remote xorb authority that failed payload verification"
        );
        return Ok(None);
    }

    let mut xorb_placements = Vec::with_capacity(parser.num_chunks() as usize);
    for index in 0..parser.num_chunks() {
        let chunk = parser.chunk_meta(index)?;
        xorb_placements.push(ChunkPlacement {
            chunk_hash: chunk.hash,
            xorb_hash: expected_hash,
            chunk_index: index,
            uncompressed_size: chunk.uncompressed_len,
        });
    }
    for (chunk_hash, expected_ref) in expected_refs {
        let Some(actual) = xorb_placements.get(expected_ref.chunk_index as usize) else {
            tracing::warn!(
                xorb_hash = %expected_hash.hex(),
                chunk_hash = %chunk_hash.hex(),
                chunk_index = expected_ref.chunk_index,
                "ignored remote xorb authority with an out-of-range placement"
            );
            return Ok(None);
        };
        if expected_ref.xorb_hash != expected_hash
            || actual.chunk_hash != *chunk_hash
            || actual.uncompressed_size != expected_ref.uncompressed_size
        {
            tracing::warn!(
                xorb_hash = %expected_hash.hex(),
                chunk_hash = %chunk_hash.hex(),
                chunk_index = expected_ref.chunk_index,
                "ignored remote xorb authority with a mismatched placement"
            );
            return Ok(None);
        }
    }
    let entry = catalog_xorb_entry(encoded_size, body_digest, &xorb_placements)?;
    Ok(Some((expected_hash, entry, xorb_placements)))
}

fn verify_prepared_xorb(
    path: &Path,
    expected_hash: MerkleHash,
    planned: &crab_staging::push_plan::PlannedXorb,
    bytes: Bytes,
) -> Result<(
    crab_metadata::capsule_protocol::XorbCatalogEntry,
    Vec<ChunkPlacement>,
)> {
    if bytes.len() as u64 != planned.bytes {
        return Err(CrabError::StagingCorrupt(format!(
            "prepared xorb {} at {} has {} bytes, plan declares {}",
            expected_hash.hex(),
            path.display(),
            bytes.len(),
            planned.bytes
        )));
    }
    let body_digest = blake3::hash(&bytes).to_hex().to_string();
    if body_digest != planned.payload_hash {
        return Err(CrabError::StagingCorrupt(format!(
            "prepared xorb {} body digest does not match its plan",
            expected_hash.hex()
        )));
    }
    let parser = XorbParser::parse(bytes)?;
    if parser.hash() != expected_hash {
        return Err(CrabError::StagingCorrupt(format!(
            "prepared xorb {} parses as {}",
            expected_hash.hex(),
            parser.hash().hex()
        )));
    }
    parser.verify_payload_digest()?;
    parser.verify_all_chunks()?;
    let mut placements = Vec::with_capacity(parser.num_chunks() as usize);
    for index in 0..parser.num_chunks() {
        let chunk = parser.chunk_meta(index)?;
        placements.push(ChunkPlacement {
            chunk_hash: chunk.hash,
            xorb_hash: expected_hash,
            chunk_index: index,
            uncompressed_size: chunk.uncompressed_len,
        });
    }
    let planned_placements = planned
        .placements
        .iter()
        .map(crab_staging::push_plan::PlannedPlacement::to_placement)
        .collect::<crab_staging::Result<Vec<_>>>()?;
    if planned_placements.len() != placements.len()
        || planned_placements
            .iter()
            .zip(&placements)
            .any(|(left, right)| {
                left.chunk_hash != right.chunk_hash
                    || left.xorb_hash != right.xorb_hash
                    || left.chunk_index != right.chunk_index
                    || left.uncompressed_size != right.uncompressed_size
            })
    {
        return Err(CrabError::StagingCorrupt(format!(
            "prepared xorb {} placements do not match its body",
            expected_hash.hex()
        )));
    }
    Ok((
        catalog_xorb_entry(planned.bytes, body_digest, &placements)?,
        placements,
    ))
}

fn catalog_xorb_entry(
    encoded_size: u64,
    body_digest: String,
    placements: &[ChunkPlacement],
) -> Result<crab_metadata::capsule_protocol::XorbCatalogEntry> {
    let mut ordered = placements.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|placement| placement.chunk_index);
    for (index, placement) in ordered.iter().enumerate() {
        if placement.chunk_index
            != u32::try_from(index).map_err(|_| {
                CrabError::Internal("xorb placement index cannot be represented".to_owned())
            })?
        {
            return Err(CrabError::StagingCorrupt(format!(
                "xorb {} placements are not dense",
                placement.xorb_hash.hex()
            )));
        }
    }
    Ok(crab_metadata::capsule_protocol::XorbCatalogEntry::new(
        encoded_size,
        body_digest,
        ordered
            .into_iter()
            .map(|placement| {
                crab_metadata::capsule_protocol::XorbChunkEntry::new(
                    placement.chunk_hash.hex(),
                    placement.uncompressed_size,
                )
            })
            .collect(),
    ))
}

fn placements_for_catalog_xorb(
    xorb_hash: MerkleHash,
    entry: &crab_metadata::capsule_protocol::XorbCatalogEntry,
) -> Result<Vec<ChunkPlacement>> {
    entry
        .chunks()
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            Ok(ChunkPlacement {
                chunk_hash: parse_merkle_hash(chunk.hash(), "catalog chunk")?,
                xorb_hash,
                chunk_index: u32::try_from(index).map_err(|_| {
                    CrabError::Internal("catalog xorb has too many chunks".to_owned())
                })?,
                uncompressed_size: chunk.uncompressed_size(),
            })
        })
        .collect()
}

fn parse_merkle_hash(value: &str, label: &str) -> Result<MerkleHash> {
    MerkleHash::from_hex(value).map_err(|error| CrabError::CorruptObject {
        path: "capsule-protocol pointer catalog".to_owned(),
        reason: format!("invalid {label} hash {value}: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crab_xet::hash::compute_data_hash;
    use crab_xet::xorb::builder::{CompressionPolicy, FixedCompression};
    use crab_xet::xorb::format::CompressionScheme;

    use super::*;

    fn build_xorb(data: &[u8], scheme: CompressionScheme) -> XorbResult {
        let chunk = Chunk {
            hash: compute_data_hash(data),
            data: data.to_vec().into(),
        };
        let policy = Arc::new(FixedCompression::new(scheme)) as Arc<dyn CompressionPolicy>;
        let mut builder = XorbBuilder::with_policy(policy);
        builder.push(&chunk, RunId(0)).expect("push chunk");
        builder
            .finalize()
            .expect("finalize xorb")
            .pop()
            .expect("one xorb")
    }

    #[tokio::test]
    async fn logical_xorb_conflict_adopts_fully_verified_existing_encoding() {
        let data = (0..128 * 1024)
            .map(|index| ((index * 31 + index / 7) % 251) as u8)
            .collect::<Vec<_>>();
        let existing = build_xorb(&data, CompressionScheme::None);
        let candidate = build_xorb(&data, CompressionScheme::LZ4);
        assert_eq!(existing.hash, candidate.hash);
        assert_ne!(existing.bytes, candidate.bytes);

        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store, "repo".to_owned());
        layout
            .store()
            .put_if_absent_verified(&layout.xorb_path(&existing.hash), existing.bytes.clone())
            .await
            .expect("seed existing encoding");
        let candidate_entry = catalog_xorb_entry(
            candidate.bytes.len() as u64,
            blake3::hash(&candidate.bytes).to_hex().to_string(),
            &candidate.placements,
        )
        .expect("candidate entry");

        let (entry, placements, created) = publish_xorb_candidate(
            &layout,
            candidate.hash,
            candidate.bytes,
            candidate_entry,
            candidate.placements,
        )
        .await
        .expect("adopt logical xorb conflict");

        assert!(!created);
        assert_eq!(entry.encoded_size(), existing.bytes.len() as u64);
        assert_eq!(
            entry.body_digest(),
            blake3::hash(&existing.bytes).to_hex().as_str()
        );
        assert_eq!(placements.len(), existing.placements.len());
    }
}
