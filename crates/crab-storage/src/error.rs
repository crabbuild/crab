//! Storage-domain errors for object-store helpers.

use std::time::Duration;

/// Result alias for storage helper operations.
pub type Result<T> = std::result::Result<T, StorageError>;

/// Errors raised by object-store transport helpers.
#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    /// Transient transport failure that may succeed on retry.
    #[error("network transient storage error: {source}")]
    NetworkTransient {
        #[source]
        source: object_store::Error,
    },

    /// Provider throttling, optionally carrying a server retry hint.
    #[error("storage throttled")]
    Throttled { retry_after: Option<Duration> },

    /// State-dependent conflict such as a failed compare-and-swap.
    #[error("storage state conflict: {path}")]
    StateConflict { path: String },

    /// Requested object was not found.
    #[error("object not found: {path}")]
    NotFound { path: String },

    /// Caller supplied an invalid content-addressed object hash.
    #[error("invalid storage object hash: {hash}")]
    InvalidHash { hash: String },

    /// Object bytes were readable but failed integrity verification.
    #[error("corrupt storage object {path}: {reason}")]
    CorruptObject { path: String, reason: String },

    /// Backend does not support the requested operation.
    #[error("object-store operation not supported: {source}")]
    NotSupported {
        #[source]
        source: object_store::Error,
    },

    /// Requested provider cannot be built as a cloud object store.
    #[error("unsupported storage provider for object-store construction: {provider:?}")]
    UnsupportedProvider {
        /// Unsupported provider kind.
        provider: crate::identity::StorageProviderKind,
    },

    /// Static-env target normalization rejected the supplied target shape.
    #[error("invalid static-env object-store target {target}: {reason}")]
    InvalidStaticEnvTarget {
        /// Target supplied by the caller.
        target: String,
        /// Validation failure reason.
        reason: String,
    },

    /// Static-env URL provider did not match the caller's expected provider.
    #[error("static-env URL provider mismatch for {bucket}: expected {expected:?}, got {actual:?}")]
    StaticEnvProviderMismatch {
        /// Expected provider selected by the caller's config.
        expected: crate::identity::StorageProviderKind,
        /// Actual provider implied by the raw URL scheme.
        actual: crate::identity::StorageProviderKind,
        /// Bucket or account named by the URL.
        bucket: String,
    },

    /// Provider-specific object-store builder rejected its configuration.
    #[error("failed to build {provider:?} object store for {bucket}: {source}")]
    ProviderConfig {
        /// Provider kind that failed to build.
        provider: crate::identity::StorageProviderKind,
        /// Bucket or container requested by the caller.
        bucket: String,
        #[source]
        source: object_store::Error,
    },

    /// URL could not be parsed for object-store construction.
    #[error("invalid object-store URL {url:?}: {source}")]
    InvalidObjectStoreUrl {
        /// URL supplied by the caller.
        url: String,
        #[source]
        source: url::ParseError,
    },

    /// URL-backed object-store construction failed.
    #[error("failed to build object store from URL {url:?}: {source}")]
    UrlStoreConfig {
        /// URL supplied by the caller.
        url: String,
        #[source]
        source: object_store::Error,
    },

    /// Credentials are present but rejected for this object.
    #[error("storage credentials rejected: {path}")]
    AuthFailed { path: String },

    /// Credentials are expired.
    #[error("storage credentials expired: {path}")]
    AuthExpired { path: String },

    /// No usable credentials were available.
    #[error("no storage credentials available")]
    NoCredentials,

    /// Credentials do not authorize this object.
    #[error("storage access forbidden: {path}")]
    Forbidden { path: String },

    /// I/O error from local transport plumbing.
    #[error("storage I/O error: {source}")]
    Io {
        #[source]
        source: std::io::Error,
    },

    /// Cancellation propagated from an upper operation.
    #[error("storage operation cancelled")]
    Cancelled,

    /// Raw object-store error that was not classified more specifically.
    #[error("object store error: {source}")]
    ObjectStore {
        #[source]
        source: object_store::Error,
    },

    /// Internal invariant failure in storage helper code.
    #[error("internal storage error: {0}")]
    Internal(String),
}

impl From<std::io::Error> for StorageError {
    fn from(source: std::io::Error) -> Self {
        Self::Io { source }
    }
}
