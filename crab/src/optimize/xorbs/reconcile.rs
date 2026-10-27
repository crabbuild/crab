//! Online reconciliation for concurrent pushes during xorb optimization.
//!
//! Xorb optimization writes destination xorbs before it can know which file versions
//! are still current. This module turns the completed journal mapping into a
//! new immutable shard snapshot. Capsule repositories publish the complete
//! verified pointer catalog through an exact-root checkpoint CAS. Repositories
//! without a Capsule root are rejected before mapping reads or publication.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::core::error::{CrabError, Result, check_cancelled};
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

mod resume;
pub(crate) use resume::prepare_resume;

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
    /// Whether the checkpoint CAS succeeded on the first attempt.
    pub cas_first_attempt: bool,
    /// Total attempts needed for checkpoint publication.
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
    let mut destination_links = 0usize;

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
            destination_links = destination_links.checked_add(dests.len()).ok_or_else(|| {
                CrabError::Configuration {
                    key: "xorb optimization reconciliation destination links".to_owned(),
                    origin: "journal destination link count overflows usize".to_owned(),
                }
            })?;
            if destination_links > MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES {
                return Err(CrabError::Configuration {
                    key: "xorb optimization reconciliation destination links".to_owned(),
                    origin: format!(
                        "journal destination links exceed {MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES} entries"
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
/// to be reread after an authority CAS conflict.
async fn load_mapping(
    store: &Store,
    router: &StoreLayout,
    mapping: &HashMap<String, Vec<String>>,
    cancel: &CancellationToken,
) -> Result<LoadedMapping> {
    let (loaded, destination_coverage) = inspect_mapping(store, router, mapping, cancel).await?;
    for (destination_hash, coverage) in destination_coverage {
        check_cancelled(cancel)?;
        if let Some(index) = coverage.iter().position(|covered| !covered) {
            return Err(CrabError::CorruptObject {
                path: router.xorb_path(&destination_hash).to_string(),
                reason: format!(
                    "destination chunk at index {index} is not covered by its mapped source xorbs"
                ),
            });
        }
    }
    Ok(loaded)
}

// Migration needs the same verified bodies and placements as publication,
// but must identify incomplete old destinations before repairing the journal.
// Only load_mapping can admit a mapping for shard publication.
async fn inspect_mapping(
    store: &Store,
    router: &StoreLayout,
    mapping: &HashMap<String, Vec<String>>,
    cancel: &CancellationToken,
) -> Result<(LoadedMapping, HashMap<MerkleHash, Vec<bool>>)> {
    let mut loaded = LoadedMapping::default();
    let mut loaded_chunk_entries = 0usize;
    let mut destination_coverage = HashMap::<MerkleHash, Vec<bool>>::new();

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

            // The executor merges sources into shared destinations. Prove each
            // source's placements now and the complete mapped-source coverage
            // of each destination after every source has been verified.
            let coverage = destination_coverage
                .entry(destination_hash)
                .or_insert_with(|| vec![false; info.chunks.len()]);
            for (index, (chunk, covered)) in info.chunks.iter().zip(coverage.iter_mut()).enumerate()
            {
                if !source_hashes.contains(&chunk.chunk_hash) {
                    continue;
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
                *covered = true;
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

    Ok((loaded, destination_coverage))
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
    selected_files: &HashSet<MerkleHash>,
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
        if !selected_files.contains(&file.metadata.file_hash) {
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
    if rewritten_files.is_empty() {
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
    selected_files: &HashSet<MerkleHash>,
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
        crate::cmd::gc::closure::publish(
            store,
            router.global_prefix(),
            &replacement.new_hash,
            replacement.bytes.clone(),
            path.as_ref(),
        )
        .await?;
        uploaded += 1;
        bytes += size;
    }
    Ok((uploaded, bytes))
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

/// Publish verified replacement recipes through the current Capsule authority.
///
/// The caller retains maintenance admission through CAS resolution. Concurrent
/// pushes cause a bounded retry from their winning authenticated view.
pub async fn finalize(
    journal: &OptimizeXorbsJournal,
    run_id: &str,
    store: &Store,
    router: &StoreLayout,
    cancel: &CancellationToken,
) -> Result<ReconcileOutcome> {
    check_cancelled(cancel)?;
    let layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
    let (src_to_dest, entries_updated, entries_unchanged) = build_mapping(journal, run_id)?;
    debug!(
        updated = entries_updated,
        unchanged = entries_unchanged,
        mappings = src_to_dest.len(),
        "reconciliation mapping built"
    );
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
    let mapping = load_mapping(store, router, &src_to_dest, cancel).await?;
    Box::pin(finalize_capsule(
        store,
        router,
        &layout,
        root,
        &mapping,
        entries_updated,
        entries_unchanged,
        cancel,
    ))
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
        let current = view.pointer_catalog().await?;
        verify_capsule_sources(&current, mapping)?;
        let (shard_hashes, selected_files) = capsule_selection(&current)?;
        let plan = build_plan(
            store,
            router,
            &shard_hashes,
            mapping,
            &selected_files,
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
            upload_replacements(store, router, &plan.replacements, cancel).await?;
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
        .await?;
        if published.published {
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

/// Check that a CAS repeat is a no-op (idempotency).
pub fn is_cas_repeat_noop(first_outcome: &ReconcileOutcome) -> bool {
    first_outcome.cas_first_attempt
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "reconcile_tests.rs"]
mod tests;
