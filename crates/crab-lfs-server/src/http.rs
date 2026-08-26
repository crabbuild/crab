//! HTTP routing and Git LFS protocol handlers.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use crab_lfs::{LfsError, LfsLockManager, LfsObjectStore, LockRecord};
use crab_storage::{StorageError, Store, UrlObjectStore};
use futures_util::StreamExt;
use object_store::ObjectMeta;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tower::ServiceBuilder;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::auth::{AuthPolicy, ClientIdentity};
use crate::config::{ActionSecret, LfsServerConfig};

const LFS_JSON: &str = "application/vnd.git-lfs+json";
const MAX_BATCH_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_SMALL_BODY_BYTES: usize = 1024 * 1024;
const MAX_LOCK_RESULTS: usize = 1_000;
pub(crate) const MAX_CONCURRENT_REQUESTS: usize = 200;

/// Shared state for one gateway process.
pub struct AppState {
    /// Validated gateway configuration.
    pub config: Arc<LfsServerConfig>,
    /// Object-store backend and URL prefix shared by all repositories.
    pub origin: UrlObjectStore,
    /// Optional principal/repository authorization policy.
    pub policy: Option<AuthPolicy>,
    /// Maximum request body size represented as a platform-sized value.
    pub max_object_bytes: usize,
    /// Bounds concurrent streamed uploads.
    pub upload_permits: Arc<tokio::sync::Semaphore>,
    /// Bounds concurrent streamed downloads for the lifetime of each body.
    pub download_permits: Arc<tokio::sync::Semaphore>,
}

impl AppState {
    /// Returns whether native TLS is responsible for establishing identity.
    #[must_use]
    pub fn native_mtls(&self) -> bool {
        self.config
            .tls
            .as_ref()
            .is_some_and(|tls| tls.client_ca_path.is_some())
    }
}

/// Builds the authenticated Git LFS route tree.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(health_response))
        .route("/lfs/{*path}", any(dispatch))
        .route("/{*path}", any(dispatch))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::auth_middleware,
        ))
        .layer(
            ServiceBuilder::new()
                .layer(axum::extract::DefaultBodyLimit::max(state.max_object_bytes))
                .layer(RequestBodyLimitLayer::new(state.max_object_bytes))
                .concurrency_limit(MAX_CONCURRENT_REQUESTS)
                .layer(axum::error_handling::HandleErrorLayer::new(
                    handle_middleware_error,
                ))
                .timeout(state.config.request_timeout)
                .layer(TraceLayer::new_for_http().make_span_with(lfs_request_span)),
        )
        .with_state(state)
}

fn lfs_request_span<B>(request: &axum::http::Request<B>) -> tracing::Span {
    tracing::info_span!(
        "lfs_http_request",
        method = %request.method(),
        path = %request.uri().path(),
    )
}

async fn health_response() -> StatusCode {
    StatusCode::OK
}

async fn handle_middleware_error(error: tower::BoxError) -> impl IntoResponse {
    if error.is::<tower::timeout::error::Elapsed>() {
        return StatusCode::REQUEST_TIMEOUT;
    }
    tracing::warn!(%error, "LFS gateway middleware failed");
    StatusCode::INTERNAL_SERVER_ERROR
}

async fn dispatch(
    State(state): State<Arc<AppState>>,
    AxumPath(path): AxumPath<String>,
    request: Request,
) -> Response {
    let endpoint = match parse_endpoint(&path) {
        Ok(endpoint) => endpoint,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    let method = request.method().clone();
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let identity = request
        .extensions()
        .get::<ClientIdentity>()
        .cloned()
        .unwrap_or(ClientIdentity {
            principal: "anonymous".to_owned(),
        });
    let body = request.into_body();

    match endpoint.kind {
        EndpointKind::Discovery => {
            if method != Method::GET {
                return method_not_allowed("GET");
            }
            if !authorized(&state, &identity, &endpoint.repository, "read") {
                return forbidden();
            }
            discovery_response(&state, &headers, &endpoint.repository)
        }
        EndpointKind::Batch => {
            if method != Method::POST {
                return method_not_allowed("POST");
            }
            batch(
                &state,
                &headers,
                &endpoint.repository,
                &identity,
                &uri,
                body,
            )
            .await
        }
        EndpointKind::Object { oid } => {
            if method == Method::GET || method == Method::HEAD {
                let action_size = match validate_action(
                    &state,
                    &uri,
                    &endpoint.repository,
                    &oid,
                    ActionOperation::Download,
                ) {
                    Ok(size) => size,
                    Err(response) => return response,
                };
                if action_size.is_none()
                    && !authorized(&state, &identity, &endpoint.repository, "read")
                {
                    return forbidden();
                }
                download(
                    &state,
                    &headers,
                    &endpoint.repository,
                    &oid,
                    method == Method::HEAD,
                    action_size,
                )
                .await
            } else if method == Method::PUT {
                let action_size = match validate_action(
                    &state,
                    &uri,
                    &endpoint.repository,
                    &oid,
                    ActionOperation::Upload,
                ) {
                    Ok(size) => size,
                    Err(response) => return response,
                };
                if action_size.is_none()
                    && !authorized(&state, &identity, &endpoint.repository, "write")
                {
                    return forbidden();
                }
                upload(
                    &state,
                    &headers,
                    &endpoint.repository,
                    &oid,
                    action_size,
                    body,
                )
                .await
            } else {
                method_not_allowed("GET, HEAD, PUT")
            }
        }
        EndpointKind::Verify { oid } => {
            if method != Method::POST {
                return method_not_allowed("POST");
            }
            let action_size = match validate_action(
                &state,
                &uri,
                &endpoint.repository,
                &oid,
                ActionOperation::Verify,
            ) {
                Ok(size) => size,
                Err(response) => return response,
            };
            if action_size.is_none()
                && !authorized(&state, &identity, &endpoint.repository, "write")
            {
                return forbidden();
            }
            verify_object(&state, &endpoint.repository, &oid, action_size, body).await
        }
        EndpointKind::Locks => {
            if method == Method::GET {
                if !authorized(&state, &identity, &endpoint.repository, "read") {
                    return forbidden();
                }
                list_locks(&state, &endpoint.repository, &uri).await
            } else if method == Method::POST {
                if !authorized(&state, &identity, &endpoint.repository, "write") {
                    return forbidden();
                }
                create_lock(&state, &endpoint.repository, &identity, body).await
            } else {
                method_not_allowed("GET, POST")
            }
        }
        EndpointKind::LocksVerify => {
            if method != Method::POST {
                return method_not_allowed("POST");
            }
            if !authorized(&state, &identity, &endpoint.repository, "write") {
                return forbidden();
            }
            verify_locks(&state, &endpoint.repository, &identity, body).await
        }
        EndpointKind::Unlock { id } => {
            if method != Method::POST {
                return method_not_allowed("POST");
            }
            let request: UnlockRequest =
                match read_json_or_default(body, MAX_SMALL_BODY_BYTES).await {
                    Ok(request) => request,
                    Err(response) => return response,
                };
            let force = request.force.unwrap_or(false);
            let action = if force { "admin" } else { "write" };
            if !authorized(&state, &identity, &endpoint.repository, action) {
                return forbidden();
            }
            unlock(&state, &endpoint.repository, &identity, &id, force).await
        }
    }
}

#[derive(Debug)]
struct ParsedEndpoint {
    repository: String,
    kind: EndpointKind,
}

#[derive(Debug)]
enum EndpointKind {
    Discovery,
    Batch,
    Object { oid: String },
    Verify { oid: String },
    Locks,
    LocksVerify,
    Unlock { id: String },
}

#[derive(Clone, Copy)]
enum ActionOperation {
    Download,
    Upload,
    Verify,
}

impl ActionOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Upload => "upload",
            Self::Verify => "verify",
        }
    }
}

fn parse_endpoint(path: &str) -> Result<ParsedEndpoint, String> {
    let path = path.trim_matches('/');
    let segments = path.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| {
        segment.is_empty() || *segment == "." || *segment == ".." || segment.contains('\\')
    }) {
        return Err("LFS path contains an invalid segment".to_owned());
    }
    let (repository_end, kind) = if segments.len() >= 2
        && segments[segments.len() - 2..] == ["info", "lfs"]
    {
        (segments.len() - 2, EndpointKind::Discovery)
    } else if segments.len() >= 4
        && segments[segments.len() - 4..] == ["info", "lfs", "objects", "batch"]
    {
        (segments.len() - 4, EndpointKind::Batch)
    } else if segments.len() >= 5
        && segments[segments.len() - 5] == "info"
        && segments[segments.len() - 4] == "lfs"
        && segments[segments.len() - 3] == "objects"
        && segments[segments.len() - 1] == "verify"
    {
        (
            segments.len() - 5,
            EndpointKind::Verify {
                oid: segments[segments.len() - 2].to_owned(),
            },
        )
    } else if segments.len() >= 4
        && segments[segments.len() - 4] == "info"
        && segments[segments.len() - 3] == "lfs"
        && segments[segments.len() - 2] == "objects"
    {
        (
            segments.len() - 4,
            EndpointKind::Object {
                oid: segments[segments.len() - 1].to_owned(),
            },
        )
    } else if segments.len() >= 4
        && segments[segments.len() - 4..] == ["info", "lfs", "locks", "verify"]
    {
        (segments.len() - 4, EndpointKind::LocksVerify)
    } else if segments.len() >= 3 && segments[segments.len() - 3..] == ["info", "lfs", "locks"] {
        (segments.len() - 3, EndpointKind::Locks)
    } else if segments.len() >= 5
        && segments[segments.len() - 5] == "info"
        && segments[segments.len() - 4] == "lfs"
        && segments[segments.len() - 3] == "locks"
        && segments[segments.len() - 1] == "unlock"
    {
        (
            segments.len() - 5,
            EndpointKind::Unlock {
                id: segments[segments.len() - 2].to_owned(),
            },
        )
    } else {
        return Err("unknown Git LFS endpoint".to_owned());
    };
    if repository_end == 0 {
        return Err("repository path is required".to_owned());
    }
    let repository = segments[..repository_end].join("/");
    let repository = repository
        .strip_suffix(".git")
        .filter(|value| !value.is_empty())
        .unwrap_or(&repository)
        .to_owned();
    if repository.is_empty() {
        return Err("repository path is required".to_owned());
    }
    Ok(ParsedEndpoint { repository, kind })
}

/// Returns whether a request has the shape of a Batch action URL.
///
/// The HTTP handler still verifies the signature, operation, object, size, and
/// expiry. This predicate only lets a standard Git LFS client reach that
/// validation without forwarding the original repository credential.
pub(crate) fn is_signed_action_candidate(method: &Method, uri: &Uri) -> bool {
    let endpoint = match parse_endpoint(uri.path().trim_start_matches('/')) {
        Ok(endpoint) => endpoint,
        Err(_) => return false,
    };
    let method_matches = match endpoint.kind {
        EndpointKind::Object { .. } => {
            matches!(method, &Method::GET | &Method::HEAD | &Method::PUT)
        }
        EndpointKind::Verify { .. } => method == Method::POST,
        _ => false,
    };
    if !method_matches {
        return false;
    }
    let Some(raw_query) = uri.query() else {
        return false;
    };
    let mut expires = false;
    let mut size = false;
    let mut token = false;
    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        match key.as_ref() {
            "expires" => expires |= !value.is_empty(),
            "size" => size |= !value.is_empty(),
            "token" => token |= !value.is_empty(),
            _ => {}
        }
    }
    expires && size && token
}

fn authorized(state: &AppState, identity: &ClientIdentity, repository: &str, action: &str) -> bool {
    state
        .policy
        .as_ref()
        .is_none_or(|policy| policy.is_authorized(&identity.principal, repository, action))
}

fn lfs_store(state: &AppState, repository: &str) -> LfsObjectStore {
    let origin_prefix = state.origin.prefix().as_ref().trim_matches('/');
    let prefix = if origin_prefix.is_empty() {
        repository.to_owned()
    } else {
        format!("{origin_prefix}/{repository}")
    };
    LfsObjectStore::new(Store::new(state.origin.store_arc()), &prefix)
}

fn lock_manager(state: &AppState, repository: &str) -> LfsLockManager {
    let origin_prefix = state.origin.prefix().as_ref().trim_matches('/');
    let prefix = if origin_prefix.is_empty() {
        repository.to_owned()
    } else {
        format!("{origin_prefix}/{repository}")
    };
    LfsLockManager::lfs(Store::new(state.origin.store_arc()), &prefix)
}

fn lfs_endpoint_url(state: &AppState, headers: &HeaderMap, repository: &str) -> String {
    let path = format!("/{repository}.git/info/lfs");
    if let Some(public_url) = state.config.public_url.as_deref() {
        return format!("{public_url}{path}");
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return path;
    };
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| *value == "http" || *value == "https")
        .unwrap_or("http");
    format!("{scheme}://{host}{path}")
}

fn action_url(
    state: &AppState,
    endpoint: &str,
    repository: &str,
    oid: &str,
    size: u64,
    operation: ActionOperation,
) -> String {
    let path = match operation {
        ActionOperation::Verify => format!("{endpoint}/objects/{oid}/verify"),
        ActionOperation::Download | ActionOperation::Upload => {
            format!("{endpoint}/objects/{oid}")
        }
    };
    let Some(secret) = state.config.action_secret.as_ref() else {
        return path;
    };
    let expires = unix_seconds().saturating_add(state.config.action_ttl.as_secs());
    let token = action_token(secret, repository, operation, oid, size, expires);
    format!("{path}?expires={expires}&size={size}&token={token}")
}

#[expect(
    clippy::result_large_err,
    reason = "HTTP response is the intentional handler error boundary"
)]
fn validate_action(
    state: &AppState,
    uri: &Uri,
    repository: &str,
    oid: &str,
    operation: ActionOperation,
) -> Result<Option<u64>, Response> {
    let Some(secret) = state.config.action_secret.as_ref() else {
        return Ok(None);
    };
    let Some(raw_query) = uri.query() else {
        return Ok(None);
    };
    let mut expires = None;
    let mut size = None;
    let mut token = None;
    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        match key.as_ref() {
            "expires" if expires.is_none() => expires = Some(value.into_owned()),
            "size" if size.is_none() => size = Some(value.into_owned()),
            "token" if token.is_none() => token = Some(value.into_owned()),
            "expires" | "size" | "token" => return Err(invalid_action()),
            _ => {}
        }
    }
    if expires.is_none() && size.is_none() && token.is_none() {
        return Ok(None);
    }
    let expires = expires
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(invalid_action)?;
    if expires <= unix_seconds() {
        return Err(invalid_action());
    }
    let size = size
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|size| *size <= state.config.max_object_bytes)
        .ok_or_else(invalid_action)?;
    let token = token.ok_or_else(invalid_action)?;
    let expected = action_token(secret, repository, operation, oid, size, expires);
    if !constant_time_equal(token.as_bytes(), expected.as_bytes()) {
        return Err(invalid_action());
    }
    Ok(Some(size))
}

fn action_token(
    secret: &ActionSecret,
    repository: &str,
    operation: ActionOperation,
    oid: &str,
    size: u64,
    expires: u64,
) -> String {
    let message = format!(
        "crab-lfs-action\0{repository}\0{}\0{oid}\0{size}\0{expires}",
        operation.as_str()
    );
    blake3::keyed_hash(secret.key(), message.as_bytes())
        .to_hex()
        .to_string()
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn invalid_action() -> Response {
    error_response(StatusCode::FORBIDDEN, "invalid or expired LFS action")
}

fn json_response<T: Serialize>(status: StatusCode, value: T) -> Response {
    let mut response = (status, Json(value)).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(LFS_JSON));
    response
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    json_response(
        status,
        Message {
            message: message.into(),
        },
    )
}

fn discovery_response(state: &AppState, headers: &HeaderMap, repository: &str) -> Response {
    json_response(
        StatusCode::OK,
        DiscoveryResponse {
            href: lfs_endpoint_url(state, headers, repository),
        },
    )
}

fn forbidden() -> Response {
    error_response(StatusCode::FORBIDDEN, "repository access denied")
}

fn method_not_allowed(allow: &'static str) -> Response {
    let mut response = error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    if let Ok(value) = HeaderValue::from_str(allow) {
        response.headers_mut().insert(header::ALLOW, value);
    }
    response
}

fn empty_response(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

fn parse_oid(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.is_ascii() {
        return Err("LFS object OID must be 64 hexadecimal characters".to_owned());
    }
    let mut oid = [0u8; 32];
    for (index, byte) in oid.iter_mut().enumerate() {
        let high = hex_value(value.as_bytes()[index * 2])?;
        let low = hex_value(value.as_bytes()[index * 2 + 1])?;
        *byte = (high << 4) | low;
    }
    Ok(oid)
}

fn hex_value(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("LFS object OID must be hexadecimal".to_owned()),
    }
}

#[derive(Debug, Serialize)]
struct Message {
    message: String,
}

#[derive(Debug, Serialize)]
struct DiscoveryResponse {
    href: String,
}

#[derive(Debug, Deserialize)]
struct BatchRequest {
    operation: String,
    transfers: Option<Vec<String>>,
    objects: Vec<BatchObjectRequest>,
    hash_algo: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BatchObjectRequest {
    oid: String,
    size: u64,
}

#[derive(Debug, Serialize)]
struct BatchResponse {
    transfer: &'static str,
    objects: Vec<BatchObjectResponse>,
    hash_algo: &'static str,
}

#[derive(Debug, Serialize)]
struct BatchObjectResponse {
    oid: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    authenticated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<BatchActions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<BatchObjectError>,
}

#[derive(Debug, Serialize)]
struct BatchActions {
    #[serde(skip_serializing_if = "Option::is_none")]
    download: Option<BatchAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload: Option<BatchAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<BatchAction>,
}

#[derive(Debug, Serialize)]
struct BatchAction {
    href: String,
}

#[derive(Debug, Serialize)]
struct BatchObjectError {
    code: u16,
    message: String,
}

#[derive(Debug, Deserialize)]
struct VerifyRequest {
    oid: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct LockRequest {
    path: String,
}

#[derive(Debug, Deserialize, Default)]
struct UnlockRequest {
    force: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct LockVerifyRequest {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct LockResponse {
    lock: HttpLock,
}

#[derive(Debug, Serialize)]
struct LockListResponse {
    locks: Vec<HttpLock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct LockVerifyResponse {
    ours: Vec<HttpLock>,
    theirs: Vec<HttpLock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct HttpLock {
    id: String,
    path: String,
    locked_at: String,
    owner: LockOwner,
}

#[derive(Debug, Serialize)]
struct LockOwner {
    name: String,
}

#[derive(Debug, Deserialize, Default)]
struct LockQuery {
    path: Option<String>,
    id: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn batch(
    state: &AppState,
    headers: &HeaderMap,
    repository: &str,
    identity: &ClientIdentity,
    _uri: &Uri,
    body: Body,
) -> Response {
    let request: BatchRequest = match read_json(body, MAX_BATCH_BODY_BYTES).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.objects.len() > state.config.max_batch_objects {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "batch contains {} objects; maximum is {}",
                request.objects.len(),
                state.config.max_batch_objects
            ),
        );
    }
    if request.operation != "download" && request.operation != "upload" {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "operation must be download or upload",
        );
    }
    if request
        .transfers
        .as_ref()
        .is_some_and(|transfers| !transfers.is_empty() && !transfers.iter().any(|v| v == "basic"))
    {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the basic transfer adapter is not listed in transfers",
        );
    }
    if request
        .hash_algo
        .as_deref()
        .is_some_and(|value| value != "sha256")
    {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "only the sha256 LFS object hash is supported",
        );
    }
    let action = if request.operation == "upload" {
        "write"
    } else {
        "read"
    };
    if !authorized(state, identity, repository, action) {
        return forbidden();
    }

    let mut parsed_objects = Vec::with_capacity(request.objects.len());
    let mut sizes = HashMap::with_capacity(request.objects.len());
    for object in request.objects {
        if object.size > state.config.max_object_bytes {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("object {} exceeds the configured maximum size", object.oid),
            );
        }
        let oid = match parse_oid(&object.oid) {
            Ok(oid) => oid,
            Err(message) => return error_response(StatusCode::UNPROCESSABLE_ENTITY, message),
        };
        if sizes
            .insert(oid, object.size)
            .is_some_and(|size| size != object.size)
        {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("object {} was requested with conflicting sizes", object.oid),
            );
        }
        parsed_objects.push((object, oid));
    }

    let store = lfs_store(state, repository);
    let endpoint = lfs_endpoint_url(state, headers, repository);
    let mut objects = Vec::with_capacity(parsed_objects.len());
    for (object, oid) in parsed_objects {
        let response = if request.operation == "upload" {
            let upload_href = action_url(
                state,
                &endpoint,
                repository,
                &object.oid,
                object.size,
                ActionOperation::Upload,
            );
            let verify_href = action_url(
                state,
                &endpoint,
                repository,
                &object.oid,
                object.size,
                ActionOperation::Verify,
            );
            batch_upload_object(&store, &upload_href, &verify_href, object, oid).await
        } else {
            let download_href = action_url(
                state,
                &endpoint,
                repository,
                &object.oid,
                object.size,
                ActionOperation::Download,
            );
            batch_download_object(&store, &download_href, object, oid).await
        };
        objects.push(response);
    }
    json_response(
        StatusCode::OK,
        BatchResponse {
            transfer: "basic",
            objects,
            hash_algo: "sha256",
        },
    )
}

async fn batch_upload_object(
    store: &LfsObjectStore,
    upload_href: &str,
    verify_href: &str,
    object: BatchObjectRequest,
    oid: [u8; 32],
) -> BatchObjectResponse {
    let result = match store.head(&oid).await {
        Ok(meta) => {
            if meta.size != object.size {
                return BatchObjectResponse {
                    oid: object.oid,
                    size: object.size,
                    authenticated: None,
                    actions: None,
                    error: Some(BatchObjectError {
                        code: 422,
                        message: format!(
                            "object exists with size {}; requested size is {}",
                            meta.size, object.size
                        ),
                    }),
                };
            }
            store.verify_size(&oid, object.size).await
        }
        Err(error) if is_missing(&error) => {
            return upload_action_response(upload_href, verify_href, object);
        }
        Err(error) => return object_error_response(object, 500, error.to_string()),
    };
    match result {
        Ok(()) => BatchObjectResponse {
            oid: object.oid,
            size: object.size,
            authenticated: None,
            actions: None,
            error: None,
        },
        Err(LfsError::ObjectCorrupt { .. }) => {
            upload_action_response(upload_href, verify_href, object)
        }
        Err(error) if is_missing(&error) => {
            upload_action_response(upload_href, verify_href, object)
        }
        Err(error) => object_error_response(object, 500, error.to_string()),
    }
}

async fn batch_download_object(
    store: &LfsObjectStore,
    download_href: &str,
    object: BatchObjectRequest,
    oid: [u8; 32],
) -> BatchObjectResponse {
    let meta = match store.head(&oid).await {
        Ok(meta) => meta,
        Err(error) if is_missing(&error) => {
            return object_error_response(object, 404, "object is not present");
        }
        Err(error) => return object_error_response(object, 500, error.to_string()),
    };
    if meta.size != object.size {
        let message = format!(
            "object exists with size {}; requested size is {}",
            meta.size, object.size
        );
        return object_error_response(object, 422, message);
    }
    if let Err(error) = store.verify_size(&oid, object.size).await {
        return object_error_response(object, 500, error.to_string());
    }
    let oid = object.oid.clone();
    BatchObjectResponse {
        oid,
        size: object.size,
        authenticated: Some(true),
        actions: Some(BatchActions {
            download: Some(BatchAction {
                href: download_href.to_owned(),
            }),
            upload: None,
            verify: None,
        }),
        error: None,
    }
}

fn upload_action_response(
    upload_href: &str,
    verify_href: &str,
    object: BatchObjectRequest,
) -> BatchObjectResponse {
    let oid = object.oid.clone();
    BatchObjectResponse {
        oid,
        size: object.size,
        authenticated: Some(true),
        actions: Some(BatchActions {
            download: None,
            upload: Some(BatchAction {
                href: upload_href.to_owned(),
            }),
            verify: Some(BatchAction {
                href: verify_href.to_owned(),
            }),
        }),
        error: None,
    }
}

fn object_error_response(
    object: BatchObjectRequest,
    code: u16,
    message: impl Into<String>,
) -> BatchObjectResponse {
    BatchObjectResponse {
        oid: object.oid,
        size: object.size,
        authenticated: None,
        actions: None,
        error: Some(BatchObjectError {
            code,
            message: message.into(),
        }),
    }
}

fn is_missing(error: &LfsError) -> bool {
    matches!(error, LfsError::ObjectMissing { .. })
        || matches!(
            error,
            LfsError::Storage {
                source: StorageError::NotFound { .. }
            }
        )
}

#[allow(clippy::result_large_err)]
async fn read_json<T>(body: Body, limit: usize) -> Result<T, Response>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = to_bytes(body, limit)
        .await
        .map_err(|source| error_response(StatusCode::PAYLOAD_TOO_LARGE, source.to_string()))?;
    serde_json::from_slice(&bytes)
        .map_err(|source| error_response(StatusCode::UNPROCESSABLE_ENTITY, source.to_string()))
}

#[allow(clippy::result_large_err)]
async fn read_json_or_default<T>(body: Body, limit: usize) -> Result<T, Response>
where
    T: for<'de> Deserialize<'de> + Default,
{
    let bytes = to_bytes(body, limit)
        .await
        .map_err(|source| error_response(StatusCode::PAYLOAD_TOO_LARGE, source.to_string()))?;
    if bytes.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes)
        .map_err(|source| error_response(StatusCode::UNPROCESSABLE_ENTITY, source.to_string()))
}

async fn download(
    state: &AppState,
    headers: &HeaderMap,
    repository: &str,
    oid_value: &str,
    head_only: bool,
    action_size: Option<u64>,
) -> Response {
    let oid = match parse_oid(oid_value) {
        Ok(oid) => oid,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    let store = lfs_store(state, repository);
    let meta = match store.head(&oid).await {
        Ok(meta) => meta,
        Err(error) => return lfs_error_response(error),
    };
    if action_size.is_some_and(|size| size != meta.size) {
        return invalid_action();
    }
    if head_only {
        return object_headers_response(StatusCode::OK, &meta, oid_value, None);
    }
    let range = match parse_range(headers.get(header::RANGE), meta.size) {
        Ok(range) => range,
        Err(response) => return response,
    };
    let permit = match Arc::clone(&state.download_permits).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "download service is shutting down",
            );
        }
    };
    let (meta, actual_range, stream) = match store.get_stream(&oid, meta.size, range).await {
        Ok(result) => result,
        Err(error) => return lfs_error_response(error),
    };
    let stream = timed_download_stream(stream, permit, state.config.request_timeout);
    let status = if actual_range.start == 0 && actual_range.end == meta.size {
        StatusCode::OK
    } else {
        StatusCode::PARTIAL_CONTENT
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    set_object_headers(&mut response, &meta, oid_value, Some(&actual_range));
    response
}

fn timed_download_stream(
    stream: crab_lfs::LfsByteStream,
    permit: tokio::sync::OwnedSemaphorePermit,
    timeout: std::time::Duration,
) -> impl futures_util::Stream<Item = std::io::Result<bytes::Bytes>> {
    let deadline = tokio::time::Instant::now() + timeout;
    futures_util::stream::unfold(
        (Some(stream), Some(permit), deadline),
        |(stream, permit, deadline)| async move {
            let mut stream = stream?;
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(chunk)) => Some((
                    chunk.map_err(std::io::Error::other),
                    (Some(stream), permit, deadline),
                )),
                Ok(None) => None,
                Err(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "LFS download exceeded request timeout",
                    )),
                    (None, permit, deadline),
                )),
            }
        },
    )
}

async fn upload(
    state: &AppState,
    headers: &HeaderMap,
    repository: &str,
    oid_value: &str,
    action_size: Option<u64>,
    body: Body,
) -> Response {
    let oid = match parse_oid(oid_value) {
        Ok(oid) => oid,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    let content_length = match content_length(headers) {
        Ok(length) => length,
        Err(response) => return response,
    };
    if action_size.is_some_and(|size| content_length.is_some_and(|length| length != size)) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "upload size does not match the Batch action",
        );
    }
    if content_length.is_some_and(|size| size > state.config.max_object_bytes) {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload exceeds the configured maximum object size",
        );
    }
    let _permit = match Arc::clone(&state.upload_permits).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "upload service is shutting down",
            );
        }
    };
    let temporary = match tempfile::NamedTempFile::new_in(&state.config.spool_dir) {
        Ok(file) => file.into_temp_path(),
        Err(source) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, source.to_string());
        }
    };
    let mut file = match tokio::fs::File::create(&temporary).await {
        Ok(file) => file,
        Err(source) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, source.to_string());
        }
    };
    let mut received = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(source) => return error_response(StatusCode::BAD_REQUEST, source.to_string()),
        };
        let chunk_size = match u64::try_from(chunk.len()) {
            Ok(size) => size,
            Err(_) => {
                return error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "upload chunk length does not fit in u64",
                );
            }
        };
        let next = match received.checked_add(chunk_size) {
            Some(next) => next,
            None => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "upload is too large"),
        };
        if next > state.config.max_object_bytes {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload exceeds the configured maximum object size",
            );
        }
        if content_length.is_some_and(|expected| next > expected) {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "upload body is larger than Content-Length",
            );
        }
        if let Err(source) = file.write_all(&chunk).await {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, source.to_string());
        }
        received = next;
    }
    if content_length.is_some_and(|expected| expected != received) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Content-Length does not match body size {received}"),
        );
    }
    if action_size.is_some_and(|expected| expected != received) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "upload size does not match the Batch action",
        );
    }
    if let Err(source) = file.flush().await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, source.to_string());
    }
    if let Err(source) = file.sync_all().await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, source.to_string());
    }
    drop(file);

    let store = lfs_store(state, repository);
    match store
        .put_stream_with_size(&oid, content_length.or(action_size), &temporary)
        .await
    {
        Ok(()) => empty_response(StatusCode::OK),
        Err(error) => lfs_error_response(error),
    }
}

async fn verify_object(
    state: &AppState,
    repository: &str,
    oid_value: &str,
    action_size: Option<u64>,
    body: Body,
) -> Response {
    let request: VerifyRequest = match read_json(body, MAX_SMALL_BODY_BYTES).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let path_oid = match parse_oid(oid_value) {
        Ok(oid) => oid,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    let request_oid = match parse_oid(&request.oid) {
        Ok(oid) => oid,
        Err(message) => return error_response(StatusCode::UNPROCESSABLE_ENTITY, message),
    };
    if path_oid != request_oid {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "verify request OID does not match the URL",
        );
    }
    if request.size > state.config.max_object_bytes {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "object exceeds the configured maximum object size",
        );
    }
    if action_size.is_some_and(|size| size != request.size) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "verify size does not match the Batch action",
        );
    }
    match lfs_store(state, repository)
        .verify_size(&path_oid, request.size)
        .await
    {
        Ok(()) => json_response(StatusCode::OK, serde_json::json!({})),
        Err(error) => lfs_error_response(error),
    }
}

#[expect(
    clippy::result_large_err,
    reason = "HTTP response is the intentional handler error boundary"
)]
fn content_length(headers: &HeaderMap) -> Result<Option<u64>, Response> {
    let Some(value) = headers.get(header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|source| error_response(StatusCode::BAD_REQUEST, source.to_string()))?;
    let value = value
        .parse::<u64>()
        .map_err(|source| error_response(StatusCode::BAD_REQUEST, source.to_string()))?;
    Ok(Some(value))
}

#[expect(
    clippy::result_large_err,
    reason = "HTTP response is the intentional handler error boundary"
)]
fn parse_range(value: Option<&HeaderValue>, size: u64) -> Result<Option<Range<u64>>, Response> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|source| range_error(size, source.to_string()))?;
    if value.matches(',').count() > 0 || !value.starts_with("bytes=") {
        return Err(range_error(size, "only one bytes range is supported"));
    }
    let value = &value["bytes=".len()..];
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| range_error(size, "invalid bytes range"))?;
    if start.is_empty() {
        let suffix = end
            .parse::<u64>()
            .map_err(|source| range_error(size, source.to_string()))?;
        if suffix == 0 || size == 0 {
            return Err(range_error(size, "range is unsatisfiable"));
        }
        return Ok(Some(size.saturating_sub(suffix)..size));
    }
    let start = start
        .parse::<u64>()
        .map_err(|source| range_error(size, source.to_string()))?;
    if start >= size {
        return Err(range_error(size, "range start is outside the object"));
    }
    let end = if end.is_empty() {
        size
    } else {
        let end = end
            .parse::<u64>()
            .map_err(|source| range_error(size, source.to_string()))?;
        end.saturating_add(1).min(size)
    };
    if end <= start {
        return Err(range_error(size, "range end precedes range start"));
    }
    Ok(Some(start..end))
}

fn range_error(size: u64, message: impl Into<String>) -> Response {
    let mut response = error_response(StatusCode::RANGE_NOT_SATISFIABLE, message);
    if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response
}

fn object_headers_response(
    status: StatusCode,
    meta: &ObjectMeta,
    oid: &str,
    range: Option<&Range<u64>>,
) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    set_object_headers(&mut response, meta, oid, range);
    response
}

fn set_object_headers(
    response: &mut Response,
    meta: &ObjectMeta,
    oid: &str,
    range: Option<&Range<u64>>,
) {
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    if let Some(range) = range {
        let length = range.end.saturating_sub(range.start);
        if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
            response.headers_mut().insert(header::CONTENT_LENGTH, value);
        }
        if let Ok(value) = HeaderValue::from_str(&format!(
            "bytes {}-{}/{}",
            range.start,
            range.end.saturating_sub(1),
            meta.size
        )) {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
    } else if let Ok(value) = HeaderValue::from_str(&meta.size.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("\"{oid}\"")) {
        response.headers_mut().insert(header::ETAG, value);
    }
}

fn lfs_error_response(error: LfsError) -> Response {
    let status = match &error {
        LfsError::ObjectMissing { .. } => StatusCode::NOT_FOUND,
        LfsError::ObjectCorrupt { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        LfsError::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        LfsError::Storage { source } => match source {
            StorageError::NotFound { .. } => StatusCode::NOT_FOUND,
            StorageError::Forbidden { .. } | StorageError::AuthFailed { .. } => {
                StatusCode::BAD_GATEWAY
            }
            _ => StatusCode::BAD_GATEWAY,
        },
    };
    error_response(status, error.to_string())
}

async fn list_locks(state: &AppState, repository: &str, uri: &Uri) -> Response {
    let query = match parse_lock_query(uri) {
        Ok(query) => query,
        Err(response) => return response,
    };
    if let Some(path) = query.path.as_deref()
        && let Err(message) = validate_lock_path(path)
    {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    let manager = lock_manager(state, repository);
    let mut records = match manager.list().await {
        Ok(records) => records,
        Err(error) => return lock_error_response(error),
    };
    filter_and_page_locks(&mut records, &query);
    let next_cursor = next_cursor(&records, query.limit.unwrap_or(100));
    let locks = match records
        .into_iter()
        .map(http_lock)
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(locks) => locks,
        Err(response) => return response,
    };
    json_response(StatusCode::OK, LockListResponse { locks, next_cursor })
}

async fn create_lock(
    state: &AppState,
    repository: &str,
    identity: &ClientIdentity,
    body: Body,
) -> Response {
    let request: LockRequest = match read_json(body, MAX_SMALL_BODY_BYTES).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(message) = validate_lock_path(&request.path) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    let manager = lock_manager(state, repository);
    match manager
        .lock_exclusive(&request.path, &identity.principal)
        .await
    {
        Ok(record) => match http_lock(record) {
            Ok(lock) => json_response(StatusCode::CREATED, LockResponse { lock }),
            Err(response) => response,
        },
        Err(error @ crab_lfs::LfsLockError::Conflict { .. }) => {
            match manager.find_by_path(&request.path).await {
                Ok(record) => match http_lock(record) {
                    Ok(lock) => json_response(StatusCode::CONFLICT, LockResponse { lock }),
                    Err(response) => response,
                },
                Err(_) => lock_error_response(error),
            }
        }
        Err(error) => lock_error_response(error),
    }
}

async fn verify_locks(
    state: &AppState,
    repository: &str,
    identity: &ClientIdentity,
    body: Body,
) -> Response {
    let request: LockVerifyRequest = match read_json_or_default(body, MAX_SMALL_BODY_BYTES).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let limit = match parse_limit(request.limit) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let manager = lock_manager(state, repository);
    let mut records = match manager.list().await {
        Ok(records) => records,
        Err(error) => return lock_error_response(error),
    };
    if let Some(cursor) = request.cursor.as_deref() {
        records.retain(|record| record.id.as_str() > cursor);
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    let has_more = records.len() > limit;
    if has_more {
        records.truncate(limit);
    }
    let next_cursor = has_more
        .then(|| records.last().map(|record| record.id.clone()))
        .flatten();
    let mut ours = Vec::new();
    let mut theirs = Vec::new();
    for record in records {
        let lock = match http_lock(record) {
            Ok(lock) => lock,
            Err(response) => return response,
        };
        if lock.owner.name == identity.principal {
            ours.push(lock);
        } else {
            theirs.push(lock);
        }
    }
    json_response(
        StatusCode::OK,
        LockVerifyResponse {
            ours,
            theirs,
            next_cursor,
        },
    )
}

async fn unlock(
    state: &AppState,
    repository: &str,
    identity: &ClientIdentity,
    id: &str,
    force: bool,
) -> Response {
    if id.is_empty() || id.contains('/') || id.contains('\\') {
        return error_response(StatusCode::BAD_REQUEST, "invalid lock ID");
    }
    let manager = lock_manager(state, repository);
    let record = match manager.find_by_id(id).await {
        Ok(record) => record,
        Err(error) => return lock_error_response(error),
    };
    let result = if force {
        manager.force_unlock_with_id(&record.path, id).await
    } else {
        manager
            .unlock_with_id(&record.path, &identity.principal, Some(id))
            .await
    };
    match result {
        Ok(record) => match http_lock(record) {
            Ok(lock) => json_response(StatusCode::OK, LockResponse { lock }),
            Err(response) => response,
        },
        Err(error) => lock_error_response(error),
    }
}

#[expect(
    clippy::result_large_err,
    reason = "HTTP response is the intentional handler error boundary"
)]
fn parse_lock_query(uri: &Uri) -> Result<LockQuery, Response> {
    let mut query = LockQuery::default();
    let Some(raw_query) = uri.query() else {
        return Ok(query);
    };
    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        match key.as_ref() {
            "path" => query.path = Some(value.into_owned()),
            "id" => query.id = Some(value.into_owned()),
            "cursor" => query.cursor = Some(value.into_owned()),
            "limit" => {
                query.limit = Some(value.parse::<usize>().map_err(|source| {
                    error_response(StatusCode::BAD_REQUEST, source.to_string())
                })?);
            }
            _ => {}
        }
    }
    query.limit = Some(parse_limit(query.limit)?);
    Ok(query)
}

#[expect(
    clippy::result_large_err,
    reason = "HTTP response is the intentional handler error boundary"
)]
fn parse_limit(limit: Option<usize>) -> Result<usize, Response> {
    let limit = limit.unwrap_or(100);
    if limit == 0 {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "lock result limit must be greater than zero",
        ));
    }
    Ok(limit.min(MAX_LOCK_RESULTS))
}

fn filter_and_page_locks(records: &mut Vec<LockRecord>, query: &LockQuery) {
    if let Some(path) = query.path.as_deref() {
        records.retain(|record| record.path == path);
    }
    if let Some(id) = query.id.as_deref() {
        records.retain(|record| record.id == id);
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    if let Some(cursor) = query.cursor.as_deref() {
        records.retain(|record| record.id.as_str() > cursor);
    }
    let limit = query.limit.unwrap_or(100);
    if records.len() > limit {
        records.truncate(limit + 1);
    }
}

fn next_cursor(records: &[LockRecord], limit: usize) -> Option<String> {
    if records.len() > limit {
        records.get(limit - 1).map(|record| record.id.clone())
    } else {
        None
    }
}

fn validate_lock_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err("lock path must be repository-relative".to_owned());
    }
    Ok(())
}

#[expect(
    clippy::result_large_err,
    reason = "HTTP response is the intentional handler error boundary"
)]
fn http_lock(record: LockRecord) -> Result<HttpLock, Response> {
    let timestamp = i64::try_from(record.locked_at)
        .map_err(|source| error_response(StatusCode::INTERNAL_SERVER_ERROR, source.to_string()))?;
    let locked_at = time::OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|source| error_response(StatusCode::INTERNAL_SERVER_ERROR, source.to_string()))?
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|source| error_response(StatusCode::INTERNAL_SERVER_ERROR, source.to_string()))?;
    Ok(HttpLock {
        id: record.id,
        path: record.path,
        locked_at,
        owner: LockOwner { name: record.owner },
    })
}

fn lock_error_response(error: crab_lfs::LfsLockError) -> Response {
    let status = match &error {
        crab_lfs::LfsLockError::Conflict { .. } | crab_lfs::LfsLockError::IdMismatch { .. } => {
            StatusCode::CONFLICT
        }
        crab_lfs::LfsLockError::NotFound { .. } => StatusCode::NOT_FOUND,
        crab_lfs::LfsLockError::Corrupt { .. } | crab_lfs::LfsLockError::Serialization(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        crab_lfs::LfsLockError::Storage(_) => StatusCode::BAD_GATEWAY,
    };
    error_response(status, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use base64::Engine;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn app() -> (Router, TempDir) {
        app_with_action_secret(None)
    }

    fn app_with_action_secret(secret: Option<&str>) -> (Router, TempDir) {
        app_with_action_secret_and_auth(secret, crate::auth::AuthConfig::None)
    }

    fn app_with_action_secret_and_auth(
        secret: Option<&str>,
        auth: crate::auth::AuthConfig,
    ) -> (Router, TempDir) {
        let spool = TempDir::new().expect("spool directory");
        let config = Arc::new(LfsServerConfig {
            listen_addr: "127.0.0.1:0".parse().expect("socket address"),
            public_url: None,
            spool_dir: spool.path().to_owned(),
            tls: None,
            auth,
            trust_proxy_mtls: false,
            policy_path: None,
            max_batch_objects: 10,
            max_object_bytes: 1024 * 1024,
            max_uploads: 2,
            request_timeout: std::time::Duration::from_secs(30),
            action_secret: secret
                .map(|value| ActionSecret::from_value(value).expect("action secret")),
            action_ttl: std::time::Duration::from_secs(900),
            origin_url: "memory://".to_owned(),
        });
        let state = Arc::new(AppState {
            config,
            origin: UrlObjectStore::new(Arc::new(InMemory::new()), ObjectPath::from("")),
            policy: None,
            max_object_bytes: 1024 * 1024,
            upload_permits: Arc::new(tokio::sync::Semaphore::new(2)),
            download_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        });
        (build_router(state), spool)
    }

    async fn request(app: &Router, method: Method, uri: &str, body: impl Into<Body>) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, LFS_JSON)
                    .body(body.into())
                    .expect("valid request"),
            )
            .await
            .expect("router response")
    }

    #[tokio::test]
    async fn standard_repository_url_discovery_preserves_git_lfs_shape() {
        let (app, _spool) = app();
        let response = request(&app, Method::GET, "/team/model.git/info/lfs", Body::empty()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_SMALL_BODY_BYTES)
            .await
            .expect("discovery body");
        let discovery: serde_json::Value = serde_json::from_slice(&body).expect("discovery JSON");
        assert_eq!(discovery["href"], "/team/model.git/info/lfs");
    }

    #[tokio::test]
    async fn basic_transfer_batch_upload_download_and_range_are_end_to_end() {
        let (app, _spool) = app();
        let payload = b"crab-lfs-http";
        let oid = format!("{:x}", Sha256::digest(payload));
        let batch = request(
            &app,
            Method::POST,
            "/repo.git/info/lfs/objects/batch",
            Body::from(
                serde_json::json!({
                    "operation": "upload",
                    "transfers": ["basic"],
                    "objects": [{"oid": oid, "size": payload.len()}]
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(batch.status(), StatusCode::OK);
        let batch_body = to_bytes(batch.into_body(), MAX_BATCH_BODY_BYTES)
            .await
            .expect("batch body");
        let batch_json: serde_json::Value =
            serde_json::from_slice(&batch_body).expect("batch JSON");
        assert_eq!(batch_json["objects"][0]["authenticated"], true);
        let upload = batch_json["objects"][0]["actions"]["upload"]["href"]
            .as_str()
            .expect("upload action");
        let upload_path = upload
            .strip_prefix("http://example.invalid")
            .unwrap_or(upload);
        let upload_uri = if upload_path.starts_with('/') {
            upload_path.to_owned()
        } else {
            format!("/{upload_path}")
        };
        let uploaded = request(
            &app,
            Method::PUT,
            &upload_uri
                .replace("http://127.0.0.1", "")
                .replace("http://localhost", ""),
            Body::from(payload.as_slice()),
        )
        .await;
        assert_eq!(uploaded.status(), StatusCode::OK);

        let batch = request(
            &app,
            Method::POST,
            "/repo/info/lfs/objects/batch",
            Body::from(
                serde_json::json!({
                    "operation": "download",
                    "objects": [{"oid": oid, "size": payload.len()}]
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(batch.status(), StatusCode::OK);
        let batch_body = to_bytes(batch.into_body(), MAX_BATCH_BODY_BYTES)
            .await
            .expect("batch body");
        let batch_json: serde_json::Value =
            serde_json::from_slice(&batch_body).expect("batch JSON");
        assert_eq!(batch_json["objects"][0]["authenticated"], true);
        assert!(
            batch_json["objects"][0]["actions"]["download"]["href"]
                .as_str()
                .is_some()
        );

        let downloaded = request(
            &app,
            Method::GET,
            &format!("/repo/info/lfs/objects/{oid}"),
            Body::empty(),
        )
        .await;
        assert_eq!(downloaded.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(downloaded.into_body(), payload.len() + 1)
                .await
                .expect("download body")
                .as_ref(),
            payload
        );

        let ranged = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/repo/info/lfs/objects/{oid}"))
                    .header(header::RANGE, "bytes=2-5")
                    .body(Body::empty())
                    .expect("valid range request"),
            )
            .await
            .expect("range response");
        assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            to_bytes(ranged.into_body(), payload.len())
                .await
                .expect("range body")
                .as_ref(),
            &payload[2..6]
        );
    }

    #[tokio::test]
    async fn signed_batch_actions_bind_operation_object_size_and_expiry() {
        let (app, _spool) = app_with_action_secret(Some("test action secret"));
        let payload = b"signed-crab-lfs-http";
        let oid = format!("{:x}", Sha256::digest(payload));
        let batch = request(
            &app,
            Method::POST,
            "/lfs/repo/info/lfs/objects/batch",
            Body::from(
                serde_json::json!({
                    "operation": "upload",
                    "objects": [{"oid": oid, "size": payload.len()}]
                })
                .to_string(),
            ),
        )
        .await;
        let batch_body = to_bytes(batch.into_body(), MAX_BATCH_BODY_BYTES)
            .await
            .expect("batch body");
        let batch_json: serde_json::Value =
            serde_json::from_slice(&batch_body).expect("batch JSON");
        assert_eq!(batch_json["objects"][0]["authenticated"], true);
        let upload = batch_json["objects"][0]["actions"]["upload"]["href"]
            .as_str()
            .expect("upload action");
        let verify = batch_json["objects"][0]["actions"]["verify"]["href"]
            .as_str()
            .expect("verify action");
        assert!(upload.contains("expires="));
        assert!(upload.contains("size="));
        assert!(upload.contains("token="));

        let tampered = request(
            &app,
            Method::PUT,
            &upload.replace("token=", "token=0"),
            Body::from(payload.as_slice()),
        )
        .await;
        assert_eq!(tampered.status(), StatusCode::FORBIDDEN);

        let expired = upload
            .split_once('?')
            .map(|(path, _)| format!("{path}?expires=0&size={}&token=invalid", payload.len()))
            .expect("signed action query");
        let expired = request(&app, Method::PUT, &expired, Body::from(payload.as_slice())).await;
        assert_eq!(expired.status(), StatusCode::FORBIDDEN);

        let uploaded = request(&app, Method::PUT, upload, Body::from(payload.as_slice())).await;
        assert_eq!(uploaded.status(), StatusCode::OK);
        let verified = request(
            &app,
            Method::POST,
            verify,
            Body::from(serde_json::json!({"oid": oid, "size": payload.len()}).to_string()),
        )
        .await;
        assert_eq!(verified.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn signed_actions_do_not_require_repository_credentials() {
        let mut users = HashMap::new();
        users.insert("alice".to_owned(), *blake3::hash(b"secret").as_bytes());
        let (app, _spool) = app_with_action_secret_and_auth(
            Some("test action secret"),
            crate::auth::AuthConfig::Basic { users },
        );
        let payload = b"credential-free-action";
        let oid = format!("{:x}", Sha256::digest(payload));
        let request_body = serde_json::json!({
            "operation": "upload",
            "objects": [{"oid": oid, "size": payload.len()}]
        })
        .to_string();
        let credentials = base64::engine::general_purpose::STANDARD.encode("alice:secret");
        let batch = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/repo.git/info/lfs/objects/batch")
                    .header(header::CONTENT_TYPE, LFS_JSON)
                    .header(header::AUTHORIZATION, format!("Basic {credentials}"))
                    .body(Body::from(request_body))
                    .expect("valid batch request"),
            )
            .await
            .expect("batch response");
        assert_eq!(batch.status(), StatusCode::OK);
        let batch_body = to_bytes(batch.into_body(), MAX_BATCH_BODY_BYTES)
            .await
            .expect("batch body");
        let batch_json: serde_json::Value =
            serde_json::from_slice(&batch_body).expect("batch JSON");
        let upload = batch_json["objects"][0]["actions"]["upload"]["href"]
            .as_str()
            .expect("upload action");

        let unauthenticated_batch = request(
            &app,
            Method::POST,
            "/repo.git/info/lfs/objects/batch",
            Body::from("{}"),
        )
        .await;
        assert_eq!(unauthenticated_batch.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthenticated_batch.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static(LFS_JSON))
        );
        assert_eq!(
            unauthenticated_batch.headers().get("lfs-authenticate"),
            Some(&HeaderValue::from_static("Basic realm=\"Git LFS\""))
        );

        let uploaded = request(&app, Method::PUT, upload, Body::from(payload.as_slice())).await;
        assert_eq!(uploaded.status(), StatusCode::OK);
        let downloaded = request(
            &app,
            Method::GET,
            &format!("/repo.git/info/lfs/objects/{oid}"),
            Body::empty(),
        )
        .await;
        assert_eq!(downloaded.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn streamed_download_timeout_releases_permit_after_body_error() {
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .expect("download permit");
        let source: crab_lfs::LfsByteStream = Box::pin(futures_util::stream::pending());
        let mut stream = Box::pin(timed_download_stream(
            source,
            permit,
            std::time::Duration::ZERO,
        ));

        let error = stream
            .next()
            .await
            .expect("timeout item")
            .expect_err("pending source should time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(permits.available_permits(), 0);
        assert!(stream.next().await.is_none());
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn file_locking_round_trip_uses_lfs_lock_contract() {
        let (app, _spool) = app();
        let created = request(
            &app,
            Method::POST,
            "/lfs/repo/info/lfs/locks",
            Body::from(serde_json::json!({"path": "model.bin"}).to_string()),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let created_body = to_bytes(created.into_body(), MAX_SMALL_BODY_BYTES)
            .await
            .expect("lock body");
        let lock_json: serde_json::Value =
            serde_json::from_slice(&created_body).expect("lock JSON");
        let id = lock_json["lock"]["id"].as_str().expect("lock ID");
        assert_eq!(lock_json["lock"]["owner"]["name"], "anonymous");
        assert!(lock_json["lock"]["locked_at"].as_str().is_some());

        let duplicate = request(
            &app,
            Method::POST,
            "/lfs/repo/info/lfs/locks",
            Body::from(serde_json::json!({"path": "model.bin"}).to_string()),
        )
        .await;
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        let duplicate_body = to_bytes(duplicate.into_body(), MAX_SMALL_BODY_BYTES)
            .await
            .expect("duplicate lock body");
        let duplicate_json: serde_json::Value =
            serde_json::from_slice(&duplicate_body).expect("duplicate lock JSON");
        assert_eq!(duplicate_json["lock"]["id"], id);

        let listed = request(
            &app,
            Method::GET,
            "/lfs/repo/info/lfs/locks?path=model.bin",
            Body::empty(),
        )
        .await;
        assert_eq!(listed.status(), StatusCode::OK);
        let listed_body = to_bytes(listed.into_body(), MAX_SMALL_BODY_BYTES)
            .await
            .expect("lock list body");
        let listed_json: serde_json::Value =
            serde_json::from_slice(&listed_body).expect("lock list JSON");
        assert_eq!(listed_json["locks"].as_array().expect("locks").len(), 1);

        let unlocked = request(
            &app,
            Method::POST,
            &format!("/lfs/repo/info/lfs/locks/{id}/unlock"),
            Body::from("{}"),
        )
        .await;
        assert_eq!(unlocked.status(), StatusCode::OK);
        let listed = request(&app, Method::GET, "/lfs/repo/info/lfs/locks", Body::empty()).await;
        let listed_body = to_bytes(listed.into_body(), MAX_SMALL_BODY_BYTES)
            .await
            .expect("empty lock list body");
        let listed_json: serde_json::Value =
            serde_json::from_slice(&listed_body).expect("empty lock list JSON");
        assert!(listed_json["locks"].as_array().expect("locks").is_empty());
    }
}
