use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use crab_remote_git::{RemoteGitRepository, RepositoryOptions};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    BranchProtection,
    app::{self, Error, Result},
    assignees::{self, Assignee},
    auth::{Identity, Principal},
    checks::{self, CheckRun},
    labels::{self, Label},
    server::{Repository, Server},
    statuses::{self, CommitStatus, StatusState},
};

mod merge;
mod merge_tree;
mod review_thread_storage;
mod review_threads;
mod reviews;
mod storage;
use storage::{NewPullRequest, PullComment, PullRequest, PullState, PullSummary};

const MAX_NUMBER: u64 = 9_007_199_254_740_991;

fn same_author(left: &Identity, right: &Identity) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/pulls", get(list).post(create))
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}",
            get(detail).patch(edit),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/comments",
            get(comments).post(comment),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/comments/{comment}",
            get(comment_detail).patch(edit_comment),
        )
        .merge(reviews::routes())
        .merge(review_threads::routes())
        .merge(merge::routes())
        .layer(axum::extract::DefaultBodyLimit::max(160 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn pull_view(
    pull: &PullRequest,
    actor: &Identity,
    server: &Server,
    repo: &Repository,
    can_write: bool,
    current: Option<&(String, String)>,
) -> Result<Value> {
    let base_oid = pull.merge.as_ref().map_or_else(
        || current.map_or(pull.base_oid.as_str(), |value| value.0.as_str()),
        |merge| merge.base_oid.as_str(),
    );
    let head_oid = pull.merge.as_ref().map_or_else(
        || current.map_or(pull.head_oid.as_str(), |value| value.1.as_str()),
        |merge| merge.head_oid.as_str(),
    );
    let branches_available = pull.merge.is_some() || current.is_some();
    let protections = repo.branch_protections(server, actor).await?;
    let protection = protections.protection(&pull.base_ref);
    let (statuses, check_runs) = match protection {
        Some(rule) if !rule.required_checks.is_empty() => (
            statuses::latest(server, repo, actor, head_oid).await?,
            checks::latest(server, repo, actor, head_oid).await?,
        ),
        _ => (vec![], vec![]),
    };
    let labels = if pull.label_ids.is_empty() {
        vec![]
    } else {
        labels::catalog(server, repo, actor).await?
    };
    let assignees = assignees::available(repo, actor);
    let requirements = merge_requirements(pull, protection, head_oid, &statuses, &check_runs);
    Ok(json!({
        "number": pull.number,
        "title": pull.title,
        "body": pull.body,
        "state": pull.state,
        "author": pull.author.name,
        "base_ref": pull.base_ref,
        "base_oid": base_oid,
        "head_ref": pull.head_ref,
        "head_oid": head_oid,
        "original_base_oid": pull.base_oid,
        "original_head_oid": pull.head_oid,
        "version": pull.version,
        "created_at": pull.created_at,
        "updated_at": pull.updated_at,
        "labels": labels::selection_view(&pull.label_ids, &labels),
        "assignees": assignees::selection_view(&pull.assignee_subjects, &assignees),
        "can_label": can_write,
        "can_assign": can_write,
        "can_edit": same_author(&pull.author, actor),
        "can_manage": pull.state != PullState::Merged
            && pull.merge_pending.is_none()
            && (can_write || same_author(&pull.author, actor)),
        "can_decide": pull.state == PullState::Open
            && current.is_some()
            && !same_author(&pull.author, actor),
        "can_merge": pull.state == PullState::Open
            && can_write
            && (pull.merge_pending.is_some()
                || (current.is_some() && requirements.satisfied)),
        "branches_available": branches_available,
        "merge_requirements": {
            "protected": requirements.protected,
            "required_approvals": requirements.required_approvals,
            "approvals": requirements.approvals,
            "changes_requested": requirements.changes_requested,
            "checks_satisfied": requirements.checks_satisfied,
            "checks": requirements.checks.iter().map(|check| json!({
                "context": check.context,
                "state": check.state,
                "description": check.description,
                "target_url": check.target_url,
                "author": check.author,
                "updated_at": check.updated_at,
                "run_id": check.run_id,
            })).collect::<Vec<_>>(),
            "satisfied": requirements.satisfied,
        },
        "merge": pull.merge.as_ref().map(|merge| json!({
            "author": merge.author.name,
            "method": merge.method,
            "commit_oid": merge.commit_oid,
            "message": merge.message,
            "created_at": merge.created_at,
        })),
        "merge_pending": pull.merge_pending.as_ref().map(|merge| json!({
            "request_id": merge.request_id,
            "author": merge.author.name,
            "method": merge.method,
            "pull_version": merge.pull_version,
            "base_oid": merge.base_oid,
            "head_oid": merge.head_oid,
            "message": merge.message,
            "created_at": merge.created_at,
        })),
    }))
}

struct MergeRequirements {
    protected: bool,
    required_approvals: usize,
    approvals: usize,
    changes_requested: usize,
    checks_satisfied: bool,
    checks: Vec<RequiredCheck>,
    satisfied: bool,
}

struct RequiredCheck {
    context: String,
    state: Option<StatusState>,
    description: Option<String>,
    target_url: Option<String>,
    author: Option<String>,
    updated_at: Option<u64>,
    run_id: Option<u64>,
}

fn merge_requirements(
    pull: &PullRequest,
    rule: Option<&BranchProtection>,
    head_oid: &str,
    statuses: &[CommitStatus],
    check_runs: &[CheckRun],
) -> MergeRequirements {
    let Some(rule) = rule else {
        return MergeRequirements {
            protected: false,
            required_approvals: 0,
            approvals: 0,
            changes_requested: 0,
            checks_satisfied: true,
            checks: vec![],
            satisfied: true,
        };
    };
    let mut approvals = 0;
    let mut changes_requested = 0;
    for decision in &pull.review_decisions {
        if decision.commit_oid != head_oid {
            continue;
        }
        match decision.state {
            storage::ReviewState::Approved => approvals += 1,
            storage::ReviewState::ChangesRequested => changes_requested += 1,
            storage::ReviewState::Commented => {}
        }
    }
    let required_approvals = usize::from(rule.required_approvals);
    let checks = rule
        .required_checks
        .iter()
        .map(|context| {
            let status = statuses
                .iter()
                .find(|status| statuses::same_context(&status.context, context));
            let run = check_runs
                .iter()
                .find(|run| statuses::same_context(&run.name, context));
            if let Some(run) =
                run.filter(|run| status.is_none_or(|status| run.updated_at >= status.created_at))
            {
                return RequiredCheck {
                    context: context.clone(),
                    state: Some(run.requirement_state()),
                    description: Some(run.output_title.clone()),
                    target_url: run.details_url.clone(),
                    author: Some(run.author.name.clone()),
                    updated_at: Some(run.updated_at),
                    run_id: Some(run.number),
                };
            }
            RequiredCheck {
                context: context.clone(),
                state: status.map(|status| status.state),
                description: status.and_then(|status| status.description.clone()),
                target_url: status.and_then(|status| status.target_url.clone()),
                author: status.map(|status| status.author.name.clone()),
                updated_at: status.map(|status| status.created_at),
                run_id: None,
            }
        })
        .collect::<Vec<_>>();
    let checks_satisfied = checks
        .iter()
        .all(|check| check.state == Some(StatusState::Success));
    let reviews_satisfied =
        required_approvals == 0 || (changes_requested == 0 && approvals >= required_approvals);
    MergeRequirements {
        protected: true,
        required_approvals,
        approvals,
        changes_requested,
        checks_satisfied,
        checks,
        satisfied: reviews_satisfied && checks_satisfied,
    }
}

fn pull_list_view(pull: &PullSummary, labels: &[Label], assignees: &[Assignee]) -> Value {
    json!({
        "number": pull.number,
        "title": pull.title,
        "state": pull.state,
        "author": pull.author.name,
        "base_ref": pull.base_ref,
        "head_ref": pull.head_ref,
        "created_at": pull.created_at,
        "updated_at": pull.updated_at,
        "labels": labels::selection_view(&pull.label_ids, labels),
        "assignees": assignees::selection_view(&pull.assignee_subjects, assignees),
    })
}

fn comment_view(comment: &PullComment, actor: &Identity) -> Value {
    json!({
        "number": comment.number,
        "body": comment.body,
        "author": comment.author.name,
        "version": comment.version,
        "created_at": comment.created_at,
        "updated_at": comment.updated_at,
        "can_edit": same_author(&comment.author, actor),
    })
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
        if !(1..=50).contains(&limit)
            || self
                .before
                .is_some_and(|value| value == 0 || value > MAX_NUMBER)
        {
            return Err(Error::Invalid("Invalid pull request page"));
        }
        Ok(limit)
    }

    fn state(&self) -> Result<ListState> {
        match self.state.as_deref().unwrap_or("open") {
            "open" => Ok(ListState::Open),
            "closed" => Ok(ListState::Closed),
            "all" => Ok(ListState::All),
            _ => Err(Error::Invalid(
                "Pull request state must be open, closed or all",
            )),
        }
    }
}

enum ListState {
    Open,
    Closed,
    All,
}

impl ListState {
    const fn code(&self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Closed => 1,
            Self::All => 2,
        }
    }
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let limit =
        u8::try_from(params.limit()?).map_err(|_| Error::Invalid("Invalid pull request page"))?;
    let state = params.state()?.code();
    let query = app::search_query(params.q.as_deref())?;
    let assignees = assignees::available(repo, &actor);
    let (pulls, next) =
        storage::list_pulls(&server, repo, &actor, params.before, limit, state, query).await?;
    let labels = if pulls.iter().all(|pull| pull.label_ids.is_empty()) {
        vec![]
    } else {
        labels::catalog(&server, repo, &actor).await?
    };
    let items = pulls
        .iter()
        .map(|pull| pull_list_view(pull, &labels, &assignees))
        .collect::<Vec<_>>();
    Ok(Json(json!({"items":items,"next":next})))
}

fn resolve_branch(repository: &RemoteGitRepository, name: &str) -> Result<String> {
    if !name.starts_with("refs/heads/") || name == "refs/heads/" {
        return Err(Error::Invalid("Pull requests require branch refs"));
    }
    repository
        .refs()
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.target.to_string())
        .ok_or(Error::Invalid("Base or head branch is unavailable"))
}

fn current_branches(
    repository: &RemoteGitRepository,
    pull: &PullRequest,
) -> Option<(String, String)> {
    let base = repository
        .refs()
        .entries
        .iter()
        .find(|entry| entry.name == pull.base_ref)?;
    let head = repository
        .refs()
        .entries
        .iter()
        .find(|entry| entry.name == pull.head_ref)?;
    Some((base.target.to_string(), head.target.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewPull {
    request_id: String,
    title: String,
    body: String,
    base_ref: String,
    head_ref: String,
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<NewPull>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let title = app::title(&input.title)?;
    app::body(&input.body, false)?;
    let request_id = app::submission(&input.request_id)?;
    if input.base_ref == input.head_ref {
        return Err(Error::Invalid("Base and head branches must differ"));
    }
    if let Some(pull) = storage::pull_submission(&server, repo, &actor, &request_id).await? {
        if !same_author(&pull.author, &actor)
            || pull.title != title
            || pull.body != input.body
            || pull.base_ref != input.base_ref
            || pull.head_ref != input.head_ref
        {
            return Err(Error::RequestConflict);
        }
        let repository = repo
            .open_current(&server, RepositoryOptions::default(), &server.cancellation)
            .await
            .ok();
        let current = repository
            .as_ref()
            .and_then(|repository| current_branches(repository, &pull));
        return Ok((
            StatusCode::CREATED,
            Json(
                pull_view(
                    &pull,
                    &actor,
                    &server,
                    repo,
                    principal.can_write(&repo.config),
                    current.as_ref(),
                )
                .await?,
            ),
        ));
    }
    let repository = repo
        .open_current(&server, RepositoryOptions::default(), &server.cancellation)
        .await?;
    let base_oid = resolve_branch(&repository, &input.base_ref)?;
    let head_oid = resolve_branch(&repository, &input.head_ref)?;
    if base_oid == head_oid {
        return Err(Error::Invalid("Head branch has no commits to compare"));
    }
    let pull = storage::create_pull(
        &server,
        repo,
        NewPullRequest {
            author: actor.clone(),
            request_id,
            title,
            body: input.body,
            base_ref: input.base_ref,
            base_oid,
            head_ref: input.head_ref,
            head_oid,
        },
    )
    .await?;
    let current = current_branches(&repository, &pull);
    Ok((
        StatusCode::CREATED,
        Json(
            pull_view(
                &pull,
                &actor,
                &server,
                repo,
                principal.can_write(&repo.config),
                current.as_ref(),
            )
            .await?,
        ),
    ))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let pull = storage::pull(&server, repo, &actor, app::number(id)?)
        .await?
        .ok_or(Error::NotFound)?;
    let repository = repo
        .open_current(&server, RepositoryOptions::default(), &server.cancellation)
        .await
        .ok();
    let current = repository
        .as_ref()
        .and_then(|repository| current_branches(repository, &pull));
    Ok(Json(
        pull_view(
            &pull,
            &actor,
            &server,
            repo,
            principal.can_write(&repo.config),
            current.as_ref(),
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PullEdit {
    version: u64,
    title: Option<String>,
    body: Option<String>,
    state: Option<PullState>,
    label_ids: Option<Vec<u64>>,
    assignees: Option<Vec<String>>,
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    input: std::result::Result<Json<PullEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let number = app::number(id)?;
    let pull = storage::pull(&server, repo, &actor, number)
        .await?
        .ok_or(Error::NotFound)?;
    let label_change = input.label_ids.is_some();
    let needs_label_catalog = input
        .label_ids
        .as_ref()
        .is_some_and(|labels| !labels.is_empty())
        || (input.label_ids.is_none() && !pull.label_ids.is_empty());
    let assignee_change = input.assignees.is_some();
    let author = same_author(&pull.author, &actor);
    if pull.merge_pending.is_some() {
        return Err(Error::MergePending);
    }
    if (input.title.is_some() || input.body.is_some()) && !author {
        return Err(Error::Forbidden);
    }
    if input.state.is_some() && !(author || principal.can_write(&repo.config)) {
        return Err(Error::Forbidden);
    }
    if label_change && !principal.can_write(&repo.config) {
        return Err(Error::LabelPermission);
    }
    if assignee_change && !principal.can_write(&repo.config) {
        return Err(Error::AssigneePermission);
    }
    if input.state == Some(PullState::Merged) || (pull.merge.is_some() && input.state.is_some()) {
        return Err(Error::Invalid("Merged pull requests cannot change state"));
    }
    if input.version != pull.version {
        return Err(Error::Conflict);
    }
    if input.title.is_none()
        && input.body.is_none()
        && input.state.is_none()
        && input.label_ids.is_none()
        && input.assignees.is_none()
    {
        return Err(Error::Invalid("No pull request changes supplied"));
    }
    let labels = if needs_label_catalog {
        labels::catalog(&server, repo, &actor).await?
    } else {
        vec![]
    };
    let assignees = assignees::available(repo, &actor);
    let title = input.title.map(|value| app::title(&value)).transpose()?;
    let body = input
        .body
        .map(|value| {
            app::body(&value, false)?;
            Ok::<_, Error>(value)
        })
        .transpose()?;
    let label_ids = input
        .label_ids
        .map(|value| labels::validate_selection(value, &labels))
        .transpose()?;
    let assignee_subjects = input
        .assignees
        .map(|value| assignees::validate_selection(value, &assignees))
        .transpose()?;
    if label_change && !principal.can_write(&repo.config) {
        return Err(Error::LabelPermission);
    }
    if assignee_change && !principal.can_write(&repo.config) {
        return Err(Error::AssigneePermission);
    }
    let pull = storage::update_pull(
        &server,
        repo,
        number,
        storage::PullEdit {
            actor: actor.clone(),
            can_manage: principal.can_write(&repo.config),
            version: input.version,
            title,
            body,
            state: input.state,
            label_ids,
            assignee_subjects,
        },
    )
    .await?;
    let repository = repo
        .open_current(&server, RepositoryOptions::default(), &server.cancellation)
        .await
        .ok();
    let current = repository
        .as_ref()
        .and_then(|repository| current_branches(repository, &pull));
    Ok(Json(
        pull_view(
            &pull,
            &actor,
            &server,
            repo,
            principal.can_write(&repo.config),
            current.as_ref(),
        )
        .await?,
    ))
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
    let (comments, next) = storage::comments(&server, repo, &actor, id, params.before, limit)
        .await?
        .ok_or(Error::NotFound)?;
    let items = comments
        .iter()
        .map(|comment| comment_view(comment, &actor))
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
    Ok((StatusCode::CREATED, Json(comment_view(&comment, &actor))))
}

async fn comment_detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, comment)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let comment = storage::comment(
        &server,
        repo,
        &actor,
        app::number(id)?,
        app::number(comment)?,
    )
    .await?
    .ok_or(Error::NotFound)?;
    Ok(Json(comment_view(&comment, &actor)))
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
    Ok(Json(comment_view(&comment, &actor)))
}
