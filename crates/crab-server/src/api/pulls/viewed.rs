use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    routing::get,
};
use serde::Deserialize;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::viewed::{
        GetViewedFiles, SetViewedFile, SetViewedInput, SetViewedOutcome, ViewedFiles, ViewedKey,
    },
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/viewed",
        get(list).put(set),
    )
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<ViewedFiles>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::actor(&principal)?;
    let cell = app::repository_cell(&server, &repo)?;
    let saved = app::query_output(
        cell.client
            .query::<GetViewedFiles>(
                &cell.target,
                None,
                ViewedKey {
                    pull: app::number(number)?,
                    actor: (&actor).into(),
                },
            )
            .await,
    )?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(saved.ok_or(Error::NotFound)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u64,
    file: String,
    viewed: bool,
}

async fn set(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<Json<ViewedFiles>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::actor(&principal)?;
    let Json(input) = input?;
    let cell = app::repository_cell(&server, &repo)?;
    app::membership(&server, &principal, &repo).await?;
    let outcome = app::command_output(
        cell.client
            .command::<SetViewedFile>(
                &cell.target,
                app::mutation_identity()?,
                SetViewedInput {
                    target: ViewedKey {
                        pull: app::number(number)?,
                        actor: (&actor).into(),
                    },
                    version: input.version,
                    file: input.file,
                    viewed: input.viewed,
                },
            )
            .await,
    )?;
    match outcome {
        SetViewedOutcome::Saved(saved) => Ok(Json(saved)),
        SetViewedOutcome::NotFound => Err(Error::NotFound),
        SetViewedOutcome::Archived => Err(Error::Archived),
        SetViewedOutcome::Conflict => Err(Error::Conflict),
        SetViewedOutcome::AtCapacity => Err(Error::Invalid(
            "Viewed-file capacity reached for this pull request",
        )),
    }
}
