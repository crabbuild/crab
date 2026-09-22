use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use cellule_runtime::{Committed, InvocationError, MutationIdentity, Observed, RequestId};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{Error, Result, actor, body, number, repository, submission, title},
    assignees::{self, Assignee},
    auth::{Identity, Principal},
    cells::{
        RepositoryCell,
        repository::{
            CommentKey, CommentPage, CommentRecord, CreateComment, CreateCommentInput,
            CreateCommentOutcome, CreateIssue, CreateIssueInput, CreateIssueOutcome, GetComment,
            GetIssue, IssueRecord, IssueSummary, ListComments, ListCommentsInput, ListIssues,
            ListIssuesInput, RepositoryAuthor, UpdateComment, UpdateCommentInput,
            UpdateCommentOutcome, UpdateIssue, UpdateIssueInput, UpdateIssueOutcome,
        },
    },
    labels::{self, Label},
    server::{Repository, Server},
};

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

fn issue_view(
    issue: &IssueRecord,
    author: &Identity,
    labels: &[Label],
    assignees: &[Assignee],
    can_manage_metadata: bool,
    full: bool,
) -> Value {
    json!({
        "number": issue.number,
        "title": issue.title,
        "body": full.then_some(&issue.body),
        "state": state_name(issue.state),
        "author": issue.author.name,
        "version": issue.version,
        "created_at": issue.created_at_ms,
        "updated_at": issue.updated_at_ms,
        "labels": labels::selection_view(&issue.label_ids, labels),
        "assignees": assignees::selection_view(&issue.assignee_subjects, assignees),
        "can_edit": same_author(&issue.author, author),
        "can_label": can_manage_metadata,
        "can_assign": can_manage_metadata,
    })
}

fn issue_summary_view(
    issue: &IssueSummary,
    author: &Identity,
    labels: &[Label],
    assignees: &[Assignee],
    can_manage_metadata: bool,
) -> Value {
    json!({
        "number": issue.number,
        "title": issue.title,
        "body": Value::Null,
        "state": state_name(issue.state),
        "author": issue.author.name,
        "version": issue.version,
        "created_at": issue.created_at_ms,
        "updated_at": issue.updated_at_ms,
        "labels": labels::selection_view(&issue.label_ids, labels),
        "assignees": assignees::selection_view(&issue.assignee_subjects, assignees),
        "can_edit": same_author(&issue.author, author),
        "can_label": can_manage_metadata,
        "can_assign": can_manage_metadata,
    })
}

fn comment_view(comment: &CommentRecord, author: &Identity) -> Value {
    json!({
        "number": comment.number,
        "body": comment.body,
        "author": comment.author.name,
        "version": comment.version,
        "created_at": comment.created_at_ms,
        "updated_at": comment.updated_at_ms,
        "can_edit": same_author(&comment.author, author),
    })
}

fn state_name(state: u8) -> &'static str {
    match state {
        0 => "open",
        1 => "closed",
        _ => "invalid",
    }
}

fn same_author(left: &RepositoryAuthor, right: &Identity) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

fn repository_author(identity: &Identity) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: identity.issuer.clone(),
        subject: identity.subject.clone(),
        name: identity.name.clone(),
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum IssueState {
    Open,
    Closed,
}

impl IssueState {
    const fn code(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Closed => 1,
        }
    }
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
    fn limit(&self) -> Result<u8> {
        let limit = self.limit.unwrap_or(30);
        u8::try_from(limit)
            .ok()
            .filter(|limit| (1..=50).contains(limit))
            .ok_or(Error::Invalid("Page size must be 1–50"))
    }

    fn state(&self) -> Result<u8> {
        match self.state.as_deref().unwrap_or("open") {
            "open" => Ok(0),
            "closed" => Ok(1),
            "all" => Ok(2),
            _ => Err(Error::Invalid("Issue state must be open, closed or all")),
        }
    }

    fn before(&self) -> Result<Option<u64>> {
        if self
            .before
            .is_some_and(|value| value == 0 || value > crate::app::MAX_NUMBER)
        {
            return Err(Error::Invalid("Invalid page cursor"));
        }
        Ok(self.before)
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
    let labels = labels::catalog(&server, &repo, &author).await?;
    let assignees = assignees::available(&repo, &author);
    let can_manage_metadata = principal.can_write(&repo.config);
    let routed = route(&server, &repo, &author, "repository.read").await?;
    let page = query_output(
        routed
            .client
            .query::<ListIssues>(
                &routed.target,
                None,
                ListIssuesInput {
                    before: params.before()?,
                    limit: params.limit()?,
                    state: params.state()?,
                    query: crate::app::search_query(params.q.as_deref())?,
                },
            )
            .await,
    )?;
    let items = page
        .items
        .iter()
        .map(|issue| issue_summary_view(issue, &author, &labels, &assignees, can_manage_metadata))
        .collect::<Vec<_>>();
    Ok(Json(json!({"items":items,"next":page.next})))
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
    let title = title(&input.title)?;
    body(&input.body, false)?;
    let routed = route(&server, &repo, &author, "repository.issue.create").await?;
    let output = command_output(
        routed
            .client
            .command::<CreateIssue>(
                &routed.target,
                mutation_identity()?,
                CreateIssueInput {
                    submission_id: submission_id(&input.request_id)?,
                    author: repository_author(&author),
                    title,
                    body: input.body,
                },
            )
            .await,
    )?;
    let issue = match output {
        CreateIssueOutcome::Created(issue) => issue,
        CreateIssueOutcome::RequestConflict => return Err(Error::RequestConflict),
    };
    let labels = labels::catalog(&server, &repo, &author).await?;
    let assignees = assignees::available(&repo, &author);
    Ok((
        StatusCode::CREATED,
        Json(issue_view(
            &issue,
            &author,
            &labels,
            &assignees,
            principal.can_write(&repo.config),
            true,
        )),
    ))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let author = actor(&principal)?;
    let routed = route(&server, &repo, &author, "repository.read").await?;
    let issue = query_output(
        routed
            .client
            .query::<GetIssue>(&routed.target, None, number(id)?)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let labels = labels::catalog(&server, &repo, &author).await?;
    let assignees = assignees::available(&repo, &author);
    Ok(Json(issue_view(
        &issue,
        &author,
        &labels,
        &assignees,
        principal.can_write(&repo.config),
        true,
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueEdit {
    version: u64,
    title: Option<String>,
    body: Option<String>,
    state: Option<IssueState>,
    label_ids: Option<Vec<u64>>,
    assignees: Option<Vec<String>>,
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
    if input.title.is_none()
        && input.body.is_none()
        && input.state.is_none()
        && input.label_ids.is_none()
        && input.assignees.is_none()
    {
        return Err(Error::Invalid("No issue changes supplied"));
    }
    let can_manage_metadata = principal.can_write(&repo.config);
    if input.label_ids.is_some() && !can_manage_metadata {
        return Err(Error::LabelPermission);
    }
    if input.assignees.is_some() && !can_manage_metadata {
        return Err(Error::AssigneePermission);
    }
    let labels = labels::catalog(&server, &repo, &author).await?;
    let assignees = assignees::available(&repo, &author);
    let title = input.title.as_deref().map(title).transpose()?;
    if let Some(value) = input.body.as_deref() {
        body(value, false)?;
    }
    let label_ids = input
        .label_ids
        .map(|value| labels::validate_selection(value, &labels))
        .transpose()?;
    let assignee_subjects = input
        .assignees
        .map(|value| assignees::validate_selection(value, &assignees))
        .transpose()?;
    let routed = route(&server, &repo, &author, "repository.issue.update").await?;
    let output = command_output(
        routed
            .client
            .command::<UpdateIssue>(
                &routed.target,
                mutation_identity()?,
                UpdateIssueInput {
                    number: number(id)?,
                    actor: repository_author(&author),
                    can_manage_metadata,
                    version: input.version,
                    title,
                    body: input.body,
                    state: input.state.map(IssueState::code),
                    label_ids,
                    assignee_subjects,
                },
            )
            .await,
    )?;
    let issue = match output {
        UpdateIssueOutcome::Updated(issue) => issue,
        UpdateIssueOutcome::NotFound => return Err(Error::NotFound),
        UpdateIssueOutcome::Forbidden => return Err(Error::Forbidden),
        UpdateIssueOutcome::LabelForbidden => return Err(Error::LabelPermission),
        UpdateIssueOutcome::LabelInvalid => {
            return Err(Error::Invalid(
                "Label selection contains an unknown or duplicate label",
            ));
        }
        UpdateIssueOutcome::AssigneeForbidden => return Err(Error::AssigneePermission),
        UpdateIssueOutcome::Conflict => return Err(Error::Conflict),
    };
    Ok(Json(issue_view(
        &issue,
        &author,
        &labels,
        &assignees,
        can_manage_metadata,
        true,
    )))
}

async fn comments(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let author = actor(&principal)?;
    if params.state.is_some() || params.q.is_some() {
        return Err(Error::Invalid("Comments do not support list filters"));
    }
    let routed = route(&server, &repo, &author, "repository.read").await?;
    let page = query_output(
        routed
            .client
            .query::<ListComments>(
                &routed.target,
                None,
                ListCommentsInput {
                    issue: number(id)?,
                    before: params.before()?,
                    limit: params.limit()?,
                },
            )
            .await,
    )?;
    let CommentPage::Found { items, next } = page else {
        return Err(Error::NotFound);
    };
    Ok(Json(json!({
        "items": items
            .iter()
            .map(|comment| comment_view(comment, &author))
            .collect::<Vec<_>>(),
        "next": next,
    })))
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
    let Json(input) = input?;
    let author = actor(&principal)?;
    body(&input.body, true)?;
    let routed = route(&server, &repo, &author, "repository.comment.create").await?;
    let output = command_output(
        routed
            .client
            .command::<CreateComment>(
                &routed.target,
                mutation_identity()?,
                CreateCommentInput {
                    submission_id: submission_id(&input.request_id)?,
                    issue: number(id)?,
                    author: repository_author(&author),
                    body: input.body,
                },
            )
            .await,
    )?;
    let comment = match output {
        CreateCommentOutcome::Created(comment) => comment,
        CreateCommentOutcome::IssueNotFound => return Err(Error::NotFound),
        CreateCommentOutcome::RequestConflict => return Err(Error::RequestConflict),
    };
    Ok((StatusCode::CREATED, Json(comment_view(&comment, &author))))
}

async fn comment_detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, comment)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let author = actor(&principal)?;
    let routed = route(&server, &repo, &author, "repository.read").await?;
    let comment = query_output(
        routed
            .client
            .query::<GetComment>(
                &routed.target,
                None,
                CommentKey {
                    issue: number(id)?,
                    number: number(comment)?,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    Ok(Json(comment_view(&comment, &author)))
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
    body(&input.body, true)?;
    let routed = route(&server, &repo, &author, "repository.comment.update").await?;
    let output = command_output(
        routed
            .client
            .command::<UpdateComment>(
                &routed.target,
                mutation_identity()?,
                UpdateCommentInput {
                    key: CommentKey {
                        issue: number(id)?,
                        number: number(comment)?,
                    },
                    actor: repository_author(&author),
                    version: input.version,
                    body: input.body,
                },
            )
            .await,
    )?;
    let comment = match output {
        UpdateCommentOutcome::Updated(comment) => comment,
        UpdateCommentOutcome::NotFound => return Err(Error::NotFound),
        UpdateCommentOutcome::Forbidden => return Err(Error::Forbidden),
        UpdateCommentOutcome::Conflict => return Err(Error::Conflict),
    };
    Ok(Json(comment_view(&comment, &author)))
}

async fn route(
    server: &Server,
    repository: &Repository,
    principal: &Identity,
    action: &'static str,
) -> Result<RepositoryCell> {
    let router = server.repository_cells().ok_or(Error::CellUnavailable)?;
    router
        .route(repository.id, principal, action)
        .await
        .map_err(|error| match error {
            crate::Error::Cell(source) => Error::Cell(source),
            source => Error::Repository(source),
        })
}

fn mutation_identity() -> Result<MutationIdentity> {
    let now_ms = crate::cells::unix_now_ms().map_err(Error::Repository)?;
    let expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(Error::CellContract("Cell request expiry overflowed"))?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

fn submission_id(value: &str) -> Result<[u8; 16]> {
    Uuid::parse_str(&submission(value)?)
        .map(Uuid::into_bytes)
        .map_err(|_| Error::Invalid("Submission ID must be a UUID"))
}

fn command_output<T>(result: std::result::Result<Committed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(committed) => Ok(committed.output),
        Err(InvocationError::Rejected(committed)) => Ok(committed.output),
        Err(InvocationError::Pending(_)) => Err(Error::CellPending),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}

fn query_output<T>(result: std::result::Result<Observed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(observed) => Ok(observed.output),
        Err(InvocationError::Rejected(_)) => Err(Error::CellContract(
            "Cell query returned a durable rejection",
        )),
        Err(InvocationError::Pending(_)) => Err(Error::CellContract(
            "Cell query returned pending mutation evidence",
        )),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}
