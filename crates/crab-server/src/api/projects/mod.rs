use super::write::ResponseError;
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{
        collaboration::{Subject, SubjectKind},
        projects::{self, Command, Outcome, Request},
    },
    repository::Repository,
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
mod history;
pub(super) mod type_filter;

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
            "/api/repos/{owner}/{name}/projects/{project}/history",
            get(history::read),
        )
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
    let types = super::organizations::issue_types::active(&server, repo.organization).await?;
    let current = app::membership(&server, &principal, &repo).await?;
    Ok(Json(
        json!({"project":project,"can_manage":current.principal.can_write(repo.id),"issue_types":types.definitions}),
    ))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Filter {
    item: Option<Uuid>,
    after: Option<Uuid>,
    revision: Option<u64>,
    q: String,
    status: Option<String>,
    hide_closed: bool,
    issue_type: Option<String>,
    type_cursor: Option<String>,
    group_by: Option<projects::grouping::GroupBy>,
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
    let group_by_type = input.group_by == Some(projects::grouping::GroupBy::IssueType);
    let (type_scope, bound_types, type_cursor) = if input.issue_type.is_some() || group_by_type {
        use crate::cells::repository::issues::issue_type::search::Filter;
        let filter = input
            .issue_type
            .as_deref()
            .map(Filter::parse)
            .transpose()
            .map_err(Error::Invalid)?;
        let types = super::organizations::issue_types::active(&server, repo.organization).await?;
        let scope = type_filter::scope(&repo, filter, &types)?;
        let cursor = type_filter::cursor((
            "repository-project-type/1",
            repo.id,
            project,
            &scope,
            types.version,
            input.item,
            &input.q,
            &input.status,
            input.hide_closed,
            input.group_by,
        ))?;
        (Some(scope), Some(types), Some(cursor))
    } else {
        (None, None, None)
    };
    type_filter::validate_cursor(
        input.after.is_some(),
        type_cursor.as_deref(),
        input.type_cursor.as_deref(),
    )?;
    let page = app::query_output(
        cell.client
            .query::<projects::Items>(
                &cell.target,
                None,
                projects::ItemQuery {
                    project,
                    item: input.item,
                    after: input.after,
                    revision: input.revision,
                    query: input.q,
                    status: input.status,
                    hide_closed: input.hide_closed,
                    default_ref: repo.default_ref().await?,
                    issue_type: type_scope,
                    group_by: input.group_by,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let types = if let Some(types) = bound_types {
        super::discussion_search::issue_types::verify(&server, &repo, Some(&types)).await?;
        types
    } else if page.items.iter().any(|row| row.issue_type.is_some()) {
        super::organizations::issue_types::active(&server, repo.organization).await?
    } else {
        Default::default()
    };
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
            let (issue_type, version) = super::organizations::issue_types::project_field(
                &repo,
                row.issue_type,
                &types.definitions,
                selected.principal.can_triage(repo.id),
            );
            value["issue_type"] = json!(issue_type);
            value["issue_type_version"] = json!(version);
            if group_by_type {
                value["issue_type_group"] =
                    json!(crate::cells::repository::issues::issue_type::group(
                        row.issue_type.is_some(),
                        issue_type.map(|definition| definition.id),
                    ));
            }
            Ok(value)
        })
        .collect::<std::result::Result<Vec<_>, serde_json::Error>>()?;
    presented["items"] = json!(items);
    presented["issue_types"] = json!(types.definitions);
    presented["type_cursor"] = json!(type_cursor);
    Ok(Json(
        json!({"page":presented,"can_manage":selected.principal.can_write(repo.id)}),
    ))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Children {
    after: Option<u64>,
    limit: Option<u8>,
}
async fn hierarchy(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(input): Query<Children>,
) -> Result<Json<Value>> {
    let key = (owner, name);
    let repo = app::repository(&server, &principal, &key)?;
    if input
        .limit
        .is_some_and(|limit| !(1..=projects::hierarchy::MAX_CHILDREN).contains(&limit))
    {
        return Err(Error::Invalid(
            "Sub-issue page limit must be between 1 and 100",
        ));
    }
    let cell = app::repository_cell(&server, &repo)?;
    let tree = app::query_output(
        cell.client
            .query::<projects::hierarchy::GetHierarchy>(
                &cell.target,
                None,
                projects::hierarchy::Query {
                    issue: app::number(number)?,
                    after: input.after,
                    limit: input.limit,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let current = app::membership(&server, &principal, &repo).await?;
    Ok(Json(
        json!({"hierarchy":tree,"can_manage":current.principal.can_triage(repo.id)}),
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
    let can_write = current.principal.can_write(repo.id);
    let can_triage = current.principal.can_triage(repo.id);
    if !input.command.permitted(can_write, can_triage) {
        return Err(if input.command.is_hierarchy() {
            Error::SubIssuePermission
        } else {
            Error::ProjectPermission
        });
    }
    let actor = app::actor(&principal)?;
    let type_id = match &input.command {
        Command::CreateIssue { type_id, .. } if !cancel => *type_id,
        _ => None,
    };
    let mut request = Request {
        request_id: input.request_id,
        command: input.command,
        actor: (&actor).into(),
        can_write,
        can_triage: !can_write && can_triage,
        issue_type: None,
        cancel,
    };
    let Some((organization, type_id)) = repo.organization.zip(type_id) else {
        return submit_cell(&server, &repo, request).await;
    };
    super::organizations::properties::policy_write(
        server,
        organization,
        move |server, _, cancel| async move {
            let current = app::membership(&server, &principal, &repo).await?;
            if !current.principal.can_write(repo.id) {
                return Err(Error::ProjectPermission);
            }
            // The Cell resolves a saved decision before examining current type
            // evidence. A disabled type cannot change the outcome of a retry.
            request.issue_type =
                super::organizations::issue_types::active_selection(&server, &repo, type_id)
                    .await?;
            app::account(&current.principal)?;
            if cancel.is_cancelled() {
                return Err(Error::MergeBusy);
            }
            submit_cell(&server, &repo, request).await
        },
    )
    .await
}

async fn submit_cell(
    server: &Server,
    repo: &Repository,
    request: Request,
) -> Result<Json<Outcome>> {
    let cell = app::repository_cell(server, repo)?;
    let output = app::command_output(
        cell.client
            .command::<projects::Change>(&cell.target, app::mutation_identity()?, request)
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
) -> std::result::Result<Json<Outcome>, ResponseError> {
    // Invalid immutable JSON cannot reach Cellar; admission and transport
    // failures remain unproven so an earlier submission can still recover.
    let Json(input) = input.map_err(Error::from).map_err(super::write::rejected)?;
    submit(server, principal, key, input, false)
        .await
        .map_err(|error| error.into_response().into())
}
async fn cancel(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<Submission>, JsonRejection>,
) -> std::result::Result<Json<Outcome>, ResponseError> {
    let Json(input) = input.map_err(Error::from).map_err(super::write::rejected)?;
    submit(server, principal, key, input, true)
        .await
        .map_err(|error| error.into_response().into())
}
#[cfg(test)]
mod hierarchy_tests;
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
