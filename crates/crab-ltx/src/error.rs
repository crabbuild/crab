//! Errors retain their local I/O, SQLite, or codec cause.

use std::fmt;

/// Result of a local replication operation.
pub type Result<T> = std::result::Result<T, CrabError>;

/// Bounded resource that a capture, recovery, or replica step refused to exceed.
///
/// Callers map these classes onto their own capacity errors, so the variant —
/// not a message string — is the contract. `InvalidState` messages stay free
/// form because no caller dispatches on them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    /// One captured Cell bundle exceeded the bundle byte budget.
    CapturedCellBundleBytes,
    /// One captured bundle exceeded the bundle entry budget.
    BundleEntries,
    /// One bundle exceeded the bundle byte budget.
    BundleBytes,
    /// One bundle footer exceeded its byte budget.
    BundleFooter,
    /// One captured Cell LTX object exceeded the object byte budget.
    CapturedCellLtxBytes,
    /// One captured LTX object exceeded the object byte budget.
    CapturedLtxBytes,
    /// One captured Cell bundle exceeded the bundle byte budget.
    CellBundleBytes,
    /// The Cell root exceeded its byte budget.
    CellRootBytes,
    /// The Cell root exceeded its segment-count budget.
    CellRootSegments,
    /// One Cell scale-load batch exceeded its byte budget.
    CellScaleBytes,
    /// The Cell scale-load checksum exceeded its byte budget.
    CellScaleChecksumLength,
    /// The Cell scale-load run exceeded its sequence budget.
    CellScaleSequence,
    /// The Cell root descriptor pages exceeded their page budget.
    CellRootSegmentPages,
    /// One Cell segment page exceeded its byte budget.
    CellSegmentPageBytes,
    /// The checksum sidecar exceeded its byte budget.
    ChecksumFileBytes,
    /// A compaction body spool exceeded its byte budget.
    CompactionBodySpool,
    /// A compaction index spool exceeded its byte budget.
    CompactionIndexSpool,
    /// A compaction read set exceeded its input budget.
    CompactionInputs,
    /// The database exceeded its byte budget.
    DatabaseBytes,
    /// The database page size fell outside the supported range.
    DatabasePageSize,
    /// The database exceeded its page-count budget.
    DatabasePages,
    /// The host refused to charge the reserved host resource units.
    HostResourceUnits,
    /// The host refused to admit the requested local disk bytes.
    LocalDiskBytes,
    /// One LTX object exceeded the object byte budget.
    LtxBytes,
    /// One LTX file exceeded its byte budget.
    LtxFileBytes,
    /// The LTX page index exceeded its byte budget.
    LtxPageIndexBytes,
    /// One node frame body exceeded its byte budget.
    NodeFrameBody,
    /// One node frame exceeded its byte budget.
    NodeFrameBytes,
    /// The paged request queue exceeded its entry budget.
    PagedRequestQueue,
    /// A recovery plan exceeded its byte budget.
    PlanBytes,
    /// A recovery plan exceeded its segment budget.
    PlanSegments,
    /// Retained replica bytes exceeded the retained-bytes budget.
    RetainedBytes,
    /// Retained capture artifacts exceeded the session plan budget.
    RetainedCaptureArtifacts,
    /// Scratch files exceeded the scratch disk byte budget.
    ScratchDiskBytes,
}

impl LimitKind {
    /// Stable diagnostic text for this limit class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CapturedCellBundleBytes => "captured Cell bundle bytes",
            Self::BundleEntries => "bundle entries",
            Self::BundleBytes => "bundle bytes",
            Self::BundleFooter => "bundle footer",
            Self::CapturedCellLtxBytes => "captured Cell LTX bytes",
            Self::CapturedLtxBytes => "captured LTX bytes",
            Self::CellBundleBytes => "Cell bundle bytes",
            Self::CellRootBytes => "Cell root bytes",
            Self::CellRootSegments => "Cell root segments",
            Self::CellScaleBytes => "Cell scale bytes",
            Self::CellScaleChecksumLength => "Cell scale checksum length",
            Self::CellScaleSequence => "Cell scale sequence",
            Self::CellRootSegmentPages => "Cell root segment pages",
            Self::CellSegmentPageBytes => "Cell segment page bytes",
            Self::ChecksumFileBytes => "checksum file bytes",
            Self::CompactionBodySpool => "compaction body spool",
            Self::CompactionIndexSpool => "compaction index spool",
            Self::CompactionInputs => "compaction inputs",
            Self::DatabaseBytes => "database bytes",
            Self::DatabasePageSize => "database page size",
            Self::DatabasePages => "database pages",
            Self::HostResourceUnits => "host resource units",
            Self::LocalDiskBytes => "local disk bytes",
            Self::LtxBytes => "LTX bytes",
            Self::LtxFileBytes => "LTX file bytes",
            Self::LtxPageIndexBytes => "LTX page index bytes",
            Self::NodeFrameBody => "node frame body",
            Self::NodeFrameBytes => "node frame bytes",
            Self::PagedRequestQueue => "paged request queue",
            Self::PlanBytes => "plan bytes",
            Self::PlanSegments => "plan segments",
            Self::RetainedBytes => "retained bytes",
            Self::RetainedCaptureArtifacts => "retained capture artifacts; rotate session",
            Self::ScratchDiskBytes => "scratch disk bytes",
        }
    }
}

impl fmt::Display for LimitKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Failure from a managed transaction without erasing a handler's domain error.
///
/// `Operation` proves SQLite rolled the transaction back. `Sqlite` includes
/// begin, rollback, or commit failures; commit failures fence the writer because
/// their outcome can be ambiguous. `Admission` occurs before SQLite starts and
/// leaves the writer reusable. `Capture` occurs after a successful commit while
/// establishing the WAL cut required for later LTX capture.
#[derive(Debug, thiserror::Error)]
pub enum TransactionError<E: std::error::Error + 'static> {
    /// Resource admission failed before SQLite started; the writer stays usable.
    #[error("transaction resource admission failed")]
    Admission(#[source] CrabError),
    /// The caller's operation failed after SQLite rolled the transaction back.
    #[error("transaction operation failed")]
    Operation(#[source] E),
    /// SQLite rejected the transaction; an ambiguous commit fences the writer.
    #[error("SQLite transaction failed")]
    Sqlite(#[source] rusqlite::Error),
    /// The commit succeeded but its WAL cut could not be established.
    #[error("WAL capture boundary failed")]
    Capture(#[source] CrabError),
}

/// Failure from a read-only managed-database callback.
#[derive(Debug, thiserror::Error)]
pub enum QueryError<E: std::error::Error + 'static> {
    /// The caller's read-only operation failed.
    #[error("query operation failed")]
    Operation(#[source] E),
    /// SQLite rejected the read-only boundary.
    #[error("SQLite read-only boundary failed")]
    Sqlite(#[source] rusqlite::Error),
    /// The database cannot serve the query in its current state.
    #[error("managed database cannot serve the query")]
    State(#[source] CrabError),
}

/// Capture and recovery failures; none imply remote publication succeeded.
#[derive(Debug, thiserror::Error)]
pub enum CrabError {
    /// Object-store transport failed.
    #[cfg(feature = "replica")]
    #[error("object-store replication failure: {0}")]
    Storage(#[from] crab_storage::StorageError),
    /// Replica metadata was not valid JSON.
    #[cfg(feature = "replica")]
    #[error("invalid replica metadata: {0}")]
    Json(#[from] serde_json::Error),
    /// A replication task failed to join.
    #[cfg(feature = "replica")]
    #[error("replication task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// LTX data did not carry the checksum its lineage expects.
    #[error("LTX checksum mismatch")]
    ChecksumMismatch,
    /// LTX data failed structural validation.
    #[error("LTX file corrupted")]
    LTXCorrupted,
    /// A referenced LTX file does not exist.
    #[error("LTX file missing")]
    LTXMissing,
    /// A transaction was requested while none is open.
    #[error("transaction not available")]
    TxNotAvailable,
    /// Local I/O failed.
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite failed outside a caller-managed transaction.
    #[error("SQLite failure: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A configured admission bound was exceeded.
    #[error("resource limit exceeded: {0}")]
    Limit(LimitKind),
    /// Sparse page I/O exceeded its deadline.
    #[cfg(feature = "replica")]
    #[error("sparse page I/O exceeded its deadline")]
    Deadline,
    /// Local replication state is invalid; the message names the invariant.
    #[error("invalid local replication state: {0}")]
    InvalidState(&'static str),
    /// Capture was fenced; close the handle and restore an authoritative plan.
    #[error("capture failed; close this handle and restore an authoritative plan")]
    Fenced,
    /// Any other failure, kept boxed so the source survives.
    #[error("{0}")]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl CrabError {
    /// Reports whether compacting an existing Cell graph can admit an append.
    #[must_use]
    pub fn is_cell_graph_limit(&self) -> bool {
        matches!(
            self,
            Self::Limit(LimitKind::CellRootSegments | LimitKind::CellRootBytes)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{CrabError, LimitKind};

    #[test]
    fn only_cell_graph_admission_limits_are_compaction_retryable() {
        assert!(CrabError::Limit(LimitKind::CellRootSegments).is_cell_graph_limit());
        assert!(CrabError::Limit(LimitKind::CellRootBytes).is_cell_graph_limit());
        assert!(!CrabError::Limit(LimitKind::LtxFileBytes).is_cell_graph_limit());
        assert!(!CrabError::ChecksumMismatch.is_cell_graph_limit());
    }

    #[test]
    fn limit_kinds_keep_their_diagnostic_text() {
        assert_eq!(
            CrabError::Limit(LimitKind::LocalDiskBytes).to_string(),
            "resource limit exceeded: local disk bytes"
        );
        assert_eq!(LimitKind::CellRootBytes.as_str(), "Cell root bytes");
    }
}
