use std::{sync::Arc, time::Instant};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::{assignees::Selection, labels, profiles::attribution::Authors, reactions},
    app::{self, Error, Result, actor, body, number, repository, title},
    auth::Principal,
    cells::repository::collaboration::Target,
    cells::repository::{
        CommentKey, CommentPage, CreateComment, CreateCommentInput, CreateCommentOutcome,
        CreateIssue, CreateIssueInput, CreateIssueOutcome, DiscussionSummary, GetComment,
        GetIssueDetail, ListComments, ListCommentsInput, ListDiscussions, UpdateComment,
        UpdateCommentInput, UpdateCommentOutcome, UpdateIssue, UpdateIssueInput,
        UpdateIssueOutcome,
    },
    server::Server,
};

pub(super) mod view;

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
    Query(params): Query<super::discussion_search::ListParameters>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &key)?;
    let routed = crate::app::repository_cell(&server, &repo)?;
    let mut input = params.input("is:issue is:open")?;
    super::discussion_search::resolve_people(&server, &principal, &mut input).await?;
    let page = crate::app::query_output(
        routed
            .client
            .query::<ListDiscussions>(&routed.target, None, input)
            .await,
    )?;
    let authors = super::profiles::attribution::Authors::load(
        &server,
        &principal,
        page.items.iter().map(|item| match item {
            DiscussionSummary::Issue(issue) => &issue.author,
            DiscussionSummary::Pull(pull) => &pull.author,
        }),
    )
    .await?;
    let assignees = Selection::load(
        &server,
        &principal,
        &repo,
        page.items.iter().flat_map(|item| match item {
            DiscussionSummary::Issue(issue) => issue.assignees.iter(),
            DiscussionSummary::Pull(pull) => pull.assignees.iter(),
        }),
    )
    .await?;
    let items = page
        .items
        .iter()
        .map(|item| match item {
            DiscussionSummary::Issue(issue) => {
                view::summary(&authors, issue, &page.labels, &assignees)
            }
            DiscussionSummary::Pull(pull) => {
                super::pulls::pull_list_view(&authors, pull, &page.labels, &assignees)
            }
        })
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"items":items,"next":page.next,"counts":page.counts,"matching":page.matching,"navigation":page.navigation,"authored_by_me":page.authored_by_me,"assigned_to_me":page.assigned_to_me,"review_requested_from_me":page.review_requested_from_me}),
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
    let title = title(&input.title)?;
    body(&input.body, false)?;
    let routed = crate::app::repository_cell(&server, &repo)?;
    let output = crate::app::command_output(
        routed
            .client
            .command::<CreateIssue>(
                &routed.target,
                crate::app::mutation_identity()?,
                CreateIssueInput {
                    submission_id: app::submission_id(&input.request_id)?,
                    author: crate::cells::repository::RepositoryAuthor::from(&author),
                    title,
                    body: input.body,
                },
            )
            .await,
    )?;
    let issue = match output {
        CreateIssueOutcome::Archived => return Err(Error::Archived),
        CreateIssueOutcome::Created(issue) => issue,
        CreateIssueOutcome::RequestConflict => return Err(Error::RequestConflict),
    };
    let enrichment_started = Instant::now();
    // A replay returns the issue's current state, which may have acquired labels
    // since creation. Decide from that result, not from the create input.
    let labels = if issue.label_ids.is_empty() {
        vec![]
    } else {
        labels::catalog(&server, &repo).await?
    };
    let reaction = reactions::one(
        &server,
        &repo,
        &author,
        Target::Issue {
            number: issue.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&issue.author)).await?;
    let assignees = Selection::load(&server, &principal, &repo, issue.assignees.iter()).await?;
    let response = (
        StatusCode::CREATED,
        Json(view::issue(
            &authors,
            &issue,
            &author,
            &labels,
            &assignees,
            assignees.principal.can_triage(repo.id),
            &reaction,
        )),
    );
    tracing::debug!(
        target: "crab_server::action",
        event = "application_response_prepared",
        elapsed_us = enrichment_started.elapsed().as_micros(),
    );
    Ok(response)
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DetailParameters {
    read: Option<String>,
    after_incarnation: Option<String>,
    after_sequence: Option<u64>,
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    Query(params): Query<DetailParameters>,
) -> Result<Response> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let author = actor(&principal)?;
    if params.read.as_deref().is_some_and(|mode| mode != "owner")
        || params.after_incarnation.is_some()
        || params.after_sequence.is_some()
    {
        return Err(Error::Invalid("Cellar supports owner-routed issue reads"));
    }
    let routed = crate::app::repository_cell(&server, &repo)?;
    let detail = crate::app::query_output(
        routed
            .client
            .query::<GetIssueDetail>(&routed.target, None, number(id)?)
            .await,
    )?;
    let issue = detail.issue;
    let labels = detail.labels.labels;
    let issue = issue.ok_or(Error::NotFound)?;
    let reaction = reactions::one(
        &server,
        &repo,
        &author,
        Target::Issue {
            number: issue.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&issue.author)).await?;
    let assignees = Selection::load(&server, &principal, &repo, issue.assignees.iter()).await?;
    let response = Json(view::issue(
        &authors,
        &issue,
        &author,
        &labels,
        &assignees,
        assignees.principal.can_triage(repo.id),
        &reaction,
    ))
    .into_response();
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueEdit {
    version: u64,
    title: Option<String>,
    body: Option<String>,
    state: Option<IssueState>,
    label_ids: Option<Vec<u64>>,
    assignee_ids: Option<Vec<uuid::Uuid>>,
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
        && input.assignee_ids.is_none()
    {
        return Err(Error::Invalid("No issue changes supplied"));
    }
    let can_triage = app::membership(&server, &principal, &repo)
        .await?
        .principal
        .can_triage(repo.id);
    if input.label_ids.is_some() && !can_triage {
        return Err(Error::LabelPermission);
    }
    if input.assignee_ids.is_some() && !can_triage {
        return Err(Error::AssigneePermission);
    }
    let labels = if input.label_ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
        labels::catalog(&server, &repo).await?
    } else {
        vec![]
    };
    let title = input.title.as_deref().map(title).transpose()?;
    if let Some(value) = input.body.as_deref() {
        body(value, false)?;
    }
    let label_ids = input
        .label_ids
        .map(|value| labels::validate_selection(value, &labels))
        .transpose()?;
    // Assignment eligibility and caller role must share the final current roster.
    let (can_triage, assignees) = if let Some(ids) = input.assignee_ids {
        let roster = crate::api::members::roster::Roster::load(&server, &principal, &repo).await?;
        if !roster.principal.can_triage(repo.id) {
            return Err(Error::AssigneePermission);
        }
        (true, Some(Selection::validate(&roster, ids)?))
    } else {
        let permissions = app::membership(&server, &principal, &repo).await?;
        (permissions.principal.can_triage(repo.id), None)
    };
    let routed = crate::app::repository_cell(&server, &repo)?;
    let output = crate::app::command_output(
        routed
            .client
            .command::<UpdateIssue>(
                &routed.target,
                crate::app::mutation_identity()?,
                UpdateIssueInput {
                    number: number(id)?,
                    actor: crate::cells::repository::RepositoryAuthor::from(&author),
                    can_triage,
                    version: input.version,
                    title,
                    body: input.body,
                    state: input.state.map(IssueState::code),
                    label_ids,
                    assignees,
                },
            )
            .await,
    )?;
    let issue = match output {
        UpdateIssueOutcome::Archived => return Err(Error::Archived),
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
    // An edit without label changes can return existing selections. Reuse any
    // catalog fetched for validation; otherwise fetch only for actual rendering.
    let labels = if labels.is_empty() && !issue.label_ids.is_empty() {
        labels::catalog(&server, &repo).await?
    } else {
        labels
    };
    let reaction = reactions::one(
        &server,
        &repo,
        &author,
        Target::Issue {
            number: issue.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&issue.author)).await?;
    let assignees = Selection::load(&server, &principal, &repo, issue.assignees.iter()).await?;
    Ok(Json(view::issue(
        &authors,
        &issue,
        &author,
        &labels,
        &assignees,
        assignees.principal.can_triage(repo.id),
        &reaction,
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
    let routed = crate::app::repository_cell(&server, &repo)?;
    let page = crate::app::query_output(
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
    let targets = items
        .iter()
        .map(|comment| Target::IssueComment {
            issue: comment.issue,
            number: comment.number,
        })
        .collect();
    let reactions = reactions::read(&server, &repo, &author, targets).await?;
    let authors = Authors::load(
        &server,
        &principal,
        items.iter().map(|comment| &comment.author),
    )
    .await?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(json!({
        "items": items
            .iter()
            .zip(reactions)
            .map(|(comment, reactions)| view::comment(&authors, comment, &author, &reactions))
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
    let routed = crate::app::repository_cell(&server, &repo)?;
    let output = crate::app::command_output(
        routed
            .client
            .command::<CreateComment>(
                &routed.target,
                crate::app::mutation_identity()?,
                CreateCommentInput {
                    submission_id: app::submission_id(&input.request_id)?,
                    issue: number(id)?,
                    author: crate::cells::repository::RepositoryAuthor::from(&author),
                    body: input.body,
                },
            )
            .await,
    )?;
    let comment = match output {
        CreateCommentOutcome::Archived => return Err(Error::Archived),
        CreateCommentOutcome::Created(comment) => comment,
        CreateCommentOutcome::IssueNotFound => return Err(Error::NotFound),
        CreateCommentOutcome::RequestConflict => return Err(Error::RequestConflict),
    };
    let reaction = reactions::one(
        &server,
        &repo,
        &author,
        Target::IssueComment {
            issue: comment.issue,
            number: comment.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&comment.author)).await?;
    app::membership(&server, &principal, &repo).await?;
    Ok((
        StatusCode::CREATED,
        Json(view::comment(&authors, &comment, &author, &reaction)),
    ))
}

async fn comment_detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, comment)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let author = actor(&principal)?;
    let routed = crate::app::repository_cell(&server, &repo)?;
    let comment = crate::app::query_output(
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
    let reaction = reactions::one(
        &server,
        &repo,
        &author,
        Target::IssueComment {
            issue: comment.issue,
            number: comment.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&comment.author)).await?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(view::comment(&authors, &comment, &author, &reaction)))
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
    let routed = crate::app::repository_cell(&server, &repo)?;
    let output = crate::app::command_output(
        routed
            .client
            .command::<UpdateComment>(
                &routed.target,
                crate::app::mutation_identity()?,
                UpdateCommentInput {
                    key: CommentKey {
                        issue: number(id)?,
                        number: number(comment)?,
                    },
                    actor: crate::cells::repository::RepositoryAuthor::from(&author),
                    version: input.version,
                    body: input.body,
                },
            )
            .await,
    )?;
    let comment = match output {
        UpdateCommentOutcome::Archived => return Err(Error::Archived),
        UpdateCommentOutcome::Updated(comment) => comment,
        UpdateCommentOutcome::NotFound => return Err(Error::NotFound),
        UpdateCommentOutcome::Forbidden => return Err(Error::Forbidden),
        UpdateCommentOutcome::Conflict => return Err(Error::Conflict),
    };
    let reaction = reactions::one(
        &server,
        &repo,
        &author,
        Target::IssueComment {
            issue: comment.issue,
            number: comment.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&comment.author)).await?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(view::comment(&authors, &comment, &author, &reaction)))
}

#[cfg(test)]
mod tests;
