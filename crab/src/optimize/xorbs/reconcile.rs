//! Online reconciliation for concurrent pushes during xorb optimization.
//!
//! Xorb optimization writes destination xorbs before it can know which file versions
//! are still current. This module turns the completed journal mapping into a
//! new immutable shard snapshot. Capsule repositories publish the complete
//! verified pointer catalog through an exact-root checkpoint CAS. Legacy
//! repositories generation-pin file-index acceleration rows and publish the
//! snapshot through the manifest CAS.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::metadata::manifest;
use crate::optimize::xorbs::journal::{OptimizeXorbsJournal, SourceStatus};
use crate::storage::StoreLayout;
use crate::storage::store::Store;

use crab_staging::recipe::{ChunkingPolicyId, FileRecipe};
use crab_xet::hash::HashedWrite;
use crab_xet::shard::{
    FileDataSequenceEntry, MDBFileInfo, MDBXorbInfo, ShardReader, ShardWriter,
    XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use crab_xet::shard_parse::extract_file_recipes;
use crab_xet::xorb::format::{MAX_XORB_SIZE, MerkleHash, XorbRef};
use crab_xet::xorb::parser::XorbParser;

const MAX_CAS_ATTEMPTS: u32 = 8;
const MAX_RECONCILIATION_MAPPING_ENTRIES: u64 = 1_000_000;
const MAX_DESTINATIONS_PER_SOURCE: usize = 1_000_000;
const MAX_DESTINATION_JSON_BYTES: usize = 80 * 1024 * 1024;
const MAX_RECONCILIATION_SHARD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RECONCILIATION_FILE_ENTRIES: usize = 1_000_000;
const MAX_RECONCILIATION_XORB_ENTRIES: usize = 1_000_000;
const MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES: usize = 10_000_000;
const SOURCES_PER_RECONCILIATION_BATCH: usize = 64;
const MAX_CAPSULE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
    journal: &OptimizeXorbsJournal,
    run_id: &str,
) -> Result<(HashMap<String, Vec<String>>, u64, u64)> {
    let counts = journal.count_by_status(run_id)?;

    let mut src_to_dest = HashMap::new();
    let mut entries_updated: u64 = 0;

    let mut after = String::new();
    loop {
        let done_sources = journal.sources_by_status_after(
            run_id,
            SourceStatus::Done,
            Some(&after),
            SOURCES_PER_RECONCILIATION_BATCH,
        )?;
        if done_sources.is_empty() {
            break;
        }
        if let Some(last) = done_sources.last() {
            after.clone_from(&last.src_xorb);
        }
        for source in done_sources {
            let Some(dest_json) = source.dest_xorbs else {
                continue;
            };
            if dest_json.len() > MAX_DESTINATION_JSON_BYTES {
                return Err(CrabError::Configuration {
                    key: "xorb optimization reconciliation destination list".to_owned(),
                    origin: format!(
                        "journal destination list for {} exceeds the bounded reconciliation size",
                        source.src_xorb
                    ),
                });
            }
            let dests: Vec<String> =
                serde_json::from_str(&dest_json).map_err(|error| CrabError::CorruptObject {
                    path: format!("xorb optimization journal source {}", source.src_xorb),
                    reason: format!("invalid destination xorb list: {error}"),
                })?;
            if dests.len() > MAX_DESTINATIONS_PER_SOURCE {
                return Err(CrabError::Configuration {
                    key: "xorb optimization reconciliation destination count".to_owned(),
                    origin: format!(
                        "journal source {} references {} destination xorbs; bounded reconciliation supports at most {MAX_DESTINATIONS_PER_SOURCE}",
                        source.src_xorb,
                        dests.len()
                    ),
                });
            }
            if !dests.is_empty() {
                entries_updated =
                    entries_updated
                        .checked_add(1)
                        .ok_or_else(|| CrabError::Configuration {
                            key: "xorb optimization reconciliation mapping count".to_owned(),
                            origin: "journal mapping count overflow".to_owned(),
                        })?;
                if entries_updated > MAX_RECONCILIATION_MAPPING_ENTRIES {
                    return Err(CrabError::Configuration {
                        key: "xorb optimization reconciliation mapping count".to_owned(),
                        origin: format!(
                            "journal maps more than {MAX_RECONCILIATION_MAPPING_ENTRIES} source xorbs in one reconciliation"
                        ),
                    });
                }
                src_to_dest.insert(source.src_xorb, dests);
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
    source_catalog: HashMap<MerkleHash, crab_metadata::capsule_protocol::XorbCatalogEntry>,
    destination_infos: HashMap<MerkleHash, Arc<MDBXorbInfo>>,
    destination_catalog: HashMap<MerkleHash, crab_metadata::capsule_protocol::XorbCatalogEntry>,
}

/// Parse a journal hash and retain the error as a corrupt-object report.
fn parse_hash(value: &str, path: &str) -> Result<MerkleHash> {
    MerkleHash::from_hex(value).map_err(|error| CrabError::CorruptObject {
        path: path.to_owned(),
        reason: format!("invalid Merkle hash {value}: {error}"),
    })
}

/// Read and validate one xorb before using its chunk metadata in a shard.
async fn load_xorb(
    store: &Store,
    router: &StoreLayout,
    hash: MerkleHash,
) -> Result<(
    XorbParser,
    crab_metadata::capsule_protocol::XorbCatalogEntry,
)> {
    let path = router.xorb_path(&hash);
    let (bytes, _) = store
        .get_with_etag_bounded(&path, MAX_XORB_SIZE as u64)
        .await?;
    let encoded_size = bytes.len() as u64;
    let body_digest = blake3::hash(&bytes).to_hex().to_string();
    let parser = XorbParser::parse(bytes).map_err(CrabError::from)?;
    if parser.hash() != hash {
        return Err(CrabError::CorruptObject {
            path: path.to_string(),
            reason: format!("xorb content hash is {}, expected {}", parser.hash(), hash),
        });
    }
    parser.verify_payload_digest().map_err(CrabError::from)?;
    parser.verify_all_chunks().map_err(CrabError::from)?;
    let chunks = (0..parser.num_chunks())
        .map(|index| {
            let chunk = parser.chunk_meta(index).map_err(CrabError::from)?;
            Ok(crab_metadata::capsule_protocol::XorbChunkEntry::new(
                chunk.hash.hex(),
                chunk.uncompressed_len,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((
        parser,
        crab_metadata::capsule_protocol::XorbCatalogEntry::new(encoded_size, body_digest, chunks),
    ))
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
    let mut loaded_chunk_entries = 0usize;

    for (source_text, destination_texts) in mapping {
        check_cancelled(cancel)?;
        let source_hash = parse_hash(source_text, "xorb optimization journal source")?;
        let source_path = router.xorb_path(&source_hash).to_string();
        let (source_parser, source_catalog) = load_xorb(store, router, source_hash).await?;
        let chunks = source_chunks(&source_parser, &source_path)?;
        loaded_chunk_entries = loaded_chunk_entries
            .checked_add(chunks.len())
            .ok_or_else(|| CrabError::Configuration {
                key: "xorb optimization reconciliation chunk metadata".to_owned(),
                origin: "source chunk metadata count overflows usize".to_owned(),
            })?;
        if loaded_chunk_entries > MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES {
            return Err(CrabError::Configuration {
                key: "xorb optimization reconciliation chunk metadata".to_owned(),
                origin: format!(
                    "loaded source chunk metadata exceeds {MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES} entries"
                ),
            });
        }
        let source_hashes: HashSet<MerkleHash> = chunks.iter().map(|chunk| chunk.hash).collect();
        let mut refs = HashMap::new();

        for destination_text in destination_texts {
            check_cancelled(cancel)?;
            let destination_hash =
                parse_hash(destination_text, "xorb optimization journal destination")?;
            let info = if let Some(info) = loaded.destination_infos.get(&destination_hash) {
                Arc::clone(info)
            } else {
                let destination_path = router.xorb_path(&destination_hash).to_string();
                let (destination_parser, catalog_entry) =
                    load_xorb(store, router, destination_hash).await?;
                let info = xorb_info(destination_hash, &destination_parser, &destination_path)?;
                loaded_chunk_entries = loaded_chunk_entries
                    .checked_add(info.chunks.len())
                    .ok_or_else(|| CrabError::Configuration {
                        key: "xorb optimization reconciliation chunk metadata".to_owned(),
                        origin: "destination chunk metadata count overflows usize".to_owned(),
                    })?;
                if loaded_chunk_entries > MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES {
                    return Err(CrabError::Configuration {
                        key: "xorb optimization reconciliation chunk metadata".to_owned(),
                        origin: format!(
                            "loaded destination chunk metadata exceeds {MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES} entries"
                        ),
                    });
                }
                loaded
                    .destination_infos
                    .insert(destination_hash, Arc::clone(&info));
                loaded
                    .destination_catalog
                    .insert(destination_hash, catalog_entry);
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
                        "chunk {} changes size from {} to {} during xorb optimization",
                        chunk.hash, chunk.size, destination_ref.uncompressed_size
                    ),
                });
            }
        }

        loaded
            .sources
            .insert(source_hash, SourcePlacement { chunks, refs });
        loaded.source_catalog.insert(source_hash, source_catalog);
        if loaded.sources.len() as u64 > MAX_RECONCILIATION_MAPPING_ENTRIES {
            return Err(CrabError::Configuration {
                key: "xorb optimization reconciliation mapping count".to_owned(),
                origin: format!(
                    "reconciliation loaded more than {MAX_RECONCILIATION_MAPPING_ENTRIES} source xorbs"
                ),
            });
        }
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
    xorbs: Vec<MerkleHash>,
}

#[derive(Debug, Default)]
struct ReconcilePlan {
    replacements: Vec<ShardRewrite>,
    replaced_sources: HashSet<MerkleHash>,
    final_shards: Vec<MerkleHash>,
    file_entries: Vec<FileIndexEntry>,
}

fn corrupt_shard(reason: impl Into<String>) -> CrabError {
    CrabError::CorruptObject {
        path: "xorb optimization shard reconciliation".to_owned(),
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
            key: "xorb optimization reconciliation".to_owned(),
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
    selected_files: Option<&HashSet<MerkleHash>>,
) -> Result<Option<ShardRewrite>> {
    let reader = ShardReader::from_bytes(body.clone(), old_hash);
    let shard_data = reader.v1_data();
    let shard_info = reader.shard_info_public().map_err(CrabError::from)?;
    let mut cursor = Cursor::new(shard_data);
    let files = shard_info
        .read_all_file_info_sections(&mut cursor)
        .map_err(|error| corrupt_shard(format!("read file-info section: {error}")))?;
    if files.len() > MAX_RECONCILIATION_FILE_ENTRIES {
        return Err(CrabError::Configuration {
            key: "xorb optimization reconciliation file entries".to_owned(),
            origin: format!(
                "shard contains {} file entries; bounded reconciliation supports at most {MAX_RECONCILIATION_FILE_ENTRIES}",
                files.len()
            ),
        });
    }
    let original_xorbs = shard_info
        .read_all_xorb_blocks_full(&mut cursor)
        .map_err(|error| corrupt_shard(format!("read xorb-info section: {error}")))?;
    if original_xorbs.len() > MAX_RECONCILIATION_XORB_ENTRIES {
        return Err(CrabError::Configuration {
            key: "xorb optimization reconciliation xorb entries".to_owned(),
            origin: format!(
                "shard contains {} xorb entries; bounded reconciliation supports at most {MAX_RECONCILIATION_XORB_ENTRIES}",
                original_xorbs.len()
            ),
        });
    }
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
        if selected_files.is_some_and(|selected| !selected.contains(&file.metadata.file_hash)) {
            files_changed = true;
            continue;
        }
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
    if selected_files.is_some() && rewritten_files.is_empty() {
        return Err(corrupt_shard(format!(
            "authenticated shard {old_hash} contains none of its catalog files"
        )));
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
        xorbs: {
            let mut xorbs = referenced_xorbs.into_iter().collect::<Vec<_>>();
            xorbs.sort_unstable_by_key(MerkleHash::hex);
            xorbs
        },
    }))
}

async fn read_shard(store: &Store, router: &StoreLayout, hash: MerkleHash) -> Result<Bytes> {
    let path = router.shard_path(&hash);
    let (body, _) = store
        .get_with_etag_bounded(&path, MAX_RECONCILIATION_SHARD_BYTES)
        .await?;
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
    selected_files: Option<&HashSet<MerkleHash>>,
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
        if let Some(rewrite) = rewrite_shard(&body, shard_hash, mapping, selected_files)? {
            replacements_by_old.insert(shard_hash, rewrite.new_hash);
            plan.replaced_sources.insert(shard_hash);
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
    publish_gc_closures: bool,
    cancel: &CancellationToken,
) -> Result<(u64, u64)> {
    let workspace = tempfile::tempdir().map_err(CrabError::Io)?;
    let mut uploaded = 0;
    let mut bytes = 0;
    for replacement in replacements {
        check_cancelled(cancel)?;
        let path = router.shard_path(&replacement.new_hash);
        let local_path = workspace
            .path()
            .join(format!("replacement-{}.shard", replacement.new_hash.hex()));
        tokio::fs::write(&local_path, &replacement.bytes)
            .await
            .map_err(CrabError::Io)?;
        let size =
            u64::try_from(replacement.bytes.len()).map_err(|_| CrabError::Configuration {
                key: "xorb optimization replacement size".to_owned(),
                origin: "replacement shard size cannot be represented".to_owned(),
            })?;
        store
            .put_multipart_file_retry_with_xet_hash(
                &path,
                &local_path,
                size,
                replacement.new_hash.into(),
                8 * 1024 * 1024,
                cancel,
                None,
            )
            .await?;
        let mut hasher = HashedWrite::new(std::io::sink());
        let verified_size = tokio::select! {
            result = store.stream_to_writer(&path, &mut hasher) => result?,
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
        };
        if verified_size != size || hasher.hash() != replacement.new_hash {
            return Err(CrabError::CorruptObject {
                path: path.to_string(),
                reason: format!(
                    "rewritten shard verification failed: expected {} bytes and hash {}, got {} bytes and hash {}",
                    size,
                    replacement.new_hash,
                    verified_size,
                    hasher.hash()
                ),
            });
        }
        if publish_gc_closures {
            crate::cmd::gc::closure::publish(
                store,
                router.global_prefix(),
                &replacement.new_hash,
                replacement.bytes.clone(),
                path.as_ref(),
            )
            .await?;
        }
        uploaded += 1;
        bytes += size;
    }
    Ok((uploaded, bytes))
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

fn capsule_selection(
    catalog: &crab_metadata::capsule_protocol::PointerCatalog,
) -> Result<(Vec<MerkleHash>, HashSet<MerkleHash>)> {
    let mut shard_hashes = BTreeSet::new();
    let mut files = HashSet::with_capacity(catalog.files().len());
    for (file_hash, entry) in catalog.files() {
        files.insert(parse_hash(file_hash, "capsule file catalog")?);
        shard_hashes.insert(entry.shard_hash().to_owned());
    }
    let shards = shard_hashes
        .into_iter()
        .map(|hash| {
            if !catalog.shards().contains_key(&hash) {
                return Err(CrabError::CorruptObject {
                    path: "capsule-protocol pointer catalog".to_owned(),
                    reason: format!("file catalog references absent shard {hash}"),
                });
            }
            parse_hash(&hash, "capsule shard catalog")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((shards, files))
}

fn capsule_replacement_catalog(
    current: &crab_metadata::capsule_protocol::PointerCatalog,
    plan: &ReconcilePlan,
    mapping: &LoadedMapping,
) -> Result<crab_metadata::capsule_protocol::PointerCatalog> {
    use crab_metadata::capsule_protocol::{FileCatalogEntry, PointerCatalog, ShardCatalogEntry};

    let file_shards = plan
        .file_entries
        .iter()
        .map(|entry| (entry.file_hash, entry.shard_hash))
        .collect::<HashMap<_, _>>();
    let rewrites = plan
        .replacements
        .iter()
        .map(|replacement| (replacement.new_hash, replacement))
        .collect::<HashMap<_, _>>();

    let mut catalog = PointerCatalog::new();
    let mut required_xorbs = BTreeSet::new();
    for shard_hash in &plan.final_shards {
        if let Some(rewrite) = rewrites.get(shard_hash) {
            let xorb_hashes = rewrite
                .xorbs
                .iter()
                .map(MerkleHash::hex)
                .collect::<Vec<_>>();
            required_xorbs.extend(xorb_hashes.iter().cloned());
            catalog.insert_shard(
                shard_hash.hex(),
                ShardCatalogEntry::new(rewrite.bytes.len() as u64, xorb_hashes),
            )?;
            continue;
        }
        let hash = shard_hash.hex();
        let entry = current
            .shards()
            .get(&hash)
            .ok_or_else(|| CrabError::CorruptObject {
                path: "capsule-protocol pointer catalog".to_owned(),
                reason: format!("canonical shard set references absent shard {hash}"),
            })?;
        required_xorbs.extend(entry.xorb_hashes().iter().cloned());
        catalog.insert_shard(hash, entry.clone())?;
    }

    for xorb_hash in required_xorbs {
        let parsed = parse_hash(&xorb_hash, "capsule xorb catalog")?;
        let current_entry = current.xorbs().get(&xorb_hash);
        let destination_entry = mapping.destination_catalog.get(&parsed);
        if let (Some(current_entry), Some(destination_entry)) = (current_entry, destination_entry)
            && current_entry != destination_entry
        {
            return Err(CrabError::CorruptObject {
                path: format!("capsule xorb catalog {xorb_hash}"),
                reason: "authenticated destination descriptor conflicts with its verified body"
                    .to_owned(),
            });
        }
        let entry =
            destination_entry
                .or(current_entry)
                .ok_or_else(|| CrabError::CorruptObject {
                    path: "capsule-protocol pointer catalog".to_owned(),
                    reason: format!("replacement shard references absent xorb {xorb_hash}"),
                })?;
        catalog.insert_xorb(xorb_hash, entry.clone())?;
    }

    for (file_hash, entry) in current.files() {
        let parsed = parse_hash(file_hash, "capsule file catalog")?;
        let source_shard = parse_hash(entry.shard_hash(), "capsule file shard")?;
        let shard_hash = match file_shards.get(&parsed) {
            Some(hash) => hash.hex(),
            None if plan.replaced_sources.contains(&source_shard) => {
                return Err(CrabError::CorruptObject {
                    path: "xorb optimization replacement catalog".to_owned(),
                    reason: format!("rewritten shard lost authenticated file {file_hash}"),
                });
            }
            None => entry.shard_hash().to_owned(),
        };
        catalog.insert_file(
            file_hash.clone(),
            FileCatalogEntry::new(entry.size(), shard_hash),
        )?;
    }
    catalog.encode()?;
    Ok(catalog)
}

fn verify_capsule_sources(
    catalog: &crab_metadata::capsule_protocol::PointerCatalog,
    mapping: &LoadedMapping,
) -> Result<()> {
    for (hash, actual) in &mapping.source_catalog {
        if let Some(authenticated) = catalog.xorbs().get(&hash.hex())
            && authenticated != actual
        {
            return Err(CrabError::CorruptObject {
                path: format!("capsule xorb catalog {}", hash.hex()),
                reason: "authenticated xorb descriptor does not match its verified body".to_owned(),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------

/// Finalize an xorb optimization run against the repository's authoritative format.
///
/// Destination xorbs are immutable. Each attempt rereads the current canonical
/// shard set, rewrites every affected `MDBFileInfo`, makes the replacement
/// closure durable, and atomically advances either the v2 capsule root or the
/// legacy manifest. Concurrent pushes cause a bounded retry against the newer
/// authority so their file roots are never lost.
pub async fn finalize(
    journal: &OptimizeXorbsJournal,
    run_id: &str,
    store: Option<&Store>,
    router: Option<&StoreLayout>,
    config: &Config,
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
        key: "xorb optimization reconciliation".to_owned(),
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
    let capsule_layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    match crab_metadata::capsule_protocol::load_root(&capsule_layout).await {
        Ok(root) => {
            return finalize_capsule(
                store,
                router,
                &capsule_layout,
                root,
                &loaded_mapping,
                entries_updated,
                entries_unchanged,
                cancel,
            )
            .await;
        }
        Err(crab_metadata::error::MetadataError::Storage {
            source: crab_storage::StorageError::NotFound { .. },
        }) => {}
        Err(error) => return Err(error.into()),
    }

    finalize_legacy(
        store,
        router,
        config,
        run_id,
        &loaded_mapping,
        entries_updated,
        entries_unchanged,
        cancel,
    )
    .await
}

async fn finalize_capsule(
    store: &Store,
    router: &StoreLayout,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    mut root: crab_metadata::capsule_protocol::RootSnapshot,
    mapping: &LoadedMapping,
    entries_updated: u64,
    entries_unchanged: u64,
    cancel: &CancellationToken,
) -> Result<ReconcileOutcome> {
    let mut total_uploaded = 0;
    let mut total_bytes = 0;

    for attempt in 1..=MAX_CAS_ATTEMPTS {
        check_cancelled(cancel)?;
        let view = crab_read::capsule_protocol::open_view_from_root_with_control(
            layout,
            root.clone(),
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: MAX_CAPSULE_BYTES,
                max_frontier_bytes: MAX_CAPSULE_BYTES,
            },
        )
        .await?;
        let current = view.pointer_catalog()?;
        verify_capsule_sources(&current, mapping)?;
        let (shard_hashes, selected_files) = capsule_selection(&current)?;
        let plan = build_plan(
            store,
            router,
            &shard_hashes,
            mapping,
            Some(&selected_files),
            cancel,
        )
        .await?;
        if plan.replacements.is_empty() {
            info!(
                entries_updated,
                entries_unchanged,
                cas_attempts = attempt,
                protocol = "capsule-v2",
                "xorb optimization reconciliation found no canonical file entries using source xorbs"
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

        let replacement = capsule_replacement_catalog(&current, &plan, mapping)?;
        let (uploaded, bytes) =
            upload_replacements(store, router, &plan.replacements, true, cancel).await?;
        total_uploaded += uploaded;
        total_bytes += bytes;
        crab_read::verify_capsule_pointer_catalog_objects(layout, &replacement).await?;
        crab_metadata::ref_registry::union_register_repo_shards(
            layout.store(),
            layout,
            plan.final_shards.iter().map(MerkleHash::hex).collect(),
        )
        .await?;
        let published = crab_remote::checkpoint::publish_capsule_checkpoint_with_catalog_from_view(
            layout,
            &view,
            replacement,
            MAX_CAPSULE_BYTES,
            cancel,
        )
        .await
        .map_err(map_checkpoint_error)?;
        if published {
            info!(
                entries_updated,
                entries_unchanged,
                shards_uploaded = total_uploaded,
                shard_bytes = total_bytes,
                cas_attempts = attempt,
                protocol = "capsule-v2",
                "xorb optimization reconciliation complete"
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
        if attempt < MAX_CAS_ATTEMPTS {
            debug!(
                attempt,
                "capsule root changed during xorb optimization reconciliation; retrying"
            );
            root = crab_metadata::capsule_protocol::load_root(layout).await?;
        }
    }

    Err(CrabError::CasConflict {
        path: layout.capsule_root_path().to_string(),
        expected_etag: None,
    })
}

async fn finalize_legacy(
    store: &Store,
    router: &StoreLayout,
    config: &Config,
    run_id: &str,
    loaded_mapping: &LoadedMapping,
    entries_updated: u64,
    entries_unchanged: u64,
    cancel: &CancellationToken,
) -> Result<ReconcileOutcome> {
    let mut total_uploaded = 0;
    let mut total_bytes = 0;

    for attempt in 1..=MAX_CAS_ATTEMPTS {
        check_cancelled(cancel)?;
        let (manifest_before, etag) = manifest::read_manifest(store, router).await?;
        if manifest_before.shard_index_hash.is_empty() {
            return Err(CrabError::Configuration {
                key: "xorb optimization reconciliation".to_owned(),
                origin: "the repository manifest has no canonical shard index".to_owned(),
            });
        }

        let shard_texts = manifest::read_bulk_shard_list_with_limit(
            store,
            router,
            &manifest_before.shard_index_hash,
            MAX_RECONCILIATION_MAPPING_ENTRIES,
        )
        .await?;
        let shard_hashes = shard_texts
            .iter()
            .map(|hash| parse_hash(hash, "manifest shard index"))
            .collect::<Result<Vec<_>>>()?;
        let plan = build_plan(store, router, &shard_hashes, loaded_mapping, None, cancel).await?;

        if plan.replacements.is_empty() {
            info!(
                entries_updated,
                entries_unchanged,
                cas_attempts = attempt,
                "xorb optimization reconciliation found no canonical file entries using source xorbs"
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
                    key: "xorb optimization reconciliation".to_owned(),
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
            upload_replacements(store, router, &plan.replacements, false, cancel).await?;
        total_uploaded += uploaded;
        total_bytes += bytes;

        let mut candidate = manifest_before;
        candidate.generation = next_generation;
        candidate.created_at = now_iso8601();
        candidate.session_id = format!("optimize-xorbs-{run_id}");
        candidate.shard_index_hash = shard_index_hash_text;
        candidate.seal_git_validation();
        // Candidate rows and roots are durable before the manifest CAS. A
        // cancelled or conflicting attempt may leave extra immutable objects,
        // but it must never expose a generation with missing index evidence.
        let publication = crate::cmd::metadb::publish_candidate_shard_indexes(
            store,
            router.repo_prefix(),
            &candidate,
            &final_shards,
            config,
            cancel,
        )
        .await?;
        debug!(
            gc_registry_generation = publication.gc_registry_generation,
            file_entries = publication.file_entries_written,
            chunk_entries = publication.chunk_entries_written,
            "candidate xorb indexes published"
        );
        check_cancelled(cancel)?;

        match manifest::write_manifest_cas(store, router, &candidate, &etag).await {
            Ok(_) => {
                // The manifest CAS is the visibility point. Reconcile only
                // the exact source roots replaced by this commit and finish
                // the registry update even when cancellation arrived during
                // the CAS request.
                let storage_router = crab_storage::StoreLayout::new(
                    store.as_storage().clone(),
                    router.repo_prefix().to_owned(),
                );
                crab_metadata::ref_registry::reconcile_compacted_repo_shards(
                    store.as_storage(),
                    &storage_router,
                    plan.replaced_sources.iter().map(MerkleHash::hex).collect(),
                    plan.final_shards.iter().map(MerkleHash::hex).collect(),
                )
                .await?;
                crate::cmd::metadb::write_generation_index_receipt(store, router, &candidate)
                    .await?;
                info!(
                    entries_updated,
                    entries_unchanged,
                    shards_uploaded = total_uploaded,
                    shard_bytes = total_bytes,
                    cas_attempts = attempt,
                    "xorb optimization reconciliation complete"
                );
                if cancel.is_cancelled() {
                    return Err(CrabError::Cancelled);
                }
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
                    "manifest changed during xorb optimization reconciliation; retrying"
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

fn map_checkpoint_error(error: crab_remote::checkpoint::CheckpointError) -> CrabError {
    match error {
        crab_remote::checkpoint::CheckpointError::Cancelled => CrabError::Cancelled,
        crab_remote::checkpoint::CheckpointError::Read(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Repack(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Pack(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Metadata(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Write(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Io(source) => source.into(),
        other => CrabError::Internal(other.to_string()),
    }
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
    use object_store::memory::InMemory;
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

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

    fn git_pack_fixture() -> (String, crab_metadata::capsule_protocol::CapsuleGitPack) {
        let workspace = tempfile::tempdir().unwrap();
        let git_dir = workspace.path().join("repository.git");
        assert!(
            Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&git_dir)
                .status()
                .unwrap()
                .success()
        );
        let mut hash = Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["hash-object", "-t", "tree", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        hash.stdin.take().unwrap().write_all(b"").unwrap();
        let tree = String::from_utf8(hash.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        let mut commit = Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["commit-tree", &tree])
            .env("GIT_AUTHOR_NAME", "Crab Test")
            .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
            .env("GIT_AUTHOR_DATE", "@1 +0000")
            .env("GIT_COMMITTER_NAME", "Crab Test")
            .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
            .env("GIT_COMMITTER_DATE", "@1 +0000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        commit.stdin.take().unwrap().write_all(b"commit\n").unwrap();
        let tip = String::from_utf8(commit.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        assert!(
            Command::new("git")
                .arg("--git-dir")
                .arg(&git_dir)
                .args(["update-ref", "refs/heads/main", &tip])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("--git-dir")
                .arg(&git_dir)
                .args(["repack", "-a", "-d", "--depth=64"])
                .status()
                .unwrap()
                .success()
        );
        let source_pack = std::fs::read_dir(git_dir.join("objects/pack"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .unwrap();
        let pack_bytes = std::fs::read(&source_pack).unwrap();
        let canonical_id = blake3::hash(&pack_bytes).to_hex().to_string();
        let installed_dir = workspace.path().join("installed");
        std::fs::create_dir_all(&installed_dir).unwrap();
        let installed = crab_git::pack::install_pack_file_from_path(
            &installed_dir,
            &source_pack,
            &canonical_id,
            MAX_CAPSULE_BYTES,
            true,
        )
        .unwrap();
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &installed.idx_path,
            &installed.rev_path,
            pack_bytes.len() as u64,
        )
        .unwrap();
        let object_count = locations.object_count();
        let object_ids = locations
            .by_ref()
            .map(|location| location.unwrap().oid)
            .collect::<Vec<_>>();
        let kinds = crab_git::pack::object_kinds_from_git_dir(&git_dir, &object_ids).unwrap();
        let ordered_kinds = object_ids
            .iter()
            .map(|oid| *kinds.get(oid).unwrap())
            .collect::<Vec<_>>();
        let checksum = gix_hash::ObjectId::from_hex(installed.git_sha1.as_bytes()).unwrap();
        let locator =
            crab_git::pack_locator::encode_pack_kind_metadata(checksum, &ordered_kinds).unwrap();
        let pack = crab_metadata::capsule_protocol::CapsuleGitPack::new(
            Bytes::from(pack_bytes),
            Bytes::from(std::fs::read(&installed.idx_path).unwrap()),
            Bytes::from(std::fs::read(&installed.rev_path).unwrap()),
            Bytes::from(locator),
            installed.git_sha1,
            object_count,
        )
        .unwrap();
        (tip, pack)
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
        let journal = OptimizeXorbsJournal::open(&path).unwrap();

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
            &Config::default(),
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
        let journal = OptimizeXorbsJournal::open(&path).unwrap();

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
            &Config::default(),
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
        let journal = OptimizeXorbsJournal::open(&path).unwrap();

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
            source_catalog: HashMap::new(),
            destination_infos: HashMap::new(),
            destination_catalog: HashMap::new(),
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
            source_catalog: HashMap::new(),
            destination_infos: HashMap::from([(destination_hash, destination_info)]),
            destination_catalog: HashMap::new(),
        };

        let rewrite = rewrite_shard(&Bytes::from(body), old_hash, &mapping, None)
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

    #[test]
    fn capsule_catalog_replaces_shard_and_prunes_source_xorb() {
        use crab_metadata::capsule_protocol::{
            FileCatalogEntry, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
        };

        let old_shard = hash(20);
        let new_shard = hash(21);
        let source_xorb = hash(22);
        let destination_xorb = hash(23);
        let file = hash(24);
        let chunk = hash(25);
        let source_entry = XorbCatalogEntry::new(
            10,
            hash(26).hex(),
            vec![XorbChunkEntry::new(chunk.hex(), 10)],
        );
        let destination_entry = XorbCatalogEntry::new(
            9,
            hash(27).hex(),
            vec![XorbChunkEntry::new(chunk.hex(), 10)],
        );
        let mut current = PointerCatalog::new();
        current
            .insert_xorb(source_xorb.hex(), source_entry)
            .unwrap();
        current
            .insert_shard(
                old_shard.hex(),
                ShardCatalogEntry::new(20, vec![source_xorb.hex()]),
            )
            .unwrap();
        current
            .insert_file(file.hex(), FileCatalogEntry::new(10, old_shard.hex()))
            .unwrap();
        let mapping = LoadedMapping {
            sources: HashMap::new(),
            source_catalog: HashMap::new(),
            destination_infos: HashMap::new(),
            destination_catalog: HashMap::from([(destination_xorb, destination_entry.clone())]),
        };
        let plan = ReconcilePlan {
            replacements: vec![ShardRewrite {
                new_hash: new_shard,
                bytes: Bytes::from_static(b"replacement"),
                file_entries: vec![FileIndexEntry {
                    file_hash: file,
                    recipe_hash: [0; 32],
                    shard_hash: new_shard,
                }],
                xorbs: vec![destination_xorb],
            }],
            replaced_sources: HashSet::from([old_shard]),
            final_shards: vec![new_shard],
            file_entries: vec![FileIndexEntry {
                file_hash: file,
                recipe_hash: [0; 32],
                shard_hash: new_shard,
            }],
        };

        let replacement = capsule_replacement_catalog(&current, &plan, &mapping).unwrap();

        assert_eq!(
            replacement.files()[&file.hex()].shard_hash(),
            new_shard.hex()
        );
        assert!(replacement.shards().contains_key(&new_shard.hex()));
        assert!(!replacement.shards().contains_key(&old_shard.hex()));
        assert_eq!(
            replacement.xorbs()[&destination_xorb.hex()],
            destination_entry
        );
        assert!(!replacement.xorbs().contains_key(&source_xorb.hex()));
    }

    #[test]
    fn capsule_rewrite_strips_foreign_files_from_shared_shard() {
        let source_xorb = hash(30);
        let destination_xorb = hash(31);
        let foreign_xorb = hash(32);
        let selected_file = hash(33);
        let foreign_file = hash(34);
        let selected_chunk = hash(35);
        let foreign_chunk = hash(36);
        let mut writer = ShardWriter::new();
        writer
            .add_xorb(xorb_info(source_xorb, &[(selected_chunk, 10)]))
            .unwrap();
        writer
            .add_xorb(xorb_info(foreign_xorb, &[(foreign_chunk, 12)]))
            .unwrap();
        writer
            .add_file(file_info(selected_file, source_xorb, 10))
            .unwrap();
        writer
            .add_file(file_info(foreign_file, foreign_xorb, 12))
            .unwrap();
        let (body, old_hash) = writer.finalize().unwrap();
        let mapping = LoadedMapping {
            sources: HashMap::from([(
                source_xorb,
                SourcePlacement {
                    chunks: vec![SourceChunk {
                        hash: selected_chunk,
                        size: 10,
                    }],
                    refs: HashMap::from([(
                        selected_chunk,
                        XorbRef {
                            xorb_hash: destination_xorb,
                            chunk_index: 0,
                            uncompressed_size: 10,
                        },
                    )]),
                },
            )]),
            source_catalog: HashMap::new(),
            destination_infos: HashMap::from([(
                destination_xorb,
                xorb_info(destination_xorb, &[(selected_chunk, 10)]),
            )]),
            destination_catalog: HashMap::new(),
        };

        let rewrite = rewrite_shard(
            &Bytes::from(body),
            old_hash,
            &mapping,
            Some(&HashSet::from([selected_file])),
        )
        .unwrap()
        .unwrap();
        let reader = ShardReader::from_bytes(rewrite.bytes, rewrite.new_hash);

        assert!(reader.get_file_info(&selected_file).unwrap().is_some());
        assert!(reader.get_file_info(&foreign_file).unwrap().is_none());
        assert!(reader.get_xorb_info(&destination_xorb).unwrap().is_some());
        assert!(reader.get_xorb_info(&foreign_xorb).unwrap().is_none());
    }

    #[tokio::test]
    async fn capsule_finalize_publishes_rewritten_xorbs_without_legacy_manifest() {
        use crab_metadata::capsule_protocol::{
            Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
            CapsuleVisibilityDelta, FileCatalogEntry, PointerCatalog, ShardCatalogEntry,
            XorbCatalogEntry, XorbChunkEntry,
        };
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/optimize-v2".to_owned());
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let root =
            crab_write::capsule_protocol::initialize(&layout, &hash(40).hex(), "refs/heads/main")
                .await
                .unwrap();

        let chunk_a = Chunk::new(Bytes::from(vec![41_u8; 1024]));
        let chunk_b = Chunk::new(Bytes::from(vec![42_u8; 1024]));
        let mut source_builder = XorbBuilder::new();
        source_builder.push(&chunk_a, RunId(0)).unwrap();
        source_builder.push(&chunk_b, RunId(0)).unwrap();
        let mut source_results = source_builder.finalize().unwrap();
        assert_eq!(source_results.len(), 1);
        let source = source_results.remove(0);
        let mut destination_a_builder = XorbBuilder::new();
        destination_a_builder.push(&chunk_a, RunId(0)).unwrap();
        let destination_a = destination_a_builder.finalize().unwrap().remove(0);
        let mut destination_b_builder = XorbBuilder::new();
        destination_b_builder.push(&chunk_b, RunId(0)).unwrap();
        let destination_b = destination_b_builder.finalize().unwrap().remove(0);
        assert_ne!(source.hash, destination_a.hash);
        assert_ne!(source.hash, destination_b.hash);

        let file_hash = hash(43);
        let source_info = xorb_info(source.hash, &[(chunk_a.hash, 1024), (chunk_b.hash, 1024)]);
        let mut shard_writer = ShardWriter::new();
        shard_writer.add_xorb(source_info).unwrap();
        shard_writer
            .add_file(MDBFileInfo {
                metadata: crab_xet::shard::FileDataSequenceHeader::new(file_hash, 1, false, false),
                segments: vec![FileDataSequenceEntry::new(source.hash, 2048, 0, 2)],
                verification: Vec::new(),
                metadata_ext: None,
            })
            .unwrap();
        let (shard_body, shard_hash) = shard_writer.finalize().unwrap();
        for (xorb_hash, body) in [
            (source.hash, source.bytes.clone()),
            (destination_a.hash, destination_a.bytes.clone()),
            (destination_b.hash, destination_b.bytes.clone()),
        ] {
            store
                .put(&layout.xorb_path(&xorb_hash), Bytes::from(body))
                .await
                .unwrap();
        }
        store
            .put(
                &layout.shard_path(&shard_hash),
                Bytes::from(shard_body.clone()),
            )
            .await
            .unwrap();

        let mut catalog = PointerCatalog::new();
        catalog
            .insert_xorb(
                source.hash.hex(),
                XorbCatalogEntry::new(
                    source.bytes.len() as u64,
                    blake3::hash(&source.bytes).to_hex().to_string(),
                    vec![
                        XorbChunkEntry::new(chunk_a.hash.hex(), 1024),
                        XorbChunkEntry::new(chunk_b.hash.hex(), 1024),
                    ],
                ),
            )
            .unwrap();
        catalog
            .insert_shard(
                shard_hash.hex(),
                ShardCatalogEntry::new(shard_body.len() as u64, vec![source.hash.hex()]),
            )
            .unwrap();
        catalog
            .insert_file(
                file_hash.hex(),
                FileCatalogEntry::new(2048, shard_hash.hex()),
            )
            .unwrap();

        let (tip, pack) = git_pack_fixture();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(tip.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            "refs/heads/main".to_owned(),
            crab_metadata::git_visibility::GitVisibilityEdit::from_replacement_objects(
                None,
                tip.clone(),
                vec![tip],
            ),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![pack],
            vec![
                CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    catalog.encode_delta().unwrap(),
                ),
                CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    visibility.encode().unwrap(),
                ),
            ],
        )
        .unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
            .await
            .unwrap();

        let journal_dir = tempfile::tempdir().unwrap();
        let journal = OptimizeXorbsJournal::open(&journal_dir.path().join("journal.db")).unwrap();
        journal.start_run("capsule-finalize", "{}").unwrap();
        journal
            .insert_source("capsule-finalize", &source.hash.hex())
            .unwrap();
        journal
            .update_source_status(
                "capsule-finalize",
                &source.hash.hex(),
                SourceStatus::Done,
                Some(
                    &serde_json::to_string(&vec![
                        destination_a.hash.hex(),
                        destination_b.hash.hex(),
                    ])
                    .unwrap(),
                ),
            )
            .unwrap();

        let outcome = finalize(
            &journal,
            "capsule-finalize",
            Some(&store),
            Some(&router),
            &Config::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.entries_updated, 1);
        assert_eq!(outcome.shards_uploaded, 1);
        let view = crab_read::capsule_protocol::open_view(
            &layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: MAX_CAPSULE_BYTES,
                max_frontier_bytes: MAX_CAPSULE_BYTES,
            },
        )
        .await
        .unwrap();
        let replacement = view.pointer_catalog().unwrap();
        assert!(!replacement.xorbs().contains_key(&source.hash.hex()));
        assert!(replacement.xorbs().contains_key(&destination_a.hash.hex()));
        assert!(replacement.xorbs().contains_key(&destination_b.hash.hex()));
        assert_ne!(
            replacement.files()[&file_hash.hex()].shard_hash(),
            shard_hash.hex()
        );
        crab_read::verify_capsule_pointer_catalog_objects(&layout, &replacement)
            .await
            .unwrap();
        assert!(matches!(
            store.get_with_etag(&router.manifest_path()).await,
            Err(CrabError::NotFound { .. })
        ));
    }
}
