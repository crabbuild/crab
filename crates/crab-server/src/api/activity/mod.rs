use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::profiles::attribution::Authors,
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::{
        Subject, SubjectKind,
        activity::{ListActivity, Query as ActivityQuery},
    },
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/activity", get(repository))
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/activity",
            get(issue),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/activity",
            get(pull),
        )
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Parameters {
    before: Option<u64>,
    limit: Option<u8>,
}

async fn page(
    server: &Server,
    principal: &Principal,
    key: (String, String),
    subject: Option<Subject>,
    params: Parameters,
) -> Result<Json<Value>> {
    let repo = app::repository(server, principal, &key)?;
    let limit = params.limit.unwrap_or(30);
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid("Activity pages require 1–50 events"));
    }
    if let Some(before) = params.before {
        app::number(before)?;
    }
    if let Some(subject) = subject {
        app::number(subject.number)?;
    }
    let cell = app::repository_cell(server, &repo)?;
    let page = app::query_output(
        cell.client
            .query::<ListActivity>(
                &cell.target,
                None,
                ActivityQuery {
                    subject,
                    before: params.before,
                    limit,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let authors = Authors::load(
        server,
        principal,
        page.items.iter().map(|event| &event.actor),
    )
    .await?;
    app::membership(server, principal, &repo).await?;
    let items = page
        .items
        .iter()
        .map(|event| view(&authors, event))
        .collect::<Vec<_>>();
    Ok(Json(json!({"items":items,"next":page.next})))
}

pub(in crate::api) fn view(
    authors: &Authors,
    event: &crate::cells::repository::collaboration::activity::Event,
) -> Value {
    json!({"number":event.number,"target":event.target,"title":event.title,
    "actor":authors.view(&event.actor),"action":event.action,"created_at":event.created_at})
}

async fn repository(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    page(&server, &principal, key, None, params).await
}

async fn issue(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    let subject = Subject {
        kind: SubjectKind::Issue,
        number,
    };
    page(&server, &principal, (owner, name), Some(subject), params).await
}

async fn pull(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    let subject = Subject {
        kind: SubjectKind::Pull,
        number,
    };
    page(&server, &principal, (owner, name), Some(subject), params).await
}

#[cfg(test)]
mod tests;
