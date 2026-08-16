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

use bytes::Bytes;
use tracing::warn;
use xet_core_structures::metadata_shard::MDBShardFileHeader;
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

/// Magic bytes at the end of a v2 shard.
const SHARD_V2_MAGIC: &[u8; 4] = b"SH02";

/// Size of the v2 trailer: `bloom_offset (u64 LE)` + `magic (4 bytes)`.
const SHARD_V2_TRAILER_SIZE: usize = 12;

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
    let v1_data = strip_v2_trailer(data);
    let mut cursor = std::io::Cursor::new(v1_data);

    // Read and discard the shard header (version verification only).
    if let Err(e) = MDBShardFileHeader::deserialize(&mut cursor) {
        warn!(error = %e, "failed to parse shard header for streaming chunk extraction");
        return Vec::new();
    }

    // Skip through the file-info section without collecting entries.
    if let Err(e) = process_shard_file_info_section(&mut cursor, |_| Ok(())) {
        warn!(error = %e, "failed to skip file-info section during streaming chunk extraction");
        return Vec::new();
    }

    // Stream through the xorb-info section, extracting chunk→XorbRef mappings.
    let mut entries = Vec::new();
    if let Err(e) = process_shard_xorb_info_section(&mut cursor, |xorb_view| {
        let xorb_hash = xorb_view.xorb_hash();
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
    }) {
        warn!(error = %e, "failed to read xorb-info section during streaming chunk extraction");
        return Vec::new();
    }

    entries
}

/// Extract `(file_hash, shard_hash)` pairs from raw shard bytes via a
/// streaming parse.
///
/// The `shard_hash` argument is the containing shard's content hash;
/// callers typically parse it from the last segment of the shard
/// object key (`.crab/shards/{hex}`). Each file-info entry in the
/// shard contributes one pair.
///
/// On any parse failure the helper logs a `warn!` and returns an
/// empty vec.
#[must_use]
pub fn extract_file_entries_streaming(
    data: &Bytes,
    shard_hash: MerkleHash,
) -> Vec<(MerkleHash, MerkleHash)> {
    let v1_data = strip_v2_trailer(data);
    let mut cursor = std::io::Cursor::new(v1_data);

    if let Err(e) = MDBShardFileHeader::deserialize(&mut cursor) {
        warn!(error = %e, "failed to parse shard header for streaming file extraction");
        return Vec::new();
    }

    let mut entries = Vec::new();
    if let Err(e) = process_shard_file_info_section(&mut cursor, |file_view| {
        entries.push((file_view.file_hash(), shard_hash));
        Ok(())
    }) {
        warn!(error = %e, "failed to read file-info section during streaming file extraction");
        return Vec::new();
    }

    entries
}

/// Extract exact ordered file recipes and reject incomplete shard terms.
pub fn extract_file_recipes(data: &Bytes) -> Result<Vec<ExtractedFileRecipe>> {
    let v1_data = strip_v2_trailer(data);
    let mut cursor = std::io::Cursor::new(v1_data);
    MDBShardFileHeader::deserialize(&mut cursor).map_err(|error| XetError::CorruptObject {
        path: "shard header".to_owned(),
        reason: error.to_string(),
    })?;

    let mut files = Vec::new();
    process_shard_file_info_section(&mut cursor, |file_view| {
        let terms = (0..file_view.num_entries())
            .map(|index| file_view.entry(index))
            .collect::<Vec<_>>();
        files.push((file_view.file_hash(), terms));
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard file-info".to_owned(),
        reason: error.to_string(),
    })?;

    let mut xorbs: HashMap<MerkleHash, Vec<(MerkleHash, u64)>> = HashMap::new();
    process_shard_xorb_info_section(&mut cursor, |xorb_view| {
        let chunks = (0..xorb_view.num_entries())
            .map(|index| {
                let chunk = xorb_view.chunk(index);
                (chunk.chunk_hash, u64::from(chunk.unpacked_segment_bytes))
            })
            .collect();
        xorbs.insert(xorb_view.xorb_hash(), chunks);
        Ok(())
    })
    .map_err(|error| XetError::CorruptObject {
        path: "shard xorb-info".to_owned(),
        reason: error.to_string(),
    })?;

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

        let recipes = extract_file_recipes(&bytes).expect("extract recipes");
        assert_eq!(recipes.len(), 2);
        assert_eq!(recipes[0].chunks.len(), 1);
        assert_eq!(recipes[0].chunks[0].1, 1024);
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
