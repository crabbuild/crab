//! Read-only pull mirrors whose branches and tags follow an upstream Git host.
//!
//! A sync lists the upstream's branches and tags, stages only the objects the
//! mirror lacks in object storage, and publishes every changed ref through the
//! diskless import path with native receive validation. One internal lease
//! per mirror serializes syncs across replicas with credential changes and
//! stopping. A private upstream's token is sealed under a key every serving
//! replica derives from `auth.state_key_file`.

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
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use crab_git::receive_plan::RefUpdate;
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ImportError, native, remote::Remote, workflow::ImportRules};
use crate::{
    app, auth::Principal, repository::Repository, server::Server, storage_root::StorageRoot,
};

/// Minimum time between unattended syncs of one mirror.
const INTERVAL: Duration = Duration::from_secs(15 * 60);
const POLL: Duration = Duration::from_secs(60);
const LEASE_TTL: Duration = Duration::from_secs(300);
const SYNC_RESOURCE: &str = "git-mirror-sync";
const CREDENTIAL_CONTEXT: &[u8] = b"crab git mirror credential v1\0";

/// Latest sync of one mirror, shared by every replica.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Status {
    pub(super) checked_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) synced_at_ms: Option<u64>,
    /// Refs the last successful sync changed.
    #[serde(default)]
    pub(super) updated_refs: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// An upstream token sealed to one mirror and source.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SealedCredential {
    version: u8,
    nonce: String,
    ciphertext: String,
}

/// Whether this server can keep a mirror's upstream token.
pub(super) fn seals_credentials(server: &Server) -> bool {
    credential_key(server).is_some()
}

/// Seal and keep the token a mirror sends its upstream.
pub(super) async fn store_credential(
    server: &Server,
    repository: Uuid,
    source: &str,
    token: &str,
) -> Result<(), ImportError> {
    let key = credential_key(server).ok_or(ImportError::Unavailable)?;
    let root = server
        .catalog()
        .ok_or(ImportError::Unavailable)?
        .root()
        .clone();
    let sealed = seal(&key, repository, source, token)?;
    root.store
        .put_overwrite(
            &credential_path(&root, repository),
            Bytes::from(serde_json::to_vec(&sealed)?),
        )
        .await?;
    Ok(())
}

/// Sync `repository` from its upstream and record the outcome.
///
/// An unattended sync skips a mirror another replica checked within
/// [`INTERVAL`]. A failed sync leaves every ref where it was.
async fn sync(
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
            let token = load_credential(server, &root, repository.id, &source).await?;
            let outcome = pull(server, repository, &source, token.as_deref(), &cancel).await;
            if let Err(error) = &outcome {
                tracing::warn!(repository = %repository.id, error = ?error, "Git mirror sync failed");
            }
            let now = now_ms()?;
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
                None => root
                    .store
                    .create_strict_with_etag(&path, body)
                    .await
                    .map(drop)?,
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

/// Run `work` while no sync or credential change of this mirror can run.
async fn exclusive<T>(
    server: &Server,
    repository: &Repository,
    work: impl AsyncFnOnce() -> Result<T, ImportError>,
) -> Result<T, ImportError> {
    let cancel = server.cancellation.child_token();
    crab_remote::publication::with_internal_lease(
        &repository.store,
        &repository.layout,
        SYNC_RESOURCE,
        LEASE_TTL,
        &cancel,
        |_| work(),
    )
    .await
}

/// Stop following the upstream; the repository becomes an ordinary one.
async fn stop(server: &Arc<Server>, repository: &Arc<Repository>) -> Result<(), ImportError> {
    let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
    let root = catalog.root().clone();
    let published = exclusive(server, repository, async || {
        let published = catalog
            .stop_mirror(repository.id)
            .await
            .map_err(ImportError::Catalog)?;
        // The token is only for this upstream; it must not outlive the mirror.
        root.store
            .delete(&credential_path(&root, repository.id))
            .await?;
        Ok(published)
    })
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
    token: Option<&str>,
    cancel: &CancellationToken,
) -> Result<usize, ImportError> {
    let remote = Remote::new(source, token)?;
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
    let changed = updates.len();
    if !updates.is_empty() {
        let haves = local.values().copied().collect::<BTreeSet<_>>();
        let wants = updates
            .iter()
            .filter_map(|update| update.new)
            .filter(|oid| !haves.contains(oid))
            .collect::<Vec<_>>();
        let root = server
            .catalog()
            .ok_or(ImportError::Unavailable)?
            .root()
            .clone();
        let path = mirror_path(&root, repository.id, "sync.pack");
        // The upstream sends what our tips cannot reach; everything else is ours.
        let staged = if wants.is_empty() {
            None
        } else {
            let haves = haves.into_iter().collect::<Vec<_>>();
            Some(native::stage(&root.store, &path, &remote, &wants, &haves, cancel).await?)
        };
        let rules = ImportRules::unattended(server, repository).await?;
        let published = native::publish_updates(
            server,
            repository,
            staged.as_ref().map(|staged| (staged, &path)),
            &upstream,
            updates,
            &rules,
            native::LfsSource { url: source, token },
            cancel,
        )
        .await;
        if staged.is_some()
            && let Err(error) = root.store.delete(&path).await
        {
            tracing::warn!(error = %error, "mirror sync kept its staged pack");
        }
        published?;
    }
    native::follow_head(server, repository, &head, cancel).await?;
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

fn credential_key(server: &Server) -> Option<[u8; 32]> {
    server.auth.as_ref()?.mirror_key().copied()
}

/// Binds a sealed token to its mirror and upstream so it cannot be replayed elsewhere.
fn associated_data(repository: Uuid, source: &str) -> Vec<u8> {
    let mut data = CREDENTIAL_CONTEXT.to_vec();
    data.extend_from_slice(repository.as_bytes());
    data.extend_from_slice(source.as_bytes());
    data
}

fn seal(
    key: &[u8; 32],
    repository: Uuid,
    source: &str,
    token: &str,
) -> Result<SealedCredential, ImportError> {
    let nonce: [u8; 24] = rand::random();
    let ciphertext = XChaCha20Poly1305::new(key.into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: token.as_bytes(),
                aad: &associated_data(repository, source),
            },
        )
        .map_err(|_| ImportError::Unavailable)?;
    Ok(SealedCredential {
        version: 1,
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    })
}

fn open(
    key: &[u8; 32],
    repository: Uuid,
    source: &str,
    sealed: &SealedCredential,
) -> Result<String, ImportError> {
    let unreadable =
        || ImportError::Invalid("The mirror's stored token cannot be read; set the token again");
    let nonce = STANDARD.decode(&sealed.nonce).map_err(|_| unreadable())?;
    let ciphertext = STANDARD
        .decode(&sealed.ciphertext)
        .map_err(|_| unreadable())?;
    if sealed.version != 1 || nonce.len() != 24 {
        return Err(unreadable());
    }
    let token = XChaCha20Poly1305::new(key.into())
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: &associated_data(repository, source),
            },
        )
        .map_err(|_| unreadable())?;
    String::from_utf8(token).map_err(|_| unreadable())
}

async fn load_credential(
    server: &Server,
    root: &StorageRoot,
    repository: Uuid,
    source: &str,
) -> Result<Option<String>, ImportError> {
    let sealed = match root
        .store
        .get_with_etag_bounded(&credential_path(root, repository), 64 * 1024)
        .await
    {
        Ok((bytes, _)) => serde_json::from_slice::<SealedCredential>(&bytes)?,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let key = credential_key(server).ok_or(ImportError::Invalid(
        "The mirror's stored token needs auth.state_key_file to unseal",
    ))?;
    open(&key, repository, source, &sealed).map(Some)
}

/// A user-facing reason that never contains credentials or internal detail.
fn summary(error: &ImportError) -> String {
    match error {
        ImportError::Git(message) => message.clone(),
        ImportError::Invalid(message) => (*message).into(),
        ImportError::Ruleset => "Organization rules restrict the mirrored refs".into(),
        ImportError::Pack(_) | ImportError::History(_) | ImportError::Validation(_) => {
            "The upstream sent history that Crab cannot verify".into()
        }
        ImportError::DestinationChanged => "The mirror's refs changed during the sync".into(),
        ImportError::Cancelled | ImportError::Timeout => "The sync did not finish".into(),
        _ => "The mirror could not sync; inspect server logs".into(),
    }
}

fn mirror_path(root: &StorageRoot, repository: Uuid, name: &str) -> object_store::path::Path {
    root.path(&format!(
        ".crab/http-server/v1/git-mirrors/{repository}/{name}"
    ))
}

fn status_path(root: &StorageRoot, repository: Uuid) -> object_store::path::Path {
    mirror_path(root, repository, "status.json")
}

fn credential_path(root: &StorageRoot, repository: Uuid) -> object_store::path::Path {
    mirror_path(root, repository, "credential.json")
}

async fn read_status(
    root: &StorageRoot,
    path: &object_store::path::Path,
) -> Result<Option<(Status, crab_storage::ETag)>, ImportError> {
    match root.store.get_with_etag_bounded(path, 64 * 1024).await {
        Ok((bytes, etag)) => Ok(Some((serde_json::from_slice(&bytes)?, etag))),
        Err(crab_storage::StorageError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn authenticated(root: &StorageRoot, repository: Uuid) -> Result<bool, ImportError> {
    match root.store.head(&credential_path(root, repository)).await {
        Ok(_) => Ok(true),
        Err(crab_storage::StorageError::NotFound { .. }) => Ok(false),
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

/// The mirror's upstream, token presence, and latest sync status.
async fn described(
    server: &Server,
    active: &Repository,
) -> Result<Json<serde_json::Value>, ImportError> {
    let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
    let root = catalog.root();
    let status = read_status(root, &status_path(root, active.id))
        .await?
        .map(|(status, _)| status);
    Ok(Json(json!({
        "source": active.mirror,
        "authenticated": authenticated(root, active.id).await?,
        "status": status,
    })))
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
    match described(&server, &active).await {
        Ok(described) => described.into_response(),
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
    // A sync can outlast any request deadline. It records its outcome in the
    // mirror's status, and shutdown drains it like a receive worker.
    let worker = Arc::clone(&server);
    let mirror = Arc::clone(&active);
    server.receives.spawn(async move {
        if let Err(error) = sync(&worker, &mirror, false).await {
            tracing::warn!(repository = %mirror.id, error = ?error, "Git mirror sync request failed");
        }
    });
    match described(&server, &active).await {
        Ok(described) => (StatusCode::ACCEPTED, described).into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CredentialRequest {
    token: String,
}

pub(super) async fn set_credential(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
    Json(request): Json<CredentialRequest>,
) -> Response {
    let active = match mirror_for(&server, &principal, repository, Principal::can_admin).await {
        Ok(active) => active,
        Err(denied) => return denied.into_response(),
    };
    let Some(source) = active.mirror.clone() else {
        return Denied::NotFound.into_response();
    };
    let result = async {
        let token = super::source::token(Some(request.token), &source)?
            .ok_or(ImportError::Invalid("Enter the upstream token"))?;
        if !seals_credentials(&server) {
            return Err(ImportError::Invalid(
                "Mirroring a private source needs auth.state_key_file to protect its token",
            ));
        }
        exclusive(&server, &active, async || {
            store_credential(&server, active.id, &source, &token).await
        })
        .await
    }
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

pub(super) async fn remove_credential(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Response {
    let active = match mirror_for(&server, &principal, repository, Principal::can_admin).await {
        Ok(active) => active,
        Err(denied) => return denied.into_response(),
    };
    let Some(catalog) = server.catalog() else {
        return ImportError::Unavailable.into_response();
    };
    let root = catalog.root().clone();
    let path = credential_path(&root, repository);
    match exclusive(&server, &active, async || {
        Ok(root.store.delete(&path).await?)
    })
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
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

    #[test]
    fn sealed_tokens_open_only_for_their_mirror_and_source() {
        let key = [9; 32];
        let mirror = Uuid::now_v7();
        let source = "https://git.example.com/team/project.git";
        let sealed = seal(&key, mirror, source, "fixture-token").unwrap();
        assert!(!sealed.ciphertext.contains("fixture-token"));
        assert_eq!(
            open(&key, mirror, source, &sealed).unwrap(),
            "fixture-token"
        );
        assert!(open(&key, Uuid::now_v7(), source, &sealed).is_err());
        assert!(open(&key, mirror, "https://git.example.com/other.git", &sealed).is_err());
        assert!(open(&[8; 32], mirror, source, &sealed).is_err());
    }
}
