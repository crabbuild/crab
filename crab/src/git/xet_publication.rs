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
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::metrics::Metrics;

// Each task can retain a complete candidate and origin readback. Keep that
// existing payload bound even when a caller requests more network concurrency.
const MAX_IN_FLIGHT_XORBS: usize = 4;

#[derive(Debug, Clone, Default)]
pub(crate) struct XetPublicationStats {
    pub xorbs_uploaded: u64,
    pub shards_uploaded: u64,
    pub xorb_bytes_uploaded: u64,
    pub origins: HashMap<MerkleHash, crab_metadata::receipts::OriginReceipt>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "publication dependencies and admission are explicit"
)]
pub(crate) async fn prepare_delta(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    base: &crab_metadata::capsule_protocol::RootSnapshot,
    pointers: &[crab_types::pointer::Pointer],
    staging: Option<&Arc<StagingAreaReadOnly>>,
    caching_store: Option<&crab_cache_store::CachingStore>,
    metrics: Option<&Metrics>,
    metadb: &crate::core::config::MetaDbTomlConfig,
    upload_concurrency: usize,
    publish_gc_roots: bool,
    cancel: &CancellationToken,
) -> Result<(
    crab_metadata::capsule_protocol::PointerCatalog,
    XetPublicationStats,
)> {
    use crab_metadata::capsule_protocol::{
        FileCatalogEntry, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry,
    };

    if pointers.is_empty() {
        return Ok((PointerCatalog::new(), XetPublicationStats::default()));
    }
    let concurrency = upload_concurrency.clamp(1, MAX_IN_FLIGHT_XORBS);
    let view = crab_read::capsule_protocol::open_git_view_from_root(
        layout,
        base.clone(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 8 * 1024 * 1024 * 1024,
        },
    )
    .await?;
    let lookup =
        view.pointer_catalog_lookup(layout, crab_metadata::capsule_protocol::MAX_CATALOG_BYTES)?;
    drop(view);

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

    let file_hashes = pointer_sizes
        .keys()
        .map(|hash| hash.hex())
        .collect::<Vec<_>>();
    let base_files = lookup.files(layout.store(), &file_hashes).await?;
    let unresolved = pointer_sizes
        .iter()
        .filter_map(|(hash, size)| match base_files.files().get(&hash.hex()) {
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
    drop(base_files);
    if unresolved.is_empty() {
        return Ok((PointerCatalog::new(), XetPublicationStats::default()));
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

    let file_hashes = unresolved.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
    let recipes = super::push_staging::recipes_for_selected_pointers(staging, &file_hashes)?;
    let recipe_hashes = recipes
        .values()
        .flatten()
        .map(|recipe| MerkleHash::from(recipe.hash()))
        .collect::<Vec<_>>();
    let mut prepared_xorbs = staging.prepared_xorbs_for_recipes(&recipe_hashes)?;

    let mut xorb_entries = HashMap::<MerkleHash, XorbCatalogEntry>::new();
    let mut placements = ChunkPlacementMap::new();

    let mut delta = PointerCatalog::new();
    let mut shard_session = PushShardSession::new();
    let mut pending_files = Vec::with_capacity(unresolved.len());
    let mut uploaded_xorbs = HashSet::new();
    let mut origins = HashMap::new();
    let mut xorb_bytes_uploaded = 0_u64;
    let mut prepared_files = Vec::with_capacity(unresolved.len());
    let mut prepared_candidates = Vec::<crab_staging::push_plan::PlannedXorb>::new();
    let mut staged_candidates = HashMap::new();

    for (file_hash, file_size) in unresolved {
        check_cancelled(cancel)?;
        let recipe = recipes
            .get(&file_hash)
            .and_then(Option::as_ref)
            .ok_or_else(|| CrabError::PointerMissingStaging {
                total: file_hashes.len(),
                missing: 1,
                example_file_hash: file_hash.hex(),
                example_size: file_size,
            })?;
        if recipe.file_size() != file_size {
            return Err(CrabError::StagingCorrupt(format!(
                "staged recipe for file {} has {} bytes, pointer size {file_size}",
                file_hash.hex(),
                recipe.file_size(),
            )));
        }
        let mut chunks = Vec::new();
        let mut next = 0;
        while next < recipe.chunk_count() {
            let page = staging.recipe_page(recipe, next)?;
            next = page.next_occurrence();
            chunks.extend(
                page.chunks
                    .iter()
                    .map(|chunk| (chunk.chunk_hash, chunk.len)),
            );
        }
        let mut plan = crab_staging::push_plan::FilePushPlan::new_verified_recipe(recipe);
        plan.prepared_xorbs = prepared_xorbs
            .remove(&MerkleHash::from(recipe.hash()))
            .unwrap_or_default();

        // The selected immutable recipe carries the ingestion verification.
        // Inspect its remote authority without reconstructing the entire file;
        // each local or remote xorb consumed below is independently verified.
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
                    staging.recipe_remote_chunk_range(recipe, next_occurrence, page_end)?
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
                    match staged_candidates.insert(chunk_hash, candidate) {
                        Some(existing) if existing != candidate => {
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

    // Query only the outgoing recipes. Selected xorb recipes are verified
    // against the reverse chunk index before any staged payload is reused.
    let mut needed_chunks = prepared_files
        .iter()
        .flat_map(|(_, _, chunks, _)| chunks.iter().map(|(hash, _)| hash.hex()))
        .collect::<HashSet<_>>();
    let requested_chunks = needed_chunks.iter().cloned().collect::<Vec<_>>();
    let base_chunks = lookup.chunks(layout.store(), &requested_chunks).await?;
    for (hash, entry) in base_chunks.xorbs() {
        if needed_chunks.is_empty() {
            break;
        }
        let mut selected = false;
        for (index, chunk) in entry.chunks().iter().enumerate() {
            if !needed_chunks.remove(chunk.hash()) {
                continue;
            }
            let chunk_hash = parse_merkle_hash(chunk.hash(), "catalog chunk")?;
            placements.insert(
                chunk_hash,
                ChunkPlacement {
                    chunk_hash,
                    xorb_hash: parse_merkle_hash(hash, "base xorb")?,
                    chunk_index: u32::try_from(index).map_err(|_| {
                        CrabError::Internal("catalog xorb has too many chunks".to_owned())
                    })?,
                    uncompressed_size: chunk.uncompressed_size(),
                },
            );
            selected = true;
        }
        if selected {
            xorb_entries.insert(parse_merkle_hash(hash, "base xorb")?, entry.clone());
        }
    }
    drop(base_chunks);
    drop(needed_chunks);

    // The recipe retains the exact add-time IDs. Re-read their immutable
    // records and current source roots before trusting any remote-only bytes.
    staged_candidates.retain(|hash, _| !placements.contains_key(hash));
    let planned_existing = staged_candidates.len();
    let verified_staged =
        super::push::revalidate_staged_remote_chunks(metadb, layout, &staged_candidates, cancel)
            .await?;
    tracing::debug!(
        planned_existing,
        stale_existing = planned_existing.saturating_sub(verified_staged.candidates.len()),
        global_existing = 0,
        "capsule staged remote proof adoption complete",
    );
    let mut reused_xorbs = verified_staged.xorbs.len();
    let requested = requested_chunks
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    for (hash, (entry, origin)) in verified_staged.xorbs {
        if xorb_entries
            .get(&hash)
            .is_some_and(|existing| existing != &entry)
        {
            return Err(CrabError::CorruptObject {
                path: layout.xorb_path(&hash).to_string(),
                reason: "verified xorb differs from the captured destination catalog".to_owned(),
            });
        }
        // The authenticated closure also proves requested chunks whose rebuildable
        // heads were absent during add. Expand only outgoing occurrences, so an
        // incomplete global index cannot force repacking the same verified bytes.
        for (index, chunk) in entry.chunks().iter().enumerate() {
            if requested.contains(chunk.hash()) {
                let chunk_hash = parse_merkle_hash(chunk.hash(), "verified catalog chunk")?;
                placements.entry(chunk_hash).or_insert(ChunkPlacement {
                    chunk_hash,
                    xorb_hash: hash,
                    chunk_index: u32::try_from(index).map_err(|_| {
                        CrabError::Internal("verified xorb has too many chunks".to_owned())
                    })?,
                    uncompressed_size: chunk.uncompressed_size(),
                });
            }
        }
        xorb_entries.insert(hash, entry);
        origins.insert(hash, origin);
    }
    let mut external_candidates = HashMap::<MerkleHash, HashMap<MerkleHash, XorbRef>>::new();
    for (hash, candidate) in verified_staged.candidates {
        let xorb_ref = candidate.xorb_ref;
        if xorb_entries.contains_key(&xorb_ref.xorb_hash) {
            // Exact source membership and current provider identity already
            // certify this descriptor. Re-reading payload would download a
            // whole reused file despite the committed origin receipt.
            placements.entry(hash).or_insert(ChunkPlacement {
                chunk_hash: hash,
                xorb_hash: xorb_ref.xorb_hash,
                chunk_index: xorb_ref.chunk_index,
                uncompressed_size: xorb_ref.uncompressed_size,
            });
        } else {
            // Manifest receipts shipped in v1.2.4 lack capsule body descriptors.
            // Full-body verification supplies that value when importing them
            // into capsule publication.
            external_candidates
                .entry(xorb_ref.xorb_hash)
                .or_default()
                .insert(hash, xorb_ref);
        }
    }
    let mut external_candidates = external_candidates.into_iter().collect::<Vec<_>>();
    external_candidates.sort_by_key(|(hash, _)| hash.hex());
    let external_reads =
        futures_util::stream::iter(external_candidates.into_iter().enumerate().map(
            |(ordinal, (expected_hash, expected_refs))| async move {
                check_cancelled(cancel)?;
                let path = layout.xorb_path(&expected_hash);
                let (bytes, identity) = match layout
                    .store()
                    .get_with_etag_bounded(&path, MAX_XORB_SIZE as u64)
                    .await
                {
                    Ok(result) => result,
                    Err(crab_storage::StorageError::NotFound { .. }) => {
                        return Ok::<_, CrabError>((ordinal, None));
                    }
                    Err(error) => return Err(error.into()),
                };
                let verified =
                    verify_external_xorb(&path, expected_hash, &expected_refs, bytes, identity)?;
                Ok((ordinal, verified))
            },
        ))
        .buffer_unordered(concurrency);
    tokio::pin!(external_reads);
    let mut verified_external = Vec::new();
    while let Some(result) = external_reads.next().await {
        verified_external.push(result?);
    }
    verified_external.sort_by_key(|(ordinal, _)| *ordinal);
    for (_, verified) in verified_external {
        let Some((xorb_hash, entry, xorb_placements, origin)) = verified else {
            continue;
        };
        reused_xorbs += 1;
        origins.insert(xorb_hash, origin);
        for placement in xorb_placements {
            placements.entry(placement.chunk_hash).or_insert(placement);
        }
        xorb_entries.insert(xorb_hash, entry);
    }

    let mut candidate_chunks = placements.keys().copied().collect::<HashSet<_>>();
    let mut seen_candidates = HashMap::new();
    for (_, _, _, plan) in &prepared_files {
        for planned in &plan.prepared_xorbs {
            let planned_hash = planned.hash()?;
            if let Some(existing) = seen_candidates.insert(planned_hash, planned) {
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
            // Direct adoption is only for wholly new outgoing payloads. A
            // single missing chunk must not upload an otherwise reused xorb;
            // residual packing below reads only the still-needed chunks.
            if !planned_placements.iter().all(|placement| {
                requested.contains(placement.chunk_hash.hex().as_str())
                    && !candidate_chunks.contains(&placement.chunk_hash)
            }) {
                continue;
            }
            candidate_chunks.extend(
                planned_placements
                    .iter()
                    .map(|placement| placement.chunk_hash),
            );
            prepared_candidates.push(planned.clone());
        }
    }

    drop(requested);

    let uploads = futures_util::stream::iter(prepared_candidates.into_iter().enumerate().map(
        |(ordinal, planned)| async move {
            check_cancelled(cancel)?;
            let planned_hash = planned.hash()?;
            let path = crab_staging::push_plan::prepared_xorb_path(staging.root(), &planned_hash);
            if planned.bytes > MAX_XORB_SIZE as u64 {
                return Err(CrabError::StagingCorrupt(format!(
                    "prepared xorb {} exceeds the {} byte limit",
                    planned_hash.hex(),
                    MAX_XORB_SIZE,
                )));
            }
            // A changed or corrupt local body must fail without allocating its
            // untrusted filesystem length; verification rejects short/long reads.
            let mut bytes = Vec::new();
            tokio::fs::File::open(&path)
                .await?
                .take(planned.bytes + 1)
                .read_to_end(&mut bytes)
                .await?;
            let bytes = Bytes::from(bytes);
            let (entry, xorb_placements) =
                verify_prepared_xorb(&path, planned_hash, &planned, bytes.clone())?;
            let (entry, xorb_placements, created, origin) = publish_xorb_candidate(
                layout,
                caching_store,
                planned_hash,
                bytes,
                entry,
                xorb_placements,
            )
            .await?;
            let uploaded_bytes = if created { entry.encoded_size() } else { 0 };
            Ok::<_, CrabError>((
                ordinal,
                planned_hash,
                entry,
                xorb_placements,
                uploaded_bytes,
                origin,
            ))
        },
    ))
    .buffer_unordered(concurrency);
    tokio::pin!(uploads);
    let mut verified_candidates = Vec::new();
    while let Some(result) = uploads.next().await {
        verified_candidates.push(result?);
    }
    verified_candidates.sort_by_key(|(ordinal, _, _, _, _, _)| *ordinal);
    for (_, planned_hash, entry, xorb_placements, uploaded_bytes, origin) in verified_candidates {
        if let Some(origin) = origin {
            origins.insert(planned_hash, origin);
        }
        if uploaded_bytes > 0 {
            uploaded_xorbs.insert(planned_hash);
            xorb_bytes_uploaded = xorb_bytes_uploaded.saturating_add(uploaded_bytes);
            if let Some(metrics) = metrics {
                metrics.add_bytes_uploaded(uploaded_bytes);
            }
        }
        for placement in xorb_placements {
            placements.entry(placement.chunk_hash).or_insert(placement);
        }
        xorb_entries.insert(planned_hash, entry);
    }

    let mut missing = prepared_files
        .iter()
        .flat_map(|(_, _, chunks, _)| chunks.iter().map(|(hash, _)| *hash))
        .filter(|hash| !placements.contains_key(hash))
        .collect::<HashSet<_>>();
    if !missing.is_empty() {
        // The same prepared body can supply several files. Gather the whole
        // residual schedule before reading, so its verified source opens once
        // rather than once per file or fixed-size request page.
        let mut read_plan = staging.begin_coalesced_chunk_read_plan()?;
        let mut requests = Vec::with_capacity(256);
        for (ordinal, (_, _, chunks, _)) in prepared_files.iter().enumerate() {
            let context = u64::try_from(ordinal)
                .map_err(|_| CrabError::Internal("pointer file ordinal overflowed".to_owned()))?;
            for (hash, size) in chunks {
                if !missing.remove(hash) {
                    continue;
                }
                requests.push((*hash, *size, context));
                if requests.len() == 256 {
                    check_cancelled(cancel)?;
                    read_plan.append(&requests)?;
                    requests.clear();
                }
            }
        }
        read_plan.append(&requests)?;
        drop(missing);
        let mut builder = XorbBuilder::new();
        while let Some(payloads) = read_plan.read_next(256, 16 * 1024 * 1024).await? {
            check_cancelled(cancel)?;
            for (context, hash, data) in payloads {
                builder.push(&Chunk { hash, data }, RunId(context))?;
                while let Some(result) = builder.take_completed() {
                    let uploaded_bytes = publish_built_xorb(
                        layout,
                        caching_store,
                        result,
                        &mut placements,
                        &mut xorb_entries,
                        &mut uploaded_xorbs,
                        &mut origins,
                    )
                    .await?;
                    xorb_bytes_uploaded = xorb_bytes_uploaded.saturating_add(uploaded_bytes);
                    if let Some(metrics) = metrics {
                        metrics.add_bytes_uploaded(uploaded_bytes);
                    }
                }
            }
        }
        for result in builder.finalize()? {
            let uploaded_bytes = publish_built_xorb(
                layout,
                caching_store,
                result,
                &mut placements,
                &mut xorb_entries,
                &mut uploaded_xorbs,
                &mut origins,
            )
            .await?;
            xorb_bytes_uploaded = xorb_bytes_uploaded.saturating_add(uploaded_bytes);
            if let Some(metrics) = metrics {
                metrics.add_bytes_uploaded(uploaded_bytes);
            }
        }
    }

    for (file_hash, file_size, chunks, _) in prepared_files {
        check_cancelled(cancel)?;
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
    let mut shards_uploaded = 0_u64;
    for (bytes, hash) in &shards {
        let path = layout.shard_path(hash);
        let bytes = Bytes::from(bytes.clone());
        let created = layout
            .store()
            .put_if_absent_verified(&path, bytes.clone())
            .await?;
        if created {
            shards_uploaded = shards_uploaded.saturating_add(1);
        }
        if created && let Some(cache) = caching_store {
            warm_published_cache_object(cache, &path, crab_cache::CacheKey::Shard(*hash), bytes)
                .await;
        }
        shard_hashes.push(hash.hex());
    }
    // Registry partitions are monotonic. Re-registering the pinned catalog
    // would rewrite every historical partition on each push without adding
    // protection; only this transaction's candidate roots need unioning.
    if publish_gc_roots {
        crab_metadata::ref_registry::union_register_repo_shards(
            layout.store(),
            layout,
            shard_hashes.iter().cloned().collect(),
        )
        .await?;
    }

    let dependency_hashes = pending_files
        .iter()
        .flat_map(|(_, _, _, hashes)| hashes.iter().map(|hash| hash.hex()))
        .collect::<Vec<_>>();
    let base_xorbs = lookup.xorbs(layout.store(), &dependency_hashes).await?;
    for (hash, entry) in base_xorbs.xorbs() {
        if xorb_entries
            .get(&parse_merkle_hash(hash, "base xorb")?)
            .is_some_and(|candidate| candidate != entry)
        {
            return Err(CrabError::CorruptObject {
                path: "capsule-protocol pointer catalog".to_owned(),
                reason: format!("xorb {hash} has conflicting descriptors"),
            });
        }
    }
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
            if base_xorbs.xorbs().contains_key(xorb_hash) {
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
    Ok((
        delta,
        XetPublicationStats {
            xorbs_uploaded: uploaded_xorbs.len() as u64,
            shards_uploaded,
            xorb_bytes_uploaded,
            origins,
        },
    ))
}

async fn publish_built_xorb(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    caching_store: Option<&crab_cache_store::CachingStore>,
    result: XorbResult,
    placements: &mut ChunkPlacementMap,
    xorb_entries: &mut HashMap<MerkleHash, crab_metadata::capsule_protocol::XorbCatalogEntry>,
    uploaded_xorbs: &mut HashSet<MerkleHash>,
    origins: &mut HashMap<MerkleHash, crab_metadata::receipts::OriginReceipt>,
) -> Result<u64> {
    let encoded_size = result.bytes.len() as u64;
    let body_digest = blake3::hash(&result.bytes).to_hex().to_string();
    let entry = catalog_xorb_entry(result.bytes.len() as u64, body_digest, &result.placements)?;
    let (entry, xorb_placements, created, origin) = publish_xorb_candidate(
        layout,
        caching_store,
        result.hash,
        result.bytes,
        entry,
        result.placements,
    )
    .await?;
    if let Some(origin) = origin {
        origins.insert(result.hash, origin);
    }
    if created {
        uploaded_xorbs.insert(result.hash);
    }
    for placement in xorb_placements {
        placements.entry(placement.chunk_hash).or_insert(placement);
    }
    xorb_entries.insert(result.hash, entry);
    Ok(if created { encoded_size } else { 0 })
}

async fn publish_xorb_candidate(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    caching_store: Option<&crab_cache_store::CachingStore>,
    xorb_hash: MerkleHash,
    bytes: Bytes,
    local_entry: crab_metadata::capsule_protocol::XorbCatalogEntry,
    local_placements: Vec<ChunkPlacement>,
) -> Result<(
    crab_metadata::capsule_protocol::XorbCatalogEntry,
    Vec<ChunkPlacement>,
    bool,
    Option<crab_metadata::receipts::OriginReceipt>,
)> {
    let path = layout.xorb_path(&xorb_hash);
    let staged = layout.store().staging_write_prefix().is_some();
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
    let cache_has_candidate = match caching_store {
        Some(cache) => {
            cache
                .local_cache()
                .contains_verified(&crab_cache::CacheKey::Xorb(xorb_hash))
                .await
                || cache_service_has_candidate_xorb(
                    cache,
                    layout.repo_prefix(),
                    xorb_hash,
                    &local_placements,
                )
                .await
        }
        None => false,
    };
    if cache_has_candidate || staged {
        match layout
            .store()
            .get_with_etag_bounded(&path, MAX_XORB_SIZE as u64)
            .await
        {
            Ok((existing, identity)) => {
                let verified = verify_external_xorb(
                    &path,
                    xorb_hash,
                    &expected_refs,
                    existing,
                    identity,
                )?
                .ok_or_else(|| CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: format!(
                        "existing xorb {} does not authenticate as the requested logical content",
                        xorb_hash.hex()
                    ),
                })?;
                return Ok((verified.1, verified.2, false, Some(verified.3)));
            }
            Err(crab_storage::StorageError::NotFound { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    if staged {
        // Protected clients may only write their session prefix. Reuse the
        // canonical encoding above when present; otherwise record verified
        // staged bytes for service-owned promotion before ref publication.
        let created = layout.store().put_if_absent_verified(&path, bytes).await?;
        return Ok((local_entry, local_placements, created, None));
    }
    let cache_bytes = caching_store.map(|_| bytes.clone());
    // Local builder/prepared verification already authenticated these bytes.
    // Carry the payload digest with the exact immutable PUT/readback identity.
    let payload_digest = XorbParser::parse(bytes.clone())?.payload_digest();
    match layout
        .store()
        .create_or_read_immutable(&path, bytes, MAX_XORB_SIZE as u64)
        .await?
    {
        crab_storage::ImmutableCreateOutcome::Created(identity) => {
            if let (Some(cache), Some(bytes)) = (caching_store, cache_bytes) {
                warm_published_cache_object(
                    cache,
                    &path,
                    crab_cache::CacheKey::Xorb(xorb_hash),
                    bytes,
                )
                .await;
            }
            let origin = crab_metadata::receipts::OriginReceipt::new(
                "canonical-origin".to_owned(),
                path.to_string(),
                xorb_hash.into(),
                payload_digest,
                local_entry.encoded_size(),
                identity.e_tag,
                identity.version,
            );
            Ok((local_entry, local_placements, true, Some(origin)))
        }
        crab_storage::ImmutableCreateOutcome::Existing {
            bytes: existing,
            identity,
        } => {
            let verified = verify_external_xorb(
                &path,
                xorb_hash,
                &expected_refs,
                existing,
                identity,
            )?
            .ok_or_else(|| CrabError::CorruptObject {
                path: path.to_string(),
                reason: format!(
                    "existing xorb {} does not authenticate as the requested logical content",
                    xorb_hash.hex()
                ),
            })?;
            Ok((verified.1, verified.2, false, Some(verified.3)))
        }
    }
}

async fn warm_published_cache_object(
    cache: &crab_cache_store::CachingStore,
    path: &object_store::path::Path,
    key: crab_cache::CacheKey,
    bytes: Bytes,
) {
    if let Err(error) = cache.local_cache().put_bytes(&key, bytes.clone()).await {
        tracing::warn!(
            path = %path,
            error = %error,
            "published immutable object could not be installed in the local cache"
        );
    }
    if let Err(error) = cache.warm_remote_only(path, bytes).await {
        tracing::warn!(
            path = %path,
            error = %error,
            "published immutable object could not be warmed in the cache service"
        );
    }
}

async fn cache_service_has_candidate_xorb(
    cache: &crab_cache_store::CachingStore,
    repo_prefix: &str,
    expected_xorb: MerkleHash,
    placements: &[ChunkPlacement],
) -> bool {
    if placements.is_empty() {
        return false;
    }
    let chunk_hashes = placements
        .iter()
        .map(|placement| <MerkleHash as Into<[u8; 32]>>::into(placement.chunk_hash))
        .collect::<Vec<_>>();
    let result = match cache.dedup_query(repo_prefix, &chunk_hashes).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(
                repo_prefix,
                error = %error,
                "cache service xorb lookup failed; using conditional origin publication"
            );
            return false;
        }
    };
    if !result.unknown.is_empty() || result.known.len() != placements.len() {
        return false;
    }

    let mut matches = vec![false; placements.len()];
    for known in result.known {
        let Some(expected) = placements.get(known.index) else {
            return false;
        };
        let Ok(actual_xorb) = MerkleHash::from_hex(&known.xorb_hash) else {
            return false;
        };
        if !known.cache_verified
            || actual_xorb != expected_xorb
            || known.chunk_index != expected.chunk_index
            || known.length != expected.uncompressed_size
            || matches[known.index]
        {
            return false;
        }
        matches[known.index] = true;
    }
    matches.into_iter().all(|matched| matched)
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
    identity: crab_storage::ETag,
) -> Result<
    Option<(
        MerkleHash,
        crab_metadata::capsule_protocol::XorbCatalogEntry,
        Vec<ChunkPlacement>,
        crab_metadata::receipts::OriginReceipt,
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
    let origin = crab_metadata::receipts::OriginReceipt::new(
        "canonical-origin".to_owned(),
        path.to_string(),
        expected_hash.into(),
        parser.payload_digest(),
        encoded_size,
        identity.e_tag,
        identity.version,
    );
    Ok(Some((expected_hash, entry, xorb_placements, origin)))
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
    #[cfg(test)]
    tests::EXPANDED_CATALOG_CHUNKS.with(|count| count.set(count.get() + entry.chunks().len()));
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
    use std::cell::Cell;
    use std::sync::Arc;
    use std::time::Instant;

    use crab_metadata::capsule_protocol::{
        Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
        FileCatalogEntry, PointerCatalog, RootSnapshot, ShardCatalogEntry, XorbCatalogEntry,
        XorbChunkEntry,
    };
    use crab_types::pointer::Pointer;
    use crab_xet::hash::compute_data_hash;
    use crab_xet::xorb::builder::{CompressionPolicy, FixedCompression};
    use crab_xet::xorb::format::CompressionScheme;

    use super::*;

    thread_local! {
        pub(super) static EXPANDED_CATALOG_CHUNKS: Cell<usize> = const { Cell::new(0) };
    }

    fn pointer_catalog(xorb_count: usize, chunks_per_xorb: usize) -> (PointerCatalog, Pointer) {
        let pointer = Pointer {
            file_hash: [3; 32],
            size: 9,
            shard_hint: None,
        };
        let mut catalog = PointerCatalog::new();
        let mut xorb_hashes = Vec::new();
        for xorb in 0..xorb_count {
            let hash = compute_data_hash(format!("xorb {xorb}").as_bytes()).hex();
            catalog
                .insert_xorb(
                    hash.clone(),
                    XorbCatalogEntry::new(
                        100,
                        "4".repeat(64),
                        (0..chunks_per_xorb)
                            .map(|chunk| {
                                let ordinal = xorb * chunks_per_xorb + chunk;
                                XorbChunkEntry::new(
                                    compute_data_hash(format!("chunk {ordinal}").as_bytes()).hex(),
                                    9,
                                )
                            })
                            .collect(),
                    ),
                )
                .unwrap();
            xorb_hashes.push(hash);
        }
        xorb_hashes.sort();
        let shard_hash = "2".repeat(64);
        catalog
            .insert_shard(shard_hash.clone(), ShardCatalogEntry::new(50, xorb_hashes))
            .unwrap();
        catalog
            .insert_file(
                MerkleHash::from(pointer.file_hash).hex(),
                FileCatalogEntry::new(pointer.size, shard_hash),
            )
            .unwrap();
        (catalog, pointer)
    }

    async fn catalog_publication(
        bytes: Bytes,
    ) -> (
        crab_storage::StoreLayout<crab_storage::Store>,
        RootSnapshot,
        CapsuleTransaction,
        Capsule,
    ) {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store, "repo".to_owned());
        let base =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let transaction = CapsuleTransaction::new(
            base.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(CapsuleSectionKind::CatalogDelta, bytes)],
        )
        .unwrap();
        (layout, base, transaction, capsule)
    }

    async fn publish_catalog(
        bytes: Bytes,
    ) -> (crab_storage::StoreLayout<crab_storage::Store>, RootSnapshot) {
        let (layout, base, transaction, capsule) = catalog_publication(bytes).await;
        let base = crab_write::capsule_protocol::publish(&layout, base, &transaction, &capsule)
            .await
            .unwrap();
        (layout, base)
    }

    #[tokio::test]
    async fn known_pointers_do_not_expand_catalog_chunk_placements() {
        let (catalog, pointer) = pointer_catalog(64, 1024);
        let (layout, base) = publish_catalog(catalog.encode().unwrap()).await;
        EXPANDED_CATALOG_CHUNKS.set(0);
        let started = Instant::now();
        let mut outcome = None;
        for _ in 0..3 {
            outcome = Some(
                prepare_delta(
                    &layout,
                    &base,
                    std::slice::from_ref(&pointer),
                    None,
                    None,
                    None,
                    &Default::default(),
                    4,
                    true,
                    &CancellationToken::new(),
                )
                .await
                .unwrap(),
            );
        }
        let elapsed = started.elapsed();
        let expanded = EXPANDED_CATALOG_CHUNKS.replace(0);
        eprintln!(
            "known-pointer preparation: {elapsed:?} for three calls, {expanded} expanded chunks"
        );
        let (delta, stats) = outcome.unwrap();
        assert_eq!(
            (
                delta.is_empty(),
                stats.xorbs_uploaded,
                stats.shards_uploaded,
                stats.xorb_bytes_uploaded,
                expanded
            ),
            (true, 0, 0, 0, 0),
        );
    }

    #[tokio::test]
    async fn edited_recipe_expands_only_its_xorb_dependencies() {
        let (catalog, _) = pointer_catalog(64, 1024);
        let (layout, base) = publish_catalog(catalog.encode().unwrap()).await;
        let root = tempfile::tempdir().unwrap();
        let data = b"chunk 100";
        let chunk_hash = compute_data_hash(data);
        let file_bytes = data.repeat(2);
        let file_hash = *blake3::hash(&file_bytes).as_bytes();
        let staging = crab_staging::StagingArea::open(root.path().to_owned())
            .await
            .unwrap();
        staging
            .pre_register_file(&MerkleHash::from(file_hash), 18)
            .unwrap();
        staging
            .stage_chunks_batch(
                &[
                    (&chunk_hash, data.as_slice()),
                    (&chunk_hash, data.as_slice()),
                ],
                &MerkleHash::from(file_hash),
                0,
            )
            .await
            .unwrap();
        staging.flush_pending().await.unwrap();
        let recipe = crab_staging::recipe::FileRecipe::from_staged_chunks(
            crab_staging::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            MerkleHash::from(file_hash),
            18,
            &[(chunk_hash, 9), (chunk_hash, 9)],
        )
        .unwrap();
        staging.publish_verified_history_recipe(&recipe).unwrap();
        staging.close().await.unwrap();
        let staging = Arc::new(
            StagingAreaReadOnly::open(root.path().to_owned())
                .await
                .unwrap(),
        );
        EXPANDED_CATALOG_CHUNKS.set(0);
        let (delta, stats) = prepare_delta(
            &layout,
            &base,
            &[Pointer {
                file_hash,
                size: 18,
                shard_hint: None,
            }],
            Some(&staging),
            None,
            None,
            &Default::default(),
            4,
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let expanded = EXPANDED_CATALOG_CHUNKS.replace(0);
        let shard_hash = delta.files()[&MerkleHash::from(file_hash).hex()].shard_hash();
        assert_eq!(
            (
                expanded,
                stats.xorbs_uploaded,
                delta.shards()[shard_hash].xorb_hashes().len()
            ),
            (1024, 0, 1),
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capsule_publication_requires_handoff_and_preserves_prepared_encoding() {
        use crab_staging::stream::{
            StreamStageProgress, StreamStageXorbBuilder, stage_reader_streaming,
        };
        for (delegated, wrong_size) in [(false, false), (true, true), (true, false)] {
            let root = tempfile::tempdir().unwrap();
            let staging = crab_staging::StagingArea::open(root.path().to_owned())
                .await
                .unwrap();
            let bytes = vec![0x71; 2 * 1024 * 1024];
            let staged = stage_reader_streaming(
                &mut std::io::Cursor::new(&bytes),
                Path::new("model.bin"),
                &staging,
                StreamStageProgress {
                    xorb_builder: Some(StreamStageXorbBuilder::new(1, || {
                        let policy = Arc::new(FixedCompression::new(CompressionScheme::None))
                            as Arc<dyn CompressionPolicy>;
                        XorbBuilder::with_policy(policy)
                    })),
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            if delegated {
                staging
                    .delegate_publication(&[(&staged.batch_id, Path::new("model.bin"))])
                    .unwrap();
            }
            staging.close().await.unwrap();
            let staging = Arc::new(
                StagingAreaReadOnly::open(root.path().to_owned())
                    .await
                    .unwrap(),
            );
            let expected = staging
                .prepared_xorbs_for_recipes(&[MerkleHash::from(staged.recipe.hash())])
                .unwrap();
            let expected = &expected[&MerkleHash::from(staged.recipe.hash())];
            assert_eq!(expected.len(), 1);
            let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
            let layout = crab_storage::StoreLayout::new(store, "repo".to_owned());
            let base = crab_write::capsule_protocol::initialize(
                &layout,
                &"1".repeat(64),
                "refs/heads/main",
            )
            .await
            .unwrap();
            let result = prepare_delta(
                &layout,
                &base,
                &[Pointer {
                    file_hash: staged.file_hash,
                    size: staged.size + u64::from(wrong_size),
                    shard_hint: None,
                }],
                Some(&staging),
                None,
                None,
                &Default::default(),
                1,
                false,
                &CancellationToken::new(),
            )
            .await;
            if !delegated || wrong_size {
                assert!(
                    result.is_err(),
                    "unaccepted or inconsistent recipe became publishable"
                );
                continue;
            }
            let (delta, stats) = result.unwrap();
            let actual = &delta.xorbs()[&expected[0].hash];
            assert_eq!(
                (
                    stats.xorbs_uploaded,
                    actual.body_digest(),
                    actual.encoded_size()
                ),
                (1, expected[0].payload_hash.as_str(), expected[0].bytes),
            );
        }
    }

    mod residual_tests;

    #[derive(Default)]
    struct UploadObserver {
        active: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    impl crab_storage::StorageObserver for UploadObserver {
        fn started(&self, operation: crab_storage::StorageOperation) {
            use std::sync::atomic::Ordering::SeqCst;
            if operation == crab_storage::StorageOperation::Put {
                let active = self.active.fetch_add(1, SeqCst) + 1;
                self.peak.fetch_max(active, SeqCst);
            }
        }

        fn finished(&self, observation: crab_storage::StorageObservation) {
            if observation.operation == crab_storage::StorageOperation::Put {
                self.active
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prepared_uploads_respect_requested_concurrency() {
        use crab_staging::stream::{
            StreamStageProgress, StreamStageXorbBuilder, stage_file_streaming,
        };
        use object_store::throttle::{ThrottleConfig, ThrottledStore};
        use std::sync::atomic::Ordering::SeqCst;

        let root = tempfile::tempdir().unwrap();
        let staging_root = root.path().join("staging");
        let staging = crab_staging::StagingArea::open(staging_root.clone())
            .await
            .unwrap();
        let mut pointers = Vec::new();
        for index in 0..6 {
            let path = root.path().join(format!("file-{index}"));
            std::fs::write(&path, vec![index; 65536]).unwrap();
            let preparation = staging.create_add_preparation().unwrap();
            let builders = StreamStageXorbBuilder::new(1, XorbBuilder::new)
                .bind_preparation(preparation.clone());
            let result = stage_file_streaming(
                &path,
                root.path(),
                &staging,
                StreamStageProgress {
                    xorb_builder: Some(builders),
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            staging.finalize_add_preparation(&preparation).unwrap();
            staging.mark_batch_published(&result.batch_id).unwrap();
            pointers.push(Pointer {
                file_hash: result.file_hash,
                size: 65536,
                shard_hint: None,
            });
        }
        staging.close().await.unwrap();
        let staging = Arc::new(StagingAreaReadOnly::open(staging_root).await.unwrap());
        for concurrency in [1, 4] {
            let observer = Arc::new(UploadObserver::default());
            let backend = ThrottledStore::new(
                object_store::memory::InMemory::new(),
                ThrottleConfig {
                    wait_put_per_call: std::time::Duration::from_millis(100),
                    ..Default::default()
                },
            );
            let store =
                crab_storage::Store::new(Arc::new(backend)).with_storage_observer(observer.clone());
            let layout = crab_storage::StoreLayout::new(store, "repo".to_owned());
            let base = crab_write::capsule_protocol::initialize(
                &layout,
                &"1".repeat(64),
                "refs/heads/main",
            )
            .await
            .unwrap();
            observer.peak.store(0, SeqCst);
            let (_, stats) = prepare_delta(
                &layout,
                &base,
                &pointers,
                Some(&staging),
                None,
                None,
                &Default::default(),
                concurrency,
                false,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(
                (stats.xorbs_uploaded, observer.peak.load(SeqCst)),
                (6, concurrency)
            );
        }
    }

    #[tokio::test]
    async fn known_pointers_reject_declared_size_conflicts() {
        let (catalog, pointer) = pointer_catalog(1, 1);
        let (layout, base) = publish_catalog(catalog.encode().unwrap()).await;
        for (pointers, duplicate_conflict) in [
            (
                vec![Pointer {
                    size: pointer.size + 1,
                    ..pointer.clone()
                }],
                false,
            ),
            (
                vec![
                    pointer.clone(),
                    Pointer {
                        size: pointer.size + 1,
                        ..pointer.clone()
                    },
                ],
                true,
            ),
        ] {
            let error = prepare_delta(
                &layout,
                &base,
                &pointers,
                None,
                None,
                None,
                &Default::default(),
                4,
                true,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert!(matches!(
                (error, duplicate_conflict),
                (CrabError::StagingCorrupt(_), true) | (CrabError::CorruptObject { .. }, false)
            ));
        }
    }

    #[tokio::test]
    async fn corrupted_catalog_pages_are_rejected_before_publication() {
        let (catalog, _) = pointer_catalog(1, 1);
        let encoded = catalog.encode().unwrap();
        for truncate in [false, true] {
            let mut bytes = encoded.to_vec();
            if truncate {
                bytes.pop();
            } else {
                bytes[22] ^= 1;
            }
            let (layout, base, transaction, capsule) =
                catalog_publication(Bytes::from(bytes)).await;
            let expected_root = base.record().digest().to_owned();
            let error =
                crab_write::capsule_protocol::publish(&layout, base, &transaction, &capsule)
                    .await
                    .unwrap_err();
            assert!(matches!(error, crab_write::WriteError::Metadata(
                crab_metadata::error::MetadataError::CorruptObject { path, .. })
                if path == "capsule-protocol pointer catalog"));
            // Reject before a ref becomes visible: root identity alone cannot
            // prove this because per-ref heads publish independently of the root.
            let root = crab_metadata::capsule_protocol::load_root(&layout)
                .await
                .unwrap();
            let refs = crab_read::capsule_protocol::read_visible_refs(&layout)
                .await
                .unwrap();
            assert_eq!(
                (root.record().digest(), refs.is_empty()),
                (expected_root.as_str(), true)
            );
        }
    }

    #[tokio::test]
    async fn unknown_pointers_still_require_staged_content() {
        let (catalog, pointer) = pointer_catalog(1, 1);
        let (layout, base) = publish_catalog(catalog.encode().unwrap()).await;
        let error = prepare_delta(
            &layout,
            &base,
            &[Pointer {
                file_hash: [9; 32],
                ..pointer
            }],
            None,
            None,
            None,
            &Default::default(),
            4,
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            CrabError::PointerMissingStaging {
                total: 1,
                missing: 1,
                ..
            }
        ));
    }

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
    async fn protected_xorb_is_verified_and_recorded_without_canonical_write() {
        let candidate = build_xorb(b"protected file contents", CompressionScheme::None);
        let bytes = candidate.bytes.clone();
        let canonical_store =
            crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(
            canonical_store
                .clone()
                .with_staging_writes("repo/staging/session".to_owned()),
            "repo".to_owned(),
        );
        let path = layout.xorb_path(&candidate.hash);
        let entry = catalog_xorb_entry(
            bytes.len() as u64,
            blake3::hash(&bytes).to_hex().to_string(),
            &candidate.placements,
        )
        .unwrap();
        publish_xorb_candidate(
            &layout,
            None,
            candidate.hash,
            candidate.bytes,
            entry,
            candidate.placements,
        )
        .await
        .unwrap();
        let writes = layout.store().flush_staged_writes(1).await.unwrap();
        let staged = object_store::path::Path::from(writes[0].staged_key.clone());
        let (stored, _) = canonical_store
            .get_with_etag_bounded(&staged, MAX_XORB_SIZE as u64)
            .await
            .unwrap();
        let absent = matches!(
            canonical_store
                .get_with_etag_bounded(&path, MAX_XORB_SIZE as u64)
                .await,
            Err(crab_storage::StorageError::NotFound { .. })
        );
        assert_eq!(
            (
                writes.len(),
                writes[0].canonical_key.as_str(),
                stored,
                absent
            ),
            (1, path.as_ref(), bytes, true)
        );
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

        for staged in [false, true] {
            let store = if staged {
                layout
                    .store()
                    .clone()
                    .with_staging_writes("repo/staging/session".to_owned())
            } else {
                layout.store().clone()
            };
            let layout = crab_storage::StoreLayout::new(store, "repo".to_owned());
            let (entry, placements, created, _origin) = publish_xorb_candidate(
                &layout,
                None,
                candidate.hash,
                candidate.bytes.clone(),
                candidate_entry.clone(),
                candidate.placements.clone(),
            )
            .await
            .expect("adopt logical xorb conflict");
            assert_eq!(
                (
                    created,
                    entry.encoded_size(),
                    entry.body_digest(),
                    placements.len(),
                    layout.store().staged_writes().len()
                ),
                (
                    false,
                    existing.bytes.len() as u64,
                    blake3::hash(&existing.bytes).to_hex().as_str(),
                    existing.placements.len(),
                    0
                ),
            );
        }
    }
}
