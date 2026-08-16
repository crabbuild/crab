//! Xorb binary layout types and sizing constants.
//!
//! A xorb is an immutable, content-addressed blob composed of per-chunk
//! compressed data plus per-chunk metadata. Its identity hash is a
//! [`MerkleHash`] over the ordered chunk-hash sequence, making it independent
//! of compression level.

pub use xet_core_structures::merklehash::MerkleHash;
pub use xet_core_structures::xorb_object::{Chunk, CompressionScheme, SerializedXorbObject};

/// Xorb content hash, a [`MerkleHash`] over the ordered chunk-hash sequence.
pub type XorbHash = MerkleHash;

/// Reference to a single chunk within a specific xorb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XorbRef {
    /// Content hash of the containing xorb.
    pub xorb_hash: XorbHash,
    /// Zero-based index of the chunk within the xorb.
    pub chunk_index: u32,
    /// Uncompressed size of the chunk in bytes.
    pub uncompressed_size: u32,
}

/// Metadata for a single chunk within a xorb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkMeta {
    /// Content hash of the uncompressed chunk data.
    pub hash: MerkleHash,
    /// Byte offset of the compressed chunk data within the xorb body.
    pub offset: u32,
    /// Size of the compressed chunk data in bytes.
    pub compressed_len: u32,
    /// Size of the original uncompressed chunk data in bytes.
    pub uncompressed_len: u32,
    /// Compression scheme used for this chunk.
    pub scheme: CompressionScheme,
}

/// Xorb header/metadata, represented with xet-core hash and compression types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XorbInfo {
    /// Content hash of this xorb.
    pub hash: XorbHash,
    /// Number of chunks in the xorb.
    pub num_chunks: u32,
    /// Total compressed size across all chunks in bytes.
    pub total_compressed_bytes: u64,
    /// Total uncompressed size across all chunks in bytes.
    pub total_uncompressed_bytes: u64,
    /// Per-chunk metadata, ordered by chunk index.
    pub chunks: Vec<ChunkMeta>,
}

/// Records where a chunk was placed during xorb packing.
#[derive(Debug, Clone)]
pub struct ChunkPlacement {
    /// Content hash of the uncompressed chunk data.
    pub chunk_hash: MerkleHash,
    /// Content hash of the xorb containing this chunk.
    pub xorb_hash: MerkleHash,
    /// Zero-based index of this chunk within the xorb.
    pub chunk_index: u32,
    /// Uncompressed size of this chunk in bytes.
    pub uncompressed_size: u32,
}

/// Target xorb size: 64 MiB of compressed data.
pub const TARGET_XORB_SIZE: usize = 64 * 1024 * 1024;

/// Default minimum xorb target size: 16 MiB.
pub const MIN_XORB_SIZE: usize = 16 * 1024 * 1024;

/// Default maximum xorb target size: 256 MiB.
pub const MAX_XORB_SIZE: usize = 256 * 1024 * 1024;

/// Default per-chunk zstd compression level.
pub const ZSTD_LEVEL: i32 = 3;

/// Xorb binary format magic bytes written at the end of the footer.
pub const XORB_MAGIC: &[u8; 4] = b"XORB";

/// Size of a serialized chunk metadata entry.
///
/// Layout: `hash`(32) + `offset`(4) + `compressed_len`(4) +
/// `uncompressed_len`(4) + `scheme`(1).
pub const CHUNK_META_ENTRY_SIZE: usize = 32 + 4 + 4 + 4 + 1;

/// Size of the xorb footer.
///
/// Layout: `num_chunks`(4) + `meta_offset`(8) + `payload_digest`(32) + `magic`(4).
pub const FOOTER_SIZE: usize = 4 + 8 + 32 + 4;
