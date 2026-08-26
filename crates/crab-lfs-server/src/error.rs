//! Error types for the Git LFS HTTP gateway.

/// Result alias used by gateway startup and configuration code.
pub type Result<T> = std::result::Result<T, LfsServerError>;

/// Errors raised while constructing or running the Git LFS gateway.
#[derive(Debug, thiserror::Error)]
pub enum LfsServerError {
    /// Configuration was missing or invalid.
    #[error("configuration error: {0}")]
    Config(String),

    /// The configured object-store origin could not be built.
    #[error("origin configuration failed: {0}")]
    OriginConfig(String),

    /// An object-store operation failed.
    #[error(transparent)]
    Storage(#[from] crab_storage::StorageError),

    /// An LFS object operation failed.
    #[error(transparent)]
    Lfs(#[from] crab_lfs::LfsError),

    /// A lock operation failed.
    #[error(transparent)]
    Lock(#[from] crab_lfs::LfsLockError),

    /// A JSON request or response could not be encoded.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// TLS configuration or serving failed.
    #[error("TLS error: {0}")]
    Tls(String),

    /// The server could not bind or serve.
    #[error("server error: {0}")]
    Server(String),
}
