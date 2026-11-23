use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    MergeCheckTarget, MergeRequirements,
    git::PullGit,
    merge, merge_methods, requirements,
    storage::{self, MergeMethod, NewPullMerge, PullMerge, PullRequest, PullState},
};
use crate::{
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::pull_auto_merge::{self as cell, Change, Job, Outcome, Stop},
    repository::{Repository, settings},
    server::Server,
};

mod queue;
mod worker;
pub(crate) use worker::run;

const TTL: Duration = Duration::from_secs(300);
type Address = (String, String, u64);

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/auto-merge",
        get(detail).put(enable).delete(disable),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enable {
    request_id: Uuid,
    version: u64,
    base_oid: String,
    head_oid: String,
    method: MergeMethod,
    #[serde(default)]
    queue: bool,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Disable {
    request_id: Uuid,
}

async fn current(server: &Server, repo: &Repository, pull: u64) -> Result<Option<cell::State>> {
    let routed = app::repository_cell(server, repo)?;
    app::query_output(
        routed
            .client
            .query::<cell::Get>(&routed.target, None, pull)
            .await,
    )
}

async fn change(server: &Server, repo: &Repository, change: Change) -> Result<()> {
    let routed = app::repository_cell(server, repo)?;
    match app::command_output(
        routed
            .client
            .command::<cell::Edit>(&routed.target, app::mutation_identity()?, change)
            .await,
    )? {
        Outcome::Saved => Ok(()),
        Outcome::Changed => Err(Error::AutoMergeChanged),
        Outcome::Disabled => Err(Error::AutoMergeDisabled),
        Outcome::Archived => Err(Error::Archived),
        Outcome::MergePending => Err(Error::MergePending),
        Outcome::QueueRequired => Err(Error::MergeQueueRequired),
    }
}

async fn view(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    pull: &PullRequest,
) -> Result<Json<Value>> {
    let actor = app::actor(principal)?;
    let grant = app::membership(server, principal, repo).await?;
    let can_write = grant.principal.can_write(repo.id);
    let (settings, state, queue_required) = tokio::try_join!(
        merge_methods::load_for_ref(server, repo, &pull.base_ref),
        current(server, repo, pull.number),
        settings::requires_merge_queue(server, repo, &pull.base_ref),
    )?;
    let active = state.as_ref().is_some_and(|state| state.stopped.is_none());
    let available = can_write
        && principal.is_user()
        && !active
        && !pull.draft
        && pull.state == PullState::Open
        && pull.merge_pending.is_none()
        && !settings::load_lifecycle(server, repo).await?.archived;
    let can_enqueue = if available && (queue_required || settings.merge_queue) {
        let tips = match PullGit::open(server, repo, pull.head_repository).await {
            Ok(git) => git.tips(&pull.base_ref, &pull.head_ref),
            Err(Error::NotFound) => None,
            Err(error) => return Err(error),
        };
        match tips {
            Some((base, head)) if base != head => {
                let report =
                    queue_requirements(server, repo, pull, &grant, (&base, &head), None).await?;
                !report.checks.is_empty() && report.satisfied
            }
            _ => false,
        }
    } else {
        false
    };
    Ok(Json(json!({
        "can_enable": settings.auto_merge && available && !queue_required,
        "can_enqueue": can_enqueue,
        "queue_method": settings.merge_queue_method,
        "queue_builds": settings.merge_queue_builds,
        "queue_timeout_minutes": settings.merge_queue_timeout_minutes,
        "can_disable": active && pull.merge_pending.is_none() && (can_write || super::same_author(&pull.author, &actor)),
        "request": state.map(|state| json!({"request_id": state.job.id, "method": state.job.method,
            "head_oid": state.job.head_oid, "scheduled_by": state.job.actor.name, "stopped": state.stopped,
            "queue": state.job.queue.is_some(), "first": state.first,
            "testing": state.ahead.is_some() && state.job.queue.and_then(|queue| queue.testing).is_some()})),
    })))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<Address>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let pull = storage::pull(&server, &repo, app::number(number)?)
        .await?
        .ok_or(Error::NotFound)?;
    view(&server, &repo, &principal, &pull).await
}

async fn enable(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<Address>,
    Json(input): Json<Enable>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let number = app::number(number)?;
    if !principal.is_user()
        || !app::membership(&server, &principal, &repo)
            .await?
            .principal
            .can_write(repo.id)
    {
        return Err(Error::MergePermission);
    }
    let actor = app::actor(&principal)?;
    if input.queue && (!input.method.supports_queue() || !input.message.is_empty()) {
        return Err(Error::Invalid(
            "The merge queue prepares its own candidate and commit message",
        ));
    }
    let message = |head: &str| {
        if input.queue {
            Ok(String::new())
        } else {
            merge::message(input.method, &input.message, number, head)
        }
    };
    if durable_principal(&server, &repo, actor.clone())
        .await?
        .is_none()
    {
        return Err(Error::Invalid(
            "Scheduling auto-merge requires durable Write access",
        ));
    }
    let pull = storage::pull(&server, &repo, number)
        .await?
        .ok_or(Error::NotFound)?;
    if let Some(saved) = current(&server, &repo, number).await?
        && saved.job.id == input.request_id
    {
        if saved.job.actor.issuer != actor.issuer
            || saved.job.actor.subject != actor.subject
            || saved.job.method != input.method
            || saved.job.queue.is_some() != input.queue
            || saved.job.head_oid != input.head_oid
            || saved.job.initial_base_oid != input.base_oid
            || saved.job.initial_version != input.version
            || saved.job.message != message(&saved.job.head_ref)?
        {
            return Err(Error::AutoMergeChanged);
        }
        return view(&server, &repo, &principal, &pull).await;
    }
    let _read = crate::api::read_admission::acquire_bulk(&server).map_err(|_| Error::MergeBusy)?;
    let configured = merge_methods::load_for_ref(&server, &repo, &pull.base_ref).await?;
    let git = PullGit::open(&server, &repo, pull.head_repository).await?;
    if input.request_id.is_nil()
        || pull.version != input.version
        || git
            .tips(&pull.base_ref, &pull.head_ref)
            .as_ref()
            .is_none_or(|(base, head)| *base != input.base_oid || *head != input.head_oid)
        || input.base_oid == input.head_oid
    {
        return Err(Error::AutoMergeChanged);
    }
    if input.queue {
        let grant = app::membership(&server, &principal, &repo).await?;
        let report = queue_requirements(
            &server,
            &repo,
            &pull,
            &grant,
            (&input.base_oid, &input.head_oid),
            None,
        )
        .await?;
        report.enforce()?;
        if report.checks.is_empty() {
            return Err(Error::MergeCandidateChecks);
        }
    }
    change(
        &server,
        &repo,
        Change::Arm {
            queue: input.queue,
            configured_queue_required: settings::configured_queue_required(&repo, &pull.base_ref),
            ruleset_queue_required: settings::queue_policy(&server, &repo, &pull.base_ref)
                .await?
                .ruleset,
            version: input.version,
            job: Job {
                pull: number,
                id: input.request_id,
                actor: (&actor).into(),
                base_ref: pull.base_ref.clone(),
                head_ref: pull.head_ref.clone(),
                head_repository: pull.head_repository,
                head_oid: input.head_oid,
                initial_base_oid: input.base_oid,
                initial_version: input.version,
                generation: if input.queue {
                    configured.merge_queue_generation
                } else {
                    configured.auto_merge_generation
                },
                queue_method_revision: if input.queue {
                    configured.merge_queue_method_revision
                } else {
                    0
                },
                comparison_revision: pull.retargeted_base.as_ref().map(|base| base.revision),
                method: input.method,
                message: message(&pull.head_ref)?,
                queue: None,
            },
        },
    )
    .await?;
    view(&server, &repo, &principal, &pull).await
}

async fn disable(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<Address>,
    Json(input): Json<Disable>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let number = app::number(number)?;
    let (send, response) = tokio::sync::oneshot::channel();
    let worker = Arc::clone(&server);
    server.receives.spawn(async move {
        let repo = &repo;
        let server = &worker;
        let done = with_lock(server, repo, number, |_cancel| async move {
            let pull = storage::pull(server, repo, number)
                .await?
                .ok_or(Error::NotFound)?;
            let grant = app::membership(server, &principal, repo).await?;
            if !grant.principal.can_write(repo.id)
                && !super::same_author(&pull.author, &app::actor(&principal)?)
            {
                return Err(Error::Forbidden);
            }
            stop(server, repo, number, input.request_id, Stop::Canceled).await?;
            view(server, repo, &principal, &pull).await
        })
        .await;
        let _ = send.send(done);
    });
    response.await.map_err(|_| Error::MergeBusy)?
}

async fn with_lock<T, F, Fut>(server: &Server, repo: &Repository, number: u64, work: F) -> Result<T>
where
    F: FnOnce(CancellationToken) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    crab_remote::publication::with_internal_lease(
        &repo.store,
        &repo.layout,
        &merge::lease_name(number),
        TTL,
        &server.cancellation,
        work,
    )
    .await
}

async fn stop(server: &Server, repo: &Repository, pull: u64, id: Uuid, reason: Stop) -> Result<()> {
    change(server, repo, Change::Disarm { pull, id, reason }).await
}

async fn durable_principal(
    server: &Server,
    repo: &Repository,
    actor: Identity,
) -> Result<Option<Principal>> {
    match app::membership(server, &Principal::auto_merge(repo.id, actor), repo).await {
        Ok(grant) => Ok(grant
            .principal
            .can_write(repo.id)
            .then_some(grant.principal)),
        Err(Error::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

fn matches_attempt(job: &Job, pending: &PullMerge) -> bool {
    let tested = pending
        .tested_candidate
        .then_some(pending.commit_oid.as_str());
    pending.request_id
        == job
            .publication_id(pending.pull_version, &pending.base_oid, tested)
            .to_string()
        && pending.head_oid == job.head_oid
        && pending.method == job.method
        && pending.message == job.message
        && pending.author.issuer == job.actor.issuer
        && pending.author.subject == job.actor.subject
        && pending.bypass_rulesets.is_empty()
        && pending.tested_candidate == job.queue.is_some()
}

pub(super) async fn active_queue(
    server: &Server,
    repo: &Repository,
    number: u64,
) -> Result<Option<cell::State>> {
    Ok(current(server, repo, number)
        .await?
        .filter(|state| state.stopped.is_none() && state.job.queue.is_some()))
}

pub(super) async fn require_direct_access(
    server: &Server,
    repo: &Repository,
    number: u64,
) -> Result<()> {
    if active_queue(server, repo, number).await?.is_some() {
        return Err(Error::MergeQueued);
    }
    Ok(())
}

async fn queue_requirements(
    server: &Server,
    repo: &Repository,
    pull: &PullRequest,
    grant: &app::Membership,
    (base, head): (&str, &str),
    tested: Option<&str>,
) -> Result<MergeRequirements> {
    requirements(
        server,
        repo,
        pull,
        Some(base),
        head,
        grant,
        &grant.merge_reviewers(server).await?,
        MergeCheckTarget::Queue(tested),
    )
    .await
}

pub(super) async fn authorize(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    pending: &PullMerge,
    number: u64,
) -> Result<()> {
    let state = current(server, repo, number).await?;
    let active = state
        .filter(|state| {
            state.stopped.is_none()
                && (state.job.queue.is_none() || state.first)
                && matches_attempt(&state.job, pending)
        })
        .ok_or(Error::AutoMergeDisabled)?;
    let (settings, queue_required) = tokio::try_join!(
        merge_methods::load_for_ref(server, repo, &active.job.base_ref),
        settings::requires_merge_queue(server, repo, &active.job.base_ref),
    )?;
    if queue_required && active.job.queue.is_none() {
        return Err(Error::MergeQueueRequired);
    }
    if (active.job.queue.is_none() && !settings.enabled.contains(&pending.method))
        || !active.job.enabled(&settings, queue_required)
        || !principal.can_write(repo.id)
    {
        return Err(Error::AutoMergeDisabled);
    }
    Ok(())
}
