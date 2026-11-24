use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    middleware,
    routing::put,
};
use serde::Deserialize;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::minimization,
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/comment-visibility/{*target}",
            put(write),
        )
        .layer(DefaultBodyLimit::max(1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u64,
    change: Change,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Change {
    Hide { reason: minimization::Reason },
    Unhide,
}

async fn write(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, path)): Path<(String, String, String)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<Json<minimization::State>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let target = super::discussion_target::target(&path)?;
    if !minimization::supports(target) {
        return Err(Error::NotFound);
    }
    let Json(input) = input?;
    if input.version >= app::MAX_NUMBER {
        return Err(Error::Invalid("Invalid comment visibility version"));
    }
    let reason = match input.change {
        Change::Hide { reason } => Some(reason),
        Change::Unhide => None,
    };
    let actor = app::actor(&principal)?;
    let membership = app::membership(&server, &principal, &repo).await?;
    if !super::discussion_target::can_manage(&membership.principal, repo.id, target.subject().kind)
    {
        return Err(Error::CommentModerationPermission);
    }
    let cell = app::repository_cell(&server, &repo)?;
    match app::command_output(
        cell.client
            .command::<minimization::Set>(
                &cell.target,
                app::mutation_identity()?,
                minimization::Input {
                    target,
                    actor: (&actor).into(),
                    version: input.version,
                    reason,
                },
            )
            .await,
    )? {
        minimization::Outcome::Saved(saved) => {
            app::membership(&server, &principal, &repo).await?;
            Ok(Json(saved))
        }
        minimization::Outcome::Archived => Err(Error::Archived),
        minimization::Outcome::NotFound => Err(Error::NotFound),
        minimization::Outcome::Conflict => Err(Error::Conflict),
    }
}

#[cfg(test)]
mod tests;
