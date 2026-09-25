//! Errors retain their local I/O, SQLite, or codec cause.

use std::fmt;
use std::time::Duration;

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

    /// Classifies what a caller may do after this failure.
    ///
    /// The class is the contract: a caller decides between retrying, refusing
    /// the request, reconciling an ambiguous outcome, and fencing its handle
    /// from this value alone. No caller may dispatch on error messages.
    #[must_use]
    pub fn classify(&self) -> FailureClass {
        match self {
            #[cfg(feature = "replica")]
            Self::Storage(error) => storage_failure_class(crab_storage::retry_class(error)),
            #[cfg(feature = "replica")]
            Self::Json(_) => FailureClass::Permanent,
            #[cfg(feature = "replica")]
            Self::Task(_) => FailureClass::Ambiguous,
            Self::ChecksumMismatch
            | Self::LTXCorrupted
            | Self::LTXMissing
            | Self::TxNotAvailable => FailureClass::Permanent,
            Self::Io(error) => io_failure_class(error),
            Self::Sqlite(error) => sqlite_failure_class(error),
            Self::Limit(_) => FailureClass::Capacity,
            #[cfg(feature = "replica")]
            Self::Deadline => FailureClass::Retryable { after: None },
            Self::InvalidState(_) | Self::Fenced => FailureClass::Fenced,
            Self::Other(_) => FailureClass::Ambiguous,
        }
    }
}

/// What a caller may do after a [`CrabError`].
///
/// The class never depends on the failure text, so a caller can branch on it
/// across versions without matching strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    /// No acknowledged side effect exists, so the same request may be attempted
    /// again, no earlier than `after` when a provider named a delay.
    Retryable {
        /// Provider-requested minimum delay before the next attempt.
        after: Option<Duration>,
    },
    /// The request cannot succeed while its inputs or selected state stay the
    /// same; the caller must choose different inputs, artifacts, or state.
    Permanent,
    /// A declared bound or the environment refused the work before it could
    /// produce an acknowledged side effect. Freeing the resource or raising the
    /// bound is required: an identical retry fails the same way.
    Capacity,
    /// Work that may already have taken effect has an unknown outcome; the
    /// caller must reconcile before retrying.
    Ambiguous,
    /// The managed handle or session is fenced; close it and restore
    /// authoritative state before serving the Cell again.
    Fenced,
}

impl FailureClass {
    /// Reports whether a caller holding its own attempt budget may retry.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    /// Returns the provider's retry delay, when it named one.
    #[must_use]
    pub fn retry_after(self) -> Option<Duration> {
        match self {
            Self::Retryable { after } => after,
            _ => None,
        }
    }
}

#[cfg(feature = "replica")]
fn storage_failure_class(class: crab_storage::RetryClass) -> FailureClass {
    use crab_storage::RetryClass;
    match class {
        RetryClass::Transient | RetryClass::StateDependent | RetryClass::InspectErrno => {
            FailureClass::Retryable { after: None }
        }
        RetryClass::Throttled { retry_after } => FailureClass::Retryable { after: retry_after },
        // The storage layer already spent its one bounded retry for these
        // classes, so a caller that retried again could loop on a corrupt read.
        RetryClass::FatalAfterOneRetry | RetryClass::Fatal => FailureClass::Permanent,
    }
}

fn io_failure_class(error: &std::io::Error) -> FailureClass {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            FailureClass::Retryable { after: None }
        }
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded | ErrorKind::OutOfMemory => {
            FailureClass::Capacity
        }
        ErrorKind::NotFound
        | ErrorKind::PermissionDenied
        | ErrorKind::AlreadyExists
        | ErrorKind::InvalidInput
        | ErrorKind::InvalidData
        | ErrorKind::Unsupported => FailureClass::Permanent,
        // Any other I/O failure can leave a partially written artifact, so the
        // caller reconciles instead of assuming the operation did nothing.
        _ => FailureClass::Ambiguous,
    }
}

fn sqlite_failure_class(error: &rusqlite::Error) -> FailureClass {
    use rusqlite::ErrorCode;
    match error.sqlite_error_code() {
        Some(
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked | ErrorCode::OperationInterrupted,
        ) => FailureClass::Retryable { after: None },
        Some(ErrorCode::DiskFull | ErrorCode::OutOfMemory) => FailureClass::Capacity,
        // SQLite rolls a failed statement back, but the underlying file I/O
        // failure may have written a partial page.
        Some(ErrorCode::SystemIoFailure) => FailureClass::Ambiguous,
        // Every remaining SQLite failure is statement-atomic and repeatable with
        // the same inputs, so a caller must change the request or fence.
        Some(_) | None => FailureClass::Permanent,
    }
}

#[cfg(test)]
mod tests {
    use super::{CrabError, FailureClass, LimitKind};
    use std::io::ErrorKind;
    use std::time::Duration;

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

    #[test]
    fn state_selection_failures_are_permanent() {
        for error in [
            CrabError::ChecksumMismatch,
            CrabError::LTXCorrupted,
            CrabError::LTXMissing,
            CrabError::TxNotAvailable,
        ] {
            assert_eq!(error.classify(), FailureClass::Permanent, "{error}");
        }
    }

    #[test]
    fn declared_limits_are_capacity_refusals() {
        for kind in [
            LimitKind::LtxFileBytes,
            LimitKind::LocalDiskBytes,
            LimitKind::CapturedLtxBytes,
            LimitKind::HostResourceUnits,
        ] {
            assert_eq!(
                CrabError::Limit(kind).classify(),
                FailureClass::Capacity,
                "{kind}"
            );
        }
    }

    #[test]
    fn fenced_states_require_restoring_authoritative_state() {
        assert_eq!(CrabError::Fenced.classify(), FailureClass::Fenced);
        assert_eq!(
            CrabError::InvalidState("sessions cannot be reused").classify(),
            FailureClass::Fenced
        );
    }

    #[test]
    fn io_failures_separate_retryable_capacity_and_ambiguous_outcomes() {
        let cases = [
            (
                ErrorKind::Interrupted,
                FailureClass::Retryable { after: None },
            ),
            (
                ErrorKind::WouldBlock,
                FailureClass::Retryable { after: None },
            ),
            (ErrorKind::TimedOut, FailureClass::Retryable { after: None }),
            (ErrorKind::StorageFull, FailureClass::Capacity),
            (ErrorKind::QuotaExceeded, FailureClass::Capacity),
            (ErrorKind::OutOfMemory, FailureClass::Capacity),
            (ErrorKind::NotFound, FailureClass::Permanent),
            (ErrorKind::PermissionDenied, FailureClass::Permanent),
            (ErrorKind::AlreadyExists, FailureClass::Permanent),
            (ErrorKind::InvalidInput, FailureClass::Permanent),
            (ErrorKind::InvalidData, FailureClass::Permanent),
            (ErrorKind::Unsupported, FailureClass::Permanent),
            // A failed page write can leave a partial artifact behind.
            (ErrorKind::Other, FailureClass::Ambiguous),
        ];
        for (kind, expected) in cases {
            let error = CrabError::Io(std::io::Error::from(kind));
            assert_eq!(error.classify(), expected, "{kind:?}");
        }
    }

    #[test]
    fn sqlite_failures_classify_by_code_not_message() {
        use rusqlite::ffi;
        // Raw SQLite result codes, one of them extended, so the classification
        // is pinned to the wire code. `rusqlite::ErrorCode::DatabaseBusy as i32`
        // is the Rust enum discriminant (3 = SQLITE_PERM), never the result
        // code, so a caller must not use it to build a failure.
        let busy_snapshot = ffi::SQLITE_BUSY | (4 << 8);
        let cases = [
            (ffi::SQLITE_BUSY, FailureClass::Retryable { after: None }),
            (ffi::SQLITE_LOCKED, FailureClass::Retryable { after: None }),
            (
                ffi::SQLITE_INTERRUPT,
                FailureClass::Retryable { after: None },
            ),
            (ffi::SQLITE_FULL, FailureClass::Capacity),
            (ffi::SQLITE_NOMEM, FailureClass::Capacity),
            (ffi::SQLITE_IOERR, FailureClass::Ambiguous),
            (ffi::SQLITE_CORRUPT, FailureClass::Permanent),
            (ffi::SQLITE_READONLY, FailureClass::Permanent),
            (ffi::SQLITE_NOTADB, FailureClass::Permanent),
            (
                rusqlite::ErrorCode::DatabaseBusy as i32,
                FailureClass::Permanent,
            ),
        ];
        for (code, expected) in cases {
            let error =
                CrabError::Sqlite(rusqlite::Error::SqliteFailure(ffi::Error::new(code), None));
            assert_eq!(error.classify(), expected, "{code}");
        }

        let extended = CrabError::Sqlite(rusqlite::Error::SqliteFailure(
            ffi::Error::new(busy_snapshot),
            None,
        ));
        assert_eq!(
            extended.classify(),
            FailureClass::Retryable { after: None },
            "extended codes keep their primary code"
        );
    }

    #[test]
    fn unknown_failures_stay_ambiguous() {
        let error = CrabError::Other(Box::new(std::io::Error::other("unclassified")));
        assert_eq!(error.classify(), FailureClass::Ambiguous);
    }

    #[test]
    fn retry_classes_carry_their_provider_hint() {
        let hinted = FailureClass::Retryable {
            after: Some(Duration::from_millis(250)),
        };
        assert!(hinted.is_retryable());
        assert_eq!(hinted.retry_after(), Some(Duration::from_millis(250)));
        assert!(!FailureClass::Capacity.is_retryable());
        assert_eq!(FailureClass::Capacity.retry_after(), None);
    }
}
