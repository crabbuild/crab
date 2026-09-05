use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use futures_util::{StreamExt, TryStreamExt};
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Release {
    number: u64,
    request_id: String,
    author: Identity,
    tag_name: String,
    tag_oid: String,
    target_oid: String,
    title: String,
    body: String,
    prerelease: bool,
    created_at: u64,
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
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ListParameters {
    before: Option<u64>,
    limit: Option<usize>,
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
}

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/releases", get(list).post(create))
        .route("/api/repos/{owner}/{name}/releases/{number}", get(detail))
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

fn same_reservation(left: &ReleaseReservation, right: &ReleaseReservation) -> bool {
    left.request_id == right.request_id
        && app_storage::same_author(&left.author, &right.author)
        && left.tag_name == right.tag_name
        && left.target_oid == right.target_oid
        && left.title == right.title
        && left.body == right.body
        && left.prerelease == right.prerelease
}

fn same_claim(left: &ReleaseClaim, right: &ReleaseClaim) -> bool {
    left.request_id == right.request_id
        && app_storage::same_author(&left.author, &right.author)
        && left.tag_name == right.tag_name
        && left.target_oid == right.target_oid
        && left.title == right.title
        && left.body == right.body
        && left.prerelease == right.prerelease
}

fn same_release(
    release: &Release,
    reservation: &ReleaseReservation,
    expected_tag_oid: &str,
) -> bool {
    release.number == reservation.number
        && release.request_id == reservation.request_id
        && app_storage::same_author(&release.author, &reservation.author)
        && release.tag_name == reservation.tag_name
        && release.tag_oid == expected_tag_oid
        && release.target_oid == reservation.target_oid
        && release.title == reservation.title
        && release.body == reservation.body
        && release.prerelease == reservation.prerelease
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
        "author": release.author.name,
        "created_at": release.created_at,
    })
}

async fn reserve(
    repo: &Repository,
    request_id: String,
    author: Identity,
    tag_name: String,
    target_oid: String,
    title: String,
    body: String,
    prerelease: bool,
) -> Result<ReleaseReservation> {
    let claim = ReleaseClaim {
        request_id: request_id.clone(),
        author: author.clone(),
        tag_name: tag_name.clone(),
        target_oid: target_oid.clone(),
        title: title.clone(),
        body: body.clone(),
        prerelease,
    };
    let request =
        app_storage::create_or_read(repo, &request_path(&request_id), claim.clone()).await?;
    if !same_claim(&request, &claim) {
        return Err(Error::RequestConflict);
    }
    let tag = app_storage::create_or_read(repo, &tag_path(&tag_name), claim.clone()).await?;
    if !same_claim(&tag, &claim) {
        return Err(Error::ReleaseConflict);
    }
    let existing =
        app_storage::read::<ReleaseReservation>(repo, &reservation_path(&request_id)).await?;
    let (number, created_at) = match existing.as_ref() {
        Some((reservation, _)) => (reservation.number, reservation.created_at),
        None => (
            app_storage::reserve_number(repo, ROOT).await?,
            app_storage::now()?,
        ),
    };
    let proposed = ReleaseReservation {
        number,
        request_id: request_id.clone(),
        author,
        tag_name,
        target_oid,
        title,
        body,
        prerelease,
        created_at,
    };
    let request = if let Some((reservation, _)) = existing {
        if !same_reservation(&reservation, &proposed) {
            return Err(Error::RequestConflict);
        }
        reservation
    } else {
        app_storage::create_or_read(repo, &reservation_path(&request_id), proposed.clone()).await?
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
        request_id,
        app::actor(&principal)?,
        tag_name,
        target_oid,
        title,
        input.body,
        input.prerelease,
    )
    .await?;
    let tag_oid = ensure_tag(Arc::clone(&server), principal, key, repo, reference, target)
        .await?
        .to_string();
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
        created_at: reservation.created_at,
    };
    let release = app_storage::create_or_read(repo, &release_path(release.number), release).await?;
    if !same_release(&release, &reservation, &tag_oid) {
        return Err(Error::ReleaseConflict);
    }
    Ok((StatusCode::CREATED, Json(view(&release))))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let release = app_storage::read::<Release>(repo, &release_path(app::number(number)?))
        .await?
        .map(|(release, _)| release)
        .ok_or(Error::ReleaseNotFound)?;
    Ok(Json(view(&release)))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(parameters): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let limit = parameters.limit()?;
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
            if let Some((release, _)) = entry {
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
