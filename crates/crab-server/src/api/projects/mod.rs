use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{
        collaboration::{Subject, SubjectKind},
        projects::{self, Command, Outcome, Request},
    },
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/projects", get(list))
        .route(
            "/api/repos/{owner}/{name}/projects/candidates",
            get(candidates),
        )
        .route(
            "/api/repos/{owner}/{name}/projects/templates",
            get(templates),
        )
        .route("/api/repos/{owner}/{name}/projects/commands", post(change))
        .route("/api/repos/{owner}/{name}/projects/cancel", post(cancel))
        .route("/api/repos/{owner}/{name}/projects/{project}", get(detail))
        .route(
            "/api/repos/{owner}/{name}/projects/{project}/items",
            get(items),
        )
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/hierarchy",
            get(hierarchy),
        )
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/projects",
            get(memberships).layer(Extension(SubjectKind::Issue)),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/projects",
            get(memberships).layer(Extension(SubjectKind::Pull)),
        )
        .layer(axum::extract::DefaultBodyLimit::max(96 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Collection {
    after: Option<Uuid>,
    archived: bool,
    templates: bool,
}
async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<Collection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let cell = app::repository_cell(&server, &repo)?;
    let page = app::query_output(
        cell.client
            .query::<projects::ListProjects>(
                &cell.target,
                None,
                projects::ProjectQuery {
                    after: input.after,
                    archived: input.archived,
                    templates: input.templates,
                },
            )
            .await,
    )?;
    let current = app::membership(&server, &principal, &repo).await?;
    Ok(Json(
        json!({"items":page.items,"next":page.next,"can_manage":current.principal.can_write(repo.id)}),
    ))
}
async fn templates(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Value>> {
    app::repository(&server, &principal, &key)?;
    Ok(Json(json!({"items":projects::templates()})))
}
async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, project)): Path<(String, String, Uuid)>,
) -> Result<Json<Value>> {
    let key = (owner, name);
    let repo = app::repository(&server, &principal, &key)?;
    let cell = app::repository_cell(&server, &repo)?;
    let project = app::query_output(
        cell.client
            .query::<projects::GetProject>(&cell.target, None, project)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let current = app::membership(&server, &principal, &repo).await?;
    Ok(Json(
        json!({"project":project,"can_manage":current.principal.can_write(repo.id)}),
    ))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Filter {
    after: Option<Uuid>,
    revision: Option<u64>,
    q: String,
    status: Option<String>,
    hide_closed: bool,
}
async fn items(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, project)): Path<(String, String, Uuid)>,
    Query(input): Query<Filter>,
) -> Result<Json<Value>> {
    let key = (owner, name);
    let repo = app::repository(&server, &principal, &key)?;
    let cell = app::repository_cell(&server, &repo)?;
    let page = app::query_output(
        cell.client
            .query::<projects::Items>(
                &cell.target,
                None,
                projects::ItemQuery {
                    project,
                    after: input.after,
                    revision: input.revision,
                    query: input.q,
                    status: input.status,
                    hide_closed: input.hide_closed,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let selected = crate::api::assignees::Selection::load(
        &server,
        &principal,
        &repo,
        page.items.iter().flat_map(|row| row.assignees.iter()),
    )
    .await?;
    let mut presented = serde_json::to_value(&page)?;
    let items = page
        .items
        .iter()
        .map(|row| {
            let mut value = serde_json::to_value(row)?;
            value["assignees"] = json!(selected.view(&row.assignees));
            Ok(value)
        })
        .collect::<std::result::Result<Vec<_>, serde_json::Error>>()?;
    presented["items"] = json!(items);
    Ok(Json(
        json!({"page":presented,"can_manage":selected.principal.can_write(repo.id)}),
    ))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Children {
    after: Option<u64>,
}
async fn hierarchy(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(input): Query<Children>,
) -> Result<Json<Value>> {
    let key = (owner, name);
    let repo = app::repository(&server, &principal, &key)?;
    let cell = app::repository_cell(&server, &repo)?;
    let tree = app::query_output(
        cell.client
            .query::<projects::hierarchy::GetHierarchy>(
                &cell.target,
                None,
                projects::hierarchy::Query {
                    issue: app::number(number)?,
                    after: input.after,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let current = app::membership(&server, &principal, &repo).await?;
    Ok(Json(
        json!({"hierarchy":tree,"can_manage":current.principal.can_write(repo.id)}),
    ))
}
async fn memberships(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Extension(kind): Extension<SubjectKind>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let key = (owner, name);
    let repo = app::repository(&server, &principal, &key)?;
    let cell = app::repository_cell(&server, &repo)?;
    let items = app::query_output(
        cell.client
            .query::<projects::Memberships>(
                &cell.target,
                None,
                Subject {
                    kind,
                    number: app::number(number)?,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let current = app::membership(&server, &principal, &repo).await?;
    Ok(Json(
        json!({"items":items.into_iter().map(|(project,item)|json!({"project":project,"item":item})).collect::<Vec<_>>(),"can_manage":current.principal.can_write(repo.id)}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    request_id: Uuid,
    command: Command,
}
async fn submit(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    input: Submission,
    cancel: bool,
) -> Result<Json<Outcome>> {
    let repo = app::repository(&server, &principal, &key)?;
    let current = app::membership(&server, &principal, &repo).await?;
    if !current.principal.can_write(repo.id) {
        return Err(Error::ProjectPermission);
    }
    let actor = app::actor(&principal)?;
    let cell = app::repository_cell(&server, &repo)?;
    let output = app::command_output(
        cell.client
            .command::<projects::Change>(
                &cell.target,
                app::mutation_identity()?,
                Request {
                    request_id: input.request_id,
                    command: input.command,
                    actor: (&actor).into(),
                    can_write: true,
                    cancel,
                },
            )
            .await,
    )?;
    if output == Outcome::RequestConflict {
        return Err(Error::Conflict);
    }
    Ok(Json(output))
}
async fn change(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<Submission>, JsonRejection>,
) -> std::result::Result<Json<Outcome>, Response> {
    // Invalid immutable JSON cannot reach Cellar; admission and transport
    // failures remain unproven so an earlier submission can still recover.
    let Json(input) = input.map_err(Error::from).map_err(super::write::rejected)?;
    submit(server, principal, key, input, false)
        .await
        .map_err(IntoResponse::into_response)
}
async fn cancel(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<Submission>, JsonRejection>,
) -> std::result::Result<Json<Outcome>, Response> {
    let Json(input) = input.map_err(Error::from).map_err(super::write::rejected)?;
    submit(server, principal, key, input, true)
        .await
        .map_err(IntoResponse::into_response)
}
#[cfg(test)]
mod tests;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CandidateFilter {
    q: String,
    kind: Option<SubjectKind>,
}
async fn candidates(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<CandidateFilter>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let cell = app::repository_cell(&server, &repo)?;
    let items = app::query_output(
        cell.client
            .query::<projects::Candidates>(
                &cell.target,
                None,
                projects::CandidateQuery {
                    q: input.q,
                    kind: input.kind,
                },
            )
            .await,
    )?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(json!({"items":items})))
}
