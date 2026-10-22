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

use super::{MAX_NUMBER, load_pull, same_author, storage, validate_text};
use crate::{
    api::{profiles::attribution::Authors, reactions},
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::collaboration::Target,
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/threads/{thread}/replies",
            get(list_replies).post(create_reply),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/threads/{thread}/replies/{reply}",
            get(reply_detail).patch(edit_reply),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewReply {
    request_id: String,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyEdit {
    version: u64,
    body: String,
}

fn reply_view(
    authors: &Authors,
    reply: &storage::PullReviewReply,
    actor: &Identity,
    reactions: &reactions::View,
) -> Value {
    json!({
        "number": reply.number,
        "author": authors.view(&reply.author),
        "body": reply.body,
        "reactions": reactions,
        "version": reply.version,
        "created_at": reply.created_at,
        "updated_at": reply.updated_at,
        "can_edit": same_author(&reply.author, actor),
    })
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReplyListParameters {
    before: Option<u64>,
    limit: Option<usize>,
}

impl ReplyListParameters {
    fn limit(&self) -> Result<u8> {
        let limit = self.limit.unwrap_or(50);
        if !(1..=50).contains(&limit)
            || self
                .before
                .is_some_and(|value| value == 0 || value > MAX_NUMBER)
        {
            return Err(Error::Invalid("Invalid review reply page"));
        }
        u8::try_from(limit).map_err(|_| Error::Invalid("Invalid review reply page"))
    }
}

async fn list_replies(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread)): Path<(String, String, u64, u64)>,
    Query(params): Query<ReplyListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, app::number(id)?).await?;
    let page = storage::replies(
        &server,
        repo,
        crate::cells::repository::PullReviewReplyListInput {
            pull: pull.number,
            thread: app::number(thread)?,
            before: params.before,
            limit: params.limit()?,
        },
    )
    .await?
    .ok_or(Error::NotFound)?;
    let targets = page
        .0
        .iter()
        .map(|reply| Target::PullReply {
            pull: pull.number,
            thread,
            number: reply.number,
        })
        .collect();
    let reactions = reactions::read(&server, repo, &actor, targets).await?;
    let authors =
        Authors::load(&server, &principal, page.0.iter().map(|item| &item.author)).await?;
    app::membership(&server, &principal, repo).await?;
    Ok(Json(json!({
        "items": page.0.iter().zip(reactions).map(|(reply, reactions)| reply_view(&authors, reply, &actor, &reactions)).collect::<Vec<_>>(),
        "next": page.1,
    })))
}

async fn create_reply(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread)): Path<(String, String, u64, u64)>,
    input: std::result::Result<Json<NewReply>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    validate_text(&input.body, true)?;
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, app::number(id)?).await?;
    let thread = app::number(thread)?;
    let request_id = app::submission(&input.request_id)?;
    if let Some(existing) =
        storage::reply_submission(&server, repo, pull.number, thread, &request_id).await?
    {
        if !same_author(&existing.author, &actor) || existing.body != input.body {
            return Err(Error::RequestConflict);
        }
        let reaction = reactions::one(
            &server,
            repo,
            &actor,
            Target::PullReply {
                pull: pull.number,
                thread,
                number: existing.number,
            },
        )
        .await?;
        let authors = Authors::load(&server, &principal, std::iter::once(&existing.author)).await?;
        app::membership(&server, &principal, repo).await?;
        return Ok((
            StatusCode::CREATED,
            Json(reply_view(&authors, &existing, &actor, &reaction)),
        ));
    }
    let reply = storage::create_reply(
        &server,
        repo,
        pull.number,
        thread,
        storage::NewPullReviewReply {
            author: actor.clone(),
            request_id,
            body: input.body,
        },
    )
    .await?;
    let reaction = reactions::one(
        &server,
        repo,
        &actor,
        Target::PullReply {
            pull: pull.number,
            thread,
            number: reply.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&reply.author)).await?;
    app::membership(&server, &principal, repo).await?;
    Ok((
        StatusCode::CREATED,
        Json(reply_view(&authors, &reply, &actor, &reaction)),
    ))
}

async fn reply_detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread, reply)): Path<(String, String, u64, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, app::number(id)?).await?;
    let reply = storage::reply(
        &server,
        repo,
        pull.number,
        app::number(thread)?,
        app::number(reply)?,
    )
    .await?
    .ok_or(Error::NotFound)?;
    let reaction = reactions::one(
        &server,
        repo,
        &actor,
        Target::PullReply {
            pull: pull.number,
            thread,
            number: reply.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&reply.author)).await?;
    app::membership(&server, &principal, repo).await?;
    Ok(Json(reply_view(&authors, &reply, &actor, &reaction)))
}

async fn edit_reply(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread, reply)): Path<(String, String, u64, u64, u64)>,
    input: std::result::Result<Json<ReplyEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    validate_text(&input.body, true)?;
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, app::number(id)?).await?;
    let reply = storage::update_reply(
        &server,
        repo,
        pull.number,
        app::number(thread)?,
        app::number(reply)?,
        actor.clone(),
        input.version,
        input.body,
    )
    .await?;
    let reaction = reactions::one(
        &server,
        repo,
        &actor,
        Target::PullReply {
            pull: pull.number,
            thread,
            number: reply.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&reply.author)).await?;
    app::membership(&server, &principal, repo).await?;
    Ok(Json(reply_view(&authors, &reply, &actor, &reaction)))
}
