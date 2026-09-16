use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, Query, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{
    app::{self, Error, Result},
    auth::{Identity, Principal},
    receive::{self, ReceiveError},
    server::{Repository, Server},
    statuses,
};

const MAX_NUMBER: u64 = 9_007_199_254_740_991;
const MAX_TAG_BYTES: usize = 255;
const MAX_ASSET_NAME_BYTES: usize = 255;
const ASSET_BUDGET: Duration = Duration::from_secs(5 * 60);

fn transfer_error(error: crate::transfer_admission::Error) -> Error {
    match error {
        crate::transfer_admission::Error::Busy => Error::ReleaseBusy,
        crate::transfer_admission::Error::Cancelled => Error::ReleaseAssetCancelled,
        crate::transfer_admission::Error::Coordination(error) => {
            Error::ReleaseAssetCoordination(error)
        }
    }
}

fn staging_error(error: crate::local_disk::Error) -> Error {
    match error {
        crate::local_disk::Error::TooLarge => Error::ReleaseAssetTooLarge,
        crate::local_disk::Error::Busy => Error::ReleaseBusy,
        crate::local_disk::Error::Cancelled => Error::ReleaseAssetCancelled,
        crate::local_disk::Error::Io(error) => Error::ReleaseAssetIo(error),
        crate::local_disk::Error::Worker(error) => Error::ReleaseAssetWorker(error),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Release {
    number: u64,
    request_id: String,
    author: Identity,
    tag_name: String,
    tag_oid: Option<String>,
    target_oid: String,
    title: String,
    body: String,
    prerelease: bool,
    draft: bool,
    #[serde(skip)]
    publication_pending: Option<ReleasePublication>,
    version: u64,
    created_at: u64,
    published_at: Option<u64>,
    updated_at: u64,
    deleted: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug)]
struct ReleasePublication {
    expected_version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseAsset {
    id: String,
    name: String,
    content_type: String,
    size: u64,
    digest: String,
    uploader: Identity,
    created_at: u64,
}

mod storage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRelease {
    request_id: String,
    tag_name: String,
    target_oid: String,
    title: String,
    body: String,
    prerelease: bool,
    draft: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseEdit {
    version: u64,
    title: String,
    body: String,
    prerelease: bool,
    draft: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseDelete {
    version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetUpload {
    request_id: String,
    name: String,
    version: u64,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ListParameters {
    before: Option<u64>,
    limit: Option<usize>,
    query: Option<String>,
}

impl ListParameters {
    fn limit(&self) -> Result<usize> {
        let limit = self.limit.unwrap_or(20);
        if !(1..=50).contains(&limit)
            || self
                .before
                .is_some_and(|value| value == 0 || value > MAX_NUMBER)
        {
            return Err(Error::Invalid("Release page size or cursor is invalid"));
        }
        Ok(limit)
    }

    fn query(&self) -> Result<Option<String>> {
        let query = self.query.as_deref().map(str::trim).unwrap_or_default();
        if query.chars().count() > 256 || query.chars().any(char::is_control) {
            return Err(Error::Invalid("Release search is invalid"));
        }
        Ok((!query.is_empty()).then(|| query.to_lowercase()))
    }
}

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/releases", get(list).post(create))
        .route(
            "/api/repos/{owner}/{name}/releases/{number}",
            get(detail).patch(edit).delete(remove),
        )
        .route(
            "/api/repos/{owner}/{name}/releases/{number}/assets",
            axum::routing::post(upload_asset),
        )
        .route(
            "/api/repos/{owner}/{name}/releases/{number}/assets/{asset_id}",
            get(download_asset).delete(remove_asset),
        )
        .layer(axum::extract::DefaultBodyLimit::max(80 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

fn tag_name(value: &str) -> Result<(String, String)> {
    if value.is_empty()
        || value.len() > MAX_TAG_BYTES
        || value.trim() != value
        || value.starts_with("refs/")
        || value.chars().any(char::is_control)
    {
        return Err(Error::Invalid("Enter a valid tag name"));
    }
    let reference = format!("refs/tags/{value}");
    crab_git::validate_push_refname(&reference)
        .map_err(|_| Error::Invalid("Enter a valid tag name"))?;
    Ok((value.to_owned(), reference))
}

fn asset_name(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > MAX_ASSET_NAME_BYTES
        || !value.is_ascii()
        || value.starts_with('.')
        || value.ends_with('.')
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'/' | b'\\' | b'"'))
    {
        return Err(Error::Invalid(
            "Asset names must be 1–255 ASCII characters without paths, quotes, controls, or leading/trailing periods",
        ));
    }
    Ok(value.to_owned())
}

fn asset_path(digest: &str) -> String {
    format!("release-assets/v1/sha256/{}/{digest}", &digest[..2])
}

fn asset_view(asset: &ReleaseAsset) -> Value {
    json!({
        "id": asset.id,
        "name": asset.name,
        "content_type": asset.content_type,
        "size": asset.size,
        "digest": format!("sha256:{}", asset.digest),
        "uploader": asset.uploader.name,
        "created_at": asset.created_at,
    })
}

fn view(release: &Release) -> Value {
    json!({
        "number": release.number,
        "tag_name": release.tag_name,
        "tag_oid": release.tag_oid,
        "target_oid": release.target_oid,
        "title": release.title,
        "body": release.body,
        "prerelease": release.prerelease,
        "draft": release.draft,
        "version": release.version,
        "author": release.author.name,
        "created_at": release.created_at,
        "published_at": release.published_at,
        "updated_at": release.updated_at,
        "assets": release.assets.iter().map(asset_view).collect::<Vec<_>>(),
    })
}

async fn current_tag(
    server: &Server,
    repo: &Repository,
    reference: &str,
    target: ObjectId,
) -> Result<Option<ObjectId>> {
    let cancellation = server.cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let repository = repo
        .open_current(server, server.options, &cancellation)
        .await?;
    let Some(tag) = repository.refs().find(reference) else {
        return Ok(None);
    };
    if tag.peeled.unwrap_or(tag.target) != target {
        return Err(Error::ReleaseConflict);
    }
    Ok(Some(tag.target))
}

fn publication_error(error: ReceiveError) -> Error {
    match error {
        ReceiveError::Archived => Error::Archived,
        ReceiveError::Busy => Error::ReleaseBusy,
        ReceiveError::Forbidden => Error::ReleasePermission,
        ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Stale { .. })
        | ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Namespace(_))
        | ReceiveError::Write(crab_write::WriteError::RefChanged { .. })
        | ReceiveError::Write(crab_write::WriteError::Namespace(_)) => Error::ReleaseConflict,
        error => Error::Release(Box::new(error)),
    }
}

async fn ensure_tag(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    repo: &Repository,
    reference: String,
    target: ObjectId,
) -> Result<ObjectId> {
    if let Some(oid) = current_tag(&server, repo, &reference, target).await? {
        return Ok(oid);
    }
    let published = receive::create_tag(
        Arc::clone(&server),
        principal,
        key,
        reference.clone(),
        target,
    )
    .await;
    match current_tag(&server, repo, &reference, target).await {
        Ok(Some(oid)) => Ok(oid),
        Ok(None) => Err(published
            .err()
            .map_or(Error::ReleaseConflict, publication_error)),
        Err(Error::ReleaseConflict) => Err(Error::ReleaseConflict),
        Err(error) => Err(error),
    }
}

async fn finish_publication(
    server: &Arc<Server>,
    principal: &Principal,
    key: (String, String),
    repo: &Repository,
    release: Release,
) -> Result<Release> {
    if release.draft && release.publication_pending.is_none() {
        return Ok(release);
    }
    let target =
        ObjectId::from_hex(release.target_oid.as_bytes()).map_err(|_| Error::ReleaseConflict)?;
    let (_, reference) = tag_name(&release.tag_name)?;
    let tag_oid = ensure_tag(
        Arc::clone(server),
        principal.clone(),
        key,
        repo,
        reference,
        target,
    )
    .await?
    .to_string();
    if release.publication_pending.is_none() {
        return if release.tag_oid.as_deref() == Some(&tag_oid) {
            Ok(release)
        } else {
            Err(Error::ReleaseConflict)
        };
    }
    let actor = app::actor(principal)?;
    storage::complete_publication(server, repo, &actor, &release, tag_oid).await
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<NewRelease>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    if !principal.can_write(&repo.config) {
        return Err(Error::ReleasePermission);
    }
    let Json(input) = input?;
    let request_id = app::submission(&input.request_id)?;
    let (tag_name, reference) = tag_name(&input.tag_name)?;
    let title = app::title(&input.title)?;
    app::body(&input.body, false)?;
    let target = ObjectId::from_hex(input.target_oid.as_bytes())
        .ok()
        .filter(|oid| oid.kind() == gix_hash::Kind::Sha1 && !oid.is_null())
        .ok_or(Error::Invalid(
            "Release target must be a full SHA-1 commit ID",
        ))?;
    let target_oid = statuses::require_commit(&server, repo, target).await?;
    current_tag(&server, repo, &reference, target).await?;
    let actor = app::actor(&principal)?;
    let release = match storage::create(
        &server,
        repo,
        storage::NewRelease {
            submission_id: request_id,
            author: actor,
            tag_name,
            target_oid,
            title,
            body: input.body,
            prerelease: input.prerelease,
            draft: input.draft,
        },
    )
    .await?
    {
        storage::ReleaseMutation::Ready(release) => {
            finish_publication(&server, &principal, key, repo, release).await?
        }
        storage::ReleaseMutation::PublicationPending(release) => {
            finish_publication(&server, &principal, key, repo, release).await?
        }
    };
    Ok((StatusCode::CREATED, Json(view(&release))))
}

async fn visible_release(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    number: u64,
) -> Result<Release> {
    let include_drafts = principal.can_write(&repo.config);
    let actor = app::actor(principal)?;
    storage::release(server, repo, &actor, app::number(number)?)
        .await?
        .filter(|release| {
            !release.deleted
                && (include_drafts || (!release.draft && release.publication_pending.is_none()))
        })
        .ok_or(Error::ReleaseNotFound)
}

async fn download_asset(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number, asset_id)): Path<(String, String, u64, String)>,
) -> Result<Response> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let release = visible_release(&server, repo, &principal, number).await?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.id == asset_id)
        .ok_or(Error::ReleaseAssetNotFound)?;
    let cancel = server.cancellation.child_token();
    let permit = server
        .acquire_transfer(&cancel)
        .await
        .map_err(transfer_error)?;
    let guard = cancel.clone().drop_guard();
    let deadline = tokio::time::Instant::now() + ASSET_BUDGET;
    let path = repo.layout.repo_path(&asset_path(&asset.digest));
    let (meta, _, stream) = tokio::select! {
        () = cancel.cancelled() => return Err(Error::ReleaseAssetCancelled),
        result = tokio::time::timeout_at(deadline, repo.store.get_stream(&path, None)) => result.map_err(|_| Error::ReleaseAssetCancelled)??,
    };
    if meta.size != asset.size {
        return Err(Error::Storage(crab_storage::StorageError::CorruptObject {
            path: meta.location.to_string(),
            reason: "release asset size does not match metadata".to_owned(),
        }));
    }
    let stream = stream
        .take_until(async move {
            tokio::select! {
                () = cancel.cancelled() => {},
                () = tokio::time::sleep_until(deadline) => {},
            }
        })
        .map(move |chunk| {
            let _ = (&permit, &guard);
            chunk
        });
    Ok((
        [
            (header::CONTENT_TYPE, asset.content_type.clone()),
            (header::CONTENT_LENGTH, asset.size.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", asset.name),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

async fn upload_asset(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(input): Query<AssetUpload>,
    headers: HeaderMap,
    request: Request,
) -> Result<impl IntoResponse> {
    let key = (owner, name);
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    if !principal.can_write(&repo.config) {
        return Err(Error::ReleasePermission);
    }
    let actor = app::actor(&principal)?;
    if repo.lifecycle(&server, &actor).await?.archived {
        return Err(Error::Archived);
    }
    let number = app::number(number)?;
    let request_id = app::submission(&input.request_id)?;
    let name = asset_name(&input.name)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 255)
        .unwrap_or("application/octet-stream")
        .to_owned();
    if headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|value| value != "identity")
    {
        return Err(Error::Invalid(
            "Release asset uploads require identity content encoding",
        ));
    }
    let staging_bytes = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(crate::server::MAX_DEPENDENCY_FILE_BYTES);
    if staging_bytes > crate::server::MAX_DEPENDENCY_FILE_BYTES {
        return Err(Error::ReleaseAssetTooLarge);
    }
    let reservation = storage::AssetReservation {
        release: number,
        request_id,
        expected_version: input.version,
        name,
        content_type,
        uploader: actor,
    };
    let reservation = match storage::reserve_asset(&server, repo, reservation).await? {
        storage::AssetReservationOutcome::Attached(release) => {
            return Ok((StatusCode::OK, Json(view(&release))));
        }
        storage::AssetReservationOutcome::Reserved(reservation) => reservation,
    };

    let cancel = server.cancellation.child_token();
    let permit = server
        .acquire_transfer(&cancel)
        .await
        .map_err(transfer_error)?;
    let _guard = cancel.clone().drop_guard();
    let worker_server = Arc::clone(&server);
    let (send, result) = tokio::sync::oneshot::channel();
    server.receives.spawn(async move {
        let _permit = permit;
        let work = async {
            let directory = worker_server
                .local_staging
                .create(staging_bytes, &cancel)
                .await
                .map_err(staging_error)?;
            let path = directory.path().join("release-asset");
            let mut file = tokio::fs::File::create(&path)
                .await
                .map_err(Error::ReleaseAssetIo)?;
            let mut stream = request.into_body().into_data_stream();
            let mut size = 0_u64;
            let mut sha256 = Sha256::new();
            let mut blake3 = blake3::Hasher::new();
            while let Some(chunk) = tokio::select! {
                () = cancel.cancelled() => return Err(Error::ReleaseAssetCancelled),
                chunk = stream.next() => chunk,
            } {
                let chunk = chunk.map_err(Error::ReleaseAssetBody)?;
                size = size
                    .checked_add(chunk.len() as u64)
                    .filter(|size| *size <= crate::server::MAX_DEPENDENCY_FILE_BYTES)
                    .ok_or(Error::ReleaseAssetTooLarge)?;
                sha256.update(&chunk);
                blake3.update(&chunk);
                file.write_all(&chunk)
                    .await
                    .map_err(Error::ReleaseAssetIo)?;
            }
            file.flush().await.map_err(Error::ReleaseAssetIo)?;
            drop(file);
            let digest = format!("{:x}", sha256.finalize());
            let expected_hash = *blake3.finalize().as_bytes();
            let repo = app::repository(&worker_server, &principal, &key)?;
            let repo = repo.as_ref();
            let actor = app::actor(&principal)?;
            if repo.lifecycle(&worker_server, &actor).await?.archived
                || !principal.can_write(&repo.config)
            {
                return Err(Error::Archived);
            }
            repo.store
                .put_multipart_file_retry(
                    &repo.layout.repo_path(&asset_path(&digest)),
                    &path,
                    size,
                    expected_hash,
                    8 * 1024 * 1024,
                    &cancel,
                    None,
                )
                .await?;
            storage::attach_asset(&worker_server, repo, &reservation, size, digest).await
        };
        tokio::pin!(work);
        let completed = tokio::select! {
            result = &mut work => result,
            () = tokio::time::sleep(ASSET_BUDGET) => { cancel.cancel(); work.await },
        };
        if let Err(Err(error)) = send.send(completed) {
            tracing::error!(error = ?error, "disconnected release asset upload failed");
        }
    });
    let release = result.await.map_err(|_| Error::ReleaseAssetCancelled)??;
    Ok((StatusCode::CREATED, Json(view(&release))))
}

async fn remove_asset(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number, asset_id)): Path<(String, String, u64, String)>,
    input: std::result::Result<Json<ReleaseDelete>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    if !principal.can_write(&repo.config) {
        return Err(Error::ReleasePermission);
    }
    let actor = app::actor(&principal)?;
    if repo.lifecycle(&server, &actor).await?.archived {
        return Err(Error::Archived);
    }
    let Json(input) = input?;
    let release = storage::delete_asset(
        &server,
        repo,
        &actor,
        app::number(number)?,
        &asset_id,
        input.version,
    )
    .await?;
    Ok(Json(view(&release)))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let release = visible_release(&server, repo, &principal, number).await?;
    Ok(Json(view(&release)))
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<ReleaseEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let key = (owner, name);
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    if !principal.can_write(&repo.config) {
        return Err(Error::ReleasePermission);
    }
    let Json(input) = input?;
    let title = app::title(&input.title)?;
    app::body(&input.body, false)?;
    let actor = app::actor(&principal)?;
    let release = match storage::update(
        &server,
        repo,
        &actor,
        app::number(number)?,
        storage::ReleaseUpdate {
            version: input.version,
            title,
            body: input.body,
            prerelease: input.prerelease,
            draft: input.draft,
        },
    )
    .await?
    {
        storage::ReleaseMutation::Ready(release) => release,
        storage::ReleaseMutation::PublicationPending(release) => {
            finish_publication(&server, &principal, key, repo, release).await?
        }
    };
    Ok(Json(view(&release)))
}

async fn remove(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<ReleaseDelete>, JsonRejection>,
) -> Result<StatusCode> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    if !principal.can_write(&repo.config) {
        return Err(Error::ReleasePermission);
    }
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    storage::delete(&server, repo, &actor, app::number(number)?, input.version).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(parameters): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    let include_drafts = principal.can_write(&repo.config);
    let limit = u8::try_from(parameters.limit()?)
        .map_err(|_| Error::Invalid("Release page size or cursor is invalid"))?;
    let query = parameters.query()?;
    let actor = app::actor(&principal)?;
    let (releases, next) = storage::list(
        &server,
        repo,
        &actor,
        parameters.before,
        limit,
        include_drafts,
        query,
    )
    .await?;
    let items = releases.iter().map(view).collect::<Vec<_>>();
    Ok(Json(json!({
        "items": items,
        "next": next,
    })))
}
