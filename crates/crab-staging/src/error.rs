//! Staging-domain errors.

/// Result alias for staging operations.
pub type Result<T> = std::result::Result<T, StagingError>;

/// Errors raised by local staging and prepared push-plan handling.
#[derive(thiserror::Error, Debug)]
pub enum StagingError {
    #[error("configuration error in {origin}: {key}")]
    Configuration { key: String, origin: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("staging corrupt: {0}")]
    StagingCorrupt(String),

    #[error("staging is locked by another process")]
    StagingLocked { holder_pid: Option<u32> },

    #[error("chunk not found: {hash}")]
    ChunkNotFound { hash: String },

    #[error("object not found: {path}")]
    NotFound { path: String },

    #[error("chunk hash mismatch: requested {requested}, got {actual}")]
    HashMismatch { requested: String, actual: String },

    #[error("segment CRC mismatch at segment {segment_id} offset {offset}")]
    CrcMismatch { segment_id: u64, offset: u64 },

    #[error("xet data-plane error")]
    Xet(#[from] crab_xet::error::XetError),

    #[error("operation cancelled")]
    Cancelled,

    #[error(
        "file changed while staging: {path} (hash {first_hash} -> {second_hash}, size {first_size} -> {second_size})"
    )]
    FileChangedDuringStaging {
        path: String,
        first_hash: String,
        second_hash: String,
        first_size: u64,
        second_size: u64,
    },

    #[error("internal staging error: {0}")]
    Internal(String),
}
