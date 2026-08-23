//! Online reconciliation for concurrent pushes during restripe.
//!
//! A restripe writes destination xorbs before it can know which file versions
//! are still current. This module turns the completed journal mapping into a
//! new immutable shard snapshot, generation-pins the file-index acceleration
//! rows to that snapshot, and then publishes the snapshot through the
//! repository manifest CAS. The manifest CAS is the visibility boundary:
//! readers anchored to the previous generation continue to use the old shard
//! set, while rows written for a failed attempt are ignored by their anchor
//! validation.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::metadata::manifest;
use crate::restripe::journal::{RestripeJournal, SourceStatus};
use crate::storage::StoreLayout;
use crate::storage::store::Store;

use crab_staging::recipe::{ChunkingPolicyId, FileRecipe};
use crab_xet::shard::{
    FileDataSequenceEntry, MDBFileInfo, MDBXorbInfo, ShardReader, ShardWriter,
    XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use crab_xet::shard_parse::extract_file_recipes;
use crab_xet::xorb::format::{MerkleHash, XorbRef};
use crab_xet::xorb::parser::XorbParser;

const MAX_CAS_ATTEMPTS: u32 = 8;

// ---------------------------------------------------------------------------
// Reconciliation outcome
// ---------------------------------------------------------------------------

/// Outcome of the reconciliation step.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ReconcileOutcome {
    /// Number of source xorbs that were rewritten to dest xorbs.
    pub entries_updated: u64,
    /// Number of source xorbs that were skipped or corrupt (unchanged).
    pub entries_unchanged: u64,
    /// Number of shards uploaded during reconciliation.
    pub shards_uploaded: u64,
    /// Total bytes uploaded for reconciliation shards.
    pub shard_bytes: u64,
    /// Whether the shard-list CAS succeeded on the first attempt.
    pub cas_first_attempt: bool,
    /// Total CAS attempts needed for the shard-list update.
    pub cas_attempts: u32,
}

// ---------------------------------------------------------------------------
// Source-to-dest mapping
// ---------------------------------------------------------------------------

/// Build the `src_xorb → dest_xorbs` mapping from completed journal entries.
fn build_mapping(
    journal: &RestripeJournal,
    run_id: &str,
) -> Result<(HashMap<String, Vec<String>>, u64, u64)> {
    let done_sources = journal.sources_by_status(run_id, SourceStatus::Done)?;
    let counts = journal.count_by_status(run_id)?;

    let mut src_to_dest: HashMap<String, Vec<String>> = HashMap::new();
    let mut entries_updated: u64 = 0;

    for source in &done_sources {
        if let Some(ref dest_json) = source.dest_xorbs {
            let dests: Vec<String> =
                serde_json::from_str(dest_json).map_err(|error| CrabError::CorruptObject {
                    path: format!("restripe journal source {}", source.src_xorb),
                    reason: format!("invalid destination xorb list: {error}"),
                })?;
            if !dests.is_empty() {
                entries_updated += 1;
                src_to_dest.insert(source.src_xorb.clone(), dests);
            }
        }
    }

    let entries_unchanged = counts.skipped + counts.corrupt;

    Ok((src_to_dest, entries_updated, entries_unchanged))
}

#[derive(Debug, Clone, Copy)]
struct SourceChunk {
    hash: MerkleHash,
    size: u32,
}

#[derive(Debug, Clone)]
struct SourcePlacement {
    chunks: Vec<SourceChunk>,
    refs: HashMap<MerkleHash, XorbRef>,
}

#[derive(Debug, Default)]
struct LoadedMapping {
    sources: HashMap<MerkleHash, SourcePlacement>,
    destination_infos: HashMap<MerkleHash, Arc<MDBXorbInfo>>,
}

/// Parse a journal hash and retain the error as a corrupt-object report.
fn parse_hash(value: &str, path: &str) -> Result<MerkleHash> {
    MerkleHash::from_hex(value).map_err(|error| CrabError::CorruptObject {
        path: path.to_owned(),
        reason: format!("invalid Merkle hash {value}: {error}"),
    })
}

/// Read and validate one xorb before using its chunk metadata in a shard.
async fn load_xorb(store: &Store, router: &StoreLayout, hash: MerkleHash) -> Result<XorbParser> {
    let path = router.xorb_path(&hash);
    let (bytes, _) = store.get_with_etag(&path).await?;
    let parser = XorbParser::parse(bytes).map_err(CrabError::from)?;
    if parser.hash() != hash {
        return Err(CrabError::CorruptObject {
            path: path.to_string(),
            reason: format!("xorb content hash is {}, expected {}", parser.hash(), hash),
        });
    }
    parser.verify_payload_digest().map_err(CrabError::from)?;
    parser.verify_all_chunks().map_err(CrabError::from)?;
    Ok(parser)
}

fn source_chunks(parser: &XorbParser, path: &str) -> Result<Vec<SourceChunk>> {
    let mut chunks = Vec::with_capacity(parser.num_chunks() as usize);
    for index in 0..parser.num_chunks() {
        let chunk = parser
            .chunk_meta(index)
            .map_err(|error| CrabError::CorruptObject {
                path: path.to_owned(),
                reason: error.to_string(),
            })?;
        chunks.push(SourceChunk {
            hash: chunk.hash,
            size: chunk.uncompressed_len,
        });
    }
    Ok(chunks)
}

/// Convert a parsed xorb's ordered chunk metadata to xet-core shard metadata.
fn xorb_info(hash: MerkleHash, parser: &XorbParser, path: &str) -> Result<Arc<MDBXorbInfo>> {
    let mut chunks = Vec::with_capacity(parser.num_chunks() as usize);
    let mut uncompressed_offset = 0u64;

    for index in 0..parser.num_chunks() {
        let chunk = parser.chunk_meta(index).map_err(CrabError::from)?;
        let offset = u32::try_from(uncompressed_offset).map_err(|_| CrabError::CorruptObject {
            path: path.to_owned(),
            reason: "uncompressed xorb offset exceeds the shard format".to_owned(),
        })?;
        chunks.push(XorbChunkSequenceEntry::new(
            chunk.hash,
            chunk.uncompressed_len,
            offset,
        ));
        uncompressed_offset = uncompressed_offset
            .checked_add(u64::from(chunk.uncompressed_len))
            .ok_or_else(|| CrabError::CorruptObject {
                path: path.to_owned(),
                reason: "uncompressed xorb size overflows".to_owned(),
            })?;
    }

    let num_entries = u32::try_from(chunks.len()).map_err(|_| CrabError::CorruptObject {
        path: path.to_owned(),
        reason: "xorb chunk count exceeds the shard format".to_owned(),
    })?;
    let num_bytes = u32::try_from(uncompressed_offset).map_err(|_| CrabError::CorruptObject {
        path: path.to_owned(),
        reason: "uncompressed xorb size exceeds the shard format".to_owned(),
    })?;

    Ok(Arc::new(MDBXorbInfo {
        metadata: XorbChunkSequenceHeader::new(hash, num_entries, num_bytes),
        chunks,
    }))
}

/// Load source chunk sequences and the destination xorb metadata referenced by
/// a completed journal. This is done once; immutable xorb objects do not need
/// to be reread after a manifest CAS conflict.
async fn load_mapping(
    store: &Store,
    router: &StoreLayout,
    mapping: &HashMap<String, Vec<String>>,
    cancel: &CancellationToken,
) -> Result<LoadedMapping> {
    let mut loaded = LoadedMapping::default();

    for (source_text, destination_texts) in mapping {
        check_cancelled(cancel)?;
        let source_hash = parse_hash(source_text, "restripe journal source")?;
        let source_path = router.xorb_path(&source_hash).to_string();
        let source_parser = load_xorb(store, router, source_hash).await?;
        let chunks = source_chunks(&source_parser, &source_path)?;
        let source_hashes: HashSet<MerkleHash> = chunks.iter().map(|chunk| chunk.hash).collect();
        let mut refs = HashMap::new();

        for destination_text in destination_texts {
            check_cancelled(cancel)?;
            let destination_hash = parse_hash(destination_text, "restripe journal destination")?;
            let info = if let Some(info) = loaded.destination_infos.get(&destination_hash) {
                Arc::clone(info)
            } else {
                let destination_path = router.xorb_path(&destination_hash).to_string();
                let destination_parser = load_xorb(store, router, destination_hash).await?;
                let info = xorb_info(destination_hash, &destination_parser, &destination_path)?;
                loaded
                    .destination_infos
                    .insert(destination_hash, Arc::clone(&info));
                info
            };

            for (index, chunk) in info.chunks.iter().enumerate() {
                if !source_hashes.contains(&chunk.chunk_hash) {
                    return Err(CrabError::CorruptObject {
                        path: router.xorb_path(&destination_hash).to_string(),
                        reason: format!(
                            "destination xorb contains chunk {} absent from source {}",
                            chunk.chunk_hash, source_hash
                        ),
                    });
                }
                let chunk_index = u32::try_from(index).map_err(|_| CrabError::CorruptObject {
                    path: router.xorb_path(&destination_hash).to_string(),
                    reason: "destination chunk index exceeds the shard format".to_owned(),
                })?;
                let destination_ref = XorbRef {
                    xorb_hash: destination_hash,
                    chunk_index,
                    uncompressed_size: chunk.unpacked_segment_bytes,
                };
                if let Some(previous) = refs.insert(chunk.chunk_hash, destination_ref)
                    && previous != destination_ref
                {
                    return Err(CrabError::CorruptObject {
                        path: router.xorb_path(&destination_hash).to_string(),
                        reason: format!(
                            "source {} maps chunk {} to multiple destination locations",
                            source_hash, chunk.chunk_hash
                        ),
                    });
                }
            }
        }

        for chunk in &chunks {
            let destination_ref =
                refs.get(&chunk.hash)
                    .ok_or_else(|| CrabError::CorruptObject {
                        path: source_path.clone(),
                        reason: format!("source chunk {} has no destination placement", chunk.hash),
                    })?;
            if destination_ref.uncompressed_size != chunk.size {
                return Err(CrabError::CorruptObject {
                    path: source_path.clone(),
                    reason: format!(
                        "chunk {} changes size from {} to {} during restripe",
                        chunk.hash, chunk.size, destination_ref.uncompressed_size
                    ),
                });
            }
        }

        loaded
            .sources
            .insert(source_hash, SourcePlacement { chunks, refs });
    }

    Ok(loaded)
}

// ---------------------------------------------------------------------------
// Shard rewriting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FileIndexEntry {
    file_hash: MerkleHash,
    recipe_hash: [u8; 32],
    shard_hash: MerkleHash,
}

#[derive(Debug)]
struct ShardRewrite {
    new_hash: MerkleHash,
    bytes: Bytes,
    file_entries: Vec<FileIndexEntry>,
}

#[derive(Debug, Default)]
struct ReconcilePlan {
    replacements: Vec<ShardRewrite>,
    final_shards: Vec<MerkleHash>,
    file_entries: Vec<FileIndexEntry>,
}

fn corrupt_shard(reason: impl Into<String>) -> CrabError {
    CrabError::CorruptObject {
        path: "restripe shard reconciliation".to_owned(),
        reason: reason.into(),
    }
}

fn append_destination_segment(
    segments: &mut Vec<FileDataSequenceEntry>,
    destination: XorbRef,
) -> Result<()> {
    let chunk_index_end = destination
        .chunk_index
        .checked_add(1)
        .ok_or_else(|| corrupt_shard("destination chunk index overflows"))?;

    if let Some(previous) = segments.last_mut()
        && previous.xorb_hash == destination.xorb_hash
        && previous.xorb_flags == 0
        && previous.chunk_index_end == destination.chunk_index
    {
        previous.unpacked_segment_bytes = previous
            .unpacked_segment_bytes
            .checked_add(destination.uncompressed_size)
            .ok_or_else(|| corrupt_shard("file segment byte count overflows"))?;
        previous.chunk_index_end = chunk_index_end;
        return Ok(());
    }

    segments.push(FileDataSequenceEntry::new(
        destination.xorb_hash,
        destination.uncompressed_size,
        destination.chunk_index,
        chunk_index_end,
    ));
    Ok(())
}

fn rewrite_file_info(file: &MDBFileInfo, mapping: &LoadedMapping) -> Result<(MDBFileInfo, bool)> {
    let mut segments = Vec::new();

    for segment in &file.segments {
        let Some(source) = mapping.sources.get(&segment.xorb_hash) else {
            segments.push(segment.clone());
            continue;
        };

        let start = usize::try_from(segment.chunk_index_start)
            .map_err(|_| corrupt_shard("source chunk range start cannot be represented"))?;
        let end = usize::try_from(segment.chunk_index_end)
            .map_err(|_| corrupt_shard("source chunk range end cannot be represented"))?;
        let source_chunks = source
            .chunks
            .get(start..end)
            .ok_or_else(|| corrupt_shard("source file segment exceeds xorb bounds"))?;
        let source_bytes = source_chunks.iter().try_fold(0u64, |total, chunk| {
            total
                .checked_add(u64::from(chunk.size))
                .ok_or_else(|| corrupt_shard("source file segment byte count overflows"))
        })?;
        if source_bytes != u64::from(segment.unpacked_segment_bytes) {
            return Err(corrupt_shard(format!(
                "source file segment covers {source_bytes} bytes, expected {}",
                segment.unpacked_segment_bytes
            )));
        }

        for chunk in source_chunks {
            let destination = source.refs.get(&chunk.hash).ok_or_else(|| {
                corrupt_shard(format!(
                    "source chunk {} has no destination placement",
                    chunk.hash
                ))
            })?;
            append_destination_segment(&mut segments, *destination)?;
        }
    }

    if segments == file.segments {
        return Ok((file.clone(), false));
    }

    if file.metadata.contains_verification() {
        return Err(CrabError::Configuration {
            key: "restripe reconciliation".to_owned(),
            origin: format!(
                "cannot rewrite verification-bearing MDBFileInfo for {} without preserving its per-segment proofs",
                file.metadata.file_hash
            ),
        });
    }

    let mut rewritten = file.clone();
    rewritten.metadata.num_entries = u32::try_from(segments.len())
        .map_err(|_| corrupt_shard("rewritten file segment count exceeds the shard format"))?;
    rewritten.segments = segments;
    Ok((rewritten, true))
}

fn recipe_hashes(body: &Bytes) -> Result<HashMap<MerkleHash, [u8; 32]>> {
    let recipes = extract_file_recipes(body).map_err(CrabError::from)?;
    recipes
        .into_iter()
        .map(|recipe| {
            let file_size = recipe.chunks.iter().try_fold(0u64, |total, (_, size)| {
                total
                    .checked_add(*size)
                    .ok_or_else(|| corrupt_shard("file recipe size overflows"))
            })?;
            let hash = FileRecipe::from_staged_chunks(
                ChunkingPolicyId::XetGearV1_64KiB,
                recipe.file_hash,
                file_size,
                &recipe.chunks,
            )?
            .hash();
            Ok((recipe.file_hash, hash))
        })
        .collect()
}

fn rewrite_shard(
    body: &Bytes,
    old_hash: MerkleHash,
    mapping: &LoadedMapping,
) -> Result<Option<ShardRewrite>> {
    let reader = ShardReader::from_bytes(body.clone(), old_hash);
    let shard_data = reader.v1_data();
    let shard_info = reader.shard_info_public().map_err(CrabError::from)?;
    let mut cursor = Cursor::new(shard_data);
    let files = shard_info
        .read_all_file_info_sections(&mut cursor)
        .map_err(|error| corrupt_shard(format!("read file-info section: {error}")))?;
    let original_xorbs = shard_info
        .read_all_xorb_blocks_full(&mut cursor)
        .map_err(|error| corrupt_shard(format!("read xorb-info section: {error}")))?;
    if files.is_empty() {
        return Ok(None);
    }

    let recipes = recipe_hashes(body)?;
    let original_xorbs: HashMap<MerkleHash, Arc<MDBXorbInfo>> = original_xorbs
        .into_iter()
        .map(|info| (info.metadata.xorb_hash, Arc::new(info)))
        .collect();

    let mut rewritten_files = Vec::with_capacity(files.len());
    let mut files_changed = false;
    let mut file_entries = Vec::with_capacity(files.len());
    for file in &files {
        let (rewritten, changed) = rewrite_file_info(file, mapping)?;
        files_changed |= changed;
        let recipe_hash = recipes
            .get(&file.metadata.file_hash)
            .copied()
            .ok_or_else(|| {
                corrupt_shard(format!("missing recipe for {}", file.metadata.file_hash))
            })?;
        file_entries.push(FileIndexEntry {
            file_hash: file.metadata.file_hash,
            recipe_hash,
            shard_hash: MerkleHash::default(),
        });
        rewritten_files.push(rewritten);
    }

    let referenced_xorbs: HashSet<MerkleHash> = rewritten_files
        .iter()
        .flat_map(|file| file.segments.iter().map(|segment| segment.xorb_hash))
        .collect();
    let original_xorb_hashes: HashSet<MerkleHash> = original_xorbs.keys().copied().collect();
    let needs_rewrite = files_changed || original_xorb_hashes != referenced_xorbs;
    if !needs_rewrite {
        return Ok(None);
    }

    let mut writer = ShardWriter::new();
    for xorb_hash in &referenced_xorbs {
        let info = mapping
            .destination_infos
            .get(xorb_hash)
            .or_else(|| original_xorbs.get(xorb_hash))
            .ok_or_else(|| {
                corrupt_shard(format!(
                    "rewritten file references xorb {} without shard metadata",
                    xorb_hash
                ))
            })?;
        writer.add_xorb(Arc::clone(info)).map_err(CrabError::from)?;
    }
    for file in rewritten_files {
        writer.add_file(file).map_err(CrabError::from)?;
    }
    let (bytes, new_hash) = writer.finalize().map_err(CrabError::from)?;
    for entry in &mut file_entries {
        entry.shard_hash = new_hash;
    }

    Ok(Some(ShardRewrite {
        new_hash,
        bytes: Bytes::from(bytes),
        file_entries,
    }))
}

async fn read_shard(store: &Store, router: &StoreLayout, hash: MerkleHash) -> Result<Bytes> {
    let path = router.shard_path(&hash);
    let (body, _) = store.get_with_etag(&path).await?;
    let actual_hash = crab_xet::hash::compute_data_hash(&body);
    if actual_hash != hash {
        return Err(CrabError::CorruptObject {
            path: path.to_string(),
            reason: format!("shard content hash is {actual_hash}, expected {hash}"),
        });
    }
    Ok(body)
}

async fn build_plan(
    store: &Store,
    router: &StoreLayout,
    shard_hashes: &[MerkleHash],
    mapping: &LoadedMapping,
    cancel: &CancellationToken,
) -> Result<ReconcilePlan> {
    let mut plan = ReconcilePlan::default();
    let mut replacements_by_old = HashMap::new();
    let mut seen_shards = HashSet::new();

    for &shard_hash in shard_hashes {
        if !seen_shards.insert(shard_hash) {
            continue;
        }
        check_cancelled(cancel)?;
        let body = read_shard(store, router, shard_hash).await?;
        if let Some(rewrite) = rewrite_shard(&body, shard_hash, mapping)? {
            replacements_by_old.insert(shard_hash, rewrite.new_hash);
            plan.file_entries
                .extend(rewrite.file_entries.iter().cloned());
            plan.replacements.push(rewrite);
        }
    }

    let mut final_seen = HashSet::new();
    for &shard_hash in shard_hashes {
        let replacement = replacements_by_old
            .get(&shard_hash)
            .copied()
            .unwrap_or(shard_hash);
        if final_seen.insert(replacement) {
            plan.final_shards.push(replacement);
        }
    }

    let mut entries_by_file = HashMap::new();
    for entry in plan.file_entries.drain(..) {
        if let Some((previous_recipe, _)) =
            entries_by_file.insert(entry.file_hash, (entry.recipe_hash, entry.shard_hash))
            && previous_recipe != entry.recipe_hash
        {
            return Err(corrupt_shard(format!(
                "file {} has conflicting recipes in the canonical shard set",
                entry.file_hash
            )));
        }
    }
    plan.file_entries = entries_by_file
        .into_iter()
        .map(|(file_hash, (recipe_hash, shard_hash))| FileIndexEntry {
            file_hash,
            recipe_hash,
            shard_hash,
        })
        .collect();

    Ok(plan)
}

async fn upload_replacements(
    store: &Store,
    router: &StoreLayout,
    replacements: &[ShardRewrite],
    cancel: &CancellationToken,
) -> Result<(u64, u64)> {
    let mut uploaded = 0;
    let mut bytes = 0;
    for replacement in replacements {
        check_cancelled(cancel)?;
        let path = router.shard_path(&replacement.new_hash);
        store.put(&path, replacement.bytes.clone()).await?;
        crate::cmd::gc::closure::publish(
            store,
            router.global_prefix(),
            &replacement.new_hash,
            replacement.bytes.clone(),
            path.as_ref(),
        )
        .await?;
        let (verified, _) = store.get_with_etag(&path).await?;
        let actual_hash = crab_xet::hash::compute_data_hash(&verified);
        if actual_hash != replacement.new_hash {
            return Err(CrabError::CorruptObject {
                path: path.to_string(),
                reason: format!(
                    "rewritten shard content hash is {actual_hash}, expected {}",
                    replacement.new_hash
                ),
            });
        }
        uploaded += 1;
        bytes += replacement.bytes.len() as u64;
    }
    Ok((uploaded, bytes))
}

async fn publish_file_index(
    store: &Store,
    router: &StoreLayout,
    entries: &[FileIndexEntry],
    generation: u64,
    shard_index_hash: MerkleHash,
) -> Result<()> {
    let committed = entries
        .iter()
        .map(|entry| {
            (
                entry.file_hash,
                crab_metadata::value_codec::CommittedFileRecord {
                    recipe_hash: entry.recipe_hash,
                    shard_hash: entry.shard_hash,
                    committed_generation: generation,
                    shard_index_hash,
                },
            )
        })
        .collect::<Vec<_>>();
    let config = crab_metadata::remote_index::RemoteIndexConfig::for_repo_with_global_prefix(
        router.repo_prefix(),
        router.global_prefix(),
    );
    crab_metadata::remote_index::write_index_entries(
        Arc::clone(store.inner()),
        &config,
        &committed,
        &[],
    )
    .await
    .map_err(CrabError::from)
}

fn now_iso8601() -> String {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::ZERO);
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
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { 9 };
    let year = year + u64::from(month <= 2);
    (year, month, day)
}

// ---------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------

/// Finalize a restripe run by reconciling the file-index and shard manifest.
///
/// Destination xorbs are immutable. For each CAS attempt this function reads
/// the current canonical shard set, rewrites every affected `MDBFileInfo`,
/// uploads the replacement shards and generation-pinned file-index rows, and
/// finally advances the manifest. A concurrent push causes a bounded retry
/// against the new manifest, so files added during the run are included.
pub async fn finalize(
    journal: &RestripeJournal,
    run_id: &str,
    store: Option<&Store>,
    router: Option<&StoreLayout>,
    cancel: &CancellationToken,
) -> Result<ReconcileOutcome> {
    let (src_to_dest, entries_updated, entries_unchanged) = build_mapping(journal, run_id)?;

    debug!(
        updated = entries_updated,
        unchanged = entries_unchanged,
        mappings = src_to_dest.len(),
        "reconciliation mapping built"
    );

    let Some(store) = store else {
        return Ok(ReconcileOutcome {
            entries_updated,
            entries_unchanged,
            shards_uploaded: 0,
            shard_bytes: 0,
            cas_first_attempt: true,
            cas_attempts: 1,
        });
    };
    let router = router.ok_or_else(|| CrabError::Configuration {
        key: "restripe reconciliation".to_owned(),
        origin: "a store layout is required to publish the reconciled metadata".to_owned(),
    })?;
    if src_to_dest.is_empty() {
        return Ok(ReconcileOutcome {
            entries_updated,
            entries_unchanged,
            shards_uploaded: 0,
            shard_bytes: 0,
            cas_first_attempt: true,
            cas_attempts: 1,
        });
    }

    let loaded_mapping = load_mapping(store, router, &src_to_dest, cancel).await?;
    let mut total_uploaded = 0;
    let mut total_bytes = 0;

    for attempt in 1..=MAX_CAS_ATTEMPTS {
        check_cancelled(cancel)?;
        let (manifest_before, etag) = manifest::read_manifest(store, router).await?;
        if manifest_before.shard_index_hash.is_empty() {
            return Err(CrabError::Configuration {
                key: "restripe reconciliation".to_owned(),
                origin: "the repository manifest has no canonical shard index".to_owned(),
            });
        }

        let shard_texts =
            manifest::read_bulk_shard_list(store, router, &manifest_before.shard_index_hash)
                .await?;
        let shard_hashes = shard_texts
            .iter()
            .map(|hash| parse_hash(hash, "manifest shard index"))
            .collect::<Result<Vec<_>>>()?;
        let plan = build_plan(store, router, &shard_hashes, &loaded_mapping, cancel).await?;

        if plan.replacements.is_empty() {
            info!(
                entries_updated,
                entries_unchanged,
                cas_attempts = attempt,
                "restripe reconciliation found no canonical file entries using source xorbs"
            );
            return Ok(ReconcileOutcome {
                entries_updated,
                entries_unchanged,
                shards_uploaded: total_uploaded,
                shard_bytes: total_bytes,
                cas_first_attempt: attempt == 1,
                cas_attempts: attempt,
            });
        }

        let next_generation =
            manifest_before
                .generation
                .checked_add(1)
                .ok_or_else(|| CrabError::Configuration {
                    key: "restripe reconciliation".to_owned(),
                    origin: "manifest generation overflow".to_owned(),
                })?;
        let final_shards = plan
            .final_shards
            .iter()
            .map(MerkleHash::hex)
            .collect::<Vec<_>>();
        let (shard_index_hash_text, _, shard_index_write) =
            manifest::compact_shard_index(next_generation, &final_shards)?;
        manifest::upload_segmented_bulk(
            store,
            router,
            &crab_metadata::manifests::BulkData {
                shard_index: shard_index_write,
                pack_index: crab_metadata::segmented::SegmentWrite::default(),
            },
        )
        .await?;
        let (uploaded, bytes) =
            upload_replacements(store, router, &plan.replacements, cancel).await?;
        total_uploaded += uploaded;
        total_bytes += bytes;

        let shard_index_hash = parse_hash(&shard_index_hash_text, "reconciled shard index")?;
        publish_file_index(
            store,
            router,
            &plan.file_entries,
            next_generation,
            shard_index_hash,
        )
        .await?;
        check_cancelled(cancel)?;

        let mut candidate = manifest_before;
        candidate.generation = next_generation;
        candidate.created_at = now_iso8601();
        candidate.session_id = format!("restripe-{run_id}");
        candidate.shard_index_hash = shard_index_hash_text;
        candidate.seal_git_validation();

        match manifest::write_manifest_cas(store, router, &candidate, &etag).await {
            Ok(_) => {
                info!(
                    entries_updated,
                    entries_unchanged,
                    shards_uploaded = total_uploaded,
                    shard_bytes = total_bytes,
                    cas_attempts = attempt,
                    "restripe reconciliation complete"
                );
                return Ok(ReconcileOutcome {
                    entries_updated,
                    entries_unchanged,
                    shards_uploaded: total_uploaded,
                    shard_bytes: total_bytes,
                    cas_first_attempt: attempt == 1,
                    cas_attempts: attempt,
                });
            }
            Err(CrabError::CasConflict { .. }) if attempt < MAX_CAS_ATTEMPTS => {
                debug!(
                    attempt,
                    "manifest changed during restripe reconciliation; retrying"
                );
            }
            Err(error) => return Err(error),
        }
    }

    Err(CrabError::CasConflict {
        path: router.manifest_path().to_string(),
        expected_etag: None,
    })
}

/// Check that a CAS repeat is a no-op (idempotency).
pub fn is_cas_repeat_noop(first_outcome: &ReconcileOutcome) -> bool {
    first_outcome.cas_first_attempt
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> MerkleHash {
        MerkleHash::from([seed; 32])
    }

    fn xorb_info(xorb_hash: MerkleHash, chunks: &[(MerkleHash, u32)]) -> Arc<MDBXorbInfo> {
        let mut offset = 0;
        let entries = chunks
            .iter()
            .map(|(chunk_hash, size)| {
                let entry = XorbChunkSequenceEntry::new(*chunk_hash, *size, offset);
                offset += *size;
                entry
            })
            .collect::<Vec<_>>();
        Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb_hash, entries.len(), offset),
            chunks: entries,
        })
    }

    fn file_info(file_hash: MerkleHash, xorb_hash: MerkleHash, size: u32) -> MDBFileInfo {
        MDBFileInfo {
            metadata: crab_xet::shard::FileDataSequenceHeader::new(file_hash, 1, false, false),
            segments: vec![FileDataSequenceEntry::new(xorb_hash, size, 0, 1)],
            verification: Vec::new(),
            metadata_ext: None,
        }
    }

    #[test]
    fn cas_repeat_noop_when_first_succeeded() {
        let outcome = ReconcileOutcome {
            entries_updated: 10,
            entries_unchanged: 5,
            shards_uploaded: 1,
            shard_bytes: 256,
            cas_first_attempt: true,
            cas_attempts: 1,
        };
        assert!(is_cas_repeat_noop(&outcome));
    }

    #[test]
    fn cas_repeat_not_noop_when_first_failed() {
        let outcome = ReconcileOutcome {
            entries_updated: 10,
            entries_unchanged: 5,
            shards_uploaded: 1,
            shard_bytes: 256,
            cas_first_attempt: false,
            cas_attempts: 3,
        };
        assert!(!is_cas_repeat_noop(&outcome));
    }

    #[tokio::test]
    async fn finalize_without_store_reports_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = RestripeJournal::open(&path).unwrap();

        journal.start_run("test-reconcile", "{}").unwrap();
        journal.insert_source("test-reconcile", "xorb-001").unwrap();
        journal.insert_source("test-reconcile", "xorb-002").unwrap();
        journal.insert_source("test-reconcile", "xorb-003").unwrap();

        journal
            .update_source_status(
                "test-reconcile",
                "xorb-001",
                SourceStatus::Done,
                Some(r#"["dest-001","dest-002"]"#),
            )
            .unwrap();
        journal
            .update_source_status(
                "test-reconcile",
                "xorb-002",
                SourceStatus::Done,
                Some(r#"["dest-003"]"#),
            )
            .unwrap();
        journal
            .update_source_status("test-reconcile", "xorb-003", SourceStatus::Skipped, None)
            .unwrap();

        let outcome = finalize(
            &journal,
            "test-reconcile",
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.entries_updated, 2);
        assert_eq!(outcome.entries_unchanged, 1);
        assert_eq!(outcome.shards_uploaded, 0);
        assert!(outcome.cas_first_attempt);
    }

    #[tokio::test]
    async fn finalize_with_empty_dest_lists_counts_zero_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = RestripeJournal::open(&path).unwrap();

        journal.start_run("test-empty", "{}").unwrap();
        journal.insert_source("test-empty", "xorb-aaa").unwrap();
        journal
            .update_source_status("test-empty", "xorb-aaa", SourceStatus::Done, Some("[]"))
            .unwrap();

        let outcome = finalize(
            &journal,
            "test-empty",
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.entries_updated, 0);
        assert_eq!(outcome.entries_unchanged, 0);
        assert_eq!(outcome.shards_uploaded, 0);
    }

    #[test]
    fn build_mapping_extracts_src_to_dest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = RestripeJournal::open(&path).unwrap();

        journal.start_run("map-test", "{}").unwrap();
        journal.insert_source("map-test", "src-a").unwrap();
        journal.insert_source("map-test", "src-b").unwrap();
        journal.insert_source("map-test", "src-c").unwrap();

        journal
            .update_source_status(
                "map-test",
                "src-a",
                SourceStatus::Done,
                Some(r#"["d1","d2"]"#),
            )
            .unwrap();
        journal
            .update_source_status("map-test", "src-b", SourceStatus::Done, Some("[]"))
            .unwrap();
        journal
            .mark_corrupt("map-test", "src-c", "hash", "bad")
            .unwrap();

        let (mapping, updated, unchanged) = build_mapping(&journal, "map-test").unwrap();

        assert_eq!(mapping.len(), 1);
        assert_eq!(mapping["src-a"], vec!["d1", "d2"]);
        assert_eq!(updated, 1);
        assert_eq!(unchanged, 1);
    }

    #[test]
    fn rewrite_coalesces_deduplicated_destination_chunks() {
        let source_hash = hash(1);
        let destination_hash = hash(2);
        let chunk_a = hash(3);
        let chunk_b = hash(4);
        let source = SourcePlacement {
            chunks: vec![
                SourceChunk {
                    hash: chunk_a,
                    size: 4,
                },
                SourceChunk {
                    hash: chunk_b,
                    size: 8,
                },
                SourceChunk {
                    hash: chunk_a,
                    size: 4,
                },
            ],
            refs: HashMap::from([
                (
                    chunk_a,
                    XorbRef {
                        xorb_hash: destination_hash,
                        chunk_index: 0,
                        uncompressed_size: 4,
                    },
                ),
                (
                    chunk_b,
                    XorbRef {
                        xorb_hash: destination_hash,
                        chunk_index: 1,
                        uncompressed_size: 8,
                    },
                ),
            ]),
        };
        let mapping = LoadedMapping {
            sources: HashMap::from([(source_hash, source)]),
            destination_infos: HashMap::new(),
        };
        let file = MDBFileInfo {
            metadata: crab_xet::shard::FileDataSequenceHeader::new(hash(5), 1, false, false),
            segments: vec![FileDataSequenceEntry::new(source_hash, 16, 0, 3)],
            verification: Vec::new(),
            metadata_ext: None,
        };

        let (rewritten, changed) = rewrite_file_info(&file, &mapping).unwrap();

        assert!(changed);
        assert_eq!(rewritten.metadata.num_entries, 2);
        assert_eq!(rewritten.segments[0].xorb_hash, destination_hash);
        assert_eq!(rewritten.segments[0].chunk_index_start, 0);
        assert_eq!(rewritten.segments[0].chunk_index_end, 2);
        assert_eq!(rewritten.segments[0].unpacked_segment_bytes, 12);
        assert_eq!(rewritten.segments[1].chunk_index_start, 0);
        assert_eq!(rewritten.segments[1].chunk_index_end, 1);
        assert_eq!(rewritten.segments[1].unpacked_segment_bytes, 4);
    }

    #[test]
    fn rewritten_shard_drops_source_xorb_metadata() {
        let source_hash = hash(10);
        let destination_hash = hash(11);
        let chunk_hash = hash(12);
        let file_hash = hash(13);
        let source_info = xorb_info(source_hash, &[(chunk_hash, 16)]);
        let destination_info = xorb_info(destination_hash, &[(chunk_hash, 16)]);
        let mut writer = ShardWriter::new();
        writer.add_xorb(source_info).unwrap();
        writer
            .add_file(file_info(file_hash, source_hash, 16))
            .unwrap();
        let (body, old_hash) = writer.finalize().unwrap();

        let mapping = LoadedMapping {
            sources: HashMap::from([(
                source_hash,
                SourcePlacement {
                    chunks: vec![SourceChunk {
                        hash: chunk_hash,
                        size: 16,
                    }],
                    refs: HashMap::from([(
                        chunk_hash,
                        XorbRef {
                            xorb_hash: destination_hash,
                            chunk_index: 0,
                            uncompressed_size: 16,
                        },
                    )]),
                },
            )]),
            destination_infos: HashMap::from([(destination_hash, destination_info)]),
        };

        let rewrite = rewrite_shard(&Bytes::from(body), old_hash, &mapping)
            .unwrap()
            .unwrap();
        let reader = ShardReader::from_bytes(rewrite.bytes, rewrite.new_hash);

        assert!(reader.get_xorb_info(&source_hash).unwrap().is_none());
        assert!(reader.get_xorb_info(&destination_hash).unwrap().is_some());
        assert_eq!(
            reader.get_file_info(&file_hash).unwrap().unwrap().segments[0].xorb_hash,
            destination_hash
        );
    }
}
