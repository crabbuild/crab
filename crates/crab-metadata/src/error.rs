//! Metadata-domain errors for schema and index helpers.

/// Result alias for metadata helper operations.
pub type Result<T> = std::result::Result<T, MetadataError>;

/// Errors raised by metadata schema and local index helpers.
#[derive(thiserror::Error, Debug)]
pub enum MetadataError {
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
