//! Shared publication mechanics; authentication and product policy stay with callers.
pub mod catalog;
pub mod generation;
pub mod journal;

/// Failure while preparing or publishing canonical Git metadata.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("ref {ref_name} no longer matches its expected old value at {path}")]
    RefChanged { ref_name: String, path: String },
    #[error("publication coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
    #[error("publication storage operation failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("publication metadata operation failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("Git pack evidence is invalid")]
    Git(#[from] crab_git::pack::PackError),
    #[error("publication file I/O failed")]
    Io(#[from] std::io::Error),
    #[error("publication worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("invalid manifest {field} hash")]
    ManifestHash {
        field: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("invalid pack identity")]
    PackIdentity {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("corrupt object at {path}: {reason}")]
    CorruptObject { path: String, reason: String },
    #[error("{0}")]
    Internal(String),
    #[error("publication cancelled")]
    Cancelled,
}

/// Shared publication result preserving dependency errors.
pub type Result<T> = std::result::Result<T, WriteError>;
