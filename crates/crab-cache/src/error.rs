//! Error type for local and remote cache contracts.

/// Result alias for cache operations.
pub type Result<T> = std::result::Result<T, CacheError>;

/// Errors raised by cache contracts and local cache storage.
#[derive(thiserror::Error, Debug)]
pub enum CacheError {
    /// A caller cancelled a long-running cache scan or eviction.
    #[error("cache operation cancelled")]
    Cancelled,

    /// Filesystem I/O failed while reading or writing the local cache.
    #[error("cache I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The cache service returned an invalid or unsupported response.
    #[error("cache service error: {reason}")]
    Service { reason: String },

    /// A cache service HTTP request timed out.
    #[cfg(feature = "remote-client")]
    #[error("cache service request timed out: {url}")]
    ServiceRequestTimeout {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// The cache service could not be reached.
    #[cfg(feature = "remote-client")]
    #[error("cache service connection failed: {url}")]
    ServiceConnection {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// A cache service HTTP request failed outside narrower classes.
    #[cfg(feature = "remote-client")]
    #[error("cache service request failed for {url}: {source}")]
    ServiceRequest {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// The configured cache service CA certificate could not be read.
    #[error("failed to read CA cert {path}: {source}")]
    ReadCaCert {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The configured cache service CA certificate was not valid PEM.
    #[cfg(feature = "remote-client")]
    #[error("invalid PEM CA cert {path}: {source}")]
    InvalidCaCert {
        path: String,
        #[source]
        source: reqwest::Error,
    },

    /// A client certificate was configured without its private key.
    #[error("cache service client cert configured without client key")]
    MissingClientKey,

    /// The configured mTLS client certificate could not be read.
    #[error("failed to read cache service client cert {path}: {source}")]
    ReadClientCert {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The configured mTLS client key could not be read.
    #[error("failed to read cache service client key {path}: {source}")]
    ReadClientKey {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The configured mTLS client certificate and key could not form an identity.
    #[cfg(feature = "remote-client")]
    #[error("invalid cache service client cert/key {cert_path} {key_path}: {source}")]
    InvalidClientIdentity {
        cert_path: String,
        key_path: String,
        #[source]
        source: reqwest::Error,
    },

    /// The cache service HTTP client could not be built.
    #[cfg(feature = "remote-client")]
    #[error("failed to build HTTP client: {source}")]
    HttpClientBuild {
        #[source]
        source: reqwest::Error,
    },

    /// A content-addressed cache entry did not match its key.
    #[error("cache hash mismatch: requested {requested}, got {actual}")]
    HashMismatch { requested: String, actual: String },

    /// A serialized cached object is malformed or inconsistent.
    #[error("corrupt cache object at {path}: {reason}")]
    CorruptObject { path: String, reason: String },

    /// The local cache index could not be opened, queried, or updated.
    #[cfg(feature = "local-cache")]
    #[error("cache index error at {path}: {source}")]
    Index {
        path: String,
        #[source]
        source: rusqlite::Error,
    },

    /// A referenced chunk was not present in a cached xorb.
    #[error("cache chunk not found: {hash}")]
    ChunkNotFound { hash: String },

    /// A lower Xet data-plane operation failed without a narrower cache error.
    #[error("cache xet error: {source}")]
    Xet {
        #[source]
        source: crab_xet::error::XetError,
    },

    /// The xet-core on-disk range cache could not be initialized.
    #[error("xet chunk cache error at {path}: {source}")]
    XetChunkCache {
        path: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    /// The prefetch profile config was invalid.
    #[error("prefetch config error: {reason}")]
    PrefetchParse { reason: String },

    /// The requested prefetch profile is not present in the config.
    #[error("prefetch profile not found: {name}")]
    PrefetchProfileNotFound { name: String },

    #[error("internal cache error: {0}")]
    Internal(String),
}

impl From<crab_xet::error::XetError> for CacheError {
    fn from(error: crab_xet::error::XetError) -> Self {
        match error {
            crab_xet::error::XetError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_xet::error::XetError::ChunkNotFound { hash } => Self::ChunkNotFound { hash },
            source @ (crab_xet::error::XetError::Decompress { .. }
            | crab_xet::error::XetError::Compress { .. }
            | crab_xet::error::XetError::Layout { .. }
            | crab_xet::error::XetError::ShardFormat { .. }
            | crab_xet::error::XetError::IncompleteShardReconstruction { .. }
            | crab_xet::error::XetError::Internal(_)) => Self::Xet { source },
        }
    }
}
