//! Errors retain their local I/O, SQLite, or codec cause.

/// Result of a local replication operation.
pub type Result<T> = std::result::Result<T, CrabError>;

/// Capture and recovery failures; none imply remote publication succeeded.
#[derive(Debug, thiserror::Error)]
pub enum CrabError {
    #[error("LTX checksum mismatch")]
    ChecksumMismatch,
    #[error("LTX file corrupted")]
    LTXCorrupted,
    #[error("LTX file missing")]
    LTXMissing,
    #[error("transaction not available")]
    TxNotAvailable,
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite failure: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("resource limit exceeded: {0}")]
    Limit(&'static str),
    #[error("invalid local replication state: {0}")]
    InvalidState(&'static str),
    #[error("capture failed; close this handle and restore an authoritative plan")]
    Fenced,
    #[error("{0}")]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}
