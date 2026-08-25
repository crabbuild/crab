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

use std::collections::HashMap;
use std::io::Read;

use bytes::Bytes;
use tracing::warn;
use xet_core_structures::metadata_shard::MDBShardFileHeader;
use xet_core_structures::metadata_shard::file_structs::FileDataSequenceEntry;
use xet_core_structures::metadata_shard::streaming_shard::{
    process_shard_file_info_section, process_shard_xorb_info_section,
};

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

/// Magic bytes at the end of a v2 shard.
const SHARD_V2_MAGIC: &[u8; 4] = b"SH02";

/// Size of the v2 trailer: `bloom_offset (u64 LE)` + `magic (4 bytes)`.
const SHARD_V2_TRAILER_SIZE: usize = 12;

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

/// Strip the v2 bloom trailer from shard bytes, returning the v1 portion.
///
/// For v1 shards (no bloom trailer), returns the full slice unchanged.
/// For v2 shards, returns the prefix up to `bloom_offset`, which is
/// the payload the xet-core deserializers understand.
#[must_use]
pub fn strip_v2_trailer(data: &[u8]) -> &[u8] {
    if data.len() >= SHARD_V2_TRAILER_SIZE && &data[data.len() - 4..] == SHARD_V2_MAGIC {
        let offset_start = data.len() - SHARD_V2_TRAILER_SIZE;
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
    let v1_data = strip_v2_trailer(data);
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
/// record limits. The parser continues to the section bookends after a limit
/// is observed so malformed input is consumed and reported deterministically.
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

    Ok((assemble_file_recipes(files, xorbs)?, chunk_entries))
}

fn assemble_file_recipes(
    files: Vec<(MerkleHash, Vec<FileDataSequenceEntry>)>,
    xorbs: HashMap<MerkleHash, Vec<(MerkleHash, u64)>>,
) -> Result<Vec<ExtractedFileRecipe>> {
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

/// Extract file recipes while enforcing file and chunk-entry caps.
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

    assemble_file_recipes(files, xorbs)
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
    let v1_data = strip_v2_trailer(data);
    let mut cursor = std::io::Cursor::new(v1_data);
    extract_file_recipes_from_reader(&mut cursor)
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
    let v1_data = strip_v2_trailer(data);
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
    }

    #[test]
    fn strip_v2_trailer_passes_v1_unchanged() {
        let data = vec![0u8; 64];
        let stripped = strip_v2_trailer(&data);
        assert_eq!(stripped.len(), data.len());
    }

    #[test]
    fn extractors_return_empty_on_garbage_bytes() {
        let garbage = Bytes::from_static(b"not a shard");
        assert!(extract_chunk_entries_streaming(&garbage).is_empty());
        assert!(extract_file_entries_streaming(&garbage, MerkleHash::default()).is_empty());
    }
}
