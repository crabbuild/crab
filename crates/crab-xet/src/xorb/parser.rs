//! Xorb parser for random-access chunk retrieval.
//!
//! [`XorbParser`] reads the serialized xorb format and provides random access
//! to individual chunks by index or range. Each chunk is decompressed according
//! to its per-chunk compression scheme and hash-verified on retrieval.

use bytes::Bytes;
use xet_core_structures::merklehash::{MerkleHash, xorb_hash};
use xet_core_structures::xorb_object::Chunk;

use crate::error::{Result, XetError};
use crate::xorb::format::{
    CHUNK_META_ENTRY_SIZE, ChunkMeta, CompressionScheme, FOOTER_SIZE, XORB_MAGIC, XorbHash,
};

/// Byte range of the serialized xorb metadata section.
#[derive(Debug, Clone, Copy)]
pub struct XorbMetadataRegion {
    pub offset: usize,
    pub len: usize,
}

/// Parsed xorb providing random access to chunks.
pub struct XorbParser {
    data: Bytes,
    chunks: Vec<ChunkMeta>,
    hash: XorbHash,
    payload_digest: [u8; 32],
    payload_len: usize,
}

impl XorbParser {
    /// Parse a serialized xorb from bytes.
    ///
    /// Reads the footer and chunk metadata index. Does not decompress any chunk
    /// data until explicitly requested.
    pub fn parse(data: Bytes) -> Result<Self> {
        let len = data.len();
        if len < FOOTER_SIZE {
            return Err(corrupt("xorb too small for footer"));
        }
        let footer_start = len - FOOTER_SIZE;
        let footer = &data[footer_start..];
        let parsed_footer = parse_xorb_footer(len, footer)?;
        let metadata = data
            .get(parsed_footer.region.offset..footer_start)
            .ok_or_else(|| corrupt("metadata region size mismatch"))?;

        let (chunks, hash) = parse_xorb_metadata(metadata, &parsed_footer)?;

        Ok(Self {
            data,
            chunks,
            hash,
            payload_digest: parsed_footer.payload_digest,
            payload_len: parsed_footer.region.offset,
        })
    }

    /// Number of chunks in this xorb.
    #[must_use]
    pub fn num_chunks(&self) -> u32 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "chunk count bounded by xorb size limit, well under u32::MAX"
        )]
        let n = self.chunks.len() as u32;
        n
    }

    /// Content hash of this xorb.
    #[must_use]
    pub fn hash(&self) -> XorbHash {
        self.hash
    }

    /// Digest bound to the exact serialized payload region by the footer.
    #[must_use]
    pub fn payload_digest(&self) -> [u8; 32] {
        self.payload_digest
    }

    /// Verify the serialized payload region against its footer digest.
    pub fn verify_payload_digest(&self) -> Result<()> {
        let actual = blake3::hash(&self.data[..self.payload_len]);
        if actual.as_bytes() != &self.payload_digest {
            return Err(corrupt("serialized payload digest mismatch"));
        }
        Ok(())
    }

    /// Decompress every chunk and verify each chunk hash.
    pub fn verify_all_chunks(&self) -> Result<()> {
        for meta in &self.chunks {
            let _ = self.decompress_chunk(meta)?;
        }
        Ok(())
    }

    /// Get chunk metadata without decompressing.
    pub fn chunk_meta(&self, index: u32) -> Result<&ChunkMeta> {
        self.chunks
            .get(index as usize)
            .ok_or_else(|| XetError::ChunkNotFound {
                hash: format!(
                    "index {index} out of range (num_chunks={})",
                    self.chunks.len()
                ),
            })
    }

    /// Decompress and return a single chunk by index.
    pub fn get_chunk(&self, index: u32) -> Result<Chunk> {
        let meta = self.chunk_meta(index)?;
        self.decompress_chunk(meta)
    }

    /// Decompress and return a range of chunks `[start, end)`.
    pub fn get_chunk_range(&self, start: u32, end: u32) -> Result<Vec<Chunk>> {
        if start > end || end > self.num_chunks() {
            return Err(XetError::ChunkNotFound {
                hash: format!(
                    "range [{start}, {end}) out of bounds (num_chunks={})",
                    self.num_chunks()
                ),
            });
        }
        (start..end).map(|i| self.get_chunk(i)).collect()
    }

    /// Return one contiguous chunk range and xet-core-compatible offsets.
    ///
    /// Raw chunks remain a slice of the serialized xorb allocation. If any
    /// chunk is compressed, the range is decoded into one owned buffer.
    pub fn get_chunk_range_bytes(&self, start: u32, end: u32) -> Result<(Bytes, Vec<u32>)> {
        if start > end || end > self.num_chunks() {
            return Err(XetError::ChunkNotFound {
                hash: format!(
                    "range [{start}, {end}) out of bounds (num_chunks={})",
                    self.num_chunks()
                ),
            });
        }
        if start == end {
            return Ok((Bytes::new(), vec![0]));
        }

        let metas = &self.chunks[start as usize..end as usize];
        let payload_start = metas[0].offset as usize;
        let last = metas
            .last()
            .ok_or_else(|| corrupt("non-empty chunk range has no last chunk"))?;
        let payload_end = last.offset as usize + last.compressed_len as usize;
        decode_chunk_range_bytes(metas, self.data.slice(payload_start..payload_end))
    }

    fn decompress_chunk(&self, meta: &ChunkMeta) -> Result<Chunk> {
        let start = meta.offset as usize;
        let end = start + meta.compressed_len as usize;
        let compressed = self.data.slice(start..end);
        decompress_chunk_bytes(meta, compressed)
    }
}

struct ParsedXorbFooter {
    num_chunks: u32,
    region: XorbMetadataRegion,
    payload_digest: [u8; 32],
}

/// Return the metadata region described by a serialized xorb footer.
pub fn xorb_metadata_region(data_len: usize, footer: &[u8]) -> Result<XorbMetadataRegion> {
    parse_xorb_footer(data_len, footer).map(|footer| footer.region)
}

/// Read the serialized-payload digest bound into a xorb footer.
pub fn xorb_payload_digest_from_footer(data_len: usize, footer: &[u8]) -> Result<[u8; 32]> {
    parse_xorb_footer(data_len, footer).map(|footer| footer.payload_digest)
}

/// Compute the xorb hash from a footer and metadata slice.
pub fn xorb_hash_from_metadata(
    data_len: usize,
    footer: &[u8],
    metadata: &[u8],
) -> Result<XorbHash> {
    let footer = parse_xorb_footer(data_len, footer)?;
    let (_, hash) = parse_xorb_metadata(metadata, &footer)?;
    Ok(hash)
}

/// Decode chunk metadata and xorb hash from a footer and metadata slice.
pub fn xorb_chunks_from_metadata(
    data_len: usize,
    footer: &[u8],
    metadata: &[u8],
) -> Result<(Vec<ChunkMeta>, XorbHash)> {
    let footer = parse_xorb_footer(data_len, footer)?;
    parse_xorb_metadata(metadata, &footer)
}

/// Verify a compressed chunk payload against its metadata.
pub fn verify_compressed_chunk(meta: &ChunkMeta, compressed: &[u8]) -> Result<()> {
    let _ = decompress_chunk_data(meta, compressed)?;
    Ok(())
}

/// Decode one contiguous serialized chunk-payload range.
///
/// `payload` starts at `metas[0].offset` in the complete xorb. Raw ranges are
/// returned without copying; compressed ranges are decoded into owned bytes.
pub fn decode_chunk_range_bytes(metas: &[ChunkMeta], payload: Bytes) -> Result<(Bytes, Vec<u32>)> {
    if metas.is_empty() {
        if payload.is_empty() {
            return Ok((Bytes::new(), vec![0]));
        }
        return Err(corrupt("empty chunk range has payload bytes"));
    }

    let payload_start = metas[0].offset as usize;
    let mut expected_offset = payload_start;
    for meta in metas {
        if meta.offset as usize != expected_offset {
            return Err(corrupt("chunk range payloads are not contiguous"));
        }
        expected_offset = expected_offset
            .checked_add(meta.compressed_len as usize)
            .ok_or_else(|| corrupt("chunk range payload length overflow"))?;
    }
    if expected_offset - payload_start != payload.len() {
        return Err(corrupt("chunk range payload length mismatch"));
    }

    if metas
        .iter()
        .all(|meta| meta.scheme == CompressionScheme::None)
    {
        let mut offsets = Vec::with_capacity(metas.len() + 1);
        for meta in metas {
            let start = meta.offset as usize - payload_start;
            let end = start + meta.compressed_len as usize;
            let chunk = decompress_chunk_bytes(meta, payload.slice(start..end))?;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "xorb payload is bounded below u32::MAX"
            )]
            offsets.push(start as u32);
            drop(chunk);
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "xorb payload is bounded below u32::MAX"
        )]
        offsets.push(payload.len() as u32);
        return Ok((payload, offsets));
    }

    let total_size = metas
        .iter()
        .map(|meta| meta.uncompressed_len as usize)
        .sum();
    let mut data = Vec::with_capacity(total_size);
    let mut offsets = Vec::with_capacity(metas.len() + 1);
    for meta in metas {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "xorb payload is bounded below u32::MAX"
        )]
        offsets.push(data.len() as u32);
        let start = meta.offset as usize - payload_start;
        let end = start + meta.compressed_len as usize;
        let chunk = decompress_chunk_bytes(meta, payload.slice(start..end))?;
        data.extend_from_slice(&chunk.data);
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "xorb payload is bounded below u32::MAX"
    )]
    offsets.push(data.len() as u32);
    Ok((Bytes::from(data), offsets))
}

fn parse_xorb_footer(data_len: usize, footer: &[u8]) -> Result<ParsedXorbFooter> {
    if data_len < FOOTER_SIZE {
        return Err(corrupt("xorb too small for footer"));
    }
    if footer.len() != FOOTER_SIZE {
        return Err(corrupt("bad footer length"));
    }
    if &footer[FOOTER_SIZE - XORB_MAGIC.len()..] != XORB_MAGIC {
        return Err(corrupt("invalid xorb magic"));
    }

    let num_chunks = u32::from_le_bytes(
        footer[0..4]
            .try_into()
            .map_err(|_| corrupt("bad num_chunks bytes"))?,
    );
    let meta_offset = u64::from_le_bytes(
        footer[4..12]
            .try_into()
            .map_err(|_| corrupt("bad meta_offset bytes"))?,
    );
    let meta_offset =
        usize::try_from(meta_offset).map_err(|_| corrupt("meta_offset does not fit usize"))?;
    let payload_digest = footer[12..44]
        .try_into()
        .map_err(|_| corrupt("bad payload digest bytes"))?;
    let num_chunks_usize =
        usize::try_from(num_chunks).map_err(|_| corrupt("num_chunks does not fit usize"))?;
    let metadata_len = num_chunks_usize
        .checked_mul(CHUNK_META_ENTRY_SIZE)
        .ok_or_else(|| corrupt("metadata size overflow"))?;
    let expected_len = meta_offset
        .checked_add(metadata_len)
        .and_then(|len| len.checked_add(FOOTER_SIZE))
        .ok_or_else(|| corrupt("xorb size overflow"))?;
    if expected_len != data_len {
        return Err(corrupt("metadata region size mismatch"));
    }

    Ok(ParsedXorbFooter {
        num_chunks,
        region: XorbMetadataRegion {
            offset: meta_offset,
            len: metadata_len,
        },
        payload_digest,
    })
}

fn parse_xorb_metadata(
    metadata: &[u8],
    footer: &ParsedXorbFooter,
) -> Result<(Vec<ChunkMeta>, XorbHash)> {
    if metadata.len() != footer.region.len {
        return Err(corrupt("metadata slice length mismatch"));
    }

    let num_chunks =
        usize::try_from(footer.num_chunks).map_err(|_| corrupt("num_chunks does not fit usize"))?;
    let mut chunks = Vec::with_capacity(num_chunks);
    let mut hash_pairs = Vec::with_capacity(num_chunks);
    let mut cursor = 0usize;
    let mut expected_payload_offset = 0u64;
    for _ in 0..num_chunks {
        let hash_bytes: [u8; 32] = metadata[cursor..cursor + 32]
            .try_into()
            .map_err(|_| corrupt("bad chunk hash bytes"))?;
        let hash = MerkleHash::from(hash_bytes);
        cursor += 32;

        let offset = u32::from_le_bytes(
            metadata[cursor..cursor + 4]
                .try_into()
                .map_err(|_| corrupt("bad chunk offset"))?,
        );
        cursor += 4;

        let compressed_len = u32::from_le_bytes(
            metadata[cursor..cursor + 4]
                .try_into()
                .map_err(|_| corrupt("bad compressed_len"))?,
        );
        cursor += 4;

        let uncompressed_len = u32::from_le_bytes(
            metadata[cursor..cursor + 4]
                .try_into()
                .map_err(|_| corrupt("bad uncompressed_len"))?,
        );
        cursor += 4;

        let scheme_byte = metadata[cursor];
        let scheme = CompressionScheme::try_from(scheme_byte)
            .map_err(|_| corrupt("invalid compression scheme byte"))?;
        cursor += 1;

        let chunk_end = (offset as u64).saturating_add(compressed_len as u64);
        if u64::from(offset) != expected_payload_offset || chunk_end > footer.region.offset as u64 {
            return Err(corrupt(
                "chunk payload ranges are not contiguous before metadata",
            ));
        }
        expected_payload_offset = chunk_end;

        hash_pairs.push((hash, u64::from(uncompressed_len)));
        chunks.push(ChunkMeta {
            hash,
            offset,
            compressed_len,
            uncompressed_len,
            scheme,
        });
    }
    if expected_payload_offset != footer.region.offset as u64 {
        return Err(corrupt(
            "chunk payload ranges do not cover the serialized payload region",
        ));
    }

    Ok((chunks, xorb_hash(&hash_pairs)))
}

fn decompress_chunk_data(meta: &ChunkMeta, compressed: &[u8]) -> Result<Chunk> {
    let decompressed = meta
        .scheme
        .decompress_from_slice(compressed)
        .map_err(|source| XetError::Decompress {
            scheme: meta.scheme.into(),
            source,
        })?;

    let chunk = Chunk::new(Bytes::from(decompressed.into_owned()));

    if chunk.hash != meta.hash {
        return Err(XetError::CorruptObject {
            path: format!("chunk at offset {}", meta.offset),
            reason: format!("hash mismatch: expected {}, got {}", meta.hash, chunk.hash),
        });
    }

    Ok(chunk)
}

fn decompress_chunk_bytes(meta: &ChunkMeta, compressed: Bytes) -> Result<Chunk> {
    let data = match meta
        .scheme
        .decompress_from_slice(&compressed)
        .map_err(|source| XetError::Decompress {
            scheme: meta.scheme.into(),
            source,
        })? {
        std::borrow::Cow::Borrowed(_) => compressed,
        std::borrow::Cow::Owned(data) => Bytes::from(data),
    };
    let chunk = Chunk::new(data);
    if chunk.hash != meta.hash {
        return Err(XetError::CorruptObject {
            path: format!("chunk at offset {}", meta.offset),
            reason: format!("hash mismatch: expected {}, got {}", meta.hash, chunk.hash),
        });
    }
    Ok(chunk)
}

fn corrupt(reason: &str) -> XetError {
    XetError::CorruptObject {
        path: "xorb".to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorb::builder::{
        AdaptiveCompression, CompressionPolicy, FixedCompression, RunId, XorbBuilder,
    };

    fn make_chunk(seed: u32, size: usize) -> Chunk {
        let data: Vec<u8> = (0..size as u32)
            .map(|i| (i.wrapping_mul(seed.wrapping_mul(2654435761))) as u8)
            .collect();
        Chunk::new(Bytes::from(data))
    }

    #[test]
    fn round_trip_single_chunk() {
        let original = make_chunk(1, 4096);
        let mut builder = XorbBuilder::new();
        builder.push(&original, RunId(0)).unwrap();
        let xorbs = builder.finalize().unwrap();

        let parsed = XorbParser::parse(xorbs[0].bytes.clone()).unwrap();
        assert_eq!(parsed.num_chunks(), 1);
        assert_eq!(parsed.hash(), xorbs[0].hash);

        let recovered = parsed.get_chunk(0).unwrap();
        assert_eq!(recovered.hash, original.hash);
        assert_eq!(recovered.data, original.data);
    }

    #[test]
    fn raw_chunk_shares_serialized_xorb_storage() {
        let original = make_chunk(17, 128 * 1024);
        let policy: std::sync::Arc<dyn CompressionPolicy> =
            std::sync::Arc::new(FixedCompression::new(CompressionScheme::None));
        let mut builder = XorbBuilder::with_policy(policy);
        builder.push(&original, RunId(0)).unwrap();
        let xorb = builder.finalize().unwrap().pop().unwrap();
        let xorb_start = xorb.bytes.as_ptr() as usize;
        let xorb_end = xorb_start + xorb.bytes.len();

        let parsed = XorbParser::parse(xorb.bytes).unwrap();
        let recovered = parsed.get_chunk(0).unwrap();
        let chunk_start = recovered.data.as_ptr() as usize;
        let chunk_end = chunk_start + recovered.data.len();

        assert!(chunk_start >= xorb_start);
        assert!(chunk_end <= xorb_end);
        assert_eq!(recovered.data, original.data);
    }

    #[test]
    fn raw_chunk_range_shares_serialized_xorb_storage() {
        let chunks: Vec<Chunk> = (0..4u32)
            .map(|index| make_chunk(index + 21, 4096 + index as usize * 97))
            .collect();
        let policy: std::sync::Arc<dyn CompressionPolicy> =
            std::sync::Arc::new(FixedCompression::new(CompressionScheme::None));
        let mut builder = XorbBuilder::with_policy(policy);
        for chunk in &chunks {
            builder.push(chunk, RunId(0)).unwrap();
        }
        let xorb = builder.finalize().unwrap().pop().unwrap();
        let xorb_start = xorb.bytes.as_ptr() as usize;
        let xorb_end = xorb_start + xorb.bytes.len();

        let parsed = XorbParser::parse(xorb.bytes).unwrap();
        let (data, offsets) = parsed.get_chunk_range_bytes(1, 4).unwrap();
        let data_start = data.as_ptr() as usize;

        assert!(data_start >= xorb_start);
        assert!(data_start + data.len() <= xorb_end);
        assert_eq!(offsets.len(), 4);
        assert_eq!(offsets[0], 0);
        assert_eq!(offsets[3] as usize, data.len());
        assert_eq!(
            data,
            chunks[1..]
                .iter()
                .flat_map(|chunk| chunk.data.iter().copied())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn compressed_chunk_range_decodes_with_complete_offsets() {
        let chunks: Vec<Chunk> = (0..3u32)
            .map(|index| Chunk::new(Bytes::from(vec![index as u8; 16 * 1024])))
            .collect();
        let policy: std::sync::Arc<dyn CompressionPolicy> =
            std::sync::Arc::new(FixedCompression::new(CompressionScheme::ByteGrouping4LZ4));
        let mut builder = XorbBuilder::with_policy(policy);
        for chunk in &chunks {
            builder.push(chunk, RunId(0)).unwrap();
        }
        let xorb = builder.finalize().unwrap().pop().unwrap();

        let parsed = XorbParser::parse(xorb.bytes).unwrap();
        let (data, offsets) = parsed.get_chunk_range_bytes(0, 3).unwrap();

        assert_eq!(offsets, vec![0, 16 * 1024, 32 * 1024, 48 * 1024]);
        assert_eq!(
            data,
            chunks
                .iter()
                .flat_map(|chunk| chunk.data.iter().copied())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn round_trip_multiple_chunks() {
        let chunks: Vec<Chunk> = (0..10u32)
            .map(|i| make_chunk(i, 2048 + i as usize * 100))
            .collect();
        let mut builder = XorbBuilder::new();
        for c in &chunks {
            builder.push(c, RunId(0)).unwrap();
        }
        let xorbs = builder.finalize().unwrap();

        let parsed = XorbParser::parse(xorbs[0].bytes.clone()).unwrap();
        assert_eq!(parsed.num_chunks(), 10);
        assert_eq!(parsed.hash(), xorbs[0].hash);

        for (i, original) in chunks.iter().enumerate() {
            let recovered = parsed.get_chunk(i as u32).unwrap();
            assert_eq!(recovered.hash, original.hash);
            assert_eq!(recovered.data, original.data);
        }
    }

    #[test]
    fn get_chunk_range_returns_correct_subset() {
        let chunks: Vec<Chunk> = (0..5u32).map(|i| make_chunk(i + 10, 1024)).collect();
        let mut builder = XorbBuilder::new();
        for c in &chunks {
            builder.push(c, RunId(0)).unwrap();
        }
        let xorbs = builder.finalize().unwrap();

        let parsed = XorbParser::parse(xorbs[0].bytes.clone()).unwrap();
        let range = parsed.get_chunk_range(1, 4).unwrap();
        assert_eq!(range.len(), 3);
        for (i, recovered) in range.iter().enumerate() {
            assert_eq!(recovered.hash, chunks[i + 1].hash);
            assert_eq!(recovered.data, chunks[i + 1].data);
        }
    }

    #[test]
    fn chunk_meta_returns_correct_metadata() {
        let original = make_chunk(42, 8192);
        let mut builder = XorbBuilder::new();
        builder.push(&original, RunId(0)).unwrap();
        let xorbs = builder.finalize().unwrap();

        let parsed = XorbParser::parse(xorbs[0].bytes.clone()).unwrap();
        let meta = parsed.chunk_meta(0).unwrap();
        assert_eq!(meta.hash, original.hash);
        assert_eq!(meta.offset, 0);
        assert_eq!(meta.uncompressed_len, 8192);
        assert!(meta.compressed_len > 0);
    }

    #[test]
    fn out_of_range_index_returns_error() {
        let mut builder = XorbBuilder::new();
        builder.push(&make_chunk(1, 1024), RunId(0)).unwrap();
        let xorbs = builder.finalize().unwrap();

        let parsed = XorbParser::parse(xorbs[0].bytes.clone()).unwrap();
        assert!(parsed.get_chunk(1).is_err());
        assert!(parsed.chunk_meta(1).is_err());
    }

    #[test]
    fn invalid_range_returns_error() {
        let mut builder = XorbBuilder::new();
        builder.push(&make_chunk(1, 1024), RunId(0)).unwrap();
        let xorbs = builder.finalize().unwrap();

        let parsed = XorbParser::parse(xorbs[0].bytes.clone()).unwrap();
        assert!(parsed.get_chunk_range(1, 0).is_err());
        assert!(parsed.get_chunk_range(0, 5).is_err());
    }

    #[test]
    fn empty_range_returns_empty_vec() {
        let mut builder = XorbBuilder::new();
        builder.push(&make_chunk(1, 1024), RunId(0)).unwrap();
        let xorbs = builder.finalize().unwrap();

        let parsed = XorbParser::parse(xorbs[0].bytes.clone()).unwrap();
        let range = parsed.get_chunk_range(0, 0).unwrap();
        assert!(range.is_empty());
    }

    #[test]
    fn parse_rejects_truncated_data() {
        assert!(XorbParser::parse(Bytes::from_static(b"short")).is_err());
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut bad = vec![0u8; FOOTER_SIZE];
        bad[FOOTER_SIZE - 4..].copy_from_slice(b"NOPE");
        assert!(XorbParser::parse(Bytes::from(bad)).is_err());
    }

    #[test]
    fn corrupt_chunk_data_detected() {
        let original = make_chunk(1, 4096);
        let mut builder = XorbBuilder::new();
        builder.push(&original, RunId(0)).unwrap();
        let xorbs = builder.finalize().unwrap();

        let mut corrupted = xorbs[0].bytes.to_vec();
        if !corrupted.is_empty() {
            corrupted[0] ^= 0xFF;
        }

        let parsed = XorbParser::parse(Bytes::from(corrupted));
        if let Ok(p) = parsed {
            assert!(p.get_chunk(0).is_err());
        }
    }

    #[test]
    fn verify_all_chunks_rejects_corrupt_chunk_data() {
        let original = make_chunk(1, 4096);
        let mut builder = XorbBuilder::new();
        builder.push(&original, RunId(0)).unwrap();
        let xorbs = builder.finalize().unwrap();

        let mut corrupted = xorbs[0].bytes.to_vec();
        corrupted[0] ^= 0xFF;

        let parsed = XorbParser::parse(Bytes::from(corrupted)).unwrap();
        assert!(parsed.verify_all_chunks().is_err());
    }

    #[test]
    fn payload_digest_rejects_mutated_serialized_payload() {
        let original = make_chunk(31, 4096);
        let mut builder = XorbBuilder::new();
        builder.push(&original, RunId(0)).unwrap();
        let xorb = builder.finalize().unwrap().pop().unwrap();
        let expected_digest = xorb.payload_digest;
        let mut corrupted = xorb.bytes.to_vec();
        corrupted[0] ^= 0xFF;

        let parsed = XorbParser::parse(Bytes::from(corrupted)).unwrap();

        assert_eq!(parsed.payload_digest(), expected_digest);
        assert!(parsed.verify_payload_digest().is_err());
    }

    #[test]
    fn round_trip_bg4_structured_floats() {
        let float_data: Vec<u8> = (0..2048u32)
            .flat_map(|i| {
                let f = (i as f32) * 0.001;
                f.to_le_bytes()
            })
            .collect();
        let bg4_chunk = Chunk::new(Bytes::from(float_data));

        let policy = AdaptiveCompression::default();
        assert_eq!(
            policy.select(&bg4_chunk.data),
            CompressionScheme::ByteGrouping4LZ4,
            "test data should trigger BG4 selection"
        );

        let text_data: Vec<u8> = b"the quick brown fox jumps over the lazy dog "
            .iter()
            .cycle()
            .take(4096)
            .copied()
            .collect();
        let text_chunk = Chunk::new(Bytes::from(text_data));

        let random_chunk = make_chunk(0xDEAD, 4096);

        let policy: Box<dyn CompressionPolicy> = Box::new(AdaptiveCompression::default());
        let mut builder = XorbBuilder::with_policy(policy);
        builder.push(&bg4_chunk, RunId(0)).unwrap();
        builder.push(&text_chunk, RunId(0)).unwrap();
        builder.push(&random_chunk, RunId(0)).unwrap();
        let xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1);

        let parsed = XorbParser::parse(xorbs[0].bytes.clone()).unwrap();
        assert_eq!(parsed.num_chunks(), 3);
        assert_eq!(parsed.hash(), xorbs[0].hash);

        let bg4_meta = parsed.chunk_meta(0).unwrap();
        assert_eq!(bg4_meta.scheme, CompressionScheme::ByteGrouping4LZ4);

        let originals = [&bg4_chunk, &text_chunk, &random_chunk];
        for (i, original) in originals.iter().enumerate() {
            let recovered = parsed.get_chunk(i as u32).unwrap();
            assert_eq!(
                recovered.data, original.data,
                "chunk {i} data mismatch after round-trip"
            );
            assert_eq!(
                recovered.hash, original.hash,
                "chunk {i} hash mismatch after round-trip"
            );
        }
    }
}
