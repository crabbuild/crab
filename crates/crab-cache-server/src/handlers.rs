//! Axum handler functions for cache service HTTP endpoints.

use std::collections::HashMap;
use std::error::Error;
use std::io::SeekFrom;
use std::io::Write as _;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{Ordering, Ordering::Relaxed};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use object_store::path::Path as ObjectPath;
use tempfile::TempPath;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use tracing::{debug, warn};

use crab_cache::path_class::{
    CacheObjectKind, CacheRouteContract, PathClass, cache_route_contract, classify_path,
    parse_cache_object_path, parse_mutable_repo_path,
};
use crab_xet::hash::{HashedWrite, MerkleHash};
use crab_xet::xorb::builder::FOOTER_SIZE;
use crab_xet::xorb::parser::XorbParser;
use crab_xet::xorb::parser::{
    verify_compressed_chunk, xorb_chunks_from_metadata, xorb_metadata_region,
};

use super::cache_store::{
    CacheRangeRead, CacheStats, CacheStore, EvictFilter, ObjectType, ServerObjectKey,
    TempPathCommitRecovery, parse_hash_hex,
};
use super::chunk_index::{ChunkLocation, DedupResult};
use super::config::{DedupScope, MutablePathMode};
use super::error::CacheServiceError;
use super::metrics::TrafficStats;
use super::origin_client::{ORIGIN_HEALTH_PROBE_PATH, origin_probe_reached_origin};
use super::state::{
    AppState, CacheMissEntry, DedupIndexIngestionError, DedupIndexRebuildStats,
    MAX_CACHE_OBJECT_BYTES,
};

use crate::auth::ClientIdentity;

const CACHE_STATUS_HEADER: HeaderName = HeaderName::from_static("x-cache");
const CACHE_HIT: &str = "HIT";
const CACHE_MISS: &str = "MISS";
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

struct CachedFetch {
    body: CachedFetchBody,
    cache_status: &'static str,
}

enum CachedFetchBody {
    Bytes(Bytes),
    OpenFile { file: std::fs::File, size: u64 },
    CachedFile { path: PathBuf, size: u64 },
}

impl CachedFetchBody {
    fn len_u64(&self) -> u64 {
        match self {
            Self::Bytes(data) => data.len() as u64,
            Self::OpenFile { size, .. } => *size,
            Self::CachedFile { size, .. } => *size,
        }
    }
}

struct FileBackedBody {
    temp_path: TempPath,
    size: u64,
    data_hash: Option<MerkleHash>,
}

#[derive(serde::Serialize)]
struct AdminStats {
    #[serde(flatten)]
    cache: CacheStats,
    limits: CacheServiceLimits,
    traffic: TrafficStats,
    dedup_index: DedupIndexStats,
}

#[derive(serde::Serialize)]
struct CacheServiceLimits {
    max_cache_bytes: u64,
    max_object_bytes: u64,
}

#[derive(serde::Serialize)]
struct Capabilities {
    schema: &'static str,
    limits: CacheServiceLimits,
    routes: CacheRouteContract,
}

#[derive(serde::Deserialize)]
pub struct AuthzCheckRequest {
    repo_path: String,
}

#[derive(serde::Serialize)]
struct AuthzCheckResponse {
    schema: &'static str,
    repo_path: String,
    policy_configured: bool,
    actions: AuthzActionDecisions,
}

#[derive(serde::Serialize)]
struct AuthzActionDecisions {
    read: bool,
    write: bool,
    dedup: bool,
    admin: bool,
}

#[derive(serde::Serialize)]
struct DedupIndexStats {
    indexed_chunks: u64,
    scope: String,
    requires_repo_context: bool,
    startup_rebuild: DedupIndexRebuildStats,
    last_ingestion_error: Option<DedupIndexIngestionError>,
}

// ---------------------------------------------------------------------------
// Dedup query request/response types
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/dedup/query`.
#[derive(serde::Deserialize)]
pub struct DedupQueryRequest {
    /// Repo prefix issuing the query, e.g. `org/team/repo`.
    repo_path: String,
    /// Hex-encoded 32-byte chunk hashes.
    chunk_hashes: Vec<String>,
}

/// Response body for `POST /v1/dedup/query`.
#[derive(serde::Serialize)]
pub struct DedupQueryResponse {
    known: Vec<KnownChunk>,
    unknown: Vec<usize>,
}

/// A chunk found in the dedup index with its xorb location.
#[derive(serde::Serialize)]
pub struct KnownChunk {
    index: usize,
    xorb_hash: String,
    chunk_index: u32,
    length: u32,
    cache_verified: bool,
}

/// `GET /v1/health` — readiness probe with origin connectivity check.
///
/// Returns 200 if the origin is reachable, 503 if not. Uses a cached
/// result with a 5-second TTL to avoid hammering the origin on every
/// probe. No authentication required — intended for Kubernetes readiness.
pub async fn health(State(state): State<Arc<AppState>>) -> (StatusCode, &'static str) {
    const FRESHNESS_TTL: Duration = Duration::from_secs(5);
    const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

    // Check if the cached result is still fresh.
    let now = tokio::time::Instant::now();
    let needs_probe = {
        let last_checked = state.origin_health_checked_at.lock().await;
        now.duration_since(*last_checked) >= FRESHNESS_TTL
    };

    if !needs_probe {
        // Return based on the cached health status.
        return if state.origin_healthy.load(Relaxed) {
            (StatusCode::OK, "ok")
        } else {
            (StatusCode::SERVICE_UNAVAILABLE, "origin unreachable")
        };
    }

    // Stale — probe the origin with a HEAD request and a 3-second timeout.
    let probe_path = ObjectPath::from(ORIGIN_HEALTH_PROBE_PATH);
    let probe_result = tokio::time::timeout(PROBE_TIMEOUT, state.origin.head(&probe_path)).await;

    let healthy = match probe_result {
        Ok(result) => {
            if origin_probe_reached_origin(&result) {
                true
            } else {
                if let Err(e) = result {
                    debug!(error = %e, "origin health probe failed");
                }
                false
            }
        }
        Err(_) => {
            debug!("origin health probe timed out");
            false
        }
    };

    // Update cached state.
    state.origin_healthy.store(healthy, Relaxed);
    {
        let mut last_checked = state.origin_health_checked_at.lock().await;
        *last_checked = tokio::time::Instant::now();
    }

    if healthy {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "origin unreachable")
    }
}

/// `GET /v1/health/live` — unconditional liveness probe.
///
/// Always returns 200. Used for Kubernetes liveness probes that should
/// not depend on origin availability.
pub async fn health_live() -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok")
}

/// `GET /v1/capabilities` — authenticated non-admin service limits.
pub async fn capabilities(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(Capabilities {
        schema: "crab-cache-service.capabilities.v1",
        limits: CacheServiceLimits {
            max_cache_bytes: state.cache_store.max_bytes(),
            max_object_bytes: MAX_CACHE_OBJECT_BYTES as u64,
        },
        routes: cache_route_contract(),
    })
}

/// `POST /v1/authz/check` — authenticated authorization diagnostics.
pub async fn authz_check(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<ClientIdentity>,
    axum::Json(req): axum::Json<AuthzCheckRequest>,
) -> Response {
    let repo_path = match normalize_repo_path(&req.repo_path) {
        Ok(repo_path) => repo_path,
        Err(e) => return e.into_response(),
    };

    let policy_configured = state.policy.is_some();
    let actions = match state.policy.as_ref() {
        Some(policy) => AuthzActionDecisions {
            read: policy.is_authorized(&identity.principal, &repo_path, "read"),
            write: policy.is_authorized(&identity.principal, &repo_path, "write"),
            dedup: policy.is_authorized(&identity.principal, &repo_path, "dedup"),
            admin: policy.has_action(&identity.principal, "admin"),
        },
        None => AuthzActionDecisions {
            read: true,
            write: true,
            dedup: true,
            admin: true,
        },
    };

    axum::Json(AuthzCheckResponse {
        schema: "crab-cache-service.authz-check.v1",
        repo_path,
        policy_configured,
        actions,
    })
    .into_response()
}

/// `GET /v1/metrics` — Prometheus exposition format.
///
/// Renders the current metrics snapshot via the installed Prometheus
/// recorder. Returns `Content-Type: text/plain; version=0.0.4; charset=utf-8`
/// per the Prometheus exposition format specification.
pub async fn metrics_endpoint(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let eviction_stats = state.cache_store.eviction_stats();
    let body = state.metrics.render_with_cache_store(
        state.cache_store.max_bytes(),
        MAX_CACHE_OBJECT_BYTES as u64,
        &eviction_stats,
    );
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// `GET /v1/{*path}` — read an immutable object (cache hit or miss + origin fetch).
pub async fn read_object(
    State(state): State<Arc<AppState>>,
    Path(wildcard): Path<String>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<ClientIdentity>,
) -> Response {
    let start = Instant::now();

    // Classify the full path (prepend /v1/ since the wildcard excludes it).
    let full_path = format!("/v1/{wildcard}");
    if classify_path(&full_path) == PathClass::Mutable {
        return handle_mutable_read(Arc::clone(&state), &wildcard, &identity).await;
    }

    let (bucket, repo_path, object_type, hash) = match parse_object_path(&wildcard) {
        Some(parsed) => parsed,
        None => {
            return CacheServiceError::BadRequest {
                reason: format!("invalid object path: {wildcard}"),
            }
            .into_response();
        }
    };

    if let Some(err) = check_repo_action(&state, &identity, &repo_path, "read") {
        return err;
    }

    let cache_key = ServerObjectKey {
        bucket,
        repo_path,
        object_type,
        hash: hash.clone(),
    };

    // Range header — serve from cache, fetching the full immutable object
    // through the cache server on cold miss so clients do not hit origin.
    match parse_range_header(&headers) {
        Ok(Some(range)) => {
            return serve_range(&state, &cache_key, &wildcard, &hash, start, range).await;
        }
        Ok(None) => {}
        Err(err) => return err.into_response(),
    }

    if cached_object_valid_for_read(&state, &cache_key, &hash) {
        if matches!(
            object_type,
            ObjectType::Pack | ObjectType::PackIndex | ObjectType::Metadata
        ) {
            // Keep large immutable cache hits file-backed so a pack does not
            // consume one response-sized allocation on the cache server.
            match state.cache_store.get_file(&cache_key) {
                Ok(Some(cached)) => {
                    state.metrics.record_cache_hit(object_type);
                    state
                        .metrics
                        .record_bytes_served(object_type, cached.size, true);
                    debug!(hash = %hash, size = cached.size, "cache hit");
                    return build_file_response(cached.file, cached.size, &hash, CACHE_HIT);
                }
                Ok(None) => {
                    debug!(hash = %hash, "cache miss, fetching from origin");
                }
                Err(e) => {
                    warn!(hash = %hash, error = %e, "cache store read error, falling through to origin");
                }
            }
        } else {
            match state.cache_store.get(&cache_key) {
                Ok(Some(data)) => {
                    state.metrics.record_cache_hit(object_type);
                    state
                        .metrics
                        .record_bytes_served(object_type, data.len() as u64, true);
                    debug!(hash = %hash, size = data.len(), "cache hit");
                    return build_ok_response(data, &hash, CACHE_HIT);
                }
                Ok(None) => {
                    debug!(hash = %hash, "cache miss, fetching from origin");
                }
                Err(e) => {
                    warn!(hash = %hash, error = %e, "cache store read error, falling through to origin");
                }
            }
        }
    } else {
        debug!(hash = %hash, "cached object failed identity check, fetching from origin");
    }

    // Cache miss — fetch from origin, verify, cache, return.
    fetch_and_cache(&state, &cache_key, &wildcard, &hash, start).await
}

/// `HEAD /v1/{*path}` — return immutable object metadata without a body.
pub async fn head_object(
    State(state): State<Arc<AppState>>,
    Path(wildcard): Path<String>,
    axum::Extension(identity): axum::Extension<ClientIdentity>,
) -> Response {
    let start = Instant::now();
    let full_path = format!("/v1/{wildcard}");
    if classify_path(&full_path) == PathClass::Mutable {
        return handle_mutable_head(&state, &wildcard, &identity).await;
    }

    let (bucket, repo_path, object_type, hash) = match parse_object_path(&wildcard) {
        Some(parsed) => parsed,
        None => {
            return CacheServiceError::BadRequest {
                reason: format!("invalid object path: {wildcard}"),
            }
            .into_response();
        }
    };

    if let Some(err) = check_repo_action(&state, &identity, &repo_path, "read") {
        return err;
    }

    let cache_key = ServerObjectKey {
        bucket,
        repo_path,
        object_type,
        hash: hash.clone(),
    };

    if cached_object_valid_for_read(&state, &cache_key, &hash) {
        match state.cache_store.get_range(&cache_key, 0..0) {
            Ok(Some(CacheRangeRead::Hit(cached))) => {
                state.metrics.record_cache_hit(object_type);
                debug!(hash = %hash, size = cached.total_size, "head hit");
                return build_head_response(cached.total_size, &hash, CACHE_HIT);
            }
            Ok(Some(CacheRangeRead::Unsatisfiable { total_size })) => {
                state.metrics.record_cache_hit(object_type);
                return build_head_response(total_size, &hash, CACHE_HIT);
            }
            Ok(None) => {
                debug!(hash = %hash, "head miss, checking origin");
            }
            Err(e) => {
                warn!(hash = %hash, error = %e, "cache metadata read error, checking origin");
            }
        }
    } else {
        debug!(hash = %hash, "cached object failed identity check for head, checking origin");
    }

    let origin_path = ObjectPath::from(wildcard.to_string());
    let result = state.origin.head(&origin_path).await;
    state
        .metrics
        .record_origin_head(object_type, start.elapsed().as_secs_f64() * 1000.0);
    match result {
        Ok(meta) => {
            state.metrics.record_cache_miss(object_type);
            build_head_response(meta.size, &hash, CACHE_MISS)
        }
        Err(CacheServiceError::OriginUnreachable { reason }) => {
            warn!(path = %wildcard, reason = %reason, "origin unreachable for head miss");
            (StatusCode::GATEWAY_TIMEOUT, reason).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// `PUT /v1/{*path}` — push-through cache warming.
///
/// Accepts the full object body and stores it in the local cache under the
/// object identity from the URL path.
pub async fn write_object(
    State(state): State<Arc<AppState>>,
    Path(wildcard): Path<String>,
    axum::Extension(identity): axum::Extension<ClientIdentity>,
    body: Body,
) -> Response {
    // Mutable paths are never writable through the cache.
    let full_path = format!("/v1/{wildcard}");
    if classify_path(&full_path) == PathClass::Mutable {
        state.metrics.record_mutable_write_rejection();
        return CacheServiceError::BadRequest {
            reason: "mutable path \u{2014} writes not accepted".to_string(),
        }
        .into_response();
    }

    let (bucket, repo_path, object_type, hash) = match parse_object_path(&wildcard) {
        Some(parsed) => parsed,
        None => {
            return CacheServiceError::BadRequest {
                reason: format!("invalid object path: {wildcard}"),
            }
            .into_response();
        }
    };

    if let Some(err) = check_repo_action(&state, &identity, &repo_path, "write") {
        return err;
    }

    let cache_key = ServerObjectKey {
        bucket,
        repo_path,
        object_type,
        hash,
    };
    let _body_permit = match state.push_warming_body_permits.acquire().await {
        Ok(permit) => permit,
        Err(_) => {
            warn!("push-warming body semaphore closed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    write_file_backed_object(&state, cache_key, body).await
}

async fn write_file_backed_object(
    state: &AppState,
    cache_key: ServerObjectKey,
    body: Body,
) -> Response {
    let staged = match stream_push_warming_body_to_temp(&state.cache_store, &cache_key, body).await
    {
        Ok(staged) => staged,
        Err(response) => return response,
    };
    if let Err(e) = validate_staged_cache_object(&cache_key, &staged).await {
        return e.into_response();
    }

    match state
        .cache_store
        .would_exceed_budget_after_put(&cache_key, staged.size)
    {
        Ok(true) => {
            debug!(hash = %cache_key.hash, "cache over budget before streamed push-warm commit, attempting emergency eviction");
            if let Err(e) = state.cache_store.emergency_evict() {
                warn!(hash = %cache_key.hash, error = %e, "emergency eviction failed before streamed push-warm commit");
            }
        }
        Ok(false) => {}
        Err(e) => {
            warn!(hash = %cache_key.hash, error = %e, "failed to estimate streamed push-warm cache growth");
        }
    }

    match state
        .cache_store
        .put_unverified_temp_path(&cache_key, staged.temp_path, staged.size)
    {
        Ok(()) => {
            if cache_key.object_type == ObjectType::Shard {
                ingest_committed_shard(state, &cache_key).await;
            }
            record_push_warming_success(state, &cache_key, staged.size);
            StatusCode::CREATED.into_response()
        }
        Err(CacheServiceError::DiskFull { .. }) => {
            warn!(hash = %cache_key.hash, "skipping streamed push-warm cache write — cache remains full");
            StatusCode::CREATED.into_response()
        }
        Err(e) => e.into_response(),
    }
}

fn record_push_warming_success(state: &AppState, cache_key: &ServerObjectKey, bytes: u64) {
    state
        .metrics
        .record_push_warming(cache_key.object_type, bytes);
    state
        .metrics
        .set_bytes_stored(state.cache_store.current_bytes());

    let high_water = (state.cache_store.max_bytes() as f64 * state.config.high_water_ratio) as u64;
    if state.cache_store.current_bytes() > high_water {
        state.evictor_notify.notify_one();
    }
}

async fn stream_push_warming_body_to_temp(
    store: &CacheStore,
    key: &ServerObjectKey,
    body: Body,
) -> std::result::Result<FileBackedBody, Response> {
    stream_chunks_to_temp(
        store,
        key,
        body.into_data_stream(),
        Some(MAX_CACHE_OBJECT_BYTES as u64),
        push_warming_body_error_response,
    )
    .await
}

async fn stream_origin_body_to_temp(
    store: &CacheStore,
    key: &ServerObjectKey,
    get_result: object_store::GetResult,
) -> std::result::Result<FileBackedBody, Response> {
    stream_chunks_to_temp(
        store,
        key,
        get_result.into_stream(),
        None,
        origin_body_error_response,
    )
    .await
}

async fn stream_chunks_to_temp<S, E, F>(
    store: &CacheStore,
    key: &ServerObjectKey,
    stream: S,
    max_bytes: Option<u64>,
    map_chunk_error: F,
) -> std::result::Result<FileBackedBody, Response>
where
    S: Stream<Item = std::result::Result<Bytes, E>>,
    F: Fn(E) -> Response,
{
    let temp_path = store
        .create_temp_object_path(key)
        .map_err(IntoResponse::into_response)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(temp_path.as_ref() as &std::path::Path)
        .await
        .map_err(|e| push_warming_temp_io_error("open", e))?;

    let mut size = 0u64;
    let mut data_hash =
        (key.object_type == ObjectType::Shard).then(|| HashedWrite::new(std::io::sink()));
    futures_util::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(&map_chunk_error)?;
        let next_size = size.checked_add(chunk.len() as u64).ok_or_else(|| {
            CacheServiceError::BadRequest {
                reason: "push-warming body size overflow".to_string(),
            }
            .into_response()
        })?;
        if max_bytes.is_some_and(|max_bytes| next_size > max_bytes) {
            return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
        }

        if let Some(hasher) = data_hash.as_mut() {
            hasher
                .write_all(&chunk)
                .map_err(|e| push_warming_temp_io_error("hash", e))?;
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| push_warming_temp_io_error("write", e))?;
        size = next_size;
    }
    file.flush()
        .await
        .map_err(|e| push_warming_temp_io_error("flush", e))?;
    drop(file);

    Ok(FileBackedBody {
        temp_path,
        size,
        data_hash: data_hash.as_ref().map(HashedWrite::hash),
    })
}

async fn verify_streamed_xorb_body(
    path: &FsPath,
    size: u64,
    expected_hash: &str,
) -> std::result::Result<(), CacheServiceError> {
    let expected =
        MerkleHash::from_hex(expected_hash).map_err(|_| CacheServiceError::BadRequest {
            reason: format!("invalid xorb hash: {expected_hash}"),
        })?;
    let size = usize::try_from(size).map_err(|_| CacheServiceError::BadRequest {
        reason: format!("xorb body too large to verify: {size} bytes"),
    })?;
    if size < FOOTER_SIZE {
        return Err(CacheServiceError::HashMismatch {
            expected: expected.hex(),
            actual: "invalid xorb: xorb too small for footer".to_string(),
        });
    }

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| push_warming_temp_io_cache_error("open", e))?;
    let footer_start = u64::try_from(size - FOOTER_SIZE)
        .map_err(|_| CacheServiceError::InternalError("xorb footer offset overflow".into()))?;
    file.seek(SeekFrom::Start(footer_start))
        .await
        .map_err(|e| push_warming_temp_io_cache_error("seek footer", e))?;
    let mut footer = vec![0u8; FOOTER_SIZE];
    file.read_exact(&mut footer)
        .await
        .map_err(|e| push_warming_temp_io_cache_error("read footer", e))?;

    let region =
        xorb_metadata_region(size, &footer).map_err(|e| CacheServiceError::HashMismatch {
            expected: expected.hex(),
            actual: format!("invalid xorb: {e}"),
        })?;
    let metadata_offset = u64::try_from(region.offset)
        .map_err(|_| CacheServiceError::InternalError("xorb metadata offset overflow".into()))?;
    file.seek(SeekFrom::Start(metadata_offset))
        .await
        .map_err(|e| push_warming_temp_io_cache_error("seek metadata", e))?;
    let mut metadata = vec![0u8; region.len];
    file.read_exact(&mut metadata)
        .await
        .map_err(|e| push_warming_temp_io_cache_error("read metadata", e))?;

    let (chunks, actual) = xorb_chunks_from_metadata(size, &footer, &metadata).map_err(|e| {
        CacheServiceError::HashMismatch {
            expected: expected.hex(),
            actual: format!("invalid xorb: {e}"),
        }
    })?;
    if actual != expected {
        return Err(CacheServiceError::HashMismatch {
            expected: expected.hex(),
            actual: actual.hex(),
        });
    }

    for chunk in chunks {
        let offset = u64::from(chunk.offset);
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| push_warming_temp_io_cache_error("seek chunk", e))?;
        let mut compressed = vec![0u8; chunk.compressed_len as usize];
        file.read_exact(&mut compressed)
            .await
            .map_err(|e| push_warming_temp_io_cache_error("read chunk", e))?;
        verify_compressed_chunk(&chunk, &compressed).map_err(|e| {
            CacheServiceError::HashMismatch {
                expected: expected.hex(),
                actual: format!("invalid xorb payload: {e}"),
            }
        })?;
    }

    Ok(())
}

async fn validate_staged_cache_object(
    key: &ServerObjectKey,
    staged: &FileBackedBody,
) -> std::result::Result<(), CacheServiceError> {
    match key.object_type {
        ObjectType::Xorb => {
            verify_streamed_xorb_body(staged.temp_path.as_ref(), staged.size, &key.hash).await
        }
        ObjectType::Shard => {
            let expected = key.hash.to_ascii_lowercase();
            let actual = staged.data_hash.map(|hash| hash.hex()).unwrap_or_default();
            if actual == expected {
                Ok(())
            } else {
                Err(CacheServiceError::HashMismatch { expected, actual })
            }
        }
        ObjectType::Pack | ObjectType::PackIndex | ObjectType::Metadata => Ok(()),
    }
}

fn push_warming_temp_io_cache_error(
    operation: &'static str,
    error: std::io::Error,
) -> CacheServiceError {
    warn!(operation, error = %error, "failed to stream push-warming body to temp file");
    CacheServiceError::InternalError(
        format!("failed to {operation} push-warming temp file: {error}").into(),
    )
}

fn push_warming_temp_io_error(operation: &'static str, error: std::io::Error) -> Response {
    push_warming_temp_io_cache_error(operation, error).into_response()
}

fn push_warming_body_error_response(error: axum::Error) -> Response {
    let mut source = Error::source(&error);
    while let Some(err) = source {
        if err.is::<http_body_util::LengthLimitError>() {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        source = err.source();
    }

    warn!(error = %error, "failed to read push-warming body");
    StatusCode::BAD_REQUEST.into_response()
}

fn origin_body_error_response(error: object_store::Error) -> Response {
    warn!(error = %error, "failed to read origin response body");
    CacheServiceError::InternalError(format!("failed to read origin response: {error}").into())
        .into_response()
}

/// `POST /v1/dedup/query` — batch chunk dedup lookup.
///
/// Accepts a JSON body with the repo prefix plus hex-encoded chunk hashes,
/// queries the chunk index, and returns known/unknown partitions.
pub async fn dedup_query(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<ClientIdentity>,
    axum::Json(req): axum::Json<DedupQueryRequest>,
) -> Response {
    let start = Instant::now();

    if req.chunk_hashes.len() > 100_000 {
        return CacheServiceError::BadRequest {
            reason: "dedup query limited to 100,000 chunk hashes per request".to_string(),
        }
        .into_response();
    }

    let repo_path = match normalize_repo_path(&req.repo_path) {
        Ok(repo_path) => repo_path,
        Err(e) => return e.into_response(),
    };

    if let Some(err) = check_repo_action(&state, &identity, &repo_path, "dedup") {
        return err;
    }

    // Parse hex-encoded hashes into [u8; 32] arrays.
    let mut hashes = Vec::with_capacity(req.chunk_hashes.len());
    for (i, hex) in req.chunk_hashes.iter().enumerate() {
        match parse_hash_hex(hex) {
            Some(h) => hashes.push(h),
            None => {
                return CacheServiceError::BadRequest {
                    reason: format!("invalid hex hash at index {i}: {hex}"),
                }
                .into_response();
            }
        }
    }

    if let Some(err) = dedup_scope_error(&state.config.dedup_scope, &repo_path) {
        return err.into_response();
    }

    // Query the chunk index.
    let result = match state.chunk_index.query_batch(&hashes) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };

    let response = build_verified_dedup_response(&state, &hashes, result);

    let latency_ms = start.elapsed().as_millis() as f64;
    state.metrics.record_dedup_query(
        latency_ms,
        response.known.len() as u64,
        response.unknown.len() as u64,
    );

    axum::Json(response).into_response()
}

fn build_verified_dedup_response(
    state: &AppState,
    hashes: &[[u8; 32]],
    result: DedupResult,
) -> DedupQueryResponse {
    let mut known = Vec::with_capacity(result.known.len());
    let mut unknown = result.unknown;
    let mut cached_xorbs: HashMap<[u8; 32], Option<XorbParser>> = HashMap::new();

    for (index, loc) in result.known {
        let Some(chunk_hash) = hashes.get(index).copied() else {
            warn!(
                index,
                candidates = hashes.len(),
                "dedup index returned an out-of-range chunk index"
            );
            continue;
        };

        let parser = cached_xorbs
            .entry(loc.xorb_hash)
            .or_insert_with(|| load_verified_cached_xorb(state, loc.xorb_hash));
        let Some(parser) = parser.as_ref() else {
            unknown.push(index);
            continue;
        };

        if cached_xorb_verifies_chunk(parser, chunk_hash, &loc) {
            known.push(KnownChunk {
                index,
                xorb_hash: MerkleHash::from(loc.xorb_hash).hex(),
                chunk_index: loc.chunk_index,
                length: loc.length,
                cache_verified: true,
            });
        } else {
            unknown.push(index);
        }
    }

    unknown.sort_unstable();
    DedupQueryResponse { known, unknown }
}

fn load_verified_cached_xorb(state: &AppState, xorb_hash: [u8; 32]) -> Option<XorbParser> {
    let expected = MerkleHash::from(xorb_hash);
    let hash_hex = expected.hex();
    let key = ServerObjectKey {
        bucket: String::new(),
        repo_path: ".crab".to_string(),
        object_type: ObjectType::Xorb,
        hash: hash_hex.clone(),
    };

    let data = match state.cache_store.get(&key) {
        Ok(Some(data)) => data,
        Ok(None) => {
            debug!(xorb_hash = %hash_hex, "dedup xorb proof missing from cache");
            return None;
        }
        Err(e) => {
            warn!(xorb_hash = %hash_hex, error = %e, "dedup xorb proof cache read failed");
            return None;
        }
    };

    let parser = match XorbParser::parse(data) {
        Ok(parser) => parser,
        Err(e) => {
            warn!(xorb_hash = %hash_hex, error = %e, "dedup xorb proof parse failed");
            return None;
        }
    };

    if parser.hash() != expected {
        warn!(
            xorb_hash = %hash_hex,
            actual = %parser.hash().hex(),
            "dedup xorb proof has wrong aggregate hash"
        );
        return None;
    }

    Some(parser)
}

fn cached_xorb_verifies_chunk(
    parser: &XorbParser,
    chunk_hash: [u8; 32],
    loc: &ChunkLocation,
) -> bool {
    let expected_chunk = MerkleHash::from(chunk_hash);
    let xorb_hash = MerkleHash::from(loc.xorb_hash);

    let chunk_meta = match parser.chunk_meta(loc.chunk_index) {
        Ok(chunk_meta) => chunk_meta,
        Err(e) => {
            warn!(
                chunk_hash = %expected_chunk.hex(),
                xorb_hash = %xorb_hash.hex(),
                chunk_index = loc.chunk_index,
                error = %e,
                "dedup xorb proof points past cached xorb metadata"
            );
            return false;
        }
    };

    if chunk_meta.hash != expected_chunk || chunk_meta.uncompressed_len != loc.length {
        warn!(
            chunk_hash = %expected_chunk.hex(),
            xorb_hash = %xorb_hash.hex(),
            chunk_index = loc.chunk_index,
            actual_hash = %chunk_meta.hash.hex(),
            actual_size = chunk_meta.uncompressed_len,
            expected_size = loc.length,
            "dedup xorb proof metadata does not match chunk"
        );
        return false;
    }

    match parser.get_chunk(loc.chunk_index) {
        Ok(chunk) if chunk.data.len() == loc.length as usize => true,
        Ok(chunk) => {
            warn!(
                chunk_hash = %expected_chunk.hex(),
                xorb_hash = %xorb_hash.hex(),
                chunk_index = loc.chunk_index,
                actual_size = chunk.data.len(),
                expected_size = loc.length,
                "dedup xorb proof decompressed to the wrong size"
            );
            false
        }
        Err(e) => {
            warn!(
                chunk_hash = %expected_chunk.hex(),
                xorb_hash = %xorb_hash.hex(),
                chunk_index = loc.chunk_index,
                error = %e,
                "dedup xorb proof failed chunk decompression"
            );
            false
        }
    }
}

fn dedup_scope_label(scope: &DedupScope) -> String {
    match scope {
        DedupScope::All => "all".to_string(),
        DedupScope::BucketPrefix(prefix) => format!("bucket-prefix:{prefix}"),
        DedupScope::Repos(repos) => format!("repos:{}", repos.join(",")),
    }
}

fn dedup_scope_error(scope: &DedupScope, repo_path: &str) -> Option<CacheServiceError> {
    let allowed = match scope {
        DedupScope::All => true,
        DedupScope::BucketPrefix(prefix) => repo_matches_scope(repo_path, prefix),
        DedupScope::Repos(repos) => repos
            .iter()
            .any(|candidate| repo_matches_scope(repo_path, candidate)),
    };
    (!allowed).then(|| CacheServiceError::Forbidden {
        reason: format!(
            "dedup scope {} does not allow repo {repo_path}",
            dedup_scope_label(scope)
        ),
    })
}

fn repo_matches_scope(repo_path: &str, scope_prefix: &str) -> bool {
    let scope_prefix = scope_prefix.trim_matches('/');
    repo_path == scope_prefix
        || repo_path
            .strip_prefix(scope_prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn normalize_repo_path(repo_path: &str) -> std::result::Result<String, CacheServiceError> {
    let repo_path = repo_path.trim().trim_matches('/');
    if repo_path.is_empty() {
        return Err(CacheServiceError::BadRequest {
            reason: "repo_path is required".to_string(),
        });
    }
    if repo_path.len() > 1024 {
        return Err(CacheServiceError::BadRequest {
            reason: "repo_path exceeds 1024 bytes".to_string(),
        });
    }
    if repo_path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(CacheServiceError::BadRequest {
            reason: format!("invalid repo_path: {repo_path}"),
        });
    }
    Ok(repo_path.to_string())
}

/// `GET /v1/admin/stats` — cache statistics (admin auth required).
pub async fn admin_stats(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<ClientIdentity>,
) -> Response {
    if let Some(err) = check_action(&state, &identity, "admin") {
        return err;
    }
    match state.cache_store.stats() {
        Ok(cache) => {
            let inflight_misses = cache_miss_lock_count(&state);
            state.metrics.set_inflight_misses(inflight_misses);
            let indexed_chunks = match state.chunk_index.len() {
                Ok(count) => count,
                Err(e) => return e.into_response(),
            };
            let last_ingestion_error = state.dedup_last_ingestion_error.read().await.clone();
            axum::Json(AdminStats {
                cache,
                limits: CacheServiceLimits {
                    max_cache_bytes: state.cache_store.max_bytes(),
                    max_object_bytes: MAX_CACHE_OBJECT_BYTES as u64,
                },
                traffic: state.metrics.snapshot(),
                dedup_index: DedupIndexStats {
                    indexed_chunks,
                    scope: dedup_scope_label(&state.config.dedup_scope),
                    requires_repo_context: !matches!(state.config.dedup_scope, DedupScope::All),
                    startup_rebuild: state.dedup_index_rebuild.clone(),
                    last_ingestion_error,
                },
            })
            .into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// Request body for `POST /v1/admin/evict`.
#[derive(serde::Deserialize)]
pub struct AdminEvictRequest {
    /// Filter by object type: "xorb", "shard", "pack", "pack-index".
    object_type: Option<String>,
    /// Evict exactly one immutable cache-service object path.
    path: Option<String>,
}

/// `POST /v1/admin/evict` — manual cache eviction (admin auth required).
pub async fn admin_evict(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<ClientIdentity>,
    axum::Json(req): axum::Json<AdminEvictRequest>,
) -> Response {
    if let Some(err) = check_action(&state, &identity, "admin") {
        return err;
    }
    if req.object_type.is_some() && req.path.is_some() {
        return CacheServiceError::BadRequest {
            reason: "admin evict accepts either object_type or path, not both".to_string(),
        }
        .into_response();
    }
    if let Some(path) = req.path.as_deref() {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return CacheServiceError::BadRequest {
                reason: "admin evict path must not be empty".to_string(),
            }
            .into_response();
        }
        let (bucket, repo_path, object_type, hash) = match parse_object_path(trimmed) {
            Some(parsed) => parsed,
            None => {
                return CacheServiceError::BadRequest {
                    reason: format!("invalid object path: {trimmed}"),
                }
                .into_response();
            }
        };
        let key = ServerObjectKey {
            bucket,
            repo_path,
            object_type,
            hash,
        };
        return match state.cache_store.evict_key(&key) {
            Ok(stats) => axum::Json(stats).into_response(),
            Err(e) => e.into_response(),
        };
    }

    let object_type = match &req.object_type {
        Some(name) => match ObjectType::from_name(name) {
            Some(ot) => Some(ot),
            None => {
                return CacheServiceError::BadRequest {
                    reason: format!("unknown object type: {name}"),
                }
                .into_response();
            }
        },
        None => None,
    };

    let filter = EvictFilter { object_type };

    match state.cache_store.evict_by_filter(&filter) {
        Ok(stats) => axum::Json(stats).into_response(),
        Err(e) => e.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check that the principal is authorized for `action` on `repo_path`.
/// Returns `None` if authorized (or no policy configured), `Some(Response)`
/// with HTTP 403 if denied.
fn check_repo_action(
    state: &AppState,
    identity: &ClientIdentity,
    repo_path: &str,
    action: &str,
) -> Option<Response> {
    let policy = state.policy.as_ref()?;
    if policy.is_authorized(&identity.principal, repo_path, action) {
        None
    } else {
        debug!(
            principal = %identity.principal,
            repo_path = %repo_path,
            action = %action,
            "authorization denied"
        );
        Some(
            CacheServiceError::Forbidden {
                reason: format!(
                    "{} not authorized for {action} on {repo_path}",
                    identity.principal
                ),
            }
            .into_response(),
        )
    }
}

/// Check that the principal has the given action (not repo-scoped).
/// Returns `None` if authorized (or no policy configured), `Some(Response)`
/// with HTTP 403 if denied.
fn check_action(state: &AppState, identity: &ClientIdentity, action: &str) -> Option<Response> {
    let policy = state.policy.as_ref()?;
    if policy.has_action(&identity.principal, action) {
        None
    } else {
        debug!(
            principal = %identity.principal,
            action = %action,
            "authorization denied"
        );
        Some(
            CacheServiceError::Forbidden {
                reason: format!("{} not authorized for {action}", identity.principal),
            }
            .into_response(),
        )
    }
}

/// Handle a GET for a mutable path according to the configured mode.
///
/// Strict mode rejects with HTTP 400; transparent mode proxies to origin
/// without caching.
async fn handle_mutable_read(
    state: Arc<AppState>,
    wildcard_path: &str,
    identity: &ClientIdentity,
) -> Response {
    match state.config.mutable_path_mode {
        MutablePathMode::Strict => {
            state.metrics.record_mutable_read_rejection("GET");
            debug!(path = %wildcard_path, "rejecting mutable path in strict mode");
            CacheServiceError::BadRequest {
                reason: "mutable path \u{2014} access origin directly".to_string(),
            }
            .into_response()
        }
        MutablePathMode::Transparent => {
            if let Some(err) = check_mutable_read_action(&state, identity, wildcard_path) {
                return err;
            }
            state.metrics.record_mutable_proxy_get();
            debug!(path = %wildcard_path, "proxying mutable path to origin (no caching)");
            let origin_path = ObjectPath::from(wildcard_path.to_string());
            let get_result = match state.origin.get(&origin_path).await {
                Ok(r) => r,
                Err(CacheServiceError::OriginUnreachable { reason }) => {
                    warn!(path = %wildcard_path, reason = %reason, "origin unreachable for mutable path");
                    return (StatusCode::GATEWAY_TIMEOUT, reason).into_response();
                }
                Err(e) => return e.into_response(),
            };

            build_mutable_origin_response(get_result, state)
        }
    }
}

/// Handle a HEAD for a mutable path according to the configured mode.
async fn handle_mutable_head(
    state: &AppState,
    wildcard_path: &str,
    identity: &ClientIdentity,
) -> Response {
    match state.config.mutable_path_mode {
        MutablePathMode::Strict => {
            state.metrics.record_mutable_read_rejection("HEAD");
            debug!(path = %wildcard_path, "rejecting mutable path head in strict mode");
            CacheServiceError::BadRequest {
                reason: "mutable path \u{2014} access origin directly".to_string(),
            }
            .into_response()
        }
        MutablePathMode::Transparent => {
            if let Some(err) = check_mutable_read_action(state, identity, wildcard_path) {
                return err;
            }
            state.metrics.record_mutable_proxy_head();
            debug!(path = %wildcard_path, "proxying mutable path head to origin (no caching)");
            let origin_path = ObjectPath::from(wildcard_path.to_string());
            match state.origin.head(&origin_path).await {
                Ok(meta) => build_origin_head_response(meta),
                Err(CacheServiceError::OriginUnreachable { reason }) => {
                    warn!(path = %wildcard_path, reason = %reason, "origin unreachable for mutable path head");
                    (StatusCode::GATEWAY_TIMEOUT, reason).into_response()
                }
                Err(e) => e.into_response(),
            }
        }
    }
}

fn check_mutable_read_action(
    state: &AppState,
    identity: &ClientIdentity,
    wildcard_path: &str,
) -> Option<Response> {
    state.policy.as_ref()?;
    match parse_mutable_repo_path(wildcard_path) {
        Some(repo_path) => match normalize_repo_path(repo_path) {
            Ok(repo_path) => check_repo_action(state, identity, &repo_path, "read"),
            Err(e) => Some(e.into_response()),
        },
        None => {
            let path = wildcard_path.trim().trim_matches('/');
            Some(
                CacheServiceError::BadRequest {
                    reason: format!("mutable path cannot be authorized to a repo: {path}"),
                }
                .into_response(),
            )
        }
    }
}

/// Parse a wildcard path into (bucket, repo_path, object_type, hash).
///
/// Expected patterns:
/// - `.crab/xorbs/ab/abcdef...`  → Xorb (CLI global path)
/// - `.crab/shards/ab/abcdef...`  → Shard (CLI global path)
/// - `org/repo/packs/sha.pack`        → Pack
/// - `org/repo/packs/sha.idx`         → PackIndex
/// - versioned SlateDB metadata objects → Metadata
fn parse_object_path(path: &str) -> Option<(String, String, ObjectType, String)> {
    let parsed = parse_cache_object_path(path)?;
    let object_type = match parsed.kind {
        CacheObjectKind::Xorb => ObjectType::Xorb,
        CacheObjectKind::Shard => ObjectType::Shard,
        CacheObjectKind::Pack => ObjectType::Pack,
        CacheObjectKind::PackIndex => ObjectType::PackIndex,
        CacheObjectKind::GeneratedPack => ObjectType::Pack,
        CacheObjectKind::Metadata => ObjectType::Metadata,
    };
    Some((
        String::new(),
        parsed.repo_path.to_string(),
        object_type,
        parsed.identity.into_owned(),
    ))
}

fn cached_object_valid_for_read(state: &AppState, key: &ServerObjectKey, hash: &str) -> bool {
    match state.cache_store.verify_cached_object_identity(key) {
        Ok(valid) => valid,
        Err(e) => {
            warn!(
                hash = %hash,
                error = %e,
                "cached object identity check failed, falling through to origin"
            );
            false
        }
    }
}

/// Parse `Range: bytes=start-end` into an exclusive range.
fn parse_range_header(
    headers: &HeaderMap,
) -> std::result::Result<Option<std::ops::Range<u64>>, CacheServiceError> {
    let Some(value) = headers.get(header::RANGE) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| CacheServiceError::BadRequest {
        reason: "Range header is not valid UTF-8".to_string(),
    })?;
    let Some(bytes_str) = value.strip_prefix("bytes=") else {
        return Err(CacheServiceError::BadRequest {
            reason: format!("unsupported Range header: {value}"),
        });
    };
    let Some((start_str, end_str)) = bytes_str.split_once('-') else {
        return Err(CacheServiceError::BadRequest {
            reason: format!("invalid Range header: {value}"),
        });
    };
    if start_str.is_empty() || end_str.is_empty() || end_str.contains(',') {
        return Err(CacheServiceError::BadRequest {
            reason: format!("only bounded single byte ranges are supported: {value}"),
        });
    }
    let start: u64 = start_str
        .parse()
        .map_err(|_| CacheServiceError::BadRequest {
            reason: format!("invalid Range start: {value}"),
        })?;
    // RFC 7233: end is inclusive, convert to exclusive.
    let end = end_str
        .parse::<u64>()
        .ok()
        .and_then(|end| end.checked_add(1))
        .ok_or_else(|| CacheServiceError::BadRequest {
            reason: format!("invalid Range end: {value}"),
        })?;
    if start >= end {
        return Err(CacheServiceError::BadRequest {
            reason: format!("invalid Range bounds: {value}"),
        });
    }
    Ok(Some(start..end))
}

/// Serve a byte range from cache, fetching and caching the full immutable
/// object through the server origin on cold miss.
async fn serve_range(
    state: &AppState,
    key: &ServerObjectKey,
    wildcard_path: &str,
    hash: &str,
    start: Instant,
    range: std::ops::Range<u64>,
) -> Response {
    if !cached_object_valid_for_read(state, key, hash) {
        debug!(hash = %hash, "cached range object failed identity check, fetching from origin");
        return fetch_and_cache_range(state, key, wildcard_path, hash, start, range).await;
    }

    match state.cache_store.get_range(key, range.clone()) {
        Ok(Some(CacheRangeRead::Hit(cached))) => {
            state.metrics.record_cache_hit(key.object_type);
            state
                .metrics
                .record_bytes_served(key.object_type, cached.data.len() as u64, true);
            debug!(
                hash = %hash,
                start = cached.range.start,
                end = cached.range.end,
                "range hit",
            );
            build_range_response(
                cached.data,
                hash,
                &cached.range,
                cached.total_size,
                CACHE_HIT,
            )
        }
        Ok(Some(CacheRangeRead::Unsatisfiable { total_size })) => {
            state.metrics.record_cache_hit(key.object_type);
            debug!(hash = %hash, start = range.start, end = range.end, total_size, "range unsatisfiable");
            build_range_not_satisfiable(hash, total_size, CACHE_HIT)
        }
        Ok(None) => {
            debug!(hash = %hash, "range miss, fetching full object from origin");
            fetch_and_cache_range(state, key, wildcard_path, hash, start, range).await
        }
        Err(e) => {
            warn!(hash = %hash, error = %e, "cache range read error, fetching full object from origin");
            fetch_and_cache_range(state, key, wildcard_path, hash, start, range).await
        }
    }
}

/// Fetch an object from origin, verify/cache it locally, and return it.
async fn fetch_and_cache(
    state: &AppState,
    key: &ServerObjectKey,
    wildcard_path: &str,
    expected_hash: &str,
    start: Instant,
) -> Response {
    match fetch_and_cache_data(state, key, wildcard_path, expected_hash, start).await {
        Ok(fetched) => {
            state.metrics.record_bytes_served(
                key.object_type,
                fetched.body.len_u64(),
                fetched.cache_status == CACHE_HIT,
            );
            build_ok_fetch_response(fetched, expected_hash).await
        }
        Err(response) => response,
    }
}

async fn fetch_and_cache_range(
    state: &AppState,
    key: &ServerObjectKey,
    wildcard_path: &str,
    expected_hash: &str,
    start: Instant,
    range: std::ops::Range<u64>,
) -> Response {
    match fetch_and_cache_data(state, key, wildcard_path, expected_hash, start).await {
        Ok(fetched) => match slice_cached_fetch_body(&fetched.body, &range).await {
            Ok(Some((slice, returned_range, total_size))) => {
                state.metrics.record_bytes_served(
                    key.object_type,
                    slice.len() as u64,
                    fetched.cache_status == CACHE_HIT,
                );
                build_range_response(
                    slice,
                    expected_hash,
                    &returned_range,
                    total_size,
                    fetched.cache_status,
                )
            }
            Ok(None) => build_range_not_satisfiable(
                expected_hash,
                fetched.body.len_u64(),
                fetched.cache_status,
            ),
            Err(response) => response,
        },
        Err(response) => response,
    }
}

async fn fetch_and_cache_data(
    state: &AppState,
    key: &ServerObjectKey,
    wildcard_path: &str,
    expected_hash: &str,
    start: Instant,
) -> std::result::Result<CachedFetch, Response> {
    let miss_key = cache_miss_lock_key(key);
    let (miss_registration, joined_existing_fill, inflight_misses) =
        CacheMissRegistration::new(state, miss_key);
    state.metrics.set_inflight_misses(inflight_misses);

    let miss_guard = miss_registration.entry.lock.lock().await;
    let result = fetch_and_cache_data_locked(
        state,
        key,
        wildcard_path,
        expected_hash,
        start,
        joined_existing_fill,
    )
    .await;

    drop(miss_guard);
    drop(miss_registration);

    result
}

async fn fetch_and_cache_data_locked(
    state: &AppState,
    key: &ServerObjectKey,
    wildcard_path: &str,
    expected_hash: &str,
    start: Instant,
    joined_existing_fill: bool,
) -> std::result::Result<CachedFetch, Response> {
    if cached_object_valid_for_read(state, key, expected_hash) {
        if matches!(
            key.object_type,
            ObjectType::Pack | ObjectType::PackIndex | ObjectType::Metadata
        ) {
            match state.cache_store.get_file(key) {
                Ok(Some(cached)) => {
                    if joined_existing_fill {
                        state.metrics.record_coalesced_miss(key.object_type);
                    }
                    state.metrics.record_cache_hit(key.object_type);
                    debug!(hash = %expected_hash, size = cached.size, "cache filled while waiting on miss lock");
                    return Ok(CachedFetch {
                        body: CachedFetchBody::OpenFile {
                            file: cached.file,
                            size: cached.size,
                        },
                        cache_status: CACHE_HIT,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(hash = %expected_hash, error = %e, "cache store recheck failed, fetching from origin");
                }
            }
        } else {
            match state.cache_store.get(key) {
                Ok(Some(data)) => {
                    if joined_existing_fill {
                        state.metrics.record_coalesced_miss(key.object_type);
                    }
                    state.metrics.record_cache_hit(key.object_type);
                    debug!(hash = %expected_hash, size = data.len(), "cache filled while waiting on miss lock");
                    return Ok(CachedFetch {
                        body: CachedFetchBody::Bytes(data),
                        cache_status: CACHE_HIT,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(hash = %expected_hash, error = %e, "cache store recheck failed, fetching from origin");
                }
            }
        }
    }

    // Build the origin path — the wildcard already excludes the `/v1/` prefix.
    let origin_path = ObjectPath::from(wildcard_path.to_string());

    let get_result = match state.origin.get(&origin_path).await {
        Ok(r) => r,
        Err(CacheServiceError::OriginUnreachable { reason }) => {
            warn!(hash = %expected_hash, reason = %reason, "origin unreachable on cache miss");
            return Err((StatusCode::GATEWAY_TIMEOUT, reason).into_response());
        }
        Err(e) => return Err(e.into_response()),
    };

    let staged = match stream_origin_body_to_temp(&state.cache_store, key, get_result).await {
        Ok(staged) => staged,
        Err(response) => return Err(response),
    };

    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    state.metrics.record_cache_miss(key.object_type);
    state
        .metrics
        .record_origin_fetch(key.object_type, latency_ms, staged.size);

    if let Err(e) = validate_staged_cache_object(key, &staged).await
        && is_integrity_checked_object(key.object_type)
    {
        return Err(e.into_response());
    }

    let body = commit_origin_fill_or_read_temp(state, key, staged, expected_hash).await?;

    if key.object_type == ObjectType::Shard
        && let CachedFetchBody::Bytes(data) = &body
    {
        ingest_cached_shard(state, expected_hash, data).await;
    }

    // Update the bytes-stored gauge after any cache mutation.
    state
        .metrics
        .set_bytes_stored(state.cache_store.current_bytes());

    Ok(CachedFetch {
        body,
        cache_status: CACHE_MISS,
    })
}

async fn commit_origin_fill_or_read_temp(
    state: &AppState,
    key: &ServerObjectKey,
    staged: FileBackedBody,
    expected_hash: &str,
) -> std::result::Result<CachedFetchBody, Response> {
    match state
        .cache_store
        .would_exceed_budget_after_put(key, staged.size)
    {
        Ok(true) => {
            debug!(hash = %expected_hash, "cache over budget before origin fill commit, attempting emergency eviction");
            if let Err(e) = state.cache_store.emergency_evict() {
                warn!(hash = %expected_hash, error = %e, "emergency eviction failed before origin fill commit");
            }
        }
        Ok(false) => {}
        Err(e) => {
            warn!(hash = %expected_hash, error = %e, "failed to estimate origin fill cache growth")
        }
    }

    match state
        .cache_store
        .would_exceed_budget_after_put(key, staged.size)
    {
        Ok(true) => {
            warn!(hash = %expected_hash, "skipping origin fill cache write — cache remains full");
            return read_staged_temp_body(staged.temp_path).await;
        }
        Ok(false) => {}
        Err(e) => {
            warn!(hash = %expected_hash, error = %e, "failed to recheck origin fill cache growth")
        }
    }

    let size = staged.size;
    match state
        .cache_store
        .put_unverified_temp_path_recoverable(key, staged.temp_path, size)
    {
        Ok(()) => {
            if key.object_type == ObjectType::Shard {
                ingest_committed_shard(state, key).await;
            }
            if matches!(
                key.object_type,
                ObjectType::Pack | ObjectType::PackIndex | ObjectType::Metadata
            ) {
                return match state.cache_store.get_file(key) {
                    Ok(Some(cached)) => Ok(CachedFetchBody::OpenFile {
                        file: cached.file,
                        size: cached.size,
                    }),
                    Ok(None) => Err(CacheServiceError::InternalError(
                        format!(
                            "committed cache object disappeared before it could be streamed: {}",
                            key.hash
                        )
                        .into(),
                    )
                    .into_response()),
                    Err(error) => Err(error.into_response()),
                };
            }
            Ok(CachedFetchBody::CachedFile {
                path: state.cache_store.object_path(key),
                size,
            })
        }
        Err(e) => {
            let (error, temp_path) = e.into_parts();
            recover_origin_fill_commit_failure(state, key, expected_hash, error, temp_path).await
        }
    }
}

async fn recover_origin_fill_commit_failure(
    state: &AppState,
    key: &ServerObjectKey,
    expected_hash: &str,
    error: CacheServiceError,
    recovery: TempPathCommitRecovery,
) -> std::result::Result<CachedFetchBody, Response> {
    warn!(hash = %expected_hash, error = %error, "failed to cache origin fill, serving staged response");
    if let TempPathCommitRecovery::TempPath(temp_path) = recovery {
        return read_staged_temp_body(temp_path).await;
    }

    let path = state.cache_store.object_path(key);
    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.is_file() => Ok(CachedFetchBody::CachedFile {
            path,
            size: meta.len(),
        }),
        Ok(_) => Err(error.into_response()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(error.into_response()),
        Err(e) => {
            warn!(hash = %expected_hash, path = %path.display(), error = %e, "failed to stat canonical cache file after origin fill commit error");
            Err(error.into_response())
        }
    }
}

async fn read_staged_temp_body(
    temp_path: TempPath,
) -> std::result::Result<CachedFetchBody, Response> {
    let data = tokio::fs::read(temp_path.as_ref() as &FsPath)
        .await
        .map_err(|e| push_warming_temp_io_error("read staged origin fill", e))?;
    Ok(CachedFetchBody::Bytes(Bytes::from(data)))
}

fn is_integrity_checked_object(object_type: ObjectType) -> bool {
    matches!(object_type, ObjectType::Shard | ObjectType::Xorb)
}

async fn ingest_cached_shard(state: &AppState, shard_hash: &str, data: &Bytes) {
    if let Err(e) = state.chunk_index.ingest_shard(data) {
        warn!(hash = %shard_hash, error = %e, "shard ingestion failed (non-fatal)");
        let mut last_error = state.dedup_last_ingestion_error.write().await;
        *last_error = Some(DedupIndexIngestionError {
            shard_hash: shard_hash.to_string(),
            error: e.to_string(),
        });
    }
}

async fn ingest_committed_shard(state: &AppState, key: &ServerObjectKey) {
    let path = state.cache_store.object_path(key);
    let data = match tokio::fs::read(&path).await {
        Ok(data) => Bytes::from(data),
        Err(e) => {
            warn!(
                hash = %key.hash,
                path = %path.display(),
                error = %e,
                "failed to read cached shard for chunk-index ingestion"
            );
            return;
        }
    };
    ingest_cached_shard(state, &key.hash, &data).await;
}

/// Build a 200 OK response for immutable cacheable objects.
fn build_ok_response(data: Bytes, hash: &str, cache_status: &str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_LENGTH, data.len().to_string()),
            (header::ETAG, format!("\"{hash}\"")),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL.to_string()),
            (CACHE_STATUS_HEADER, cache_status.to_string()),
        ],
        data,
    )
        .into_response()
}

fn build_file_response(file: std::fs::File, size: u64, hash: &str, cache_status: &str) -> Response {
    let body = Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file)));
    (
        StatusCode::OK,
        [
            (header::CONTENT_LENGTH, size.to_string()),
            (header::ETAG, format!("\"{hash}\"")),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL.to_string()),
            (CACHE_STATUS_HEADER, cache_status.to_string()),
        ],
        body,
    )
        .into_response()
}

async fn build_ok_fetch_response(fetched: CachedFetch, hash: &str) -> Response {
    match fetched.body {
        CachedFetchBody::Bytes(data) => build_ok_response(data, hash, fetched.cache_status),
        CachedFetchBody::OpenFile { file, size } => {
            build_file_response(file, size, hash, fetched.cache_status)
        }
        CachedFetchBody::CachedFile { path, size } => {
            let file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(e) => {
                    return CacheServiceError::InternalError(
                        format!(
                            "failed to open cached response file {}: {e}",
                            path.display()
                        )
                        .into(),
                    )
                    .into_response();
                }
            };
            let body = Body::from_stream(ReaderStream::new(file));
            (
                StatusCode::OK,
                [
                    (header::CONTENT_LENGTH, size.to_string()),
                    (header::ETAG, format!("\"{hash}\"")),
                    (header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL.to_string()),
                    (CACHE_STATUS_HEADER, fetched.cache_status.to_string()),
                ],
                body,
            )
                .into_response()
        }
    }
}

fn build_range_response(
    data: Bytes,
    hash: &str,
    range: &std::ops::Range<u64>,
    total_size: u64,
    cache_status: &str,
) -> Response {
    let last_byte = range.end.saturating_sub(1);
    (
        StatusCode::PARTIAL_CONTENT,
        [
            (header::CONTENT_LENGTH, data.len().to_string()),
            (
                header::CONTENT_RANGE,
                format!("bytes {}-{last_byte}/{total_size}", range.start),
            ),
            (header::ETAG, format!("\"{hash}\"")),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL.to_string()),
            (CACHE_STATUS_HEADER, cache_status.to_string()),
        ],
        data,
    )
        .into_response()
}

async fn slice_cached_fetch_body(
    body: &CachedFetchBody,
    range: &std::ops::Range<u64>,
) -> std::result::Result<Option<(Bytes, std::ops::Range<u64>, u64)>, Response> {
    match body {
        CachedFetchBody::Bytes(data) => {
            let total_size = data.len() as u64;
            Ok(slice_body_range(data, range).map(|(data, range)| (data, range, total_size)))
        }
        CachedFetchBody::OpenFile { file, size } => {
            let file = file.try_clone().map_err(|e| {
                CacheServiceError::InternalError(
                    format!("failed to clone cached range file: {e}").into(),
                )
                .into_response()
            })?;
            read_open_file_range(tokio::fs::File::from_std(file), *size, range).await
        }
        CachedFetchBody::CachedFile { path, size } => {
            read_cached_file_range(path, *size, range).await
        }
    }
}

async fn read_open_file_range(
    mut file: tokio::fs::File,
    total_size: u64,
    range: &std::ops::Range<u64>,
) -> std::result::Result<Option<(Bytes, std::ops::Range<u64>, u64)>, Response> {
    if range.start >= total_size {
        return Ok(None);
    }
    let end = range.end.min(total_size);
    if range.start >= end {
        return Ok(None);
    }

    let len = usize::try_from(end - range.start).map_err(|_| {
        CacheServiceError::BadRequest {
            reason: "requested range is too large to serve".to_string(),
        }
        .into_response()
    })?;
    file.seek(SeekFrom::Start(range.start)).await.map_err(|e| {
        CacheServiceError::InternalError(format!("failed to seek cached range file: {e}").into())
            .into_response()
    })?;
    let mut data = vec![0u8; len];
    file.read_exact(&mut data).await.map_err(|e| {
        CacheServiceError::InternalError(format!("failed to read cached range file: {e}").into())
            .into_response()
    })?;

    Ok(Some((Bytes::from(data), range.start..end, total_size)))
}

async fn read_cached_file_range(
    path: &FsPath,
    total_size: u64,
    range: &std::ops::Range<u64>,
) -> std::result::Result<Option<(Bytes, std::ops::Range<u64>, u64)>, Response> {
    if range.start >= total_size {
        return Ok(None);
    }
    let end = range.end.min(total_size);
    if range.start >= end {
        return Ok(None);
    }

    let len = usize::try_from(end - range.start).map_err(|_| {
        CacheServiceError::BadRequest {
            reason: "requested range is too large to serve".to_string(),
        }
        .into_response()
    })?;
    let mut file = tokio::fs::File::open(path).await.map_err(|e| {
        CacheServiceError::InternalError(
            format!("failed to open cached range file {}: {e}", path.display()).into(),
        )
        .into_response()
    })?;
    file.seek(SeekFrom::Start(range.start)).await.map_err(|e| {
        CacheServiceError::InternalError(
            format!("failed to seek cached range file {}: {e}", path.display()).into(),
        )
        .into_response()
    })?;
    let mut data = vec![0u8; len];
    file.read_exact(&mut data).await.map_err(|e| {
        CacheServiceError::InternalError(
            format!("failed to read cached range file {}: {e}", path.display()).into(),
        )
        .into_response()
    })?;

    Ok(Some((Bytes::from(data), range.start..end, total_size)))
}

fn build_range_not_satisfiable(hash: &str, total_size: u64, cache_status: &str) -> Response {
    (
        StatusCode::RANGE_NOT_SATISFIABLE,
        [
            (header::ETAG, format!("\"{hash}\"")),
            (header::CONTENT_RANGE, format!("bytes */{total_size}")),
            (CACHE_STATUS_HEADER, cache_status.to_string()),
        ],
    )
        .into_response()
}

fn build_head_response(size: u64, hash: &str, cache_status: &str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_LENGTH, size.to_string()),
            (header::ETAG, format!("\"{hash}\"")),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL.to_string()),
            (CACHE_STATUS_HEADER, cache_status.to_string()),
        ],
    )
        .into_response()
}

fn build_origin_head_response(meta: object_store::ObjectMeta) -> Response {
    let mut response = (
        StatusCode::OK,
        [(header::CONTENT_LENGTH, meta.size.to_string())],
    )
        .into_response();
    if let Some(etag) = meta.e_tag
        && let Ok(value) = etag.parse()
    {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

fn build_mutable_origin_response(
    get_result: object_store::GetResult,
    state: Arc<AppState>,
) -> Response {
    let size = get_result.meta.size;
    let etag = get_result.meta.e_tag.clone();
    let body = Body::from_stream(get_result.into_stream().map(move |chunk| {
        match &chunk {
            Ok(data) => state.metrics.record_mutable_proxy_bytes(data.len() as u64),
            Err(_) => state.metrics.record_mutable_proxy_stream_error("GET"),
        }
        chunk
    }));
    let mut response = (
        StatusCode::OK,
        [(header::CONTENT_LENGTH, size.to_string())],
        body,
    )
        .into_response();
    if let Some(etag) = etag
        && let Ok(value) = etag.parse()
    {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

fn slice_body_range(
    data: &Bytes,
    range: &std::ops::Range<u64>,
) -> Option<(Bytes, std::ops::Range<u64>)> {
    let start = usize::try_from(range.start).ok()?;
    if start >= data.len() {
        return None;
    }
    let requested_end = usize::try_from(range.end).ok()?;
    let end = requested_end.min(data.len());
    if start >= end {
        return None;
    }
    Some((data.slice(start..end), start as u64..end as u64))
}

fn cache_miss_lock_key(key: &ServerObjectKey) -> String {
    format!(
        "{}:{}:{}:{}",
        key.object_type.as_u8(),
        key.bucket,
        key.repo_path,
        key.hash
    )
}

struct CacheMissRegistration<'a> {
    locks: &'a std::sync::Mutex<HashMap<String, Arc<CacheMissEntry>>>,
    metrics: &'a super::metrics::CacheMetrics,
    key: String,
    entry: Arc<CacheMissEntry>,
}

impl<'a> CacheMissRegistration<'a> {
    fn new(state: &'a AppState, key: String) -> (Self, bool, u64) {
        let (entry, joined_existing_fill, inflight_misses) = {
            let mut locks = lock_cache_miss_registry(&state.cache_miss_locks);
            let entry = Arc::clone(
                locks
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(CacheMissEntry::new())),
            );
            let joined_existing_fill = entry.users.fetch_add(1, Ordering::AcqRel) != 0;
            let inflight_misses = u64::try_from(locks.len()).unwrap_or(u64::MAX);
            (entry, joined_existing_fill, inflight_misses)
        };
        (
            Self {
                locks: &state.cache_miss_locks,
                metrics: &state.metrics,
                key,
                entry,
            },
            joined_existing_fill,
            inflight_misses,
        )
    }
}

impl Drop for CacheMissRegistration<'_> {
    fn drop(&mut self) {
        let mut locks = lock_cache_miss_registry(self.locks);
        let previous_users = self.entry.users.fetch_sub(1, Ordering::AcqRel);
        if previous_users == 1
            && locks
                .get(&self.key)
                .is_some_and(|current| Arc::ptr_eq(current, &self.entry))
        {
            locks.remove(&self.key);
        }
        self.metrics
            .set_inflight_misses(u64::try_from(locks.len()).unwrap_or(u64::MAX));
    }
}

fn lock_cache_miss_registry(
    locks: &std::sync::Mutex<HashMap<String, Arc<CacheMissEntry>>>,
) -> std::sync::MutexGuard<'_, HashMap<String, Arc<CacheMissEntry>>> {
    match locks.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn cache_miss_lock_count(state: &AppState) -> u64 {
    let locks = lock_cache_miss_registry(&state.cache_miss_locks);
    u64::try_from(locks.len()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::body::to_bytes;
    use crab_xet::xorb::format::Chunk;
    use object_store::memory::InMemory;
    use object_store::{ObjectStore, ObjectStoreExt};

    use crate::auth::{AuthPolicy, PolicyRule};
    use crate::cache_store::CacheStore;
    use crate::chunk_index::ChunkIndex;
    use crate::config::{AuthConfig, CacheServerConfig};
    use crate::db::{CACHE_DB_FILE, CacheDb};
    use crate::metrics::CacheMetrics;
    use crate::origin_client::OriginClient;
    use crab_xet::hash::compute_data_hash;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};

    struct TestDedupState {
        state: AppState,
        _tempdir: tempfile::TempDir,
    }

    struct DedupXorbFixture {
        chunk_hash: [u8; 32],
        xorb_hash_hex: String,
        bytes: Bytes,
        location: ChunkLocation,
    }

    fn hex_hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn global_path(kind: &str, hash: &str) -> String {
        format!(".crab/{kind}/{}/{hash}", &hash[..2])
    }

    fn test_dedup_state() -> TestDedupState {
        test_dedup_state_with_origin(Arc::new(InMemory::new()))
    }

    fn test_dedup_state_with_origin(origin: Arc<dyn ObjectStore>) -> TestDedupState {
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        let db = CacheDb::open_or_create(&cache_root.join(CACHE_DB_FILE)).unwrap();
        let cache_store = Arc::new(
            CacheStore::open(cache_root.clone(), 1_073_741_824, db.connect().unwrap()).unwrap(),
        );
        let chunk_index = ChunkIndex::open(db.connect().unwrap()).unwrap();

        let psk_hash = blake3::hash(b"dedup-test-key");
        let config = CacheServerConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            tls: None,
            auth: AuthConfig::Psk {
                key_hash: *psk_hash.as_bytes(),
            },
            origin_url: "memory://".to_string(),
            cache_root,
            max_cache_bytes: 1_073_741_824,
            dedup_scope: DedupScope::All,
            drain_timeout: Duration::from_secs(1),
            mutable_path_mode: MutablePathMode::Strict,
            high_water_ratio: 0.95,
            low_water_ratio: 0.90,
            policy_path: None,
        };

        TestDedupState {
            state: AppState {
                cache_store,
                chunk_index,
                origin: OriginClient::from_store(origin),
                config,
                metrics: CacheMetrics::stub(),
                policy: None,
                evictor_notify: Arc::new(tokio::sync::Notify::new()),
                origin_healthy: AtomicBool::new(true),
                origin_health_checked_at: tokio::sync::Mutex::new(tokio::time::Instant::now()),
                cache_miss_locks: std::sync::Mutex::new(HashMap::new()),
                push_warming_body_permits: tokio::sync::Semaphore::new(8),
                dedup_index_rebuild: DedupIndexRebuildStats {
                    status: "not_run".to_string(),
                    entries: 0,
                    error: None,
                },
                dedup_last_ingestion_error: tokio::sync::RwLock::new(None),
            },
            _tempdir: tempdir,
        }
    }

    fn dedup_xorb_fixture() -> DedupXorbFixture {
        let chunk = Chunk::new(Bytes::from_static(b"cache service dedup proof"));
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0)).unwrap();
        let mut xorbs = builder.finalize().unwrap();
        let xorb = xorbs.pop().unwrap();
        let placement = xorb.placements[0].clone();

        DedupXorbFixture {
            chunk_hash: chunk.hash.into(),
            xorb_hash_hex: xorb.hash.hex(),
            bytes: xorb.bytes,
            location: ChunkLocation {
                xorb_hash: xorb.hash.into(),
                chunk_index: placement.chunk_index,
                length: placement.uncompressed_size,
            },
        }
    }

    fn cached_xorb_key(fixture: &DedupXorbFixture) -> ServerObjectKey {
        ServerObjectKey {
            bucket: String::new(),
            repo_path: ".crab".to_string(),
            object_type: ObjectType::Xorb,
            hash: fixture.xorb_hash_hex.clone(),
        }
    }

    fn test_identity() -> ClientIdentity {
        ClientIdentity {
            principal: "test-client".to_string(),
        }
    }

    #[tokio::test]
    async fn cancelled_cache_miss_fill_releases_registration() {
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let state = Arc::new(state);
        let key = cache_miss_lock_key(&ServerObjectKey {
            bucket: String::new(),
            repo_path: ".crab".to_owned(),
            object_type: ObjectType::Pack,
            hash: hex_hash('a'),
        });
        let state_for_fill = Arc::clone(&state);

        let result = tokio::time::timeout(Duration::from_millis(10), async move {
            let (registration, joined_existing_fill, inflight_misses) =
                CacheMissRegistration::new(&state_for_fill, key);
            assert!(!joined_existing_fill);
            assert_eq!(inflight_misses, 1);
            let _guard = registration.entry.lock.lock().await;
            std::future::pending::<()>().await;
        })
        .await;

        assert!(result.is_err());
        assert_eq!(cache_miss_lock_count(&state), 0);
        assert_eq!(state.metrics.snapshot().inflight_misses, 0);
    }

    #[test]
    fn cache_miss_waiter_release_keeps_active_fill_registered() {
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let key = cache_miss_lock_key(&ServerObjectKey {
            bucket: String::new(),
            repo_path: ".crab".to_owned(),
            object_type: ObjectType::Pack,
            hash: hex_hash('b'),
        });

        let (first, first_joined, _) = CacheMissRegistration::new(&state, key.clone());
        let (second, second_joined, _) = CacheMissRegistration::new(&state, key);
        assert!(!first_joined);
        assert!(second_joined);
        assert_eq!(cache_miss_lock_count(&state), 1);

        drop(second);
        assert_eq!(cache_miss_lock_count(&state), 1);
        drop(first);
        assert_eq!(cache_miss_lock_count(&state), 0);
    }

    fn read_policy_for(repos: &[&str]) -> AuthPolicy {
        AuthPolicy {
            rules: vec![PolicyRule {
                principal: "test-client".to_string(),
                repos: repos.iter().map(|repo| (*repo).to_string()).collect(),
                actions: vec!["read".to_string()],
            }],
        }
    }

    #[test]
    fn dedup_response_marks_index_hit_unknown_when_xorb_not_cached() {
        let fixture = dedup_xorb_fixture();
        let test = test_dedup_state();
        let result = DedupResult {
            known: vec![(0, fixture.location)],
            unknown: Vec::new(),
        };

        let response = build_verified_dedup_response(&test.state, &[fixture.chunk_hash], result);

        assert!(response.known.is_empty());
        assert_eq!(response.unknown, vec![0]);
    }

    #[test]
    fn dedup_response_verifies_cached_xorb_before_known() {
        let fixture = dedup_xorb_fixture();
        let test = test_dedup_state();
        test.state
            .cache_store
            .put_unverified(&cached_xorb_key(&fixture), fixture.bytes.clone())
            .unwrap();
        let result = DedupResult {
            known: vec![(0, fixture.location)],
            unknown: Vec::new(),
        };

        let response = build_verified_dedup_response(&test.state, &[fixture.chunk_hash], result);

        assert!(response.unknown.is_empty());
        assert_eq!(response.known.len(), 1);
        let known = &response.known[0];
        assert_eq!(known.index, 0);
        assert_eq!(known.xorb_hash, fixture.xorb_hash_hex);
        assert_eq!(known.chunk_index, 0);
        assert_eq!(known.length, b"cache service dedup proof".len() as u32);
        assert!(known.cache_verified);
    }

    #[test]
    fn dedup_response_marks_mismatched_cached_xorb_unknown() {
        let fixture = dedup_xorb_fixture();
        let test = test_dedup_state();
        test.state
            .cache_store
            .put_unverified(&cached_xorb_key(&fixture), fixture.bytes.clone())
            .unwrap();
        let mut location = fixture.location;
        location.length += 1;
        let result = DedupResult {
            known: vec![(0, location)],
            unknown: Vec::new(),
        };

        let response = build_verified_dedup_response(&test.state, &[fixture.chunk_hash], result);

        assert!(response.known.is_empty());
        assert_eq!(response.unknown, vec![0]);
    }

    #[tokio::test]
    async fn head_object_reports_cache_hit_without_origin_body() {
        let fixture = dedup_xorb_fixture();
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let key = cached_xorb_key(&fixture);
        state
            .cache_store
            .put_unverified(&key, fixture.bytes.clone())
            .unwrap();
        let path = global_path("xorbs", &fixture.xorb_hash_hex);
        let state = Arc::new(state);

        let response = head_object(
            State(Arc::clone(&state)),
            Path(path),
            axum::Extension(test_identity()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_STATUS_HEADER).unwrap(),
            CACHE_HIT
        );
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).unwrap(),
            fixture.bytes.len().to_string().as_str()
        );
        assert_eq!(
            response.headers().get(header::ETAG).unwrap(),
            format!("\"{}\"", fixture.xorb_hash_hex).as_str()
        );
        assert_eq!(state.metrics.snapshot().cache_hits, 1);
        assert_eq!(state.metrics.snapshot().cache_misses, 0);
        assert_eq!(state.metrics.snapshot().origin_head_requests, 0);
    }

    #[tokio::test]
    async fn head_object_reports_cache_miss_from_origin_metadata() {
        let fixture = dedup_xorb_fixture();
        let origin = Arc::new(InMemory::new());
        let path = global_path("xorbs", &fixture.xorb_hash_hex);
        origin
            .put(
                &ObjectPath::from(path.clone()),
                fixture.bytes.clone().into(),
            )
            .await
            .unwrap();
        let TestDedupState { state, _tempdir } = test_dedup_state_with_origin(origin);
        let state = Arc::new(state);

        let response = head_object(
            State(Arc::clone(&state)),
            Path(path),
            axum::Extension(test_identity()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_STATUS_HEADER).unwrap(),
            CACHE_MISS
        );
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).unwrap(),
            fixture.bytes.len().to_string().as_str()
        );
        assert_eq!(state.metrics.snapshot().cache_hits, 0);
        assert_eq!(state.metrics.snapshot().cache_misses, 1);
        assert_eq!(state.metrics.snapshot().origin_head_requests, 1);
        assert_eq!(
            state
                .metrics
                .snapshot()
                .by_object_type
                .xorb
                .origin_head_requests,
            1
        );
    }

    #[tokio::test]
    async fn mutable_origin_response_does_not_poll_body_before_client_consumes_it() {
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let state = Arc::new(state);
        let origin = InMemory::new();
        let path = ObjectPath::from("refs/heads/main");
        let data = Bytes::from_static(b"streamed mutable object");
        origin.put(&path, data.clone().into()).await.unwrap();
        let mut get_result = origin.get(&path).await.unwrap();
        let body_polled = Arc::new(AtomicBool::new(false));
        let body_polled_for_stream = Arc::clone(&body_polled);
        get_result.payload = object_store::GetResultPayload::Stream(
            futures_util::stream::once(async move {
                body_polled_for_stream.store(true, Ordering::Relaxed);
                Ok::<_, object_store::Error>(data)
            })
            .boxed(),
        );

        let response = build_mutable_origin_response(get_result, Arc::clone(&state));

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).unwrap(),
            b"streamed mutable object".len().to_string().as_str()
        );
        assert!(!body_polled.load(Ordering::Relaxed));
        assert_eq!(state.metrics.snapshot().mutable_proxy_bytes, 0);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        assert_eq!(body.as_ref(), b"streamed mutable object");
        assert!(body_polled.load(Ordering::Relaxed));
        assert_eq!(
            state.metrics.snapshot().mutable_proxy_bytes,
            b"streamed mutable object".len() as u64
        );
    }

    #[tokio::test]
    async fn transparent_mutable_get_proxies_after_repo_policy_allows() {
        let origin = Arc::new(InMemory::new());
        origin
            .put(
                &ObjectPath::from("org/repo/manifest"),
                Bytes::from_static(b"manifest bytes").into(),
            )
            .await
            .unwrap();
        let TestDedupState {
            mut state,
            _tempdir,
        } = test_dedup_state_with_origin(origin);
        state.config.mutable_path_mode = MutablePathMode::Transparent;
        state.policy = Some(read_policy_for(&["org/repo"]));
        let state = Arc::new(state);

        let response = read_object(
            State(Arc::clone(&state)),
            Path("org/repo/manifest".to_string()),
            HeaderMap::new(),
            axum::Extension(test_identity()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), b"manifest bytes");
        assert_eq!(state.metrics.snapshot().mutable_proxy_gets, 1);
    }

    #[tokio::test]
    async fn transparent_mutable_get_enforces_repo_policy_before_origin_proxy() {
        let origin = Arc::new(InMemory::new());
        origin
            .put(
                &ObjectPath::from("org/private/manifest"),
                Bytes::from_static(b"secret manifest").into(),
            )
            .await
            .unwrap();
        let TestDedupState {
            mut state,
            _tempdir,
        } = test_dedup_state_with_origin(origin);
        state.config.mutable_path_mode = MutablePathMode::Transparent;
        state.policy = Some(read_policy_for(&["org/allowed"]));
        let state = Arc::new(state);

        let response = read_object(
            State(Arc::clone(&state)),
            Path("org/private/manifest".to_string()),
            HeaderMap::new(),
            axum::Extension(test_identity()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(state.metrics.snapshot().mutable_proxy_gets, 0);
        assert_eq!(state.metrics.snapshot().mutable_proxy_bytes, 0);
    }

    #[tokio::test]
    async fn transparent_mutable_head_enforces_repo_policy_before_origin_proxy() {
        let origin = Arc::new(InMemory::new());
        origin
            .put(
                &ObjectPath::from("org/private/manifest"),
                Bytes::from_static(b"secret manifest").into(),
            )
            .await
            .unwrap();
        let TestDedupState {
            mut state,
            _tempdir,
        } = test_dedup_state_with_origin(origin);
        state.config.mutable_path_mode = MutablePathMode::Transparent;
        state.policy = Some(read_policy_for(&["org/allowed"]));
        let state = Arc::new(state);

        let response = head_object(
            State(Arc::clone(&state)),
            Path("org/private/manifest".to_string()),
            axum::Extension(test_identity()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(state.metrics.snapshot().mutable_proxy_heads, 0);
    }

    #[tokio::test]
    async fn transparent_mutable_get_rejects_policy_path_without_repo_scope() {
        let TestDedupState {
            mut state,
            _tempdir,
        } = test_dedup_state();
        state.config.mutable_path_mode = MutablePathMode::Transparent;
        state.policy = Some(read_policy_for(&["*"]));
        let state = Arc::new(state);

        let response = read_object(
            State(Arc::clone(&state)),
            Path("opaque-control-object".to_string()),
            HeaderMap::new(),
            axum::Extension(test_identity()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(state.metrics.snapshot().mutable_proxy_gets, 0);
    }

    #[tokio::test]
    async fn read_object_rejects_malformed_range_before_cache_or_origin() {
        let fixture = dedup_xorb_fixture();
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=abc-def".parse().unwrap());
        let state = Arc::new(state);

        let response = read_object(
            State(Arc::clone(&state)),
            Path(global_path("xorbs", &fixture.xorb_hash_hex)),
            headers,
            axum::Extension(test_identity()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers().get(CACHE_STATUS_HEADER).is_none());
        assert_eq!(state.metrics.snapshot().cache_hits, 0);
        assert_eq!(state.metrics.snapshot().cache_misses, 0);
        assert_eq!(state.metrics.snapshot().origin_fetches, 0);
    }

    #[tokio::test]
    async fn read_object_clips_range_end_past_cached_object_size() {
        let fixture = dedup_xorb_fixture();
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let key = cached_xorb_key(&fixture);
        state
            .cache_store
            .put_unverified(&key, fixture.bytes.clone())
            .unwrap();
        let start = 5usize;
        let requested_last = fixture.bytes.len() + 99;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::RANGE,
            format!("bytes={start}-{requested_last}").parse().unwrap(),
        );
        let state = Arc::new(state);

        let response = read_object(
            State(Arc::clone(&state)),
            Path(global_path("xorbs", &fixture.xorb_hash_hex)),
            headers,
            axum::Extension(test_identity()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE).unwrap(),
            format!(
                "bytes {}-{}/{}",
                start,
                fixture.bytes.len() - 1,
                fixture.bytes.len()
            )
            .as_str()
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, fixture.bytes.slice(start..fixture.bytes.len()));
        assert_eq!(state.metrics.snapshot().cache_hits, 1);
        assert_eq!(state.metrics.snapshot().origin_fetches, 0);
    }

    #[tokio::test]
    async fn fetch_and_cache_data_streams_origin_fill_to_cached_file() {
        let fixture = dedup_xorb_fixture();
        let origin = Arc::new(InMemory::new());
        let object_path = global_path("xorbs", &fixture.xorb_hash_hex);
        origin
            .put(
                &ObjectPath::from(object_path.clone()),
                fixture.bytes.clone().into(),
            )
            .await
            .unwrap();
        let TestDedupState { state, _tempdir } = test_dedup_state_with_origin(origin);
        let key = cached_xorb_key(&fixture);

        let fetched = match fetch_and_cache_data(
            &state,
            &key,
            &object_path,
            &fixture.xorb_hash_hex,
            Instant::now(),
        )
        .await
        {
            Ok(fetched) => fetched,
            Err(response) => panic!("origin fill failed with HTTP {}", response.status()),
        };

        assert_eq!(fetched.cache_status, CACHE_MISS);
        let CachedFetchBody::CachedFile { path, size } = fetched.body else {
            panic!("cold origin fill should return a committed cache file");
        };
        assert_eq!(size, fixture.bytes.len() as u64);
        assert_eq!(
            Bytes::from(tokio::fs::read(&path).await.unwrap()),
            fixture.bytes
        );

        let body = CachedFetchBody::CachedFile { path, size };
        let (slice, returned_range, total_size) =
            match slice_cached_fetch_body(&body, &(5..500)).await.unwrap() {
                Some(slice) => slice,
                None => panic!("cached file range should be satisfiable"),
            };
        assert_eq!(slice, fixture.bytes.slice(5..fixture.bytes.len()));
        assert_eq!(returned_range, 5..fixture.bytes.len() as u64);
        assert_eq!(total_size, fixture.bytes.len() as u64);
    }

    #[tokio::test]
    async fn fetch_and_cache_data_keeps_pack_origin_fill_file_backed() {
        let body = Bytes::from_static(b"pack origin fill remains file backed");
        let object_path = "org/repo/packs/pack-pack-fill.pack";
        let origin = Arc::new(InMemory::new());
        origin
            .put(&ObjectPath::from(object_path), body.clone().into())
            .await
            .unwrap();
        let TestDedupState { state, _tempdir } = test_dedup_state_with_origin(origin);
        let key = ServerObjectKey {
            bucket: String::new(),
            repo_path: "org/repo".to_string(),
            object_type: ObjectType::Pack,
            hash: "pack-fill".to_string(),
        };

        let fetched = fetch_and_cache_data(&state, &key, object_path, "pack-fill", Instant::now())
            .await
            .unwrap();
        let CachedFetchBody::OpenFile { file, size } = fetched.body else {
            panic!("cold pack fill should return an open cache file");
        };
        assert_eq!(size, body.len() as u64);
        let fetched_body = CachedFetchBody::OpenFile { file, size };

        let (slice, returned_range, total_size) = slice_cached_fetch_body(&fetched_body, &(5..500))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slice, body.slice(5..body.len()));
        assert_eq!(returned_range, 5..body.len() as u64);
        assert_eq!(total_size, body.len() as u64);
    }

    #[tokio::test]
    async fn recover_origin_fill_commit_failure_serves_staged_temp_body() {
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let key = ServerObjectKey {
            bucket: String::new(),
            repo_path: "org/repo".to_string(),
            object_type: ObjectType::Pack,
            hash: "pack-recover".to_string(),
        };
        let body = b"staged origin fallback";
        let temp_path = state.cache_store.create_temp_object_path(&key).unwrap();
        let temp_file_path = temp_path.to_path_buf();
        tokio::fs::write(&temp_file_path, body).await.unwrap();

        let recovered = match recover_origin_fill_commit_failure(
            &state,
            &key,
            "pack-recover",
            CacheServiceError::DiskFull {
                reason: "test commit failure".to_string(),
            },
            TempPathCommitRecovery::TempPath(temp_path),
        )
        .await
        {
            Ok(recovered) => recovered,
            Err(response) => {
                panic!(
                    "staged origin fallback failed with HTTP {}",
                    response.status()
                )
            }
        };

        let CachedFetchBody::Bytes(data) = recovered else {
            panic!("staged temp file should be served as bytes");
        };
        assert_eq!(data.as_ref(), body);
        assert!(!temp_file_path.exists());
    }

    #[tokio::test]
    async fn stream_chunks_to_temp_limits_only_when_configured() {
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let key = ServerObjectKey {
            bucket: String::new(),
            repo_path: "org/repo".to_string(),
            object_type: ObjectType::Pack,
            hash: "pack-limit".to_string(),
        };
        let map_error = |error: Infallible| match error {};

        let limited = stream_chunks_to_temp(
            &state.cache_store,
            &key,
            futures_util::stream::iter([Ok::<_, Infallible>(Bytes::from_static(b"abcd"))]),
            Some(3),
            map_error,
        )
        .await;

        let response = match limited {
            Ok(_) => panic!("configured limit should reject oversized stream"),
            Err(response) => response,
        };
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let staged = stream_chunks_to_temp(
            &state.cache_store,
            &key,
            futures_util::stream::iter([Ok::<_, Infallible>(Bytes::from_static(b"abcd"))]),
            None,
            map_error,
        )
        .await
        .expect("uncapped origin fill stream should be staged");

        assert_eq!(staged.size, 4);
        assert_eq!(
            tokio::fs::read(staged.temp_path.as_ref() as &FsPath)
                .await
                .unwrap(),
            b"abcd"
        );
    }

    #[tokio::test]
    async fn stream_push_warming_shard_body_computes_data_hash() {
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let body = Bytes::from_static(b"streamed shard bytes");
        let key = ServerObjectKey {
            bucket: String::new(),
            repo_path: ".crab".to_string(),
            object_type: ObjectType::Shard,
            hash: compute_data_hash(&body).hex(),
        };

        let staged = match stream_push_warming_body_to_temp(
            &state.cache_store,
            &key,
            Body::from(body),
        )
        .await
        {
            Ok(staged) => staged,
            Err(response) => {
                panic!(
                    "streaming shard body failed with HTTP {}",
                    response.status()
                )
            }
        };

        assert_eq!(staged.size, b"streamed shard bytes".len() as u64);
        assert_eq!(
            staged.data_hash,
            Some(compute_data_hash(&Bytes::from_static(
                b"streamed shard bytes"
            )))
        );
        assert_eq!(
            Bytes::from(
                tokio::fs::read(staged.temp_path.as_ref() as &std::path::Path)
                    .await
                    .unwrap()
            ),
            Bytes::from_static(b"streamed shard bytes")
        );
    }

    #[tokio::test]
    async fn stream_push_warming_pack_body_skips_data_hash() {
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let body = Bytes::from_static(b"pack bytes");
        let key = ServerObjectKey {
            bucket: String::new(),
            repo_path: "org/repo".to_string(),
            object_type: ObjectType::Pack,
            hash: "pack-stream".to_string(),
        };

        let staged = match stream_push_warming_body_to_temp(
            &state.cache_store,
            &key,
            Body::from(body),
        )
        .await
        {
            Ok(staged) => staged,
            Err(response) => {
                panic!("streaming pack body failed with HTTP {}", response.status())
            }
        };

        assert_eq!(staged.size, b"pack bytes".len() as u64);
        assert_eq!(staged.data_hash, None);
    }

    #[tokio::test]
    async fn write_object_streams_xorb_body_and_commits_cache_entry() {
        let fixture = dedup_xorb_fixture();
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let key = cached_xorb_key(&fixture);
        let state = Arc::new(state);

        let response = write_object(
            State(Arc::clone(&state)),
            Path(global_path("xorbs", &fixture.xorb_hash_hex)),
            axum::Extension(test_identity()),
            Body::from(fixture.bytes.clone()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(state.cache_store.get(&key).unwrap().unwrap(), fixture.bytes);
        assert_eq!(
            state
                .metrics
                .snapshot()
                .by_object_type
                .xorb
                .push_warming_writes,
            1
        );
    }

    #[tokio::test]
    async fn write_object_rejects_corrupt_streamed_xorb_body() {
        let fixture = dedup_xorb_fixture();
        let TestDedupState { state, _tempdir } = test_dedup_state();
        let key = cached_xorb_key(&fixture);
        let mut corrupt = fixture.bytes.to_vec();
        corrupt[0] ^= 0xFF;
        let state = Arc::new(state);

        let response = write_object(
            State(Arc::clone(&state)),
            Path(global_path("xorbs", &fixture.xorb_hash_hex)),
            axum::Extension(test_identity()),
            Body::from(Bytes::from(corrupt)),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(state.cache_store.get(&key).unwrap().is_none());
        assert_eq!(
            state
                .metrics
                .snapshot()
                .by_object_type
                .xorb
                .push_warming_writes,
            0
        );
    }

    #[tokio::test]
    async fn write_object_waits_for_body_permit_before_polling_body() {
        let TestDedupState {
            mut state,
            _tempdir,
        } = test_dedup_state();
        state.push_warming_body_permits = tokio::sync::Semaphore::new(1);
        let state = Arc::new(state);
        let held_permit = state.push_warming_body_permits.acquire().await.unwrap();
        let body_polled = Arc::new(AtomicBool::new(false));
        let body_polled_for_stream = Arc::clone(&body_polled);
        let body = Body::from_stream(futures_util::stream::once(async move {
            body_polled_for_stream.store(true, Ordering::Relaxed);
            Ok::<_, Infallible>(Bytes::from_static(b"permit-bounded body"))
        }));

        let response = write_object(
            State(Arc::clone(&state)),
            Path("org/repo/packs/pack-permit.pack".to_string()),
            axum::Extension(test_identity()),
            body,
        );
        tokio::pin!(response);

        tokio::select! {
            response = &mut response => {
                panic!("write completed before body permit was released: {}", response.status());
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
        assert!(!body_polled.load(Ordering::Relaxed));

        drop(held_permit);
        let response = response.await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(body_polled.load(Ordering::Relaxed));
    }

    #[test]
    fn reject_xet_xorb_path() {
        let hash_hex = hex_hash('a');
        assert!(parse_object_path(&format!("org/repo/xet/xorbs/{hash_hex}")).is_none());
    }

    #[test]
    fn reject_single_segment_xet_xorb_path() {
        let hash_hex = hex_hash('a');
        assert!(parse_object_path(&format!("repo/xet/xorbs/{hash_hex}")).is_none());
    }

    #[test]
    fn parse_global_crab_shard_path() {
        let hash_hex = hex_hash('b');
        let (bucket, repo, ot, hash) =
            parse_object_path(&global_path("shards", &hash_hex)).unwrap();
        assert_eq!(bucket, "");
        assert_eq!(repo, ".crab");
        assert_eq!(ot, ObjectType::Shard);
        assert_eq!(hash, hash_hex);
    }

    #[test]
    fn parse_global_crab_xorb_path() {
        let hash_hex = hex_hash('c');
        let (bucket, repo, ot, hash) = parse_object_path(&global_path("xorbs", &hash_hex)).unwrap();
        assert_eq!(bucket, "");
        assert_eq!(repo, ".crab");
        assert_eq!(ot, ObjectType::Xorb);
        assert_eq!(hash, hash_hex);
    }

    #[test]
    fn reject_xet_shard_path() {
        let hash_hex = hex_hash('d');
        assert!(parse_object_path(&format!("b/org/repo/xet/shards/{hash_hex}")).is_none());
    }

    #[test]
    fn reject_xet_file_index_path() {
        let hash_hex = hex_hash('e');
        assert!(parse_object_path(&format!("b/org/repo/xet/file-index/{hash_hex}")).is_none());
    }

    #[test]
    fn parse_pack_path() {
        let (bucket, repo, ot, hash) = parse_object_path("org/repo/packs/pack-abc.pack").unwrap();
        assert_eq!(bucket, "");
        assert_eq!(repo, "org/repo");
        assert_eq!(ot, ObjectType::Pack);
        assert_eq!(hash, "pack-abc");
    }

    #[test]
    fn parse_pack_index_path() {
        let (bucket, repo, ot, hash) = parse_object_path("org/repo/packs/pack-abc.idx").unwrap();
        assert_eq!(bucket, "");
        assert_eq!(repo, "org/repo");
        assert_eq!(ot, ObjectType::PackIndex);
        assert_eq!(hash, "pack-abc");
    }

    #[test]
    fn parse_generated_pack_paths_use_distinct_cache_identities() {
        let hash = hex_hash('c');
        let artifact = format!("org/repo/generated-packs/v1/artifacts/cc/{hash}.pack");
        let request = format!("org/repo/generated-packs/v1/requests/cc/{hash}.json");
        let (_, artifact_repo, artifact_type, artifact_id) =
            parse_object_path(&artifact).expect("generated pack artifact");
        let (_, request_repo, request_type, request_id) =
            parse_object_path(&request).expect("generated pack request");

        assert_eq!(artifact_repo, "org/repo");
        assert_eq!(request_repo, "org/repo");
        assert_eq!(artifact_type, ObjectType::Pack);
        assert_eq!(request_type, ObjectType::Pack);
        assert_ne!(artifact_id, request_id);
    }

    #[test]
    fn parse_file_index_sst_metadata_path() {
        let path = "org/repo/file_index_db/compacted/01KVK1S7EDDF30QW005R4TY0R4.sst";
        let (bucket, repo, ot, hash) = parse_object_path(path).unwrap();
        assert_eq!(bucket, "");
        assert_eq!(repo, "org/repo");
        assert_eq!(ot, ObjectType::Metadata);
        assert_eq!(hash, blake3::hash(path.as_bytes()).to_hex().to_string());
    }

    #[test]
    fn parse_file_index_manifest_metadata_path() {
        let path = "org/repo/file_index_db/manifest/00000000000000000008.manifest";
        let (bucket, repo, ot, hash) = parse_object_path(path).unwrap();
        assert_eq!(bucket, "");
        assert_eq!(repo, "org/repo");
        assert_eq!(ot, ObjectType::Metadata);
        assert_eq!(hash, blake3::hash(path.as_bytes()).to_hex().to_string());
    }

    #[test]
    fn parse_file_index_wal_metadata_path() {
        let path = "org/repo/file_index_db/wal/00000000000000000001.sst";
        let (bucket, repo, ot, hash) = parse_object_path(path).unwrap();
        assert_eq!(bucket, "");
        assert_eq!(repo, "org/repo");
        assert_eq!(ot, ObjectType::Metadata);
        assert_eq!(hash, blake3::hash(path.as_bytes()).to_hex().to_string());
    }

    #[test]
    fn parse_file_index_compactions_metadata_path() {
        let path = "org/repo/file_index_db/compactions/00000000000000000001.compactions";
        let (bucket, repo, ot, hash) = parse_object_path(path).unwrap();
        assert_eq!(bucket, "");
        assert_eq!(repo, "org/repo");
        assert_eq!(ot, ObjectType::Metadata);
        assert_eq!(hash, blake3::hash(path.as_bytes()).to_hex().to_string());
    }

    #[test]
    fn parse_chunk_index_metadata_path() {
        let path = ".crab/chunk_index_db/manifest/00000000000000000008.manifest";
        let (bucket, repo, ot, hash) = parse_object_path(path).unwrap();
        assert_eq!(bucket, "");
        assert_eq!(repo, ".crab");
        assert_eq!(ot, ObjectType::Metadata);
        assert_eq!(hash, blake3::hash(path.as_bytes()).to_hex().to_string());
    }

    #[test]
    fn parse_invalid_path_returns_none() {
        assert!(parse_object_path("b/org/repo/refs/heads/main").is_none());
        assert!(parse_object_path("").is_none());
        assert!(parse_object_path("bucket-only").is_none());
        assert!(parse_object_path(".crab/shards/deadbeef").is_none());
        assert!(parse_object_path(&format!(".crab/file-index/{}", hex_hash('e'))).is_none());
        assert!(parse_object_path("b/org/repo/xet/xorbs/not-hex").is_none());
        assert!(parse_object_path("org/repo/packs/pack-abc.meta").is_none());
        assert!(parse_object_path("org/repo/packs/pack-abc").is_none());
        assert!(parse_object_path("org/repo/file_index_db/manifest/").is_none());
        assert!(parse_object_path(".crab/chunk_index_db/manifest/current").is_none());
    }

    #[test]
    fn dedup_scope_matches_path_boundaries() {
        assert!(dedup_scope_error(&DedupScope::All, "any/repo").is_none());
        assert!(
            dedup_scope_error(
                &DedupScope::BucketPrefix("org/team".to_string()),
                "org/team/model"
            )
            .is_none()
        );
        assert!(
            dedup_scope_error(
                &DedupScope::Repos(vec!["org/repo".to_string()]),
                "org/repo/sub"
            )
            .is_none()
        );
        assert!(
            dedup_scope_error(
                &DedupScope::BucketPrefix("org/team".to_string()),
                "org/team2/model"
            )
            .is_some()
        );
        assert!(
            dedup_scope_error(&DedupScope::Repos(vec!["org/repo".to_string()]), "org").is_some()
        );
    }

    #[test]
    fn normalize_repo_path_rejects_empty_and_parent_segments() {
        assert_eq!(normalize_repo_path("/org/repo/").unwrap(), "org/repo");
        assert!(normalize_repo_path("").is_err());
        assert!(normalize_repo_path("org//repo").is_err());
        assert!(normalize_repo_path("org/../repo").is_err());
        assert!(normalize_repo_path("org/./repo").is_err());
    }

    #[test]
    fn reject_leading_slash_xet_path() {
        let hash_hex = hex_hash('f');
        let result = parse_object_path(&format!("/b/org/repo/xet/xorbs/{hash_hex}"));
        assert!(result.is_none());
    }

    #[test]
    fn reject_deep_xet_repo_path() {
        let hash_hex = hex_hash('1');
        assert!(parse_object_path(&format!("b/org/team/sub/repo/xet/xorbs/{hash_hex}")).is_none());
    }

    #[test]
    fn range_header_parsing() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=0-99".parse().unwrap());
        let range = parse_range_header(&headers).unwrap().unwrap();
        assert_eq!(range, 0..100); // inclusive end → exclusive

        headers.insert(header::RANGE, "bytes=100-199".parse().unwrap());
        let range = parse_range_header(&headers).unwrap().unwrap();
        assert_eq!(range, 100..200);
    }

    #[test]
    fn range_header_missing_returns_none() {
        let headers = HeaderMap::new();
        assert!(parse_range_header(&headers).unwrap().is_none());
    }

    #[test]
    fn range_header_invalid_is_bad_request() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=abc-def".parse().unwrap());
        assert!(matches!(
            parse_range_header(&headers),
            Err(CacheServiceError::BadRequest { .. })
        ));

        headers.insert(header::RANGE, "items=0-10".parse().unwrap());
        assert!(matches!(
            parse_range_header(&headers),
            Err(CacheServiceError::BadRequest { .. })
        ));

        headers.insert(header::RANGE, "bytes=10-".parse().unwrap());
        assert!(matches!(
            parse_range_header(&headers),
            Err(CacheServiceError::BadRequest { .. })
        ));

        headers.insert(header::RANGE, "bytes=-10".parse().unwrap());
        assert!(matches!(
            parse_range_header(&headers),
            Err(CacheServiceError::BadRequest { .. })
        ));
    }
}
