//! Coordination-domain errors.

/// Coordination-domain result alias.
pub type Result<T> = std::result::Result<T, CoordinationError>;

/// Errors raised by active-active write coordination contracts.
#[derive(Debug, thiserror::Error)]
pub enum CoordinationError {
    #[error("coordination storage operation failed for {path}: {source}")]
    #[cfg(feature = "object-store-lock")]
    ObjectStore {
        path: String,
        #[source]
        source: object_store::Error,
    },

    #[error("coordination CAS conflict on {path}")]
    CasConflict {
        path: String,
        expected_etag: Option<String>,
    },

    #[error("coordination non-fast-forward on {ref_name}: have {have}, want {want}")]
    NonFastForward {
        ref_name: String,
        have: String,
        want: String,
    },

    #[error("coordination state not found: {path}")]
    NotFound { path: String },

    #[error("coordination configuration error in {origin}: {key}")]
    Configuration { key: String, origin: String },

    #[error("coordination serialization failed for {context}: {source}")]
    Serialize {
        key: String,
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("push lock for {ref_name} is held by {holder}")]
    PushLockHeld {
        ref_name: String,
        holder: String,
        expires_at_unix: Option<u64>,
    },

    #[error("malformed push lock at {path}: {source}")]
    MalformedPushLock {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}
