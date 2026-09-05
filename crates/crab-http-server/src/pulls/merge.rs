use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use crab_remote_git::RepositoryOptions;
use gix_hash::ObjectId;
use serde::Deserialize;
use serde_json::Value;

use super::{merge_requirements, merge_tree, pull_view, storage};
use crate::{
    app::{self, Error, Result},
    app_storage,
    auth::Principal,
    checks,
    receive::{self, ReceiveError},
    server::Server,
    statuses,
};
use storage::{MergeMethod, NewPullMerge, PullMerge, PullRequest, PullState};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/merge",
        post(execute),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeInput {
    request_id: String,
    version: u64,
    method: MergeMethod,
    base_oid: String,
    head_oid: String,
    #[serde(default)]
    message: String,
}

fn oid(value: &str) -> Result<ObjectId> {
    ObjectId::from_hex(value.as_bytes())
        .ok()
        .filter(|oid| !oid.is_null())
        .ok_or(Error::Invalid("Merge requires exact SHA-1 commit IDs"))
}

fn current_ref(repository: &crab_remote_git::RemoteGitRepository, name: &str) -> Option<String> {
    repository
        .refs()
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.target.to_string())
}

fn map_receive(error: ReceiveError) -> Error {
    match &error {
        ReceiveError::Busy
        | ReceiveError::Coordination(
            crab_coordination::CoordinationError::PushLockHeld { .. }
            | crab_coordination::CoordinationError::GcFenceHeld { .. },
        ) => Error::MergeBusy,
        ReceiveError::Request(_)
        | ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Stale { .. })
        | ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::NonFastForward {
            ..
        })
        | ReceiveError::Write(crab_write::WriteError::RefChanged { .. }) => Error::MergeConflict,
        _ => Error::Merge(Box::new(error)),
    }
}

fn same_request(record: &PullMerge, input: &NewPullMerge) -> bool {
    // Any current writer may recover a recorded merge after the initiating
    // session disappears, but the immutable request details must still match.
    record.request_id == input.request_id
        && record.method == input.method
        && record.pull_version == input.pull_version
        && record.base_oid == input.base_oid
        && record.head_oid == input.head_oid
        && record.message == input.message
}

async fn build_merge_commit(
    repository: &crab_remote_git::RemoteGitRepository,
    cancellation: &tokio_util::sync::CancellationToken,
    base: ObjectId,
    head: ObjectId,
    actor: &crate::auth::Identity,
    message: &str,
    created_at: u64,
) -> Result<merge_tree::Plan> {
    let operation = repository
        .operation(crab_remote_git::OperationKind::Repository, cancellation)
        .await
        .map_err(|error| Error::Repository(crate::Error::Remote(error)))?;
    let built = merge_tree::build(
        repository, &operation, base, head, actor, message, created_at,
    )
    .await;
    match built {
        Ok(plan) => {
            operation
                .finish(Ok(()))
                .await
                .map_err(|error| Error::Repository(crate::Error::Remote(error)))?;
            Ok(plan)
        }
        Err(merge_tree::Error::Object(crate::git_objects::Error::Remote(error))) => {
            let error = operation.finish::<()>(Err(error)).await.err().unwrap_or(
                crab_remote_git::Error::InternalInvariant {
                    invariant: "failed pull request merge read unexpectedly succeeded",
                },
            );
            Err(Error::Repository(crate::Error::Remote(error)))
        }
        Err(merge_tree::Error::Conflict | merge_tree::Error::History) => {
            operation
                .finish(Ok(()))
                .await
                .map_err(|error| Error::Repository(crate::Error::Remote(error)))?;
            Err(Error::MergeConflict)
        }
        Err(merge_tree::Error::Object(error)) => {
            operation
                .finish(Ok(()))
                .await
                .map_err(|error| Error::Repository(crate::Error::Remote(error)))?;
            Err(Error::MergeObject(Box::new(error)))
        }
    }
}

async fn finish(
    server: &Arc<Server>,
    principal: &Principal,
    repo: &crate::server::Repository,
    pull: &PullRequest,
    record: &PullMerge,
) -> Result<Value> {
    let repository = repo
        .open_current(server, RepositoryOptions::default(), &server.cancellation)
        .await?;
    let base = current_ref(&repository, &pull.base_ref);
    let head = current_ref(&repository, &pull.head_ref);
    if base.as_deref() != Some(record.commit_oid.as_str()) {
        if base.as_deref() != Some(record.base_oid.as_str())
            || head.as_deref() != Some(record.head_oid.as_str())
        {
            storage::abort_merge(repo, pull.number, record).await?;
            return Err(Error::MergeConflict);
        }
        let latest = app_storage::read::<PullRequest>(repo, &storage::pull_path(pull.number))
            .await?
            .map(|(pull, _)| pull)
            .ok_or(Error::NotFound)?;
        if latest.state != PullState::Open
            || latest
                .merge_pending
                .as_ref()
                .is_none_or(|pending| pending.request_id != record.request_id)
        {
            storage::abort_merge(repo, pull.number, record).await?;
            return Err(Error::MergeConflict);
        }
        let update = match record.method {
            MergeMethod::FastForward => {
                receive::fast_forward(
                    Arc::clone(server),
                    principal.clone(),
                    (repo.config.owner.clone(), repo.config.name.clone()),
                    pull.base_ref.clone(),
                    oid(&record.base_oid)?,
                    oid(&record.head_oid)?,
                )
                .await
            }
            MergeMethod::MergeCommit => {
                let plan = match build_merge_commit(
                    &repository,
                    &server.cancellation,
                    oid(&record.base_oid)?,
                    oid(&record.head_oid)?,
                    &record.author,
                    &record.message,
                    record.created_at / 1_000,
                )
                .await
                {
                    Ok(plan) if plan.oid.to_string() == record.commit_oid => plan,
                    Ok(_) => {
                        storage::abort_merge(repo, pull.number, record).await?;
                        return Err(Error::MergeConflict);
                    }
                    Err(Error::MergeConflict) => {
                        storage::abort_merge(repo, pull.number, record).await?;
                        return Err(Error::MergeConflict);
                    }
                    Err(error) => return Err(error),
                };
                receive::publish_pull_request_objects(
                    Arc::clone(server),
                    principal.clone(),
                    (repo.config.owner.clone(), repo.config.name.clone()),
                    crab_git::receive_plan::RefUpdate {
                        name: pull.base_ref.clone(),
                        old: Some(oid(&record.base_oid)?),
                        new: Some(plan.oid),
                    },
                    plan.objects,
                )
                .await
            }
        };
        if let Err(error) = update {
            let repository = repo
                .open_current(server, RepositoryOptions::default(), &server.cancellation)
                .await?;
            if current_ref(&repository, &pull.base_ref).as_deref()
                != Some(record.commit_oid.as_str())
            {
                let error = map_receive(error);
                if matches!(&error, Error::MergeConflict) {
                    storage::abort_merge(repo, pull.number, record).await?;
                }
                return Err(error);
            }
        }
    }
    let pull = storage::complete_merge(repo, pull.number, record).await?;
    pull_view(
        &pull,
        &app::actor(principal)?,
        repo,
        principal.can_write(&repo.config),
        None,
    )
    .await
}

async fn execute(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    input: std::result::Result<Json<MergeInput>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    if !principal.can_write(&repo.config) {
        return Err(Error::MergePermission);
    }
    let id = app::number(id)?;
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let request_id = app::submission(&input.request_id)?;
    let (pull, _) = app_storage::read::<PullRequest>(repo, &storage::pull_path(id))
        .await?
        .ok_or(Error::NotFound)?;
    let candidate = NewPullMerge {
        author: actor,
        request_id,
        method: input.method,
        pull_version: input.version,
        base_oid: input.base_oid,
        head_oid: input.head_oid,
        message: match input.method {
            MergeMethod::FastForward => String::new(),
            MergeMethod::MergeCommit => {
                let message = input.message.trim();
                let message = if message.is_empty() {
                    format!(
                        "Merge pull request #{} from {}",
                        id,
                        pull.head_ref
                            .strip_prefix("refs/heads/")
                            .unwrap_or(&pull.head_ref)
                    )
                } else {
                    message.to_owned()
                };
                if message.chars().count() > 256
                    || message
                        .chars()
                        .any(|character| character.is_control() && character != '\n')
                {
                    return Err(Error::Invalid(
                        "Merge commit message must contain 1–256 characters",
                    ));
                }
                message
            }
        },
    };
    if let Some(record) = &pull.merge {
        if !same_request(record, &candidate) {
            return Err(Error::MergeConflict);
        }
        return Ok((
            StatusCode::OK,
            Json(pull_view(&pull, &candidate.author, repo, true, None).await?),
        ));
    }
    if let Some(record) = &pull.merge_pending {
        if !same_request(record, &candidate) {
            return Err(Error::MergeConflict);
        }
        let value = finish(&server, &principal, repo, &pull, record).await?;
        return Ok((StatusCode::OK, Json(value)));
    }
    if let Some(record) = storage::recover_merge(repo, id, &candidate).await? {
        let pull = storage::begin_merge(repo, id, &record).await?;
        let value = finish(&server, &principal, repo, &pull, &record).await?;
        return Ok((StatusCode::OK, Json(value)));
    }
    if pull.state != PullState::Open
        || pull.version != candidate.pull_version
        || candidate.base_oid == candidate.head_oid
    {
        return Err(Error::MergeConflict);
    }
    let repository = repo
        .open_current(&server, RepositoryOptions::default(), &server.cancellation)
        .await?;
    if current_ref(&repository, &pull.base_ref).as_deref() != Some(candidate.base_oid.as_str())
        || current_ref(&repository, &pull.head_ref).as_deref() != Some(candidate.head_oid.as_str())
    {
        return Err(Error::MergeConflict);
    }
    // These reads order merge admission before later status or check-run updates.
    // Once the reservation exists, retries recover that admitted publication.
    let (statuses, check_runs) = match repo.config.protection(&pull.base_ref) {
        Some(rule) if !rule.required_checks.is_empty() => (
            statuses::latest(repo, &candidate.head_oid).await?,
            checks::latest(repo, &candidate.head_oid).await?,
        ),
        _ => (vec![], vec![]),
    };
    if !merge_requirements(
        &pull,
        &repo.config,
        &candidate.head_oid,
        &statuses,
        &check_runs,
    )
    .satisfied
    {
        return Err(Error::MergeBlocked);
    }
    let created_at = app_storage::now()?;
    let commit_oid = match candidate.method {
        MergeMethod::FastForward => oid(&candidate.head_oid)?,
        MergeMethod::MergeCommit => {
            build_merge_commit(
                &repository,
                &server.cancellation,
                oid(&candidate.base_oid)?,
                oid(&candidate.head_oid)?,
                &candidate.author,
                &candidate.message,
                created_at / 1_000,
            )
            .await?
            .oid
        }
    };
    let record = storage::reserve_merge(repo, id, &candidate, commit_oid, created_at).await?;
    let pull = storage::begin_merge(repo, id, &record).await?;
    let value = finish(&server, &principal, repo, &pull, &record).await?;
    Ok((StatusCode::OK, Json(value)))
}
