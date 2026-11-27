//! Read-only pull mirrors whose branches and tags follow an upstream Git host.
//!
//! A sync lists the upstream's branches and tags, fetches only the objects the
//! mirror lacks into bounded local staging, and publishes every changed ref
//! through the canonical receive validation, visibility, and LFS proofs. One
//! internal lease per mirror serializes syncs across replicas with stopping.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use crab_git::receive_plan::RefUpdate;
use futures_util::StreamExt;
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ImportError, native, remote::Remote};
use crate::{
    app, auth::Principal, git::receive::ReceiveError, repository::Repository, server::Server,
};

/// Minimum time between unattended syncs of one mirror.
const INTERVAL: Duration = Duration::from_secs(15 * 60);
const POLL: Duration = Duration::from_secs(60);
const LEASE_TTL: Duration = Duration::from_secs(300);
const SYNC_RESOURCE: &str = "git-mirror-sync";
const MAX_PACK_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const INITIAL_RESERVATION: u64 = 64 * 1024 * 1024;

/// Latest sync of one mirror, shared by every replica.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Status {
    pub(crate) checked_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) synced_at_ms: Option<u64>,
    /// Refs the last successful sync changed.
    #[serde(default)]
    pub(crate) updated_refs: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// Sync `repository` from its upstream and record the outcome.
///
/// An unattended sync skips a mirror another replica checked within
/// [`INTERVAL`]. A failed sync leaves every ref where it was.
pub(crate) async fn sync(
    server: &Arc<Server>,
    repository: &Arc<Repository>,
    unattended: bool,
) -> Result<Status, ImportError> {
    let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
    let source = repository.mirror.clone().ok_or(ImportError::NotFound)?;
    // The allowlist bounds where the server fetches from, for every sync.
    if !server
        .git_import
        .as_ref()
        .is_some_and(|context| context.allows(&source))
    {
        return Err(ImportError::Invalid(
            "The mirror's upstream host is no longer in the server import allowlist",
        ));
    }
    let root = catalog.root().clone();
    let path = status_path(&root, repository.id);
    // Unattended checks read before leasing, so an idle mirror costs one read.
    if unattended
        && let Some((status, _)) = read_status(&root, &path).await?
        && recent(&status)?
    {
        return Ok(status);
    }
    let cancel = server.cancellation.child_token();
    crab_remote::publication::with_internal_lease(
        &repository.store,
        &repository.layout,
        SYNC_RESOURCE,
        LEASE_TTL,
        &cancel,
        |cancel| async move {
            let (previous, etag) = read_status(&root, &path).await?.unzip();
            let now = now_ms()?;
            if unattended
                && let Some(status) = previous.as_ref()
                && recent(status)?
            {
                return Ok(status.clone());
            }
            // Stopping takes this lease; a sync that waited behind it must see
            // the change instead of its in-memory route.
            let (document, _) = catalog.load().await.map_err(ImportError::Catalog)?;
            if document
                .repository_by_id(repository.id)
                .and_then(|record| record.mirror.as_ref())
                .is_none_or(|mirror| mirror.source != source)
            {
                return Err(ImportError::NotFound);
            }
            let outcome = pull(server, repository, &source, &cancel).await;
            if let Err(error) = &outcome {
                tracing::warn!(repository = %repository.id, error = ?error, "Git mirror sync failed");
            }
            let status = Status {
                checked_at_ms: now,
                synced_at_ms: match outcome {
                    Ok(_) => Some(now),
                    Err(_) => previous.as_ref().and_then(|status| status.synced_at_ms),
                },
                updated_refs: match &outcome {
                    Ok(updated) => *updated,
                    Err(_) => previous.as_ref().map_or(0, |status| status.updated_refs),
                },
                error: outcome.as_ref().err().map(summary),
            };
            // The lease orders writers; the condition keeps a lost lease honest.
            let body = Bytes::from(serde_json::to_vec(&status)?);
            match etag {
                Some(etag) => root.store.update(&path, body, etag).await.map(drop)?,
                None => root.store.create_strict_with_etag(&path, body).await.map(drop)?,
            }
            outcome.map(|_| status)
        },
    )
    .await
}

/// Whether another sync checked this mirror within [`INTERVAL`].
fn recent(status: &Status) -> Result<bool, ImportError> {
    Ok(now_ms()?.saturating_sub(status.checked_at_ms) < INTERVAL.as_millis() as u64)
}

/// Stop following the upstream; the repository becomes an ordinary one.
pub(crate) async fn stop(
    server: &Arc<Server>,
    repository: &Arc<Repository>,
) -> Result<(), ImportError> {
    let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
    let root = catalog.root().clone();
    let cancel = server.cancellation.child_token();
    let published = crab_remote::publication::with_internal_lease(
        &repository.store,
        &repository.layout,
        SYNC_RESOURCE,
        LEASE_TTL,
        &cancel,
        |_| async move {
            catalog
                .stop_mirror(repository.id)
                .await
                .map_err(ImportError::Catalog)
        },
    )
    .await?;
    if let Err(error) = root.store.delete(&status_path(&root, repository.id)).await {
        tracing::warn!(error = %error, "stopped mirror kept its sync status");
    }
    server
        .install_catalog(published)
        .await
        .map_err(ImportError::Setup)
}

/// Sync every mirror this replica serves, at most once per [`INTERVAL`] each.
pub(crate) async fn run(server: Arc<Server>) -> crate::Result<()> {
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(POLL) => {}
        }
        let mirrors = server
            .repositories
            .values()
            .into_iter()
            .filter(|repository| repository.mirror.is_some())
            .collect::<Vec<_>>();
        for repository in mirrors {
            if server.cancellation.is_cancelled() {
                return Ok(());
            }
            // A sync that ran recorded its outcome in the mirror's status.
            if let Err(error) = sync(&server, &repository, true).await {
                tracing::debug!(repository = %repository.id, error = ?error, "Git mirror sync skipped");
            }
        }
    }
}

async fn pull(
    server: &Arc<Server>,
    repository: &Arc<Repository>,
    source: &str,
    cancel: &CancellationToken,
) -> Result<usize, ImportError> {
    let remote = Remote::new(source, None)?;
    let upstream = native::discover(&remote).await?;
    let view = repository
        .open_ref_view()
        .await
        .map_err(ImportError::Setup)?;
    let local = view
        .refs()
        .iter()
        .filter(|(name, _)| name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
        .map(|(name, oid)| Ok((name.clone(), object_id(oid)?)))
        .collect::<Result<BTreeMap<_, _>, ImportError>>()?;
    let updates = edits(&local, &upstream.refs)?;
    let head = format!("refs/heads/{}", upstream.head);
    if updates.is_empty() && view.head() == head {
        return Ok(0);
    }
    let haves = local.values().copied().collect::<BTreeSet<_>>();
    let wants = updates
        .iter()
        .filter_map(|update| update.new)
        .filter(|oid| !haves.contains(oid))
        .collect::<BTreeSet<_>>();
    let pack = if wants.is_empty() {
        None
    } else {
        let wants = wants.into_iter().collect::<Vec<_>>();
        let haves = haves.into_iter().collect::<Vec<_>>();
        Some(fetch(server, &remote, &wants, &haves, cancel).await?)
    };
    let changed = updates.len();
    crate::git::receive::publish_mirror(
        Arc::clone(server),
        Arc::clone(repository),
        updates,
        pack,
        head,
    )
    .await
    .map_err(|error| ImportError::Publication(Box::new(error)))?;
    Ok(changed)
}

/// Edits that make the mirror's branches and tags equal the upstream's.
fn edits(
    local: &BTreeMap<String, ObjectId>,
    upstream: &BTreeMap<String, String>,
) -> Result<Vec<RefUpdate>, ImportError> {
    let mut updates = Vec::new();
    for (name, oid) in upstream {
        let new = object_id(oid)?;
        let old = local.get(name).copied();
        if old != Some(new) {
            updates.push(RefUpdate {
                name: name.clone(),
                old,
                new: Some(new),
            });
        }
    }
    updates.extend(
        local
            .iter()
            .filter(|(name, _)| !upstream.contains_key(*name))
            .map(|(name, oid)| RefUpdate {
                name: name.clone(),
                old: Some(*oid),
                new: None,
            }),
    );
    Ok(updates)
}

/// Fetch objects reachable from `wants` but not `haves` into local staging.
async fn fetch(
    server: &Server,
    remote: &Remote,
    wants: &[ObjectId],
    haves: &[ObjectId],
    cancel: &CancellationToken,
) -> Result<(crate::local_disk::StagingDirectory, std::path::PathBuf), ImportError> {
    let directory = server
        .local_staging
        .create(INITIAL_RESERVATION, cancel)
        .await
        .map_err(ImportError::Staging)?;
    let path = directory.path().join("mirror.pack");
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(ImportError::Io)?;
    let mut response = std::pin::pin!(remote.fetch(wants, haves).await?);
    let mut decoder = crab_git::fetch_wire::FetchResponse::new();
    let (mut size, mut reserved) = (0_u64, INITIAL_RESERVATION);
    loop {
        let chunk = tokio::select! {
            () = cancel.cancelled() => return Err(ImportError::Cancelled),
            chunk = response.next() => chunk,
        };
        let Some(chunk) = chunk else { break };
        let mut pack = Vec::new();
        decoder
            .feed(&chunk?, &mut |bytes: &[u8]| {
                pack.extend_from_slice(bytes);
                Ok::<_, std::convert::Infallible>(())
            })
            .map_err(|error| ImportError::Git(error.to_string()))?;
        size = size
            .checked_add(pack.len() as u64)
            .filter(|size| *size <= MAX_PACK_BYTES)
            .ok_or_else(|| {
                ImportError::Git("The upstream changes exceed the mirror sync limit".into())
            })?;
        if size > reserved {
            reserved = size
                .saturating_add(crate::local_disk::RESERVATION_GROWTH)
                .min(MAX_PACK_BYTES);
            server
                .local_staging
                .resize(&directory, reserved)
                .map_err(ImportError::Staging)?;
        }
        file.write_all(&pack).await.map_err(ImportError::Io)?;
    }
    crab_git::fetch_wire::FetchResponse::finish(decoder)
        .map_err(|error| ImportError::Git(error.to_string()))?;
    file.flush().await.map_err(ImportError::Io)?;
    drop(file);
    server
        .local_staging
        .resize(&directory, size)
        .map_err(ImportError::Staging)?;
    Ok((directory, path))
}

/// A user-facing reason that never contains credentials or internal detail.
fn summary(error: &ImportError) -> String {
    match error {
        ImportError::Git(message) => message.clone(),
        ImportError::Invalid(message) => (*message).into(),
        ImportError::Pack(_) | ImportError::History(_) => {
            "The upstream sent history that Crab cannot verify".into()
        }
        ImportError::Cancelled | ImportError::Timeout => "The sync did not finish".into(),
        ImportError::Publication(source) => match source.downcast_ref::<ReceiveError>() {
            Some(ReceiveError::RefRule(message)) => (*message).into(),
            Some(ReceiveError::Archived) => "Archived repositories do not sync".into(),
            Some(ReceiveError::MirrorSource(error)) => summary(error),
            _ => "The upstream changes could not be published; inspect server logs".into(),
        },
        _ => "The mirror could not sync; inspect server logs".into(),
    }
}

fn status_path(
    root: &crate::storage_root::StorageRoot,
    repository: Uuid,
) -> object_store::path::Path {
    root.path(&format!(
        ".crab/http-server/v1/git-mirrors/{repository}.json"
    ))
}

async fn read_status(
    root: &crate::storage_root::StorageRoot,
    path: &object_store::path::Path,
) -> Result<Option<(Status, crab_storage::ETag)>, ImportError> {
    match root.store.get_with_etag_bounded(path, 64 * 1024).await {
        Ok((bytes, etag)) => Ok(Some((serde_json::from_slice(&bytes)?, etag))),
        Err(crab_storage::StorageError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn object_id(hex: &str) -> Result<ObjectId, ImportError> {
    ObjectId::from_hex(hex.as_bytes())
        .map_err(|_| ImportError::Git("The mirror holds an invalid object ID".into()))
}

fn now_ms() -> Result<u64, ImportError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .map_err(|_| ImportError::Unavailable)
}

/// Why a mirror request cannot proceed.
enum Denied {
    NotFound,
    Forbidden,
    Membership(Box<app::Error>),
}

impl IntoResponse for Denied {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Membership(error) => return error.into_response(),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "Mirror not found"),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "You cannot manage this mirror",
            ),
        };
        (
            status,
            Json(json!({"error":{"code":code,"message":message}})),
        )
            .into_response()
    }
}

/// Resolve a mirror the principal may use as `allowed` permits.
async fn mirror_for(
    server: &Arc<Server>,
    principal: &Principal,
    repository: Uuid,
    allowed: fn(&Principal, Uuid) -> bool,
) -> Result<Arc<Repository>, Denied> {
    let active = server
        .repositories
        .by_id(repository)
        .ok_or(Denied::NotFound)?;
    let current = app::membership(server, principal, &active)
        .await
        .map_err(|error| Denied::Membership(Box::new(error)))?;
    if !current.principal.can_read(repository) || active.mirror.is_none() {
        return Err(Denied::NotFound);
    }
    if !allowed(&current.principal, repository) {
        return Err(Denied::Forbidden);
    }
    Ok(active)
}

pub(super) async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Response {
    let active = match mirror_for(&server, &principal, repository, Principal::can_read).await {
        Ok(active) => active,
        Err(denied) => return denied.into_response(),
    };
    let Some(catalog) = server.catalog() else {
        return ImportError::Unavailable.into_response();
    };
    match read_status(catalog.root(), &status_path(catalog.root(), repository)).await {
        Ok(status) => {
            let status = status.map(|(status, _)| status);
            Json(json!({"source": active.mirror, "status": status})).into_response()
        }
        Err(error) => error.into_response(),
    }
}

pub(super) async fn request_sync(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Response {
    let active = match mirror_for(&server, &principal, repository, Principal::can_write).await {
        Ok(active) => active,
        Err(denied) => return denied.into_response(),
    };
    // A client that disconnects must not cancel a sync between its publication
    // and its recorded status.
    let worker = Arc::clone(&server);
    let mirror = Arc::clone(&active);
    let outcome = tokio::spawn(async move { sync(&worker, &mirror, false).await })
        .await
        .unwrap_or_else(|error| Err(ImportError::Io(std::io::Error::other(error))));
    match outcome {
        Ok(status) => Json(json!({"source": active.mirror, "status": status})).into_response(),
        Err(ImportError::NotFound) => Denied::NotFound.into_response(),
        Err(error) => {
            tracing::warn!(%repository, error = ?error, "Git mirror sync request failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error":{"code":"mirror_sync_failed","message":summary(&error)}})),
            )
                .into_response()
        }
    }
}

pub(super) async fn request_stop(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Response {
    let active = match mirror_for(&server, &principal, repository, Principal::can_admin).await {
        Ok(active) => active,
        Err(denied) => return denied.into_response(),
    };
    match stop(&server, &active).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_create_move_and_delete_until_refs_match_the_upstream() {
        let oid = |byte: u8| ObjectId::from_bytes_or_panic(&[byte; 20]);
        let local = BTreeMap::from([
            ("refs/heads/main".to_owned(), oid(1)),
            ("refs/heads/gone".to_owned(), oid(2)),
            ("refs/tags/v1".to_owned(), oid(3)),
        ]);
        let upstream = BTreeMap::from([
            ("refs/heads/main".to_owned(), oid(4).to_string()),
            ("refs/heads/new".to_owned(), oid(1).to_string()),
            ("refs/tags/v1".to_owned(), oid(3).to_string()),
        ]);
        let updates = edits(&local, &upstream).unwrap();
        let summary = updates
            .iter()
            .map(|update| (update.name.as_str(), update.old, update.new))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("refs/heads/main", Some(oid(1)), Some(oid(4))),
                ("refs/heads/new", None, Some(oid(1))),
                ("refs/heads/gone", Some(oid(2)), None),
            ]
        );
    }
}
