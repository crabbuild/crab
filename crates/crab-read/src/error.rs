use crab_storage::StorageError;

pub type Result<T> = std::result::Result<T, ReadError>;

/// Errors from read-side reconstruction and hydration.
#[derive(thiserror::Error, Debug)]
pub enum ReadError {
    #[error("pointer parse failed")]
    Pointer(#[from] crab_types::pointer::PointerParseError),

    #[error("cache error")]
    Cache(#[from] crab_cache::CacheError),

    #[error("cache store error")]
    CacheStore(#[from] crab_cache_store::CacheStoreError),

    #[error("storage error")]
    Storage(#[from] StorageError),

    #[error("metadata error")]
    Metadata(#[from] crab_metadata::error::MetadataError),

    #[error("xet data-plane error")]
    Xet(#[from] crab_xet::error::XetError),

    #[error("I/O error")]
    Io(#[from] std::io::Error),

    #[error("configuration error in {origin}: {key}")]
    Configuration { key: String, origin: String },

    #[error("not found: {path}")]
    NotFound { path: String },

    #[error("corrupt object at {path}: {reason}")]
    CorruptObject { path: String, reason: String },

    #[error("hash mismatch: requested {requested}, actual {actual}")]
    HashMismatch { requested: String, actual: String },

    #[error("incomplete shard reconstruction for {file_hash}")]
    IncompleteShardReconstruction {
        file_hash: String,
        uncovered_chunks: u64,
        example_chunk_hash: String,
        example_chunk_index: u32,
    },

    #[error("read operation cancelled")]
    Cancelled,

    #[error("{0}")]
    Internal(String),
}

impl ReadError {
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}
