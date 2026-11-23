use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    routing::post,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    codeowners,
    git::{self, PullGit},
    storage::{self, PullRequest, PullState},
};
use crate::{
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::{
        pull_base::{Change, Input, Outcome, Retarget},
        review_requests::automatic::Plan,
    },
    repository::Repository,
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/base",
        post(change),
    )
}

async fn change(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(change): Json<Change>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::actor(&principal)?;
    let number = app::number(number)?;
    let pull = storage::pull(&server, &repo, number)
        .await?
        .ok_or(Error::NotFound)?;
    let grant = app::membership(&server, &principal, &repo).await?;
    if !grant.principal.can_write(repo.id) && !super::same_author(&pull.author, &actor) {
        return Err(Error::Forbidden);
    }
    let base = change
        .base_ref
        .strip_prefix("refs/heads/")
        .ok_or(Error::Invalid("Select a base branch"))?;
    crate::api::branches::branch_ref(base).map_err(Error::Invalid)?;
    super::super::statuses::parse_oid(&change.base_oid)?;
    super::super::statuses::parse_oid(&change.head_oid)?;
    if change.version != pull.version
        || pull.state != PullState::Open
        || pull.merge_pending.is_some()
    {
        // A committed receipt remains recoverable after the refs move or the
        // pull request closes. The Cell checks it before the live comparison.
        return persist(&server, &repo, &principal, &actor, number, change, None).await;
    }
    let _read = crate::api::read_admission::acquire_bulk(&server).map_err(|_| Error::MergeBusy)?;
    let fork = pull
        .head_repository
        .map(|id| git::fork_repository(&server, &repo, id))
        .transpose()?;
    let refs = if fork.is_some() {
        vec![change.base_ref.clone()]
    } else {
        vec![change.base_ref.clone(), pull.head_ref.clone()]
    };
    let head_ref = pull.head_ref.clone();
    let work = |cancel: CancellationToken| {
        inspect(&server, &repo, &principal, &actor, &pull, change, cancel)
    };
    // Share merge and update-branch lock order. Both captured tips remain
    // valid through the transactional base change and approval invalidation.
    crab_remote::publication::with_leases(
        &repo.store,
        &repo.layout,
        refs,
        Duration::from_secs(300),
        &server.cancellation,
        |_holders, scoped| async move {
            if let Some(fork) = fork {
                crab_remote::publication::with_leases(
                    &fork.store,
                    &fork.layout,
                    [head_ref],
                    Duration::from_secs(300),
                    &scoped,
                    |_holders, scoped| work(scoped),
                )
                .await
            } else {
                work(scoped).await
            }
        },
    )
    .await
}

async fn inspect(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    actor: &Identity,
    pull: &PullRequest,
    change: Change,
    cancel: CancellationToken,
) -> Result<Json<Value>> {
    let git = PullGit::open(server, repo, pull.head_repository).await?;
    let current = git.tips(&change.base_ref, &pull.head_ref);
    let matches = current
        .as_ref()
        .is_some_and(|(base, head)| *base == change.base_oid && *head == change.head_oid);
    let owners = if matches {
        let tips = (change.base_oid.as_str(), change.head_oid.as_str());
        // The preview and commit mutation use the same Git merge-base rules.
        super::super::query::pull_comparison_base(
            &git.base,
            git.fork.as_ref(),
            tips.0,
            tips.1,
            &cancel,
        )
        .await
        .map_err(crate::Error::from)?;
        Some(if pull.draft {
            Plan::default()
        } else {
            codeowners::requests(server, repo, principal, &git, tips).await?
        })
    } else {
        None
    };
    if cancel.is_cancelled() {
        return Err(Error::MergeBusy);
    }
    persist(server, repo, principal, actor, pull.number, change, owners).await
}

async fn persist(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    actor: &Identity,
    pull: u64,
    change: Change,
    owners: Option<Plan>,
) -> Result<Json<Value>> {
    let grant = app::membership(server, principal, repo).await?;
    let cell = app::repository_cell(server, repo)?;
    let outcome = app::command_output(
        cell.client
            .command::<Retarget>(
                &cell.target,
                app::mutation_identity()?,
                Input {
                    pull,
                    actor: actor.into(),
                    can_write: grant.principal.can_write(repo.id),
                    comparison_current: owners.is_some(),
                    change,
                    code_owner_requests: owners.unwrap_or_default(),
                },
            )
            .await,
    )?;
    match outcome {
        Outcome::Saved => Ok(Json(json!({"saved":true}))),
        Outcome::NotFound => Err(Error::NotFound),
        Outcome::Forbidden => Err(Error::Forbidden),
        Outcome::Archived => Err(Error::Archived),
        Outcome::InvalidBase => Err(Error::Invalid("Select a different base branch")),
        Outcome::Conflict => Err(Error::Conflict),
        Outcome::ComparisonChanged => Err(Error::CreationRejected(
            crate::cells::repository::collaboration::creation::Failure::ComparisonChanged,
        )),
        Outcome::RequestConflict => Err(Error::RequestConflict),
        Outcome::MergePending => Err(Error::MergePending),
    }
}
