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
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    app::{Error, Result, actor, body, number, repository, submission, title},
    app_storage,
    auth::{Identity, Principal},
    server::Server,
};
mod storage;
use storage::{Comment, Issue, IssueState};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/issues", get(list).post(create))
        .route(
            "/api/repos/{owner}/{name}/issues/{number}",
            get(detail).patch(edit),
        )
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/comments",
            get(comments).post(comment),
        )
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/comments/{comment}",
            get(comment_detail).patch(edit_comment),
        )
        .layer(axum::extract::DefaultBodyLimit::max(80 * 1024))
        .route_layer(middleware::from_fn_with_state(server, crate::app::admit))
}
fn issue_view(issue: &Issue, author: &Identity, full: bool) -> Value {
    json!({"number":issue.number,"title":issue.title,"body":full.then_some(&issue.body),"state":issue.state,
        "author":issue.author.name,"version":issue.version,"created_at":issue.created_at,"updated_at":issue.updated_at,
        "can_edit":storage::same_author(&issue.author, author)})
}
fn comment_view(comment: &Comment, author: &Identity) -> Value {
    json!({"number":comment.number,"body":comment.body,"author":comment.author.name,"version":comment.version,
        "created_at":comment.created_at,"updated_at":comment.updated_at,"can_edit":storage::same_author(&comment.author, author)})
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ListParameters {
    before: Option<u64>,
    limit: Option<usize>,
    state: Option<String>,
    q: Option<String>,
}
impl ListParameters {
    fn limit(&self) -> Result<usize> {
        let limit = self.limit.unwrap_or(30);
        if !(1..=50).contains(&limit) {
            return Err(Error::Invalid("Page size must be 1–50"));
        }
        if self
            .before
            .is_some_and(|value| value == 0 || value > storage::MAX_NUMBER)
        {
            return Err(Error::Invalid("Invalid page cursor"));
        }
        Ok(limit)
    }
    fn state(&self) -> Result<Option<IssueState>> {
        match self.state.as_deref().unwrap_or("open") {
            "open" => Ok(Some(IssueState::Open)),
            "closed" => Ok(Some(IssueState::Closed)),
            "all" => Ok(None),
            _ => Err(Error::Invalid("Issue state must be open, closed or all")),
        }
    }
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &key)?;
    let author = actor(&principal)?;
    let limit = params.limit()?;
    let state = params.state()?;
    let query = crate::app::search_query(params.q.as_deref())?;
    let last = app_storage::last_number(repo, storage::ROOT).await?;
    let mut next = last.min(params.before.map_or(last, |before| before - 1));
    let mut items = Vec::new();
    let mut scanned = 0;
    // Point reads make ordering independent of provider LIST ordering and bound sparse scans.
    while next > 0 && items.len() < limit && scanned < 200 {
        let bottom = next.saturating_sub(8);
        let batch =
            futures_util::stream::iter(((bottom + 1)..=next).rev().map(|id| async move {
                storage::read::<Issue>(repo, &storage::issue_path(id)).await
            }))
            .buffered(8)
            .try_collect::<Vec<_>>()
            .await?;
        for entry in batch {
            next -= 1;
            scanned += 1;
            if let Some((issue, _)) = entry
                && state.is_none_or(|state| state == issue.state)
                && crate::app::matches_query(
                    query.as_deref(),
                    &[&issue.title, &issue.body, &issue.author.name],
                )
            {
                items.push(issue_view(&issue, &author, false));
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
struct NewIssue {
    request_id: String,
    title: String,
    body: String,
}
async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<NewIssue>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = repository(&server, &principal, &key)?;
    let Json(input) = input?;
    let author = actor(&principal)?;
    let request_id = submission(&input.request_id)?;
    let title = title(&input.title)?;
    body(&input.body, false)?;
    let issue = storage::create_issue(repo, author.clone(), request_id, title, input.body).await?;
    Ok((StatusCode::CREATED, Json(issue_view(&issue, &author, true))))
}
async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let (issue, _) = storage::read::<Issue>(repo, &storage::issue_path(number(id)?))
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(issue_view(&issue, &actor(&principal)?, true)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueEdit {
    version: u64,
    title: Option<String>,
    body: Option<String>,
    state: Option<IssueState>,
}
async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    input: std::result::Result<Json<IssueEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let Json(input) = input?;
    let author = actor(&principal)?;
    let path = storage::issue_path(number(id)?);
    let (mut issue, etag) = storage::read::<Issue>(repo, &path)
        .await?
        .ok_or(Error::NotFound)?;
    if !storage::same_author(&issue.author, &author) {
        return Err(Error::Forbidden);
    }
    if input.version != issue.version {
        return Err(Error::Conflict);
    }
    if input.title.is_none() && input.body.is_none() && input.state.is_none() {
        return Err(Error::Invalid("No issue changes supplied"));
    }
    if let Some(value) = input.title {
        issue.title = title(&value)?;
    }
    if let Some(value) = input.body {
        body(&value, false)?;
        issue.body = value;
    }
    if let Some(value) = input.state {
        issue.state = value;
    }
    issue.version = issue
        .version
        .checked_add(1)
        .filter(|value| *value < storage::MAX_NUMBER)
        .ok_or(Error::Conflict)?;
    issue.updated_at = storage::now()?;
    storage::update(repo, &path, &issue, etag).await?;
    Ok(Json(issue_view(&issue, &author, true)))
}

async fn comments(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let id = number(id)?;
    if storage::read::<Issue>(repo, &storage::issue_path(id))
        .await?
        .is_none()
    {
        return Err(Error::NotFound);
    }
    let author = actor(&principal)?;
    let limit = params.limit()?;
    if params.state.is_some() || params.q.is_some() {
        return Err(Error::Invalid("Comments do not support list filters"));
    }
    let last = app_storage::last_number(repo, &storage::comments_root(id)).await?;
    let mut next = last.min(params.before.map_or(last, |before| before - 1));
    let mut items = Vec::new();
    let mut scanned = 0;
    while next > 0 && items.len() < limit && scanned < 200 {
        let bottom = next.saturating_sub(8);
        let batch =
            futures_util::stream::iter(((bottom + 1)..=next).rev().map(|number| async move {
                storage::read::<Comment>(repo, &storage::comment_path(id, number)).await
            }))
            .buffered(8)
            .try_collect::<Vec<_>>()
            .await?;
        for entry in batch {
            next -= 1;
            scanned += 1;
            if let Some((comment, _)) = entry {
                items.push(comment_view(&comment, &author));
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
    let repo = repository(&server, &principal, &(owner, name))?;
    let id = number(id)?;
    if storage::read::<Issue>(repo, &storage::issue_path(id))
        .await?
        .is_none()
    {
        return Err(Error::NotFound);
    }
    let Json(input) = input?;
    let author = actor(&principal)?;
    body(&input.body, true)?;
    let comment = storage::create_comment(
        repo,
        id,
        author.clone(),
        submission(&input.request_id)?,
        input.body,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(comment_view(&comment, &author))))
}

async fn comment_detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, comment)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let path = storage::comment_path(number(id)?, number(comment)?);
    let (comment, _) = storage::read::<Comment>(repo, &path)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(comment_view(&comment, &actor(&principal)?)))
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
    let repo = repository(&server, &principal, &(owner, name))?;
    let Json(input) = input?;
    let author = actor(&principal)?;
    let path = storage::comment_path(number(id)?, number(comment)?);
    let (mut comment, etag) = storage::read::<Comment>(repo, &path)
        .await?
        .ok_or(Error::NotFound)?;
    if !storage::same_author(&comment.author, &author) {
        return Err(Error::Forbidden);
    }
    if input.version != comment.version {
        return Err(Error::Conflict);
    }
    body(&input.body, true)?;
    comment.body = input.body;
    comment.version = comment
        .version
        .checked_add(1)
        .filter(|value| *value < storage::MAX_NUMBER)
        .ok_or(Error::Conflict)?;
    comment.updated_at = storage::now()?;
    storage::update(repo, &path, &comment, etag).await?;
    Ok(Json(comment_view(&comment, &author)))
}
