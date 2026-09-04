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

use super::{merge_requirements, pull_view, storage};
use crate::{
    app::{self, Error, Result},
    app_storage,
    auth::Principal,
    receive::{self, ReceiveError},
    server::Server,
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
    if base.as_deref() != Some(record.head_oid.as_str()) {
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
        let update = receive::fast_forward(
            Arc::clone(server),
            principal.clone(),
            (repo.config.owner.clone(), repo.config.name.clone()),
            pull.base_ref.clone(),
            oid(&record.base_oid)?,
            oid(&record.head_oid)?,
        )
        .await;
        if let Err(error) = update {
            let repository = repo
                .open_current(server, RepositoryOptions::default(), &server.cancellation)
                .await?;
            if current_ref(&repository, &pull.base_ref).as_deref() != Some(record.head_oid.as_str())
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
    Ok(pull_view(
        &pull,
        &app::actor(principal)?,
        &repo.config,
        principal.can_write(&repo.config),
        None,
    ))
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
    };
    if let Some(record) = &pull.merge {
        if !same_request(record, &candidate) {
            return Err(Error::MergeConflict);
        }
        return Ok((
            StatusCode::OK,
            Json(pull_view(
                &pull,
                &candidate.author,
                &repo.config,
                true,
                None,
            )),
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
    if !merge_requirements(&pull, &repo.config, &candidate.head_oid).satisfied {
        return Err(Error::MergeBlocked);
    }
    let record = storage::reserve_merge(repo, id, &candidate).await?;
    let pull = storage::begin_merge(repo, id, &record).await?;
    let value = finish(&server, &principal, repo, &pull, &record).await?;
    Ok((StatusCode::OK, Json(value)))
}
