//! Error type for Xet-backed Crab data-plane modules.

use xet_core_structures::CoreError;

/// Result alias for Xet-backed Crab data-plane operations.
pub type Result<T> = std::result::Result<T, XetError>;

/// Errors raised while parsing or reconstructing Xet-backed Crab data.
#[derive(thiserror::Error, Debug)]
pub enum XetError {
    /// A serialized object is malformed or inconsistent.
    #[error("corrupt object at {path}: {reason}")]
    CorruptObject { path: String, reason: String },

    /// The requested chunk index is not present in the object.
    #[error("chunk not found: {hash}")]
    ChunkNotFound { hash: String },

    /// Decompression failed for a serialized chunk payload.
    #[error("decompress failed (scheme={scheme}): {source}")]
    Decompress {
        scheme: &'static str,
        #[source]
        source: CoreError,
    },

    /// Compression failed while building a xorb payload.
    #[error("compression failed (scheme={scheme}): {source}")]
    Compress {
        scheme: &'static str,
        #[source]
        source: CoreError,
    },

    /// A value cannot be represented by the current xorb v1 wire layout.
    #[error("{field} {value} does not fit in xorb v1 layout")]
    Layout { field: String, value: String },

    /// A value cannot be represented by the current shard reconstruction layout.
    #[error("shard format cannot encode {field}={value}; limit is 4294967295")]
    ShardFormat { field: String, value: String },

    /// A file's reconstruction terms do not cover every expected chunk.
    #[error(
        "incomplete shard reconstruction for file {file_hash}: {uncovered_chunks} uncovered chunks; first missing chunk {example_chunk_index} ({example_chunk_hash})"
    )]
    IncompleteShardReconstruction {
        file_hash: String,
        path: Option<String>,
        uncovered_chunks: usize,
        example_chunk_hash: String,
        example_chunk_index: u32,
    },

    /// A consistency check failed inside the xet-backed data plane.
    #[error("internal xet error: {0}")]
    Internal(String),
}
