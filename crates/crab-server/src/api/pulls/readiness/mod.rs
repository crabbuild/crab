use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    routing::put,
};
use serde::Deserialize;
use serde_json::Value;

use super::{current_branches, pull_view, storage};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{
        PullReadiness, SetPullReadiness, SetPullReadinessInput, SetPullReadinessOutcome,
    },
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/readiness",
        put(set),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u64,
    draft: bool,
}

async fn set(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::actor(&principal)?;
    let Json(input) = input?;
    let cell = app::repository_cell(&server, &repo)?;
    let state = if input.draft {
        PullReadiness::Draft
    } else {
        PullReadiness::Ready
    };
    let number = app::number(number)?;
    let version = app::number(input.version)?;
    let result = app::command_output(
        cell.client
            .command::<SetPullReadiness>(
                &cell.target,
                app::mutation_identity()?,
                SetPullReadinessInput {
                    number,
                    version,
                    state,
                    actor: (&actor).into(),
                    can_manage: principal.can_write(repo.id),
                },
            )
            .await,
    )?;
    let pull = match result {
        SetPullReadinessOutcome::Applied(pull) => storage::from_pull(*pull),
        SetPullReadinessOutcome::Archived => return Err(Error::Archived),
        SetPullReadinessOutcome::NotFound => return Err(Error::NotFound),
        SetPullReadinessOutcome::Forbidden => return Err(Error::Forbidden),
        SetPullReadinessOutcome::Closed => {
            return Err(Error::Invalid(
                "Only open pull requests can change readiness",
            ));
        }
        SetPullReadinessOutcome::MergePending => return Err(Error::MergePending),
        SetPullReadinessOutcome::Conflict => return Err(Error::Conflict),
    };
    let repository = repo
        .open_current(
            &server,
            crab_remote_git::RepositoryOptions::default(),
            &server.cancellation,
        )
        .await
        .ok();
    let current = repository
        .as_ref()
        .and_then(|repository| current_branches(repository, &pull));
    Ok(Json(
        pull_view(
            pull.number,
            &actor,
            &server,
            &repo,
            &principal,
            current.as_ref(),
        )
        .await?,
    ))
}

#[cfg(test)]
mod tests;
