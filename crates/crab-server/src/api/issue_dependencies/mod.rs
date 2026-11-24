use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::JsonRejection},
    middleware,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::dependencies as graph,
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/issues/{issue}/dependencies/{direction}",
            get(list),
        )
        .route(
            "/api/repos/{owner}/{name}/issues/{issue}/dependencies/{direction}/{other}",
            get(detail).put(edit),
        )
        .layer(DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageParameters {
    after: Option<u64>,
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, issue, direction)): Path<(String, String, u64, graph::Direction)>,
    Query(params): Query<PageParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let cell = app::repository_cell(&server, &repo)?;
    let page = app::query_output(
        cell.client
            .query::<graph::List>(
                &cell.target,
                None,
                graph::Query {
                    issue: app::number(issue)?,
                    direction,
                    after: params.after.map(app::number).transpose()?,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let membership = app::membership(&server, &principal, &repo).await?;
    Ok(Json(json!({
        "items": page.items, "next": page.next,
        "can_manage": !page.archived && membership.principal.can_triage(repo.id),
    })))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, issue, direction, other)): Path<(
        String,
        String,
        u64,
        graph::Direction,
        u64,
    )>,
) -> Result<Json<graph::State>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let pair = direction.pair(app::number(issue)?, app::number(other)?);
    let cell = app::repository_cell(&server, &repo)?;
    let state = app::query_output(
        cell.client
            .query::<graph::Get>(&cell.target, None, pair)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(state))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    active: bool,
    version: u64,
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, issue, direction, other)): Path<(
        String,
        String,
        u64,
        graph::Direction,
        u64,
    )>,
    input: std::result::Result<Json<Selection>, JsonRejection>,
) -> Result<Json<graph::State>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let Json(input) = input?;
    if input.version > app::MAX_NUMBER {
        return Err(Error::Invalid("Invalid dependency version"));
    }
    let pair = direction.pair(app::number(issue)?, app::number(other)?);
    let actor = app::actor(&principal)?;
    let cell = app::repository_cell(&server, &repo)?;
    if !app::membership(&server, &principal, &repo)
        .await?
        .principal
        .can_triage(repo.id)
    {
        return Err(Error::DependencyPermission);
    }
    let outcome = app::command_output(
        cell.client
            .command::<graph::Set>(
                &cell.target,
                app::mutation_identity()?,
                graph::Edit {
                    pair,
                    actor: (&actor).into(),
                    active: input.active,
                    version: input.version,
                },
            )
            .await,
    )?;
    match outcome {
        graph::Outcome::Applied(state) => Ok(Json(state)),
        graph::Outcome::Archived => Err(Error::Archived),
        graph::Outcome::NotFound => Err(Error::NotFound),
        graph::Outcome::Conflict => Err(Error::Conflict),
        graph::Outcome::Cycle => Err(Error::Invalid("Issue dependencies cannot form a cycle")),
        graph::Outcome::DegreeLimit => Err(Error::Invalid(
            "An issue can have at most 50 dependencies in either direction",
        )),
        graph::Outcome::TraversalLimit => Err(Error::Invalid(
            "This dependency graph is too large to validate the new relationship",
        )),
    }
}

#[cfg(test)]
mod tests;
