/// Result of a Cell runtime contract operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Identity, control-codec and schema failures with original causes retained.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Cell identity: {0}")]
    Identity(&'static str),
    #[error("invalid Cell control record: {0}")]
    Control(&'static str),
    #[error("Cell control JSON failed")]
    Json(#[from] serde_json::Error),
    #[error("Cell runtime SQLite schema failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Cell authority storage failed")]
    Storage(#[from] crab_storage::StorageError),
}
