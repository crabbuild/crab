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
use futures_util::{StreamExt, TryStreamExt};
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{
    app::{self, Error, Result},
    app_storage,
    auth::{Identity, Principal},
    receive::{self, ReceiveError},
    server::{Repository, Server},
    statuses,
};

const ROOT: &str = "app/v1/releases";
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseClaim {
    request_id: String,
    author: Identity,
    tag_name: String,
    target_oid: String,
    title: String,
    body: String,
    prerelease: bool,
    draft: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseReservation {
    number: u64,
    request_id: String,
    author: Identity,
    tag_name: String,
    target_oid: String,
    title: String,
    body: String,
    prerelease: bool,
    draft: bool,
    created_at: u64,
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
    version: u64,
    created_at: u64,
    published_at: Option<u64>,
    updated_at: u64,
    deleted: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum TagClaim {
    Active {
        claim: ReleaseClaim,
    },
    Deleted {
        request_id: String,
        number: u64,
        version: u64,
    },
}

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
                .is_some_and(|value| value == 0 || value > app_storage::MAX_NUMBER)
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

fn release_path(number: u64) -> String {
    format!("{ROOT}/{number:016}/release.json")
}

fn request_path(request_id: &str) -> String {
    format!("{ROOT}/requests/{request_id}.json")
}

fn reservation_path(request_id: &str) -> String {
    format!("{ROOT}/reservations/{request_id}.json")
}

fn tag_path(tag_name: &str) -> String {
    format!(
        "{ROOT}/tags/{}.json",
        blake3::hash(tag_name.as_bytes()).to_hex()
    )
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
    format!("{ROOT}/assets/sha256/{}/{digest}", &digest[..2])
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

fn same_reservation(left: &ReleaseReservation, right: &ReleaseReservation) -> bool {
    left.request_id == right.request_id
        && app_storage::same_author(&left.author, &right.author)
        && left.tag_name == right.tag_name
        && left.target_oid == right.target_oid
        && left.title == right.title
        && left.body == right.body
        && left.prerelease == right.prerelease
        && left.draft == right.draft
}

fn same_claim(left: &ReleaseClaim, right: &ReleaseClaim) -> bool {
    left.request_id == right.request_id
        && app_storage::same_author(&left.author, &right.author)
        && left.tag_name == right.tag_name
        && left.target_oid == right.target_oid
        && left.title == right.title
        && left.body == right.body
        && left.prerelease == right.prerelease
        && left.draft == right.draft
}

fn same_release(release: &Release, reservation: &ReleaseReservation) -> bool {
    release.number == reservation.number
        && release.request_id == reservation.request_id
        && app_storage::same_author(&release.author, &reservation.author)
        && release.tag_name == reservation.tag_name
        && release.target_oid == reservation.target_oid
        && release.created_at == reservation.created_at
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

fn matches_query(release: &Release, query: Option<&str>) -> bool {
    query.is_none_or(|query| {
        [
            &release.tag_name,
            &release.title,
            &release.body,
            &release.author.name,
        ]
        .iter()
        .any(|value| value.to_lowercase().contains(query))
    })
}

async fn claim_tag(repo: &Repository, claim: &ReleaseClaim) -> Result<()> {
    let path = tag_path(&claim.tag_name);
    for _ in 0..10 {
        let stored = app_storage::read::<TagClaim>(repo, &path).await?;
        let tag = match stored {
            Some((TagClaim::Active { claim: current }, _)) => {
                if same_claim(&current, claim) {
                    return Ok(());
                }
                return Err(Error::ReleaseConflict);
            }
            Some((TagClaim::Deleted { request_id, .. }, _)) if request_id == claim.request_id => {
                return Err(Error::ReleaseConflict);
            }
            Some((TagClaim::Deleted { .. }, etag)) => {
                match app_storage::update(
                    repo,
                    &path,
                    &TagClaim::Active {
                        claim: claim.clone(),
                    },
                    etag,
                )
                .await
                {
                    Ok(()) => return Ok(()),
                    Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => {
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            None => {
                app_storage::create_or_read(
                    repo,
                    &path,
                    TagClaim::Active {
                        claim: claim.clone(),
                    },
                )
                .await?
            }
        };
        match tag {
            TagClaim::Active { claim: current } if same_claim(&current, claim) => return Ok(()),
            TagClaim::Active { .. } => return Err(Error::ReleaseConflict),
            TagClaim::Deleted { .. } => continue,
        }
    }
    Err(Error::Conflict)
}

async fn release_tag_claim(repo: &Repository, release: &Release, version: u64) -> Result<()> {
    let path = tag_path(&release.tag_name);
    for _ in 0..10 {
        let (claim, etag) = app_storage::read::<TagClaim>(repo, &path)
            .await?
            .ok_or(Error::ReleaseConflict)?;
        match claim {
            TagClaim::Deleted {
                request_id,
                number,
                version: deleted_version,
            } if request_id == release.request_id
                && number == release.number
                && deleted_version == version =>
            {
                return Ok(());
            }
            TagClaim::Deleted { .. } => return Err(Error::ReleaseConflict),
            TagClaim::Active { claim } if claim.request_id != release.request_id => return Ok(()),
            TagClaim::Active { claim }
                if claim.tag_name != release.tag_name || claim.target_oid != release.target_oid =>
            {
                return Err(Error::ReleaseConflict);
            }
            TagClaim::Active { .. } => {}
        }
        match app_storage::update(
            repo,
            &path,
            &TagClaim::Deleted {
                request_id: release.request_id.clone(),
                number: release.number,
                version,
            },
            etag,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}

async fn reserve(repo: &Repository, claim: ReleaseClaim) -> Result<ReleaseReservation> {
    let request =
        app_storage::create_or_read(repo, &request_path(&claim.request_id), claim.clone()).await?;
    if !same_claim(&request, &claim) {
        return Err(Error::RequestConflict);
    }
    claim_tag(repo, &claim).await?;
    let existing =
        app_storage::read::<ReleaseReservation>(repo, &reservation_path(&claim.request_id)).await?;
    let (number, created_at) = match existing.as_ref() {
        Some((reservation, _)) => (reservation.number, reservation.created_at),
        None => (
            app_storage::reserve_number(repo, ROOT).await?,
            app_storage::now()?,
        ),
    };
    let proposed = ReleaseReservation {
        number,
        request_id: claim.request_id.clone(),
        author: claim.author.clone(),
        tag_name: claim.tag_name.clone(),
        target_oid: claim.target_oid.clone(),
        title: claim.title.clone(),
        body: claim.body.clone(),
        prerelease: claim.prerelease,
        draft: claim.draft,
        created_at,
    };
    let request = if let Some((reservation, _)) = existing {
        if !same_reservation(&reservation, &proposed) {
            return Err(Error::RequestConflict);
        }
        reservation
    } else {
        app_storage::create_or_read(repo, &reservation_path(&claim.request_id), proposed.clone())
            .await?
    };
    if !same_reservation(&request, &proposed) {
        return Err(Error::RequestConflict);
    }
    Ok(request)
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
    let reservation = reserve(
        repo,
        ReleaseClaim {
            request_id,
            author: app::actor(&principal)?,
            tag_name,
            target_oid,
            title,
            body: input.body,
            prerelease: input.prerelease,
            draft: input.draft,
        },
    )
    .await?;
    let tag_oid = if reservation.draft {
        None
    } else {
        Some(
            ensure_tag(Arc::clone(&server), principal, key, repo, reference, target)
                .await?
                .to_string(),
        )
    };
    let release = Release {
        number: reservation.number,
        request_id: reservation.request_id.clone(),
        author: reservation.author.clone(),
        tag_name: reservation.tag_name.clone(),
        tag_oid: tag_oid.clone(),
        target_oid: reservation.target_oid.clone(),
        title: reservation.title.clone(),
        body: reservation.body.clone(),
        prerelease: reservation.prerelease,
        draft: reservation.draft,
        version: 1,
        created_at: reservation.created_at,
        published_at: (!reservation.draft).then_some(reservation.created_at),
        updated_at: reservation.created_at,
        deleted: false,
        assets: Vec::new(),
    };
    let release = app_storage::create_or_read(repo, &release_path(release.number), release).await?;
    if release.deleted
        || !same_release(&release, &reservation)
        || tag_oid.is_some_and(|tag_oid| release.tag_oid.as_deref() != Some(&tag_oid))
    {
        return Err(Error::ReleaseConflict);
    }
    Ok((StatusCode::CREATED, Json(view(&release))))
}

async fn visible_release(
    repo: &Repository,
    principal: &Principal,
    number: u64,
) -> Result<(Release, crab_storage::ETag)> {
    let include_drafts = principal.can_write(&repo.config);
    app_storage::read::<Release>(repo, &release_path(app::number(number)?))
        .await?
        .filter(|(release, _)| !release.deleted && (include_drafts || !release.draft))
        .ok_or(Error::ReleaseNotFound)
}

async fn download_asset(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number, asset_id)): Path<(String, String, u64, String)>,
) -> Result<Response> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let (release, _) = visible_release(repo, &principal, number).await?;
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
    if repo.lifecycle().await?.archived {
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
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|size| size > crate::server::MAX_DEPENDENCY_FILE_BYTES)
    {
        return Err(Error::ReleaseAssetTooLarge);
    }
    let (release, _) = visible_release(repo, &principal, number).await?;
    if release
        .assets
        .iter()
        .any(|asset| asset.name == name && asset.id != request_id)
    {
        return Err(Error::ReleaseAssetConflict);
    }
    if let Some(asset) = release.assets.iter().find(|asset| asset.id == request_id) {
        if asset.name == name {
            return Ok((StatusCode::OK, Json(view(&release))));
        }
        return Err(Error::RequestConflict);
    }
    if release.version != input.version {
        return Err(Error::Conflict);
    }

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
            let directory = tokio::task::spawn_blocking(tempfile::tempdir)
                .await
                .map_err(Error::ReleaseAssetWorker)?
                .map_err(Error::ReleaseAssetIo)?;
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
            if repo.lifecycle().await?.archived || !principal.can_write(&repo.config) {
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
            let release_path = release_path(number);
            let (mut release, etag) = visible_release(repo, &principal, number).await?;
            if let Some(asset) = release.assets.iter().find(|asset| asset.id == request_id) {
                if asset.name == name
                    && asset.size == size
                    && asset.digest == digest
                    && asset.content_type == content_type
                {
                    return Ok(release);
                }
                return Err(Error::RequestConflict);
            }
            if release.version != input.version {
                return Err(Error::Conflict);
            }
            if release.assets.iter().any(|asset| asset.name == name) {
                return Err(Error::ReleaseAssetConflict);
            }
            let created_at = app_storage::now()?;
            release.assets.push(ReleaseAsset {
                id: request_id,
                name,
                content_type,
                size,
                digest,
                uploader: app::actor(&principal)?,
                created_at,
            });
            release
                .assets
                .sort_by(|left, right| left.name.cmp(&right.name));
            release.version = release
                .version
                .checked_add(1)
                .filter(|version| *version < app_storage::MAX_NUMBER)
                .ok_or(Error::Conflict)?;
            release.updated_at = created_at;
            app_storage::update(repo, &release_path, &release, etag).await?;
            Ok::<_, Error>(release)
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
    if repo.lifecycle().await?.archived {
        return Err(Error::Archived);
    }
    let Json(input) = input?;
    let path = release_path(app::number(number)?);
    let (mut release, etag) = visible_release(repo, &principal, number).await?;
    if release.version != input.version {
        return Err(Error::Conflict);
    }
    let index = release
        .assets
        .iter()
        .position(|asset| asset.id == asset_id)
        .ok_or(Error::ReleaseAssetNotFound)?;
    release.assets.remove(index);
    release.version = release
        .version
        .checked_add(1)
        .filter(|version| *version < app_storage::MAX_NUMBER)
        .ok_or(Error::Conflict)?;
    release.updated_at = app_storage::now()?;
    app_storage::update(repo, &path, &release, etag).await?;
    Ok(Json(view(&release)))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let include_drafts = principal.can_write(&repo.config);
    let release = app_storage::read::<Release>(repo, &release_path(app::number(number)?))
        .await?
        .map(|(release, _)| release)
        .filter(|release| !release.deleted && (include_drafts || !release.draft))
        .ok_or(Error::ReleaseNotFound)?;
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
    let path = release_path(app::number(number)?);
    let (mut release, etag) = app_storage::read::<Release>(repo, &path)
        .await?
        .filter(|(release, _)| !release.deleted)
        .ok_or(Error::ReleaseNotFound)?;
    if release.version != input.version {
        return Err(Error::Conflict);
    }
    let now = app_storage::now()?;
    if release.draft && !input.draft {
        let target = ObjectId::from_hex(release.target_oid.as_bytes())
            .map_err(|_| Error::ReleaseConflict)?;
        let (_, reference) = tag_name(&release.tag_name)?;
        release.tag_oid = Some(
            ensure_tag(
                Arc::clone(&server),
                principal.clone(),
                key,
                repo,
                reference,
                target,
            )
            .await?
            .to_string(),
        );
        release.published_at = Some(now);
    } else if !release.draft && input.draft {
        release.published_at = None;
    }
    release.title = title;
    release.body = input.body;
    release.prerelease = input.prerelease;
    release.draft = input.draft;
    release.version = release
        .version
        .checked_add(1)
        .filter(|version| *version < app_storage::MAX_NUMBER)
        .ok_or(Error::Conflict)?;
    release.updated_at = now;
    if !principal.can_write(&repo.config) {
        return Err(Error::ReleasePermission);
    }
    app_storage::update(repo, &path, &release, etag).await?;
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
    let path = release_path(app::number(number)?);
    let (mut release, etag) = app_storage::read::<Release>(repo, &path)
        .await?
        .ok_or(Error::ReleaseNotFound)?;
    if release.deleted {
        if input.version.checked_add(1) != Some(release.version) {
            return Err(Error::ReleaseNotFound);
        }
        release_tag_claim(repo, &release, input.version).await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    if release.version != input.version {
        return Err(Error::Conflict);
    }
    release.version = release
        .version
        .checked_add(1)
        .filter(|version| *version < app_storage::MAX_NUMBER)
        .ok_or(Error::Conflict)?;
    release.updated_at = app_storage::now()?;
    release.deleted = true;
    if !principal.can_write(&repo.config) {
        return Err(Error::ReleasePermission);
    }
    app_storage::update(repo, &path, &release, etag).await?;
    release_tag_claim(repo, &release, input.version).await?;
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
    let limit = parameters.limit()?;
    let query = parameters.query()?;
    let last = app_storage::last_number(repo, ROOT).await?;
    let mut next = last.min(parameters.before.map_or(last, |before| before - 1));
    let mut items = Vec::new();
    let mut scanned = 0;
    while next > 0 && items.len() < limit && scanned < 200 {
        let bottom = next.saturating_sub(8);
        let batch =
            futures_util::stream::iter(((bottom + 1)..=next).rev().map(|number| async move {
                app_storage::read::<Release>(repo, &release_path(number)).await
            }))
            .buffered(8)
            .try_collect::<Vec<_>>()
            .await?;
        for entry in batch {
            next -= 1;
            scanned += 1;
            if let Some((release, _)) = entry
                && !release.deleted
                && (include_drafts || !release.draft)
                && matches_query(&release, query.as_deref())
            {
                items.push(view(&release));
            }
            if items.len() == limit || scanned == 200 {
                break;
            }
        }
    }
    Ok(Json(json!({
        "items": items,
        "next": (next > 0).then_some(next + 1),
    })))
}
