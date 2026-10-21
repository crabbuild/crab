use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ListParameters, same_author, storage};
use crate::{
    api::reactions,
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::collaboration::Target,
    server::Server,
};
use storage::PullComment;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/comments",
            get(comments).post(comment),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/comments/{comment}",
            get(comment_detail).patch(edit_comment),
        )
}

fn comment_view(comment: &PullComment, actor: &Identity, reactions: &reactions::View) -> Value {
    json!({
        "number": comment.number,
        "body": comment.body,
        "reactions": reactions,
        "author": comment.author.name,
        "version": comment.version,
        "created_at": comment.created_at,
        "updated_at": comment.updated_at,
        "can_edit": same_author(&comment.author, actor),
    })
}

async fn comments(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let id = app::number(id)?;
    if params.state.is_some() || params.q.is_some() {
        return Err(Error::Invalid("Comments do not support list filters"));
    }
    let actor = app::actor(&principal)?;
    let limit =
        u8::try_from(params.limit()?).map_err(|_| Error::Invalid("Invalid pull request page"))?;
    let (comments, next) = storage::comments(&server, repo, id, params.before, limit)
        .await?
        .ok_or(Error::NotFound)?;
    let targets = comments
        .iter()
        .map(|comment| Target::PullComment {
            pull: id,
            number: comment.number,
        })
        .collect();
    let reactions = reactions::read(&server, repo, &actor, targets).await?;
    let items = comments
        .iter()
        .zip(reactions)
        .map(|(comment, reactions)| comment_view(comment, &actor, &reactions))
        .collect::<Vec<_>>();
    Ok(Json(json!({"items":items,"next":next})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewComment {
    request_id: String,
    body: String,
}

async fn comment(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    input: std::result::Result<Json<NewComment>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let id = app::number(id)?;
    let Json(input) = input?;
    app::body(&input.body, true)?;
    let actor = app::actor(&principal)?;
    let comment = storage::create_comment(
        &server,
        repo,
        id,
        actor.clone(),
        app::submission(&input.request_id)?,
        input.body,
    )
    .await?;
    let reaction = reactions::one(
        &server,
        repo,
        &actor,
        Target::PullComment {
            pull: id,
            number: comment.number,
        },
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(comment_view(&comment, &actor, &reaction)),
    ))
}

async fn comment_detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, comment)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let comment = storage::comment(&server, repo, app::number(id)?, app::number(comment)?)
        .await?
        .ok_or(Error::NotFound)?;
    let reaction = reactions::one(
        &server,
        repo,
        &actor,
        Target::PullComment {
            pull: id,
            number: comment.number,
        },
    )
    .await?;
    Ok(Json(comment_view(&comment, &actor, &reaction)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentEdit {
    version: u64,
    body: String,
}

async fn edit_comment(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, comment)): Path<(String, String, u64, u64)>,
    input: std::result::Result<Json<CommentEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    app::body(&input.body, true)?;
    let comment = storage::update_comment(
        &server,
        repo,
        app::number(id)?,
        app::number(comment)?,
        actor.clone(),
        input.version,
        input.body,
    )
    .await?;
    let reaction = reactions::one(
        &server,
        repo,
        &actor,
        Target::PullComment {
            pull: id,
            number: comment.number,
        },
    )
    .await?;
    Ok(Json(comment_view(&comment, &actor, &reaction)))
}
