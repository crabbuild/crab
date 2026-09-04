use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, Request, State, rejection::JsonRejection},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use crab_storage::StorageError;
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::{Identity, Principal},
    server::{Repository, Server},
};
mod storage;
use storage::{Comment, Issue, IssueState};

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Not found")]
    NotFound,
    #[error("Only the author can edit this content")]
    Forbidden,
    #[error("This content changed; reload before saving your draft")]
    Conflict,
    #[error(
        "This submission ID was already used for different content; check the existing discussion before submitting again"
    )]
    RequestConflict,
    #[error("Collaboration storage failed")]
    Storage(#[from] StorageError),
    #[error("Collaboration data encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("Invalid request body")]
    Body(#[from] JsonRejection),
    #[error("Clock failed")]
    Clock(#[from] std::time::SystemTimeError),
}
type Result<T> = std::result::Result<T, Error>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, "invalid_request", *message),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "Discussion not found"),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Only the author can edit this content",
            ),
            Self::Conflict | Self::Storage(StorageError::StateConflict { .. }) => (
                StatusCode::CONFLICT,
                "conflict",
                "This content changed; reload before saving your draft",
            ),
            Self::RequestConflict => (
                StatusCode::CONFLICT,
                "submission_conflict",
                "This submission ID was already used for different content; check the existing discussion before submitting again",
            ),
            Self::Body(error) => (
                error.status(),
                "invalid_request",
                "Invalid JSON request or request body too large",
            ),
            _ => (
                StatusCode::BAD_GATEWAY,
                "storage_error",
                "Discussion storage is unavailable. A write may have succeeded; retry the same submission to recover it",
            ),
        };
        (
            status,
            Json(json!({"error":{"code":code,"message":message}})),
        )
            .into_response()
    }
}

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
        .route_layer(middleware::from_fn_with_state(server, admit))
}

async fn admit(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    let Ok(_permit) = server.app_admission.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":{"message":"Discussion requests are busy; retry shortly"}})),
        )
            .into_response();
    };
    let started = Instant::now();
    let response = tokio::select! {
        () = server.cancellation.cancelled() => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"message":"Server is shutting down; retry the same submission after restart"}}))).into_response(),
        response = tokio::time::timeout(Duration::from_secs(30), next.run(request)) => match response {
            Ok(response) => response,
            Err(_) => (StatusCode::GATEWAY_TIMEOUT, Json(json!({"error":{"message":"Discussion request timed out; retry the same submission to recover a possible completed write"}}))).into_response(),
        },
    };
    (
        [(
            "server-timing",
            format!("app;dur={:.3}", started.elapsed().as_secs_f64() * 1000.0),
        )],
        response,
    )
        .into_response()
}

fn repository<'a>(
    server: &'a Server,
    principal: &Principal,
    key: &(String, String),
) -> Result<&'a Repository> {
    server
        .repositories
        .get(key)
        .filter(|repo| principal.can_read(&repo.config))
        .ok_or(Error::NotFound)
}
fn actor(principal: &Principal) -> Result<Identity> {
    let mut identity = principal.identity().ok_or(Error::Forbidden)?;
    identity.name = identity.name.chars().take(160).collect();
    Ok(identity)
}
fn number(value: u64) -> Result<u64> {
    if value == 0 || value >= storage::MAX_NUMBER {
        return Err(Error::NotFound);
    }
    Ok(value)
}
fn submission(value: &str) -> Result<String> {
    if value.len() != 36
        || !value.bytes().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(Error::Invalid("Submission ID must be a UUID"));
    }
    Ok(value.to_ascii_lowercase())
}
fn title(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 256 || value.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Title must contain 1–256 characters without control characters",
        ));
    }
    Ok(value.to_owned())
}
fn body(value: &str, required: bool) -> Result<()> {
    if value.len() > 65_536 || value.contains('\0') || (required && value.trim().is_empty()) {
        return Err(Error::Invalid(
            "Content must be at most 64 KiB, without NUL characters; comments cannot be empty",
        ));
    }
    Ok(())
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
    let last = storage::last_number(repo, storage::ROOT).await?;
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
    if params.state.is_some() {
        return Err(Error::Invalid("Comments do not have a state filter"));
    }
    let last = storage::last_number(repo, &storage::comments_root(id)).await?;
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
