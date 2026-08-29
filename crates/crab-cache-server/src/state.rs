//! Shared application state and router construction for the cache service.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::time::Instant;
use tower::ServiceBuilder;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use tracing::warn;

use crate::auth;
use crate::auth::AuthPolicy;
use crate::cache_store::CacheStore;
use crate::chunk_index::ChunkIndex;
use crate::config::CacheServerConfig;
use crate::metrics::CacheMetrics;
use crate::origin_client::OriginClient;

pub const MAX_CACHE_OBJECT_BYTES: usize = 256 * 1024 * 1024;
const MAX_CONCURRENT_PUSH_WARMING_BODIES: usize = 8;

/// Per-object state shared by callers waiting for one origin fill.
pub struct CacheMissEntry {
    pub(crate) lock: tokio::sync::Mutex<()>,
    pub(crate) users: AtomicUsize,
}

impl CacheMissEntry {
    pub(crate) fn new() -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
            users: AtomicUsize::new(0),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DedupIndexRebuildStats {
    pub status: String,
    pub entries: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DedupIndexIngestionError {
    pub shard_hash: String,
    pub error: String,
}

/// Shared application state accessible from all handlers via `State<Arc<AppState>>`.
pub struct AppState {
    pub cache_store: Arc<CacheStore>,
    pub chunk_index: ChunkIndex,
    pub origin: OriginClient,
    pub config: CacheServerConfig,
    pub metrics: CacheMetrics,
    /// Authorization policy loaded from YAML. `None` means open access.
    pub policy: Option<AuthPolicy>,
    /// Notify handle for nudging the background evictor after writes.
    pub evictor_notify: Arc<tokio::sync::Notify>,
    /// Cached origin health status (optimistic: starts `true`).
    pub origin_healthy: AtomicBool,
    /// Timestamp of the last origin health probe. Initialized in the past
    /// to force the first `/v1/health` request to actually probe the origin.
    pub origin_health_checked_at: tokio::sync::Mutex<Instant>,
    /// Per-object cold-miss registrations. Coalesces concurrent misses so a
    /// single origin GET warms an immutable object while duplicate callers
    /// wait. The registry mutex is synchronous because its drop guard must
    /// remove a registration when an async request is cancelled.
    pub cache_miss_locks: std::sync::Mutex<HashMap<String, Arc<CacheMissEntry>>>,
    /// Bounds concurrent streamed bodies for push-warming PUT requests.
    pub push_warming_body_permits: tokio::sync::Semaphore,
    pub dedup_index_rebuild: DedupIndexRebuildStats,
    pub dedup_last_ingestion_error: tokio::sync::RwLock<Option<DedupIndexIngestionError>>,
}

impl AppState {
    /// Creates the semaphore that bounds streamed push-warming bodies.
    ///
    /// This remains public while the shipped binary constructs [`AppState`]
    /// through the compatibility Adapter in `crab`.
    pub fn push_warming_body_permits() -> tokio::sync::Semaphore {
        tokio::sync::Semaphore::new(MAX_CONCURRENT_PUSH_WARMING_BODIES)
    }
}

/// Build the axum router with all cache service endpoints and tower middleware.
///
/// Middleware stack (outermost → innermost):
/// - Concurrency limit (200 simultaneous requests)
/// - Request timeout (300 s) with HTTP 408 on expiry
/// - HTTP tracing via `tower-http`
/// - Auth middleware (extracts `ClientIdentity` into request extensions)
///
/// Health and metrics endpoints are mounted on a separate sub-router that
/// is merged *after* the authenticated routes, so they skip auth entirely.
pub fn build_router(state: Arc<AppState>) -> Router {
    // Authenticated routes — auth middleware runs before these handlers.
    let authenticated = Router::new()
        .route("/v1/capabilities", get(crate::handlers::capabilities))
        .route("/v1/authz/check", post(crate::handlers::authz_check))
        .route("/v1/dedup/query", post(crate::handlers::dedup_query))
        .route("/v1/admin/stats", get(crate::handlers::admin_stats))
        .route("/v1/admin/evict", post(crate::handlers::admin_evict))
        .route(
            "/v1/{*path}",
            get(crate::handlers::read_object)
                .head(crate::handlers::head_object)
                .put(crate::handlers::write_object),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ));

    // Unauthenticated routes — health and metrics skip auth.
    let public = Router::new()
        .route("/health", get(crate::handlers::health))
        .route("/health/live", get(crate::handlers::health_live))
        .route("/v1/health", get(crate::handlers::health))
        .route("/v1/health/live", get(crate::handlers::health_live))
        .route("/v1/metrics", get(crate::handlers::metrics_endpoint));

    public
        .merge(authenticated)
        .layer(
            ServiceBuilder::new()
                .layer(DefaultBodyLimit::max(MAX_CACHE_OBJECT_BYTES))
                .layer(RequestBodyLimitLayer::new(MAX_CACHE_OBJECT_BYTES))
                .concurrency_limit(200)
                .layer(axum::error_handling::HandleErrorLayer::new(
                    handle_middleware_error,
                ))
                .timeout(Duration::from_secs(300))
                .layer(TraceLayer::new_for_http()),
        )
        .with_state(state)
}

async fn handle_middleware_error(error: tower::BoxError) -> Response {
    if error.is::<tower::timeout::error::Elapsed>() {
        return StatusCode::REQUEST_TIMEOUT.into_response();
    }

    warn!(error = %error, "cache service middleware failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn middleware_timeout_error_maps_to_request_timeout() {
        let response =
            handle_middleware_error(Box::new(tower::timeout::error::Elapsed::new())).await;

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn unknown_middleware_error_maps_to_internal_server_error() {
        let response = handle_middleware_error(Box::new(std::io::Error::other("broken"))).await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
