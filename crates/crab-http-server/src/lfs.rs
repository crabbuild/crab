mod locking;

use std::{ops::Range, sync::Arc, time::Duration};

use axum::{
    Extension, Json,
    body::Body,
    extract::{
        FromRequest, Path, Query, Request, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use crab_git::lfs_pointer::{LFS_VERSION_URL, LfsPointer};
use crab_lfs::{LfsError, LfsLockError, LfsObjectStore};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::{
    app,
    auth::Principal,
    server::{Repository, Server},
};

const CONTENT_TYPE: &str = "application/vnd.git-lfs+json";
const BUDGET: Duration = Duration::from_secs(5 * 60);
type Result<T> = std::result::Result<T, Error>;

pub(crate) use locking::{create_lock, list_locks, unlock_lock, verify_locks};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("{0}")]
    Request(&'static str),
    #[error("repository not found")]
    NotFound,
    #[error("write access required")]
    Forbidden,
    #[error("repository is archived and read-only")]
    Archived,
    #[error("LFS transfer exceeds server limits")]
    TooLarge,
    #[error("LFS requests are busy")]
    Busy,
    #[error("LFS lock is owned by another user")]
    LockOwner,
    #[error("LFS lock changed concurrently")]
    LockConflict,
    #[error("LFS operation cancelled or timed out")]
    Cancelled,
    #[error("LFS byte range is not satisfiable")]
    RangeNotSatisfiable { size: u64 },
    #[error("invalid LFS request body")]
    Json(#[from] JsonRejection),
    #[error("invalid LFS lock query")]
    Query(#[from] QueryRejection),
    #[error("invalid LFS object identity")]
    Identity(#[from] crab_git::lfs_pointer::LfsPointerError),
    #[error("LFS object operation failed")]
    Object(#[from] LfsError),
    #[error("LFS lock operation failed")]
    Lock(#[from] LfsLockError),
    #[error("LFS lock coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
    #[error("invalid stored LFS lock timestamp")]
    LockTimestamp,
    #[error("LFS request stream failed")]
    Body(#[from] axum::Error),
    #[error("LFS temporary file operation failed")]
    Io(#[from] std::io::Error),
    #[error("LFS worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("repository settings failed")]
    Settings(#[source] Box<app::Error>),
}

impl From<crab_remote::publication::Error> for Error {
    fn from(error: crab_remote::publication::Error) -> Self {
        match error {
            crab_remote::publication::Error::Cancelled => Self::Cancelled,
            crab_remote::publication::Error::Coordination(error) => Self::Coordination(error),
        }
    }
}

impl From<crate::transfer_admission::Error> for Error {
    fn from(error: crate::transfer_admission::Error) -> Self {
        match error {
            crate::transfer_admission::Error::Busy => Self::Busy,
            crate::transfer_admission::Error::Cancelled => Self::Cancelled,
            crate::transfer_admission::Error::Coordination(error) => Self::Coordination(error),
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        if let Self::RangeNotSatisfiable { size } = &self {
            tracing::error!(error = ?self, "LFS transfer failed");
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [
                    (header::CONTENT_TYPE, CONTENT_TYPE.to_owned()),
                    (header::ACCEPT_RANGES, "bytes".to_owned()),
                    (header::CONTENT_RANGE, format!("bytes */{size}")),
                ],
                Json(json!({"message":"LFS byte range is not satisfiable"})),
            )
                .into_response();
        }
        tracing::error!(error = ?self, "LFS transfer failed");
        let (status, message) = match self {
            Self::Request(message) => (StatusCode::UNPROCESSABLE_ENTITY, message),
            Self::NotFound | Self::Object(LfsError::ObjectMissing { .. }) => {
                (StatusCode::NOT_FOUND, "Repository or LFS object not found")
            }
            Self::Forbidden => (StatusCode::FORBIDDEN, "Write access required"),
            Self::LockOwner => (
                StatusCode::FORBIDDEN,
                "Lock is owned by another user; use force to unlock it",
            ),
            Self::LockConflict
            | Self::Lock(LfsLockError::Conflict { .. } | LfsLockError::IdMismatch { .. }) => (
                StatusCode::CONFLICT,
                "LFS lock changed; refresh the lock list and retry",
            ),
            Self::Archived => (
                StatusCode::FORBIDDEN,
                "Repository is archived and read-only",
            ),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "LFS allows at most 200 objects per batch and 512 MiB per object",
            ),
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "LFS requests are busy; retry shortly",
            ),
            Self::Cancelled => (
                StatusCode::REQUEST_TIMEOUT,
                "LFS operation cancelled or timed out",
            ),
            Self::RangeNotSatisfiable { .. } => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                "LFS byte range is not satisfiable",
            ),
            Self::Json(error) => (error.status(), "Invalid LFS request"),
            Self::Query(error) => (error.status(), "Invalid LFS lock query"),
            Self::Body(_) | Self::Identity(_) => (StatusCode::BAD_REQUEST, "Invalid LFS request"),
            Self::Lock(LfsLockError::NotFound { .. }) => {
                (StatusCode::NOT_FOUND, "LFS lock not found")
            }
            Self::Object(LfsError::ObjectCorrupt { .. }) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "LFS object size or SHA-256 does not match",
            ),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "LFS storage unavailable; retry or inspect server logs",
            ),
        };
        (
            status,
            [(header::CONTENT_TYPE, CONTENT_TYPE)],
            Json(json!({"message":message})),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Operation {
    Upload,
    Download,
}

#[derive(Deserialize)]
pub(crate) struct Batch {
    operation: Operation,
    transfers: Option<Vec<String>>,
    hash_algo: Option<String>,
    objects: Vec<Object>,
}

#[derive(Deserialize)]
struct Object {
    oid: String,
    size: u64,
}

#[derive(Deserialize)]
pub(crate) struct Size {
    size: u64,
}

fn pointer(oid: &str, size: u64) -> Result<LfsPointer> {
    if size > crate::server::MAX_DEPENDENCY_FILE_BYTES {
        return Err(Error::TooLarge);
    }
    if oid.len() != 64 {
        return Err(Error::Request("Expected a SHA-256 object ID"));
    }
    Ok(LfsPointer::parse(
        format!("version {LFS_VERSION_URL}\noid sha256:{oid}\nsize {size}\n").as_bytes(),
    )?)
}

fn repository(
    server: &Server,
    principal: &Principal,
    owner: &str,
    name: &str,
    write: bool,
) -> Result<Arc<Repository>> {
    let name = name.strip_suffix(".git").ok_or(Error::NotFound)?;
    let entry = server
        .repositories
        .get(&(owner.to_owned(), name.to_owned()))
        .filter(|entry| principal.can_read(&entry.config))
        .ok_or(Error::NotFound)?;
    if write && !principal.can_write(&entry.config) {
        return Err(Error::Forbidden);
    }
    Ok(entry)
}

async fn ensure_active(repository: &Repository) -> Result<()> {
    if repository
        .lifecycle()
        .await
        .map_err(|error| Error::Settings(Box::new(error)))?
        .archived
    {
        return Err(Error::Archived);
    }
    Ok(())
}

pub(crate) async fn batch(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response> {
    repository(&server, &principal, &owner, &name, false)?;
    let _permit = server.admission.try_acquire().map_err(|_| Error::Busy)?;
    let Json(batch) = tokio::select! {
        () = server.cancellation.cancelled() => return Err(Error::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(30), Json::<Batch>::from_request(request, &server)) => result.map_err(|_| Error::Cancelled)??,
    };
    let upload = matches!(batch.operation, Operation::Upload);
    let entry = repository(&server, &principal, &owner, &name, upload)?;
    if upload {
        ensure_active(&entry).await?;
    }
    if batch.objects.len() > 200 {
        return Err(Error::TooLarge);
    }
    if batch
        .hash_algo
        .as_deref()
        .is_some_and(|hash| hash != "sha256")
        || batch
            .transfers
            .as_ref()
            .is_some_and(|transfers| !transfers.iter().any(|transfer| transfer == "basic"))
    {
        return Err(Error::Request("Only SHA-256 basic transfers are supported"));
    }
    let objects = batch
        .objects
        .iter()
        .map(|object| pointer(&object.oid, object.size))
        .collect::<Result<Vec<_>>>()?;
    // Host was validated by the boundary; authenticated deployments always use
    // their configured origin so action URLs cannot redirect Git credentials.
    let origin = match &server.auth {
        Some(auth) => auth.origin(),
        None => format!(
            "http://{}",
            headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .ok_or(Error::NotFound)?
        ),
    };
    let mut action_headers = serde_json::Map::new();
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        action_headers.insert("Authorization".into(), json!(value));
    }
    let lfs = LfsObjectStore::new(entry.store.clone(), &entry.config.prefix);
    let work = async {
        let mut results = Vec::with_capacity(objects.len());
        for (object, pointer) in batch.objects.iter().zip(objects) {
            let mut result = json!({"oid":object.oid,"size":object.size,"authenticated":true});
            match lfs.verify_size(&pointer.oid, pointer.size).await {
                Ok(()) if upload => {}
                Ok(()) => {
                    result["actions"] = json!({"download":{"href":format!("{origin}/git/{owner}/{name}/info/lfs/objects/{}?size={}",object.oid,object.size),"header":action_headers}});
                }
                Err(LfsError::ObjectMissing { .. } | LfsError::ObjectCorrupt { .. }) if upload => {
                    result["actions"] = json!({"upload":{"href":format!("{origin}/git/{owner}/{name}/info/lfs/objects/{}?size={}",object.oid,object.size),"header":action_headers}});
                }
                Err(LfsError::ObjectMissing { .. }) => {
                    result["error"] = json!({"code":404,"message":"LFS object not found"})
                }
                Err(LfsError::ObjectCorrupt { .. }) => {
                    result["error"] =
                        json!({"code":422,"message":"LFS object size or SHA-256 does not match"})
                }
                Err(error) => return Err(Error::Object(error)),
            }
            results.push(result);
        }
        Ok::<_, Error>(results)
    };
    let results = tokio::select! {
        () = server.cancellation.cancelled() => return Err(Error::Cancelled),
        result = tokio::time::timeout(BUDGET, work) => result.map_err(|_| Error::Cancelled)??,
    };
    Ok((
        [(header::CONTENT_TYPE, CONTENT_TYPE)],
        Json(json!({"transfer":"basic","hash_algo":"sha256","objects":results})),
    )
        .into_response())
}

pub(crate) async fn download(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid)): Path<(String, String, String)>,
    Query(size): Query<Size>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response> {
    let entry = repository(&server, &principal, &owner, &name, false)?;
    let pointer = pointer(&oid, size.size)?;
    let etag = format!("\"{}\"", crab_git::lfs_pointer::hex_encode(&pointer.oid));
    let range = if method == Method::GET {
        requested_range(&headers, pointer.size, &etag)?
    } else {
        None
    };
    let cancel = server.cancellation.child_token();
    let permit = server.acquire_transfer(&cancel).await?;
    let guard = cancel.clone().drop_guard();
    let lfs = LfsObjectStore::new(entry.store.clone(), &entry.config.prefix);
    let deadline = tokio::time::Instant::now() + BUDGET;
    let (_, delivered_range, stream) = tokio::select! {
        () = cancel.cancelled() => return Err(Error::Cancelled),
        result = tokio::time::timeout_at(deadline, lfs.get_stream(&pointer.oid, pointer.size, range.clone())) => result.map_err(|_| Error::Cancelled)??,
    };
    let stream = stream.take_until(async move {
        tokio::select! { () = cancel.cancelled() => {}, () = tokio::time::sleep_until(deadline) => {} }
    }).map(move |chunk| {
        // The response owns transfer capacity through its last byte or disconnect.
        let _ = (&permit, &guard);
        chunk
    });
    let body = Body::from_stream(stream);
    if range.is_some() {
        return Ok((
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::ACCEPT_RANGES, "bytes".to_owned()),
                (
                    header::CONTENT_RANGE,
                    format!(
                        "bytes {}-{}/{}",
                        delivered_range.start,
                        delivered_range.end - 1,
                        pointer.size
                    ),
                ),
                (
                    header::CONTENT_LENGTH,
                    (delivered_range.end - delivered_range.start).to_string(),
                ),
                (header::ETAG, etag),
            ],
            body,
        )
            .into_response());
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::ACCEPT_RANGES, "bytes".to_owned()),
            (header::CONTENT_LENGTH, pointer.size.to_string()),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response())
}

pub(crate) fn requested_range(
    headers: &HeaderMap,
    size: u64,
    etag: &str,
) -> Result<Option<Range<u64>>> {
    let mut values = headers.get_all(header::RANGE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Ok(None);
    }
    if !if_range_matches(headers, etag) {
        return Ok(None);
    }
    let value = value
        .to_str()
        .map_err(|_| Error::RangeNotSatisfiable { size })?;
    let Some((unit, ranges)) = value.split_once('=') else {
        return Err(Error::RangeNotSatisfiable { size });
    };
    if !unit.eq_ignore_ascii_case("bytes") || ranges.contains(',') {
        return Ok(None);
    }
    parse_byte_range(value, size).map(Some)
}

fn if_range_matches(headers: &HeaderMap, etag: &str) -> bool {
    let mut values = headers.get_all(header::IF_RANGE).iter();
    let Some(value) = values.next() else {
        return true;
    };
    values.next().is_none() && value.as_bytes() == etag.as_bytes()
}

pub(crate) fn parse_byte_range(value: &str, size: u64) -> Result<Range<u64>> {
    let (unit, range) = value
        .split_once('=')
        .ok_or(Error::RangeNotSatisfiable { size })?;
    if !unit.eq_ignore_ascii_case("bytes") || range.contains(',') {
        return Err(Error::RangeNotSatisfiable { size });
    }
    let (first, last) = range
        .split_once('-')
        .ok_or(Error::RangeNotSatisfiable { size })?;
    if first.is_empty() {
        let suffix = last
            .parse::<u64>()
            .map_err(|_| Error::RangeNotSatisfiable { size })?;
        if suffix == 0 || size == 0 {
            return Err(Error::RangeNotSatisfiable { size });
        }
        return Ok(size.saturating_sub(suffix)..size);
    }
    let first = first
        .parse::<u64>()
        .map_err(|_| Error::RangeNotSatisfiable { size })?;
    if first >= size {
        return Err(Error::RangeNotSatisfiable { size });
    }
    let end = if last.is_empty() {
        size
    } else {
        let last = last
            .parse::<u64>()
            .map_err(|_| Error::RangeNotSatisfiable { size })?;
        if last < first {
            return Err(Error::RangeNotSatisfiable { size });
        }
        last.saturating_add(1).min(size)
    };
    Ok(first..end)
}

pub(crate) async fn upload(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid)): Path<(String, String, String)>,
    Query(size): Query<Size>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response> {
    ensure_active(repository(&server, &principal, &owner, &name, true)?.as_ref()).await?;
    let pointer = pointer(&oid, size.size)?;
    if headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|value| value != "identity")
    {
        return Err(Error::Request(
            "LFS uploads require identity content encoding",
        ));
    }
    let cancel = server.cancellation.child_token();
    let permit = server.acquire_transfer(&cancel).await?;
    let _guard = cancel.clone().drop_guard();
    let worker_server = Arc::clone(&server);
    let (send, result) = tokio::sync::oneshot::channel();
    server.receives.spawn(async move {
        let _permit = permit;
        let work = async {
            let directory = tokio::task::spawn_blocking(tempfile::tempdir).await??;
            let path = directory.path().join("lfs");
            let mut file = tokio::fs::File::create(&path).await?;
            let mut stream = request.into_body().into_data_stream();
            let mut received = 0_u64;
            loop {
                let chunk = tokio::select! {
                    () = cancel.cancelled() => return Err(Error::Cancelled),
                    chunk = stream.next() => chunk,
                };
                let Some(chunk) = chunk else {
                    break;
                };
                let chunk = chunk?;
                received = received
                    .checked_add(chunk.len() as u64)
                    .filter(|size| *size <= pointer.size)
                    .ok_or(Error::Request("LFS upload exceeds declared size"))?;
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            drop(file);
            if received != pointer.size {
                return Err(Error::Request("LFS upload does not match declared size"));
            }
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let entry = repository(&worker_server, &principal, &owner, &name, true)?;
            ensure_active(&entry).await?;
            let lfs = LfsObjectStore::new(entry.store.clone(), &entry.config.prefix);
            // Once multipart publication starts, drain it through completion/abort.
            // Dropping this future on disconnect could strand uploaded parts.
            lfs.put_stream_with_size(&pointer.oid, Some(pointer.size), &path)
                .await?;
            Ok::<_, Error>(())
        };
        tokio::pin!(work);
        let completed = tokio::select! {
            result = &mut work => result,
            () = tokio::time::sleep(BUDGET) => { cancel.cancel(); work.await },
        };
        if let Err(Err(error)) = send.send(completed) {
            tracing::error!(error = ?error, "disconnected LFS upload failed");
        }
    });
    result.await.map_err(|_| Error::Cancelled)??;
    Ok(StatusCode::OK.into_response())
}
