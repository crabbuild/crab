//! Errors retain their local I/O, SQLite, or codec cause.

/// Result of a local replication operation.
pub type Result<T> = std::result::Result<T, CrabError>;

/// Failure from a managed transaction without erasing a handler's domain error.
///
/// `Operation` proves SQLite rolled the transaction back. `Sqlite` includes
/// begin, rollback, or commit failures; commit failures fence the writer because
/// their outcome can be ambiguous. `Capture` occurs after a successful commit
/// while establishing the WAL cut required for later LTX capture.
#[derive(Debug, thiserror::Error)]
pub enum TransactionError<E: std::error::Error + 'static> {
    #[error("transaction operation failed")]
    Operation(#[source] E),
    #[error("SQLite transaction failed")]
    Sqlite(#[source] rusqlite::Error),
    #[error("WAL capture boundary failed")]
    Capture(#[source] CrabError),
}

/// Failure from a read-only managed-database callback.
#[derive(Debug, thiserror::Error)]
pub enum QueryError<E: std::error::Error + 'static> {
    #[error("query operation failed")]
    Operation(#[source] E),
    #[error("SQLite read-only boundary failed")]
    Sqlite(#[source] rusqlite::Error),
    #[error("managed database cannot serve the query")]
    State(#[source] CrabError),
}

/// Capture and recovery failures; none imply remote publication succeeded.
#[derive(Debug, thiserror::Error)]
pub enum CrabError {
    #[cfg(feature = "replica")]
    #[error("object-store replication failure: {0}")]
    Storage(#[from] crab_storage::StorageError),
    #[cfg(feature = "replica")]
    #[error("invalid replica metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[cfg(feature = "replica")]
    #[error("replication task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
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
    #[cfg(feature = "replica")]
    #[error("sparse page I/O exceeded its deadline")]
    Deadline,
    #[error("invalid local replication state: {0}")]
    InvalidState(&'static str),
    #[error("capture failed; close this handle and restore an authoritative plan")]
    Fenced,
    #[error("{0}")]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl CrabError {
    /// Reports whether compacting an existing Cell graph can admit an append.
    #[must_use]
    pub fn is_cell_graph_limit(&self) -> bool {
        matches!(self, Self::Limit("Cell root segments" | "Cell root bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::CrabError;

    #[test]
    fn only_cell_graph_admission_limits_are_compaction_retryable() {
        assert!(CrabError::Limit("Cell root segments").is_cell_graph_limit());
        assert!(CrabError::Limit("Cell root bytes").is_cell_graph_limit());
        assert!(!CrabError::Limit("LTX file bytes").is_cell_graph_limit());
        assert!(!CrabError::ChecksumMismatch.is_cell_graph_limit());
    }
}
