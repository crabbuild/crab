//! Metadata-domain errors for schema and index helpers.

/// Result alias for metadata helper operations.
pub type Result<T> = std::result::Result<T, MetadataError>;

/// Errors raised by metadata schema and local index helpers.
#[derive(thiserror::Error, Debug)]
pub enum MetadataError {
    /// A file lookup could not acquire process-wide execution capacity.
    #[cfg(feature = "file-index-reader")]
    #[error("file lookup admission closed")]
    FileLookupAdmission {
        #[source]
        source: tokio::sync::AcquireError,
    },
    /// A file lookup's blocking parser failed to join.
    #[cfg(feature = "file-index-reader")]
    #[error("file lookup worker failed")]
    FileLookupWorker {
        #[source]
        source: tokio::task::JoinError,
    },
    /// A snapshot-bound file lookup exhausted its caller-owned budget.
    #[cfg(feature = "file-index-reader")]
    #[error("file lookup exceeds {resource} limit ({maximum})")]
    FileLookupLimit {
        resource: &'static str,
        maximum: usize,
    },
    /// Local filesystem operation failed.
    #[error("metadata I/O error: {source}")]
    Io {
        #[from]
        #[source]
        source: std::io::Error,
    },

    /// SQLite-backed metadata index operation failed.
    #[cfg(feature = "local-index")]
    #[error("{context}: {source}")]
    Sqlite {
        /// Operation context for the failing SQLite call.
        context: &'static str,
        #[source]
        source: rusqlite::Error,
    },

    /// Stored metadata bytes were malformed.
    #[error("corrupt metadata object {path}: {reason}")]
    CorruptObject { path: String, reason: String },

    /// Xet-backed metadata payload operation failed.
    #[error(transparent)]
    Xet {
        #[from]
        source: crab_xet::error::XetError,
    },

    /// Object-store transport failed while reading or writing metadata.
    #[cfg(feature = "storage")]
    #[error(transparent)]
    Storage {
        #[from]
        source: crab_storage::StorageError,
    },

    /// Publication was cancelled before attempting the active marker.
    #[cfg(feature = "storage")]
    #[error("ref journal publication cancelled before commit")]
    RefJournalCancelled,

    /// Durable attempt evidence prevents a new execution of this plan.
    #[cfg(feature = "storage")]
    #[error(
        "publication plan {plan_id} was already attempted; reconcile its outcome before starting another operation"
    )]
    PlanAlreadyAttempted { plan_id: String },

    /// The commit marker write failed without exact marker or compaction-frontier proof.
    #[cfg(feature = "storage")]
    #[error(
        "ref journal transaction {transaction_id} may have committed; reconcile durable commit evidence before retrying; current refs alone cannot prove the outcome"
    )]
    RefJournalCommitUncertain {
        transaction_id: String,
        #[source]
        source: Box<crab_storage::StorageError>,
        /// Readback failure, when the marker was not simply absent.
        verification: Option<Box<crab_storage::StorageError>>,
    },

    /// SlateDB metadata reader could not be opened.
    #[cfg(any(feature = "file-index-reader", feature = "remote-index"))]
    #[error("metadata database open failed for {db} at {path}: {source}")]
    SlateDbOpen {
        /// Logical metadata database name.
        db: String,
        /// Object-store path used for the database.
        path: String,
        #[source]
        source: slatedb::Error,
    },

    /// Object-store transport failed while publishing or checking remote-index metadata.
    #[cfg(feature = "remote-index")]
    #[error("metadata object-store operation failed: {source}")]
    ObjectStore {
        /// Preserved object-store source error.
        #[source]
        source: object_store::Error,
    },

    /// An immutable Git catalog proof outlived its exact SlateDB checkpoint.
    #[cfg(feature = "remote-index")]
    #[error("published Git catalog checkpoint {catalog_digest} is missing")]
    GitCatalogCheckpointMissing { catalog_digest: String },

    /// SlateDB metadata reader could not read a key.
    #[cfg(any(feature = "file-index-reader", feature = "remote-index"))]
    #[error("metadata database read failed for {db}: {source}")]
    SlateDbRead {
        /// Logical metadata database name.
        db: String,
        #[source]
        source: slatedb::Error,
    },

    /// SlateDB metadata writer could not commit a batch.
    #[cfg(feature = "remote-index")]
    #[error("metadata database write failed for {db}: {source}")]
    SlateDbWrite {
        /// Logical metadata database name.
        db: String,
        #[source]
        source: slatedb::Error,
    },

    /// SlateDB metadata reader could not close cleanly.
    #[cfg(any(feature = "file-index-reader", feature = "remote-index"))]
    #[error("metadata database close failed for {db}: {source}")]
    SlateDbClose {
        /// Logical metadata database name.
        db: String,
        #[source]
        source: slatedb::Error,
    },

    /// A SlateDB operation and the required database close both failed.
    #[cfg(feature = "remote-index")]
    #[error("metadata database operation failed for {db}: {operation}; close also failed: {close}")]
    SlateDbOperationAndClose {
        /// Logical metadata database name.
        db: String,
        /// Primary operation failure.
        #[source]
        operation: Box<MetadataError>,
        /// Typed close failure retained for diagnostics.
        close: slatedb::Error,
    },

    /// A manifest update was attempted but its response did not prove the outcome.
    #[cfg(feature = "storage")]
    #[error(
        "manifest update {candidate_digest} at {path} may have committed; reconcile durable evidence before retrying"
    )]
    ManifestCommitUncertain {
        path: String,
        candidate_digest: String,
        #[source]
        source: Box<crab_storage::StorageError>,
    },

    /// Manifest pointer could not be updated because another writer won the CAS.
    #[error("manifest CAS conflict at {path}")]
    ManifestCasConflict {
        path: String,
        expected_etag: Option<String>,
    },

    /// Internal invariant failure in metadata helper code.
    #[error("internal metadata error: {0}")]
    Internal(String),
}
