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
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    RepositoryConfig,
    app::{self, Error, Result},
    app_storage,
    assignees::{self, Assignee},
    auth::{Identity, Principal},
    checks::{self, CheckRun},
    labels::{self, Label},
    server::{Repository, Server},
    statuses::{self, CommitStatus, StatusState},
};

mod merge;
mod merge_tree;
mod reviews;
mod storage;
use storage::{NewPullRequest, PullComment, PullRequest, PullState};

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
        .merge(merge::routes())
        .layer(axum::extract::DefaultBodyLimit::max(80 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn pull_view(
    pull: &PullRequest,
    actor: &Identity,
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
    let (statuses, check_runs) = match repo.config.protection(&pull.base_ref) {
        Some(rule) if !rule.required_checks.is_empty() => (
            statuses::latest(repo, head_oid).await?,
            checks::latest(repo, head_oid).await?,
        ),
        _ => (vec![], vec![]),
    };
    let labels = labels::catalog(repo).await?;
    let assignees = assignees::available(repo, actor);
    let requirements = merge_requirements(pull, &repo.config, head_oid, &statuses, &check_runs);
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
        "can_edit": app_storage::same_author(&pull.author, actor),
        "can_manage": pull.state != PullState::Merged
            && pull.merge_pending.is_none()
            && (can_write || app_storage::same_author(&pull.author, actor)),
        "can_decide": pull.state == PullState::Open
            && current.is_some()
            && !app_storage::same_author(&pull.author, actor),
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
    config: &RepositoryConfig,
    head_oid: &str,
    statuses: &[CommitStatus],
    check_runs: &[CheckRun],
) -> MergeRequirements {
    let Some(rule) = config.protection(&pull.base_ref) else {
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

fn pull_list_view(pull: &PullRequest, labels: &[Label], assignees: &[Assignee]) -> Value {
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
        "can_edit": app_storage::same_author(&comment.author, actor),
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
                .is_some_and(|value| value == 0 || value > app_storage::MAX_NUMBER)
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
    fn matches(&self, state: PullState) -> bool {
        match self {
            Self::Open => state == PullState::Open,
            Self::Closed => state != PullState::Open,
            Self::All => true,
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
    app::actor(&principal)?;
    let limit = params.limit()?;
    let state = params.state()?;
    let query = app::search_query(params.q.as_deref())?;
    let labels = labels::catalog(repo).await?;
    let assignees = assignees::available(repo, &app::actor(&principal)?);
    let last = app_storage::last_number(repo, storage::ROOT).await?;
    let mut next = last.min(params.before.map_or(last, |before| before - 1));
    let mut items = Vec::new();
    let mut scanned = 0;
    while next > 0 && items.len() < limit && scanned < 200 {
        let bottom = next.saturating_sub(8);
        let batch = futures_util::stream::iter(((bottom + 1)..=next).rev().map(|id| async move {
            app_storage::read::<PullRequest>(repo, &storage::pull_path(id)).await
        }))
        .buffered(8)
        .try_collect::<Vec<_>>()
        .await?;
        for entry in batch {
            next -= 1;
            scanned += 1;
            if let Some((pull, _)) = entry
                && state.matches(pull.state)
                && app::matches_query(
                    query.as_deref(),
                    &[&pull.title, &pull.body, &pull.author.name],
                )
            {
                items.push(pull_list_view(&pull, &labels, &assignees));
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
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let title = app::title(&input.title)?;
    app::body(&input.body, false)?;
    let request_id = app::submission(&input.request_id)?;
    if input.base_ref == input.head_ref {
        return Err(Error::Invalid("Base and head branches must differ"));
    }
    if let Some(pull) = storage::recover_pull(
        repo,
        &actor,
        &request_id,
        &title,
        &input.body,
        &input.base_ref,
        &input.head_ref,
    )
    .await?
    {
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
    let (pull, _) = app_storage::read::<PullRequest>(repo, &storage::pull_path(app::number(id)?))
        .await?
        .ok_or(Error::NotFound)?;
    let repository = repo
        .open_current(&server, RepositoryOptions::default(), &server.cancellation)
        .await
        .ok();
    let actor = app::actor(&principal)?;
    let current = repository
        .as_ref()
        .and_then(|repository| current_branches(repository, &pull));
    Ok(Json(
        pull_view(
            &pull,
            &actor,
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
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let path = storage::pull_path(app::number(id)?);
    let (mut pull, etag) = app_storage::read::<PullRequest>(repo, &path)
        .await?
        .ok_or(Error::NotFound)?;
    let label_change = input.label_ids.is_some();
    let assignee_change = input.assignees.is_some();
    let author = app_storage::same_author(&pull.author, &actor);
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
    let labels = labels::catalog(repo).await?;
    let assignees = assignees::available(repo, &actor);
    if let Some(value) = input.title {
        pull.title = app::title(&value)?;
    }
    if let Some(value) = input.body {
        app::body(&value, false)?;
        pull.body = value;
    }
    if let Some(value) = input.state {
        pull.state = value;
    }
    if let Some(value) = input.label_ids {
        pull.label_ids = labels::validate_selection(value, &labels)?;
    }
    if let Some(value) = input.assignees {
        pull.assignee_subjects = assignees::validate_selection(value, &assignees)?;
    }
    pull.version = pull
        .version
        .checked_add(1)
        .filter(|value| *value < app_storage::MAX_NUMBER)
        .ok_or(Error::Conflict)?;
    pull.updated_at = app_storage::now()?;
    if label_change && !principal.can_write(&repo.config) {
        return Err(Error::LabelPermission);
    }
    if assignee_change && !principal.can_write(&repo.config) {
        return Err(Error::AssigneePermission);
    }
    app_storage::update(repo, &path, &pull, etag).await?;
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
    let id = app::number(id)?;
    if app_storage::read::<PullRequest>(repo, &storage::pull_path(id))
        .await?
        .is_none()
    {
        return Err(Error::NotFound);
    }
    if params.state.is_some() || params.q.is_some() {
        return Err(Error::Invalid("Comments do not support list filters"));
    }
    let actor = app::actor(&principal)?;
    let limit = params.limit()?;
    let last = app_storage::last_number(repo, &storage::comments_root(id)).await?;
    let mut next = last.min(params.before.map_or(last, |before| before - 1));
    let mut items = Vec::new();
    let mut scanned = 0;
    while next > 0 && items.len() < limit && scanned < 200 {
        let bottom = next.saturating_sub(8);
        let batch =
            futures_util::stream::iter(((bottom + 1)..=next).rev().map(|number| async move {
                app_storage::read::<PullComment>(repo, &storage::comment_path(id, number)).await
            }))
            .buffered(8)
            .try_collect::<Vec<_>>()
            .await?;
        for entry in batch {
            next -= 1;
            scanned += 1;
            if let Some((comment, _)) = entry {
                items.push(comment_view(&comment, &actor));
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
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let id = app::number(id)?;
    if app_storage::read::<PullRequest>(repo, &storage::pull_path(id))
        .await?
        .is_none()
    {
        return Err(Error::NotFound);
    }
    let Json(input) = input?;
    app::body(&input.body, true)?;
    let actor = app::actor(&principal)?;
    let comment = storage::create_comment(
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
    let path = storage::comment_path(app::number(id)?, app::number(comment)?);
    let (comment, _) = app_storage::read::<PullComment>(repo, &path)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(comment_view(&comment, &app::actor(&principal)?)))
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
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let path = storage::comment_path(app::number(id)?, app::number(comment)?);
    let (mut comment, etag) = app_storage::read::<PullComment>(repo, &path)
        .await?
        .ok_or(Error::NotFound)?;
    if !app_storage::same_author(&comment.author, &actor) {
        return Err(Error::Forbidden);
    }
    if input.version != comment.version {
        return Err(Error::Conflict);
    }
    app::body(&input.body, true)?;
    comment.body = input.body;
    comment.version = comment
        .version
        .checked_add(1)
        .filter(|value| *value < app_storage::MAX_NUMBER)
        .ok_or(Error::Conflict)?;
    comment.updated_at = app_storage::now()?;
    app_storage::update(repo, &path, &comment, etag).await?;
    Ok(Json(comment_view(&comment, &actor)))
}
