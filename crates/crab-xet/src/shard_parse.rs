//! Streaming shard parsers shared across the sync and rebuild paths.
//!
//! Two zero-copy helpers over raw shard bytes:
//!
//! - [`extract_chunk_entries_streaming`] — iterate the xorb-info
//!   section, emitting `(chunk_hash, XorbRef)` pairs. Used by the
//!   cross-client shard synchronizer to refresh the ChunkIndex and
//!   by the rebuild command to repopulate `chunk_index_db`.
//! - [`extract_file_entries_streaming`] — iterate the file-info
//!   section, emitting `(file_hash, shard_hash)` pairs. The
//!   `shard_hash` is supplied by the caller (the containing shard's
//!   content hash, parsed from the object key) because the file-info
//!   entries themselves do not carry their owning shard's identity.
//!   Used by the rebuild command to repopulate `file_index_db`.
//!
//! Both helpers are warn-then-empty on any parse failure: a single
//! malformed shard should not abort a rebuild run over the whole
//! bucket.

use std::collections::{HashMap, HashSet};
use std::io::Read;

use bytes::Bytes;
use tracing::warn;
use xet_core_structures::CoreError;
use xet_core_structures::metadata_shard::MDBShardFileHeader;
use xet_core_structures::metadata_shard::file_structs::FileDataSequenceEntry;

mod records;
use records::{process_shard_file_info_section, process_shard_xorb_info_section};

use crate::error::{Result, XetError};
use crate::xorb::format::{MerkleHash, XorbRef};

/// Exact ordered recipe reconstructed from one committed shard file entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedFileRecipe {
    pub file_hash: MerkleHash,
    pub chunks: Vec<(MerkleHash, u64)>,
}

/// File recipes and chunk-index entries extracted from one metadata shard.
pub type ExtractedFileAndChunkEntries = (Vec<ExtractedFileRecipe>, Vec<(MerkleHash, XorbRef)>);

/// One reconstruction range from a shard file entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractedFileTerm {
    pub xorb_hash: MerkleHash,
    pub unpacked_segment_bytes: u32,
    pub chunk_index_start: u32,
    pub chunk_index_end: u32,
}

fn map_parse_error(section: &'static str, error: CoreError) -> XetError {
    XetError::CorruptObject {
        path: section.to_owned(),
        reason: error.to_string(),
    }
}

/// Visit every file entry without retaining terms from other files.
///
/// The reader is consumed through both shard sections so malformed trailing
/// xorb metadata still fails the replay. Callback storage is caller-owned and
/// may spill each file's terms directly to disk.
pub fn visit_file_entries_from_reader<R, F>(reader: &mut R, mut visit: F) -> Result<u64>
where
    R: Read,
    F: FnMut(MerkleHash, &mut dyn Iterator<Item = ExtractedFileTerm>) -> std::io::Result<()>,
{
    MDBShardFileHeader::deserialize(reader)
        .map_err(|error| map_parse_error("shard header", error))?;
    let mut count = 0_u64;
    let mut callback_error = None;
    let parsed = process_shard_file_info_section(reader, |file_view| {
        count = count
            .checked_add(1)
            .ok_or_else(|| CoreError::InvalidShard("file entry count overflow".to_owned()))?;
        let mut terms = (0..file_view.num_entries()).map(|index| {
            let term = file_view.entry(index);
            ExtractedFileTerm {
                xorb_hash: term.xorb_hash,
                unpacked_segment_bytes: term.unpacked_segment_bytes,
                chunk_index_start: term.chunk_index_start,
                chunk_index_end: term.chunk_index_end,
            }
        });
        if let Err(source) = visit(file_view.file_hash(), &mut terms) {
            callback_error = Some(source);
            return Err(CoreError::Other("file replay callback failed".to_owned()));
        }
        Ok(())
    });
    if let Some(source) = callback_error {
        return Err(XetError::ShardReplayIo {
            section: "shard file-info",
            source,
        });
    }
    parsed.map_err(|error| map_parse_error("shard file-info", error))?;
    process_shard_xorb_info_section(reader, |_| Ok(()))
        .map_err(|error| map_parse_error("shard xorb-info", error))?;
    Ok(count)
}

/// Visit every xorb chunk without retaining chunks from other xorbs.
///
/// Chunk indices are emitted in canonical xorb order. The reader is consumed
/// through both sections so the whole shard is structurally validated.
pub fn visit_xorb_chunks_from_reader<R, F>(reader: &mut R, mut visit: F) -> Result<u64>
where
    R: Read,
    F: FnMut(MerkleHash, u32, MerkleHash, u32) -> std::io::Result<()>,
{
    MDBShardFileHeader::deserialize(reader)
        .map_err(|error| map_parse_error("shard header", error))?;
    process_shard_file_info_section(reader, |_| Ok(()))
        .map_err(|error| map_parse_error("shard file-info", error))?;
    let mut count = 0_u64;
    let mut callback_error = None;
    let parsed = process_shard_xorb_info_section(reader, |xorb_view| {
        let xorb_hash = xorb_view.xorb_hash();
        for index in 0..xorb_view.num_entries() {
            let chunk_index = u32::try_from(index)
                .map_err(|_| CoreError::InvalidShard("xorb chunk index overflow".to_owned()))?;
            let chunk = xorb_view.chunk(index);
            if let Err(source) = visit(
                xorb_hash,
                chunk_index,
                chunk.chunk_hash,
                chunk.unpacked_segment_bytes,
            ) {
                callback_error = Some(source);
                return Err(CoreError::Other("xorb replay callback failed".to_owned()));
            }
            count = count.checked_add(1).ok_or_else(|| {
                CoreError::InvalidShard("xorb chunk entry count overflow".to_owned())
            })?;
        }
        Ok(())
    });
    if let Some(source) = callback_error {
        return Err(XetError::ShardReplayIo {
            section: "shard xorb-info",
            source,
        });
    }
    parsed.map_err(|error| map_parse_error("shard xorb-info", error))?;
    Ok(count)
}

/// Magic bytes at the end of Crab's canonical v1 bloom trailer.
const SHARD_V1_MAGIC: &[u8; 4] = b"SH01";

/// Size of the v1 bloom trailer: `bloom_offset (u64 LE)` + `magic (4 bytes)`.
const SHARD_V1_TRAILER_SIZE: usize = 12;

/// Maximum shard body accepted by the in-memory Xet readers.
///
/// Larger shards must be rejected by the object-store boundary before they
/// reach these parsers. Keeping the same limit here also protects callers
/// that already hold a `Bytes` value from spending unbounded CPU on it.
pub const MAX_SHARD_SIZE_BYTES: usize = 512 * 1024 * 1024;

/// Maximum number of file records accepted from one shard.
pub const MAX_SHARD_FILE_ENTRIES: usize = 1_000_000;

/// Maximum number of chunk records accepted from one shard.
pub const MAX_SHARD_CHUNK_ENTRIES: usize = 1_000_000;

/// Return the xet shard body without Crab's optional canonical v1 bloom trailer.
#[must_use]
pub fn strip_bloom_trailer(data: &[u8]) -> &[u8] {
    if data.len() >= SHARD_V1_TRAILER_SIZE && &data[data.len() - 4..] == SHARD_V1_MAGIC {
        let offset_start = data.len() - SHARD_V1_TRAILER_SIZE;
        if let Ok(bytes) = data[offset_start..offset_start + 8].try_into() {
            let bloom_offset = u64::from_le_bytes(bytes) as usize;
            if bloom_offset <= data.len() {
                return &data[..bloom_offset];
            }
        }
    }
    data
}

/// Extract `(chunk_hash, XorbRef)` pairs from raw shard bytes via a
/// streaming parse.
///
/// Walks the header, discards the file-info section, then iterates
/// the xorb-info section entry-by-entry. This avoids constructing
/// the full `MDBShardInfo` / `MDBInMemoryShard` intermediate, keeping
/// peak memory low even for large shards.
///
/// On any parse failure the helper logs a `warn!` and returns an
/// empty vec so the caller can skip the shard without aborting a
/// multi-shard operation (sync, rebuild, inventory).
#[must_use]
pub fn extract_chunk_entries_streaming(data: &Bytes) -> Vec<(MerkleHash, XorbRef)> {
    if data.len() > MAX_SHARD_SIZE_BYTES {
        warn!(
            size = data.len(),
            limit = MAX_SHARD_SIZE_BYTES,
            "refusing to parse oversized shard for streaming chunk extraction"
        );
        return Vec::new();
    }
    let v1_data = strip_bloom_trailer(data);
    let mut cursor = std::io::Cursor::new(v1_data);

    match extract_chunk_entries_from_reader(&mut cursor) {
        Ok(entries) => entries,
        Err(e) => {
            warn!(error = %e, "failed to parse shard for streaming chunk extraction");
            Vec::new()
        }
    }
}

/// Extract chunk-index entries from a reader without materializing the shard body.
pub fn extract_chunk_entries_from_reader<R: Read>(
    reader: &mut R,
) -> Result<Vec<(MerkleHash, XorbRef)>> {
    extract_chunk_entries_from_reader_with_limit(reader, MAX_SHARD_CHUNK_ENTRIES)
}

/// Extract chunk-index entries while enforcing a record-count cap.
pub fn extract_chunk_entries_from_reader_with_limit<R: Read>(
    reader: &mut R,
    max_entries: usize,
) -> Result<Vec<(MerkleHash, XorbRef)>> {
    MDBShardFileHeader::deserialize(reader).map_err(|error| XetError::CorruptObject {
        path: "shard header".to_owned(),
        reason: error.to_string(),
    })?;
    process_shard_file_info_section(reader, |_| Ok(())).map_err(|error| {
        XetError::CorruptObject {
            path: "shard file-info".to_owned(),
            reason: error.to_string(),
        }
    })?;

    let mut entries = Vec::new();
    let mut over_limit = false;
    process_shard_xorb_info_section(reader, |xorb_view| {
        let xorb_hash = xorb_view.xorb_hash();
        if xorb_view.num_entries() > max_entries.saturating_sub(entries.len()) {
            over_limit = true;
            return Ok(());
        }
        for idx in 0..xorb_view.num_entries() {
            let chunk = xorb_view.chunk(idx);
            entries.push((
                chunk.chunk_hash,
                XorbRef {
                    xorb_hash,
                    chunk_index: idx as u32,
                    uncompressed_size: chunk.unpacked_segment_bytes,
                },
            ));
        }
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard xorb-info".to_owned(),
        reason: error.to_string(),
    })?;
    if over_limit {
        return Err(XetError::CorruptObject {
            path: "shard xorb-info".to_owned(),
            reason: format!("chunk entry count exceeds safety limit {max_entries}"),
        });
    }
    Ok(entries)
}

/// Extract file recipes and chunk-index entries in one bounded reader pass.
pub fn extract_file_and_chunk_entries_from_reader<R: Read>(
    reader: &mut R,
) -> Result<ExtractedFileAndChunkEntries> {
    extract_file_and_chunk_entries_from_reader_with_limits(
        reader,
        MAX_SHARD_FILE_ENTRIES,
        MAX_SHARD_CHUNK_ENTRIES,
    )
}

/// Extract file recipes and chunk-index entries while enforcing per-section
/// record limits and a cap on total expanded chunk occurrences. The parser
/// continues to the section bookends after a limit is observed so malformed
/// input is consumed and reported deterministically.
pub fn extract_file_and_chunk_entries_from_reader_with_limits<R: Read>(
    reader: &mut R,
    max_file_entries: usize,
    max_chunk_entries: usize,
) -> Result<ExtractedFileAndChunkEntries> {
    MDBShardFileHeader::deserialize(reader).map_err(|error| XetError::CorruptObject {
        path: "shard header".to_owned(),
        reason: error.to_string(),
    })?;

    let mut files = Vec::new();
    let mut file_terms = 0usize;
    let mut over_file_limit = false;
    process_shard_file_info_section(reader, |file_view| {
        let term_count = file_view.num_entries();
        if files.len() >= max_file_entries
            || term_count > max_chunk_entries.saturating_sub(file_terms)
        {
            over_file_limit = true;
            return Ok(());
        }
        let terms = (0..term_count)
            .map(|index| file_view.entry(index))
            .collect::<Vec<_>>();
        file_terms += term_count;
        files.push((file_view.file_hash(), terms));
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard file-info".to_owned(),
        reason: error.to_string(),
    })?;

    let mut xorbs = HashMap::new();
    let mut chunk_entries = Vec::new();
    let mut over_chunk_limit = false;
    process_shard_xorb_info_section(reader, |xorb_view| {
        let xorb_hash = xorb_view.xorb_hash();
        if xorb_view.num_entries() > max_chunk_entries.saturating_sub(chunk_entries.len()) {
            over_chunk_limit = true;
            return Ok(());
        }
        let mut chunks = Vec::with_capacity(xorb_view.num_entries());
        for index in 0..xorb_view.num_entries() {
            let chunk = xorb_view.chunk(index);
            chunks.push((chunk.chunk_hash, u64::from(chunk.unpacked_segment_bytes)));
            chunk_entries.push((
                chunk.chunk_hash,
                XorbRef {
                    xorb_hash,
                    chunk_index: index as u32,
                    uncompressed_size: chunk.unpacked_segment_bytes,
                },
            ));
        }
        xorbs.insert(xorb_hash, chunks);
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard xorb-info".to_owned(),
        reason: error.to_string(),
    })?;

    if over_file_limit {
        return Err(XetError::CorruptObject {
            path: "shard file-info".to_owned(),
            reason: format!("file entry count exceeds safety limit {max_file_entries}"),
        });
    }
    if over_chunk_limit {
        return Err(XetError::CorruptObject {
            path: "shard xorb-info".to_owned(),
            reason: format!("chunk entry count exceeds safety limit {max_chunk_entries}"),
        });
    }

    Ok((
        assemble_file_recipes(files, xorbs, max_chunk_entries)?,
        chunk_entries,
    ))
}

fn assemble_file_recipes(
    files: Vec<(MerkleHash, Vec<FileDataSequenceEntry>)>,
    xorbs: HashMap<MerkleHash, Vec<(MerkleHash, u64)>>,
    max_occurrences: usize,
) -> Result<Vec<ExtractedFileRecipe>> {
    let mut occurrences = 0usize;
    files
        .into_iter()
        .map(|(file_hash, terms)| {
            let mut chunks = Vec::new();
            for term in terms {
                let xorb = xorbs
                    .get(&term.xorb_hash)
                    .ok_or_else(|| XetError::CorruptObject {
                        path: "shard file recipe".to_owned(),
                        reason: format!(
                            "file {} references absent xorb {}",
                            file_hash.hex(),
                            term.xorb_hash.hex()
                        ),
                    })?;
                let start = usize::try_from(term.chunk_index_start).map_err(|_| {
                    XetError::CorruptObject {
                        path: "shard file recipe".to_owned(),
                        reason: "chunk range start cannot be represented".to_owned(),
                    }
                })?;
                let end =
                    usize::try_from(term.chunk_index_end).map_err(|_| XetError::CorruptObject {
                        path: "shard file recipe".to_owned(),
                        reason: "chunk range end cannot be represented".to_owned(),
                    })?;
                let selected = xorb
                    .get(start..end)
                    .ok_or_else(|| XetError::CorruptObject {
                        path: "shard file recipe".to_owned(),
                        reason: format!("chunk range {start}..{end} exceeds xorb bounds"),
                    })?;
                let selected_bytes = selected.iter().try_fold(0u64, |total, (_, size)| {
                    total
                        .checked_add(*size)
                        .ok_or_else(|| XetError::CorruptObject {
                            path: "shard file recipe".to_owned(),
                            reason: "chunk byte count overflow".to_owned(),
                        })
                })?;
                if selected_bytes != u64::from(term.unpacked_segment_bytes) {
                    return Err(XetError::CorruptObject {
                        path: "shard file recipe".to_owned(),
                        reason: format!(
                            "term covers {selected_bytes} bytes, expected {}",
                            term.unpacked_segment_bytes
                        ),
                    });
                }
                occurrences = occurrences
                    .checked_add(selected.len())
                    .filter(|count| *count <= max_occurrences)
                    .ok_or_else(|| XetError::CorruptObject {
                        path: "shard file recipe".to_owned(),
                        reason: format!(
                            "expanded recipe exceeds {max_occurrences} chunk occurrences"
                        ),
                    })?;
                chunks.extend_from_slice(selected);
            }
            Ok(ExtractedFileRecipe { file_hash, chunks })
        })
        .collect()
}

/// Extract exact file recipes from a reader without materializing the shard body.
pub fn extract_file_recipes_from_reader<R: Read>(
    reader: &mut R,
) -> Result<Vec<ExtractedFileRecipe>> {
    extract_file_recipes_from_reader_with_limits(
        reader,
        MAX_SHARD_FILE_ENTRIES,
        MAX_SHARD_CHUNK_ENTRIES,
    )
}

/// Extract file recipes while enforcing a file-entry cap.
pub fn extract_file_recipes_from_reader_with_limit<R: Read>(
    reader: &mut R,
    max_file_entries: usize,
) -> Result<Vec<ExtractedFileRecipe>> {
    extract_file_recipes_from_reader_with_limits(reader, max_file_entries, MAX_SHARD_CHUNK_ENTRIES)
}

/// Extract file recipes while bounding file records, chunk entries and total expanded occurrences.
pub fn extract_file_recipes_from_reader_with_limits<R: Read>(
    reader: &mut R,
    max_file_entries: usize,
    max_chunk_entries: usize,
) -> Result<Vec<ExtractedFileRecipe>> {
    MDBShardFileHeader::deserialize(reader).map_err(|error| XetError::CorruptObject {
        path: "shard header".to_owned(),
        reason: error.to_string(),
    })?;

    let mut files = Vec::new();
    let mut file_terms = 0usize;
    let mut over_file_limit = false;
    process_shard_file_info_section(reader, |file_view| {
        let term_count = file_view.num_entries();
        if files.len() >= max_file_entries
            || term_count > max_chunk_entries.saturating_sub(file_terms)
        {
            over_file_limit = true;
            return Ok(());
        }
        let terms = (0..term_count)
            .map(|index| file_view.entry(index))
            .collect::<Vec<_>>();
        file_terms += term_count;
        files.push((file_view.file_hash(), terms));
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard file-info".to_owned(),
        reason: error.to_string(),
    })?;

    if over_file_limit {
        return Err(XetError::CorruptObject {
            path: "shard file-info".to_owned(),
            reason: format!(
                "file recipe or term count exceeds safety limits ({max_file_entries} files, {max_chunk_entries} terms)"
            ),
        });
    }

    let mut xorbs = HashMap::new();
    let mut chunk_entries = 0usize;
    let mut over_chunk_limit = false;
    process_shard_xorb_info_section(reader, |xorb_view| {
        let entry_count = xorb_view.num_entries();
        if entry_count > max_chunk_entries.saturating_sub(chunk_entries) {
            over_chunk_limit = true;
            return Ok(());
        }
        let chunks = (0..entry_count)
            .map(|index| {
                let chunk = xorb_view.chunk(index);
                (chunk.chunk_hash, u64::from(chunk.unpacked_segment_bytes))
            })
            .collect();
        chunk_entries += entry_count;
        xorbs.insert(xorb_view.xorb_hash(), chunks);
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard xorb-info".to_owned(),
        reason: error.to_string(),
    })?;

    if over_chunk_limit {
        return Err(XetError::CorruptObject {
            path: "shard xorb-info".to_owned(),
            reason: format!("chunk entry count exceeds safety limit {max_chunk_entries}"),
        });
    }

    assemble_file_recipes(files, xorbs, max_chunk_entries)
}

/// Extract exact file recipes from raw shard bytes.
pub fn extract_file_recipes(data: &Bytes) -> Result<Vec<ExtractedFileRecipe>> {
    if data.len() > MAX_SHARD_SIZE_BYTES {
        return Err(XetError::CorruptObject {
            path: "shard".to_owned(),
            reason: format!(
                "body size {} exceeds safety limit {MAX_SHARD_SIZE_BYTES}",
                data.len()
            ),
        });
    }
    let v1_data = strip_bloom_trailer(data);
    let mut cursor = std::io::Cursor::new(v1_data);
    extract_file_recipes_from_reader(&mut cursor)
}

/// Extract recipes only for requested file hashes from raw shard bytes.
///
/// Limits apply to the selected recipes and their referenced xorbs, rather
/// than unrelated terms elsewhere in the shard. This keeps point and batch
/// lookup bounded when a valid large shard contains more total terms than one
/// reconstruction request may materialize.
pub fn extract_file_recipes_for_hashes(
    data: &Bytes,
    file_hashes: &HashSet<MerkleHash>,
) -> Result<Vec<ExtractedFileRecipe>> {
    extract_file_recipes_for_hashes_with_limit(data, file_hashes, MAX_SHARD_CHUNK_ENTRIES)
}

/// Extract selected recipes while bounding terms, chunk metadata and expanded occurrences.
///
/// Repeated ranges count each occurrence before the output vector grows. The
/// requested limit is capped at the standard in-memory shard limit.
pub fn extract_file_recipes_for_hashes_with_limit(
    data: &Bytes,
    file_hashes: &HashSet<MerkleHash>,
    max_selected_entries: usize,
) -> Result<Vec<ExtractedFileRecipe>> {
    if data.len() > MAX_SHARD_SIZE_BYTES {
        return Err(XetError::CorruptObject {
            path: "shard".to_owned(),
            reason: format!(
                "body size {} exceeds safety limit {MAX_SHARD_SIZE_BYTES}",
                data.len()
            ),
        });
    }
    let v1_data = strip_bloom_trailer(data);
    let mut cursor = std::io::Cursor::new(v1_data);
    extract_file_recipes_for_hashes_from_reader_with_limits(
        &mut cursor,
        file_hashes,
        MAX_SHARD_FILE_ENTRIES,
        max_selected_entries.min(MAX_SHARD_CHUNK_ENTRIES),
    )
}

fn extract_file_recipes_for_hashes_from_reader_with_limits<R: Read>(
    reader: &mut R,
    file_hashes: &HashSet<MerkleHash>,
    max_file_entries: usize,
    max_selected_entries: usize,
) -> Result<Vec<ExtractedFileRecipe>> {
    MDBShardFileHeader::deserialize(reader).map_err(|error| XetError::CorruptObject {
        path: "shard header".to_owned(),
        reason: error.to_string(),
    })?;

    let mut files = Vec::new();
    let mut selected_terms = 0usize;
    let mut file_entries = 0usize;
    let mut over_file_limit = false;
    let mut needed_xorbs = HashSet::new();
    process_shard_file_info_section(reader, |file_view| {
        file_entries += 1;
        if file_entries > max_file_entries {
            over_file_limit = true;
            return Ok(());
        }
        let file_hash = file_view.file_hash();
        if !file_hashes.contains(&file_hash) {
            return Ok(());
        }
        let term_count = file_view.num_entries();
        if term_count > max_selected_entries.saturating_sub(selected_terms) {
            over_file_limit = true;
            return Ok(());
        }
        let terms = (0..term_count)
            .map(|index| file_view.entry(index))
            .collect::<Vec<_>>();
        selected_terms += term_count;
        needed_xorbs.extend(terms.iter().map(|term| term.xorb_hash));
        files.push((file_hash, terms));
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard file-info".to_owned(),
        reason: error.to_string(),
    })?;
    if over_file_limit {
        return Err(XetError::CorruptObject {
            path: "shard file-info".to_owned(),
            reason: format!(
                "file or selected recipe count exceeds safety limits ({max_file_entries} files, {max_selected_entries} selected terms)"
            ),
        });
    }
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let mut xorbs = HashMap::new();
    let mut selected_chunks = 0usize;
    let mut over_chunk_limit = false;
    process_shard_xorb_info_section(reader, |xorb_view| {
        let xorb_hash = xorb_view.xorb_hash();
        if !needed_xorbs.contains(&xorb_hash) {
            return Ok(());
        }
        let entry_count = xorb_view.num_entries();
        if entry_count > max_selected_entries.saturating_sub(selected_chunks) {
            over_chunk_limit = true;
            return Ok(());
        }
        let chunks = (0..entry_count)
            .map(|index| {
                let chunk = xorb_view.chunk(index);
                (chunk.chunk_hash, u64::from(chunk.unpacked_segment_bytes))
            })
            .collect();
        selected_chunks += entry_count;
        xorbs.insert(xorb_hash, chunks);
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard xorb-info".to_owned(),
        reason: error.to_string(),
    })?;
    if over_chunk_limit {
        return Err(XetError::CorruptObject {
            path: "shard xorb-info".to_owned(),
            reason: format!(
                "selected chunk entry count exceeds safety limit {max_selected_entries}"
            ),
        });
    }

    assemble_file_recipes(files, xorbs, max_selected_entries)
}

/// Extract `(file_hash, shard_hash)` pairs from raw shard bytes via a
/// streaming parse.
///
/// The `shard_hash` argument is the containing shard's content hash;
/// callers typically parse it from the last segment of the shard
/// object key (`.crab/shards/{first-two-hex}/{hex}`). Each file-info entry in the
/// shard contributes one pair.
///
/// On any parse failure the helper logs a `warn!` and returns an
/// empty vec.
#[must_use]
pub fn extract_file_entries_streaming(
    data: &Bytes,
    shard_hash: MerkleHash,
) -> Vec<(MerkleHash, MerkleHash)> {
    if data.len() > MAX_SHARD_SIZE_BYTES {
        warn!(
            size = data.len(),
            limit = MAX_SHARD_SIZE_BYTES,
            "refusing to parse oversized shard for streaming file extraction"
        );
        return Vec::new();
    }
    let v1_data = strip_bloom_trailer(data);
    let mut cursor = std::io::Cursor::new(v1_data);

    if let Err(e) = MDBShardFileHeader::deserialize(&mut cursor) {
        warn!(error = %e, "failed to parse shard header for streaming file extraction");
        return Vec::new();
    }

    let mut entries = Vec::new();
    let mut over_limit = false;
    if let Err(e) = process_shard_file_info_section(&mut cursor, |file_view| {
        if entries.len() >= MAX_SHARD_FILE_ENTRIES {
            over_limit = true;
            return Ok(());
        }
        entries.push((file_view.file_hash(), shard_hash));
        Ok(())
    }) {
        warn!(error = %e, "failed to read file-info section during streaming file extraction");
        return Vec::new();
    }

    if over_limit {
        warn!(
            limit = MAX_SHARD_FILE_ENTRIES,
            "shard file-info entry count exceeds streaming safety limit"
        );
        return Vec::new();
    }

    entries
}

/// Extract every xorb hash referenced by file-info terms from a reader.
///
/// The reader is consumed through the shard header and file-info section only;
/// the full shard body never needs to be materialized in memory.
pub fn extract_file_xorb_hashes_from_reader<R: Read>(reader: &mut R) -> Result<Vec<MerkleHash>> {
    extract_file_xorb_hashes_from_reader_with_limit(reader, MAX_SHARD_CHUNK_ENTRIES)
}

/// Extract file-referenced xorbs while enforcing a caller-provided count cap.
pub fn extract_file_xorb_hashes_from_reader_with_limit<R: Read>(
    reader: &mut R,
    max_hashes: usize,
) -> Result<Vec<MerkleHash>> {
    MDBShardFileHeader::deserialize(reader).map_err(|error| XetError::CorruptObject {
        path: "shard header".to_owned(),
        reason: error.to_string(),
    })?;

    let mut hashes = Vec::new();
    let mut over_limit = false;
    process_shard_file_info_section(reader, |file_view| {
        let entries = file_view.num_entries();
        if entries > max_hashes.saturating_sub(hashes.len()) {
            over_limit = true;
            return Ok(());
        }
        hashes.extend((0..entries).map(|index| file_view.entry(index).xorb_hash));
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard file-info".to_owned(),
        reason: error.to_string(),
    })?;
    if over_limit {
        return Err(XetError::CorruptObject {
            path: "shard file-info".to_owned(),
            reason: format!("file-info xorb reference count exceeds safety limit {max_hashes}"),
        });
    }
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use xet_core_structures::metadata_shard::file_structs::{
        FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo,
    };
    use xet_core_structures::metadata_shard::xorb_structs::{
        MDBXorbInfo, XorbChunkSequenceEntry, XorbChunkSequenceHeader,
    };

    use super::*;
    use crate::shard::ShardWriter;

    fn make_xorb(seed: u64, num_chunks: usize) -> Arc<MDBXorbInfo> {
        let xorb_hash = MerkleHash::from([seed, seed, seed, seed]);
        let chunks: Vec<XorbChunkSequenceEntry> = (0..num_chunks)
            .map(|i| {
                let h = seed.wrapping_add(i as u64 + 1);
                XorbChunkSequenceEntry::new(
                    MerkleHash::from([h, h, h, h]),
                    1024u32,
                    (i as u32) * 1024,
                )
            })
            .collect();
        Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb_hash, num_chunks, num_chunks * 1024),
            chunks,
        })
    }

    fn make_file(seed: u64, xorb_seed: u64) -> MDBFileInfo {
        let file_hash = MerkleHash::from([seed, seed, seed, seed]);
        MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
            segments: vec![FileDataSequenceEntry::new(
                MerkleHash::from([xorb_seed, xorb_seed, xorb_seed, xorb_seed]),
                1024u32,
                0u32,
                1u32,
            )],
            verification: vec![],
            metadata_ext: None,
        }
    }

    #[test]
    fn materialized_recipes_bound_repeated_chunk_occurrences() {
        let mut writer = ShardWriter::new();
        writer.add_xorb(make_xorb(1, 2)).unwrap();
        let mut file = make_file(100, 1);
        file.metadata = FileDataSequenceHeader::new(file.metadata.file_hash, 3u32, false, false);
        file.segments =
            vec![FileDataSequenceEntry::new(MerkleHash::from([1u64; 4]), 2048u32, 0u32, 2u32); 3];
        let selected = HashSet::from([file.metadata.file_hash]);
        writer.add_file(file).unwrap();
        let (bytes, _) = writer.finalize().unwrap();
        let bytes = Bytes::from(bytes);
        for limit in [4, 6] {
            let combined = extract_file_and_chunk_entries_from_reader_with_limits(
                &mut std::io::Cursor::new(&bytes),
                1,
                limit,
            )
            .map(|(recipes, _)| recipes);
            let full = extract_file_recipes_from_reader_with_limits(
                &mut std::io::Cursor::new(&bytes),
                1,
                limit,
            );
            let selected = extract_file_recipes_for_hashes_with_limit(&bytes, &selected, limit);
            for result in [combined, full, selected] {
                if limit == 4 {
                    assert!(
                        matches!(result, Err(XetError::CorruptObject { reason, .. }) if reason.contains("chunk occurrences"))
                    );
                } else {
                    assert_eq!(result.unwrap()[0].chunks.len(), 6);
                }
            }
        }
    }

    /// A freshly built shard round-trips through both extractors:
    /// every xorb chunk appears in `extract_chunk_entries_streaming`
    /// and every file appears in `extract_file_entries_streaming`
    /// paired with the caller-supplied shard hash.
    #[test]
    fn round_trip_extracts_chunks_and_files() {
        let mut writer = ShardWriter::new();
        writer.add_xorb(make_xorb(1, 3)).expect("add xorb 1");
        writer.add_xorb(make_xorb(2, 2)).expect("add xorb 2");
        writer.add_file(make_file(100, 1)).expect("add file 1");
        writer.add_file(make_file(200, 2)).expect("add file 2");
        let (bytes, shard_hash) = writer.finalize().expect("finalize");
        let bytes = Bytes::from(bytes);

        let chunk_entries = extract_chunk_entries_streaming(&bytes);
        assert_eq!(
            chunk_entries.len(),
            5,
            "2 xorbs with 3 + 2 chunks must yield 5 chunk entries"
        );

        let file_entries = extract_file_entries_streaming(&bytes, shard_hash);
        assert_eq!(file_entries.len(), 2, "both file entries must surface");
        for (_, stored_shard) in &file_entries {
            assert_eq!(
                *stored_shard, shard_hash,
                "every entry must be tagged with the caller-supplied shard hash"
            );
        }

        let mut reader = std::io::Cursor::new(bytes.as_ref());
        let file_xorb_hashes = extract_file_xorb_hashes_from_reader(&mut reader).unwrap();
        assert_eq!(
            file_xorb_hashes,
            vec![
                MerkleHash::from([1, 1, 1, 1]),
                MerkleHash::from([2, 2, 2, 2])
            ]
        );

        let mut limited_reader = std::io::Cursor::new(bytes.as_ref());
        let limited = extract_file_xorb_hashes_from_reader_with_limit(&mut limited_reader, 1);
        assert!(
            limited.is_err(),
            "the per-shard xorb-reference cap must fail closed"
        );

        let recipes = extract_file_recipes(&bytes).expect("extract recipes");
        assert_eq!(recipes.len(), 2);
        assert_eq!(recipes[0].chunks.len(), 1);
        assert_eq!(recipes[0].chunks[0].1, 1024);

        let mut reader = std::io::Cursor::new(bytes.as_ref());
        let (streamed_recipes, streamed_chunks) =
            extract_file_and_chunk_entries_from_reader(&mut reader).unwrap();
        assert_eq!(streamed_recipes, recipes);
        assert_eq!(streamed_chunks, chunk_entries);

        let mut limited_reader = std::io::Cursor::new(bytes.as_ref());
        assert!(
            extract_file_and_chunk_entries_from_reader_with_limits(&mut limited_reader, 1, 5)
                .is_err()
        );
        let mut limited_reader = std::io::Cursor::new(bytes.as_ref());
        assert!(
            extract_file_and_chunk_entries_from_reader_with_limits(&mut limited_reader, 2, 4)
                .is_err()
        );
        let mut limited_reader = std::io::Cursor::new(bytes.as_ref());
        assert!(extract_file_recipes_from_reader_with_limits(&mut limited_reader, 2, 4).is_err());

        let selected = HashSet::from([MerkleHash::from([100, 100, 100, 100])]);
        let mut targeted_reader = std::io::Cursor::new(bytes.as_ref());
        let targeted = extract_file_recipes_for_hashes_from_reader_with_limits(
            &mut targeted_reader,
            &selected,
            2,
            3,
        )
        .expect("unselected recipe terms must not consume the lookup limit");
        assert_eq!(targeted.len(), 1);
        assert_eq!(
            targeted[0].file_hash,
            MerkleHash::from([100, 100, 100, 100])
        );

        let mut visited_files = Vec::new();
        let mut reader = std::io::Cursor::new(bytes.as_ref());
        let file_count = visit_file_entries_from_reader(&mut reader, |file_hash, terms| {
            visited_files.push((file_hash, terms.collect::<Vec<_>>()));
            Ok(())
        })
        .expect("visit files");
        assert_eq!(file_count, 2);
        assert_eq!(visited_files.len(), 2);
        assert_eq!(visited_files[0].1.len(), 1);
        assert_eq!(visited_files[0].1[0].chunk_index_start, 0);
        assert_eq!(visited_files[0].1[0].chunk_index_end, 1);

        let mut visited_chunks = Vec::new();
        let mut reader = std::io::Cursor::new(bytes.as_ref());
        let chunk_count = visit_xorb_chunks_from_reader(
            &mut reader,
            |xorb_hash, chunk_index, chunk_hash, size| {
                visited_chunks.push((xorb_hash, chunk_index, chunk_hash, size));
                Ok(())
            },
        )
        .expect("visit chunks");
        assert_eq!(chunk_count, 5);
        assert_eq!(visited_chunks.len(), 5);
        assert_eq!(visited_chunks[0].1, 0);
        assert_eq!(visited_chunks[0].3, 1024);
    }

    #[test]
    fn strip_bloom_trailer_passes_plain_v1_unchanged() {
        let data = vec![0u8; 64];
        let stripped = strip_bloom_trailer(&data);
        assert_eq!(stripped.len(), data.len());
    }

    #[test]
    fn extractors_return_empty_on_garbage_bytes() {
        let garbage = Bytes::from_static(b"not a shard");
        assert!(extract_chunk_entries_streaming(&garbage).is_empty());
        assert!(extract_file_entries_streaming(&garbage, MerkleHash::default()).is_empty());
    }

    #[test]
    fn streaming_visitor_replays_more_than_legacy_materialization_limit() {
        let chunk_count = MAX_SHARD_CHUNK_ENTRIES + 1;
        let mut writer = ShardWriter::new();
        writer
            .add_xorb(make_xorb(900, chunk_count))
            .expect("add large xorb metadata");
        let (bytes, _) = writer.finalize().expect("finalize large shard");
        let mut reader = std::io::Cursor::new(bytes);
        let visited = visit_xorb_chunks_from_reader(
            &mut reader,
            |_xorb_hash, _chunk_index, _chunk_hash, _size| Ok(()),
        )
        .expect("streaming visitor must not impose the old materialization cap");

        assert_eq!(visited, chunk_count as u64);
    }
}
