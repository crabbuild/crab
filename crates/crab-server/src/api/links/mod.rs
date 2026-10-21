use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    middleware,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::{
        Subject, SubjectKind,
        links::{GetLink, ListLinks, Outcome, Pair, Query as LinkQuery, Selection, SetLink},
    },
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/links",
            get(list).layer(Extension(SubjectKind::Issue)),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/links",
            get(list).layer(Extension(SubjectKind::Pull)),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{pull}/linked-issues/{issue}",
            get(detail).put(set),
        )
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Parameters {
    before: Option<u64>,
    limit: Option<u8>,
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Extension(kind): Extension<SubjectKind>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let limit = params.limit.unwrap_or(30);
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid("Linked work pages require 1–50 items"));
    }
    if let Some(before) = params.before {
        app::number(before)?;
    }
    let subject = Subject {
        kind,
        number: app::number(number)?,
    };
    let cell = app::repository_cell(&server, &repo)?;
    let page = app::query_output(
        cell.client
            .query::<ListLinks>(
                &cell.target,
                None,
                LinkQuery {
                    subject,
                    before: params.before,
                    limit,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    Ok(Json(
        json!({"items":page.items,"next":page.next,"can_manage":principal.can_write(repo.id)}),
    ))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, pull, issue)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let pair = Pair {
        pull: app::number(pull)?,
        issue: app::number(issue)?,
    };
    let cell = app::repository_cell(&server, &repo)?;
    let link = app::query_output(cell.client.query::<GetLink>(&cell.target, None, pair).await)?
        .ok_or(Error::NotFound)?;
    Ok(Json(json!({"active":link.active,"version":link.version})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    active: bool,
    version: u64,
}

async fn set(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, pull, issue)): Path<(String, String, u64, u64)>,
    input: std::result::Result<Json<Edit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    if !principal.can_write(repo.id) {
        return Err(Error::LinkPermission);
    }
    let actor = app::actor(&principal)?;
    let Json(input) = input?;
    if input.version > app::MAX_NUMBER {
        return Err(Error::Invalid("Invalid linked work version"));
    }
    let pair = Pair {
        pull: app::number(pull)?,
        issue: app::number(issue)?,
    };
    let cell = app::repository_cell(&server, &repo)?;
    let result = app::command_output(
        cell.client
            .command::<SetLink>(
                &cell.target,
                app::mutation_identity()?,
                Selection {
                    pair,
                    active: input.active,
                    version: input.version,
                    actor: (&actor).into(),
                },
            )
            .await,
    )?;
    let link = match result {
        Outcome::Applied(link) => link,
        Outcome::Archived => return Err(Error::Archived),
        Outcome::NotFound => return Err(Error::NotFound),
        Outcome::Conflict => return Err(Error::Conflict),
    };
    Ok(Json(json!({"active":link.active,"version":link.version})))
}

#[cfg(test)]
mod tests;
