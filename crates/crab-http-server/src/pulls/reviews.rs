use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use crab_remote_git::RepositoryOptions;
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{current_branches, storage};
use crate::{
    app::{self, Error, Result},
    app_storage,
    auth::{Identity, Principal},
    server::Server,
};
use storage::{NewPullReview, PullRequest, PullReview, PullState, ReviewState};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/reviews",
            get(list).post(create),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/reviews/{review}",
            get(detail).patch(edit),
        )
}

fn review_view(review: &PullReview, actor: &Identity, current_head: Option<&str>) -> Value {
    json!({
        "number": review.number,
        "author": review.author.name,
        "body": review.body,
        "state": review.state,
        "commit_oid": review.commit_oid,
        "current": current_head.is_some_and(|oid| oid == review.commit_oid),
        "version": review.version,
        "created_at": review.created_at,
        "updated_at": review.updated_at,
        "can_edit": app_storage::same_author(&review.author, actor),
    })
}

async fn pull_and_head(
    server: &Server,
    repo: &crate::server::Repository,
    id: u64,
) -> Result<(PullRequest, Option<String>)> {
    let (pull, _) = app_storage::read::<PullRequest>(repo, &storage::pull_path(id))
        .await?
        .ok_or(Error::NotFound)?;
    if let Some(merge) = &pull.merge {
        let head = merge.commit_oid.clone();
        return Ok((pull, Some(head)));
    }
    let repository = repo
        .open_current(server, RepositoryOptions::default(), &server.cancellation)
        .await
        .ok();
    let head = repository
        .as_ref()
        .and_then(|repository| current_branches(repository, &pull))
        .map(|(_, head)| head);
    Ok((pull, head))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ListParameters {
    before: Option<u64>,
    limit: Option<usize>,
}

impl ListParameters {
    fn limit(&self) -> Result<usize> {
        let limit = self.limit.unwrap_or(30);
        if !(1..=50).contains(&limit)
            || self
                .before
                .is_some_and(|value| value == 0 || value > app_storage::MAX_NUMBER)
        {
            return Err(Error::Invalid("Invalid review page"));
        }
        Ok(limit)
    }
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let id = app::number(id)?;
    let (_, head) = pull_and_head(&server, repo, id).await?;
    let actor = app::actor(&principal)?;
    let limit = params.limit()?;
    let root = storage::reviews_root(id);
    let last = app_storage::last_number(repo, &root).await?;
    let mut next = last.min(params.before.map_or(last, |before| before - 1));
    let mut items = Vec::new();
    let mut scanned = 0;
    while next > 0 && items.len() < limit && scanned < 200 {
        let bottom = next.saturating_sub(8);
        let batch =
            futures_util::stream::iter(((bottom + 1)..=next).rev().map(|number| async move {
                app_storage::read::<PullReview>(repo, &storage::review_path(id, number)).await
            }))
            .buffered(8)
            .try_collect::<Vec<_>>()
            .await?;
        for entry in batch {
            next -= 1;
            scanned += 1;
            if let Some((review, _)) = entry {
                items.push(review_view(&review, &actor, head.as_deref()));
            }
            if items.len() == limit || scanned == 200 {
                break;
            }
        }
    }
    Ok(Json(
        json!({"items":items,"next":(next > 0).then_some(next + 1)}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewReview {
    request_id: String,
    body: String,
    state: ReviewState,
}

fn validate_body(body: &str, state: ReviewState) -> Result<()> {
    app::body(body, state != ReviewState::Approved)
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    input: std::result::Result<Json<NewReview>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let id = app::number(id)?;
    let Json(input) = input?;
    validate_body(&input.body, input.state)?;
    let actor = app::actor(&principal)?;
    let request_id = app::submission(&input.request_id)?;
    if let Some(review) =
        storage::recover_review(repo, id, &actor, &request_id, &input.body, input.state).await?
    {
        let (_, head) = pull_and_head(&server, repo, id).await?;
        return Ok((
            StatusCode::CREATED,
            Json(review_view(&review, &actor, head.as_deref())),
        ));
    }
    let (pull, head) = pull_and_head(&server, repo, id).await?;
    if pull.state != PullState::Open {
        return Err(Error::Invalid("Closed pull requests cannot be reviewed"));
    }
    let head = head.ok_or(Error::Invalid(
        "Pull request branches must exist before submitting a review",
    ))?;
    if input.state != ReviewState::Commented && app_storage::same_author(&pull.author, &actor) {
        return Err(Error::OwnReview);
    }
    let review = storage::create_review(
        repo,
        id,
        NewPullReview {
            author: actor.clone(),
            request_id,
            body: input.body,
            state: input.state,
            commit_oid: head.clone(),
        },
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(review_view(&review, &actor, Some(&head))),
    ))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, review)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let id = app::number(id)?;
    let (_, head) = pull_and_head(&server, repo, id).await?;
    let (review, _) =
        app_storage::read::<PullReview>(repo, &storage::review_path(id, app::number(review)?))
            .await?
            .ok_or(Error::NotFound)?;
    Ok(Json(review_view(
        &review,
        &app::actor(&principal)?,
        head.as_deref(),
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewEdit {
    version: u64,
    body: String,
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, review)): Path<(String, String, u64, u64)>,
    input: std::result::Result<Json<ReviewEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let id = app::number(id)?;
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let path = storage::review_path(id, app::number(review)?);
    let (mut review, etag) = app_storage::read::<PullReview>(repo, &path)
        .await?
        .ok_or(Error::NotFound)?;
    if !app_storage::same_author(&review.author, &actor) {
        return Err(Error::Forbidden);
    }
    if input.version != review.version {
        return Err(Error::Conflict);
    }
    validate_body(&input.body, review.state)?;
    review.body = input.body;
    review.version = review
        .version
        .checked_add(1)
        .filter(|value| *value < app_storage::MAX_NUMBER)
        .ok_or(Error::Conflict)?;
    review.updated_at = app_storage::now()?;
    app_storage::update(repo, &path, &review, etag).await?;
    let (_, head) = pull_and_head(&server, repo, id).await?;
    Ok(Json(review_view(&review, &actor, head.as_deref())))
}
