//! Error contract for virtual filesystem operations.

use tokio_util::sync::CancellationToken;

/// Result alias for VFS operations.
pub type Result<T> = std::result::Result<T, VfsError>;

/// Errors raised by mount, snapshot, overlay, and hydration operations.
#[derive(thiserror::Error, Debug)]
pub enum VfsError {
    #[error("authentication failed for {path}")]
    AuthFailed { path: String },

    #[error("operation cancelled")]
    Cancelled,

    #[error("configuration error in {origin}: {key}")]
    Configuration { key: String, origin: String },

    #[error("corrupt object at {path}: {reason}")]
    CorruptObject { path: String, reason: String },

    #[error("forbidden: {path}")]
    Forbidden { path: String },

    #[error("hash mismatch: requested {requested}, got {actual}")]
    HashMismatch { requested: String, actual: String },

    #[error("internal VFS error: {0}")]
    Internal(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not found: {path}")]
    NotFound { path: String },

    #[error("VFS database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("VFS JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("VFS cache error: {0}")]
    Cache(#[from] crab_cache::CacheError),

    #[error("VFS read error: {0}")]
    Read(#[from] crab_read::ReadError),

    #[error("VFS staging error: {0}")]
    Staging(#[from] crab_staging::StagingError),

    #[error("VFS storage error: {0}")]
    Storage(#[from] crab_storage::StorageError),

    #[error("VFS URL error: {0}")]
    Url(#[from] crab_git::UrlError),

    #[error("invalid Crab pointer: {0}")]
    Pointer(#[from] crab_types::pointer::PointerParseError),
}

impl VfsError {
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

pub fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(VfsError::Cancelled);
    }
    Ok(())
}
