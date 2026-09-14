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
    #[error("Cell LTX publication failed")]
    Ltx(#[from] crab_ltx::CrabError),
    #[error("invalid Cell command: {0}")]
    Command(&'static str),
    #[error("request ID was already used for different command bytes")]
    RequestConflict,
    #[error("Cell has a local commit awaiting durable publication")]
    PendingPublication,
    #[error("Cell executor is fenced pending origin recovery")]
    Fenced,
    #[error("Cell runtime worker pool is closed")]
    RuntimeClosed,
    #[error("Cell is not active on its assigned worker")]
    CellNotActive,
    #[error("Cell is already active on its assigned worker")]
    CellAlreadyActive,
    #[error("Cell runtime capacity exhausted: {0}")]
    Capacity(&'static str),
    #[error("failed to start Cell SQL worker")]
    WorkerStart(#[source] Box<std::io::Error>),
}
