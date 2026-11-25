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
    api::{
        assignees::Selection, labels, organizations::issue_types, profiles::attribution::Authors,
        reactions,
    },
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

mod move_issue;
mod pins;
mod templates;
pub(in crate::api) mod transferred;
pub(super) mod view;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/issues/templates",
            get(templates::list),
        )
        .route(
            "/api/repos/{owner}/{name}/issues/pins",
            get(pins::list).put(pins::set),
        )
        .route("/api/repos/{owner}/{name}/issues", get(list).post(create))
        .route(
            "/api/repos/{owner}/{name}/issues/{number}",
            get(detail).patch(edit),
        )
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/transfer",
            get(move_issue::destinations).post(move_issue::submit),
        )
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/type",
            axum::routing::put(issue_types::assign),
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
    let review_teams =
        super::discussion_search::resolve_people(&server, &principal, &repo, &mut input).await?;
    let type_scope =
        super::discussion_search::issue_types::bind(&server, &repo, &mut input).await?;
    let pull_heads = input.pull_heads.clone();
    let page = crate::app::query_output(
        routed
            .client
            .query::<ListDiscussions>(&routed.target, None, input)
            .await,
    )?;
    let rollups = if principal.is_public_reader(repo.id) {
        Default::default()
    } else {
        super::pulls::rollups::load(
            &server,
            &repo,
            pull_heads.as_ref(),
            page.check_revision,
            page.items.iter().filter_map(|item| match item {
                DiscussionSummary::Pull(pull) => Some(pull),
                DiscussionSummary::Issue(_) => None,
            }),
        )
        .await?
    };
    let authors = super::profiles::attribution::Authors::load(
        &server,
        &principal,
        page.items.iter().map(|item| match item {
            DiscussionSummary::Issue(issue) => &issue.author,
            DiscussionSummary::Pull(pull) => &pull.author,
        }),
    )
    .await?;
    let unfiltered_types;
    let types =
        if let Some(scope) = &type_scope {
            &scope.definitions[..]
        } else if page.items.iter().any(
            |item| matches!(item, DiscussionSummary::Issue(issue) if issue.issue_type.is_some()),
        ) {
            unfiltered_types = issue_types::active(&server, repo.organization).await?;
            &unfiltered_types.definitions[..]
        } else {
            &[]
        };
    super::discussion_search::issue_types::verify(&server, &repo, type_scope.as_ref()).await?;
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
    // Match counts can disclose check and fork data even when rows are redacted.
    if assignees.principal.is_public_reader(repo.id) != principal.is_public_reader(repo.id) {
        return Err(Error::PageChanged);
    }
    let items = page
        .items
        .iter()
        .map(|item| match item {
            DiscussionSummary::Issue(issue) => Ok(view::summary(
                &authors,
                issue,
                &page.labels,
                &assignees,
                issue_types::visible(&repo, issue.issue_type, types),
            )),
            DiscussionSummary::Pull(pull) => super::pulls::pull_list_view(
                &authors,
                pull,
                &page.labels,
                &assignees,
                &rollups,
                assignees.principal.is_public_reader(repo.id),
            ),
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(bound) = review_teams {
        bound.verify(&server, &principal, &repo).await?;
    }
    Ok(Json(
        json!({"repository_id":repo.id,"can_bulk_edit":assignees.principal.can_triage(repo.id),"items":items,"next":page.next,"counts":page.counts,"matching":page.matching,"navigation":page.navigation,"lock_filter":page.lock_filter,"authored_by_me":page.authored_by_me,"assigned_to_me":page.assigned_to_me,"review_requested_from_me":page.review_requested_from_me}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewIssue {
    #[serde(default)]
    metadata: crate::cells::repository::collaboration::creation::Selection,
    request_id: String,
    title: String,
    body: String,
    template_grant: Option<templates::Grant>,
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<NewIssue>, JsonRejection>,
) -> Result<Response> {
    let repo = repository(&server, &principal, &key)?;
    let Json(input) = input?;
    let author = actor(&principal)?;
    let title = title(&input.title)?;
    body(&input.body, false)?;
    let mut metadata =
        super::discussion_creation::resolve(&server, &principal, &repo, input.metadata).await?;
    if !metadata.can_triage
        && let Some(grant) = input.template_grant
    {
        // A signed template may supply only its exact default labels and
        // assignees. The native creation transaction still checks eligibility.
        metadata.can_triage = grant
            .allows(&server, &repo, &principal, &metadata.selection)
            .await?;
    }
    let routed = app::repository_cell(&server, &repo)?;
    let committed = routed
        .client
        .command::<CreateIssue>(
            &routed.target,
            crate::app::mutation_identity()?,
            CreateIssueInput {
                metadata,
                submission_id: app::submission_id(&input.request_id)?,
                author: crate::cells::repository::RepositoryAuthor::from(&author),
                title,
                body: input.body,
            },
        )
        .await
        .map_err(Error::Cell)?;
    let minimum = Some(committed.receipt);
    let output = committed.output;
    let number = match output {
        CreateIssueOutcome::MetadataRejected(reason) => {
            return Ok(super::write::rejected(Error::CreationRejected(reason)));
        }
        CreateIssueOutcome::Archived => return Err(Error::Archived),
        CreateIssueOutcome::Created(issue) => issue.number,
        CreateIssueOutcome::Transferred(source) => {
            return Ok((
                StatusCode::OK,
                Json(json!({"number": source, "transferred": true})),
            )
                .into_response());
        }
        CreateIssueOutcome::RequestConflict => return Err(Error::RequestConflict),
    };
    let enrichment_started = Instant::now();
    let response = (
        StatusCode::CREATED,
        rendered(&server, &principal, &repo, number, minimum).await?,
    );
    tracing::debug!(
        target: "crab_server::action",
        event = "application_response_prepared",
        elapsed_us = enrichment_started.elapsed().as_micros(),
    );
    Ok(response.into_response())
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
    if params.read.as_deref().is_some_and(|mode| mode != "owner")
        || params.after_incarnation.is_some()
        || params.after_sequence.is_some()
    {
        return Err(Error::Invalid("Cellar supports owner-routed issue reads"));
    }
    let number = number(id)?;
    match rendered(&server, &principal, &repo, number, None).await {
        Err(Error::NotFound) => Ok(transferred::detail(&server, &principal, &repo, number)
            .await?
            .into_response()),
        result => Ok(result?.into_response()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueEdit {
    version: u64,
    title: Option<String>,
    body: Option<String>,
    state: Option<IssueState>,
    state_reason: Option<crate::cells::repository::issues::lifecycle::Reason>,
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
        && input.state_reason.is_none()
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
    let (editor, assignees) = if let Some(ids) = input.assignee_ids {
        let roster = crate::api::members::roster::Roster::load(&server, &principal, &repo).await?;
        if !roster.membership.principal.can_triage(repo.id) {
            return Err(Error::AssigneePermission);
        }
        let assignees = Selection::validate(&roster, ids)?;
        (roster.membership.principal, Some(assignees))
    } else {
        let permissions = app::membership(&server, &principal, &repo).await?;
        (permissions.principal, None)
    };
    let routed = app::repository_cell(&server, &repo)?;
    let committed = routed
        .client
        .command::<UpdateIssue>(
            &routed.target,
            crate::app::mutation_identity()?,
            UpdateIssueInput {
                state_reason: input.state_reason,
                number: number(id)?,
                actor: crate::cells::repository::RepositoryAuthor::from(&author),
                can_triage: editor.can_triage(repo.id),
                can_moderate: editor.can_moderate(repo.id),
                version: input.version,
                title,
                body: input.body,
                state: input.state.map(IssueState::code),
                label_ids,
                assignees,
            },
        )
        .await
        .map_err(Error::Cell)?;
    let minimum = Some(committed.receipt);
    let output = committed.output;
    let issue = match output {
        UpdateIssueOutcome::InvalidStateReason => {
            return Err(Error::Invalid(
                "Close reasons require a closed issue; reopening clears the close reason",
            ));
        }
        UpdateIssueOutcome::Archived => return Err(Error::Archived),
        UpdateIssueOutcome::Updated(issue) => issue,
        UpdateIssueOutcome::NotFound => return Err(Error::NotFound),
        UpdateIssueOutcome::Forbidden => return Err(Error::IssueEditPermission),
        UpdateIssueOutcome::LabelForbidden => return Err(Error::LabelPermission),
        UpdateIssueOutcome::LabelInvalid => {
            return Err(Error::Invalid(
                "Label selection contains an unknown or duplicate label",
            ));
        }
        UpdateIssueOutcome::AssigneeForbidden => return Err(Error::AssigneePermission),
        UpdateIssueOutcome::Conflict => return Err(Error::Conflict),
    };
    rendered(&server, &principal, &repo, issue.number, minimum).await
}

async fn rendered(
    server: &Server,
    principal: &Principal,
    repo: &crate::repository::Repository,
    id: u64,
    minimum: Option<crate::cells::Receipt>,
) -> Result<Json<Value>> {
    let author = principal.member_identity(repo.id);
    let routed = app::repository_cell(server, repo)?;
    // Mutation receipts keep their original record bytes. Read lifecycle and
    // content together after the commit so replay/enrichment cannot mix versions.
    let detail = app::query_output(
        routed
            .client
            .query::<GetIssueDetail>(&routed.target, minimum, id)
            .await,
    )?;
    let issue = detail.issue.as_ref().ok_or(Error::NotFound)?;
    let reaction =
        reactions::one(server, repo, author.as_ref(), Target::Issue { number: id }).await?;
    let authors = Authors::load(server, principal, std::iter::once(&issue.author)).await?;
    let types = if principal.can_triage(repo.id) || detail.issue_type.selection.is_some() {
        issue_types::active(server, repo.organization)
            .await?
            .definitions
    } else {
        Vec::new()
    };
    let assignees = Selection::load(server, principal, repo, issue.assignees.iter()).await?;
    let mut issue = view::issue(
        &authors,
        issue,
        &detail,
        author.as_ref(),
        &assignees,
        repo.id,
        &reaction,
    )?;
    issue["issue_type"] = json!(issue_types::visible(
        repo,
        detail.issue_type.selection,
        &types
    ));
    if repo.organization.is_some() && assignees.principal.can_triage(repo.id) {
        issue["issue_types"] = json!(types);
        issue["issue_type_version"] = json!(detail.issue_type.version);
    }
    Ok(Json(issue))
}

async fn comments(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let author = principal.member_identity(repo.id);
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
    let reactions = reactions::read(&server, &repo, author.as_ref(), targets).await?;
    let authors = Authors::load(
        &server,
        &principal,
        items.iter().map(|comment| &comment.author),
    )
    .await?;
    let editor = app::membership(&server, &principal, &repo).await?;
    Ok(Json(json!({
        "items": items
            .iter()
            .zip(reactions)
            .map(|(comment, reactions)| view::comment(&authors, comment, author.as_ref(), editor.principal.can_moderate(repo.id), &reactions))
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
    let membership = app::membership(&server, &principal, &repo).await?;
    let routed = app::repository_cell(&server, &repo)?;
    let output = crate::app::command_output(
        routed
            .client
            .command::<CreateComment>(
                &routed.target,
                crate::app::mutation_identity()?,
                CreateCommentInput {
                    can_post_locked: membership.principal.can_moderate(repo.id),
                    can_mark_duplicate: membership.principal.can_write(repo.id),
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
        CreateCommentOutcome::Locked => return Err(Error::ConversationLocked),
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
        Json(view::comment(&authors, &comment, &author, false, &reaction)),
    ))
}

async fn comment_detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, comment)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &(owner, name))?;
    let author = principal.member_identity(repo.id);
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
        author.as_ref(),
        Target::IssueComment {
            issue: comment.issue,
            number: comment.number,
        },
    )
    .await?;
    let authors = Authors::load(&server, &principal, std::iter::once(&comment.author)).await?;
    let editor = app::membership(&server, &principal, &repo).await?;
    Ok(Json(view::comment(
        &authors,
        &comment,
        author.as_ref(),
        editor.principal.can_moderate(repo.id),
        &reaction,
    )))
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
    let editor = app::membership(&server, &principal, &repo).await?;
    let routed = app::repository_cell(&server, &repo)?;
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
                    can_moderate: editor.principal.can_moderate(repo.id),
                    version: input.version,
                    body: input.body,
                },
            )
            .await,
    )?;
    let comment = match output {
        UpdateCommentOutcome::Archived => return Err(Error::Archived),
        UpdateCommentOutcome::Locked => return Err(Error::ConversationLocked),
        UpdateCommentOutcome::Updated(comment) => comment,
        UpdateCommentOutcome::NotFound => return Err(Error::NotFound),
        UpdateCommentOutcome::Forbidden => return Err(Error::IssueEditPermission),
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
    let editor = app::membership(&server, &principal, &repo).await?;
    Ok(Json(view::comment(
        &authors,
        &comment,
        &author,
        editor.principal.can_moderate(repo.id),
        &reaction,
    )))
}

#[cfg(test)]
mod tests;
