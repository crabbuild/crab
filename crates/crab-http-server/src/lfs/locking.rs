use std::{future::Future, sync::Arc, time::Duration};

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use crab_coordination::LFS_LOCKS_RESOURCE;
use crab_lfs::{LfsLockError, LfsLockManager, LockRecord};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio_util::sync::CancellationToken;

use super::{CONTENT_TYPE, Error, Result, ensure_active, repository};
use crate::{
    auth::Principal,
    server::{Repository, Server},
};

const BUDGET: Duration = Duration::from_secs(30);
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_TOKEN_BYTES: usize = 128;
const DEFAULT_LIMIT: usize = 100;

#[derive(Deserialize)]
pub(crate) struct CreateLock {
    path: String,
}

#[derive(Deserialize)]
struct ListLocks {
    path: Option<String>,
    id: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
    refspec: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct VerifyLocks {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
pub(crate) struct UnlockLock {
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
struct LockOwner {
    name: String,
}

#[derive(Serialize)]
struct ApiLock {
    id: String,
    path: String,
    locked_at: String,
    owner: LockOwner,
}

#[derive(Serialize)]
struct LocksResponse {
    locks: Vec<ApiLock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct LockResponse {
    lock: ApiLock,
}

#[derive(Serialize)]
struct VerifyResponse {
    ours: Vec<ApiLock>,
    theirs: Vec<ApiLock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(Error::Request(
            "LFS lock path must contain 1–4096 UTF-8 bytes",
        ));
    }
    crab_remote_git::GitPath::new(bytes::Bytes::copy_from_slice(path.as_bytes()))
        .map_err(|_| Error::Request("LFS lock path must be repository-relative"))?;
    Ok(())
}

fn validate_token(value: Option<&str>) -> Result<()> {
    if value.is_some_and(|value| {
        value.is_empty() || value.len() > MAX_TOKEN_BYTES || value.chars().any(char::is_control)
    }) {
        return Err(Error::Request(
            "LFS lock ID and cursor must contain 1–128 bytes without controls",
        ));
    }
    Ok(())
}

fn page_limit(limit: Option<usize>) -> Result<usize> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=DEFAULT_LIMIT).contains(&limit) {
        return Err(Error::Request("LFS lock limit must be between 1 and 100"));
    }
    Ok(limit)
}

fn api_lock(repository: &Repository, record: LockRecord) -> Result<ApiLock> {
    let owner = repository
        .config
        .members
        .iter()
        .find(|member| member.subject == record.owner)
        .map(|member| member.name.clone())
        .unwrap_or_else(|| record.owner.clone());
    let seconds = i64::try_from(record.locked_at).map_err(|_| Error::LockTimestamp)?;
    let locked_at = OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|_| Error::LockTimestamp)?
        .format(&Rfc3339)
        .map_err(|_| Error::LockTimestamp)?;
    Ok(ApiLock {
        id: record.id,
        path: record.path,
        locked_at,
        owner: LockOwner { name: owner },
    })
}

async fn operation<T>(
    server: &Server,
    operation: impl Future<Output = std::result::Result<T, LfsLockError>>,
) -> Result<T> {
    let _permit = server.admission.try_acquire().map_err(|_| Error::Busy)?;
    tokio::select! {
        () = server.cancellation.cancelled() => Err(Error::Cancelled),
        result = tokio::time::timeout(BUDGET, operation) => {
            Ok(result.map_err(|_| Error::Cancelled)??)
        }
    }
}

async fn mutation<T, F, Fut>(server: &Server, repository: &Repository, action: F) -> Result<T>
where
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let _permit = server.admission.try_acquire().map_err(|_| Error::Busy)?;
    let cancel = server.cancellation.child_token();
    // Receive holds this same lease through its lock check and ref commit. A lock
    // mutation therefore lands wholly before or after an authoritative push.
    let work = crab_remote::publication::with_internal_lease(
        &repository.store,
        &repository.layout,
        LFS_LOCKS_RESOURCE,
        BUDGET,
        &cancel,
        |lease_cancel| async move {
            tokio::select! {
                () = lease_cancel.cancelled() => Err(Error::Cancelled),
                result = action(lease_cancel.clone()) => result,
            }
        },
    );
    tokio::pin!(work);
    let timeout = tokio::time::sleep(BUDGET);
    tokio::pin!(timeout);
    tokio::select! {
        result = &mut work => result,
        () = server.cancellation.cancelled() => {
            cancel.cancel();
            work.await
        }
        () = &mut timeout => {
            cancel.cancel();
            work.await
        }
    }
}

async fn page(
    server: &Server,
    manager: &LfsLockManager,
    path: Option<&str>,
    id: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<(Vec<LockRecord>, Option<String>)> {
    let mut records = operation(
        server,
        manager.list_page(path, id, cursor, limit.saturating_add(1)),
    )
    .await?;
    let next_cursor = (records.len() > limit)
        .then(|| records.get(limit - 1).map(|record| record.id.clone()))
        .flatten();
    records.truncate(limit);
    Ok((records, next_cursor))
}

pub(crate) async fn create_lock(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<CreateLock>,
) -> Result<Response> {
    validate_path(&input.path)?;
    let entry = repository(&server, &principal, &owner, &name, true)?;
    ensure_active(&entry).await?;
    let identity = principal.identity().ok_or(Error::Forbidden)?;
    let manager = LfsLockManager::lfs(entry.store.clone(), &entry.config.prefix);
    let path = input.path;
    let subject = identity.subject;
    let result = mutation(&server, &entry, move |_cancel| async move {
        match manager.lock(&path, &subject).await {
            Ok(record) => Ok(Ok(record)),
            Err(LfsLockError::Conflict { .. }) => match manager.find_by_path(&path).await {
                Ok(record) => Ok(Err(record)),
                Err(LfsLockError::NotFound { .. }) => Err(Error::LockConflict),
                Err(error) => Err(error.into()),
            },
            Err(error) => Err(error.into()),
        }
    })
    .await?;
    let record = match result {
        Ok(record) => record,
        Err(existing) => {
            return Ok((
                StatusCode::CONFLICT,
                [(header::CONTENT_TYPE, CONTENT_TYPE)],
                Json(json!({
                    "lock": api_lock(&entry, existing)?,
                    "message": "LFS path is already locked",
                })),
            )
                .into_response());
        }
    };
    Ok((
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, CONTENT_TYPE)],
        Json(LockResponse {
            lock: api_lock(&entry, record)?,
        }),
    )
        .into_response())
}

pub(crate) async fn list_locks(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    uri: Uri,
) -> Result<Response> {
    let Query(query) = Query::<ListLocks>::try_from_uri(&uri)?;
    let path = query.path.as_deref().filter(|value| !value.is_empty());
    let id = query.id.as_deref().filter(|value| !value.is_empty());
    let cursor = query.cursor.as_deref().filter(|value| !value.is_empty());
    if let Some(path) = path {
        validate_path(path)?;
    }
    validate_token(id)?;
    validate_token(cursor)?;
    if query
        .refspec
        .as_deref()
        .is_some_and(|value| value.len() > 1024 || value.chars().any(char::is_control))
    {
        return Err(Error::Request(
            "LFS lock refspec must be at most 1024 bytes without controls",
        ));
    }
    let limit = page_limit(query.limit)?;
    let entry = repository(&server, &principal, &owner, &name, false)?;
    let manager = LfsLockManager::lfs(entry.store.clone(), &entry.config.prefix);
    let (records, next_cursor) = page(&server, &manager, path, id, cursor, limit).await?;
    let locks = records
        .into_iter()
        .map(|record| api_lock(&entry, record))
        .collect::<Result<Vec<_>>>()?;
    Ok((
        [(header::CONTENT_TYPE, CONTENT_TYPE)],
        Json(LocksResponse { locks, next_cursor }),
    )
        .into_response())
}

pub(crate) async fn verify_locks(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<VerifyLocks>,
) -> Result<Response> {
    let cursor = input.cursor.as_deref().filter(|value| !value.is_empty());
    validate_token(cursor)?;
    let limit = page_limit(input.limit)?;
    let entry = repository(&server, &principal, &owner, &name, true)?;
    let identity = principal.identity().ok_or(Error::Forbidden)?;
    let manager = LfsLockManager::lfs(entry.store.clone(), &entry.config.prefix);
    let (records, next_cursor) = page(&server, &manager, None, None, cursor, limit).await?;
    let mut ours = Vec::new();
    let mut theirs = Vec::new();
    for record in records {
        let mine = record.owner == identity.subject;
        let record = api_lock(&entry, record)?;
        if mine {
            ours.push(record);
        } else {
            theirs.push(record);
        }
    }
    Ok((
        [(header::CONTENT_TYPE, CONTENT_TYPE)],
        Json(VerifyResponse {
            ours,
            theirs,
            next_cursor,
        }),
    )
        .into_response())
}

pub(crate) async fn unlock_lock(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, String)>,
    Json(input): Json<UnlockLock>,
) -> Result<Response> {
    validate_token(Some(&id))?;
    let entry = repository(&server, &principal, &owner, &name, true)?;
    let identity = principal.identity().ok_or(Error::Forbidden)?;
    let manager = LfsLockManager::lfs(entry.store.clone(), &entry.config.prefix);
    let subject = identity.subject;
    let force = input.force;
    let record = mutation(&server, &entry, move |_cancel| async move {
        let current = manager.find_by_id_including_released(&id).await?;
        if !force && current.owner != subject {
            return Err(Error::LockOwner);
        }
        if force {
            Ok(manager.force_unlock_with_id(&current.path, &id).await?)
        } else {
            Ok(manager
                .unlock_with_id(&current.path, &subject, Some(&id))
                .await?)
        }
    })
    .await?;
    Ok((
        [(header::CONTENT_TYPE, CONTENT_TYPE)],
        Json(LockResponse {
            lock: api_lock(&entry, record)?,
        }),
    )
        .into_response())
}
