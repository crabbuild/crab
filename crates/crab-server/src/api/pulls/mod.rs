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
    api::assignees::Selection,
    api::checks::{self, CheckRun},
    api::labels,
    api::reactions,
    api::statuses::{self, CommitStatus, StatusState},
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::collaboration::Target,
    repository::Repository,
    server::Server,
};

mod comments;
mod merge;
mod readiness;
mod review_requests;
mod review_threads;
mod reviewers;
mod reviews;
mod storage;
use crate::cells::repository::{ListPulls, PullSummary};
use storage::{NewPullRequest, PullRequest, PullState};

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
        .merge(comments::routes())
        .merge(reviews::routes())
        .merge(review_threads::routes())
        .merge(merge::routes())
        .merge(readiness::routes())
        .merge(review_requests::routes())
        .merge(reviewers::routes())
        .layer(axum::extract::DefaultBodyLimit::max(160 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn pull_view(
    pull: &PullRequest,
    actor: &Identity,
    server: &Server,
    repo: &Repository,
    principal: &Principal,
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
    let protections = crate::repository::settings::load(server, repo).await?;
    let protection = protections.protection(&pull.base_ref);
    let (statuses, check_runs) = match protection {
        Some(rule) if !rule.required_checks.is_empty() => (
            statuses::latest(server, repo, head_oid).await?,
            checks::latest(server, repo, head_oid).await?,
        ),
        _ => (vec![], vec![]),
    };
    let labels = if pull.label_ids.is_empty() {
        vec![]
    } else {
        labels::catalog(server, repo).await?
    };
    let requirements = merge_requirements(pull, protection, head_oid, &statuses, &check_runs);
    let reaction = reactions::one(
        server,
        repo,
        actor,
        Target::Pull {
            number: pull.number,
        },
    )
    .await?;
    let authors = super::profiles::attribution::Authors::load(
        server,
        principal,
        std::iter::once(&pull.author)
            .chain(pull.merge.as_ref().map(|merge| &merge.author))
            .chain(pull.merge_pending.as_ref().map(|merge| &merge.author)),
    )
    .await?;
    let assignees = Selection::load(server, principal, repo, pull.assignees.iter()).await?;
    let can_write = assignees.principal.can_write(repo.id);
    let can_triage = assignees.principal.can_triage(repo.id);
    Ok(json!({
        "draft": pull.draft,
        "number": pull.number,
        "title": pull.title,
        "body": pull.body,
        "reactions": reaction,
        "state": pull.state,
        "author": authors.view(&pull.author),
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
        "assignees": assignees.view(&pull.assignees),
        "can_label": can_triage,
        "can_assign": can_triage,
        "can_edit": same_author(&pull.author, actor),
        "can_manage": pull.state != PullState::Merged
            && pull.merge_pending.is_none()
            && (can_triage || same_author(&pull.author, actor)),
        "can_decide": pull.state == PullState::Open
            && current.is_some()
            && !same_author(&pull.author, actor),
        "can_merge": pull.state == PullState::Open
            && !pull.draft
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
            "author": authors.view(&merge.author),
            "method": merge.method,
            "commit_oid": merge.commit_oid,
            "message": merge.message,
            "created_at": merge.created_at,
        })),
        "merge_pending": pull.merge_pending.as_ref().map(|merge| json!({
            "request_id": merge.request_id,
            "author": authors.view(&merge.author),
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

pub(super) fn pull_list_view(
    authors: &super::profiles::attribution::Authors,
    pull: &PullSummary,
    labels: &[crate::cells::repository::search::index::LabelSummary],
    assignees: &Selection,
) -> Value {
    json!({
        "kind": "pull",
        "draft": pull.draft,
        "version": pull.version,
        "comment_count": pull.comment_count,
        "reaction_count": pull.reaction_count,
        "number": pull.number,
        "title": pull.title,
        "state": pull.state,
        "author": authors.view(&pull.author),
        "base_ref": pull.base_ref,
        "head_ref": pull.head_ref,
        "created_at": pull.created_at_ms,
        "updated_at": pull.updated_at_ms,
        "labels": pull.label_ids.iter().filter_map(|id|labels.iter().find(|label|label.id==*id)).collect::<Vec<_>>(),
        "assignees": assignees.view(&pull.assignees),
    })
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<super::discussion_search::ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    let routed = app::repository_cell(&server, repo)?;
    let mut input = params.input("is:open")?;
    super::discussion_search::resolve_people(&server, &principal, &mut input).await?;
    let page = app::query_output(
        routed
            .client
            .query::<ListPulls>(&routed.target, None, input)
            .await,
    )?;
    let authors = super::profiles::attribution::Authors::load(
        &server,
        &principal,
        page.items.iter().map(|pull| &pull.author),
    )
    .await?;
    let assignees = Selection::load(
        &server,
        &principal,
        repo,
        page.items.iter().flat_map(|pull| pull.assignees.iter()),
    )
    .await?;
    let pulls = &page.items;
    let items = pulls
        .iter()
        .map(|pull| pull_list_view(&authors, pull, &page.labels, &assignees))
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"items":items,"next":page.next,"counts":page.counts,"matching":page.matching,"navigation":page.navigation,"authored_by_me":page.authored_by_me,"assigned_to_me":page.assigned_to_me,"review_requested_from_me":page.review_requested_from_me}),
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
    #[serde(default)]
    draft: bool,
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
    if let Some(pull) = storage::pull_submission(&server, repo, &request_id).await? {
        // Native submission validation compares the original payload, even when
        // the current PR body or readiness changed after creation.
        let pull = storage::create_pull(
            &server,
            repo,
            NewPullRequest {
                draft: input.draft,
                author: actor.clone(),
                request_id,
                title,
                body: input.body,
                base_ref: input.base_ref,
                head_ref: input.head_ref,
                base_oid: pull.base_oid,
                head_oid: pull.head_oid,
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
        return Ok((
            StatusCode::CREATED,
            Json(pull_view(&pull, &actor, &server, repo, &principal, current.as_ref()).await?),
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
            draft: input.draft,
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
        Json(pull_view(&pull, &actor, &server, repo, &principal, current.as_ref()).await?),
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
    let pull = storage::pull(&server, repo, app::number(id)?)
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
        pull_view(&pull, &actor, &server, repo, &principal, current.as_ref()).await?,
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
    assignee_ids: Option<Vec<uuid::Uuid>>,
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
    let pull = storage::pull(&server, repo, number)
        .await?
        .ok_or(Error::NotFound)?;
    let label_change = input.label_ids.is_some();
    let needs_label_catalog = input
        .label_ids
        .as_ref()
        .is_some_and(|labels| !labels.is_empty())
        || (input.label_ids.is_none() && !pull.label_ids.is_empty());
    let assignee_change = input.assignee_ids.is_some();
    let can_triage = app::membership(&server, &principal, repo)
        .await?
        .principal
        .can_triage(repo.id);
    let author = same_author(&pull.author, &actor);
    if pull.merge_pending.is_some() {
        return Err(Error::MergePending);
    }
    if (input.title.is_some() || input.body.is_some()) && !author {
        return Err(Error::Forbidden);
    }
    if input.state.is_some() && !(author || can_triage) {
        return Err(Error::Forbidden);
    }
    if label_change && !can_triage {
        return Err(Error::LabelPermission);
    }
    if assignee_change && !can_triage {
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
        && input.assignee_ids.is_none()
    {
        return Err(Error::Invalid("No pull request changes supplied"));
    }
    let labels = if needs_label_catalog {
        labels::catalog(&server, repo).await?
    } else {
        vec![]
    };
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
    // Assignment eligibility and caller role must share the final current roster.
    let (can_triage, assignees) = if let Some(ids) = input.assignee_ids {
        let roster = crate::api::members::roster::Roster::load(&server, &principal, repo).await?;
        if !roster.principal.can_triage(repo.id) {
            return Err(Error::AssigneePermission);
        }
        (true, Some(Selection::validate(&roster, ids)?))
    } else {
        let permissions = app::membership(&server, &principal, repo).await?;
        (permissions.principal.can_triage(repo.id), None)
    };
    if label_change && !can_triage {
        return Err(Error::LabelPermission);
    }
    let pull = storage::update_pull(
        &server,
        repo,
        number,
        storage::PullEdit {
            actor: actor.clone(),
            can_triage,
            version: input.version,
            title,
            body,
            state: input.state,
            label_ids,
            assignees,
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
        pull_view(&pull, &actor, &server, repo, &principal, current.as_ref()).await?,
    ))
}

#[cfg(test)]
mod tests;
