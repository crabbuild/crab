//! Cache-service-specific error type with HTTP status code mapping.
//!
//! Separate from the CLI `CrabError` because the cache service is a standalone
//! HTTP server with its own error semantics.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Errors produced by the cache service.
///
/// Each variant maps to an HTTP status code via [`IntoResponse`].
#[derive(thiserror::Error, Debug)]
pub enum CacheServiceError {
    /// Object found in cache; used for internal flow control, not surfaced as an error.
    #[error("cache hit")]
    CacheHit,

    /// Object not found in cache.
    #[error("cache miss")]
    CacheMiss,

    /// Content hash does not match the expected hash.
    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    /// Origin object store is unreachable.
    #[error("origin unreachable: {reason}")]
    OriginUnreachable { reason: String },

    /// Object does not exist at origin (legitimate 404). Distinguished
    /// from `OriginUnreachable` so handlers can respond with 404 rather
    /// than 504.
    #[error("origin not found: {path}")]
    OriginNotFound { path: String },

    /// Local disk is full and emergency eviction could not free space.
    #[error("disk full: {reason}")]
    DiskFull { reason: String },

    /// Missing or invalid credentials.
    #[error("unauthorized: {reason}")]
    Unauthorized { reason: String },

    /// Valid credentials but insufficient access for the requested resource.
    #[error("forbidden: {reason}")]
    Forbidden { reason: String },

    /// Unexpected internal error.
    #[error("internal error: {0}")]
    InternalError(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Malformed request (e.g. mutable path in strict mode).
    #[error("bad request: {reason}")]
    BadRequest { reason: String },

    /// Configuration file is missing, malformed, or contains invalid values.
    #[error("config error: {0}")]
    ConfigError(String),
}

impl IntoResponse for CacheServiceError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::CacheHit => StatusCode::OK,
            Self::CacheMiss => StatusCode::NOT_FOUND,
            Self::HashMismatch { .. } => StatusCode::CONFLICT,
            Self::OriginUnreachable { .. } => StatusCode::GATEWAY_TIMEOUT,
            Self::OriginNotFound { .. } => StatusCode::NOT_FOUND,
            Self::DiskFull { .. } => StatusCode::INSUFFICIENT_STORAGE,
            Self::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
            Self::Forbidden { .. } => StatusCode::FORBIDDEN,
            Self::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::BadRequest { .. } => StatusCode::BAD_REQUEST,
            Self::ConfigError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = self.to_string();
        (status, body).into_response()
    }
}

/// Convenience alias for cache service results.
pub type Result<T> = std::result::Result<T, CacheServiceError>;
