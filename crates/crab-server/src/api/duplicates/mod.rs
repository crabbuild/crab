use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    middleware,
    routing::put,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::{Subject, SubjectKind, duplicates},
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/duplicates/{kind}/{number}/undo",
            put(undo),
        )
        .layer(DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    event: u64,
}

async fn undo(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, kind, number)): Path<(String, String, SubjectKind, u64)>,
    Json(input): Json<Request>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let cell = app::repository_cell(&server, &repo)?;
    let actor = app::actor(&principal)?;
    let removal = duplicates::Removal {
        subject: Subject {
            kind,
            number: app::number(number)?,
        },
        event: app::number(input.event)?,
        actor: (&actor).into(),
    };
    let membership = app::membership(&server, &principal, &repo).await?;
    if !membership.principal.can_write(repo.id) {
        return Err(Error::Forbidden);
    }
    match app::command_output(
        cell.client
            .command::<duplicates::Undo>(&cell.target, app::mutation_identity()?, removal)
            .await,
    )? {
        duplicates::Outcome::Removed => Ok(Json(json!({"undone": true}))),
        duplicates::Outcome::Archived => Err(Error::Archived),
        duplicates::Outcome::NotFound => Err(Error::NotFound),
        duplicates::Outcome::Conflict => Err(Error::Conflict),
    }
}

#[cfg(test)]
mod tests;
